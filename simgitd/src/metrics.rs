use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

pub struct Metrics {
    registry: Registry,
    rpc_requests_total: IntCounterVec,
    rpc_duration_seconds: HistogramVec,
    lock_conflicts_total: IntCounter,
    lock_wait_timeouts_total: IntCounter,
    lock_wait_duration_seconds: HistogramVec,
    session_creates_total: IntCounter,
    session_commits_total: IntCounter,
    session_aborts_total: IntCounter,
    active_sessions: IntGauge,
    active_locks: IntGauge,
    contention_by_path: Mutex<HashMap<String, u64>>,
}

impl Metrics {
    pub fn new() -> Result<Self> {
        let registry = Registry::new_custom(Some("simgit".to_owned()), None)?;

        let rpc_requests_total = IntCounterVec::new(
            Opts::new("rpc_requests_total", "Total RPC requests by method/outcome"),
            &["method", "outcome"],
        )?;
        let rpc_duration_seconds = HistogramVec::new(
            HistogramOpts::new("rpc_duration_seconds", "RPC handler latency in seconds"),
            &["method"],
        )?;
        let lock_conflicts_total = IntCounter::new(
            "lock_conflicts_total",
            "Total borrow/lock conflict responses",
        )?;
        let lock_wait_timeouts_total = IntCounter::new(
            "lock_wait_timeouts_total",
            "Total lock.wait calls that timed out",
        )?;
        let lock_wait_duration_seconds = HistogramVec::new(
            HistogramOpts::new("lock_wait_duration_seconds", "lock.wait duration in seconds"),
            &["outcome"],
        )?;
        let session_creates_total = IntCounter::new(
            "session_creates_total",
            "Total successful session.create calls",
        )?;
        let session_commits_total = IntCounter::new(
            "session_commits_total",
            "Total successful session.commit calls",
        )?;
        let session_aborts_total = IntCounter::new(
            "session_aborts_total",
            "Total successful session.abort calls",
        )?;
        let active_sessions = IntGauge::new("active_sessions", "Current ACTIVE sessions")?;
        let active_locks = IntGauge::new("active_locks", "Current lock table size")?;

        registry.register(Box::new(rpc_requests_total.clone()))?;
        registry.register(Box::new(rpc_duration_seconds.clone()))?;
        registry.register(Box::new(lock_conflicts_total.clone()))?;
        registry.register(Box::new(lock_wait_timeouts_total.clone()))?;
        registry.register(Box::new(lock_wait_duration_seconds.clone()))?;
        registry.register(Box::new(session_creates_total.clone()))?;
        registry.register(Box::new(session_commits_total.clone()))?;
        registry.register(Box::new(session_aborts_total.clone()))?;
        registry.register(Box::new(active_sessions.clone()))?;
        registry.register(Box::new(active_locks.clone()))?;

        Ok(Self {
            registry,
            rpc_requests_total,
            rpc_duration_seconds,
            lock_conflicts_total,
            lock_wait_timeouts_total,
            lock_wait_duration_seconds,
            session_creates_total,
            session_commits_total,
            session_aborts_total,
            active_sessions,
            active_locks,
            contention_by_path: Mutex::new(HashMap::new()),
        })
    }

    pub fn observe_rpc(
        &self,
        method: &str,
        success: bool,
        error_code: Option<i32>,
        elapsed_seconds: f64,
    ) {
        let outcome = if success { "ok" } else { "error" };
        self.rpc_requests_total
            .with_label_values(&[method, outcome])
            .inc();
        self.rpc_duration_seconds
            .with_label_values(&[method])
            .observe(elapsed_seconds);

        if method == "session.create" && success {
            self.session_creates_total.inc();
        }
        if method == "session.commit" && success {
            self.session_commits_total.inc();
        }
        if method == "session.abort" && success {
            self.session_aborts_total.inc();
        }
        if error_code == Some(simgit_sdk::ERR_BORROW_CONFLICT) {
            self.lock_conflicts_total.inc();
        }
    }

    pub fn set_active_counts(&self, sessions: usize, locks: usize) {
        self.active_sessions.set(sessions as i64);
        self.active_locks.set(locks as i64);
    }

    pub fn observe_lock_wait(&self, acquired: bool, elapsed_seconds: f64) {
        let outcome = if acquired { "acquired" } else { "timeout" };
        self.lock_wait_duration_seconds
            .with_label_values(&[outcome])
            .observe(elapsed_seconds);
        if !acquired {
            self.lock_wait_timeouts_total.inc();
        }
    }

    pub fn record_lock_contention_path(&self, path: &Path) {
        let path = path.to_string_lossy().to_string();
        let mut counts = self.contention_by_path.lock().unwrap();
        *counts.entry(path).or_insert(0) += 1;
    }

    pub fn top_contended_paths(&self, limit: usize) -> Vec<(String, u64)> {
        let mut entries: Vec<_> = self
            .contention_by_path
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(limit);
        entries
    }

    pub fn render(&self) -> Result<String> {
        let mf = self.registry.gather();
        let mut buf = Vec::new();
        TextEncoder::new().encode(&mf, &mut buf)?;
        Ok(String::from_utf8(buf)?)
    }
}

async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    match metrics.render() {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub async fn serve(metrics: Arc<Metrics>, addr: &str) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(metrics, listener).await
}

pub async fn serve_listener(
    metrics: Arc<Metrics>,
    listener: tokio::net::TcpListener,
) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn metrics_endpoint_exposes_key_series() {
        let metrics = Arc::new(Metrics::new().expect("metrics init"));
        metrics.observe_rpc("session.create", true, None, 0.001);
        metrics.observe_lock_wait(false, 0.05);
        metrics.set_active_counts(2, 3);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");

        let server_metrics = Arc::clone(&metrics);
        let server_task = tokio::spawn(async move {
            let _ = serve_listener(server_metrics, listener).await;
        });

        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .expect("connect metrics server");
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("write request");

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let response = String::from_utf8(response).expect("valid utf8");

        assert!(response.contains("HTTP/1.1 200 OK"));
        assert!(response.contains("simgit_rpc_requests_total"));
        assert!(response.contains("simgit_rpc_duration_seconds"));
        assert!(response.contains("simgit_lock_conflicts_total"));
        assert!(response.contains("simgit_lock_wait_duration_seconds"));
        assert!(response.contains("simgit_lock_wait_timeouts_total"));
        assert!(response.contains("simgit_active_sessions"));
        assert!(response.contains("simgit_active_locks"));

        server_task.abort();
    }

    #[test]
    fn top_contended_paths_returns_sorted_counts() {
        let metrics = Metrics::new().expect("metrics init");
        metrics.record_lock_contention_path(Path::new("/src/a.rs"));
        metrics.record_lock_contention_path(Path::new("/src/b.rs"));
        metrics.record_lock_contention_path(Path::new("/src/a.rs"));

        let top = metrics.top_contended_paths(2);
        assert_eq!(top[0], ("/src/a.rs".to_owned(), 2));
        assert_eq!(top[1], ("/src/b.rs".to_owned(), 1));
    }
}
