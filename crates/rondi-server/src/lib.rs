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
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};

pub mod rrdcached;

/// Default bound on the memory held by values the `rrdcached` cache has
/// accepted but not yet written.
pub const DEFAULT_RRDCACHED_QUEUE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub root: PathBuf,
    pub socket: PathBuf,
    pub queue_capacity: usize,
    pub store: StoreOptions,
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

/// A JSON daemon prepared before runtime threads start.
pub struct PreparedServer {
    args: ServerConfig,
    store: Store,
    listener: std::os::unix::net::UnixListener,
}

/// Recover the store and publish a mode-0600 socket using a private staging
/// directory. Call before starting any threads because binding uses fchdir.
pub fn prepare_server(args: ServerConfig) -> Result<PreparedServer, Box<dyn std::error::Error>> {
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
    let (replayed, skipped) = store.recover_counting_skipped()?;
    if skipped > 0 {
        tracing::warn!(
            skipped,
            "server_journal_records_for_missing_databases_skipped"
        );
    }
    tracing::info!(root = %args.root.display(), recovered = replayed, "server_started");
    let listener = rrdcached::open_json_listener(&args.socket)?;
    listener.set_nonblocking(true)?;
    Ok(PreparedServer {
        args,
        store,
        listener,
    })
}

/// Run the prepared local HTTP/JSON daemon, holding its store lock until exit.
pub async fn run(prepared: PreparedServer) -> Result<(), Box<dyn std::error::Error>> {
    let PreparedServer {
        args,
        store,
        listener,
    } = prepared;
    let listener = UnixListener::from_std(listener)?;
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
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    // listen_thread_main logs a failed accept(2) and keeps
                    // listening. The pause stops an
                    // exhausted descriptor table from spinning the loop.
                    Err(error) => {
                        tracing::error!(error = %error, "server_accept_failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
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
