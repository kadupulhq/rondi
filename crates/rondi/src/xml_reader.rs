//! A pull reader for `rrdtool restore` dumps that releases nodes when the
//! reference reader would.
//!
//! Which walk step meets a syntax error, whether a restore still writes its
//! file and the line numbers in its messages all depend on how much of the
//! input the XML parser had seen when the walk asked for the next node
//! (RS-41, RS-43). The reader therefore keeps two views of the dump: a
//! tokenizer that splits the whole document into constructs, each tagged
//! with how many bytes must have been delivered before a streaming parser
//! can handle it, and a delivery schedule that hands those bytes over the
//! way the reference does: a 4-byte probe and then 512-byte blocks for a
//! named file, one line at a time for standard input.
//!
//! A hand-written tokenizer is used instead of the workspace's DOM parser
//! because the observable behavior depends on partial documents, on where
//! each construct ends and on errors met halfway through; a DOM parser only
//! reports whether the whole text is well formed.
//!
//! Compatibility: delivery, availability and post-error rules reproduce
//! RRDtool 1.11.0 linked with libxml2 2.9 (RS-9 to RS-15a, RS-40 to RS-44).
//! Provenance: black-box compatibility tests; see
//! docs/compatibility/restore.md.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Write;

/// How the dump reaches the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// A named file, read in blocks.
    File,
    /// Standard input, read a line at a time.
    Stdin,
}

/// A node or end tag as the walk sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Event {
    Start { name: String, empty: bool },
    End { name: String },
    Text { text: String, blank: bool },
    Comment,
    Instruction,
    CData,
    Doctype,
}

impl Event {
    fn is_node(&self) -> bool {
        !matches!(self, Event::End { .. })
    }
}

/// The outcome of one read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Read {
    Event(Event),
    /// The document ended normally.
    Eof,
    /// The parser reported a fatal error, available from [`Reader::failure`].
    Failed,
}

// ---------------------------------------------------------------------------
// Input preparation.

/// Block size the reference reader hands to the parser for a named file.
const BLOCK: usize = 512;
/// Size of each read from a named file into the reader's buffer.
const READ_SIZE: usize = 4096;
/// Bytes the reader takes up front to detect the encoding.
const PROBE: usize = 4;

/// Compatibility: standard input is read a line at a time as C strings, so
/// a NUL ends the line early and a ctrl-Z ends the input (RS-9).
/// Provenance: black-box compatibility test (`doc-nul-in-value@stdin`,
/// `doc-tail-nul@stdin`, `doc-ctrl-z@stdin`).
fn stdin_lines(input: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut stream = Vec::with_capacity(input.len());
    let mut ends = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let line_len = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(rest.len(), |at| at + 1);
        let line = &rest[..line_len];
        rest = &rest[line_len..];
        let stop = line.iter().position(|byte| *byte == 0x1a);
        let usable = &line[..stop.unwrap_or(line.len())];
        let kept = usable
            .iter()
            .position(|byte| *byte == 0)
            .map_or(usable, |nul| &usable[..nul]);
        stream.extend_from_slice(kept);
        if !kept.is_empty() {
            ends.push(stream.len());
        }
        if stop.is_some() {
            break;
        }
    }
    (stream, ends)
}

/// The order in which input bytes become visible to the parser.
enum Feed {
    File {
        len: usize,
        buffered: usize,
        handed: usize,
        eof: bool,
        done: bool,
    },
    Lines {
        ends: Vec<usize>,
        next: usize,
        len: usize,
        done: bool,
    },
}

impl Feed {
    /// The next delivery: the stream offset delivered so far and whether
    /// this delivery also signals the end of the input.
    ///
    /// Compatibility: a named file is buffered in 4096-byte reads after a
    /// 4-byte probe and handed over in 512-byte blocks; a short final block
    /// carries the end-of-input signal only once a read has returned
    /// nothing. Provenance: black-box compatibility test (every truncation
    /// of `seed-mini`, vectors `cut-*`).
    fn next(&mut self) -> Option<(usize, bool)> {
        match self {
            Feed::File {
                len,
                buffered,
                handed,
                eof,
                done,
            } => {
                if *done {
                    return None;
                }
                if *buffered - *handed < BLOCK && !*eof {
                    let read = READ_SIZE.min(*len - *buffered);
                    if read == 0 {
                        *eof = true;
                    } else {
                        *buffered += read;
                    }
                }
                if *buffered - *handed >= BLOCK {
                    *handed += BLOCK;
                    Some((*handed, false))
                } else {
                    *handed = *buffered;
                    *done = *eof;
                    Some((*handed, *eof))
                }
            }
            Feed::Lines {
                ends,
                next,
                len,
                done,
            } => {
                if *done {
                    return None;
                }
                match ends.get(*next) {
                    Some(end) => {
                        *next += 1;
                        Some((*end, false))
                    }
                    None => {
                        *done = true;
                        Some((*len, true))
                    }
                }
            }
        }
    }
}

/// Maps offsets in the decoded text back to the delivered stream.
enum OffsetMap {
    Shift(usize),
    Table(Vec<usize>),
}

impl OffsetMap {
    fn raw(&self, index: usize) -> usize {
        match self {
            OffsetMap::Shift(shift) => index + shift,
            OffsetMap::Table(table) => table
                .get(index)
                .copied()
                .unwrap_or_else(|| table.last().copied().unwrap_or(0)),
        }
    }
}

/// The document as UTF-8 text, with where tokenizing must stop and why.
struct Decoded<'a> {
    text: Cow<'a, str>,
    map: OffsetMap,
    /// A decoding error at the end of `text`, if the text stops early.
    error: Option<String>,
    /// Set when the declared encoding cannot be read at all.
    unsupported: Option<String>,
}

/// The value of `encoding=` in a leading XML declaration, if any.
fn declared_encoding(bytes: &[u8]) -> Option<String> {
    if !bytes.starts_with(b"<?xml") {
        return None;
    }
    let end = find(bytes, b"?>", 0).unwrap_or(bytes.len().min(512));
    let head = &bytes[..end];
    let at = find(head, b"encoding", 0)?;
    let mut rest = &head[at + "encoding".len()..];
    rest = trim_start(rest);
    rest = rest.strip_prefix(b"=")?;
    rest = trim_start(rest);
    let quote = *rest.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let close = rest[1..].iter().position(|byte| *byte == quote)?;
    Some(String::from_utf8_lossy(&rest[1..1 + close]).into_owned())
}

fn trim_start(mut bytes: &[u8]) -> &[u8] {
    while let [b' ' | b'\t' | b'\r' | b'\n', rest @ ..] = bytes {
        bytes = rest;
    }
    bytes
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= haystack.len()).then_some(from);
    }
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

