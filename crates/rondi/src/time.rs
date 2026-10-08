//! At-style time parsing ported from RRDtool 1.11.0 `src/rrd_parsetime.c`.
//!
//! The scanner, the token tables, `tod`, `assign_date`, `day`, `plus_minus`
//! and `rrd_proc_start_end` keep the C control flow, integer widths and
//! messages. Local time goes through libc `localtime_r`/`mktime` like
//! upstream, so DST resolution follows the same `tm_isdst` hand-offs.

use libc::{c_int, c_long, time_t};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeType {
    Absolute,
    RelativeToStart,
    RelativeToEnd,
    RelativeToEpoch,
}

/// `rrd_time_value_t`.
#[derive(Clone, Copy)]
pub struct TimeValue {
    pub kind: TimeType,
    pub offset: i64,
    pub tm: libc::tm,
}

impl TimeValue {
    /// `mktime(&tv.tm) + tv.offset`, as rrd_create.c and rrd_update.c use an
    /// absolute time. Times relative to `start` or `end` have no value here.
    pub fn absolute(&self) -> Option<i64> {
        if matches!(
            self.kind,
            TimeType::RelativeToStart | TimeType::RelativeToEnd
        ) {
            return None;
        }
        let mut tm = self.tm;
        Some(mktime(&mut tm).wrapping_add(self.offset))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Midnight,
    Noon,
    Teatime,
    Pm,
    Am,
    Yesterday,
    Today,
    Tomorrow,
    Now,
    Start,
    End,
    Epoch,
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
    MonthsMinutes,
    Number,
    Plus,
    Minus,
    Dot,
    Colon,
    Slash,
    Id,
    Eof,
    /// JAN..DEC as 0..=11.
    Month(c_long),
    /// SUN..SAT as 0..=6.
    Weekday(c_int),
}

const VARIOUS_WORDS: &[(&str, Token)] = &[
    ("midnight", Token::Midnight),
    ("noon", Token::Noon),
    ("teatime", Token::Teatime),
    ("am", Token::Am),
    ("pm", Token::Pm),
    ("tomorrow", Token::Tomorrow),
    ("yesterday", Token::Yesterday),
    ("today", Token::Today),
    ("now", Token::Now),
    ("n", Token::Now),
    ("start", Token::Start),
    ("s", Token::Start),
    ("end", Token::End),
    ("e", Token::End),
    ("epoch", Token::Epoch),
    ("jan", Token::Month(0)),
    ("feb", Token::Month(1)),
    ("mar", Token::Month(2)),
    ("apr", Token::Month(3)),
    ("may", Token::Month(4)),
    ("jun", Token::Month(5)),
    ("jul", Token::Month(6)),
    ("aug", Token::Month(7)),
    ("sep", Token::Month(8)),
    ("oct", Token::Month(9)),
    ("nov", Token::Month(10)),
    ("dec", Token::Month(11)),
    ("january", Token::Month(0)),
    ("february", Token::Month(1)),
    ("march", Token::Month(2)),
    ("april", Token::Month(3)),
    ("june", Token::Month(5)),
    ("july", Token::Month(6)),
    ("august", Token::Month(7)),
    ("september", Token::Month(8)),
    ("october", Token::Month(9)),
    ("november", Token::Month(10)),
    ("december", Token::Month(11)),
    ("sunday", Token::Weekday(0)),
    ("sun", Token::Weekday(0)),
    ("monday", Token::Weekday(1)),
    ("mon", Token::Weekday(1)),
    ("tuesday", Token::Weekday(2)),
    ("tue", Token::Weekday(2)),
    ("wednesday", Token::Weekday(3)),
    ("wed", Token::Weekday(3)),
    ("thursday", Token::Weekday(4)),
    ("thu", Token::Weekday(4)),
    ("friday", Token::Weekday(5)),
    ("fri", Token::Weekday(5)),
    ("saturday", Token::Weekday(6)),
    ("sat", Token::Weekday(6)),
];

const TIME_MULTIPLIERS: &[(&str, Token)] = &[
    ("second", Token::Seconds),
    ("seconds", Token::Seconds),
    ("sec", Token::Seconds),
    ("s", Token::Seconds),
    ("minute", Token::Minutes),
    ("minutes", Token::Minutes),
    ("min", Token::Minutes),
    ("m", Token::MonthsMinutes),
    ("hour", Token::Hours),
    ("hours", Token::Hours),
    ("hr", Token::Hours),
    ("h", Token::Hours),
    ("day", Token::Days),
    ("days", Token::Days),
    ("d", Token::Days),
    ("week", Token::Weeks),
    ("weeks", Token::Weeks),
    ("wk", Token::Weeks),
    ("w", Token::Weeks),
    ("month", Token::Months),
    ("months", Token::Months),
    ("mon", Token::Months),
    ("year", Token::Years),
    ("years", Token::Years),
    ("yr", Token::Years),
    ("y", Token::Years),
];

/// C `atol`: glibc and Apple libc both return `strtol(s, NULL, 10)`, which
/// saturates on overflow.
fn atol(digits: &[u8]) -> c_long {
    let mut value: c_long = 0;
    for &digit in digits.iter().take_while(|byte| byte.is_ascii_digit()) {
        value = match value
            .checked_mul(10)
            .and_then(|value| value.checked_add(c_long::from(digit - b'0')))
        {
            Some(value) => value,
            None => return c_long::MAX,
        };
    }
    value
}

/// C `atoi`: `(int) strtol(s, NULL, 10)` in both libcs.
fn atoi(digits: &[u8]) -> c_int {
    atol(digits) as c_int
}

/// C `isspace` for the bytes the scanner can see.
fn is_c_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

struct Parser<'a> {
    input: &'a [u8],
    /// `sct`: next unread byte of the argument.
    sct: usize,
    /// `scc`: arguments not yet fetched; rrd_parsetime always passes one.
    scc: c_int,
    need: bool,
    sc_token: Vec<u8>,
    sc_tokid: Token,
    specials: &'static [(&'static str, Token)],
    tv: TimeValue,
    /// `plus_minus`'s function-scope statics.
    op: Token,
    prev_multiplier: Token,
}

