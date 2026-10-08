use super::optparse::{ArgType, DONE, ERROR, LongOpt, OptParse, opt};
use rondi::time::{TimeValue, rrd_parsetime, rrd_proc_start_end};
use std::ffi::{CStr, CString};

const LONGOPT_UNITS_SI: i32 = 255;
const LONGOPT_ADD_JSONTIME: i32 = 254;

const fn short(c: u8) -> i32 {
    c as i32
}

/// rrd_graph.c:5063.
const GRAPH_LONGOPTS: &[LongOpt] = &[
    opt("alt-autoscale", short(b'A'), ArgType::None),
    opt("imgformat", short(b'a'), ArgType::Required),
    opt("font-smoothing-threshold", short(b'B'), ArgType::Required),
    opt("base", short(b'b'), ArgType::Required),
    opt("color", short(b'c'), ArgType::Required),
    opt("full-size-mode", short(b'D'), ArgType::None),
    opt("daemon", short(b'd'), ArgType::Required),
    opt("slope-mode", short(b'E'), ArgType::None),
    opt("end", short(b'e'), ArgType::Required),
    opt("force-rules-legend", short(b'F'), ArgType::None),
    opt("imginfo", short(b'f'), ArgType::Required),
    opt("graph-render-mode", short(b'G'), ArgType::Required),
    opt("no-legend", short(b'g'), ArgType::None),
    opt("height", short(b'h'), ArgType::Required),
    opt("no-minor", short(b'I'), ArgType::None),
    opt("interlaced", short(b'i'), ArgType::None),
    opt("alt-autoscale-min", short(b'J'), ArgType::None),
    opt("only-graph", short(b'j'), ArgType::None),
    opt("units-length", short(b'L'), ArgType::Required),
    opt("lower-limit", short(b'l'), ArgType::Required),
    opt("alt-autoscale-max", short(b'M'), ArgType::None),
    opt("zoom", short(b'm'), ArgType::Required),
    opt("no-gridfit", short(b'N'), ArgType::None),
    opt("font", short(b'n'), ArgType::Required),
    opt("logarithmic", short(b'o'), ArgType::None),
    opt("pango-markup", short(b'P'), ArgType::None),
    opt("font-render-mode", short(b'R'), ArgType::Required),
    opt("rigid", short(b'r'), ArgType::None),
    opt("step", short(b'S'), ArgType::Required),
    opt("start", short(b's'), ArgType::Required),
    opt("tabwidth", short(b'T'), ArgType::Required),
    opt("title", short(b't'), ArgType::Required),
    opt("upper-limit", short(b'u'), ArgType::Required),
    opt("vertical-label", short(b'v'), ArgType::Required),
    opt("watermark", short(b'W'), ArgType::Required),
    opt("width", short(b'w'), ArgType::Required),
    opt("units-exponent", short(b'X'), ArgType::Required),
    opt("x-grid", short(b'x'), ArgType::Required),
    opt("alt-y-grid", short(b'Y'), ArgType::None),
    opt("y-grid", short(b'y'), ArgType::Required),
    opt("lazy", short(b'z'), ArgType::None),
    opt("use-nan-for-all-missing-data", short(b'Z'), ArgType::None),
    opt("units", LONGOPT_UNITS_SI, ArgType::Required),
    opt("add-jsontime", LONGOPT_ADD_JSONTIME, ArgType::None),
    opt("alt-y-mrtg", 1000, ArgType::None),
    opt("disable-rrdtool-tag", 1001, ArgType::None),
    opt("right-axis", 1002, ArgType::Required),
    opt("right-axis-label", 1003, ArgType::Required),
    opt("right-axis-format", 1004, ArgType::Required),
    opt("legend-position", 1005, ArgType::Required),
    opt("legend-direction", 1006, ArgType::Required),
    opt("border", 1007, ArgType::Required),
    opt("grid-dash", 1008, ArgType::Required),
    opt("dynamic-labels", 1009, ArgType::None),
    opt("week-fmt", 1010, ArgType::Required),
    opt("graph-type", 1011, ArgType::Required),
    opt("left-axis-format", 1012, ArgType::Required),
    opt("left-axis-formatter", 1013, ArgType::Required),
    opt("right-axis-formatter", 1014, ArgType::Required),
    opt("allow-shrink", 1015, ArgType::None),
    opt("utc", 1016, ArgType::None),
    opt("vertical-label-angle", 1017, ArgType::Required),
    opt("right-axis-label-angle", 1018, ArgType::Required),
    opt("right-axis-range", 1019, ArgType::Required),
];

