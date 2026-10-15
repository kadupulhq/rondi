use std::fmt;

/// Whether an option takes a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arity {
    Flag,
    Value,
}

/// One row of a command's option table.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OptionSpec<K> {
    pub(crate) long: &'static str,
    pub(crate) short: Option<char>,
    pub(crate) arity: Arity,
    pub(crate) key: K,
}

impl<K> OptionSpec<K> {
    /// An option that takes a value, as `--long v`, `--long=v`, `-s v` or
    /// `-sv`.
    pub(crate) const fn value(long: &'static str, short: Option<char>, key: K) -> Self {
        Self {
            long,
            short,
            arity: Arity::Value,
            key,
        }
    }

    /// An option without a value.
    pub(crate) const fn flag(long: &'static str, short: Option<char>, key: K) -> Self {
        Self {
            long,
            short,
            arity: Arity::Flag,
            key,
        }
    }
}

/// An option the scanner recognised, with its value when it takes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Found<K> {
    pub(crate) key: K,
    pub(crate) value: Option<String>,
}

impl<K> Found<K> {
    /// The value, or an empty string for a flag.
    pub(crate) fn text(&self) -> &str {
        self.value.as_deref().unwrap_or("")
    }
}

// Compatibility: option error texts are cut to 47 bytes, so a long name
// loses its closing quote and possibly more.
// Provenance: black-box compatibility test (cli-truncate-001 to -005, -012).
const OPTION_MESSAGE_LIMIT: usize = 47;

/// A word the option table rejects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OptionError {
    /// `--name` (or `--name=value`) is not in the table. Holds everything
    /// after the leading `--`.
    UnknownLong(String),
    /// A short option letter is not in the table. Holds the single byte, so
    /// the first byte of a multibyte character is reported on its own, as
    /// RRDtool does.
    UnknownShort(u8),
    /// A long option that takes a value was the last word.
    MissingLongValue(&'static str),
    /// A short option that takes a value was the last thing on the line.
    MissingShortValue(char),
    /// A long flag was given as `--name=value`.
    UnexpectedValue(&'static str),
}

impl OptionError {
    /// The message as RRDtool prints it, already cut to its byte limit.
    pub(crate) fn message_bytes(&self) -> Vec<u8> {
        let mut text = Vec::new();
        match self {
            Self::UnknownLong(name) => {
                text.extend_from_slice(b"invalid option -- '");
                text.extend_from_slice(name.as_bytes());
            }
            Self::UnknownShort(byte) => {
                text.extend_from_slice(b"invalid option -- '");
                text.push(*byte);
            }
            Self::MissingLongValue(name) => {
                text.extend_from_slice(b"option requires argument -- '");
                text.extend_from_slice(name.as_bytes());
            }
            Self::MissingShortValue(letter) => {
                text.extend_from_slice(b"option requires an argument -- '");
                text.extend_from_slice(letter.to_string().as_bytes());
            }
            Self::UnexpectedValue(name) => {
                text.extend_from_slice(b"option takes no arguments -- '");
                text.extend_from_slice(name.as_bytes());
            }
        }
        text.push(b'\'');
        text.truncate(OPTION_MESSAGE_LIMIT);
        text
    }
}

impl fmt::Display for OptionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from_utf8_lossy(&self.message_bytes()))
    }
}

impl std::error::Error for OptionError {}

/// Walks a command's words against its option table.
///
/// Options may appear anywhere; the other words are collected, in order,
/// as operands. The first word that is exactly `--` ends option scanning
/// and is dropped. A lone `-` is an operand.
pub(crate) struct OptionScanner<'t, K> {
    table: &'t [OptionSpec<K>],
    words: Vec<String>,
    next: usize,
    /// Byte offset of the next letter when inside a group of short options
    /// in `words[next]`.
    letter: Option<usize>,
    operands: Vec<String>,
    /// Option words, each followed by a separate value word it consumed.
    option_words: Vec<String>,
    /// A long option that was the last word and lacked its value.
    dangling: Option<String>,
    saw_separator: bool,
}

