//! Reading one element argument into an [`Element`] (GR-40 to GR-59).

use super::fields::Fields;
use super::{
    Cf, DefSpec, Element, Graph, Kind, Offset, TextAlign, TimeResolver, ValueFormatter, VdefSpec,
};
use crate::rpn::{Expression, RpnError};
use crate::vdef::VdefFunction;

/// Legend text is cut to this many bytes (GR-66).
const MOST_LEGEND_BYTES: usize = 200;

/// The dash pattern of a bare `dashes` flag.
const DEFAULT_DASHES: [f64; 2] = [5.0, 5.0];

/// The gradient height of an AREA without `gradheight=`.
const DEFAULT_GRADIENT_HEIGHT: f64 = 50.0;

type Parsed = Result<Element, String>;

/// Reads `text` and appends its element to `graph`.
pub(super) fn parse(graph: &mut Graph, text: &str, times: TimeResolver<'_>) -> Result<(), String> {
    let (name, rest) = match text.split_once(':') {
        Some((name, rest)) => (name, Some(rest)),
        None => (text, None),
    };
    let mut fields = Fields::new(text, rest);
    let element = match name {
        "DEF" => def(graph, &mut fields, times),
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
    graph.elements.push(element);
    Ok(())
}

fn element(kind: Kind) -> Element {
    Element {
        kind,
        line_width: 1.0,
        value: f64::NAN,
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

/// Splits `name=rest` at the first `=`.
fn definition(field: &str) -> (&str, &str) {
    field.split_once('=').unwrap_or((field, ""))
}

fn reuse_check(graph: &Graph, name: &str) -> Result<(), String> {
    match graph.variable(name) {
        Some(_) => Err(format!("trying to reuse vname {name}")),
        None => Ok(()),
    }
}

/// `DEF:<vname>=<file>:<ds>:<CF>[:step=][:start=][:end=][:reduce=][:daemon=]`
/// (GR-43).
fn def(graph: &Graph, fields: &mut Fields<'_>, times: TimeResolver<'_>) -> Parsed {
    let step = fields.value("step");
    let start = fields.value("start");
    let end = fields.value("end");
    let reduce = fields.value("reduce");
    let daemon = fields.value("daemon");
    let element_text = fields.element();
    let first = fields.next().unwrap_or_default();
    let (name, file) = definition(&first);
    let ds = fields
        .next()
        .ok_or_else(|| format!("No argument for definition of DS in {element_text}"))?;
    let cf_text = fields
        .next()
        .ok_or_else(|| format!("No argument for definition of CF in {element_text}"))?;
    let cf = Cf::parse(&cf_text).ok_or_else(|| format!("bad CF: {cf_text}"))?;
    let step = match step {
        Some(text) => match text.parse::<u64>() {
            Ok(step) if step > 0 && text.bytes().all(|byte| byte.is_ascii_digit()) => step,
            _ => return Err(format!("Bad step value: {text}")),
        },
        None => graph.step,
    };
    let (start, end) = times(start.as_deref(), end.as_deref(), graph.start, graph.end)?;
    if start > end {
        return Err(format!("start ({start}) should be less than end ({end})"));
    }
    let reduce = match reduce {
        Some(text) => Cf::parse(&text).ok_or_else(|| format!("bad reduce CF: {text}"))?,
        None => cf,
    };
    Ok(Element {
        name: Some(name.to_owned()),
        def: Some(DefSpec {
            file: file.to_owned(),
            ds,
            cf,
            reduce,
            start,
            end,
            step,
            daemon: daemon.filter(|address| !address.is_empty()),
        }),
        ..element(Kind::Def)
    })
}

/// `CDEF:<vname>=<rpn>` (GR-45).
fn cdef(graph: &Graph, fields: &mut Fields<'_>) -> Parsed {
    let first = fields.next().unwrap_or_default();
    let Some((name, rpn)) = first.split_once('=') else {
        return Err(RpnError::Unrecognized(first).to_string());
    };
    reuse_check(graph, name)?;
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
    let expression = Expression::parse(rpn, lookup).map_err(|error| error.to_string())?;
    let reads_series = expression
        .variables()
        .any(|index| index == own || graph.elements[index].kind != Kind::Vdef);
    if !reads_series {
        return Err(RpnError::NoVariables.to_string());
    }
    Ok(Element {
        name: Some(name.to_owned()),
        cdef: Some(expression),
        ..element(Kind::Cdef)
    })
}

/// `VDEF:<vname>=<vname>,[<param>,]<FUNCTION>` (GR-46).
fn vdef(graph: &Graph, fields: &mut Fields<'_>) -> Parsed {
    let first = fields.next().unwrap_or_default();
    let (name, rpn) = match first.split_once('=') {
        Some((name, rpn)) => (name, rpn),
        None => ("", first.as_str()),
    };
    if !name.is_empty() {
        reuse_check(graph, name)?;
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
    let spec = vdef_function(function, name)?;
    Ok(Element {
        name: Some(name.to_owned()),
        source: Some(source),
        vdef_spec: Some(spec),
        ..element(Kind::Vdef)
    })
}

/// The `[<param>,]<FUNCTION>` part of a VDEF.
//
// Compatibility: the function is the upper-case word that follows; any
// other text after it, or a word that does not start the text, is "Unknown
// function string"; after a parameter an empty word is "Unknown function ''".
// Provenance: black-box compatibility test (vdef-011 to -027).
fn vdef_function(text: &str, name: &str) -> Result<VdefSpec, String> {
    let (parameter, rest) = match number_prefix(text) {
        Some((value, after)) if after.starts_with(',') => (Some(value), &after[1..]),
        _ => (None, text),
    };
    let word_len = rest
        .bytes()
        .take_while(u8::is_ascii_uppercase)
        .count();
    let (word, after) = rest.split_at(word_len);
    if !after.is_empty() && (parameter.is_none() || !word.is_empty()) {
        return Err(format!(
            "Unknown function string '{text}' in VDEF '{name}'"
        ));
    }
    let function = VdefFunction::parse(word)
        .ok_or_else(|| format!("Unknown function '{word}' in VDEF '{name}'\n"))?;
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
        (true, value) => value,
        (false, _) => None,
    };
    Ok(VdefSpec {
        function,
        parameter,
    })
}

/// The longest prefix of `text` that reads as a number, and the rest.
fn number_prefix(text: &str) -> Option<(f64, &str)> {
    (1..=text.len())
        .rev()
        .filter(|&end| text.is_char_boundary(end))
        .find_map(|end| {
            let prefix = &text[..end];
            let looks_numeric = prefix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || b"+-.eE".contains(&byte));
            looks_numeric
                .then(|| prefix.parse::<f64>().ok())
                .flatten()
                .map(|value| (value, &text[end..]))
        })
}

/// `SHIFT:<vname>:<offset>` (GR-55).
fn shift(graph: &Graph, fields: &mut Fields<'_>, text: &str) -> Parsed {
    let name = fields
        .next()
        .ok_or("No argument for definition of vdef/rrd in SHIFT")?;
    let offset = fields.next().ok_or("No shift given")?;
    let source = graph
        .variable(&name)
        .ok_or_else(|| format!("vname {name} not found"))?;
    if graph.elements[source].kind == Kind::Vdef {
        return Err(format!("Cannot shift a VDEF: '{name}' in line '{text}'\n"));
    }
    let offset = match graph.variable(&offset) {
        Some(index) if graph.elements[index].kind == Kind::Vdef => Offset::Vdef(index),
        _ => Offset::Seconds(integer(&offset)?),
    };
    Ok(Element {
        source: Some(source),
        offset: Some(offset),
        ..element(Kind::Shift)
    })
}

/// A whole number of seconds with an optional sign.
fn integer(text: &str) -> Result<i64, String> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("error parsing number {text}"));
    }
    text.parse::<i64>()
        .map_err(|_| format!("error parsing number {text}"))
}