/// Decodes the delivered stream.
///
/// Compatibility: a UTF-8 byte order mark is skipped, UTF-16 with a byte
/// order mark is accepted from a named file, a declared Latin-1 or ASCII
/// encoding is converted to UTF-8, an unknown encoding is refused and
/// invalid UTF-8 is a syntax error (RS-11). Provenance: black-box
/// compatibility test (`doc-*` vectors).
fn decode(stream: &[u8], delivery: Delivery) -> Decoded<'_> {
    if let Some(rest) = stream.strip_prefix(b"\xef\xbb\xbf") {
        return decode_utf8(rest, 3);
    }
    let utf16 = match stream {
        [0xff, 0xfe, ..] => Some(false),
        [0xfe, 0xff, ..] => Some(true),
        _ => None,
    };
    if let Some(big_endian) = utf16 {
        if delivery == Delivery::File {
            return decode_utf16(stream, big_endian);
        }
        return Decoded {
            text: Cow::Borrowed(""),
            map: OffsetMap::Shift(0),
            error: Some(EXTRA_CONTENT.into()),
            unsupported: None,
        };
    }
    match declared_encoding(stream).map(|name| name.to_ascii_lowercase()) {
        None => decode_utf8(stream, 0),
        Some(name) => match name.as_str() {
            "utf-8" | "utf8" | "us-ascii" | "ascii" | "utf-16" => decode_utf8(stream, 0),
            "iso-8859-1" | "iso_8859-1" | "latin1" | "latin-1" | "iso-latin-1" | "l1" => {
                decode_latin1(stream)
            }
            // Only the declaration is kept, to place the diagnostic.
            _ => Decoded {
                text: Cow::Owned(
                    String::from_utf8_lossy(
                        &stream[..find(stream, b"?>", 0).map_or(stream.len(), |at| at + 2)],
                    )
                    .into_owned(),
                ),
                map: OffsetMap::Shift(0),
                error: None,
                unsupported: declared_encoding(stream),
            },
        },
    }
}

fn decode_utf8(bytes: &[u8], shift: usize) -> Decoded<'_> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Decoded {
            text: Cow::Borrowed(text),
            map: OffsetMap::Shift(shift),
            error: None,
            unsupported: None,
        },
        Err(error) => {
            let valid = error.valid_up_to();
            let shown = bytes[valid..]
                .iter()
                .take(4)
                .map(|byte| format!("0x{byte:02X}"))
                .collect::<Vec<_>>()
                .join(" ");
            Decoded {
                text: Cow::Borrowed(std::str::from_utf8(&bytes[..valid]).unwrap_or_default()),
                map: OffsetMap::Shift(shift),
                error: Some(format!(
                    "Input is not proper UTF-8, indicate encoding !\nBytes: {shown}"
                )),
                unsupported: None,
            }
        }
    }
}

fn decode_latin1(bytes: &[u8]) -> Decoded<'_> {
    let mut text = String::with_capacity(bytes.len());
    let mut table = Vec::with_capacity(bytes.len() + 1);
    for (offset, byte) in bytes.iter().enumerate() {
        let ch = char::from(*byte);
        for _ in 0..ch.len_utf8() {
            table.push(offset);
        }
        text.push(ch);
    }
    table.push(bytes.len());
    Decoded {
        text: Cow::Owned(text),
        map: OffsetMap::Table(table),
        error: None,
        unsupported: None,
    }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Decoded<'_> {
    let units = bytes[2..].as_chunks::<2>().0.iter().map(|pair| {
        if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        }
    });
    let mut text = String::new();
    let mut table = Vec::new();
    let mut offset = 2;
    let mut error = None;
    for decoded in char::decode_utf16(units) {
        match decoded {
            Ok(ch) => {
                for _ in 0..ch.len_utf8() {
                    table.push(offset);
                }
                text.push(ch);
                offset += 2 * ch.len_utf16();
            }
            Err(_) => {
                error = Some("Input is not proper UTF-16".to_owned());
                break;
            }
        }
    }
    table.push(offset);
    Decoded {
        text: Cow::Owned(text),
        map: OffsetMap::Table(table),
        error,
        unsupported: None,
    }
}

// ---------------------------------------------------------------------------
// Tokenizer.

/// One piece of the document and the delivery it needs.
#[derive(Debug)]
struct Construct {
    /// Stream offset where the construct begins.
    start: usize,
    /// Stream bytes that must be delivered before the parser handles the
    /// construct; `None` when only the end-of-input signal makes it final.
    ready: Option<usize>,
    item: Item,
}

#[derive(Debug)]
enum Item {
    Node(Event),
    /// A well-formed end tag.
    Close(String),
    /// An end tag the parser rejected; it still stops a post-error walk.
    BrokenClose,
    /// A diagnostic that does not stop the parser.
    Note {
        message: String,
        at: usize,
        warning: bool,
    },
    Fatal {
        message: String,
        at: usize,
        /// The input ended inside the document rather than inside a
        /// construct.
        document_end: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Prolog,
    Content,
    Epilog,
}

/// Nesting the tokenizer accepts. RRDtool showed no limit up to 20000
/// levels, and no dump nests deeper than four, so this only bounds the
/// state an adversarial document can build.
const MAX_DEPTH: usize = 100_000;

const EXTRA_CONTENT: &str = "Extra content at the end of the document";

struct Tokenizer<'a> {
    text: Cow<'a, str>,
    map: OffsetMap,
    /// Error to report where the decoded text stops early.
    stop_error: Option<String>,
    unsupported: Option<String>,
    pos: usize,
    phase: Phase,
    /// Open elements: name and the line its start tag began on.
    open: Vec<(String, usize)>,
    queue: VecDeque<Construct>,
    finished: bool,
    /// Entities declared in an internal DTD subset.
    declared: std::collections::HashSet<String>,
    line_pos: usize,
    line_no: usize,
    /// Whether any construct was produced yet.
    progressed: bool,
}

fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\n')
}

fn is_name_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || ch == ':' || (ch as u32) >= 0xc0 && !is_bad_char(ch)
}

fn is_name_char(ch: char) -> bool {
    is_name_start(ch) || ch.is_ascii_digit() || ch == '-' || ch == '.' || ch == '\u{b7}'
}

fn is_bad_char(ch: char) -> bool {
    let code = ch as u32;
    (code < 0x20 && !matches!(ch, '\t' | '\n' | '\r')) || code == 0xfffe || code == 0xffff
}

fn bad_char_message(ch: char) -> String {
    if ch == '\0' {
        "Char 0x0 out of allowed range".into()
    } else {
        format!("PCDATA invalid Char value {}", ch as u32)
    }
}

impl<'a> Tokenizer<'a> {
    fn new(decoded: Decoded<'a>) -> Self {
        Self {
            text: decoded.text,
            map: decoded.map,
            stop_error: decoded.error,
            unsupported: decoded.unsupported,
            pos: 0,
            phase: Phase::Prolog,
            open: Vec::new(),
            queue: VecDeque::new(),
            finished: false,
            declared: std::collections::HashSet::new(),
            line_pos: 0,
            line_no: 1,
            progressed: false,
        }
    }

    fn bytes(&self) -> &[u8] {
        self.text.as_bytes()
    }

    fn raw(&self, index: usize) -> usize {
        self.map.raw(index)
    }

    fn line_at(&mut self, index: usize) -> usize {
        if index < self.line_pos {
            return self.line_no;
        }
        let bytes = self.text.as_bytes();
        let end = index.min(bytes.len());
        self.line_no += bytes[self.line_pos..end]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        self.line_pos = end;
        self.line_no
    }

    fn char_at(&self, index: usize) -> Option<char> {
        self.text.get(index..)?.chars().next()
    }

    fn emit(&mut self, start: usize, ready: Option<usize>, item: Item) {
        self.progressed = true;
        let start = self.raw(start);
        let ready = ready.map(|index| self.raw(index));
        self.queue.push_back(Construct { start, ready, item });
    }

