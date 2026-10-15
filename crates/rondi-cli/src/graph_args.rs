//! The command lines of `graph`, `graphv` and `xport`: options, the output
//! name and the element words.
//!
//! Written from docs/compatibility/graph.md (GR-1 to GR-33, GR-88) and
//! black-box runs of RRDtool 1.11.0. Options are scanned in order with the
//! shared [`OptionScanner`]; most values are checked as they are read and
//! the rest after the last option, in the order of GR-17.

use std::error::Error;

use super::cparse::{atoi, atol, rrd_strtodbl};
use super::frontend::{OptionScanner, OptionSpec};
use rondi::format_value::{DurationError, format_duration};
use rondi::time::{TimeSpec, parse_time_spec, resolve_start_end};

type Failure = Box<dyn Error>;

/// Starts below this are refused (time-spec.md TS-61).
const EARLIEST_START: i64 = 315_360_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opt {
    Start,
    End,
    Step,
    Title,
    VerticalLabel,
    VerticalLabelAngle,
    Width,
    Height,
    OnlyGraph,
    FullSizeMode,
    UpperLimit,
    LowerLimit,
    Rigid,
    AllowShrink,
    AltAutoscale,
    AltAutoscaleMin,
    AltAutoscaleMax,
    XGrid,
    YGrid,
    LeftFormatter,
    LeftFormat,
    Logarithmic,
    UnitsExponent,
    UnitsLength,
    Units,
    RightAxis,
    RightAxisLabel,
    RightAxisLabelAngle,
    RightAxisRange,
    RightFormatter,
    RightFormat,
    NoLegend,
    ForceRulesLegend,
    LegendPosition,
    LegendDirection,
    Lazy,
    Daemon,
    ImgInfo,
    Color,
    GridDash,
    Border,
    Zoom,
    Font,
    FontRenderMode,
    FontSmoothing,
    PangoMarkup,
    GraphRenderMode,
    ImgFormat,
    TabWidth,
    Base,
    Watermark,
    MissingAsNan,
    AddJsonTime,
    Utc,
    NoRrdtoolTag,
    /// Flags that only change pixels: accepted and otherwise ignored.
    Cosmetic,
    /// A value that only changes pixels and is never checked.
    CosmeticValue,
    GraphType,
    MaxRows,
    Json,
    ShowTime,
    EnumDs,
}

const fn value(long: &'static str, short: Option<char>, key: Opt) -> OptionSpec<Opt> {
    OptionSpec::value(long, short, key)
}

const fn flag(long: &'static str, short: Option<char>, key: Opt) -> OptionSpec<Opt> {
    OptionSpec::flag(long, short, key)
}

/// GR-16.
const GRAPH_TABLE: &[OptionSpec<Opt>] = &[
    value("start", Some('s'), Opt::Start),
    value("end", Some('e'), Opt::End),
    value("step", Some('S'), Opt::Step),
    value("title", Some('t'), Opt::Title),
    value("vertical-label", Some('v'), Opt::VerticalLabel),
    value("vertical-label-angle", None, Opt::VerticalLabelAngle),
    value("width", Some('w'), Opt::Width),
    value("height", Some('h'), Opt::Height),
    flag("only-graph", Some('j'), Opt::OnlyGraph),
    flag("full-size-mode", Some('D'), Opt::FullSizeMode),
    value("upper-limit", Some('u'), Opt::UpperLimit),
    value("lower-limit", Some('l'), Opt::LowerLimit),
    flag("rigid", Some('r'), Opt::Rigid),
    flag("allow-shrink", None, Opt::AllowShrink),
    flag("alt-autoscale", Some('A'), Opt::AltAutoscale),
    flag("alt-autoscale-min", Some('J'), Opt::AltAutoscaleMin),
    flag("alt-autoscale-max", Some('M'), Opt::AltAutoscaleMax),
    flag("no-gridfit", Some('N'), Opt::Cosmetic),
    value("x-grid", Some('x'), Opt::XGrid),
    value("week-fmt", None, Opt::CosmeticValue),
    value("y-grid", Some('y'), Opt::YGrid),
    value("left-axis-formatter", None, Opt::LeftFormatter),
    value("left-axis-format", None, Opt::LeftFormat),
    flag("alt-y-grid", Some('Y'), Opt::Cosmetic),
    flag("logarithmic", Some('o'), Opt::Logarithmic),
    value("units-exponent", Some('X'), Opt::UnitsExponent),
    value("units-length", Some('L'), Opt::UnitsLength),
    value("units", None, Opt::Units),
    value("right-axis", None, Opt::RightAxis),
    value("right-axis-label", None, Opt::RightAxisLabel),
    value("right-axis-label-angle", None, Opt::RightAxisLabelAngle),
    value("right-axis-range", None, Opt::RightAxisRange),
    value("right-axis-formatter", None, Opt::RightFormatter),
    value("right-axis-format", None, Opt::RightFormat),
    flag("no-legend", Some('g'), Opt::NoLegend),
    flag("force-rules-legend", Some('F'), Opt::ForceRulesLegend),
    value("legend-position", None, Opt::LegendPosition),
    value("legend-direction", None, Opt::LegendDirection),
    flag("lazy", Some('z'), Opt::Lazy),
    value("daemon", Some('d'), Opt::Daemon),
    value("imginfo", Some('f'), Opt::ImgInfo),
    value("color", Some('c'), Opt::Color),
    value("grid-dash", None, Opt::GridDash),
    value("border", None, Opt::Border),
    flag("dynamic-labels", None, Opt::Cosmetic),
    value("zoom", Some('m'), Opt::Zoom),
    value("font", Some('n'), Opt::Font),
    value("font-render-mode", Some('R'), Opt::FontRenderMode),
    value("font-smoothing-threshold", Some('B'), Opt::FontSmoothing),
    flag("pango-markup", Some('P'), Opt::PangoMarkup),
    value("graph-render-mode", Some('G'), Opt::GraphRenderMode),
    flag("slope-mode", Some('E'), Opt::Cosmetic),
    value("imgformat", Some('a'), Opt::ImgFormat),
    flag("interlaced", Some('i'), Opt::Cosmetic),
    value("tabwidth", Some('T'), Opt::TabWidth),
    value("base", Some('b'), Opt::Base),
    value("watermark", Some('W'), Opt::Watermark),
    flag("use-nan-for-all-missing-data", Some('Z'), Opt::MissingAsNan),
    flag("add-jsontime", None, Opt::AddJsonTime),
    flag("utc", None, Opt::Utc),
    flag("disable-rrdtool-tag", None, Opt::NoRrdtoolTag),
    flag("alt-y-mrtg", None, Opt::Cosmetic),
    flag("no-minor", None, Opt::Cosmetic),
    value("graph-type", None, Opt::GraphType),
    // Compatibility: a short flag without a long form or visible effect.
    // Provenance: black-box compatibility test (repeat-014).
    flag("", Some('I'), Opt::Cosmetic),
];