/// rrd_xport.c:98.
const XPORT_LONGOPTS: &[LongOpt] = &[
    opt("start", short(b's'), ArgType::Required),
    opt("end", short(b'e'), ArgType::Required),
    opt("maxrows", short(b'm'), ArgType::Required),
    opt("step", short(b'S'), ArgType::Required),
    opt("enumds", 262, ArgType::None),
    opt("json", 263, ArgType::None),
    opt("showtime", short(b't'), ArgType::None),
    opt("daemon", short(b'd'), ArgType::Required),
];

/// The settings `rrd_graph_options` leaves in `image_desc_t` that Rondi's
/// renderer and export path read.
pub(crate) struct GraphOptions {
    pub(crate) imgformat: String,
    pub(crate) colors: super::GraphColors,
    pub(crate) imginfo: Option<String>,
    pub(crate) base: u32,
    pub(crate) daemon: Option<String>,
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) step: Option<i32>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) title: Option<String>,
    pub(crate) vertical_label: Option<String>,
    pub(crate) vertical_label_angle: f64,
    pub(crate) lower_limit: Option<f64>,
    pub(crate) upper_limit: Option<f64>,
    pub(crate) no_legend: bool,
    pub(crate) rigid: bool,
    pub(crate) allow_shrink: bool,
    pub(crate) alt_autoscale: bool,
    pub(crate) alt_autoscale_min: bool,
    pub(crate) alt_autoscale_max: bool,
    pub(crate) only_graph: bool,
    pub(crate) full_size_mode: bool,
    pub(crate) force_rules_legend: bool,
    pub(crate) legend_direction: rondi::graph_layout::LegendDirection,
    pub(crate) grid_dash: Vec<f64>,
    pub(crate) border: u32,
    /// Settings `graph_size_location` and `leg_place` read.
    pub(crate) layout: GraphLayout,
    /// `argv[optind..]`: the image filename followed by the script.
    pub(crate) positionals: Vec<String>,
}

/// The `image_desc_t` layout fields the options set, applied to the
/// prepared image. `None` keeps `rrd_graph_init`'s value.
#[derive(Default)]
pub(crate) struct GraphLayout {
    watermark: Option<String>,
    second_axis_legend: Option<String>,
    second_axis_scale: Option<f64>,
    no_x_grid: bool,
    no_y_grid: bool,
    units_length: Option<i32>,
    units_exponent: Option<i32>,
    no_rrdtool_tag: bool,
    legend_position: Option<rondi::graph_layout::LegendPosition>,
    tabwidth: Option<f64>,
    text_sizes: Vec<(usize, f64)>,
    y_grid: Option<(f64, i32)>,
    logarithmic: bool,
    allow_missing_ds: bool,
}

