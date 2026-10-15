//! `TUNE <file> <argc> <argv0> <args...>` (RC-90, RC-91).

use super::files::Access;
use super::service::{Answer, Shared};
use rondi::{RrdDataSourceTune, RrdInfo, RrdTuneBound};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The `rrdtool tune` options: short letter, long name.
const OPTIONS: &[(u8, &str)] = &[
    (b'h', "heartbeat"),
    (b'i', "minimum"),
    (b'a', "maximum"),
    (b'd', "data-source-type"),
    (b'r', "data-source-rename"),
    (b'p', "deltapos"),
    (b'n', "deltaneg"),
    (b'f', "failure-threshold"),
    (b'w', "window-length"),
    (b'x', "alpha"),
    (b'y', "beta"),
    (b'z', "gamma"),
    (b'v', "gamma-deviation"),
    (b's', "smoothing-window"),
    (b'S', "smoothing-window-deviation"),
    (b'b', "aberrant-reset"),
    (b't', "step"),
    (b'D', "daemon"),
];

/// Data source names are at most 19 bytes.
const NAME_MAX: usize = 19;

/// The value of one data source field being changed.
struct Source {
    original: String,
    current: Vec<u8>,
    change: RrdDataSourceTune,
}

// Compatibility: the argument count must be a positive number and match
// the words sent; the file is opened read-write before the options are
// read; options are read like getopt_long, letting other words through;
// each option is applied in order and the first failing one stops the
// rest, keeping the settings before it (RC-90). Provenance: black-box
// compatibility test.
// Rondi policy (RC-91): a count larger than the words sent is answered
// with the count error instead of reading past the request.
pub(crate) fn tune(shared: &Shared, fields: &[Vec<u8>]) -> Answer {
    let failed = |text: String| Answer::Failed(text.into_bytes());
    let count_text = String::from_utf8_lossy(&fields[2]).into_owned();
    let words = &fields[3..];
    let count = rondi::c_strtol(&count_text, 10);
    let count = match count {
        (value, end) if end > 0 && value > 0 => value as usize,
        _ => return failed(format!("Invalid argument count specified: {count_text}")),
    };
    if words.len() > count {
        return failed(format!("Too many arguments (expected {count})"));
    }
    if words.len() < count {
        return failed(format!("Invalid argument count specified: {count_text}"));
    }
    let joined = shared.names.join(&fields[1]);
    let target = match shared.locate(&joined) {
        Ok(target) => target,
        Err(answer) => return answer,
    };
    let got = |text: String| failed(format!("Got error {text}"));
    let file = match target.open(Access::Write) {
        Ok(file) => file,
        Err(error) => return got(super::service::opening(&joined, &error)),
    };
    let path = Path::new(OsStr::from_bytes(&joined));
    let info = match file
        .try_clone()
        .map_err(rondi::StoreError::from)
        .and_then(|reader| rondi::inspect_rrd_open_file(reader, path))
    {
        Ok(info) => info,
        Err(error) => return got(error.to_string()),
    };
    let mut sources = info
        .data_sources
        .iter()
        .map(|source| Source {
            original: source.name.clone(),
            current: source.raw_name.clone(),
            change: RrdDataSourceTune {
                name: source.name.clone(),
                kind: None,
                new_name: None,
                heartbeat: None,
                minimum: None,
                maximum: None,
            },
        })
        .collect::<Vec<_>>();
    let outcome = read_options(&words[1.min(words.len())..], &info, &mut sources);
    let changes = sources
        .into_iter()
        .map(|source| source.change)
        .filter(|change| {
            change.kind.is_some()
                || change.new_name.is_some()
                || change.heartbeat.is_some()
                || change.minimum.is_some()
                || change.maximum.is_some()
        })
        .collect::<Vec<_>>();
    if !changes.is_empty()
        && let Err(error) = rondi::tune_rrd_data_sources_file(file, path, &changes)
    {
        return got(error.to_string());
    }
    match outcome {
        Ok(()) => Answer::Done(b"0 Success\n".to_vec()),
        Err(text) => got(text),
    }
}

