use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rondi::{DatabaseConfig, Store, StoreError, StoreOptions, Update};
use serde::Deserialize;
use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as StdBufRead, BufReader as StdBufReader, Write as StdWrite};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};

pub const DEFAULT_RRDCACHED_QUEUE_BYTES: usize = 64 * 1024 * 1024;
const JOURNAL_BASE: &str = "rrd.journal";
const JOURNAL_MAX: u64 = 1024 * 1024 * 1024;
const RRD_CMD_MAX: usize = 4096;

/// Stands in for the fully buffered stdio stream upstream journals through:
/// bytes reach the file only when a st_blksize buffer fills or the file is
/// closed, and nothing is synced.
struct RrdcachedJournalFile {
    file: File,
    buffer: Vec<u8>,
    block: usize,
}

impl RrdcachedJournalFile {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            if self.buffer.len() == self.block {
                self.file.write_all(&self.buffer)?;
                self.buffer.clear();
            }
            let take = (self.block - self.buffer.len()).min(bytes.len());
            self.buffer.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
        }
        Ok(())
    }

    fn close(mut self) -> std::io::Result<()> {
        self.file.write_all(&self.buffer)
    }
}

/// Journal files as upstream keeps them: the set written since the last
/// rotation and the set before it, which the next rotation deletes.
struct RrdcachedJournal {
    directory: PathBuf,
    file: Option<RrdcachedJournalFile>,
    size: u64,
    current: Vec<PathBuf>,
    old: Vec<PathBuf>,
    // journal_new_file failure forces config_flush_at_shutdown on.
    disabled: bool,
}

impl RrdcachedJournal {
    fn close(&mut self) {
        if let Some(file) = self.file.take()
            && let Err(error) = file.close()
        {
            tracing::error!(error = %error, "rrdcached_journal_close_failed");
        }
        self.size = 0;
    }

    fn new_file(&mut self) {
        self.close();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let path = self.directory.join(format!(
            "{JOURNAL_BASE}.{:010}.{:06}",
            now.as_secs() as i32,
            now.subsec_micros()
        ));
        let opened = (|| -> std::io::Result<RrdcachedJournalFile> {
            use std::os::unix::fs::MetadataExt;
            check_rrdcached_journal_directory(&self.directory)?;
            let file = open_rrdcached_journal(&path)?;
            let metadata = file.metadata()?;
            self.size = metadata.len();
            Ok(RrdcachedJournalFile {
                file,
                buffer: Vec::new(),
                block: usize::try_from(metadata.blksize())
                    .ok()
                    .filter(|block| *block > 0)
                    .unwrap_or(8192),
            })
        })();
        match opened {
            Ok(file) => {
                tracing::debug!(journal = %path.display(), "rrdcached_journal_started");
                self.file = Some(file);
                self.current.push(path);
            }
            Err(error) => {
                tracing::error!(
                    journal = %path.display(),
                    error = %error,
                    "rrdcached_journaling_disabled_values_flush_at_shutdown"
                );
                self.disabled = true;
            }
        }
    }

    fn write(&mut self, command: &str, arguments: &[u8]) -> usize {
        let Some(file) = self.file.as_mut() else {
            return 0;
        };
        let mut line = Vec::with_capacity(command.len() + arguments.len() + 2);
        line.extend_from_slice(command.as_bytes());
        line.push(b' ');
        line.extend_from_slice(arguments);
        line.push(b'\n');
        if let Err(error) = file.write(&line) {
            tracing::error!(error = %error, "rrdcached_journal_write_failed");
            return 0;
        }
        self.size = self.size.saturating_add(line.len() as u64);
        if self.size > JOURNAL_MAX {
            self.new_file();
        }
        line.len()
    }

    fn rotate(&mut self) {
        self.close();
        let removed = std::mem::replace(&mut self.old, std::mem::take(&mut self.current));
        self.new_file();
        remove_journal_files(&removed);
    }

    fn done(&mut self, flush_at_shutdown: bool) {
        self.close();
        if flush_at_shutdown {
            tracing::info!("rrdcached_removing_journals");
            remove_journal_files(&self.old);
            remove_journal_files(&self.current);
        } else {
            tracing::info!("rrdcached_expedited_shutdown_journals_kept");
        }
    }
}

/// Upstream opens with O_WRONLY|O_CREAT|O_APPEND and mode 0644, following
/// symlinks. Rondi keeps the flags but creates the file 0600, since it holds
/// every file name and value, and refuses a symlink, a hard-linked file, or
/// one another user owns, so a daemon started as root cannot be steered into
/// appending to an arbitrary file.
fn open_rrdcached_journal(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)?;
    check_rrdcached_journal_handle(&file.metadata()?)?;
    Ok(file)
}

