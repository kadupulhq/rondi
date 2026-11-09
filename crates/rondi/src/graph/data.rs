//! Fetching, reducing, computing and exporting the series of a graph
//! (GR-47 to GR-49).

use super::{Cf, DaemonHook, Graph, Kind, Offset, Series, VdefValue};
use crate::StoreError;
use crate::rpn::{Evaluator, Row, Source};
use crate::rrd_binary::fetch_rrd_file;
use crate::vdef::evaluate_vdef;

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

/// `ceil(value / step)` and `floor(value / step)` for a positive step.
fn ceil_div(value: i64, step: i64) -> i64 {
    value.div_euclid(step) + i64::from(value.rem_euclid(step) != 0)
}

pub(super) fn fetch(graph: &mut Graph, daemon: DaemonHook<'_>) -> Result<(), StoreError> {
    for index in 0..graph.elements.len() {
        let Some(def) = graph.elements[index].def.clone() else {
            continue;
        };
        let address = def.daemon.clone().or_else(|| graph.daemon.clone());
        if let Some(address) = address.filter(|address| !address.is_empty())
            && let Some(message) = daemon(&address, &def.file)?
        {
            graph.pending_error = Some(message);
        }
        let series = match read(&def) {
            Ok(series) => series,
            // Compatibility: with -Z a file or data source that cannot be
            // read gives unknown values over the DEF's window.
            // Provenance: black-box compatibility test (def-072).
            Err(_) if graph.allow_missing_ds => unknown_series(&def),
            Err(error) => return Err(error),
        };
        graph.elements[index].series = reduce(series, def.step, def.reduce)?;
    }
    Ok(())
}

/// The DEF's data source as fetched, at the archive step.
fn read(def: &super::DefSpec) -> Result<Series, StoreError> {
    // A zero image step (a window shorter than the width) asks for 1 s.
    let fetched = fetch_rrd_file(
        &def.file,
        def.cf.name(),
        def.start,
        def.end,
        def.step.max(1),
    )?;
    let column = fetched
        .data_sources
        .iter()
        .position(|name| *name == def.ds)
        .ok_or_else(|| {
            StoreError::Rrd(format!("No DS called '{}' in '{}'", def.ds, def.file))
        })?;
    let step = fetched.step.max(1);
    let start = match fetched.rows.first() {
        Some(row) => row.timestamp - step as i64,
        None => fetched.start,
    };
    Ok(Series {
        start,
        step,
        values: fetched
            .rows
            .iter()
            .map(|row| row.values.get(column).copied().flatten().unwrap_or(f64::NAN))
            .collect(),
    })
}

fn unknown_series(def: &super::DefSpec) -> Series {
    let step = def.step.max(1);
    let start = def.start.div_euclid(step as i64) * step as i64;
    let end = ceil_div(def.end, step as i64) * step as i64;
    let count = usize::try_from((end - start) / step as i64).unwrap_or(0);
    Series {
        start,
        step,
        values: vec![f64::NAN; count],
    }
}

/// Combines rows when the archive step is finer than the wanted step
/// (GR-47).
fn reduce(series: Series, wanted: u64, cf: Cf) -> Result<Series, StoreError> {
    let step = series.step;
    if wanted <= step {
        return Ok(series);
    }
    let factor = wanted.div_ceil(step);
    let new_step = factor * step;
    let (first, last) = (series.start, series.end());
    let span = new_step as i64;
    let full = last.div_euclid(span) - ceil_div(first, span);
    if full < 0 {
        // Compatibility: the row count is printed as an unsigned number,
        // so a window shorter than one new step shows a huge count.
        // Provenance: black-box compatibility test (prep-015, num-097).
        let rows = (full.wrapping_mul(span) / step as i64) as u64;
        return Err(StoreError::Rrd(format!(
            "SANITY CHECK: {rows} rows cannot be reduced by {factor} \n"
        )));
    }
    let start = first.div_euclid(span) * span;
    let end = ceil_div(last, span) * span;
    let buckets = usize::try_from((end - start) / span).unwrap_or(0);
    let values = (0..buckets)
        .map(|bucket| {
            let bucket_start = start + bucket as i64 * span;
            if bucket_start < first || bucket_start + span > last {
                return f64::NAN;
            }
            let from = usize::try_from((bucket_start - first) / step as i64).unwrap_or(0);
            let rows = series.values.iter().skip(from).take(factor as usize);
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
    let end = series.iter().map(|series| series.end()).min().unwrap_or(start);
    let rows = usize::try_from((end - start) / step as i64).unwrap_or(0);
    let first_time = start + step as i64;
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
        let own_step = own.step.max(1) as i64;
        let first = usize::try_from(ceil_div(first_time - own.start, own_step) - 1).unwrap_or(0);
        Source::Series {
            values: &own.values,
            step: own.step,
            first,
        }
    });
    let mut values = Vec::with_capacity(rows);
    let mut previous = f64::NAN;
    for row in 0..rows {
        let time = start + (row as i64 + 1) * step as i64;
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

/// Moves a series by whole steps (GR-49, GR-55).
fn shift(graph: &mut Graph, index: usize) {
    let element = &graph.elements[index];
    let (Some(source), Some(offset)) = (element.source, element.offset) else {
        return;
    };
    let seconds = match offset {
        Offset::Seconds(seconds) => seconds,
        Offset::Vdef(vdef) => graph.elements[vdef].vdef.value as i64,
    };
    let series = &mut graph.elements[source].series;
    let step = series.step.max(1) as i64;
    // Compatibility: the offset is truncated toward zero to whole steps,
    // and the start wraps as a 64-bit time does.
    // Provenance: black-box compatibility test (prep-063 to -079,
    // graph_xport_parity shift_by_a_huge_offset_wraps_like_rrdtool).
    series.start = series.start.wrapping_add(seconds / step * step);
}

pub(super) fn export(graph: &Graph, with_drawn: bool) -> Result<Export, StoreError> {
    let columns: Vec<(&super::Element, &super::Element)> = graph
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
    let step = columns
        .iter()
        .filter(|(_, source)| source.kind != Kind::Vdef)
        .fold(0, |step, (_, source)| gcd(step, source.series.step));
    let step = if step == 0 { graph.step.max(1) } else { step };
    let span = step as i64;
    let start = graph.start.div_euclid(span) * span;
    let end = ceil_div(graph.end, span) * span;
    let count = usize::try_from((end - start) / span).unwrap_or(0);
    let rows = (0..count)
        .map(|row| {
            let time = start + (row as i64 + 1) * span;
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
        assert_eq!(reduce(fetched.clone(), 1000, Cf::Average).unwrap().step, 1200);
        assert_eq!(reduce(fetched.clone(), 300, Cf::Average).unwrap().values.len(), 7);
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
}