impl GraphLayout {
    pub(crate) fn apply(&self, im: &mut rondi::graph::GraphImage) {
        im.watermark.clone_from(&self.watermark);
        im.second_axis_legend.clone_from(&self.second_axis_legend);
        if let Some(scale) = self.second_axis_scale {
            im.second_axis_scale = scale;
        }
        if self.no_x_grid {
            im.draw_x_grid = false;
        }
        if self.no_y_grid {
            im.draw_y_grid = false;
        }
        if let Some(length) = self.units_length {
            im.unitslength = length;
            im.forceleftspace = true;
        }
        if let Some(exponent) = self.units_exponent {
            im.unitsexponent = exponent;
        }
        if self.no_rrdtool_tag {
            im.extra_flags |= rondi::graph_layout::NO_RRDTOOL_TAG;
        }
        if let Some(position) = self.legend_position {
            im.legendposition = position;
        }
        if let Some(tabwidth) = self.tabwidth {
            im.tabwidth = tabwidth;
        }
        for &(index, size) in &self.text_sizes {
            im.text_prop[index] = size;
        }
        if let Some((step, factor)) = self.y_grid {
            im.ygridstep = step;
            im.ylabfact = factor;
        }
        if self.logarithmic {
            im.logarithmic = true;
        }
        if self.allow_missing_ds {
            im.allow_missing_ds = true;
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AxisFormatter {
    Numeric,
    Other,
}

pub(crate) fn rrd_graph_options(args: &[String], now: i64) -> Result<GraphOptions, String> {
    let mut graph = GraphOptions {
        imgformat: String::from("PNG"),
        colors: super::GraphColors::default(),
        imginfo: None,
        base: 1000,
        daemon: None,
        start: 0,
        end: 0,
        step: None,
        width: 400,
        height: 100,
        title: None,
        vertical_label: None,
        vertical_label_angle: 90.0,
        lower_limit: None,
        upper_limit: None,
        no_legend: false,
        rigid: false,
        allow_shrink: false,
        alt_autoscale: false,
        alt_autoscale_min: false,
        alt_autoscale_max: false,
        only_graph: false,
        full_size_mode: false,
        force_rules_legend: false,
        legend_direction: rondi::graph_layout::LegendDirection::TopDown,
        grid_dash: Vec::new(),
        border: 2,
        layout: GraphLayout::default(),
        positionals: Vec::new(),
    };
    let mut units_si = false;
    let mut jsontime = false;
    let mut logarithmic = false;
    let mut minval = f64::NAN;
    let mut primary_axis_format = None::<String>;
    let mut primary_axis_formatter = AxisFormatter::Numeric;
    let mut second_axis_format = None::<String>;
    let mut second_axis_formatter = AxisFormatter::Numeric;
    let mut start_tv = rrd_parsetime("end-24h", now)?;
    let mut end_tv = rrd_parsetime("now", now)?;
    let mut options = OptParse::new(args.to_vec());
    loop {
        let option = options.long(GRAPH_LONGOPTS);
        if option == DONE {
            break;
        }
        let value = options.value().to_owned();
        match option {
            1005 => {
                use rondi::graph_layout::LegendPosition;
                graph.layout.legend_position = Some(match value.as_str() {
                    "north" => LegendPosition::North,
                    "west" => LegendPosition::West,
                    "south" => LegendPosition::South,
                    "east" => LegendPosition::East,
                    _ => return Err(format!("unknown legend-position '{value}'")),
                });
            }
            1006 => match value.as_str() {
                "topdown" => {
                    graph.legend_direction = rondi::graph_layout::LegendDirection::TopDown;
                }
                "bottomup" => {
                    graph.legend_direction = rondi::graph_layout::LegendDirection::BottomUp;
                }
                "bottomup2" => {
                    graph.legend_direction = rondi::graph_layout::LegendDirection::BottomUp2;
                }
                _ => return Err(format!("unknown legend-position '{value}'")),
            },
            LONGOPT_UNITS_SI => {
                if units_si {
                    return Err(String::from("--units can only be used once!"));
                }
                if value != "si" {
                    return Err(format!("invalid argument for --units: {value}"));
                }
                units_si = true;
            }
            LONGOPT_ADD_JSONTIME => {
                if jsontime {
                    return Err(String::from("--add-jsontime can only be used once!"));
                }
                jsontime = true;
            }
            _ if option == short(b'T') => {
                graph.layout.tabwidth = Some(rrd_strtodbl(&value, Some("option -T"))?);
            }
            _ if option == short(b'S') => graph.step = Some(atoi(&value)),
            _ if option == short(b's') => {
                start_tv =
                    rrd_parsetime(&value, now).map_err(|error| format!("start time: {error}"))?;
            }
            _ if option == short(b'e') => {
                end_tv =
                    rrd_parsetime(&value, now).map_err(|error| format!("end time: {error}"))?;
            }
            _ if option == short(b'x') => {
                if value == "none" {
                    graph.layout.no_x_grid = true;
                } else {
                    parse_x_grid(&value)?;
                }
            }
            _ if option == short(b'y') => {
                if value == "none" {
                    graph.layout.no_y_grid = true;
                } else {
                    graph.layout.y_grid = Some(parse_y_grid(&value)?);
                }
            }
            1008 => {
                let Some((on, off)) = scan_two_doubles(&value) else {
                    return Err(String::from("invalid grid-dash format"));
                };
                match (rrd_strtodbl(&on, None), rrd_strtodbl(&off, None)) {
                    (Ok(on), Ok(off)) => graph.grid_dash = vec![on, off],
                    _ => return Err(String::from("expected grid-dash format float:float")),
                }
            }
            1002 => {
                let scale = scan_two_doubles(&value).and_then(|(scale, shift)| {
                    let scale = rrd_strtodbl(&scale, None).ok()?;
                    rrd_strtodbl(&shift, None).ok()?;
                    Some(scale)
                });
                match scale {
                    Some(0.0) => {
                        return Err(String::from("the second_axis_scale  must not be 0"));
                    }
                    Some(scale) => graph.layout.second_axis_scale = Some(scale),
                    None => {
                        return Err(String::from(
                            "invalid right-axis format expected scale:shift",
                        ));
                    }
                }
            }
            1004 => second_axis_format = Some(value),
            1012 => primary_axis_format = Some(value),
            1013 | 1014 => {
                let formatter = match value.as_str() {
                    "numeric" => AxisFormatter::Numeric,
                    "timestamp" | "duration" => AxisFormatter::Other,
                    _ if option == 1013 => {
                        return Err(String::from("Unknown left axis formatter"));
                    }
                    _ => return Err(String::from("Unknown right axis formatter")),
                };
                if option == 1013 {
                    primary_axis_formatter = formatter;
                } else {
                    second_axis_formatter = formatter;
                }
            }
            1017 => {
                let angle = rrd_strtodbl(&value, Some("option --vertical-label-angle"))?;
                // The renderer has no meaning for a non-finite rotation.
                graph.vertical_label_angle = if angle.is_finite() { angle } else { 90.0 };
            }
            1018 => {
                rrd_strtodbl(&value, Some("option --right-axis-label-angle"))?;
            }
            1019 => parse_right_axis_range(&value)?,
            _ if option == short(b'v') => graph.vertical_label = Some(value),
            _ if option == short(b'u') => {
                let maxval = rrd_strtodbl(&value, Some("option -u"))?;
                graph.upper_limit = maxval.is_finite().then_some(maxval);
            }
            _ if option == short(b'l') => {
                minval = rrd_strtodbl(&value, Some("option -l"))?;
                graph.lower_limit = minval.is_finite().then_some(minval);
            }
            _ if option == short(b'b') => {
                let base = atol(&value);
                if base != 1024 && base != 1000 {
                    return Err(String::from(
                        "the only sensible value for base apart from 1000 is 1024",
                    ));
                }
                graph.base = base as u32;
            }
            _ if option == short(b'w') || option == short(b'h') => {
                let pixels = atol(&value);
                if pixels < 10 {
                    return Err(String::from(if option == short(b'w') {
                        "width below 10 pixels"
                    } else {
                        "height below 10 pixels"
                    }));
                }
                let pixels = u32::try_from(pixels).unwrap_or(u32::MAX);
                if option == short(b'w') {
                    graph.width = pixels;
                } else {
                    graph.height = pixels;
                }
            }
            _ if option == short(b'f') => graph.imginfo = Some(value),
            _ if option == short(b'a') => {
                if !matches!(
                    value.as_str(),
                    "PNG"
                        | "SVG"
                        | "EPS"
                        | "PDF"
                        | "XML"
                        | "XMLENUM"
                        | "CSV"
                        | "TSV"
                        | "SSV"
                        | "JSON"
                        | "JSONTIME"
                ) {
                    return Err(format!("unsupported graphics format '{value}'"));
                }
                graph.imgformat = value;
            }
            1011 => {
                if !matches!(value.as_str(), "TIME" | "XY") {
                    return Err(format!("unsupported graphics type '{value}'"));
                }
            }
            1007 => graph.border = atoi(&value).max(0) as u32,
            _ if option == short(b'o') => {
                logarithmic = true;
                graph.layout.logarithmic = true;
            }
            _ if option == short(b'c') => parse_color(&value, &mut graph.colors)?,
            _ if option == short(b'n') => {
                let (index, size) = parse_font(&value)?;
                // Only DEFAULT carries on to the later properties.
                for property in index..6 {
                    if size > 0.0 {
                        graph.layout.text_sizes.push((property, size));
                    }
                    if property == index && index != 0 {
                        break;
                    }
                }
            }
            _ if option == short(b'm') => {
                let zoom = rrd_strtodbl(&value, Some("option -m"))?;
                if zoom <= 0.0 {
                    return Err(String::from("zoom factor must be > 0"));
                }
            }
            _ if option == short(b't') => graph.title = Some(value),
            _ if option == short(b'R') => {
                if !matches!(value.as_str(), "normal" | "light" | "mono") {
                    return Err(format!("unknown font-render-mode '{value}'"));
                }
            }
            _ if option == short(b'G') => {
                if !matches!(value.as_str(), "normal" | "mono") {
                    return Err(format!("unknown graph-render-mode '{value}'"));
                }
            }
            _ if option == short(b'd') => {
                if graph.daemon.is_some() {
                    return Err(String::from("You cannot specify --daemon more than once."));
                }
                graph.daemon = Some(value);
            }
            _ if option == short(b'g') => graph.no_legend = true,
            _ if option == short(b'j') => graph.only_graph = true,
            _ if option == short(b'D') => graph.full_size_mode = true,
            _ if option == short(b'F') => graph.force_rules_legend = true,
            _ if option == short(b'r') => graph.rigid = true,
            _ if option == short(b'A') => graph.alt_autoscale = true,
            _ if option == short(b'J') => graph.alt_autoscale_min = true,
            _ if option == short(b'M') => graph.alt_autoscale_max = true,
            1015 => graph.allow_shrink = true,
            _ if option == short(b'X') => graph.layout.units_exponent = Some(atoi(&value)),
            _ if option == short(b'L') => graph.layout.units_length = Some(atoi(&value)),
            _ if option == short(b'W') => graph.layout.watermark = Some(value),
            _ if option == short(b'Z') => graph.layout.allow_missing_ds = true,
            1001 => graph.layout.no_rrdtool_tag = true,
            1003 => graph.layout.second_axis_legend = Some(value),
            ERROR => return Err(options.errmsg),
            // -I -Y -N -P -E -z -i -B, 1009, 1010, 1016 and --alt-y-mrtg
            // only change Cairo/Pango rendering.
            _ => {}
        }
    }
    for (format, formatter) in [
        (primary_axis_format, primary_axis_formatter),
        (second_axis_format, second_axis_formatter),
    ] {
        if let Some(format) = format.filter(|format| !format.is_empty())
            && formatter == AxisFormatter::Numeric
        {
            bad_format_axis(&format)?;
        }
    }
    if logarithmic && minval <= 0.0 {
        return Err(String::from(
            "for a logarithmic yaxis you must specify a lower-limit > 0",
        ));
    }
    (graph.start, graph.end) = proc_start_end(&mut start_tv, &mut end_tv)?;
    graph.positionals = options.positionals().to_vec();
    Ok(graph)
}

/// `rrd_xport`'s option state before `rrd_graph_script`.
pub(crate) struct XportOptions {
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) step: i32,
    pub(crate) maxrows: i64,
    pub(crate) daemon: Option<String>,
    pub(crate) json: bool,
    pub(crate) showtime: bool,
    pub(crate) enumds: bool,
    pub(crate) positionals: Vec<String>,
}

pub(crate) fn rrd_xport_options(args: &[String], now: i64) -> Result<XportOptions, String> {
    let mut xport = XportOptions {
        start: 0,
        end: 0,
        step: 0,
        maxrows: 400,
        daemon: None,
        json: false,
        showtime: false,
        enumds: false,
        positionals: Vec::new(),
    };
    let mut start_tv = rrd_parsetime("end-24h", now)?;
    let mut end_tv = rrd_parsetime("now", now)?;
    let mut options = OptParse::new(args.to_vec());
    loop {
        let option = options.long(XPORT_LONGOPTS);
        if option == DONE {
            break;
        }
        let value = options.value().to_owned();
        match option {
            262 => xport.enumds = true,
            263 => xport.json = true,
            ERROR => return Err(options.errmsg),
            _ if option == short(b'S') => xport.step = atoi(&value),
            _ if option == short(b't') => xport.showtime = true,
            _ if option == short(b's') => {
                start_tv =
                    rrd_parsetime(&value, now).map_err(|error| format!("start time: {error}"))?;
            }
            _ if option == short(b'e') => {
                end_tv =
                    rrd_parsetime(&value, now).map_err(|error| format!("end time: {error}"))?;
            }
            _ if option == short(b'm') => {
                xport.maxrows = atol(&value);
                if xport.maxrows < 10 {
                    return Err(String::from("maxrows below 10 rows"));
                }
            }
            _ if option == short(b'd') => {
                if xport.daemon.is_some() {
                    return Err(String::from("You cannot specify --daemon more than once."));
                }
                xport.daemon = Some(value);
            }
            _ => {}
        }
    }
    (xport.start, xport.end) = proc_start_end(&mut start_tv, &mut end_tv)?;
    xport.positionals = options.positionals().to_vec();
    Ok(xport)
}

/// `rrd_proc_start_end` plus the range checks both callers repeat.
fn proc_start_end(start_tv: &mut TimeValue, end_tv: &mut TimeValue) -> Result<(i64, i64), String> {
    let (start, end) = rrd_proc_start_end(start_tv, end_tv)?;
    if start < 3600 * 24 * 365 * 10 {
        return Err(format!(
            "the first entry to fetch should be after 1980 ({start})"
        ));
    }
    if end < start {
        return Err(format!("start ({start}) should be less than end ({end})"));
    }
    Ok((start, end))
}

fn c_string(value: &str) -> CString {
    // Arguments come from argv or a pipe-mode line, neither of which can
    // carry a NUL; C would stop reading there.
    CString::new(value.split('\0').next().unwrap_or_default()).unwrap_or_default()
}

pub(crate) fn atoi(value: &str) -> i32 {
    let value = c_string(value);
    // SAFETY: `value` is a valid NUL-terminated string.
    unsafe { libc::atoi(value.as_ptr()) }
}

// `long` is 32 bits on some targets.
#[allow(clippy::useless_conversion)]
pub(crate) fn atol(value: &str) -> i64 {
    let value = c_string(value);
    // SAFETY: `value` is a valid NUL-terminated string.
    i64::from(unsafe { libc::atol(value.as_ptr()) })
}

fn buffer_text(buffer: &[u8]) -> String {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// `sscanf(arg, "%40[0-9.e+-]:%40[0-9.e+-]", ...) == 2`.
fn scan_two_doubles(value: &str) -> Option<(String, String)> {
    let input = c_string(value);
    let mut first = [0_u8; 41];
    let mut second = [0_u8; 41];
    // SAFETY: both buffers hold the 40 bytes and NUL the format allows.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%40[0-9.e+-]:%40[0-9.e+-]".as_ptr(),
            first.as_mut_ptr(),
            second.as_mut_ptr(),
        )
    };
    (matched == 2).then(|| (buffer_text(&first), buffer_text(&second)))
}

/// rrd_graph.c:5240.
fn parse_x_grid(value: &str) -> Result<(), String> {
    let input = c_string(value);
    let mut gtm = [0_u8; 12];
    let mut mtm = [0_u8; 12];
    let mut ltm = [0_u8; 12];
    let (mut gridst, mut mgridst, mut labst, mut precis): (
        libc::c_long,
        libc::c_long,
        libc::c_long,
        libc::c_long,
    ) = (0, 0, 0, 0);
    let mut stroff: libc::c_int = 0;
    // SAFETY: the %10[ conversions write at most 11 bytes into 12-byte
    // buffers and every other pointer has the type its conversion expects.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%10[A-Z]:%ld:%10[A-Z]:%ld:%10[A-Z]:%ld:%ld:%n".as_ptr(),
            gtm.as_mut_ptr(),
            &mut gridst as *mut libc::c_long,
            mtm.as_mut_ptr(),
            &mut mgridst as *mut libc::c_long,
            ltm.as_mut_ptr(),
            &mut labst as *mut libc::c_long,
            &mut precis as *mut libc::c_long,
            &mut stroff as *mut libc::c_int,
        )
    };
    if matched != 7 || stroff == 0 {
        return Err(String::from("invalid x-grid format"));
    }
    for keyword in [&gtm, &mtm, &ltm] {
        let keyword = buffer_text(keyword);
        if !matches!(
            keyword.as_str(),
            "SECOND" | "MINUTE" | "HOUR" | "DAY" | "WEEK" | "MONTH" | "YEAR"
        ) {
            return Err(format!("unknown keyword {keyword}"));
        }
    }
    Ok(())
}

