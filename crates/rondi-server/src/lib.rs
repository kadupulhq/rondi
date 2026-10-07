use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rondi::{DatabaseConfig, Store, StoreError, Update};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::io::{
    BufRead as StdBufRead, BufReader as StdBufReader, Seek, SeekFrom, Write as StdWrite,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};

pub const DEFAULT_RRDCACHED_QUEUE_BYTES: usize = 64 * 1024 * 1024;

fn open_private_rrdcached_journal(path: &Path) -> Result<File, Box<dyn std::error::Error>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        options
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .mode(0o600);
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "rrdcached journal is not a regular file: {}",
                path.display()
            )
            .into());
        }
        // Journal contents are trusted during recovery. Refuse a file planted
        // by a different local user and repair permissions on owned files.
        // SAFETY: getuid takes no pointers and has no side effects.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid {
            return Err(format!(
                "rrdcached journal is not owned by this user: {}",
                path.display()
            )
            .into());
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        Ok(options.open(path)?)
    }
}

fn canonical_journal_path(
    root: &Path,
    path: &Path,
    allow_missing: bool,
) -> Result<PathBuf, String> {
    let canonical = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| format!("invalid journal path: {}", path.display()))?;
            let filename = path
                .file_name()
                .ok_or_else(|| format!("invalid journal path: {}", path.display()))?;
            std::fs::canonicalize(parent)
                .map_err(|error| error.to_string())?
                .join(filename)
        }
        Err(error) => return Err(error.to_string()),
    };
    if !canonical.starts_with(root) {
        return Err(format!(
            "rrdcached journal path is outside base directory: {}",
            path.display()
        ));
    }
    Ok(canonical)
}

fn wall_time_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub queue_capacity: usize,
}

/// Configuration for the legacy rrdcached line protocol. This first protocol
/// slice deliberately listens on a Unix socket only; UPDATE is acknowledged
/// after its journal record is synced, and queued writes flush later.
#[derive(Debug, Clone)]
pub struct RrdcachedConfig {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub journal_directory: Option<PathBuf>,
    pub pid_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub no_overwrite: bool,
    pub allow_recursive_mkdir: bool,
    pub socket_mode: Option<u32>,
    pub socket_commands: Option<Vec<String>>,
    pub socket_group: Option<u32>,
    pub allocation_chunk: usize,
    pub write_timeout_seconds: u64,
    pub flush_interval_seconds: u64,
    pub queue_threads: usize,
    pub max_pending_bytes: usize,
}

struct PidFile {
    path: PathBuf,
    pid: u32,
    _file: File,
}

impl PidFile {
    fn create(path: &Path) -> Result<Option<Self>, Box<dyn std::error::Error>> {
        let Some(path) = (!path.as_os_str().is_empty()).then(|| path.to_path_buf()) else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pid = std::process::id();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o644);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = std::fs::read_to_string(&path)?;
                let existing_pid = existing
                    .trim()
                    .parse::<libc::pid_t>()
                    .map_err(|_| format!("invalid rrdcached pid file: {}", path.display()))?;
                if existing_pid <= 0 {
                    return Err(format!("invalid rrdcached pid file: {}", path.display()).into());
                }
                // SAFETY: signal 0 only checks whether this PID is present.
                let alive = unsafe { libc::kill(existing_pid, 0) } == 0
                    || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
                if alive {
                    return Err(format!(
                        "rrdcached pid file is owned by live process {existing_pid}"
                    )
                    .into());
                }
                std::fs::remove_file(&path)?;
                options.open(&path)?
            }
            Err(error) => return Err(error.into()),
        };
        let mut file = file;
        writeln!(&mut file, "{pid}")?;
        file.sync_all()?;
        Ok(Some(Self {
            path,
            pid,
            _file: file,
        }))
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path)
            .is_ok_and(|contents| contents.trim() == self.pid.to_string())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Default)]
struct RrdcachedStats {
    updates_received: AtomicU64,
    updates_written: AtomicU64,
    datasets_written: AtomicU64,
    flushes_received: AtomicU64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
enum RrdcachedJournalRecord {
    Update {
        id: u64,
        path: PathBuf,
        samples: Vec<String>,
    },
    Flushed {
        id: u64,
    },
    Forgotten {
        ids: Vec<u64>,
        #[serde(default)]
        path: Option<PathBuf>,
    },
    Expired {
        path: PathBuf,
    },
}

#[derive(Debug, Clone)]
struct PendingRrdUpdate {
    id: u64,
    samples: Vec<String>,
}

#[derive(Default)]
struct CacheTree {
    root: Option<Box<CacheTreeNode>>,
    len: usize,
}

struct CacheTreeNode {
    path: PathBuf,
    last_flush_time: i64,
    left: Option<Box<CacheTreeNode>>,
    right: Option<Box<CacheTreeNode>>,
    height: usize,
}

impl CacheTree {
    fn contains(&self, path: &Path) -> bool {
        let mut node = self.root.as_deref();
        while let Some(current) = node {
            node = match path.cmp(&current.path) {
                std::cmp::Ordering::Less => current.left.as_deref(),
                std::cmp::Ordering::Greater => current.right.as_deref(),
                std::cmp::Ordering::Equal => return true,
            };
        }
        false
    }

    fn insert(&mut self, path: PathBuf, now: i64) -> bool {
        let (root, inserted) = cache_tree_insert(self.root.take(), path, now);
        self.root = root;
        self.len += usize::from(inserted);
        inserted
    }

    fn mark_flushed(&mut self, path: &Path, now: i64) {
        let mut node = self.root.as_deref_mut();
        while let Some(current) = node {
            match path.cmp(&current.path) {
                std::cmp::Ordering::Less => node = current.left.as_deref_mut(),
                std::cmp::Ordering::Greater => node = current.right.as_deref_mut(),
                std::cmp::Ordering::Equal => {
                    current.last_flush_time = now;
                    return;
                }
            }
        }
    }

    fn last_flush_time(&self, path: &Path) -> Option<i64> {
        let mut node = self.root.as_deref();
        while let Some(current) = node {
            match path.cmp(&current.path) {
                std::cmp::Ordering::Less => node = current.left.as_deref(),
                std::cmp::Ordering::Greater => node = current.right.as_deref(),
                std::cmp::Ordering::Equal => return Some(current.last_flush_time),
            }
        }
        None
    }

    fn idle_paths(&self, now: i64, age: u64) -> Vec<PathBuf> {
        fn visit(node: Option<&CacheTreeNode>, now: i64, age: u64, paths: &mut Vec<PathBuf>) {
            if let Some(node) = node {
                visit(node.left.as_deref(), now, age, paths);
                if now.saturating_sub(node.last_flush_time) >= age.min(i64::MAX as u64) as i64 {
                    paths.push(node.path.clone());
                }
                visit(node.right.as_deref(), now, age, paths);
            }
        }
        let mut paths = Vec::new();
        visit(self.root.as_deref(), now, age, &mut paths);
        paths
    }

    fn remove(&mut self, path: &Path) -> bool {
        let (root, removed) = cache_tree_remove(self.root.take(), path);
        self.root = root;
        self.len -= usize::from(removed);
        removed
    }

    fn len(&self) -> usize {
        self.len
    }

    fn height(&self) -> usize {
        cache_tree_node_height(&self.root)
    }

    fn paths(&self) -> Vec<PathBuf> {
        fn visit(node: Option<&CacheTreeNode>, paths: &mut Vec<PathBuf>) {
            if let Some(node) = node {
                visit(node.left.as_deref(), paths);
                paths.push(node.path.clone());
                visit(node.right.as_deref(), paths);
            }
        }

        let mut paths = Vec::with_capacity(self.len);
        visit(self.root.as_deref(), &mut paths);
        paths
    }
}

fn cache_tree_node_height(node: &Option<Box<CacheTreeNode>>) -> usize {
    node.as_ref().map_or(0, |node| node.height)
}

fn cache_tree_update_height(node: &mut CacheTreeNode) {
    node.height = 1 + cache_tree_node_height(&node.left).max(cache_tree_node_height(&node.right));
}

fn cache_tree_balance(node: &CacheTreeNode) -> isize {
    cache_tree_node_height(&node.right) as isize - cache_tree_node_height(&node.left) as isize
}

fn cache_tree_rotate_left(mut root: Box<CacheTreeNode>) -> Box<CacheTreeNode> {
    let mut pivot = root
        .right
        .take()
        .expect("right-heavy AVL node has a right child");
    root.right = pivot.left.take();
    cache_tree_update_height(&mut root);
    pivot.left = Some(root);
    cache_tree_update_height(&mut pivot);
    pivot
}

fn cache_tree_rotate_right(mut root: Box<CacheTreeNode>) -> Box<CacheTreeNode> {
    let mut pivot = root
        .left
        .take()
        .expect("left-heavy AVL node has a left child");
    root.left = pivot.right.take();
    cache_tree_update_height(&mut root);
    pivot.right = Some(root);
    cache_tree_update_height(&mut pivot);
    pivot
}

fn cache_tree_rebalance(mut node: Box<CacheTreeNode>) -> Box<CacheTreeNode> {
    cache_tree_update_height(&mut node);
    match cache_tree_balance(&node) {
        balance if balance < -1 => {
            if cache_tree_balance(node.left.as_deref().unwrap()) > 0 {
                node.left = node.left.take().map(cache_tree_rotate_left);
            }
            cache_tree_rotate_right(node)
        }
        balance if balance > 1 => {
            if cache_tree_balance(node.right.as_deref().unwrap()) < 0 {
                node.right = node.right.take().map(cache_tree_rotate_right);
            }
            cache_tree_rotate_left(node)
        }
        _ => node,
    }
}

fn cache_tree_insert(
    node: Option<Box<CacheTreeNode>>,
    path: PathBuf,
    now: i64,
) -> (Option<Box<CacheTreeNode>>, bool) {
    let Some(mut node) = node else {
        return (
            Some(Box::new(CacheTreeNode {
                path,
                last_flush_time: now,
                left: None,
                right: None,
                height: 1,
            })),
            true,
        );
    };
    let inserted = match path.cmp(&node.path) {
        std::cmp::Ordering::Less => {
            let (left, inserted) = cache_tree_insert(node.left.take(), path, now);
            node.left = left;
            inserted
        }
        std::cmp::Ordering::Greater => {
            let (right, inserted) = cache_tree_insert(node.right.take(), path, now);
            node.right = right;
            inserted
        }
        std::cmp::Ordering::Equal => return (Some(node), false),
    };
    (Some(cache_tree_rebalance(node)), inserted)
}

fn cache_tree_pop_min(
    mut node: Box<CacheTreeNode>,
) -> (Option<Box<CacheTreeNode>>, Box<CacheTreeNode>) {
    let Some(left) = node.left.take() else {
        let right = node.right.take();
        return (right, node);
    };
    let (left, min) = cache_tree_pop_min(left);
    node.left = left;
    (Some(cache_tree_rebalance(node)), min)
}

fn cache_tree_remove(
    node: Option<Box<CacheTreeNode>>,
    path: &Path,
) -> (Option<Box<CacheTreeNode>>, bool) {
    let Some(mut node) = node else {
        return (None, false);
    };
    let removed = match path.cmp(&node.path) {
        std::cmp::Ordering::Less => {
            let (left, removed) = cache_tree_remove(node.left.take(), path);
            node.left = left;
            removed
        }
        std::cmp::Ordering::Greater => {
            let (right, removed) = cache_tree_remove(node.right.take(), path);
            node.right = right;
            removed
        }
        std::cmp::Ordering::Equal => {
            return match (node.left.take(), node.right.take()) {
                (None, right) => (right, true),
                (left, None) => (left, true),
                (left, Some(right)) => {
                    let (right, mut successor) = cache_tree_pop_min(right);
                    successor.left = left;
                    successor.right = right;
                    (Some(cache_tree_rebalance(successor)), true)
                }
            };
        }
    };
    if removed {
        (Some(cache_tree_rebalance(node)), true)
    } else {
        (Some(node), false)
    }
}

struct RrdcachedQueue {
    pending: std::collections::BTreeMap<PathBuf, Vec<PendingRrdUpdate>>,
    pending_order: std::collections::VecDeque<PathBuf>,
    known: CacheTree,
    suspended: std::collections::HashSet<PathBuf>,
    journal: File,
    next_id: u64,
    journal_bytes: u64,
    pending_bytes: usize,
    max_pending_bytes: usize,
    write_timeout_seconds: u64,
    allocation_chunk: usize,
}

impl RrdcachedQueue {
    fn open(
        root: &Path,
        max_pending_bytes: usize,
        write_timeout_seconds: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::open_in_directory(root, root, max_pending_bytes, write_timeout_seconds)
    }

