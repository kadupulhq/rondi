//! Tokenizer for RPN expressions.
//!
//! Tokens are separated by single commas and tried in a fixed order: number,
//! operator, `PREV(name)`, variable. See "Grammar" in
//! `docs/compatibility/rpn.md`.

use super::{Operator, RpnError, Token};
use crate::rrd_binary::rrd_mul_add;

/// Characters a number token may contain.
const NUMBER_CHARS: &str = "0123456789.e+-";
/// Longest accepted number token.
const NUMBER_MAX_LEN: usize = 40;
/// Decimal exponent bounds, after moving the decimal point to the end of the
/// mantissa digits.
const EXPONENT_RANGE: std::ops::RangeInclusive<i64> = -1021..=1024;

pub(super) fn parse(
    text: &str,
    lookup: &dyn Fn(&str) -> Option<usize>,
) -> Result<Vec<Token>, RpnError> {
    if text.is_empty() {
        return Err(RpnError::Empty);
    }
    let mut tokens = Vec::new();
    let mut rest = text;
    loop {
        let (token, len) = token(rest, lookup)?;
        tokens.push(token);
        // A token only succeeds when a comma or the end follows it.
        match rest[len..].strip_prefix(',') {
            // Compatibility: one trailing comma is accepted.
            // Provenance: black-box compatibility test (parse-03, parse-14).
            Some(next) if !next.is_empty() => rest = next,
            _ => return Ok(tokens),
        }
    }
}

/// Reads the token at the start of `rest` and returns it with its length in
/// bytes.
fn token(rest: &str, lookup: &dyn Fn(&str) -> Option<usize>) -> Result<(Token, usize), RpnError> {
    let text = rest.split(',').next().unwrap_or_default();
    let last = text.len() == rest.len();

    // Compatibility: a number must be followed by a comma, so a number in the
    // last position falls through to the other token kinds.
    // Provenance: black-box compatibility test (parse-01, constcdef-01).
    if !last
        && text.len() <= NUMBER_MAX_LEN
        && let Some(value) = number(text)
    {
        return Ok((Token::Number(value), text.len()));
    }
    if let Some(operator) = Operator::from_name(text) {
        return Ok((Token::Operator(operator), text.len()));
    }
    if let Some(token) = previous_of(rest, lookup)? {
        return Ok(token);
    }
    if is_name(text)
        && let Some(element) = lookup(text)
    {
        return Ok((Token::Variable(element), text.len()));
    }
    Err(RpnError::Unrecognized(rest.to_owned()))
}

