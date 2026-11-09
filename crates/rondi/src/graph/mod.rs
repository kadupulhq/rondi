//! The elements of `graph`, `graphv` and `xport` scripts and their data.
//!
//! A [`Graph`] is built in three steps: [`Graph::parse_elements`] reads the
//! element arguments (GR-40 to GR-59), [`Graph::fetch`] reads every DEF's
//! file (GR-43, GR-47) and [`Graph::compute`] works out the CDEFs, VDEFs and
//! SHIFTs in element order (GR-45, GR-46, GR-48, GR-49). The values the
//! print elements show and the export grid come from the computed series.
//! The rules are in docs/compatibility/graph.md.

mod data;
mod elements;
mod fields;

use crate::StoreError;
use crate::rpn::Expression;
use crate::vdef::VdefFunction;

pub use data::Export;

/// What an element is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    Def,
    Cdef,
    Vdef,
    Shift,
    Line,
    Area,
    Tick,
    Hrule,
    Vrule,
    Print,
    Gprint,
    #[default]
    Comment,
    TextAlign,
    Xport,
}

impl Kind {
    /// Whether the element puts something on the canvas or into the legend
    /// of an image.
    pub fn draws(self) -> bool {
        matches!(
            self,
            Self::Line | Self::Area | Self::Tick | Self::Hrule | Self::Vrule | Self::Gprint
        )
    }
}

/// How a PRINT or GPRINT shows its value (GR-58).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ValueFormatter {
    #[default]
    Numeric,
    /// `:valstrftime`.
    Timestamp,
    /// `:valstrfduration`.
    Duration,
}

/// `TEXTALIGN` (GR-56).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    #[default]
    Justified,
}

/// A consolidation function of a DEF, of its reduction or of a PRINT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cf {
    Average,
    Minimum,
    Maximum,
    Last,
}

impl Cf {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "AVERAGE" => Self::Average,
            "MIN" => Self::Minimum,
            "MAX" => Self::Maximum,
            "LAST" => Self::Last,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Average => "AVERAGE",
            Self::Minimum => "MIN",
            Self::Maximum => "MAX",
            Self::Last => "LAST",
        }
    }

    /// The function over the known values; unknown when there are none.
    fn apply(self, values: impl Iterator<Item = f64>) -> f64 {
        let mut known = values.filter(|value| !value.is_nan());
        let Some(first) = known.next() else {
            return f64::NAN;
        };
        match self {
            Self::Average => {
                let (sum, count) = known.fold((first, 1_u64), |(sum, count), value| {
                    (sum + value, count + 1)
                });
                sum / count as f64
            }
            Self::Minimum => known.fold(first, |low, value| if value < low { value } else { low }),
            Self::Maximum => {
                known.fold(first, |high, value| if value > high { value } else { high })
            }
            Self::Last => known.last().unwrap_or(first),
        }
    }
}

/// The value and time a VDEF computed (GR-46).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VdefValue {
    pub value: f64,
    /// `None` for functions that give no time, or over no known value.
    pub time: Option<i64>,
}

impl Default for VdefValue {
    fn default() -> Self {
        Self {
            value: f64::NAN,
            time: None,
        }
    }
}

/// A series of values at a fixed step. `start` is the start of the first
/// interval; value `n` is stamped `start + (n + 1) * step`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series {
    pub start: i64,
    pub step: u64,
    pub values: Vec<f64>,
}

impl Series {
    /// The end of the last interval, clamped to the 64-bit range.
    pub fn end(&self) -> i64 {
        let span = self.values.len() as i128 * i128::from(self.step.max(1));
        clamp_time(i128::from(self.start) + span)
    }

    /// The value whose interval holds `time`: from just after its start to
    /// its end. Unknown outside the series.
    pub fn at(&self, time: i64) -> f64 {
        // A shifted start may sit anywhere in the 64-bit range, so the
        // offset is taken in 128 bits.
        let step = i128::from(self.step.max(1));
        let offset = i128::from(time) - i128::from(self.start);
        if offset <= 0 {
            return f64::NAN;
        }
        let index = (offset + step - 1) / step - 1;
        usize::try_from(index)
            .ok()
            .and_then(|index| self.values.get(index).copied())
            .unwrap_or(f64::NAN)
    }
}

