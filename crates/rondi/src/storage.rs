use crate::data_source::apply_gauge_update;
use crate::format::{
    DatabaseConfig, DatabaseFile, FORMAT_VERSION, FetchResult, RrdFetchResult, Update,
};
use crate::import_export::{decode_snapshot, encode_snapshot};
use crate::queries::fetch_retained;
use crate::rrd_binary::{RrdInfo, fetch_rrd_file, inspect_path, update_rrd_file};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invalid database name; use 1-64 ASCII letters, digits, '_' or '-'")]
    InvalidName,
    #[error("database already exists: {0}")]
    AlreadyExists(String),
    #[error("database not found: {0}")]
    NotFound(String),
    #[error("invalid database configuration: {0}")]
    InvalidConfig(String),
    #[error("update timestamp {incoming} must be later than {last}")]
    OutOfOrder { incoming: i64, last: i64 },
    #[error("request ID was already used with different update contents")]
    RequestIdConflict,
    #[error("invalid sample value")]
    InvalidValue,
    #[error("invalid RRD timestamp: {0}")]
    RrdTimestamp(String),
    #[error("unsupported storage format version {0}")]
    FormatVersion(u32),
    #[error("invalid RRD file: {0}")]
    RrdFormat(String),
    #[error("unsupported RRD operation: {0}")]
    RrdUnsupported(String),
    /// An RPN expression error reported with RRDtool's own wording.
    #[error("{0}")]
    RrdExpression(String),
    /// An `.rrd` error carrying the exact text RRDtool passes to
    /// rrd_set_error, with no Rondi prefix.
    #[error("{0}")]
    Rrd(String),
    /// An `.rrd` file that rrd_open could not open, map or read, with
    /// RRDtool's exact text.
    #[error("{0}")]
    RrdFile(String),
    #[error("storage ownership lock is held by another process")]
    Owned,
    #[error("could not lock RRD")]
    RrdLocked,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage format error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Every update rewrites the whole snapshot and a long gap materializes up to
/// `rows` points, so the row count bounds per-update work and file size.
pub const DEFAULT_MAX_ROWS: usize = 100_000;

/// How long a durable request ID stays available for retry deduplication.
pub const DEFAULT_IDEMPOTENCY_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// Compaction runs once the journal holds this many records, or twice the
/// number kept by the previous compaction, whichever is larger, so its cost
/// stays amortized over the appends that triggered it.
const COMPACT_MIN_RECORDS: usize = 1024;

const JOURNAL: &str = "rondi.journal";
const JOURNAL_COMPACT_TMP: &str = "rondi.journal.compact.tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreOptions {
    /// Largest `rows` value accepted by `create` and `import_snapshot`.
    pub max_rows: usize,
    /// Minimum time a journaled request ID is kept for retry deduplication.
    pub idempotency_window: Duration,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_rows: DEFAULT_MAX_ROWS,
            idempotency_window: DEFAULT_IDEMPOTENCY_WINDOW,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct JournalRecord {
    id: String,
    database: String,
    update: Update,
    /// Unix seconds when the record was journaled. Records written before
    /// this field existed read as 0 and are treated as outside the window.
    #[serde(default)]
    accepted: i64,
}

struct JournalIndex {
    ids: HashMap<String, JournalRecord>,
    records: usize,
    compact_at: usize,
}

impl JournalIndex {
    fn new(records: Vec<JournalRecord>) -> Self {
        let count = records.len();
        Self {
            ids: index_journal(records),
            records: count,
            compact_at: count.saturating_mul(2).max(COMPACT_MIN_RECORDS),
        }
    }
}