/// GR-88.
const XPORT_TABLE: &[OptionSpec<Opt>] = &[
    value("start", Some('s'), Opt::Start),
    value("end", Some('e'), Opt::End),
    value("maxrows", Some('m'), Opt::MaxRows),
    value("step", Some('S'), Opt::Step),
    flag("json", None, Opt::Json),
    flag("showtime", Some('t'), Opt::ShowTime),
    flag("enumds", None, Opt::EnumDs),
    value("daemon", Some('d'), Opt::Daemon),
];

/// What `--imgformat` asks for (GR-23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageFormat {
    Png,
    /// SVG, EPS or PDF, which Rondi refuses (GR-93); the name is kept for
    /// the message.
    Vector(&'static str),
    Data(DataFormat),
}

/// The data exports of GR-86.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataFormat {
    Xml,
    XmlEnum,
    Json,
    JsonTime,
    Csv,
    Tsv,
    Ssv,
}

impl ImageFormat {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "PNG" => Self::Png,
            "SVG" => Self::Vector("SVG"),
            "EPS" => Self::Vector("EPS"),
            "PDF" => Self::Vector("PDF"),
            "XML" => Self::Data(DataFormat::Xml),
            "XMLENUM" => Self::Data(DataFormat::XmlEnum),
            "JSON" => Self::Data(DataFormat::Json),
            "JSONTIME" => Self::Data(DataFormat::JsonTime),
            "CSV" => Self::Data(DataFormat::Csv),
            "TSV" => Self::Data(DataFormat::Tsv),
            "SSV" => Self::Data(DataFormat::Ssv),
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AxisFormatter {
    Numeric,
    Timestamp,
    Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegendPlacement {
    North,
    South,
    East,
    West,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LegendOrder {
    TopDown,
    BottomUp,
    /// Like `BottomUp`, keeping leading and trailing comments in place.
    BottomUpKeepComments,
}

/// The `--color` tags (GR-22).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorTag {
    Back,
    Canvas,
    ShadeA,
    ShadeB,
    Grid,
    MajorGrid,
    Font,
    Axis,
    Frame,
    Arrow,
}

/// Text sizes per `--font` tag, in points; `None` keeps the default.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct TextSizes {
    pub(crate) title: Option<f64>,
    pub(crate) axis: Option<f64>,
    pub(crate) unit: Option<f64>,
    pub(crate) legend: Option<f64>,
    pub(crate) watermark: Option<f64>,
}

/// One axis's label formatter and format.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AxisLabels {
    pub(crate) formatter: AxisFormatter,
    pub(crate) format: Option<String>,
}

impl Default for AxisLabels {
    fn default() -> Self {
        Self {
            formatter: AxisFormatter::Numeric,
            format: None,
        }
    }
}

impl AxisLabels {
    /// GR-25: a duration format that cannot be used. It is reported only
    /// once the labels are drawn.
    pub(crate) fn duration_unusable(&self) -> bool {
        self.formatter == AxisFormatter::Duration
            && self.format.as_deref().is_some_and(|format| {
                matches!(
                    format_duration(format, 0.0, "."),
                    Err(DurationError::WrongFormat)
                )
            })
    }
}

/// A parsed `graph` or `graphv` command line.
#[derive(Clone, Debug)]
pub(crate) struct GraphArgs {
    pub(crate) output: String,
    pub(crate) elements: Vec<String>,
    pub(crate) start: i64,
    pub(crate) end: i64,
    /// `--step` as `atoi` read it; 0 or less asks for nothing (GR-32).
    pub(crate) step: i64,
    pub(crate) width: i64,
    pub(crate) height: i64,
    pub(crate) format: ImageFormat,
    pub(crate) imginfo: Option<String>,
    pub(crate) zoom: f64,
    pub(crate) lazy: bool,
    pub(crate) add_jsontime: bool,
    pub(crate) daemon: Option<String>,
    pub(crate) utc: bool,
    pub(crate) base: u32,
    pub(crate) missing_as_nan: bool,
    pub(crate) title: Option<String>,
    pub(crate) vertical_label: Option<String>,
    pub(crate) vertical_label_angle: f64,
    pub(crate) right_axis_label: Option<String>,
    pub(crate) watermark: Option<String>,
    pub(crate) text_sizes: TextSizes,
    pub(crate) colors: Vec<(ColorTag, [u8; 4])>,
    pub(crate) upper_limit: Option<f64>,
    pub(crate) lower_limit: Option<f64>,
    pub(crate) rigid: bool,
    pub(crate) allow_shrink: bool,
    /// `-A` widens both ends; it is not the same as `-J -M` (GR-71).
    pub(crate) alt_autoscale: bool,
    pub(crate) alt_autoscale_min: bool,
    pub(crate) alt_autoscale_max: bool,
    pub(crate) logarithmic: bool,
    pub(crate) units_exponent: Option<i32>,
    pub(crate) units_length: Option<i32>,
    pub(crate) only_graph: bool,
    pub(crate) full_size_mode: bool,
    pub(crate) no_rrdtool_tag: bool,
    pub(crate) border: i32,
    pub(crate) grid_dash: (f64, f64),
    /// `--x-grid none` anywhere hides the x grid, whatever comes later.
    pub(crate) hide_x_grid: bool,
    /// The label format of the last full `--x-grid` spec (GR-86).
    pub(crate) x_label_format: Option<String>,
    pub(crate) y_grid: Option<YGrid>,
    pub(crate) right_axis: Option<(f64, f64)>,
    pub(crate) left_labels: AxisLabels,
    pub(crate) right_labels: AxisLabels,
    pub(crate) no_legend: bool,
    pub(crate) force_rules_legend: bool,
    pub(crate) legend_placement: LegendPlacement,
    pub(crate) legend_order: LegendOrder,
    pub(crate) tab_width: Option<f64>,
    /// `--graph-type XY`, refused once a graph is laid out (GR-19).
    pub(crate) xy_graph: bool,
}

/// `--y-grid`: hidden, or a grid step and label factor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum YGrid {
    Hidden,
    Step { step: f64, factor: i32 },
}

impl Default for GraphArgs {
    fn default() -> Self {
        Self {
            output: String::new(),
            elements: Vec::new(),
            start: 0,
            end: 0,
            step: 0,
            width: 400,
            height: 100,
            format: ImageFormat::Png,
            imginfo: None,
            zoom: 1.0,
            lazy: false,
            add_jsontime: false,
            daemon: None,
            utc: false,
            base: 1000,
            missing_as_nan: false,
            title: None,
            vertical_label: None,
            vertical_label_angle: 90.0,
            right_axis_label: None,
            watermark: None,
            text_sizes: TextSizes::default(),
            colors: Vec::new(),
            upper_limit: None,
            lower_limit: None,
            rigid: false,
            allow_shrink: false,
            alt_autoscale: false,
            alt_autoscale_min: false,
            alt_autoscale_max: false,
            logarithmic: false,
            units_exponent: None,
            units_length: None,
            only_graph: false,
            full_size_mode: false,
            no_rrdtool_tag: false,
            border: 2,
            grid_dash: (1.0, 1.0),
            hide_x_grid: false,
            x_label_format: None,
            y_grid: None,
            right_axis: None,
            left_labels: AxisLabels::default(),
            right_labels: AxisLabels::default(),
            no_legend: false,
            force_rules_legend: false,
            legend_placement: LegendPlacement::South,
            legend_order: LegendOrder::TopDown,
            tab_width: None,
            xy_graph: false,
        }
    }
}

