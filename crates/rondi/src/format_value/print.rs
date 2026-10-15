//! The value format of PRINT and GPRINT items (docs/compatibility/format-strings.md
//! FS-7 to FS-23).

use super::field::Field;
use super::host::host_number;
use super::scaled::scaled_number;
use super::si::{UnitRequest, UnitScale};
use super::spec::NumberSpec;

/// The expression a PRINT or GPRINT value format must match, as RRDtool
/// reports it in its error message.
pub const PRINT_FORMAT_PATTERN: &str =
    "^(?:[^%]+|%%)*%[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG](?:[^%]+|%%)*(?:%[sS])?(?:[^%]+|%%)*$";

/// A value format that did not match [`PRINT_FORMAT_PATTERN`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid format string '{field}' (should match '{PRINT_FORMAT_PATTERN}')")]
pub struct InvalidPrintFormat {
    /// The field after the `%S` rewrite.
    pub field: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Piece {
    Text(String),
    Number(NumberSpec),
    Prefix,
}

/// Which printf produces the digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Printer<'a> {
    /// The host C library rules, with the locale's decimal point.
    Host { decimal_point: &'a str },
    /// RRDtool's own printf (xport PRINT entries).
    Scaled,
}

/// A checked PRINT or GPRINT value format.
#[derive(Debug, Clone, PartialEq)]
pub struct PrintFormat {
    pieces: Vec<Piece>,
    unit: Option<UnitRequest>,
    field: String,
}

/// The text of one formatted value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintText {
    /// The formatted text, with long runs kept counted.
    pub field: Field,
    /// False when RRDtool's own printf failed on the number; the text then
    /// ends where that conversion started.
    pub complete: bool,
}

impl PrintFormat {
    /// Checks a value format. The first `%S` becomes `%s` before the check,
    /// and [`PrintFormat::field`] returns that rewritten text, which the
    /// item keeps (FS-8).
    pub fn parse(field: &str) -> Result<Self, InvalidPrintFormat> {
        let mut text = field.to_owned();
        let shared = match text.find("%S") {
            Some(at) => {
                text.replace_range(at..at + 2, "%s");
                true
            }
            None => false,
        };
        let pieces = match parse_pieces(&text) {
            Some(pieces) => pieces,
            None => return Err(InvalidPrintFormat { field: text }),
        };
        // Compatibility: scaling is triggered by "%s" anywhere in the
        // rewritten format, even inside "%%s", which then prints as "%s".
        // Provenance: black-box compatibility test (PRINT:v:%lf %%S prints
        // "2.500000 %s" for 2500).
        let unit = if shared {
            Some(UnitRequest::Shared)
        } else if text.contains("%s") {
            Some(UnitRequest::Own)
        } else {
            None
        };
        Ok(Self {
            pieces,
            unit,
            field: text,
        })
    }

    /// The format after the `%S` rewrite.
    pub fn field(&self) -> &str {
        &self.field
    }

    /// Formats `value`, scaling it first when the format asks for an SI
    /// prefix.
    pub fn render(&self, value: f64, scale: &mut UnitScale, printer: Printer<'_>) -> PrintText {
        let (value, symbol) = match self.unit {
            Some(request) => scale.apply(value, request),
            None => (value, ' '),
        };
        let mut field = Field::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(text) => field.push_str(text),
                Piece::Prefix => {
                    let mut buffer = [0; 4];
                    field.push_str(symbol.encode_utf8(&mut buffer));
                }
                Piece::Number(spec) => match printer {
                    Printer::Host { decimal_point } => {
                        field.append(host_number(spec, value, decimal_point));
                    }
                    Printer::Scaled => match scaled_number(spec, value) {
                        Some(number) => field.append(number),
                        None => {
                            return PrintText {
                                field,
                                complete: false,
                            };
                        }
                    },
                },
            }
        }
        PrintText {
            field,
            complete: true,
        }
    }
}

/// Splits a format into pieces if it has the shape of
/// [`PRINT_FORMAT_PATTERN`]: text and `%%`, one `%[flag][width][.prec]l`
/// conversion with at most one flag, text, at most one `%s`/`%S`, text.
fn parse_pieces(text: &str) -> Option<Vec<Piece>> {
    let bytes = text.as_bytes();
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut number = false;
    let mut prefix = false;
    let mut at = 0;
    while at < bytes.len() {
        let next = text[at..]
            .find('%')
            .map_or(bytes.len(), |offset| at + offset);
        literal.push_str(&text[at..next]);
        if next == bytes.len() {
            break;
        }
        let after = &bytes[next + 1..];
        if after.first() == Some(&b'%') {
            literal.push('%');
            at = next + 2;
            continue;
        }
        if !number {
            let used = number_conversion(after)?;
            let (spec, _) = NumberSpec::parse_prefix(&after[..used], false)?;
            flush(&mut pieces, &mut literal);
            pieces.push(Piece::Number(spec));
            number = true;
            at = next + 1 + used;
        } else if !prefix && matches!(after.first(), Some(b's' | b'S')) {
            flush(&mut pieces, &mut literal);
            pieces.push(Piece::Prefix);
            prefix = true;
            at = next + 2;
        } else {
            return None;
        }
    }
    flush(&mut pieces, &mut literal);
    number.then_some(pieces)
}

