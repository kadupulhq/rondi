//! The serving loop: accepts connections, runs the write threads and the
//! flush timer, and stops on a signal.

use super::config::LogLevel;
use super::listener::{BoundSocket, Listener};
use super::log::Log;
use super::signals::{DaemonSignal, Shutdown, SignalEffect, SignalState};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Notify, Semaphore, mpsc, watch};
use tokio::task::JoinSet;

/// Connections served at once; further clients wait in the listen backlog.
const MAX_CONNECTIONS: usize = 4096;

/// Write threads actually started, whatever `-t` asks for.
const MAX_WRITE_THREADS: usize = 64;

/// How long shutdown waits for open connections to finish their request.
const CONNECTION_DRAIN: Duration = Duration::from_secs(10);

/// The request protocol and the value cache, as the daemon layer uses
/// them. Everything about requests, replies, the cache and the journal
/// lives behind this trait.
pub(crate) trait Engine: Send + Sync + 'static {
    /// Serves one client connection until it closes, or until `shutdown`
    /// turns true while the client is idle.
    fn serve<S>(
        &self,
        stream: S,
        permissions: Option<Arc<[String]>>,
        shutdown: watch::Receiver<bool>,
    ) -> impl Future<Output = ()> + Send + 'static
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static;

    /// Takes the next entry queued for a write thread.
    fn next_queued(&self) -> Option<PathBuf>;

    /// Notified whenever an entry is queued.
    fn queued(&self) -> Arc<Notify>;

    /// Writes an entry's pending values to its file. Blocks.
    fn write(&self, path: &Path);

    /// The `-f` pass: queue entries older than `-w`, drop entries idle for
    /// `idle_seconds`, start a new journal file. Blocks.
    fn flush_pass(&self, idle_seconds: u64);

    /// Entries holding pending values.
    fn pending(&self) -> Vec<PathBuf>;

    /// Whether a SIGTERM shutdown writes pending values.
    fn flushes_at_shutdown(&self) -> bool;

    /// Closes the journal, removing its files when `written` is true.
    /// Blocks.
    fn finish(&self, written: bool);
}

/// Daemon settings that the serving loop uses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    pub(crate) write_threads: usize,
    pub(crate) flush_interval: Duration,
    pub(crate) write_jitter: Duration,
    pub(crate) journaled: bool,
}

/// How serving ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) shutdown: Shutdown,
    pub(crate) written: bool,
}

pub(crate) struct Daemon<E> {
    pub(crate) engine: Arc<E>,
    pub(crate) listeners: Vec<Listener>,
    pub(crate) timing: Timing,
    pub(crate) log: Arc<Log>,
}

impl<E: Engine> Daemon<E> {
    /// Serves until a stopping signal arrives on `signals`, then shuts
    /// down. A closed signal channel counts as SIGTERM.
    pub(crate) async fn run(self, mut signals: mpsc::Receiver<DaemonSignal>) -> Outcome {
        let Daemon {
            engine,
            listeners,
            timing,
            log,
        } = self;
        let (stop, stopping) = watch::channel(false);
        let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        let mut tasks = JoinSet::new();
        for listener in listeners {
            tasks.spawn(accept_loop(
                Arc::clone(&engine),
                listener,
                Arc::clone(&connections),
                stopping.clone(),
                Arc::clone(&log),
            ));
        }
        let threads = timing.write_threads.clamp(1, MAX_WRITE_THREADS);
        for seed in 0..threads {
            tasks.spawn(write_thread(
                Arc::clone(&engine),
                timing.write_jitter,
                jitter_seed(seed),
                stopping.clone(),
            ));
        }
        tasks.spawn(flush_timer(
            Arc::clone(&engine),
            timing.flush_interval,
            stopping.clone(),
        ));

        let mut state = SignalState::default();
        let shutdown = loop {
            let signal = signals.recv().await.unwrap_or(DaemonSignal::Terminate);
            match state.receive(signal) {
                SignalEffect::Stop(shutdown) => {
                    log.event(
                        LogLevel::Notice,
                        "rrdcached_signal",
                        &format!("caught {}", signal.name()),
                    );
                    break shutdown;
                }
                SignalEffect::Ignore => log.event(
                    LogLevel::Debug,
                    "rrdcached_signal_ignored",
                    &format!("{} received and ignored", signal.name()),
                ),
            }
        };
        log.event(LogLevel::Info, "rrdcached_stopping", "starting shutdown");

        // Listeners close and idle clients are let go before the final
        // write, so no request adds values behind it.
        let _ = stop.send(true);
        while tasks.join_next().await.is_some() {}
        let drained = tokio::time::timeout(
            CONNECTION_DRAIN,
            connections.acquire_many(MAX_CONNECTIONS as u32),
        )
        .await;
        if drained.is_err() {
            log.event(
                LogLevel::Warning,
                "rrdcached_drain_timeout",
                "connections still open at shutdown",
            );
        }

        let written = shutdown.writes_pending(engine.flushes_at_shutdown());
        let finisher = Arc::clone(&engine);
        let _ = tokio::task::spawn_blocking(move || {
            if written {
                for path in finisher.pending() {
                    finisher.write(&path);
                }
            }
            finisher.finish(written);
        })
        .await;
        if written {
            log.event(
                LogLevel::Info,
                "rrdcached_flushed",
                "clean shutdown; all RRDs flushed",
            );
        } else if timing.journaled {
            log.event(
                LogLevel::Info,
                "rrdcached_journal_kept",
                "expedited shutdown; journals will be used at next startup",
            );
        }
        Outcome { shutdown, written }
    }
}