impl GraphArgs {
    /// GR-25: the axis whose duration format fails once labels are drawn.
    pub(crate) fn unusable_duration_format(&self) -> bool {
        if self.y_grid == Some(YGrid::Hidden) || self.only_graph || self.logarithmic {
            return false;
        }
        self.left_labels.duration_unusable()
            || (self.right_axis.is_some() && self.right_labels.duration_unusable())
    }
}

/// A parsed `xport` command line.
#[derive(Clone, Debug)]
pub(crate) struct XportArgs {
    pub(crate) elements: Vec<String>,
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) step: i64,
    pub(crate) max_rows: i64,
    pub(crate) daemon: Option<String>,
    pub(crate) json: bool,
    pub(crate) show_time: bool,
    pub(crate) enum_ds: bool,
}

/// GR-18: options that may be given only once.
#[derive(Default)]
struct OnceOnly {
    units: bool,
    add_jsontime: bool,
    daemon: bool,
}

impl OnceOnly {
    fn see(&mut self, key: Opt) -> Result<(), Failure> {
        let (seen, message) = match key {
            Opt::Units => (&mut self.units, "--units can only be used once!"),
            Opt::AddJsonTime => (
                &mut self.add_jsontime,
                "--add-jsontime can only be used once!",
            ),
            Opt::Daemon => (
                &mut self.daemon,
                "You cannot specify --daemon more than once.",
            ),
            _ => return Ok(()),
        };
        if std::mem::replace(seen, true) {
            return Err(message.into());
        }
        Ok(())
    }
}

/// The `--start`/`--end` pair while options are read.
struct Window {
    start: Option<TimeSpec>,
    end: Option<TimeSpec>,
    now: i64,
}

impl Window {
    fn new(now: i64) -> Self {
        Self {
            start: None,
            end: None,
            now,
        }
    }

    fn set(&mut self, is_start: bool, text: &str) -> Result<(), Failure> {
        let label = if is_start { "start time" } else { "end time" };
        let spec = parse_time_spec(text, self.now).map_err(|error| format!("{label}: {error}"))?;
        if is_start {
            self.start = Some(spec);
        } else {
            self.end = Some(spec);
        }
        Ok(())
    }

    /// TS-4 defaults, TS-56 to TS-61.
    fn resolve(&self) -> Result<(i64, i64), Failure> {
        let start = match self.start {
            Some(spec) => spec,
            None => parse_time_spec("end-24h", self.now)?,
        };
        let end = match self.end {
            Some(spec) => spec,
            None => TimeSpec::at(self.now),
        };
        let (start, end) = resolve_start_end(&start, &end)?;
        if start < EARLIEST_START {
            return Err(format!("the first entry to fetch should be after 1980 ({start})").into());
        }
        if start > end {
            return Err(format!("start ({start}) should be less than end ({end})").into());
        }
        Ok((start, end))
    }
}

/// GR-21: a floating-point option read over its whole value.
fn float_option(name: &str, text: &str) -> Result<f64, Failure> {
    Ok(rrd_strtodbl(text, Some(&format!("option {name}")))?)
}

fn keyword<T: Copy>(text: &str, table: &[(&str, T)]) -> Option<T> {
    table
        .iter()
        .find(|(name, _)| *name == text)
        .map(|(_, item)| *item)
}

/// Reads a `graph` or `graphv` command line; `args[0]` is the command.
pub(crate) fn parse_graph(args: &[String], now: i64) -> Result<GraphArgs, Failure> {
    let mut parsed = GraphArgs::default();
    let mut window = Window::new(now);
    let words = args.get(1..).unwrap_or_default();
    let mut scanner = OptionScanner::new(words, GRAPH_TABLE);
    let mut once = OnceOnly::default();
    for found in scanner.by_ref() {
        let found = found?;
        once.see(found.key)?;
        apply_graph_option(&mut parsed, &mut window, found.key, found.text())?;
    }
    // GR-17: the checks that wait for the last option.
    for labels in [&parsed.left_labels, &parsed.right_labels] {
        if labels.formatter == AxisFormatter::Numeric
            && let Some(format) = labels.format.as_deref()
        {
            check_axis_format(format)?;
        }
    }
    if parsed.logarithmic && parsed.lower_limit.is_some_and(|limit| limit <= 0.0) {
        return Err("for a logarithmic yaxis you must specify a lower-limit > 0".into());
    }
    (parsed.start, parsed.end) = window.resolve()?;
    let mut words = scanner.into_operands().into_iter();
    parsed.output = words.next().ok_or("missing filename")?;
    parsed.elements = words.collect();
    Ok(parsed)
}

