//! The parts of one printf floating-point conversion.

/// Flag characters of a conversion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// `-`: pad on the right.
    pub left: bool,
    /// `+`: always print a sign.
    pub plus: bool,
    /// ` `: print a space where a plus sign would go.
    pub space: bool,
    /// `0`: pad numbers with zeros after the sign.
    pub zero: bool,
    /// `#`: always print the decimal point; `%g` keeps trailing zeros.
    pub alternate: bool,
}

impl Flags {
    fn set(&mut self, flag: u8) -> bool {
        match flag {
            b'-' => self.left = true,
            b'+' => self.plus = true,
            b' ' => self.space = true,
            b'0' => self.zero = true,
            b'#' => self.alternate = true,
            _ => return false,
        }
        true
    }
}

/// How the digits are laid out: `%f`, `%e` or `%g`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notation {
    Fixed,
    Exponent,
    General,
}

/// One floating-point conversion such as `%-12.3lf` or `%0.10e`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumberSpec {
    pub flags: Flags,
    /// Minimum field width; 0 when none was given.
    pub width: u64,
    /// Digits after the point (significant digits for `%g`).
    pub precision: Option<u64>,
    pub notation: Notation,
    /// Upper-case conversion letter: `INF`, `NAN`, `E`.
    pub upper: bool,
}

impl NumberSpec {
    pub const fn new(notation: Notation) -> Self {
        Self {
            flags: Flags {
                left: false,
                plus: false,
                space: false,
                zero: false,
                alternate: false,
            },
            width: 0,
            precision: None,
            notation,
            upper: false,
        }
    }

    /// Parses a whole conversion such as `%.0f` or `%6.2lf`: any flags, a
    /// width, a precision, an optional `l` and one of `eEfFgG`. Digit runs
    /// too long for `u64` saturate.
    pub fn parse(text: &str) -> Option<Self> {
        let (spec, used) = Self::parse_prefix(text.as_bytes().strip_prefix(b"%")?, true)?;
        (used + 1 == text.len()).then_some(spec)
    }

    /// Parses a conversion after its `%`, returning it and the number of
    /// bytes it used. `many_flags` allows more than one flag character.
    pub(crate) fn parse_prefix(bytes: &[u8], many_flags: bool) -> Option<(Self, usize)> {
        let mut flags = Flags::default();
        let mut at = 0;
        while at < bytes.len() && flags.set(bytes[at]) {
            at += 1;
            if !many_flags {
                break;
            }
        }
        let (width, digits) = parse_digits(&bytes[at..]);
        at += digits;
        let mut precision = None;
        if bytes.get(at) == Some(&b'.') {
            let (value, digits) = parse_digits(&bytes[at + 1..]);
            at += 1 + digits;
            precision = Some(value);
        }
        if bytes.get(at) == Some(&b'l') {
            at += 1;
        }
        let letter = *bytes.get(at)?;
        let notation = match letter.to_ascii_lowercase() {
            b'f' => Notation::Fixed,
            b'e' => Notation::Exponent,
            b'g' => Notation::General,
            _ => return None,
        };
        Some((
            Self {
                flags,
                width,
                precision,
                notation,
                upper: letter.is_ascii_uppercase(),
            },
            at + 1,
        ))
    }
}

/// Reads a run of ASCII digits, saturating at `u64::MAX`.
pub(crate) fn parse_digits(bytes: &[u8]) -> (u64, usize) {
    let count = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    let value = bytes[..count].iter().fold(0_u64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'))
    });
    (value, count)
}

#[cfg(test)]
mod tests {
    use super::{Notation, NumberSpec, parse_digits};

    #[test]
    fn parses_whole_conversions() {
        let spec = NumberSpec::parse("%-+12.3lE").unwrap();
        assert!(spec.flags.left && spec.flags.plus && !spec.flags.zero);
        assert_eq!(spec.width, 12);
        assert_eq!(spec.precision, Some(3));
        assert_eq!(spec.notation, Notation::Exponent);
        assert!(spec.upper);

        let spec = NumberSpec::parse("%0.10e").unwrap();
        assert!(spec.flags.zero);
        assert_eq!(spec.width, 0);
        assert_eq!(spec.precision, Some(10));

        let spec = NumberSpec::parse("%.f").unwrap();
        assert_eq!(spec.precision, Some(0));
        assert_eq!(NumberSpec::parse("%g").unwrap().precision, None);
    }

    #[test]
    fn rejects_other_text() {
        for text in ["", "%", "%d", "%5.2", "x%f", "%f ", "%llf", "%s"] {
            assert_eq!(NumberSpec::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn digit_runs_saturate() {
        assert_eq!(parse_digits(b"123x"), (123, 3));
        assert_eq!(parse_digits(b"x"), (0, 0));
        assert_eq!(parse_digits(b"99999999999999999999999"), (u64::MAX, 23));
    }
}
