//! Graph element model and data preparation shared by `graph` and `xport`.
//!
//! Ports `rrd_graph_script` and its element parsers from RRDtool 1.11.0
//! `rrd_graph_helper.c`, `data_fetch`, `data_calc`, `vdef_parse`,
//! `vdef_calc` and the value half of `print_calc` from `rrd_graph.c`, and
//! `rrd_xport_fn` from `rrd_xport.c`. Every DEF keeps its own fetch window
//! and step; CDEFs run over the intersection of their inputs at the greatest
//! common divisor of their steps; VDEF and PRINT read the referenced
//! element's own series.

use crate::{
    StoreError, fetch_rrd_file,
    rpn::{self, Node, Op, RpnStack},
    rrd_binary::rrd_nan,
    rrd_number::parse_rrd_number,
    vdef::{VdefFunction, evaluate_vdef},
};
use std::collections::HashMap;

/// `FMT_LEG_LEN` with `HAVE_SNPRINTF`.
pub const FMT_LEG_LEN: usize = 200;
const MAX_VNAME_LEN: usize = 255;
const DS_NAM_SIZE: usize = 20;
const MAX_AXIS: i64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gf {
    Print,
    Gprint,
    Comment,
    Hrule,
    Vrule,
    Line,
    Area,
    Stack,
    Tick,
    TextAlign,
    Def,
    Cdef,
    Vdef,
    Shift,
    Xport,
}

fn gf_conv(name: &str) -> Option<Gf> {
    Some(match name {
        "PRINT" => Gf::Print,
        "GPRINT" => Gf::Gprint,
        "COMMENT" => Gf::Comment,
        "HRULE" => Gf::Hrule,
        "VRULE" => Gf::Vrule,
        "LINE" => Gf::Line,
        "AREA" => Gf::Area,
        "STACK" => Gf::Stack,
        "TICK" => Gf::Tick,
        "TEXTALIGN" => Gf::TextAlign,
        "DEF" => Gf::Def,
        "CDEF" => Gf::Cdef,
        "VDEF" => Gf::Vdef,
        "XPORT" => Gf::Xport,
        "SHIFT" => Gf::Shift,
        _ => return None,
    })
}

/// `enum cf_en`, in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cf {
    Average,
    Minimum,
    Maximum,
    Last,
    HwPredict,
    Seasonal,
    DevPredict,
    DevSeasonal,
    Failures,
    MhwPredict,
}

impl Cf {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "AVERAGE" => Self::Average,
            "MIN" => Self::Minimum,
            "MAX" => Self::Maximum,
            "LAST" => Self::Last,
            "HWPREDICT" => Self::HwPredict,
            "MHWPREDICT" => Self::MhwPredict,
            "DEVPREDICT" => Self::DevPredict,
            "SEASONAL" => Self::Seasonal,
            "DEVSEASONAL" => Self::DevSeasonal,
            "FAILURES" => Self::Failures,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Average => "AVERAGE",
            Self::Minimum => "MIN",
            Self::Maximum => "MAX",
            Self::Last => "LAST",
            Self::HwPredict => "HWPREDICT",
            Self::Seasonal => "SEASONAL",
            Self::DevPredict => "DEVPREDICT",
            Self::DevSeasonal => "DEVSEASONAL",
            Self::Failures => "FAILURES",
            Self::MhwPredict => "MHWPREDICT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueFormatter {
    Numeric,
    Timestamp,
    Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    Justified,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vdef {
    pub op: Option<VdefFunction>,
    pub param: f64,
    pub val: f64,
    pub when: i64,
    pub never: bool,
}

/// `graph_desc_t`.
#[derive(Debug, Clone)]
pub struct GraphElement {
    pub gf: Gf,
    pub stack: bool,
    pub debug: i64,
    pub skipscale: bool,
    pub vname: String,
    /// `vidx`; `None` is C's -1 (a numeric value instead of a variable).
    pub vidx: Option<usize>,
    pub rrd: String,
    pub ds_nam: String,
    pub daemon: String,
    /// `None` is C's `(enum cf_en) -1`.
    pub cf: Option<Cf>,
    pub cf_reduce: Cf,
    pub cf_reduce_set: bool,
    /// RGBA; `None` keeps the zero-initialized colour.
    pub color: Option<[u8; 4]>,
    pub color2: Option<[u8; 4]>,
    pub gradheight: f64,
    pub format: String,
    pub legend: String,
    pub strftm: bool,
    pub vformatter: ValueFormatter,
    pub yrule: f64,
    pub xrule: i64,
    pub vf: Vdef,
    pub rpn: String,
    rpnp: Vec<Node>,
    pub shidx: Option<usize>,
    pub shval: i64,
    pub shift: i64,
    pub start: i64,
    pub end: i64,
    pub start_orig: i64,
    pub end_orig: i64,
    pub step: u64,
    pub step_orig: u64,
    /// Zero marks a VDEF for CDEF substitution, as in `data_calc`.
    pub ds_cnt: u64,
    /// The element's own series: the selected DS of a DEF, or a CDEF result.
    pub data: Vec<f64>,
    pub linewidth: f64,
    pub dash: bool,
    pub dashes: Vec<f64>,
    pub dash_offset: f64,
    pub txtalign: TextAlign,
    pub xaxisidx: i64,
    pub yaxisidx: i64,
}

impl GraphElement {
    fn new(im: &GraphImage) -> Self {
        Self {
            gf: Gf::Print,
            stack: false,
            debug: 0,
            skipscale: false,
            vname: String::new(),
            vidx: None,
            rrd: String::new(),
            ds_nam: String::new(),
            daemon: String::new(),
            cf: Some(Cf::Average),
            cf_reduce: Cf::Average,
            cf_reduce_set: false,
            color: None,
            color2: None,
            gradheight: 50.0,
            format: String::new(),
            legend: String::new(),
            strftm: false,
            vformatter: ValueFormatter::Numeric,
            yrule: rrd_nan(),
            xrule: 0,
            vf: Vdef {
                op: None,
                param: 0.0,
                val: 0.0,
                when: 0,
                never: false,
            },
            rpn: String::new(),
            rpnp: Vec::new(),
            shidx: None,
            shval: 0,
            shift: 0,
            start: im.start,
            end: im.end,
            start_orig: im.start,
            end_orig: im.end,
            step: im.step,
            step_orig: im.step,
            ds_cnt: 0,
            data: Vec::new(),
            linewidth: 0.0,
            dash: false,
            dashes: Vec::new(),
            dash_offset: 0.0,
            txtalign: TextAlign::Left,
            xaxisidx: 0,
            yaxisidx: 0,
        }
    }
}

/// Resolves a DEF's `start=`/`end=` pair like `rrd_parsetime` followed by
/// `rrd_proc_start_end`. Arguments are the given specifications and the
/// element's current window; errors carry RRDtool's full message.
pub type TimeResolver<'a> =
    &'a dyn Fn(Option<&str>, Option<&str>, i64, i64) -> Result<(i64, i64), String>;

/// The parts of `image_desc_t` used for data preparation.
pub struct GraphImage {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    pub gdes: Vec<GraphElement>,
    /// `--daemon`, used by DEFs without their own `daemon=`.
    pub daemon_addr: Option<String>,
    /// `--use-nan-for-all-missing-data`.
    pub allow_missing_ds: bool,
    gdef_map: HashMap<String, usize>,
}

impl GraphImage {
    pub fn new(start: i64, end: i64, step: u64) -> Self {
        Self {
            start,
            end,
            step,
            gdes: Vec::new(),
            daemon_addr: None,
            allow_missing_ds: false,
            gdef_map: HashMap::new(),
        }
    }