fn apply_graph_option(
    parsed: &mut GraphArgs,
    window: &mut Window,
    key: Opt,
    text: &str,
) -> Result<(), Failure> {
    match key {
        Opt::Start | Opt::End => window.set(key == Opt::Start, text)?,
        Opt::Step => parsed.step = i64::from(atoi(text)),
        Opt::Title => parsed.title = Some(text.to_owned()),
        Opt::VerticalLabel => parsed.vertical_label = Some(text.to_owned()),
        Opt::VerticalLabelAngle => {
            parsed.vertical_label_angle = float_option("--vertical-label-angle", text)?;
        }
        Opt::RightAxisLabelAngle => {
            float_option("--right-axis-label-angle", text)?;
        }
        Opt::Width => {
            parsed.width = atol(text);
            if parsed.width < 10 {
                return Err("width below 10 pixels".into());
            }
        }
        Opt::Height => {
            parsed.height = atol(text);
            if parsed.height < 10 {
                return Err("height below 10 pixels".into());
            }
        }
        Opt::OnlyGraph => parsed.only_graph = true,
        Opt::FullSizeMode => parsed.full_size_mode = true,
        Opt::UpperLimit => parsed.upper_limit = Some(float_option("-u", text)?),
        Opt::LowerLimit => parsed.lower_limit = Some(float_option("-l", text)?),
        Opt::Rigid => parsed.rigid = true,
        Opt::AllowShrink => parsed.allow_shrink = true,
        Opt::AltAutoscale => parsed.alt_autoscale = true,
        Opt::AltAutoscaleMin => parsed.alt_autoscale_min = true,
        Opt::AltAutoscaleMax => parsed.alt_autoscale_max = true,
        // Compatibility: `none` hides the grid for good but leaves the
        // label format of an earlier spec in force, and a later spec does
        // not bring the grid back.
        // Provenance: black-box compatibility test (xgridtime-005, -010 to
        // -012).
        Opt::XGrid => match parse_x_grid(text)? {
            None => parsed.hide_x_grid = true,
            Some(format) => parsed.x_label_format = Some(format),
        },
        Opt::YGrid => parsed.y_grid = Some(parse_y_grid(text)?),
        Opt::LeftFormatter => {
            parsed.left_labels.formatter =
                axis_formatter(text).ok_or("Unknown left axis formatter")?;
        }
        Opt::RightFormatter => {
            parsed.right_labels.formatter =
                axis_formatter(text).ok_or("Unknown right axis formatter")?;
        }
        Opt::LeftFormat => parsed.left_labels.format = Some(text.to_owned()),
        Opt::RightFormat => parsed.right_labels.format = Some(text.to_owned()),
        Opt::Logarithmic => parsed.logarithmic = true,
        Opt::UnitsExponent => parsed.units_exponent = Some(atoi(text)),
        Opt::UnitsLength => parsed.units_length = Some(atoi(text)),
        Opt::Units => {
            if text != "si" {
                return Err(format!("invalid argument for --units: {text}").into());
            }
        }
        Opt::RightAxis => parsed.right_axis = Some(parse_right_axis(text)?),
        Opt::RightAxisLabel => parsed.right_axis_label = Some(text.to_owned()),
        Opt::RightAxisRange => check_right_axis_range(text)?,
        Opt::NoLegend => parsed.no_legend = true,
        Opt::ForceRulesLegend => parsed.force_rules_legend = true,
        Opt::LegendPosition => {
            parsed.legend_placement = keyword(
                text,
                &[
                    ("north", LegendPlacement::North),
                    ("south", LegendPlacement::South),
                    ("east", LegendPlacement::East),
                    ("west", LegendPlacement::West),
                ],
            )
            .ok_or_else(|| format!("unknown legend-position '{text}'"))?;
        }
        Opt::LegendDirection => {
            // Compatibility: the direction error names legend-position.
            // Provenance: black-box compatibility test (legend-direction-*).
            parsed.legend_order = keyword(
                text,
                &[
                    ("topdown", LegendOrder::TopDown),
                    ("bottomup", LegendOrder::BottomUp),
                    ("bottomup2", LegendOrder::BottomUpKeepComments),
                ],
            )
            .ok_or_else(|| format!("unknown legend-position '{text}'"))?;
        }
        Opt::Lazy => parsed.lazy = true,
        Opt::Daemon => parsed.daemon = Some(text.to_owned()),
        Opt::ImgInfo => parsed.imginfo = Some(text.to_owned()),
        Opt::Color => {
            let color = parse_color(text)?;
            parsed.colors.push(color);
        }
        Opt::GridDash => parsed.grid_dash = parse_grid_dash(text)?,
        Opt::Border => parsed.border = atoi(text),
        Opt::Zoom => {
            parsed.zoom = float_option("-m", text)?;
            if parsed.zoom <= 0.0 {
                return Err("zoom factor must be > 0".into());
            }
        }
        Opt::Font => {
            let (tag, size) = parse_font(text)?;
            tag.apply(&mut parsed.text_sizes, size);
        }
        Opt::FontRenderMode => {
            if !matches!(text, "normal" | "light" | "mono") {
                return Err(format!("unknown font-render-mode '{text}'").into());
            }
        }
        Opt::GraphRenderMode => {
            if !matches!(text, "normal" | "mono") {
                return Err(format!("unknown graph-render-mode '{text}'").into());
            }
        }
        Opt::ImgFormat => {
            parsed.format = ImageFormat::from_name(text)
                .ok_or_else(|| format!("unsupported graphics format '{text}'"))?;
        }
        Opt::TabWidth => parsed.tab_width = Some(float_option("-T", text)?),
        Opt::Base => {
            parsed.base = match atoi(text) {
                1000 => 1000,
                1024 => 1024,
                _ => return Err("the only sensible value for base apart from 1000 is 1024".into()),
            };
        }
        Opt::Watermark => parsed.watermark = Some(text.to_owned()),
        Opt::MissingAsNan => parsed.missing_as_nan = true,
        Opt::AddJsonTime => parsed.add_jsontime = true,
        Opt::Utc => parsed.utc = true,
        Opt::NoRrdtoolTag => parsed.no_rrdtool_tag = true,
        Opt::GraphType => {
            parsed.xy_graph = match text {
                "TIME" => false,
                "XY" => true,
                _ => return Err(format!("unsupported graphics type '{text}'").into()),
            };
        }
        Opt::FontSmoothing | Opt::Cosmetic | Opt::CosmeticValue | Opt::PangoMarkup => {}
        Opt::MaxRows | Opt::Json | Opt::ShowTime | Opt::EnumDs => {
            unreachable!("xport option in the graph table")
        }
    }
    Ok(())
}

/// Reads an `xport` command line; `args[0]` is the command.
pub(crate) fn parse_xport(args: &[String], now: i64) -> Result<XportArgs, Failure> {
    let mut parsed = XportArgs {
        elements: Vec::new(),
        start: 0,
        end: 0,
        step: 0,
        max_rows: 400,
        daemon: None,
        json: false,
        show_time: false,
        enum_ds: false,
    };
    let mut window = Window::new(now);
    let words = args.get(1..).unwrap_or_default();
    let mut scanner = OptionScanner::new(words, XPORT_TABLE);
    let mut once = OnceOnly::default();
    for found in scanner.by_ref() {
        let found = found?;
        once.see(found.key)?;
        let text = found.text();
        match found.key {
            Opt::Start | Opt::End => window.set(found.key == Opt::Start, text)?,
            Opt::MaxRows => {
                parsed.max_rows = atol(text);
                if parsed.max_rows < 10 {
                    return Err("maxrows below 10 rows".into());
                }
            }
            Opt::Step => parsed.step = i64::from(atoi(text)),
            Opt::Json => parsed.json = true,
            Opt::ShowTime => parsed.show_time = true,
            Opt::EnumDs => parsed.enum_ds = true,
            Opt::Daemon => parsed.daemon = Some(text.to_owned()),
            other => unreachable!("graph option {other:?} in the xport table"),
        }
    }
    (parsed.start, parsed.end) = window.resolve()?;
    parsed.elements = scanner.into_operands();
    Ok(parsed)
}

fn axis_formatter(text: &str) -> Option<AxisFormatter> {
    keyword(
        text,
        &[
            ("numeric", AxisFormatter::Numeric),
            ("timestamp", AxisFormatter::Timestamp),
            ("duration", AxisFormatter::Duration),
        ],
    )
}

/// FS-11: one floating-point conversion with an `l`, literal text and `%%`.
fn check_axis_format(format: &str) -> Result<(), Failure> {
    if format.is_empty() || axis_format_shape(format.as_bytes()) {
        return Ok(());
    }
    Err(format!(
        "invalid format string '{format}' (should match '^(?:[^%]+|%%)*%[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG](?:[^%]+|%%)*$')"
    )
    .into())
}

