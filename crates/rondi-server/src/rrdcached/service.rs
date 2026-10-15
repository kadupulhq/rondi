//! The request handlers and the state they share: the file names, the
//! value cache and the journal (docs/compatibility/rrdcached.md).
//!
//! Handlers run on blocking threads. The cache lock is never held across
//! file I/O: a handler reads what it needs under the lock, works on files
//! without it, and takes it again to record the result.

use super::cache::{self, State};
use super::commands::{self, Command};
use super::fetch::{self, FetchReply};
use super::files::{Access, Denied, Names, Target};
use super::journal::{self, Journal};
use super::protocol::{self, Request};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use tokio::sync::Notify;

/// Daemon options the handlers use.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Settings {
    /// `-O`.
    pub(crate) no_overwrite: bool,
    /// `-R`.
    pub(crate) recursive_create: bool,
    /// `-w`, in seconds.
    pub(crate) write_interval: i64,
    /// `-F`.
    pub(crate) flush_at_shutdown: bool,
    /// Bound on the estimated memory of entries and values.
    pub(crate) max_pending_bytes: usize,
}

pub(crate) struct Shared {
    pub(crate) names: Names,
    pub(crate) settings: Settings,
    state: Mutex<State>,
    /// Signalled when a write of an entry finishes.
    written: Condvar,
    /// Notified when an entry is queued for a write thread.
    pub(crate) queued: Arc<Notify>,
}

/// What a handler answers.
pub(crate) enum Answer {
    /// A complete success reply; a batch does not send it.
    Done(Vec<u8>),
    /// An error, without the `-1 ` and the final LF.
    Failed(Vec<u8>),
    /// An error after which the connection closes; a batch closes too.
    FailedThenClose(Vec<u8>),
    /// A reply after which the connection closes.
    DoneThenClose(Vec<u8>),
    /// `FETCH` and `FETCHBIN` rows, sent as they are formatted.
    Fetch(Box<FetchReply>),
}

impl Answer {
    fn failed(text: impl Into<Vec<u8>>) -> Self {
        Self::Failed(text.into())
    }
}

/// The current time in whole seconds.
pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn as_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Text after `-1 ` for an RRD error: `RRD Error: <message>`.
fn rrd_error(message: impl std::fmt::Display) -> Answer {
    Answer::failed(format!("RRD Error: {message}"))
}

/// `opening '<path>': <reason>`, the reader's text for a failed open.
pub(crate) fn opening(joined: &[u8], error: &std::io::Error) -> String {
    format!("opening '{}': {}", lossy(joined), super::error_text(error))
}

/// The outcome of writing an entry's values.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Written {
    /// The name has no entry.
    Absent,
    /// The entry is suspended and was left alone.
    Suspended,
    /// The entry's values, if any, were written.
    Done,
}

/// The cache holds as much as the memory bound allows.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Full;

/// The reply to a request refused by the memory bound.
const FULL: &str = "Pending updates exceed the daemon's memory limit; retry later";