    /// A fatal error the parser reports with its caret at `at` (an offset
    /// into the decoded text).
    fn fatal(&mut self, start: usize, ready: Option<usize>, message: impl Into<String>, at: usize) {
        let at = self.raw(at);
        self.fatal_at(start, ready, message.into(), at, false);
    }

    /// A diagnostic printed before a fatal error at the same place.
    fn note(&mut self, start: usize, ready: Option<usize>, message: impl Into<String>, at: usize) {
        let at = self.raw(at);
        self.emit(
            start,
            ready,
            Item::Note {
                message: message.into(),
                at,
                warning: false,
            },
        );
    }

    fn fatal_at(
        &mut self,
        start: usize,
        ready: Option<usize>,
        message: String,
        at: usize,
        document_end: bool,
    ) {
        self.emit(
            start,
            ready,
            Item::Fatal {
                message,
                at,
                document_end,
            },
        );
        self.finished = true;
    }

    /// The end of the decoded text: either the real end of the input or a
    /// decoding error.
    fn at_end(&mut self, start: usize) {
        let len = self.bytes().len();
        if let Some(message) = self.stop_error.take() {
            let ready = self.raw(len) + 1;
            let at = self.raw(len);
            self.queue.push_back(Construct {
                start: self.raw(start),
                ready: Some(ready),
                item: Item::Fatal {
                    message,
                    at,
                    document_end: false,
                },
            });
            self.finished = true;
            return;
        }
        match self.phase {
            Phase::Epilog => self.finished = true,
            _ => {
                // The parser stops where its leftover begins, or at the very
                // start when it never got past the encoding probe.
                let at = if self.progressed || len >= PROBE {
                    self.raw(start.min(len))
                } else {
                    self.raw(0)
                };
                self.fatal_at(start, None, EXTRA_CONTENT.to_owned(), at, true);
            }
        }
    }

    fn next(&mut self) -> Option<Construct> {
        while self.queue.is_empty() && !self.finished {
            self.step();
        }
        self.queue.pop_front()
    }

    fn step(&mut self) {
        if let Some(name) = self.unsupported.take() {
            let end = find(self.bytes(), b"?>", 0).map_or(self.bytes().len(), |at| at + 2);
            let at = find(self.bytes(), name.as_bytes(), 0).map_or(end, |at| at + name.len() + 1);
            self.fatal(0, Some(end), format!("Unsupported encoding {name}"), at);
            return;
        }
        if self.pos == 0 && self.phase == Phase::Prolog && self.bytes().starts_with(b"<?xml") {
            let next = self.bytes().get(5).copied();
            if next.is_none() {
                // Nothing after the target: the parser sees a misplaced
                // declaration that never ends.
                self.pos = 5;
                self.note(
                    0,
                    None,
                    "XML declaration allowed only at the start of the document",
                    5,
                );
                self.note(0, None, "ParsePI: PI xml space expected", 5);
                self.fatal(0, None, "ParsePI: PI xml never end ...", 5);
                return;
            }
            if next.is_some_and(is_blank) {
                self.declaration();
                return;
            }
        }
        let len = self.bytes().len();
        let start = self.pos;
        if start >= len {
            self.at_end(start);
            return;
        }
        let byte = self.bytes()[start];
        if byte != b'<' {
            match self.phase {
                Phase::Content => self.text_run(),
                Phase::Prolog | Phase::Epilog => self.outside_text(),
            }
            return;
        }
        // The parser looks at two bytes before it decides what a `<` opens.
        if start + 1 >= len {
            self.pos = len;
            self.at_end(start);
            return;
        }
        let rest = &self.bytes()[start..];
        // Compatibility: a `<!` that could still open a comment or a CDATA
        // section waits for more input; at the end of the input it is left
        // unparsed. Provenance: black-box compatibility test
        // (`cut-1025@stdin`).
        let undecided = |prefix: &[u8]| rest.len() < prefix.len() && prefix.starts_with(rest);
        if undecided(b"<!--")
            || undecided(b"<![CDATA[")
            || undecided(b"<!DOCTYPE")
            || undecided(b"<?xml ")
        {
            self.pos = len;
            self.at_end(start);
            return;
        }
        if rest.starts_with(b"<!--") {
            self.comment();
        } else if rest.starts_with(b"<?") {
            self.instruction();
        } else if rest.starts_with(b"<![CDATA[") {
            self.cdata();
        } else if rest.starts_with(b"<!DOCTYPE") {
            self.doctype();
        } else if rest.starts_with(b"<!") {
            self.fatal(
                start,
                Some(start + 2),
                "StartTag: invalid element name",
                start + 1,
            );
        } else if rest.starts_with(b"</") {
            self.end_tag();
        } else {
            self.start_tag();
        }
    }

    /// Whitespace or text outside the root element.
    fn outside_text(&mut self) {
        let start = self.pos;
        let len = self.bytes().len();
        let mut at = start;
        while at < len && is_blank(self.bytes()[at]) {
            at += 1;
        }
        self.pos = at;
        if at >= len || self.bytes()[at] == b'<' {
            return;
        }
        let ready = (at + 2 <= len).then_some(at + 2);
        // Compatibility: text after the root element is reported even when
        // it is the last byte of the input. Provenance: black-box
        // compatibility test (`doc-tail-char@stdin`).
        let message = match self.phase {
            Phase::Prolog => "Document is empty",
            _ => EXTRA_CONTENT,
        };
        if ready.is_none() && self.stop_error.is_some() {
            self.at_end(start);
            return;
        }
        self.fatal(at, ready, message, at);
    }

    fn declaration(&mut self) {
        let bytes = self.bytes();
        let Some(close) = find(bytes, b"?>", 0) else {
            let len = self.bytes().len();
            self.fatal(0, None, "parsing XML declaration: '?>' expected", len);
            return;
        };
        let end = close + 2;
        let head = String::from_utf8_lossy(&bytes[..close]).into_owned();
        self.pos = end;
        self.progressed = true;
        let Some(at) = head.find("version") else {
            self.fatal(0, Some(end), "Malformed declaration expecting version", 5);
            return;
        };
        let after = &head[at + "version".len()..];
        let trimmed = after.trim_start().strip_prefix('=').map(str::trim_start);
        let Some(value) = trimmed.and_then(|rest| {
            let quote = rest.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let body = &rest[1..];
            body.find(quote).map(|close| &body[..close])
        }) else {
            self.fatal(0, Some(end), "Malformed declaration expecting version", 5);
            return;
        };
        let version = value.to_owned();
        // The caret of the reference diagnostic sits just after the quoted
        // version.
        let quote_end = head
            .find(&format!("{version}\""))
            .or_else(|| head.find(&format!("{version}'")))
            .map_or(close, |offset| offset + version.len() + 1);
        if version == "1.0" {
            return;
        }
        if version.starts_with("1.") {
            let at = self.raw(quote_end);
            self.emit(
                0,
                Some(end),
                Item::Note {
                    message: format!("Unsupported version '{version}'"),
                    at,
                    warning: true,
                },
            );
        } else {
            let at = self.raw(quote_end);
            self.fatal_at(
                0,
                Some(end),
                format!("Unsupported version '{version}'"),
                at,
                false,
            );
        }
    }

