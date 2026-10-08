/// Implements the numeric and `now +/- duration` start forms accepted by the
/// pinned RRDtool create command. The full at-style grammar remains broader.
pub fn parse_rrd_time(value: &str, now: i64) -> Result<i64, Box<dyn std::error::Error>> {
    let normalized = value.replace(['_', ','], " ").to_ascii_lowercase();
    if let Some(timestamp) = parse_rrd_reference_time(&normalized, now) {
        return Ok(timestamp);
    }
    if let Ok(epoch) = normalized.trim().parse::<i64>() {
        return Ok(epoch);
    }

    // A sign begins the at-style offset. Try each occurrence because absolute
    // references may themselves contain separators (for example dates).
    for (index, character) in normalized.char_indices() {
        if !matches!(character, '+' | '-') {
            continue;
        }
        let reference = normalized[..index].trim();
        let offset = normalized[index..]
            .chars()
            .filter(|character| !character.is_ascii_whitespace() && *character != '_')
            .collect::<String>();
        let Some(reference_time) = parse_rrd_reference_time(reference, now) else {
            continue;
        };
        return apply_rrd_offsets(reference_time, &offset);
    }

    Err(format!("unsupported RRDtool time specification: {value}").into())
}

fn parse_rrd_reference_time(value: &str, now: i64) -> Option<i64> {
    let value = value.trim();
    if matches!(value, "now" | "n") {
        return Some(now);
    }
    if value == "epoch" {
        return Some(0);
    }
    if let Some(timestamp) = parse_rrd_absolute_date(value, now) {
        return Some(timestamp);
    }
    let (special_time, rest) = value.split_once(char::is_whitespace)?;
    let hour = match special_time {
        "midnight" => 0,
        "noon" => 12,
        "teatime" => 16,
        _ => return None,
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return set_local_hour(now, hour);
    }
    if let Some(timestamp) = parse_rrd_absolute_date(&format!("{rest} {hour:02}:00"), now) {
        return Some(timestamp);
    }
    let base = set_local_hour(now, hour)?;
    let day_delta = match rest {
        "today" => Some(0),
        "yesterday" => Some(-1),
        "tomorrow" => Some(1),
        _ => parse_weekday(rest).map(|day| {
            let raw = libc::time_t::try_from(base).unwrap_or_default();
            let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
            if unsafe { libc::localtime_r(&raw, &mut local) }.is_null() {
                return 0;
            }
            day - local.tm_wday
        }),
    }?;
    shift_local_calendar(base, CalendarUnit::Days, i64::from(day_delta)).ok()
}

fn parse_weekday(value: &str) -> Option<i32> {
    Some(match value {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

#[cfg(unix)]
fn set_local_hour(timestamp: i64, hour: i32) -> Option<i64> {
    let mut raw = libc::time_t::try_from(timestamp).ok()?;
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&raw, &mut local) }.is_null() {
        return None;
    }
    local.tm_hour = hour;
    local.tm_min = 0;
    local.tm_sec = 0;
    local.tm_isdst = -1;
    raw = unsafe { libc::mktime(&mut local) };
    if raw == -1 {
        return None;
    }
    #[cfg(target_pointer_width = "64")]
    {
        Some(raw)
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        Some(raw as i64)
    }
}

#[cfg(not(unix))]
fn set_local_hour(timestamp: i64, hour: i32) -> Option<i64> {
    Some(timestamp - timestamp.rem_euclid(86_400) + i64::from(hour) * 3_600)
}

#[derive(Clone, Copy)]
enum CalendarUnit {
    Days,
    Months,
    Years,
}

#[cfg(unix)]
fn shift_local_calendar(
    timestamp: i64,
    unit: CalendarUnit,
    amount: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    let mut timestamp = libc::time_t::try_from(timestamp)?;
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&timestamp, &mut local) }.is_null() {
        return Err("RRDtool local time is outside the supported range".into());
    }
    let amount = i32::try_from(amount)?;
    match unit {
        CalendarUnit::Days => {
            local.tm_mday = local.tm_mday.checked_add(amount).ok_or("date overflow")?
        }
        CalendarUnit::Months => {
            local.tm_mon = local.tm_mon.checked_add(amount).ok_or("date overflow")?
        }
        CalendarUnit::Years => {
            local.tm_year = local.tm_year.checked_add(amount).ok_or("date overflow")?
        }
    }
    local.tm_isdst = -1;
    timestamp = unsafe { libc::mktime(&mut local) };
    if timestamp == -1 {
        return Err("RRDtool calendar time is outside the supported range".into());
    }
    #[cfg(target_pointer_width = "64")]
    {
        Ok(timestamp)
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        Ok(timestamp as i64)
    }
}

