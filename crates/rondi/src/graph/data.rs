//! Fetching, reducing, computing and exporting the series of a graph
//! (GR-47 to GR-49).

use super::{Cf, DaemonHook, DefSpec, Element, Graph, Kind, Offset, Series, VdefValue};
use crate::StoreError;
use crate::rpn::{Evaluator, Row, Source};
use crate::rrd_binary::fetch_rrd_file;
use crate::vdef::evaluate_vdef;

/// The most rows one series or one export grid may hold (GR-92, RD-017).
pub(super) const MOST_ROWS: usize = 10_000_000;

/// The file name prefix the reference reserves for fetch callbacks.
const CALLBACK_PREFIX: &str = "cb//";

/// The exported series on one grid (GR-48). Row `n` is stamped
/// `start + (n + 1) * step`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Export {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    pub legends: Vec<String>,
    pub rows: Vec<Vec<f64>>,
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

/// `ceil(value / step)` for a positive step.
fn ceil_div(value: i64, step: i64) -> i64 {
    value.div_euclid(step) + i64::from(value.rem_euclid(step) != 0)
}

/// The GR-92 refusal, also used where a row count or a time would not
/// fit.
fn too_many_rows() -> StoreError {
    StoreError::RrdUnsupported("RRD fetch range exceeds the 10 million row safety limit".into())
}

/// A step as a signed number of seconds.
fn signed_step(step: u64) -> Result<i64, StoreError> {
    i64::try_from(step.max(1)).map_err(|_| too_many_rows())
}

/// The number of `step` rows from `start` to `end`, none when `end` is
/// not after `start`, refused above [`MOST_ROWS`].
fn row_count(start: i64, end: i64, step: u64) -> Result<usize, StoreError> {
    let rows = (i128::from(end) - i128::from(start)).max(0) / i128::from(step.max(1));
    usize::try_from(rows)
        .ok()
        .filter(|&rows| rows <= MOST_ROWS)
        .ok_or_else(too_many_rows)
}

/// `value` rounded up to a multiple of `span`.
fn round_up(value: i64, span: i64) -> Result<i64, StoreError> {
    ceil_div(value, span)
        .checked_mul(span)
        .ok_or_else(too_many_rows)
}

/// Why a DEF could not be read.
enum ReadError {
    /// The file could not be read: with `-Z` this also clears an earlier
    /// unreachable-daemon error.
    File(StoreError),
    /// The file has no such data source.
    DataSource(StoreError),
}

/// What makes a later DEF take an earlier DEF's result instead of reading
/// its file (GR-98).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ReuseKey {
    file: String,
    cf: Cf,
    reduce: Cf,
    start: u32,
    end: u32,
    step: u64,
    daemon: Option<String>,
}

impl ReuseKey {
    // Compatibility: the key is the file name as written, the CF, the
    // `reduce=` CF (AVERAGE when not given), the window's start and end in
    // their low 32 bits, the step and the daemon address; the data source
    // is not part of it. An earlier DEF is found by its reduced step, a
    // later one looks with the step it asks for.
    // Provenance: black-box compatibility test (graph_engine_parity fetch
    // reuse, probes of each part of the key).
    fn new(def: &DefSpec, step: u64, daemon: Option<&String>) -> Self {
        Self {
            file: def.file.clone(),
            cf: def.cf,
            reduce: def.reduce_given.unwrap_or(Cf::Average),
            start: def.start as u32,
            end: def.end as u32,
            step,
            daemon: daemon.cloned(),
        }
    }
}