/// `PREV(name)`. Returns `None` when `rest` does not start with `PREV(` and
/// at least one name character, so the caller tries a variable instead.
///
/// Compatibility: the byte after the name closes the token whatever it is,
/// so `PREV(a,` and `PREV(a]` read as `PREV(a)`. A token not followed by a
/// comma or the end is garbage, reported before the name is looked up; when
/// the name runs to the end of the expression, the lookup comes first.
/// Provenance: black-box compatibility test (parse-19, parse-20, parse-23)
/// and probes; see "Variables" in docs/compatibility/rpn.md.
fn previous_of(
    rest: &str,
    lookup: &dyn Fn(&str) -> Option<usize>,
) -> Result<Option<(Token, usize)>, RpnError> {
    const OPEN: &str = "PREV(";
    let Some(inner) = rest.strip_prefix(OPEN) else {
        return Ok(None);
    };
    let name_len = inner.find(|c| !is_name_char(c)).unwrap_or(inner.len());
    if name_len == 0 {
        return Ok(None);
    }
    let name = &inner[..name_len];
    let resolve = || lookup(name).ok_or_else(|| RpnError::UnknownVariable(name.to_owned()));
    let garbage = || RpnError::TrailingGarbage(rest.to_owned());
    // Byte offsets: the closing byte may be any byte, so the token can end
    // inside a multi-byte character, which is then garbage.
    let len = OPEN.len() + name_len + 1;
    match rest.as_bytes().get(len - 1) {
        None => Err(resolve().map_or_else(|error| error, |_| garbage())),
        Some(_) => match rest.as_bytes().get(len) {
            None | Some(b',') => Ok(Some((Token::PreviousOf(resolve()?), len))),
            Some(_) => Err(garbage()),
        },
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_name(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_name_char)
}

/// Converts a number token, or returns `None` if it is not one.
///
/// The accepted form is an optional sign, digits with at most one decimal
/// point (at least one digit overall), and an optional lower-case `e` with an
/// optional sign and optional digits (`1e` and `1e+` mean 1).
fn number(text: &str) -> Option<f64> {
    if text.is_empty() || !text.chars().all(|c| NUMBER_CHARS.contains(c)) {
        return None;
    }
    let (negative, unsigned) = match text.as_bytes()[0] {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (mantissa, exponent_text) = unsigned.split_once('e').unwrap_or((unsigned, ""));
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if !digits(whole) || !digits(fraction) || whole.len() + fraction.len() == 0 {
        return None;
    }
    let exponent = exponent(exponent_text)?;

    // Compatibility: the decimal exponent, counted after the last mantissa
    // digit, must lie in -1021..=1024; values outside are rejected rather
    // than rounded to zero or infinity (`1e1025` and `0.1e-1021` fail,
    // `1e1024` is inf, `1e-1021` is 0).
    // Provenance: black-box compatibility test (number-11 to number-13);
    // bounds probed against RRDtool 1.11.0, see docs/compatibility/rpn.md.
    let fraction_digits = i64::try_from(fraction.len()).ok()?;
    let decimal_exponent = exponent.checked_sub(fraction_digits)?;
    if !EXPONENT_RANGE.contains(&decimal_exponent) {
        return None;
    }
    // Compatibility: the value is not the correctly rounded one. The digits
    // are accumulated as `m * 10 + digit` in binary64 (fused on aarch64),
    // then scaled by binary powering: for each set bit of the exponent's
    // magnitude, lowest first, multiply (or, for a negative exponent,
    // divide) by 10, 10^2, 10^4, ..., squaring the factor every step. This
    // reproduces RRDtool to the bit on 403 random literals, and explains
    // `1e308,SIN` and the platform split of `0.49999999999999994,ROUND`.
    // Provenance: black-box compatibility test (unaryedge-09, mod-13,
    // rounding-06); see "Numbers" in docs/compatibility/rpn.md.
    let mantissa = whole
        .bytes()
        .chain(fraction.bytes())
        .fold(0.0, |value, digit| {
            rrd_mul_add(value, 10.0, f64::from(digit - b'0'))
        });
    let magnitude = scale_by_power_of_ten(mantissa, decimal_exponent);
    Some(if negative { -magnitude } else { magnitude })
}

fn scale_by_power_of_ten(mut value: f64, exponent: i64) -> f64 {
    let mut factor = 10.0_f64;
    let mut bits = exponent.unsigned_abs();
    while bits != 0 {
        if bits & 1 == 1 {
            if exponent > 0 {
                value *= factor;
            } else {
                value /= factor;
            }
        }
        factor *= factor;
        bits >>= 1;
    }
    value
}

/// The exponent digits after `e`, which may be empty or a bare sign.
fn exponent(text: &str) -> Option<i64> {
    let (negative, magnitude) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    if !magnitude.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // A 40-character token cannot hold an exponent beyond i64, but cap the
    // value anyway: anything this large is out of range.
    let value = magnitude
        .bytes()
        .try_fold(0_i64, |acc, digit| {
            acc.checked_mul(10)?.checked_add(i64::from(digit - b'0'))
        })
        .unwrap_or(i64::MAX / 2);
    Some(if negative { -value } else { value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_the_black_box_grammar() {
        for (text, value) in [
            ("1.5", 1.5),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e3", 1e3),
            ("1e+3", 1e3),
            ("1e-3", 1e-3),
            ("+1", 1.0),
            ("-1", -1.0),
            ("00012", 12.0),
            ("1e", 1.0),
            ("1e+", 1.0),
            ("1e-", 1.0),
            ("1e1024", f64::INFINITY),
            ("-1e1024", f64::NEG_INFINITY),
            ("0.1e1025", f64::INFINITY),
            ("1e-1021", 0.0),
            ("1000e-1021", 0.0),
        ] {
            assert_eq!(number(text), Some(value), "{text}");
        }
        for text in [
            "1E3",
            "1e1025",
            "1e1100",
            "-1e1100",
            "1e-1022",
            "0.1e-1021",
            "1e-1100",
            "1..2",
            "1-2",
            "--1",
            "1.5e2.5",
            "0x10",
            "inf",
            "nan",
            ".",
            "-",
            "+",
            "e5",
            "0e2000",
            "1e5e5",
            "",
        ] {
            assert_eq!(number(text), None, "{text}");
        }
    }

    fn names(name: &str) -> Option<usize> {
        ["a", "b", "a-b", "x1", "MAX"]
            .iter()
            .position(|candidate| *candidate == name)
    }

    fn tokens(text: &str) -> Result<Vec<Token>, RpnError> {
        parse(text, &names)
    }

    #[test]
    fn tokens_resolve_in_order() {
        assert_eq!(
            tokens("a,1.5,+,PREV(b),x1,MAX,a-b,"),
            Ok(vec![
                Token::Variable(0),
                Token::Number(1.5),
                Token::Operator(Operator::Add),
                Token::PreviousOf(1),
                Token::Variable(3),
                // An operator name wins over a variable of the same name.
                Token::Operator(Operator::Max),
                Token::Variable(2),
            ])
        );
        assert_eq!(tokens("a,-,5"), Err(RpnError::Unrecognized("5".into())));
        assert_eq!(
            tokens("a,1,2"),
            Err(RpnError::Unrecognized("2".into())),
            "a trailing number is not a number"
        );
    }

    #[test]
    fn errors_quote_the_remaining_text() {
        let unrecognized = |text: &str| Err(RpnError::Unrecognized(text.into()));
        assert_eq!(tokens(""), Err(RpnError::Empty));
        assert_eq!(tokens(",a"), unrecognized(",a"));
        assert_eq!(tokens("a,,1,+"), unrecognized(",1,+"));
        assert_eq!(tokens("a, 1,+"), unrecognized(" 1,+"));
        assert_eq!(tokens("a,1,+ "), unrecognized("+ "));
        assert_eq!(tokens("a,dup,+"), unrecognized("dup,+"));
        assert_eq!(tokens("a,COUNT(a),+"), unrecognized("COUNT(a),+"));
        assert_eq!(tokens("a,PREV(),+"), unrecognized("PREV(),+"));
        assert_eq!(tokens("a,PREV( a),+"), unrecognized("PREV( a),+"));
        assert_eq!(
            tokens("a,PREV(a,+"),
            Err(RpnError::TrailingGarbage("PREV(a,+".into()))
        );
        assert_eq!(
            tokens("a,PREV(nope)x,+"),
            Err(RpnError::TrailingGarbage("PREV(nope)x,+".into()))
        );
        assert_eq!(
            tokens("a,PREV(nope),+"),
            Err(RpnError::UnknownVariable("nope".into()))
        );
    }

    #[test]
    fn prev_closes_on_any_byte_after_the_name() {
        let garbage = |text: &str| Err(RpnError::TrailingGarbage(text.into()));
        let not_found = || Err(RpnError::UnknownVariable("nope".into()));
        let prev_a = Token::PreviousOf(0);
        let add = Token::Operator(Operator::Add);
        assert_eq!(tokens("PREV(a,,+"), Ok(vec![prev_a, add]));
        assert_eq!(tokens("PREV(a],+"), Ok(vec![prev_a, add]));
        assert_eq!(tokens("PREV(a,"), Ok(vec![prev_a]));
        assert_eq!(tokens("PREV(a"), garbage("PREV(a"));
        assert_eq!(tokens("PREV(nope"), not_found());
        assert_eq!(tokens("PREV(nope,,+"), not_found());
        assert_eq!(tokens("PREV(nope,+"), garbage("PREV(nope,+"));
        assert_eq!(tokens("PREV(a)),+"), garbage("PREV(a)),+"));
        assert_eq!(tokens("PREV(aé,+"), garbage("PREV(aé,+"));
    }
}