/// Reads the options and records each change; stops at the first error.
fn read_options(words: &[Vec<u8>], info: &RrdInfo, sources: &mut [Source]) -> Result<(), String> {
    let mut index = 0;
    let mut positional = Vec::new();
    while index < words.len() {
        let word = &words[index];
        index += 1;
        if word == b"--" {
            positional.extend(words[index..].iter());
            break;
        }
        let option = match word.strip_prefix(b"--") {
            Some(long) => Some(long_option(long, words, &mut index)?),
            None => match word.as_slice() {
                [b'-', letter, rest @ ..] => Some(short_option(*letter, rest, words, &mut index)?),
                _ => None,
            },
        };
        match option {
            Some((letter, argument)) => apply(letter, &argument, info, sources)?,
            None => positional.push(word),
        }
    }
    refuse_structure_words(&positional)
}

/// The next word, as an option's argument.
fn take_argument(words: &[Vec<u8>], index: &mut usize) -> Option<Vec<u8>> {
    let argument = words.get(*index)?.clone();
    *index += 1;
    Some(argument)
}

/// `--name` or `--name=value`: the option's letter and its argument.
fn long_option(long: &[u8], words: &[Vec<u8>], index: &mut usize) -> Result<(u8, Vec<u8>), String> {
    let (name, attached) = match long.iter().position(|&byte| byte == b'=') {
        Some(at) => (&long[..at], Some(long[at + 1..].to_vec())),
        None => (long, None),
    };
    let Some(&(letter, full)) = OPTIONS.iter().find(|(_, full)| full.as_bytes() == name) else {
        return Err(format!(
            "invalid option -- '{}'",
            String::from_utf8_lossy(long)
        ));
    };
    let argument = attached
        .or_else(|| take_argument(words, index))
        .ok_or_else(|| format!("option requires argument -- '{full}'"))?;
    Ok((letter, argument))
}

/// `-x` with its argument attached or in the next word.
fn short_option(
    letter: u8,
    rest: &[u8],
    words: &[Vec<u8>],
    index: &mut usize,
) -> Result<(u8, Vec<u8>), String> {
    if !OPTIONS.iter().any(|(short, _)| *short == letter) {
        return Err(format!(
            "invalid option -- '{}'",
            String::from_utf8_lossy(&[letter])
        ));
    }
    let argument = if rest.is_empty() {
        take_argument(words, index)
            .ok_or_else(|| format!("option requires an argument -- '{}'", char::from(letter)))?
    } else {
        rest.to_vec()
    };
    Ok((letter, argument))
}

// Rondi difference (known defect, RD-019): data source and archive
// changes are not supported; other words are ignored like upstream.
fn refuse_structure_words(positional: &[&Vec<u8>]) -> Result<(), String> {
    for word in positional {
        for prefix in [&b"DS:"[..], b"DEL:", b"DELRRA:", b"RRA:", b"RRA#"] {
            if word.starts_with(prefix) {
                return Err(format!(
                    "{} is not supported by Rondi",
                    String::from_utf8_lossy(word)
                ));
            }
        }
    }
    Ok(())
}

/// `name:value`, with a name of 1 to 19 bytes.
fn name_and_value(argument: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = argument.iter().position(|&byte| byte == b':')?;
    let name = &argument[..at];
    (!name.is_empty() && name.len() <= NAME_MAX).then(|| (name, &argument[at + 1..]))
}

fn find<'a>(sources: &'a mut [Source], name: &[u8]) -> Result<&'a mut Source, String> {
    sources
        .iter_mut()
        .find(|source| source.current == name)
        .ok_or_else(|| {
            format!(
                "unknown data source name '{}'",
                String::from_utf8_lossy(name)
            )
        })
}