impl<'t, K: Copy> OptionScanner<'t, K> {
    pub(crate) fn new(words: &[String], table: &'t [OptionSpec<K>]) -> Self {
        Self {
            table,
            words: words.to_vec(),
            next: 0,
            letter: None,
            operands: Vec::new(),
            option_words: Vec::new(),
            dangling: None,
            saw_separator: false,
        }
    }

    /// The operands seen so far; all of them once the scanner is exhausted.
    pub(crate) fn operands(&self) -> &[String] {
        &self.operands
    }

    pub(crate) fn into_operands(self) -> Vec<String> {
        self.operands
    }

    /// The words rearranged for a second scan: the option words in the
    /// order they were seen (with the value words they consumed), then the
    /// operands behind a `--` if the original had one, then a long option
    /// that was left without its value.
    ///
    /// `tune` reads its options twice and the second reading sees this
    /// order, so a short option that lacked its value at the end takes the
    /// first operand as its value there.
    // Compatibility: `tune a.rrd -h` fails with the heartbeat error and
    // `tune x:700 -h` sets the heartbeat from the file name.
    // Provenance: black-box compatibility test.
    pub(crate) fn rearranged(&self) -> Vec<String> {
        let mut words = self.option_words.clone();
        if self.saw_separator {
            words.push(String::from("--"));
        }
        words.extend(self.operands.iter().cloned());
        words.extend(self.dangling.iter().cloned());
        words
    }

    /// Whether a long option at the end was left without its value.
    pub(crate) fn has_dangling_long(&self) -> bool {
        self.dangling.is_some()
    }

    fn by_long(&self, name: &str) -> Option<&'t OptionSpec<K>> {
        self.table.iter().find(|spec| spec.long == name)
    }

    fn by_short(&self, byte: u8) -> Option<&'t OptionSpec<K>> {
        self.table.iter().find(|spec| {
            spec.short
                .is_some_and(|letter| letter.is_ascii() && letter as u32 == u32::from(byte))
        })
    }

    /// Takes the word after the current one as a value, if there is one.
    fn take_following(&mut self) -> Option<String> {
        let value = self.words.get(self.next).cloned()?;
        self.next += 1;
        self.option_words.push(value.clone());
        Some(value)
    }

    fn scan_long(&mut self, word: &str) -> Result<Found<K>, OptionError> {
        let body = &word[2..];
        let (name, inline) = match body.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (body, None),
        };
        let Some(spec) = self.by_long(name) else {
            return Err(OptionError::UnknownLong(body.to_owned()));
        };
        match (spec.arity, inline) {
            (Arity::Flag, None) => Ok(Found {
                key: spec.key,
                value: None,
            }),
            (Arity::Flag, Some(_)) => Err(OptionError::UnexpectedValue(spec.long)),
            (Arity::Value, Some(value)) => Ok(Found {
                key: spec.key,
                value: Some(value.to_owned()),
            }),
            (Arity::Value, None) => match self.take_following() {
                Some(value) => Ok(Found {
                    key: spec.key,
                    value: Some(value),
                }),
                None => {
                    // The word goes behind the operands for a second scan.
                    self.option_words.pop();
                    self.dangling = Some(word.to_owned());
                    Err(OptionError::MissingLongValue(spec.long))
                }
            },
        }
    }

    /// Scans the letter at `offset` of the current group, which is
    /// `words[next - 1]`.
    fn scan_letter(&mut self, offset: usize) -> Result<Found<K>, OptionError> {
        let word = self.words[self.next - 1].clone();
        let bytes = word.as_bytes();
        let byte = bytes[offset];
        let rest = offset + 1;
        let Some(spec) = self.by_short(byte) else {
            self.letter = (rest < bytes.len()).then_some(rest);
            return Err(OptionError::UnknownShort(byte));
        };
        let letter = spec.short.unwrap_or('?');
        match spec.arity {
            Arity::Flag => {
                self.letter = (rest < bytes.len()).then_some(rest);
                Ok(Found {
                    key: spec.key,
                    value: None,
                })
            }
            Arity::Value if rest < bytes.len() => {
                // The letter is ASCII, so `rest` is a character boundary.
                Ok(Found {
                    key: spec.key,
                    value: Some(word[rest..].to_owned()),
                })
            }
            Arity::Value => match self.take_following() {
                Some(value) => Ok(Found {
                    key: spec.key,
                    value: Some(value),
                }),
                None => Err(OptionError::MissingShortValue(letter)),
            },
        }
    }
}