    fn find_var(&self, key: &str) -> Option<usize> {
        self.gdef_map.get(key).copied()
    }
}

/// How `rrd_graph_script` stopped.
#[derive(Debug, PartialEq)]
pub enum ScriptError {
    /// `rrd_set_error` was called with this message.
    Error(String),
    /// A parser failed without setting an error (an invalid colour): RRDtool
    /// stops reading the script and keeps the elements parsed so far.
    Silent,
}

fn err<T>(message: impl Into<String>) -> Result<T, ScriptError> {
    Err(ScriptError::Error(message.into()))
}

struct KeyValue {
    keyvalue: Option<String>,
    key: String,
    value: String,
    flag: u8,
}

/// `parsedargs_t`.
struct ParsedArgs {
    arg_orig: String,
    kv: Vec<KeyValue>,
}

const POSKEYS: [&str; 10] = [
    "pos0", "pos1", "pos2", "pos3", "pos4", "pos5", "pos6", "pos7", "pos8", "pos9",
];

impl ParsedArgs {
    /// Port of `parseArguments`.
    fn parse(origarg: &str) -> Result<Self, ScriptError> {
        let mut parsed = Self {
            arg_orig: origarg.to_owned(),
            kv: Vec::new(),
        };
        let mut bytes = origarg.as_bytes().to_vec();
        let mut poscnt = 0;
        let mut field_start = 0;
        let mut pos = 0;
        loop {
            let c = bytes.get(pos).copied().unwrap_or(0);
            match c {
                b'\\' => {
                    if bytes.get(pos + 1) == Some(&b':') {
                        bytes.remove(pos);
                    }
                }
                0 | b':' => {
                    let field = String::from_utf8_lossy(&bytes[field_start..pos]).into_owned();
                    let keyvalue = field.clone();
                    let (mut key, value) = if let Some((key, value)) = field.split_once('=') {
                        (key.to_owned(), value.to_owned())
                    } else if poscnt > 0 && field == "STACK" {
                        ("stack".to_owned(), "1".to_owned())
                    } else if poscnt > 0 && field == "strftime" {
                        ("strftime".to_owned(), "1".to_owned())
                    } else if poscnt > 0 && field == "dashes" {
                        ("dashes".to_owned(), "5,5".to_owned())
                    } else if poscnt > 0 && field == "valstrftime" {
                        ("vformatter".to_owned(), "timestamp".to_owned())
                    } else if poscnt > 0 && field == "valstrfduration" {
                        ("vformatter".to_owned(), "duration".to_owned())
                    } else if poscnt > 0 && field == "skipscale" {
                        ("skipscale".to_owned(), "1".to_owned())
                    } else {
                        if poscnt > 9 {
                            return err("too many positional arguments");
                        }
                        let key = POSKEYS[poscnt].to_owned();
                        poscnt += 1;
                        (key, field)
                    };
                    match key.as_str() {
                        "label" => key = "legend".to_owned(),
                        "colour" => key = "color".to_owned(),
                        "colour2" => key = "color2".to_owned(),
                        _ => {}
                    }
                    parsed.kv.push(KeyValue {
                        keyvalue: Some(keyvalue),
                        key,
                        value,
                        flag: 0,
                    });
                    field_start = pos + 1;
                }
                _ => {}
            }
            if c == 0 {
                break;
            }
            pos += 1;
        }
        Ok(parsed)
    }

    /// `getKeyValueArgument`: the last matching key wins.
    fn get(&mut self, key: &str, flag: u8) -> Option<String> {
        let entry = self.kv.iter_mut().rev().find(|entry| entry.key == key)?;
        if flag != 0 {
            entry.flag = flag;
        }
        Some(entry.value.clone())
    }

    /// `getFirstUnusedArgument`.
    fn first_unused(&mut self, flag: u8) -> Option<usize> {
        let index = self.kv.iter().position(|entry| entry.flag == 0)?;
        self.kv[index].flag = flag;
        Some(index)
    }

    /// `resetParsedArguments`.
    fn reset(&mut self) {
        for entry in &mut self.kv {
            if entry.flag != 255 {
                entry.flag = 0;
            }
        }
    }

    /// `checkUnusedValues`.
    fn unused(&self) -> Option<String> {
        let unused: Vec<&str> = self
            .kv
            .iter()
            .filter(|entry| entry.flag == 0)
            .map(|entry| entry.keyvalue.as_deref().unwrap_or(""))
            .collect();
        (!unused.is_empty()).then(|| unused.join(":"))
    }
}

/// `getLong` with base 10: 0 for a full parse, 1 for trailing bytes, -1 when
/// nothing converts.
fn get_long(text: &str) -> (i32, i64) {
    let bytes = text.as_bytes();
    let mut position = 0;
    while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
        position += 1;
    }
    let negative = match bytes.get(position) {
        Some(b'-') => {
            position += 1;
            true
        }
        Some(b'+') => {
            position += 1;
            false
        }
        _ => false,
    };
    let digits_start = position;
    let mut value: i64 = 0;
    let mut overflow = false;
    while let Some(digit @ b'0'..=b'9') = bytes.get(position).copied() {
        match value
            .checked_mul(10)
            .and_then(|value| value.checked_add(i64::from(digit - b'0')))
        {
            Some(next) => value = next,
            None => overflow = true,
        }
        position += 1;
    }
    if position == digits_start {
        return (-1, 0);
    }
    let value = if overflow {
        if negative { i64::MIN } else { i64::MAX }
    } else if negative {
        -value
    } else {
        value
    };
    (i32::from(position != bytes.len()), value)
}

/// `getDouble`: zero only for a complete `rrd_strtodbl` conversion.
fn get_double(text: &str) -> Option<f64> {
    parse_rrd_number(text)
}

/// `parse_color`: `RGB`, `RGBA`, `RRGGBB` or `RRGGBBAA` hex digits.
pub fn parse_color(text: &str) -> Option<[u8; 4]> {
    if !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |index: usize| u8::from_str_radix(&text[index..index + 1], 16).unwrap_or(0);
    let pair = |index: usize| u8::from_str_radix(&text[index..index + 2], 16).unwrap_or(0);
    match text.len() {
        3 => Some([digit(0) * 0x11, digit(1) * 0x11, digit(2) * 0x11, 0xff]),
        4 => Some([
            digit(0) * 0x11,
            digit(1) * 0x11,
            digit(2) * 0x11,
            digit(3) * 0x11,
        ]),
        6 => Some([pair(0), pair(2), pair(4), 0xff]),
        8 => Some([pair(0), pair(2), pair(4), pair(6)]),
        _ => None,
    }
}

/// `strncpy` into a fixed buffer of `len` bytes.
fn truncate_bytes(text: &str, len: usize) -> String {
    if text.len() <= len {
        return text.to_owned();
    }
    String::from_utf8_lossy(&text.as_bytes()[..len]).into_owned()
}

/// The `PARSE_*` bit set of `newGraphDescription`.
#[derive(Clone, Copy, Default)]
struct Parse {
    vname: bool,
    rrd: bool,
    ds: bool,
    cf: bool,
    color: bool,
    color2: bool,
    legend: bool,
    rpn: bool,
    start: bool,
    step: bool,
    end: bool,
    stack: bool,
    linewidth: bool,
    xaxis: bool,
    yaxis: bool,
    reduce: bool,
    skipscale: bool,
    daemon: bool,
    dashes: bool,
    gradheight: bool,
    format: bool,
    strftime_vfmt: bool,
    fraction: bool,
    vnamedef: bool,
    vnameref: bool,
    vnamerefnum: bool,
    positional: bool,
    retry: bool,
}

impl Parse {
    fn vname_rrd_ds_cf() -> Self {
        Self {
            positional: true,
            vname: true,
            vnamedef: true,
            rrd: true,
            ds: true,
            cf: true,
            ..Self::default()
        }
    }

    fn vname_color_legend() -> Self {
        Self {
            positional: true,
            vname: true,
            vnameref: true,
            vnamerefnum: true,
            color: true,
            color2: true,
            legend: true,
            ..Self::default()
        }
    }

    fn vname_rpn() -> Self {
        Self {
            positional: true,
            vname: true,
            vnamedef: true,
            rpn: true,
            ..Self::default()
        }
    }

    fn vname_ref_pos() -> Self {
        Self {
            positional: true,
            vname: true,
            vnameref: true,
            ..Self::default()
        }
    }
}