/// Accepts clients on one listener until shutdown, then closes it, which
/// removes a Unix socket's file.
async fn accept_loop<E: Engine>(
    engine: Arc<E>,
    listener: Listener,
    connections: Arc<Semaphore>,
    mut stopping: watch::Receiver<bool>,
    log: Arc<Log>,
) {
    let Listener {
        socket,
        permissions,
    } = listener;
    let registered = match socket {
        BoundSocket::Unix(std_listener, file) => std_listener
            .set_nonblocking(true)
            .and_then(|()| tokio::net::UnixListener::from_std(std_listener))
            .map(|listener| Accepting::Unix {
                listener,
                _file: file,
            }),
        BoundSocket::Tcp(std_listener) => std_listener
            .set_nonblocking(true)
            .and_then(|()| tokio::net::TcpListener::from_std(std_listener))
            .map(Accepting::Tcp),
    };
    let accepting = match registered {
        Ok(accepting) => accepting,
        Err(error) => {
            log.event(
                LogLevel::Err,
                "rrdcached_listener_failed",
                &format!("cannot accept on a listener: {error}"),
            );
            return;
        }
    };
    loop {
        let permit = tokio::select! {
            permit = Arc::clone(&connections).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
            _ = stopping.wait_for(|stop| *stop) => return,
        };
        let accepted = tokio::select! {
            accepted = accepting.accept(&engine, permissions.clone(), stopping.clone()) => accepted,
            _ = stopping.wait_for(|stop| *stop) => return,
        };
        match accepted {
            Ok(serving) => {
                tokio::spawn(async move {
                    serving.await;
                    drop(permit);
                });
            }
            Err(error) => {
                // A failed accept, such as a full descriptor table, is
                // logged and retried after a pause so it cannot spin.
                log.event(
                    LogLevel::Err,
                    "rrdcached_accept_failed",
                    &format!("accept failed: {error}"),
                );
                drop(permit);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

enum Accepting {
    Unix {
        listener: tokio::net::UnixListener,
        // Removes the socket file when the listener closes.
        _file: super::listener::SocketFile,
    },
    Tcp(tokio::net::TcpListener),
}

/// A served connection, boxed so both stream types share one type.
type Serving = std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

impl Accepting {
    async fn accept<E: Engine>(
        &self,
        engine: &Arc<E>,
        permissions: Option<Arc<[String]>>,
        stopping: watch::Receiver<bool>,
    ) -> std::io::Result<Serving> {
        Ok(match self {
            Self::Unix { listener, .. } => {
                let (stream, _) = listener.accept().await?;
                Box::pin(engine.serve(stream, permissions, stopping))
            }
            Self::Tcp(listener) => {
                let (stream, _) = listener.accept().await?;
                let _ = stream.set_nodelay(true);
                Box::pin(engine.serve(stream, permissions, stopping))
            }
        })
    }
}

/// One write thread: takes queued entries and writes them until shutdown.
/// Entries left queued at shutdown keep their values in the cache, where
/// the final write or the journal covers them.
async fn write_thread<E: Engine>(
    engine: Arc<E>,
    jitter: Duration,
    mut seed: u64,
    mut stopping: watch::Receiver<bool>,
) {
    let queued = engine.queued();
    loop {
        let notified = queued.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if *stopping.borrow() {
            return;
        }
        let Some(path) = engine.next_queued() else {
            tokio::select! {
                _ = notified => continue,
                _ = stopping.wait_for(|stop| *stop) => return,
            }
        };
        // Another entry may be waiting; let an idle thread look.
        queued.notify_one();
        if !jitter.is_zero() {
            // Compatibility: `-z` delays each write by a random time below
            // the given delay. Provenance: rrdcached(1).
            seed = next_random(seed);
            let delay = Duration::from_nanos(seed % jitter.as_nanos().max(1) as u64);
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                _ = stopping.wait_for(|stop| *stop) => return,
            }
        }
        let writer = Arc::clone(&engine);
        let _ = tokio::task::spawn_blocking(move || writer.write(&path)).await;
    }
}

/// Runs the `-f` pass every `period`, starting one period after startup.
// Compatibility: idle entries are written within `-w` plus `-f`, and an
// entry with nothing pending leaves the cache once it has been idle for
// `-f` (RCD-40, RCD-41). Provenance: rrdcached(1) and black-box
// compatibility test.
async fn flush_timer<E: Engine>(
    engine: Arc<E>,
    period: Duration,
    mut stopping: watch::Receiver<bool>,
) {
    let period = period.max(Duration::from_secs(1));
    let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            _ = stopping.wait_for(|stop| *stop) => return,
        }
        let pass = Arc::clone(&engine);
        let idle = period.as_secs();
        let _ = tokio::task::spawn_blocking(move || pass.flush_pass(idle)).await;
    }
}

fn jitter_seed(thread: usize) -> u64 {
    let clock = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    clock
        ^ u64::from(std::process::id()).rotate_left(32)
        ^ (thread as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

/// xorshift64*: enough to spread writes, not for anything secret.
fn next_random(mut state: u64) -> u64 {
    if state == 0 {
        state = 0x2545_f491_4f6c_dd1d;
    }
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// An engine that answers `PING` and records what the daemon asks of it.
    #[derive(Default)]
    struct FakeEngine {
        state: Mutex<FakeState>,
        queued: Arc<Notify>,
        flushes_at_shutdown: bool,
    }

    #[derive(Default)]
    struct FakeState {
        pending: BTreeMap<PathBuf, u32>,
        queue: VecDeque<PathBuf>,
        written: Vec<PathBuf>,
        passes: u32,
        finished: Option<bool>,
    }

    impl FakeEngine {
        fn with_pending(paths: &[&str], flushes_at_shutdown: bool) -> Self {
            let engine = Self {
                flushes_at_shutdown,
                ..Self::default()
            };
            for path in paths {
                engine
                    .state
                    .lock()
                    .unwrap()
                    .pending
                    .insert(PathBuf::from(path), 1);
            }
            engine
        }

        fn enqueue(&self, path: &str) {
            self.state
                .lock()
                .unwrap()
                .queue
                .push_back(PathBuf::from(path));
            self.queued.notify_one();
        }
    }

    impl Engine for FakeEngine {
        // The connection future must not borrow the engine.
        #[allow(clippy::manual_async_fn)]
        fn serve<S>(
            &self,
            stream: S,
            _permissions: Option<Arc<[String]>>,
            mut shutdown: watch::Receiver<bool>,
        ) -> impl Future<Output = ()> + Send + 'static
        where
            S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        {
            async move {
                let mut lines = BufReader::new(stream);
                loop {
                    let mut line = String::new();
                    let read = tokio::select! {
                        read = lines.read_line(&mut line) => read.unwrap_or(0),
                        _ = shutdown.wait_for(|stop| *stop) => 0,
                    };
                    if read == 0 {
                        return;
                    }
                    let _ = lines.get_mut().write_all(b"0 PONG\n").await;
                }
            }
        }

        fn next_queued(&self) -> Option<PathBuf> {
            self.state.lock().unwrap().queue.pop_front()
        }

        fn queued(&self) -> Arc<Notify> {
            Arc::clone(&self.queued)
        }

        fn write(&self, path: &Path) {
            let mut state = self.state.lock().unwrap();
            state.pending.remove(path);
            state.written.push(path.to_path_buf());
        }

        fn flush_pass(&self, _idle_seconds: u64) {
            self.state.lock().unwrap().passes += 1;
        }

        fn pending(&self) -> Vec<PathBuf> {
            self.state.lock().unwrap().pending.keys().cloned().collect()
        }

        fn flushes_at_shutdown(&self) -> bool {
            self.flushes_at_shutdown
        }

        fn finish(&self, written: bool) {
            self.state.lock().unwrap().finished = Some(written);
        }
    }

    fn timing() -> Timing {
        Timing {
            write_threads: 2,
            flush_interval: Duration::from_secs(1),
            write_jitter: Duration::ZERO,
            journaled: false,
        }
    }

    fn daemon(engine: &Arc<FakeEngine>, listeners: Vec<Listener>) -> Daemon<FakeEngine> {
        Daemon {
            engine: Arc::clone(engine),
            listeners,
            timing: timing(),
            log: Arc::new(Log::start(LogLevel::Err, None)),
        }
    }

    async fn stop_with(engine: &Arc<FakeEngine>, signals: &[DaemonSignal]) -> Outcome {
        let (sender, receiver) = mpsc::channel(8);
        for signal in signals {
            sender.send(*signal).await.unwrap();
        }
        daemon(engine, Vec::new()).run(receiver).await
    }

    #[tokio::test]
    async fn signals_choose_whether_pending_values_are_written() {
        for (signals, flushes, written) in [
            (&[DaemonSignal::Terminate][..], true, true),
            (&[DaemonSignal::Terminate], false, false),
            (
                &[
                    DaemonSignal::Hangup,
                    DaemonSignal::Alarm,
                    DaemonSignal::User1,
                ],
                false,
                true,
            ),
            (&[DaemonSignal::User2, DaemonSignal::User1], true, false),
            (&[DaemonSignal::Interrupt], true, true),
        ] {
            let engine = Arc::new(FakeEngine::with_pending(&["a.rrd", "b.rrd"], flushes));
            let outcome = stop_with(&engine, signals).await;
            assert_eq!(outcome.written, written, "{signals:?} {flushes}");
            let state = engine.state.lock().unwrap();
            assert_eq!(state.finished, Some(written));
            assert_eq!(state.pending.is_empty(), written);
        }
    }

    #[tokio::test]
    async fn write_threads_drain_the_queue() {
        let engine = Arc::new(FakeEngine::default());
        let (sender, receiver) = mpsc::channel(1);
        let running = tokio::spawn(daemon(&engine, Vec::new()).run(receiver));
        for path in ["a.rrd", "b.rrd", "c.rrd"] {
            engine.enqueue(path);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while engine.state.lock().unwrap().written.len() < 3 {
            assert!(tokio::time::Instant::now() < deadline, "queue not drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        sender.send(DaemonSignal::Terminate).await.unwrap();
        running.await.unwrap();
        assert!(engine.state.lock().unwrap().queue.is_empty());
    }

    #[tokio::test]
    async fn flush_timer_runs_once_per_interval() {
        let engine = Arc::new(FakeEngine::default());
        let (sender, receiver) = mpsc::channel(1);
        let running = tokio::spawn(daemon(&engine, Vec::new()).run(receiver));
        tokio::time::sleep(Duration::from_millis(2500)).await;
        sender.send(DaemonSignal::Terminate).await.unwrap();
        running.await.unwrap();
        assert_eq!(engine.state.lock().unwrap().passes, 2);
    }

    #[tokio::test]
    async fn idle_clients_do_not_hold_up_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let std_listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        let listeners = vec![
            Listener {
                socket: BoundSocket::Tcp(tcp),
                permissions: None,
            },
            Listener {
                socket: BoundSocket::Unix(
                    std_listener,
                    super::super::listener::tests_socket_file(&path),
                ),
                permissions: None,
            },
        ];
        let engine = Arc::new(FakeEngine::default());
        let (sender, receiver) = mpsc::channel(1);
        let running = tokio::spawn(daemon(&engine, listeners).run(receiver));
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        client.write_all(b"PING\n").await.unwrap();
        let mut reply = String::new();
        BufReader::new(&mut client)
            .read_line(&mut reply)
            .await
            .unwrap();
        assert_eq!(reply, "0 PONG\n");
        let _idle = tokio::net::UnixStream::connect(&path).await.unwrap();
        sender.send(DaemonSignal::Terminate).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("shutdown waited on idle clients")
            .unwrap();
        assert!(!path.exists(), "the Unix socket file was left behind");
    }

    #[test]
    fn random_delays_vary() {
        let first = next_random(1);
        assert_ne!(first, next_random(first));
        assert_ne!(next_random(0), 0);
    }
}