/// A process-exclusive store. Mutations through one `Store` instance are
/// serialized so read-modify-write and journal idempotency remain atomic for
/// callers that share it across threads. The OS releases its advisory lock on
/// exit, so a daemon crash does not leave stale ownership behind.
pub struct Store {
    root: PathBuf,
    _lock: File,
    options: StoreOptions,
    mutations: Mutex<()>,
    /// Journal records by request ID, loaded on first use so durable updates
    /// do not re-read the whole journal. Only touched while `mutations` is held.
    journal_ids: Mutex<Option<JournalIndex>>,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with(root, StoreOptions::default())
    }

    pub fn open_with(root: impl AsRef<Path>, options: StoreOptions) -> Result<Self, StoreError> {
        if options.max_rows == 0 {
            return Err(StoreError::InvalidConfig(
                "maximum rows must be positive".into(),
            ));
        }
        if options.idempotency_window.is_zero() {
            return Err(StoreError::InvalidConfig(
                "idempotency window must be positive".into(),
            ));
        }
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let lock = lock_options.open(root.join(".rondi.lock"))?;
        lock.try_lock_exclusive().map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                StoreError::Owned
            } else {
                StoreError::Io(e)
            }
        })?;
        Ok(Self {
            root,
            _lock: lock,
            options,
            mutations: Mutex::new(()),
            journal_ids: Mutex::new(None),
        })
    }

    pub fn create(&self, name: &str, config: DatabaseConfig) -> Result<(), StoreError> {
        let _mutation = self.mutation_guard()?;
        validate_name(name)?;
        if config.step == 0 || config.heartbeat == 0 || config.rows == 0 {
            return Err(StoreError::InvalidConfig(
                "step, heartbeat, and rows must be positive".into(),
            ));
        }
        self.check_rows(config.rows)?;
        let step = i64::try_from(config.step).map_err(|_| {
            StoreError::InvalidConfig("step exceeds supported timestamp range".into())
        })?;
        let path = self.path(name);
        let bucket_start = config
            .start
            .div_euclid(step)
            .checked_mul(step)
            .ok_or_else(|| {
                StoreError::InvalidConfig("start exceeds supported bucket range".into())
            })?;
        let db = DatabaseFile {
            version: FORMAT_VERSION,
            last_update: config.start,
            bucket_start,
            config,
            points: Vec::new(),
            known_seconds: 0,
            weighted_sum: 0.0,
        };
        self.write_new_db(name, &path, &db)
    }

    pub fn update(&self, name: &str, update: Update) -> Result<(), StoreError> {
        let _mutation = self.mutation_guard()?;
        self.update_unlocked(name, update)
    }

    fn update_unlocked(&self, name: &str, update: Update) -> Result<(), StoreError> {
        let mut db = self.read_db(name)?;
        apply_gauge_update(&mut db, &update)?;
        self.write_db(&self.path(name), &db)
    }

    /// Journal first, sync it, then update and sync the round-robin file. A
    /// successful return means both operations crossed the filesystem sync
    /// boundary. Recovery can replay a journal record after a crash between
    /// these writes.
    pub fn update_durable(&self, name: &str, update: Update, id: &str) -> Result<(), StoreError> {
        let _mutation = self.mutation_guard()?;
        validate_name(name)?;
        if id.is_empty() || id.len() > 128 {
            return Err(StoreError::InvalidConfig(
                "request id must contain 1-128 bytes".into(),
            ));
        }
        let mut journal_ids = self.journal_ids()?;
        let index = journal_ids.as_mut().expect("journal index is loaded");
        if let Some(prior) = index.ids.get(id) {
            if prior.database != name || prior.update != update {
                return Err(StoreError::RequestIdConflict);
            }
            let mut db = self.read_db(name)?;
            if db.last_update < update.timestamp {
                apply_gauge_update(&mut db, &update)?;
                self.write_db(&self.path(name), &db)?;
            }
            return Ok(());
        }
        let mut db = self.read_db(name)?;
        // Validate before acknowledging the journal record.
        apply_gauge_update(&mut db, &update)?;
        let mut journal_options = OpenOptions::new();
        journal_options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            journal_options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let mut journal = journal_options.open(self.root.join(JOURNAL))?;
        let record = JournalRecord {
            id: id.into(),
            database: name.into(),
            update,
            accepted: unix_now(),
        };
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        if let Err(error) = journal.write_all(&line).and_then(|()| journal.sync_all()) {
            // A failed append can leave a partial line. Reloading the index
            // truncates it before the next record is appended.
            *journal_ids = None;
            return Err(error.into());
        }
        index.ids.insert(record.id.clone(), record);
        index.records += 1;
        self.write_db(&self.path(name), &db)?;
        if index.records >= index.compact_at {
            // Every journaled record is now applied, so compaction only has
            // to honor the idempotency window.
            self.compact_journal(&mut journal_ids, unix_now())?;
        }
        Ok(())
    }

    pub fn recover(&self) -> Result<usize, StoreError> {
        let _mutation = self.mutation_guard()?;
        let mut journal_ids = self.journal_ids_lock()?;
        let records = self.read_journal()?;
        let mut replayed = 0;
        for record in &records {
            let db = self.read_db(&record.database)?;
            if record.update.timestamp <= db.last_update {
                continue;
            }
            self.update_unlocked(&record.database, record.update.clone())?;
            replayed += 1;
        }
        *journal_ids = Some(JournalIndex::new(records));
        self.compact_journal(&mut journal_ids, unix_now())?;
        Ok(replayed)
    }

    /// Rewrite the journal without records that are both applied to their
    /// database and older than the idempotency window. The replacement is
    /// synced before the rename and the directory after it, so a crash leaves
    /// either the old or the new journal; a leftover temporary file is
    /// truncated by the next compaction and never read.
    fn compact_journal(
        &self,
        journal_ids: &mut MutexGuard<'_, Option<JournalIndex>>,
        now: i64,
    ) -> Result<(), StoreError> {
        let window = i64::try_from(self.options.idempotency_window.as_secs()).unwrap_or(i64::MAX);
        let cutoff = now.saturating_sub(window);
        let records = self.read_journal()?;
        let total = records.len();
        let mut last_updates: HashMap<String, Option<i64>> = HashMap::new();
        let mut kept = Vec::with_capacity(total);
        for record in records {
            if record.accepted >= cutoff {
                kept.push(record);
                continue;
            }
            let last_update = match last_updates.get(&record.database) {
                Some(last_update) => *last_update,
                None => {
                    let last_update = match self.read_db(&record.database) {
                        Ok(db) => Some(db.last_update),
                        Err(StoreError::NotFound(_) | StoreError::InvalidName) => None,
                        Err(error) => return Err(error),
                    };
                    last_updates.insert(record.database.clone(), last_update);
                    last_update
                }
            };
            // Keep anything recovery could still need to replay.
            if last_update.is_none_or(|last| record.update.timestamp > last) {
                kept.push(record);
            }
        }
        if kept.len() < total {
            let mut bytes = Vec::new();
            for record in &kept {
                serde_json::to_writer(&mut bytes, record)?;
                bytes.push(b'\n');
            }
            let tmp = self.root.join(JOURNAL_COMPACT_TMP);
            let mut options = OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp, self.root.join(JOURNAL))?;
            File::open(&self.root)?.sync_all()?;
        }
        **journal_ids = Some(JournalIndex::new(kept));
        Ok(())
    }

    fn check_rows(&self, rows: usize) -> Result<(), StoreError> {
        if rows > self.options.max_rows {
            return Err(StoreError::InvalidConfig(format!(
                "rows must not exceed {}",
                self.options.max_rows
            )));
        }
        Ok(())
    }

    fn journal_ids_lock(&self) -> Result<MutexGuard<'_, Option<JournalIndex>>, StoreError> {
        self.journal_ids
            .lock()
            .map_err(|_| StoreError::InvalidConfig("journal index lock is poisoned".into()))
    }

    fn journal_ids(&self) -> Result<MutexGuard<'_, Option<JournalIndex>>, StoreError> {
        let mut journal_ids = self.journal_ids_lock()?;
        if journal_ids.is_none() {
            *journal_ids = Some(JournalIndex::new(self.read_journal()?));
        }
        Ok(journal_ids)
    }

    /// Read every complete journal record. A crash mid-append can leave a
    /// partial last line that was never acknowledged; it is cut off and
    /// synced so the next append starts on a new line.
    fn read_journal(&self) -> Result<Vec<JournalRecord>, StoreError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let mut journal = match options.open(self.root.join(JOURNAL)) {
            Ok(journal) => journal,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut input = Vec::new();
        journal.read_to_end(&mut input)?;
        let complete_length = input
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        if complete_length < input.len() {
            journal.set_len(complete_length as u64)?;
            journal.sync_all()?;
        }
        input[..complete_length]
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
            .map(|(line_no, line)| {
                serde_json::from_slice(line).map_err(|error| {
                    StoreError::InvalidConfig(format!("journal line {}: {error}", line_no + 1))
                })
            })
            .collect()
    }

    pub fn fetch(&self, name: &str) -> Result<FetchResult, StoreError> {
        let db = self.read_db(name)?;
        Ok(fetch_retained(name, db))
    }

    /// Inspect structural metadata in a native `.rrd` file without modifying it.
    /// Only the common 64-bit little-endian v3 layout is currently recognized.
    pub fn inspect_rrd(&self, name: &str) -> Result<RrdInfo, StoreError> {
        validate_name(name)?;
        inspect_path(&self.root.join(format!("{name}.rrd")))
    }

    /// Fetch archived values from an existing RRDtool file using RRDtool's
    /// archive selection and ring-buffer range rules. This is read-only.
    pub fn fetch_rrd(
        &self,
        name: &str,
        consolidation: &str,
        start: i64,
        end: i64,
        resolution: u64,
    ) -> Result<RrdFetchResult, StoreError> {
        validate_name(name)?;
        fetch_rrd_file(
            self.root.join(format!("{name}.rrd")),
            consolidation,
            start,
            end,
            resolution,
        )
    }

    /// Update an existing RRDtool file in place for the supported RRD subset.
    /// The RRD's own whole-file write lock is used for upstream coordination.
    pub fn update_rrd(
        &self,
        name: &str,
        timestamp: i64,
        value: Option<f64>,
    ) -> Result<(), StoreError> {
        let _mutation = self.mutation_guard()?;
        validate_name(name)?;
        update_rrd_file(self.root.join(format!("{name}.rrd")), timestamp, value)
    }

    /// Return a snapshot using Rondi's versioned JSON format.
    pub fn export_snapshot(&self, name: &str) -> Result<Vec<u8>, StoreError> {
        encode_snapshot(&self.read_db(name)?)
    }

    /// Import a Rondi snapshot as a new database under the configured root.
    /// This does not read or convert an RRDtool `.rrd` file.
    pub fn import_snapshot(&self, name: &str, snapshot: &[u8]) -> Result<(), StoreError> {
        let _mutation = self.mutation_guard()?;
        validate_name(name)?;
        let path = self.path(name);
        let db = decode_snapshot(snapshot)?;
        self.check_rows(db.config.rows)?;
        self.write_new_db(name, &path, &db)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}.rondi"))
    }

    fn mutation_guard(&self) -> Result<MutexGuard<'_, ()>, StoreError> {
        self.mutations
            .lock()
            .map_err(|_| StoreError::InvalidConfig("store mutation lock is poisoned".into()))
    }

    fn read_db(&self, name: &str) -> Result<DatabaseFile, StoreError> {
        validate_name(name)?;
        let path = self.path(name);
        if !path.exists() {
            return Err(StoreError::NotFound(name.into()));
        }
        let mut bytes = Vec::new();
        open_read_nofollow(&path)?.read_to_end(&mut bytes)?;
        decode_snapshot(&bytes)
    }

    fn write_db(&self, path: &Path, db: &DatabaseFile) -> Result<(), StoreError> {
        let tmp = self.write_temp(path, db)?;
        fs::rename(&tmp, path)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn write_new_db(&self, name: &str, path: &Path, db: &DatabaseFile) -> Result<(), StoreError> {
        let tmp = self.write_temp(path, db)?;
        match fs::hard_link(&tmp, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp);
                return Err(StoreError::AlreadyExists(name.into()));
            }
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                return Err(StoreError::Io(error));
            }
        }
        fs::remove_file(tmp)?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    fn write_temp(&self, path: &Path, db: &DatabaseFile) -> Result<PathBuf, StoreError> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let mut tmp_name = path
            .file_name()
            .ok_or(StoreError::InvalidName)?
            .to_os_string();
        tmp_name.push(format!(".tmp.{}.{}", std::process::id(), nonce));
        let tmp = path.with_file_name(tmp_name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(&encode_snapshot(db)?)?;
        file.sync_all()?;
        Ok(tmp)
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn index_journal(records: Vec<JournalRecord>) -> HashMap<String, JournalRecord> {
    let mut ids = HashMap::with_capacity(records.len());
    for record in records {
        ids.entry(record.id.clone()).or_insert(record);
    }
    ids
}

fn validate_name(name: &str) -> Result<(), StoreError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(StoreError::InvalidName);
    }
    Ok(())
}

