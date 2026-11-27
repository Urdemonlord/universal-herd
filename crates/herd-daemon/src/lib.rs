use anyhow::{Context, Result};
use directories::ProjectDirs;
use herd_core::scan_projects;
use herd_protocol::{Request, Response};
use serde_json::json;
use std::{fs, io, path::PathBuf};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::{TcpListener, TcpStream}};
use tracing::{info, warn};

pub const DEFAULT_PORT: u16 = 47831;

pub fn config_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("id", "Meow Labs", "Universal Herd").context("cannot resolve OS config directory")?;
    let path = dirs.config_dir().to_path_buf();
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn load_or_create_token() -> Result<String> {
    let path = config_dir()?.join("daemon.token");
    if path.exists() { return Ok(fs::read_to_string(path)?.trim().to_string()); }
    let token = uuid::Uuid::new_v4().to_string();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    use std::io::Write;
    file.write_all(token.as_bytes())?;
    Ok(token)
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("herd_daemon=info".parse()?)).init();
    let token = load_or_create_token()?;
    let listener = TcpListener::bind(("127.0.0.1", DEFAULT_PORT)).await
        .with_context(|| format!("cannot bind localhost:{DEFAULT_PORT}; is another Herd daemon running?"))?;
    info!(port = DEFAULT_PORT, "Universal Herd daemon started");
    loop {
        let (stream, _) = listener.accept().await?;
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, token).await { warn!(%error, "client connection ended with error"); }
        });
    }
}

async fn handle_client(stream: TcpStream, token: String) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    while reader.read_line(&mut line).await? > 0 {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) if request.token.as_deref() == Some(token.as_str()) => dispatch(request).await,
            Ok(request) => Response::error(request.id, -32001, "unauthorized"),
            Err(error) => Response::error(serde_json::Value::Null, -32700, format!("parse error: {error}")),
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
        line.clear();
    }
    Ok(())
}

async fn dispatch(request: Request) -> Response {
    if request.jsonrpc != "2.0" { return Response::error(request.id, -32600, "invalid JSON-RPC version"); }
    match request.method.as_str() {
        "system.status" => Response::success(request.id, json!({
            "name": "Universal Herd", "version": env!("CARGO_PKG_VERSION"),
            "protocol": "1.0", "status": "running", "platform": std::env::consts::OS,
        })),
        "projects.scan" => {
            let path = request.params.get("path").and_then(|v| v.as_str()).map(PathBuf::from);
            let Some(path) = path else { return Response::error(request.id, -32602, "params.path is required"); };
            let depth = request.params.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            match scan_projects(&path, depth) {
                Ok(projects) => Response::success(request.id, json!({"projects": projects})),
                Err(error) => Response::error(request.id, -32010, format!("project scan failed: {error}")),
            }
        }
        _ => Response::error(request.id, -32601, "method not found"),
    }
}

pub fn token_path() -> Result<PathBuf> { Ok(config_dir()?.join("daemon.token")) }

pub fn read_token() -> io::Result<String> { Ok(fs::read_to_string(token_path().map_err(io::Error::other)?)?.trim().to_string()) }