    fn open_in_directory(
        root: &Path,
        journal_directory: &Path,
        max_pending_bytes: usize,
        write_timeout_seconds: u64,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let replay_time = wall_time_seconds();
        let journal_path = journal_directory.join(".rrdcached.journal");
        let journal = open_private_rrdcached_journal(&journal_path)?;
        let mut pending = std::collections::BTreeMap::<PathBuf, Vec<PendingRrdUpdate>>::new();
        let mut flushed = std::collections::HashSet::new();
        let mut forgotten = std::collections::HashSet::new();
        let mut known = CacheTree::default();
        let mut paths_by_id = std::collections::HashMap::<u64, PathBuf>::new();
        let mut next_id = 1;
        if journal.metadata()?.len() > 0 {
            let mut replay = journal.try_clone()?;
            replay.seek(SeekFrom::Start(0))?;
            for line in StdBufReader::new(replay).lines() {
                let line = line?;
                if line.is_empty() {
                    continue;
                }
                let record: RrdcachedJournalRecord = serde_json::from_str(&line)?;
                match record {
                    RrdcachedJournalRecord::Update { id, path, samples } => {
                        let path = canonical_journal_path(root, &path, true)?;
                        next_id = next_id.max(id.saturating_add(1));
                        known.insert(path.clone(), replay_time);
                        paths_by_id.insert(id, path.clone());
                        pending
                            .entry(path)
                            .or_default()
                            .push(PendingRrdUpdate { id, samples });
                    }
                    RrdcachedJournalRecord::Flushed { id } => {
                        flushed.insert(id);
                        if let Some(path) = paths_by_id.get(&id) {
                            known.mark_flushed(path, replay_time);
                        }
                    }
                    RrdcachedJournalRecord::Forgotten { ids, path } => {
                        forgotten.extend(ids);
                        if let Some(path) = path {
                            known.remove(&path);
                        } else {
                            for id in forgotten.iter() {
                                if let Some(path) = paths_by_id.get(id) {
                                    known.remove(path);
                                }
                            }
                        }
                    }
                    RrdcachedJournalRecord::Expired { path } => {
                        let path = canonical_journal_path(root, &path, true)?;
                        known.remove(&path);
                    }
                }
            }
        }
        for entries in pending.values_mut() {
            entries.retain(|entry| !flushed.contains(&entry.id) && !forgotten.contains(&entry.id));
        }
        pending.retain(|_, entries| !entries.is_empty());
        let mut pending_order = Vec::new();
        for (path, entries) in &pending {
            if entries.first().is_some_and(|_| {
                known.last_flush_time(path).is_some_and(|last_flush| {
                    replay_time.saturating_sub(last_flush)
                        >= write_timeout_seconds.min(i64::MAX as u64) as i64
                })
            }) {
                pending_order.push((entries[0].id, path.clone()));
            }
        }
        pending_order.sort_by_key(|(id, _)| *id);
        let pending_order = pending_order.into_iter().map(|(_, path)| path).collect();
        let pending_bytes = pending
            .values()
            .flatten()
            .map(|entry| pending_entry_bytes(&entry.samples))
            .fold(0_usize, usize::saturating_add);
        if pending_bytes > max_pending_bytes {
            return Err(format!(
                "recovered rrdcached queue requires {pending_bytes} bytes, exceeding configured limit {max_pending_bytes}"
            )
            .into());
        }
        let journal_bytes = journal.metadata()?.len();
        Ok(Self {
            pending,
            pending_order,
            known,
            suspended: std::collections::HashSet::new(),
            journal,
            next_id,
            journal_bytes,
            pending_bytes,
            max_pending_bytes,
            write_timeout_seconds,
            allocation_chunk: 1,
        })
    }

    fn schedule_eligible(&mut self, now: i64) {
        let timeout = self.write_timeout_seconds.min(i64::MAX as u64) as i64;
        let eligible = self
            .pending
            .keys()
            .filter(|path| {
                !self.suspended.contains(*path)
                    && self
                        .known
                        .last_flush_time(path)
                        .is_some_and(|last_flush| now.saturating_sub(last_flush) >= timeout)
            })
            .filter(|path| !self.pending_order.contains(path))
            .cloned()
            .collect::<Vec<_>>();
        self.pending_order.extend(eligible);
    }

    fn schedule_path(&mut self, path: &Path, now: i64) {
        if self.suspended.contains(path)
            || self.pending_order.iter().any(|queued| queued == path)
            || !self.pending.contains_key(path)
        {
            return;
        }
        let timeout = self.write_timeout_seconds.min(i64::MAX as u64) as i64;
        if self
            .known
            .last_flush_time(path)
            .is_some_and(|last_flush| now.saturating_sub(last_flush) >= timeout)
        {
            self.pending_order.push_back(path.to_path_buf());
        }
    }

    fn append(&mut self, record: &RrdcachedJournalRecord) -> Result<(), String> {
        let mut encoded = serde_json::to_vec(record).map_err(|error| error.to_string())?;
        encoded.push(b'\n');
        self.journal
            .write_all(&encoded)
            .and_then(|()| self.journal.sync_data())
            .map_err(|error| error.to_string())?;
        self.journal_bytes = self.journal_bytes.saturating_add(encoded.len() as u64);
        Ok(())
    }

    fn expire_idle(&mut self, now: i64, age: u64) -> usize {
        let expired = self
            .known
            .idle_paths(now, age)
            .into_iter()
            .filter(|path| !self.pending.contains_key(path))
            .collect::<Vec<_>>();
        for path in &expired {
            if let Err(error) = self.append(&RrdcachedJournalRecord::Expired { path: path.clone() })
            {
                tracing::error!(file = %path.display(), error = %error, "rrdcached_expiry_journal_failed");
                continue;
            }
            self.known.remove(path);
            self.suspended.remove(path);
        }
        expired
            .iter()
            .filter(|path| !self.known.contains(path))
            .count()
    }

    fn enqueue(&mut self, path: PathBuf, samples: &[&str]) -> Result<(), String> {
        let metadata =
            std::fs::metadata(&path).map_err(|error| format!("No such file: {error}"))?;
        if !metadata.is_file() {
            return Err(format!("Not a regular file: {}", path.display()));
        }
        let info = rondi::inspect_rrd_file(&path).map_err(|error| error.to_string())?;
        let mut last_timestamp = self
            .pending
            .get(&path)
            .and_then(|entries| entries.last())
            .and_then(|entry| entry.samples.last())
            .and_then(|sample| sample.split_once(':'))
            .map(|(timestamp, _)| rrdcached_update_timestamp(timestamp))
            .transpose()?
            .unwrap_or((info.last_update, info.last_update_usec));
        for sample in samples {
            let Some((timestamp, values)) = sample.split_once(':') else {
                return Err(format!("Cannot find timestamp in '{sample}'!"));
            };
            let timestamp = rrdcached_update_timestamp(timestamp)
                .map_err(|_| format!("Cannot find timestamp in '{sample}'!"))?;
            let values = values.split(':').collect::<Vec<_>>();
            if values.len() != info.data_sources.len()
                || values.iter().any(|value| {
                    !value.eq_ignore_ascii_case("U")
                        && rondi::parse_rrd_number(value).is_none_or(|number| !number.is_finite())
                })
            {
                return Err(format!("Invalid update value: {sample}"));
            }
            if timestamp <= last_timestamp {
                let timestamp_seconds = timestamp.0 as f64 + timestamp.1 as f64 / 1_000_000.0;
                let last_timestamp_seconds =
                    last_timestamp.0 as f64 + last_timestamp.1 as f64 / 1_000_000.0;
                return Err(format!(
                    "illegal attempt to update using time {:.6} when last update time is {:.6} (minimum one second step)",
                    timestamp_seconds, last_timestamp_seconds
                ));
            }
            last_timestamp = timestamp;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let samples = samples
            .iter()
            .map(|sample| (*sample).to_owned())
            .collect::<Vec<_>>();
        let added_bytes = pending_entry_bytes(&samples);
        if self.pending_bytes.saturating_add(added_bytes) > self.max_pending_bytes {
            return Err(format!(
                "rrdcached pending queue is full ({} of {} bytes)",
                self.pending_bytes, self.max_pending_bytes
            ));
        }
        self.append(&RrdcachedJournalRecord::Update {
            id,
            path: path.clone(),
            samples: samples.clone(),
        })?;
        self.known.insert(path.clone(), wall_time_seconds());
        let entries = self.pending.entry(path.clone()).or_default();
        if entries.len() == entries.capacity() {
            entries.reserve(self.allocation_chunk);
        }
        entries.push(PendingRrdUpdate { id, samples });
        self.pending_bytes = self.pending_bytes.saturating_add(added_bytes);
        self.schedule_path(&path, wall_time_seconds());
        Ok(())
    }
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    config: DatabaseConfig,
}
#[derive(Deserialize)]
struct UpdateBody {
    id: String,
    timestamp: i64,
    value: Option<f64>,
}
struct Pending {
    name: String,
    update: Update,
    id: String,
    reply: oneshot::Sender<Result<(), StoreError>>,
}

/// Run the local HTTP/JSON daemon. The Store's advisory lock owns its root for
/// this function's lifetime.
pub async fn run(args: ServerConfig) -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .try_init();
    if args.queue_capacity == 0 {
        return Err("queue capacity must be positive".into());
    }
    let store = Store::open(&args.root)?;
    let replayed = store.recover()?;
    tracing::info!(root = %args.root.display(), recovered = replayed, "server_started");
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if args.socket.exists() {
        use std::os::unix::fs::FileTypeExt;
        match UnixStream::connect(&args.socket).await {
            Ok(_) => {
                return Err(format!(
                    "socket is already accepting connections: {}",
                    args.socket.display()
                )
                .into());
            }
            Err(_)
                if std::fs::symlink_metadata(&args.socket)?
                    .file_type()
                    .is_socket() =>
            {
                std::fs::remove_file(&args.socket)?;
            }
            Err(error) => {
                return Err(format!(
                    "refusing to replace existing non-socket path {}: {error}",
                    args.socket.display()
                )
                .into());
            }
        }
    }
    let listener = UnixListener::bind(&args.socket)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o600))?;
    let (tx, mut rx) = mpsc::channel::<Pending>(args.queue_capacity);
    let store = Arc::new(store);
    let worker_store = Arc::clone(&store);
    let worker = tokio::task::spawn_blocking(move || {
        while let Some(request) = rx.blocking_recv() {
            let database = request.name.clone();
            let timestamp = request.update.timestamp;
            let request_id = request.id.clone();
            let result = worker_store.update_durable(&request.name, request.update, &request.id);
            match &result {
                Ok(()) => tracing::info!(database, timestamp, request_id, "write_durable"),
                Err(error) => {
                    tracing::error!(database, timestamp, request_id, error = %error, "write_failed")
                }
            }
            let _ = request.reply.send(result);
        }
    });

    let mut connections = JoinSet::new();
    let mut shutdown_signals = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
        )
    };
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let sender = tx.clone();
                let request_store = Arc::clone(&store);
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let sender = sender.clone();
                        let store = Arc::clone(&request_store);
                        async move { Ok::<_, Infallible>(handle(request, sender, store).await) }
                    });
                    if let Err(error) = http1::Builder::new()
                        .keep_alive(false)
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        tracing::warn!(error = %error, "request_failed");
                    }
                });
            }
            _ = shutdown_signals.0.recv() => { break; },
            _ = shutdown_signals.1.recv() => { break; },
        }
    }
    drop(tx);
    let drained = timeout(Duration::from_secs(30), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
        tracing::warn!("shutdown_drain_timeout");
    }
    worker.await?;
    let _ = std::fs::remove_file(args.socket);
    tracing::info!("server_stopped");
    Ok(())
}