fn axis_format_shape(bytes: &[u8]) -> bool {
    let mut conversions = 0;
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'%' {
            at += 1;
            continue;
        }
        if bytes.get(at + 1) == Some(&b'%') {
            at += 2;
            continue;
        }
        let Some(end) = conversion_end(bytes, at + 1) else {
            return false;
        };
        conversions += 1;
        at = end;
    }
    conversions == 1
}

/// The end of `[-+ 0#]?[0-9]*(\.[0-9]+)?l[eEfFgG]` starting at `at`.
fn conversion_end(bytes: &[u8], mut at: usize) -> Option<usize> {
    if bytes.get(at).is_some_and(|byte| b"-+ 0#".contains(byte)) {
        at += 1;
    }
    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
        at += 1;
    }
    if bytes.get(at) == Some(&b'.') {
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        at += 1 + digits;
    }
    if bytes.get(at) != Some(&b'l')
        || !bytes
            .get(at + 1)
            .is_some_and(|byte| b"eEfFgG".contains(byte))
    {
        return None;
    }
    Some(at + 2)
}

/// The leading run of characters a decimal number may contain, as the
/// option scanners of GR-23 and GR-24 take it.
fn number_run(text: &str) -> (&str, &str) {
    let length = text
        .bytes()
        .take_while(|byte| byte.is_ascii_digit() || b".e+-".contains(byte))
        .count();
    text.split_at(length)
}

/// C `%d`: optional white space and sign, at least one digit, kept as a
/// 32-bit value. Returns the number and the rest of the text.
fn scan_int(text: &str) -> Option<(i32, &str)> {
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r', '\x0b', '\x0c']);
    let (negative, unsigned) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits = unsigned.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    // Compatibility: the value saturates at 64 bits and is then narrowed
    // to 32, so 2147483648 is negative and 4294967297 is 1.
    // Provenance: black-box compatibility test (scan-071 to -073).
    let magnitude = unsigned[..digits].bytes().fold(0_i64, |total, byte| {
        total
            .saturating_mul(10)
            .saturating_add(i64::from(byte - b'0'))
    });
    let value = if negative { -magnitude } else { magnitude };
    Some((value as i32, &unsigned[digits..]))
}

/// `--x-grid` (GR-24): `None` for `none`, else the label format.
fn parse_x_grid(text: &str) -> Result<Option<String>, Failure> {
    if text == "none" {
        return Ok(None);
    }
    let invalid = || -> Failure { "invalid x-grid format".into() };
    let mut rest = text;
    let mut keywords = Vec::with_capacity(3);
    for _ in 0..3 {
        let letters = rest.bytes().take_while(u8::is_ascii_uppercase).count();
        if !(1..=10).contains(&letters) || rest.as_bytes().get(letters) != Some(&b':') {
            return Err(invalid());
        }
        keywords.push(&rest[..letters]);
        let (_, after) = scan_int(&rest[letters + 1..]).ok_or_else(invalid)?;
        rest = after.strip_prefix(':').ok_or_else(invalid)?;
    }
    let (_, after) = scan_int(rest).ok_or_else(invalid)?;
    let format = after.strip_prefix(':').ok_or_else(invalid)?;
    const UNITS: [&str; 7] = ["SECOND", "MINUTE", "HOUR", "DAY", "WEEK", "MONTH", "YEAR"];
    if let Some(unknown) = keywords.iter().find(|word| !UNITS.contains(word)) {
        return Err(format!("unknown keyword {unknown}").into());
    }
    Ok(Some(format.to_owned()))
}

/// `--y-grid` (GR-24).
fn parse_y_grid(text: &str) -> Result<YGrid, Failure> {
    if text == "none" {
        return Ok(YGrid::Hidden);
    }
    let invalid = || -> Failure { "invalid y-grid format".into() };
    let (run, rest) = number_run(text);
    let after = rest.strip_prefix(':').ok_or_else(invalid)?;
    if run.is_empty() {
        return Err(invalid());
    }
    let (factor, _) = scan_int(after).ok_or_else(invalid)?;
    let step = float_option("-y", run)?;
    if step <= 0.0 {
        return Err("grid step must be > 0".into());
    }
    if factor <= 0 {
        return Err("label factor must be > 0".into());
    }
    Ok(YGrid::Step { step, factor })
}

/// Two `number_run`s around a `:`, trailing text ignored.
fn run_pair(text: &str) -> Option<(&str, &str)> {
    let (first, rest) = number_run(text);
    let rest = rest.strip_prefix(':')?;
    let (second, _) = number_run(rest);
    (!first.is_empty() && !second.is_empty()).then_some((first, second))
}

/// `--right-axis scale:shift` (GR-24).
fn parse_right_axis(text: &str) -> Result<(f64, f64), Failure> {
    let invalid = || -> Failure { "invalid right-axis format expected scale:shift".into() };
    let (scale, shift) = run_pair(text).ok_or_else(invalid)?;
    let scale = rrd_strtodbl(scale, None).map_err(|_| invalid())?;
    let shift = rrd_strtodbl(shift, None).map_err(|_| invalid())?;
    if scale == 0.0 {
        return Err("the second_axis_scale  must not be 0".into());
    }
    Ok((scale, shift))
}

/// `--grid-dash on:off` (GR-24).
fn parse_grid_dash(text: &str) -> Result<(f64, f64), Failure> {
    let (on, off) = run_pair(text).ok_or("invalid grid-dash format")?;
    let convert = |run| {
        rrd_strtodbl(run, None)
            .map_err(|_| -> Failure { "expected grid-dash format float:float".into() })
    };
    Ok((convert(on)?, convert(off)?))
}

/// `--right-axis-range min:max` (GR-24); only pixels depend on the value.
fn check_right_axis_range(text: &str) -> Result<(), Failure> {
    let (low, high) = text
        .split_once(':')
        .ok_or("invalid right-axis-range format expected min:max")?;
    let side = |part: &str| -> Result<Option<f64>, Failure> {
        if part.is_empty() {
            Ok(None)
        } else {
            float_option("--right-axis-range", part).map(Some)
        }
    };
    if let (Some(low), Some(high)) = (side(low)?, side(high)?)
        && low > high
    {
        return Err("right-axis-range min must not be larger than max".into());
    }
    Ok(())
}

/// The `--font` tags (GR-23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontTag {
    Default,
    Title,
    Axis,
    Unit,
    Legend,
    Watermark,
}

impl FontTag {
    fn apply(self, sizes: &mut TextSizes, size: f64) {
        if size <= 0.0 || size.is_nan() {
            return;
        }
        let slots = match self {
            Self::Default => vec![
                &mut sizes.title,
                &mut sizes.axis,
                &mut sizes.unit,
                &mut sizes.legend,
                &mut sizes.watermark,
            ],
            Self::Title => vec![&mut sizes.title],
            Self::Axis => vec![&mut sizes.axis],
            Self::Unit => vec![&mut sizes.unit],
            Self::Legend => vec![&mut sizes.legend],
            Self::Watermark => vec![&mut sizes.watermark],
        };
        for slot in slots {
            *slot = Some(size);
        }
    }
}

