//! Local-time operators: LTIME and NEWDAY, NEWWEEK, NEWMONTH, NEWYEAR.
//!
//! The local zone comes from the process environment (`TZ`) through the C
//! library, as it does for RRDtool.

/// The broken-down fields the operators use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fields {
    year: i64,
    month: i64,
    /// Zero-based day of the year.
    year_day: i64,
    /// Zero is Sunday.
    week_day: i64,
    hour: i64,
    minute: i64,
    second: i64,
}

/// A calendar period for the NEW* operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Period {
    Day,
    Week,
    Month,
    Year,
}

/// `LTIME`: the row time plus an offset derived from the local and UTC
/// broken-down times.
///
/// Compatibility: the offset is the field-wise clock difference with a day
/// added when the local date is later and subtracted when it is earlier,
/// testing "later" first. West of UTC on 31 December this adds a day where it
/// should subtract one, so the offset is 48 hours too large there.
/// Provenance: reference-source-inspection; see "LTIME" in
/// docs/compatibility/rpn.md (vectors ltime-02, ltime-07, ltime-08).
pub(super) fn local_time(time: i64) -> f64 {
    let local = local_fields(time);
    let utc = utc_fields(time);
    let mut offset = (local.second - utc.second)
        + 60 * (local.minute - utc.minute)
        + 3600 * (local.hour - utc.hour);
    if local.year_day > utc.year_day || local.year > utc.year {
        offset += 86_400;
    } else if local.year_day < utc.year_day || local.year < utc.year {
        offset -= 86_400;
    }
    (time + offset) as f64
}

/// Whether the local period containing `time` differs from the one
/// containing `time - step`.
pub(super) fn starts_new(period: Period, time: i64, step: u64) -> bool {
    let earlier = time.saturating_sub_unsigned(step);
    let now = local_fields(time);
    let before = local_fields(earlier);
    match period {
        Period::Day => (now.year, now.year_day) != (before.year, before.year_day),
        // Compatibility: weeks start on Sunday. macOS ignores the locale here
        // and the CI glibc image matches; locales whose week starts on
        // another day are not consulted.
        // Provenance: black-box compatibility test (newweek-01 to newweek-05).
        Period::Week => now.week_day == 0 && before.week_day != 0,
        Period::Month => (now.year, now.month) != (before.year, before.month),
        Period::Year => now.year != before.year,
    }
}

#[cfg(unix)]
fn local_fields(time: i64) -> Fields {
    // SAFETY: both pointers are valid for the call; localtime_r is the
    // reentrant form.
    broken_down(time, |clock, tm| unsafe { libc::localtime_r(clock, tm) })
}

#[cfg(unix)]
fn utc_fields(time: i64) -> Fields {
    // SAFETY: as above.
    broken_down(time, |clock, tm| unsafe { libc::gmtime_r(clock, tm) })
}

#[cfg(unix)]
fn broken_down(
    time: i64,
    convert: impl FnOnce(*const libc::time_t, *mut libc::tm) -> *mut libc::tm,
) -> Fields {
    // SAFETY: libc::tm is plain integers plus, on some targets, a nullable
    // zone pointer; all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let clock = time as libc::time_t;
    // A time the C library cannot represent leaves the zeroed fields.
    let _ = convert(&clock, &mut tm);
    Fields {
        year: i64::from(tm.tm_year),
        month: i64::from(tm.tm_mon),
        year_day: i64::from(tm.tm_yday),
        week_day: i64::from(tm.tm_wday),
        hour: i64::from(tm.tm_hour),
        minute: i64::from(tm.tm_min),
        second: i64::from(tm.tm_sec),
    }
}

/// Without a C library the local zone is UTC.
#[cfg(not(unix))]
fn local_fields(time: i64) -> Fields {
    utc_fields(time)
}

#[cfg(not(unix))]
fn utc_fields(time: i64) -> Fields {
    let days = time.div_euclid(86_400);
    let seconds = time.rem_euclid(86_400);
    // Civil date from a day count (proleptic Gregorian, March-based years).
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_march_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_march_year + 2) / 153;
    let month = if month_index < 10 {
        month_index + 2
    } else {
        month_index - 10
    };
    let year = year_of_era + era * 400 + i64::from(month <= 1);
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let year_day = if month_index < 10 {
        day_of_march_year + 59 + i64::from(leap)
    } else {
        day_of_march_year - 306
    };
    Fields {
        year: year - 1900,
        month,
        year_day,
        week_day: (days + 4).rem_euclid(7),
        hour: seconds / 3_600,
        minute: seconds / 60 % 60,
        second: seconds % 60,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2021-01-01 00:00:00 UTC, a Friday.
    const NEW_YEAR_2021: i64 = 1_609_459_200;

    #[test]
    fn utc_fields_match_the_calendar() {
        let fields = utc_fields(NEW_YEAR_2021 + 3_723);
        assert_eq!(
            fields,
            Fields {
                year: 121,
                month: 0,
                year_day: 0,
                week_day: 5,
                hour: 1,
                minute: 2,
                second: 3,
            }
        );
        let leap_day_end = utc_fields(1_583_020_799); // 2020-02-29 23:59:59
        assert_eq!((leap_day_end.month, leap_day_end.year_day), (1, 59));
        let new_years_eve = utc_fields(NEW_YEAR_2021 - 1);
        assert_eq!((new_years_eve.year, new_years_eve.year_day), (120, 365));
    }

    #[test]
    fn periods_compare_with_the_previous_row() {
        // These run in whatever zone the test process has; a step of a
        // whole week always crosses a day, and never a year in mid-June.
        let mid_june = 1_623_758_400; // 2021-06-15 12:00 UTC
        assert!(starts_new(Period::Day, mid_june, 7 * 86_400));
        assert!(!starts_new(Period::Year, mid_june, 7 * 86_400));
        assert!(!starts_new(Period::Month, mid_june, 1));
    }
}