impl GraphImage {
    /// Port of `newGraphDescription`. The element is appended before any
    /// check, so a silent failure leaves it partly filled, as upstream.
    fn new_graph_description(
        &mut self,
        gf: Gf,
        pa: &mut ParsedArgs,
        bits: Parse,
        times: TimeResolver<'_>,
    ) -> Result<usize, ScriptError> {
        if !bits.retry {
            let element = GraphElement::new(self);
            self.gdes.push(element);
        }
        let gdi = self.gdes.len() - 1;
        self.gdes[gdi].gf = gf;
        if let Some(text) = pa.get("debug", 1) {
            let (status, value) = get_long(&text);
            if status != 0 {
                return err(format!("Bad debug value: {text}"));
            }
            self.gdes[gdi].debug = value;
        } else {
            self.gdes[gdi].debug = 0;
        }
        let mut vname = bits.vname.then(|| pa.get("vname", 1)).flatten();
        let mut rrd = bits.rrd.then(|| pa.get("rrd", 1)).flatten();
        let mut ds = bits.ds.then(|| pa.get("ds", 1)).flatten();
        let mut cf = bits.cf.then(|| pa.get("cf", 1)).flatten();
        let mut color = bits.color.then(|| pa.get("color", 1)).flatten();
        let mut color2 = bits.color2.then(|| pa.get("color2", 1)).flatten();
        let mut rpn_text = bits.rpn.then(|| pa.get("rpn", 1)).flatten();
        let mut legend = bits.legend.then(|| pa.get("legend", 1)).flatten();
        let mut fraction = bits.fraction.then(|| pa.get("fraction", 1)).flatten();
        if bits.format {
            if let Some(format) = pa.get("format", 1) {
                self.gdes[gdi].format = truncate_bytes(&format, FMT_LEG_LEN);
            }
        }
        if bits.strftime_vfmt {
            let strft = pa.get("strftime", 1);
            let formatter = pa.get("vformatter", 1);
            self.gdes[gdi].strftm = strft.is_some();
            if let Some(formatter) = formatter {
                self.gdes[gdi].vformatter = match formatter.as_str() {
                    "timestamp" => ValueFormatter::Timestamp,
                    "duration" => ValueFormatter::Duration,
                    _ => return err(format!("Unsupported vformatter: {formatter}")),
                };
            }
        }
        if bits.stack {
            self.gdes[gdi].stack = pa.get("stack", 1).is_some();
        }
        if bits.skipscale {
            self.gdes[gdi].skipscale = pa.get("skipscale", 1).is_some();
        }
        if bits.reduce {
            if let Some(reduce) = pa.get("reduce", 1) {
                match Cf::parse(&reduce) {
                    Some(value) => {
                        self.gdes[gdi].cf_reduce = value;
                        self.gdes[gdi].cf_reduce_set = true;
                    }
                    None => return err(format!("bad reduce CF: {reduce}")),
                }
            }
        }
        if bits.daemon {
            if let Some(daemon) = pa.get("daemon", 1) {
                self.gdes[gdi].daemon = truncate_bytes(&daemon, 255);
            }
        }
        if bits.xaxis {
            let mut xaxis = 0;
            if let Some(text) = pa.get("xaxis", 1) {
                let (status, value) = get_long(&text);
                if status != 0 || !(1..=MAX_AXIS).contains(&value) {
                    return err(format!("Bad xaxis value: {text}"));
                }
                xaxis = value;
            }
            self.gdes[gdi].xaxisidx = xaxis;
        }
        if bits.yaxis {
            let mut yaxis = 0;
            if let Some(text) = pa.get("yaxis", 1) {
                let (status, value) = get_long(&text);
                if status != 0 || !(1..=MAX_AXIS).contains(&value) {
                    return err(format!("Bad yaxis value: {text}"));
                }
                yaxis = value;
            }
            self.gdes[gdi].yaxisidx = yaxis;
        }
        if bits.linewidth {
            let mut linewidth = 1.0;
            if let Some(text) = pa.get("linewidth", 1).filter(|text| !text.is_empty()) {
                match get_double(&text).filter(|value| *value >= 0.0) {
                    Some(value) => linewidth = value,
                    None => return err(format!("Bad line width: {text}")),
                }
            }
            self.gdes[gdi].linewidth = linewidth;
        }
        if bits.gradheight {
            if let Some(text) = pa.get("gradheight", 1).filter(|text| !text.is_empty()) {
                match get_double(&text) {
                    Some(value) => self.gdes[gdi].gradheight = value,
                    None => return err(format!("Bad gradheight: {text}")),
                }
            }
        }
        if bits.step {
            if let Some(text) = pa.get("step", 1) {
                let (status, value) = get_long(&text);
                if status != 0 || value < 1 {
                    return err(format!("Bad step value: {text}"));
                }
                self.gdes[gdi].step = value as u64;
            }
        }
        if bits.start || bits.end {
            let start = bits.start.then(|| pa.get("start", 1)).flatten();
            let end = bits.end.then(|| pa.get("end", 1)).flatten();
            let (start, end) = times(
                start.as_deref(),
                end.as_deref(),
                self.gdes[gdi].start,
                self.gdes[gdi].end,
            )
            .map_err(ScriptError::Error)?;
            if start < 3600 * 24 * 365 * 10 {
                return err(format!(
                    "the first entry to fetch should be after 1980 ({start})"
                ));
            }
            if end < start {
                return err(format!("start ({start}) should be less than end ({end})"));
            }
            let element = &mut self.gdes[gdi];
            element.start = start;
            element.start_orig = start;
            element.end = end;
            element.end_orig = end;
        }
        if bits.dashes {
            if let Some(dashes) = pa.get("dashes", 1) {
                let element = &mut self.gdes[gdi];
                element.dash = true;
                element.dash_offset = 0.0;
                let ndash = dashes.matches(',').count() + 1;
                element.dashes.clear();
                let mut rest = dashes.as_str();
                for index in 0..ndash {
                    let (value, consumed) = strtod_prefix(rest);
                    if consumed == 0 {
                        return err(format!("Could not parse number: {rest}"));
                    }
                    element.dashes.push(value);
                    if consumed < rest.len() {
                        if rest.as_bytes()[consumed] != b',' {
                            return err(format!("expected a ',' at : {}", &rest[consumed..]));
                        }
                        rest = &rest[consumed + 1..];
                    } else if index != ndash - 1 {
                        return err(format!("unexpected end at : {rest}"));
                    }
                }
            }
            if let Some(offset) = pa.get("dash-offset", 1) {
                match get_double(&offset) {
                    Some(value) => self.gdes[gdi].dash_offset = value,
                    None => return err(format!("Could not parse dash-offset: {offset}")),
                }
            }
        }

        if bits.positional && bits.vnamedef && bits.rrd && bits.ds && bits.cf {
            if vname.is_none() || rrd.is_none() {
                let Some(first) = pa.first_unused(1) else {
                    return err(format!(
                        "No argument for definition of vdef/rrd in {}",
                        pa.arg_orig
                    ));
                };
                vname.get_or_insert_with(|| pa.kv[first].key.clone());
                rrd.get_or_insert_with(|| pa.kv[first].value.clone());
            }
            if ds.is_none() {
                let Some(first) = pa.first_unused(1) else {
                    return err(format!(
                        "No argument for definition of DS in {}",
                        pa.arg_orig
                    ));
                };
                ds = Some(pa.kv[first].value.clone());
            }
            if cf.is_none() {
                let Some(first) = pa.first_unused(1) else {
                    return err(format!(
                        "No argument for definition of CF in {}",
                        pa.arg_orig
                    ));
                };
                cf = Some(pa.kv[first].value.clone());
            }
        } else if bits.positional && bits.vnamerefnum && bits.color && bits.color2 && bits.legend {
            if vname.is_none() {
                match pa.first_unused(1) {
                    Some(first) => vname = Some(pa.kv[first].value.clone()),
                    None => return err("No positional VNAME"),
                }
            }
            if bits.fraction && fraction.is_none() {
                match pa.first_unused(1) {
                    Some(first) => fraction = Some(pa.kv[first].value.clone()),
                    None => return err("No positional FRACTION"),
                }
            }
            if legend.is_none() {
                if let Some(first) = pa.first_unused(1) {
                    legend = pa.kv[first].keyvalue.clone();
                }
            }
        } else if bits.positional && bits.vnamedef && bits.rpn {
            if vname.is_none() || rpn_text.is_none() {
                let Some(first) = pa.first_unused(1) else {
                    return err(format!(
                        "No argument for definition of vdef/rrd in {}",
                        pa.arg_orig
                    ));
                };
                vname.get_or_insert_with(|| pa.kv[first].key.clone());
                rpn_text.get_or_insert_with(|| pa.kv[first].value.clone());
            }
        } else if bits.positional && bits.vnameref && vname.is_none() {
            let Some(first) = pa.first_unused(1) else {
                return err(format!(
                    "No argument for definition of vdef/rrd in {}",
                    pa.arg_orig
                ));
            };
            vname = Some(pa.kv[first].value.clone());
        }

        if let Some(name) = vname.as_mut() {
            if let Some((head, colors)) = name.clone().split_once('#') {
                let (first, second) = match colors.split_once('#') {
                    Some((first, second)) => (first.to_owned(), Some(second.to_owned())),
                    None => (colors.to_owned(), None),
                };
                *name = head.to_owned();
                if bits.color && color.is_none() {
                    color = Some(first);
                }
                if bits.color2 && color2.is_none() {
                    color2 = second;
                }
            }
        }

        if let Some(name) = vname.as_deref() {
            let idx = self.find_var(name);
            if bits.vnamedef {
                if idx.is_some() {
                    return err(format!("trying to reuse vname {name}"));
                }
            } else if bits.vnameref {
                self.gdes[gdi].vidx = idx;
                if idx.is_none() {
                    if bits.vnamerefnum {
                        let Some(value) = get_double(name) else {
                            return err(format!("{name} is not a vname nor a number"));
                        };
                        if gf == Gf::Vrule {
                            let mut xrule = rpn::c_long(value);
                            if xrule == 0 {
                                xrule += 1;
                            }
                            self.gdes[gdi].xrule = xrule;
                        } else {
                            self.gdes[gdi].yrule = value;
                        }
                    } else {
                        return err(format!("vname {name} not found"));
                    }
                }
            }
        }

        let element = &mut self.gdes[gdi];
        if let Some(name) = &vname {
            element.vname = truncate_bytes(name, MAX_VNAME_LEN);
        }
        if let Some(rrd) = &rrd {
            element.rrd = truncate_bytes(rrd, 1023);
        }
        if let Some(ds) = &ds {
            element.ds_nam = truncate_bytes(ds, DS_NAM_SIZE - 1);
        }
        if let Some(cf) = &cf {
            match Cf::parse(cf) {
                Some(value) => element.cf = Some(value),
                None => return err(format!("bad CF: {cf}")),
            }
        } else if bits.cf {
            element.cf = None;
        }
        if let Some(text) = &color {
            element.color = Some(parse_color(text).ok_or(ScriptError::Silent)?);
        }
        if let Some(text) = &color2 {
            element.color2 = Some(parse_color(text).ok_or(ScriptError::Silent)?);
        }
        if let Some(rpn_text) = rpn_text {
            element.rpn = rpn_text;
        }
        if let Some(legend) = legend.filter(|legend| !legend.is_empty()) {
            element.legend = truncate_bytes(&legend, FMT_LEG_LEN);
        }
        if let Some(fraction) = fraction {
            if fraction == "vname" {
                let source = element.vidx.map(|index| self.gdes[index].gf);
                if !matches!(source, Some(Gf::Def | Gf::Cdef)) {
                    return err(format!(
                        "variable '{}' not DEF nor CDEF when using dynamic fractions",
                        self.gdes[gdi].vname
                    ));
                }
                self.gdes[gdi].cf = Some(Cf::Last);
                self.gdes[gdi].yrule = 0.5;
            } else {
                match get_double(&fraction) {
                    Some(value) => self.gdes[gdi].yrule = value,
                    None => {
                        return err(format!(
                            "error parsing number {}",
                            vname.as_deref().unwrap_or("(null)")
                        ));
                    }
                }
            }
        }
        if matches!(gf, Gf::Def | Gf::Vdef | Gf::Cdef) {
            self.gdef_map.insert(self.gdes[gdi].vname.clone(), gdi);
        }
        Ok(gdi)
    }