/// `XPORT:<vname>[:legend]` (GR-57, GR-89).
fn xport(graph: &Graph, fields: &mut Fields<'_>, text: &str) -> Parsed {
    let name = fields.next().ok_or("No positional VNAME")?;
    let legend = fields.next().unwrap_or_default();
    let source = match graph.variable(&name) {
        Some(index) if graph.elements[index].kind == Kind::Vdef => {
            return Err(format!("Cannot shift a VDEF: '{name}' in line '{text}'\n"));
        }
        Some(index) => Some(index),
        // Compatibility: a number is accepted and exports nothing.
        // Provenance: graph_xport_parity.rs (the reference reads outside its
        // element list there).
        None if name.parse::<f64>().is_ok() => None,
        None => return Err(format!("{name} is not a vname nor a number")),
    };
    Ok(Element {
        source,
        legend: cut(legend),
        ..element(Kind::Xport)
    })
}

/// `PRINT` and `GPRINT`, with a VDEF or with a DEF or CDEF and a CF
/// (GR-58, GR-59).
fn print(graph: &Graph, fields: &mut Fields<'_>, kind: Kind) -> Parsed {
    let strftime = fields.flag("strftime");
    let formatter = if fields.flag("valstrftime") {
        ValueFormatter::Timestamp
    } else if fields.flag("valstrfduration") {
        ValueFormatter::Duration
    } else {
        ValueFormatter::Numeric
    };
    let name = fields.next().ok_or("No positional VNAME")?;
    let source = graph
        .variable(&name)
        .ok_or_else(|| format!("undefined vname {name}"))?;
    let missing = "No positional CF/FORMAT";
    let cf = if graph.elements[source].kind == Kind::Vdef {
        None
    } else {
        let text = fields.next().ok_or(missing)?;
        Some(Cf::parse(&text).ok_or_else(|| format!("bad CF for DEF/CDEF: {text}"))?)
    };
    let format = fields.next().ok_or(missing)?;
    Ok(Element {
        source: Some(source),
        format,
        strftime,
        formatter,
        cf,
        ..element(kind)
    })
}