/// rrd_graph.c:5280.
fn parse_y_grid(value: &str) -> Result<(f64, i32), String> {
    let input = c_string(value);
    let mut step_text = [0_u8; 41];
    let mut ylabfact: libc::c_int = 0;
    // SAFETY: the %40[ conversion writes at most 41 bytes.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%40[0-9.e+-]:%d".as_ptr(),
            step_text.as_mut_ptr(),
            &mut ylabfact as *mut libc::c_int,
        )
    };
    if matched != 2 {
        return Err(String::from("invalid y-grid format"));
    }
    let step = rrd_strtodbl(&buffer_text(&step_text), Some("option -y"))?;
    if step <= 0.0 {
        return Err(String::from("grid step must be > 0"));
    }
    if ylabfact < 1 {
        return Err(String::from("label factor must be > 0"));
    }
    Ok((step, ylabfact))
}

/// rrd_graph.c:5431.
fn parse_right_axis_range(value: &str) -> Result<(), String> {
    let Some((min_text, max_text)) = value.split_once(':') else {
        return Err(String::from(
            "invalid right-axis-range format expected min:max",
        ));
    };
    if min_text.len() >= 64 {
        return Err(String::from("right-axis-range min is too long"));
    }
    let mut min = f64::NAN;
    let mut max = f64::NAN;
    if !min_text.is_empty() {
        min = rrd_strtodbl(min_text, Some("option --right-axis-range"))?;
    }
    if !max_text.is_empty() {
        max = rrd_strtodbl(max_text, Some("option --right-axis-range"))?;
    }
    if !min.is_nan() && !max.is_nan() && min > max {
        return Err(String::from(
            "right-axis-range min must not be larger than max",
        ));
    }
    Ok(())
}