#[cfg(not(unix))]
fn shift_local_calendar(
    timestamp: i64,
    unit: CalendarUnit,
    amount: i64,
) -> Result<i64, Box<dyn std::error::Error>> {
    let seconds = match unit {
        CalendarUnit::Days => 86_400,
        CalendarUnit::Months => 31 * 86_400,
        CalendarUnit::Years => 366 * 86_400,
    };
    timestamp
        .checked_add(amount.checked_mul(seconds).ok_or("date overflow")?)
        .ok_or_else(|| "date overflow".into())
}

fn apply_rrd_offsets(reference: i64, offsets: &str) -> Result<i64, Box<dyn std::error::Error>> {
    let bytes = offsets.as_bytes();
    let mut index = 0;
    let mut sign = 1_i64;
    let mut previous_unit = "";
    let mut timestamp = reference;
    let mut saw_offset = false;
    while index < bytes.len() {
        if bytes[index] == b'+' || bytes[index] == b'-' {
            sign = if bytes[index] == b'+' { 1 } else { -1 };
            index += 1;
        } else if !saw_offset {
            return Err(format!("invalid RRDtool time offset: {offsets}").into());
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return Err(format!("invalid RRDtool time offset: {offsets}").into());
        }
        let amount = offsets[start..index].parse::<i64>()?;
        let unit_start = index;
        while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
            index += 1;
        }
        let mut unit = &offsets[unit_start..index];
        if unit.is_empty() {
            unit = "s";
        }
        if unit == "m" {
            unit = match previous_unit {
                "d" | "day" | "days" | "w" | "wk" | "week" | "weeks" | "mon" | "month"
                | "months" | "y" | "yr" | "year" | "years" => "mon",
                "s" | "sec" | "second" | "seconds" | "min" | "minute" | "minutes" | "h" | "hr"
                | "hour" | "hours" => "min",
                _ if amount < 6 => "mon",
                _ => "min",
            };
        }
        let signed = amount
            .checked_mul(sign)
            .ok_or("RRDtool time offset overflows")?;
        match unit {
            "s" | "sec" | "second" | "seconds" => {
                timestamp = timestamp
                    .checked_add(signed)
                    .ok_or("time offset overflows")?;
            }
            "m" | "min" | "minute" | "minutes" => {
                timestamp = timestamp
                    .checked_add(signed.checked_mul(60).ok_or("time offset overflows")?)
                    .ok_or("time offset overflows")?;
            }
            "h" | "hr" | "hour" | "hours" => {
                timestamp = timestamp
                    .checked_add(signed.checked_mul(3_600).ok_or("time offset overflows")?)
                    .ok_or("time offset overflows")?;
            }
            "d" | "day" | "days" => {
                timestamp = shift_local_calendar(timestamp, CalendarUnit::Days, signed)?;
            }
            "w" | "wk" | "week" | "weeks" => {
                timestamp = shift_local_calendar(
                    timestamp,
                    CalendarUnit::Days,
                    signed.checked_mul(7).ok_or("date overflow")?,
                )?;
            }
            "mon" | "month" | "months" => {
                timestamp = shift_local_calendar(timestamp, CalendarUnit::Months, signed)?;
            }
            "y" | "yr" | "year" | "years" => {
                timestamp = shift_local_calendar(timestamp, CalendarUnit::Years, signed)?;
            }
            _ => return Err(format!("unsupported RRDtool time unit: {unit}").into()),
        }
        previous_unit = unit;
        saw_offset = true;
    }
    if !saw_offset {
        return Err(format!("invalid RRDtool time offset: {offsets}").into());
    }
    Ok(timestamp)
}

