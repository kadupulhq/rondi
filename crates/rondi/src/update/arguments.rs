//! Template and argument text of an update (UP-4 to UP-8).

use super::pdp::Time;
use crate::time::{parse_time_spec, parse_update_time_unclamped};

/// One parsed update argument.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Argument {
    pub time: Time,
    /// Reading texts in DS order; `"U"` for a DS the template leaves unnamed.
    pub readings: Vec<String>,
}

/// Why an argument was rejected. Both texts lack the `<file>: ` prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ArgumentError {
    Message(String),
    /// The time parser's own message (TS-67 to TS-70).
    Time(String),
}

/// Maps each template name, in order, to the index of the DS it names
/// (UP-4). `None` when there is no template.
pub(crate) fn template_order(
    template: Option<&str>,
    names: &[&str],
) -> Result<Option<Vec<usize>>, String> {
    let Some(template) = template else {
        return Ok(None);
    };
    template
        .split(':')
        .enumerate()
        .map(|(k, name)| {
            if k >= names.len() {
                return Err("tmplt contains more DS definitions than RRD".to_string());
            }
            names
                .iter()
                .position(|candidate| *candidate == name)
                .ok_or_else(|| format!("unknown DS name '{name}'"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Splits an argument into time text and value text (UP-6).
fn split(argument: &str) -> Option<(&str, &str)> {
    argument
        .split_once('@')
        .or_else(|| argument.split_once(':'))
}

fn parse_time(time_text: &str, at_form: bool, now: Time) -> Result<Time, ArgumentError> {
    if at_form {
        let spec = parse_time_spec(time_text, now.secs)
            .map_err(|error| ArgumentError::Time(format!("ds time: {time_text}: {error}")))?;
        return spec
            .absolute()
            .map(|secs| Time { secs, usec: 0 })
            .ok_or_else(|| {
                ArgumentError::Time(format!(
                    "specifying time relative to the 'start' or 'end' makes no sense here: {time_text}"
                ))
            });
    }
    if time_text == "N" {
        return Ok(now);
    }
    let now_seconds = now.secs as f64 + now.usec as f64 / 1e6;
    let (secs, usec) =
        parse_update_time_unclamped(time_text, now_seconds).map_err(ArgumentError::Time)?;
    // Compatibility: the microseconds are stored as converted, as the bits
    // of a signed value, even outside 0 to 999999.
    // Provenance: black-box compatibility test (csrc_update a19).
    Ok(Time {
        secs,
        usec: usec as u64,
    })
}

/// Parses one argument for a file with `ds_count` data sources (UP-5 to
/// UP-8). The reading count is checked before the time is parsed.
pub(crate) fn parse_argument(
    argument: &str,
    ds_count: usize,
    order: Option<&[usize]>,
    now: Time,
) -> Result<Argument, ArgumentError> {
    let Some((time_text, value_text)) = split(argument) else {
        return Err(ArgumentError::Message(format!(
            "expected timestamp not found in data source from {argument}"
        )));
    };
    let expected = order.map_or(ds_count, <[usize]>::len);
    let mut pieces: Vec<&str> = value_text.splitn(expected + 1, ':').collect();
    if pieces.len() > expected {
        let rest = pieces.pop().unwrap_or_default();
        return Err(ArgumentError::Message(format!(
            "found extra data on update argument: {rest}"
        )));
    }
    if pieces.len() < expected {
        return Err(ArgumentError::Message(format!(
            "expected {expected} data source readings (got {}) from {time_text}",
            pieces.len()
        )));
    }
    let time = parse_time(time_text, argument.contains('@'), now)?;
    let readings = match order {
        None => pieces.iter().map(|text| (*text).to_string()).collect(),
        Some(order) => {
            // Compatibility: a DS named twice takes the later reading.
            // Provenance: black-box compatibility test (vector tpl-last-wins).
            let mut readings = vec!["U".to_string(); ds_count];
            for (&ds, text) in order.iter().zip(&pieces) {
                if let Some(slot) = readings.get_mut(ds) {
                    *slot = (*text).to_string();
                }
            }
            readings
        }
    };
    Ok(Argument { time, readings })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Time = Time { secs: 5, usec: 7 };
    const NAMES: [&str; 2] = ["a", "b"];

    fn message(text: &str) -> ArgumentError {
        ArgumentError::Message(text.to_string())
    }

    #[test]
    fn template_errors() {
        assert_eq!(
            template_order(Some("zz:a:b:a"), &NAMES),
            Err("unknown DS name 'zz'".to_string())
        );
        assert_eq!(
            template_order(Some("a:b:zz"), &NAMES),
            Err("tmplt contains more DS definitions than RRD".to_string())
        );
        assert_eq!(
            template_order(Some(""), &NAMES),
            Err("unknown DS name ''".to_string())
        );
        assert_eq!(template_order(None, &NAMES), Ok(None));
    }

    #[test]
    fn template_mapping() {
        assert_eq!(
            template_order(Some("b:a:b"), &NAMES),
            Err("tmplt contains more DS definitions than RRD".to_string())
        );
        let order = template_order(Some("b:a"), &NAMES).unwrap().unwrap();
        let parsed = parse_argument("100:1:2", 2, Some(&order), NOW).unwrap();
        assert_eq!(parsed.readings, ["2", "1"]);
        let order = template_order(Some("b"), &NAMES).unwrap().unwrap();
        let parsed = parse_argument("100:7", 2, Some(&order), NOW).unwrap();
        assert_eq!(parsed.readings, ["U", "7"]);
        assert_eq!(parsed.time, Time { secs: 100, usec: 0 });
    }

    #[test]
    fn split_rules() {
        assert_eq!(split("1000000014:5@6"), Some(("1000000014:5", "6")));
        assert_eq!(split("1000000014:5"), Some(("1000000014", "5")));
        assert_eq!(split("nosep"), None);
        assert_eq!(
            parse_argument("nosep", 1, None, NOW),
            Err(message(
                "expected timestamp not found in data source from nosep"
            ))
        );
        // The time text is the part before the '@', colon included.
        assert!(matches!(
            parse_argument("1000000014:5@6", 1, None, NOW),
            Err(ArgumentError::Time(text)) if text.starts_with("ds time: 1000000014:5: ")
        ));
    }

    #[test]
    fn at_form_time() {
        let parsed = parse_argument("1000000100@5", 1, None, NOW).unwrap();
        assert_eq!(
            parsed.time,
            Time {
                secs: 1_000_000_100,
                usec: 0
            }
        );
        assert!(matches!(
            parse_argument("foo@5", 1, None, NOW),
            Err(ArgumentError::Time(text)) if text == "ds time: foo: unparsable time: foo"
        ));
        assert!(matches!(
            parse_argument("end-1h@5", 1, None, NOW),
            Err(ArgumentError::Time(text))
                if text == "specifying time relative to the 'start' or 'end' makes no sense here: end-1h"
        ));
    }

    #[test]
    fn count_errors() {
        assert_eq!(
            parse_argument("1000000005:1:2:3:4", 2, None, NOW),
            Err(message("found extra data on update argument: 3:4"))
        );
        assert_eq!(
            parse_argument("1000000005:1:2::", 2, None, NOW),
            Err(message("found extra data on update argument: :"))
        );
        assert_eq!(
            parse_argument("1000000018:U:U:", 2, None, NOW),
            Err(message("found extra data on update argument: "))
        );
        assert_eq!(
            parse_argument("abc:5", 2, None, NOW),
            Err(message("expected 2 data source readings (got 1) from abc"))
        );
    }

    #[test]
    fn time_fraction() {
        let parsed = parse_argument("1000000003.4:1:2", 2, None, NOW).unwrap();
        assert_eq!(
            parsed.time,
            Time {
                secs: 1_000_000_003,
                usec: 399_999
            }
        );
        let parsed = parse_argument("1000000002.999999:1:2", 2, None, NOW).unwrap();
        assert_eq!(
            parsed.time,
            Time {
                secs: 1_000_000_002,
                usec: 999_999
            }
        );
    }

    #[test]
    fn now_is_passed_in() {
        let parsed = parse_argument("N:1:2", 2, None, NOW).unwrap();
        assert_eq!(parsed.time, NOW);
    }
}