impl<K: Copy> Iterator for OptionScanner<'_, K> {
    type Item = Result<Found<K>, OptionError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(offset) = self.letter.take() {
            return Some(self.scan_letter(offset));
        }
        loop {
            let word = self.words.get(self.next)?.clone();
            self.next += 1;
            if self.saw_separator {
                self.operands.push(word);
                continue;
            }
            if word == "--" {
                self.saw_separator = true;
                continue;
            }
            if word.starts_with("--") {
                self.option_words.push(word.clone());
                return Some(self.scan_long(&word));
            }
            if word.len() > 1 && word.starts_with('-') {
                self.option_words.push(word);
                return Some(self.scan_letter(1));
            }
            self.operands.push(word);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[OptionSpec<char>] = &[
        OptionSpec::value("start", Some('s'), 's'),
        OptionSpec::value("end", Some('e'), 'e'),
        OptionSpec::flag("align-start", Some('a'), 'a'),
        OptionSpec::flag("noflush", Some('F'), 'F'),
        OptionSpec::value("rraindex", None, 'R'),
        OptionSpec::flag("skip-past-updates", Some('k'), 'k'),
    ];

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    /// Scans to the first error, as most commands do.
    type Scanned = (Vec<(char, Option<String>)>, Vec<String>);

    fn scan(list: &[&str]) -> Result<Scanned, String> {
        let input = words(list);
        let mut scanner = OptionScanner::new(&input, TABLE);
        let mut found = Vec::new();
        for item in scanner.by_ref() {
            let item = item.map_err(|error| error.to_string())?;
            found.push((item.key, item.value));
        }
        Ok((found, scanner.into_operands()))
    }

    fn opt(key: char, value: &str) -> (char, Option<String>) {
        (key, Some(value.to_owned()))
    }

    #[test]
    fn long_names_are_exact() {
        assert_eq!(scan(&["--start", "x"]).unwrap().0, vec![opt('s', "x")]);
        for word in ["--sta", "--st", "--START", "--a", "--rrainde"] {
            let expected = format!("invalid option -- '{}'", &word[2..]);
            assert_eq!(scan(&[word, "v"]).unwrap_err(), expected);
        }
    }

    #[test]
    fn unknown_long_reports_everything_after_the_dashes() {
        assert_eq!(scan(&["---x"]).unwrap_err(), "invalid option -- '-x'");
        assert_eq!(scan(&["--=x"]).unwrap_err(), "invalid option -- '=x'");
        assert_eq!(
            scan(&["--skip-past-update=1"]).unwrap_err(),
            "invalid option -- 'skip-past-update=1'"
        );
    }

    #[test]
    fn long_values_take_the_next_word_or_the_inline_text() {
        assert_eq!(scan(&["--start", "--"]).unwrap().0, vec![opt('s', "--")]);
        assert_eq!(scan(&["--start", "-e"]).unwrap().0, vec![opt('s', "-e")]);
        assert_eq!(scan(&["--start", ""]).unwrap().0, vec![opt('s', "")]);
        assert_eq!(scan(&["--start=a=b"]).unwrap().0, vec![opt('s', "a=b")]);
        assert_eq!(scan(&["--start="]).unwrap().0, vec![opt('s', "")]);
        assert_eq!(
            scan(&["f", "--start"]).unwrap_err(),
            "option requires argument -- 'start'"
        );
    }

    #[test]
    fn long_flags_reject_inline_values() {
        assert_eq!(
            scan(&["--noflush="]).unwrap_err(),
            "option takes no arguments -- 'noflush'"
        );
        // Cut to 47 bytes: the closing quote is lost.
        assert_eq!(
            scan(&["--skip-past-updates=1"]).unwrap_err(),
            "option takes no arguments -- 'skip-past-updates"
        );
    }

    #[test]
    fn short_options_and_groups() {
        assert_eq!(scan(&["-s1000"]).unwrap().0, vec![opt('s', "1000")]);
        assert_eq!(scan(&["-s=1"]).unwrap().0, vec![opt('s', "=1")]);
        assert_eq!(scan(&["-s", "-e"]).unwrap().0, vec![opt('s', "-e")]);
        assert_eq!(scan(&["-sa"]).unwrap().0, vec![opt('s', "a")]);
        assert_eq!(
            scan(&["-aFs", "9"]).unwrap().0,
            vec![('a', None), ('F', None), opt('s', "9")]
        );
        assert_eq!(
            scan(&["-s"]).unwrap_err(),
            "option requires an argument -- 's'"
        );
        assert_eq!(scan(&["-r"]).unwrap_err(), "invalid option -- 'r'");
        assert_eq!(scan(&["-aQ"]).unwrap_err(), "invalid option -- 'Q'");
    }

    #[test]
    fn unknown_multibyte_letter_reports_its_first_byte() {
        let input = words(&["-é"]);
        let error = OptionScanner::new(&input, TABLE)
            .next()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.message_bytes(), b"invalid option -- '\xc3'");
    }

    #[test]
    fn operands_keep_their_order_around_options() {
        let (found, operands) = scan(&["-s", "T", "f.rrd", "-e", "U", "AVERAGE"]).unwrap();
        assert_eq!(found, vec![opt('s', "T"), opt('e', "U")]);
        assert_eq!(operands, words(&["f.rrd", "AVERAGE"]));
    }

    #[test]
    fn double_dash_ends_options_once() {
        let (found, operands) = scan(&["-a", "--", "--", "-s", "x", "-"]).unwrap();
        assert_eq!(found, vec![('a', None)]);
        assert_eq!(operands, words(&["--", "-s", "x", "-"]));
        assert_eq!(scan(&["-"]).unwrap().1, words(&["-"]));
    }

    #[test]
    fn repeated_options_are_all_reported() {
        let (found, _) = scan(&["-s", "1", "--start", "2"]).unwrap();
        assert_eq!(found, vec![opt('s', "1"), opt('s', "2")]);
    }

    #[test]
    fn invalid_option_name_is_cut_to_47_bytes() {
        let name = "x".repeat(26);
        assert_eq!(
            scan(&[&format!("--{name}")]).unwrap_err(),
            format!("invalid option -- '{name}'")
        );
        let long = "x".repeat(40);
        let message = scan(&[&format!("--{long}")]).unwrap_err();
        assert_eq!(message.len(), 47);
        assert_eq!(message, format!("invalid option -- '{}", "x".repeat(28)));
    }

    #[test]
    fn scanning_continues_after_an_unknown_option() {
        let input = words(&["a.rrd", "--bogus", "-s", "1", "b.rrd"]);
        let mut scanner = OptionScanner::new(&input, TABLE);
        let items = scanner.by_ref().collect::<Vec<_>>();
        assert_eq!(items.len(), 2);
        assert!(items[0].is_err());
        assert_eq!(scanner.operands(), &words(&["a.rrd", "b.rrd"])[..]);
    }

    #[test]
    fn rearranged_words_put_options_first() {
        let input = words(&["x.rrd", "-s"]);
        let mut scanner = OptionScanner::new(&input, TABLE);
        while scanner.next().is_some() {}
        assert_eq!(scanner.rearranged(), words(&["-s", "x.rrd"]));

        let input = words(&["a.rrd", "b.rrd", "--start"]);
        let mut scanner = OptionScanner::new(&input, TABLE);
        while scanner.next().is_some() {}
        assert!(scanner.has_dangling_long());
        assert_eq!(scanner.rearranged(), words(&["a.rrd", "b.rrd", "--start"]));

        // A value word that happens to be `--` is not the separator.
        let input = words(&["a.rrd", "-s", "--", "-e", "1", "--", "-q"]);
        let mut scanner = OptionScanner::new(&input, TABLE);
        while scanner.next().is_some() {}
        assert_eq!(
            scanner.rearranged(),
            words(&["-s", "--", "-e", "1", "--", "a.rrd", "-q"])
        );
    }
}
