//! Turns tokens into a [`Spec`]: a reference point plus offsets.
//!
//! The grammar, word lists and error order follow
//! docs/compatibility/time-spec.md; each rule names the TS entry it
//! implements. Nothing here reads the clock or the time zone, so the parse
//! of a given text is the same at every instant.

use super::lexer::{Tok, Token, is_separator, tokenize};
use super::{Anchor, DatePart, TimeSpecError as Error};

/// A parsed specification, not yet evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Spec {
    pub base: Base,
    pub shift: Shift,
}

/// What the offsets are applied to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Base {
    /// The current time, written or implied.
    Now,
    /// The other option's time.
    Anchor(Anchor),
    /// The `epoch` keyword (TS-16).
    Epoch,
    /// Seconds since the epoch, written as a number.
    Seconds(i64),
    /// A local date and time of day.
    Wall(Wall),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Wall {
    pub day: Day,
    pub clock: Clock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Day {
    /// Today plus a number of days (`yesterday`, `today`, `tomorrow`, or no
    /// day at all).
    FromToday(i32),
    /// A weekday of the current Sunday-based week, 0 being Sunday.
    Weekday(i32),
    /// A calendar date; `day` is not range checked for month names.
    Date { year: Year, month: i32, day: i32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Year {
    Current,
    /// Compatibility: a yearless date after a numeric time of day lands in
    /// year 30000.
    /// Provenance: black-box compatibility test; see
    /// docs/compatibility/time-spec.md TS-26 (defect).
    Far,
    /// A full year number after the two-digit rules of TS-33.
    Given(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Clock {
    /// Keep the current local time of day, seconds included.
    Current,
    /// A written time of day; seconds are zero.
    At { hour: i32, minute: i32 },
}

/// The sum of all offsets. Calendar amounts are kept apart from exact
/// seconds because they are applied before the local-time conversion and
/// the seconds after it (TS-43).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Shift {
    pub days: i32,
    pub months: i32,
    pub years: i32,
    /// Whether any day, week, month or year amount was written, even zero.
    pub calendar: bool,
    pub seconds: i64,
}

pub(super) fn parse(text: &str) -> Result<Spec, Error> {
    let lexed = tokenize(text);
    let mut parser = Parser {
        text,
        tokens: lexed.tokens,
        pos: 0,
    };
    if parser.tokens.is_empty() {
        return Err(match lexed.stopped_at {
            // Compatibility: a specification that starts with a foreign byte
            // is echoed with that character doubled.
            // Provenance: black-box compatibility test; see
            // docs/compatibility/time-spec.md TS-9.
            Some(at) => {
                let rest = &text[at..];
                let first = rest.chars().next().unwrap_or_default();
                Error::Unparsable(format!("{first}{rest}"))
            }
            None => Error::Unparsable(String::new()),
        });
    }
    let base = parser.reference()?;
    let shift = parser.offsets()?;
    Ok(Spec { base, shift })
}

struct Parser<'a> {
    text: &'a str,
    tokens: Vec<Token<'a>>,
    pos: usize,
}

/// Either a date or, for a large number in date position, a timestamp.
enum DateOrSeconds {
    Date(Day),
    Seconds(i64),
}

#[derive(Clone, Copy)]
enum Meridiem {
    Am,
    Pm,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
    /// `m`: months or minutes depending on context (TS-40).
    MonthsOrMinutes,
    /// Any other token after an amount; the amount counts as seconds.
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UnitKind {
    Calendar,
    Exact,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<Tok<'a>> {
        self.peek_at(0)
    }

    fn peek_at(&self, ahead: usize) -> Option<Tok<'a>> {
        self.tokens.get(self.pos + ahead).map(|token| token.tok)
    }

    fn bump(&mut self) -> Option<Tok<'a>> {
        let tok = self.peek();
        self.pos += 1;
        tok
    }

    fn peek_sign(&self) -> bool {
        matches!(self.peek(), Some(Tok::Plus | Tok::Minus))
    }

    /// TS-13: the whole input, less leading separators.
    fn unparsable(&self) -> Error {
        let trimmed = self
            .text
            .trim_start_matches(|c: char| c.is_ascii() && is_separator(c as u8));
        Error::Unparsable(trimmed.to_owned())
    }

    /// TS-13: the input from the current token on.
    fn trailing(&self) -> Error {
        let at = self.tokens.get(self.pos).map_or(self.text.len(), |t| t.at);
        Error::TrailingText(self.text[at..].to_owned())
    }

    fn reference(&mut self) -> Result<Base, Error> {
        match self.peek() {
            Some(Tok::Plus | Tok::Minus) => Ok(Base::Now),
            Some(Tok::Word(word)) => {
                if let Some(base) = reference_word(word) {
                    self.bump();
                    return self.after_reference_word(base);
                }
                if let Some(hour) = named_clock(word) {
                    self.bump();
                    return self.day_after_clock(Clock::At { hour, minute: 0 }, Year::Current);
                }
                if let Some(month) = month_name(word) {
                    self.bump();
                    let day = self.month_name_date(month, Year::Current)?;
                    let clock = self.clock_after_date(false)?.unwrap_or(Clock::Current);
                    return Ok(Base::Wall(Wall { day, clock }));
                }
                Err(self.unparsable())
            }
            Some(Tok::Number(_)) if self.hour_here() => {
                let clock = self.numeric_clock()?;
                self.day_after_clock(clock, Year::Far)
            }
            Some(Tok::Number(_)) => match self.numeric_date(Year::Current)? {
                DateOrSeconds::Seconds(value) => Ok(Base::Seconds(value)),
                DateOrSeconds::Date(day) => {
                    let clock = self
                        .clock_after_date(true)?
                        .unwrap_or(Clock::At { hour: 0, minute: 0 });
                    Ok(Base::Wall(Wall { day, clock }))
                }
            },
            _ => Err(self.unparsable()),
        }
    }

    /// TS-14 to TS-16.
    fn after_reference_word(&self, base: Base) -> Result<Base, Error> {
        match base {
            Base::Now if self.peek().is_some() && !self.peek_sign() => Err(Error::NowWithoutOffset),
            Base::Anchor(_) | Base::Epoch if !self.peek_sign() => Err(Error::AnchorWithoutOffset),
            _ => Ok(base),
        }
    }

    /// TS-19: a number of at most two characters, at most 23, that is not
    /// followed by `/` or `.`.
    fn hour_here(&self) -> bool {
        let Some(Tok::Number(digits)) = self.peek() else {
            return false;
        };
        digits.len() <= 2
            && number(digits) <= 23
            && !matches!(self.peek_at(1), Some(Tok::Slash | Tok::Dot))
    }

    /// TS-20, TS-21: `H`, `H:M`, either optionally with `am`/`pm`. The
    /// caller has checked [`Self::hour_here`].
    fn numeric_clock(&mut self) -> Result<Clock, Error> {
        let Some(Tok::Number(digits)) = self.bump() else {
            return Err(self.unparsable());
        };
        let mut hour = low32(number(digits));
        let mut minute = 0;
        if self.peek() == Some(Tok::Colon) {
            self.bump();
            let Some(Tok::Number(digits)) = self.bump() else {
                return Err(Error::MissingMinutes);
            };
            // Compatibility: minutes are reduced to 32 bits before the
            // check, so a reduced negative value passes.
            // Provenance: black-box compatibility test.
            minute = low32(number(digits));
            if minute > 59 {
                return Err(Error::MinutesTooLarge(minute));
            }
        }
        if let Some(Tok::Word(word)) = self.peek()
            && let Some(meridiem) = meridiem(word)
        {
            self.bump();
            if hour > 12 {
                return Err(Error::MeridiemHourTooLarge);
            }
            hour = match meridiem {
                Meridiem::Am => hour % 12,
                Meridiem::Pm => hour % 12 + 12,
            };
        }
        Ok(Clock::At { hour, minute })
    }

    /// TS-23 to TS-27: the optional day after a time of day. `no_year` is
    /// the year of a date written without one.
    fn day_after_clock(&mut self, clock: Clock, no_year: Year) -> Result<Base, Error> {
        let day = match self.peek() {
            Some(Tok::Word(word)) => {
                if let Some(offset) = relative_day(word) {
                    self.bump();
                    Day::FromToday(offset)
                } else if let Some(weekday) = weekday(word) {
                    self.bump();
                    Day::Weekday(weekday)
                } else if let Some(month) = month_name(word) {
                    self.bump();
                    self.month_name_date(month, no_year)?
                } else {
                    Day::FromToday(0)
                }
            }
            Some(Tok::Number(_)) => match self.numeric_date(no_year)? {
                DateOrSeconds::Seconds(value) => return Ok(Base::Seconds(value)),
                DateOrSeconds::Date(day) => day,
            },
            _ => Day::FromToday(0),
        };
        Ok(Base::Wall(Wall { day, clock }))
    }

    /// TS-25, TS-27: the day number and optional year after a month name.
    fn month_name_date(&mut self, month: i32, no_year: Year) -> Result<Day, Error> {
        let Some(Tok::Number(digits)) = self.peek() else {
            return Err(Error::MissingDayAfterMonthName);
        };
        self.bump();
        let day = low32(number(digits));
        let year = match self.peek() {
            Some(Tok::Number(digits)) => {
                self.bump();
                Year::Given(full_year(number(digits))?)
            }
            _ => no_year,
        };
        Ok(Day::Date { year, month, day })
    }

    /// TS-28, TS-29: a number that is not an hour.
    fn numeric_date(&mut self, no_year: Year) -> Result<DateOrSeconds, Error> {
        let Some(Tok::Number(digits)) = self.bump() else {
            return Err(self.unparsable());
        };
        let value = number(digits);
        if value > 315_360_000 {
            // TS-32 leaves a value at or past the 64-bit limit undefined;
            // reject it rather than guess.
            if value == i64::MAX {
                return Err(Error::OutOfRange);
            }
            return Ok(DateOrSeconds::Seconds(value));
        }
        if value > 19_700_101 && value < 24_000_101 {
            // Compatibility: split by character position, so leading zeros
            // shift the fields.
            // Provenance: black-box compatibility test; see
            // docs/compatibility/time-spec.md TS-28.
            let field = |range: std::ops::Range<usize>| {
                digits
                    .get(range.start..range.end.min(digits.len()))
                    .map_or(0, number)
            };
            let year = field(0..4);
            let month = field(4..6);
            let day = field(6..digits.len());
            check_month_first(month, day)?;
            return Ok(DateOrSeconds::Date(Day::Date {
                year: Year::Given(full_year(year)?),
                month: low32(month),
                day: low32(day),
            }));
        }
        let separator = match self.peek() {
            Some(Tok::Slash) => '/',
            Some(Tok::Dot) => '.',
            _ => {
                return Err(if (1..=12).contains(&value) {
                    Error::BadDay(0)
                } else {
                    Error::BadMonth(low32(value))
                });
            }
        };
        let separator_tok = self.peek();
        self.bump();
        let second_part = if separator == '/' {
            DatePart::Day
        } else {
            DatePart::Month
        };
        let Some(Tok::Number(second)) = self.bump() else {
            return Err(Error::MissingDatePart {
                part: second_part,
                separator,
            });
        };
        let second = number(second);
        let year = if self.peek() == separator_tok {
            self.bump();
            let Some(Tok::Number(year)) = self.bump() else {
                return Err(Error::MissingDatePart {
                    part: DatePart::Year,
                    separator,
                });
            };
            Some(number(year))
        } else {
            None
        };
        let (month, day) = if separator == '/' {
            check_month_first(value, second)?;
            (value, second)
        } else {
            check_day_first(value, second)?;
            (second, value)
        };
        let year = match year {
            Some(year) => Year::Given(full_year(year)?),
            None => no_year,
        };
        Ok(DateOrSeconds::Date(Day::Date {
            year,
            month: low32(month),
            day: low32(day),
        }))
    }

    /// TS-30: an optional time of day after a date. After a numeric date a
    /// lone `am`/`pm` is accepted and means midnight, and a lone sign is
    /// dropped.
    fn clock_after_date(&mut self, numeric_date: bool) -> Result<Option<Clock>, Error> {
        if self.hour_here() {
            return self.numeric_clock().map(Some);
        }
        if numeric_date
            && let Some(Tok::Word(word)) = self.peek()
            && meridiem(word).is_some()
        {
            // Compatibility: `01/02/2003 pm` is midnight.
            // Provenance: black-box compatibility test; see
            // docs/compatibility/time-spec.md TS-30 (defect).
            self.bump();
            return Ok(Some(Clock::At { hour: 0, minute: 0 }));
        }
        if numeric_date && self.peek_sign() {
            // Compatibility: a sign right after a numeric date is swallowed,
            // so `01/02/2003 +1d` leaves `1d` as trailing text and
            // `01/02/2003 +` is accepted.
            // Provenance: black-box compatibility test (vector tod-050 and
            // probes of RRDtool 1.11.0).
            self.bump();
        }
        Ok(None)
    }

    /// TS-35 to TS-41.
    fn offsets(&mut self) -> Result<Shift, Error> {
        let mut shift = Shift::default();
        while let Some(tok) = self.peek() {
            let negative = match tok {
                Tok::Plus => false,
                Tok::Minus => true,
                _ => return Err(self.trailing()),
            };
            self.bump();
            if !matches!(self.peek(), Some(Tok::Number(_))) {
                return Err(Error::SignWithoutNumber(if negative { '-' } else { '+' }));
            }
            let mut context = None;
            while let Some(Tok::Number(digits)) = self.peek() {
                self.bump();
                let count = low32(number(digits));
                let unit = settle_unit(self.amount_unit(), count, &mut context);
                let signed = if negative {
                    count.wrapping_neg()
                } else {
                    count
                };
                shift.add(unit, signed);
            }
        }
        Ok(shift)
    }
}

impl Parser<'_> {
    /// Takes the unit of an amount whose number was just read.
    ///
    /// Compatibility: whatever token follows the number is its unit, a sign
    /// included. A sign taken this way is ignored, so the next number keeps
    /// the current sign: `+5-1h` adds five seconds and one hour, `-1-1`
    /// subtracts two seconds.
    /// Provenance: black-box compatibility test (vectors ref-113, ref-114
    /// and probes of RRDtool 1.11.0); this refines TS-37.
    fn amount_unit(&mut self) -> Unit {
        match self.bump() {
            None => Unit::Seconds,
            Some(Tok::Word(word)) => unit_word(word).unwrap_or(Unit::Unknown),
            Some(_) => Unit::Unknown,
        }
    }
}

