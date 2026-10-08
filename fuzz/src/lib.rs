//! Shared fuzz harness code.
//!
//! The CLI and the rrdcached protocol handlers are private to their crates, so
//! their sources are compiled here a second time with small entry points added
//! next to them. Nothing in this crate ships.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Fixed "now" so time-relative parsing is deterministic across runs.
pub const NOW: i64 = 1_000_000_100;

/// Seeds that every stateful target copies into a fresh directory.
pub const SEED_RRDS: &[(&str, &[u8])] = &[
    (
        "g.rrd",
        include_bytes!("../seeds/rrd_file/gauge_v3_upd.rrd"),
    ),
    ("c.rrd", include_bytes!("../seeds/rrd_file/counter.rrd")),
    ("d.rrd", include_bytes!("../seeds/rrd_file/dcounter_v5.rrd")),
];

/// Per-process scratch directory, created once and kept for the run.
pub fn scratch() -> &'static Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        // TZ is fixed so local-time arithmetic does not depend on the host.
        // SAFETY: called once, before any fuzz iteration starts other threads.
        unsafe { std::env::set_var("TZ", "UTC") };
        // libFuzzer's C main skips Rust's SIGPIPE setup; without this a
        // response written after the client half closes kills the process
        // with no report, as it would not in the real daemon.
        // SAFETY: installing SIG_IGN has no handler code to run.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
        tempfile::Builder::new()
            .prefix("rondi-fuzz-")
            .tempdir()
            .expect("scratch dir")
    })
    .path()
}

/// A fresh, canonical directory holding the seed RRDs.
pub fn fresh_root() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let root = scratch().join(format!("r{}", id % 4));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    for (name, bytes) in SEED_RRDS {
        std::fs::write(root.join(name), bytes).expect("seed");
    }
    std::fs::canonicalize(&root).expect("canonical root")
}

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    })
}

#[allow(dead_code, unused_imports, unused_variables, clippy::all)]
pub mod cli {
    include!("../../crates/rondi-cli/src/main.rs");

    pub fn fuzz_time(spec: &str, other: &str) {
        let now = crate::NOW;
        let _ = rondi::time::parse_rrd_time(spec, now);
        let _ = rondi::time::rrd_parsetime(spec, now);
        let _ = rondi::time::resolve_rrd_range_times(Some(spec), Some(other), now);
        let _ = rondi::time::resolve_rrd_range_times(Some(other), Some(spec), now);
        let _ = rondi::time::resolve_rrd_range_times(Some(spec), None, now);
        let _ = parse_rrd_update_timestamp(spec, now as f64 + 0.25);
        let _ = parse_rrd_scaled_duration(spec, 1);
        let _ = parse_rrd_scaled_duration(spec, 300);
        let _ = rondi::parse_rrd_number(spec);
    }

    /// `rrdtool xport` with caller-supplied arguments after `xport`.
    pub fn fuzz_xport(args: &[String]) -> Result<String, String> {
        let mut argv = vec![String::from("xport")];
        argv.extend_from_slice(args);
        match render_xport(&argv) {
            Ok((output, None)) => Ok(output),
            Ok((_, Some(error))) => Err(error),
            Err(error) => Err(error.to_string()),
        }
    }

    /// `rrdtool graph <out> ...`; output goes to a scratch file.
    pub fn fuzz_graph(args: &[String]) -> Result<(), String> {
        let out = crate::scratch().join("graph.out");
        let mut argv = vec![String::from("graph"), out.display().to_string()];
        argv.extend_from_slice(args);
        rrdtool_graph(&argv, true).map_err(|e| e.to_string())
    }

    /// `rrdtool update`/`updatev` on a seed file.
    pub fn fuzz_update(
        file: &std::path::Path,
        args: &[String],
        verbose: bool,
    ) -> Result<(), String> {
        let mut argv = vec![
            String::from(if verbose { "updatev" } else { "update" }),
            file.display().to_string(),
        ];
        argv.extend_from_slice(args);
        rrdtool_update_impl(&argv, verbose).map_err(|e| e.to_string())
    }

    /// `rrdtool create` into the scratch directory.
    pub fn fuzz_create(file: &std::path::Path, args: &[String]) -> Result<(), String> {
        let mut argv = vec![String::from("create"), file.display().to_string()];
        argv.extend_from_slice(args);
        rrdtool_create(&argv).map_err(|e| e.to_string())
    }
}

