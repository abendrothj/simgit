//! Async client over the simgitd Unix domain socket (JSON-RPC 2.0).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

use crate::error::SdkError;
use crate::types::*;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Default socket path: `$XDG_RUNTIME_DIR/simgit/control.sock`
/// Falls back to `/tmp/simgit-<uid>/control.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("simgit").join("control.sock");
    }
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/simgit-{uid}/control.sock"))
}

// Re-export libc just for the uid call above — not part of the public API.
extern crate libc;

/// Stateless client; each method opens a fresh socket connection.
/// For high-frequency call sites, consider wrapping in a connection pool.
pub struct Client {
    socket_path: PathBuf,
}

impl Client {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self { socket_path: socket_path.as_ref().to_owned() }
    }

    pub fn from_env() -> Self {
        Self::new(default_socket_path())
    }

    // ── low-level RPC ────────────────────────────────────────────────────────

    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, SdkError> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                SdkError::DaemonNotFound(self.socket_path.display().to_string())
            } else {
                SdkError::Io(e)
            }
        })?;

        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id:      next_id(),
            method:  method.into(),
            params,
        };
        let mut payload = serde_json::to_string(&req)?;
        payload.push('\n'); // newline-delimited JSON

        let (read_half, mut write_half) = stream.into_split();
        write_half.write_all(payload.as_bytes()).await?;
        write_half.shutdown().await?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await?;

        let resp: RpcResponse = serde_json::from_str(line.trim())?;

        if let Some(err) = resp.error {
            return Err(match err.code {
                ERR_BORROW_CONFLICT => {
                    let be: BorrowError = serde_json::from_value(
                        err.data.unwrap_or(serde_json::Value::Null),
                    )?;
                    SdkError::BorrowConflict(be)
                }
                ERR_SESSION_NOT_FOUND => SdkError::SessionNotFound(err.message),
                ERR_MERGE_CONFLICT => {
                    let paths: Vec<PathBuf> = serde_json::from_value(
                        err.data.unwrap_or(serde_json::Value::Null),
                    )
                    .unwrap_or_default();
                    SdkError::MergeConflict(paths)
                }
                ERR_QUOTA_EXCEEDED => SdkError::QuotaExceeded(err.message),
                _ => SdkError::Rpc { code: err.code, message: err.message },
            });
        }

        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    // ── session methods ──────────────────────────────────────────────────────

    /// Create a new agent session. Returns the session and its mount path.
    pub async fn session_create(
        &self,
        task_id:      impl Into<String>,
        agent_label:  Option<String>,
        base_commit:  Option<String>,
        peers:        bool,
    ) -> Result<SessionInfo, SdkError> {
        let result = self
            .call(
                "session.create",
                serde_json::json!({
                    "task_id":     task_id.into(),
                    "agent_label": agent_label,
                    "base_commit": base_commit,
                    "peers":       peers,
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Flatten the session's delta layer into a git branch and release locks.
    pub async fn session_commit(
        &self,
        session_id:  Uuid,
        branch_name: Option<String>,
        message:     Option<String>,
    ) -> Result<SessionInfo, SdkError> {
        let result = self
            .call(
                "session.commit",
                serde_json::json!({
                    "session_id":  session_id,
                    "branch_name": branch_name,
                    "message":     message,
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Discard the session's delta layer and release all locks.
    pub async fn session_abort(&self, session_id: Uuid) -> Result<(), SdkError> {
        self.call("session.abort", serde_json::json!({ "session_id": session_id }))
            .await?;
        Ok(())
    }

    /// List sessions, optionally filtered by status.
    pub async fn session_list(
        &self,
        status: Option<SessionStatus>,
    ) -> Result<Vec<SessionInfo>, SdkError> {
        let result = self
            .call("session.list", serde_json::json!({ "status": status }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Return the unified diff of an in-flight session.
    pub async fn session_diff(&self, session_id: Uuid) -> Result<DiffResult, SdkError> {
        let result = self
            .call("session.diff", serde_json::json!({ "session_id": session_id }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    // ── lock methods ─────────────────────────────────────────────────────────

    /// List current locks, optionally filtered by path prefix.
    pub async fn lock_list(
        &self,
        path: Option<&Path>,
    ) -> Result<Vec<LockInfo>, SdkError> {
        let result = self
            .call("lock.list", serde_json::json!({ "path": path }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Long-poll until the write lock on `path` is free (or timeout elapses).
    pub async fn lock_wait(
        &self,
        path:       &Path,
        timeout_ms: Option<u64>,
    ) -> Result<bool, SdkError> {
        let result = self
            .call(
                "lock.wait",
                serde_json::json!({ "path": path, "timeout_ms": timeout_ms }),
            )
            .await?;
        Ok(result["acquired"].as_bool().unwrap_or(false))
    }

    /// List recent events, optionally filtered by source session.
    pub async fn event_list(
        &self,
        session_id: Option<Uuid>,
        limit: Option<usize>,
    ) -> Result<Vec<PeerEvent>, SdkError> {
        let result = self
            .call(
                "event.list",
                serde_json::json!({
                    "session_id": session_id,
                    "limit": limit,
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }
}