pub(super) fn fetch(graph: &mut Graph, daemon: DaemonHook<'_>) -> Result<(), StoreError> {
    let mut done: Vec<(ReuseKey, DefSpec)> = Vec::new();
    for index in 0..graph.elements.len() {
        let Some(def) = graph.elements[index].def.clone() else {
            continue;
        };
        let address = def
            .daemon
            .clone()
            .or_else(|| graph.daemon.clone())
            .filter(|address| !address.is_empty());
        let wanted = ReuseKey::new(&def, def.fetch_step, address.as_ref());
        // A reused fetch is the earlier DEF's, reduced as it was, with this
        // DEF's own data source taken from it.
        let reused = done
            .iter()
            .find(|(key, _)| *key == wanted)
            .map(|(_, earlier)| {
                let mut spec = earlier.clone();
                spec.ds.clone_from(&def.ds);
                spec
            });
        // GR-37: every DEF still makes its own daemon request.
        if !def.file.starts_with(CALLBACK_PREFIX)
            && let Some(address) = &address
            && let Some(message) = daemon(address, &def.file)?
        {
            graph.pending_error = Some(message);
        }
        let fresh = reused.is_none();
        let spec = reused.unwrap_or_else(|| def.clone());
        let series = match read(&spec) {
            Ok(series) => series,
            // Compatibility: with -Z a file or data source that cannot be
            // read gives unknown values over the DEF's window, and a file
            // that cannot be read clears an earlier daemon connection error.
            // The fetch size limit is not lifted.
            // Provenance: black-box compatibility test (def-072,
            // graph_engine_parity unreachable daemon, probes with -Z).
            Err(ReadError::File(error)) if missing_allowed(graph, &error) => {
                graph.pending_error = None;
                unknown_series(&spec)?
            }
            Err(ReadError::DataSource(error)) if missing_allowed(graph, &error) => {
                unknown_series(&spec)?
            }
            Err(ReadError::File(error) | ReadError::DataSource(error)) => return Err(error),
        };
        let series = reduce(series, spec.step, spec.reduce)?;
        if fresh {
            done.push((ReuseKey::new(&def, series.step, address.as_ref()), def));
        }
        graph.elements[index].series = series;
    }
    Ok(())
}

fn missing_allowed(graph: &Graph, error: &StoreError) -> bool {
    graph.allow_missing_ds && !matches!(error, StoreError::RrdUnsupported(_))
}

/// The DEF's data source as fetched, at the archive step.
fn read(def: &DefSpec) -> Result<Series, ReadError> {
    if def.file.starts_with(CALLBACK_PREFIX) {
        // Compatibility: such names are never opened or sent to a daemon.
        // Provenance: black-box compatibility test (graph_data_differential
        // print formatters, probes with --daemon).
        return Err(ReadError::File(StoreError::Rrd(
            "use rrd_fetch_cb_register to register your callback prior to calling \
             rrd_fetch_fn_cb"
                .into(),
        )));
    }
    // A zero image step (a window shorter than the width) asks for 1 s.
    let fetched = fetch_rrd_file(
        &def.file,
        def.cf.name(),
        def.start,
        def.end,
        def.fetch_step.max(1),
    )
    .map_err(ReadError::File)?;
    let column = fetched
        .data_sources
        .iter()
        .position(|name| *name == def.ds)
        .ok_or_else(|| {
            ReadError::DataSource(StoreError::Rrd(format!(
                "No DS called '{}' in '{}'",
                def.ds, def.file
            )))
        })?;
    let step = fetched.step.max(1);
    let start = match fetched.rows.first() {
        Some(row) => row.timestamp.saturating_sub_unsigned(step),
        None => fetched.start,
    };
    Ok(Series {
        start,
        step,
        values: fetched
            .rows
            .iter()
            .map(|row| {
                row.values
                    .get(column)
                    .copied()
                    .flatten()
                    .unwrap_or(f64::NAN)
            })
            .collect(),
    })
}

/// Unknown values over the DEF's window, at its wanted step.
fn unknown_series(def: &DefSpec) -> Result<Series, StoreError> {
    let step = def.step.max(1);
    let span = signed_step(step)?;
    let start = def.start.div_euclid(span) * span;
    let end = round_up(def.end, span)?;
    Ok(Series {
        start,
        step,
        values: vec![f64::NAN; row_count(start, end, step)?],
    })
}