/// RRDtool's at-style parser uses local calendar time plus `mktime`'s DST
/// normalization. Cover common fully specified calendar forms while retaining
/// that system behavior. Month/day names and broader at-style references remain
/// outside this bounded implementation.
#[cfg(unix)]
fn parse_rrd_absolute_date(value: &str, now: i64) -> Option<i64> {
    use std::ffi::CString;

    let normalized = value.replace(['_', ','], " ");
    let input = CString::new(normalized).ok()?;
    const FORMATS: [&[u8]; 16] = [
        b"%Y-%m-%d %H:%M:%S\0",
        b"%Y-%m-%d %H:%M\0",
        b"%Y-%m-%dT%H:%M:%S\0",
        b"%m/%d/%Y %H:%M:%S\0",
        b"%m/%d/%Y %H:%M\0",
        b"%d.%m.%Y %H:%M:%S\0",
        b"%d.%m.%Y %H:%M\0",
        b"%H:%M:%S %Y-%m-%d\0",
        b"%H:%M %Y-%m-%d\0",
        b"%b %d %Y %H:%M:%S\0",
        b"%b %d %Y %H:%M\0",
        b"%B %d %Y %H:%M:%S\0",
        b"%B %d %Y %H:%M\0",
        b"%H:%M %b %d %Y\0",
        b"%H:%M:%S %B %d %Y\0",
        b"%I:%M %p %b %d %Y\0",
    ];
    for format in FORMATS {
        // strptime and mktime use the same local-time and DST rules as RRDtool's
        // rrd_parsetime + mktime path for these calendar forms.
        let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
        let end = unsafe { libc::strptime(input.as_ptr(), format.as_ptr().cast(), &mut tm) };
        if end.is_null() || unsafe { *end } != 0 {
            continue;
        }
        tm.tm_isdst = -1;
        let timestamp = unsafe { libc::mktime(&mut tm) };
        if timestamp != -1 {
            return Some(timestamp as i64);
        }
    }

    // RRDtool preserves the current local time-of-day when a calendar date is
    // given without an explicit time component.
    const DATE_ONLY_FORMATS: [(&[u8], bool); 5] = [
        (b"%B %d %Y\0", true),
        (b"%b %d %Y\0", true),
        (b"%m/%d/%Y\0", false),
        (b"%d.%m.%Y\0", false),
        (b"%Y%m%d\0", false),
    ];
    let mut raw_now = libc::time_t::try_from(now).ok()?;
    for (format, preserve_time) in DATE_ONLY_FORMATS {
        let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
        if unsafe { libc::localtime_r(&raw_now, &mut tm) }.is_null() {
            return None;
        }
        let end = unsafe { libc::strptime(input.as_ptr(), format.as_ptr().cast(), &mut tm) };
        if end.is_null() || unsafe { *end } != 0 {
            continue;
        }
        if !preserve_time {
            tm.tm_hour = 0;
            tm.tm_min = 0;
            tm.tm_sec = 0;
        }
        tm.tm_isdst = -1;
        raw_now = unsafe { libc::mktime(&mut tm) };
        if raw_now != -1 {
            #[cfg(target_pointer_width = "64")]
            return Some(raw_now);
            #[cfg(not(target_pointer_width = "64"))]
            return Some(raw_now as i64);
        }
    }
    None
}

#[cfg(not(unix))]
fn parse_rrd_absolute_date(_value: &str, _now: i64) -> Option<i64> {
    None
}