    fn parse_def(
        &mut self,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let bits = Parse {
            start: true,
            step: true,
            end: true,
            reduce: true,
            daemon: true,
            ..Parse::vname_rrd_ds_cf()
        };
        let gdi = match self.new_graph_description(Gf::Def, pa, bits, times) {
            Ok(gdi) => gdi,
            Err(original) => {
                // The first unused field may have been read as a keyword
                // such as `step=`; retry with that key disguised, keeping
                // the first error if the retry also fails.
                pa.reset();
                let Some(first) = pa.kv.iter().position(|entry| entry.flag == 0) else {
                    return Err(original);
                };
                if POSKEYS.contains(&pa.kv[first].key.as_str()) {
                    return Err(original);
                }
                let key = &mut pa.kv[first].key;
                *key = format!("\u{80}{}", key.get(1..).unwrap_or(""));
                match self.new_graph_description(
                    Gf::Def,
                    pa,
                    Parse {
                        retry: true,
                        ..bits
                    },
                    times,
                ) {
                    Ok(gdi) => gdi,
                    Err(_) => return Err(original),
                }
            }
        };
        if self.gdes[gdi].step == 0 {
            self.gdes[gdi].step = self.step;
        }
        let element = &self.gdes[gdi];
        element.dprint(|| {
            format!(
                "{RULE}DEF   : {}\nVNAME : {}\nRRD   : {}\nDS    : {}\nCF    : {}\nSTART : ({})\nSTEP  : ({})\nEND   : ({})\nREDUCE: ({})\nDAEMON: {}\n{RULE}",
                pa.arg_orig,
                element.vname,
                element.rrd,
                element.ds_nam,
                element.cf.map_or(-1, |cf| cf as i32),
                element.start,
                element.step,
                element.end,
                element.cf_reduce as i32,
                element.daemon
            )
        });
        Ok(())
    }

    fn parse_cvdef(
        &mut self,
        gf: Gf,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let gdi = self.new_graph_description(gf, pa, Parse::vname_rpn(), times)?;
        if gf == Gf::Cdef {
            let rpn = self.gdes[gdi].rpn.clone();
            let nodes = rpn::rpn_parse(&rpn, &|name| self.find_var(name))
                .map_err(|error| ScriptError::Error(error.to_string()))?;
            self.gdes[gdi].rpnp = nodes;
        } else {
            let rpn = self.gdes[gdi].rpn.clone();
            let Some((name, function)) = rpn.split_once(',') else {
                return err(format!("Comma expected in VDEF definition {rpn}"));
            };
            let name = truncate_bytes(name, MAX_VNAME_LEN);
            let Some(vidx) = self.find_var(&name) else {
                return err(format!("Not a valid vname: {name} in line {rpn}"));
            };
            self.gdes[gdi].vidx = Some(vidx);
            if !matches!(self.gdes[vidx].gf, Gf::Def | Gf::Cdef) {
                return err(format!(
                    "variable '{name}' not DEF nor CDEF in VDEF '{rpn}'"
                ));
            }
            vdef_parse(&mut self.gdes[gdi], function)?;
        }
        let element = &self.gdes[gdi];
        element.dprint(|| {
            format!(
                "{RULE}{}  : {}\nVNAME : {}\nRPN   : {}\n{RULE}",
                if gf == Gf::Cdef { "CDEF" } else { "VDEF" },
                pa.arg_orig,
                element.vname,
                element.rpn
            )
        });
        Ok(())
    }

    fn parse_line_area(
        &mut self,
        gf: Gf,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let bits = match gf {
            Gf::Line => Parse {
                stack: true,
                skipscale: true,
                linewidth: true,
                dashes: true,
                xaxis: true,
                yaxis: true,
                ..Parse::vname_color_legend()
            },
            Gf::Area => Parse {
                stack: true,
                skipscale: true,
                xaxis: true,
                yaxis: true,
                gradheight: true,
                ..Parse::vname_color_legend()
            },
            _ => Parse {
                xaxis: true,
                yaxis: true,
                ..Parse::vname_color_legend()
            },
        };
        let gdi = self.new_graph_description(gf, pa, bits, times)?;
        if gf != Gf::Stack {
            let element = &self.gdes[gdi];
            element.dprint(|| {
                let mut text = format!(
                    "{RULE}{}  : {}\n{}{}LEGEND: \"{}\"\nSTACK : {}\nSKIPSCALE : {}\n",
                    if gf == Gf::Line { "LINE" } else { "AREA" },
                    pa.arg_orig,
                    element.val_or_vname(),
                    element.colors_text(true),
                    element.legend,
                    i32::from(element.stack),
                    i32::from(element.skipscale)
                );
                if gf == Gf::Line {
                    text.push_str(&format!("WIDTH : {}\n", c_g(element.linewidth)));
                }
                text.push_str(&format!(
                    "XAXIS : {}\nYAXIS : {}\n",
                    element.xaxisidx, element.yaxisidx
                ));
                if gf == Gf::Line && !element.dashes.is_empty() {
                    text.push_str(&format!(
                        "DASHES: {} - {}",
                        element.dashes.len(),
                        c_g(element.dashes[0])
                    ));
                    for dash in &element.dashes[1..] {
                        text.push_str(&format!(", {}", c_g(*dash)));
                    }
                    text.push('\n');
                }
                text.push_str(RULE);
                text
            });
        }
        if gf == Gf::Stack {
            self.gdes[gdi].stack = true;
            let previous = self.gdes[..=gdi]
                .iter()
                .rev()
                .find(|element| matches!(element.gf, Gf::Line | Gf::Area))
                .map(|element| (element.gf, element.linewidth));
            match previous {
                Some((gf, linewidth)) => {
                    self.gdes[gdi].gf = gf;
                    self.gdes[gdi].linewidth = linewidth;
                }
                None => {
                    return err(format!(
                        "No previous LINE or AREA found for {}",
                        pa.arg_orig
                    ));
                }
            }
            let element = &self.gdes[gdi];
            element.dprint(|| {
                format!(
                    "{RULE}STACK : {}\n{}{}LEGEND: \"{}\"\nSTACK : {}\nWIDTH : {}\nXAXIS : {}\nYAXIS : {}\nDASHES: TODI\n{RULE}",
                    pa.arg_orig,
                    element.val_or_vname(),
                    element.colors_text(true),
                    element.legend,
                    i32::from(element.stack),
                    c_g(element.linewidth),
                    element.xaxisidx,
                    element.yaxisidx
                )
            });
        }
        legend_shift(&mut self.gdes[gdi].legend);
        Ok(())
    }