/// Combines rows when the archive step is finer than the wanted step
/// (GR-47).
fn reduce(series: Series, wanted: u64, cf: Cf) -> Result<Series, StoreError> {
    let step = series.step.max(1);
    if wanted <= step {
        return Ok(series);
    }
    let factor = wanted.div_ceil(step);
    let new_step = factor.checked_mul(step).ok_or_else(too_many_rows)?;
    let span = signed_step(new_step)?;
    let archive_step = signed_step(step)?;
    let (first, last) = (series.start, series.end());
    let full = last.div_euclid(span) - ceil_div(first, span);
    if full < 0 {
        // Compatibility: the row count is printed as an unsigned number,
        // so a window shorter than one new step shows a huge count.
        // Provenance: black-box compatibility test (prep-015, num-097).
        let rows = (full.wrapping_mul(span) / archive_step) as u64;
        return Err(StoreError::Rrd(format!(
            "SANITY CHECK: {rows} rows cannot be reduced by {factor} \n"
        )));
    }
    let start = first.div_euclid(span) * span;
    let end = round_up(last, span)?;
    let buckets = row_count(start, end, new_step)?;
    let per_bucket = usize::try_from(factor).unwrap_or(usize::MAX);
    let values = (0..buckets)
        .map(|bucket| {
            // Every bucket ends by `end`, so its start fits.
            let bucket_start = start + bucket as i64 * span;
            if bucket_start < first || last - span < bucket_start {
                return f64::NAN;
            }
            let from = usize::try_from((bucket_start - first) / archive_step).unwrap_or(0);
            let rows = series.values.iter().skip(from).take(per_bucket);
            cf.apply(rows.copied())
        })
        .collect();
    Ok(Series {
        start,
        step: new_step,
        values,
    })
}

pub(super) fn compute(graph: &mut Graph) -> Result<(), StoreError> {
    for index in 0..graph.elements.len() {
        match graph.elements[index].kind {
            Kind::Cdef => {
                let series = cdef(graph, index)?;
                graph.elements[index].series = series;
            }
            Kind::Vdef => {
                let value = vdef(graph, index)?;
                graph.elements[index].vdef = value;
            }
            Kind::Shift => shift(graph, index),
            _ => {}
        }
    }
    Ok(())
}

/// Evaluates a CDEF over the common window of its series (GR-48).
fn cdef(graph: &Graph, index: usize) -> Result<Series, StoreError> {
    let element = &graph.elements[index];
    let Some(expression) = element.cdef.as_ref() else {
        return Ok(Series::default());
    };
    let series: Vec<&Series> = expression
        .variables()
        .filter(|&variable| variable != index && graph.elements[variable].kind != Kind::Vdef)
        .map(|variable| &graph.elements[variable].series)
        .collect();
    let step = series.iter().fold(0, |step, series| gcd(step, series.step));
    if step == 0 {
        return Ok(Series::default());
    }
    let start = series.iter().map(|series| series.start).max().unwrap_or(0);
    let end = series
        .iter()
        .map(|series| series.end())
        .min()
        .unwrap_or(start);
    if start > end {
        // Compatibility: series without a common window fail.
        // Provenance: black-box compatibility test (graph_engine_parity
        // disjoint windows, probes of the window edges).
        return Err(StoreError::Rrd("malloc im->gdes[gdi].data".into()));
    }
    let rows = row_count(start, end, step)?;
    let span = signed_step(step)?;
    let first_time = i128::from(start) + i128::from(span);
    let mut evaluator = Evaluator::new(expression, |variable| {
        let source = &graph.elements[variable];
        if variable == index {
            return Source::Series {
                values: &[],
                step,
                first: 0,
            };
        }
        if source.kind == Kind::Vdef {
            return Source::Constant(source.vdef.value);
        }
        let own = &source.series;
        let own_step = i128::from(own.step.max(1));
        let offset = first_time - i128::from(own.start);
        let first = usize::try_from(
            offset.div_euclid(own_step) + i128::from(offset.rem_euclid(own_step) != 0) - 1,
        )
        .unwrap_or(0);
        Source::Series {
            values: &own.values,
            step: own.step,
            first,
        }
    });
    let mut values = Vec::with_capacity(rows);
    let mut previous = f64::NAN;
    for row in 0..rows {
        // Every row ends by `end`, so its time fits.
        let time = start + (row as i64 + 1) * span;
        previous = evaluator
            .evaluate(&Row {
                time,
                index: row,
                step,
                previous,
            })
            .map_err(|error| StoreError::RrdExpression(error.to_string()))?;
        values.push(previous);
    }
    Ok(Series {
        start,
        step,
        values,
    })
}

