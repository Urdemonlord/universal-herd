//! Tauri commands: thin wrappers over the daemon JSON-RPC client.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::client;

fn err_string(error: anyhow::Error) -> String {
    format!("{error:#}")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Canonicalize a user supplied directory for a clear error when it
/// does not exist or is not a directory.
fn canonical_dir(path: &str) -> Result<PathBuf, String> {
    let raw = if path.trim().is_empty() {
        home_dir().ok_or_else(|| "no directory given and HOME is not set".to_string())?
    } else {
        PathBuf::from(path.trim())
    };
    std::fs::canonicalize(&raw)
        .map_err(|e| format!("cannot access {}: {e}", raw.display()))
        .and_then(|absolute| {
            if absolute.is_dir() {
                Ok(absolute)
            } else {
                Err(format!("{} is not a directory", absolute.display()))
            }
        })
}

#[tauri::command]
pub async fn daemon_status() -> Result<Value, String> {
    client::call("system.status", json!({})).await.map_err(err_string)
}

#[tauri::command]
pub async fn scan_projects(path: String) -> Result<Value, String> {
    let absolute = canonical_dir(&path)?;
    client::call("projects.scan", json!({"path": absolute, "max_depth": 3}))
        .await
        .map_err(err_string)
}

#[tauri::command]
pub async fn list_projects() -> Result<Value, String> {
    client::call("projects.list", json!({})).await.map_err(err_string)
}

#[tauri::command]
pub async fn list_runtimes() -> Result<Value, String> {
    client::call("runtimes.list", json!({})).await.map_err(err_string)
}

#[tauri::command]
pub async fn list_services() -> Result<Value, String> {
    client::call("services.list", json!({})).await.map_err(err_string)
}

#[tauri::command]
pub async fn list_domains() -> Result<Value, String> {
    client::call("domains.list", json!({})).await.map_err(err_string)
}

#[tauri::command]
pub async fn park_project(path: String) -> Result<Value, String> {
    let absolute = canonical_dir(&path)?;
    client::call("projects.park", json!({"path": absolute})).await.map_err(err_string)
}

#[tauri::command]
pub async fn link_project(path: String, domain: String) -> Result<Value, String> {
    let absolute = canonical_dir(&path)?;
    client::call("projects.link", json!({"path": absolute, "domain": domain}))
        .await
        .map_err(err_string)
}

#[tauri::command]
pub async fn start_service(name: String) -> Result<Value, String> {
    client::call("services.start", json!({"name": name})).await.map_err(err_string)
}

#[tauri::command]
pub async fn stop_service(name: String) -> Result<Value, String> {
    client::call("services.stop", json!({"name": name})).await.map_err(err_string)
}

/// Where the daemon binary lives.
///
/// There is no standalone `herd-daemon` binary in this repo: the
/// daemon runs as `herd daemon` (CLI binary). So look for `herd-daemon`
/// first (a future standalone binary), then `herd` on PATH, then the
/// CLI build output of this workspace.
fn find_daemon_binary() -> Option<(PathBuf, Option<&'static str>)> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for (name, arg) in [("herd-daemon", None), ("herd", Some("daemon"))] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some((candidate, arg));
            }
        }
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.join("../../..");
    for (name, arg) in [("herd-daemon", None), ("herd", Some("daemon"))] {
        let candidate = workspace_root.join("target").join("debug").join(name);
        if candidate.is_file() {
            return Some((candidate, arg));
        }
    }
    None
}

#[tauri::command]
pub async fn start_daemon() -> Result<Value, String> {
    if let Ok(status) = client::call("system.status", json!({})).await {
        return Ok(json!({"started": false, "already_running": true, "status": status}));
    }
    let (binary, arg) = find_daemon_binary().ok_or_else(|| {
        "Herd daemon binary not found. Start the daemon manually with `herd daemon` \
         in a terminal, then press Start daemon again."
            .to_string()
    })?;
    let mut command = tokio::process::Command::new(&binary);
    if let Some(arg) = arg {
        command.arg(arg);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command
        .spawn()
        .with_context(|| format!("failed to start {}", binary.display()))
        .map_err(err_string)?;
    let pid = child.id().expect("spawned daemon process has no pid");
    drop(child);
    for _ in 0..25 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Ok(status) = client::call("system.status", json!({})).await {
            return Ok(json!({"started": true, "pid": pid, "status": status}));
        }
    }
    Err(format!(
        "daemon process started (pid {pid}) but did not answer on 127.0.0.1:47831 \
         within 5 seconds. Check whether it exited, or start it manually with \
         `herd daemon`."
    ))
}