/// C `scanf("%lu")`: optional white space and sign, then digits.
fn scan_unsigned(text: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(text);
    let trimmed = text.trim_start();
    let digits = trimmed.trim_start_matches(['+', '-']);
    if !digits.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let (value, _) = rondi::c_strtol(trimmed, 10);
    Some(value as u64)
}

/// A bound: `U` for none, else a finite number.
fn scan_bound(text: &[u8]) -> Option<RrdTuneBound> {
    if text == b"U" {
        return Some(RrdTuneBound::Unbounded);
    }
    scan_float(text).map(RrdTuneBound::Value)
}

/// C `scanf("%lf")` for a finite decimal number; trailing text is ignored.
fn scan_float(text: &[u8]) -> Option<f64> {
    let text = String::from_utf8_lossy(text);
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut end = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let whole = digits(end);
    end += whole;
    let mut fraction = 0;
    if bytes.get(end) == Some(&b'.') {
        fraction = digits(end + 1);
        end += 1 + fraction;
    }
    if whole + fraction == 0 {
        return None;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let count = digits(exponent);
        if count > 0 {
            end = exponent + count;
        }
    }
    trimmed[..end]
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// The consolidation functions of the Holt-Winters archives.
const HOLT_WINTERS: &[&str] = &[
    "HWPREDICT",
    "MHWPREDICT",
    "SEASONAL",
    "DEVSEASONAL",
    "DEVPREDICT",
];

fn has_cf(info: &RrdInfo, names: &[&str]) -> bool {
    info.archives
        .iter()
        .any(|archive| names.contains(&archive.consolidation.as_str()))
}

// Compatibility: each option parses `name:value` before looking the name
// up, and reports `invalid arguments for <setting>` or `unknown data
// source name '<name>'` (RC-90). Provenance: black-box compatibility
// test.
fn apply(
    letter: u8,
    argument: &[u8],
    info: &RrdInfo,
    sources: &mut [Source],
) -> Result<(), String> {
    match letter {
        b'h' => {
            let parsed = name_and_value(argument)
                .and_then(|(name, value)| Some((name, scan_unsigned(value)?)));
            let (name, heartbeat) = parsed.ok_or("invalid arguments for heartbeat")?;
            find(sources, name)?.change.heartbeat = Some(heartbeat);
        }
        b'i' | b'a' => {
            let setting = if letter == b'i' { "minimum" } else { "maximum" };
            let parsed =
                name_and_value(argument).and_then(|(name, value)| Some((name, scan_bound(value)?)));
            let (name, bound) =
                parsed.ok_or_else(|| format!("invalid arguments for {setting} ds value"))?;
            let source = find(sources, name)?;
            if letter == b'i' {
                source.change.minimum = Some(bound);
            } else {
                source.change.maximum = Some(bound);
            }
        }
        b'd' => {
            let parsed = name_and_value(argument).and_then(|(name, value)| {
                let length = value.iter().take_while(|b| b.is_ascii_uppercase()).count();
                (length > 0).then(|| (name, &value[..length]))
            });
            let (name, kind) = parsed.ok_or("invalid arguments for data source type")?;
            let source = find(sources, name)?;
            if let Some(error) = rondi::dst_conv_error(kind) {
                return Err(String::from_utf8_lossy(&error).into_owned());
            }
            source.change.kind = Some(String::from_utf8_lossy(kind).into_owned());
        }
        b'r' => {
            // Compatibility: the new name is the leading run of name
            // characters, cut at 19 bytes; the reference words the error
            // like -d's. Provenance: black-box compatibility test.
            let parsed = name_and_value(argument).and_then(|(name, value)| {
                let length = value
                    .iter()
                    .take_while(|&&byte| {
                        byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
                    })
                    .count()
                    .min(NAME_MAX);
                (length > 0).then(|| (name, &value[..length]))
            });
            let (name, new) = parsed.ok_or("invalid arguments for data source type")?;
            let source = find(sources, name)?;
            source.current = new.to_vec();
            source.change.new_name = Some(String::from_utf8_lossy(new).into_owned());
            if source.change.new_name.as_deref() == Some(source.original.as_str()) {
                source.change.new_name = None;
            }
        }
        // Compatibility: --daemon is ignored and a step of zero or less
        // leaves the step alone. Provenance: black-box compatibility test.
        b'D' => {}
        b't' if rondi::c_strtol(&String::from_utf8_lossy(argument), 10).0 <= 0 => {}
        b'b' => {
            find(sources, argument)?;
            holt_winters(info, letter)?;
        }
        b'x' | b'y' | b'z' | b'v' | b's' | b'S' => {
            // Compatibility: the smoothing windows (-s, -S) accept 0 and 1.
            // Provenance: black-box compatibility test.
            let value = scan_float(argument).ok_or("Unable to parse Holt-Winters parameter")?;
            if matches!(letter, b'x' | b'y' | b'z' | b'v') && !(value > 0.0 && value < 1.0) {
                return Err("Holt-Winters parameter must be between 0 and 1".to_owned());
            }
            holt_winters(info, letter)?;
        }
        b'f' | b'w' => {
            let value = rondi::c_strtol(&String::from_utf8_lossy(argument), 10).0;
            if !(1..=28).contains(&value) {
                return Err("Parameter must be between 1 and 28".to_owned());
            }
            failures(info, letter)?;
        }
        b'p' | b'n' => failures(info, letter)?,
        other => return Err(unsupported(other)),
    }
    Ok(())
}

/// The refusal of an option Rondi does not support.
// Rondi difference (known defect, RD-019): Holt-Winters tuning and step
// changes are not supported.
fn unsupported(letter: u8) -> String {
    let full = OPTIONS
        .iter()
        .find(|(short, _)| *short == letter)
        .map_or("", |(_, full)| full);
    format!("--{full} is not supported by Rondi")
}

// Compatibility: on a file without the archives these options tune, the
// reference reports that they do not exist; an aberrant reset of such a
// file does nothing (RC-90). Provenance: black-box compatibility test.
fn holt_winters(info: &RrdInfo, letter: u8) -> Result<(), String> {
    match (has_cf(info, HOLT_WINTERS), letter) {
        (false, b'b') => Ok(()),
        (false, _) => Err("Holt-Winters RRA does not exist in this RRD".to_owned()),
        (true, _) => Err(unsupported(letter)),
    }
}

fn failures(info: &RrdInfo, letter: u8) -> Result<(), String> {
    if has_cf(info, &["FAILURES"]) {
        Err(unsupported(letter))
    } else {
        Err("Failures RRA does not exist in this RRD".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_scan_like_scanf() {
        assert_eq!(scan_unsigned(b"900abc"), Some(900));
        assert_eq!(scan_unsigned(b" 7"), Some(7));
        assert_eq!(scan_unsigned(b"x"), None);
        assert_eq!(scan_unsigned(b""), None);
        assert_eq!(scan_bound(b"1abc"), Some(RrdTuneBound::Value(1.0)));
        assert_eq!(scan_bound(b"5e3"), Some(RrdTuneBound::Value(5000.0)));
        assert_eq!(scan_bound(b"-5"), Some(RrdTuneBound::Value(-5.0)));
        assert_eq!(scan_bound(b"U"), Some(RrdTuneBound::Unbounded));
        assert_eq!(scan_float(b"0.5abc"), Some(0.5));
        for refused in [&b"abc"[..], b"inf", b"nan", b"", b"1e999", b"U5", b"u"] {
            assert_eq!(scan_bound(refused), None);
        }
        assert_eq!(name_and_value(b"x:900"), Some((&b"x"[..], &b"900"[..])));
        assert_eq!(name_and_value(b":5"), None);
        assert_eq!(name_and_value(b"x"), None);
        assert_eq!(name_and_value(b"abcdefghijklmnopqrst:1"), None);
    }
}
