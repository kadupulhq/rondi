//! Pure-Rust locale types and `LC_TIME` formatting tables for Tier 1 locales.
//!
//! This module has no ambient authority: it never reads environment variables
//! or calls OS locale APIs. Callers pass an explicit [`Locale`] value.

use std::fmt;
use std::str::FromStr;

/// Tier 1 locales supported by Rondi's pure-Rust localization and `LC_TIME`
/// formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Locale {
    #[default]
    En,
    De,
    Es,
    Fr,
    It,
    PtBr,
    Nl,
    Ru,
    Uk,
    Ja,
    Ko,
    ZhCn,
    ZhTw,
    Ar,
    Hi,
    Pl,
    Tr,
    Sv,
    Vi,
    Id,
}

/// Error returned when parsing an unsupported locale identifier via [`FromStr`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLocaleError {
    raw: String,
}

impl ParseLocaleError {
    /// The unrecognized input string.
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for ParseLocaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported locale '{}'", self.raw)
    }
}

impl std::error::Error for ParseLocaleError {}

/// Localized `LC_TIME` names and composite format templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeLocaleData {
    /// Full month names indexed `0..12` (January to December), used by `%B`.
    pub months_full: [&'static str; 12],
    /// Abbreviated month names indexed `0..12` (Jan to Dec), used by `%b`/`%h`.
    pub months_abbr: [&'static str; 12],
    /// Full weekday names indexed `0..7` (Sunday to Saturday), used by `%A`.
    pub weekdays_full: [&'static str; 7],
    /// Abbreviated weekday names indexed `0..7` (Sun to Sat), used by `%a`.
    pub weekdays_abbr: [&'static str; 7],
    /// Ante meridiem indicator (`%p`).
    pub am: &'static str,
    /// Post meridiem indicator (`%p`).
    pub pm: &'static str,
    /// Lowercase ante meridiem indicator (`%P`).
    pub am_lower: &'static str,
    /// Lowercase post meridiem indicator (`%P`).
    pub pm_lower: &'static str,
    /// Locale date-and-time template (`%c`).
    pub date_time_fmt: &'static str,
    /// Locale date template (`%x`).
    pub date_fmt: &'static str,
    /// Locale time template (`%X`).
    pub time_fmt: &'static str,
    /// Locale 12-hour clock time template (`%r`).
    pub time_12h_fmt: &'static str,
}

impl Locale {
    /// All 20 supported Tier 1 locales in canonical order.
    pub const ALL: &'static [Locale] = &[
        Self::En,
        Self::De,
        Self::Es,
        Self::Fr,
        Self::It,
        Self::PtBr,
        Self::Nl,
        Self::Ru,
        Self::Uk,
        Self::Ja,
        Self::Ko,
        Self::ZhCn,
        Self::ZhTw,
        Self::Ar,
        Self::Hi,
        Self::Pl,
        Self::Tr,
        Self::Sv,
        Self::Vi,
        Self::Id,
    ];

    /// Canonical BCP-47 language tag for this locale.
    pub const fn code(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::De => "de",
            Self::Es => "es",
            Self::Fr => "fr",
            Self::It => "it",
            Self::PtBr => "pt-BR",
            Self::Nl => "nl",
            Self::Ru => "ru",
            Self::Uk => "uk",
            Self::Ja => "ja",
            Self::Ko => "ko",
            Self::ZhCn => "zh-CN",
            Self::ZhTw => "zh-TW",
            Self::Ar => "ar",
            Self::Hi => "hi",
            Self::Pl => "pl",
            Self::Tr => "tr",
            Self::Sv => "sv",
            Self::Vi => "vi",
            Self::Id => "id",
        }
    }

    /// Endonym (native name) of this locale.
    pub const fn native_name(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::De => "Deutsch",
            Self::Es => "Español",
            Self::Fr => "Français",
            Self::It => "Italiano",
            Self::PtBr => "Português (Brasil)",
            Self::Nl => "Nederlands",
            Self::Ru => "Русский",
            Self::Uk => "Українська",
            Self::Ja => "日本語",
            Self::Ko => "한국어",
            Self::ZhCn => "简体中文",
            Self::ZhTw => "繁體中文",
            Self::Ar => "العربية",
            Self::Hi => "हिन्दी",
            Self::Pl => "Polski",
            Self::Tr => "Türkçe",
            Self::Sv => "Svenska",
            Self::Vi => "Tiếng Việt",
            Self::Id => "Bahasa Indonesia",
        }
    }

    /// Parses a BCP-47 or POSIX locale identifier (such as `de_DE.UTF-8`,
    /// `pt-BR`, `zh_Hant_TW`, `C`, or `POSIX@euro`).
    ///
    /// Returns `None` for empty or unrecognized identifiers.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Strip POSIX `@modifier` and `.codeset` suffixes.
        let base = trimmed
            .split('@')
            .next()
            .unwrap_or("")
            .split('.')
            .next()
            .unwrap_or("")
            .trim();
        if base.is_empty() {
            return None;
        }
        let normalized = base.to_ascii_lowercase().replace('_', "-");
        if matches!(normalized.as_str(), "c" | "posix") {
            return Some(Self::En);
        }
        let mut parts = normalized.split('-');
        let lang = parts.next()?;
        let rest: Vec<&str> = parts.collect();
        match lang {
            "en" => Some(Self::En),
            "de" => Some(Self::De),
            "es" => Some(Self::Es),
            "fr" => Some(Self::Fr),
            "it" => Some(Self::It),
            "pt" => Some(Self::PtBr),
            "nl" => Some(Self::Nl),
            "ru" => Some(Self::Ru),
            "uk" => Some(Self::Uk),
            "ja" => Some(Self::Ja),
            "ko" => Some(Self::Ko),
            "zh" => {
                let traditional = rest
                    .iter()
                    .any(|sub| matches!(*sub, "tw" | "hk" | "mo" | "hant"));
                if traditional {
                    Some(Self::ZhTw)
                } else {
                    Some(Self::ZhCn)
                }
            }
            "ar" => Some(Self::Ar),
            "hi" => Some(Self::Hi),
            "pl" => Some(Self::Pl),
            "tr" => Some(Self::Tr),
            "sv" => Some(Self::Sv),
            "vi" => Some(Self::Vi),
            "id" | "in" => Some(Self::Id),
            _ => None,
        }
    }

    /// Resolves a POSIX locale environment variable value, falling back to
    /// [`Locale::En`] when empty, `C`/`POSIX`, or unrecognized.
    pub fn from_posix_env(raw: &str) -> Self {
        Self::parse(raw).unwrap_or(Self::En)
    }

    /// Returns the pure-Rust `LC_TIME` formatting tables for this locale.
    pub const fn time_data(self) -> &'static TimeLocaleData {
        match self {
            Self::En => &TIME_EN,
            Self::De => &TIME_DE,
            Self::Es => &TIME_ES,
            Self::Fr => &TIME_FR,
            Self::It => &TIME_IT,
            Self::PtBr => &TIME_PT_BR,
            Self::Nl => &TIME_NL,
            Self::Ru => &TIME_RU,
            Self::Uk => &TIME_UK,
            Self::Ja => &TIME_JA,
            Self::Ko => &TIME_KO,
            Self::ZhCn => &TIME_ZH_CN,
            Self::ZhTw => &TIME_ZH_TW,
            Self::Ar => &TIME_AR,
            Self::Hi => &TIME_HI,
            Self::Pl => &TIME_PL,
            Self::Tr => &TIME_TR,
            Self::Sv => &TIME_SV,
            Self::Vi => &TIME_VI,
            Self::Id => &TIME_ID,
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Locale {
    type Err = ParseLocaleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s).ok_or_else(|| ParseLocaleError { raw: s.to_owned() })
    }
}