    fn parse_hvrule(
        &mut self,
        gf: Gf,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let bits = Parse {
            xaxis: true,
            yaxis: true,
            dashes: true,
            ..Parse::vname_color_legend()
        };
        let gdi = self.new_graph_description(gf, pa, bits, times)?;
        let element = &self.gdes[gdi];
        element.dprint(|| {
            format!(
                "{RULE}{} : {}\n{}{}LEGEND: \"{}\"\nDASHES: TODO\nXAXIS : {}\nYAXIS : {}\n{RULE}",
                if gf == Gf::Vrule { "VRULE" } else { "HRULE" },
                pa.arg_orig,
                element.val_or_vname(),
                element.colors_text(true),
                element.legend,
                element.xaxisidx,
                element.yaxisidx
            )
        });
        legend_shift(&mut self.gdes[gdi].legend);
        if let Some(vidx) = self.gdes[gdi].vidx {
            if self.gdes[vidx].gf != Gf::Vdef {
                return err(format!(
                    "Using vname {} of wrong type in line {}\n",
                    self.gdes[gdi].vname, pa.arg_orig
                ));
            }
        }
        Ok(())
    }

    fn parse_gprint(
        &mut self,
        gf: Gf,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let bits = Parse {
            vname: true,
            vnameref: true,
            cf: true,
            format: true,
            strftime_vfmt: true,
            ..Parse::default()
        };
        let gdi = self.new_graph_description(gf, pa, bits, times)?;
        if self.gdes[gdi].vname.is_empty() {
            let Some(first) = pa.first_unused(1) else {
                return err("No positional VNAME");
            };
            let name = truncate_bytes(
                pa.kv[first].keyvalue.as_deref().unwrap_or(""),
                MAX_VNAME_LEN,
            );
            self.gdes[gdi].vname = name.clone();
            match self.find_var(&name) {
                Some(vidx) => self.gdes[gdi].vidx = Some(vidx),
                None => return err(format!("undefined vname {name}")),
            }
        }
        let vidx = self.gdes[gdi].vidx.unwrap_or(0);
        match self.gdes[vidx].gf {
            Gf::Def | Gf::Cdef => {
                if self.gdes[gdi].cf.is_none() {
                    let Some(first) = pa.first_unused(1) else {
                        return err("No positional CDEF");
                    };
                    let value = pa.kv[first].value.clone();
                    match Cf::parse(&value) {
                        Some(cf) => self.gdes[gdi].cf = Some(cf),
                        None => return err(format!("bad CF for DEF/CDEF: {value}")),
                    }
                }
            }
            Gf::Vdef => {}
            _ => {
                return err(format!(
                    "Encountered unknown type variable '{}'",
                    self.gdes[vidx].vname
                ));
            }
        }
        if self.gdes[gdi].format.is_empty() {
            let Some(first) = pa.first_unused(1) else {
                return err("No positional CF/FORMAT");
            };
            self.gdes[gdi].format =
                truncate_bytes(pa.kv[first].keyvalue.as_deref().unwrap_or(""), FMT_LEG_LEN);
        }
        let element = &self.gdes[gdi];
        element.dprint(|| {
            let mut text = format!(
                "{RULE}{} : {}\nVNAME : {} ({})\n",
                if gf == Gf::Gprint { "GPRINT" } else { "PRINT " },
                pa.arg_orig,
                element.vname,
                element.vidx_c()
            );
            if let Some(cf) = element.cf {
                text.push_str(&format!("CF : ({})\n", cf as u32));
            }
            text.push_str(&format!("FORMAT: \"{}\"\n{RULE}", element.legend));
            text
        });
        Ok(())
    }

    fn parse_comment(
        &mut self,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let bits = Parse {
            legend: true,
            ..Parse::default()
        };
        let gdi = self.new_graph_description(Gf::Comment, pa, bits, times)?;
        if self.gdes[gdi].legend.is_empty() {
            let Some(first) = pa.first_unused(1) else {
                return err("No positional CF/FORMAT");
            };
            self.gdes[gdi].legend =
                truncate_bytes(pa.kv[first].keyvalue.as_deref().unwrap_or(""), FMT_LEG_LEN);
        }
        let element = &self.gdes[gdi];
        element.dprint(|| {
            format!(
                "{RULE}COMMENT : {}\nLEGEND  : \"{}\"\n",
                pa.arg_orig, element.legend
            )
        });
        Ok(())
    }

    fn parse_tick(
        &mut self,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let bits = Parse {
            fraction: true,
            ..Parse::vname_color_legend()
        };
        let gdi = self.new_graph_description(Gf::Tick, pa, bits, times)?;
        let element = &self.gdes[gdi];
        element.dprint(|| {
            format!(
                "{RULE}TICK  : {}\nVNAME : {} ({})\n{}{}LEGEND: \"{}\"\nXAXIS : {}\nYAXIS : {}\n{RULE}",
                pa.arg_orig,
                element.vname,
                element.vidx_c(),
                element.colors_text(false),
                if element.cf == Some(Cf::Last) {
                    format!("FRAC  : {}\n", element.vname)
                } else {
                    format!("FRAC  : {}\n", c_g(element.yrule))
                },
                element.legend,
                element.xaxisidx,
                element.yaxisidx
            )
        });
        legend_shift(&mut self.gdes[gdi].legend);
        Ok(())
    }

    fn parse_textalign(
        &mut self,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let gdi = self.new_graph_description(Gf::TextAlign, pa, Parse::default(), times)?;
        let align = pa
            .get("align", 1)
            .or_else(|| pa.first_unused(1).map(|first| pa.kv[first].value.clone()));
        let Some(align) = align else {
            return err("No alignment given");
        };
        self.gdes[gdi].txtalign = match align.as_str() {
            "left" => TextAlign::Left,
            "right" => TextAlign::Right,
            "justified" => TextAlign::Justified,
            "center" => TextAlign::Center,
            _ => return err(format!("Unknown alignment type '{align}'")),
        };
        let element = &self.gdes[gdi];
        element.dprint(|| {
            format!(
                "{RULE}TEXTALIGN : {}\nALIGNMENT : {align} ({})\n{RULE}",
                pa.arg_orig, element.txtalign as u32
            )
        });
        Ok(())
    }

