//! A model of the libxml2 2.9 `xmlTextReader` that rrd_restore.c pulls its
//! nodes from: the reader feeding a push parser (xmlreader.c
//! `xmlTextReaderPushData`, `xmlTextReaderRead`, `xmlTextReaderDoExpand`)
//! and the parts of the push parser (parser.c `xmlParseChunk`,
//! `xmlParseTryOrFinish`) a dump exercises.
//!
//! RRDtool reports `xmlTextReaderGetParserLineNumber`, the line the parser
//! has consumed to, and keeps reading after the reader fails, so both depend
//! on exactly when each chunk reaches the parser. A named file is read 4
//! bytes and then 512-byte chunks at a time; stdin goes through
//! rrd_restore.c's fgets callback, one line per read. Parse errors are raised
//! with libxml2's messages, and for a named file printed the way its default
//! handler prints them. The DTD internal subset is skipped rather than
//! parsed, only UTF-8 input is decoded, and namespace checks are not made.

const CHUNK_SIZE: usize = 512;
const BIG_BUFFER_SIZE: usize = 300;
/// Context lines show at most this many bytes (xmlParserPrintFileContext).
const CONTEXT_LEN: usize = 80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NodeType {
    None,
    Element,
    EndElement,
    Text,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// `xmlNewTextReaderFilename`; parse errors go to stderr.
    File,
    /// `xmlReaderForIO` over rrd_restore.c's fgets callback, with errors
    /// silenced by `ignoringErrorFunc`.
    Stdin,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Document,
    Dtd,
    Element,
    Text,
    Cdata,
    Comment,
    Pi,
    EntityRef,
}

struct Node {
    kind: Kind,
    /// NODE_IS_EMPTY: written as `<name/>`.
    empty: bool,
    name: String,
    content: Vec<u8>,
    parent: Option<usize>,
    first: Option<usize>,
    last: Option<usize>,
    next: Option<usize>,
    prev: Option<usize>,
}

/// `xmlTextReaderState`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    None,
    Start,
    Element,
    End,
    Backtrack,
    Done,
    Error,
}

/// `xmlTextReaderMode`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Initial,
    Interactive,
    Error,
    Eof,
}

/// `xmlParserInputState`, the states the push parser passes through here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Inst {
    Eof,
    Start,
    Misc,
    Prolog,
    StartTag,
    Content,
    Cdata,
    EndTag,
    Dtd,
    Epilog,
}

/// `ctxt->progressive`: zero, one, or the state an incomplete construct
/// is waiting in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Progressive {
    Off,
    On,
    Pi,
    Dtd,
    Comment,
    StartTag,
}

pub(crate) struct XmlReader<'a> {
    data: &'a [u8],
    source: Source,
    file_name: String,
    /// libxml2's stderr output for a named file.
    pub(crate) diagnostics: Vec<u8>,
    // Input buffer: what the read callback has delivered.
    taken: usize,
    input_done: bool,
    buf: Vec<u8>,
    /// `reader->cur`: bytes handed to the parser.
    pushed: usize,
    // Push parser.
    /// End of the parser's buffer; a trailing CR is held back while parsing.
    pend: usize,
    cur: usize,
    line: usize,
    inst: Inst,
    progressive: Progressive,
    charset: bool,
    doc_started: bool,
    node_tab: Vec<usize>,
    /// Open element names, the line each started on and how many namespace
    /// prefixes it bound (nameTab/pushTab).
    name_tab: Vec<(String, usize, usize)>,
    /// Bound namespace prefixes, innermost last ("" for the default).
    ns_tab: Vec<String>,
    unsupported_encoding: bool,
    well_formed: bool,
    err_no: bool,
    disable_sax: bool,
    external_subset: bool,
    last_error: Option<String>,
    // Tree.
    nodes: Vec<Node>,
    free: Vec<usize>,
    // Reader.
    node: Option<usize>,
    state: State,
    mode: Mode,
    depth: isize,
}

const DOC: usize = 0;

fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || matches!(byte, b'_' | b':') || byte >= 0x80
}

fn is_name_char(byte: u8) -> bool {
    is_name_start(byte) || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
}

fn is_xml_char(value: u32) -> bool {
    matches!(value, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x1_0000..=0x10_ffff)
}

impl<'a> XmlReader<'a> {
    /// `xmlNewTextReaderFilename` or `xmlReaderForIO`: the input buffer
    /// starts with a 4-byte read, which seeds the push parser when complete.
    pub(crate) fn new(data: &'a [u8], source: Source, file_name: &str) -> Self {
        let mut reader = Self {
            data,
            source,
            file_name: file_name.to_owned(),
            diagnostics: Vec::new(),
            taken: 0,
            input_done: false,
            buf: Vec::with_capacity(data.len().min(1 << 20)),
            pushed: 0,
            pend: 0,
            cur: 0,
            line: 1,
            inst: Inst::Start,
            progressive: Progressive::Off,
            charset: false,
            doc_started: false,
            node_tab: Vec::new(),
            name_tab: Vec::new(),
            ns_tab: Vec::new(),
            unsupported_encoding: false,
            well_formed: true,
            err_no: false,
            disable_sax: false,
            external_subset: false,
            last_error: None,
            nodes: vec![Node {
                kind: Kind::Document,
                empty: false,
                name: String::new(),
                content: Vec::new(),
                parent: None,
                first: None,
                last: None,
                next: None,
                prev: None,
            }],
            free: Vec::new(),
            node: None,
            state: State::Start,
            mode: Mode::Initial,
            depth: 0,
        };
        reader.read_input(4);
        if reader.buf.len() >= 4 {
            reader.pushed = 4;
            reader.pend = 4;
        }
        reader
    }

    /// `xmlParserInputBufferGrow` through the read callback.
    fn read_input(&mut self, len: usize) -> usize {
        if self.input_done {
            return 0;
        }
        let rest = &self.data[self.taken..];
        let added = match self.source {
            Source::File => {
                let count = len.min(rest.len());
                self.buf.extend_from_slice(&rest[..count]);
                self.taken += count;
                count
            }
            Source::Stdin => {
                // fgets(buffer, len) then strlen: up to len - 1 bytes or
                // through a newline, cut at a NUL byte.
                let limit = (len - 1).min(rest.len());
                let line = rest[..limit]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(limit, |newline| newline + 1);
                let kept = rest[..line]
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(line);
                self.buf.extend_from_slice(&rest[..kept]);
                self.taken += line;
                kept
            }
        };
        if added == 0 {
            self.input_done = true;
        }
        added
    }

    // ---- Reader -------------------------------------------------------

    /// `xmlTextReaderGetParserLineNumber`.
    pub(crate) fn line(&self) -> usize {
        self.line
    }