/// Broken-down calendar date and time passed to [`format_strftime`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateTimeParts {
    /// Full Gregorian year (e.g. `2001`).
    pub year: i32,
    /// Month of the year (`1..=12`).
    pub month: u8,
    /// Day of the month (`1..=31`).
    pub day: u8,
    /// Day of the week (`0` = Sunday .. `6` = Saturday).
    pub weekday: u8,
    /// Day of the year (`1..=366`).
    pub yearday: u16,
    /// Hour of the day (`0..=23`).
    pub hour: u8,
    /// Minute (`0..=59`).
    pub minute: u8,
    /// Second (`0..=60`).
    pub second: u8,
    /// Offset from UTC in seconds (positive east of UTC).
    pub utc_offset_secs: i32,
    /// Time zone abbreviation (e.g. `"UTC"`).
    pub tz_name: String,
    /// Optional Unix epoch timestamp in seconds (for `%s`).
    pub epoch_secs: Option<i64>,
}

impl DateTimeParts {
    /// Converts a Unix timestamp (seconds since `1970-01-01T00:00:00Z`) to
    /// UTC broken-down calendar parts in pure Rust.
    pub fn from_utc_timestamp(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400) as u32;
        let hour = (sod / 3_600) as u8;
        let minute = ((sod % 3_600) / 60) as u8;
        let second = (sod % 60) as u8;
        let (year, month, day, yearday) = civil_from_days(days);
        // 1970-01-01 was Thursday (4).
        let weekday = (days + 4).rem_euclid(7) as u8;
        Self {
            year,
            month,
            day,
            weekday,
            yearday,
            hour,
            minute,
            second,
            utc_offset_secs: 0,
            tz_name: "UTC".to_owned(),
            epoch_secs: Some(secs),
        }
    }
}

/// Formats `dt` using a `strftime` format string and the pure-Rust `LC_TIME`
/// tables for `locale`.
pub fn format_strftime(fmt: &str, dt: &DateTimeParts, locale: Locale) -> String {
    let mut out = String::with_capacity(fmt.len() + 16);
    format_strftime_into(fmt, dt, locale, &mut out, 0);
    out
}