fn clamp_time(time: i128) -> i64 {
    time.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// What a DEF reads (GR-43).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DefSpec {
    file: String,
    ds: String,
    cf: Cf,
    /// The reduction CF, the DEF's CF by default.
    reduce: Cf,
    /// `reduce=`, when given.
    reduce_given: Option<Cf>,
    start: i64,
    end: i64,
    /// The step the file is read for: `step=`, or the image step.
    fetch_step: u64,
    /// The step the result is reduced to: the larger of `step=` and the
    /// image step (GR-47).
    step: u64,
    daemon: Option<String>,
}

/// What a VDEF computes (GR-46).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VdefSpec {
    function: VdefFunction,
    parameter: Option<f64>,
}

/// How far a SHIFT moves its series (GR-55).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Offset {
    Seconds(i64),
    /// The value of a VDEF, by element index.
    Vdef(usize),
}

/// One element of the script.
#[derive(Debug, Clone, Default)]
pub struct Element {
    pub kind: Kind,
    /// The variable name of a DEF, CDEF or VDEF.
    pub name: Option<String>,
    /// The element whose data this element uses: the DEF, CDEF or VDEF named
    /// by a drawing, print, SHIFT or XPORT element, or the series of a VDEF.
    pub source: Option<usize>,
    /// Legend text; drawing elements carry two leading spaces for the
    /// colour box. GPRINT text is filled in when the value is formatted.
    pub legend: String,
    /// The PRINT or GPRINT format.
    pub format: String,
    /// `:strftime` on a PRINT or GPRINT.
    pub strftime: bool,
    pub formatter: ValueFormatter,
    pub color: Option<[u8; 4]>,
    pub color2: Option<[u8; 4]>,
    pub line_width: f64,
    pub stack: bool,
    pub skip_scale: bool,
    pub gradient_height: f64,
    pub dashes: Vec<f64>,
    pub dash_offset: f64,
    /// The HRULE value, the TICK fraction, or the number a drawing element
    /// draws in place of a variable.
    pub value: f64,
    /// The VRULE time.
    pub time: i64,
    pub align: TextAlign,
    /// A VDEF's result, after [`Graph::compute`].
    pub vdef: VdefValue,
    /// A DEF's or CDEF's data, after [`Graph::fetch`] and
    /// [`Graph::compute`].
    pub series: Series,
    pub(crate) def: Option<DefSpec>,
    pub(crate) cdef: Option<Expression>,
    pub(crate) vdef_spec: Option<VdefSpec>,
    pub(crate) offset: Option<Offset>,
    /// The CF of a deprecated PRINT or GPRINT over a DEF or CDEF.
    pub(crate) cf: Option<Cf>,
    /// The whole-step offset the last SHIFT of this series applied
    /// (GR-38).
    pub(crate) shifted: i64,
    /// The `debug=` block of a DEF, moved to [`Graph::debug`].
    pub(crate) debug: String,
}

/// Resolves the `start=` and `end=` of a DEF against the image window:
/// `(start text, end text, image start, image end)`; the error is printed
/// after `ERROR: `.
pub type TimeResolver<'a> =
    &'a dyn Fn(Option<&str>, Option<&str>, i64, i64) -> Result<(i64, i64), String>;

/// Called with a daemon address and file name before a DEF reads the file.
/// `Ok(Some(message))` means the daemon could not be reached: the file is
/// read directly and the message is kept as [`Graph::pending_error`].
pub type DaemonHook<'a> = &'a mut dyn FnMut(&str, &str) -> Result<Option<String>, StoreError>;

/// A script failure; the text is printed after `ERROR: `.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError(pub String);

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ScriptError {}