impl Parser<'_> {
    fn token_text(&self) -> String {
        String::from_utf8_lossy(&self.sc_token).into_owned()
    }

    fn rest(&self) -> String {
        String::from_utf8_lossy(&self.input[self.sct..]).into_owned()
    }

    fn parse_token(&mut self) -> Token {
        self.sc_tokid = self
            .specials
            .iter()
            .find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(&self.sc_token))
            .map_or(Token::Id, |(_, token)| *token);
        self.sc_tokid
    }

    fn token(&mut self) -> Token {
        loop {
            self.sc_token.clear();
            self.sc_tokid = Token::Eof;
            if self.need {
                if self.scc < 1 {
                    return self.sc_tokid;
                }
                self.sct = 0;
                self.scc -= 1;
                self.need = false;
            }
            while self.sct < self.input.len()
                && (is_c_space(self.input[self.sct]) || matches!(self.input[self.sct], b'_' | b','))
            {
                self.sct += 1;
            }
            if self.sct >= self.input.len() {
                self.need = true;
                continue;
            }
            let first = self.input[self.sct];
            self.sct += 1;
            self.sc_token.push(first);
            if first.is_ascii_digit() {
                while self.sct < self.input.len() && self.input[self.sct].is_ascii_digit() {
                    self.sc_token.push(self.input[self.sct]);
                    self.sct += 1;
                }
                self.sc_tokid = Token::Number;
                return self.sc_tokid;
            }
            if first.is_ascii_alphabetic() {
                while self.sct < self.input.len() && self.input[self.sct].is_ascii_alphabetic() {
                    self.sc_token.push(self.input[self.sct]);
                    self.sct += 1;
                }
                return self.parse_token();
            }
            self.sc_tokid = match first {
                b':' => Token::Colon,
                b'.' => Token::Dot,
                b'+' => Token::Plus,
                b'-' => Token::Minus,
                b'/' => Token::Slash,
                _ => {
                    self.sct -= 1;
                    Token::Eof
                }
            };
            return self.sc_tokid;
        }
    }

    fn expect2(
        &mut self,
        desired: Token,
        complaint: impl FnOnce() -> String,
    ) -> Result<(), String> {
        if self.token() != desired {
            return Err(complaint());
        }
        Ok(())
    }

    /// `doop` is `None` for PREVIOUS_OP: repeat the last sign with the
    /// prefetched number.
    fn plus_minus(&mut self, doop: Option<Token>) -> Result<(), String> {
        if let Some(op) = doop {
            self.op = op;
            self.expect2(Token::Number, || {
                format!(
                    "There should be number after '{}'",
                    if op == Token::Plus { '+' } else { '-' }
                )
            })?;
            self.prev_multiplier = Token::Eof;
        }
        let mut delta = atoi(&self.sc_token);
        if self.token() == Token::MonthsMinutes {
            self.sc_tokid = match self.prev_multiplier {
                Token::Days | Token::Weeks | Token::Months | Token::Years => Token::Months,
                Token::Seconds | Token::Minutes | Token::Hours => Token::Minutes,
                _ if delta < 6 => Token::Months,
                _ => Token::Minutes,
            };
        }
        self.prev_multiplier = self.sc_tokid;
        let plus = self.op == Token::Plus;
        let signed = |value: c_int| if plus { value } else { value.wrapping_neg() };
        let tm = &mut self.tv.tm;
        match self.sc_tokid {
            Token::Years => tm.tm_year = tm.tm_year.wrapping_add(signed(delta)),
            Token::Months => tm.tm_mon = tm.tm_mon.wrapping_add(signed(delta)),
            Token::Weeks | Token::Days => {
                if self.sc_tokid == Token::Weeks {
                    delta = delta.wrapping_mul(7);
                }
                tm.tm_mday = tm.tm_mday.wrapping_add(signed(delta));
            }
            Token::Hours => {
                self.tv.offset = self
                    .tv
                    .offset
                    .wrapping_add(i64::from(signed(delta.wrapping_mul(60).wrapping_mul(60))));
            }
            Token::Minutes => {
                self.tv.offset = self
                    .tv
                    .offset
                    .wrapping_add(i64::from(signed(delta.wrapping_mul(60))));
            }
            _ => self.tv.offset = self.tv.offset.wrapping_add(i64::from(signed(delta))),
        }
        Ok(())
    }

    fn tod(&mut self) -> Result<(), String> {
        let mut minute: c_int = 0;
        let scc_sv = self.scc;
        let sct_sv = self.sct;
        let sc_tokid_sv = self.sc_tokid;

        if self.sc_token.len() > 2 {
            return Ok(());
        }
        let mut hour = atoi(&self.sc_token);

        self.token();
        if matches!(self.sc_tokid, Token::Slash | Token::Dot) {
            self.scc = scc_sv;
            self.sct = sct_sv;
            self.sc_tokid = sc_tokid_sv;
            self.sc_token = hour.to_string().into_bytes();
            return Ok(());
        }
        if self.sc_tokid == Token::Colon {
            self.expect2(Token::Number, || {
                String::from("Parsing HH:MM syntax, expecting MM as number, got none")
            })?;
            minute = atoi(&self.sc_token);
            if minute > 59 {
                return Err(format!("parsing HH:MM syntax, got MM = {minute} (>59!)"));
            }
            self.token();
        }

        if matches!(self.sc_tokid, Token::Am | Token::Pm) {
            if hour > 12 {
                return Err(String::from("there cannot be more than 12 AM or PM hours"));
            }
            if self.sc_tokid == Token::Pm {
                if hour != 12 {
                    hour += 12;
                }
            } else if hour == 12 {
                hour = 0;
            }
            self.token();
        } else if hour > 23 {
            self.scc = scc_sv;
            self.sct = sct_sv;
            self.sc_tokid = sc_tokid_sv;
            self.sc_token = hour.to_string().into_bytes();
            return Ok(());
        }
        self.tv.tm.tm_hour = hour;
        self.tv.tm.tm_min = minute;
        self.tv.tm.tm_sec = 0;
        if self.tv.tm.tm_hour == 24 {
            self.tv.tm.tm_hour = 0;
            self.tv.tm.tm_mday += 1;
        }
        Ok(())
    }

    // The `%d` conversions receive a `long` in C; both supported ABIs print
    // its low 32 bits, hence the `as c_int` casts in the messages.
    fn assign_date(&mut self, mday: c_long, mon: c_long, year: c_long) -> Result<(), String> {
        let mut year = year;
        if year > 138 {
            if year > 1970 {
                year -= 1900;
            } else {
                return Err(format!(
                    "invalid year {} (should be either 00-99 or >1900)",
                    year as c_int
                ));
            }
        } else if (0..38).contains(&year) {
            year += 100;
        }
        if year < 70 {
            return Err(String::from(
                "won't handle dates before epoch (01/01/1970), sorry",
            ));
        }
        self.tv.tm.tm_mday = mday as c_int;
        self.tv.tm.tm_mon = mon as c_int;
        self.tv.tm.tm_year = year as c_int;
        Ok(())
    }

    fn day(&mut self) -> Result<(), String> {
        let mut mday: c_long = 0;
        let mut year = c_long::from(self.tv.tm.tm_year);
        match self.sc_tokid {
            Token::Yesterday | Token::Today => {
                if self.sc_tokid == Token::Yesterday {
                    self.tv.tm.tm_mday -= 1;
                }
                self.token();
            }
            Token::Tomorrow => {
                self.tv.tm.tm_mday += 1;
                self.token();
            }
            Token::Month(mon) => {
                self.expect2(Token::Number, || {
                    String::from("the day of the month should follow month name")
                })?;
                mday = atol(&self.sc_token);
                if self.token() == Token::Number {
                    year = atol(&self.sc_token);
                    self.token();
                } else {
                    year = c_long::from(self.tv.tm.tm_year);
                }
                self.assign_date(mday, mon, year)?;
            }
            Token::Weekday(wday) => {
                self.tv.tm.tm_mday += wday - self.tv.tm.tm_wday;
                self.token();
            }
            Token::Number => {
                let mut mon = atol(&self.sc_token);
                if mon > 10 * 365 * 24 * 60 * 60 {
                    localtime(mon as time_t, &mut self.tv.tm);
                    self.token();
                    return Ok(());
                }
                if mon > 19_700_101 && mon < 24_000_101 {
                    year = atol(&self.sc_token[..4]);
                    mon = atol(&self.sc_token[4..6]);
                    mday = atol(&self.sc_token[6..8]);
                    self.token();
                } else {
                    self.token();
                    if mon <= 31 && matches!(self.sc_tokid, Token::Slash | Token::Dot) {
                        let sep = self.sc_tokid;
                        let (name, separator) = if sep == Token::Dot {
                            ("month", '.')
                        } else {
                            ("day", '/')
                        };
                        self.expect2(Token::Number, || {
                            format!("there should be {name} number after '{separator}'")
                        })?;
                        mday = atol(&self.sc_token);
                        if self.token() == sep {
                            self.expect2(Token::Number, || {
                                format!("there should be year number after '{separator}'")
                            })?;
                            year = atol(&self.sc_token);
                            self.token();
                        }
                        // European DD.MM.YYYY.
                        if sep == Token::Dot {
                            std::mem::swap(&mut mday, &mut mon);
                        }
                    }
                }
                mon -= 1;
                if !(0..=11).contains(&mon) {
                    return Err(format!("did you really mean month {}?", (mon + 1) as c_int));
                }
                if !(1..=31).contains(&mday) {
                    return Err(format!(
                        "I'm afraid that {} is not a valid day of the month",
                        mday as c_int
                    ));
                }
                self.assign_date(mday, mon, year)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn parse(&mut self, now: time_t) -> Result<(), String> {
        let mut hr = 0;
        self.tv.kind = TimeType::Absolute;
        self.tv.offset = 0;
        localtime(now, &mut self.tv.tm);
        self.tv.tm.tm_isdst = -1;

        self.token();
        match self.sc_tokid {
            Token::Plus | Token::Minus => {}
            Token::Epoch | Token::Start | Token::End | Token::Now => {
                let time_reference = self.sc_tokid;
                if time_reference != Token::Now {
                    self.tv.kind = match time_reference {
                        Token::Epoch => TimeType::RelativeToEpoch,
                        Token::Start => TimeType::RelativeToStart,
                        _ => TimeType::RelativeToEnd,
                    };
                    let tm = &mut self.tv.tm;
                    tm.tm_sec = 0;
                    tm.tm_min = 0;
                    tm.tm_hour = 0;
                    tm.tm_mday = 0;
                    tm.tm_mon = 0;
                    tm.tm_year = 0;
                }
                self.token();
                if !matches!(self.sc_tokid, Token::Plus | Token::Minus) {
                    if time_reference != Token::Now {
                        return Err(String::from(
                            "'start' or 'end' MUST be followed by +|- offset",
                        ));
                    } else if self.sc_tokid != Token::Eof {
                        return Err(String::from(
                            "if 'now' is followed by a token it must be +|- offset",
                        ));
                    }
                }
            }
            Token::Number => {
                let hour_sv = self.tv.tm.tm_hour;
                let year_sv = self.tv.tm.tm_year;
                // Sentinels show whether tod() and day() set anything.
                self.tv.tm.tm_hour = 30;
                self.tv.tm.tm_year = 30000;
                self.tod()?;
                self.day()?;
                if self.tv.tm.tm_hour == 30 && self.tv.tm.tm_year != 30000 {
                    self.tod()?;
                }
                if self.tv.tm.tm_hour == 30 {
                    self.tv.tm.tm_hour = hour_sv;
                }
                if self.tv.tm.tm_year == 30000 {
                    self.tv.tm.tm_year = year_sv;
                }
            }
            Token::Month(_) => {
                self.day()?;
                if self.sc_tokid == Token::Number {
                    self.tod()?;
                }
            }
            Token::Teatime | Token::Noon | Token::Midnight => {
                if self.sc_tokid == Token::Teatime {
                    hr += 4;
                }
                if self.sc_tokid != Token::Midnight {
                    hr += 12;
                }
                self.tv.tm.tm_hour = hr;
                self.tv.tm.tm_min = 0;
                self.tv.tm.tm_sec = 0;
                self.token();
                self.day()?;
            }
            _ => {
                return Err(format!(
                    "unparsable time: {}{}",
                    self.token_text(),
                    self.rest()
                ));
            }
        }

        if matches!(self.sc_tokid, Token::Plus | Token::Minus) {
            self.specials = TIME_MULTIPLIERS;
            while matches!(self.sc_tokid, Token::Plus | Token::Minus | Token::Number) {
                if self.sc_tokid == Token::Number {
                    self.plus_minus(None)?;
                } else {
                    self.plus_minus(Some(self.sc_tokid))?;
                }
                self.token();
            }
        }

        if self.sc_tokid != Token::Eof {
            return Err(format!(
                "unparsable trailing text: '...{}{}'",
                self.token_text(),
                self.rest()
            ));
        }

        // Normalizes the calendar fields once, after every day/month/year
        // offset has been accumulated.
        if self.tv.kind == TimeType::Absolute && mktime(&mut self.tv.tm) == -1 {
            return Err(String::from(
                "the specified time is incorrect (out of range?)",
            ));
        }
        Ok(())
    }
}

/// `rrd_parsetime()`. `now` stands in for upstream's `time(NULL)`.
pub fn rrd_parsetime(spec: &str, now: i64) -> Result<TimeValue, String> {
    let mut parser = Parser {
        input: spec.as_bytes(),
        sct: 0,
        scc: 1,
        need: true,
        sc_token: Vec::new(),
        sc_tokid: Token::Eof,
        specials: VARIOUS_WORDS,
        tv: TimeValue {
            kind: TimeType::Absolute,
            offset: 0,
            tm: zeroed_tm(),
        },
        op: Token::Plus,
        prev_multiplier: Token::Eof,
    };
    parser.parse(now as time_t)?;
    Ok(parser.tv)
}

/// `rrd_proc_start_end()`.
pub fn rrd_proc_start_end(
    start_tv: &mut TimeValue,
    end_tv: &mut TimeValue,
) -> Result<(i64, i64), String> {
    if start_tv.kind == TimeType::RelativeToEnd && end_tv.kind == TimeType::RelativeToStart {
        return Err(String::from(
            "the start and end times cannot be specified relative to each other",
        ));
    }
    if start_tv.kind == TimeType::RelativeToStart {
        return Err(String::from(
            "the start time cannot be specified relative to itself",
        ));
    }
    if end_tv.kind == TimeType::RelativeToEnd {
        return Err(String::from(
            "the end time cannot be specified relative to itself",
        ));
    }

    // The anchor is broken down with its own tm_isdst, not -1, so a day
    // offset across a DST change stays a multiple of 86400 s.
    let shifted = |anchor: i64, relative: &TimeValue| {
        let mut tmtmp = zeroed_tm();
        localtime(anchor as time_t, &mut tmtmp);
        tmtmp.tm_mday = tmtmp.tm_mday.wrapping_add(relative.tm.tm_mday);
        tmtmp.tm_mon = tmtmp.tm_mon.wrapping_add(relative.tm.tm_mon);
        tmtmp.tm_year = tmtmp.tm_year.wrapping_add(relative.tm.tm_year);
        mktime(&mut tmtmp).wrapping_add(relative.offset)
    };

    let mut start = if start_tv.kind == TimeType::RelativeToEnd {
        let anchor = mktime(&mut end_tv.tm).wrapping_add(end_tv.offset);
        shifted(anchor, start_tv)
    } else {
        mktime(&mut start_tv.tm).wrapping_add(start_tv.offset)
    };
    let end = if end_tv.kind == TimeType::RelativeToStart {
        start = mktime(&mut start_tv.tm).wrapping_add(start_tv.offset);
        shifted(start, end_tv)
    } else {
        mktime(&mut end_tv.tm).wrapping_add(end_tv.offset)
    };
    Ok((start, end))
}

/// Parses a time that must not refer to `start` or `end`, for callers such
/// as VRULE that only need the resulting timestamp.
pub fn parse_rrd_time(value: &str, now: i64) -> Result<i64, Box<dyn std::error::Error>> {
    rrd_parsetime(value, now)?.absolute().ok_or_else(|| {
        "specifying time relative to the 'start' or 'end' makes no sense here".into()
    })
}

/// Parses fetch, xport and graph `--start`/`--end` like rrd_fetch.c: absent
/// options default to `end-24h` and `now`, and parser errors carry the
/// option's prefix.
pub fn resolve_rrd_range_times(
    start_spec: Option<&str>,
    end_spec: Option<&str>,
    now: i64,
) -> Result<(i64, i64), Box<dyn std::error::Error>> {
    let mut start_tv = rrd_parsetime(start_spec.unwrap_or("end-24h"), now)
        .map_err(|error| format!("start time: {error}"))?;
    let mut end_tv = rrd_parsetime(end_spec.unwrap_or("now"), now)
        .map_err(|error| format!("end time: {error}"))?;
    Ok(rrd_proc_start_end(&mut start_tv, &mut end_tv)?)
}

fn zeroed_tm() -> libc::tm {
    // SAFETY: libc::tm is plain integers plus, on some targets, a nullable
    // zone pointer; all-zero is a valid value.
    unsafe { std::mem::zeroed() }
}

#[cfg(unix)]
fn localtime(time: time_t, tm: &mut libc::tm) {
    // SAFETY: both pointers are valid for the call. Like upstream, a failed
    // conversion leaves `tm` as it was.
    unsafe { libc::localtime_r(&time, tm) };
}

#[cfg(unix)]
fn mktime(tm: &mut libc::tm) -> i64 {
    // SAFETY: `tm` is a valid, exclusively borrowed struct tm.
    unsafe { libc::mktime(tm) as i64 }
}

// Without a POSIX libc the calendar arithmetic runs in UTC.
#[cfg(not(unix))]
fn localtime(time: time_t, tm: &mut libc::tm) {
    let days = time.div_euclid(86_400);
    let seconds = time.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let mday = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    tm.tm_sec = (seconds % 60) as c_int;
    tm.tm_min = (seconds / 60 % 60) as c_int;
    tm.tm_hour = (seconds / 3_600) as c_int;
    tm.tm_mday = mday as c_int;
    tm.tm_mon = (month - 1) as c_int;
    tm.tm_year = (year - 1900) as c_int;
    tm.tm_wday = (days + 4).rem_euclid(7) as c_int;
    tm.tm_isdst = 0;
}

#[cfg(not(unix))]
fn mktime(tm: &mut libc::tm) -> i64 {
    let months = i64::from(tm.tm_year) * 12 + i64::from(tm.tm_mon);
    let year = 1900 + months.div_euclid(12);
    let month = months.rem_euclid(12) + 1;
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let month_index = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_index + 2) / 5;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468 + i64::from(tm.tm_mday) - 1;
    let time = days * 86_400
        + i64::from(tm.tm_hour) * 3_600
        + i64::from(tm.tm_min) * 60
        + i64::from(tm.tm_sec);
    localtime(time, tm);
    time
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
        TimeType, UpdateTimestamp, parse_rrd_time, parse_rrd_update_timestamp,
        resolve_rrd_range_times, rrd_parsetime,
    };

    #[test]
    fn parses_epochs_and_fixed_offsets_from_now() {
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
            parse_rrd_time("1000000000+1h-30min 5s", 0).unwrap(),
            1_000_001_795
        );
        assert_eq!(
            parse_rrd_time("-3600", 2_000_000_000).unwrap(),
            1_999_996_400
        );
    }

    #[test]
    fn reports_rrd_parsetime_messages() {
        let error = |spec: &str| rrd_parsetime(spec, 2_000_000_000).err().unwrap();
        assert_eq!(error("12:60"), "parsing HH:MM syntax, got MM = 60 (>59!)");
        assert_eq!(error("13pm"), "there cannot be more than 12 AM or PM hours");
        assert_eq!(error("13/01/2003"), "did you really mean month 13?");
        assert_eq!(
            error("01/32/2003"),
            "I'm afraid that 32 is not a valid day of the month"
        );
        assert_eq!(
            error("01/02/1969"),
            "invalid year 1969 (should be either 00-99 or >1900)"
        );
        assert_eq!(
            error("01/02/50"),
            "won't handle dates before epoch (01/01/1970), sorry"
        );
        assert_eq!(
            error("1 jan"),
            "the day of the month should follow month name"
        );
        assert_eq!(
            error("now foo"),
            "if 'now' is followed by a token it must be +|- offset"
        );
        assert_eq!(error("now+"), "There should be number after '+'");
        assert_eq!(
            error("epoch"),
            "'start' or 'end' MUST be followed by +|- offset"
        );
        assert_eq!(error("foo"), "unparsable time: foo");
        assert_eq!(error("@x"), "unparsable time: @@x");
        assert_eq!(
            error("Jan 1 2004 noon"),
            "unparsable trailing text: '...noon'"
        );
        assert_eq!(error("315360000"), "did you really mean month 315360000?");
        assert_eq!(error("2003-01-02"), "did you really mean month 2003?");
    }

    #[test]
    fn start_and_end_references_stay_relative() {
        let parsed = rrd_parsetime("end-1h", 2_000_000_000).unwrap();
        assert_eq!(parsed.kind, TimeType::RelativeToEnd);
        assert_eq!(parsed.offset, -3_600);
        assert!(parsed.absolute().is_none());
        assert!(parse_rrd_time("start+1d", 2_000_000_000).is_err());
    }

    #[test]
    fn the_m_unit_guess_resets_at_each_sign() {
        let months = rrd_parsetime("now-1h-5m", 2_000_000_000).unwrap();
        assert_eq!(months.offset, -3_600);
        let minutes = rrd_parsetime("now-1h 5m", 2_000_000_000).unwrap();
        assert_eq!(minutes.offset, -3_900);
    }

    #[test]
    fn resolves_range_times_relative_to_the_other_endpoint() {
        assert_eq!(
            resolve_rrd_range_times(Some("end-30s"), Some("2000000000"), 2_000_000_000).unwrap(),
            (1_999_999_970, 2_000_000_000)
        );
        assert_eq!(
            resolve_rrd_range_times(Some("2000000000"), Some("start+30s"), 2_000_000_000).unwrap(),
            (2_000_000_000, 2_000_000_030)
        );
        assert_eq!(
            resolve_rrd_range_times(None, None, 2_000_000_000).unwrap(),
            (1_999_913_600, 2_000_000_000)
        );
    }

    #[test]
    fn rejects_self_and_mutually_relative_range_times() {
        let now = 2_000_000_000;
        let message = |start, end| {
            resolve_rrd_range_times(start, end, now)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            message(Some("start-1h"), None),
            "the start time cannot be specified relative to itself"
        );
        assert_eq!(
            message(None, Some("end+1h")),
            "the end time cannot be specified relative to itself"
        );
        assert_eq!(
            message(Some("end-1h"), Some("start+1h")),
            "the start and end times cannot be specified relative to each other"
        );
        assert_eq!(
            message(Some("foo"), None),
            "start time: unparsable time: foo"
        );
        assert_eq!(message(None, Some("bar")), "end time: unparsable time: bar");
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