/// A VDEF over every row of its series (GR-46, GR-49).
fn vdef(graph: &Graph, index: usize) -> Result<VdefValue, StoreError> {
    let element = &graph.elements[index];
    let (Some(spec), Some(source)) = (element.vdef_spec, element.source) else {
        return Ok(VdefValue::default());
    };
    let series = &graph.elements[source].series;
    if series.values.is_empty() {
        return Ok(VdefValue::default());
    }
    let result = evaluate_vdef(
        spec.function,
        spec.parameter,
        &series.values,
        series.start,
        series.step,
    )
    .map_err(|error| StoreError::RrdExpression(error.to_string()))?;
    Ok(VdefValue {
        value: result.value,
        time: result.timestamp,
    })
}

/// Moves a series by whole steps (GR-38, GR-49, GR-55).
fn shift(graph: &mut Graph, index: usize) {
    let element = &graph.elements[index];
    let (Some(source), Some(offset)) = (element.source, element.offset) else {
        return;
    };
    let seconds = match offset {
        Offset::Seconds(seconds) => seconds,
        // An unknown value shifts by nothing; `as` saturates the rest.
        Offset::Vdef(vdef) => graph.elements[vdef].vdef.value as i64,
    };
    let target = &mut graph.elements[source];
    let step = i64::try_from(target.series.step.max(1)).unwrap_or(i64::MAX);
    // Compatibility: the offset is truncated toward zero to whole steps and
    // replaces the one an earlier SHIFT of the series gave; the start wraps
    // as a 64-bit time does.
    // Provenance: black-box compatibility test (prep-063 to -079, shift-038
    // to -043, graph_xport_parity huge offsets).
    let whole = seconds / step * step;
    target.series.start = target
        .series
        .start
        .wrapping_sub(target.shifted)
        .wrapping_add(whole);
    target.shifted = whole;
}

/// The grid that exported and drawn series share (GR-48): the greatest
/// common step of their series, from the image start rounded down to it to
/// the image end rounded up to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grid {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    /// At most [`MOST_ROWS`].
    pub rows: usize,
}

impl Grid {
    /// The stamp of row `row`: the end of its step.
    pub fn time(&self, row: usize) -> i64 {
        // Every row ends by `end`, so its stamp fits.
        self.start + (row as i64 + 1) * self.step as i64
    }
}

/// The grid of the series `sources` (VDEFs have none), refused above
/// [`MOST_ROWS`] rows (GR-92).
pub(super) fn grid<'g>(
    graph: &Graph,
    sources: impl IntoIterator<Item = &'g Element>,
) -> Result<Grid, StoreError> {
    let step = sources
        .into_iter()
        .filter(|source| source.kind != Kind::Vdef)
        .fold(0, |step, source| gcd(step, source.series.step));
    let step = if step == 0 { graph.step.max(1) } else { step };
    let span = signed_step(step)?;
    let start = graph
        .start
        .div_euclid(span)
        .checked_mul(span)
        .ok_or_else(too_many_rows)?;
    let end = round_up(graph.end, span)?;
    Ok(Grid {
        start,
        end,
        step,
        rows: row_count(start, end, step)?,
    })
}