/// A graph or xport script.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    /// The image window.
    pub start: i64,
    pub end: i64,
    /// The image step: the step each DEF wants unless it gives `step=`.
    pub step: u64,
    pub elements: Vec<Element>,
    /// The image-wide `--daemon` or `RRDCACHED_ADDRESS`.
    pub daemon: Option<String>,
    /// `-Z`: a missing file or data source gives unknown values (GR-43).
    pub allow_missing_ds: bool,
    /// An error RRDtool leaves set without failing, reported once the
    /// command finished.
    pub pending_error: Option<String>,
    /// What `debug=` asked to write to standard error, in element order
    /// (GR-96).
    pub debug: String,
}

impl Graph {
    pub fn new(start: i64, end: i64, step: u64) -> Self {
        Self {
            start,
            end,
            step,
            ..Self::default()
        }
    }

    /// The element defining the variable `name`; the first definition wins.
    fn variable(&self, name: &str) -> Option<usize> {
        self.elements.iter().position(|element| {
            matches!(element.kind, Kind::Def | Kind::Cdef | Kind::Vdef)
                && element.name.as_deref() == Some(name)
        })
    }

    /// Reads every element argument in order (GR-40 to GR-59).
    pub fn parse_elements(
        &mut self,
        args: &[String],
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        for arg in args {
            elements::parse(self, arg, times).map_err(ScriptError)?;
        }
        Ok(())
    }

    /// Reads the data of every DEF (GR-43, GR-47).
    pub fn fetch(&mut self, daemon: DaemonHook<'_>) -> Result<(), StoreError> {
        data::fetch(self, daemon)
    }

    /// Computes the CDEFs, VDEFs and SHIFTs in element order (GR-48,
    /// GR-49).
    pub fn compute(&mut self) -> Result<(), StoreError> {
        data::compute(self)
    }

    /// The value a PRINT or GPRINT element shows: its VDEF's value, or the
    /// CF over its DEF's or CDEF's whole series (GR-49, GR-58).
    pub fn print_value(&self, index: usize) -> f64 {
        let element = &self.elements[index];
        let Some(source) = element.source.map(|source| &self.elements[source]) else {
            return f64::NAN;
        };
        match element.cf {
            // Compatibility: the CF form skips infinite values as well as
            // unknown ones.
            // Provenance: black-box compatibility test (alias_modes CDEF and
            // VDEF scalars, probes with INF and NEGINF rows).
            Some(cf) => cf.apply(
                source
                    .series
                    .values
                    .iter()
                    .copied()
                    .filter(|value| value.is_finite()),
            ),
            None => source.vdef.value,
        }
    }

    /// The value the variable of element `index` has at `time`: a VDEF's
    /// value, or the DEF's or CDEF's value whose interval holds `time`.
    pub fn value_at(&self, index: usize, time: i64) -> f64 {
        let element = &self.elements[index];
        if element.kind == Kind::Vdef {
            element.vdef.value
        } else {
            element.series.at(time)
        }
    }

    /// The exported series on their common grid (GR-48, GR-86): the XPORT
    /// elements, and with `with_drawn` the LINE and AREA elements too.
    pub fn export(&self, with_drawn: bool) -> Result<Export, StoreError> {
        data::export(self, with_drawn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_values_by_time() {
        let series = Series {
            start: 100,
            step: 10,
            values: vec![1.0, 2.0, 3.0],
        };
        assert!(series.at(100).is_nan());
        assert_eq!(series.at(101), 1.0);
        assert_eq!(series.at(110), 1.0);
        assert_eq!(series.at(111), 2.0);
        assert_eq!(series.at(130), 3.0);
        assert!(series.at(131).is_nan());
        assert_eq!(series.end(), 130);
    }

    #[test]
    fn consolidation_over_known_values() {
        let values = [f64::NAN, 2.0, 4.0, f64::NAN, 3.0];
        let apply = |cf: Cf| cf.apply(values.iter().copied());
        assert_eq!(apply(Cf::Average), 3.0);
        assert_eq!(apply(Cf::Minimum), 2.0);
        assert_eq!(apply(Cf::Maximum), 4.0);
        assert_eq!(apply(Cf::Last), 3.0);
        assert!(Cf::Average.apply([f64::NAN].into_iter()).is_nan());
        assert_eq!(Cf::parse("MIN").map(Cf::name), Some("MIN"));
        assert_eq!(Cf::parse("min"), None);
    }
}