    fn parse_shift(
        &mut self,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let gdi = self.new_graph_description(Gf::Shift, pa, Parse::vname_ref_pos(), times)?;
        let vidx = self.gdes[gdi].vidx.unwrap_or(0);
        match self.gdes[vidx].gf {
            Gf::Def | Gf::Cdef => {
                self.gdes[gdi].dprint(|| "- vname is of type DEF or CDEF, OK\n".to_owned());
            }
            Gf::Vdef => {
                return err(format!(
                    "Cannot shift a VDEF: '{}' in line '{}'\n",
                    self.gdes[vidx].vname, pa.arg_orig
                ));
            }
            _ => {
                return err(format!(
                    "Encountered unknown type variable '{}' in line '{}'",
                    self.gdes[vidx].vname, pa.arg_orig
                ));
            }
        }
        let shift = pa
            .get("shift", 1)
            .or_else(|| pa.first_unused(1).map(|first| pa.kv[first].value.clone()));
        let Some(shift) = shift else {
            return err("No shift given");
        };
        match self.find_var(&shift) {
            Some(shidx) => match self.gdes[shidx].gf {
                Gf::Def | Gf::Cdef => {
                    return err(format!(
                        "Offset cannot be a (C)DEF: '{}' in line '{}'\n",
                        self.gdes[shidx].vname, pa.arg_orig
                    ));
                }
                Gf::Vdef => {
                    self.gdes[gdi].dprint(|| "- vname is of type VDEF, OK\n".to_owned());
                    self.gdes[gdi].shidx = Some(shidx);
                }
                _ => {
                    return err(format!(
                        "Encountered unknown type variable '{}' in line '{}'",
                        self.gdes[vidx].vname, pa.arg_orig
                    ));
                }
            },
            None => {
                let (status, value) = get_long(&shift);
                if status != 0 {
                    return err(format!("error parsing number {shift}"));
                }
                self.gdes[gdi].shval = value;
                self.gdes[gdi].shidx = None;
            }
        }
        let element = &self.gdes[gdi];
        element.dprint(|| {
            let shift_by = match element.shidx {
                Some(shidx) => format!("SHIFTBY : {} ({shidx})\n", self.gdes[shidx].vname),
                None => format!("SHIFTBY : {}\n", element.shval),
            };
            format!(
                "{RULE}SHIFT   : {}\nVNAME   : {} ({vidx})\n{shift_by}{RULE}",
                pa.arg_orig, self.gdes[vidx].vname
            )
        });
        Ok(())
    }

    fn parse_xport(
        &mut self,
        pa: &mut ParsedArgs,
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        let gdi = self.new_graph_description(Gf::Xport, pa, Parse::vname_color_legend(), times)?;
        // A numeric XPORT leaves vidx at -1 and RRDtool then inspects the
        // element before the array; treat the constant as unexported.
        let Some(vidx) = self.gdes[gdi].vidx else {
            return Ok(());
        };
        match self.gdes[vidx].gf {
            Gf::Def | Gf::Cdef => {
                let element = &self.gdes[gdi];
                element.dprint(|| {
                    format!(
                        "- vname is of type DEF or CDEF, OK\n{RULE}LINE  : {}\nVNAME : {} ({vidx})\nLEGEND: \"{}\"\n{RULE}",
                        pa.arg_orig, element.vname, element.legend
                    )
                });
                Ok(())
            }
            Gf::Vdef => err(format!(
                "Cannot shift a VDEF: '{}' in line '{}'\n",
                self.gdes[vidx].vname, pa.arg_orig
            )),
            _ => err(format!(
                "Encountered unknown type variable '{}' in line '{}'",
                self.gdes[vidx].vname, pa.arg_orig
            )),
        }
    }

    /// Port of `rrd_graph_script`: parse each graph element in order.
    pub fn graph_script(
        &mut self,
        args: &[String],
        times: TimeResolver<'_>,
    ) -> Result<(), ScriptError> {
        for arg in args {
            let mut pa = ParsedArgs::parse(arg)?;
            let cmd = pa.get("cmd", 255).or_else(|| pa.get("pos0", 255));
            let Some(cmd) = cmd else {
                return err(format!("no command set in argument {}", pa.arg_orig));
            };
            let gf = match gf_conv(&cmd) {
                Some(gf) => gf,
                None if cmd.starts_with("LINE") => {
                    pa.kv.push(KeyValue {
                        keyvalue: None,
                        key: "linewidth".to_owned(),
                        value: cmd[4..].to_owned(),
                        flag: 0,
                    });
                    Gf::Line
                }
                None => {
                    return err(format!(
                        "'{cmd}' is not a valid function name in {}",
                        pa.arg_orig
                    ));
                }
            };
            match gf {
                Gf::Def => self.parse_def(&mut pa, times)?,
                Gf::Cdef | Gf::Vdef => self.parse_cvdef(gf, &mut pa, times)?,
                Gf::Line | Gf::Area | Gf::Stack => self.parse_line_area(gf, &mut pa, times)?,
                Gf::Print | Gf::Gprint => self.parse_gprint(gf, &mut pa, times)?,
                Gf::Comment => self.parse_comment(&mut pa, times)?,
                Gf::Hrule | Gf::Vrule => self.parse_hvrule(gf, &mut pa, times)?,
                Gf::Tick => self.parse_tick(&mut pa, times)?,
                Gf::TextAlign => self.parse_textalign(&mut pa, times)?,
                Gf::Shift => self.parse_shift(&mut pa, times)?,
                Gf::Xport => self.parse_xport(&mut pa, times)?,
            }
            if let Some(unused) = pa.unused() {
                return err(format!(
                    "Unused Arguments \"{unused}\" in command : {}",
                    pa.arg_orig
                ));
            }
        }
        Ok(())
    }
}

/// `strtod`-style prefix conversion for dash lists: the value and the bytes
/// consumed (0 when nothing converts).
fn strtod_prefix(text: &str) -> (f64, usize) {
    let end = text.find(',').unwrap_or(text.len());
    match get_double(&text[..end]) {
        Some(value) => (value, end),
        None => (0.0, 0),
    }
}

const RULE: &str = "=================================\n";

/// `%g` through the C library, as the parser's debug output prints it.
fn c_g(value: f64) -> String {
    let mut buffer = [0 as libc::c_char; 64];
    let length =
        unsafe { libc::snprintf(buffer.as_mut_ptr(), buffer.len(), c"%g".as_ptr(), value) };
    let length = usize::try_from(length).unwrap_or(0).min(buffer.len() - 1);
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), length) };
    String::from_utf8_lossy(bytes).into_owned()
}

impl GraphElement {
    /// `dprintf`: parser diagnostics for `debug=1`.
    fn dprint(&self, text: impl FnOnce() -> String) {
        if self.debug & 1 != 0 {
            eprint!("{}", text());
        }
    }

    fn vidx_c(&self) -> i64 {
        self.vidx.map_or(-1, |index| index as i64)
    }

    fn val_or_vname(&self) -> String {
        match self.vidx {
            None if self.gf == Gf::Vrule => format!("VAL   : {}\n", self.xrule),
            None => format!("VAL   : {}\n", c_g(self.yrule)),
            Some(index) => format!("VNAME : {} ({index})\n", self.vname),
        }
    }

    fn colors_text(&self, second: bool) -> String {
        let channels = |color: Option<[u8; 4]>, unset: f64| match color {
            Some(rgba) => rgba.map(|channel| f64::from(channel) / 255.0),
            None => [unset, unset, unset, 0.0],
        };
        let mut text = String::new();
        let [r, g, b, a] = channels(self.color, 0.0);
        text.push_str(&format!(
            "COLOR : r={} g={} b={} a={}\n",
            c_g(r),
            c_g(g),
            c_g(b),
            c_g(a)
        ));
        if second {
            let [r, g, b, a] = channels(self.color2, rrd_nan());
            text.push_str(&format!(
                "COLOR2: r={} g={} b={} a={}\n",
                c_g(r),
                c_g(g),
                c_g(b),
                c_g(a)
            ));
        }
        text
    }
}

/// `legend_shift`: two spaces before a non-empty legend for the colour box.
fn legend_shift(legend: &mut String) {
    if !legend.is_empty() {
        legend.insert_str(0, "  ");
    }
}