/// Serve a journaled subset of the rrdcached ASCII protocol over a
/// permission-restricted Unix socket. UPDATE is acknowledged after the
/// accepted write is synced to the journal; file visibility follows a flush.
pub async fn run_rrdcached(args: RrdcachedConfig) -> Result<(), Box<dyn std::error::Error>> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if let Some(path) = args.log_file.as_deref() {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let _ = tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(file)
            .try_init();
    } else {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .try_init();
    }
    let _store = Store::open(&args.root)?;
    let root = std::fs::canonicalize(&args.root)?;
    // RRDtool strips trailing slashes from -b before joining request names.
    let echo_base = match args.root.to_string_lossy().trim_end_matches('/') {
        "" => root.clone(),
        base => PathBuf::from(base),
    };
    let mut queue = match args.journal_directory.as_deref() {
        Some(directory) => RrdcachedQueue::open_in_directory(
            &root,
            &std::fs::canonicalize(directory)?,
            args.max_pending_bytes,
            args.write_timeout_seconds,
        )?,
        None => RrdcachedQueue::open(&root, args.max_pending_bytes, args.write_timeout_seconds)?,
    };
    queue.allocation_chunk = args.allocation_chunk.max(1);
    let queue = Arc::new(Mutex::new(queue));
    let _pid_file = args
        .pid_file
        .as_deref()
        .map(PidFile::create)
        .transpose()?
        .flatten();
    let mut shutdown_signals = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::interrupt())?,
            signal(SignalKind::terminate())?,
        )
    };
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if args.socket.exists() {
        use std::os::unix::fs::FileTypeExt;
        match UnixStream::connect(&args.socket).await {
            Ok(_) => {
                return Err(format!("socket is already active: {}", args.socket.display()).into());
            }
            Err(_)
                if std::fs::symlink_metadata(&args.socket)?
                    .file_type()
                    .is_socket() =>
            {
                std::fs::remove_file(&args.socket)?;
            }
            Err(error) => return Err(format!("refusing to replace socket path: {error}").into()),
        }
    }
    let listener = UnixListener::bind(&args.socket)?;
    use std::os::unix::fs::PermissionsExt;
    if let Some(group) = args.socket_group {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(args.socket.as_os_str().as_bytes())?;
        // SAFETY: the CString is NUL terminated and remains alive for the call.
        let result = unsafe { libc::chown(path.as_ptr(), libc::getuid(), group as libc::gid_t) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    if let Some(socket_mode) = args
        .socket_mode
        .or_else(|| args.socket_group.map(|_| 0o760))
    {
        std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(socket_mode))?;
    }
    tracing::info!(root = %root.display(), socket = %args.socket.display(), "rrdcached_started");
    let stats = Arc::new(RrdcachedStats::default());
    let mut connections = JoinSet::new();
    let flush_interval = Duration::from_secs(args.flush_interval_seconds.max(1));
    let mut expiry_tick =
        tokio::time::interval_at(tokio::time::Instant::now() + flush_interval, flush_interval);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let root = root.clone();
                let echo_base = echo_base.clone();
                let stats = Arc::clone(&stats);
                let queue = Arc::clone(&queue);
                let no_overwrite = args.no_overwrite;
                let allow_recursive_mkdir = args.allow_recursive_mkdir;
                let socket_commands = args.socket_commands.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_rrdcached_connection(stream, root, echo_base, stats, queue, no_overwrite, allow_recursive_mkdir, socket_commands).await {
                        tracing::warn!(error = %error, "rrdcached_connection_failed");
                    }
                });
            }
            _ = expiry_tick.tick(), if args.flush_interval_seconds > 0 => {
                let paths = queue.lock().map(|mut queue| {
                    queue.schedule_eligible(wall_time_seconds());
                    queue.pending_order.iter().cloned().collect::<Vec<_>>()
                }).unwrap_or_default();
                flush_rrdcached_paths(paths, Arc::clone(&queue), Arc::clone(&stats), args.queue_threads).await;
                let mut queue = queue.lock().map_err(|_| "rrdcached queue lock poisoned")?;
                queue.expire_idle(wall_time_seconds(), args.flush_interval_seconds);
            }
            _ = shutdown_signals.0.recv() => break,
            _ = shutdown_signals.1.recv() => break,
        }
    }
    while connections.join_next().await.is_some() {}
    let paths = queue
        .lock()
        .map(|queue| queue.pending.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    flush_rrdcached_paths(
        paths,
        Arc::clone(&queue),
        Arc::clone(&stats),
        args.queue_threads,
    )
    .await;
    let _ = std::fs::remove_file(&args.socket);
    tracing::info!("rrdcached_stopped");
    Ok(())
}

async fn flush_rrdcached_paths(
    paths: Vec<PathBuf>,
    queue: Arc<Mutex<RrdcachedQueue>>,
    stats: Arc<RrdcachedStats>,
    worker_count: usize,
) {
    let worker_count = worker_count.max(1);
    for batch in paths.chunks(worker_count) {
        let mut tasks = JoinSet::new();
        // The blocking task must own the path after this batch's borrow ends.
        #[allow(clippy::unnecessary_to_owned)]
        for path in batch.iter().cloned() {
            let queue = Arc::clone(&queue);
            let stats = Arc::clone(&stats);
            tasks.spawn_blocking(move || flush_rrdcached_path(&path, &queue, &stats));
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "rrdcached_periodic_flush_failed")
                }
                Err(error) => tracing::error!(error = %error, "rrdcached_flush_worker_failed"),
            }
        }
    }
}

async fn read_bounded_async_line<R>(
    reader: &mut R,
    line: &mut String,
    max_bytes: usize,
) -> std::io::Result<usize>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            break;
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len().saturating_add(count) > max_bytes {
            let remaining = max_bytes.saturating_add(1).saturating_sub(bytes.len());
            let consume = remaining.min(available.len());
            bytes.extend_from_slice(&available[..consume]);
            let _ = available;
            reader.consume(consume);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "rrdcached request exceeds 1 MiB",
            ));
        }
        let complete = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            break;
        }
    }
    let length = bytes.len();
    *line = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(length)
}

#[allow(clippy::too_many_arguments)] // Per-connection settings fixed at startup.
async fn serve_rrdcached_connection(
    stream: UnixStream,
    root: PathBuf,
    echo_base: PathBuf,
    stats: Arc<RrdcachedStats>,
    queue: Arc<Mutex<RrdcachedQueue>>,
    no_overwrite: bool,
    allow_recursive_mkdir: bool,
    socket_commands: Option<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        if read_bounded_async_line(&mut reader, &mut line, 1024 * 1024).await? == 0 {
            return Ok(());
        }
        if line
            .split_ascii_whitespace()
            .next()
            .is_some_and(|command| command.eq_ignore_ascii_case("FETCHBIN"))
        {
            if socket_commands
                .as_deref()
                .is_some_and(|allowed| !allowed.iter().any(|candidate| candidate == "FETCHBIN"))
            {
                writer.write_all(b"-1 Permission denied.\n").await?;
                writer.flush().await?;
                continue;
            }
            let response = handle_rrdcached_fetchbin(&root, &line, &stats, &queue);
            writer.write_all(&response).await?;
            writer.flush().await?;
            continue;
        }
        let Some(response) = handle_rrdcached_line(
            &root,
            &echo_base,
            &line,
            &stats,
            &queue,
            no_overwrite,
            allow_recursive_mkdir,
            socket_commands.as_deref(),
        ) else {
            return Ok(());
        };
        let is_dump = line
            .split_ascii_whitespace()
            .next()
            .is_some_and(|command| command.eq_ignore_ascii_case("DUMP"));
        let dump_succeeded = is_dump && !response.starts_with("-1 ");
        writer.write_all(response.as_bytes()).await?;
        writer.flush().await?;
        if dump_succeeded {
            return Ok(());
        }
        if line.trim().eq_ignore_ascii_case("BATCH") && response.starts_with("0 Go ahead.") {
            let mut errors = Vec::new();
            let mut command_number = 0_u64;
            loop {
                line.clear();
                if read_bounded_async_line(&mut reader, &mut line, 1024 * 1024).await? == 0 {
                    return Ok(());
                }
                if line.trim() == "." {
                    break;
                }
                command_number += 1;
                if line.split_ascii_whitespace().next().is_some_and(|command| {
                    command.eq_ignore_ascii_case("DUMP")
                        || command.eq_ignore_ascii_case("TUNE")
                        || command.eq_ignore_ascii_case("FETCHBIN")
                }) {
                    let command = line.split_ascii_whitespace().next().unwrap_or_default();
                    errors.push(format!("{command_number} Can't use '{command}' here."));
                    continue;
                }
                match handle_rrdcached_line(
                    &root,
                    &echo_base,
                    &line,
                    &stats,
                    &queue,
                    no_overwrite,
                    allow_recursive_mkdir,
                    socket_commands.as_deref(),
                ) {
                    Some(command_response) if command_response.starts_with('-') => {
                        let message = command_response
                            .split_once(' ')
                            .map(|(_, message)| message.trim())
                            .unwrap_or(command_response.trim());
                        errors.push(format!("{command_number} {message}"));
                    }
                    None => return Ok(()),
                    _ => {}
                }
            }
            let mut batch_response = format!("{} errors\n", errors.len());
            for error in errors {
                batch_response.push_str(&error);
                batch_response.push('\n');
            }
            writer.write_all(batch_response.as_bytes()).await?;
            writer.flush().await?;
        }
    }
}