/// TS-40: decides what `m` means from the units before it under the same
/// sign, and records the kind of a known unit for the next `m`.
fn settle_unit(unit: Unit, count: i32, context: &mut Option<UnitKind>) -> Unit {
    let unit = match (unit, *context) {
        (Unit::MonthsOrMinutes, Some(UnitKind::Calendar)) => Unit::Months,
        (Unit::MonthsOrMinutes, Some(UnitKind::Exact)) => Unit::Minutes,
        (Unit::MonthsOrMinutes, None) if count < 6 => Unit::Months,
        (Unit::MonthsOrMinutes, None) => Unit::Minutes,
        (other, _) => other,
    };
    match unit {
        Unit::Days | Unit::Weeks | Unit::Months | Unit::Years => {
            *context = Some(UnitKind::Calendar);
        }
        Unit::Seconds | Unit::Minutes | Unit::Hours => {
            *context = Some(UnitKind::Exact);
        }
        Unit::Unknown | Unit::MonthsOrMinutes => {}
    }
    unit
}

impl Shift {
    /// Compatibility: counts and second products are 32-bit and wrap; the
    /// sum of exact seconds is 64-bit.
    /// Provenance: black-box compatibility test; see
    /// docs/compatibility/time-spec.md TS-41, TS-42.
    fn add(&mut self, unit: Unit, count: i32) {
        let seconds_per = match unit {
            Unit::Days => {
                self.days = self.days.wrapping_add(count);
                self.calendar = true;
                return;
            }
            Unit::Weeks => {
                self.days = self.days.wrapping_add(count.wrapping_mul(7));
                self.calendar = true;
                return;
            }
            Unit::Months => {
                self.months = self.months.wrapping_add(count);
                self.calendar = true;
                return;
            }
            Unit::Years => {
                self.years = self.years.wrapping_add(count);
                self.calendar = true;
                return;
            }
            Unit::Minutes => 60,
            Unit::Hours => 3600,
            Unit::Seconds | Unit::Unknown | Unit::MonthsOrMinutes => 1,
        };
        self.seconds = self
            .seconds
            .wrapping_add(i64::from(count.wrapping_mul(seconds_per)));
    }
}