    fn comment(&mut self) {
        let start = self.pos;
        let Some(close) = find(self.bytes(), b"-->", start + 4) else {
            if self.stop_error.is_some() {
                self.at_end(start);
            } else {
                let len = self.bytes().len();
                self.fatal(start, None, "Comment not terminated", len);
            }
            return;
        };
        let end = close + 3;
        self.pos = end;
        let body = &self.bytes()[start + 4..close];
        if let Some(dashes) = find(body, b"--", 0) {
            let shown = String::from_utf8_lossy(&body[..dashes]).into_owned();
            self.fatal(
                start,
                Some(end),
                format!("Double hyphen within comment: <!--{shown}"),
                start + 4 + dashes,
            );
            return;
        }
        if let Some((offset, bad)) = self.text[start + 4..close]
            .char_indices()
            .find(|(_, ch)| is_bad_char(*ch))
        {
            self.fatal(start, Some(end), bad_char_message(bad), start + 4 + offset);
            return;
        }
        self.node(start, Some(end), Event::Comment);
    }

    fn instruction(&mut self) {
        let start = self.pos;
        let mut name_end = start + 2;
        while self.char_at(name_end).is_some_and(is_name_char) {
            name_end += self.char_at(name_end).map_or(1, char::len_utf8);
        }
        let target = self.text[start + 2..name_end].to_owned();
        let Some(close) = find(self.bytes(), b"?>", start + 2) else {
            if self.stop_error.is_some() {
                self.at_end(start);
            } else {
                let len = self.bytes().len();
                self.fatal(
                    start,
                    None,
                    format!("ParsePI: PI {target} never end ..."),
                    len,
                );
            }
            return;
        };
        let end = close + 2;
        self.pos = end;
        if target.is_empty() {
            self.fatal(start, Some(end), "xmlParsePI : no target name", start + 2);
        } else if target.eq_ignore_ascii_case("xml") {
            self.fatal(
                start,
                Some(end),
                "XML declaration allowed only at the start of the document",
                name_end,
            );
        } else {
            self.node(start, Some(end), Event::Instruction);
        }
    }

    fn cdata(&mut self) {
        let start = self.pos;
        let Some(close) = find(self.bytes(), b"]]>", start + 9) else {
            if self.stop_error.is_some() {
                self.at_end(start);
            } else {
                let len = self.bytes().len();
                self.fatal(start, None, "CData section not finished", len);
            }
            return;
        };
        let end = close + 3;
        self.pos = end;
        if self.phase != Phase::Content {
            self.fatal(
                start,
                Some(end),
                "StartTag: invalid element name",
                start + 1,
            );
            return;
        }
        self.node(start, Some(end), Event::CData);
    }

    fn doctype(&mut self) {
        let start = self.pos;
        let bytes = self.bytes();
        let mut at = start + 9;
        let mut quote = None;
        let mut subset = None;
        let mut end = None;
        while at < bytes.len() {
            let byte = bytes[at];
            match quote {
                Some(open) if byte == open => quote = None,
                Some(_) => {}
                None => match byte {
                    b'"' | b'\'' => quote = Some(byte),
                    b'[' if subset.is_none() => {
                        let Some(close) = subset_end(bytes, at + 1) else {
                            break;
                        };
                        subset = Some((at + 1, close));
                        at = close;
                    }
                    b'>' => {
                        end = Some(at + 1);
                        break;
                    }
                    _ => {}
                },
            }
            at += 1;
        }
        let Some(end) = end else {
            if self.stop_error.is_some() {
                self.at_end(start);
            } else {
                self.truncated_doctype(start);
            }
            return;
        };
        self.pos = end;
        if self.phase != Phase::Prolog {
            self.fatal(
                start,
                Some(end),
                "internal error: detected an error in element content\n",
                start,
            );
            return;
        }
        if let Some((from, to)) = subset {
            let names = entity_names(&self.bytes()[from..to]);
            self.declared.extend(names);
        }
        self.node(start, Some(end), Event::Doctype);
    }

    fn start_tag(&mut self) {
        let start = self.pos;
        let len = self.bytes().len();
        let first = self.char_at(start + 1);
        if !first.is_some_and(is_name_start) {
            self.fatal(
                start,
                Some(start + 2),
                "StartTag: invalid element name",
                start + 1,
            );
            return;
        }
        let name_end = self.scan_name(start + 1);
        let name = self.text[start + 1..name_end].to_owned();
        let next_gt = find(self.bytes(), b">", name_end).map(|at| at + 1);
        let mut at = name_end;
        let mut attributes = std::collections::HashSet::new();
        let outcome = loop {
            while at < len && is_blank(self.bytes()[at]) {
                at += 1;
            }
            if at >= len {
                break Err(None);
            }
            let rest = &self.bytes()[at..];
            if rest.starts_with(b"/>") {
                break Ok((at + 2, true));
            }
            if rest[0] == b'>' {
                break Ok((at + 1, false));
            }
            if rest[0] == b'/' && rest.len() == 1 {
                break Err(None);
            }
            if at == name_end || !self.char_at(at).is_some_and(is_name_start) {
                break Err(Some((
                    format!("Couldn't find end of Start Tag {name}"),
                    at,
                    false,
                )));
            }
            let attr_end = self.scan_name(at);
            let attr = self.text[at..attr_end].to_owned();
            at = attr_end;
            while at < len && is_blank(self.bytes()[at]) {
                at += 1;
            }
            if at >= len {
                break Err(None);
            }
            if self.bytes()[at] != b'=' {
                break Err(Some((
                    format!("Specification mandates value for attribute {attr}"),
                    at,
                    true,
                )));
            }
            at += 1;
            while at < len && is_blank(self.bytes()[at]) {
                at += 1;
            }
            if at >= len {
                break Err(None);
            }
            let quote = self.bytes()[at];
            if quote != b'"' && quote != b'\'' {
                break Err(Some(("AttValue: \" or ' expected".to_owned(), at, true)));
            }
            let Some(close) = self.bytes()[at + 1..]
                .iter()
                .position(|byte| *byte == quote)
            else {
                break Err(None);
            };
            at += close + 2;
            if !attributes.insert(attr.clone()) {
                break Err(Some((format!("Attribute {attr} redefined"), at, false)));
            }
        };
        match outcome {
            Ok((end, empty)) => {
                self.pos = end;
                if self.phase == Phase::Epilog {
                    self.fatal(start, Some(start + 2), EXTRA_CONTENT, start);
                    return;
                }
                if !empty && self.open.len() >= MAX_DEPTH {
                    self.fatal(
                        start,
                        Some(end),
                        format!("Excessive depth in document: {MAX_DEPTH}"),
                        start,
                    );
                    return;
                }
                let line = self.line_at(start);
                self.node(
                    start,
                    Some(end),
                    Event::Start {
                        name: name.clone(),
                        empty,
                    },
                );
                if empty {
                    if self.open.is_empty() {
                        self.phase = Phase::Epilog;
                    }
                } else {
                    self.open.push((name, line));
                    self.phase = Phase::Content;
                }
            }
            Err(Some((message, caret, bad_attribute))) => {
                let ready = next_gt;
                self.pos = ready.unwrap_or(len);
                if self.phase == Phase::Epilog {
                    self.fatal(start, Some(start + 2), EXTRA_CONTENT, start);
                } else if ready.is_none() && self.stop_error.is_some() {
                    self.at_end(start);
                } else if bad_attribute {
                    // A malformed attribute is reported three times over,
                    // ending with the start tag itself.
                    self.note(start, ready, message, caret);
                    self.note(start, ready, "attributes construct error", caret);
                    self.fatal(
                        start,
                        ready,
                        format!("Couldn't find end of Start Tag {name}"),
                        caret,
                    );
                } else {
                    self.fatal(start, ready, message, caret);
                }
            }
            Err(None) => {
                // The start tag runs into the end of the input. The
                // reference still creates the element under the name read
                // so far (`parse_tag_rrd: Unknown tag: na`).
                self.pos = len;
                if self.stop_error.is_some() {
                    self.at_end(start);
                    return;
                }
                if self.phase == Phase::Epilog {
                    self.fatal(start, None, EXTRA_CONTENT, start);
                    return;
                }
                self.node(
                    start,
                    None,
                    Event::Start {
                        name: name.clone(),
                        empty: false,
                    },
                );
                self.fatal(
                    start,
                    None,
                    format!("Couldn't find end of Start Tag {name}"),
                    len,
                );
            }
        }
    }

