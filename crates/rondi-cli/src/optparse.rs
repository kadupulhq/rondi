/// `enum optparse_argtype`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgType {
    None,
    Required,
}

/// `struct optparse_long`. `short` is an `int`; table entries above 255
/// identify long-only options.
pub(crate) struct LongOpt {
    pub(crate) name: &'static str,
    pub(crate) short: i32,
    pub(crate) argtype: ArgType,
}

pub(crate) const fn opt(name: &'static str, short: i32, argtype: ArgType) -> LongOpt {
    LongOpt {
        name,
        short,
        argtype,
    }
}

/// The value `optparse` returns once no options remain.
pub(crate) const DONE: i32 = -1;
/// The value `optparse` returns for an error; the text is in `errmsg`.
pub(crate) const ERROR: i32 = b'?' as i32;

/// `struct optparse`. `errmsg` is a 48-byte buffer upstream, so messages
/// are cut at 47 bytes.
pub(crate) struct OptParse {
    pub(crate) argv: Vec<String>,
    pub(crate) optind: usize,
    pub(crate) optopt: i32,
    pub(crate) optarg: Option<String>,
    pub(crate) errmsg: String,
    subopt: usize,
    /// Indices of non-options passed over and not yet permuted.
    skipped: Vec<usize>,
}

const ERRMSG_SIZE: usize = 48;

fn is_dashdash(arg: &[u8]) -> bool {
    arg == b"--"
}

fn is_shortopt(arg: &[u8]) -> bool {
    arg.len() >= 2 && arg[0] == b'-' && arg[1] != b'-'
}

fn is_longopt(arg: &[u8]) -> bool {
    arg.len() >= 3 && arg[0] == b'-' && arg[1] == b'-'
}

/// A `char` read from argv promoted to `int`, as the C code returns it.
fn char_value(byte: u8) -> i32 {
    byte as libc::c_char as i32
}

/// `optstring_from_long`: each short name is stored through a `char`, so
/// long-only codes are truncated to their low byte.
fn optstring_from_long(longopts: &[LongOpt]) -> Vec<u8> {
    let mut optstring = Vec::new();
    for longopt in longopts {
        if longopt.short != 0 {
            optstring.push(longopt.short as u8);
            if longopt.argtype == ArgType::Required {
                optstring.push(b':');
            }
        }
    }
    optstring
}

/// `argtype`: -1 for unknown, else the number of colons after `c`. No
/// RRDtool table uses `OPTPARSE_OPTIONAL`, so there is never a second one.
fn argtype(optstring: &[u8], c: u8) -> i32 {
    if c == b':' {
        return -1;
    }
    // The C string ends at the first NUL a truncated short name produced.
    let optstring = optstring
        .split(|byte| *byte == 0)
        .next()
        .unwrap_or_default();
    let Some(position) = optstring.iter().position(|byte| *byte == c) else {
        return -1;
    };
    i32::from(optstring.get(position + 1) == Some(&b':'))
}

