//! JSON-RPC 2.0 server over a Unix domain socket.
//!
//! Each connection is handled in its own Tokio task.
//! Each request is a newline-terminated JSON object; the response is too.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{error, warn};

use simgit_sdk::{RpcRequest, RpcResponse, RpcError};

use crate::daemon::AppState;
use super::methods;

pub struct RpcServer {
    state: AppState,
}

impl RpcServer {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub async fn serve(self, socket_path: &Path) -> Result<()> {
        // Remove stale socket from previous run.
        let _ = std::fs::remove_file(socket_path);

        // Restrict socket to owner only.
        let listener = UnixListener::bind(socket_path)?;
        secure_socket(socket_path)?;

        tracing::info!(path = %socket_path.display(), "RPC socket listening");

        let state = Arc::new(self.state);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let state = Arc::clone(&state);
                    tokio::spawn(handle_connection(stream, state));
                }
                Err(e) => error!("accept error: {e}"),
            }
        }
    }
}

async fn handle_connection(stream: UnixStream, state: Arc<AppState>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => { warn!("read error: {e}"); break; }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        let response = match serde_json::from_str::<RpcRequest>(trimmed) {
            Err(e) => {
                error_response(0, -32700, format!("parse error: {e}"), None)
            }
            Ok(req) => {
                let id = req.id;
                let method = req.method;
                let params = req.params;

                state.metrics.set_active_counts(
                    state.sessions.list_active().len(),
                    state.borrows.list(None).len(),
                );

                let started = Instant::now();
                let response = match methods::dispatch(&state, &method, params).await {
                    Ok(result) => RpcResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result:  Some(result),
                        error:   None,
                    },
                    Err(rpc_err) => RpcResponse {
                        jsonrpc: "2.0".into(),
                        id,
                        result:  None,
                        error:   Some(rpc_err),
                    },
                };

                let success = response.error.is_none();
                let error_code = response.error.as_ref().map(|e| e.code);
                state.metrics.observe_rpc(
                    &method,
                    success,
                    error_code,
                    started.elapsed().as_secs_f64(),
                );

                state.metrics.set_active_counts(
                    state.sessions.list_active().len(),
                    state.borrows.list(None).len(),
                );

                response
            }
        };

        let payload = match serde_json::to_string(&response) {
            Ok(payload) => format!("{payload}\n"),
            Err(err) => {
                error!(?err, ?response, "failed to serialize rpc response");
                let fallback = error_response(
                    response.id,
                    -32603,
                    format!("internal error: failed to serialize response: {err}"),
                    None,
                );
                match serde_json::to_string(&fallback) {
                    Ok(payload) => format!("{payload}\n"),
                    Err(fallback_err) => {
                        error!(?fallback_err, "failed to serialize fallback rpc error response");
                        break;
                    }
                }
            }
        };
        if write_half.write_all(payload.as_bytes()).await.is_err() {
            break;
        }
    }
}

fn error_response(id: u64, code: i32, message: String, data: Option<serde_json::Value>) -> RpcResponse {
    RpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result:  None,
        error:   Some(RpcError { code, message, data }),
    }
}

/// Set Unix socket permissions to 0600 (owner r/w only).
fn secure_socket(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)?;
    let mut perms = meta.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}