/// `--font TAG:size[:font]` (GR-23). The font name only changes pixels.
fn parse_font(text: &str) -> Result<(FontTag, f64), Failure> {
    let invalid = || -> Failure { "invalid text property format".into() };
    let letters = text.bytes().take_while(u8::is_ascii_uppercase).count();
    if !(1..=10).contains(&letters) || text.as_bytes().get(letters) != Some(&b':') {
        return Err(invalid());
    }
    let tag = &text[..letters];
    let (run, rest) = number_run(&text[letters + 1..]);
    if run.is_empty() {
        return Err(invalid());
    }
    let size = rrd_strtodbl(run, None).map_err(|_| invalid())?;
    let tag = keyword(
        tag,
        &[
            ("DEFAULT", FontTag::Default),
            ("TITLE", FontTag::Title),
            ("AXIS", FontTag::Axis),
            ("UNIT", FontTag::Unit),
            ("LEGEND", FontTag::Legend),
            ("WATERMARK", FontTag::Watermark),
        ],
    )
    .ok_or_else(|| format!("invalid fonttag '{tag}'"))?;
    // Compatibility: up to two stray characters after the size are
    // ignored; more must start with the `:` before the font name.
    // Provenance: black-box compatibility test (scan-013 to -020, -028).
    if !rest.starts_with(':') && rest.len() > 2 {
        return Err(format!("expected : after font size in '{text}'").into());
    }
    Ok((tag, size))
}

/// `--color TAG#hex` (GR-22).
fn parse_color(text: &str) -> Result<(ColorTag, [u8; 4]), Failure> {
    let invalid = || -> Failure { "invalid color def format".into() };
    let letters = text.bytes().take_while(u8::is_ascii_uppercase).count();
    if !(1..=10).contains(&letters) || text.as_bytes().get(letters) != Some(&b'#') {
        return Err(invalid());
    }
    let (consumed, value) = scan_hex(&text[letters + 1..]).ok_or_else(invalid)?;
    // Compatibility: the length is checked before the tag is looked up.
    // Provenance: black-box compatibility test (order-005).
    if matches!(consumed, 1 | 2 | 5 | 7) {
        return Err("the color format is #RRGGBB[AA]".into());
    }
    let tag = &text[..letters];
    let tag = keyword(
        tag,
        &[
            ("BACK", ColorTag::Back),
            ("CANVAS", ColorTag::Canvas),
            ("SHADEA", ColorTag::ShadeA),
            ("SHADEB", ColorTag::ShadeB),
            ("GRID", ColorTag::Grid),
            ("MGRID", ColorTag::MajorGrid),
            ("FONT", ColorTag::Font),
            ("AXIS", ColorTag::Axis),
            ("FRAME", ColorTag::Frame),
            ("ARROW", ColorTag::Arrow),
        ],
    )
    .ok_or_else(|| format!("invalid color name '{tag}'"))?;
    let rgba = match consumed {
        3 => expand_nibbles(value, false),
        4 => expand_nibbles(value, true),
        6 => {
            let [_, r, g, b] = (value as u32).to_be_bytes();
            [r, g, b, 0xff]
        }
        _ => (value as u32).to_be_bytes(),
    };
    Ok((tag, rgba))
}

fn expand_nibbles(value: u64, alpha: bool) -> [u8; 4] {
    let nibble = |shift: u32| ((value >> shift) & 0xf) as u8 * 0x11;
    if alpha {
        [nibble(12), nibble(8), nibble(4), nibble(0)]
    } else {
        [nibble(8), nibble(4), nibble(0), 0xff]
    }
}