pub(super) fn export(graph: &Graph, with_drawn: bool) -> Result<Export, StoreError> {
    let columns: Vec<(&Element, &Element)> = graph
        .elements
        .iter()
        .filter(|element| {
            element.kind == Kind::Xport
                || (with_drawn && matches!(element.kind, Kind::Line | Kind::Area))
        })
        .filter_map(|element| {
            element
                .source
                .map(|source| (element, &graph.elements[source]))
        })
        .collect();
    if columns.is_empty() {
        return Err(StoreError::Rrd("no XPORT found, nothing to do".into()));
    }
    let grid = grid(graph, columns.iter().map(|(_, source)| *source))?;
    let rows = (0..grid.rows)
        .map(|row| {
            let time = grid.time(row);
            columns
                .iter()
                .map(|(_, source)| {
                    if source.kind == Kind::Vdef {
                        source.vdef.value
                    } else {
                        source.series.at(time)
                    }
                })
                .collect()
        })
        .collect();
    let Grid {
        start, end, step, ..
    } = grid;
    Ok(Export {
        start,
        end,
        step,
        legends: columns
            .iter()
            .map(|(element, _)| element.legend.clone())
            .collect(),
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(start: i64, step: u64, values: &[f64]) -> Series {
        Series {
            start,
            step,
            values: values.to_vec(),
        }
    }

    #[test]
    fn reduction_buckets_align_to_the_new_step() {
        // Rows of 300 s from 3600; three to a bucket ending at multiples of
        // 900, the last one only partly fetched.
        let fetched = series(3600, 300, &[1.0, 2.0, 3.0, 4.0, f64::NAN, 6.0, 7.0]);
        let reduced = reduce(fetched.clone(), 900, Cf::Average).unwrap();
        assert_eq!((reduced.start, reduced.step), (3600, 900));
        assert_eq!(reduced.values.len(), 3);
        assert_eq!(reduced.values[0], 2.0);
        assert_eq!(reduced.values[1], 5.0);
        assert!(reduced.values[2].is_nan());
        let maximum = reduce(fetched.clone(), 900, Cf::Maximum).unwrap();
        assert_eq!(maximum.values[1], 6.0);
        // A step that is not a multiple rounds up to one.
        assert_eq!(
            reduce(fetched.clone(), 1000, Cf::Average).unwrap().step,
            1200
        );
        assert_eq!(
            reduce(fetched.clone(), 300, Cf::Average)
                .unwrap()
                .values
                .len(),
            7
        );
        let error = reduce(series(3500, 300, &[1.0]), 86400, Cf::Average).unwrap_err();
        assert_eq!(
            error.to_string(),
            "SANITY CHECK: 18446744073709551328 rows cannot be reduced by 288 \n"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(gcd(300, 1800), 300);
        assert_eq!(gcd(0, 60), 60);
        assert_eq!(ceil_div(7, 3), 3);
        assert_eq!(ceil_div(-7, 3), -2);
        assert_eq!(ceil_div(6, 3), 2);
    }

    /// A window start on a multiple of 600 after 1980.
    const T: i64 = 1_000_000_200;

    fn times(
        start: Option<&str>,
        end: Option<&str>,
        image_start: i64,
        image_end: i64,
    ) -> Result<(i64, i64), String> {
        let parse = |text: Option<&str>, default| {
            text.map_or(Ok(default), |text| {
                text.parse().map_err(|_| text.to_owned())
            })
        };
        Ok((parse(start, image_start)?, parse(end, image_end)?))
    }

    fn graph(elements: &[&str]) -> Graph {
        let mut graph = Graph::new(T, T + 1200, 300);
        let args: Vec<String> = elements.iter().map(|text| (*text).to_owned()).collect();
        graph.parse_elements(&args, &times).unwrap();
        graph
    }

    fn no_daemon(_: &str, _: &str) -> Result<Option<String>, StoreError> {
        Ok(None)
    }

    #[test]
    fn row_counts_are_bounded() {
        assert_eq!(row_count(0, 10_000_000, 1).unwrap(), MOST_ROWS);
        assert_eq!(row_count(5, 0, 1).unwrap(), 0);
        assert!(row_count(0, 10_000_001, 1).is_err());
        assert!(row_count(i64::MIN, i64::MAX, 1).is_err());
        assert!(signed_step(u64::MAX).is_err());
        assert!(round_up(i64::MAX - 1, 1000).is_err());
    }

    #[test]
    fn reduction_steps_are_checked() {
        // The bucket step would not fit in 64 bits.
        assert!(reduce(series(0, 2, &[1.0]), u64::MAX, Cf::Average).is_err());
        // It fits as an unsigned number but not as a time.
        assert!(reduce(series(0, 3, &[1.0]), u64::MAX, Cf::Average).is_err());
        // A zero archive step is read as one second.
        let reduced = reduce(series(T, 0, &[1.0, 3.0]), 2, Cf::Average).unwrap();
        assert_eq!((reduced.step, reduced.values), (2, vec![2.0]));
    }

    #[test]
    fn unknown_series_and_exports_are_bounded() {
        let mut def = graph(&["DEF:a=missing.rrd:x:AVERAGE"]).elements[0]
            .def
            .clone()
            .unwrap();
        assert_eq!(unknown_series(&def).unwrap().values.len(), 4);
        def.step = 1;
        def.end = def.start + 10_000_001;
        assert_eq!(
            unknown_series(&def).unwrap_err().to_string(),
            too_many_rows().to_string()
        );
        let mut wide = graph(&["DEF:a=missing.rrd:x:AVERAGE", "XPORT:a"]);
        assert_eq!(wide.export(false).unwrap().rows.len(), 4);
        (wide.step, wide.end) = (1, T + 10_000_001);
        assert_eq!(
            wide.export(false).unwrap_err().to_string(),
            too_many_rows().to_string()
        );
    }

    #[test]
    fn grids_of_coprime_steps_are_bounded() {
        let mut graph = graph(&["DEF:a=f.rrd:x:AVERAGE", "DEF:b=f.rrd:x:AVERAGE"]);
        graph.elements[0].series = series(T, 1800, &[1.0]);
        graph.elements[1].series = series(T, 1801, &[1.0]);
        let shared = graph.grid(&[0, 1]).unwrap();
        assert_eq!((shared.step, shared.rows), (1, 1200));
        assert_eq!(shared.time(0), T + 1);
        graph.end = T + 20_000_000;
        assert_eq!(
            graph.grid(&[0, 1]).unwrap_err().to_string(),
            too_many_rows().to_string()
        );
        // A start near the bottom of the time range does not overflow.
        graph.start = i64::MIN;
        assert!(graph.grid(&[0, 1]).is_err());
    }

    #[test]
    fn each_def_starts_after_1980() {
        let mut graph = Graph::new(T, T + 1200, 300);
        let early = ["DEF:a=f.rrd:x:AVERAGE:start=315359999".to_owned()];
        assert_eq!(
            graph.parse_elements(&early, &times).unwrap_err().0,
            "the first entry to fetch should be after 1980 (315359999)"
        );
        let earliest = ["DEF:a=f.rrd:x:AVERAGE:start=315360000".to_owned()];
        assert!(graph.parse_elements(&earliest, &times).is_ok());
    }

    #[test]
    fn cdef_windows_over_mixed_steps() {
        let mut graph = graph(&[
            "DEF:a=f.rrd:x:AVERAGE",
            "DEF:b=f.rrd:x:AVERAGE",
            "CDEF:c=a,b,+",
        ]);
        graph.elements[0].series = series(T, 300, &[1.0, 2.0, 3.0, 4.0]);
        graph.elements[1].series = series(T + 600, 200, &[10.0, 20.0, 30.0]);
        graph.compute().unwrap();
        let c = &graph.elements[2].series;
        // The common window from the latest start to the earliest end, on
        // the greatest common step.
        assert_eq!((c.start, c.step), (T + 600, 100));
        assert_eq!(c.values, [13.0, 13.0, 23.0, 24.0, 34.0, 34.0]);
    }

    #[test]
    fn cdef_windows_are_checked() {
        let mut graph = graph(&[
            "DEF:a=f.rrd:x:AVERAGE",
            "DEF:b=f.rrd:x:AVERAGE",
            "CDEF:c=a,b,+",
        ]);
        // Steps of 20,000,000 s and 20,000,001 s share a one-second step, so
        // the common window holds too many rows; none are allocated.
        graph.elements[0].series = series(T, 20_000_000, &[1.0]);
        graph.elements[1].series = series(T, 20_000_001, &[1.0]);
        assert_eq!(
            graph.compute().unwrap_err().to_string(),
            too_many_rows().to_string()
        );
        // Windows that do not meet.
        graph.elements[0].series = series(T, 300, &[1.0]);
        graph.elements[1].series = series(T + 600, 300, &[1.0]);
        assert_eq!(
            graph.compute().unwrap_err().to_string(),
            "malloc im->gdes[gdi].data"
        );
        // Windows that only touch give no rows.
        graph.elements[1].series = series(T + 300, 300, &[1.0]);
        graph.compute().unwrap();
        assert!(graph.elements[2].series.values.is_empty());
    }

    #[test]
    fn a_failed_daemon_flush_stops_the_fetch() {
        let mut graph = graph(&["DEF:a=missing.rrd:x:AVERAGE", "DEF:b=other.rrd:x:AVERAGE"]);
        graph.daemon = Some("unix:/nowhere".into());
        graph.allow_missing_ds = true;
        let mut calls = Vec::new();
        let mut hook = |address: &str, file: &str| {
            calls.push((address.to_owned(), file.to_owned()));
            Err(StoreError::Rrd("flush refused".into()))
        };
        assert_eq!(
            graph.fetch(&mut hook).unwrap_err().to_string(),
            "flush refused"
        );
        assert_eq!(calls, [("unix:/nowhere".into(), "missing.rrd".into())]);
        assert!(graph.elements[0].series.values.is_empty());
    }

    #[test]
    fn shifts_truncate_toward_zero_and_replace() {
        let mut graph = graph(&["DEF:a=f.rrd:x:AVERAGE", "SHIFT:a:-450"]);
        graph.elements[0].series = series(T, 300, &[1.0]);
        graph.compute().unwrap();
        assert_eq!(graph.elements[0].series.start, T - 300);
        let mut graph = graph_with_shifts();
        graph.compute().unwrap();
        // The second SHIFT replaces the first instead of adding to it.
        assert_eq!(graph.elements[0].series.start, T + 600);
    }

    fn graph_with_shifts() -> Graph {
        let mut graph = graph(&["DEF:a=f.rrd:x:AVERAGE", "SHIFT:a:-450", "SHIFT:a:700"]);
        graph.elements[0].series = series(T, 300, &[1.0]);
        graph
    }

    fn rrd_file(dir: &std::path::Path) -> String {
        let path = dir.join("f.rrd");
        crate::rrd_binary::create_rrd_file(
            &path,
            T - 3000,
            1,
            &["DS:x:GAUGE:600:U:U".to_owned()],
            &["RRA:AVERAGE:0.5:1:100".to_owned()],
            false,
        )
        .unwrap();
        path.to_str().unwrap().to_owned()
    }

    #[test]
    fn missing_data_sources_and_the_row_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = rrd_file(dir.path());
        let missing = format!("DEF:a={file}:nope:AVERAGE");
        let mut strict = graph(&[&missing]);
        assert_eq!(
            strict.fetch(&mut no_daemon).unwrap_err().to_string(),
            format!("No DS called 'nope' in '{file}'")
        );
        // def-073: with -Z the data source reads as unknown values.
        let mut lenient = graph(&[&missing]);
        lenient.allow_missing_ds = true;
        lenient.fetch(&mut no_daemon).unwrap();
        let values = &lenient.elements[0].series.values;
        assert!(!values.is_empty() && values.iter().all(|value| value.is_nan()));
        // -Z does not lift the row limit.
        let wide = format!("DEF:a={file}:x:AVERAGE:step=1:end={}", T + 10_000_001);
        let mut wide = graph(&[&wide]);
        wide.allow_missing_ds = true;
        let message = wide.fetch(&mut no_daemon).unwrap_err().to_string();
        assert!(
            message.contains("RRD fetch range exceeds the 10 million row safety limit"),
            "{message}"
        );
    }
}
