use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use herd_protocol::{Request, Response};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
use tokio::{io::{AsyncBufReadExt, AsyncWriteExt, BufReader}, net::TcpStream, time::timeout};

#[derive(Parser)]
#[command(name = "herd", version, about = "Universal Herd developer environment manager")]
struct Cli { #[command(subcommand)] command: Command }

#[derive(Subcommand)]
enum Command {
    /// Start the background daemon in the foreground.
    Daemon,
    /// Show daemon status.
    Status,
    /// Discover projects under a directory.
    Projects { #[command(subcommand)] command: ProjectCommand },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Scan a directory for common project manifests.
    Scan { #[arg(default_value = ".")] path: PathBuf, #[arg(long, default_value_t = 3)] depth: usize },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon => herd_daemon::run().await,
        Command::Status => {
            let response = call("system.status", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
        Command::Projects { command: ProjectCommand::Scan { path, depth } } => {
            let absolute = std::fs::canonicalize(&path).with_context(|| format!("cannot access {}", path.display()))?;
            let response = call("projects.scan", json!({"path": absolute, "max_depth": depth})).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
    }
}

async fn call(method: &str, params: Value) -> Result<Value> {
    let token = herd_daemon::read_token().context("daemon token not found; start the daemon with `herd daemon`")?;
    let mut stream = timeout(Duration::from_secs(3), TcpStream::connect(("127.0.0.1", herd_daemon::DEFAULT_PORT)))
        .await.context("timed out connecting to Herd daemon")??;
    let request = Request { jsonrpc: "2.0".into(), id: json!(1), method: method.into(), params, token: Some(token) };
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line)?;
    if let Some(error) = response.error { anyhow::bail!("daemon error {}: {}", error.code, error.message); }
    Ok(response.result.unwrap_or(Value::Null))
}