/// rrd_graph.c:5598.
fn parse_color(value: &str, colors: &mut super::GraphColors) -> Result<(), String> {
    let input = c_string(value);
    let mut name = [0_u8; 12];
    let mut color: libc::c_ulong = 0;
    let (mut col_start, mut col_end): (libc::c_int, libc::c_int) = (0, 0);
    // SAFETY: %10[ writes at most 11 bytes into a 12-byte buffer, %lx an
    // unsigned long, and each %n an int.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%10[A-Z]#%n%8lx%n".as_ptr(),
            name.as_mut_ptr(),
            &mut col_start as *mut libc::c_int,
            &mut color as *mut libc::c_ulong,
            &mut col_end as *mut libc::c_int,
        )
    };
    if matched != 2 {
        return Err(String::from("invalid color def format"));
    }
    let color = color as u64;
    let color = match col_end - col_start {
        3 => {
            ((color & 0xF00) * 0x110000)
                | ((color & 0x0F0) * 0x011000)
                | ((color & 0x00F) * 0x001100)
                | 0xFF
        }
        4 => {
            ((color & 0xF000) * 0x11000)
                | ((color & 0x0F00) * 0x01100)
                | ((color & 0x00F0) * 0x00110)
                | ((color & 0x000F) * 0x00011)
        }
        6 => (color << 8) + 0xff,
        8 => color,
        _ => return Err(String::from("the color format is #RRGGBB[AA]")),
    };
    let rgba = [
        (color >> 24) as u8,
        (color >> 16) as u8,
        (color >> 8) as u8,
        color as u8,
    ];
    let name = buffer_text(&name);
    let target = match name.as_str() {
        "BACK" => &mut colors.back,
        "CANVAS" => &mut colors.canvas,
        "SHADEA" => &mut colors.shade_a,
        "SHADEB" => &mut colors.shade_b,
        "GRID" => &mut colors.grid,
        "MGRID" => &mut colors.mgrid,
        "FONT" => &mut colors.font,
        "ARROW" => &mut colors.arrow,
        "AXIS" => &mut colors.axis,
        "FRAME" => &mut colors.frame,
        _ => return Err(format!("invalid color name '{name}'")),
    };
    *target = rgba;
    Ok(())
}