#[allow(dead_code, unused_imports, unused_variables, clippy::all)]
pub mod server {
    include!("../../crates/rondi-server/src/lib.rs");

    /// Feed one client byte stream through the real connection loop
    /// (including BATCH and FETCHBIN) and return everything it wrote back.
    pub async fn fuzz_rrdcached(
        root: &Path,
        input: &[u8],
        socket_commands: Option<Vec<String>>,
    ) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let queue = match RrdcachedQueue::open(root, 1 << 20, 0) {
            Ok(queue) => Arc::new(Mutex::new(queue)),
            Err(_) => return Vec::new(),
        };
        let stats = Arc::new(RrdcachedStats::default());
        let (client, server) = UnixStream::pair().expect("socketpair");
        let (_stop, stop_rx) = tokio::sync::watch::channel(false);
        let serve = serve_rrdcached_connection(
            server,
            root.to_path_buf(),
            root.to_path_buf(),
            stats,
            Arc::clone(&queue),
            false,
            true,
            socket_commands,
            stop_rx,
        );
        let (mut reader, mut writer) = client.into_split();
        let input = input.to_vec();
        let write = async move {
            let _ = writer.write_all(&input).await;
            let _ = writer.shutdown().await;
            drop(writer);
        };
        let read = async move {
            let mut out = Vec::new();
            let _ = (&mut reader).take(16 << 20).read_to_end(&mut out).await;
            out
        };
        let (_, _, out) = tokio::join!(serve, write, read);
        // Flush whatever is still queued so replayed samples reach the files.
        let paths = queue
            .lock()
            .map(|q| q.pending.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let stats = RrdcachedStats::default();
        for path in paths {
            let _ = flush_rrdcached_path(&path, &queue, &stats);
        }
        out
    }

    /// Replay an arbitrary journal and flush what it recovers.
    pub fn fuzz_journal(root: &Path, journal: &[u8]) {
        std::fs::write(root.join(RRDCACHED_JOURNAL_NAME), journal).expect("journal");
        let Ok(queue) = RrdcachedQueue::open(root, 1 << 20, 0) else {
            return;
        };
        let queue = Mutex::new(queue);
        let stats = RrdcachedStats::default();
        let paths = queue
            .lock()
            .map(|q| q.pending.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for path in paths {
            // Replay must never hand back a path outside the base directory.
            assert!(
                path.starts_with(root),
                "journal escaped root: {}",
                path.display()
            );
            let _ = flush_rrdcached_path(&path, &queue, &stats);
        }
        if let Ok(mut queue) = queue.lock() {
            let _ = queue.rotate_journal();
        }
    }

    /// Serve one raw HTTP/1 request stream through the JSON API handler.
    pub async fn fuzz_http(root: &Path, input: &[u8]) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let Ok(store) = Store::open(root) else {
            return Vec::new();
        };
        let _ = store.recover();
        let store = Arc::new(store);
        let (tx, mut rx) = mpsc::channel::<Pending>(8);
        let worker_store = Arc::clone(&store);
        let worker = async move {
            while let Some(request) = rx.recv().await {
                let result =
                    worker_store.update_durable(&request.name, request.update, &request.id);
                let _ = request.reply.send(result);
            }
        };
        let (client, server) = tokio::io::duplex(1 << 16);
        let service_store = Arc::clone(&store);
        let service = service_fn(move |request| {
            let sender = tx.clone();
            let store = Arc::clone(&service_store);
            async move { Ok::<_, Infallible>(handle(request, sender, store).await) }
        });
        let conn = async move {
            let _ = http1::Builder::new()
                .keep_alive(true)
                .serve_connection(TokioIo::new(server), service)
                .await;
        };
        let (mut reader, mut writer) = tokio::io::split(client);
        let input = input.to_vec();
        let write = async move {
            let _ = writer.write_all(&input).await;
            let _ = writer.shutdown().await;
        };
        let read = async move {
            let mut out = Vec::new();
            let _ = (&mut reader).take(16 << 20).read_to_end(&mut out).await;
            out
        };
        let (_, _, out, _) = tokio::join!(conn, write, read, worker);
        out
    }
}