#[allow(clippy::too_many_arguments)] // Per-connection settings fixed at startup.
fn handle_rrdcached_line(
    root: &Path,
    echo_base: &Path,
    line: &str,
    stats: &RrdcachedStats,
    queue: &Mutex<RrdcachedQueue>,
    no_overwrite: bool,
    allow_recursive_mkdir: bool,
    socket_commands: Option<&[String]>,
) -> Option<String> {
    let mut fields = rrdcached_fields(line);
    let command = fields.first()?.to_ascii_uppercase();
    let raw_command = fields.remove(0);
    if let Some(allowed) = socket_commands {
        let permission_name = if command == "." { "BATCH" } else { &command };
        let known = matches!(
            permission_name,
            "UPDATE"
                | "WROTE"
                | "TUNE"
                | "DUMP"
                | "FLUSH"
                | "FLUSHALL"
                | "PENDING"
                | "FORGET"
                | "QUEUE"
                | "STATS"
                | "HELP"
                | "PING"
                | "BATCH"
                | "FETCH"
                | "FETCHBIN"
                | "INFO"
                | "FIRST"
                | "LAST"
                | "CREATE"
                | "LIST"
                | "SUSPEND"
                | "RESUME"
                | "SUSPENDALL"
                | "RESUMEALL"
                | "QUIT"
                | "."
        );
        if known
            && !matches!(permission_name, "HELP" | "QUIT")
            && !allowed.iter().any(|candidate| candidate == permission_name)
        {
            return Some("-1 Permission denied.\n".to_owned());
        }
    }
    let response = match command.as_str() {
        "QUIT" if fields.is_empty() => return None,
        "PING" if fields.is_empty() => "0 PONG\n".to_owned(),
        "UPDATE" => {
            stats.updates_received.fetch_add(1, Ordering::Relaxed);
            if fields.is_empty() {
                "-1 Usage: UPDATE <filename> <values> [<values> ...]\n".to_owned()
            } else if fields.len() == 1 {
                match resolve_rrdcached_path(root, &fields[0]) {
                    Ok(_) => "-1 No values updated.\n".to_owned(),
                    Err(error) => format!("-1 {error}\n"),
                }
            } else {
                match resolve_rrdcached_path(root, &fields[0]).and_then(|path| {
                    queue
                        .lock()
                        .map_err(|_| "rrdcached queue lock poisoned".to_owned())?
                        .enqueue(
                            path,
                            &fields[1..].iter().map(String::as_str).collect::<Vec<_>>(),
                        )
                }) {
                    Ok(()) => format!("0 errors, enqueued {} value(s).\n", fields.len() - 1),
                    Err(error) => format!("-1 {error}\n"),
                }
            }
        }
        "FLUSH" if fields.len() == 1 => {
            stats.flushes_received.fetch_add(1, Ordering::Relaxed);
            let echo = rrdcached_echo_path(echo_base, &fields[0]);
            match resolve_rrdcached_path(root, &fields[0]) {
                Ok(path) => {
                    let (known, suspended) = queue
                        .lock()
                        .map(|queue| (queue.known.contains(&path), queue.suspended.contains(&path)))
                        .unwrap_or_default();
                    match flush_rrdcached_path(&path, queue, stats) {
                        Ok(true) => format!("0 Successfully flushed {echo}.\n"),
                        Ok(false) if known || suspended => {
                            format!("0 Successfully flushed {echo}.\n")
                        }
                        Ok(false) => format!("0 Nothing to flush: {echo}.\n"),
                        Err(error) => format!("-1 {error}\n"),
                    }
                }
                _ => format!("-1 No such file: {echo}.\n"),
            }
        }
        "FLUSHALL" if fields.is_empty() => {
            stats.flushes_received.fetch_add(1, Ordering::Relaxed);
            let paths = queue
                .lock()
                .map(|queue| queue.pending.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            for path in paths {
                if let Err(error) = flush_rrdcached_path(&path, queue, stats) {
                    return Some(format!("-1 {error}\n"));
                }
            }
            "0 Started flush.\n".to_owned()
        }
        "FORGET" if fields.len() == 1 => match resolve_rrdcached_path(root, &fields[0]) {
            Ok(path) => match forget_rrdcached_path(&path, queue) {
                Ok(true) => "0 Gone!\n".to_owned(),
                Ok(false) => "-1 No such file or directory\n".to_owned(),
                Err(error) => format!("-1 {error}\n"),
            },
            Err(_) => "-1 No such file or directory\n".to_owned(),
        },
        "SUSPEND" if fields.len() == 1 => {
            let echo = rrdcached_echo_path(echo_base, &fields[0]);
            match resolve_rrdcached_path(root, &fields[0]) {
                Ok(path) => match suspend_rrdcached_path(&path, queue) {
                    Ok(SuspendResult::Changed) => format!("0 {echo} suspended\n"),
                    Ok(SuspendResult::Unchanged) => format!("0 {echo} already suspended\n"),
                    Err(()) => format!("-1 {echo} - No such file or directory\n"),
                },
                Err(_) => format!("-1 {echo} - No such file or directory\n"),
            }
        }
        "RESUME" if fields.len() == 1 => {
            let echo = rrdcached_echo_path(echo_base, &fields[0]);
            match resolve_rrdcached_path(root, &fields[0]) {
                Ok(path) => match resume_rrdcached_path(&path, queue) {
                    Ok(SuspendResult::Changed) => format!("0 {echo} resumed\n"),
                    Ok(SuspendResult::Unchanged) => format!("0 {echo} not suspended\n"),
                    Err(()) => format!("-1 {echo} - No such file or directory\n"),
                },
                Err(_) => format!("-1 {echo} - No such file or directory\n"),
            }
        }
        "SUSPENDALL" if fields.is_empty() => {
            let count = suspend_all_rrdcached_paths(queue);
            format!("0 {count} rrds suspend\n")
        }
        "RESUMEALL" if fields.is_empty() => {
            let count = resume_all_rrdcached_paths(queue);
            format!("0 {count} rrds resumed\n")
        }
        "PENDING" if fields.len() == 1 => {
            let path = rrdcached_pending_key(root, &fields[0]);
            let pending = queue
                .lock()
                .ok()
                .and_then(|queue| queue.pending.get(&path).cloned())
                .unwrap_or_default();
            let samples = pending
                .iter()
                .flat_map(|entry| entry.samples.iter())
                .cloned()
                .collect::<Vec<_>>();
            let mut response = format!("{} updates pending\n", samples.len());
            for sample in samples {
                response.push_str(&sample);
                response.push('\n');
            }
            response
        }
        "QUEUE" if fields.is_empty() => {
            let queue = match queue.lock() {
                Ok(queue) => queue,
                Err(_) => return Some("-1 rrdcached queue lock poisoned\n".to_owned()),
            };
            let mut body = String::new();
            for path in &queue.pending_order {
                let Some(entries) = queue.pending.get(path) else {
                    continue;
                };
                let samples = entries
                    .iter()
                    .map(|entry| entry.samples.len())
                    .sum::<usize>();
                body.push_str(&format!("{samples} {}\n", path.display()));
            }
            format!("{} in queue.\n{body}", body.lines().count())
        }
        "LAST" if fields.len() == 1 => match rrdcached_last(root, &fields[0], queue) {
            Ok(timestamp) => format!("0 {timestamp}\n"),
            Err(error) => format!("-1 {error}\n"),
        },
        "FIRST" if fields.len() == 2 => {
            let index = fields[1].parse::<usize>();
            match (resolve_rrdcached_path(root, &fields[0]), index) {
                (Ok(path), Ok(index)) => match rondi::first_rrd_time(path, index) {
                    Ok(timestamp) => format!("0 {timestamp}\n"),
                    Err(error) => format!("-1 {error}\n"),
                },
                (_, Err(_)) => format!("-1 Invalid index specified: {}\n", fields[1]),
                (Err(error), _) => format!("-1 {error}\n"),
            }
        }
        "INFO" if fields.len() == 1 => match rrdcached_info(root, echo_base, &fields[0]) {
            Ok(response) => response,
            Err(error) => format!("-1 RRD Error: {error}\n"),
        },
        "LIST" => {
            match rrdcached_list(root, &fields.iter().map(String::as_str).collect::<Vec<_>>()) {
                Ok(response) => response,
                Err(error) => format!("-1 {error}\n"),
            }
        }
        "FETCH" if fields.len() >= 2 => {
            match resolve_rrdcached_path(root, &fields[0]).and_then(|path| {
                flush_rrdcached_path(&path, queue, stats)?;
                rrdcached_fetch(root, &fields.iter().map(String::as_str).collect::<Vec<_>>())
            }) {
                Ok(body) => format!("{} Success\n{body}", body.lines().count()),
                Err(error) => format!("-1 {error}\n"),
            }
        }
        "STATS" if fields.is_empty() => {
            let queue = match queue.lock() {
                Ok(queue) => queue,
                Err(_) => return Some("-1 rrdcached queue lock poisoned\n".to_owned()),
            };
            format!(
                "9 Statistics follow\nQueueLength: {}\nUpdatesReceived: {}\nFlushesReceived: {}\nUpdatesWritten: {}\nDataSetsWritten: {}\nTreeNodesNumber: {}\nTreeDepth: {}\nJournalBytes: {}\nJournalRotate: 0\n",
                queue
                    .pending
                    .keys()
                    .filter(|path| !queue.suspended.contains(*path))
                    .count(),
                stats.updates_received.load(Ordering::Relaxed),
                stats.flushes_received.load(Ordering::Relaxed),
                stats.updates_written.load(Ordering::Relaxed),
                stats.datasets_written.load(Ordering::Relaxed),
                queue.known.len(),
                queue.known.height(),
                queue.journal_bytes,
            )
        }
        "CREATE" => match rrdcached_create(
            root,
            &fields.iter().map(String::as_str).collect::<Vec<_>>(),
            no_overwrite,
            allow_recursive_mkdir,
        ) {
            Ok(()) => "0 RRD created OK\n".to_owned(),
            Err(error) => format!("-1 {error}\n"),
        },
        "DUMP" if !fields.is_empty() => {
            match resolve_rrdcached_path(root, &fields[0]).and_then(|path| {
                flush_rrdcached_path(&path, queue, stats)?;
                // RRDtool 1.11.0's daemon handler reads only the filename and
                // always calls rrd_dump_cb_r with the default DTD header.
                // Trailing header options are therefore ignored on the wire.
                rondi::dump_rrd_file_with_header(path, rondi::RrdDumpHeader::Dtd)
                    .map_err(|error| error.to_string())
            }) {
                Ok(xml) => xml,
                Err(error) => format!("-1 {error}\n"),
            }
        }
        "DUMP" => "-1 Usage: DUMP <filename> [-h none|xsd|dtd]\n".to_owned(),
        "TUNE" if fields.len() >= 2 => {
            let argc = match fields[1].parse::<usize>() {
                Ok(argc) if (1..=65_536).contains(&argc) => argc,
                _ => {
                    return Some(format!(
                        "-1 Invalid argument count specified: {}\n",
                        fields[1]
                    ));
                }
            };
            if fields.len() - 2 > argc {
                return Some(format!("-1 Too many arguments (expected {argc})\n"));
            }
            if fields.len() - 2 != argc {
                return Some(format!("-1 Invalid argument count specified: {argc}\n"));
            }
            match resolve_rrdcached_path(root, &fields[0]).and_then(|path| {
                tune_rrdcached_file(
                    &path,
                    &fields[3..].iter().map(String::as_str).collect::<Vec<_>>(),
                )
            }) {
                Ok(()) => "0 Success\n".to_owned(),
                Err(error) => format!("-1 Got error {error}\n"),
            }
        }
        "TUNE" => "-1 Usage: TUNE <filename> [options]\n".to_owned(),
        "FIRST" => "-1 Usage: FIRST <filename> <rra index>\n".to_owned(),
        "BATCH" if fields.is_empty() => {
            "0 Go ahead.  End with dot '.' on its own line.\n".to_owned()
        }
        "BATCH" => "-1 Usage: BATCH\n".to_owned(),
        "HELP" => rrdcached_help(&fields.iter().map(String::as_str).collect::<Vec<_>>()),
        // Upstream handlers read only the fields they need, so extra
        // arguments are ignored and a missing filename gets the usage text.
        _ => {
            let needed = match command.as_str() {
                "PING" | "QUIT" | "QUEUE" | "STATS" | "FLUSHALL" | "SUSPENDALL" | "RESUMEALL" => {
                    Some(0)
                }
                "FLUSH" | "PENDING" | "FORGET" | "INFO" | "LAST" | "SUSPEND" | "RESUME" => Some(1),
                _ => None,
            };
            if let Some(needed) = needed.filter(|needed| fields.len() > *needed) {
                let mut retry = command;
                for field in &fields[..needed] {
                    retry.push(' ');
                    retry.push_str(&field.replace('\\', "\\\\").replace(' ', "\\ "));
                }
                return handle_rrdcached_line(
                    root,
                    echo_base,
                    &retry,
                    stats,
                    queue,
                    no_overwrite,
                    allow_recursive_mkdir,
                    socket_commands,
                );
            }
            match command.as_str() {
                "FETCH" => {
                    "-1 Usage: FETCH <file> <CF> [<start> [<end>] [<column>...]]\n".to_owned()
                }
                _ if needed == Some(1) => format!("-1 Usage: {command} <filename>\n"),
                "WROTE" => format!("-1 Can't use '{raw_command}' here.\n"),
                _ => format!("-1 Unknown command: {raw_command}\n"),
            }
        }
    };
    Some(response)
}

fn rrdcached_fields(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut escaped = false;
    for character in line.trim_end_matches(['\r', '\n']).chars() {
        if escaped {
            field.push(character);
            escaped = false;
        } else {
            match character {
                '\\' => escaped = true,
                ' ' => {
                    if !field.is_empty() {
                        fields.push(std::mem::take(&mut field));
                    }
                }
                _ => field.push(character),
            }
        }
    }
    if escaped {
        field.push('\\');
    }
    if !field.is_empty() {
        fields.push(field);
    }
    fields
}

fn rrdcached_help(fields: &[&str]) -> String {
    const SYNTAX: &[&str] = &[
        "UPDATE <filename> <values> [<values> ...]\n",
        "TUNE <filename> [options]\n",
        "DUMP <filename> [-h none|xsd|dtd]\n",
        "FLUSH <filename>\n",
        "FLUSHALL\n",
        "PENDING <filename>\n",
        "FORGET <filename>\n",
        "QUEUE\n",
        "STATS\n",
        "HELP [<command>]\n",
        "PING\n",
        "BATCH\n",
        "FETCH <file> <CF> [<start> [<end>] [<column>...]]\n",
        "FETCHBIN <file> <CF> [<start> [<end>] [<column>...]]\n",
        "INFO <filename>\n",
        "FIRST <filename> <rra index>\n",
        "LAST <filename>\n",
        "CREATE <filename> [-b start] [-s step] [-O] <DS definitions> <RRA definitions>\n",
        "LIST [RECURSIVE] /[<path>]\n",
        "SUSPEND <filename>\n",
        "RESUME <filename>\n",
        "SUSPENDALL\n",
        "RESUMEALL\n",
        "QUIT\n",
    ];

    if let [command] = fields {
        let command = command.to_ascii_uppercase();
        let entry = match command.as_str() {
            "UPDATE" => Some((
                0,
                "Adds the given file to the internal cache if it is not yet known and\nappends the given value(s) to the entry. See the rrdcached(1) manpage\nfor details.\n\nEach <values> has the following form:\n  <values> = <time>:<value>[:<value>[...]]\nSee the rrdupdate(1) manpage for details.\n",
            )),
            "TUNE" => Some((
                1,
                "Tunes the given file, takes the parameters as defined in rrdtool.\n",
            )),
            "DUMP" => Some((2, "Dumps the specified RRD to XML.\n")),
            "FLUSH" => Some((
                3,
                "Adds the given filename to the head of the update queue and returns\nafter it has been dequeued.\n",
            )),
            "FLUSHALL" => Some((
                4,
                "Triggers writing of all pending updates.  Returns immediately.\n",
            )),
            "PENDING" => Some((
                5,
                "Shows any 'pending' updates for a file, in order.\nThe updates shown have not yet been written to the underlying RRD file.\n",
            )),
            "FORGET" => Some((
                6,
                "Removes the file completely from the cache.\nAny pending updates for the file will be lost.\n",
            )),
            "QUEUE" => Some((
                7,
                "Shows all files in the output queue.\nThe output is zero or more lines in the following format:\n(where <num_vals> is the number of values to be written)\n\n<num_vals> <filename>\n",
            )),
            "STATS" => Some((
                8,
                "Returns some performance counters, see the rrdcached(1) manpage for\na description of the values.\n",
            )),
            "HELP" => Some((9, "")),
            "PING" => Some((10, "PING given, PONG returned\n")),
            "BATCH" => Some((
                11,
                "The 'BATCH' command permits the client to initiate a bulk load\n   of commands to rrdcached.\n\nUsage:\n\n    client: BATCH\n    server: 0 Go ahead.  End with dot '.' on its own line.\n    client: command #1\n    client: command #2\n    client: ... and so on\n    client: .\n    server: 2 errors\n    server: 7 message for command #7\n    server: 9 message for command #9\n\nFor more information, consult the rrdcached(1) documentation.\n",
            )),
            "FETCH" => Some((
                12,
                "The 'FETCH' can be used by the client to retrieve values from an RRD file.\n",
            )),
            "FETCHBIN" => Some((
                13,
                "The 'FETCHBIN' can be used by the client to retrieve values from an RRD file.\n",
            )),
            "INFO" => Some((
                14,
                "The INFO command retrieves information about a specified RRD file.\nThis is returned in standard rrdinfo format, a sequence of lines\nwith the format <keyname> = <value>\nNote that this is the data as of the last update of the RRD file itself,\nnot the last time data was received via rrdcached, so there may be pending\nupdates in the queue.  If this bothers you, then first run a FLUSH.\n",
            )),
            "FIRST" => Some((
                15,
                "The FIRST command retrieves the first data time for a specified RRA in\nan RRD file.\n",
            )),
            "LAST" => Some((
                16,
                "The LAST command retrieves the last update time for a specified RRD file.\nNote that this is the time of the last update of the RRD file itself, not\nthe last time data was received via rrdcached, so there may be pending\nupdates in the queue.  If this bothers you, then first run a FLUSH.\n",
            )),
            "CREATE" => Some((
                17,
                "The CREATE command will create an RRD file, overwriting any existing file\nunless the -O option is given or rrdcached was started with the -O option.\nThe start parameter needs to be in seconds since 1/1/70 (AT-style syntax is\nnot acceptable) and the step is in seconds (default is 300).\nThe DS and RRA definitions are as for the 'rrdtool create' command.\n",
            )),
            "LIST" => Some((
                18,
                "This command lists the RRD files in the storage base directory (/).\nNote that this is the list of RRD files on storage as of the last update.\nThere may be pending updates in the queue, so a FLUSH may have to be run\nbeforehand.\nWhen invoked with 'LIST RECURSIVE /<path>' it will behave similarly to\n'ls -R' but limited to rrd files (listing all the rrd bases in the subtree\n of <path>, skipping empty directories).\n",
            )),
            "SUSPEND" => Some((
                19,
                "The SUSPEND command will suspend writing to an RRD file. While a file is\nsuspended, all metrics for it are cached in memory until RESUME is called\nfor that file or RESUMEALL is called.\n",
            )),
            "RESUME" => Some((
                20,
                "The RESUME command will resume writing to an RRD file previously suspended\nby SUSPEND or SUSPENDALL.\n",
            )),
            "SUSPENDALL" => Some((
                21,
                "The SUSPENDALL command will suspend writing to all RRD files. While a file\nis suspended, all metrics for it are cached in memory until RESUME is called\nfor that file or RESUMEALL is called.\n",
            )),
            "RESUMEALL" => Some((
                22,
                "The RESUMEALL command will resume writing to all RRD files previously suspended.\n",
            )),
            "QUIT" => Some((23, "Disconnect from rrdcached.\n")),
            _ => None,
        };
        if let Some((index, help)) = entry {
            let mut body = format!("Usage: {}\n", SYNTAX[index]);
            if command != "HELP" {
                body.push_str(help);
                body.push('\n');
            }
            return format!(
                "{} Help for {command}\n{body}",
                body.bytes().filter(|b| *b == b'\n').count()
            );
        }
    }

    let mut response = format!("{} Command overview\n", SYNTAX.len());
    response.push_str(&SYNTAX.concat());
    response
}

fn rrdcached_info(root: &Path, echo_base: &Path, filename: &str) -> Result<String, String> {
    let path = resolve_rrdcached_path(root, filename)?;
    let echo = rrdcached_echo_path(echo_base, filename);
    let info = rondi::inspect_rrd_file(&path).map_err(|error| error.to_string())?;
    let mut lines = vec![
        format!("filename 2 {echo}"),
        format!("rrd_version 2 {}", info.version),
        format!("step 1 {}", info.step),
        format!("last_update 1 {}", info.last_update),
        format!("header_size 1 {}", info.header_size),
    ];
    for (index, source) in info.data_sources.iter().enumerate() {
        lines.push(format!("ds[{}].index 1 {index}", source.name));
        lines.push(format!("ds[{}].type 2 {}", source.name, source.kind));
        lines.push(format!(
            "ds[{}].minimal_heartbeat 1 {}",
            source.name, source.heartbeat
        ));
        lines.push(format!(
            "ds[{}].min 0 {}",
            source.name,
            source
                .minimum
                .map_or_else(|| "NaN".to_owned(), rrdcached_float)
        ));
        lines.push(format!(
            "ds[{}].max 0 {}",
            source.name,
            source
                .maximum
                .map_or_else(|| "NaN".to_owned(), rrdcached_float)
        ));
        lines.push(format!(
            "ds[{}].last_ds 2 {}",
            source.name, source.last_value
        ));
        lines.push(format!(
            "ds[{}].value 0 {}",
            source.name,
            rrdcached_float(source.pdp_value)
        ));
        lines.push(format!(
            "ds[{}].unknown_sec 1 {}",
            source.name, source.unknown_seconds
        ));
    }
    for (index, archive) in info.archives.iter().enumerate() {
        lines.push(format!("rra[{index}].cf 2 {}", archive.consolidation));
        lines.push(format!("rra[{index}].rows 1 {}", archive.rows));
        lines.push(format!("rra[{index}].cur_row 1 {}", archive.current_row));
        lines.push(format!(
            "rra[{index}].pdp_per_row 1 {}",
            archive.pdp_per_row
        ));
        lines.push(format!(
            "rra[{index}].xff 0 {}",
            rrdcached_float(archive.xff)
        ));
        for (ds_index, prep) in archive.cdp_prep.iter().enumerate() {
            lines.push(format!(
                "rra[{index}].cdp_prep[{ds_index}].value 0 {}",
                rrdcached_float(prep.value)
            ));
            lines.push(format!(
                "rra[{index}].cdp_prep[{ds_index}].unknown_datapoints 1 {}",
                prep.unknown_datapoints
            ));
        }
    }
    let count = lines.len();
    Ok(format!(
        "{count} Info for {echo} follows\n{}\n",
        lines.join("\n")
    ))
}

fn rrdcached_last(
    root: &Path,
    filename: &str,
    queue: &Mutex<RrdcachedQueue>,
) -> Result<i64, String> {
    let path = resolve_rrdcached_path(root, filename)?;
    let info = rondi::inspect_rrd_file(&path).map_err(|error| error.to_string())?;
    let pending_timestamp = queue
        .lock()
        .map_err(|_| "rrdcached queue lock poisoned".to_owned())?
        .pending
        .get(&path)
        .and_then(|entries| entries.last())
        .and_then(|entry| entry.samples.last())
        .and_then(|sample| sample.split_once(':'))
        .map(|(timestamp, _)| {
            timestamp
                .parse::<f64>()
                .map(|timestamp| timestamp as i64)
                .map_err(|error| format!("Invalid timestamp in pending update: {error}"))
        })
        .transpose()?;
    let mut timestamp = pending_timestamp.unwrap_or(info.last_update);
    let step = i64::try_from(info.step).map_err(|error| error.to_string())?;
    timestamp -= timestamp % step;
    if timestamp < 1 {
        return Err("Error: rrdcached: Invalid timestamp returned".to_owned());
    }
    Ok(timestamp)
}

fn rrdcached_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    let formatted = format!("{value:.10e}");
    let Some((mantissa, exponent)) = formatted.split_once('e') else {
        return formatted;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return formatted;
    };
    format!("{mantissa}e{exponent:+03}")
}