/// TS-29 for `MM/DD` and `YYYYMMDD`.
fn check_month_first(month: i64, day: i64) -> Result<(), Error> {
    if !(1..=12).contains(&month) {
        return Err(Error::BadMonth(low32(month)));
    }
    if !(1..=31).contains(&day) {
        return Err(Error::BadDay(low32(day)));
    }
    Ok(())
}

/// TS-29 for `DD.MM`.
fn check_day_first(day: i64, month: i64) -> Result<(), Error> {
    // Compatibility: a day above 31 is reported as a month.
    // Provenance: black-box compatibility test; see
    // docs/compatibility/time-spec.md TS-29 (defect).
    if day > 31 {
        return Err(Error::BadMonth(low32(day)));
    }
    if !(1..=12).contains(&month) {
        return Err(Error::BadMonth(low32(month)));
    }
    if day < 1 {
        return Err(Error::BadDay(low32(day)));
    }
    Ok(())
}

/// TS-33: the year as written to a full year.
fn full_year(written: i64) -> Result<i64, Error> {
    match written {
        0..=37 => Ok(2000 + written),
        38..=69 => Err(Error::YearBeforeEpoch),
        70..=138 => Ok(1900 + written),
        139..=1970 => Err(Error::InvalidYear(written)),
        _ => Ok(written),
    }
}