    /// `xmlGetLastError()->message` as rrd_restore.c's get_xml_element
    /// cleans it: newlines become spaces and the text stops at the first
    /// later non-printable byte.
    pub(crate) fn last_error_text(&self) -> String {
        let Some(message) = &self.last_error else {
            return "?".into();
        };
        let mut bytes: Vec<u8> = message
            .bytes()
            .map(|byte| if byte == b'\n' { b' ' } else { byte })
            .collect();
        if let Some(cut) = bytes
            .iter()
            .skip(1)
            .position(|byte| !(0x20..0x7f).contains(byte))
        {
            bytes.truncate(cut + 1);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// `xmlTextReaderNodeType`, collapsed to what rrd_restore.c tests.
    pub(crate) fn node_type(&self) -> NodeType {
        let Some(node) = self.node else {
            return NodeType::None;
        };
        let node = &self.nodes[node];
        match node.kind {
            Kind::Element if matches!(self.state, State::End | State::Backtrack) => {
                NodeType::EndElement
            }
            Kind::Element => NodeType::Element,
            Kind::Text if node.content.iter().all(|byte| is_blank(*byte)) => NodeType::Other,
            Kind::Text => NodeType::Text,
            _ => NodeType::Other,
        }
    }

    /// `xmlTextReaderName` for an element.
    pub(crate) fn name(&self) -> &str {
        self.node.map_or("", |node| &self.nodes[node].name)
    }

    /// `xmlTextReaderValue` for a text node.
    pub(crate) fn value(&self) -> String {
        self.node.map_or_else(String::new, |node| {
            String::from_utf8_lossy(&self.nodes[node].content).into_owned()
        })
    }

    fn successor(&self, node: usize) -> Option<usize> {
        let mut current = Some(node);
        while let Some(index) = current {
            if let Some(next) = self.nodes[index].next {
                return Some(next);
            }
            current = self.nodes[index].parent;
        }
        None
    }

    /// `xmlTextReaderDoExpand`.
    fn do_expand(&mut self) -> i32 {
        let Some(node) = self.node else {
            return -1;
        };
        loop {
            if self.inst == Inst::Eof
                || self.successor(node).is_some()
                || (self.node_tab.len() as isize) < self.depth
                || self.mode == Mode::Eof
            {
                return 1;
            }
            if self.push_data() < 0 {
                self.mode = Mode::Error;
                return -1;
            }
            if self.mode == Mode::Eof {
                return 1;
            }
        }
    }

    /// `xmlTextReaderRead`: 1 with a node, 0 at the end, -1 on error.
    pub(crate) fn read(&mut self) -> i32 {
        if self.mode == Mode::Initial {
            self.mode = Mode::Interactive;
            loop {
                if self.push_data() < 0 {
                    self.mode = Mode::Error;
                    self.state = State::Error;
                    return -1;
                }
                if !(self.node_tab.is_empty()
                    && self.mode != Mode::Eof
                    && self.state != State::Done)
                {
                    break;
                }
            }
            let first = if self.doc_started {
                self.nodes[DOC].first
            } else {
                None
            };
            self.node = first.or_else(|| self.node_tab.first().copied());
            if self.node.is_none() {
                self.mode = Mode::Error;
                self.state = State::Error;
                return -1;
            }
            self.state = State::Element;
            self.depth = 0;
            return self.node_found();
        }
        let old_state = self.state;
        let old_depth = self.node_tab.len();
        let old_node = self.node;
        let Some(node) = self.node else {
            return if self.mode == Mode::Eof { 0 } else { -1 };
        };
        while self.nodes[node].next.is_none()
            && self.node_tab.len() == old_depth
            && (old_state == State::Backtrack
                || self.nodes[node].first.is_none()
                || self.nodes[node].kind == Kind::EntityRef
                || self.nodes[node].first.is_some_and(|child| {
                    self.nodes[child].kind == Kind::Text && self.nodes[child].next.is_none()
                })
                || matches!(self.nodes[node].kind, Kind::Dtd | Kind::Document))
            && self
                .node_tab
                .last()
                .is_none_or(|open| *open == node || Some(*open) == self.nodes[node].parent)
            && self.inst != Inst::Eof
        {
            if self.push_data() < 0 {
                self.mode = Mode::Error;
                self.state = State::Error;
                return -1;
            }
        }
        let kind = self.nodes[node].kind;
        let child = self.nodes[node].first;
        if let (true, Some(child)) = (
            old_state != State::Backtrack && !matches!(kind, Kind::EntityRef | Kind::Dtd),
            child,
        ) {
            self.node = Some(child);
            self.depth += 1;
            self.state = State::Element;
            return self.node_found();
        }
        let childless_open = old_state == State::Element
            && kind == Kind::Element
            && self.nodes[node].first.is_none()
            && !self.nodes[node].empty;
        if let Some(next) = self.nodes[node].next {
            if childless_open {
                self.state = State::End;
                return self.node_found();
            }
            self.node = Some(next);
            self.state = State::Element;
            if let Some(prev) = self.nodes[next]
                .prev
                .filter(|prev| self.nodes[*prev].kind != Kind::Dtd)
            {
                self.unlink(prev);
                self.free_tree(prev);
            }
            return self.node_found();
        }
        if childless_open {
            self.state = State::End;
            return self.node_found();
        }
        let parent = self.nodes[node].parent;
        match parent {
            None | Some(DOC) => {
                if self.mode != Mode::Eof {
                    let end = self.pend;
                    let failed = self.parse_chunk(end, true) != 0;
                    self.state = State::Done;
                    if failed {
                        self.node = parent;
                        return -1;
                    }
                }
                self.node = None;
                self.depth = -1;
                if let Some(old) = old_node.filter(|old| self.nodes[*old].kind != Kind::Dtd) {
                    self.unlink(old);
                    self.free_tree(old);
                }
                self.state = State::Done;
                0
            }
            Some(parent) => {
                self.node = Some(parent);
                if let Some(last) = self.nodes[parent].last {
                    self.unlink(last);
                    self.free_tree(last);
                }
                self.depth -= 1;
                self.state = State::Backtrack;
                self.node_found()
            }
        }
    }

    /// The `node_found` tail of xmlTextReaderRead: an unfinished trailing
    /// text or CDATA node is read to its end first.
    fn node_found(&mut self) -> i32 {
        let unfinished = self.node.is_some_and(|node| {
            self.nodes[node].next.is_none()
                && matches!(self.nodes[node].kind, Kind::Text | Kind::Cdata)
        });
        if unfinished && self.do_expand() < 0 {
            return -1;
        }
        1
    }

    /// `xmlTextReaderPushData`.
    fn push_data(&mut self) -> i32 {
        let old_state = self.state;
        self.state = State::None;
        while self.state == State::None {
            if self.buf.len() < self.pushed + CHUNK_SIZE {
                if self.mode == Mode::Eof {
                    break;
                }
                if self.read_input(4096) == 0 {
                    self.mode = Mode::Eof;
                    break;
                }
            }
            if self.buf.len() >= self.pushed + CHUNK_SIZE {
                let end = self.pushed + CHUNK_SIZE;
                let status = self.parse_chunk(end, false);
                self.pushed = end;
                if status != 0 {
                    self.well_formed = false;
                }
                if !self.well_formed {
                    break;
                }
            } else {
                let end = self.buf.len();
                let status = self.parse_chunk(end, false);
                self.pushed = end;
                if status != 0 {
                    self.well_formed = false;
                }
                break;
            }
        }
        if self.mode == Mode::Eof && self.state != State::Done {
            let end = self.buf.len();
            let status = self.parse_chunk(end, true);
            self.pushed = end;
            self.state = State::Done;
            if status != 0 {
                if !self.well_formed {
                    return -1;
                }
                self.well_formed = false;
            }
        }
        self.state = old_state;
        if !self.well_formed {
            self.mode = Mode::Eof;
            return -1;
        }
        0
    }

    // ---- Tree ---------------------------------------------------------

    fn new_node(&mut self, kind: Kind, name: String, content: Vec<u8>) -> usize {
        let node = Node {
            kind,
            empty: false,
            name,
            content,
            parent: None,
            first: None,
            last: None,
            next: None,
            prev: None,
        };
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = node;
                index
            }
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            }
        }
    }

    fn add_child(&mut self, parent: usize, child: usize) {
        self.nodes[child].parent = Some(parent);
        match self.nodes[parent].last {
            Some(last) => {
                self.nodes[last].next = Some(child);
                self.nodes[child].prev = Some(last);
            }
            None => self.nodes[parent].first = Some(child),
        }
        self.nodes[parent].last = Some(child);
    }

    fn unlink(&mut self, node: usize) {
        let (parent, prev, next) = {
            let entry = &self.nodes[node];
            (entry.parent, entry.prev, entry.next)
        };
        match prev {
            Some(prev) => self.nodes[prev].next = next,
            None => {
                if let Some(parent) = parent {
                    self.nodes[parent].first = next;
                }
            }
        }
        match next {
            Some(next) => self.nodes[next].prev = prev,
            None => {
                if let Some(parent) = parent {
                    self.nodes[parent].last = prev;
                }
            }
        }
        let entry = &mut self.nodes[node];
        entry.parent = None;
        entry.prev = None;
        entry.next = None;
    }

    /// `xmlTextReaderFreeNode`: the slot and its subtree become reusable.
    fn free_tree(&mut self, node: usize) {
        let mut pending = vec![node];
        while let Some(index) = pending.pop() {
            let mut child = self.nodes[index].first;
            while let Some(current) = child {
                child = self.nodes[current].next;
                pending.push(current);
            }
            let entry = &mut self.nodes[index];
            entry.content = Vec::new();
            entry.name = String::new();
            entry.first = None;
            entry.last = None;
            self.free.push(index);
        }
    }

    // ---- SAX2 callbacks (only while !disableSAX) -----------------------

    fn sax_parent(&self) -> usize {
        self.node_tab.last().copied().unwrap_or(DOC)
    }

    fn start_element(&mut self, name: String) {
        let parent = self.sax_parent();
        let node = self.new_node(Kind::Element, name, Vec::new());
        self.add_child(parent, node);
        self.node_tab.push(node);
        if self.at(self.cur) == b'/' && self.at(self.cur + 1) == b'>' {
            self.nodes[node].empty = true;
        }
        self.state = State::Element;
    }

    fn characters(&mut self, text: &[u8]) {
        let Some(&parent) = self.node_tab.last() else {
            return;
        };
        if let Some(last) = self.nodes[parent]
            .last
            .filter(|last| self.nodes[*last].kind == Kind::Text)
        {
            self.nodes[last].content.extend_from_slice(text);
            return;
        }
        let node = self.new_node(Kind::Text, String::new(), text.to_vec());
        self.add_child(parent, node);
    }

    fn cdata_block(&mut self, text: &[u8]) {
        let Some(&parent) = self.node_tab.last() else {
            return;
        };
        if let Some(last) = self.nodes[parent]
            .last
            .filter(|last| self.nodes[*last].kind == Kind::Cdata)
        {
            self.nodes[last].content.extend_from_slice(text);
            return;
        }
        let node = self.new_node(Kind::Cdata, String::new(), text.to_vec());
        self.add_child(parent, node);
    }

    fn add_leaf(&mut self, kind: Kind, name: String) {
        let parent = self.sax_parent();
        let node = self.new_node(kind, name, Vec::new());
        self.add_child(parent, node);
    }

    // ---- Errors -------------------------------------------------------

    /// `__xmlRaiseError` through the default handler, which prints
    /// `file:line: parser error : message` and two lines of context.
    fn raise(&mut self, domain: &str, level: &str, message: String) {
        if self.source == Source::File {
            let mut out = format!(
                "{}:{}: {domain} {level} : {message}",
                self.file_name, self.line
            )
            .into_bytes();
            if !message.ends_with('\n') {
                out.push(b'\n');
            }
            self.context(&mut out);
            self.diagnostics.extend_from_slice(&out);
        }
        self.last_error = Some(message);
    }

    /// `xmlParserPrintFileContextInternal`.
    fn context(&self, out: &mut Vec<u8>) {
        let mut start = self.cur;
        while start > 0 && matches!(self.at(start), b'\n' | b'\r') {
            start -= 1;
        }
        let mut count = 0;
        while count < CONTEXT_LEN && start > 0 && !matches!(self.at(start), b'\n' | b'\r') {
            count += 1;
            start -= 1;
        }
        if matches!(self.at(start), b'\n' | b'\r') {
            start += 1;
        }
        let column = self.cur.saturating_sub(start);
        let mut content = Vec::new();
        let mut position = start;
        while content.len() < CONTEXT_LEN && !matches!(self.at(position), 0 | b'\n' | b'\r') {
            content.push(self.at(position));
            position += 1;
        }
        out.extend_from_slice(&content);
        out.push(b'\n');
        let mut index = 0;
        while index < column && index < CONTEXT_LEN - 1 && index < content.len() {
            out.push(if content[index] == b'\t' { b'\t' } else { b' ' });
            index += 1;
        }
        out.extend_from_slice(b"^\n");
    }

    /// `xmlFatalErr` and its message variants.
    fn fatal(&mut self, message: impl Into<String>) {
        if self.disable_sax && self.inst == Inst::Eof {
            return;
        }
        self.err_no = true;
        self.raise("parser", "error", message.into());
        self.well_formed = false;
        self.disable_sax = true;
    }

    /// `xmlErrMsgStr`: an error that does not stop the parse.
    fn error(&mut self, message: String) {
        if self.disable_sax && self.inst == Inst::Eof {
            return;
        }
        self.err_no = true;
        self.raise("parser", "error", message);
    }

    /// `xmlNsErr`.
    fn ns_error(&mut self, message: String) {
        if self.disable_sax && self.inst == Inst::Eof {
            return;
        }
        self.err_no = true;
        self.raise("namespace", "error", message);
    }

    /// `xmlWarningMsg`.
    fn warning(&mut self, message: String) {
        if self.disable_sax && self.inst == Inst::Eof {
            return;
        }
        self.raise("parser", "warning", message);
    }

    /// `xmlHaltParser`.
    fn halt(&mut self) {
        self.inst = Inst::Eof;
        self.disable_sax = true;
    }

    // ---- Push parser --------------------------------------------------

    fn at(&self, index: usize) -> u8 {
        if index < self.pend {
            self.buf[index]
        } else {
            0
        }
    }

    fn advance(&mut self, count: usize) {
        let end = (self.cur + count).min(self.pend.max(self.cur));
        self.line += self.buf[self.cur..end]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        self.cur += count;
    }

    fn skip_blanks(&mut self) -> usize {
        let start = self.cur;
        while is_blank(self.at(self.cur)) {
            self.advance(1);
        }
        self.cur - start
    }

    /// `xmlParseLookupSequence`: the offset of `sequence` at or after
    /// `from` in the buffer.
    fn lookup(&self, from: usize, sequence: &[u8]) -> Option<usize> {
        if from >= self.pend {
            return None;
        }
        self.buf[from..self.pend]
            .windows(sequence.len())
            .position(|window| window == sequence)
            .map(|offset| from + offset)
    }

    /// `xmlParseGetLasts`.
    fn lasts(&self) -> (Option<usize>, Option<usize>) {
        if self.progressive == Progressive::Off {
            return (None, None);
        }
        let Some(lt) = self.buf[..self.pend].iter().rposition(|byte| *byte == b'<') else {
            return (None, None);
        };
        let mut position = lt + 1;
        while position < self.pend && self.buf[position] != b'>' {
            let byte = self.buf[position];
            position += 1;
            if matches!(byte, b'\'' | b'"') {
                while position < self.pend && self.buf[position] != byte {
                    position += 1;
                }
                if position < self.pend {
                    position += 1;
                }
            }
        }
        if position < self.pend {
            return (Some(lt), Some(position));
        }
        (
            Some(lt),
            self.buf[..lt].iter().rposition(|byte| *byte == b'>'),
        )
    }

    /// `xmlParseCheckTransition`.
    fn worth_parsing(&self, old: usize, new: usize) -> bool {
        let waits_for_gt = matches!(
            self.inst,
            Inst::StartTag | Inst::Cdata | Inst::EndTag | Inst::Dtd
        ) || matches!(
            self.progressive,
            Progressive::Comment | Progressive::Pi | Progressive::Dtd
        );
        !waits_for_gt || self.buf[old..new].contains(&b'>')
    }

    /// `xmlParseChunk` with the bytes up to `end`: 0, or nonzero once the
    /// parse has failed.
    fn parse_chunk(&mut self, end: usize, terminate: bool) -> i32 {
        if self.err_no && self.disable_sax {
            return 1;
        }
        if self.inst == Inst::Eof {
            return -1;
        }
        let mut held_cr = false;
        let mut end = end;
        if end > self.pend && !terminate && self.buf[end - 1] == b'\r' {
            held_cr = true;
            end -= 1;
        }
        let old = if end > self.pend { self.pend } else { 0 };
        self.pend = self.pend.max(end);
        if terminate || old == 0 || self.pend == 0 || self.worth_parsing(old, self.pend) {
            self.try_or_finish(terminate);
        }
        if self.inst == Inst::Eof {
            return i32::from(self.err_no);
        }
        if self.err_no && self.disable_sax {
            return 1;
        }
        if held_cr {
            self.pend += 1;
        }
        if terminate {
            let remaining = self.pend - self.cur.min(self.pend);
            if self.inst != Inst::Epilog || remaining > 0 {
                self.fatal("Extra content at the end of the document\n");
            }
            self.inst = Inst::Eof;
        }
        i32::from(!self.well_formed)
    }

    /// `xmlParseTryOrFinish`.
    fn try_or_finish(&mut self, terminate: bool) {
        let (mut last_lt, mut last_gt) = self.lasts();
        while self.inst != Inst::Eof {
            if self.err_no && self.disable_sax {
                return;
            }
            let avail = self.pend.saturating_sub(self.cur);
            if avail < 1 {
                return;
            }
            let (c, n) = (self.at(self.cur), self.at(self.cur + 1));
            match self.inst {
                Inst::Eof => return,
                Inst::Start => {
                    if !self.charset {
                        if avail < 4 {
                            return;
                        }
                        if self.buf[self.cur..].starts_with(b"\xef\xbb\xbf") {
                            self.cur += 3;
                        }
                        self.charset = true;
                        continue;
                    }
                    if avail < 2 {
                        return;
                    }
                    if c == 0 {
                        self.fatal("Document is empty\n");
                        self.halt();
                        return;
                    }
                    if c == b'<' && n == b'?' {
                        if avail < 5 {
                            return;
                        }
                        let end = self.lookup(self.cur, b"?>");
                        if !terminate && end.is_none() {
                            return;
                        }
                        if &self.buf[self.cur + 2..self.cur + 5] == b"xml"
                            && is_blank(self.at(self.cur + 5))
                        {
                            self.parse_xml_decl();
                            if self.unsupported_encoding {
                                self.halt();
                                return;
                            }
                        }
                    }
                    if !self.disable_sax {
                        self.doc_started = true;
                    }
                    self.inst = Inst::Misc;
                }
                Inst::Misc | Inst::Prolog | Inst::Epilog => {
                    self.skip_blanks();
                    let avail = self.pend.saturating_sub(self.cur);
                    if avail < 2 {
                        return;
                    }
                    let state = self.inst;
                    let (c, n) = (self.at(self.cur), self.at(self.cur + 1));
                    if c == b'<' && n == b'?' {
                        if !terminate && self.lookup(self.cur, b"?>").is_none() {
                            self.progressive = Progressive::Pi;
                            return;
                        }
                        self.parse_pi();
                        if self.inst == Inst::Eof {
                            return;
                        }
                        self.inst = state;
                        self.progressive = Progressive::On;
                    } else if c == b'<'
                        && n == b'!'
                        && self.at(self.cur + 2) == b'-'
                        && self.at(self.cur + 3) == b'-'
                    {
                        if !terminate && self.lookup(self.cur, b"-->").is_none() {
                            self.progressive = Progressive::Comment;
                            return;
                        }
                        self.parse_comment();
                        if self.inst == Inst::Eof {
                            return;
                        }
                        self.inst = state;
                        self.progressive = Progressive::On;
                    } else if state == Inst::Misc
                        && self.buf[self.cur..self.pend].starts_with(b"<!DOCTYPE")
                    {
                        if !terminate && self.lookup(self.cur, b">").is_none() {
                            self.progressive = Progressive::Dtd;
                            return;
                        }
                        self.progressive = Progressive::Off;
                        self.parse_doctype();
                        if self.inst == Inst::Eof {
                            return;
                        }
                        self.inst = if self.at(self.cur) == b'[' {
                            Inst::Dtd
                        } else {
                            Inst::Prolog
                        };
                    } else if c == b'<'
                        && n == b'!'
                        && avail < if state == Inst::Misc { 9 } else { 4 }
                    {
                        return;
                    } else if state == Inst::Epilog {
                        self.fatal("Extra content at the end of the document\n");
                        self.halt();
                        return;
                    } else {
                        self.inst = Inst::StartTag;
                        if state == Inst::Misc || self.progressive == Progressive::Off {
                            self.progressive = Progressive::StartTag;
                        }
                        (last_lt, last_gt) = self.lasts();
                    }
                }
                Inst::Dtd => {
                    let Some(end) = self.internal_subset_end() else {
                        return;
                    };
                    self.advance(end + 1 - self.cur);
                    self.inst = Inst::Prolog;
                }
                Inst::StartTag => {
                    if avail < 2 {
                        return;
                    }
                    if c != b'<' {
                        self.fatal("Document is empty\n");
                        self.halt();
                        return;
                    }
                    if !terminate && last_gt.is_none_or(|gt| self.cur >= gt) {
                        return;
                    }
                    let line = self.line;
                    let Some((name, bound)) = self.parse_start_tag() else {
                        self.halt();
                        return;
                    };
                    if self.at(self.cur) == b'/' && self.at(self.cur + 1) == b'>' {
                        self.cur += 2;
                        if !self.disable_sax {
                            self.node_tab.pop();
                        }
                        self.ns_tab.truncate(self.ns_tab.len() - bound);
                        self.inst = if self.name_tab.is_empty() {
                            Inst::Epilog
                        } else {
                            Inst::Content
                        };
                        self.progressive = Progressive::On;
                        continue;
                    }
                    if self.at(self.cur) == b'>' {
                        self.advance(1);
                    } else {
                        self.fatal(format!("Couldn't find end of Start Tag {name}\n"));
                        self.node_tab.pop();
                    }
                    self.name_tab.push((name, line, bound));
                    self.inst = Inst::Content;
                    self.progressive = Progressive::On;
                }
                Inst::Content => {
                    if avail < 2 {
                        return;
                    }
                    let before = self.cur;
                    if c == b'<' && n == b'/' {
                        self.inst = Inst::EndTag;
                        continue;
                    } else if c == b'<' && n == b'?' {
                        if !terminate && self.lookup(self.cur, b"?>").is_none() {
                            self.progressive = Progressive::Pi;
                            return;
                        }
                        self.parse_pi();
                        self.inst = Inst::Content;
                        self.progressive = Progressive::On;
                    } else if c == b'<' && n != b'!' {
                        self.inst = Inst::StartTag;
                        continue;
                    } else if c == b'<'
                        && n == b'!'
                        && self.at(self.cur + 2) == b'-'
                        && self.at(self.cur + 3) == b'-'
                    {
                        if avail < 4 {
                            return;
                        }
                        if !terminate && self.lookup(self.cur + 4, b"-->").is_none() {
                            self.progressive = Progressive::Comment;
                            return;
                        }
                        self.parse_comment();
                        self.inst = Inst::Content;
                        self.progressive = Progressive::On;
                    } else if self.buf[self.cur..self.pend].starts_with(b"<![CDATA[") {
                        self.cur += 9;
                        self.inst = Inst::Cdata;
                        continue;
                    } else if c == b'<' && n == b'!' && avail < 9 {
                        return;
                    } else if c == b'&' {
                        if !terminate && self.lookup(self.cur, b";").is_none() {
                            return;
                        }
                        self.parse_reference();
                    } else {
                        if avail < BIG_BUFFER_SIZE
                            && !terminate
                            && last_lt.is_none_or(|lt| self.cur > lt)
                        {
                            return;
                        }
                        self.parse_char_data();
                    }
                    if self.cur == before && self.inst != Inst::Eof {
                        self.fatal("internal error: detected an error in element content\n\n");
                        self.halt();
                    }
                }
                Inst::EndTag => {
                    if avail < 2 {
                        return;
                    }
                    if !terminate && last_gt.is_none_or(|gt| self.cur >= gt) {
                        return;
                    }
                    self.parse_end_tag();
                    if let Some((_, _, bound)) = self.name_tab.pop() {
                        self.ns_tab.truncate(self.ns_tab.len() - bound);
                    }
                    if self.inst != Inst::Eof {
                        self.inst = if self.name_tab.is_empty() {
                            Inst::Epilog
                        } else {
                            Inst::Content
                        };
                    }
                }
                Inst::Cdata => match self.lookup(self.cur, b"]]>") {
                    None => {
                        if avail >= BIG_BUFFER_SIZE + 2 {
                            let block = self.buf[self.cur..self.cur + BIG_BUFFER_SIZE].to_vec();
                            if !self.disable_sax {
                                self.cdata_block(&block);
                            }
                            self.advance(BIG_BUFFER_SIZE);
                        }
                        return;
                    }
                    Some(end) => {
                        let block = self.buf[self.cur..end].to_vec();
                        if !self.disable_sax
                            && (!block.is_empty() || self.buf[..self.cur].ends_with(b"<![CDATA["))
                        {
                            self.cdata_block(&block);
                        }
                        self.advance(end + 3 - self.cur);
                        self.inst = Inst::Content;
                    }
                },
            }
        }
    }

    /// The `]` S? `>` that ends a DOCTYPE internal subset, outside quotes
    /// and comments.
    fn internal_subset_end(&self) -> Option<usize> {
        let mut quote = None;
        let mut position = self.cur;
        while position < self.pend {
            let byte = self.buf[position];
            if let Some(open) = quote {
                if byte == open {
                    quote = None;
                }
            } else if byte == b'<' && self.buf[position..self.pend].starts_with(b"<!--") {
                position = self.lookup(position + 4, b"-->")? + 2;
            } else if matches!(byte, b'"' | b'\'') {
                quote = Some(byte);
            } else if byte == b']' {
                if self.at(position + 1) == b']' {
                    position += 1;
                } else {
                    let gt = position
                        + 1
                        + self.buf[position + 1..self.pend]
                            .iter()
                            .position(|byte| !is_blank(*byte))?;
                    if self.buf[gt] == b'>' {
                        return Some(gt);
                    }
                }
            }
            position += 1;
        }
        None
    }

    /// `xmlParseName` at the cursor.
    fn parse_name(&mut self) -> Option<String> {
        if !is_name_start(self.at(self.cur)) {
            return None;
        }
        self.take_while(is_name_char)
    }

    /// `xmlParseNCName`: a name without colons.
    fn parse_ncname(&mut self) -> Option<String> {
        let byte = self.at(self.cur);
        if byte == b':' || !is_name_start(byte) {
            return None;
        }
        self.take_while(|byte| byte != b':' && is_name_char(byte))
    }

    fn take_while(&mut self, accept: impl Fn(u8) -> bool) -> Option<String> {
        let start = self.cur;
        while self.cur < self.pend && accept(self.buf[self.cur]) {
            self.cur += 1;
        }
        (self.cur > start).then(|| String::from_utf8_lossy(&self.buf[start..self.cur]).into_owned())
    }

    /// `xmlParseQName`: the prefix, if one parsed, and the full name.
    fn parse_qname(&mut self) -> Option<(Option<String>, String)> {
        let Some(local) = self.parse_ncname() else {
            if self.at(self.cur) == b':' {
                let name = self.parse_name()?;
                self.ns_error(format!("Failed to parse QName '{name}'\n"));
                return Some((None, name));
            }
            return None;
        };
        if self.at(self.cur) != b':' {
            return Some((None, local));
        }
        self.cur += 1;
        let prefix = local;
        let Some(local) = self.parse_ncname() else {
            self.ns_error(format!("Failed to parse QName '{prefix}:'\n"));
            let rest = self.take_while(is_name_char).unwrap_or_default();
            return Some((None, format!("{prefix}:{rest}")));
        };
        if self.at(self.cur) == b':' {
            self.ns_error(format!("Failed to parse QName '{prefix}:{local}:'\n"));
            self.cur += 1;
            let rest = self.parse_name().unwrap_or_default();
            return Some((None, format!("{prefix}:{local}:{rest}")));
        }
        Some((Some(prefix.clone()), format!("{prefix}:{local}")))
    }

    fn bound(&self, prefix: &str) -> bool {
        prefix == "xml" || self.ns_tab.iter().any(|bound| bound == prefix)
    }

    /// `xmlParseStartTag2`: the element name and how many prefixes it
    /// bound, or None when it has no name.
    fn parse_start_tag(&mut self) -> Option<(String, usize)> {
        self.cur += 1;
        let Some((prefix, name)) = self.parse_qname() else {
            self.fatal("StartTag: invalid element name\n");
            return None;
        };
        self.skip_blanks();
        let mut attributes: Vec<(Option<String>, String)> = Vec::new();
        let mut bound = 0;
        loop {
            let byte = self.at(self.cur);
            if byte == b'>'
                || (byte == b'/' && self.at(self.cur + 1) == b'>')
                || !(byte >= 0x20 || matches!(byte, b'\t' | b'\n' | b'\r'))
                || self.inst == Inst::Eof
            {
                break;
            }
            if let Some((attribute_prefix, attribute)) = self.parse_attribute() {
                match attribute_prefix.as_deref() {
                    None if attribute == "xmlns" => {
                        self.ns_tab.push(String::new());
                        bound += 1;
                    }
                    Some("xmlns") => {
                        self.ns_tab.push(attribute["xmlns:".len()..].to_owned());
                        bound += 1;
                    }
                    _ => attributes.push((attribute_prefix, attribute)),
                }
            }
            let byte = self.at(self.cur);
            if byte == b'>' || (byte == b'/' && self.at(self.cur + 1) == b'>') {
                break;
            }
            if self.skip_blanks() == 0 {
                self.fatal("attributes construct error\n");
                break;
            }
        }
        let local = |name: &str| name.rsplit(':').next().unwrap_or(name).to_owned();
        for index in 0..attributes.len() {
            let (attribute_prefix, attribute) = attributes[index].clone();
            if let Some(attribute_prefix) = attribute_prefix.filter(|bound| !self.bound(bound)) {
                let message = format!(
                    "Namespace prefix {attribute_prefix} for {} on {} is not defined\n",
                    local(&attribute),
                    local(&name)
                );
                self.ns_error(message);
            }
            if attributes[..index]
                .iter()
                .any(|(_, earlier)| *earlier == attribute)
            {
                self.attribute_redefined(&attribute);
            }
        }
        if let Some(prefix) = prefix.filter(|bound| !self.bound(bound)) {
            let message = format!(
                "Namespace prefix {prefix} on {} is not defined\n",
                local(&name)
            );
            self.ns_error(message);
        }
        if !self.disable_sax {
            self.start_element(name.clone());
        }
        Some((name, bound))
    }

    /// `xmlErrAttributeDup`.
    fn attribute_redefined(&mut self, attribute: &str) {
        self.fatal(format!("Attribute {attribute} redefined\n"));
    }

    /// `xmlParseAttribute2`: the attribute's prefix and name, or None when
    /// it has no name or no value. The value is checked and dropped.
    fn parse_attribute(&mut self) -> Option<(Option<String>, String)> {
        let Some((prefix, name)) = self.parse_qname() else {
            self.fatal("error parsing attribute name\n");
            return None;
        };
        self.skip_blanks();
        if self.at(self.cur) != b'=' {
            self.fatal(format!(
                "Specification mandates value for attribute {}\n",
                name.rsplit(':').next().unwrap_or(&name)
            ));
            return None;
        }
        self.advance(1);
        self.skip_blanks();
        let quote = self.at(self.cur);
        if !matches!(quote, b'"' | b'\'') {
            self.fatal("AttValue: \" or ' expected\n");
            return None;
        }
        self.advance(1);
        loop {
            match self.at(self.cur) {
                0 => {
                    self.fatal("AttValue: ' expected\n");
                    return None;
                }
                b'<' => {
                    self.fatal("Unescaped '<' not allowed in attributes values\n");
                    return None;
                }
                byte if byte == quote => {
                    self.advance(1);
                    return Some((prefix, name));
                }
                _ => self.advance(1),
            }
        }
    }

    /// `xmlParseEndTag2`.
    fn parse_end_tag(&mut self) {
        self.cur += 2;
        let (expected, line, _) = self.name_tab.last().cloned().unwrap_or_default();
        let length = expected.len();
        let matched = if self.buf[self.cur..self.pend].starts_with(expected.as_bytes())
            && (self.at(self.cur + length) == b'>' || is_blank(self.at(self.cur + length)))
        {
            self.cur += length;
            None
        } else {
            match self.parse_name() {
                Some(name) if name == expected => None,
                Some(name) => Some(name),
                None => Some("unparsable".to_owned()),
            }
        };
        self.skip_blanks();
        if self.at(self.cur) == b'>' {
            self.advance(1);
        } else {
            self.fatal("expected '>'\n");
        }
        if let Some(name) = matched {
            self.fatal(format!(
                "Opening and ending tag mismatch: {expected} line {line} and {name}\n"
            ));
        }
        if !self.disable_sax {
            self.node_tab.pop();
        }
    }

    /// `xmlParseCharData`: text up to the next markup or the buffer end.
    /// A `]]>` is reported from the start of the run, after the lines up
    /// to it have been counted.
    fn parse_char_data(&mut self) {
        let start = self.cur;
        let mut end = start;
        while end < self.pend && !matches!(self.buf[end], b'<' | b'&') {
            let byte = self.buf[end];
            if byte == b']' && self.buf[end..self.pend].starts_with(b"]]>") {
                let newlines = self.buf[start..end]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count();
                self.line += newlines;
                self.fatal("Sequence ']]>' not allowed in content\n");
                self.cur = end + 1;
                return;
            }
            if byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r') {
                self.emit_text(start, end);
                self.advance(end - start);
                self.fatal(format!("PCDATA invalid Char value {byte}\n"));
                self.advance(1);
                return;
            }
            end += 1;
        }
        self.emit_text(start, end);
        self.advance(end - start);
    }

    fn emit_text(&mut self, start: usize, end: usize) {
        if end > start && !self.disable_sax {
            let text = self.buf[start..end].to_vec();
            self.characters(&text);
        }
    }

    /// `xmlParseReference`: character references and the predefined
    /// entities become text; anything else is undeclared.
    fn parse_reference(&mut self) {
        if self.at(self.cur + 1) == b'#' {
            let value = self.parse_char_ref();
            if let Some(character) = value.and_then(char::from_u32).filter(|_| !self.disable_sax) {
                let mut encoded = [0; 4];
                let text = character.encode_utf8(&mut encoded).as_bytes().to_vec();
                self.characters(&text);
            }
            return;
        }
        self.advance(1);
        let Some(name) = self.parse_name() else {
            self.fatal("xmlParseEntityRef: no name\n");
            return;
        };
        if self.at(self.cur) != b';' {
            self.fatal("EntityRef: expecting ';'\n");
            return;
        }
        self.advance(1);
        let predefined = match name.as_str() {
            "lt" => Some(b"<"),
            "gt" => Some(b">"),
            "amp" => Some(b"&"),
            "apos" => Some(b"'"),
            "quot" => Some(b"\""),
            _ => None,
        };
        match predefined {
            Some(text) => {
                if !self.disable_sax {
                    self.characters(text);
                }
            }
            None if self.external_subset => {
                self.error(format!("Entity '{name}' not defined\n"));
                if !self.disable_sax && !self.node_tab.is_empty() {
                    self.add_leaf(Kind::EntityRef, name);
                }
            }
            None => self.fatal(format!("Entity '{name}' not defined\n")),
        }
    }

    /// `xmlParseCharRef`: the referenced character, or None after an error.
    fn parse_char_ref(&mut self) -> Option<u32> {
        let hex = self.at(self.cur + 2) == b'x';
        self.cur += if hex { 3 } else { 2 };
        let radix = if hex { 16 } else { 10 };
        let mut value: u32 = 0;
        while self.at(self.cur) != b';' {
            match char::from(self.at(self.cur)).to_digit(radix) {
                Some(digit) => value = (value * radix + digit).min(0x11_0000),
                None => {
                    self.fatal(if hex {
                        "CharRef: invalid hexadecimal value\n"
                    } else {
                        "CharRef: invalid decimal value\n"
                    });
                    value = 0;
                    break;
                }
            }
            self.advance(1);
        }
        if self.at(self.cur) == b';' {
            self.cur += 1;
        }
        if value >= 0x11_0000 {
            self.fatal("xmlParseCharRef: character reference out of bounds\n");
            None
        } else if is_xml_char(value) {
            Some(value)
        } else {
            self.fatal(format!("xmlParseCharRef: invalid xmlChar value {value}\n"));
            None
        }
    }

    /// `xmlParseComment`, scanning the way its fast path does: a `--`
    /// inside is reported where it stands and the scan goes on, a control
    /// character ends it, and a comment cut off by the end of the input is
    /// reported at that end.
    fn parse_comment(&mut self) {
        let body = self.cur + 4;
        self.cur = body;
        loop {
            if self.cur >= self.pend {
                self.fatal("Comment not terminated\n");
                return;
            }
            let byte = self.buf[self.cur];
            if byte == b'-' && self.at(self.cur + 1) == b'-' {
                if self.at(self.cur + 2) == b'>' {
                    self.advance(3);
                    if !self.disable_sax {
                        self.add_leaf(Kind::Comment, String::new());
                    }
                    return;
                }
                let so_far =
                    String::from_utf8_lossy(&self.buf[body..self.cur.min(body + 50)]).into_owned();
                self.fatal(if so_far.is_empty() {
                    "Double hyphen within comment\n".to_owned()
                } else {
                    format!("Double hyphen within comment: <!--{so_far}\n")
                });
                if self.inst == Inst::Eof {
                    return;
                }
                self.advance(1);
            } else if byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r') {
                self.fatal(format!("xmlParseComment: invalid xmlChar value {byte}\n"));
                return;
            }
            self.advance(1);
        }
    }

    /// `xmlParsePI`.
    fn parse_pi(&mut self) {
        self.cur += 2;
        let Some(target) = self.parse_name() else {
            self.fatal("xmlParsePI : no target name\n");
            return;
        };
        if target == "xml" {
            self.fatal("XML declaration allowed only at the start of the document\n");
        } else if target.eq_ignore_ascii_case("xml") {
            self.fatal("Invalid PI name\n");
        }
        if !(self.at(self.cur) == b'?' && self.at(self.cur + 1) == b'>') && self.skip_blanks() == 0
        {
            self.fatal(format!("ParsePI: PI {target} space expected\n"));
        }
        loop {
            let byte = self.at(self.cur);
            if byte == b'?' && self.at(self.cur + 1) == b'>' {
                self.advance(2);
                if !self.disable_sax {
                    self.add_leaf(Kind::Pi, target);
                }
                return;
            }
            if self.cur >= self.pend || (byte < 0x20 && !matches!(byte, b'\t' | b'\n' | b'\r')) {
                self.fatal(format!("ParsePI: PI {target} never end ...\n"));
                return;
            }
            self.advance(1);
        }
    }

    /// `xmlParseXMLDecl`. The encoding is checked but not applied: the
    /// input is always read as UTF-8.
    fn parse_xml_decl(&mut self) {
        self.cur += 5;
        self.skip_blanks();
        match self.parse_version_info() {
            None => self.fatal("Malformed declaration expecting version\n"),
            Some(version) if version != "1.0" => {
                let message = format!("Unsupported version '{version}'\n");
                if version.starts_with("1.") {
                    self.warning(message);
                } else {
                    self.fatal(message);
                }
            }
            Some(_) => {}
        }
        if !is_blank(self.at(self.cur)) {
            if self.at(self.cur) == b'?' && self.at(self.cur + 1) == b'>' {
                self.cur += 2;
                return;
            }
            self.fatal("Blank needed here\n");
        }
        let encoding = self.parse_encoding_decl();
        if self.unsupported_encoding || self.inst == Inst::Eof {
            return;
        }
        if encoding.is_some() && !is_blank(self.at(self.cur)) {
            if self.at(self.cur) == b'?' && self.at(self.cur + 1) == b'>' {
                self.cur += 2;
                return;
            }
            self.fatal("Blank needed here\n");
        }
        self.skip_blanks();
        self.parse_standalone_decl();
        self.skip_blanks();
        if self.at(self.cur) == b'?' && self.at(self.cur + 1) == b'>' {
            self.cur += 2;
        } else if self.at(self.cur) == b'>' {
            self.fatal("parsing XML declaration: '?>' expected\n");
            self.advance(1);
        } else {
            self.fatal("parsing XML declaration: '?>' expected\n");
            // MOVETO_ENDTAG moves the cursor without counting lines.
            while !matches!(self.at(self.cur), 0 | b'>') {
                self.cur += 1;
            }
            self.advance(1);
        }
    }

    /// `name S? = S?` of a declaration attribute; false after an error.
    fn decl_attribute(&mut self, name: &[u8]) -> Option<bool> {
        self.skip_blanks();
        if !self.buf[self.cur..self.pend].starts_with(name) {
            return None;
        }
        self.cur += name.len();
        self.skip_blanks();
        if self.at(self.cur) != b'=' {
            self.fatal("expected '='\n");
            return Some(false);
        }
        self.advance(1);
        self.skip_blanks();
        Some(true)
    }

    /// A quoted declaration value read by `value`, with libxml2's quote
    /// errors. The version survives a missing closing quote; the encoding
    /// does not.
    fn decl_value(
        &mut self,
        keep_unclosed: bool,
        value: impl FnOnce(&mut Self) -> Option<String>,
    ) -> Option<String> {
        let quote = self.at(self.cur);
        if !matches!(quote, b'"' | b'\'') {
            self.fatal("String not started expecting ' or \"\n");
            return None;
        }
        self.advance(1);
        let parsed = value(self);
        if self.at(self.cur) != quote {
            self.fatal("String not closed expecting \" or '\n");
            return parsed.filter(|_| keep_unclosed);
        }
        self.advance(1);
        parsed
    }

    /// `xmlParseVersionInfo`.
    fn parse_version_info(&mut self) -> Option<String> {
        if self.decl_attribute(b"version") != Some(true) {
            return None;
        }
        self.decl_value(true, |reader| {
            let start = reader.cur;
            if !reader.at(reader.cur).is_ascii_digit() {
                return None;
            }
            reader.advance(1);
            if reader.at(reader.cur) != b'.' {
                return None;
            }
            reader.advance(1);
            while reader.at(reader.cur).is_ascii_digit() {
                reader.advance(1);
            }
            Some(String::from_utf8_lossy(&reader.buf[start..reader.cur]).into_owned())
        })
    }

    /// `xmlParseEncodingDecl`, failing on a name iconv would not know.
    fn parse_encoding_decl(&mut self) -> Option<String> {
        if self.decl_attribute(b"encoding") != Some(true) {
            return None;
        }
        let encoding = self.decl_value(false, |reader| {
            if !reader.at(reader.cur).is_ascii_alphabetic() {
                reader.fatal("Invalid XML encoding name\n");
                return None;
            }
            reader.take_while(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
            })
        })?;
        let upper = encoding.to_ascii_uppercase();
        // UTF-8 is native and UTF-16 would already have been switched to, so
        // neither is kept as `input->encoding`.
        if matches!(upper.as_str(), "UTF-16" | "UTF16") {
            self.fatal("Document labelled UTF-16 but has UTF-8 content\n");
            return None;
        }
        if matches!(upper.as_str(), "UTF-8" | "UTF8") {
            return None;
        }
        let known = matches!(
            upper.as_str(),
            "UTF-8"
                | "UTF8"
                | "UTF-16"
                | "UTF16"
                | "US-ASCII"
                | "ASCII"
                | "LATIN1"
                | "ISO-LATIN-1"
                | "UCS-2"
                | "UCS2"
                | "UCS-4"
                | "UCS4"
        ) || upper.starts_with("ISO-8859-")
            || upper.starts_with("WINDOWS-")
            || upper.starts_with("CP");
        if !known {
            self.fatal(format!("Unsupported encoding {encoding}\n"));
            self.unsupported_encoding = true;
            return None;
        }
        Some(encoding)
    }

    /// `xmlParseSDDecl`.
    fn parse_standalone_decl(&mut self) {
        if self.decl_attribute(b"standalone") != Some(true) {
            return;
        }
        let _ = self.decl_value(false, |reader| {
            let rest = &reader.buf[reader.cur..reader.pend];
            if rest.starts_with(b"no") {
                reader.cur += 2;
            } else if rest.starts_with(b"yes") {
                reader.cur += 3;
            } else {
                reader.fatal("standalone accepts only 'yes' or 'no'\n");
            }
            Some(String::new())
        });
    }

    /// `xmlParseDocTypeDecl` up to an internal subset.
    fn parse_doctype(&mut self) {
        self.cur += 9;
        self.skip_blanks();
        let name = self.parse_name();
        if name.is_none() {
            self.fatal("xmlParseDocTypeDecl : no DOCTYPE name !\n");
        }
        self.skip_blanks();
        let rest = &self.buf[self.cur..self.pend];
        if rest.starts_with(b"SYSTEM") {
            self.cur += 6;
            if self.skip_blanks() == 0 {
                self.fatal("Space required after 'SYSTEM'\n");
            }
            if self.parse_literal() {
                self.external_subset = true;
            } else {
                self.fatal("SYSTEM or PUBLIC, the URI is missing\n");
            }
        } else if rest.starts_with(b"PUBLIC") {
            self.cur += 6;
            if self.skip_blanks() == 0 {
                self.fatal("Space required after 'PUBLIC'\n");
            }
            if self.parse_literal() {
                self.external_subset = true;
                if self.skip_blanks() == 0 {
                    self.fatal("Space required after the Public Identifier\n");
                }
                if !self.parse_literal() {
                    self.fatal("SYSTEM or PUBLIC, the URI is missing\n");
                }
            } else {
                self.fatal("PUBLIC, the Public Identifier is missing\n");
            }
        }
        self.skip_blanks();
        if !self.disable_sax {
            let node = self.new_node(Kind::Dtd, name.unwrap_or_default(), Vec::new());
            self.add_child(DOC, node);
        }
        if self.at(self.cur) == b'[' {
            return;
        }
        if self.at(self.cur) != b'>' {
            self.fatal("DOCTYPE improperly terminated\n");
        }
        self.advance(1);
    }

    /// `xmlParseSystemLiteral`: false when no literal starts here; an
    /// unterminated one runs to the end of the buffer.
    fn parse_literal(&mut self) -> bool {
        let quote = self.at(self.cur);
        if !matches!(quote, b'"' | b'\'') {
            self.fatal("SystemLiteral \" or ' expected\n");
            return false;
        }
        self.advance(1);
        match self.lookup(self.cur, &[quote]) {
            Some(end) => self.advance(end + 1 - self.cur),
            None => {
                self.advance(self.pend.saturating_sub(self.cur));
                self.fatal("Unfinished System or Public ID \" or ' expected\n");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodes(reader: &mut XmlReader) -> Vec<(NodeType, String, usize)> {
        let mut out = Vec::new();
        while reader.read() == 1 {
            let kind = reader.node_type();
            let label = match kind {
                NodeType::Text => reader.value(),
                NodeType::Element | NodeType::EndElement => reader.name().to_owned(),
                _ => continue,
            };
            out.push((kind, label, reader.line()));
        }
        out
    }

    #[test]
    fn walks_elements_and_text_like_the_reader() {
        let xml = b"<?xml version=\"1.0\"?>\n<!DOCTYPE rrd SYSTEM \"x.dtd\">\n<rrd>\n <a>1</a>\n <b/>\n</rrd>\n";
        let mut reader = XmlReader::new(xml, Source::Stdin, "-");
        let seen: Vec<_> = nodes(&mut reader)
            .into_iter()
            .map(|(kind, label, _)| (kind, label))
            .collect();
        assert_eq!(
            seen,
            [
                (NodeType::Element, "rrd".into()),
                (NodeType::Element, "a".into()),
                (NodeType::Text, "1".into()),
                (NodeType::EndElement, "a".into()),
                (NodeType::Element, "b".into()),
                (NodeType::EndElement, "rrd".into()),
            ]
        );
        assert_eq!(reader.read(), 0);
    }

    #[test]
    fn stdin_lines_reach_the_parser_one_at_a_time() {
        let xml = b"<rrd>\n<a>x</a>\n<b>y</b>\n</rrd>\n";
        let mut reader = XmlReader::new(xml, Source::Stdin, "-");
        let lines: Vec<_> = nodes(&mut reader)
            .into_iter()
            .filter(|(kind, _, _)| *kind == NodeType::Text)
            .map(|(_, _, line)| line)
            .collect();
        assert_eq!(lines, [3, 5]);
    }

    #[test]
    fn content_after_the_root_fails_the_read_that_reaches_it() {
        let xml = b"<rrd>\n<a>x</a>\n</rrd>\n<x/>\n";
        let mut reader = XmlReader::new(xml, Source::Stdin, "-");
        let mut results = Vec::new();
        loop {
            let rc = reader.read();
            results.push(rc);
            if rc != 1 {
                break;
            }
        }
        assert_eq!(results.last(), Some(&-1));
        assert_eq!(
            reader.last_error_text(),
            "Extra content at the end of the document "
        );
        assert!(reader.diagnostics.is_empty());
    }

    #[test]
    fn file_errors_print_libxml2_context() {
        let mut reader = XmlReader::new(b"hello world\n", Source::File, "in.xml");
        assert_eq!(reader.read(), -1);
        assert_eq!(
            String::from_utf8(reader.diagnostics).unwrap(),
            "in.xml:1: parser error : Document is empty\nhello world\n^\n"
        );
    }

    #[test]
    fn freed_nodes_are_reused() {
        let mut xml = b"<rrd>".to_vec();
        for _ in 0..1000 {
            xml.extend_from_slice(b"<row><v>1</v></row>\n");
        }
        xml.extend_from_slice(b"</rrd>");
        let mut reader = XmlReader::new(&xml, Source::File, "in.xml");
        while reader.read() == 1 {}
        assert!(reader.nodes.len() < 400, "{}", reader.nodes.len());
    }
}