fn pending_entry_bytes(samples: &[String]) -> usize {
    std::mem::size_of::<PendingRrdUpdate>().saturating_add(
        samples
            .iter()
            .map(|sample| std::mem::size_of::<String>().saturating_add(sample.len()))
            .fold(0_usize, usize::saturating_add),
    )
}

fn tune_rrdcached_file(path: &Path, arguments: &[&str]) -> Result<(), String> {
    let info = rondi::inspect_rrd_file(path).map_err(|error| error.to_string())?;
    let mut changes = info
        .data_sources
        .iter()
        .map(|source| rondi::RrdDataSourceTune {
            name: source.name.clone(),
            kind: None,
            new_name: None,
            heartbeat: None,
            minimum: None,
            maximum: None,
        })
        .collect::<Vec<_>>();
    let mut names = info
        .data_sources
        .iter()
        .map(|source| source.name.clone())
        .collect::<Vec<_>>();
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index];
        let (setting, inline_value) = if let Some(value) = argument.strip_prefix("--heartbeat=") {
            ("heartbeat", Some(value))
        } else if let Some(value) = argument.strip_prefix("--data-source-type=") {
            ("type", Some(value))
        } else if let Some(value) = argument.strip_prefix("--data-source-rename=") {
            ("rename", Some(value))
        } else if let Some(value) = argument.strip_prefix("--minimum=") {
            ("minimum", Some(value))
        } else if let Some(value) = argument.strip_prefix("--maximum=") {
            ("maximum", Some(value))
        } else {
            let setting = match argument {
                "-h" | "--heartbeat" => "heartbeat",
                "-d" | "--data-source-type" => "type",
                "-r" | "--data-source-rename" => "rename",
                "-i" | "--minimum" => "minimum",
                "-a" | "--maximum" => "maximum",
                unsupported => return Err(format!("unsupported tune option {unsupported}")),
            };
            index += 1;
            let value = arguments
                .get(index)
                .ok_or_else(|| format!("tune {setting} requires a value"))?;
            (setting, Some(*value))
        };
        let value = inline_value.unwrap_or_default();
        if setting == "rename" {
            let (old_name, new_name) = value
                .split_once(':')
                .ok_or_else(|| "invalid arguments for data source rename".to_owned())?;
            let ds_index = names
                .iter()
                .position(|name| name == old_name)
                .ok_or_else(|| format!("No DS called {old_name}"))?;
            changes[ds_index].new_name = Some(new_name.to_owned());
            names[ds_index] = new_name.to_owned();
        } else {
            let (name, value) = value
                .split_once(':')
                .ok_or_else(|| format!("invalid arguments for {setting}"))?;
            let ds_index = names
                .iter()
                .position(|source| source == name)
                .ok_or_else(|| format!("No DS called {name}"))?;
            match setting {
                "type" => changes[ds_index].kind = Some(value.to_owned()),
                "heartbeat" => {
                    changes[ds_index].heartbeat = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| "invalid arguments for heartbeat".to_owned())?,
                    );
                }
                "minimum" | "maximum" => {
                    let bound = if value == "U" {
                        rondi::RrdTuneBound::Unbounded
                    } else {
                        rondi::RrdTuneBound::Value(
                            value
                                .parse::<f64>()
                                .map_err(|_| format!("invalid arguments for {setting} ds value"))?,
                        )
                    };
                    if setting == "minimum" {
                        changes[ds_index].minimum = Some(bound);
                    } else {
                        changes[ds_index].maximum = Some(bound);
                    }
                }
                _ => unreachable!(),
            }
        }
        index += 1;
    }
    rondi::tune_rrd_data_sources(path, &changes).map_err(|error| error.to_string())
}

