//! Splits an at-style time specification into tokens.
//!
//! Compatibility: digit runs, letter runs and the punctuation `+ - : . /`
//! are tokens; blanks, `_` and `,` only separate them; any other byte ends
//! the specification without an error.
//! Provenance: black-box compatibility test; see
//! docs/compatibility/time-spec.md TS-7 to TS-9.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tok<'a> {
    /// A run of ASCII digits, as written (leading zeros kept).
    Number(&'a str),
    /// A run of ASCII letters, as written.
    Word(&'a str),
    Plus,
    Minus,
    Colon,
    Dot,
    Slash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Token<'a> {
    pub tok: Tok<'a>,
    /// Byte offset of the token in the specification.
    pub at: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Lexed<'a> {
    pub tokens: Vec<Token<'a>>,
    /// Offset of the byte that ended the specification early, if any.
    pub stopped_at: Option<usize>,
}

/// Bytes that separate tokens and are otherwise ignored. The blank set is
/// the C locale's white space, which includes `\r`, `\v` and `\f`.
pub(super) fn is_separator(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c | b'_' | b','
    )
}

pub(super) fn tokenize(text: &str) -> Lexed<'_> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if is_separator(byte) {
            at += 1;
            continue;
        }
        let (tok, end) = if byte.is_ascii_digit() {
            let end = run_end(bytes, at, u8::is_ascii_digit);
            (Tok::Number(&text[at..end]), end)
        } else if byte.is_ascii_alphabetic() {
            let end = run_end(bytes, at, u8::is_ascii_alphabetic);
            (Tok::Word(&text[at..end]), end)
        } else {
            let tok = match byte {
                b'+' => Tok::Plus,
                b'-' => Tok::Minus,
                b':' => Tok::Colon,
                b'.' => Tok::Dot,
                b'/' => Tok::Slash,
                _ => {
                    return Lexed {
                        tokens,
                        stopped_at: Some(at),
                    };
                }
            };
            (tok, at + 1)
        };
        tokens.push(Token { tok, at });
        at = end;
    }
    Lexed {
        tokens,
        stopped_at: None,
    }
}

fn run_end(bytes: &[u8], start: usize, keep: fn(&u8) -> bool) -> usize {
    bytes[start..]
        .iter()
        .position(|byte| !keep(byte))
        .map_or(bytes.len(), |len| start + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<Tok<'_>> {
        tokenize(text).tokens.into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn splits_on_kind_changes_without_separators() {
        assert_eq!(
            kinds("12:00Jan2,2003"),
            [
                Tok::Number("12"),
                Tok::Colon,
                Tok::Number("00"),
                Tok::Word("Jan"),
                Tok::Number("2"),
                Tok::Number("2003"),
            ]
        );
        assert_eq!(
            kinds("+1d2h"),
            [
                Tok::Plus,
                Tok::Number("1"),
                Tok::Word("d"),
                Tok::Number("2"),
                Tok::Word("h"),
            ]
        );
    }

    #[test]
    fn separators_include_c_white_space_underscore_and_comma() {
        assert_eq!(kinds(" \t\n\r\x0b\x0c_,now"), [Tok::Word("now")],);
        assert_eq!(tokenize("now").tokens[0].at, 0);
        assert_eq!(tokenize("_ now").tokens[0].at, 2);
    }

    #[test]
    fn foreign_byte_ends_the_specification() {
        let lexed = tokenize("1000000000#+1d");
        assert_eq!(lexed.tokens.len(), 1);
        assert_eq!(lexed.stopped_at, Some(10));
        let lexed = tokenize("12é3");
        assert_eq!(lexed.tokens.len(), 1);
        assert_eq!(lexed.stopped_at, Some(2));
        assert_eq!(tokenize("").stopped_at, None);
    }

    #[test]
    fn punctuation_tokens() {
        assert_eq!(
            kinds("+-:./"),
            [Tok::Plus, Tok::Minus, Tok::Colon, Tok::Dot, Tok::Slash]
        );
    }
}
