//! At-style time specifications such as `now-1d`, `noon yesterday`,
//! `20030102 13:00` or `end-3w`.
//!
//! The accepted grammar, evaluation rules and error texts are those of
//! docs/compatibility/time-spec.md (TS-1 to TS-71), established by
//! documentation and black-box runs of RRDtool 1.11.0. Parsing is split
//! into a tokenizer ([`lexer`]), a grammar that builds a typed
//! specification ([`parser`]), and the evaluation below, which applies the
//! specification to local time through the host C library ([`local`]).

mod lexer;
mod local;
mod parser;
mod update;

use std::fmt;

use local::Fields;
use parser::{Base, Clock, Day, Shift, Wall, Year};

pub use update::{UpdateTimestamp, parse_update_timestamp};

/// The option a relative specification refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Start,
    End,
}

/// The part of a numeric date that a separator must be followed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatePart {
    Day,
    Month,
    Year,
}

impl fmt::Display for DatePart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Day => "day",
            Self::Month => "month",
            Self::Year => "year",
        })
    }
}

/// Why a specification was rejected. The texts are part of the
/// compatibility contract (TS-58, TS-63).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TimeSpecError {
    #[error("unparsable time: {0}")]
    Unparsable(String),
    #[error("unparsable trailing text: '...{0}'")]
    TrailingText(String),
    #[error("if 'now' is followed by a token it must be +|- offset")]
    NowWithoutOffset,
    #[error("'start' or 'end' MUST be followed by +|- offset")]
    AnchorWithoutOffset,
    #[error("There should be number after '{0}'")]
    SignWithoutNumber(char),
    /// Not in the specification's table; observed for `12:` and `12:x`.
    #[error("Parsing HH:MM syntax, expecting MM as number, got none")]
    MissingMinutes,
    #[error("parsing HH:MM syntax, got MM = {0} (>59!)")]
    MinutesTooLarge(i32),
    #[error("there cannot be more than 12 AM or PM hours")]
    MeridiemHourTooLarge,
    #[error("the day of the month should follow month name")]
    MissingDayAfterMonthName,
    #[error("did you really mean month {0}?")]
    BadMonth(i32),
    #[error("I'm afraid that {0} is not a valid day of the month")]
    BadDay(i32),
    #[error("there should be {part} number after '{separator}'")]
    MissingDatePart { part: DatePart, separator: char },
    #[error("won't handle dates before epoch (01/01/1970), sorry")]
    YearBeforeEpoch,
    #[error("invalid year {0} (should be either 00-99 or >1900)")]
    InvalidYear(i64),
    #[error("the specified time is incorrect (out of range?)")]
    OutOfRange,
    #[error("the start time cannot be specified relative to itself")]
    StartRelativeToItself,
    #[error("the end time cannot be specified relative to itself")]
    EndRelativeToItself,
    #[error("the start and end times cannot be specified relative to each other")]
    MutuallyRelative,
}

impl From<TimeSpecError> for String {
    fn from(error: TimeSpecError) -> Self {
        error.to_string()
    }
}

/// A parsed specification: a timestamp, or offsets still to be applied to
/// the other option's time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeSpec(Resolved);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolved {
    At(i64),
    Relative(Anchor, Shift),
}

impl TimeSpec {
    /// A specification that is already a timestamp.
    pub fn at(seconds: i64) -> Self {
        Self(Resolved::At(seconds))
    }

    /// The timestamp, unless the specification is relative to `start` or
    /// `end`.
    pub fn absolute(&self) -> Option<i64> {
        match self.0 {
            Resolved::At(seconds) => Some(seconds),
            Resolved::Relative(..) => None,
        }
    }

    /// The option a relative specification refers to.
    pub fn anchor(&self) -> Option<Anchor> {
        match self.0 {
            Resolved::At(_) => None,
            Resolved::Relative(anchor, _) => Some(anchor),
        }
    }
}

/// Parses `text` and evaluates everything that does not depend on the other
/// option. `now` is the current time in seconds since the epoch; local
/// fields come from the zone named by `TZ`.
pub fn parse_time_spec(text: &str, now: i64) -> Result<TimeSpec, TimeSpecError> {
    let spec = parser::parse(text)?;
    let resolved = match spec.base {
        Base::Anchor(anchor) => Resolved::Relative(anchor, spec.shift),
        Base::Now => Resolved::At(shift_instant(now, spec.shift, DaylightSaving::Unknown)?),
        Base::Seconds(seconds) => {
            Resolved::At(shift_instant(seconds, spec.shift, DaylightSaving::Keep)?)
        }
        Base::Epoch => Resolved::At(epoch(spec.shift)),
        Base::Wall(wall) => Resolved::At(wall_time(wall, spec.shift, now)?),
    };
    Ok(TimeSpec(resolved))
}

