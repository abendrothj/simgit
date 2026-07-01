//! Async client over TCP loopback to the simgitd daemon (JSON-RPC 2.0).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
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
        ERR_COMMIT_PENDING => Ok(SdkError::CommitPending(err.message)),
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

enum CommitAttemptError {
    Unsent(SdkError),
    SentTransport(SdkError),
    Response(SdkError),
}

/// Read the TCP port from the port file and return a `TcpStream` connected to
/// the daemon on loopback.
async fn connect_to_daemon(port_file: &Path) -> Result<TcpStream, SdkError> {
    let port_str = std::fs::read_to_string(port_file)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SdkError::DaemonNotFound(port_file.display().to_string())
            } else {
                SdkError::Io(e)
            }
        })?
        .trim()
        .to_owned();
    let port: u16 = port_str.parse().map_err(|_| {
        SdkError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid port in {}: {port_str}", port_file.display()),
        ))
    })?;
    TcpStream::connect(format!("127.0.0.1:{port}"))
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                SdkError::DaemonNotFound(port_file.display().to_string())
            } else {
                SdkError::Io(e)
            }
        })
}

/// Default port-file path.
///
/// Unix: `$XDG_RUNTIME_DIR/simgit/control.port` or `/tmp/simgit-<uid>/control.port`.
/// Windows: `%LOCALAPPDATA%\simgit\control.port`.
pub fn default_port_file() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime).join("simgit").join("control.port");
        }
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/simgit-{uid}/control.port"))
    }
    #[cfg(windows)]
    {
        let local_app_data = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| r"C:\Users\Default\AppData\Local".to_owned());
        PathBuf::from(local_app_data).join("simgit").join("control.port")
    }
}

/// Legacy alias for backwards compatibility.
/// Prefer [`default_port_file`] in new code.
pub fn default_socket_path() -> PathBuf {
    default_port_file()
}

/// Stateless client; each method opens a fresh TCP connection.
pub struct Client {
    port_file: PathBuf,
}

impl Client {
    pub fn new(port_file: impl AsRef<Path>) -> Self {
        Self { port_file: port_file.as_ref().to_owned() }
    }

