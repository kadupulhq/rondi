//! The daemon layer's view of the request protocol and value cache that
//! live in the crate root. This adapter is the only place the daemon
//! touches them, so the protocol can be replaced behind [`Engine`].

use super::daemon::Engine;
// `super::super` rather than `crate`: the fuzz crate includes this crate's
// root as a module of its own.
use super::super::{
    RrdcachedBase, RrdcachedQueue, RrdcachedStats, flush_rrdcached_path, rrdcached_find_command,
    serve_rrdcached_connection, wall_time_seconds,
};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, watch};

/// Settings the protocol layer needs at startup.
pub(crate) struct EngineSettings<'a> {
    pub(crate) base: Option<&'a Path>,
    pub(crate) confine_to_base: bool,
    pub(crate) journal: Option<&'a Path>,
    pub(crate) max_pending_bytes: usize,
    pub(crate) write_interval: u64,
    pub(crate) flush_at_shutdown: bool,
    pub(crate) allocation_chunk: usize,
    pub(crate) no_overwrite: bool,
    pub(crate) recursive_create: bool,
}

pub(crate) struct ProtocolEngine {
    base: RrdcachedBase,
    stats: Arc<RrdcachedStats>,
    queue: Arc<Mutex<RrdcachedQueue>>,
    queued: Arc<Notify>,
    no_overwrite: bool,
    recursive_create: bool,
}

impl ProtocolEngine {
    /// Opens the cache, replaying the journal. Also returns whether any
    /// journal entry was replayed.
    pub(crate) fn open(settings: &EngineSettings<'_>) -> Result<(Self, bool), String> {
        let base = RrdcachedBase::new(settings.base, settings.confine_to_base)
            .map_err(|error| error.to_string())?;
        let stats = RrdcachedStats::default();
        let (mut queue, replayed) = RrdcachedQueue::open(
            &base,
            settings.journal,
            settings.max_pending_bytes,
            settings.write_interval,
            settings.flush_at_shutdown,
            &stats,
        )
        .map_err(|error| error.to_string())?;
        queue.allocation_chunk = settings.allocation_chunk.max(1);
        let queued = Arc::clone(&queue.wake);
        Ok((
            Self {
                base,
                stats: Arc::new(stats),
                queue: Arc::new(Mutex::new(queue)),
                queued,
                no_overwrite: settings.no_overwrite,
                recursive_create: settings.recursive_create,
            },
            replayed,
        ))
    }

    /// Whether `-P` may name `word`: any command word the protocol knows.
    pub(crate) fn knows_command(word: &str) -> bool {
        rrdcached_find_command(word).is_some()
    }

    fn queue(&self) -> MutexGuard<'_, RrdcachedQueue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Engine for ProtocolEngine {
    fn serve<S>(
        &self,
        stream: S,
        permissions: Option<Arc<[String]>>,
        shutdown: watch::Receiver<bool>,
    ) -> impl Future<Output = ()> + Send + 'static
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let served = serve_rrdcached_connection(
            Box::new(stream),
            self.base.clone(),
            Arc::clone(&self.stats),
            Arc::clone(&self.queue),
            self.no_overwrite,
            self.recursive_create,
            permissions.map(|commands| commands.to_vec()),
            shutdown,
        );
        async move {
            if let Err(error) = served.await {
                tracing::debug!(error = %error, "rrdcached_connection_ended");
            }
        }
    }

    fn next_queued(&self) -> Option<PathBuf> {
        self.queue().pending_order.pop_front()
    }

    fn queued(&self) -> Arc<Notify> {
        Arc::clone(&self.queued)
    }

    fn write(&self, path: &Path) {
        if let Err(error) = flush_rrdcached_path(path, &self.queue, &self.stats) {
            tracing::warn!(path = %path.display(), error, "rrdcached_write_failed");
        }
    }

    fn flush_pass(&self, idle_seconds: u64) {
        let now = wall_time_seconds();
        let mut queue = self.queue();
        queue.schedule_eligible(now);
        queue.expire_idle(now, idle_seconds);
        queue.rotate_journal();
    }

    fn pending(&self) -> Vec<PathBuf> {
        self.queue().pending.keys().cloned().collect()
    }

    fn flushes_at_shutdown(&self) -> bool {
        self.queue().flushes_at_shutdown()
    }

    fn finish(&self, written: bool) {
        let mut queue = self.queue();
        queue.flush_at_shutdown = written;
        queue.journal_done();
    }
}
