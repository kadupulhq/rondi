//! The timestamp of an `update` value written with `:` (TS-67, TS-68).

use std::fmt;

/// An update time with its sub-second part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateTimestamp {
    pub seconds: i64,
    /// 0 to 999999.
    pub microseconds: u32,
}

impl fmt::Display for UpdateTimestamp {
    /// The form sent to rrdcached: whole seconds, with a six-digit fraction
    /// only when there is one.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.microseconds == 0 {
            write!(f, "{}", self.seconds)
        } else {
            write!(f, "{}.{:06}", self.seconds, self.microseconds)
        }
    }
}

/// Parses `N` or a decimal number of seconds. A negative value counts back
/// from `now`. Errors carry the decimal parser's message.
///
/// Compatibility: `N` is sent to rrdcached as whole seconds.
/// Provenance: black-box compatibility test (the text RRDtool writes to an
/// rrdcached socket).
pub fn parse_update_timestamp(text: &str, now: f64) -> Result<UpdateTimestamp, String> {
    if text == "N" {
        return Ok(UpdateTimestamp {
            seconds: crate::numeric_cast::c_long(now.floor()),
            microseconds: 0,
        });
    }
    let mut value =
        crate::rrd_number::rrd_strtodbl(text, "error while parsing time in get_time_from_reading")?;
    if value < 0.0 {
        value += now;
    }
    let seconds = crate::numeric_cast::c_long(value.floor());
    // The fraction is kept to the microsecond (TS-68).
    let fraction = crate::numeric_cast::c_long((value - seconds as f64) * 1e6);
    Ok(UpdateTimestamp {
        seconds,
        microseconds: u32::try_from(fraction.clamp(0, 999_999)).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whole_and_fractional_seconds() {
        let parsed = parse_update_timestamp("1000000100.5", 0.0).unwrap();
        assert_eq!(
            (parsed.seconds, parsed.microseconds),
            (1_000_000_100, 500_000)
        );
        assert_eq!(parsed.to_string(), "1000000100.500000");
        let parsed = parse_update_timestamp("1e9", 0.0).unwrap();
        assert_eq!(parsed.to_string(), "1000000000");
        assert_eq!(
            parse_update_timestamp("+1000000100", 0.0).unwrap().seconds,
            1_000_000_100
        );
    }

    #[test]
    fn now_and_negative_values() {
        let now = 1_700_000_000.25;
        assert_eq!(
            parse_update_timestamp("N", now).unwrap(),
            UpdateTimestamp {
                seconds: 1_700_000_000,
                microseconds: 0
            }
        );
        assert_eq!(
            parse_update_timestamp("-100", now).unwrap().seconds,
            1_699_999_900
        );
    }

    #[test]
    fn rejects_trailing_text_and_never_panics() {
        assert!(parse_update_timestamp("1000000100+1d", 0.0).is_err());
        assert!(parse_update_timestamp("noon", 0.0).is_err());
        assert!(parse_update_timestamp("", 0.0).is_err());
        for text in ["nan", "inf", "-inf", "1e400", "-1e400"] {
            let _ = parse_update_timestamp(text, 1.0);
        }
    }
}
