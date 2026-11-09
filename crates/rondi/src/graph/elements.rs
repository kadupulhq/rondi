//! Reading one element argument into an [`Element`] (GR-40 to GR-59, GR-95, GR-96).

use super::fields::{Choice, Fields, Pair};
use super::{
    Cf, DefSpec, Element, Graph, Kind, Offset, TextAlign, TimeResolver, ValueFormatter, VdefSpec,
};
use crate::rpn::{Expression, RpnError};
use crate::rrd_binary::rrd_nan;
use crate::vdef::VdefFunction;

/// Legend text and print formats are cut to this many bytes (GR-66,
/// format-strings.md FS-7).
const MOST_TEXT_BYTES: usize = 200;

/// The dash pattern of a bare `dashes` flag.
const DEFAULT_DASHES: [f64; 2] = [5.0, 5.0];

/// The gradient height of an AREA without `gradheight=`.
const DEFAULT_GRADIENT_HEIGHT: f64 = 50.0;

/// The earliest start a DEF may fetch from: 1980-01-01 (time-spec.md
/// TS-61).
const EARLIEST_FETCH: i64 = 315_360_000;

/// The most bytes of a VDEF parameter and of its function name.
const MOST_PARAMETER_BYTES: usize = 40;
const MOST_FUNCTION_BYTES: usize = 29;

type Parsed = Result<Element, String>;

/// Reads `text` and appends its element to `graph`.
pub(super) fn parse(graph: &mut Graph, text: &str, times: TimeResolver<'_>) -> Result<(), String> {
    let (name, mut fields) = Fields::parse(text)?;
    let keys: &[&str] = match name.as_str() {
        "DEF" => &[
            "vname", "rrd", "ds", "cf", "step", "start", "end", "reduce", "daemon",
        ],
        "CDEF" | "VDEF" => &["vname", "rpn"],
        "SHIFT" => &["vname", "shift"],
        "XPORT" => &["vname", "legend", "color"],
        "PRINT" | "GPRINT" => &["vname", "cf", "format", "vformatter", "strftime"],
        "COMMENT" => &["legend"],
        "TEXTALIGN" => &["align"],
        "HRULE" | "VRULE" => &["vname", "legend", "color", "dashes", "dash-offset"],
        "TICK" => &["vname", "legend", "color", "fraction"],
        "AREA" => &[
            "vname",
            "legend",
            "color",
            "gradheight",
            "skipscale",
            "stack",
        ],
        "STACK" => &["vname", "legend", "color"],
        _ if name.starts_with("LINE") => &[
            "vname",
            "legend",
            "color",
            "dashes",
            "dash-offset",
            "skipscale",
            "stack",
        ],
        _ => &[],
    };
    fields.bind(keys);
    let debug = debug_level(&mut fields)?;
    let element = match name.as_str() {
        "DEF" => def(graph, &mut fields, times, debug),
        "CDEF" => cdef(graph, &mut fields),
        "VDEF" => vdef(graph, &mut fields),
        "SHIFT" => shift(graph, &mut fields, text),
        "XPORT" => xport(graph, &mut fields, text),
        "PRINT" => print(graph, &mut fields, Kind::Print),
        "GPRINT" => print(graph, &mut fields, Kind::Gprint),
        "COMMENT" => comment(&mut fields),
        "TEXTALIGN" => text_align(&mut fields),
        "HRULE" => rule(graph, &mut fields, text, Kind::Hrule),
        "VRULE" => rule(graph, &mut fields, text, Kind::Vrule),
        "TICK" => tick(graph, &mut fields),
        "AREA" => drawing(graph, &mut fields, Kind::Area, 0.0, false),
        "STACK" => stack(graph, &mut fields, text),
        _ => match name.strip_prefix("LINE") {
            Some(width) => drawing(graph, &mut fields, Kind::Line, line_width(width)?, false),
            None => Err(format!("'{name}' is not a valid function name in {text}")),
        },
    }?;
    fields.finish()?;
    let mut element = element;
    graph.debug.push_str(&std::mem::take(&mut element.debug));
    graph.elements.push(element);
    Ok(())
}

/// `debug=<integer>` on any element (GR-96).
fn debug_level(fields: &mut Fields<'_>) -> Result<i64, String> {
    match fields.named("debug") {
        None => Ok(0),
        Some(text) => integer(&text).ok_or_else(|| format!("Bad debug value: {text}")),
    }
}