/// The decimal value of a digit run, saturating at `i64::MAX` (TS-41).
fn number(digits: &str) -> i64 {
    digits.bytes().fold(0_i64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'))
    })
}

/// The low 32 bits of a value as a signed number.
fn low32(value: i64) -> i32 {
    value as i32
}

fn is_any(word: &str, choices: &[&str]) -> bool {
    choices
        .iter()
        .any(|choice| word.eq_ignore_ascii_case(choice))
}

fn reference_word(word: &str) -> Option<Base> {
    if is_any(word, &["now", "n"]) {
        Some(Base::Now)
    } else if is_any(word, &["start", "s"]) {
        Some(Base::Anchor(Anchor::Start))
    } else if is_any(word, &["end", "e"]) {
        Some(Base::Anchor(Anchor::End))
    } else if word.eq_ignore_ascii_case("epoch") {
        Some(Base::Epoch)
    } else {
        None
    }
}

fn named_clock(word: &str) -> Option<i32> {
    [("midnight", 0), ("noon", 12), ("teatime", 16)]
        .into_iter()
        .find_map(|(name, hour)| word.eq_ignore_ascii_case(name).then_some(hour))
}

fn meridiem(word: &str) -> Option<Meridiem> {
    if word.eq_ignore_ascii_case("am") {
        Some(Meridiem::Am)
    } else if word.eq_ignore_ascii_case("pm") {
        Some(Meridiem::Pm)
    } else {
        None
    }
}