/// C `%x`: white space, sign, `0x` prefix and hex digits. Returns the
/// number of characters consumed and the value (wrapping), or `None` when
/// no digit was read.
fn scan_hex(text: &str) -> Option<(usize, u64)> {
    let bytes = text.as_bytes();
    let mut at = bytes
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .count();
    let negative = bytes.get(at) == Some(&b'-');
    if matches!(bytes.get(at), Some(b'-' | b'+')) {
        at += 1;
    }
    let digits_from = |start: usize| {
        bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_hexdigit())
            .count()
    };
    if bytes.get(at) == Some(&b'0')
        && matches!(bytes.get(at + 1), Some(b'x' | b'X'))
        && digits_from(at + 2) > 0
    {
        at += 2;
    }
    let digits = digits_from(at);
    if digits == 0 {
        return None;
    }
    let value = text[at..at + digits].bytes().fold(0_u64, |total, byte| {
        let digit = (byte as char).to_digit(16).unwrap_or(0);
        total.wrapping_mul(16).wrapping_add(u64::from(digit))
    });
    let value = if negative {
        value.wrapping_neg()
    } else {
        value
    };
    Some((at + digits, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    const NOW: i64 = 1_000_100_000;

    fn graph(list: &[&str]) -> Result<GraphArgs, String> {
        let mut args = vec!["graph"];
        args.extend_from_slice(list);
        parse_graph(&words(&args), NOW).map_err(|error| error.to_string())
    }

    #[test]
    fn options_fill_the_fields() {
        let parsed = graph(&[
            "out.png",
            "-s",
            "1000000000",
            "-e",
            "1000001200",
            "-w",
            "300",
            "-h",
            "50",
            "-A",
            "--units-length",
            "4",
            "-x",
            "MINUTE:1:MINUTE:5:MINUTE:5:0:%H",
            "-y",
            "2:3",
            "--right-axis",
            "2:5",
            "-n",
            "DEFAULT:9",
            "-n",
            "LEGEND:12",
            "-c",
            "BACK#123",
            "--legend-position",
            "west",
            "--legend-direction",
            "bottomup2",
            "-a",
            "CSV",
            "DEF:x=f.rrd:x:AVERAGE",
        ])
        .unwrap();
        assert_eq!((parsed.start, parsed.end), (1_000_000_000, 1_000_001_200));
        assert_eq!((parsed.width, parsed.height), (300, 50));
        assert!(parsed.alt_autoscale && !parsed.alt_autoscale_min);
        assert_eq!(parsed.units_length, Some(4));
        assert_eq!(parsed.x_label_format.as_deref(), Some("%H"));
        let sticky = graph(&["x", "-x", "MINUTE:1:MINUTE:5:MINUTE:5:0:%M", "-x", "none"]).unwrap();
        assert!(sticky.hide_x_grid);
        assert_eq!(sticky.x_label_format.as_deref(), Some("%M"));
        assert_eq!(
            parsed.y_grid,
            Some(YGrid::Step {
                step: 2.0,
                factor: 3
            })
        );
        assert_eq!(parsed.right_axis, Some((2.0, 5.0)));
        assert_eq!(parsed.text_sizes.axis, Some(9.0));
        assert_eq!(parsed.text_sizes.legend, Some(12.0));
        assert_eq!(
            parsed.colors,
            vec![(ColorTag::Back, [0x11, 0x22, 0x33, 0xff])]
        );
        assert_eq!(parsed.legend_placement, LegendPlacement::West);
        assert_eq!(parsed.legend_order, LegendOrder::BottomUpKeepComments);
        assert_eq!(parsed.format, ImageFormat::Data(DataFormat::Csv));
        assert_eq!(parsed.output, "out.png");
        assert_eq!(parsed.elements, words(&["DEF:x=f.rrd:x:AVERAGE"]));
    }

    #[test]
    fn deferred_checks_follow_gr17() {
        assert_eq!(
            graph(&["x", "--start", "abc", "-w", "5"]).unwrap_err(),
            "start time: unparsable time: abc"
        );
        assert_eq!(
            graph(&["x", "-w", "5", "--start", "abc"]).unwrap_err(),
            "width below 10 pixels"
        );
        assert!(
            graph(&[
                "--left-axis-format",
                "%d",
                "--start",
                "end-1h",
                "--end",
                "start+1h"
            ])
            .unwrap_err()
            .starts_with("invalid format string '%d'")
        );
        assert_eq!(
            graph(&["--start", "end-1h", "--end", "start+1h"]).unwrap_err(),
            "the start and end times cannot be specified relative to each other"
        );
        assert_eq!(
            graph(&["-e", "1000000000", "-s", "end-25y"]).unwrap_err(),
            "the first entry to fetch should be after 1980 (211081600)"
        );
        assert_eq!(
            graph(&["-s", "1000000001", "-e", "1000000000"]).unwrap_err(),
            "start (1000000001) should be less than end (1000000000)"
        );
        assert_eq!(
            graph(&["-s", "1000000000"]).unwrap_err(),
            "missing filename"
        );
        assert_eq!(
            graph(&["-l", "0", "-o", "--start", "end-1h", "--end", "start+1h"]).unwrap_err(),
            "for a logarithmic yaxis you must specify a lower-limit > 0"
        );
        assert!(graph(&["x", "-o", "-l", "nan"]).is_ok());
        let timestamp = graph(&[
            "x",
            "--left-axis-format",
            "%d",
            "--left-axis-formatter",
            "timestamp",
        ]);
        assert!(timestamp.is_ok());
    }

    #[test]
    fn duration_formats_are_kept_for_later() {
        let parsed = graph(&[
            "x",
            "--left-axis-formatter",
            "duration",
            "--left-axis-format",
            "%lf",
        ])
        .unwrap();
        assert!(parsed.unusable_duration_format());
        let hidden = graph(&[
            "x",
            "--left-axis-formatter",
            "duration",
            "--left-axis-format",
            "%lf",
            "-y",
            "none",
        ])
        .unwrap();
        assert!(!hidden.unusable_duration_format());
        let right = graph(&[
            "x",
            "--right-axis-formatter",
            "duration",
            "--right-axis-format",
            "%lf",
        ])
        .unwrap();
        assert!(!right.unusable_duration_format());
        let fine = graph(&[
            "x",
            "--left-axis-formatter",
            "duration",
            "--left-axis-format",
            "%H",
        ])
        .unwrap();
        assert!(!fine.unusable_duration_format());
    }

    #[test]
    fn keyword_and_number_errors() {
        let cases = [
            (vec!["-m", "0"], "zoom factor must be > 0"),
            (
                vec!["-b", "1e3"],
                "the only sensible value for base apart from 1000 is 1024",
            ),
            (vec!["--units", "SI"], "invalid argument for --units: SI"),
            (vec!["-a", "GIF"], "unsupported graphics format 'GIF'"),
            (vec!["-R", "x"], "unknown font-render-mode 'x'"),
            (vec!["-G", "x"], "unknown graph-render-mode 'x'"),
            (
                vec!["--legend-position", "up"],
                "unknown legend-position 'up'",
            ),
            (
                vec!["--legend-direction", "up"],
                "unknown legend-position 'up'",
            ),
            (
                vec!["--left-axis-formatter", "x"],
                "Unknown left axis formatter",
            ),
            (
                vec!["--right-axis-formatter", "x"],
                "Unknown right axis formatter",
            ),
            (
                vec!["-u", "5x"],
                "option -u - Converted '5x' to 5.000000, but cannot convert 'x'",
            ),
            (vec!["-T", ""], "option -T - Cannot convert '' to float"),
            (
                vec!["--right-axis-label-angle", "a"],
                "option --right-axis-label-angle - Cannot convert 'a' to float",
            ),
            (vec!["-h", "9"], "height below 10 pixels"),
        ];
        for (options, message) in cases {
            let mut list = vec!["x"];
            list.extend(options);
            assert_eq!(graph(&list).unwrap_err(), message, "{list:?}");
        }
        let parsed = graph(&["x", "-b", "1024x", "--step", "1m", "--border", "-1"]).unwrap();
        assert_eq!((parsed.base, parsed.step, parsed.border), (1024, 1, -1));
    }

    #[test]
    fn single_use_options_and_graph_type() {
        assert_eq!(
            graph(&["x", "--units", "si", "--units", "si", "-w", "5"]).unwrap_err(),
            "--units can only be used once!"
        );
        assert_eq!(
            graph(&["x", "--add-jsontime", "--add-jsontime"]).unwrap_err(),
            "--add-jsontime can only be used once!"
        );
        assert_eq!(
            graph(&["x", "-d", "", "--daemon", "b"]).unwrap_err(),
            "You cannot specify --daemon more than once."
        );
        assert!(graph(&["x", "--lazy", "--lazy", "-I"]).is_ok());
        assert!(graph(&["x", "--graph-type", "XY"]).unwrap().xy_graph);
        assert!(
            !graph(&["x", "--graph-type", "XY", "--graph-type", "TIME"])
                .unwrap()
                .xy_graph
        );
        assert_eq!(
            graph(&["x", "--graph-type", "time"]).unwrap_err(),
            "unsupported graphics type 'time'"
        );
    }

    #[test]
    fn font_values() {
        assert_eq!(parse_font("DEFAULT:10").unwrap(), (FontTag::Default, 10.0));
        assert_eq!(
            parse_font("TITLE:1e1:Times").unwrap(),
            (FontTag::Title, 10.0)
        );
        assert_eq!(parse_font("LEGEND:1;:").unwrap(), (FontTag::Legend, 1.0));
        for bad in [
            "DEFAULT",
            "DEFAULT:",
            "DEFAULT: 10",
            "DEFAULT:nan",
            "DEFAULT:1.2.3",
            "ABCDEFGHIJK:1",
            ":1",
        ] {
            assert_eq!(
                parse_font(bad).unwrap_err().to_string(),
                "invalid text property format",
                "{bad}"
            );
        }
        assert_eq!(
            parse_font("DEFAULT:0x10").unwrap_err().to_string(),
            "expected : after font size in 'DEFAULT:0x10'"
        );
        assert_eq!(
            parse_font("BOGUS:10").unwrap_err().to_string(),
            "invalid fonttag 'BOGUS'"
        );
        assert_eq!(
            parse_font("BOGUS:10yyy").unwrap_err().to_string(),
            "invalid fonttag 'BOGUS'"
        );
        let mut sizes = TextSizes::default();
        FontTag::Legend.apply(&mut sizes, 12.0);
        FontTag::Default.apply(&mut sizes, -1.0);
        assert_eq!(sizes.legend, Some(12.0));
        FontTag::Default.apply(&mut sizes, 8.0);
        assert_eq!(
            sizes,
            TextSizes {
                title: Some(8.0),
                axis: Some(8.0),
                unit: Some(8.0),
                legend: Some(8.0),
                watermark: Some(8.0),
            }
        );
    }

    #[test]
    fn color_values() {
        assert_eq!(
            parse_color("FRAME#10203040").unwrap(),
            (ColorTag::Frame, [0x10, 0x20, 0x30, 0x40])
        );
        assert_eq!(
            parse_color("GRID#1234").unwrap(),
            (ColorTag::Grid, [0x11, 0x22, 0x33, 0x44])
        );
        assert!(parse_color("BACK#102030405").is_ok());
        assert!(parse_color("BACK# 123").is_ok());
        assert!(parse_color("BACK#-10203").is_ok());
        assert!(parse_color("BACK#0X1").is_ok());
        for bad in ["BACK#10203", "BACK#-1020", "BACK#0x", "BACK#12"] {
            assert_eq!(
                parse_color(bad).unwrap_err().to_string(),
                "the color format is #RRGGBB[AA]",
                "{bad}"
            );
        }
        for bad in [
            "BACK#gg",
            "BACK#",
            "BACK",
            "back#123",
            "ABCDEFGHIJK#123",
            "BACK#--1",
        ] {
            assert_eq!(
                parse_color(bad).unwrap_err().to_string(),
                "invalid color def format",
                "{bad}"
            );
        }
        assert_eq!(
            parse_color("ABCDEFGHIJ#123").unwrap_err().to_string(),
            "invalid color name 'ABCDEFGHIJ'"
        );
        assert_eq!(
            parse_color("X#1").unwrap_err().to_string(),
            "the color format is #RRGGBB[AA]"
        );
    }

    #[test]
    fn grid_values() {
        assert_eq!(parse_x_grid("none").unwrap(), None);
        assert_eq!(
            parse_x_grid("MINUTE: +1:HOUR:5:DAY:5:-5:%H:%M")
                .unwrap()
                .as_deref(),
            Some("%H:%M")
        );
        assert_eq!(
            parse_x_grid("MINUTE:1:FOO:5:BAR:5:0:x")
                .unwrap_err()
                .to_string(),
            "unknown keyword FOO"
        );
        for bad in [
            "NONE",
            "MINUTE:1.5:MINUTE:5:MINUTE:5:0:%H",
            "MINUTE:1:MINUTE:5:MINUTE:5:0",
            "",
        ] {
            assert_eq!(
                parse_x_grid(bad).unwrap_err().to_string(),
                "invalid x-grid format",
                "{bad}"
            );
        }
        assert_eq!(
            parse_y_grid("1: 2x").unwrap(),
            YGrid::Step {
                step: 1.0,
                factor: 2
            }
        );
        assert_eq!(
            parse_y_grid("1:4294967297").unwrap(),
            YGrid::Step {
                step: 1.0,
                factor: 1
            }
        );
        assert_eq!(
            parse_y_grid("1:2147483648").unwrap_err().to_string(),
            "label factor must be > 0"
        );
        assert_eq!(
            parse_y_grid("0:1").unwrap_err().to_string(),
            "grid step must be > 0"
        );
        assert_eq!(
            parse_y_grid("1-:2").unwrap_err().to_string(),
            "option -y - Converted '1-' to 1.000000, but cannot convert '-'"
        );
        for bad in [" 1:2", "1E1:2", "1:", "nan:1", "1"] {
            assert_eq!(
                parse_y_grid(bad).unwrap_err().to_string(),
                "invalid y-grid format",
                "{bad}"
            );
        }
        assert_eq!(parse_right_axis("1:0x").unwrap(), (1.0, 0.0));
        assert_eq!(
            parse_right_axis("0:1").unwrap_err().to_string(),
            "the second_axis_scale  must not be 0"
        );
        assert!(parse_right_axis("1:0-").is_err());
        assert_eq!(parse_grid_dash("1:3:5").unwrap(), (1.0, 3.0));
        assert_eq!(
            parse_grid_dash("1-:2").unwrap_err().to_string(),
            "expected grid-dash format float:float"
        );
        assert_eq!(
            parse_grid_dash("1").unwrap_err().to_string(),
            "invalid grid-dash format"
        );
        assert!(check_right_axis_range(":").is_ok());
        assert_eq!(
            check_right_axis_range("2:1").unwrap_err().to_string(),
            "right-axis-range min must not be larger than max"
        );
        assert_eq!(
            check_right_axis_range("1").unwrap_err().to_string(),
            "invalid right-axis-range format expected min:max"
        );
    }

    #[test]
    fn axis_format_shapes() {
        for good in ["%lf", "%.2lf", "%10.3le", "%lf %%", "x%+5lGy", ""] {
            assert!(check_axis_format(good).is_ok(), "{good}");
        }
        for bad in ["%d", "%s", "%lf%lf", "abc", "%%", "%.lf", "%5.1lf %s", "%l"] {
            assert!(check_axis_format(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn xport_options() {
        let parse = |list: &[&str]| {
            let mut args = vec!["xport"];
            args.extend_from_slice(list);
            parse_xport(&words(&args), NOW).map_err(|error| error.to_string())
        };
        let parsed = parse(&[
            "-s",
            "1000000000",
            "-e",
            "1000001200",
            "-m",
            "50",
            "--step",
            "120",
            "--json",
            "-t",
            "--enumds",
            "-d",
            "",
            "XPORT:x",
        ])
        .unwrap();
        assert_eq!((parsed.max_rows, parsed.step), (50, 120));
        assert!(parsed.json && parsed.show_time && parsed.enum_ds);
        assert_eq!(parsed.daemon.as_deref(), Some(""));
        assert_eq!(parsed.elements, words(&["XPORT:x"]));
        assert_eq!(parse(&["-m", "abc"]).unwrap_err(), "maxrows below 10 rows");
        assert_eq!(parse(&["-w", "5"]).unwrap_err(), "invalid option -- 'w'");
        assert_eq!(
            parse(&["--json=1"]).unwrap_err(),
            "option takes no arguments -- 'json'"
        );
        assert_eq!(
            parse(&["-d", "a", "-d", "b", "-m", "5"]).unwrap_err(),
            "You cannot specify --daemon more than once."
        );
    }
}