    /// Reports a DOCTYPE cut off by the end of the input, with the
    /// diagnostics the parser prints for the part that is missing.
    fn truncated_doctype(&mut self, start: usize) {
        let len = self.bytes().len();
        let mut at = start + "<!DOCTYPE".len();
        while at < len && is_blank(self.bytes()[at]) {
            at += 1;
        }
        let name_end = self.scan_name(at);
        if name_end == at {
            self.note(start, None, "xmlParseDocTypeDecl : no DOCTYPE name !", len);
        } else {
            at = name_end;
            while at < len && is_blank(self.bytes()[at]) {
                at += 1;
            }
            let rest = &self.bytes()[at..];
            if rest.starts_with(b"SYSTEM") || rest.starts_with(b"PUBLIC") {
                let mut literal = at + 6;
                while literal < len && is_blank(self.bytes()[literal]) {
                    literal += 1;
                }
                if literal >= len {
                    self.note(start, None, "SystemLiteral \" or ' expected", len);
                    self.note(start, None, "SYSTEM or PUBLIC, the URI is missing", len);
                } else {
                    self.note(
                        start,
                        None,
                        "Unfinished System or Public ID \" or ' expected",
                        len,
                    );
                }
            }
        }
        self.fatal(start, None, "DOCTYPE improperly terminated", len);
    }

    fn scan_name(&self, from: usize) -> usize {
        let mut at = from;
        while let Some(ch) = self.char_at(at).filter(|ch| is_name_char(*ch)) {
            at += ch.len_utf8();
        }
        at
    }

    fn end_tag(&mut self) {
        let start = self.pos;
        let len = self.bytes().len();
        let name_end = if self.char_at(start + 2).is_some_and(is_name_start) {
            self.scan_name(start + 2)
        } else {
            start + 2
        };
        let name = self.text[start + 2..name_end].to_owned();
        let mut at = name_end;
        while at < len && is_blank(self.bytes()[at]) {
            at += 1;
        }
        let complete = at < len && self.bytes()[at] == b'>';
        let ready = if complete {
            Some(at + 1)
        } else {
            find(self.bytes(), b">", at).map(|gt| gt + 1)
        };
        if ready.is_none() && self.stop_error.is_some() {
            self.pos = len;
            self.at_end(start);
            return;
        }
        self.pos = ready.unwrap_or(len);
        let Some((open, line)) = self.open.last().cloned() else {
            self.fatal(start, ready.or(Some(start + 2)), EXTRA_CONTENT, start);
            return;
        };
        if complete && name == open {
            self.open.pop();
            if self.open.is_empty() {
                self.phase = Phase::Epilog;
            }
            self.emit(start, ready, Item::Close(name));
            return;
        }
        let message = if name.is_empty() {
            format!("Opening and ending tag mismatch: {open} line {line} and unparsable")
        } else if name != open {
            format!("Opening and ending tag mismatch: {open} line {line} and {name}")
        } else {
            "expected '>'".to_owned()
        };
        self.emit(start, ready, Item::BrokenClose);
        let caret = if complete { at + 1 } else { at };
        // An end tag cut off by the end of the input is first reported as
        // missing its `>`.
        if ready.is_none() && !name.is_empty() && name != open {
            self.note(start, ready, "expected '>'", caret);
        }
        self.fatal(start, ready, message, caret);
    }

    /// Character data up to the next `<`, with references expanded.
    fn text_run(&mut self) {
        let start = self.pos;
        let len = self.bytes().len();
        let end = self.bytes()[start..]
            .iter()
            .position(|byte| *byte == b'<')
            .map_or(len, |at| start + at);
        self.pos = end;
        let reaches_end = end == len;
        if reaches_end && self.stop_error.is_some() {
            self.at_end(start);
            return;
        }
        let ready = (!reaches_end).then_some(end + 1);
        // Compatibility: a single trailing byte of character data is never
        // handed to the document, even at the end of the input.
        // Provenance: black-box compatibility test (truncations ending in
        // one byte of white space or `<`).
        if reaches_end && end - start < 2 {
            self.at_end(start);
            return;
        }
        let raw = self.text[start..end].to_owned();
        if raw.contains("]]>") {
            self.fatal(start, ready, "Sequence ']]>' not allowed in content", start);
            return;
        }
        let mut text = String::with_capacity(raw.len());
        let mut chars = raw.char_indices().peekable();
        while let Some((offset, ch)) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek().is_some_and(|(_, next)| *next == '\n') {
                        chars.next();
                    }
                    text.push('\n');
                }
                '&' => {
                    let tail = &raw[offset + 1..];
                    let Some(semi) = tail.find(';') else {
                        self.fatal(start, ready, "EntityRef: expecting ';'", start + offset + 1);
                        return;
                    };
                    let reference = &tail[..semi];
                    for _ in 0..reference.chars().count() + 1 {
                        chars.next();
                    }
                    match self.reference(reference) {
                        Ok(Some(expanded)) => text.push(expanded),
                        Ok(None) => {}
                        Err(Expansion::Undefined(name)) => {
                            let at = self.raw(start + offset + semi + 2);
                            self.emit(
                                start,
                                ready,
                                Item::Note {
                                    message: format!("Entity '{name}' not defined"),
                                    at,
                                    warning: false,
                                },
                            );
                        }
                        Err(Expansion::Fatal(message)) => {
                            self.fatal(start, ready, message, start + offset + 2 + semi);
                            return;
                        }
                    }
                }
                ch if is_bad_char(ch) => {
                    self.fatal(start, ready, bad_char_message(ch), start + offset);
                    return;
                }
                ch => text.push(ch),
            }
        }
        let blank = text.bytes().all(is_blank);
        self.node(start, ready, Event::Text { text, blank });
        if reaches_end {
            self.at_end(end);
        }
    }

    fn reference(&self, reference: &str) -> Result<Option<char>, Expansion> {
        if let Some(number) = reference.strip_prefix('#') {
            let value = match number.strip_prefix('x') {
                Some(hex) if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
                    u32::from_str_radix(hex, 16).ok()
                }
                None if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) => {
                    number.parse::<u32>().ok()
                }
                _ => {
                    return Err(Expansion::Fatal(
                        "xmlParseCharRef: invalid decimal value".into(),
                    ));
                }
            };
            let value = value.unwrap_or(u32::MAX);
            return match char::from_u32(value).filter(|ch| !is_bad_char(*ch)) {
                Some(ch) => Ok(Some(ch)),
                None => Err(Expansion::Fatal(format!(
                    "xmlParseCharRef: invalid xmlChar value {value}"
                ))),
            };
        }
        match reference {
            "lt" => Ok(Some('<')),
            "gt" => Ok(Some('>')),
            "amp" => Ok(Some('&')),
            "apos" => Ok(Some('\'')),
            "quot" => Ok(Some('"')),
            "" => Err(Expansion::Fatal("xmlParseEntityRef: no name".into())),
            name if !name.chars().next().is_some_and(is_name_start)
                || !name.chars().all(is_name_char) =>
            {
                Err(Expansion::Fatal("EntityRef: expecting ';'".into()))
            }
            // Compatibility: an entity declared in an internal subset is
            // dropped rather than substituted (RS-11).
            // Provenance: black-box compatibility test
            // (`value-internal-entity`).
            name if self.declared.contains(name) => Ok(None),
            name => Err(Expansion::Undefined(name.to_owned())),
        }
    }

    fn node(&mut self, start: usize, ready: Option<usize>, event: Event) {
        self.emit(start, ready, Item::Node(event));
    }
}