/// Port of `vdef_parse`.
fn vdef_parse(gdes: &mut GraphElement, text: &str) -> Result<(), ScriptError> {
    // The scan sets are matched by the host sscanf: Apple's libc collates
    // `[A-Z]` ranges by locale, so lowercase letters can match there.
    let c_text = std::ffi::CString::new(text).unwrap_or_default();
    let mut double_str = [0 as libc::c_char; 41];
    let mut func_buf = [0 as libc::c_char; 30];
    let mut n: libc::c_int = 0;
    unsafe {
        libc::sscanf(
            c_text.as_ptr(),
            c"%40[0-9.e+-],%29[A-Z]%n".as_ptr(),
            double_str.as_mut_ptr(),
            func_buf.as_mut_ptr(),
            &mut n,
        );
    }
    let c_str = |buffer: &[libc::c_char]| unsafe {
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    let (param, func) = match get_double(&c_str(&double_str)) {
        Some(param) => (param, c_str(&func_buf)),
        None => {
            let mut n: libc::c_int = 0;
            unsafe {
                libc::sscanf(
                    c_text.as_ptr(),
                    c"%29[A-Z]%n".as_ptr(),
                    func_buf.as_mut_ptr(),
                    &mut n,
                );
            }
            if usize::try_from(n).ok() != Some(text.len()) {
                return err(format!(
                    "Unknown function string '{text}' in VDEF '{}'",
                    gdes.vname
                ));
            }
            (rrd_nan(), c_str(&func_buf))
        }
    };
    let func = func.as_str();
    let Some(op) = VdefFunction::parse(func) else {
        return err(format!(
            "Unknown function '{func}' in VDEF '{}'\n",
            gdes.vname
        ));
    };
    gdes.vf.op = Some(op);
    match op {
        VdefFunction::Percent | VdefFunction::PercentNan => {
            if param.is_nan() {
                return err(format!(
                    "Function '{func}' needs parameter in VDEF '{}'\n",
                    gdes.vname
                ));
            }
            if !(0.0..=100.0).contains(&param) {
                return err(format!(
                    "Parameter '{param:.6}' out of range in VDEF '{}'\n",
                    gdes.vname
                ));
            }
            gdes.vf.param = param;
        }
        _ => {
            if !param.is_nan() {
                return err(format!(
                    "Function '{func}' needs no parameter in VDEF '{}'\n",
                    gdes.vname
                ));
            }
            gdes.vf.param = rrd_nan();
        }
    }
    gdes.vf.val = rrd_nan();
    gdes.vf.when = 0;
    gdes.vf.never = true;
    Ok(())
}

/// Port of `rrd_reduce_data` for a single series.
fn reduce_data(
    cf: Cf,
    cur_step: u64,
    start: &mut i64,
    end: &mut i64,
    step: &mut u64,
    data: &mut Vec<f64>,
) -> Result<(), StoreError> {
    let reduce_factor = (*step as f64 / cur_step as f64).ceil() as u64;
    *step = cur_step * reduce_factor;
    let (cur, new_step) = (cur_step as i64, *step as i64);
    let mut row_cnt = (*end - *start) / cur;
    let end_offset = end.rem_euclid(new_step);
    let start_offset = start.rem_euclid(new_step);
    let mut src = 0_usize;
    let mut reduced = Vec::new();
    let factor = reduce_factor as i64;
    if start_offset != 0 {
        *start -= start_offset;
        let skiprows = factor - start_offset / cur;
        src += skiprows as usize;
        reduced.push(rrd_nan());
        row_cnt -= skiprows;
    }
    if end_offset != 0 {
        *end = *end - end_offset + new_step;
        row_cnt -= end_offset / cur;
    }
    if row_cnt % factor != 0 {
        return Err(StoreError::RrdExpression(format!(
            "SANITY CHECK: {row_cnt} rows cannot be reduced by {reduce_factor} \n"
        )));
    }
    while row_cnt >= factor {
        let mut newval = rrd_nan();
        let mut validval = 0_u32;
        for i in 0..reduce_factor as usize {
            let value = data.get(src + i).copied().unwrap_or_else(rrd_nan);
            if value.is_nan() {
                continue;
            }
            validval += 1;
            if newval.is_nan() {
                newval = value;
            } else {
                match cf {
                    Cf::Minimum => newval = if newval < value { newval } else { value },
                    Cf::Failures | Cf::Maximum => {
                        newval = if newval > value { newval } else { value }
                    }
                    Cf::Last => newval = value,
                    _ => newval += value,
                }
            }
        }
        if validval == 0 {
            newval = rrd_nan();
        } else if !matches!(cf, Cf::Minimum | Cf::Failures | Cf::Maximum | Cf::Last) {
            newval /= f64::from(validval);
        }
        reduced.push(newval);
        src += reduce_factor as usize;
        row_cnt -= factor;
    }
    if end_offset != 0 {
        reduced.push(rrd_nan());
    }
    *data = reduced;
    Ok(())
}

/// `rrd_strerror(errno)` for an I/O error.
fn strerror(error: &std::io::Error) -> String {
    match error.raw_os_error() {
        Some(code) => unsafe {
            std::ffi::CStr::from_ptr(libc::strerror(code))
                .to_string_lossy()
                .into_owned()
        },
        None => error.to_string(),
    }
}

fn gcd(mut left: u64, mut right: u64) -> u64 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

/// Called before each DEF fetch with the daemon address that applies to it
/// (the DEF's `daemon=` or the image-wide one), so the caller can flush the
/// file through rrdcached first.
pub type DaemonHook<'a> = &'a mut dyn FnMut(&str, &str) -> Result<(), StoreError>;

impl GraphImage {
    /// Port of `data_fetch`.
    pub fn data_fetch(&mut self, daemon: DaemonHook<'_>) -> Result<(), StoreError> {
        for i in 0..self.gdes.len() {
            if self.gdes[i].gf != Gf::Def {
                continue;
            }
            let element = &self.gdes[i];
            let address = if element.daemon.is_empty() {
                self.daemon_addr.clone()
            } else {
                Some(element.daemon.clone())
            };
            if let Some(address) = address.filter(|address| !address.is_empty()) {
                daemon(&address, &element.rrd)?;
            }
            let cf = element.cf.unwrap_or(Cf::Average);
            let fetched = if element.rrd.starts_with("cb//") {
                // rrd_fetch_fn_cb without a registered callback.
                Err(StoreError::RrdExpression(
                    "use rrd_fetch_cb_register to register your callback prior to calling rrd_fetch_fn_cb"
                        .into(),
                ))
            } else {
                fetch_rrd_file(
                    &element.rrd,
                    cf.as_str(),
                    element.start,
                    element.end,
                    // A zero step asks rrd_fetch_fn for the finest archive.
                    element.step.max(1),
                )
                .map_err(|error| match error {
                    StoreError::Io(io) => StoreError::RrdExpression(format!(
                        "opening '{}': {}",
                        element.rrd,
                        strerror(&io)
                    )),
                    error => error,
                })
            };
            let (mut start, mut end, ft_step, mut data) = match fetched {
                Ok(fetched) => {
                    let index = fetched
                        .data_sources
                        .iter()
                        .position(|name| *name == element.ds_nam);
                    let Some(index) = index.or(self.allow_missing_ds.then_some(usize::MAX)) else {
                        return Err(StoreError::RrdExpression(format!(
                            "No DS called '{}' in '{}'",
                            element.ds_nam, element.rrd
                        )));
                    };
                    let data = fetched
                        .rows
                        .iter()
                        .map(|row| {
                            row.values
                                .get(index)
                                .copied()
                                .flatten()
                                .unwrap_or_else(rrd_nan)
                        })
                        .collect();
                    (fetched.start, fetched.end, fetched.step, data)
                }
                Err(_) if self.allow_missing_ds => {
                    // rrd_fetch_empty
                    let mut step = element.step;
                    if step == 0 {
                        step = ((element.end - element.start) / 100) as u64;
                    }
                    let step_i64 = step as i64;
                    let start = element.start - element.start.rem_euclid(step_i64);
                    let end = element.end + (step_i64 - element.end.rem_euclid(step_i64));
                    let rows = ((end - start) / step_i64 + 1) as usize;
                    (start, end, step, vec![rrd_nan(); rows])
                }
                Err(error) => return Err(error),
            };
            let element = &mut self.gdes[i];
            element.step = element.step.max(self.step);
            if ft_step < element.step {
                let reduce_cf = if element.cf_reduce_set {
                    element.cf_reduce
                } else {
                    cf
                };
                let mut step = element.step;
                reduce_data(
                    reduce_cf, ft_step, &mut start, &mut end, &mut step, &mut data,
                )?;
                element.step = step;
            } else {
                element.step = ft_step;
            }
            element.start = start;
            element.end = end;
            element.data = data;
            element.ds_cnt = 1;
        }
        Ok(())
    }

    /// Port of `data_calc`: SHIFT, VDEF and CDEF in definition order.
    pub fn data_calc(&mut self) -> Result<(), StoreError> {
        let mut stack = RpnStack::default();
        for gdi in 0..self.gdes.len() {
            match self.gdes[gdi].gf {
                Gf::Shift => {
                    let vidx = self.gdes[gdi].vidx.unwrap_or(0);
                    let shift = match self.gdes[gdi].shidx {
                        Some(shidx) => rpn::c_long(self.gdes[shidx].vf.val),
                        None => self.gdes[gdi].shval,
                    };
                    let vdp = &mut self.gdes[vidx];
                    vdp.start -= vdp.shift;
                    vdp.end -= vdp.shift;
                    let step = vdp.step as i64;
                    vdp.shift = if step == 0 {
                        shift
                    } else {
                        shift / step * step
                    };
                    vdp.start += vdp.shift;
                    vdp.end += vdp.shift;
                }
                Gf::Vdef => {
                    self.gdes[gdi].ds_cnt = 0;
                    self.vdef_calc(gdi);
                }
                Gf::Cdef => self.cdef_calc(gdi, &mut stack)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn cdef_calc(&mut self, gdi: usize, stack: &mut RpnStack) -> Result<(), StoreError> {
        let mut rpnp = std::mem::take(&mut self.gdes[gdi].rpnp);
        let (mut start, mut end) = (0_i64, 0_i64);
        let mut steps = Vec::new();
        for node in &mut rpnp {
            if !matches!(node.op, Op::Variable | Op::PrevOther) {
                continue;
            }
            let source = &self.gdes[node.ptr];
            if source.ds_cnt == 0 {
                node.val = source.vf.val;
                node.op = Op::Number;
            } else {
                steps.push(source.step);
                if start < source.start {
                    start = source.start;
                }
                if end == 0 || end > source.end {
                    end = source.end;
                }
                node.data = 0;
                node.step = source.step as i64;
            }
        }
        for node in &mut rpnp {
            if matches!(node.op, Op::Variable | Op::PrevOther) {
                let source = &self.gdes[node.ptr];
                let diff = start - source.start;
                if diff > 0 {
                    node.data += (diff / source.step as i64) as isize;
                }
            }
        }
        if steps.is_empty() {
            self.gdes[gdi].rpnp = rpnp;
            return Err(StoreError::RrdExpression(
                "rpn expressions without DEF or CDEF variables are not supported".into(),
            ));
        }
        let step = steps.into_iter().reduce(gcd).unwrap_or(0);
        let rows = usize::try_from((end - start) / step as i64).unwrap_or(0);
        let mut output = vec![0.0; rows];
        let gdes = &self.gdes;
        let series = |index: usize| gdes[index].data.as_slice();
        let mut now = start + step as i64;
        let mut dataidx = 0;
        let result = loop {
            if now > end {
                break Ok(());
            }
            if dataidx >= rows {
                break Ok(());
            }
            if let Err(error) = rpn::rpn_calc(
                &mut rpnp,
                stack,
                now,
                &mut output,
                dataidx as i32,
                step as i32,
                &series,
            ) {
                break Err(error);
            }
            dataidx += 1;
            now += step as i64;
        };
        let element = &mut self.gdes[gdi];
        element.ds_cnt = 1;
        element.start = start;
        element.end = end;
        element.step = step;
        element.data = output;
        element.rpnp = rpnp;
        result
    }

    /// Port of `vdef_calc` over the source element's own window and step.
    fn vdef_calc(&mut self, gdi: usize) {
        let vidx = self.gdes[gdi].vidx.unwrap_or(0);
        let source = &self.gdes[vidx];
        let steps = if source.step == 0 {
            0
        } else {
            usize::try_from((source.end - source.start) / source.step as i64).unwrap_or(0)
        };
        let mut values = source.data.clone();
        values.resize(steps.max(values.len()), rrd_nan());
        values.truncate(steps);
        let vf = self.gdes[gdi].vf;
        let percentile = vf
            .op
            .filter(|op| matches!(op, VdefFunction::Percent | VdefFunction::PercentNan))
            .map(|_| vf.param);
        let result = vf.op.and_then(|op| {
            evaluate_vdef(op, percentile, &values, source.start, source.step.max(1)).ok()
        });
        let vf = &mut self.gdes[gdi].vf;
        match result {
            Some(result) => {
                vf.val = result.value;
                vf.when = result.timestamp.unwrap_or(0);
                vf.never = result.timestamp.is_none();
            }
            None => {
                vf.val = rrd_nan();
                vf.when = 0;
                vf.never = true;
            }
        }
    }

    /// The value half of `print_calc` for the PRINT/GPRINT element `i`:
    /// a VDEF's value, or the element's CF over its source's own series.
    pub fn print_value(&self, i: usize) -> f64 {
        let element = &self.gdes[i];
        let vidx = element.vidx.unwrap_or(0);
        let source = &self.gdes[vidx];
        if source.gf == Gf::Vdef {
            return source.vf.val;
        }
        let max_ii = if source.step == 0 {
            0
        } else {
            usize::try_from((source.end - source.start) / source.step as i64).unwrap_or(0)
        };
        let cf = element.cf.unwrap_or(Cf::Average);
        let mut printval = rrd_nan();
        let mut validsteps = 0_u64;
        for value in source.data.iter().take(max_ii).copied() {
            if !value.is_finite() {
                continue;
            }
            if printval.is_nan() {
                printval = value;
                validsteps += 1;
                continue;
            }
            match cf {
                Cf::Minimum => printval = if printval < value { printval } else { value },
                Cf::Failures | Cf::Maximum => {
                    printval = if printval > value { printval } else { value }
                }
                Cf::Last => printval = value,
                _ => {
                    validsteps += 1;
                    printval += value;
                }
            }
        }
        if (cf == Cf::Average || cf > Cf::Last) && validsteps > 1 {
            printval /= validsteps as f64;
        }
        printval
    }

    /// Port of `rrd_xport_fn` after `data_fetch`/`data_calc`. `dolines`
    /// also exports LINE/AREA/STACK series, as graph XML/JSON/CSV do.
    pub fn xport(&self, dolines: bool) -> Result<XportData, StoreError> {
        let columns: Vec<usize> = self
            .gdes
            .iter()
            .enumerate()
            .filter(|(_, element)| match element.gf {
                Gf::Line | Gf::Area | Gf::Stack => dolines && !is_numeric(&element.vname),
                Gf::Xport => !is_numeric(&element.vname),
                _ => false,
            })
            .map(|(index, _)| index)
            .collect();
        if columns.is_empty() {
            return Err(StoreError::RrdExpression(
                "no XPORT found, nothing to do".into(),
            ));
        }
        let step = columns
            .iter()
            .map(|index| self.gdes[self.gdes[*index].vidx.unwrap_or(0)].step)
            .reduce(gcd)
            .unwrap_or(0);
        if step == 0 {
            return Err(StoreError::RrdExpression("xport step is zero".into()));
        }
        let step_i64 = step as i64;
        let start = self.start - self.start.rem_euclid(step_i64);
        let mut end = self.end - self.end.rem_euclid(step_i64);
        if self.end > end {
            end += step_i64;
        }
        let row_cnt = usize::try_from((end - start) / step_i64).unwrap_or(0);
        let mut rows = Vec::with_capacity(row_cnt);
        for dst_row in 0..row_cnt {
            let now = start + dst_row as i64 * step_i64;
            let row = columns
                .iter()
                .map(|index| self.value_at(self.gdes[*index].vidx.unwrap_or(0), now))
                .collect();
            rows.push(row);
        }
        Ok(XportData {
            start,
            end,
            step,
            columns: columns.clone(),
            legends: columns
                .iter()
                .map(|index| self.gdes[*index].legend.clone())
                .collect(),
            rows,
        })
    }
}

impl GraphImage {
    /// The value of element `vidx` for the xport row starting at `now`.
    pub fn value_at(&self, vidx: usize, now: i64) -> f64 {
        let source = &self.gdes[vidx];
        if source.step == 0 {
            return rrd_nan();
        }
        // C division truncates toward zero, so a row up to one step before
        // the source start still reads index 0.
        let source_step = source.step as i64;
        let idx = (now - source.start) / source_step;
        let count = (source.end - source.start) / source_step;
        if idx >= 0 && idx < count {
            source
                .data
                .get(idx as usize)
                .copied()
                .unwrap_or_else(rrd_nan)
        } else {
            rrd_nan()
        }
    }
}

/// `rrd_xport_fn` output.
#[derive(Debug, Clone)]
pub struct XportData {
    pub start: i64,
    pub end: i64,
    pub step: u64,
    /// Graph element index of each column.
    pub columns: Vec<usize>,
    pub legends: Vec<String>,
    pub rows: Vec<Vec<f64>>,
}

/// `is_numeric` from `rrd_xport.c`.
pub fn is_numeric(text: &str) -> bool {
    let bytes = text.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    if first != b'-' && first != b'.' && !first.is_ascii_digit() {
        return false;
    }
    if !bytes[1..]
        .iter()
        .all(|byte| byte.is_ascii_digit() || *byte == b'.')
    {
        return false;
    }
    bytes.iter().filter(|byte| **byte == b'.').count() <= 1
}