fn flush(pieces: &mut Vec<Piece>, literal: &mut String) {
    if !literal.is_empty() {
        pieces.push(Piece::Text(std::mem::take(literal)));
    }
}

/// Length of `[-+ 0#]?[0-9]*(?:[.][0-9]+)?l[eEfFgG]` at the start of
/// `bytes`, if it is there.
fn number_conversion(bytes: &[u8]) -> Option<usize> {
    let mut at = usize::from(matches!(
        bytes.first(),
        Some(b'-' | b'+' | b' ' | b'0' | b'#')
    ));
    at += digits(&bytes[at..]);
    if bytes.get(at) == Some(&b'.') {
        let count = digits(&bytes[at + 1..]);
        if count == 0 {
            return None;
        }
        at += 1 + count;
    }
    if bytes.get(at) != Some(&b'l') {
        return None;
    }
    matches!(
        bytes.get(at + 1),
        Some(b'e' | b'E' | b'f' | b'F' | b'g' | b'G')
    )
    .then_some(at + 2)
}

fn digits(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

#[cfg(test)]
mod tests {
    use super::{PRINT_FORMAT_PATTERN, PrintFormat, Printer};
    use crate::format_value::si::UnitScale;

    const HOST: Printer<'static> = Printer::Host { decimal_point: "." };

    fn render(format: &str, value: f64) -> String {
        let format = PrintFormat::parse(format).unwrap();
        let mut scale = UnitScale::new(1000);
        let text = format.render(value, &mut scale, HOST);
        text.field.to_string_within(1 << 20).unwrap()
    }

    // FS-9 accepted and rejected examples.
    #[test]
    fn checks_the_documented_shape() {
        for good in [
            "%lf",
            "%6.2lf %s",
            "%00lf",
            "%lfe",
            "%%%lf%%",
            "%-lf",
            "%#.3lE",
            "a%lgb%Sc",
            "%lf%%s",
            "%%S %lf",
        ] {
            assert!(PrintFormat::parse(good).is_ok(), "{good:?}");
        }
        for bad in [
            "%d",
            "%f",
            "%e",
            "%s",
            "%lf %lf",
            "%llf",
            "%Lf",
            "%*lf",
            "%.lf",
            "%-+lf",
            "%lf%",
            "%%",
            "",
            "text",
            "%s %lf",
            "%lf %s %s",
            "%lf %S %S",
            "%lf %x",
        ] {
            assert!(PrintFormat::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn error_names_the_rewritten_field() {
        let error = PrintFormat::parse("%lf %S %S").unwrap_err();
        assert_eq!(error.field, "%lf %s %S");
        assert_eq!(
            error.to_string(),
            format!("invalid format string '%lf %s %S' (should match '{PRINT_FORMAT_PATTERN}')")
        );
        assert_eq!(PrintFormat::parse("%S").unwrap_err().field, "%s");
    }

    #[test]
    fn rewrite_is_kept_for_the_item() {
        let format = PrintFormat::parse("%lf %S").unwrap();
        assert_eq!(format.field(), "%lf %s");
    }

    #[test]
    fn literal_text_and_percent_pairs() {
        assert_eq!(render("%%%lf%%", 1.0), "%1.000000%");
        assert_eq!(render("x%lfy\\n", 1.0), "x1.000000y\\n");
        assert_eq!(render("%lf %s", 1.0), "1.000000  ");
        assert_eq!(render("%lf %%S", 2500.0), "2.500000 %s");
        assert_eq!(render("%%s%lf", 2500.0), "%s2.500000");
        assert_eq!(render("%lf %%%s", 2500.0), "2.500000 %k");
    }

    #[test]
    fn scaled_printer_stops_at_a_failed_conversion() {
        let format = PrintFormat::parse("abc%lfdef").unwrap();
        let mut scale = UnitScale::new(1000);
        let text = format.render(1e21, &mut scale, Printer::Scaled);
        assert!(!text.complete);
        assert_eq!(text.field.to_string_within(64).unwrap(), "abc");
        let format = PrintFormat::parse("abc%lf%s|").unwrap();
        let text = format.render(1e21, &mut scale, Printer::Scaled);
        assert!(text.complete);
        assert_eq!(text.field.to_string_within(64).unwrap(), "abc1.000000?|");
    }

    #[test]
    fn huge_width_is_measured_not_built() {
        let format = PrintFormat::parse("%99999999999lf").unwrap();
        let mut scale = UnitScale::new(1000);
        let text = format.render(1.0, &mut scale, HOST);
        assert_eq!(text.field.len(), 99_999_999_999);
    }
}