enum Expansion {
    Undefined(String),
    Fatal(String),
}

/// The offset of the `]` that closes an internal subset starting at `from`.
fn subset_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote = None;
    let mut at = from;
    while at < bytes.len() {
        let byte = bytes[at];
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None if bytes[at..].starts_with(b"<!--") => {
                at = find(bytes, b"-->", at + 4)? + 2;
            }
            None if byte == b'"' || byte == b'\'' => quote = Some(byte),
            None if byte == b']' => return Some(at),
            None => {}
        }
        at += 1;
    }
    None
}

fn entity_names(subset: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut from = 0;
    while let Some(at) = find(subset, b"<!ENTITY", from) {
        let rest = trim_start(&subset[at + 8..]);
        let rest = rest.strip_prefix(b"%").map_or(rest, trim_start);
        let len = rest
            .iter()
            .position(|byte| is_blank(*byte) || *byte == b'>')
            .unwrap_or(rest.len());
        names.push(String::from_utf8_lossy(&rest[..len]).into_owned());
        from = at + 8;
    }
    names
}

// ---------------------------------------------------------------------------
// Reader.

#[derive(Debug)]
struct Slot {
    event: Event,
    /// Number of enclosing elements.
    depth: usize,
    /// Absolute index of the enclosing element's start event.
    parent: Option<usize>,
    /// For start and end events, the absolute index of the start event.
    element: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// More input may still arrive.
    Live,
    /// All input arrived without a fatal error.
    Finished,
    /// The input ended with a fatal error. Nodes already built are still
    /// handed out; `stop_at_end` makes the first end event fail instead.
    Failed { stop_at_end: bool },
    /// Every further read fails.
    Dead,
}

struct OpenElement {
    index: usize,
    has_children: bool,
    name: String,
}

/// What the reader knows about the event it delivered last.
#[derive(Clone, Debug)]
struct Position {
    event: Event,
    parent: Option<usize>,
    element: Option<usize>,
}

/// Where the reader writes the XML library's own diagnostics.
pub(crate) struct Diagnostics<'w> {
    pub(crate) out: &'w mut dyn Write,
    /// The file name the diagnostics carry; `None` silences them.
    pub(crate) name: Option<String>,
}

pub(crate) struct Reader<'a, 'w> {
    stream: &'a [u8],
    tokens: Tokenizer<'a>,
    pending: Option<Construct>,
    feed: Feed,
    delivered: usize,
    terminated: bool,
    events: VecDeque<Slot>,
    /// Absolute index of `events[0]`.
    base: usize,
    cursor: usize,
    /// Absolute index of the last node event.
    last_node: Option<usize>,
    /// Elements the parser has opened and not closed.
    open: Vec<OpenElement>,
    state: State,
    failure: Option<String>,
    last: Option<Position>,
    /// Stream offset the parser has consumed up to.
    position: usize,
    line_pos: usize,
    line_no: u64,
    /// Where diagnostics last counted lines up to, and the line there.
    diagnostic_line: (usize, usize),
    diagnostics: Diagnostics<'w>,
}

impl<'a, 'w> Reader<'a, 'w> {
    pub(crate) fn new(input: &'a Prepared<'_>, diagnostics: Diagnostics<'w>) -> Self {
        let stream: &[u8] = &input.stream;
        let decoded = decode(stream, input.delivery);
        let feed = match input.delivery {
            Delivery::File => Feed::File {
                len: stream.len(),
                buffered: stream.len().min(PROBE),
                handed: stream.len().min(PROBE),
                eof: false,
                done: false,
            },
            Delivery::Stdin => Feed::Lines {
                ends: input.line_ends.clone(),
                next: 0,
                len: stream.len(),
                done: false,
            },
        };
        Self {
            stream,
            tokens: Tokenizer::new(decoded),
            pending: None,
            feed,
            delivered: 0,
            terminated: false,
            events: VecDeque::new(),
            base: 0,
            cursor: 0,
            last_node: None,
            open: Vec::new(),
            state: State::Live,
            failure: None,
            last: None,
            position: 0,
            line_pos: 0,
            line_no: 1,
            diagnostic_line: (0, 1),
            diagnostics,
        }
    }

    /// The library message of the fatal error, once one was reported.
    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The event the reader is positioned on.
    pub(crate) fn last(&self) -> Option<&Event> {
        self.last.as_ref().map(|position| &position.event)
    }

    /// Whether every further read fails (the reference would retry forever).
    pub(crate) fn is_dead(&self) -> bool {
        self.state == State::Dead
    }

    /// The parser's current line: one more than the line feeds it consumed.
    pub(crate) fn line(&mut self) -> u64 {
        let end = self.position.min(self.stream.len());
        if end > self.line_pos {
            self.line_no += self.stream[self.line_pos..end]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count() as u64;
            self.line_pos = end;
        }
        self.line_no
    }

    pub(crate) fn read(&mut self) -> Read {
        match self.state {
            State::Dead => Read::Failed,
            State::Live => self.read_live(),
            State::Finished => match self.slot(self.cursor) {
                Some(_) => self.deliver(),
                None => Read::Eof,
            },
            State::Failed { stop_at_end } => match self.slot(self.cursor) {
                // Compatibility: after a syntax error in a construct the
                // reference still hands out the nodes it built, but cannot
                // leave an element; after a document that merely ended
                // early it also ends the open elements. Provenance:
                // black-box compatibility test (truncations of `seed-mini`
                // ending in `\n<`, `\n</d`, `\n</ds`, `<!--`, `<na` and
                // `<xff>5.0000000000e-01</`).
                Some(slot) if stop_at_end && !slot.event.is_node() => {
                    self.state = State::Dead;
                    Read::Failed
                }
                // Text that may still grow needs input the parser can no
                // longer supply.
                Some(slot)
                    if stop_at_end
                        && self.text_may_grow(self.cursor)
                        && self.last_node.is_none_or(|last| last <= self.cursor)
                        && self.open.len() >= slot.depth =>
                {
                    let _ = self.deliver();
                    Read::Failed
                }
                Some(_) => self.deliver(),
                None if stop_at_end => {
                    self.state = State::Dead;
                    Read::Failed
                }
                None => Read::Eof,
            },
        }
    }

