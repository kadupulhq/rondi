//! The value cache: one entry per file name holding the values accepted
//! for it and not yet written, the write queue, the counters `STATS`
//! reports and the journal.

use super::depth::Depth;
use super::journal::Journal;
use std::collections::{HashMap, VecDeque};

/// Estimated memory of one queued value beyond its text.
const VALUE_OVERHEAD: usize = 48;

/// Estimated memory of one entry beyond its name.
const ENTRY_OVERHEAD: usize = 160;

/// The counters of `STATS` that are not derived from the cache.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub(crate) updates_received: u64,
    pub(crate) flushes_received: u64,
    pub(crate) updates_written: u64,
    pub(crate) data_sets_written: u64,
}

#[derive(Debug)]
pub(crate) struct Entry {
    /// Accepted values, as received, oldest first.
    pub(crate) values: Vec<String>,
    /// The timestamp a new value must exceed.
    pub(crate) last: f64,
    /// When the oldest value still held arrived (seconds).
    pub(crate) first_at: i64,
    /// When the entry last received a value or was written (seconds).
    pub(crate) touched: i64,
    pub(crate) suspended: bool,
    pub(crate) queued: bool,
    /// A write of this entry's values is in progress.
    pub(crate) writing: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) entries: HashMap<Vec<u8>, Entry>,
    depth: Depth,
    /// Entries waiting for a write thread, in order.
    pub(crate) queue: VecDeque<Vec<u8>>,
    pub(crate) stats: Stats,
    /// Estimated memory held by entries and their values.
    pub(crate) held: usize,
    pub(crate) journal: Option<Journal>,
}

/// The cost of a value in the memory estimate.
pub(crate) fn value_cost(value: &[u8]) -> usize {
    value.len() + VALUE_OVERHEAD
}

/// The cost of an entry in the memory estimate.
pub(crate) fn entry_cost(name: &[u8]) -> usize {
    name.len() + ENTRY_OVERHEAD
}

/// Why a value was not queued.
#[derive(Debug, PartialEq)]
pub(crate) enum Refused {
    NoTimestamp(String),
    TooOld { time: f64, last: f64 },
}

impl Refused {
    pub(crate) fn text(&self) -> String {
        match self {
            Self::NoTimestamp(value) => format!("Cannot find timestamp in '{value}'!"),
            Self::TooOld { time, last } => format!(
                "illegal attempt to update using time {time:.6} when last update time is \
                 {last:.6} (minimum one second step)"
            ),
        }
    }
}

/// The timestamp a value starts with: a decimal number, optionally signed,
/// with an optional fraction and exponent, followed by `:`.
// Compatibility: leading white space is skipped; `N`, at-style times,
// hexadecimal, `inf` and `nan` are refused; the number converts as
// RRDtool converts decimals, so 1000000300.123456789 and
// 1000000300.1234567891 differ (RC-42, RC-43). Provenance: black-box
// compatibility test.
pub(crate) fn timestamp(value: &str) -> Option<f64> {
    let bytes = value.as_bytes();
    let mut at = bytes
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    let start = at;
    if matches!(bytes.get(at), Some(b'+' | b'-')) {
        at += 1;
    }
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };
    let whole = digits(at);
    at += whole;
    let mut fraction = 0;
    if bytes.get(at) == Some(&b'.') {
        fraction = digits(at + 1);
        at += 1 + fraction;
    }
    if whole + fraction == 0 {
        return None;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let mut exponent = at + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let count = digits(exponent);
        if count > 0 {
            at = exponent + count;
        }
    }
    if bytes.get(at) != Some(&b':') {
        return None;
    }
    rondi::parse_rrd_number(&value[start..at])
}

impl State {
    pub(crate) fn with_journal(journal: Option<Journal>) -> Self {
        Self {
            journal,
            ..Self::default()
        }
    }

    /// The height `TreeDepth` reports.
    pub(crate) fn depth(&self) -> u32 {
        self.depth.height()
    }

