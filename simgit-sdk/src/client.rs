//! Async client over the simgitd Unix domain socket (JSON-RPC 2.0).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

use crate::error::SdkError;
use crate::types::*;
use std::time::Duration;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn decode_rpc_error(err: RpcError) -> Result<SdkError, SdkError> {
    match err.code {
        ERR_BORROW_CONFLICT => {
            let be: BorrowError = serde_json::from_value(
                err.data.unwrap_or(serde_json::Value::Null),
            )?;
            Ok(SdkError::BorrowConflict(be))
        }
        ERR_SESSION_NOT_FOUND => Ok(SdkError::SessionNotFound(err.message)),
        ERR_MERGE_CONFLICT => {
            let detail: MergeConflictDetail = serde_json::from_value(
                err.data.unwrap_or(serde_json::Value::Null),
            )?;
            Ok(SdkError::MergeConflict(detail))
        }
        ERR_QUOTA_EXCEEDED => Ok(SdkError::QuotaExceeded(err.message)),
        ERR_DEADLINE_EXCEEDED => Ok(SdkError::DeadlineExceeded(err.message)),
        _ => Ok(SdkError::Rpc { code: err.code, message: err.message }),
    }
}

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn rpc_timeout_secs() -> u64 {
    std::env::var("SIMGIT_RPC_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(30)
}

fn rpc_retry_count() -> u32 {
    std::env::var("SIMGIT_RPC_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(2)
}

fn duration_to_timeout_secs(timeout: Duration) -> u64 {
    let millis = timeout.as_millis();
    if millis == 0 {
        return 1;
    }
    ((millis + 999) / 1000) as u64
}

fn deadline_epoch_ms_from_timeout(timeout: Duration) -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let deadline = now.saturating_add(timeout);
    i64::try_from(deadline.as_millis()).unwrap_or(i64::MAX)
}

fn is_retryable_io(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::NotFound
    )
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
        self.call_with_timeout(method, params, None).await
    }

    async fn call_with_timeout(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout_override: Option<Duration>,
    ) -> Result<serde_json::Value, SdkError> {
        let attempts = rpc_retry_count() + 1;
        let timeout_secs = timeout_override
            .map(duration_to_timeout_secs)
            .unwrap_or_else(rpc_timeout_secs);
        let mut last_err: Option<SdkError> = None;

        for attempt in 1..=attempts {
            let stream = match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => stream,
                Err(e) => {
                    let err = if e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::ConnectionRefused
                    {
                        SdkError::DaemonNotFound(self.socket_path.display().to_string())
                    } else {
                        SdkError::Io(e)
                    };
                    if attempt < attempts {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                        last_err = Some(err);
                        continue;
                    }
                    return Err(err);
                }
            };

            let req = RpcRequest {
                jsonrpc: "2.0".into(),
                id:      next_id(),
                method:  method.into(),
                params:  params.clone(),
            };
            let mut payload = serde_json::to_string(&req)?;
            payload.push('\n'); // newline-delimited JSON

            let (read_half, mut write_half) = stream.into_split();
            if let Err(e) = write_half.write_all(payload.as_bytes()).await {
                if attempt < attempts && is_retryable_io(&e) {
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                    last_err = Some(SdkError::Io(e));
                    continue;
                }
                return Err(SdkError::Io(e));
            }
            if let Err(e) = write_half.shutdown().await {
                if attempt < attempts && is_retryable_io(&e) {
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                    last_err = Some(SdkError::Io(e));
                    continue;
                }
                return Err(SdkError::Io(e));
            }

            let mut reader = BufReader::new(read_half);
            let mut line = String::new();
            let read_result = tokio::time::timeout(
                Duration::from_secs(timeout_secs),
                reader.read_line(&mut line)
            )
            .await;

            let bytes = match read_result {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => {
                    if attempt < attempts && is_retryable_io(&e) {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                        last_err = Some(SdkError::Io(e));
                        continue;
                    }
                    return Err(SdkError::Io(e));
                }
                Err(_) => {
                    let e = std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("RPC response timeout ({}s)", timeout_secs),
                    );
                    if attempt < attempts {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                        last_err = Some(SdkError::Io(e));
                        continue;
                    }
                    return Err(SdkError::Io(e));
                }
            };

            if bytes == 0 || line.trim().is_empty() {
                let e = std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "empty RPC response",
                );
                if attempt < attempts {
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                    last_err = Some(SdkError::Io(e));
                    continue;
                }
                return Err(SdkError::Io(e));
            }

            let resp: RpcResponse = match serde_json::from_str(line.trim()) {
                Ok(resp) => resp,
                Err(e) => {
                    if attempt < attempts {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                        last_err = Some(SdkError::Json(e));
                        continue;
                    }
                    return Err(SdkError::Json(e));
                }
            };

            if let Some(err) = resp.error {
                return Err(decode_rpc_error(err)?);
            }

            return Ok(resp.result.unwrap_or(serde_json::Value::Null));
        }

        Err(last_err.unwrap_or_else(|| {
            SdkError::Io(std::io::Error::other("RPC call failed after retries"))
        }))
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
    ) -> Result<SessionCommitResult, SdkError> {
        self.session_commit_with_timeout(session_id, branch_name, message, None)
            .await
    }

    /// Flatten the session's delta layer into a git branch and release locks.
    ///
    /// `timeout_override` applies to this call only (transport timeout +
    /// daemon-side `deadline_epoch_ms`), without changing global SDK defaults.
    pub async fn session_commit_with_timeout(
        &self,
        session_id: Uuid,
        branch_name: Option<String>,
        message: Option<String>,
        timeout_override: Option<Duration>,
    ) -> Result<SessionCommitResult, SdkError> {
        let effective_timeout = timeout_override.unwrap_or_else(|| Duration::from_secs(rpc_timeout_secs()));
        let deadline_epoch_ms = deadline_epoch_ms_from_timeout(effective_timeout);
        let result = self
            .call_with_timeout(
                "session.commit",
                serde_json::json!({
                    "session_id":  session_id,
                    "branch_name": branch_name,
                    "message":     message,
                    "deadline_epoch_ms": deadline_epoch_ms,
                }),
                timeout_override,
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

    /// Wait for the next event from a source session (or global stream).
    ///
    /// Returns `Ok(None)` on timeout.
    pub async fn event_subscribe(
        &self,
        session_id: Option<Uuid>,
        timeout_ms: Option<u64>,
    ) -> Result<Option<PeerEvent>, SdkError> {
        let result = self
            .call(
                "event.subscribe",
                serde_json::json!({
                    "session_id": session_id,
                    "timeout_ms": timeout_ms,
                }),
            )
            .await?;

        if result["timeout"].as_bool().unwrap_or(false) {
            return Ok(None);
        }
        let event = result
            .get("event")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if event.is_null() {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(event)?))
    }
}