    fn read_live(&mut self) -> Read {
        let depth_at_start = self.open.len();
        // Compatibility: the reader asks for more input only while the
        // parser could still add a sibling after its position or a child
        // to it. Provenance: black-box compatibility test (truncations of
        // `seed-mini`, `wide-mismatch-late@stdin`, `two-roots@stdin`).
        while self.needs_input(depth_at_start) {
            if self.push().is_err() {
                return Read::Failed;
            }
        }
        loop {
            match self.state {
                State::Live => {}
                _ => return self.read(),
            }
            if self.slot(self.cursor).is_some() {
                break;
            }
            if self.push().is_err() {
                return Read::Failed;
            }
        }
        if self.text_may_grow(self.cursor) {
            // Text is handed out only once something follows it, the
            // parser has left its parent, or the input ended.
            let depth = self.slot(self.cursor).map_or(0, |slot| slot.depth);
            while self.state == State::Live
                && self.last_node.is_none_or(|last| last <= self.cursor)
                && self.open.len() >= depth
            {
                if self.push().is_err() {
                    let _ = self.deliver();
                    return Read::Failed;
                }
            }
        }
        self.deliver()
    }

    /// Whether the reader must see more input before it can move on from
    /// its current position.
    fn needs_input(&self, depth_at_start: usize) -> bool {
        if self.state != State::Live || self.open.len() != depth_at_start {
            return false;
        }
        let Some(last) = &self.last else {
            return self.slot(self.cursor).is_none();
        };
        let at = self.cursor;
        let open_element = match &last.event {
            Event::Start { empty: false, .. } => last.element,
            _ => None,
        };
        let next_sibling_at = match open_element {
            None => Some(at),
            Some(element) => match self.slot(at).map(|slot| &slot.event) {
                None => None,
                Some(Event::End { .. }) => Some(at + 1),
                Some(Event::Text { .. }) => match self.slot(at + 1) {
                    None => None,
                    Some(slot) if slot.element == Some(element) => Some(at + 2),
                    Some(_) => return false,
                },
                Some(_) => return false,
            },
        };
        if next_sibling_at
            .and_then(|index| self.slot(index))
            .is_some_and(|slot| slot.event.is_node())
        {
            return false;
        }
        match self.open.last() {
            None => true,
            Some(current) => {
                Some(current.index) == last.element || Some(current.index) == last.parent
            }
        }
    }

    /// Whether the event at `index` is text with nothing known after it.
    fn text_may_grow(&self, index: usize) -> bool {
        let is_text = self
            .slot(index)
            .is_some_and(|slot| matches!(slot.event, Event::Text { .. } | Event::CData));
        is_text
            && self
                .slot(index + 1)
                .is_none_or(|slot| !slot.event.is_node())
    }

    fn slot(&self, index: usize) -> Option<&Slot> {
        self.events.get(index.checked_sub(self.base)?)
    }

    fn deliver(&mut self) -> Read {
        let Some(slot) = self.slot(self.cursor) else {
            return Read::Eof;
        };
        let event = slot.event.clone();
        self.last = Some(Position {
            event: event.clone(),
            parent: slot.parent,
            element: slot.element,
        });
        self.cursor += 1;
        // The reader never looks behind its position again.
        while self.base < self.cursor && !self.events.is_empty() {
            self.events.pop_front();
            self.base += 1;
        }
        Read::Event(event)
    }

    fn push_event(&mut self, event: Event, element: Option<usize>) {
        let index = self.base + self.events.len();
        if event.is_node() {
            self.last_node = Some(index);
        }
        // Callers pop a closed element before adding its end event, so the
        // end event sits outside it, at its depth.
        let depth = self.open.len();
        let parent = self.open.last().map(|open| open.index);
        self.events.push_back(Slot {
            event,
            depth,
            parent,
            element,
        });
    }

    /// Delivers the next piece of input and lets the parser handle every
    /// construct it completes.
    fn push(&mut self) -> Result<(), ()> {
        let Some((delivered, terminate)) = self.feed.next() else {
            self.state = State::Finished;
            return Ok(());
        };
        self.delivered = delivered;
        self.terminated |= terminate;
        loop {
            let construct = match self.pending.take() {
                Some(construct) => construct,
                None => match self.tokens.next() {
                    Some(construct) => construct,
                    None => break,
                },
            };
            let ready =
                self.terminated || construct.ready.is_some_and(|ready| ready <= self.delivered);
            if !ready {
                self.position = construct.start.min(self.delivered);
                self.pending = Some(construct);
                break;
            }
            // A syntax error stops the parser where it found it; a leftover
            // it never parses leaves it where the leftover begins.
            self.position = match (&construct.item, construct.ready) {
                (
                    Item::Fatal {
                        document_end: true, ..
                    },
                    _,
                ) => construct.start,
                (Item::Fatal { at, .. }, _) => *at,
                (_, Some(ready)) => ready,
                (_, None) => self.stream.len(),
            };
            if let Err((message, document_end)) = self.apply(construct) {
                self.fail(message, terminate, document_end);
                return Err(());
            }
        }
        if self.pending.is_none() {
            self.position = self.delivered;
        }
        if self.terminated {
            self.position = self.stream.len();
            self.state = State::Finished;
        }
        Ok(())
    }

    fn apply(&mut self, construct: Construct) -> Result<(), (String, bool)> {
        match construct.item {
            Item::Node(event) => {
                if let Some(parent) = self.open.last_mut() {
                    parent.has_children = true;
                }
                let index = self.base + self.events.len();
                let element = matches!(event, Event::Start { .. }).then_some(index);
                let opens = match &event {
                    Event::Start { name, empty: false } => Some(name.clone()),
                    _ => None,
                };
                self.push_event(event, element);
                if let Some(name) = opens {
                    self.open.push(OpenElement {
                        index,
                        has_children: false,
                        name,
                    });
                }
            }
            Item::Close(name) => {
                let element = self.open.pop().map(|open| open.index);
                self.push_event(Event::End { name }, element);
            }
            Item::BrokenClose => {
                let element = self.open.last().map(|open| open.index);
                self.push_event(
                    Event::End {
                        name: String::new(),
                    },
                    element,
                );
            }
            Item::Note {
                message,
                at,
                warning,
            } => self.diagnose(&message, at, warning),
            Item::Fatal {
                message,
                at,
                document_end,
            } => {
                self.diagnose(&message, at, false);
                return Err((message, document_end));
            }
        }
        Ok(())
    }

    fn fail(&mut self, message: String, terminate: bool, document_end: bool) {
        self.failure = Some(message);
        // Compatibility: only the end of the input inside the document lets
        // the reader end the open elements; after any other syntax error it
        // fails at the first end event, so a read that would leave an
        // element retries forever (RS-15a). Provenance: black-box
        // compatibility test (`walk-spin-after-comment@stdin`,
        // `wide-mismatch-late`).
        let document_end = document_end && terminate;
        if document_end {
            // End events for the elements the parser left open, innermost
            // first; an element that never got a child has none.
            while let Some(element) = self.open.pop() {
                if element.has_children {
                    let index = element.index;
                    self.push_event(Event::End { name: element.name }, Some(index));
                }
            }
        }
        self.state = State::Failed {
            stop_at_end: !document_end,
        };
    }

