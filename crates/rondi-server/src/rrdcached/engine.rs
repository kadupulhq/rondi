//! The request protocol behind the daemon's [`Engine`] boundary.

use super::commands::Command;
use super::daemon::Engine;
use super::files::Names;
use super::journal::Journal;
use super::service::{Settings, Shared};
use std::ffi::OsString;
use std::future::Future;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
    pub(crate) no_overwrite: bool,
    pub(crate) recursive_create: bool,
}

pub(crate) struct ProtocolEngine {
    shared: Arc<Shared>,
}

impl ProtocolEngine {
    /// Opens the cache, replaying the journal. Also returns whether any
    /// journal entry was replayed.
    pub(crate) fn open(settings: &EngineSettings<'_>) -> Result<(Self, bool), String> {
        let names = Names::new(settings.base, settings.confine_to_base).map_err(|error| {
            format!(
                "cannot open base directory {}: {error}",
                settings.base.unwrap_or(Path::new("")).display()
            )
        })?;
        let (journal, files) = match settings.journal {
            Some(path) => {
                let (journal, files) = Journal::open(path)
                    .map_err(|error| format!("cannot open journal {}: {error}", path.display()))?;
                (Some(journal), files)
            }
            None => (None, Vec::new()),
        };
        let shared = Shared::new(
            names,
            Settings {
                no_overwrite: settings.no_overwrite,
                recursive_create: settings.recursive_create,
                write_interval: i64::try_from(settings.write_interval).unwrap_or(i64::MAX),
                flush_at_shutdown: settings.flush_at_shutdown,
                max_pending_bytes: settings.max_pending_bytes,
            },
            journal,
        );
        let replayed = shared.replay(&files)?;
        Ok((
            Self {
                shared: Arc::new(shared),
            },
            replayed,
        ))
    }

    /// Whether `-P` may name `word`: any command word the protocol knows.
    pub(crate) fn knows_command(word: &str) -> bool {
        Command::find(word.as_bytes()).is_some()
    }
}

fn to_path(name: Vec<u8>) -> PathBuf {
    PathBuf::from(OsString::from_vec(name))
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
        super::session::serve(stream, Arc::clone(&self.shared), permissions, shutdown)
    }

    fn next_queued(&self) -> Option<PathBuf> {
        self.shared.next_queued().map(to_path)
    }

    fn queued(&self) -> Arc<Notify> {
        Arc::clone(&self.shared.queued)
    }

    fn write(&self, path: &Path) {
        self.shared.write_entry(path.as_os_str().as_bytes());
    }

    fn flush_pass(&self, idle_seconds: u64) {
        self.shared
            .flush_pass(i64::try_from(idle_seconds).unwrap_or(i64::MAX));
    }

    fn pending(&self) -> Vec<PathBuf> {
        self.shared
            .pending_names()
            .into_iter()
            .map(to_path)
            .collect()
    }

    // Compatibility: with -j and without -F a SIGTERM keeps the values in
    // the journal instead of writing them (RCD-20). Provenance:
    // rrdcached(1) and black-box compatibility test.
    fn flushes_at_shutdown(&self) -> bool {
        !self.shared.journaled() || self.shared.settings.flush_at_shutdown
    }

    fn finish(&self, written: bool) {
        self.shared.finish(written);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_word_is_known() {
        for word in ["PING", "batch", ".", "WROTE", "fetchbin", "QUIT"] {
            assert!(ProtocolEngine::knows_command(word), "{word}");
        }
        assert!(!ProtocolEngine::knows_command("NOPE"));
    }
}