/// Formats `dt` with [`format_strftime`] and bounds the result to at most
/// `capacity` bytes (truncating cleanly at a UTF-8 character boundary).
pub fn format_strftime_bytes(
    fmt: &str,
    dt: &DateTimeParts,
    locale: Locale,
    capacity: usize,
) -> Vec<u8> {
    if capacity == 0 {
        return Vec::new();
    }
    let formatted = format_strftime(fmt, dt, locale);
    let bytes = formatted.as_bytes();
    if bytes.len() <= capacity {
        return bytes.to_vec();
    }
    let mut end = capacity;
    while end > 0 && !formatted.is_char_boundary(end) {
        end -= 1;
    }
    bytes[..end].to_vec()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PadStyle {
    Default,
    None,
    Space,
    Zero,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseMod {
    Normal,
    Upper,
    Swap,
}

fn format_strftime_into(
    fmt: &str,
    dt: &DateTimeParts,
    locale: Locale,
    out: &mut String,
    depth: u8,
) {
    // Guard against recursive composite templates.
    if depth > 4 {
        return;
    }
    let data = locale.time_data();
    let bytes = fmt.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            let start = i;
            i += 1;
            while i < bytes.len() && bytes[i] != b'%' {
                i += 1;
            }
            out.push_str(&fmt[start..i]);
            continue;
        }
        let spec_start = i;
        i += 1;
        if i >= bytes.len() {
            out.push('%');
            break;
        }
        let mut pad = PadStyle::Default;
        let mut case_mod = CaseMod::Normal;
        while i < bytes.len() {
            match bytes[i] {
                b'-' => pad = PadStyle::None,
                b'_' => pad = PadStyle::Space,
                b'0' => pad = PadStyle::Zero,
                b'^' => case_mod = CaseMod::Upper,
                b'#' => case_mod = CaseMod::Swap,
                _ => break,
            }
            i += 1;
        }
        let width_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let width: Option<usize> = if i > width_start {
            fmt[width_start..i].parse().ok()
        } else {
            None
        };
        if i < bytes.len() && matches!(bytes[i], b'E' | b'O') {
            i += 1;
        }
        if i >= bytes.len() {
            out.push_str(&fmt[spec_start..]);
            break;
        }
        let conv = bytes[i];
        i += 1;
        match conv {
            b'%' => out.push('%'),
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'a' => push_text(
                out,
                data.weekdays_abbr[(dt.weekday % 7) as usize],
                case_mod,
                width,
                pad,
            ),
            b'A' => push_text(
                out,
                data.weekdays_full[(dt.weekday % 7) as usize],
                case_mod,
                width,
                pad,
            ),
            b'b' | b'h' => {
                let idx = month_index(dt.month);
                push_text(out, data.months_abbr[idx], case_mod, width, pad);
            }
            b'B' => {
                let idx = month_index(dt.month);
                push_text(out, data.months_full[idx], case_mod, width, pad);
            }
            b'p' => {
                let s = if dt.hour < 12 { data.am } else { data.pm };
                push_text(out, s, case_mod, width, pad);
            }
            b'P' => {
                let s = if dt.hour < 12 {
                    data.am_lower
                } else {
                    data.pm_lower
                };
                push_text(out, s, case_mod, width, pad);
            }
            b'c' => format_strftime_into(data.date_time_fmt, dt, locale, out, depth + 1),
            b'x' => format_strftime_into(data.date_fmt, dt, locale, out, depth + 1),
            b'X' => format_strftime_into(data.time_fmt, dt, locale, out, depth + 1),
            b'r' => format_strftime_into(data.time_12h_fmt, dt, locale, out, depth + 1),
            b'C' => push_int(out, i64::from(dt.year.div_euclid(100)), 2, b'0', pad, width),
            b'd' => push_int(out, i64::from(dt.day), 2, b'0', pad, width),
            b'e' => push_int(out, i64::from(dt.day), 2, b' ', pad, width),
            b'D' => format_strftime_into("%m/%d/%y", dt, locale, out, depth + 1),
            b'F' => format_strftime_into("%Y-%m-%d", dt, locale, out, depth + 1),
            b'H' => push_int(out, i64::from(dt.hour), 2, b'0', pad, width),
            b'k' => push_int(out, i64::from(dt.hour), 2, b' ', pad, width),
            b'I' => {
                let h12 = match dt.hour % 12 {
                    0 => 12,
                    h => h,
                };
                push_int(out, i64::from(h12), 2, b'0', pad, width);
            }
            b'l' => {
                let h12 = match dt.hour % 12 {
                    0 => 12,
                    h => h,
                };
                push_int(out, i64::from(h12), 2, b' ', pad, width);
            }
            b'j' => push_int(out, i64::from(dt.yearday), 3, b'0', pad, width),
            b'm' => push_int(out, i64::from(dt.month), 2, b'0', pad, width),
            b'M' => push_int(out, i64::from(dt.minute), 2, b'0', pad, width),
            b'R' => format_strftime_into("%H:%M", dt, locale, out, depth + 1),
            b'S' => push_int(out, i64::from(dt.second), 2, b'0', pad, width),
            b'T' => format_strftime_into("%H:%M:%S", dt, locale, out, depth + 1),
            b'u' => {
                let iso_wday = if dt.weekday == 0 { 7 } else { dt.weekday };
                push_int(out, i64::from(iso_wday), 1, b'0', pad, width);
            }
            b'w' => push_int(out, i64::from(dt.weekday % 7), 1, b'0', pad, width),
            b'U' => {
                let yday0 = i32::from(dt.yearday.saturating_sub(1));
                let w = (yday0 + 7 - i32::from(dt.weekday % 7)) / 7;
                push_int(out, i64::from(w), 2, b'0', pad, width);
            }
            b'W' => {
                let yday0 = i32::from(dt.yearday.saturating_sub(1));
                let mon_wday = (i32::from(dt.weekday % 7) + 6) % 7;
                let w = (yday0 + 7 - mon_wday) / 7;
                push_int(out, i64::from(w), 2, b'0', pad, width);
            }
            b'V' => {
                let (_, week) = iso_week_year(dt);
                push_int(out, i64::from(week), 2, b'0', pad, width);
            }
            b'G' => {
                let (iso_year, _) = iso_week_year(dt);
                push_int(out, i64::from(iso_year), 4, b'0', pad, width);
            }
            b'g' => {
                let (iso_year, _) = iso_week_year(dt);
                push_int(
                    out,
                    i64::from(iso_year.rem_euclid(100)),
                    2,
                    b'0',
                    pad,
                    width,
                );
            }
            b'y' => push_int(out, i64::from(dt.year.rem_euclid(100)), 2, b'0', pad, width),
            b'Y' => push_int(out, i64::from(dt.year), 4, b'0', pad, width),
            b'z' => {
                let sign = if dt.utc_offset_secs < 0 { '-' } else { '+' };
                let abs = dt.utc_offset_secs.unsigned_abs();
                let hours = abs / 3_600;
                let mins = (abs % 3_600) / 60;
                let formatted = format!("{sign}{hours:02}{mins:02}");
                out.push_str(&formatted);
            }
            b'Z' => push_text(out, &dt.tz_name, case_mod, width, pad),
            b's' => {
                let secs = dt.epoch_secs.unwrap_or_else(|| {
                    let days = days_from_civil(dt.year, dt.month, dt.day);
                    days * 86_400
                        + i64::from(dt.hour) * 3_600
                        + i64::from(dt.minute) * 60
                        + i64::from(dt.second)
                        - i64::from(dt.utc_offset_secs)
                });
                push_int(out, secs, 1, b'0', pad, width);
            }
            _ => {
                out.push_str(&fmt[spec_start..i]);
            }
        }
    }
}