#[cfg(test)]
mod tests {
    use super::decode_rpc_error;
    use crate::error::SdkError;
    use crate::types::{RpcError, ERR_DEADLINE_EXCEEDED, ERR_MERGE_CONFLICT};
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn decode_merge_conflict_preserves_paths_and_peer_sessions() {
        let err = RpcError {
            code: ERR_MERGE_CONFLICT,
            message: "pre-commit conflict: overlapping active session paths".to_owned(),
            data: Some(json!({
                "session_id": "019d8664-4b4c-79d3-8155-44df5d3203c5",
                "kind": "active_session_overlap",
                "conflicts": [
                    {
                        "session_id": "019d8629-0609-7f02-9290-4a98e6a307ed",
                        "task_id": "stress-peer",
                        "paths": ["hotspot/shared.txt"],
                        "path_conflicts": [
                            {
                                "path": "hotspot/shared.txt",
                                "ours_ops": ["modify"],
                                "peer_ops": ["modify"]
                            }
                        ]
                    }
                ]
            })),
        };

        let sdk_error = decode_rpc_error(err).expect("decode conflict");
        match sdk_error {
            SdkError::MergeConflict(detail) => {
                assert_eq!(detail.conflicts.len(), 1);
                assert_eq!(detail.conflicts[0].paths, vec![PathBuf::from("hotspot/shared.txt")]);
                assert_eq!(detail.conflicts[0].task_id, "stress-peer");
                let rendered = detail.to_string();
                assert!(rendered.contains("hotspot/shared.txt"));
                assert!(rendered.contains("019d8629-0609-7f02-9290-4a98e6a307ed"));
            }
            other => panic!("expected merge conflict, got {other:?}"),
        }
    }

    #[test]
    fn decode_deadline_exceeded_maps_to_explicit_variant() {
        let err = RpcError {
            code: ERR_DEADLINE_EXCEEDED,
            message: "commit deadline exceeded before flatten".to_owned(),
            data: None,
        };

        let sdk_error = decode_rpc_error(err).expect("decode deadline");
        match sdk_error {
            SdkError::DeadlineExceeded(message) => {
                assert!(message.contains("deadline exceeded"));
            }
            other => panic!("expected deadline exceeded, got {other:?}"),
        }
    }
}
