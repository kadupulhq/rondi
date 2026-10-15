//! Local-time conversions through the host C library.
//!
//! Compatibility: time zone rules, the resolution of skipped and repeated
//! local times, and the handling of dates outside the library's range are
//! whatever the host C library does, as for the reference.
//! Provenance: black-box compatibility test; see
//! docs/compatibility/time-spec.md TS-46, TS-48, TS-52 and TS-53.

/// Broken-down local time. Fields use the C library's conventions: months
/// from 0, years from 1900, and a daylight-saving flag that is negative
/// when unknown.
#[derive(Clone, Copy)]
pub(super) struct Fields(libc::tm);

// POSIX; the libc crate does not declare it on every target.
unsafe extern "C" {
    fn tzset();
}

impl Fields {
    /// The local fields of an instant, or `None` when the C library cannot
    /// represent it.
    pub fn of(instant: i64) -> Option<Self> {
        let instant = libc::time_t::try_from(instant).ok()?;
        // SAFETY: `tm` is plain data; zero is a valid value for every field,
        // including the zone-name pointer on platforms that have one.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        // SAFETY: tzset only reads TZ. localtime_r writes into `tm`, which
        // outlives the call, and returns null on failure.
        let converted = unsafe {
            tzset();
            libc::localtime_r(&instant, &mut tm)
        };
        (!converted.is_null()).then_some(Self(tm))
    }

    /// Midnight at the start of the given day, with an unknown
    /// daylight-saving flag.
    pub fn midnight(year_since_1900: i32, month0: i32, day: i32) -> Self {
        // SAFETY: as above, all-zero is a valid `tm`.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year_since_1900;
        tm.tm_mon = month0;
        tm.tm_mday = day;
        tm.tm_isdst = -1;
        Self(tm)
    }

    pub fn year_since_1900(&self) -> i32 {
        self.0.tm_year
    }

    pub fn weekday(&self) -> i32 {
        self.0.tm_wday
    }

    pub fn set_date(&mut self, year_since_1900: i32, month0: i32, day: i32) {
        self.0.tm_year = year_since_1900;
        self.0.tm_mon = month0;
        self.0.tm_mday = day;
    }

    pub fn set_clock(&mut self, hour: i32, minute: i32, second: i32) {
        self.0.tm_hour = hour;
        self.0.tm_min = minute;
        self.0.tm_sec = second;
    }

    pub fn forget_daylight_saving(&mut self) {
        self.0.tm_isdst = -1;
    }

    /// Adds calendar amounts without normalizing; 32-bit fields wrap.
    pub fn add_calendar(&mut self, years: i32, months: i32, days: i32) {
        self.0.tm_year = self.0.tm_year.wrapping_add(years);
        self.0.tm_mon = self.0.tm_mon.wrapping_add(months);
        self.0.tm_mday = self.0.tm_mday.wrapping_add(days);
    }

    /// Converts to seconds since the epoch, normalizing out-of-range fields.
    /// The C library's failure value, −1, is returned as is.
    pub fn to_instant(mut self) -> i64 {
        // SAFETY: mktime reads and normalizes the `tm` it is given, which is
        // a valid, exclusively borrowed local.
        let instant = unsafe { libc::mktime(&mut self.0) };
        // time_t is 32-bit on some targets.
        #[allow(clippy::useless_conversion)]
        i64::from(instant)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_the_instant() {
        let fields = Fields::of(1_000_000_000).unwrap();
        assert_eq!(fields.to_instant(), 1_000_000_000);
    }

    #[test]
    fn normalizes_out_of_range_fields() {
        let reference = Fields::of(1_000_000_000).unwrap();
        let mut moved = reference;
        moved.add_calendar(0, 0, 1);
        assert_eq!(moved.to_instant() - reference.to_instant(), 86_400);
    }

    #[test]
    fn unrepresentable_instants_are_none() {
        assert!(Fields::of(i64::MAX).is_none());
    }
}