fn check_rrdcached_journal_handle(metadata: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != euid {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(())
}

/// A directory other users can write lets them swap journal names between
/// checks, so it is refused unless sticky and owned by this user.
fn check_rrdcached_journal_directory(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(directory)?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if metadata.mode() & 0o022 != 0 && (metadata.mode() & 0o1000 == 0 || metadata.uid() != euid) {
        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(())
}

fn remove_journal_files(files: &[PathBuf]) {
    for file in files {
        tracing::debug!(journal = %file.display(), "rrdcached_removing_old_journal");
        let _ = std::fs::remove_file(file);
    }
}

/// Port of rrd_daemon.c buffer_get_field. `None` is an exhausted buffer and
/// `Some("")` is one holding only its terminating NUL, which still yields an
/// empty field.
fn rrdcached_buffer_field(buffer: &mut Option<&str>) -> Option<String> {
    let text = (*buffer)?;
    let mut field = String::new();
    let mut characters = text.char_indices();
    while let Some((index, character)) = characters.next() {
        match character {
            ' ' => {
                *buffer = Some(&text[index + 1..]);
                return Some(field);
            }
            '\\' => field.push(characters.next()?.1),
            _ => field.push(character),
        }
    }
    *buffer = None;
    Some(field)
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
    pub store: StoreOptions,
}

/// Configuration for the legacy rrdcached line protocol. This first protocol
/// slice deliberately listens on a Unix socket only. With a journal directory,
/// UPDATE is journaled as upstream does (buffered, never synced) before the
/// acknowledgment, and queued writes flush later.
#[derive(Debug, Clone)]
pub struct RrdcachedConfig {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub journal_directory: Option<PathBuf>,
    pub flush_at_shutdown: bool,
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

#[derive(Debug, Clone)]
struct PendingRrdUpdate {
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
    // ci->last_update_stamp: seeded from the file's whole-second last_up and
    // advanced by each accepted sample, even after its values are written.
    last_update_stamp: f64,
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

    fn node_mut(&mut self, path: &Path) -> Option<&mut CacheTreeNode> {
        let mut node = self.root.as_deref_mut();
        while let Some(current) = node {
            match path.cmp(&current.path) {
                std::cmp::Ordering::Less => node = current.left.as_deref_mut(),
                std::cmp::Ordering::Greater => node = current.right.as_deref_mut(),
                std::cmp::Ordering::Equal => return Some(current),
            }
        }
        None
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
                last_update_stamp: 0.0,
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
    journal: Option<RrdcachedJournal>,
    journal_rotations: u64,
    journal_bytes: u64,
    flush_at_shutdown: bool,
    pending_bytes: usize,
    max_pending_bytes: usize,
    write_timeout_seconds: u64,
    allocation_chunk: usize,
    // A flusher owns a path while it applies a snapshot of its entries, so a
    // concurrent FLUSH, FETCH, or worker waits instead of applying them again.
    flush_owners: std::collections::HashMap<PathBuf, Arc<Mutex<()>>>,
}

impl RrdcachedQueue {
    fn new(max_pending_bytes: usize, write_timeout_seconds: u64) -> Self {
        Self {
            pending: std::collections::BTreeMap::new(),
            pending_order: std::collections::VecDeque::new(),
            known: CacheTree::default(),
            suspended: std::collections::HashSet::new(),
            journal: None,
            journal_rotations: 0,
            journal_bytes: 0,
            flush_at_shutdown: true,
            pending_bytes: 0,
            max_pending_bytes,
            write_timeout_seconds,
            allocation_chunk: 1,
            flush_owners: std::collections::HashMap::new(),
        }
    }

    /// Returns the queue and whether any journal file replayed an entry.
    fn open(
        root: &Path,
        journal_directory: Option<&Path>,
        max_pending_bytes: usize,
        write_timeout_seconds: u64,
        flush_at_shutdown: bool,
        stats: &RrdcachedStats,
    ) -> Result<(Self, bool), Box<dyn std::error::Error>> {
        let mut queue = Self::new(max_pending_bytes, write_timeout_seconds);
        // Without -j upstream always flushes at shutdown (read_options).
        queue.flush_at_shutdown = flush_at_shutdown || journal_directory.is_none();
        let had_journal =
            journal_directory.is_some_and(|directory| queue.journal_init(root, directory, stats));
        if queue.pending_bytes > max_pending_bytes {
            return Err(format!(
                "recovered rrdcached queue requires {} bytes, exceeding configured limit {max_pending_bytes}",
                queue.pending_bytes
            )
            .into());
        }
        Ok((queue, had_journal))
    }

    fn flushes_at_shutdown(&self) -> bool {
        self.flush_at_shutdown
            || self
                .journal
                .as_ref()
                .is_some_and(|journal| journal.disabled)
    }

    fn journal_init(&mut self, root: &Path, directory: &Path, stats: &RrdcachedStats) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let mut journal = RrdcachedJournal {
            directory: directory.to_path_buf(),
            file: None,
            size: 0,
            current: Vec::new(),
            old: Vec::new(),
            disabled: false,
        };
        // Renaming, replaying, and later unlinking by name are only safe
        // where no other user can swap the names underneath.
        if let Err(error) = check_rrdcached_journal_directory(directory) {
            tracing::error!(
                directory = %directory.display(),
                error = %error,
                "rrdcached_journaling_disabled_values_flush_at_shutdown"
            );
            journal.disabled = true;
            self.journal = Some(journal);
            return false;
        }
        // Pre-rotation journal names, renamed so they replay first.
        let _ = std::fs::rename(
            directory.join(format!("{JOURNAL_BASE}.old")),
            directory.join(format!("{JOURNAL_BASE}.0000")),
        );
        let _ = std::fs::rename(
            directory.join(JOURNAL_BASE),
            directory.join(format!("{JOURNAL_BASE}.0001")),
        );
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::error!(
                    directory = %directory.display(),
                    error = %error,
                    "rrdcached_journal_opendir_failed"
                );
                self.journal = Some(journal);
                return false;
            }
        };
        for entry in entries.flatten() {
            if entry
                .file_name()
                .as_bytes()
                .starts_with(JOURNAL_BASE.as_bytes())
            {
                journal.current.push(directory.join(entry.file_name()));
            }
        }
        journal.current.sort_by(|left, right| {
            left.as_os_str()
                .as_bytes()
                .cmp(right.as_os_str().as_bytes())
        });
        let mut had_journal = false;
        for file in &journal.current {
            had_journal |= self.journal_replay(root, file, stats);
        }
        journal.new_file();
        self.journal = Some(journal);
        had_journal
    }

    fn journal_replay(&mut self, root: &Path, file: &Path, stats: &RrdcachedStats) -> bool {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        // Upstream stats the path, then opens it. Checking the opened handle
        // instead closes the window for swapping in a symlink, which is
        // refused outright.
        let handle = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(file)
        {
            Ok(handle) => handle,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(
                        journal = %file.display(),
                        error = %error,
                        "rrdcached_journal_open_failed"
                    );
                }
                return false;
            }
        };
        // A journal is trusted input, so upstream skips one another user
        // could have written.
        let (uid, mode, mut rejected) = match handle.metadata() {
            Ok(metadata) => (
                metadata.uid(),
                metadata.mode(),
                (!metadata.is_file()).then_some("not a regular file"),
            ),
            Err(_) => (0, 0, Some("stat error")),
        };
        // SAFETY: geteuid has no preconditions.
        if uid != unsafe { libc::geteuid() } {
            rejected = Some("not owned by daemon user");
        }
        if mode & 0o022 != 0 {
            rejected = Some("must not be user/group writable");
        }
        if let Some(reason) = rejected {
            tracing::error!(journal = %file.display(), reason, "rrdcached_journal_replay_rejected");
            return false;
        }
        tracing::info!(journal = %file.display(), "rrdcached_replaying_journal");
        let now = wall_time_seconds();
        let mut reader = StdBufReader::new(handle);
        let mut line = Vec::new();
        let (mut entries, mut failures, mut number) = (0_u64, 0_u64, 0_u64);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            // fgets into an RRD_CMD_MAX buffer splits longer lines, and
            // strlen stops at an embedded NUL.
            for chunk in line.chunks(RRD_CMD_MAX - 1) {
                number += 1;
                let chunk = chunk.split(|byte| *byte == 0).next().unwrap_or_default();
                match chunk.split_last() {
                    None => {}
                    Some((b'\n', entry)) => {
                        if self.replay_entry(root, entry, now, stats) {
                            entries += 1;
                        } else {
                            failures += 1;
                        }
                    }
                    Some(_) => {
                        tracing::info!(
                            journal = %file.display(),
                            line = number,
                            "rrdcached_journal_malformed_entry"
                        );
                        failures += 1;
                    }
                }
            }
        }
        tracing::info!(journal = %file.display(), entries, failures, "rrdcached_journal_replayed");
        entries > 0
    }

    /// Dispatches one journal entry the way handle_request does for a NULL
    /// socket: only commands allowed in the journal context succeed.
    fn replay_entry(
        &mut self,
        root: &Path,
        entry: &[u8],
        now: i64,
        stats: &RrdcachedStats,
    ) -> bool {
        let Ok(entry) = std::str::from_utf8(entry) else {
            return false;
        };
        let mut buffer = Some(entry);
        let Some(command) = rrdcached_buffer_field(&mut buffer) else {
            return false;
        };
        if command.eq_ignore_ascii_case("UPDATE") {
            let Some(file) = rrdcached_buffer_field(&mut buffer) else {
                return false;
            };
            stats.updates_received.fetch_add(1, Ordering::Relaxed);
            let Ok(path) = resolve_rrdcached_path(root, &file) else {
                return false;
            };
            let mut samples = Vec::new();
            while let Some(sample) = rrdcached_buffer_field(&mut buffer) {
                samples.push(sample);
            }
            self.update(path, None, &samples, None, now).is_ok()
        } else if command.eq_ignore_ascii_case("WROTE") {
            let path = Path::new(buffer.unwrap_or_default());
            if self.known.contains(path) {
                self.drop_pending(path);
                self.known.mark_flushed(path, now);
            }
            true
        } else if command.eq_ignore_ascii_case("FORGET") {
            rrdcached_buffer_field(&mut buffer)
                .and_then(|file| resolve_rrdcached_path(root, &file).ok())
                .is_some_and(|path| self.forget(&path))
        } else {
            false
        }
    }

    fn journal_write(&mut self, command: &str, arguments: &[u8]) {
        if let Some(journal) = self.journal.as_mut() {
            let written = journal.write(command, arguments);
            self.journal_bytes = self.journal_bytes.saturating_add(written as u64);
        }
    }

    fn rotate_journal(&mut self) {
        if let Some(journal) = self.journal.as_mut() {
            self.journal_rotations = self.journal_rotations.saturating_add(1);
            journal.rotate();
        }
    }

    fn journal_done(&mut self) {
        let flush_at_shutdown = self.flushes_at_shutdown();
        if let Some(journal) = self.journal.as_mut() {
            journal.done(flush_at_shutdown);
        }
    }

    fn drop_pending(&mut self, path: &Path) {
        let entries = self.pending.remove(path).unwrap_or_default();
        self.pending_order
            .retain(|pending_path| pending_path != path);
        let removed_bytes = entries
            .iter()
            .map(|entry| pending_entry_bytes(&entry.samples))
            .fold(0_usize, usize::saturating_add);
        self.pending_bytes = self.pending_bytes.saturating_sub(removed_bytes);
    }

    fn forget(&mut self, path: &Path) -> bool {
        if !self.known.remove(path) {
            return false;
        }
        self.drop_pending(path);
        self.suspended.remove(path);
        true
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

    fn expire_idle(&mut self, now: i64, age: u64) -> usize {
        let expired = self
            .known
            .idle_paths(now, age)
            .into_iter()
            .filter(|path| !self.pending.contains_key(path))
            .collect::<Vec<_>>();
        for path in &expired {
            self.known.remove(path);
            self.suspended.remove(path);
        }
        expired.len()
    }

    #[cfg(test)]
    fn enqueue(&mut self, path: PathBuf, samples: &[&str]) -> Result<usize, String> {
        let arguments = format!("{} {}", path.display(), samples.join(" "));
        let samples = samples
            .iter()
            .map(|sample| (*sample).to_owned())
            .collect::<Vec<_>>();
        self.update(
            path,
            None,
            &samples,
            Some(arguments.as_bytes()),
            wall_time_seconds(),
        )
    }

    /// Port of handle_request_update from the cache lookup on
    /// (rrd_daemon.c:1679-1856). `info` is the RRD read for a path not yet
    /// cached, taken before the queue lock; `journal_arguments` is the request
    /// text after the command word, or `None` during replay, which neither
    /// journals nor applies the pending cap. Only timestamps are checked here:
    /// values are first parsed when rrd_update_r writes the batch.
    fn update(
        &mut self,
        path: PathBuf,
        info: Option<&rondi::RrdInfo>,
        samples: &[String],
        journal_arguments: Option<&[u8]>,
        now: i64,
    ) -> Result<usize, String> {
        if !rrdcached_journal_path_is_safe(&path) {
            // The canonical path may be a symlink target the client cannot see.
            return Err("Invalid file name".to_owned());
        }
        let added_bytes = pending_entry_bytes(samples);
        if journal_arguments.is_some()
            && self.pending_bytes.saturating_add(added_bytes) > self.max_pending_bytes
        {
            return Err(format!(
                "rrdcached pending queue is full ({} of {} bytes)",
                self.pending_bytes, self.max_pending_bytes
            ));
        }
        if !self.known.contains(&path) {
            let last_update = match info {
                Some(info) => info.last_update,
                None => inspect_rrdcached_target(&path)?.last_update,
            };
            if last_update < 1 {
                return Err("Error: rrdcached: Invalid timestamp returned".to_owned());
            }
            self.known.insert(path.clone(), now);
            if let Some(node) = self.known.node_mut(&path) {
                node.last_update_stamp = last_update as f64;
            }
        }
        if let Some(arguments) = journal_arguments {
            self.journal_write("update", arguments);
        }
        let mut last_update_stamp = self
            .known
            .node_mut(&path)
            .map_or(0.0, |node| node.last_update_stamp);
        let mut accepted = Vec::new();
        let mut result = Ok(());
        for sample in samples {
            let Some(stamp) = rrdcached_sample_stamp(sample) else {
                result = Err(format!("Cannot find timestamp in '{sample}'!"));
                break;
            };
            if stamp <= last_update_stamp {
                result = Err(format!(
                    "illegal attempt to update using time {stamp:.6} when last update time is {last_update_stamp:.6} (minimum one second step)"
                ));
                break;
            }
            last_update_stamp = stamp;
            accepted.push(sample.clone());
        }
        if let Some(node) = self.known.node_mut(&path) {
            node.last_update_stamp = last_update_stamp;
        }
        let count = accepted.len();
        // Samples before a rejected one stay queued, as upstream appends each
        // before parsing the next.
        if count > 0 {
            self.pending_bytes = self
                .pending_bytes
                .saturating_add(pending_entry_bytes(&accepted));
            let entries = self.pending.entry(path.clone()).or_default();
            if entries.len() == entries.capacity() {
                entries.reserve(self.allocation_chunk);
            }
            entries.push(PendingRrdUpdate { samples: accepted });
        }
        result?;
        self.schedule_path(&path, now);
        if count == 0 {
            return Err("No values updated.".to_owned());
        }
        Ok(count)
    }
}

