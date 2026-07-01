//! JSON-RPC 2.0 server over TCP loopback.
//!
//! Binds on `127.0.0.1:0` (random port).  Writes the assigned port number to a
//! discovery file so clients can connect.  This works identically on Linux,
//! macOS, and Windows — no platform-specific IPC primitives needed.
//!
//! Each connection is handled in its own Tokio task.
//! Each request is a newline-terminated JSON object; the response is too.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
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

    /// Start the RPC server on a random TCP port bound to loopback.
    ///
    /// Writes the assigned port to `port_file` so clients can discover it.
    /// The file is secured to current-user-only where the platform supports it.
    pub async fn serve(self, port_file: &Path) -> Result<()> {
        // Remove stale file from previous run.
        let _ = std::fs::remove_file(port_file);

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;

        // Write port number to discovery file.
        if let Some(parent) = port_file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(port_file, addr.port().to_string())?;
        crate::platform::secure_rpc_endpoint(port_file)?;

        tracing::info!(port = addr.port(), file = %port_file.display(), "RPC server listening");

        let state = Arc::new(self.state);
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    tracing::debug!(%peer, "RPC connection accepted");
                    let state = Arc::clone(&state);
                    tokio::spawn(handle_connection(stream, state));
                }
                Err(e) => error!("accept error: {e}"),
            }
        }
    }
}

async fn handle_connection(stream: TcpStream, state: Arc<AppState>) {
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