/// rrd_graph.c:5653.
fn parse_font(value: &str) -> Result<(usize, f64), String> {
    let input = c_string(value);
    let mut prop = [0_u8; 15];
    let mut size_text = [0_u8; 41];
    let mut end: libc::c_int = 0;
    // SAFETY: %10[ and %40[ fit their 15- and 41-byte buffers.
    let matched = unsafe {
        libc::sscanf(
            input.as_ptr(),
            c"%10[A-Z]:%40[0-9.e+-]%n".as_ptr(),
            prop.as_mut_ptr(),
            size_text.as_mut_ptr(),
            &mut end as *mut libc::c_int,
        )
    };
    let size = match rrd_strtodbl(&buffer_text(&size_text), None) {
        Ok(size) if matched >= 2 => size,
        _ => return Err(String::from("invalid text property format")),
    };
    let prop = buffer_text(&prop);
    let Some(index) = ["DEFAULT", "TITLE", "AXIS", "UNIT", "LEGEND", "WATERMARK"]
        .iter()
        .position(|name| *name == prop)
    else {
        return Err(format!("invalid fonttag '{prop}'"));
    };
    let end = usize::try_from(end).unwrap_or_default();
    if input.as_bytes().len() > end + 2 && input.as_bytes().get(end) != Some(&b':') {
        return Err(format!("expected : after font size in '{value}'"));
    }
    Ok((index, size))
}