/// `COMMENT:<text>` (GR-59).
fn comment(fields: &mut Fields<'_>) -> Parsed {
    let text = fields.next().ok_or("No positional CF/FORMAT")?;
    Ok(Element {
        legend: cut(text),
        ..element(Kind::Comment)
    })
}

/// `TEXTALIGN:left|right|center|justified` (GR-56).
fn text_align(fields: &mut Fields<'_>) -> Parsed {
    let word = fields.next().ok_or("No alignment given")?;
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

/// The `<value>[#color[#color]]` field of a drawing element: the variable or
/// number, and the colours. `Err(())` in the colours means an invalid colour.
struct Value {
    name: String,
    colors: Result<Option<([u8; 4], Option<[u8; 4]>)>, ()>,
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

/// Legend text as stored: cut to [`MOST_LEGEND_BYTES`] at a character
/// boundary.
fn cut(mut text: String) -> String {
    if text.len() > MOST_LEGEND_BYTES {
        let mut end = MOST_LEGEND_BYTES;
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
        return Ok((Some(index), f64::NAN));
    }
    match name.parse::<f64>() {
        Ok(value) if !name.eq_ignore_ascii_case("nan") => Ok((None, value)),
        _ => Err(format!("{name} is not a vname nor a number")),
    }
}

/// `dashes[=list]` and `dash-offset=n` (GR-50).
fn dash_fields(fields: &mut Fields<'_>, target: &mut Element) -> Result<(), String> {
    match fields.key("dashes") {
        Some(None) => target.dashes = DEFAULT_DASHES.to_vec(),
        Some(Some(list)) => {
            target.dashes = list
                .split(',')
                .map(|item| {
                    item.parse::<f64>()
                        .map_err(|_| format!("Could not parse number: {item}"))
                })
                .collect::<Result<_, _>>()?;
        }
        None => {}
    }
    if let Some(offset) = fields.value("dash-offset") {
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
    target.stack = fields.flag("STACK") || stack;
    target.skip_scale = fields.flag("skipscale");
    if kind == Kind::Line {
        dash_fields(fields, &mut target)?;
    } else {
        target.gradient_height = DEFAULT_GRADIENT_HEIGHT;
        if let Some(height) = fields.value("gradheight") {
            target.gradient_height = height
                .parse::<f64>()
                .map_err(|_| format!("Bad gradheight: {height}"))?;
        }
    }
    let field = fields.next().ok_or("No positional VNAME")?;
    let value = value_field(&field);
    (target.source, target.value) = resolve(graph, &value.name)?;
    let legend = fields.next();
    colored(&mut target, value.colors, legend);
    Ok(target)
}

/// Stores the colours and the legend they allow.
fn colored(
    target: &mut Element,
    colors: Result<Option<([u8; 4], Option<[u8; 4]>)>, ()>,
    legend: Option<String>,
) {
    if let Ok(Some((first, second))) = colors {
        target.color = Some(first);
        target.color2 = second;
    }
    target.legend = drawing_legend(legend, colors.is_ok());
}

/// The deprecated `STACK:<vname>[#color][:legend]` (GR-51).
fn stack(graph: &Graph, fields: &mut Fields<'_>, text: &str) -> Parsed {
    let field = fields.next().ok_or("No positional VNAME")?;
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
    let value = value_field(&field);
    (target.source, target.value) = resolve(graph, &value.name)?;
    let legend = fields.next();
    colored(&mut target, value.colors, legend);
    Ok(target)
}

/// `TICK:<vname>#color:<fraction>[:legend]` (GR-53).
fn tick(graph: &Graph, fields: &mut Fields<'_>) -> Parsed {
    let field = fields.next().ok_or("No positional VNAME")?;
    let fraction = fields.next().ok_or("No positional FRACTION")?;
    let value = value_field(&field);
    let fraction = fraction
        .parse::<f64>()
        .map_err(|_| format!("error parsing number {}", value.name))?;
    let mut target = element(Kind::Tick);
    (target.source, _) = resolve(graph, &value.name)?;
    // The TICK fraction takes the place of a rule value.
    target.value = fraction;
    let legend = fields.next();
    colored(&mut target, value.colors, legend);
    Ok(target)
}

/// `HRULE:<value>#color[:legend[:dashes]]` and `VRULE:<time>#color...`
/// (GR-54).
fn rule(graph: &Graph, fields: &mut Fields<'_>, text: &str, kind: Kind) -> Parsed {
    let mut target = element(kind);
    dash_fields(fields, &mut target)?;
    let field = fields.next().ok_or("No positional VNAME")?;
    let value = value_field(&field);
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
    let legend = fields.next();
    colored(&mut target, value.colors, legend);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(args: &[&str]) -> Result<Graph, String> {
        let mut graph = Graph::new(1000, 2000, 10);
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
        assert_eq!((def.step, def.cf, def.reduce), (60, Cf::Maximum, Cf::Maximum));
        assert_eq!(error(&["DEF:x"]), "No argument for definition of DS in DEF:x");
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
        assert_eq!(vdef("nope,AVERAGE"), "Not a valid vname: nope in line nope,AVERAGE");
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
            error(&["STACK:x"]),
            "No previous LINE or AREA found for STACK:x"
        );
        assert_eq!(error(&[DEF, "TICK:x#f00"]), "No positional FRACTION");
        assert_eq!(error(&[DEF, "TICK:x#f00:a"]), "error parsing number x");
        assert_eq!(error(&[DEF, "LINE1:x:a:dashes=1,"]), "Could not parse number: ");
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
        assert_eq!((elements[2].source, elements[2].legend.as_str()), (Some(1), "  a:b"));
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