#[derive(Debug, PartialEq, Eq)]
enum RangeTimeSpec {
    Absolute(i64),
    RelativeToStart(String),
    RelativeToEnd(String),
}

fn parse_range_time_spec(
    value: &str,
    now: i64,
) -> Result<RangeTimeSpec, Box<dyn std::error::Error>> {
    let normalized = value.trim().to_ascii_lowercase();
    for (reference, kind) in [("start", 0_u8), ("end", 1_u8), ("s", 0_u8), ("e", 1_u8)] {
        if let Some(offset) = normalized.strip_prefix(reference)
            && offset.starts_with(['+', '-'])
        {
            if kind == 0 {
                return Ok(RangeTimeSpec::RelativeToStart(offset.to_owned()));
            }
            return Ok(RangeTimeSpec::RelativeToEnd(offset.to_owned()));
        }
    }
    if let Ok(timestamp) = normalized.parse::<i64>() {
        return Ok(RangeTimeSpec::Absolute(if timestamp > 0 {
            timestamp
        } else {
            now.checked_add(timestamp)
                .ok_or("relative fetch time overflows")?
        }));
    }
    Ok(RangeTimeSpec::Absolute(parse_rrd_time(&normalized, now)?))
}

/// Resolve RRDtool's pair-dependent `start`/`end` references. In RRDtool,
/// `start-1d` is based on the resolved end and `end+1d` on the resolved start;
/// references to the same endpoint or mutually relative endpoints are invalid.
pub fn resolve_rrd_range_times(
    start_spec: Option<&str>,
    end_spec: Option<&str>,
    default_start: i64,
    default_end: i64,
    now: i64,
) -> Result<(i64, i64), Box<dyn std::error::Error>> {
    use RangeTimeSpec::{Absolute, RelativeToEnd, RelativeToStart};
    let start_spec = match start_spec {
        Some(value) => parse_range_time_spec(value, now)?,
        None if end_spec.is_none() => Absolute(default_start),
        None => RelativeToEnd(String::from("-24h")),
    };
    let end_spec = end_spec
        .map(|value| parse_range_time_spec(value, now))
        .transpose()?
        .unwrap_or(Absolute(default_end));
    match (start_spec, end_spec) {
        (Absolute(start), Absolute(end)) => Ok((start, end)),
        (RelativeToEnd(offset), Absolute(end)) => Ok((apply_rrd_offsets(end, &offset)?, end)),
        (Absolute(start), RelativeToStart(offset)) => {
            Ok((start, apply_rrd_offsets(start, &offset)?))
        }
        (RelativeToStart(_), _) => {
            Err("the start time cannot be specified relative to itself".into())
        }
        (_, RelativeToEnd(_)) => Err("the end time cannot be specified relative to itself".into()),
        (RelativeToEnd(_), RelativeToStart(_)) => {
            Err("the start and end times cannot be specified relative to each other".into())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateTimestamp {
    pub seconds: i64,
    pub microseconds: u64,
}

impl UpdateTimestamp {
    pub fn format_rrd(self) -> String {
        if self.microseconds == 0 {
            self.seconds.to_string()
        } else {
            format!("{}.{:06}", self.seconds, self.microseconds)
        }
    }
}

/// RRDtool accepts `N` for the current time and interprets negative numeric
/// update times as offsets from the current time.
pub fn parse_rrd_update_timestamp(
    value: &str,
    now: f64,
) -> Result<UpdateTimestamp, Box<dyn std::error::Error>> {
    let timestamp = if value == "N" {
        now
    } else {
        let timestamp = crate::parse_rrd_number(value)
            .filter(|timestamp| timestamp.is_finite())
            .ok_or("invalid numeric timestamp")?;
        if timestamp < 0.0 {
            now + timestamp
        } else {
            timestamp
        }
    };
    if !timestamp.is_finite() || timestamp < i64::MIN as f64 || timestamp >= i64::MAX as f64 {
        return Err("update timestamp is outside the supported range".into());
    }
    let mut seconds = timestamp.floor() as i64;
    let mut microseconds = ((timestamp - seconds as f64) * 1_000_000.0) as u64;
    if microseconds >= 1_000_000 {
        seconds = seconds
            .checked_add(1)
            .ok_or("update timestamp is outside the supported range")?;
        microseconds = 0;
    }
    Ok(UpdateTimestamp {
        seconds,
        microseconds,
    })
}

#[cfg(test)]
mod time_spec_tests {
    use super::{
        RangeTimeSpec, UpdateTimestamp, parse_range_time_spec, parse_rrd_time,
        parse_rrd_update_timestamp, resolve_rrd_range_times,
    };

    #[test]
    fn parses_rrd_create_numeric_and_common_relative_times() {
        assert_eq!(
            parse_rrd_time("1000000000", 2_000_000_000).unwrap(),
            1_000_000_000
        );
        assert_eq!(parse_rrd_time("now", 2_000_000_000).unwrap(), 2_000_000_000);
        assert_eq!(
            parse_rrd_time("now - 1 hour", 2_000_000_000).unwrap(),
            1_999_996_400
        );
        assert_eq!(
            parse_rrd_time("now+2d", 2_000_000_000).unwrap(),
            2_000_172_800
        );
    }

    #[test]
    fn rejects_unsupported_or_overflowing_rrd_time_forms() {
        assert!(parse_rrd_time("end-1d", 2_000_000_000).is_err());
        assert!(parse_rrd_time("now+999999999999999999999d", 2_000_000_000).is_err());
    }

    #[test]
    fn resolves_range_times_relative_to_the_other_endpoint() {
        assert_eq!(
            resolve_rrd_range_times(
                Some("end-30s"),
                Some("2000000000"),
                1_999_913_600,
                2_000_000_000,
                2_000_000_000,
            )
            .unwrap(),
            (1_999_999_970, 2_000_000_000)
        );
        assert_eq!(
            resolve_rrd_range_times(
                Some("2000000000"),
                Some("start+30s"),
                1_999_913_600,
                2_000_000_000,
                2_000_000_000,
            )
            .unwrap(),
            (2_000_000_000, 2_000_000_030)
        );
    }

    #[test]
    fn rejects_self_and_mutually_relative_range_times() {
        let now = 2_000_000_000;
        assert!(resolve_rrd_range_times(Some("start-1h"), None, now - 86_400, now, now).is_err());
        assert!(resolve_rrd_range_times(None, Some("end+1h"), now - 86_400, now, now).is_err());
        assert!(
            resolve_rrd_range_times(Some("end-1h"), Some("start+1h"), now - 86_400, now, now)
                .is_err()
        );
    }

    #[test]
    fn fetch_negative_and_zero_times_are_relative_to_now() {
        assert_eq!(
            parse_range_time_spec("1000000000", 2_000_000_000).unwrap(),
            RangeTimeSpec::Absolute(1_000_000_000)
        );
        assert_eq!(
            parse_range_time_spec("-3600", 2_000_000_000).unwrap(),
            RangeTimeSpec::Absolute(1_999_996_400)
        );
    }

    #[test]
    fn update_n_and_negative_times_are_relative_to_current_time() {
        assert_eq!(
            parse_rrd_update_timestamp("N", 2_000_000_000.75).unwrap(),
            UpdateTimestamp {
                seconds: 2_000_000_000,
                microseconds: 750_000
            }
        );
        assert_eq!(
            parse_rrd_update_timestamp("-60", 2_000_000_000.75).unwrap(),
            UpdateTimestamp {
                seconds: 1_999_999_940,
                microseconds: 750_000
            }
        );
        assert_eq!(
            parse_rrd_update_timestamp("10.9", 2_000_000_000.75).unwrap(),
            UpdateTimestamp {
                seconds: 10,
                microseconds: 900_000
            }
        );
        assert!(parse_rrd_update_timestamp("NaN", 2_000_000_000.75).is_err());
    }
}