/// `longopts_match`: exact name, optionally followed by `=value`.
fn longopts_match(longname: &str, option: &[u8]) -> bool {
    let name = longname.as_bytes();
    option.starts_with(name) && matches!(option.get(name.len()), None | Some(b'='))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl OptParse {
    /// `optparse_init`: `argv[0]` is the command name and is never parsed.
    pub(crate) fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            optind: 1,
            optopt: 0,
            optarg: None,
            errmsg: String::new(),
            subopt: 0,
            skipped: Vec::new(),
        }
    }

    /// The positionals left after option parsing.
    pub(crate) fn positionals(&self) -> &[String] {
        self.argv.get(self.optind..).unwrap_or_default()
    }

    fn set_error(&mut self, message: Vec<u8>) {
        let mut message = message;
        message.truncate(ERRMSG_SIZE - 1);
        self.errmsg = lossy(&message);
    }

    fn arg(&self, index: usize) -> Option<&[u8]> {
        self.argv.get(index).map(String::as_bytes)
    }

    /// `optparse` for an argument known to be a short option (the only way
    /// `optparse_long` reaches it).
    fn short(&mut self, optstring: &[u8]) -> i32 {
        self.errmsg.clear();
        self.optopt = 0;
        self.optarg = None;
        let arg = self.argv[self.optind].as_bytes();
        // An invalid option leaves `subopt` set upstream; no caller keeps
        // parsing after an error, so clamping only avoids reading past the end.
        let position = (self.subopt + 1).min(arg.len() - 1);
        let c = arg[position];
        let has_rest = position + 1 < arg.len();
        self.optopt = char_value(c);
        match argtype(optstring, c) {
            -1 => {
                self.set_error([b"invalid option -- '".as_slice(), &[c, b'\'']].concat());
                self.optind += 1;
                ERROR
            }
            0 => {
                if has_rest {
                    self.subopt += 1;
                } else {
                    self.subopt = 0;
                    self.optind += 1;
                }
                char_value(c)
            }
            _ => {
                self.subopt = 0;
                self.optind += 1;
                if has_rest {
                    self.optarg = Some(lossy(
                        &self.argv[self.optind - 1].as_bytes()[position + 1..],
                    ));
                } else if let Some(next) = self.argv.get(self.optind) {
                    self.optarg = Some(next.clone());
                    self.optind += 1;
                } else {
                    self.set_error(
                        [b"option requires an argument -- '".as_slice(), &[c, b'\'']].concat(),
                    );
                    self.optarg = None;
                    return ERROR;
                }
                char_value(c)
            }
        }
    }

    /// `optparse_long`. Upstream skips each non-option by recursing and
    /// permutes it behind the arguments the next option consumed. The
    /// skipped words are only observable once parsing ends, so they are
    /// collected here and placed in one pass when it does: no stack frame
    /// per word and no quadratic shifting.
    pub(crate) fn long(&mut self, longopts: &[LongOpt]) -> i32 {
        while let Some(arg) = self.arg(self.optind) {
            if is_dashdash(arg) || is_shortopt(arg) || is_longopt(arg) {
                break;
            }
            self.skipped.push(self.optind);
            self.optind += 1;
        }
        let result = self.long_at(longopts);
        if result == DONE {
            self.finish();
        }
        result
    }

    /// The `argv` and `optind` upstream's permutation leaves at the end:
    /// consumed options (and `--`) in order, then the skipped words, then
    /// whatever followed.
    fn finish(&mut self) {
        if self.skipped.is_empty() {
            return;
        }
        let end = self.optind.min(self.argv.len());
        let mut words = std::mem::take(&mut self.argv)
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>();
        let skipped = std::mem::take(&mut self.skipped);
        let mut argv = Vec::with_capacity(words.len());
        let nonoptions = skipped
            .iter()
            .filter_map(|index| words[*index].take())
            .collect::<Vec<_>>();
        argv.extend(words[..end].iter_mut().filter_map(Option::take));
        self.optind = argv.len();
        argv.extend(nonoptions);
        argv.extend(words[end..].iter_mut().filter_map(Option::take));
        self.argv = argv;
    }

    /// `optparse_long` once `argv[optind]` is an option, `--`, or the end.
    fn long_at(&mut self, longopts: &[LongOpt]) -> i32 {
        let Some(arg) = self.arg(self.optind) else {
            return DONE;
        };
        if is_dashdash(arg) {
            self.optind += 1;
            return DONE;
        }
        if is_shortopt(arg) {
            return self.short(&optstring_from_long(longopts));
        }
        self.errmsg.clear();
        self.optopt = 0;
        self.optarg = None;
        let option = self.argv[self.optind].as_bytes()[2..].to_vec();
        self.optind += 1;
        for longopt in longopts {
            if !longopts_match(longopt.name, &option) {
                continue;
            }
            self.optopt = longopt.short;
            let value = option
                .iter()
                .position(|byte| *byte == b'=')
                .map(|position| lossy(&option[position + 1..]));
            if longopt.argtype == ArgType::None && value.is_some() {
                self.set_error(format!("option takes no arguments -- '{}'", longopt.name).into());
                return ERROR;
            }
            if value.is_some() {
                self.optarg = value;
            } else if longopt.argtype == ArgType::Required {
                self.optarg = self.argv.get(self.optind).cloned();
                self.optind += 1;
                if self.optarg.is_none() {
                    self.set_error(
                        format!("option requires argument -- '{}'", longopt.name).into(),
                    );
                    return ERROR;
                }
            }
            return self.optopt;
        }
        let mut message = b"invalid option -- '".to_vec();
        message.extend_from_slice(&option);
        message.push(b'\'');
        self.set_error(message);
        ERROR
    }

    /// The current `optarg`; the C code reads it only after a match that
    /// set it.
    pub(crate) fn value(&self) -> &str {
        self.optarg.as_deref().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[LongOpt] = &[
        opt("start", b's' as i32, ArgType::Required),
        opt("rigid", b'r' as i32, ArgType::None),
        opt("lazy", b'z' as i32, ArgType::None),
        opt("units", 255, ArgType::Required),
        opt("jsontime", 1000, ArgType::None),
    ];

    type Parsed = (Vec<(i32, Option<String>)>, Vec<String>, String);

    fn parse(args: &[&str]) -> Parsed {
        let mut options = OptParse::new(args.iter().map(|arg| arg.to_string()).collect());
        let mut seen = Vec::new();
        loop {
            let opt = options.long(TABLE);
            if opt == DONE || opt == ERROR {
                return (seen, options.positionals().to_vec(), options.errmsg);
            }
            seen.push((opt, options.optarg.clone()));
        }
    }

    #[test]
    fn permutes_nonoptions_after_options() {
        let (seen, rest, _) = parse(&["cmd", "a", "-s", "1", "b", "--rigid", "c"]);
        assert_eq!(seen, [(b's' as i32, Some("1".into())), (b'r' as i32, None)]);
        assert_eq!(rest, ["a", "b", "c"]);
    }

    #[test]
    fn dashdash_stops_parsing_and_keeps_order() {
        let (seen, rest, _) = parse(&["cmd", "a", "--", "-r", "b"]);
        assert!(seen.is_empty());
        assert_eq!(rest, ["a", "-r", "b"]);
    }

    #[test]
    fn attached_and_bundled_short_options() {
        let (seen, _, _) = parse(&["cmd", "-rzs5", "--start=7"]);
        assert_eq!(
            seen,
            [
                (b'r' as i32, None),
                (b'z' as i32, None),
                (b's' as i32, Some("5".into())),
                (b's' as i32, Some("7".into())),
            ]
        );
    }

    #[test]
    fn error_texts() {
        assert_eq!(parse(&["cmd", "-Z"]).2, "invalid option -- 'Z'");
        assert_eq!(
            parse(&["cmd", "-s"]).2,
            "option requires an argument -- 's'"
        );
        assert_eq!(
            parse(&["cmd", "--start"]).2,
            "option requires argument -- 'start'"
        );
        assert_eq!(
            parse(&["cmd", "--rigid=1"]).2,
            "option takes no arguments -- 'rigid'"
        );
        assert_eq!(parse(&["cmd", "--sta"]).2, "invalid option -- 'sta'");
        assert_eq!(
            parse(&["cmd", "--a-very-long-option-name-that-overflows=1"]).2,
            "invalid option -- 'a-very-long-option-name-that"
        );
    }

    #[test]
    fn missing_argument_after_a_permuted_word() {
        assert_eq!(
            parse(&["cmd", "file", "--start"]).2,
            "option requires argument -- 'start'"
        );
        assert_eq!(
            parse(&["cmd", "file", "-s"]).2,
            "option requires an argument -- 's'"
        );
    }

    #[test]
    fn many_words_and_long_clusters_stay_iterative() {
        let mut args = vec![String::from("cmd")];
        for index in 0..100_000 {
            args.push(format!("w{index}"));
            if index % 1000 == 0 {
                args.push(String::from("-r"));
            }
        }
        args.push(format!("-{}", "z".repeat(100_000)));
        let mut options = OptParse::new(args);
        let mut seen = 0;
        loop {
            match options.long(TABLE) {
                DONE => break,
                ERROR => panic!("{}", options.errmsg),
                _ => seen += 1,
            }
        }
        assert_eq!(seen, 100 + 100_000);
        let rest = options.positionals();
        assert_eq!(rest.len(), 100_000);
        assert_eq!(rest[0], "w0");
        assert_eq!(rest[99_999], "w99999");
    }

    #[test]
    fn long_only_codes_are_returned_whole() {
        let (seen, _, _) = parse(&["cmd", "--units", "si", "--jsontime"]);
        assert_eq!(seen, [(255, Some("si".into())), (1000, None)]);
    }
}