fn rrdcached_list(root: &Path, fields: &[&str]) -> Result<String, String> {
    let (recursive, requested) = match fields {
        [path] => (false, *path),
        ["RECURSIVE", path] => (true, *path),
        _ => return Err("Usage: LIST [RECURSIVE] /[<path>]".to_owned()),
    };
    if !requested.starts_with('/') {
        return Err("Usage: LIST [RECURSIVE] /[<path>]".to_owned());
    }
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let relative = requested.trim_start_matches('/');
    let directory = root.join(relative);
    let canonical = std::fs::canonicalize(&directory)
        .map_err(|error| format!("List {}: {error}", directory.display()))?;
    if !canonical.starts_with(&root) {
        return Err(format!("Cannot read: {}", directory.display()));
    }
    let mut entries = Vec::new();
    collect_rrdcached_list(&root, &canonical, recursive, &mut entries)?;
    let body = entries
        .iter()
        .map(|path| format!("{path}\n"))
        .collect::<String>();
    Ok(format!("{} RRDs\n{body}", entries.len()))
}

fn collect_rrdcached_list(
    root: &Path,
    directory: &Path,
    recursive: bool,
    entries: &mut Vec<String>,
) -> Result<(), String> {
    for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            if recursive {
                collect_rrdcached_list(root, &path, recursive, entries)?;
            } else {
                let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
                entries.push(relative.to_string_lossy().replace('\\', "/"));
            }
        } else if metadata.is_file() && path.extension().is_some_and(|ext| ext == "rrd") {
            let canonical = std::fs::canonicalize(&path).map_err(|error| error.to_string())?;
            if canonical.starts_with(root) {
                let relative = canonical
                    .strip_prefix(root)
                    .map_err(|error| error.to_string())?;
                entries.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

fn rrdcached_fetch(root: &Path, fields: &[&str]) -> Result<String, String> {
    let (result, selected) = rrdcached_fetch_data(root, fields)?;
    let mut body = format!(
        "FlushVersion: 1\nStart: {}\nEnd: {}\nStep: {}\nDSCount: {}\nDSName: {}\n",
        result.start,
        result.end,
        result.step,
        selected.len(),
        selected
            .iter()
            .map(|index| result.data_sources[*index].as_str())
            .collect::<Vec<_>>()
            .join(" ")
    );
    for row in result.rows {
        body.push_str(&format!("{:10}:", row.timestamp));
        for index in &selected {
            let value = row.values[*index].map_or_else(|| "nan".to_owned(), format_c_exponent17);
            body.push_str(&format!(" {value}"));
        }
        body.push('\n');
    }
    Ok(body)
}

/// C `%0.17e`: Rust omits the exponent sign and padding that clients parse.
fn format_c_exponent17(value: f64) -> String {
    if !value.is_finite() {
        return value.to_string();
    }
    let text = format!("{value:.17e}");
    let (mantissa, exponent) = text.split_once('e').expect("scientific notation has e");
    let exponent = exponent.parse::<i32>().expect("valid scientific exponent");
    format!("{mantissa}e{exponent:+03}")
}

fn rrdcached_fetch_data(
    root: &Path,
    fields: &[&str],
) -> Result<(rondi::RrdFetchResult, Vec<usize>), String> {
    if fields.len() < 2 {
        return Err("Usage: FETCH <file> <CF> [<start> [<end>] [<column>...]]".to_owned());
    }
    let path = resolve_rrdcached_path(root, fields[0])?;
    let info = rondi::inspect_rrd_file(&path).map_err(|error| error.to_string())?;
    let start = fields
        .get(2)
        .map(|value| value.parse::<i64>().map_err(|error| error.to_string()))
        .transpose()?
        .unwrap_or(info.last_update.saturating_sub(86_400));
    let end = fields
        .get(3)
        .map(|value| value.parse::<i64>().map_err(|error| error.to_string()))
        .transpose()?
        .unwrap_or(info.last_update);
    let result = rondi::fetch_rrd_file(&path, fields[1], start, end, 1)
        .map_err(|error| error.to_string())?;
    let selected = if fields.len() > 4 {
        fields[4..]
            .iter()
            .map(|name| {
                result
                    .data_sources
                    .iter()
                    .position(|source| source == name)
                    .ok_or_else(|| format!("Unknown data source: {name}"))
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        (0..result.data_sources.len()).collect::<Vec<_>>()
    };
    Ok((result, selected))
}

fn handle_rrdcached_fetchbin(
    root: &Path,
    line: &str,
    stats: &RrdcachedStats,
    queue: &Mutex<RrdcachedQueue>,
) -> Vec<u8> {
    let fields = rrdcached_fields(line)
        .into_iter()
        .skip(1)
        .collect::<Vec<_>>();
    if fields.len() < 2 {
        return b"-1 Usage: FETCHBIN <file> <CF> [<start> [<end>] [<column>...]]\n".to_vec();
    }
    let fetched = resolve_rrdcached_path(root, &fields[0]).and_then(|path| {
        flush_rrdcached_path(&path, queue, stats)?;
        rrdcached_fetch_data(root, &fields.iter().map(String::as_str).collect::<Vec<_>>())
    });
    let (result, selected) = match fetched {
        Ok(result) => result,
        Err(error) => return format!("-1 {error}\n").into_bytes(),
    };
    let mut response = format!("{} Success\n", selected.len() + 5).into_bytes();
    response.extend_from_slice(b"FlushVersion: 1\n");
    response.extend_from_slice(format!("Start: {}\n", result.start).as_bytes());
    response.extend_from_slice(format!("End: {}\n", result.end).as_bytes());
    response.extend_from_slice(format!("Step: {}\n", result.step).as_bytes());
    response.extend_from_slice(format!("DSCount: {}\n", selected.len()).as_bytes());
    let endian = if cfg!(target_endian = "big") {
        "BIG"
    } else {
        "LITTLE"
    };
    for source_index in selected {
        let source_name = &result.data_sources[source_index];
        response.extend_from_slice(
            format!(
                "DSName-{source_name}: BinaryData {} {} {endian}\n",
                result.rows.len(),
                std::mem::size_of::<f64>()
            )
            .as_bytes(),
        );
        for row in &result.rows {
            let value = row.values[source_index].unwrap_or_else(rrd_nan);
            response.extend_from_slice(&value.to_ne_bytes());
        }
        response.push(b'\n');
    }
    response
}

#[inline]
fn rrd_nan() -> f64 {
    #[cfg(target_arch = "x86_64")]
    {
        f64::from_bits(0xfff8_0000_0000_0000)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        f64::NAN
    }
}

fn resolve_rrdcached_path(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let requested = Path::new(requested);
    let candidate = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| error.to_string())?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return Err("Access denied: path is outside the configured base directory".to_owned());
    }
    Ok(canonical)
}

/// RRDtool echoes `-b` as given joined with the name the client sent, not
/// the resolved file; access is still checked on the resolved path.
fn rrdcached_echo_path(echo_base: &Path, requested: &str) -> String {
    if requested.starts_with('/') {
        requested.to_owned()
    } else {
        format!("{}/{requested}", echo_base.display())
    }
}

fn rrdcached_pending_key(root: &Path, requested: &str) -> PathBuf {
    let path = Path::new(requested);
    if path.is_absolute() {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    } else {
        std::fs::canonicalize(root.join(path)).unwrap_or_else(|_| root.join(path))
    }
}

#[derive(Clone, Copy)]
enum SuspendResult {
    Changed,
    Unchanged,
}

fn suspend_rrdcached_path(path: &Path, queue: &Mutex<RrdcachedQueue>) -> Result<SuspendResult, ()> {
    let mut queue = queue.lock().map_err(|_| ())?;
    if !queue.known.contains(path) {
        return Err(());
    }
    Ok(if queue.suspended.insert(path.to_path_buf()) {
        SuspendResult::Changed
    } else {
        SuspendResult::Unchanged
    })
}

fn resume_rrdcached_path(path: &Path, queue: &Mutex<RrdcachedQueue>) -> Result<SuspendResult, ()> {
    let mut queue = queue.lock().map_err(|_| ())?;
    if !queue.known.contains(path) {
        return Err(());
    }
    Ok(if queue.suspended.remove(path) {
        SuspendResult::Changed
    } else {
        SuspendResult::Unchanged
    })
}

fn suspend_all_rrdcached_paths(queue: &Mutex<RrdcachedQueue>) -> usize {
    let Ok(mut queue) = queue.lock() else {
        return 0;
    };
    let known = queue.known.paths();
    known
        .into_iter()
        .filter(|path| queue.suspended.insert(path.clone()))
        .count()
}

fn resume_all_rrdcached_paths(queue: &Mutex<RrdcachedQueue>) -> usize {
    let Ok(mut queue) = queue.lock() else {
        return 0;
    };
    let count = queue.suspended.len();
    queue.suspended.clear();
    count
}

fn forget_rrdcached_path(path: &Path, queue: &Mutex<RrdcachedQueue>) -> Result<bool, String> {
    let mut queue = queue
        .lock()
        .map_err(|_| "rrdcached queue lock poisoned".to_owned())?;
    if !queue.known.contains(path) {
        return Ok(false);
    }
    let ids = queue
        .pending
        .get(path)
        .into_iter()
        .flatten()
        .map(|entry| entry.id)
        .collect::<Vec<_>>();
    queue.append(&RrdcachedJournalRecord::Forgotten {
        ids,
        path: Some(path.to_path_buf()),
    })?;
    let entries = queue.pending.remove(path).unwrap_or_default();
    queue
        .pending_order
        .retain(|pending_path| pending_path != path);
    let removed_bytes = entries
        .iter()
        .map(|entry| pending_entry_bytes(&entry.samples))
        .fold(0_usize, usize::saturating_add);
    queue.pending_bytes = queue.pending_bytes.saturating_sub(removed_bytes);
    queue.known.remove(path);
    queue.suspended.remove(path);
    Ok(true)
}

fn flush_rrdcached_path(
    path: &Path,
    queue: &Mutex<RrdcachedQueue>,
    stats: &RrdcachedStats,
) -> Result<bool, String> {
    let entries = {
        let queue = queue
            .lock()
            .map_err(|_| "rrdcached queue lock poisoned".to_owned())?;
        if queue.suspended.contains(path) {
            return Ok(false);
        }
        queue.pending.get(path).cloned()
    };
    let Some(entries) = entries else {
        return Ok(false);
    };
    for entry in entries {
        let samples = entry.samples.iter().map(String::as_str).collect::<Vec<_>>();
        let datasets = update_rrdcached_file(path, &samples)?;
        let entry_bytes = pending_entry_bytes(&entry.samples);
        let mut queue = queue
            .lock()
            .map_err(|_| "rrdcached queue lock poisoned".to_owned())?;
        queue.append(&RrdcachedJournalRecord::Flushed { id: entry.id })?;
        queue.known.mark_flushed(path, wall_time_seconds());
        if let Some(pending) = queue.pending.get_mut(path) {
            pending.retain(|candidate| candidate.id != entry.id);
            if pending.is_empty() {
                queue.pending.remove(path);
                queue
                    .pending_order
                    .retain(|pending_path| pending_path != path);
            }
        }
        queue.pending_bytes = queue.pending_bytes.saturating_sub(entry_bytes);
        stats
            .updates_written
            .fetch_add(entry.samples.len() as u64, Ordering::Relaxed);
        stats
            .datasets_written
            .fetch_add(datasets, Ordering::Relaxed);
    }
    Ok(true)
}

fn rrdcached_update_timestamp(value: &str) -> Result<(i64, u64), String> {
    let timestamp = rondi::parse_rrd_number(value)
        .filter(|timestamp| timestamp.is_finite())
        .ok_or_else(|| "invalid numeric timestamp".to_owned())?;
    if !timestamp.is_finite() || timestamp < i64::MIN as f64 || timestamp >= i64::MAX as f64 {
        return Err("timestamp is outside the supported range".to_owned());
    }
    let mut seconds = timestamp.floor() as i64;
    let mut microseconds = ((timestamp - seconds as f64) * 1_000_000.0) as u64;
    if microseconds >= 1_000_000 {
        seconds = seconds
            .checked_add(1)
            .ok_or_else(|| "timestamp is outside the supported range".to_owned())?;
        microseconds = 0;
    }
    Ok((seconds, microseconds))
}

fn update_rrdcached_file(path: &Path, samples: &[&str]) -> Result<u64, String> {
    let info = rondi::inspect_rrd_file(path).map_err(|error| error.to_string())?;
    let mut last_update = (info.last_update, info.last_update_usec);
    let mut count = 0;
    for sample in samples {
        let (timestamp, values) = sample
            .split_once(':')
            .ok_or_else(|| format!("Invalid update value: {sample}"))?;
        let (timestamp, timestamp_usec) = rrdcached_update_timestamp(timestamp)
            .map_err(|error| format!("Invalid timestamp in {sample}: {error}"))?;
        if (timestamp, timestamp_usec) <= last_update {
            continue;
        }
        let values = values
            .split(':')
            .map(|value| {
                if value == "U" {
                    Ok(None)
                } else {
                    value
                        .parse::<f64>()
                        .map(|_| Some(value))
                        .map_err(|error| format!("Invalid data value {value}: {error}"))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        if values.len() != info.data_sources.len() {
            return Err(format!(
                "Expected {} data values, got {}",
                info.data_sources.len(),
                values.len()
            ));
        }
        rondi::update_rrd_raw_values_precise(path, timestamp, timestamp_usec, &values)
            .map_err(|error| error.to_string())?;
        last_update = (timestamp, timestamp_usec);
        count += values.len() as u64;
    }
    Ok(count)
}

fn rrdcached_create(
    root: &Path,
    fields: &[&str],
    no_overwrite_default: bool,
    allow_recursive_mkdir: bool,
) -> Result<(), String> {
    if fields.len() < 3 {
        return Err(
            "Usage: CREATE <filename> [-b start] [-s step] [-O] <DS definitions> <RRA definitions>"
                .to_owned(),
        );
    }
    let requested = Path::new(fields[0]);
    if requested
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err("Access denied: parent traversal is not allowed".to_owned());
    }
    let output = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };
    if !output.starts_with(root) {
        return Err("Access denied: path is outside the configured base directory".to_owned());
    }
    if let Some(parent) = output.parent() {
        if !parent.exists() && !allow_recursive_mkdir {
            return Err(format!(
                "No permission to recursively create: {}\nDid you pass -R to the daemon?",
                parent.display()
            ));
        }
        if allow_recursive_mkdir {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
    }
    let canonical_parent = std::fs::canonicalize(output.parent().ok_or("invalid output path")?)
        .map_err(|error| error.to_string())?;
    if !canonical_parent.starts_with(root) {
        return Err(
            "Access denied: parent resolves outside the configured base directory".to_owned(),
        );
    }
    let output = canonical_parent.join(output.file_name().ok_or("invalid output filename")?);
    let mut step = 300_u64;
    let mut start = 1_000_000_000_i64;
    let mut no_overwrite = no_overwrite_default;
    let mut definitions = Vec::new();
    let mut index = 1;
    while index < fields.len() {
        match fields[index] {
            "-s" | "--step" => {
                index += 1;
                step = fields
                    .get(index)
                    .ok_or("missing step")?
                    .parse()
                    .map_err(|_| "invalid step")?;
            }
            "-b" | "--start" => {
                index += 1;
                start = fields
                    .get(index)
                    .ok_or("missing start")?
                    .parse()
                    .map_err(|_| "invalid start")?;
            }
            "-O" | "--no-overwrite" => no_overwrite = true,
            option if option.starts_with('-') => {
                return Err(format!("unsupported CREATE option: {option}"));
            }
            definition => definitions.push(definition.to_owned()),
        }
        index += 1;
    }
    let sources = definitions
        .iter()
        .filter(|line| line.starts_with("DS:"))
        .cloned()
        .collect::<Vec<_>>();
    let archives = definitions
        .iter()
        .filter(|line| line.starts_with("RRA:"))
        .cloned()
        .collect::<Vec<_>>();
    if sources.len() + archives.len() != definitions.len() {
        return Err("CREATE only supports DS and RRA definitions".to_owned());
    }
    rondi::create_rrd_file(output, start, step, &sources, &archives, no_overwrite)
        .map_err(|error| error.to_string())
}

async fn handle(
    request: Request<Incoming>,
    sender: mpsc::Sender<Pending>,
    store: Arc<Store>,
) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let body = match Limited::new(body, 1024 * 1024).collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => {
            return api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                "request body exceeds 1 MiB",
            );
        }
    };
    let path = parts.uri.path();
    match (parts.method.as_str(), path) {
        ("GET", "/v1/health") => response(
            StatusCode::OK,
            serde_json::json!({"status":"ok","api_version":1}),
        ),
        ("POST", "/v1/databases") => {
            let request: CreateBody = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(error) => {
                    return api_error(StatusCode::BAD_REQUEST, "invalid_json", &error.to_string());
                }
            };
            match store.create(&request.name, request.config) {
                Ok(()) => response(StatusCode::CREATED, serde_json::json!({"status":"created"})),
                Err(error) => store_error(error),
            }
        }
        ("POST", _) if path.starts_with("/v1/databases/") && path.ends_with("/updates") => {
            let name = path
                .trim_start_matches("/v1/databases/")
                .trim_end_matches("/updates")
                .trim_end_matches('/');
            let request: UpdateBody = match serde_json::from_slice(&body) {
                Ok(request) => request,
                Err(error) => {
                    return api_error(StatusCode::BAD_REQUEST, "invalid_json", &error.to_string());
                }
            };
            let (reply, result) = oneshot::channel();
            let pending = Pending {
                name: name.into(),
                update: Update {
                    timestamp: request.timestamp,
                    value: request.value,
                },
                id: request.id,
                reply,
            };
            if let Err(error) = sender.try_send(pending) {
                return match error {
                    mpsc::error::TrySendError::Full(_) => response(
                        StatusCode::TOO_MANY_REQUESTS,
                        serde_json::json!({"error":{"code":"queue_full","message":"retry request"}}),
                    ),
                    mpsc::error::TrySendError::Closed(_) => response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        serde_json::json!({"error":{"code":"worker_unavailable","message":"write worker stopped"}}),
                    ),
                };
            }
            match result.await {
                Ok(Ok(())) => response(StatusCode::OK, serde_json::json!({"status":"durable"})),
                Ok(Err(error)) => store_error(error),
                Err(_) => response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({"error":{"code":"worker_unavailable","message":"write worker stopped"}}),
                ),
            }
        }
        ("GET", _) if path.starts_with("/v1/databases/") && path.ends_with("/points") => {
            let name = path
                .trim_start_matches("/v1/databases/")
                .trim_end_matches("/points")
                .trim_end_matches('/');
            match store.fetch(name) {
                Ok(result) => response(
                    StatusCode::OK,
                    serde_json::to_value(result).unwrap_or_default(),
                ),
                Err(error) => store_error(error),
            }
        }
        _ => response(
            StatusCode::NOT_FOUND,
            serde_json::json!({"error":{"code":"not_found","message":"unknown endpoint"}}),
        ),
    }
}

