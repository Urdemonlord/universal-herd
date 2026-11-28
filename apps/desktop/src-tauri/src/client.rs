//! JSON-RPC client for the Herd daemon.
//!
//! Mirrors the CLI client in `apps/cli/src/main.rs`: one NDJSON request
//! over TCP 127.0.0.1:47831, authenticated with the token from
//! `daemon.token`, 3 second connect timeout. Errors are written for a
//! human reader: a missing token means the daemon has never run on
//! this machine, a refused connection means it is not running now.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use herd_protocol::{Request, Response};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// How long to wait for the daemon TCP connect before giving up.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Call a daemon method with the token read from `daemon.token`.
pub async fn call(method: &str, params: Value) -> Result<Value> {
    let token = herd_daemon::read_token()
        .context("daemon token not found, the daemon has never run on this machine")?;
    call_with_token(method, params, &token).await
}

/// Call a daemon method with an explicit token (used by tests).
pub async fn call_with_token(method: &str, params: Value, token: &str) -> Result<Value> {
    call_at(method, params, token, ("127.0.0.1", herd_daemon::DEFAULT_PORT)).await
}

/// Call a daemon method at an explicit address (used by tests with a
/// mock TCP server).
pub async fn call_at(method: &str, params: Value, token: &str, addr: (&str, u16)) -> Result<Value> {
    let socket: SocketAddr =
        format!("{}:{}", addr.0, addr.1).parse().context("invalid daemon address")?;
    let connect = TcpStream::connect(socket);
    let mut stream = timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| anyhow::anyhow!("timed out connecting to the Herd daemon after 3 seconds"))?
        .context("cannot connect to the Herd daemon on 127.0.0.1:47831, is it running?")?;
    let request = Request {
        jsonrpc: "2.0".into(),
        id: json!(1),
        method: method.into(),
        params,
        token: Some(token.to_string()),
    };
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line)?;
    if let Some(error) = response.error {
        anyhow::bail!("daemon error {}: {}", error.code, error.message);
    }
    Ok(response.result.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Spawn a mock NDJSON daemon on a random loopback port. The handler
    /// receives the decoded request and returns the result value, or an
    /// (code, message) error. Returns the port to call.
    async fn mock_daemon(
        handler: impl Fn(&Request) -> Result<Value, (i32, String)> + Send + Sync + 'static,
    ) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = std::sync::Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stream).lines();
                    if let Ok(Some(line)) = lines.next_line().await {
                        if let Ok(request) = serde_json::from_str::<Request>(&line) {
                            let response = match handler(&request) {
                                Ok(result) => Response::success(request.id, result),
                                Err((code, message)) => Response::error(request.id, code, message),
                            };
                            let mut bytes = serde_json::to_vec(&response).unwrap();
                            bytes.push(b'\n');
                            let _ = lines.into_inner().write_all(&bytes).await;
                        }
                    }
                });
            }
        });
        (port, task)
    }

    #[tokio::test]
    async fn success_response_returns_result() {
        let (port, _task) = mock_daemon(|request| {
            assert_eq!(request.method, "system.status");
            assert_eq!(request.token.as_deref(), Some("test-token"));
            Ok(json!({"status": "running", "protocol": "1.3"}))
        })
        .await;
        let result =
            call_at("system.status", json!({}), "test-token", ("127.0.0.1", port)).await.unwrap();
        assert_eq!(result["status"], "running");
        assert_eq!(result["protocol"], "1.3");
    }

    #[tokio::test]
    async fn daemon_error_becomes_readable_error() {
        let (port, _task) = mock_daemon(|_| Err((-32602, "params.path is required".into()))).await;
        let error = call_at("projects.scan", json!({}), "test-token", ("127.0.0.1", port))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("-32602"), "{error}");
        assert!(error.to_string().contains("params.path is required"), "{error}");
    }

    #[tokio::test]
    async fn connection_refused_says_daemon_is_not_running() {
        // Port 1 on loopback refuses immediately; the message must point
        // the user at the daemon, not at a socket detail.
        let error =
            call_at("system.status", json!({}), "test-token", ("127.0.0.1", 1)).await.unwrap_err();
        assert!(error.to_string().contains("is it running?"), "{error}");
    }
}