fn relative_day(word: &str) -> Option<i32> {
    [("yesterday", -1), ("today", 0), ("tomorrow", 1)]
        .into_iter()
        .find_map(|(name, days)| word.eq_ignore_ascii_case(name).then_some(days))
}

const WEEKDAYS: [&str; 7] = [
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// TS-10: the full name or its first three letters, nothing in between.
fn name_index(word: &str, names: &[&str]) -> Option<i32> {
    names
        .iter()
        .position(|name| word.eq_ignore_ascii_case(name) || word.eq_ignore_ascii_case(&name[..3]))
        .and_then(|index| i32::try_from(index).ok())
}

fn weekday(word: &str) -> Option<i32> {
    name_index(word, &WEEKDAYS)
}

/// The month number, 1 to 12.
fn month_name(word: &str) -> Option<i32> {
    name_index(word, &MONTHS).map(|index| index + 1)
}

fn unit_word(word: &str) -> Option<Unit> {
    let table: [(&[&str], Unit); 8] = [
        (&["s", "sec", "second", "seconds"], Unit::Seconds),
        (&["min", "minute", "minutes"], Unit::Minutes),
        (&["h", "hr", "hour", "hours"], Unit::Hours),
        (&["d", "day", "days"], Unit::Days),
        (&["w", "wk", "week", "weeks"], Unit::Weeks),
        (&["mon", "month", "months"], Unit::Months),
        (&["y", "yr", "year", "years"], Unit::Years),
        (&["m"], Unit::MonthsOrMinutes),
    ];
    table
        .into_iter()
        .find_map(|(names, unit)| is_any(word, names).then_some(unit))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shift(text: &str) -> Shift {
        parse(text).unwrap().shift
    }

    fn base(text: &str) -> Base {
        parse(text).unwrap().base
    }

    fn err(text: &str) -> String {
        parse(text).unwrap_err().to_string()
    }

    fn wall(day: Day, hour: i32, minute: i32) -> Base {
        Base::Wall(Wall {
            day,
            clock: Clock::At { hour, minute },
        })
    }

    fn date(year: Year, month: i32, day: i32) -> Day {
        Day::Date { year, month, day }
    }

    #[test]
    fn reference_keywords() {
        assert_eq!(base("now"), Base::Now);
        assert_eq!(base("N-1"), Base::Now);
        assert_eq!(base("-1d"), Base::Now);
        assert_eq!(base("start+1"), Base::Anchor(Anchor::Start));
        assert_eq!(base("E-1"), Base::Anchor(Anchor::End));
        assert_eq!(base("epoch+1"), Base::Epoch);
        assert_eq!(
            err("now foo"),
            "if 'now' is followed by a token it must be +|- offset"
        );
        assert_eq!(err("now 12:00"), err("now now"));
        for text in ["start", "end 5", "epoch", "s x"] {
            assert_eq!(err(text), "'start' or 'end' MUST be followed by +|- offset");
        }
    }

    #[test]
    fn times_of_day() {
        let today = Day::FromToday(0);
        assert_eq!(base("noon"), wall(today, 12, 0));
        assert_eq!(base("TeaTime"), wall(today, 16, 0));
        assert_eq!(base("5"), wall(today, 5, 0));
        assert_eq!(base("12:34"), wall(today, 12, 34));
        assert_eq!(base("9pm"), wall(today, 21, 0));
        assert_eq!(base("12am"), wall(today, 0, 0));
        assert_eq!(base("0pm"), wall(today, 12, 0));
        assert_eq!(base("12 pm"), wall(today, 12, 0));
        assert_eq!(base("1:5"), wall(today, 1, 5));
        assert_eq!(err("13pm"), "there cannot be more than 12 AM or PM hours");
        assert_eq!(err("12:60"), "parsing HH:MM syntax, got MM = 60 (>59!)");
        assert_eq!(err("12:123"), "parsing HH:MM syntax, got MM = 123 (>59!)");
        assert_eq!(
            err("12:4294967356"),
            "parsing HH:MM syntax, got MM = 60 (>59!)"
        );
        assert_eq!(base("12:99999999999999999999"), wall(today, 12, -1));
        assert_eq!(
            err("12:"),
            "Parsing HH:MM syntax, expecting MM as number, got none"
        );
        assert_eq!(
            err("12:30:45 01/02/2003"),
            "unparsable trailing text: '...:45 01/02/2003'"
        );
        assert_eq!(
            err("noon pm 01/02/2003"),
            "unparsable trailing text: '...pm 01/02/2003'"
        );
        assert_eq!(err("12:00 now"), "unparsable trailing text: '...now'");
    }

    #[test]
    fn hours_need_two_characters_at_most() {
        assert_eq!(err("24"), "did you really mean month 24?");
        assert_eq!(
            err("009"),
            "I'm afraid that 0 is not a valid day of the month"
        );
        assert_eq!(err("12.30"), "did you really mean month 30?");
        assert_eq!(base("00"), wall(Day::FromToday(0), 0, 0));
    }

    #[test]
    fn days_after_a_time() {
        assert_eq!(base("noon yesterday"), wall(Day::FromToday(-1), 12, 0));
        assert_eq!(base("midnight tomorrow"), wall(Day::FromToday(1), 0, 0));
        assert_eq!(base("noon Sat"), wall(Day::Weekday(6), 12, 0));
        assert_eq!(base("noon sunday"), wall(Day::Weekday(0), 12, 0));
        assert_eq!(base("noon Jan 2"), wall(date(Year::Current, 1, 2), 12, 0));
        assert_eq!(base("13:00 Jan 2"), wall(date(Year::Far, 1, 2), 13, 0));
        assert_eq!(base("13:00 01/02"), wall(date(Year::Far, 1, 2), 13, 0));
        assert_eq!(base("teatime 2.1"), wall(date(Year::Current, 1, 2), 16, 0));
        assert_eq!(
            base("12:00 01/02/2003"),
            wall(date(Year::Given(2003), 1, 2), 12, 0)
        );
        assert_eq!(base("12:00 1000000000"), Base::Seconds(1_000_000_000));
        assert_eq!(
            err("noon yesterday tomorrow"),
            "unparsable trailing text: '...tomorrow'"
        );
        assert_eq!(
            err("13:00 Jan"),
            "the day of the month should follow month name"
        );
        assert_eq!(
            err("1 jan"),
            "the day of the month should follow month name"
        );
        assert_eq!(
            err("noon 5"),
            "I'm afraid that 0 is not a valid day of the month"
        );
        assert_eq!(err("monday"), "unparsable time: monday");
        assert_eq!(err("today 13:00"), "unparsable time: today 13:00");
    }

    #[test]
    fn dates_first() {
        let midnight = |year, month, day| wall(date(year, month, day), 0, 0);
        assert_eq!(base("01/02/2003"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(base("1/2/03"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(base("2.1.03"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(base("20030102"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(base("001/02/2003"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(base("1 /2/2003"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(base("01/02/2003 pm"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(
            base("01/02/2003 12:30pm"),
            wall(date(Year::Given(2003), 1, 2), 12, 30)
        );
        assert_eq!(
            base("20030102 13"),
            wall(date(Year::Given(2003), 1, 2), 13, 0)
        );
        assert_eq!(
            base("Jan 2 2003"),
            Base::Wall(Wall {
                day: date(Year::Given(2003), 1, 2),
                clock: Clock::Current
            })
        );
        assert_eq!(
            base("jan 2, 2003 1:05pm"),
            wall(date(Year::Given(2003), 1, 2), 13, 5)
        );
        assert_eq!(
            base("Jan 2003"),
            Base::Wall(Wall {
                day: date(Year::Current, 1, 2003),
                clock: Clock::Current
            })
        );
        assert_eq!(err("Jan 2 12:00"), "unparsable trailing text: '...:00'");
        assert_eq!(err("Jan 2 2003 pm"), "unparsable trailing text: '...pm'");
        assert_eq!(
            err("Jan 2 2003 2004"),
            "unparsable trailing text: '...2004'"
        );
        assert_eq!(err("01/02/2003 24"), "unparsable trailing text: '...24'");
        assert_eq!(
            err("01/02/2003 noon"),
            "unparsable trailing text: '...noon'"
        );
        assert_eq!(err("1/2.2003"), "unparsable trailing text: '....2003'");
        assert_eq!(base("01/02/2003 +"), midnight(Year::Given(2003), 1, 2));
        assert_eq!(err("01/02/2003 +1d"), "unparsable trailing text: '...1d'");
        assert_eq!(err("20030102 -1h"), "unparsable trailing text: '...1h'");
        assert_eq!(shift("01/02/2003 pm +1d").days, 1);
        assert_eq!(shift("Jan 2 2003 +1d").days, 1);
    }

    #[test]
    fn date_errors_in_order() {
        let cases = [
            ("13/45/69", "did you really mean month 13?"),
            (
                "01/45/50",
                "I'm afraid that 45 is not a valid day of the month",
            ),
            (
                "1/2/50",
                "won't handle dates before epoch (01/01/1970), sorry",
            ),
            ("13/2/", "there should be year number after '/'"),
            ("1/", "there should be day number after '/'"),
            ("1/x", "there should be day number after '/'"),
            ("1.", "there should be month number after '.'"),
            ("1.2.", "there should be year number after '.'"),
            ("32.01.2003", "did you really mean month 32?"),
            ("32.13.2003", "did you really mean month 32?"),
            ("0.13.2003", "did you really mean month 13?"),
            (
                "0.01.2003",
                "I'm afraid that 0 is not a valid day of the month",
            ),
            (
                "1/4294967298/2003",
                "I'm afraid that 2 is not a valid day of the month",
            ),
            ("1.4294967297", "did you really mean month 1?"),
            (
                "19700102",
                "invalid year 1970 (should be either 00-99 or >1900)",
            ),
            ("0019700102", "did you really mean month 70?"),
            (
                "24000100",
                "I'm afraid that 0 is not a valid day of the month",
            ),
            ("24000101", "did you really mean month 24000101?"),
            ("19700101", "did you really mean month 19700101?"),
            ("315360000", "did you really mean month 315360000?"),
            ("2003-01-02", "did you really mean month 2003?"),
        ];
        for (text, message) in cases {
            assert_eq!(err(text), message, "{text}");
        }
    }

    #[test]
    fn year_table() {
        assert_eq!(full_year(0), Ok(2000));
        assert_eq!(full_year(37), Ok(2037));
        assert_eq!(full_year(38), Err(Error::YearBeforeEpoch));
        assert_eq!(full_year(69), Err(Error::YearBeforeEpoch));
        assert_eq!(full_year(70), Ok(1970));
        assert_eq!(full_year(100), Ok(2000));
        assert_eq!(full_year(138), Ok(2038));
        assert_eq!(full_year(139), Err(Error::InvalidYear(139)));
        assert_eq!(full_year(1970), Err(Error::InvalidYear(1970)));
        assert_eq!(full_year(1971), Ok(1971));
        assert_eq!(full_year(99_999), Ok(99_999));
        assert_eq!(
            err("12/31/69 +20y"),
            "won't handle dates before epoch (01/01/1970), sorry"
        );
    }

    #[test]
    fn seconds_since_the_epoch() {
        assert_eq!(base("315360001"), Base::Seconds(315_360_001));
        assert_eq!(
            base("67767976233532800"),
            Base::Seconds(67_767_976_233_532_800)
        );
        assert_eq!(
            err("9223372036854775807"),
            "the specified time is incorrect (out of range?)"
        );
        assert_eq!(err("99999999999999999999"), err("9223372036854775807"));
        assert_eq!(
            err("1000000000 12:00"),
            "unparsable trailing text: '...12:00'"
        );
        assert_eq!(err("1000000000.5"), "unparsable trailing text: '....5'");
        assert_eq!(base("1000000000#+1d"), Base::Seconds(1_000_000_000));
    }

    #[test]
    fn offsets_and_units() {
        assert_eq!(shift("+86400").seconds, 86_400);
        assert_eq!(shift("-1-1").seconds, -2);
        assert_eq!(shift("+1d 2h").seconds, 7200);
        assert_eq!(shift("+1d,2h").days, 1);
        let mixed = shift("-5mon1w2d");
        assert_eq!((mixed.months, mixed.days), (-5, -9));
        assert_eq!(shift("+1x").seconds, 1);
        assert_eq!(shift("+1mins").seconds, 1);
        assert_eq!(shift("+1foo2h").seconds, 7201);
        assert_eq!(shift("+1.5d").seconds, 1);
        assert_eq!(shift("+1.5d").days, 5);
        assert_eq!(shift("+1:5").seconds, 6);
        assert_eq!(shift("+1 2").seconds, 1);
        assert_eq!(shift("+1 2 3").seconds, 4);
        assert_eq!(shift("+1 2 3 4").seconds, 4);
        assert_eq!(shift("+1e3s").seconds, 4);
        assert_eq!(err("+1 2h"), "unparsable trailing text: '...h'");
        assert_eq!(shift("+5-1h").seconds, 3605);
        assert_eq!(shift("+5-1s+1s").seconds, 7);
        assert_eq!(shift("+5-1h-1h").seconds, 5);
        assert_eq!(shift("-1h+5").seconds, -3595);
        assert_eq!(shift("+5-").seconds, 5);
        assert!(!shift("+1h").calendar);
        assert!(shift("+0d").calendar);
    }

    #[test]
    fn sign_needs_a_number() {
        for (text, sign) in [("now+", '+'), ("+d", '+'), ("++1d", '+'), ("+-1d", '+')] {
            assert_eq!(
                err(text),
                format!("There should be number after '{sign}'"),
                "{text}"
            );
        }
        assert_eq!(err("+1d-"), "There should be number after '-'");
        assert_eq!(err("-"), "There should be number after '-'");
    }

    #[test]
    fn m_means_months_or_minutes() {
        assert_eq!(shift("+5m").months, 5);
        assert_eq!(shift("+6m").seconds, 360);
        assert_eq!(shift("-5m").months, -5);
        assert_eq!(shift("+1m2m").months, 3);
        assert_eq!(shift("+6m2m").seconds, 480);
        assert_eq!(shift("+1h 1x 7m").seconds, 3600 + 1 + 420);
        assert_eq!(shift("-1h-5m").months, -5);
        assert_eq!(shift("-1h 5m").seconds, -3600 - 300);
        assert_eq!(shift("+1d 3m").months, 3);
        assert_eq!(shift("+4294967297m").months, 1);
        assert_eq!(shift("+4294967302m").seconds, 360);
    }

    #[test]
    fn large_amounts_wrap_to_32_bits() {
        assert_eq!(shift("+4294967296s").seconds, 0);
        assert_eq!(shift("+99999999999s").seconds, 1_215_752_191);
        assert_eq!(shift("+3000000000s").seconds, -1_294_967_296);
        assert_eq!(shift("+999999999999999h").seconds, 661_123_568);
        assert_eq!(shift("+9999999999999999999s").seconds, -1);
        assert_eq!(
            shift("+2000000000s+2000000000s").seconds,
            4_000_000_000,
            "the exact sum itself is 64-bit"
        );
        assert_eq!(shift("+2147483647d+1d").days, i32::MIN);
    }

    #[test]
    fn unparsable_echo() {
        assert_eq!(err(""), "unparsable time: ");
        assert_eq!(err(" _,"), "unparsable time: ");
        assert_eq!(err(" monday"), "unparsable time: monday");
        assert_eq!(err("x@"), "unparsable time: x@");
        assert_eq!(err("@x"), "unparsable time: @@x");
        assert_eq!(err(" #x"), "unparsable time: ##x");
        assert_eq!(err("T12:00"), "unparsable time: T12:00");
        assert_eq!(err(":5"), "unparsable time: :5");
    }

    #[test]
    fn whole_words_only() {
        for word in ["Janu", "Ja", "sept", "sta+1", "epo"] {
            assert_eq!(err(word), format!("unparsable time: {word}"));
        }
        for word in ["tues", "thur", "sundays"] {
            assert_eq!(
                err(&format!("noon {word}")),
                format!("unparsable trailing text: '...{word}'")
            );
        }
    }
}