fn response(status: StatusCode, value: serde_json::Value) -> Response<Full<Bytes>> {
    let body = Bytes::from(serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()));
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(body))
        .expect("valid JSON response")
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response<Full<Bytes>> {
    response(
        status,
        serde_json::json!({"error":{"code":code,"message":message}}),
    )
}

fn store_error(error: StoreError) -> Response<Full<Bytes>> {
    let (status, code) = match &error {
        StoreError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        StoreError::AlreadyExists(_) => (StatusCode::CONFLICT, "already_exists"),
        StoreError::OutOfOrder { .. }
        | StoreError::RrdTimestamp(_)
        | StoreError::RequestIdConflict
        | StoreError::Owned => (StatusCode::CONFLICT, "conflict"),
        StoreError::InvalidName
        | StoreError::InvalidConfig(_)
        | StoreError::InvalidValue
        | StoreError::RrdExpression(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
        StoreError::Io(_) => (StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
        StoreError::FormatVersion(_)
        | StoreError::RrdFormat(_)
        | StoreError::RrdUnsupported(_)
        | StoreError::Json(_) => (StatusCode::INTERNAL_SERVER_ERROR, "storage_corrupt"),
    };
    response(
        status,
        serde_json::json!({"error":{"code":code,"message":error.to_string()}}),
    )
}

#[cfg(test)]
mod rrdcached_queue_tests {
    use super::*;

    #[test]
    fn private_journal_refuses_symlink_and_preserves_target() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("victim");
        std::fs::write(&target, b"do not replay").unwrap();
        let journal = temp.path().join(".rrdcached.journal");
        std::os::unix::fs::symlink(&target, &journal).unwrap();
        assert!(open_private_rrdcached_journal(&journal).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"do not replay");
    }

    #[test]
    fn replay_rejects_canonical_paths_outside_the_storage_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("base");
        let outside = temp.path().join("outside.rrd");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(&outside, b"outside").unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let mut journal = File::create(root.join(".rrdcached.journal")).unwrap();
        serde_json::to_writer(
            &mut journal,
            &RrdcachedJournalRecord::Update {
                id: 1,
                path: outside.clone(),
                samples: vec!["1000000010:1".to_owned()],
            },
        )
        .unwrap();
        journal.write_all(b"\n").unwrap();
        drop(journal);
        assert!(RrdcachedQueue::open(&root, 1024, 300).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"outside");
    }

    #[test]
    fn invalid_updates_are_rejected_before_journaling() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("metric.rrd");
        rondi::create_rrd_file(
            &file,
            1_000_000_000,
            10,
            &["DS:value:GAUGE:30:U:U".to_owned()],
            &["RRA:AVERAGE:0.5:1:8".to_owned()],
            true,
        )
        .unwrap();
        let mut queue = RrdcachedQueue::open(&root, 1024, 300).unwrap();
        assert!(queue.enqueue(file.clone(), &["1000000010.x:1"]).is_err());
        assert!(queue.enqueue(file.clone(), &["1000000010:1:2"]).is_err());
        assert_eq!(queue.journal_bytes, 0);
        assert!(!queue.pending.contains_key(&file));
    }

    #[test]
    fn rrdcached_protocol_fields_unescape_spaces_and_backslashes() {
        assert_eq!(
            rrdcached_fields(r"FLUSH directory/space\ name/and\\slash.rrd"),
            ["FLUSH", "directory/space name/and\\slash.rrd"]
        );
    }

    #[test]
    fn queue_schedules_pending_paths_after_write_timeout_in_fifo_order() {
        let unique = format!(
            "rondi-rrdcached-schedule-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let z_path = root.join("queue-z.rrd");
        let a_path = root.join("queue-a.rrd");
        let mut queue = RrdcachedQueue::open(&root, 1024, 10).unwrap();
        queue.known.insert(z_path.clone(), 100);
        queue.known.insert(a_path.clone(), 100);
        queue.pending.insert(
            z_path.clone(),
            vec![PendingRrdUpdate {
                id: 1,
                samples: vec!["1000000010:1".to_owned()],
            }],
        );
        queue.pending.insert(
            a_path.clone(),
            vec![PendingRrdUpdate {
                id: 2,
                samples: vec!["1000000010:1".to_owned()],
            }],
        );

        queue.schedule_eligible(109);
        assert!(queue.pending_order.is_empty());
        queue.schedule_path(&z_path, 110);
        queue.schedule_path(&a_path, 110);
        assert_eq!(
            queue.pending_order.iter().cloned().collect::<Vec<_>>(),
            [z_path, a_path]
        );
        queue.schedule_eligible(120);
        assert_eq!(queue.pending_order.len(), 2, "paths are queued only once");
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_tree_expires_only_idle_nodes_and_refreshes_after_flush() {
        let mut tree = CacheTree::default();
        let stale = PathBuf::from("stale.rrd");
        let refreshed = PathBuf::from("refreshed.rrd");
        tree.insert(stale.clone(), 100);
        tree.insert(refreshed.clone(), 100);
        tree.mark_flushed(&refreshed, 108);

        assert_eq!(tree.idle_paths(109, 10), Vec::<PathBuf>::new());
        assert_eq!(tree.idle_paths(110, 10), vec![stale]);
        assert_eq!(tree.idle_paths(117, 10), vec![PathBuf::from("stale.rrd")]);
        assert_eq!(tree.idle_paths(118, 10).len(), 2);
        assert_eq!(tree.idle_paths(118, 0).len(), 2);
    }

    #[test]
    fn queue_expiry_keeps_pending_updates_and_clears_suspended_idle_nodes() {
        let unique = format!(
            "rondi-rrdcached-expiry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let idle = root.join("idle.rrd");
        let pending = root.join("pending.rrd");
        let mut queue = RrdcachedQueue::open(&root, 1024, 300).unwrap();
        queue.known.insert(idle.clone(), 100);
        queue.known.insert(pending.clone(), 100);
        queue
            .append(&RrdcachedJournalRecord::Update {
                id: 1,
                path: idle.clone(),
                samples: vec!["1000000010:1".to_owned()],
            })
            .unwrap();
        queue
            .append(&RrdcachedJournalRecord::Flushed { id: 1 })
            .unwrap();
        queue
            .append(&RrdcachedJournalRecord::Update {
                id: 2,
                path: pending.clone(),
                samples: vec!["1000000020:1".to_owned()],
            })
            .unwrap();
        queue.suspended.insert(idle.clone());
        queue.pending.insert(
            pending.clone(),
            vec![PendingRrdUpdate {
                id: 2,
                samples: vec!["1000000010:1".to_owned()],
            }],
        );

        assert_eq!(queue.expire_idle(110, 10), 1);
        assert!(!queue.known.contains(&idle));
        assert!(!queue.suspended.contains(&idle));
        assert!(queue.known.contains(&pending));
        assert!(queue.pending.contains_key(&pending));
        drop(queue);
        let reopened = RrdcachedQueue::open(&root, 1024, 300).unwrap();
        assert!(!reopened.known.contains(&idle));
        assert!(reopened.known.contains(&pending));
        assert_eq!(reopened.pending.len(), 1);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_queue_applies_backpressure_before_journaling_and_checks_recovery_limit() {
        let unique = format!(
            "rondi-rrdcached-queue-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let rrd = root.join("queue.rrd");
        rondi::create_rrd_file(
            &rrd,
            1_000_000_000,
            10,
            &["DS:load:GAUGE:20:U:U".to_owned()],
            &["RRA:AVERAGE:0.5:1:8".to_owned()],
            false,
        )
        .unwrap();

        let one_entry_bytes = pending_entry_bytes(&["1000000010:1".to_owned()]);
        let mut queue = RrdcachedQueue::open(&root, one_entry_bytes, 300).unwrap();
        queue.enqueue(rrd.clone(), &["1000000010:1"]).unwrap();
        let journal_bytes = queue.journal_bytes;
        assert_eq!(
            queue.pending_bytes,
            pending_entry_bytes(&["1000000010:1".to_owned()])
        );
        assert!(queue.enqueue(rrd, &["1000000020:2"]).is_err());
        assert_eq!(queue.journal_bytes, journal_bytes);
        drop(queue);

        assert!(RrdcachedQueue::open(&root, 1, 300).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn allocation_chunk_reserves_pending_update_entries_in_configured_blocks() {
        let unique = format!(
            "rondi-rrdcached-allocation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let rrd = root.join("allocation.rrd");
        rondi::create_rrd_file(
            &rrd,
            1_000_000_000,
            10,
            &["DS:load:GAUGE:20:U:U".to_owned()],
            &["RRA:AVERAGE:0.5:1:8".to_owned()],
            false,
        )
        .unwrap();
        let mut queue = RrdcachedQueue::open(&root, 1024, 300).unwrap();
        queue.allocation_chunk = 4;
        queue.enqueue(rrd.clone(), &["1000000010:1"]).unwrap();
        assert!(queue.pending[&rrd].capacity() >= 4);
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn configured_journal_directory_persists_and_replays_accepted_updates() {
        let unique = format!(
            "rondi-rrdcached-journal-dir-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let parent = std::env::temp_dir().join(unique);
        let root = parent.join("rrd-root");
        let journal_directory = parent.join("journal");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&journal_directory).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let journal_directory = std::fs::canonicalize(journal_directory).unwrap();
        let path = root.join("recovered.rrd");

        let mut queue =
            RrdcachedQueue::open_in_directory(&root, &journal_directory, 1024, 300).unwrap();
        queue
            .append(&RrdcachedJournalRecord::Update {
                id: 1,
                path: path.clone(),
                samples: vec!["1000000010:1.5".to_owned()],
            })
            .unwrap();
        drop(queue);

        assert!(journal_directory.join(".rrdcached.journal").is_file());
        assert!(!root.join(".rrdcached.journal").exists());
        let recovered =
            RrdcachedQueue::open_in_directory(&root, &journal_directory, 1024, 300).unwrap();
        assert_eq!(recovered.pending[&path][0].samples, ["1000000010:1.5"]);
        drop(recovered);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn journal_replay_rebuilds_live_cache_tree_in_event_order() {
        let unique = format!(
            "rondi-rrdcached-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let flushed_path = root.join("flushed.rrd");
        let forgotten_path = root.join("forgotten.rrd");
        let pending_path = root.join("pending.rrd");
        let legacy_forgotten_path = root.join("legacy-forgotten.rrd");
        let mut queue = RrdcachedQueue::open(&root, 1024, 300).unwrap();
        for (id, path) in [(1, &flushed_path), (2, &forgotten_path), (3, &pending_path)] {
            queue
                .append(&RrdcachedJournalRecord::Update {
                    id,
                    path: path.clone(),
                    samples: vec![format!("10000000{id}:1")],
                })
                .unwrap();
        }
        queue
            .append(&RrdcachedJournalRecord::Flushed { id: 1 })
            .unwrap();
        queue
            .append(&RrdcachedJournalRecord::Forgotten {
                ids: vec![2],
                path: Some(forgotten_path.clone()),
            })
            .unwrap();
        queue
            .append(&RrdcachedJournalRecord::Update {
                id: 4,
                path: legacy_forgotten_path.clone(),
                samples: vec!["1000000140:1".to_owned()],
            })
            .unwrap();
        queue
            .journal
            .write_all(b"{\"record\":\"forgotten\",\"ids\":[4]}\n")
            .unwrap();
        queue.journal.sync_data().unwrap();
        drop(queue);

        let reopened = RrdcachedQueue::open(&root, 1024, 300).unwrap();
        assert!(reopened.known.contains(&flushed_path));
        assert!(!reopened.known.contains(&forgotten_path));
        assert!(reopened.known.contains(&pending_path));
        assert!(!reopened.known.contains(&legacy_forgotten_path));
        assert_eq!(reopened.known.len(), 2);
        assert!(!reopened.pending.contains_key(&flushed_path));
        assert!(!reopened.pending.contains_key(&forgotten_path));
        assert!(!reopened.pending.contains_key(&legacy_forgotten_path));
        assert_eq!(reopened.pending.len(), 1);
        drop(reopened);
        std::fs::remove_dir_all(root).unwrap();
    }
}