    pub fn from_env() -> Self {
        Self::new(default_port_file())
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
            let stream = match connect_to_daemon(&self.port_file).await {
                Ok(stream) => stream,
                Err(e) => {
                    if attempt < attempts && matches!(e, SdkError::Io(ref io) if is_retryable_io(io)) {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                        last_err = Some(e);
                        continue;
                    }
                    return Err(e);
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

    pub async fn session_commit(
        &self,
        session_id:  Uuid,
        branch_name: Option<String>,
        message:     Option<String>,
    ) -> Result<SessionCommitResult, SdkError> {
        self.session_commit_with_timeout(session_id, branch_name, message, None)
            .await
    }

    pub async fn session_commit_with_timeout(
        &self,
        session_id: Uuid,
        branch_name: Option<String>,
        message: Option<String>,
        timeout_override: Option<Duration>,
    ) -> Result<SessionCommitResult, SdkError> {
        let effective_timeout = timeout_override.unwrap_or_else(|| Duration::from_secs(rpc_timeout_secs()));
        let deadline_epoch_ms = deadline_epoch_ms_from_timeout(effective_timeout);
        let request_id = Uuid::new_v4();
        let attempts = rpc_retry_count() + 1;

        for attempt in 1..=attempts {
            let params = serde_json::json!({
                "session_id":  session_id,
                "branch_name": branch_name,
                "message":     message,
                "deadline_epoch_ms": deadline_epoch_ms,
                "request_id": request_id,
            });

            match self.commit_call_once(params, effective_timeout).await {
                Ok(result) => return Ok(serde_json::from_value(result)?),
                Err(CommitAttemptError::Response(SdkError::CommitPending(message))) => {
                    match self
                        .poll_commit_status(session_id, request_id, effective_timeout)
                        .await
                    {
                        Ok(Some(Ok(result))) => return Ok(result),
                        Ok(Some(Err(error))) => return Err(error),
                        Ok(None) => {
                            if attempt == attempts {
                                return Err(SdkError::CommitPending(message));
                            }
                        }
                        Err(error) => {
                            if attempt == attempts {
                                return Err(error);
                            }
                        }
                    }
                }
                Err(CommitAttemptError::SentTransport(error)) => {
                    match self
                        .poll_commit_status(session_id, request_id, effective_timeout)
                        .await
                    {
                        Ok(Some(Ok(result))) => return Ok(result),
                        Ok(Some(Err(error_from_status))) => return Err(error_from_status),
                        Ok(None) => {
                            if attempt == attempts {
                                return Err(error);
                            }
                        }
                        Err(status_error) => {
                            if attempt == attempts {
                                return Err(status_error);
                            }
                        }
                    }
                }
                Err(CommitAttemptError::Unsent(error)) => {
                    if attempt == attempts {
                        return Err(error);
                    }
                    tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                }
                Err(CommitAttemptError::Response(error)) => return Err(error),
            }
        }

        Err(SdkError::Io(std::io::Error::other(
            "commit failed after retries",
        )))
    }

    pub async fn session_commit_status(
        &self,
        session_id: Uuid,
        request_id: Uuid,
    ) -> Result<SessionCommitStatus, SdkError> {
        let result = self
            .call(
                "commit.status",
                serde_json::json!({
                    "session_id": session_id,
                    "request_id": request_id,
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn session_abort(&self, session_id: Uuid) -> Result<(), SdkError> {
        self.call("session.abort", serde_json::json!({ "session_id": session_id }))
            .await?;
        Ok(())
    }

    pub async fn session_list(
        &self,
        status: Option<SessionStatus>,
    ) -> Result<Vec<SessionInfo>, SdkError> {
        let result = self
            .call("session.list", serde_json::json!({ "status": status }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn session_diff(&self, session_id: Uuid) -> Result<DiffResult, SdkError> {
        let result = self
            .call("session.diff", serde_json::json!({ "session_id": session_id }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    // ── lock methods ─────────────────────────────────────────────────────────

    pub async fn lock_list(
        &self,
        path: Option<&Path>,
    ) -> Result<Vec<LockInfo>, SdkError> {
        let result = self
            .call("lock.list", serde_json::json!({ "path": path }))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

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

// ── commit helpers (private) ──────────────────────────────────────────────

impl Client {
    async fn commit_call_once(
        &self,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, CommitAttemptError> {
        let stream = connect_to_daemon(&self.port_file)
            .await
            .map_err(|e| match e {
                SdkError::DaemonNotFound(msg) => {
                    CommitAttemptError::Unsent(SdkError::DaemonNotFound(msg))
                }
                SdkError::Io(io) => CommitAttemptError::Unsent(SdkError::Io(io)),
                other => CommitAttemptError::Unsent(other),
            })?;

        let req = RpcRequest {
            jsonrpc: "2.0".into(),
            id: next_id(),
            method: "session.commit".into(),
            params,
        };
        let mut payload = serde_json::to_string(&req)
            .map_err(|e| CommitAttemptError::Unsent(SdkError::Json(e)))?;
        payload.push('\n');

        let (read_half, mut write_half) = stream.into_split();
        write_half
            .write_all(payload.as_bytes())
            .await
            .map_err(|e| CommitAttemptError::Unsent(SdkError::Io(e)))?;
        write_half
            .shutdown()
            .await
            .map_err(|e| CommitAttemptError::SentTransport(SdkError::Io(e)))?;

        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        let read_result = tokio::time::timeout(timeout, reader.read_line(&mut line)).await;

        let bytes = match read_result {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(CommitAttemptError::SentTransport(SdkError::Io(e))),
            Err(_) => {
                return Err(CommitAttemptError::SentTransport(SdkError::Io(
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("RPC response timeout ({}s)", duration_to_timeout_secs(timeout)),
                    ),
                )));
            }
        };

        if bytes == 0 || line.trim().is_empty() {
            return Err(CommitAttemptError::SentTransport(SdkError::Io(
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "empty RPC response"),
            )));
        }

        let resp: RpcResponse = serde_json::from_str(line.trim())
            .map_err(|e| CommitAttemptError::SentTransport(SdkError::Json(e)))?;

        if let Some(err) = resp.error {
            return Err(CommitAttemptError::Response(
                decode_rpc_error(err).unwrap_or_else(|e| e),
            ));
        }

        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }

    async fn poll_commit_status(
        &self,
        session_id: Uuid,
        request_id: Uuid,
        poll_window: Duration,
    ) -> Result<Option<Result<SessionCommitResult, SdkError>>, SdkError> {
        let deadline = tokio::time::Instant::now() + poll_window;
        loop {
            let status = self.session_commit_status(session_id, request_id).await?;
            match status.state {
                CommitRequestState::Success => {
                    return Ok(Some(Ok(status.result.ok_or_else(|| {
                        SdkError::Io(std::io::Error::other(
                            "commit status success missing result payload",
                        ))
                    })?)));
                }
                CommitRequestState::Failed => {
                    let error = status.error.ok_or_else(|| {
                        SdkError::Io(std::io::Error::other(
                            "commit status failure missing error payload",
                        ))
                    })?;
                    return Ok(Some(Err(decode_rpc_error(error)?)));
                }
                CommitRequestState::NotFound => return Ok(None),
                CommitRequestState::Pending => {
                    if tokio::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
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