fn month_index(month: u8) -> usize {
    usize::from(month.clamp(1, 12) - 1)
}

fn push_text(out: &mut String, text: &str, case_mod: CaseMod, width: Option<usize>, pad: PadStyle) {
    let transformed: String;
    let slice = match case_mod {
        CaseMod::Normal => text,
        CaseMod::Upper => {
            transformed = text.to_uppercase();
            &transformed
        }
        CaseMod::Swap => {
            transformed = text
                .chars()
                .flat_map(|ch| {
                    if ch.is_uppercase() {
                        ch.to_lowercase().collect::<Vec<_>>()
                    } else {
                        ch.to_uppercase().collect::<Vec<_>>()
                    }
                })
                .collect();
            &transformed
        }
    };
    if let Some(w) = width {
        let count = slice.chars().count();
        if count < w {
            let fill = if pad == PadStyle::Zero { '0' } else { ' ' };
            for _ in 0..(w - count) {
                out.push(fill);
            }
        }
    }
    out.push_str(slice);
}

fn push_int(
    out: &mut String,
    value: i64,
    default_width: usize,
    default_fill: u8,
    pad: PadStyle,
    explicit_width: Option<usize>,
) {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let target_width = match (pad, explicit_width) {
        (PadStyle::None, None) => 0,
        (_, Some(w)) => w,
        (_, None) => default_width,
    };
    let fill = match pad {
        PadStyle::Space => ' ',
        PadStyle::Zero => '0',
        PadStyle::None => ' ',
        PadStyle::Default => default_fill as char,
    };
    let total_len = digits.len() + usize::from(negative);
    if target_width > total_len {
        let padding = target_width - total_len;
        if fill == '0' {
            if negative {
                out.push('-');
            }
            for _ in 0..padding {
                out.push('0');
            }
            out.push_str(&digits);
            return;
        }
        for _ in 0..padding {
            out.push(fill);
        }
    }
    if negative {
        out.push('-');
    }
    out.push_str(&digits);
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_year(year: i32) -> i32 {
    if is_leap_year(year) { 366 } else { 365 }
}

/// Converts days since Unix epoch `1970-01-01` to `(year, month 1..=12, day 1..=31, yearday 1..=366)`.
fn civil_from_days(days_since_epoch: i64) -> (i32, u8, u8, u16) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = (yoe as i64 + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    if m <= 2 {
        y += 1;
    }
    let yday = day_of_year(y, m, d);
    (y, m, d, yday)
}

/// Converts Gregorian `(year, month 1..=12, day 1..=31)` to days since `1970-01-01`.
fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn day_of_year(year: i32, month: u8, day: u8) -> u16 {
    const CUMULATIVE: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let idx = month_index(month);
    let leap = u16::from(month > 2 && is_leap_year(year));
    CUMULATIVE[idx] + leap + u16::from(day)
}

/// Computes `(iso_week_year, iso_week_number)` per ISO 8601.
fn iso_week_year(dt: &DateTimeParts) -> (i32, u8) {
    let iso_wday = if dt.weekday == 0 {
        7
    } else {
        i32::from(dt.weekday)
    };
    let yday = i32::from(dt.yearday);
    let raw_week = (yday - iso_wday + 10) / 7;
    if raw_week < 1 {
        let prev_year = dt.year - 1;
        let last_day = days_from_civil(prev_year, 12, 31);
        let prev_wday = ((last_day + 4).rem_euclid(7)) as u8;
        let prev_iso_wday = if prev_wday == 0 {
            7
        } else {
            i32::from(prev_wday)
        };
        let prev_weeks = (days_in_year(prev_year) - prev_iso_wday + 10) / 7;
        (prev_year, prev_weeks as u8)
    } else {
        let last_day = days_from_civil(dt.year, 12, 31);
        let end_wday = ((last_day + 4).rem_euclid(7)) as u8;
        let end_iso_wday = if end_wday == 0 {
            7
        } else {
            i32::from(end_wday)
        };
        let max_weeks = (days_in_year(dt.year) - end_iso_wday + 10) / 7;
        if raw_week > max_weeks {
            (dt.year + 1, 1)
        } else {
            (dt.year, raw_week as u8)
        }
    }
}

static TIME_EN: TimeLocaleData = TimeLocaleData {
    months_full: [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ],
    months_abbr: [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ],
    weekdays_full: [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ],
    weekdays_abbr: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %b %e %H:%M:%S %Y",
    date_fmt: "%m/%d/%y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_DE: TimeLocaleData = TimeLocaleData {
    months_full: [
        "Januar",
        "Februar",
        "März",
        "April",
        "Mai",
        "Juni",
        "Juli",
        "August",
        "September",
        "Oktober",
        "November",
        "Dezember",
    ],
    months_abbr: [
        "Jan", "Feb", "Mär", "Apr", "Mai", "Jun", "Jul", "Aug", "Sep", "Okt", "Nov", "Dez",
    ],
    weekdays_full: [
        "Sonntag",
        "Montag",
        "Dienstag",
        "Mittwoch",
        "Donnerstag",
        "Freitag",
        "Samstag",
    ],
    weekdays_abbr: ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d.%m.%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_ES: TimeLocaleData = TimeLocaleData {
    months_full: [
        "enero",
        "febrero",
        "marzo",
        "abril",
        "mayo",
        "junio",
        "julio",
        "agosto",
        "septiembre",
        "octubre",
        "noviembre",
        "diciembre",
    ],
    months_abbr: [
        "ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic",
    ],
    weekdays_full: [
        "domingo",
        "lunes",
        "martes",
        "miércoles",
        "jueves",
        "viernes",
        "sábado",
    ],
    weekdays_abbr: ["dom", "lun", "mar", "mié", "jue", "vie", "sáb"],
    am: "a. m.",
    pm: "p. m.",
    am_lower: "a. m.",
    pm_lower: "p. m.",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_FR: TimeLocaleData = TimeLocaleData {
    months_full: [
        "janvier",
        "février",
        "mars",
        "avril",
        "mai",
        "juin",
        "juillet",
        "août",
        "septembre",
        "octobre",
        "novembre",
        "décembre",
    ],
    months_abbr: [
        "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.",
        "déc.",
    ],
    weekdays_full: [
        "dimanche", "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi",
    ],
    weekdays_abbr: ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_IT: TimeLocaleData = TimeLocaleData {
    months_full: [
        "gennaio",
        "febbraio",
        "marzo",
        "aprile",
        "maggio",
        "giugno",
        "luglio",
        "agosto",
        "settembre",
        "ottobre",
        "novembre",
        "dicembre",
    ],
    months_abbr: [
        "gen", "feb", "mar", "apr", "mag", "giu", "lug", "ago", "set", "ott", "nov", "dic",
    ],
    weekdays_full: [
        "domenica",
        "lunedì",
        "martedì",
        "mercoledì",
        "giovedì",
        "venerdì",
        "sabato",
    ],
    weekdays_abbr: ["dom", "lun", "mar", "mer", "gio", "ven", "sab"],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_PT_BR: TimeLocaleData = TimeLocaleData {
    months_full: [
        "janeiro",
        "fevereiro",
        "março",
        "abril",
        "maio",
        "junho",
        "julho",
        "agosto",
        "setembro",
        "outubro",
        "novembro",
        "dezembro",
    ],
    months_abbr: [
        "jan", "fev", "mar", "abr", "mai", "jun", "jul", "ago", "set", "out", "nov", "dez",
    ],
    weekdays_full: [
        "domingo",
        "segunda-feira",
        "terça-feira",
        "quarta-feira",
        "quinta-feira",
        "sexta-feira",
        "sábado",
    ],
    weekdays_abbr: ["dom", "seg", "ter", "qua", "qui", "sex", "sáb"],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_NL: TimeLocaleData = TimeLocaleData {
    months_full: [
        "januari",
        "februari",
        "maart",
        "april",
        "mei",
        "juni",
        "juli",
        "augustus",
        "september",
        "oktober",
        "november",
        "december",
    ],
    months_abbr: [
        "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
    ],
    weekdays_full: [
        "zondag",
        "maandag",
        "dinsdag",
        "woensdag",
        "donderdag",
        "vrijdag",
        "zaterdag",
    ],
    weekdays_abbr: ["zo", "ma", "di", "wo", "do", "vr", "za"],
    am: "a.m.",
    pm: "p.m.",
    am_lower: "a.m.",
    pm_lower: "p.m.",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d-%m-%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_RU: TimeLocaleData = TimeLocaleData {
    months_full: [
        "январь",
        "февраль",
        "март",
        "апрель",
        "май",
        "июнь",
        "июль",
        "август",
        "сентябрь",
        "октябрь",
        "ноябрь",
        "декабрь",
    ],
    months_abbr: [
        "янв", "фев", "мар", "апр", "май", "июн", "июл", "авг", "сен", "окт", "ноя", "дек",
    ],
    weekdays_full: [
        "воскресенье",
        "понедельник",
        "вторник",
        "среда",
        "четверг",
        "пятница",
        "суббота",
    ],
    weekdays_abbr: ["вс", "пн", "вт", "ср", "чт", "пт", "сб"],
    am: "ДП",
    pm: "ПП",
    am_lower: "дп",
    pm_lower: "пп",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d.%m.%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_UK: TimeLocaleData = TimeLocaleData {
    months_full: [
        "січень",
        "лютий",
        "березень",
        "квітень",
        "травень",
        "червень",
        "липень",
        "серпень",
        "вересень",
        "жовтень",
        "листопад",
        "грудень",
    ],
    months_abbr: [
        "січ", "лют", "бер", "кві", "тра", "чер", "лип", "сер", "вер", "жов", "лис", "гру",
    ],
    weekdays_full: [
        "неділя",
        "понеділок",
        "вівторок",
        "середа",
        "четвер",
        "п'ятниця",
        "субота",
    ],
    weekdays_abbr: ["нд", "пн", "вт", "ср", "чт", "пт", "сб"],
    am: "дп",
    pm: "пп",
    am_lower: "дп",
    pm_lower: "пп",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d.%m.%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_JA: TimeLocaleData = TimeLocaleData {
    months_full: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    months_abbr: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    weekdays_full: [
        "日曜日",
        "月曜日",
        "火曜日",
        "水曜日",
        "木曜日",
        "金曜日",
        "土曜日",
    ],
    weekdays_abbr: ["日", "月", "火", "水", "木", "金", "土"],
    am: "午前",
    pm: "午後",
    am_lower: "午前",
    pm_lower: "午後",
    date_time_fmt: "%Y年%m月%d日 %H時%M分%S秒",
    date_fmt: "%Y/%m/%d",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%p %I時%M分%S秒",
};

static TIME_KO: TimeLocaleData = TimeLocaleData {
    months_full: [
        "1월", "2월", "3월", "4월", "5월", "6월", "7월", "8월", "9월", "10월", "11월", "12월",
    ],
    months_abbr: [
        "1월", "2월", "3월", "4월", "5월", "6월", "7월", "8월", "9월", "10월", "11월", "12월",
    ],
    weekdays_full: [
        "일요일",
        "월요일",
        "화요일",
        "수요일",
        "목요일",
        "금요일",
        "토요일",
    ],
    weekdays_abbr: ["일", "월", "화", "수", "목", "금", "토"],
    am: "오전",
    pm: "오후",
    am_lower: "오전",
    pm_lower: "오후",
    date_time_fmt: "%Y년 %m월 %d일 (%a) %p %I:%M:%S %Z",
    date_fmt: "%Y. %m. %d.",
    time_fmt: "%p %I:%M:%S",
    time_12h_fmt: "%p %I:%M:%S",
};

static TIME_ZH_CN: TimeLocaleData = TimeLocaleData {
    months_full: [
        "一月",
        "二月",
        "三月",
        "四月",
        "五月",
        "六月",
        "七月",
        "八月",
        "九月",
        "十月",
        "十一月",
        "十二月",
    ],
    months_abbr: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    weekdays_full: [
        "星期日",
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
    ],
    weekdays_abbr: ["周日", "周一", "周二", "周三", "周四", "周五", "周六"],
    am: "上午",
    pm: "下午",
    am_lower: "上午",
    pm_lower: "下午",
    date_time_fmt: "%Y年%m月%d日 %A %H:%M:%S %Z",
    date_fmt: "%Y/%m/%d",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%p %I:%M:%S",
};

static TIME_ZH_TW: TimeLocaleData = TimeLocaleData {
    months_full: [
        "一月",
        "二月",
        "三月",
        "四月",
        "五月",
        "六月",
        "七月",
        "八月",
        "九月",
        "十月",
        "十一月",
        "十二月",
    ],
    months_abbr: [
        "1月", "2月", "3月", "4月", "5月", "6月", "7月", "8月", "9月", "10月", "11月", "12月",
    ],
    weekdays_full: [
        "星期日",
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
    ],
    weekdays_abbr: ["週日", "週一", "週二", "週三", "週四", "週五", "週六"],
    am: "上午",
    pm: "下午",
    am_lower: "上午",
    pm_lower: "下午",
    date_time_fmt: "%Y年%m月%d日 %A %H:%M:%S %Z",
    date_fmt: "%Y/%m/%d",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%p %I:%M:%S",
};

static TIME_AR: TimeLocaleData = TimeLocaleData {
    months_full: [
        "يناير",
        "فبراير",
        "مارس",
        "أبريل",
        "مايو",
        "يونيو",
        "يوليو",
        "أغسطس",
        "سبتمبر",
        "أكتوبر",
        "نوفمبر",
        "ديسمبر",
    ],
    months_abbr: [
        "يناير",
        "فبراير",
        "مارس",
        "أبريل",
        "مايو",
        "يونيو",
        "يوليو",
        "أغسطس",
        "سبتمبر",
        "أكتوبر",
        "نوفمبر",
        "ديسمبر",
    ],
    weekdays_full: [
        "الأحد",
        "الاثنين",
        "الثلاثاء",
        "الأربعاء",
        "الخميس",
        "الجمعة",
        "السبت",
    ],
    weekdays_abbr: ["أحد", "اثنين", "ثلاثاء", "أربعاء", "خميس", "جمعة", "سبت"],
    am: "ص",
    pm: "م",
    am_lower: "ص",
    pm_lower: "م",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_HI: TimeLocaleData = TimeLocaleData {
    months_full: [
        "जनवरी",
        "फ़रवरी",
        "मार्च",
        "अप्रैल",
        "मई",
        "जून",
        "जुलाई",
        "अगस्त",
        "सितंबर",
        "अक्तूबर",
        "नवंबर",
        "दिसंबर",
    ],
    months_abbr: [
        "जन॰",
        "फ़र॰",
        "मार्च",
        "अप्रैल",
        "मई",
        "जून",
        "जुल॰",
        "अग॰",
        "सित॰",
        "अक्तू॰",
        "नव॰",
        "दिस॰",
    ],
    weekdays_full: [
        "रविवार",
        "सोमवार",
        "मंगलवार",
        "बुधवार",
        "गुरुवार",
        "शुक्रवार",
        "शनिवार",
    ],
    weekdays_abbr: ["रवि", "सोम", "मंगल", "बुध", "गुरु", "शुक्र", "शनि"],
    am: "पूर्वाह्न",
    pm: "अपराह्न",
    am_lower: "पूर्वाह्न",
    pm_lower: "अपराह्न",
    date_time_fmt: "%A %d %B %Y %I:%M:%S %p %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%I:%M:%S %p",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_PL: TimeLocaleData = TimeLocaleData {
    months_full: [
        "styczeń",
        "luty",
        "marzec",
        "kwiecień",
        "maj",
        "czerwiec",
        "lipiec",
        "sierpień",
        "wrzesień",
        "październik",
        "listopad",
        "grudzień",
    ],
    months_abbr: [
        "sty", "lut", "mar", "kwi", "maj", "cze", "lip", "sie", "wrz", "paź", "lis", "gru",
    ],
    weekdays_full: [
        "niedziela",
        "poniedziałek",
        "wtorek",
        "środa",
        "czwartek",
        "piątek",
        "sobota",
    ],
    weekdays_abbr: ["niedz.", "pon.", "wt.", "śr.", "czw.", "pt.", "sob."],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d.%m.%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_TR: TimeLocaleData = TimeLocaleData {
    months_full: [
        "Ocak", "Şubat", "Mart", "Nisan", "Mayıs", "Haziran", "Temmuz", "Ağustos", "Eylül", "Ekim",
        "Kasım", "Aralık",
    ],
    months_abbr: [
        "Oca", "Şub", "Mar", "Nis", "May", "Haz", "Tem", "Ağu", "Eyl", "Eki", "Kas", "Ara",
    ],
    weekdays_full: [
        "Pazar",
        "Pazartesi",
        "Salı",
        "Çarşamba",
        "Perşembe",
        "Cuma",
        "Cumartesi",
    ],
    weekdays_abbr: ["Paz", "Pzt", "Sal", "Çar", "Per", "Cum", "Cmt"],
    am: "ÖÖ",
    pm: "ÖS",
    am_lower: "öö",
    pm_lower: "ös",
    date_time_fmt: "%d %b %Y %a %H:%M:%S %Z",
    date_fmt: "%d.%m.%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_SV: TimeLocaleData = TimeLocaleData {
    months_full: [
        "januari",
        "februari",
        "mars",
        "april",
        "maj",
        "juni",
        "juli",
        "augusti",
        "september",
        "oktober",
        "november",
        "december",
    ],
    months_abbr: [
        "jan", "feb", "mar", "apr", "maj", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
    ],
    weekdays_full: [
        "söndag", "måndag", "tisdag", "onsdag", "torsdag", "fredag", "lördag",
    ],
    weekdays_abbr: ["sön", "mån", "tis", "ons", "tor", "fre", "lör"],
    am: "fm",
    pm: "em",
    am_lower: "fm",
    pm_lower: "em",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%Y-%m-%d",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_VI: TimeLocaleData = TimeLocaleData {
    months_full: [
        "Tháng 1",
        "Tháng 2",
        "Tháng 3",
        "Tháng 4",
        "Tháng 5",
        "Tháng 6",
        "Tháng 7",
        "Tháng 8",
        "Tháng 9",
        "Tháng 10",
        "Tháng 11",
        "Tháng 12",
    ],
    months_abbr: [
        "Thg 1", "Thg 2", "Thg 3", "Thg 4", "Thg 5", "Thg 6", "Thg 7", "Thg 8", "Thg 9", "Thg 10",
        "Thg 11", "Thg 12",
    ],
    weekdays_full: [
        "Chủ Nhật",
        "Thứ Hai",
        "Thứ Ba",
        "Thứ Tư",
        "Thứ Năm",
        "Thứ Sáu",
        "Thứ Bảy",
    ],
    weekdays_abbr: ["CN", "T2", "T3", "T4", "T5", "T6", "T7"],
    am: "SA",
    pm: "CH",
    am_lower: "sa",
    pm_lower: "ch",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

static TIME_ID: TimeLocaleData = TimeLocaleData {
    months_full: [
        "Januari",
        "Februari",
        "Maret",
        "April",
        "Mei",
        "Juni",
        "Juli",
        "Agustus",
        "September",
        "Oktober",
        "November",
        "Desember",
    ],
    months_abbr: [
        "Jan", "Feb", "Mar", "Apr", "Mei", "Jun", "Jul", "Agu", "Sep", "Okt", "Nov", "Des",
    ],
    weekdays_full: [
        "Minggu", "Senin", "Selasa", "Rabu", "Kamis", "Jumat", "Sabtu",
    ],
    weekdays_abbr: ["Min", "Sen", "Sel", "Rab", "Kam", "Jum", "Sab"],
    am: "AM",
    pm: "PM",
    am_lower: "am",
    pm_lower: "pm",
    date_time_fmt: "%a %d %b %Y %H:%M:%S %Z",
    date_fmt: "%d/%m/%Y",
    time_fmt: "%H:%M:%S",
    time_12h_fmt: "%I:%M:%S %p",
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_20_locales_have_unique_codes_and_round_trip() {
        assert_eq!(Locale::ALL.len(), 20);
        let mut codes = HashSet::new();
        for &locale in Locale::ALL {
            assert!(
                codes.insert(locale.code()),
                "duplicate code {}",
                locale.code()
            );
            assert!(!locale.native_name().is_empty());
            assert_eq!(Locale::parse(locale.code()), Some(locale));
            assert_eq!(locale.code().parse::<Locale>().unwrap(), locale);
            assert_eq!(locale.to_string(), locale.code());
        }
    }

    #[test]
    fn parse_handles_posix_bcp47_encodings_modifiers_and_fallbacks() {
        let cases = [
            ("C", Some(Locale::En)),
            ("POSIX", Some(Locale::En)),
            ("C.UTF-8", Some(Locale::En)),
            ("en_US.UTF-8", Some(Locale::En)),
            ("de_DE.UTF-8", Some(Locale::De)),
            ("de_DE@euro", Some(Locale::De)),
            ("de_AT.ISO-8859-1", Some(Locale::De)),
            ("es_ES.UTF-8", Some(Locale::Es)),
            ("es_MX", Some(Locale::Es)),
            ("fr_FR.UTF-8", Some(Locale::Fr)),
            ("fr_CA", Some(Locale::Fr)),
            ("it_IT.UTF-8", Some(Locale::It)),
            ("pt_BR.UTF-8", Some(Locale::PtBr)),
            ("pt-BR", Some(Locale::PtBr)),
            ("pt", Some(Locale::PtBr)),
            ("nl_NL.UTF-8", Some(Locale::Nl)),
            ("ru_RU.UTF-8", Some(Locale::Ru)),
            ("uk_UA.UTF-8", Some(Locale::Uk)),
            ("ja_JP.UTF-8", Some(Locale::Ja)),
            ("ko_KR.UTF-8", Some(Locale::Ko)),
            ("zh_CN.UTF-8", Some(Locale::ZhCn)),
            ("zh-Hans-CN", Some(Locale::ZhCn)),
            ("zh_SG", Some(Locale::ZhCn)),
            ("zh", Some(Locale::ZhCn)),
            ("zh_TW.UTF-8", Some(Locale::ZhTw)),
            ("zh_HK.UTF-8", Some(Locale::ZhTw)),
            ("zh-Hant", Some(Locale::ZhTw)),
            ("ar_SA.UTF-8", Some(Locale::Ar)),
            ("ar_EG", Some(Locale::Ar)),
            ("hi_IN.UTF-8", Some(Locale::Hi)),
            ("pl_PL.UTF-8", Some(Locale::Pl)),
            ("tr_TR.UTF-8", Some(Locale::Tr)),
            ("sv_SE.UTF-8", Some(Locale::Sv)),
            ("vi_VN.UTF-8", Some(Locale::Vi)),
            ("id_ID.UTF-8", Some(Locale::Id)),
            ("in_ID.UTF-8", Some(Locale::Id)),
            ("", None),
            ("   ", None),
            ("xx_XX.UTF-8", None),
        ];
        for (input, expected) in cases {
            assert_eq!(Locale::parse(input), expected, "input={input:?}");
        }
        assert_eq!(Locale::from_posix_env(""), Locale::En);
        assert_eq!(Locale::from_posix_env("xx_XX"), Locale::En);
        assert_eq!(Locale::from_posix_env("ja_JP.UTF-8"), Locale::Ja);
    }

    #[test]
    fn utc_timestamp_conversion_and_strftime_en_match_c_locale() {
        // 1_000_000_000 = Sun Sep 9 01:46:40 UTC 2001 (day 252 of year).
        let dt = DateTimeParts::from_utc_timestamp(1_000_000_000);
        assert_eq!((dt.year, dt.month, dt.day), (2001, 9, 9));
        assert_eq!((dt.hour, dt.minute, dt.second), (1, 46, 40));
        assert_eq!(dt.weekday, 0);
        assert_eq!(dt.yearday, 252);
        assert_eq!(
            format_strftime("%a %A %b %B %c", &dt, Locale::En),
            "Sun Sunday Sep September Sun Sep  9 01:46:40 2001"
        );
        assert_eq!(
            format_strftime(
                "%F %T %p %P %z %Z %s %V %G %g %u %w %U %W %%",
                &dt,
                Locale::En
            ),
            "2001-09-09 01:46:40 AM am +0000 UTC 1000000000 36 2001 01 7 0 36 36 %"
        );
        // Epoch 0 = Thu Jan 1 00:00:00 1970.
        let epoch = DateTimeParts::from_utc_timestamp(0);
        assert_eq!(
            format_strftime("%a %Y-%m-%d %H:%M:%S", &epoch, Locale::En),
            "Thu 1970-01-01 00:00:00"
        );
        // Pre-epoch -1 = Wed Dec 31 23:59:59 1969.
        let pre = DateTimeParts::from_utc_timestamp(-1);
        assert_eq!(
            format_strftime("%a %Y-%m-%d %H:%M:%S", &pre, Locale::En),
            "Wed 1969-12-31 23:59:59"
        );
    }

    #[test]
    fn all_20_time_tables_are_complete_and_format_cleanly() {
        let dt = DateTimeParts::from_utc_timestamp(1_000_000_000);
        for &locale in Locale::ALL {
            let td = locale.time_data();
            assert!(td.months_full.iter().all(|s| !s.is_empty()));
            assert!(td.months_abbr.iter().all(|s| !s.is_empty()));
            assert!(td.weekdays_full.iter().all(|s| !s.is_empty()));
            assert!(td.weekdays_abbr.iter().all(|s| !s.is_empty()));
            assert!(!td.am.is_empty() && !td.pm.is_empty());
            assert!(!td.am_lower.is_empty() && !td.pm_lower.is_empty());
            let rendered = format_strftime("%A %a %B %b %p %P | %c | %x | %X | %r", &dt, locale);
            assert!(!rendered.is_empty(), "{locale}");
            assert!(
                !rendered.contains('%'),
                "unexpanded specifier in {locale}: {rendered}"
            );
            // Verify UTF-8-safe capacity truncation.
            let truncated = format_strftime_bytes("%A %B %c", &dt, locale, 7);
            assert!(truncated.len() <= 7);
            assert!(std::str::from_utf8(&truncated).is_ok());
        }
        // Spot-check distinct script fidelity.
        assert_eq!(
            format_strftime("%A %B", &dt, Locale::De),
            "Sonntag September"
        );
        assert_eq!(format_strftime("%A %B", &dt, Locale::Ja), "日曜日 9月");
        assert_eq!(format_strftime("%A %B", &dt, Locale::Ko), "일요일 9월");
        assert_eq!(format_strftime("%a %B", &dt, Locale::ZhCn), "周日 九月");
        assert_eq!(format_strftime("%a %B", &dt, Locale::ZhTw), "週日 九月");
        assert_eq!(format_strftime("%A %B", &dt, Locale::Ar), "الأحد سبتمبر");
        assert_eq!(format_strftime("%A %B", &dt, Locale::Hi), "रविवार सितंबर");
        assert_eq!(
            format_strftime("%A %B", &dt, Locale::Ru),
            "воскресенье сентябрь"
        );
        assert_eq!(format_strftime("%A %B", &dt, Locale::Uk), "неділя вересень");
    }
}