/// Resolves a start and end pair where either may refer to the other
/// (TS-56 to TS-58). Range and ordering checks are the caller's.
pub fn resolve_start_end(start: &TimeSpec, end: &TimeSpec) -> Result<(i64, i64), TimeSpecError> {
    match (start.0, end.0) {
        (Resolved::Relative(Anchor::Start, _), _) => Err(TimeSpecError::StartRelativeToItself),
        (_, Resolved::Relative(Anchor::End, _)) => Err(TimeSpecError::EndRelativeToItself),
        (Resolved::Relative(..), Resolved::Relative(..)) => Err(TimeSpecError::MutuallyRelative),
        (Resolved::Relative(_, shift), Resolved::At(end)) => {
            Ok((shift_instant(end, shift, DaylightSaving::Keep)?, end))
        }
        (Resolved::At(start), Resolved::Relative(_, shift)) => {
            Ok((start, shift_instant(start, shift, DaylightSaving::Keep)?))
        }
        (Resolved::At(start), Resolved::At(end)) => Ok((start, end)),
    }
}

/// Parses an optional start and end against a default window, as a graph
/// `DEF`'s `start=`/`end=` do. Parse errors carry the `start time: ` or
/// `end time: ` prefix of TS-2; resolution errors do not (TS-3).
pub fn resolve_window(
    start: Option<&str>,
    end: Option<&str>,
    window: (i64, i64),
    now: i64,
) -> Result<(i64, i64), String> {
    let start = match start {
        Some(text) => parse_time_spec(text, now).map_err(|error| format!("start time: {error}"))?,
        None => TimeSpec::at(window.0),
    };
    let end = match end {
        Some(text) => parse_time_spec(text, now).map_err(|error| format!("end time: {error}"))?,
        None => TimeSpec::at(window.1),
    };
    Ok(resolve_start_end(&start, &end)?)
}

/// How the daylight-saving flag of a reference instant enters the single
/// local-time conversion (TS-47).
#[derive(Clone, Copy, PartialEq, Eq)]
enum DaylightSaving {
    /// Keep the flag in force at the instant, so a calendar shift keeps its
    /// UTC offset.
    Keep,
    /// Let the conversion pick the offset in force at the shifted time.
    Unknown,
}

/// Applies `shift` to an instant (TS-43). Calendar amounts go through the
/// instant's local fields and one conversion; seconds are added after.
fn shift_instant(
    instant: i64,
    shift: Shift,
    daylight_saving: DaylightSaving,
) -> Result<i64, TimeSpecError> {
    if !shift.calendar {
        return Ok(instant.wrapping_add(shift.seconds));
    }
    let mut fields = Fields::of(instant).ok_or(TimeSpecError::OutOfRange)?;
    if daylight_saving == DaylightSaving::Unknown {
        fields.forget_daylight_saving();
    }
    fields.add_calendar(shift.years, shift.months, shift.days);
    Ok(checked(fields.to_instant())?.wrapping_add(shift.seconds))
}

/// Compatibility: `epoch` is local midnight starting 31 December 1899, and
/// a failed conversion of it is not reported, so a C library that cannot
/// convert dates before 1900 yields −1.
/// Provenance: black-box compatibility test; see
/// docs/compatibility/time-spec.md TS-16 and TS-53 (defect).
fn epoch(shift: Shift) -> i64 {
    let mut fields = Fields::midnight(-1, 11, 31);
    fields.add_calendar(shift.years, shift.months, shift.days);
    fields.to_instant().wrapping_add(shift.seconds)
}

/// TS-22 to TS-30: a local date and time of day, defaults taken from `now`.
fn wall_time(wall: Wall, shift: Shift, now: i64) -> Result<i64, TimeSpecError> {
    let today = Fields::of(now).ok_or(TimeSpecError::OutOfRange)?;
    let mut fields = today;
    fields.forget_daylight_saving();
    match wall.day {
        Day::FromToday(days) => fields.add_calendar(0, 0, days),
        Day::Weekday(weekday) => fields.add_calendar(0, 0, weekday - today.weekday()),
        Day::Date { year, month, day } => {
            let year_since_1900 = match year {
                Year::Current => today.year_since_1900(),
                Year::Far => 30_000 - 1900,
                // Compatibility: the year field is 32-bit and wraps.
                // Provenance: black-box compatibility test; see
                // docs/compatibility/time-spec.md TS-34.
                Year::Given(year) => year.wrapping_sub(1900) as i32,
            };
            fields.set_date(year_since_1900, month - 1, day);
        }
    }
    if let Clock::At { hour, minute } = wall.clock {
        fields.set_clock(hour, minute, 0);
    }
    fields.add_calendar(shift.years, shift.months, shift.days);
    Ok(checked(fields.to_instant())?.wrapping_add(shift.seconds))
}

/// The C library reports a conversion failure as −1.
fn checked(instant: i64) -> Result<i64, TimeSpecError> {
    if instant == -1 {
        Err(TimeSpecError::OutOfRange)
    } else {
        Ok(instant)
    }
}

#[cfg(test)]
mod tests;