/// An integer with an optional sign and nothing else.
fn integer(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn element(kind: Kind) -> Element {
    Element {
        kind,
        line_width: 1.0,
        value: rrd_nan(),
        ..Element::default()
    }
}

/// `LINE<width>`: a non-negative number, 1 when absent (GR-50).
fn line_width(text: &str) -> Result<f64, String> {
    if text.is_empty() {
        return Ok(1.0);
    }
    match text.trim_start().parse::<f64>() {
        Ok(width) if width >= 0.0 => Ok(width),
        _ => Err(format!("Bad line width: {text}")),
    }
}

fn reuse_check(graph: &Graph, name: &str) -> Result<(), String> {
    match graph.variable(name) {
        Some(_) => Err(format!("trying to reuse vname {name}")),
        None => Ok(()),
    }
}

/// The `<vname>=<definition>` of a DEF, CDEF or VDEF: from `vname=` and
/// the definition key, or else split from the next positional field. A
/// positional field without `=` gives no name.
fn name_and_definition(fields: &mut Fields<'_>, key: &str) -> (Option<String>, Option<String>) {
    let named_definition = fields.named(key);
    match fields.named("vname") {
        Some(name) => (Some(name), named_definition.or_else(|| fields.next_value())),
        None => match fields.next_pair() {
            Some(Pair { key: name, value }) => (name, named_definition.or(Some(value))),
            None => (None, named_definition),
        },
    }
}

/// The consolidation function code a DEF's debug block shows.
fn cf_code(cf: Cf) -> u8 {
    match cf {
        Cf::Average => 0,
        Cf::Minimum => 1,
        Cf::Maximum => 2,
        Cf::Last => 3,
    }
}

/// `DEF:<vname>=<file>:<ds>:<CF>[:step=][:start=][:end=][:reduce=][:daemon=]`
/// (GR-43).
//
// Compatibility: the named options are checked before the positional
// fields: `reduce=`, `step=`, the window, its 1980 bound and its order, and
// only then the data source and the CF.
// Provenance: black-box compatibility test (def-001 to -076, probes of
// error order).
fn def(graph: &Graph, fields: &mut Fields<'_>, times: TimeResolver<'_>, debug: i64) -> Parsed {
    let element_text = fields.element();
    let reduce = match fields.named("reduce") {
        Some(text) => Some(Cf::parse(&text).ok_or_else(|| format!("bad reduce CF: {text}"))?),
        None => None,
    };
    let fetch_step = match fields.named("step") {
        Some(text) => match text.parse::<u64>() {
            Ok(step) if step > 0 && text.bytes().all(|byte| byte.is_ascii_digit()) => step,
            _ => return Err(format!("Bad step value: {text}")),
        },
        None => graph.step,
    };
    let start = fields.named("start");
    let end = fields.named("end");
    let (start, end) = times(start.as_deref(), end.as_deref(), graph.start, graph.end)?;
    // Compatibility: each DEF's own start has the image start's lower
    // bound.
    // Provenance: black-box compatibility test (probes of `start=` below
    // 315360000).
    if start < EARLIEST_FETCH {
        return Err(format!(
            "the first entry to fetch should be after 1980 ({start})"
        ));
    }
    if start > end {
        return Err(format!("start ({start}) should be less than end ({end})"));
    }
    let daemon = fields.named("daemon");
    let named_file = fields.named("rrd");
    let (name, file) = match fields.named("vname") {
        Some(name) => (name, named_file.or_else(|| fields.next_value())),
        None => match fields.next_pair() {
            Some(Pair { key, value }) => (key.unwrap_or_default(), named_file.or(Some(value))),
            None => (String::new(), named_file),
        },
    };
    let file = file.unwrap_or_default();
    let ds = fields
        .value_or_next("ds")
        .ok_or_else(|| format!("No argument for definition of DS in {element_text}"))?;
    let cf_text = fields
        .value_or_next("cf")
        .ok_or_else(|| format!("No argument for definition of CF in {element_text}"))?;
    let cf = Cf::parse(&cf_text).ok_or_else(|| format!("bad CF: {cf_text}"))?;
    let daemon = daemon.filter(|address| !address.is_empty());
    let mut target = Element {
        name: Some(name),
        def: Some(DefSpec {
            file,
            ds,
            cf,
            reduce: reduce.unwrap_or(cf),
            reduce_given: reduce,
            start,
            end,
            fetch_step,
            // GR-47: a `step=` finer than the image step chooses the archive
            // but does not refine the result.
            step: fetch_step.max(graph.step),
            daemon,
        }),
        ..element(Kind::Def)
    };
    if debug != 0 {
        target.debug = def_debug_block(element_text, &target, reduce);
    }
    Ok(target)
}

/// The block a DEF with `debug=` writes to standard error (GR-96).
fn def_debug_block(element_text: &str, target: &Element, reduce: Option<Cf>) -> String {
    let Some(def) = target.def.as_ref() else {
        return String::new();
    };
    let rule = "=================================";
    format!(
        "{rule}\nDEF   : {element_text}\nVNAME : {}\nRRD   : {}\nDS    : {}\nCF    : {}\n\
         START : ({})\nSTEP  : ({})\nEND   : ({})\nREDUCE: ({})\nDAEMON: {}\n{rule}\n",
        target.name.as_deref().unwrap_or_default(),
        def.file,
        def.ds,
        cf_code(def.cf),
        def.start,
        def.fetch_step,
        def.end,
        reduce.map_or(0, cf_code),
        def.daemon.as_deref().unwrap_or_default(),
    )
}

/// `CDEF:<vname>=<rpn>` (GR-45).
fn cdef(graph: &Graph, fields: &mut Fields<'_>) -> Parsed {
    let element_text = fields.element();
    let (name, rpn) = name_and_definition(fields, "rpn");
    let rpn =
        rpn.ok_or_else(|| format!("No argument for definition of vdef/rrd in {element_text}"))?;
    let name = name.unwrap_or_default();
    if !name.is_empty() {
        reuse_check(graph, &name)?;
    }
    // Compatibility: a CDEF may name itself; it then reads its own series,
    // which is still empty, so every value is unknown.
    // Provenance: black-box compatibility test (cdef-019).
    let own = graph.elements.len();
    let lookup = |variable: &str| {
        if variable == name {
            Some(own)
        } else {
            graph.variable(variable)
        }
    };
    let expression = Expression::parse(&rpn, lookup).map_err(|error| error.to_string())?;
    let reads_series = expression
        .variables()
        .any(|index| index == own || graph.elements[index].kind != Kind::Vdef);
    if !reads_series {
        return Err(RpnError::NoVariables.to_string());
    }
    Ok(Element {
        name: Some(name),
        cdef: Some(expression),
        ..element(Kind::Cdef)
    })
}

/// `VDEF:<vname>=<vname>,[<param>,]<FUNCTION>` (GR-46).
fn vdef(graph: &Graph, fields: &mut Fields<'_>) -> Parsed {
    let element_text = fields.element();
    let (name, rpn) = name_and_definition(fields, "rpn");
    let rpn =
        rpn.ok_or_else(|| format!("No argument for definition of vdef/rrd in {element_text}"))?;
    let name = name.unwrap_or_default();
    if !name.is_empty() {
        reuse_check(graph, &name)?;
    }
    let Some((variable, function)) = rpn.split_once(',') else {
        return Err(format!("Comma expected in VDEF definition {rpn}"));
    };
    let source = graph
        .variable(variable)
        .ok_or_else(|| format!("Not a valid vname: {variable} in line {rpn}"))?;
    if graph.elements[source].kind == Kind::Vdef {
        return Err(format!(
            "variable '{variable}' not DEF nor CDEF in VDEF '{rpn}'"
        ));
    }
    let spec = vdef_function(function, &name)?;
    Ok(Element {
        name: Some(name),
        source: Some(source),
        vdef_spec: Some(spec),
        ..element(Kind::Vdef)
    })
}

/// A byte of a VDEF function name.
//
// Compatibility: function names are read from the upper-case letters. On
// macOS the lower-case letters `b` to `z` also continue them; on Linux any
// lower-case letter ends them, like `a`, digits and punctuation do.
// Provenance: black-box compatibility test (vdef-011 to -027, probes of
// every letter after a parameter on the pinned macOS and Linux oracles).
fn function_byte(byte: u8) -> bool {
    byte.is_ascii_uppercase() || (!cfg!(target_os = "linux") && (b'b'..=b'z').contains(&byte))
}

/// The leading function name of `text`, at most [`MOST_FUNCTION_BYTES`].
fn function_word(text: &str) -> &str {
    let len = text
        .bytes()
        .take(MOST_FUNCTION_BYTES)
        .take_while(|&byte| function_byte(byte))
        .count();
    &text[..len]
}

/// A VDEF parameter: a decimal number, possibly followed by an exponent
/// marker with no digits (`95e`, `1e+`), which is ignored.
fn vdef_parameter(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut index = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let whole = bytes[index..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    index += whole;
    let mut fraction = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        fraction = bytes[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        index += fraction;
    }
    if whole + fraction == 0 {
        return None;
    }
    let mut number_end = index;
    if bytes.get(index) == Some(&b'e') {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let digits = bytes[index..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        index += digits;
        if digits > 0 {
            number_end = index;
        }
    }
    if index != bytes.len() {
        return None;
    }
    let value = text[..number_end].parse::<f64>().ok()?;
    value.is_finite().then_some(value)
}

/// The `[<param>,]<FUNCTION>` part of a VDEF.
//
// Compatibility: a part that starts with up to 40 bytes of digits, `.`,
// `e`, `+` and `-` is a parameter form. Without a comma after those bytes
// the function is ''. A parameter that is not a number makes the whole
// part an unknown function string. After the comma the function is the
// leading name and the rest is ignored. Without a parameter the part must
// be a name and nothing else.
// Provenance: black-box compatibility test (vdef-011 to -027,
// rpn_differential VDEF parameter cases, probes of each form).
fn vdef_function(text: &str, name: &str) -> Result<VdefSpec, String> {
    let unknown_string = || format!("Unknown function string '{text}' in VDEF '{name}'");
    let unknown = |word: &str| format!("Unknown function '{word}' in VDEF '{name}'\n");
    let lead = text
        .bytes()
        .take(MOST_PARAMETER_BYTES)
        .take_while(|byte| byte.is_ascii_digit() || b".e+-".contains(byte))
        .count();
    let (parameter, word) = if lead > 0 {
        let Some(after) = text[lead..].strip_prefix(',') else {
            return Err(unknown(""));
        };
        let parameter = vdef_parameter(&text[..lead]).ok_or_else(unknown_string)?;
        (Some(parameter), function_word(after))
    } else {
        let word = function_word(text);
        if word.len() != text.len() {
            return Err(unknown_string());
        }
        (None, word)
    };
    let function = VdefFunction::parse(word).ok_or_else(|| unknown(word))?;
    let percentile = matches!(function, VdefFunction::Percent | VdefFunction::PercentNan);
    let parameter = match (percentile, parameter) {
        (true, None) => {
            return Err(format!(
                "Function '{word}' needs parameter in VDEF '{name}'\n"
            ));
        }
        (true, Some(value)) if !(0.0..=100.0).contains(&value) => {
            return Err(format!(
                "Parameter '{value:.6}' out of range in VDEF '{name}'\n"
            ));
        }
        (false, Some(_)) => {
            return Err(format!(
                "Function '{word}' needs no parameter in VDEF '{name}'\n"
            ));
        }
        (_, value) => value,
    };
    Ok(VdefSpec {
        function,
        parameter,
    })
}

/// `SHIFT:<vname>:<offset>` (GR-55, GR-38).
fn shift(graph: &Graph, fields: &mut Fields<'_>, text: &str) -> Parsed {
    let name = fields
        .value_or_next("vname")
        .ok_or("No argument for definition of vdef/rrd in SHIFT")?;
    let offset = fields.value_or_next("shift").ok_or("No shift given")?;
    let source = graph
        .variable(&name)
        .ok_or_else(|| format!("vname {name} not found"))?;
    if graph.elements[source].kind == Kind::Vdef {
        return Err(format!("Cannot shift a VDEF: '{name}' in line '{text}'\n"));
    }
    let offset = match graph.variable(&offset) {
        Some(index) if graph.elements[index].kind == Kind::Vdef => Offset::Vdef(index),
        Some(_) => {
            return Err(format!(
                "Offset cannot be a (C)DEF: '{offset}' in line '{text}'\n"
            ));
        }
        None => Offset::Seconds(
            shift_seconds(&offset).ok_or_else(|| format!("error parsing number {offset}"))?,
        ),
    };
    Ok(Element {
        source: Some(source),
        offset: Some(offset),
        ..element(Kind::Shift)
    })
}

/// A SHIFT offset in seconds.
//
// Compatibility: leading white space and a sign are accepted, then decimal
// digits and nothing else; a value beyond the 64-bit range is clamped to
// it.
// Provenance: black-box compatibility test (alias_modes SHIFT offsets,
// shift-001 to -034).
fn shift_seconds(text: &str) -> Option<i64> {
    let text = text.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (negative, digits) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let magnitude = digits.bytes().fold(0_i128, |value, digit| {
        (value * 10 + i128::from(digit - b'0')).min(i128::from(i64::MAX) + 1)
    });
    let value = if negative { -magnitude } else { magnitude };
    Some(value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

/// Whether an XPORT name is a number written with digits, a `-` sign and
/// a decimal point only.
fn plain_number(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let mut dots = 0;
    let mut seen_digit = false;
    for byte in digits.bytes() {
        match byte {
            b'0'..=b'9' => seen_digit = true,
            b'.' => dots += 1,
            _ => return false,
        }
    }
    seen_digit && dots <= 1
}

/// `XPORT:<vname>[#color][:legend]` (GR-57, GR-89).
fn xport(graph: &Graph, fields: &mut Fields<'_>, text: &str) -> Parsed {
    let field = fields.value_or_next("vname").ok_or("No positional VNAME")?;
    let legend = fields.text_or_next("legend").unwrap_or_default();
    // Compatibility: a colour after the name, or `color=`, is accepted and
    // not used, valid or not.
    // Provenance: black-box compatibility test (graph_data_differential
    // element grammar).
    let _ = fields.named("color");
    let name = field
        .split_once('#')
        .map_or(field.as_str(), |(name, _)| name);
    let source = match graph.variable(name) {
        Some(index) if graph.elements[index].kind == Kind::Vdef => {
            return Err(format!("Cannot shift a VDEF: '{name}' in line '{text}'\n"));
        }
        Some(index) => Some(index),
        // Compatibility: a number is not a series.
        // Provenance: black-box compatibility test (element-056).
        None if plain_number(name) => {
            return Err(format!(
                "Encountered unknown type variable '' in line '{text}'"
            ));
        }
        // Other spellings of a number (an exponent, a `+` sign, `inf`) export
        // nothing; graph_xport_parity.rs pins this Rondi behavior, which
        // differs from the reference (graph.md, Unsettled questions).
        None if name.parse::<f64>().is_ok() => None,
        None => return Err(format!("{name} is not a vname nor a number")),
    };
    Ok(Element {
        source,
        legend: cut(legend),
        ..element(Kind::Xport)
    })
}

/// The CF of a deprecated PRINT or GPRINT (GR-97).
//
// Compatibility: the archive functions of Holt-Winters data are accepted
// and read as AVERAGE, except FAILURES, which reads as MAX.
// Provenance: black-box compatibility test (graph_data_differential print
// formatters, probes of each name).
fn print_cf(text: &str) -> Option<Cf> {
    Cf::parse(text).or(match text {
        "HWPREDICT" | "MHWPREDICT" | "SEASONAL" | "DEVPREDICT" | "DEVSEASONAL" => Some(Cf::Average),
        "FAILURES" => Some(Cf::Maximum),
        _ => None,
    })
}

/// `PRINT` and `GPRINT`, with a VDEF or with a DEF or CDEF and a CF
/// (GR-58, GR-59).
fn print(graph: &Graph, fields: &mut Fields<'_>, kind: Kind) -> Parsed {
    let strftime = fields.flag_or_named("strftime", "strftime");
    let formatter = match fields.last_of(&[
        Choice::Flag("valstrftime"),
        Choice::Flag("valstrfduration"),
        Choice::Named("vformatter"),
    ]) {
        None => ValueFormatter::Numeric,
        Some((0, _)) => ValueFormatter::Timestamp,
        Some((1, _)) => ValueFormatter::Duration,
        Some((_, value)) => match value.as_deref().unwrap_or_default() {
            "timestamp" => ValueFormatter::Timestamp,
            "duration" => ValueFormatter::Duration,
            other => return Err(format!("Unsupported vformatter: {other}")),
        },
    };
    // Compatibility: a name or CF given by key reports other errors than
    // positional ones.
    // Provenance: black-box compatibility test (alias_modes keyword PRINT
    // cases, probes).
    let source = match fields.named("vname") {
        Some(name) => graph
            .variable(&name)
            .ok_or_else(|| format!("vname {name} not found"))?,
        None => {
            let name = fields.next().ok_or("No positional VNAME")?;
            graph
                .variable(&name)
                .ok_or_else(|| format!("undefined vname {name}"))?
        }
    };
    let named_cf = fields.named("cf");
    let cf = if graph.elements[source].kind == Kind::Vdef {
        None
    } else {
        Some(match named_cf {
            Some(text) => print_cf(&text).ok_or_else(|| format!("bad CF: {text}"))?,
            None => {
                let text = fields.next_value().ok_or("No positional CDEF")?;
                print_cf(&text).ok_or_else(|| format!("bad CF for DEF/CDEF: {text}"))?
            }
        })
    };
    let format = fields
        .named("format")
        .filter(|format| !format.is_empty())
        .or_else(|| fields.next())
        .ok_or("No positional CF/FORMAT")?;
    Ok(Element {
        source: Some(source),
        format: cut(format),
        strftime,
        formatter,
        cf,
        ..element(kind)
    })
}

/// `COMMENT:<text>` (GR-59).
fn comment(fields: &mut Fields<'_>) -> Parsed {
    let text = fields
        .named("legend")
        .filter(|text| !text.is_empty())
        .or_else(|| fields.next())
        .ok_or("No positional CF/FORMAT")?;
    Ok(Element {
        legend: cut(text),
        ..element(Kind::Comment)
    })
}

/// `TEXTALIGN:left|right|center|justified` (GR-56).
fn text_align(fields: &mut Fields<'_>) -> Parsed {
    let word = fields.value_or_next("align").ok_or("No alignment given")?;
    let align = match word.as_str() {
        "left" => TextAlign::Left,
        "right" => TextAlign::Right,
        "center" => TextAlign::Center,
        "justified" => TextAlign::Justified,
        _ => return Err(format!("Unknown alignment type '{word}'")),
    };
    Ok(Element {
        align,
        ..element(Kind::TextAlign)
    })
}

/// The colours of a drawing element; `Err(())` means an invalid colour.
type Colors = Result<Option<([u8; 4], Option<[u8; 4]>)>, ()>;

/// The `<value>[#color[#color]]` field of a drawing element: the variable or
/// number, and the colours.
struct Value {
    name: String,
    colors: Colors,
}

fn value_field(field: &str) -> Value {
    match field.split_once('#') {
        None => Value {
            name: field.to_owned(),
            colors: Ok(None),
        },
        Some((name, colors)) => Value {
            name: name.to_owned(),
            colors: parse_colors(colors).map(Some).ok_or(()),
        },
    }
}

/// The value field of a drawing element and its colours, `color=`
/// replacing those of the field.
//
// Compatibility: `color=` takes one colour without `#`.
// Provenance: black-box compatibility test (graph_data_differential image
// size, probes of `color=`).
fn value_and_colors(fields: &mut Fields<'_>) -> Result<Value, String> {
    let field = fields.value_or_next("vname").ok_or("No positional VNAME")?;
    let mut value = value_field(&field);
    if let Some(color) = fields.named("color") {
        value.colors = parse_color(&color)
            .map(|color| Some((color, None)))
            .ok_or(());
    }
    Ok(value)
}

/// One or two `#`-separated colours of 3, 4, 6 or 8 hexadecimal digits
/// (GR-50); the leading `#` is already removed.
fn parse_colors(text: &str) -> Option<([u8; 4], Option<[u8; 4]>)> {
    let (first, second) = match text.split_once('#') {
        Some((first, second)) => (first, Some(second)),
        None => (text, None),
    };
    let second = match second {
        Some(second) => Some(parse_color(second)?),
        None => None,
    };
    Some((parse_color(first)?, second))
}

fn parse_color(hex: &str) -> Option<[u8; 4]> {
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let digit = |index: usize| u8::from_str_radix(&hex[index..=index], 16).ok();
    let pair = |index: usize| u8::from_str_radix(&hex[index..index + 2], 16).ok();
    match hex.len() {
        3 | 4 => {
            let mut color = [255; 4];
            for (index, slot) in color.iter_mut().enumerate().take(hex.len()) {
                *slot = digit(index)? * 17;
            }
            Some(color)
        }
        6 | 8 => {
            let mut color = [255; 4];
            for (index, slot) in color.iter_mut().enumerate().take(hex.len() / 2) {
                *slot = pair(index * 2)?;
            }
            Some(color)
        }
        _ => None,
    }
}

/// Text as stored: cut to [`MOST_TEXT_BYTES`] at a character boundary.
fn cut(mut text: String) -> String {
    if text.len() > MOST_TEXT_BYTES {
        let mut end = MOST_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The legend a drawing element shows: two spaces for the colour box and
/// the text; nothing for an empty text or an invalid colour (GR-50, GR-66).
fn drawing_legend(text: Option<String>, colors_valid: bool) -> String {
    match text {
        Some(text) if colors_valid && !text.is_empty() => cut(format!("  {text}")),
        _ => String::new(),
    }
}

/// Resolves the variable or number a drawing element draws.
fn resolve(graph: &Graph, name: &str) -> Result<(Option<usize>, f64), String> {
    if let Some(index) = graph.variable(name) {
        return Ok((Some(index), rrd_nan()));
    }
    match name.parse::<f64>() {
        Ok(value) if !name.eq_ignore_ascii_case("nan") => Ok((None, value)),
        _ => Err(format!("{name} is not a vname nor a number")),
    }
}

/// `dashes[=list]` and `dash-offset=n` (GR-50).
fn dash_fields(fields: &mut Fields<'_>, target: &mut Element) -> Result<(), String> {
    match fields.named("dashes") {
        Some(list) => {
            target.dashes = list
                .split(',')
                .map(|item| {
                    item.parse::<f64>()
                        .map_err(|_| format!("Could not parse number: {item}"))
                })
                .collect::<Result<_, _>>()?;
        }
        None if fields.flag("dashes") => target.dashes = DEFAULT_DASHES.to_vec(),
        None => {}
    }
    if let Some(offset) = fields.named("dash-offset") {
        target.dash_offset = offset
            .parse::<f64>()
            .map_err(|_| format!("Could not parse dash-offset: {offset}"))?;
    }
    Ok(())
}

/// `LINE` and `AREA` (GR-50 to GR-52).
fn drawing(graph: &Graph, fields: &mut Fields<'_>, kind: Kind, width: f64, stack: bool) -> Parsed {
    let mut target = element(kind);
    target.line_width = width;
    target.stack = fields.flag_or_named("STACK", "stack") || stack;
    target.skip_scale = fields.flag_or_named("skipscale", "skipscale");
    if kind == Kind::Line {
        dash_fields(fields, &mut target)?;
    } else {
        target.gradient_height = DEFAULT_GRADIENT_HEIGHT;
        if let Some(height) = fields.named("gradheight") {
            target.gradient_height = height
                .parse::<f64>()
                .map_err(|_| format!("Bad gradheight: {height}"))?;
        }
    }
    let value = value_and_colors(fields)?;
    (target.source, target.value) = resolve(graph, &value.name)?;
    let legend = fields.text_or_next("legend");
    colored(&mut target, value.colors, legend);
    Ok(target)
}

/// Stores the colours and the legend they allow.
fn colored(target: &mut Element, colors: Colors, legend: Option<String>) {
    if let Ok(Some((first, second))) = colors {
        target.color = Some(first);
        target.color2 = second;
    }
    target.legend = drawing_legend(legend, colors.is_ok());
}

/// The deprecated `STACK:<vname>[#color][:legend]` (GR-51).
fn stack(graph: &Graph, fields: &mut Fields<'_>, text: &str) -> Parsed {
    let field = fields.value_or_next("vname").ok_or("No positional VNAME")?;
    let mut value = value_field(&field);
    let (source, number) = resolve(graph, &value.name)?;
    // Compatibility: the variable is checked first, then `color=`, which
    // must be valid here, then the earlier LINE or AREA.
    // Provenance: black-box compatibility test (probes of STACK with
    // `color=` and without an earlier LINE or AREA).
    if let Some(color) = fields.named("color") {
        let color =
            parse_color(&color).ok_or("STACK should already be turned into LINE or AREA here")?;
        value.colors = Ok(Some((color, None)));
    }
    let previous = graph
        .elements
        .iter()
        .rev()
        .find(|element| matches!(element.kind, Kind::Line | Kind::Area))
        .ok_or_else(|| format!("No previous LINE or AREA found for {text}"))?;
    let mut target = element(previous.kind);
    target.line_width = previous.line_width;
    target.gradient_height = previous.gradient_height;
    target.stack = true;
    (target.source, target.value) = (source, number);
    let legend = fields.text_or_next("legend");
    colored(&mut target, value.colors, legend);
    Ok(target)
}

/// `TICK:<vname>#color:<fraction>[:legend]` (GR-53).
fn tick(graph: &Graph, fields: &mut Fields<'_>) -> Parsed {
    let value = value_and_colors(fields)?;
    let fraction = fields
        .value_or_next("fraction")
        .ok_or("No positional FRACTION")?;
    let fraction = fraction
        .parse::<f64>()
        .map_err(|_| format!("error parsing number {}", value.name))?;
    let mut target = element(Kind::Tick);
    (target.source, _) = resolve(graph, &value.name)?;
    // The TICK fraction takes the place of a rule value.
    target.value = fraction;
    let legend = fields.text_or_next("legend");
    colored(&mut target, value.colors, legend);
    Ok(target)
}

/// `HRULE:<value>#color[:legend[:dashes]]` and `VRULE:<time>#color...`
/// (GR-54).
fn rule(graph: &Graph, fields: &mut Fields<'_>, text: &str, kind: Kind) -> Parsed {
    let mut target = element(kind);
    dash_fields(fields, &mut target)?;
    let value = value_and_colors(fields)?;
    match graph.variable(&value.name) {
        Some(index) if graph.elements[index].kind == Kind::Vdef => target.source = Some(index),
        Some(_) => {
            return Err(format!(
                "Using vname {} of wrong type in line {text}\n",
                value.name
            ));
        }
        None => {
            let number = value
                .name
                .parse::<f64>()
                .map_err(|_| format!("{} is not a vname nor a number", value.name))?;
            if kind == Kind::Hrule {
                target.value = number;
            } else {
                target.time = number as i64;
            }
        }
    }
    let legend = fields.text_or_next("legend");
    colored(&mut target, value.colors, legend);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(args: &[&str]) -> Result<Graph, String> {
        let mut graph = Graph::new(1_000_000_000, 1_000_001_000, 10);
        let times = |_: Option<&str>, _: Option<&str>, start: i64, end: i64| Ok((start, end));
        for arg in args {
            parse(&mut graph, arg, &times)?;
        }
        Ok(graph)
    }

    fn error(args: &[&str]) -> String {
        parse_all(args).unwrap_err()
    }

    const DEF: &str = "DEF:x=f.rrd:x:AVERAGE";

    #[test]
    fn definitions() {
        let graph = parse_all(&[DEF, "DEF:z=f.rrd:step=60:x:MAX", "CDEF:c=x,2,*"]).unwrap();
        let def = graph.elements[1].def.as_ref().unwrap();
        assert_eq!(
            (def.step, def.cf, def.reduce),
            (60, Cf::Maximum, Cf::Maximum)
        );
        assert_eq!(
            error(&["DEF:x"]),
            "No argument for definition of DS in DEF:x"
        );
        assert_eq!(
            error(&["DEF:x=f:x"]),
            "No argument for definition of CF in DEF:x=f:x"
        );
        assert_eq!(error(&["DEF:x=f:x:avg"]), "bad CF: avg");
        assert_eq!(error(&["DEF:x=f:x:MIN:step=2m"]), "Bad step value: 2m");
        assert_eq!(error(&["DEF:x=f:x:MIN:reduce=X"]), "bad reduce CF: X");
        assert_eq!(
            error(&["DEF:x=f:x:MIN:step=1:step=2"]),
            "Unused Arguments \"step=1\" in command : DEF:x=f:x:MIN:step=1:step=2"
        );
        assert_eq!(error(&[DEF, "CDEF:x=x"]), "trying to reuse vname x");
        assert_eq!(error(&[DEF, "CDEF:c"]), "don't understand 'c'");
        assert_eq!(
            error(&[DEF, "VDEF:v=x,AVERAGE", "CDEF:c=v,2,*"]),
            "rpn expressions without DEF or CDEF variables are not supported"
        );
        assert!(parse_all(&[DEF, "CDEF:c=c,2,*"]).is_ok());
    }

    #[test]
    fn vdef_functions() {
        let vdef = |text: &str| error(&[DEF, &format!("VDEF:v={text}")]);
        assert!(parse_all(&[DEF, "VDEF:v=x,95,PERCENT"]).is_ok());
        assert_eq!(vdef("x"), "Comma expected in VDEF definition x");
        assert_eq!(
            vdef("nope,AVERAGE"),
            "Not a valid vname: nope in line nope,AVERAGE"
        );
        assert_eq!(vdef("x,BOGUS"), "Unknown function 'BOGUS' in VDEF 'v'\n");
        assert_eq!(vdef("x,2,*"), "Unknown function '' in VDEF 'v'\n");
        assert_eq!(
            vdef("x,average"),
            "Unknown function string 'average' in VDEF 'v'"
        );
        assert_eq!(
            vdef("x,AVERAGE,2,*"),
            "Unknown function string 'AVERAGE,2,*' in VDEF 'v'"
        );
        assert_eq!(
            vdef("x,PERCENT"),
            "Function 'PERCENT' needs parameter in VDEF 'v'\n"
        );
        assert_eq!(
            vdef("x,101,PERCENT"),
            "Parameter '101.000000' out of range in VDEF 'v'\n"
        );
        assert_eq!(
            error(&[DEF, "VDEF:w=x,AVERAGE", "VDEF:v=w,AVERAGE"]),
            "variable 'w' not DEF nor CDEF in VDEF 'w,AVERAGE'"
        );
    }

    #[test]
    fn drawing_elements() {
        let graph = parse_all(&[
            DEF,
            "LINE2:x#ff0000:leg:STACK:dashes=1,2",
            "AREA:x#f00#00ff0080:a:gradheight=10",
            "STACK:x:",
            "TICK:x#f00:0.5:t",
            "LINE1:3#gg0000:dropped",
        ])
        .unwrap();
        let [_, line, area, stack, tick, constant] = graph.elements.as_slice() else {
            panic!("six elements");
        };
        assert_eq!(line.legend, "  leg");
        assert_eq!((line.line_width, line.stack), (2.0, true));
        assert_eq!(line.dashes, vec![1.0, 2.0]);
        assert_eq!(area.color2, Some([0, 255, 0, 128]));
        assert_eq!(area.color, Some([255, 0, 0, 255]));
        assert_eq!(area.gradient_height, 10.0);
        assert_eq!((stack.kind, stack.stack), (Kind::Area, true));
        assert_eq!((tick.value, tick.legend.as_str()), (0.5, "  t"));
        assert_eq!((constant.value, constant.legend.as_str()), (3.0, ""));
        assert_eq!(error(&["LINE1x#f00"]), "Bad line width: 1x#f00");
        assert_eq!(error(&["LINE1"]), "No positional VNAME");
        assert_eq!(error(&["LINE1:nope"]), "nope is not a vname nor a number");
        assert_eq!(
            error(&[DEF, "AREA:x:a:dashes"]),
            "Unused Arguments \"dashes\" in command : AREA:x:a:dashes"
        );
        assert_eq!(
            error(&[DEF, "STACK:x"]),
            "No previous LINE or AREA found for STACK:x"
        );
        // The variable is checked first, then `color=`, then the earlier
        // LINE or AREA.
        assert_eq!(error(&["STACK:x"]), "x is not a vname nor a number");
        assert_eq!(
            error(&[DEF, "STACK:x:color=zz"]),
            "STACK should already be turned into LINE or AREA here"
        );
        let graph = parse_all(&[DEF, "AREA:x", "STACK:x:color=00ff00:L"]).unwrap();
        assert_eq!(graph.elements[2].color, Some([0, 255, 0, 255]));
        assert_eq!(graph.elements[2].legend, "  L");
        assert_eq!(error(&[DEF, "TICK:x#f00"]), "No positional FRACTION");
        assert_eq!(error(&[DEF, "TICK:x#f00:a"]), "error parsing number x");
        assert_eq!(
            error(&[DEF, "LINE1:x:a:dashes=1,"]),
            "Could not parse number: "
        );
    }

    #[test]
    fn rules_prints_and_text() {
        let graph = parse_all(&[
            DEF,
            "VDEF:v=x,MAXIMUM",
            "HRULE:v#f00:a\\:b",
            "VRULE:1000.5",
            "PRINT:x:MAX:%lf",
            "GPRINT:v:%H:strftime",
            "PRINT:v:%lf:valstrfduration",
            "COMMENT:c",
            "TEXTALIGN:right",
            "SHIFT:x:v",
            "XPORT:x:leg",
        ])
        .unwrap();
        let elements = &graph.elements;
        assert_eq!(
            (elements[2].source, elements[2].legend.as_str()),
            (Some(1), "  a:b")
        );
        assert_eq!(elements[3].time, 1000);
        assert_eq!(elements[4].cf, Some(Cf::Maximum));
        assert!(elements[5].strftime);
        assert_eq!(elements[6].formatter, ValueFormatter::Duration);
        assert_eq!(elements[8].align, TextAlign::Right);
        assert_eq!(elements[9].offset, Some(Offset::Vdef(1)));
        assert_eq!(elements[10].legend, "leg");
        assert_eq!(
            error(&[DEF, "HRULE:x#f00"]),
            "Using vname x of wrong type in line HRULE:x#f00\n"
        );
        assert_eq!(error(&[DEF, "PRINT:x:%lf"]), "bad CF for DEF/CDEF: %lf");
        assert_eq!(error(&["PRINT:nope:%lf"]), "undefined vname nope");
        assert_eq!(error(&["COMMENT"]), "No positional CF/FORMAT");
        assert_eq!(error(&["TEXTALIGN:LEFT"]), "Unknown alignment type 'LEFT'");
        assert_eq!(error(&["TEXTALIGN"]), "No alignment given");
        assert_eq!(error(&[DEF, "SHIFT:x"]), "No shift given");
        assert_eq!(error(&[DEF, "SHIFT:x:1e2"]), "error parsing number 1e2");
        assert_eq!(
            error(&["SHIFT"]),
            "No argument for definition of vdef/rrd in SHIFT"
        );
        assert_eq!(
            error(&[DEF, "VDEF:v=x,LAST", "XPORT:v"]),
            "Cannot shift a VDEF: 'v' in line 'XPORT:v'\n"
        );
        assert_eq!(
            error(&["FOO:x"]),
            "'FOO' is not a valid function name in FOO:x"
        );
        assert_eq!(cut("é".repeat(150)).len(), 200);
    }
}