    /// Adds an entry unless one exists; its first value must be later than
    /// `last`.
    pub(crate) fn insert(&mut self, name: &[u8], last: f64, now: i64) {
        if self.entries.contains_key(name) {
            return;
        }
        self.entries.insert(
            name.to_vec(),
            Entry {
                values: Vec::new(),
                last,
                first_at: now,
                touched: now,
                suspended: false,
                queued: false,
                writing: false,
            },
        );
        self.depth.insert(name);
        self.held += entry_cost(name);
    }

    /// Removes an entry and its values.
    pub(crate) fn remove(&mut self, name: &[u8]) -> Option<Entry> {
        let entry = self.entries.remove(name)?;
        self.depth.remove(name);
        self.queue.retain(|queued| queued != name);
        self.held = self.held.saturating_sub(
            entry_cost(name)
                + entry
                    .values
                    .iter()
                    .map(|value| value_cost(value.as_bytes()))
                    .sum::<usize>(),
        );
        Some(entry)
    }

    /// Queues `values` for an entry, left to right, stopping at the first
    /// refused one (RC-44, RC-49). Returns how many were queued.
    pub(crate) fn add_values<'a>(
        &mut self,
        name: &[u8],
        values: impl IntoIterator<Item = &'a str>,
        now: i64,
    ) -> (usize, Option<Refused>) {
        let Some(entry) = self.entries.get_mut(name) else {
            return (0, None);
        };
        let mut count = 0;
        for value in values {
            let Some(time) = timestamp(value) else {
                return (count, Some(Refused::NoTimestamp(value.to_owned())));
            };
            // Compatibility: each value must be later than the previous
            // one (RC-44). Provenance: black-box compatibility test.
            if time.partial_cmp(&entry.last) != Some(std::cmp::Ordering::Greater) {
                let last = entry.last;
                return (count, Some(Refused::TooOld { time, last }));
            }
            if entry.values.is_empty() {
                entry.first_at = now;
            }
            entry.last = time;
            entry.values.push(value.to_owned());
            entry.touched = now;
            self.held += value_cost(value.as_bytes());
            count += 1;
        }
        (count, None)
    }

    /// Queues an entry for a write thread if it holds values and is not
    /// suspended, queued or being written. Returns whether it was queued.
    pub(crate) fn enqueue(&mut self, name: &[u8]) -> bool {
        let Some(entry) = self.entries.get_mut(name) else {
            return false;
        };
        if entry.values.is_empty() || entry.suspended || entry.queued || entry.writing {
            return false;
        }
        entry.queued = true;
        self.queue.push_back(name.to_vec());
        true
    }

    /// Queues every entry whose oldest value is at least `age` seconds old
    /// (RCD-40). Returns how many were queued.
    pub(crate) fn enqueue_older_than(&mut self, now: i64, age: i64) -> usize {
        let due = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.values.is_empty() && now - entry.first_at >= age)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        due.iter().filter(|name| self.enqueue(name)).count()
    }

    /// Drops entries without values that saw no activity for `idle`
    /// seconds.
    pub(crate) fn expire(&mut self, now: i64, idle: i64) {
        let idle_names = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.values.is_empty()
                    && !entry.writing
                    && !entry.suspended
                    && now - entry.touched >= idle
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in idle_names {
            self.remove(&name);
        }
    }

    /// Takes an entry's values for writing and marks it as being written.
    pub(crate) fn take_values(&mut self, name: &[u8]) -> Vec<String> {
        let Some(entry) = self.entries.get_mut(name) else {
            return Vec::new();
        };
        let values = std::mem::take(&mut entry.values);
        entry.writing = !values.is_empty();
        if entry.queued {
            entry.queued = false;
            self.queue.retain(|queued| queued != name);
        }
        self.held = self.held.saturating_sub(
            values
                .iter()
                .map(|value| value_cost(value.as_bytes()))
                .sum::<usize>(),
        );
        values
    }

    /// The `STATS` reply (RC-55).
    pub(crate) fn statistics(&self) -> String {
        let (bytes, rotations) = self
            .journal
            .as_ref()
            .map_or((0, 0), |journal| (journal.bytes, journal.rotations));
        format!(
            "9 Statistics follow\nQueueLength: {}\nUpdatesReceived: {}\nFlushesReceived: {}\n\
             UpdatesWritten: {}\nDataSetsWritten: {}\nTreeNodesNumber: {}\nTreeDepth: {}\n\
             JournalBytes: {bytes}\nJournalRotate: {rotations}\n",
            self.queue.len(),
            self.stats.updates_received,
            self.stats.flushes_received,
            self.stats.updates_written,
            self.stats.data_sets_written,
            self.entries.len(),
            self.depth(),
        )
    }

    pub(crate) fn journal(&mut self, line: &[u8]) {
        if let Some(journal) = &mut self.journal {
            journal.append(line);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_need_a_number_and_a_colon() {
        assert_eq!(timestamp("1000000300:1:2"), Some(1_000_000_300.0));
        assert_eq!(timestamp("1000000300:"), Some(1_000_000_300.0));
        assert_eq!(timestamp("+1000000300.5:x"), Some(1_000_000_300.5));
        assert_eq!(timestamp(".5e9:1"), Some(5e8));
        assert_eq!(timestamp("1000000300.:1"), Some(1_000_000_300.0));
        assert_eq!(timestamp("\t1000000300:1"), Some(1_000_000_300.0));
        assert_eq!(timestamp("1e:1"), None);
        assert_eq!(timestamp("1e9:1"), Some(1e9));
        assert_eq!(timestamp("1e400:1"), Some(f64::INFINITY));
        assert_eq!(timestamp("-0:1").map(f64::is_sign_negative), Some(true));
        for refused in [
            "N:1",
            "n:1",
            "1000000300@1",
            "x",
            ":1",
            "1000000300",
            "0x3B9ACB2C:1",
            "inf:1",
            "nan:1",
            "1000000300x:1",
            "",
            ".:1",
            "+:1",
        ] {
            assert_eq!(timestamp(refused), None, "{refused}");
        }
        assert_ne!(
            timestamp("1000000300.123456789:1"),
            timestamp("1000000300.1234567891:1")
        );
        assert_eq!(
            timestamp("1000000300.1234567891:1"),
            timestamp("1000000300.1234567892:1")
        );
    }

    #[test]
    fn values_stop_at_the_first_refusal() {
        let mut state = State::default();
        state.insert(b"/b/a.rrd", 1_000_000_000.0, 10);
        let (count, refused) = state.add_values(
            b"/b/a.rrd",
            ["1000000300:1", "1000000200:2", "1000000600:3"],
            11,
        );
        assert_eq!(count, 1);
        assert_eq!(
            refused.unwrap().text(),
            "illegal attempt to update using time 1000000200.000000 when last update time is \
             1000000300.000000 (minimum one second step)"
        );
        assert_eq!(
            Refused::NoTimestamp(String::new()).text(),
            "Cannot find timestamp in ''!"
        );
        assert_eq!(
            state.entries[&b"/b/a.rrd".to_vec()].values,
            ["1000000300:1"]
        );
        assert!(state.enqueue(b"/b/a.rrd"));
        assert!(!state.enqueue(b"/b/a.rrd"));
        assert_eq!(state.take_values(b"/b/a.rrd"), ["1000000300:1"]);
        assert!(state.queue.is_empty());
        assert_eq!(state.held, entry_cost(b"/b/a.rrd"));
        state
            .entries
            .get_mut(b"/b/a.rrd".as_slice())
            .unwrap()
            .writing = false;
        state.expire(20, 5);
        assert!(state.entries.is_empty());
        assert_eq!(state.held, 0);
        assert_eq!(state.depth(), 0);
    }

    #[test]
    fn old_entries_queue_in_one_pass() {
        let mut state = State::default();
        for name in [&b"a"[..], b"b", b"c"] {
            state.insert(name, 0.0, 0);
        }
        state.add_values(b"a", ["1:1"], 0);
        state.add_values(b"b", ["1:1"], 5);
        state.add_values(b"c", ["1:1"], 0);
        state.entries.get_mut(b"c".as_slice()).unwrap().suspended = true;
        assert_eq!(state.enqueue_older_than(10, 10), 1);
        assert_eq!(state.queue, [b"a".to_vec()]);
        assert!(state.statistics().contains("QueueLength: 1\n"));
        assert!(
            state
                .statistics()
                .contains("TreeNodesNumber: 3\nTreeDepth: 2\n")
        );
        state.remove(b"a");
        assert!(state.queue.is_empty());
    }
}