/// The `rrd_strtodbl(value, &eostamp, ...) != 1 || *eostamp != ':'` test:
/// rrd_strtod must stop exactly at the first colon, and the NaN/Inf
/// spellings, which return 2, are refused.
fn rrdcached_sample_stamp(sample: &str) -> Option<f64> {
    let (head, _) = sample.split_once(':')?;
    let special = ["nan", "inf", "-nan", "-inf"].iter().any(|prefix| {
        head.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    });
    if special {
        return None;
    }
    rondi::parse_rrd_number(head)
}

/// Rondi keys the cache by canonical path, which a symlink can give a
/// newline; journaling that in a `wrote` or `forget` line would forge entries.
fn rrdcached_journal_path_is_safe(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    !path.as_os_str().as_bytes().contains(&b'\n')
}

/// The text after the command word, which upstream journals raw and still
/// escaped. Replay decodes it with the same field parser the live request
/// uses, so the journal reproduces exactly what was acknowledged.
fn rrdcached_request_arguments(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let line = line.strip_suffix('\r').unwrap_or(line);
    let mut buffer = Some(line);
    let _ = rrdcached_buffer_field(&mut buffer);
    buffer.unwrap_or_default()
}

/// Reading the RRD can wait on another process's file lock, so callers do it
/// before taking the queue lock (as upstream does before cache_lock).
fn inspect_rrdcached_target(path: &Path) -> Result<rondi::RrdInfo, String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("No such file: {error}"))?;
    if !metadata.is_file() {
        return Err(format!("Not a regular file: {}", path.display()));
    }
    rondi::inspect_rrd_file(path).map_err(|error| error.to_string())
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
    let store = Store::open_with(&args.root, args.store)?;
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
    let stats = Arc::new(RrdcachedStats::default());
    let (mut queue, had_journal) = RrdcachedQueue::open(
        &root,
        args.journal_directory.as_deref(),
        args.max_pending_bytes,
        args.write_timeout_seconds,
        args.flush_at_shutdown,
        &stats,
    )?;
    queue.allocation_chunk = args.allocation_chunk.max(1);
    // journal_init treats replayed entries as a crash and, when shutdown
    // would flush, writes everything at once.
    let startup_flush = (had_journal && queue.flushes_at_shutdown())
        .then(|| queue.pending.keys().cloned().collect::<Vec<_>>());
    let queue = Arc::new(Mutex::new(queue));
    if let Some(paths) = startup_flush {
        flush_rrdcached_paths(
            paths,
            Arc::clone(&queue),
            Arc::clone(&stats),
            args.queue_threads,
        )
        .await;
    }
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
    let mut connections = JoinSet::new();
    let (shutdown_sender, shutdown) = tokio::sync::watch::channel(false);
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
                let shutdown = shutdown.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_rrdcached_connection(stream, root, echo_base, stats, queue, no_overwrite, allow_recursive_mkdir, socket_commands, shutdown).await {
                        tracing::warn!(error = %error, "rrdcached_connection_failed");
                    }
                });
            }
            _ = expiry_tick.tick(), if args.flush_interval_seconds > 0 => {
                // flush_thread_main queues old values and expires idle nodes,
                // then rotates the journal while the queued writes proceed.
                let tick_queue = Arc::clone(&queue);
                let flush_interval_seconds = args.flush_interval_seconds;
                let paths = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>, String> {
                    let mut queue = tick_queue.lock().map_err(|_| "rrdcached queue lock poisoned")?;
                    let now = wall_time_seconds();
                    queue.schedule_eligible(now);
                    queue.expire_idle(now, flush_interval_seconds);
                    let paths = queue.pending_order.iter().cloned().collect::<Vec<_>>();
                    queue.rotate_journal();
                    Ok(paths)
                })
                .await??;
                flush_rrdcached_paths(paths, Arc::clone(&queue), Arc::clone(&stats), args.queue_threads).await;
            }
            _ = shutdown_signals.0.recv() => break,
            _ = shutdown_signals.1.recv() => break,
        }
    }
    // An idle client must not hold the final flush hostage. Connections stop
    // reading new requests now; any still busy after the grace period are
    // aborted so the pending updates can still reach disk.
    let _ = shutdown_sender.send(true);
    let drained = timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::warn!(
            connections = connections.len(),
            "rrdcached_shutdown_aborted_connections"
        );
        connections.shutdown().await;
    }
    // With -j and without -F upstream leaves pending values to the journal.
    let paths = queue
        .lock()
        .ok()
        .filter(|queue| queue.flushes_at_shutdown())
        .map(|queue| queue.pending.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    flush_rrdcached_paths(
        paths,
        Arc::clone(&queue),
        Arc::clone(&stats),
        args.queue_threads,
    )
    .await;
    if let Ok(mut queue) = queue.lock() {
        queue.journal_done();
    }
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
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        let read = tokio::select! {
            read = read_bounded_async_line(&mut reader, &mut line, 1024 * 1024) => read?,
            _ = shutdown.wait_for(|stopping| *stopping) => return Ok(()),
        };
        if read == 0 {
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
                let read = tokio::select! {
                    read = read_bounded_async_line(&mut reader, &mut line, 1024 * 1024) => read?,
                    _ = shutdown.wait_for(|stopping| *stopping) => return Ok(()),
                };
                if read == 0 {
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
            } else {
                let arguments = rrdcached_request_arguments(line);
                // Upstream journals the request truncated at a NUL or at
                // RRD_CMD_MAX while acting on all of it. Refusing such a
                // request keeps replay equal to what was acknowledged.
                if arguments.contains('\0') || arguments.len() > RRD_CMD_MAX - 1 {
                    return Some(format!(
                        "-1 Request must be under {RRD_CMD_MAX} bytes without NUL\n"
                    ));
                }
                let mut buffer = Some(arguments);
                let file = rrdcached_buffer_field(&mut buffer).unwrap_or_default();
                let mut samples = Vec::new();
                while let Some(sample) = rrdcached_buffer_field(&mut buffer) {
                    samples.push(sample);
                }
                match resolve_rrdcached_path(root, &file).and_then(|path| {
                    let cached = queue
                        .lock()
                        .map_err(|_| "rrdcached queue lock poisoned".to_owned())?
                        .known
                        .contains(&path);
                    // stat and rrd_open may block, so upstream reads a new
                    // file before taking cache_lock again.
                    let info = if cached {
                        None
                    } else {
                        Some(inspect_rrdcached_target(&path)?)
                    };
                    queue
                        .lock()
                        .map_err(|_| "rrdcached queue lock poisoned".to_owned())?
                        .update(
                            path,
                            info.as_ref(),
                            &samples,
                            Some(arguments.as_bytes()),
                            wall_time_seconds(),
                        )
                }) {
                    Ok(count) => format!("0 errors, enqueued {count} value(s).\n"),
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
                "9 Statistics follow\nQueueLength: {}\nUpdatesReceived: {}\nFlushesReceived: {}\nUpdatesWritten: {}\nDataSetsWritten: {}\nTreeNodesNumber: {}\nTreeDepth: {}\nJournalBytes: {}\nJournalRotate: {}\n",
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
                queue.journal_rotations,
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
    if !queue.forget(path) {
        return Ok(false);
    }
    use std::os::unix::ffi::OsStrExt;
    queue.journal_write("forget", path.as_os_str().as_bytes());
    Ok(true)
}

fn flush_rrdcached_path(
    path: &Path,
    queue: &Mutex<RrdcachedQueue>,
    stats: &RrdcachedStats,
) -> Result<bool, String> {
    let owner = Arc::clone(
        queue
            .lock()
            .map_err(|_| "rrdcached queue lock poisoned".to_owned())?
            .flush_owners
            .entry(path.to_path_buf())
            .or_default(),
    );
    let result = match owner.lock() {
        Ok(_owned) => flush_owned_rrdcached_path(path, queue, stats),
        Err(_) => Err("rrdcached flush lock poisoned".to_owned()),
    };
    if let Ok(mut queue) = queue.lock() {
        // Owners are cloned only under the queue lock, so a count of two
        // (the map and this caller) means nobody else is waiting.
        if Arc::strong_count(&owner) == 2 {
            queue.flush_owners.remove(path);
        }
    }
    result
}

/// Port of queue_thread_main for one cache item (rrd_daemon.c:1228-1285):
/// the values leave the cache before the write, a failed rrd_update_r is only
/// logged, and `wrote` is journaled either way.
fn flush_owned_rrdcached_path(
    path: &Path,
    queue: &Mutex<RrdcachedQueue>,
    stats: &RrdcachedStats,
) -> Result<bool, String> {
    let entries = {
        let mut queue = queue
            .lock()
            .map_err(|_| "rrdcached queue lock poisoned".to_owned())?;
        if queue.suspended.contains(path) {
            return Ok(false);
        }
        let Some(entries) = queue.pending.get(path).cloned() else {
            return Ok(false);
        };
        queue.drop_pending(path);
        queue.known.mark_flushed(path, wall_time_seconds());
        entries
    };
    let samples = entries
        .iter()
        .flat_map(|entry| entry.samples.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let status = update_rrdcached_file(path, &samples);
    if let Err(error) = &status {
        tracing::warn!(
            file = %path.display(),
            error = %error,
            "rrdcached_rrd_update_failed"
        );
    }
    use std::os::unix::ffi::OsStrExt;
    queue
        .lock()
        .map_err(|_| "rrdcached queue lock poisoned".to_owned())?
        .journal_write("wrote", path.as_os_str().as_bytes());
    if status.is_ok() {
        stats.updates_written.fetch_add(1, Ordering::Relaxed);
        stats
            .datasets_written
            .fetch_add(samples.len() as u64, Ordering::Relaxed);
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

/// Opens an RRD for a daemon write. Upstream writes through whatever the
/// name resolves to; Rondi refuses a symlink, a file with another link, which
/// a local user could plant to point a privileged daemon's write elsewhere,
/// and an owner other than root when the daemon is root or root when it is
/// not. The checks run on the descriptor that is then written.
fn open_rrdcached_write_target(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| error.to_string())?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    // SAFETY: geteuid has no preconditions.
    let privileged = unsafe { libc::geteuid() } == 0;
    if !metadata.is_file() || metadata.nlink() != 1 || privileged != (metadata.uid() == 0) {
        return Err("refusing to write a linked, special, or differently owned file".to_owned());
    }
    Ok(file)
}

/// rrd_update_r over a cache item's values: one open and lock for the batch,
/// samples applied in turn, and the first failure stops the rest, leaving
/// earlier ones written.
fn update_rrdcached_file(path: &Path, samples: &[&str]) -> Result<(), String> {
    let info = rondi::inspect_rrd_file(path).map_err(|error| error.to_string())?;
    let mut updates = Vec::with_capacity(samples.len());
    let mut failure = None;
    for sample in samples {
        let parsed = (|| -> Result<rondi::RrdRawUpdate<'_>, String> {
            // process_arg prefers '@' time syntax, which the daemon never
            // queues as a number, so such a sample fails like a bad time.
            let (timestamp, values) = sample
                .split_once(':')
                .filter(|_| !sample.contains('@'))
                .ok_or_else(|| {
                    format!("expected timestamp not found in data source from {sample}")
                })?;
            let (timestamp, timestamp_usec) = rrdcached_update_timestamp(timestamp)
                .map_err(|error| format!("Invalid timestamp in {sample}: {error}"))?;
            let values = values
                .split(':')
                .map(|value| (!value.starts_with('U')).then_some(value))
                .collect::<Vec<_>>();
            if values.len() != info.data_sources.len() {
                return Err(format!(
                    "expected {} data source readings (got {}) from {sample}",
                    info.data_sources.len(),
                    values.len()
                ));
            }
            Ok(rondi::RrdRawUpdate {
                timestamp,
                timestamp_usec,
                values,
            })
        })();
        match parsed {
            Ok(update) => updates.push(update),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    if !updates.is_empty() {
        rondi::update_rrd_raw_batch_file(open_rrdcached_write_target(path)?, path, &updates, false)
            .map_err(|error| error.to_string())?;
    }
    failure.map_or(Ok(()), Err)
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
    let filename = output.file_name().ok_or("invalid output filename")?;
    let output = canonical_parent.join(filename);
    // The store lock and journal sit beside the RRD files. Replacing one
    // would lose acknowledged updates.
    if filename
        .to_str()
        .is_some_and(|name| name == ".rondi.lock" || name == "rondi.journal")
    {
        return Err(format!("{}: Permission denied", output.display()));
    }
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
        | StoreError::Owned
        | StoreError::RrdLocked => (StatusCode::CONFLICT, "conflict"),
        StoreError::InvalidName
        | StoreError::InvalidConfig(_)
        | StoreError::InvalidValue
        | StoreError::RrdExpression(_)
        | StoreError::Rrd(_) => (StatusCode::BAD_REQUEST, "invalid_request"),
        StoreError::Io(_) => (StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
        StoreError::FormatVersion(_)
        | StoreError::RrdFormat(_)
        | StoreError::RrdFile(_)
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
    fn enqueue_checks_only_timestamps_and_flush_drops_the_batch_at_a_bad_value() {
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
        let before = std::fs::read(&file).unwrap();
        let queue = Mutex::new(RrdcachedQueue::new(1024 * 1024, 300));
        let enqueue = |samples: &[&str]| queue.lock().unwrap().enqueue(file.clone(), samples);
        assert_eq!(
            enqueue(&["1000000010.x:1"]),
            Err("Cannot find timestamp in '1000000010.x:1'!".to_owned())
        );
        assert_eq!(
            enqueue(&["nan:1"]),
            Err("Cannot find timestamp in 'nan:1'!".to_owned())
        );
        assert_eq!(enqueue(&["1000000010:x", "1000000020:1:2"]), Ok(2));
        assert_eq!(
            enqueue(&["1000000030:3", "1000000025:4", "1000000040:5"]),
            Err("illegal attempt to update using time 1000000025.000000 when last update time is 1000000030.000000 (minimum one second step)".to_owned())
        );
        assert_eq!(
            queue.lock().unwrap().pending[&file]
                .iter()
                .flat_map(|entry| entry.samples.clone())
                .collect::<Vec<_>>(),
            ["1000000010:x", "1000000020:1:2", "1000000030:3"]
        );
        let stats = RrdcachedStats::default();
        assert_eq!(flush_rrdcached_path(&file, &queue, &stats), Ok(true));
        assert!(queue.lock().unwrap().pending.is_empty());
        assert_eq!(stats.updates_written.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read(&file).unwrap(), before);
        // The cache keeps the newest accepted stamp after the batch is gone.
        assert!(enqueue(&["1000000030:1"]).is_err());
        assert_eq!(enqueue(&["1000000031:1"]), Ok(1));
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
        let mut queue = RrdcachedQueue::new(1024, 10);
        queue.known.insert(z_path.clone(), 100);
        queue.known.insert(a_path.clone(), 100);
        queue.pending.insert(
            z_path.clone(),
            vec![PendingRrdUpdate {
                samples: vec!["1000000010:1".to_owned()],
            }],
        );
        queue.pending.insert(
            a_path.clone(),
            vec![PendingRrdUpdate {
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
        let mut queue = RrdcachedQueue::new(1024, 300);
        queue.known.insert(idle.clone(), 100);
        queue.known.insert(pending.clone(), 100);
        queue.suspended.insert(idle.clone());
        queue.pending.insert(
            pending.clone(),
            vec![PendingRrdUpdate {
                samples: vec!["1000000010:1".to_owned()],
            }],
        );

        assert_eq!(queue.expire_idle(110, 10), 1);
        assert!(!queue.known.contains(&idle));
        assert!(!queue.suspended.contains(&idle));
        assert!(queue.known.contains(&pending));
        assert!(queue.pending.contains_key(&pending));
        drop(queue);
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
        let mut queue = RrdcachedQueue::new(one_entry_bytes, 300);
        queue.enqueue(rrd.clone(), &["1000000010:1"]).unwrap();
        let journal_bytes = queue.journal_bytes;
        assert_eq!(
            queue.pending_bytes,
            pending_entry_bytes(&["1000000010:1".to_owned()])
        );
        assert!(queue.enqueue(rrd.clone(), &["1000000020:2"]).is_err());
        assert_eq!(queue.journal_bytes, journal_bytes);
        drop(queue);

        let journal = root.join("journal");
        std::fs::create_dir(&journal).unwrap();
        std::fs::write(
            journal.join("rrd.journal.0000000001.000000"),
            format!("update {} 1000000010:1\n", rrd.display()),
        )
        .unwrap();
        let stats = RrdcachedStats::default();
        assert!(RrdcachedQueue::open(&root, Some(&journal), 1, 300, false, &stats).is_err());
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
        let mut queue = RrdcachedQueue::new(1024, 300);
        queue.allocation_chunk = 4;
        queue.enqueue(rrd.clone(), &["1000000010:1"]).unwrap();
        assert!(queue.pending[&rrd].capacity() >= 4);
        drop(queue);
        std::fs::remove_dir_all(root).unwrap();
    }

    fn create_test_rrd(path: &Path) {
        rondi::create_rrd_file(
            path,
            1_000_000_000,
            10,
            &["DS:value:GAUGE:30:U:U".to_owned()],
            &[
                "RRA:AVERAGE:0.5:1:500".to_owned(),
                "RRA:MAX:0.5:5:100".to_owned(),
            ],
            false,
        )
        .unwrap();
    }

    #[test]
    fn concurrent_flushes_of_one_file_apply_each_sample_once() {
        for attempt in 0..5 {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let file = root.join("metric.rrd");
            let reference = root.join("reference.rrd");
            create_test_rrd(&file);
            std::fs::copy(&file, &reference).unwrap();
            let mut queue = RrdcachedQueue::new(64 * 1024 * 1024, 100_000);
            let mut samples = Vec::new();
            for step in 1..=100_i64 {
                let sample = format!("{}:{}", 1_000_000_000 + step * 10, step % 37);
                queue.enqueue(file.clone(), &[sample.as_str()]).unwrap();
                samples.push(sample);
            }
            let queue = Arc::new(Mutex::new(queue));
            let stats = Arc::new(RrdcachedStats::default());
            let barrier = Arc::new(std::sync::Barrier::new(4));
            let workers = (0..4)
                .map(|_| {
                    let queue = Arc::clone(&queue);
                    let stats = Arc::clone(&stats);
                    let barrier = Arc::clone(&barrier);
                    let file = file.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        flush_rrdcached_path(&file, &queue, &stats)
                    })
                })
                .collect::<Vec<_>>();
            let results = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect::<Vec<_>>();
            let samples = samples.iter().map(String::as_str).collect::<Vec<_>>();
            update_rrdcached_file(&reference, &samples).unwrap();
            assert!(
                results.iter().all(Result::is_ok),
                "attempt {attempt}: concurrent FLUSH returned errors: {results:?}"
            );
            assert_eq!(
                rondi::dump_rrd_file(&file).unwrap(),
                rondi::dump_rrd_file(&reference).unwrap(),
                "attempt {attempt}: concurrent flush produced a different RRD"
            );
            assert_eq!(stats.updates_written.load(Ordering::Relaxed), 1);
            assert_eq!(stats.datasets_written.load(Ordering::Relaxed), 100);
        }
    }

    #[test]
    fn create_cannot_replace_daemon_owned_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("metric.rrd");
        create_test_rrd(&file);
        for reserved in [".rondi.lock", "rondi.journal"] {
            let created = rrdcached_create(
                &root,
                &[reserved, "DS:x:GAUGE:30:U:U", "RRA:AVERAGE:0.5:1:10"],
                false,
                false,
            );
            assert!(created.is_err(), "CREATE replaced {reserved}");
        }
    }

    fn journal_files(directory: &Path) -> Vec<String> {
        let mut names = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    fn private_journal(directory: &Path, name: &str, contents: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = directory.join(name);
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    #[test]
    fn buffer_field_matches_upstream_escaping() {
        let mut buffer = Some(r"update a\ b.rrd 1:2  x\\");
        assert_eq!(
            rrdcached_buffer_field(&mut buffer).as_deref(),
            Some("update")
        );
        assert_eq!(
            rrdcached_buffer_field(&mut buffer).as_deref(),
            Some("a b.rrd")
        );
        assert_eq!(rrdcached_buffer_field(&mut buffer).as_deref(), Some("1:2"));
        assert_eq!(rrdcached_buffer_field(&mut buffer).as_deref(), Some(""));
        assert_eq!(rrdcached_buffer_field(&mut buffer).as_deref(), Some(r"x\"));
        assert_eq!(rrdcached_buffer_field(&mut buffer), None);
        let mut trailing = Some(r"a\");
        assert_eq!(rrdcached_buffer_field(&mut trailing), None);
        assert_eq!(
            rrdcached_request_arguments("UPDATE a\\ b.rrd 1:2\r\n"),
            r"a\ b.rrd 1:2"
        );
    }

    #[test]
    fn journal_file_reaches_disk_only_in_whole_blocks_or_on_close() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("journal");
        let mut journal = RrdcachedJournalFile {
            file: File::create(&path).unwrap(),
            buffer: Vec::new(),
            block: 8,
        };
        journal.write(b"12345").unwrap();
        journal.write(b"678").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
        journal.write(b"9abcdefghij").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"123456789abcdefg");
        journal.close().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"123456789abcdefghij");
    }

    #[test]
    fn no_journal_is_written_without_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("metric.rrd");
        create_test_rrd(&file);
        let stats = RrdcachedStats::default();
        let (mut queue, had_journal) =
            RrdcachedQueue::open(&root, None, 1024 * 1024, 300, false, &stats).unwrap();
        assert!(!had_journal);
        assert!(queue.flushes_at_shutdown());
        queue.enqueue(file.clone(), &["1000000010:1"]).unwrap();
        queue.rotate_journal();
        queue.journal_done();
        assert_eq!(queue.journal_bytes, 0);
        assert_eq!(queue.journal_rotations, 0);
        assert_eq!(journal_files(&root), ["metric.rrd"]);
    }

    #[test]
    fn replay_follows_update_wrote_and_forget_entries_in_file_order() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let journal = root.join("journal");
        std::fs::create_dir(&journal).unwrap();
        for name in ["a.rrd", "b.rrd", "c d.rrd"] {
            create_test_rrd(&root.join(name));
        }
        let a = root.join("a.rrd");
        let b = root.join("b.rrd");
        let c = root.join("c d.rrd");
        // The legacy name sorts after the zero-padded rename of rrd.journal.old.
        private_journal(
            &journal,
            "rrd.journal",
            &format!(
                "update a.rrd 1000000020:2\nwrote {}\nupdate a.rrd 1000000030:3\n",
                a.display()
            ),
        );
        private_journal(
            &journal,
            "rrd.journal.old",
            &format!(
                "update a.rrd 1000000010:1\nupdate b.rrd 1000000010:1\nforget {}\nFLUSH a.rrd\n\
                 update c\\ d.rrd 1000000010:4\nupdate missing.rrd 1000000010:1\n\
                 update {} 1000000010:1\nnot\x00ended",
                b.display(),
                temp.path().join("../outside.rrd").display()
            ),
        );
        let stats = RrdcachedStats::default();
        let (queue, had_journal) =
            RrdcachedQueue::open(&root, Some(&journal), 1024 * 1024, 300, false, &stats).unwrap();
        assert!(had_journal);
        assert_eq!(
            queue.pending[&a]
                .iter()
                .flat_map(|entry| entry.samples.clone())
                .collect::<Vec<_>>(),
            ["1000000030:3"]
        );
        assert_eq!(queue.pending[&c][0].samples, ["1000000010:4"]);
        assert!(!queue.known.contains(&b));
        assert_eq!(queue.known.len(), 2);
        assert_eq!(stats.updates_received.load(Ordering::Relaxed), 7);
        assert_eq!(queue.journal_bytes, 0);
        let mut names = journal_files(&journal);
        let started = names.pop().unwrap();
        assert!(started.starts_with("rrd.journal.") && started.len() == 29);
        assert_eq!(names, ["rrd.journal.0000", "rrd.journal.0001"]);
    }

    #[test]
    fn replay_skips_journals_other_users_could_write() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let journal = root.join("journal");
        std::fs::create_dir(&journal).unwrap();
        create_test_rrd(&root.join("a.rrd"));
        let writable = private_journal(&journal, "rrd.journal.1", "update a.rrd 1000000010:1\n");
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o664)).unwrap();
        // A FIFO must be skipped, not block startup waiting for a writer.
        let fifo = std::ffi::CString::new(
            journal
                .join("rrd.journal.2")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: the CString is NUL terminated and outlives the call.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let stats = RrdcachedStats::default();
        let (queue, had_journal) =
            RrdcachedQueue::open(&root, Some(&journal), 1024 * 1024, 300, false, &stats).unwrap();
        assert!(!had_journal);
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn rotation_keeps_the_previous_set_and_shutdown_removes_journals_only_when_flushing() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("metric.rrd");
        create_test_rrd(&file);
        for flush_at_shutdown in [false, true] {
            let journal = root.join(format!("journal-{flush_at_shutdown}"));
            std::fs::create_dir(&journal).unwrap();
            let replayed = private_journal(&journal, "rrd.journal.0000", "update metric.rrd x\n");
            let stats = RrdcachedStats::default();
            let (mut queue, had_journal) = RrdcachedQueue::open(
                &root,
                Some(&journal),
                1024 * 1024,
                300,
                flush_at_shutdown,
                &stats,
            )
            .unwrap();
            assert!(!had_journal);
            let first = journal.join(journal_files(&journal).pop().unwrap());
            queue.enqueue(file.clone(), &["1000000010:1"]).unwrap();
            let line = format!("update {} 1000000010:1\n", file.display());
            assert_eq!(queue.journal_bytes, line.len() as u64);
            assert_eq!(std::fs::read(&first).unwrap(), b"");
            std::thread::sleep(Duration::from_millis(2));
            queue.rotate_journal();
            assert_eq!(std::fs::read_to_string(&first).unwrap(), line);
            assert!(replayed.exists());
            std::thread::sleep(Duration::from_millis(2));
            queue.rotate_journal();
            assert!(!replayed.exists());
            assert!(!first.exists());
            assert_eq!(journal_files(&journal).len(), 2);
            assert_eq!(queue.journal_rotations, 2);
            queue.journal_done();
            assert_eq!(journal_files(&journal).is_empty(), flush_at_shutdown);
        }
    }

    #[test]
    fn journal_open_refuses_symlinks_hard_links_and_shared_directories() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let directory = std::fs::canonicalize(temp.path()).unwrap();
        let target = directory.join("victim");
        std::fs::write(&target, b"keep").unwrap();
        let link = directory.join("rrd.journal.link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(open_rrdcached_journal(&link).is_err());
        let hard = directory.join("rrd.journal.hard");
        std::fs::hard_link(&target, &hard).unwrap();
        assert!(open_rrdcached_journal(&hard).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        use std::os::unix::fs::MetadataExt;
        let created = open_rrdcached_journal(&directory.join("rrd.journal.ok")).unwrap();
        assert_eq!(created.metadata().unwrap().mode() & 0o077, 0);

        let shared = directory.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        let mut journal = RrdcachedJournal {
            directory: shared.clone(),
            file: None,
            size: 0,
            current: Vec::new(),
            old: Vec::new(),
            disabled: false,
        };
        journal.new_file();
        assert!(journal.disabled && journal.file.is_none());
        private_journal(&shared, "rrd.journal.1", "update a.rrd 1000000010:1\n");
        let stats = RrdcachedStats::default();
        let (queue, had_journal) =
            RrdcachedQueue::open(&directory, Some(&shared), 1024, 300, false, &stats).unwrap();
        assert!(!had_journal && queue.flushes_at_shutdown());
        assert_eq!(journal_files(&shared), ["rrd.journal.1"]);
        std::fs::remove_file(shared.join("rrd.journal.1")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        journal.disabled = false;
        journal.new_file();
        assert!(!journal.disabled && journal.file.is_some());
    }

    #[test]
    fn replay_refuses_a_journal_swapped_for_a_symlink_after_listing() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        create_test_rrd(&root.join("a.rrd"));
        let journal = root.join("journal");
        std::fs::create_dir(&journal).unwrap();
        let real = private_journal(&root, "elsewhere", "update a.rrd 1000000010:1\n");
        let listed = private_journal(&journal, "rrd.journal.1", "");
        std::fs::remove_file(&listed).unwrap();
        std::os::unix::fs::symlink(&real, &listed).unwrap();
        let stats = RrdcachedStats::default();
        let mut queue = RrdcachedQueue::new(1024 * 1024, 300);
        assert!(!queue.journal_replay(&root, &listed, &stats));
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn requests_that_would_journal_differently_are_refused_before_journaling() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        create_test_rrd(&root.join("evil\nname.rrd"));
        std::os::unix::fs::symlink(root.join("evil\nname.rrd"), root.join("link.rrd")).unwrap();
        create_test_rrd(&root.join("a.rrd"));
        let journal = root.join("journal");
        std::fs::create_dir(&journal).unwrap();
        let stats = RrdcachedStats::default();
        let (queue, _) =
            RrdcachedQueue::open(&root, Some(&journal), 1024 * 1024, 300, false, &stats).unwrap();
        let queue = Mutex::new(queue);
        let request = |line: &str| {
            handle_rrdcached_line(&root, &root, line, &stats, &queue, false, false, None).unwrap()
        };
        assert_eq!(
            request("UPDATE link.rrd 1000000010:1\n"),
            "-1 Invalid file name\n"
        );
        assert!(request("UPDATE a.rrd 1000000010:1\0 1000000020:2\n").starts_with("-1 "));
        let long = format!("UPDATE a.rrd {}\n", vec!["1000000010:1"; 400].join(" "));
        assert!(request(&long).starts_with("-1 "));
        assert_eq!(queue.lock().unwrap().journal_bytes, 0);
        assert_eq!(
            request("UPDATE a.rrd  1000000010:1\n"),
            "-1 Cannot find timestamp in ''!\n"
        );
        assert_eq!(
            request("UPDATE a\\.rrd 1000000010:1\n"),
            "0 errors, enqueued 1 value(s).\n"
        );
        let mut queue = queue.into_inner().unwrap();
        queue.journal_done();
        assert_eq!(
            journal_contents_of(&journal),
            "update a.rrd  1000000010:1\nupdate a\\.rrd 1000000010:1\n"
        );
        let (replayed, _) =
            RrdcachedQueue::open(&root, Some(&journal), 1024 * 1024, 300, false, &stats).unwrap();
        assert_eq!(
            replayed.pending[&root.join("a.rrd")][0].samples,
            ["1000000010:1"]
        );
        assert_eq!(replayed.pending.len(), 1);
    }

    fn journal_contents_of(directory: &Path) -> String {
        journal_files(directory)
            .into_iter()
            .map(|name| std::fs::read_to_string(directory.join(name)).unwrap())
            .collect()
    }

    /// journal_replay counts an entry whose file is gone and keeps replaying
    /// (rrd_daemon.c:3674-3677), so a vanished directory cannot stop startup.
    #[test]
    fn replay_skips_records_whose_directory_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let kept = root.join("kept.rrd");
        create_test_rrd(&kept);
        let journal = root.join("journal");
        std::fs::create_dir(&journal).unwrap();
        private_journal(
            &journal,
            "rrd.journal.1",
            "update gone/x.rrd 1000000010:1\nupdate kept.rrd 1000000010:1\n",
        );
        let stats = RrdcachedStats::default();
        let (queue, had_journal) =
            RrdcachedQueue::open(&root, Some(&journal), 1 << 20, 300, false, &stats)
                .expect("a stale journal record must not stop the daemon from starting");
        assert!(had_journal);
        assert!(queue.pending.contains_key(&kept));
        assert_eq!(queue.pending.len(), 1);
    }

    #[test]
    fn flush_refuses_to_write_through_a_hard_link() {
        let temp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(temp.path()).unwrap();
        let outside = base.join("outside.rrd");
        create_test_rrd(&outside);
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        let linked = root.join("linked.rrd");
        std::fs::hard_link(&outside, &linked).unwrap();
        let before = std::fs::read(&outside).unwrap();
        let queue = Mutex::new(RrdcachedQueue::new(1024 * 1024, 300));
        queue
            .lock()
            .unwrap()
            .enqueue(linked.clone(), &["1000000010:1"])
            .unwrap();
        let stats = RrdcachedStats::default();
        assert_eq!(flush_rrdcached_path(&linked, &queue, &stats), Ok(true));
        assert_eq!(std::fs::read(&outside).unwrap(), before);
        assert_eq!(stats.updates_written.load(Ordering::Relaxed), 0);
    }

    /// queue_thread_main takes a file's values out of the cache before
    /// rrd_update_r and only logs a failure (rrd_daemon.c:1228-1262), so a
    /// value refused at write is dropped with the rest of its batch rather
    /// than blocking the file.
    #[test]
    fn values_refused_at_write_drop_their_batch() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let counter = root.join("counter.rrd");
        rondi::create_rrd_file(
            &counter,
            1_000_000_000,
            10,
            &["DS:c:COUNTER:30:U:U".to_owned()],
            &["RRA:AVERAGE:0.5:1:8".to_owned()],
            true,
        )
        .unwrap();
        let queue = Mutex::new(RrdcachedQueue::new(1 << 20, 300));
        queue
            .lock()
            .unwrap()
            .enqueue(counter.clone(), &["1000000010:1.5"])
            .unwrap();
        queue
            .lock()
            .unwrap()
            .enqueue(counter.clone(), &["1000000020:100"])
            .unwrap();
        let stats = RrdcachedStats::default();
        assert_eq!(flush_rrdcached_path(&counter, &queue, &stats), Ok(true));
        assert!(!queue.lock().unwrap().pending.contains_key(&counter));
        assert_eq!(
            rondi::inspect_rrd_file(&counter).unwrap().last_update,
            1_000_000_000
        );
        queue
            .lock()
            .unwrap()
            .enqueue(counter.clone(), &["1000000030:200"])
            .unwrap();
        assert_eq!(flush_rrdcached_path(&counter, &queue, &stats), Ok(true));
        assert_eq!(
            rondi::inspect_rrd_file(&counter).unwrap().last_update,
            1_000_000_030
        );
    }

    /// `5e` passes UPDATE and rrd_strtodbl accepts it at write too.
    #[test]
    fn flush_parses_values_like_rrd_update() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let file = root.join("metric.rrd");
        create_test_rrd(&file);
        let queue = Mutex::new(RrdcachedQueue::new(1 << 20, 300));
        queue
            .lock()
            .unwrap()
            .enqueue(file.clone(), &["1000000010:5e"])
            .unwrap();
        let stats = RrdcachedStats::default();
        assert_eq!(flush_rrdcached_path(&file, &queue, &stats), Ok(true));
        assert_eq!(
            rondi::inspect_rrd_file(&file).unwrap().last_update,
            1_000_000_010
        );
    }
}