impl Shared {
    pub(crate) fn new(names: Names, settings: Settings, journal: Option<Journal>) -> Self {
        Self {
            names,
            settings,
            state: Mutex::new(State::with_journal(journal)),
            written: Condvar::new(),
            queued: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs one request (other than `QUIT`, `BATCH` and `.`). `line` is
    /// the request without its line end.
    pub(crate) fn execute(&self, command: Command, line: &[u8]) -> Answer {
        let request = protocol::split(line);
        let fields = &request.fields;
        let usage = || Answer::failed(command.usage());
        match command {
            Command::Ping => Answer::Done(b"0 PONG\n".to_vec()),
            Command::Help => Answer::Done(commands::help(request.field(1))),
            Command::Stats => Answer::Done(self.state().statistics().into_bytes()),
            Command::Queue => Answer::Done(self.queue_listing()),
            Command::FlushAll => Answer::Done(self.flush_all()),
            Command::SuspendAll => Answer::Done(self.suspend_all(true)),
            Command::ResumeAll => Answer::Done(self.suspend_all(false)),
            Command::List => match super::listing::list(&self.names, &fields[1..]) {
                Ok(reply) => Answer::Done(reply),
                Err(error) => Answer::Failed(error),
            },
            Command::Update => match request.field(1) {
                None => usage(),
                Some(name) => self
                    .update(name, &request, Source::Client)
                    .unwrap_or_else(|Full| Answer::failed(FULL)),
            },
            Command::Pending => match request.field(1) {
                None => usage(),
                Some(name) => Answer::Done(self.pending(name)),
            },
            Command::Forget => match request.field(1) {
                None => usage(),
                Some(name) => self.forget(name),
            },
            Command::Flush => match request.field(1) {
                None => usage(),
                Some(name) => self.flush(name),
            },
            Command::Suspend | Command::Resume => match request.field(1) {
                None => Answer::FailedThenClose(command.usage().into_bytes()),
                Some(name) => self.suspend(name, command == Command::Suspend),
            },
            Command::Info => match request.field(1) {
                None => usage(),
                Some(name) => self.info(name),
            },
            Command::Last => match request.field(1) {
                None => usage(),
                Some(name) => self.last(name),
            },
            Command::First => match (request.field(1), request.field(2)) {
                (Some(name), Some(index)) => self.first(name, index),
                _ => usage(),
            },
            Command::Dump => match request.field(1) {
                None => usage(),
                Some(name) => self.dump(name),
            },
            Command::Fetch | Command::FetchBin => {
                if fields.len() < 3 {
                    return usage();
                }
                fetch::fetch(self, fields, command == Command::FetchBin)
            }
            Command::Create => match request.field(1) {
                None => usage(),
                Some(name) => super::create::create(self, name, &fields[2..]),
            },
            Command::Tune => {
                if fields.len() < 3 {
                    return usage();
                }
                super::tune::tune(self, fields)
            }
            Command::Batch | Command::Quit | Command::Wrote | Command::Dot => {
                unreachable!("{command:?} is handled by the connection")
            }
        }
    }

    /// The target of a joined name, or the `-B` refusal.
    pub(crate) fn locate(&self, joined: &[u8]) -> Result<Target, Answer> {
        self.names
            .locate(joined)
            .map_err(|Denied(text)| Answer::Failed(text))
    }

    /// Reads a file's header, with the reader's error text on failure.
    pub(crate) fn inspect(&self, target: &Target, joined: &[u8]) -> Result<rondi::RrdInfo, String> {
        let file = target
            .open(Access::Read)
            .map_err(|error| opening(joined, &error))?;
        rondi::inspect_rrd_open_file(file, as_path(joined)).map_err(|error| error.to_string())
    }

    /// Queues the values of an `UPDATE` request or journal line.
    // Compatibility: a name without an entry must be an existing regular
    // RRD file, whose last update the first value must exceed; the
    // request counts in UpdatesReceived once its name is present; values
    // queue left to right until one is refused (RC-40 to RC-49).
    // Provenance: rrdcached(1) and black-box compatibility test.
    pub(crate) fn update(
        &self,
        name: &[u8],
        request: &Request<'_>,
        source: Source,
    ) -> Result<Answer, Full> {
        // Rondi policy (RC-49): the journal records the request text as
        // sent, so a request it could not record whole is refused.
        if source == Source::Client && (request.rest.len() > 4095 || request.rest.contains(&0)) {
            return Ok(Answer::failed(
                "Request must be under 4096 bytes without NUL",
            ));
        }
        self.state().stats.updates_received += 1;
        let joined = self.names.join(name);
        let target = match self.locate(&joined) {
            Ok(target) => target,
            Err(answer) => return Ok(answer),
        };
        let values = request.fields[2..]
            .iter()
            .map(|value| lossy(value))
            .collect::<Vec<_>>();
        // Rondi policy (RCD-60): values beyond the memory bound are
        // refused before they are journaled or acknowledged.
        let cost = values
            .iter()
            .map(|value| cache::value_cost(value.as_bytes()))
            .sum::<usize>();
        let fits = |state: &State, extra: usize| {
            state.held + cost + extra <= self.settings.max_pending_bytes
        };
        // The file is read without the lock; an entry removed meanwhile
        // sends the request back to read it.
        let mut last = None;
        let mut state = loop {
            if last.is_none() && !self.state().entries.contains_key(&joined) {
                match self.last_update(&target, &joined) {
                    Ok(time) => last = Some(time),
                    Err(answer) => return Ok(answer),
                }
            }
            let mut state = self.state();
            if state.entries.contains_key(&joined) {
                if !fits(&state, 0) {
                    return Err(Full);
                }
                break state;
            }
            let Some(time) = last.take() else {
                continue;
            };
            if !fits(&state, cache::entry_cost(&joined)) {
                return Err(Full);
            }
            state.insert(&joined, time, now());
            break state;
        };
        if source == Source::Client {
            let mut line = b"update ".to_vec();
            line.extend_from_slice(request.rest);
            state.journal(&line);
        }
        let now = now();
        let (count, refused) = state.add_values(&joined, values.iter().map(String::as_str), now);
        if state
            .entries
            .get(&joined)
            .is_some_and(|entry| now - entry.first_at >= self.settings.write_interval)
            && state.enqueue(&joined)
        {
            self.queued.notify_one();
        }
        drop(state);
        Ok(match refused {
            Some(refused) => Answer::failed(refused.text()),
            None if count == 0 => Answer::failed("No values updated."),
            None => Answer::Done(format!("0 errors, enqueued {count} value(s).\n").into_bytes()),
        })
    }

    /// The last update time of a file about to get a cache entry.
    fn last_update(&self, target: &Target, joined: &[u8]) -> Result<f64, Answer> {
        let shown = lossy(joined);
        match target.status() {
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                return Err(Answer::failed(format!("No such file: {shown}")));
            }
            Err(error) => {
                return Err(Answer::failed(format!(
                    "stat failed with error {}.",
                    error.raw_os_error().unwrap_or(0)
                )));
            }
            Ok(status) if status.st_mode & libc::S_IFMT != libc::S_IFREG => {
                return Err(Answer::failed(format!("Not a regular file: {shown}")));
            }
            Ok(_) => {}
        }
        let info = self.inspect(target, joined).map_err(rrd_error)?;
        Ok(info.last_update as f64 + info.last_update_usec as f64 / 1e6)
    }

    fn pending(&self, name: &[u8]) -> Vec<u8> {
        let joined = self.names.join(name);
        let state = self.state();
        let values = state
            .entries
            .get(&joined)
            .map_or(&[][..], |entry| entry.values.as_slice());
        let mut reply = format!("{} updates pending\n", values.len()).into_bytes();
        for value in values {
            reply.extend_from_slice(value.as_bytes());
            reply.push(b'\n');
        }
        reply
    }

    fn forget(&self, name: &[u8]) -> Answer {
        let joined = self.names.join(name);
        if let Err(answer) = self.locate(&joined) {
            return answer;
        }
        let mut state = self.state();
        if state.remove(&joined).is_none() {
            return Answer::failed("No such file or directory");
        }
        let mut line = b"forget ".to_vec();
        line.extend_from_slice(&joined);
        state.journal(&line);
        Answer::Done(b"0 Gone!\n".to_vec())
    }

    // Compatibility: FLUSH writes the entry before answering and counts in
    // FlushesReceived once its name is present; without an entry it
    // reports whether the name is a regular file (RC-50, RC-112).
    // Provenance: rrdcached(1) and black-box compatibility test.
    fn flush(&self, name: &[u8]) -> Answer {
        self.state().stats.flushes_received += 1;
        let joined = self.names.join(name);
        let target = match self.locate(&joined) {
            Ok(target) => target,
            Err(answer) => return answer,
        };
        let shown = lossy(&joined);
        if self.write_entry(&joined) != Written::Absent {
            return Answer::Done(format!("0 Successfully flushed {shown}.\n").into_bytes());
        }
        match target.status() {
            Ok(status) if status.st_mode & libc::S_IFMT == libc::S_IFREG => {
                Answer::Done(format!("0 Nothing to flush: {shown}.\n").into_bytes())
            }
            _ => Answer::failed(format!("No such file: {shown}.")),
        }
    }

    fn flush_all(&self) -> Vec<u8> {
        let mut state = self.state();
        let names = state.entries.keys().cloned().collect::<Vec<_>>();
        let queued = names.iter().filter(|name| state.enqueue(name)).count();
        drop(state);
        for _ in 0..queued {
            self.queued.notify_one();
        }
        b"0 Started flush.\n".to_vec()
    }

    fn queue_listing(&self) -> Vec<u8> {
        let state = self.state();
        let mut reply = format!("{} in queue.\n", state.queue.len()).into_bytes();
        for name in &state.queue {
            let count = state
                .entries
                .get(name)
                .map_or(0, |entry| entry.values.len());
            reply.extend_from_slice(format!("{count} ").as_bytes());
            reply.extend_from_slice(name);
            reply.push(b'\n');
        }
        reply
    }

    // Compatibility: SUSPEND and RESUME name an existing entry, and a
    // name without one closes the connection after the error (RC-110).
    // Provenance: rrdcached(1) and black-box compatibility test.
    fn suspend(&self, name: &[u8], suspend: bool) -> Answer {
        let joined = self.names.join(name);
        let shown = lossy(&joined);
        let mut state = self.state();
        let Some(entry) = state.entries.get_mut(&joined) else {
            return Answer::FailedThenClose(
                format!("{shown} - No such file or directory").into_bytes(),
            );
        };
        let text = match (suspend, entry.suspended) {
            (true, true) => format!("0 {shown} already suspended\n"),
            (false, false) => format!("0 {shown} not suspended\n"),
            (true, false) => format!("0 {shown} suspended\n"),
            (false, true) => format!("0 {shown} resumed\n"),
        };
        let dequeue = suspend && entry.queued;
        entry.queued &= !suspend;
        entry.suspended = suspend;
        if dequeue {
            state.queue.retain(|queued| queued != &joined);
        }
        Answer::Done(text.into_bytes())
    }

    fn suspend_all(&self, suspend: bool) -> Vec<u8> {
        let mut state = self.state();
        let mut changed = 0;
        for entry in state.entries.values_mut() {
            if entry.suspended != suspend {
                entry.suspended = suspend;
                entry.queued &= !suspend;
                changed += 1;
            }
        }
        if suspend {
            state.queue.clear();
            format!("0 {changed} rrds suspend\n").into_bytes()
        } else {
            format!("0 {changed} rrds resumed\n").into_bytes()
        }
    }

    fn info(&self, name: &[u8]) -> Answer {
        let joined = self.names.join(name);
        let target = match self.locate(&joined) {
            Ok(target) => target,
            Err(answer) => return answer,
        };
        match self.inspect(&target, &joined) {
            Ok(info) => Answer::Done(info_reply(&joined, &info)),
            Err(error) => rrd_error(error),
        }
    }

    // Compatibility: LAST answers the latest time the cache accepted for
    // the file, or else the file's last update, rounded down to a multiple
    // of the file's step; nothing is written first (RC-61, info-004).
    // Provenance: black-box compatibility test.
    fn last(&self, name: &[u8]) -> Answer {
        let joined = self.names.join(name);
        let target = match self.locate(&joined) {
            Ok(target) => target,
            Err(answer) => return answer,
        };
        match self.inspect(&target, &joined) {
            Ok(info) => {
                let cached = self.state().entries.get(&joined).map(|entry| entry.last);
                let last = match cached {
                    // The saturating conversion keeps a huge accepted
                    // time from wrapping.
                    Some(time) if time.is_finite() => time.floor() as i64,
                    _ => info.last_update,
                };
                let step = i64::try_from(info.step).unwrap_or(i64::MAX).max(1);
                let last = last - last.rem_euclid(step);
                Answer::Done(format!("0 {last}\n").into_bytes())
            }
            Err(error) => rrd_error(error),
        }
    }

    // Compatibility: the index is required and must be a decimal integer
    // that fits (RC-62). Provenance: black-box compatibility test.
    fn first(&self, name: &[u8], index: &[u8]) -> Answer {
        let parsed = std::str::from_utf8(index)
            .ok()
            .filter(|text| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|text| text.parse::<u32>().ok());
        let Some(index) = parsed else {
            return Answer::failed(format!("Invalid index specified: {}", lossy(index)));
        };
        let joined = self.names.join(name);
        let target = match self.locate(&joined) {
            Ok(target) => target,
            Err(answer) => return answer,
        };
        let info = match self.inspect(&target, &joined) {
            Ok(info) => info,
            Err(error) => return rrd_error(error),
        };
        match rondi::first_rrd_time_from_info(&info, index as usize) {
            Ok(first) => Answer::Done(format!("0 {first}\n").into_bytes()),
            Err(error) => rrd_error(error),
        }
    }

    // Compatibility: DUMP sends the XML with the DTD header and no status
    // line, then closes the connection; errors name the file as sent
    // (RC-95). Provenance: black-box compatibility test.
    // Rondi policy: DUMP is confined by -B like every other command.
    fn dump(&self, name: &[u8]) -> Answer {
        let joined = self.names.join(name);
        let target = match self.locate(&joined) {
            Ok(target) => target,
            Err(answer) => return answer,
        };
        self.write_entry(&joined);
        let failed = |what: &str, reason: String| {
            let mut text = name.to_vec();
            text.extend_from_slice(format!(": {what}: {reason}").as_bytes());
            Answer::Failed(text)
        };
        match target.status() {
            Err(error) => return failed("failed to stat", super::error_text(&error)),
            Ok(status) if status.st_mode & libc::S_IFMT != libc::S_IFREG => {
                return failed("failed to stat", super::errno_text(0));
            }
            Ok(_) => {}
        }
        let file = match target.open(Access::Read) {
            Ok(file) => file,
            Err(error) => return failed("failed to stat", super::error_text(&error)),
        };
        match rondi::dump_rrd_open_file_bytes(file, as_path(&joined), rondi::RrdDumpHeader::Dtd) {
            Ok(xml) => Answer::DoneThenClose(xml),
            Err(error) => failed("failed to relay dump", error.to_string()),
        }
    }

    /// Writes an entry's pending values to its file and waits for any
    /// write of it already in progress.
    // Compatibility: a write applies values up to the first one the file
    // refuses; the error is only logged, the values are dropped either way
    // and `wrote <name>` is journaled (RC-45, RCD-30). Provenance:
    // black-box compatibility test.
    pub(crate) fn write_entry(&self, name: &[u8]) -> Written {
        let mut state = self.state();
        loop {
            match state.entries.get(name) {
                None => return Written::Absent,
                Some(entry) if entry.suspended => return Written::Suspended,
                Some(entry) if entry.writing => {
                    state = self
                        .written
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                Some(_) => break,
            }
        }
        let values = state.take_values(name);
        if values.is_empty() {
            return Written::Done;
        }
        drop(state);
        self.apply(name, &values);
        let mut state = self.state();
        state.stats.updates_written += 1;
        state.stats.data_sets_written += values.len() as u64;
        let mut line = b"wrote ".to_vec();
        line.extend_from_slice(name);
        state.journal(&line);
        let now = now();
        let mut again = None;
        if let Some(entry) = state.entries.get_mut(name) {
            entry.writing = false;
            entry.touched = now;
            // Rondi policy: values that arrived during the write follow
            // the `wrote` line in the journal again, so a replay does not
            // drop them with the values just written.
            if !entry.values.is_empty() {
                let mut line = b"update ".to_vec();
                line.extend_from_slice(&protocol::escape(name));
                for value in &entry.values {
                    line.push(b' ');
                    line.extend_from_slice(&protocol::escape(value.as_bytes()));
                }
                again = Some(line);
            }
        }
        if let Some(line) = again {
            state.journal(&line);
        }
        drop(state);
        self.written.notify_all();
        Written::Done
    }

    fn apply(&self, name: &[u8], values: &[String]) {
        let path = as_path(name);
        let file = match self.names.locate(name) {
            Ok(target) => target.open(Access::Write),
            Err(Denied(_)) => Err(std::io::Error::from_raw_os_error(libc::EACCES)),
        };
        let result = match file {
            Ok(file) => {
                let values = values.iter().map(String::as_str).collect::<Vec<_>>();
                rondi::update_rrd_text_file(file, path, None, &values, false)
                    .1
                    .map_err(|error| error.to_string())
            }
            Err(error) => Err(opening(name, &error)),
        };
        if let Err(error) = result {
            tracing::warn!(path = %path.display(), error, "rrdcached_write_failed");
        }
    }

    /// Takes the next entry queued for a write thread.
    pub(crate) fn next_queued(&self) -> Option<Vec<u8>> {
        let mut state = self.state();
        let name = state.queue.pop_front()?;
        if let Some(entry) = state.entries.get_mut(&name) {
            entry.queued = false;
        }
        Some(name)
    }

    /// The `-f` pass: queue entries older than `-w`, drop entries idle for
    /// `idle` seconds, start a new journal file (RCD-40, RCD-41).
    pub(crate) fn flush_pass(&self, idle: i64) {
        let now = now();
        let mut state = self.state();
        let queued = state.enqueue_older_than(now, self.settings.write_interval);
        state.expire(now, idle);
        if let Some(journal) = &mut state.journal {
            journal.rotate();
        }
        drop(state);
        for _ in 0..queued {
            self.queued.notify_one();
        }
    }

    /// Names of the entries holding values.
    pub(crate) fn pending_names(&self) -> Vec<Vec<u8>> {
        self.state()
            .entries
            .iter()
            .filter(|(_, entry)| !entry.values.is_empty())
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(crate) fn journaled(&self) -> bool {
        self.state().journal.is_some()
    }

    /// Closes the journal, removing its files when `written`.
    pub(crate) fn finish(&self, written: bool) {
        if let Some(journal) = &mut self.state().journal {
            journal.finish(written);
        }
    }

    /// Replays the journal files found at startup, then starts a new
    /// journal file (RCD-31, RCD-32). Returns whether any update line was
    /// replayed.
    pub(crate) fn replay(&self, files: &[std::ffi::CString]) -> Result<bool, String> {
        let mut replayed = false;
        for name in files {
            let text = {
                let state = self.state();
                let Some(journal) = state.journal.as_ref() else {
                    return Ok(false);
                };
                match journal.read(name) {
                    Ok(Some(text)) => text,
                    Ok(None) => {
                        tracing::warn!(
                            journal = %journal.display(name).display(),
                            "rrdcached_journal_skipped"
                        );
                        continue;
                    }
                    Err(error) => {
                        return Err(format!(
                            "cannot read journal {}: {error}",
                            journal.display(name).display()
                        ));
                    }
                }
            };
            for line in journal::lines(&text) {
                replayed |= self.replay_line(line)?;
            }
        }
        let mut state = self.state();
        if let Some(journal) = &mut state.journal {
            journal
                .start()
                .map_err(|error| format!("cannot start a journal file: {error}"))?;
        }
        Ok(replayed)
    }

    // Compatibility: `update` (any case) queues values like UPDATE and
    // always counts in UpdatesReceived; `wrote` drops an entry's values;
    // `forget` removes the entry; anything else is skipped (RCD-32).
    // Provenance: black-box compatibility test.
    // Rondi policy: under -B a name outside the base directory is refused
    // like a request's, and values beyond the memory bound stop startup
    // instead of being dropped.
    fn replay_line(&self, line: &[u8]) -> Result<bool, String> {
        let request = protocol::split(line);
        let Some(word) = request.field(0) else {
            return Ok(false);
        };
        let name = request.field(1);
        if word.eq_ignore_ascii_case(b"update") {
            let Some(name) = name else {
                self.state().stats.updates_received += 1;
                return Ok(false);
            };
            return match self.update(name, &request, Source::Journal) {
                Err(Full) => Err("journal replay exceeds the pending value memory limit".into()),
                Ok(_) => Ok(true),
            };
        }
        let Some(name) = name else {
            return Ok(false);
        };
        let joined = self.names.join(name);
        let mut state = self.state();
        if word.eq_ignore_ascii_case(b"wrote") {
            state.take_values(&joined);
            if let Some(entry) = state.entries.get_mut(&joined) {
                entry.writing = false;
            }
        } else if word.eq_ignore_ascii_case(b"forget") {
            state.remove(&joined);
        }
        Ok(false)
    }
}

/// Where an `UPDATE` comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Client,
    Journal,
}

/// `%.10e`-style text; `NaN` for NaN as the reply prints it (RC-60).
fn info_float(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else {
        fetch::c_exponent(value, 10)
    }
}

/// The `INFO` reply: `<key> <type> <value>` lines (RC-60).
// Compatibility: the keys of `rrdtool info`; type 0 is a float, 1 an
// integer, 2 a string. Provenance: rrdcached(1) and black-box
// compatibility test.
fn info_reply(joined: &[u8], info: &rondi::RrdInfo) -> Vec<u8> {
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let mut line = |key: &[u8], kind: u8, value: &[u8]| {
        let mut text = key.to_vec();
        text.extend_from_slice(format!(" {kind} ").as_bytes());
        text.extend_from_slice(value);
        lines.push(text);
    };
    line(b"filename", 2, joined);
    line(b"rrd_version", 2, info.version.as_bytes());
    line(b"step", 1, info.step.to_string().as_bytes());
    line(b"last_update", 1, info.last_update.to_string().as_bytes());
    line(b"header_size", 1, info.header_size.to_string().as_bytes());
    for (index, source) in info.data_sources.iter().enumerate() {
        let key =
            |field: &str| [b"ds[".as_slice(), &source.raw_name, b"].", field.as_bytes()].concat();
        let bound = |value: Option<f64>| info_float(value.unwrap_or(f64::NAN));
        line(&key("index"), 1, index.to_string().as_bytes());
        line(&key("type"), 2, &source.raw_kind);
        line(
            &key("minimal_heartbeat"),
            1,
            source.heartbeat.to_string().as_bytes(),
        );
        line(&key("min"), 0, bound(source.minimum).as_bytes());
        line(&key("max"), 0, bound(source.maximum).as_bytes());
        line(&key("last_ds"), 2, &source.raw_last_value);
        line(&key("value"), 0, info_float(source.pdp_value).as_bytes());
        line(
            &key("unknown_sec"),
            1,
            source.unknown_seconds.to_string().as_bytes(),
        );
    }
    for (index, archive) in info.archives.iter().enumerate() {
        let key = |field: &str| format!("rra[{index}].{field}").into_bytes();
        line(&key("cf"), 2, &archive.raw_consolidation);
        line(&key("rows"), 1, archive.rows.to_string().as_bytes());
        line(
            &key("cur_row"),
            1,
            archive.current_row.to_string().as_bytes(),
        );
        line(
            &key("pdp_per_row"),
            1,
            archive.pdp_per_row.to_string().as_bytes(),
        );
        line(&key("xff"), 0, info_float(archive.xff).as_bytes());
        for (slot, prep) in archive.cdp_prep.iter().enumerate() {
            let prep_key =
                |field: &str| format!("rra[{index}].cdp_prep[{slot}].{field}").into_bytes();
            line(&prep_key("value"), 0, info_float(prep.value).as_bytes());
            line(
                &prep_key("unknown_datapoints"),
                1,
                prep.unknown_datapoints.to_string().as_bytes(),
            );
        }
    }
    let mut reply = format!("{} Info for ", lines.len()).into_bytes();
    reply.extend_from_slice(joined);
    reply.extend_from_slice(b" follows\n");
    for line in lines {
        reply.extend_from_slice(&line);
        reply.push(b'\n');
    }
    reply
}
