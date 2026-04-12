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

#[derive(Clone)]
pub struct Metrics {
    registry: Registry,
    rpc_requests_total: IntCounterVec,
    rpc_duration_seconds: HistogramVec,
    lock_conflicts_total: IntCounter,
    session_creates_total: IntCounter,
    session_commits_total: IntCounter,
    session_aborts_total: IntCounter,
    active_sessions: IntGauge,
    active_locks: IntGauge,
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
            session_creates_total,
            session_commits_total,
            session_aborts_total,
            active_sessions,
            active_locks,
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
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .with_state(metrics);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