pub(crate) fn open_read_nofollow(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DatabaseConfig {
        DatabaseConfig {
            step: 10,
            heartbeat: 15,
            rows: 3,
            start: 1_700_000_000,
        }
    }

    #[test]
    fn bucket_boundaries_and_retention_rollover() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        for (timestamp, value) in [
            (1_700_000_010, 2.0),
            (1_700_000_020, 4.0),
            (1_700_000_030, 8.0),
            (1_700_000_040, 16.0),
        ] {
            store
                .update(
                    "cpu",
                    Update {
                        timestamp,
                        value: Some(value),
                    },
                )
                .unwrap();
        }
        let points = store.fetch("cpu").unwrap().points;
        assert_eq!(
            points.iter().map(|p| p.value.unwrap()).collect::<Vec<_>>(),
            [4.0, 8.0, 16.0]
        );
        assert_eq!(points[0].timestamp, 1_700_000_020);
    }

    #[test]
    fn concurrent_retries_with_one_request_id_append_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(dir.path()).unwrap());
        store.create("cpu", config()).unwrap();
        let update = Update {
            timestamp: 1_700_000_010,
            value: Some(3.5),
        };
        let start = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers = (0..2)
            .map(|_| {
                let store = std::sync::Arc::clone(&store);
                let start = std::sync::Arc::clone(&start);
                let update = update.clone();
                std::thread::spawn(move || {
                    start.wait();
                    store.update_durable("cpu", update, "retry-1")
                })
            })
            .collect::<Vec<_>>();
        start.wait();
        for worker in workers {
            worker.join().unwrap().unwrap();
        }

        let journal = fs::read_to_string(dir.path().join("rondi.journal")).unwrap();
        assert_eq!(journal.lines().count(), 1);
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(3.5));
    }

    #[test]
    fn large_gap_processes_only_bounded_retention_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .create(
                "cpu",
                DatabaseConfig {
                    step: 1,
                    heartbeat: 2_000_000,
                    rows: 3,
                    start: 1_700_000_000,
                },
            )
            .unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_701_000_000,
                    value: Some(6.0),
                },
            )
            .unwrap();
        let points = store.fetch("cpu").unwrap().points;
        assert_eq!(points.len(), 3);
        assert_eq!(points[0].timestamp, 1_700_999_998);
        assert!(points.iter().all(|point| point.value == Some(6.0)));
    }

    #[test]
    fn irregular_samples_are_time_weighted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_004,
                    value: Some(2.0),
                },
            )
            .unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(8.0),
                },
            )
            .unwrap();
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(5.6));
    }

    #[test]
    fn unaligned_start_uses_the_next_epoch_step_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut cfg = config();
        cfg.start += 3;
        store.create("cpu", cfg.clone()).unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: cfg.start + 7,
                    value: Some(4.0),
                },
            )
            .unwrap();
        let point = &store.fetch("cpu").unwrap().points[0];
        assert_eq!(point.timestamp, 1_700_000_010);
        assert_eq!(point.value, Some(4.0));
    }

    #[test]
    fn heartbeat_expiry_and_unknown_values_make_unknown_buckets() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut cfg = config();
        cfg.rows = 6;
        store.create("cpu", cfg).unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(1.0),
                },
            )
            .unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_020,
                    value: None,
                },
            )
            .unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_040,
                    value: Some(5.0),
                },
            )
            .unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_050,
                    value: Some(5.0),
                },
            )
            .unwrap();
        let values = store
            .fetch("cpu")
            .unwrap()
            .points
            .into_iter()
            .map(|p| p.value)
            .collect::<Vec<_>>();
        assert_eq!(values, [Some(1.0), None, None, None, Some(5.0)]);
    }

    #[test]
    fn closing_interval_beyond_heartbeat_makes_the_whole_pdp_unknown() {
        // RRDtool 1.11.0 fetches 1000000010 as nan for these updates.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store
            .create(
                "cpu",
                DatabaseConfig {
                    step: 10,
                    heartbeat: 5,
                    rows: 5,
                    start: 1_000_000_000,
                },
            )
            .unwrap();
        for (timestamp, value) in [
            (1_000_000_003, 4.0),
            (1_000_000_006, 4.0),
            (1_000_000_012, 8.0),
        ] {
            store
                .update(
                    "cpu",
                    Update {
                        timestamp,
                        value: Some(value),
                    },
                )
                .unwrap();
        }
        let points = store.fetch("cpu").unwrap().points;
        assert_eq!(points[0].timestamp, 1_000_000_010);
        assert_eq!(points[0].value, None);
    }

    #[test]
    fn reopen_and_out_of_order_validation() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
            store
                .update(
                    "cpu",
                    Update {
                        timestamp: 1_700_000_010,
                        value: Some(3.0),
                    },
                )
                .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(3.0));
        assert!(matches!(
            store.update(
                "cpu",
                Update {
                    timestamp: 1_700_000_009,
                    value: Some(1.0)
                }
            ),
            Err(StoreError::OutOfOrder { .. })
        ));
        assert!(matches!(
            store.update(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(2.0)
                }
            ),
            Err(StoreError::OutOfOrder { .. })
        ));
    }

    #[test]
    fn exactly_half_known_seconds_produce_a_known_pdp() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_005,
                    value: None,
                },
            )
            .unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(5.0),
                },
            )
            .unwrap();
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(5.0));
    }

    #[test]
    fn journal_replays_an_accepted_write_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot;
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
            snapshot = std::fs::read(dir.path().join("cpu.rondi")).unwrap();
            store
                .update_durable(
                    "cpu",
                    Update {
                        timestamp: 1_700_000_010,
                        value: Some(7.0),
                    },
                    "req-1",
                )
                .unwrap();
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("rondi.journal"))
                .unwrap()
                .write_all(b"{\"id\":\"truncated")
                .unwrap();
        }
        // Simulate a crash after the journal sync and before database rename.
        std::fs::write(dir.path().join("cpu.rondi"), snapshot).unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.recover().unwrap(), 1);
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(7.0));
        assert_eq!(store.recover().unwrap(), 0);
    }

    #[test]
    fn recovery_truncates_a_torn_journal_tail() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
            store
                .update_durable(
                    "cpu",
                    Update {
                        timestamp: 1_700_000_010,
                        value: Some(1.0),
                    },
                    "req-1",
                )
                .unwrap();
            std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("rondi.journal"))
                .unwrap()
                .write_all(b"{\"id\":\"torn")
                .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        store.recover().unwrap();
        for (timestamp, id) in [(1_700_000_020, "req-2"), (1_700_000_030, "req-3")] {
            store
                .update_durable(
                    "cpu",
                    Update {
                        timestamp,
                        value: Some(2.0),
                    },
                    id,
                )
                .unwrap();
        }
        store.recover().unwrap();
        let journal = fs::read_to_string(dir.path().join("rondi.journal")).unwrap();
        assert_eq!(journal.lines().count(), 3);
        assert!(!journal.contains("torn"));
    }

    #[test]
    fn durable_update_after_a_torn_tail_starts_a_new_journal_line() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
            std::fs::write(dir.path().join("rondi.journal"), b"{\"id\":\"torn").unwrap();
        }
        {
            let store = Store::open(dir.path()).unwrap();
            store
                .update_durable(
                    "cpu",
                    Update {
                        timestamp: 1_700_000_010,
                        value: Some(1.0),
                    },
                    "req-1",
                )
                .unwrap();
        }
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.recover().unwrap(), 0);
        assert_eq!(
            fs::read_to_string(dir.path().join("rondi.journal"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[test]
    fn create_rejects_row_counts_above_the_store_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut cfg = config();
        cfg.rows = usize::MAX;
        assert!(matches!(
            store.create("cpu", cfg),
            Err(StoreError::InvalidConfig(_))
        ));
        let mut cfg = config();
        cfg.rows = DEFAULT_MAX_ROWS;
        store.create("cpu", cfg).unwrap();
    }

    fn compact_at(store: &Store, now: i64) {
        let mut ids = store.journal_ids().unwrap();
        store.compact_journal(&mut ids, now).unwrap();
    }

    fn journal_lines(dir: &Path) -> usize {
        fs::read_to_string(dir.join(JOURNAL))
            .unwrap()
            .lines()
            .count()
    }

    const WINDOW: i64 = 24 * 60 * 60;

    #[test]
    fn store_options_reject_zero_limits() {
        let dir = tempfile::tempdir().unwrap();
        for options in [
            StoreOptions {
                max_rows: 0,
                ..StoreOptions::default()
            },
            StoreOptions {
                idempotency_window: Duration::ZERO,
                ..StoreOptions::default()
            },
        ] {
            assert!(matches!(
                Store::open_with(dir.path(), options),
                Err(StoreError::InvalidConfig(_))
            ));
        }
    }

    #[test]
    fn configured_row_cap_applies_to_create_and_import() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_with(
            dir.path(),
            StoreOptions {
                max_rows: 10,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let mut cfg = config();
        cfg.rows = 11;
        assert!(matches!(
            store.create("big", cfg),
            Err(StoreError::InvalidConfig(message)) if message == "rows must not exceed 10"
        ));
        let mut cfg = config();
        cfg.rows = 10;
        store.create("cpu", cfg).unwrap();
        let other = tempfile::tempdir().unwrap();
        let source = Store::open(other.path()).unwrap();
        let mut cfg = config();
        cfg.rows = 11;
        source.create("big", cfg).unwrap();
        let snapshot = source.export_snapshot("big").unwrap();
        assert!(matches!(
            store.import_snapshot("big", &snapshot),
            Err(StoreError::InvalidConfig(_))
        ));
    }

    #[test]
    fn retry_inside_the_window_stays_idempotent_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        let update = Update {
            timestamp: 1_700_000_010,
            value: Some(7.0),
        };
        store
            .update_durable("cpu", update.clone(), "req-1")
            .unwrap();
        compact_at(&store, unix_now() + WINDOW - 60);
        assert_eq!(journal_lines(dir.path()), 1);
        store.update_durable("cpu", update, "req-1").unwrap();
        assert!(matches!(
            store.update_durable(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(9.0),
                },
                "req-1"
            ),
            Err(StoreError::RequestIdConflict)
        ));
    }

    #[test]
    fn retry_after_the_window_is_a_new_request() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        let update = Update {
            timestamp: 1_700_000_010,
            value: Some(7.0),
        };
        store
            .update_durable("cpu", update.clone(), "req-1")
            .unwrap();
        compact_at(&store, unix_now() + WINDOW + 60);
        assert_eq!(journal_lines(dir.path()), 0);
        // The ID is forgotten, so the old sample is rejected as stale rather
        // than acknowledged as a duplicate.
        assert!(matches!(
            store.update_durable("cpu", update, "req-1"),
            Err(StoreError::OutOfOrder { .. })
        ));
        store
            .update_durable(
                "cpu",
                Update {
                    timestamp: 1_700_000_020,
                    value: Some(9.0),
                },
                "req-1",
            )
            .unwrap();
        assert_eq!(journal_lines(dir.path()), 1);
    }

    #[test]
    fn compaction_keeps_records_recovery_still_needs() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
        }
        // A record older than the window whose database write never landed.
        fs::write(
            dir.path().join(JOURNAL),
            b"{\"id\":\"req-1\",\"database\":\"cpu\",\"update\":{\"timestamp\":1700000010,\"value\":7.0},\"accepted\":1}\n",
        )
        .unwrap();
        let store = Store::open(dir.path()).unwrap();
        compact_at(&store, unix_now());
        assert_eq!(journal_lines(dir.path()), 1);
        assert_eq!(store.recover().unwrap(), 1);
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(7.0));
        // Applied and outside the window, so recovery's compaction drops it.
        assert_eq!(journal_lines(dir.path()), 0);
        assert_eq!(store.recover().unwrap(), 0);
    }

    #[test]
    fn records_without_an_acceptance_time_compact_once_applied() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
            store
                .update_durable(
                    "cpu",
                    Update {
                        timestamp: 1_700_000_010,
                        value: Some(7.0),
                    },
                    "req-1",
                )
                .unwrap();
        }
        let legacy = fs::read_to_string(dir.path().join(JOURNAL))
            .unwrap()
            .replace(",\"accepted\":", ",\"ignored\":");
        fs::write(dir.path().join(JOURNAL), legacy).unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.recover().unwrap(), 0);
        assert_eq!(journal_lines(dir.path()), 0);
    }

    #[test]
    fn durable_updates_compact_once_the_threshold_is_reached() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        // An applied record from outside the window.
        fs::write(
            dir.path().join(JOURNAL),
            b"{\"id\":\"old\",\"database\":\"cpu\",\"update\":{\"timestamp\":1700000000,\"value\":1.0},\"accepted\":1}\n",
        )
        .unwrap();
        store.journal_ids().unwrap().as_mut().unwrap().compact_at = 2;
        store
            .update_durable(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(7.0),
                },
                "req-1",
            )
            .unwrap();
        let journal = fs::read_to_string(dir.path().join(JOURNAL)).unwrap();
        assert_eq!(journal.lines().count(), 1);
        assert!(journal.contains("req-1"));
        let ids = store.journal_ids().unwrap();
        let index = ids.as_ref().unwrap();
        assert!(!index.ids.contains_key("old"));
        assert_eq!(index.compact_at, COMPACT_MIN_RECORDS);
    }

    #[test]
    fn interrupted_compaction_leaves_a_usable_journal() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.create("cpu", config()).unwrap();
            store
                .update_durable(
                    "cpu",
                    Update {
                        timestamp: 1_700_000_010,
                        value: Some(7.0),
                    },
                    "req-1",
                )
                .unwrap();
        }
        // A crash before the rename leaves a stray, possibly partial, file.
        fs::write(dir.path().join(JOURNAL_COMPACT_TMP), b"{\"id\":\"par").unwrap();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.recover().unwrap(), 0);
        assert_eq!(journal_lines(dir.path()), 1);
        store
            .update_durable(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(7.0),
                },
                "req-1",
            )
            .unwrap();
        compact_at(&store, unix_now() + WINDOW + 60);
        assert_eq!(journal_lines(dir.path()), 0);
        assert!(!dir.path().join(JOURNAL_COMPACT_TMP).exists());
        assert_eq!(store.fetch("cpu").unwrap().points[0].value, Some(7.0));
    }

    #[test]
    fn durable_retry_requires_matching_request_id_contents() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        let update = Update {
            timestamp: 1_700_000_010,
            value: Some(7.0),
        };
        store
            .update_durable("cpu", update.clone(), "req-1")
            .unwrap();
        store.update_durable("cpu", update, "req-1").unwrap();
        assert!(matches!(
            store.update_durable(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(9.0),
                },
                "req-1"
            ),
            Err(StoreError::RequestIdConflict)
        ));
    }

    #[test]
    fn storage_file_has_persistent_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        let contents = std::fs::read_to_string(dir.path().join("cpu.rondi")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(json["version"], FORMAT_VERSION);
    }

    #[test]
    fn snapshot_export_import_round_trips_into_a_new_database() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("cpu", config()).unwrap();
        store
            .update(
                "cpu",
                Update {
                    timestamp: 1_700_000_010,
                    value: Some(3.5),
                },
            )
            .unwrap();

        let snapshot = store.export_snapshot("cpu").unwrap();
        store.import_snapshot("copy", &snapshot).unwrap();
        let original = store.fetch("cpu").unwrap();
        let imported = store.fetch("copy").unwrap();
        assert_eq!(imported.step, original.step);
        assert_eq!(imported.points, original.points);
        assert!(matches!(
            store.import_snapshot("copy", &snapshot),
            Err(StoreError::AlreadyExists(_))
        ));
    }

    #[test]
    fn store_ownership_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = Store::open(dir.path()).unwrap();
        assert!(matches!(Store::open(dir.path()), Err(StoreError::Owned)));
        drop(first);
        assert!(Store::open(dir.path()).is_ok());
    }

    #[test]
    fn concurrent_create_never_replaces_the_winning_database() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let outcomes = std::thread::scope(|scope| {
            (0..8)
                .map(|_| scope.spawn(|| store.create("cpu", config()).is_ok()))
                .map(|thread| thread.join().unwrap())
                .filter(|created| *created)
                .count()
        });
        assert_eq!(outcomes, 1);
        assert!(store.fetch("cpu").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn database_symlinks_are_not_followed_outside_the_storage_root() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external_file = outside.path().join("external.rondi");
        std::fs::write(&external_file, b"keep this file unchanged").unwrap();
        let store = Store::open(dir.path()).unwrap();
        symlink(&external_file, dir.path().join("cpu.rondi")).unwrap();
        assert!(matches!(store.fetch("cpu"), Err(StoreError::Io(_))));
        assert_eq!(
            std::fs::read(external_file).unwrap(),
            b"keep this file unchanged"
        );
    }
}