    /// Prints a diagnostic the way the XML library does for a named file:
    /// location, message, the offending line and a caret.
    fn diagnose(&mut self, message: &str, at: usize, warning: bool) {
        let Some(name) = self.diagnostics.name.clone() else {
            return;
        };
        let at = at.min(self.stream.len());
        if at < self.diagnostic_line.0 {
            self.diagnostic_line = (0, 1);
        }
        let (counted, line) = self.diagnostic_line;
        let line = line
            + self.stream[counted..at]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
        self.diagnostic_line = (at, line);
        // The context window: back to the start of the line or 80 bytes,
        // forward to the end of the line, 80 bytes or a NUL, with the
        // caret under the error column but never past 79 columns or the
        // printed text. A position on a line break belongs to the line it
        // ends.
        let byte = |index: usize| self.stream.get(index).copied().unwrap_or(0);
        let is_break = |value: u8| value == b'\n' || value == b'\r';
        let mut from = at;
        if from > 0 && is_break(byte(from)) {
            from -= 1;
        }
        let mut steps = 0;
        while steps < 80 && from > 0 && !is_break(byte(from)) {
            from -= 1;
            steps += 1;
        }
        if is_break(byte(from)) {
            from += 1;
        }
        let context: Vec<u8> = self.stream[from.min(self.stream.len())..]
            .iter()
            .copied()
            .take_while(|value| *value != 0 && !is_break(*value))
            .take(80)
            .collect();
        let column = at.saturating_sub(from).min(79).min(context.len());
        let caret: Vec<u8> = context[..column]
            .iter()
            .map(|value| if *value == b'\t' { b'\t' } else { b' ' })
            .collect();
        let kind = if warning { "warning" } else { "error" };
        let mut text = format!("{name}:{line}: parser {kind} : {message}\n").into_bytes();
        text.extend_from_slice(&context);
        text.push(b'\n');
        text.extend_from_slice(&caret);
        text.extend_from_slice(b"^\n");
        // Nothing useful can be done when stderr cannot be written.
        let _ = self.diagnostics.out.write_all(&text);
    }
}

/// The dump after the input source's own preparation.
pub(crate) struct Prepared<'a> {
    delivery: Delivery,
    stream: Cow<'a, [u8]>,
    line_ends: Vec<usize>,
}

/// Prepares raw input for reading: standard input is cut into lines.
pub(crate) fn prepare(input: &[u8], delivery: Delivery) -> Prepared<'_> {
    match delivery {
        Delivery::File => Prepared {
            delivery,
            stream: Cow::Borrowed(input),
            line_ends: Vec::new(),
        },
        Delivery::Stdin => {
            let (stream, line_ends) = stdin_lines(input);
            Prepared {
                delivery,
                stream: Cow::Owned(stream),
                line_ends,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(xml: &[u8], delivery: Delivery) -> (Vec<Read>, Option<String>) {
        let prepared = prepare(xml, delivery);
        let mut sink = std::io::sink();
        let mut reader = Reader::new(
            &prepared,
            Diagnostics {
                out: &mut sink,
                name: None,
            },
        );
        let mut seen = Vec::new();
        for _ in 0..64 {
            let read = reader.read();
            let stop = read == Read::Eof || reader.is_dead();
            seen.push(read);
            if stop {
                break;
            }
        }
        (seen, reader.failure().map(str::to_owned))
    }

    fn start(name: &str) -> Read {
        Read::Event(Event::Start {
            name: name.into(),
            empty: false,
        })
    }

    fn end(name: &str) -> Read {
        Read::Event(Event::End { name: name.into() })
    }

    #[test]
    fn well_formed_document_streams_every_node() {
        let (seen, failure) = events(b"<a><b>1 &amp; 2</b><!-- c --></a>\n", Delivery::File);
        assert_eq!(
            seen,
            vec![
                start("a"),
                start("b"),
                Read::Event(Event::Text {
                    text: "1 & 2".into(),
                    blank: false
                }),
                end("b"),
                Read::Event(Event::Comment),
                end("a"),
                Read::Eof
            ]
        );
        assert_eq!(failure, None);
    }

    #[test]
    fn self_closing_tags_have_no_end_event() {
        let (seen, _) = events(b"<a><b/></a>", Delivery::Stdin);
        assert_eq!(
            seen,
            vec![
                start("a"),
                Read::Event(Event::Start {
                    name: "b".into(),
                    empty: true
                }),
                end("a"),
                Read::Eof
            ]
        );
    }

    #[test]
    fn mismatched_end_tag_is_reported_with_its_line() {
        let (seen, failure) = events(b"<a><b>\n</c></a>", Delivery::File);
        assert_eq!(seen[0], Read::Failed);
        assert_eq!(
            failure.as_deref(),
            Some("Opening and ending tag mismatch: b line 1 and c")
        );
    }

    #[test]
    fn truncated_start_tag_still_creates_the_element_at_the_end() {
        let (seen, failure) = events(b"<a>\n<b>x</b>\n<na", Delivery::Stdin);
        assert!(seen.contains(&start("na")), "{seen:?}");
        assert_eq!(
            failure.as_deref(),
            Some("Couldn't find end of Start Tag na")
        );
    }

    #[test]
    fn stdin_lines_stop_at_nul_and_ctrl_z() {
        let (stream, ends) = stdin_lines(b"ab\0cd\nef\ngh\x1aij\nkl\n");
        assert_eq!(stream, b"abef\ngh");
        assert_eq!(ends, vec![2, 5, 7]);
    }

    #[test]
    fn file_blocks_follow_the_probe_and_read_sizes() {
        let mut feed = Feed::File {
            len: 700,
            buffered: 4,
            handed: 4,
            eof: false,
            done: false,
        };
        assert_eq!(feed.next(), Some((516, false)));
        assert_eq!(feed.next(), Some((700, true)));
        assert_eq!(feed.next(), None);
        let mut small = Feed::File {
            len: 179,
            buffered: 4,
            handed: 4,
            eof: false,
            done: false,
        };
        assert_eq!(small.next(), Some((179, false)));
        assert_eq!(small.next(), Some((179, true)));
    }

    #[test]
    fn latin1_names_are_converted_to_utf8() {
        let (seen, _) = events(
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><a>\xe9t\xe9</a>",
            Delivery::File,
        );
        assert!(seen.contains(&Read::Event(Event::Text {
            text: "\u{e9}t\u{e9}".into(),
            blank: false
        })));
    }

    #[test]
    fn unknown_encoding_and_bad_utf8_fail() {
        let (_, failure) = events(
            b"<?xml version=\"1.0\" encoding=\"no-such-charset\"?><a/>",
            Delivery::File,
        );
        assert_eq!(
            failure.as_deref(),
            Some("Unsupported encoding no-such-charset")
        );
        let (_, failure) = events(b"<a>\xff</a>", Delivery::File);
        assert!(failure.unwrap().starts_with("Input is not proper UTF-8"));
    }

    #[test]
    fn undefined_entities_are_dropped_without_failing() {
        let (seen, failure) = events(b"<a>&nope;60</a>", Delivery::File);
        assert_eq!(failure, None);
        assert!(seen.contains(&Read::Event(Event::Text {
            text: "60".into(),
            blank: false
        })));
    }

    #[test]
    fn content_after_the_root_fails() {
        let (_, failure) = events(b"<a></a>\n<b/>", Delivery::Stdin);
        assert_eq!(failure.as_deref(), Some(EXTRA_CONTENT));
        let (seen, failure) = events(b"<a></a>\n<!-- c -->\n", Delivery::Stdin);
        assert_eq!(failure, None);
        assert_eq!(seen.last(), Some(&Read::Eof));
    }
}
