use crate::data_source::apply_gauge_update;
use crate::format::{
    DatabaseConfig, DatabaseFile, FORMAT_VERSION, FetchResult, RrdFetchResult, Update,
};
use crate::import_export::{decode_snapshot, encode_snapshot};
use crate::queries::fetch_retained;
use crate::rrd_binary::{RrdInfo, fetch_rrd_file, inspect_path, update_rrd_file};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
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
    #[error("storage ownership lock is held by another process")]
    Owned,
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage format error: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Serialize, Deserialize)]
struct JournalRecord {
    id: String,
    database: String,
    update: Update,
}

/// A process-exclusive store. Mutations through one `Store` instance are
/// serialized so read-modify-write and journal idempotency remain atomic for
/// callers that share it across threads. The OS releases its advisory lock on
/// exit, so a daemon crash does not leave stale ownership behind.
pub struct Store {
    root: PathBuf,
    _lock: File,
    mutations: Mutex<()>,
}

impl Store {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
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
            mutations: Mutex::new(()),
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
        if let Some(prior) = self.find_journal_record(id)? {
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
        let mut journal = journal_options.open(self.root.join("rondi.journal"))?;
        serde_json::to_writer(
            &mut journal,
            &JournalRecord {
                id: id.into(),
                database: name.into(),
                update,
            },
        )?;
        journal.write_all(b"\n")?;
        journal.sync_all()?;
        self.write_db(&self.path(name), &db)
    }

    pub fn recover(&self) -> Result<usize, StoreError> {
        let _mutation = self.mutation_guard()?;
        let path = self.root.join("rondi.journal");
        if !path.exists() {
            return Ok(0);
        }
        let mut input = String::new();
        open_read_nofollow(&path)?.read_to_string(&mut input)?;
        let complete_length = input.rfind('\n').map_or(0, |index| index + 1);
        let complete = &input[..complete_length];
        let mut replayed = 0;
        for (line_no, line) in complete.lines().enumerate() {
            let record: JournalRecord = serde_json::from_str(line).map_err(|e| {
                StoreError::InvalidConfig(format!("journal line {}: {e}", line_no + 1))
            })?;
            let db = self.read_db(&record.database)?;
            if record.update.timestamp <= db.last_update {
                continue;
            }
            self.update_unlocked(&record.database, record.update)?;
            replayed += 1;
        }
        Ok(replayed)
    }

    fn find_journal_record(&self, id: &str) -> Result<Option<JournalRecord>, StoreError> {
        let path = self.root.join("rondi.journal");
        if !path.exists() {
            return Ok(None);
        }
        let mut input = String::new();
        open_read_nofollow(&path)?.read_to_string(&mut input)?;
        let complete_length = input.rfind('\n').map_or(0, |index| index + 1);
        for (line_no, line) in input[..complete_length].lines().enumerate() {
            let record: JournalRecord = serde_json::from_str(line).map_err(|error| {
                StoreError::InvalidConfig(format!("journal line {}: {error}", line_no + 1))
            })?;
            if record.id == id {
                return Ok(Some(record));
            }
        }
        Ok(None)
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