const AXIS_FORMAT_PATTERN: &str =
    "^(?:[^%]+|%%)*%[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG](?:[^%]+|%%)*$";

/// `bad_format_axis` (rrd_graph.c:5890): one `%lf`-style conversion and
/// otherwise only literal text or `%%`.
fn bad_format_axis(format: &str) -> Result<(), String> {
    let bytes = format.as_bytes();
    let safe = |mut position: usize| {
        while position < bytes.len() {
            match (bytes[position], bytes.get(position + 1)) {
                (b'%', Some(b'%')) => position += 2,
                (b'%', _) => break,
                _ => position += 1,
            }
        }
        position
    };
    let mut position = safe(0);
    let conversion = (|| {
        if bytes.get(position) != Some(&b'%') {
            return None;
        }
        position += 1;
        if bytes
            .get(position)
            .is_some_and(|byte| b"-+ 0#".contains(byte))
        {
            position += 1;
        }
        while bytes.get(position).is_some_and(u8::is_ascii_digit) {
            position += 1;
        }
        if bytes.get(position) == Some(&b'.')
            && bytes.get(position + 1).is_some_and(u8::is_ascii_digit)
        {
            position += 1;
            while bytes.get(position).is_some_and(u8::is_ascii_digit) {
                position += 1;
            }
        }
        if bytes.get(position) != Some(&b'l') {
            return None;
        }
        position += 1;
        if !bytes
            .get(position)
            .is_some_and(|byte| b"eEfFgG".contains(byte))
        {
            return None;
        }
        Some(position + 1)
    })();
    match conversion {
        Some(rest) if safe(rest) == bytes.len() => Ok(()),
        _ => Err(format!(
            "invalid format string '{format}' (should match '{AXIS_FORMAT_PATTERN}')"
        )),
    }
}

/// `rrd_strtod` (rrd_strtod.c:97). The second value is the end of the
/// conversion; 0 means none, which also covers an out-of-range exponent.
fn rrd_strtod(text: &[u8]) -> (f64, usize) {
    let digit = |position: usize| text.get(position).filter(|byte| byte.is_ascii_digit());
    let mut position = 0;
    while text
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
    {
        position += 1;
    }
    let mut negative = false;
    match text.get(position) {
        Some(b'-') => {
            negative = true;
            position += 1;
        }
        Some(b'+') => position += 1,
        _ => {}
    }
    let mut number = 0.0_f64;
    let mut exponent = 0_i32;
    let mut digits = 0;
    while let Some(byte) = digit(position) {
        number = number * 10.0 + f64::from(byte - b'0');
        position += 1;
        digits += 1;
    }
    if text.get(position) == Some(&b'.') {
        position += 1;
        let mut decimals = 0_i32;
        while let Some(byte) = digit(position) {
            number = number * 10.0 + f64::from(byte - b'0');
            position += 1;
            digits += 1;
            decimals = decimals.wrapping_add(1);
        }
        exponent = exponent.wrapping_sub(decimals);
    }
    if digits == 0 {
        return (0.0, 0);
    }
    if negative {
        number = -number;
    }
    if matches!(text.get(position), Some(b'e' | b'E')) {
        position += 1;
        let mut exponent_negative = false;
        match text.get(position) {
            Some(b'-') => {
                exponent_negative = true;
                position += 1;
            }
            Some(b'+') => position += 1,
            _ => {}
        }
        let mut n = 0_i32;
        while let Some(byte) = digit(position) {
            n = n.wrapping_mul(10).wrapping_add(i32::from(byte - b'0'));
            position += 1;
        }
        exponent = if exponent_negative {
            exponent.wrapping_sub(n)
        } else {
            exponent.wrapping_add(n)
        };
    }
    if !(f64::MIN_EXP..=f64::MAX_EXP).contains(&exponent) {
        return (f64::INFINITY, 0);
    }
    let mut p10 = 10.0_f64;
    let mut n = exponent.unsigned_abs();
    while n != 0 {
        if n & 1 == 1 {
            if exponent < 0 {
                number /= p10;
            } else {
                number *= p10;
            }
        }
        n >>= 1;
        p10 *= p10;
    }
    (number, position)
}

/// `rrd_strtodbl` (rrd_strtod.c:59): `Ok` only when the whole string
/// converts. With a label the error carries RRDtool's message.
pub(crate) fn rrd_strtodbl(text: &str, error: Option<&str>) -> Result<f64, String> {
    let bytes = text.as_bytes();
    let (value, end) = rrd_strtod(bytes);
    if end == 0 {
        let prefix = |head: &str| {
            bytes
                .get(..head.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(head.as_bytes()))
        };
        if prefix("-nan") || prefix("nan") {
            return Ok(f64::NAN);
        }
        if prefix("inf") {
            return Ok(f64::INFINITY);
        }
        if prefix("-inf") {
            return Ok(f64::NEG_INFINITY);
        }
        return Err(error.map_or_else(String::new, |error| {
            format!("{error} - Cannot convert '{text}' to float")
        }));
    }
    if end < bytes.len() {
        return Err(error.map_or_else(String::new, |error| {
            format!(
                "{error} - Converted '{text}' to {}, but cannot convert '{}'",
                c_format_double(c"%lf", value),
                String::from_utf8_lossy(&bytes[end..])
            )
        }));
    }
    Ok(value)
}

/// Formats one double with the C library so locale and spelling of
/// non-finite values match RRDtool.
pub(crate) fn c_format_double(format: &CStr, value: f64) -> String {
    let mut buffer = vec![0_u8; 64];
    loop {
        // SAFETY: the buffer length bounds the write and `format` holds one
        // double conversion.
        let written = unsafe {
            libc::snprintf(
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                format.as_ptr(),
                value,
            )
        };
        let Ok(written) = usize::try_from(written) else {
            return String::new();
        };
        if written < buffer.len() {
            return String::from_utf8_lossy(&buffer[..written]).into_owned();
        }
        buffer.resize(written + 1, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtodbl_matches_rrd_strtod() {
        assert_eq!(rrd_strtodbl("1.5", None), Ok(1.5));
        assert_eq!(rrd_strtodbl("1e", None), Ok(1.0));
        assert!(rrd_strtodbl("Infinity", None).unwrap().is_infinite());
        assert_eq!(
            rrd_strtodbl("abc", Some("option -l")),
            Err(String::from("option -l - Cannot convert 'abc' to float"))
        );
        assert_eq!(
            rrd_strtodbl("2x", Some("option -u")),
            Err(String::from(
                "option -u - Converted '2x' to 2.000000, but cannot convert 'x'"
            ))
        );
        assert_eq!(rrd_strtodbl("1e400", None), Ok(f64::INFINITY));
        assert!(rrd_strtodbl("1e2000", None).is_err());
    }

    #[test]
    fn axis_format_check() {
        assert!(bad_format_axis("%5.1lf %%").is_ok());
        assert!(bad_format_axis("x %lf y").is_ok());
        assert!(bad_format_axis("%d").is_err());
        assert!(bad_format_axis("%lf %lf").is_err());
        assert!(bad_format_axis("%.lf").is_err());
    }
}
