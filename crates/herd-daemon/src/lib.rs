use anyhow::{Context, Result};
use certs::{CA_ISSUER, certs_dir, create_leaf, leaf_exists, remove_leaf};
use directories::ProjectDirs;
use herd_core::{ProjectCandidate, detect_project, scan_projects};
use herd_protocol::{PROTOCOL_VERSION, Request, Response};
use proxy::ProxyHandle;
use runtimes::{
    InstallAction, detect_runtimes, find_program, install_plan, known_runtime, known_runtime_names,
    path_dirs, query_toolchain_version, report_installed_version,
};
use serde_json::{Value, json};
use state::{
    DomainCert, RuntimePin, ServiceDef, StateError, StateStore, auto_domain, catalog_default,
    normalize_domain, validate_service_name,
};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use supervisor::Supervisor;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock},
};
use tracing::{info, warn};

pub mod certs;
pub mod proxy;
pub mod runtimes;
pub mod state;
pub mod supervisor;

pub const DEFAULT_PORT: u16 = 47831;

/// Config directory. `HERD_CONFIG_DIR` overrides the OS directory; it is
/// a test/dev escape hatch (the integration test points it at a temp dir)
/// and is not part of the normal user flow.
pub fn config_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("HERD_CONFIG_DIR") {
        let path = PathBuf::from(dir);
        fs::create_dir_all(&path)?;
        return Ok(path);
    }
    let dirs = ProjectDirs::from("id", "Meow Labs", "Universal Herd")
        .context("cannot resolve OS config directory")?;
    let path = dirs.config_dir().to_path_buf();
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn load_or_create_token() -> Result<String> {
    let path = config_dir()?.join("daemon.token");
    if path.exists() {
        return Ok(fs::read_to_string(path)?.trim().to_string());
    }
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

/// Shared daemon state: the persistent JSON store plus the in-memory
/// service supervisor. One instance is shared by all client connections.
pub struct DaemonState {
    store: RwLock<StateStore>,
    supervisor: Mutex<Supervisor>,
    /// The config directory (state.json, daemon.token, certs/, logs/).
    config_dir: PathBuf,
    /// Serializes certificate creation (G3): `create_leaf` and
    /// `ensure_ca` share fixed temp paths, so two concurrent
    /// `domains.secure` calls could overwrite each other's key/CSR.
    /// Held across the whole blocking certificate call, and by
    /// `domains.unsecure` while it deletes the leaf files.
    certs: Mutex<()>,
    /// The running HTTP proxy (PR-1), if `proxy.start` was called.
    /// Only the proxy handlers touch this slot; the HTTP request
    /// handler never locks it, so it cannot deadlock against the
    /// store/supervisor locks.
    proxy: Mutex<Option<ProxyHandle>>,
}

impl DaemonState {
    pub fn load(dir: &std::path::Path) -> Result<Arc<Self>, StateError> {
        let store = StateStore::load(dir)?;
        let supervisor = Supervisor::new(dir.join("logs")).map_err(StateError::Io)?;
        Ok(Arc::new(Self {
            store: RwLock::new(store),
            supervisor: Mutex::new(supervisor),
            config_dir: dir.to_path_buf(),
            certs: Mutex::new(()),
            proxy: Mutex::new(None),
        }))
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("herd_daemon=info".parse()?),
        )
        .init();
    let token = load_or_create_token()?;
    let dir = config_dir()?;
    let state = DaemonState::load(&dir)
        .map_err(|error| anyhow::anyhow!("cannot load daemon state: {error}"))?;
    let listener = TcpListener::bind(("127.0.0.1", DEFAULT_PORT)).await.with_context(|| {
        format!("cannot bind localhost:{DEFAULT_PORT}; is another Herd daemon running?")
    })?;
    info!(port = DEFAULT_PORT, "Universal Herd daemon started");
    serve(listener, state, token).await
}

/// Accept loop, split out of `run` so tests can serve on a port 0
/// listener with a temp config dir.
pub async fn serve(listener: TcpListener, state: Arc<DaemonState>, token: String) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let token = token.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(error) = handle_client(stream, token, state).await {
                warn!(%error, "client connection ended with error");
            }
        });
    }
}

async fn handle_client(stream: TcpStream, token: String, state: Arc<DaemonState>) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    while reader.read_line(&mut line).await? > 0 {
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) if request.token.as_deref() == Some(token.as_str()) => {
                dispatch(request, Arc::clone(&state)).await
            }
            Ok(request) => Response::error(request.id, -32001, "unauthorized"),
            Err(error) => {
                Response::error(serde_json::Value::Null, -32700, format!("parse error: {error}"))
            }
        };
        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write.write_all(&encoded).await?;
        line.clear();
    }
    Ok(())
}

fn invalid_params(id: Value, message: impl Into<String>) -> Response {
    Response::error(id, -32602, message)
}

fn internal_error(id: Value, message: impl Into<String>) -> Response {
    Response::error(id, -32010, message)
}

/// Map a state error to its JSON-RPC code: bad input is -32602, IO and
/// anything else is -32010.
fn state_error(id: Value, error: StateError) -> Response {
    match error {
        StateError::Invalid(message) => invalid_params(id, message),
        StateError::Io(error) => internal_error(id, format!("state error: {error}")),
    }
}

fn required_str<'a>(request: &'a Request, field: &str) -> Result<&'a str, Response> {
    request
        .params
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| invalid_params(request.id.clone(), format!("params.{field} is required")))
}

fn canonical_dir(request: &Request, field: &str) -> Result<PathBuf, Response> {
    let raw = required_str(request, field)?;
    let path = fs::canonicalize(raw).map_err(|_| {
        invalid_params(request.id.clone(), format!("params.{field} does not exist: {raw}"))
    })?;
    if !path.is_dir() {
        return Err(invalid_params(
            request.id.clone(),
            format!("params.{field} is not a directory: {}", path.display()),
        ));
    }
    Ok(path)
}

/// Lexically absolutize and normalize a path without touching the
/// filesystem: `.` segments are dropped, `..` pops the previous
/// segment. Used for comparing stored paths whose spelling may
/// differ (`/srv/web/.` vs `/srv/web`); see `stored_dir` and the
/// proxy route computation.
pub fn lexical_normalize(raw: &Path) -> PathBuf {
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(raw)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path naming a stored registration (unpark/unlink):
/// canonicalize when the directory still exists, otherwise
/// canonicalize the deepest existing ancestor and append the deleted
/// tail lexically, so symlinks in the surviving prefix still
/// resolve (e.g. on macOS /var is a symlink to /private/var, and a
/// directory deleted under it must still match the canonical path
/// stored when it was linked). Only when the symlink itself is in
/// the deleted tail can the path no longer be resolved, and that
/// case still reports "not parked/linked".
pub fn stored_dir(raw: &Path) -> PathBuf {
    if let Ok(canonical) = fs::canonicalize(raw) {
        return canonical;
    }
    let absolute = lexical_normalize(raw);
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut prefix = absolute.as_path();
    loop {
        match fs::canonicalize(prefix) {
            Ok(canonical) => {
                let mut out = canonical;
                for component in suffix.iter().rev() {
                    out.push(component);
                }
                return out;
            }
            Err(_) => match prefix.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => {
                    if let Some(name) = prefix.file_name() {
                        suffix.push(name.to_os_string());
                    }
                    prefix = parent;
                }
                _ => return absolute,
            },
        }
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[allow(clippy::too_many_lines)]
async fn dispatch(request: Request, state: Arc<DaemonState>) -> Response {
    if request.jsonrpc != "2.0" {
        return Response::error(request.id, -32600, "invalid JSON-RPC version");
    }
    match request.method.as_str() {
        "system.status" => Response::success(
            request.id,
            json!({
                "name": "Universal Herd", "version": env!("CARGO_PKG_VERSION"),
                "protocol": PROTOCOL_VERSION, "status": "running", "platform": std::env::consts::OS,
            }),
        ),
        "projects.scan" => {
            let path = request.params.get("path").and_then(|v| v.as_str()).map(PathBuf::from);
            let Some(path) = path else {
                return invalid_params(request.id, "params.path is required");
            };
            let depth =
                request.params.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            match scan_projects(&path, depth) {
                Ok(projects) => Response::success(request.id, json!({"projects": projects})),
                Err(error) => internal_error(request.id, format!("project scan failed: {error}")),
            }
        }
        "projects.park" => {
            let path = match canonical_dir(&request, "path") {
                Ok(path) => path,
                Err(response) => return response,
            };
            let path = path_string(&path);
            let mut store = state.store.write().await;
            match store.apply(|s| s.park(&path)) {
                Ok(()) => Response::success(request.id, json!({"path": path, "parked": true})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.unpark" => {
            let path = match required_str(&request, "path") {
                Ok(raw) => path_string(&stored_dir(Path::new(raw))),
                Err(response) => return response,
            };
            let mut store = state.store.write().await;
            match store.apply(|s| s.unpark(&path)) {
                Ok(()) => Response::success(request.id, json!({"path": path, "parked": false})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.link" => {
            let path = match canonical_dir(&request, "path") {
                Ok(path) => path,
                Err(response) => return response,
            };
            let domain = match required_str(&request, "domain").and_then(|raw| {
                normalize_domain(raw).map_err(|error| state_error(request.id.clone(), error))
            }) {
                Ok(domain) => domain,
                Err(response) => return response,
            };
            let path = path_string(&path);
            let mut store = state.store.write().await;
            match store.apply(|s| s.link(&path, domain.clone())) {
                Ok(()) => Response::success(request.id, json!({"path": path, "domain": domain})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.unlink" => {
            let path = match required_str(&request, "path") {
                Ok(raw) => path_string(&stored_dir(Path::new(raw))),
                Err(response) => return response,
            };
            let mut store = state.store.write().await;
            match store.apply(|s| s.unlink(&path)) {
                Ok(()) => Response::success(request.id, json!({"path": path, "linked": false})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.list" => {
            let depth =
                request.params.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            let store = state.store.read().await;
            let found = known_projects(&store, depth);
            let projects: Vec<Value> = found
                .iter()
                .map(|(p, domain, source)| {
                    let runtime = store
                        .runtime_pin(&path_string(&p.path))
                        .map(|pin| json!({"runtime": pin.runtime, "version": pin.version}));
                    json!({
                        "name": p.name, "path": p.path, "kind": p.kind, "manifest": p.manifest,
                        "domain": domain, "source": source, "runtime": runtime,
                    })
                })
                .collect();
            Response::success(request.id, json!({"projects": projects}))
        }
        "runtimes.list" => {
            let runtimes = tokio::task::spawn_blocking(detect_runtimes).await.unwrap_or_default();
            Response::success(request.id, json!({"runtimes": runtimes}))
        }
        "runtimes.install" => {
            let runtime = match required_str(&request, "runtime") {
                Ok(runtime) => runtime.to_string(),
                Err(response) => return response,
            };
            let version =
                request.params.get("version").and_then(|v| v.as_str()).map(str::to_string);
            let rustup_present = find_program("rustup", &path_dirs()).is_some();
            let action = match install_plan(&runtime, version.as_deref(), rustup_present) {
                Ok(action) => action,
                Err(message) => return internal_error(request.id, message),
            };
            let InstallAction::ViaRustup { toolchain } = action;
            let toolchain_for_query = toolchain.clone();
            let installed = tokio::task::spawn_blocking(move || {
                let output = std::process::Command::new("rustup")
                    .args(["toolchain", "install", &toolchain])
                    .output();
                match output {
                    Ok(output) if output.status.success() => Ok(()),
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        Err(stderr.lines().last().unwrap_or("rustup failed").trim().to_string())
                    }
                    Err(error) => Err(format!("cannot run rustup: {error}")),
                }
            })
            .await;
            match installed {
                Ok(Ok(())) => {}
                Ok(Err(message)) => return internal_error(request.id, message),
                Err(error) => {
                    return internal_error(request.id, format!("install task failed: {error}"));
                }
            }
            // Report the version that was actually installed, not the
            // daemon's active compiler: `RUSTUP_TOOLCHAIN` can make
            // re-detection report a different toolchain than the one
            // just installed.
            let version = match version {
                Some(requested) => {
                    report_installed_version(Some(requested), &toolchain_for_query, None)
                }
                None => {
                    let toolchain = toolchain_for_query.clone();
                    let queried = tokio::task::spawn_blocking(move || {
                        let rustup = find_program("rustup", &path_dirs())?;
                        query_toolchain_version(&rustup, &toolchain)
                    })
                    .await
                    .unwrap_or_default();
                    report_installed_version(None, &toolchain_for_query, queried)
                }
            };
            Response::success(
                request.id,
                json!({"runtime": runtime, "version": version, "installed": true}),
            )
        }
        "projects.set_runtime" => {
            let path = match canonical_dir(&request, "path") {
                Ok(path) => path,
                Err(response) => return response,
            };
            let runtime = match required_str(&request, "runtime") {
                Ok(runtime) => runtime.to_string(),
                Err(response) => return response,
            };
            if known_runtime(&runtime).is_none() {
                return invalid_params(
                    request.id,
                    format!(
                        "unknown runtime {runtime:?}; known runtimes: {}",
                        known_runtime_names().join(", ")
                    ),
                );
            }
            let version =
                request.params.get("version").and_then(|v| v.as_str()).map(str::to_string);
            {
                let store = state.store.read().await;
                let depth = 3;
                if !known_projects(&store, depth).iter().any(|(p, _, _)| p.path == path) {
                    return invalid_params(
                        request.id,
                        format!(
                            "path {} is not a known project; park or link it first",
                            path.display()
                        ),
                    );
                }
            }
            if let Some(wanted) = &version {
                let detected =
                    tokio::task::spawn_blocking(detect_runtimes).await.unwrap_or_default();
                match detected.iter().find(|r| r.name == runtime) {
                    None => {
                        return invalid_params(
                            request.id,
                            format!(
                                "runtime {runtime:?} is not installed on this machine; install it first"
                            ),
                        );
                    }
                    Some(found) if found.version != *wanted => {
                        return invalid_params(
                            request.id,
                            format!(
                                "version {wanted:?} of {runtime} is not installed; installed version is {}",
                                found.version
                            ),
                        );
                    }
                    Some(_) => {}
                }
            }
            let pin = RuntimePin {
                path: path_string(&path),
                runtime: runtime.clone(),
                version: version.clone(),
            };
            let mut store = state.store.write().await;
            match store.apply(|s| s.set_runtime_pin(pin)) {
                Ok(()) => Response::success(
                    request.id,
                    json!({"path": path, "runtime": runtime, "version": version}),
                ),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.clear_runtime" => {
            // Clearing must also work after the directory was deleted,
            // so resolve the stored path without requiring it to exist.
            let path = match required_str(&request, "path") {
                Ok(raw) => path_string(&stored_dir(Path::new(raw))),
                Err(response) => return response,
            };
            let mut store = state.store.write().await;
            match store.apply(|s| s.clear_runtime_pin(&path)) {
                Ok(()) => Response::success(request.id, json!({"path": path, "pinned": false})),
                Err(error) => state_error(request.id, error),
            }
        }
        "domains.secure" => {
            let domain = match required_str(&request, "domain").and_then(|raw| {
                normalize_domain(raw).map_err(|error| state_error(request.id.clone(), error))
            }) {
                Ok(domain) => domain,
                Err(response) => return response,
            };
            {
                let store = state.store.read().await;
                if !known_domains(&store, 3).iter().any(|d| d == &domain) {
                    return invalid_params(
                        request.id,
                        format!(
                            "domain {domain:?} is not linked to any project; link a project first with projects.link"
                        ),
                    );
                }
            }
            let openssl = match find_program("openssl", &path_dirs()) {
                Some(path) => path,
                None => {
                    return internal_error(
                        request.id,
                        "openssl is not installed; install openssl to create local certificates",
                    );
                }
            };
            let certs = certs_dir(state.config_dir());
            let domain_for_task = domain.clone();
            // Serialize certificate creation per config directory:
            // create_leaf (and ensure_ca) share fixed temp paths, so
            // two concurrent secures could overwrite each other's
            // key/CSR. The guard is held across the blocking call.
            let _cert_guard = state.certs.lock().await;
            let created = tokio::task::spawn_blocking(move || {
                create_leaf(&certs, &domain_for_task, &openssl)
            })
            .await;
            let expires_at = match created {
                Ok(Ok(expires_at)) => expires_at,
                Ok(Err(message)) => return internal_error(request.id, message),
                Err(error) => {
                    return internal_error(request.id, format!("certificate task failed: {error}"));
                }
            };
            let record = DomainCert {
                domain: domain.clone(),
                cert_file: format!("{domain}.crt"),
                key_file: format!("{domain}.key"),
                created_at: unix_secs(SystemTime::now()),
                expires_at,
                issuer: CA_ISSUER.to_string(),
            };
            let mut store = state.store.write().await;
            match store.apply(|s| {
                s.upsert_domain(record);
                Ok(())
            }) {
                Ok(()) => Response::success(
                    request.id,
                    json!({"domain": domain, "secured": true, "expires_at": expires_at, "issuer": CA_ISSUER}),
                ),
                Err(error) => {
                    // The certificate files were just created but the
                    // record did not persist: remove them so a retry
                    // starts clean instead of seeing a stale pair.
                    let certs = certs_dir(state.config_dir());
                    let _ = remove_leaf(&certs, &domain);
                    state_error(request.id, error)
                }
            }
        }
        "domains.unsecure" => {
            let domain = match required_str(&request, "domain").and_then(|raw| {
                normalize_domain(raw).map_err(|error| state_error(request.id.clone(), error))
            }) {
                Ok(domain) => domain,
                Err(response) => return response,
            };
            let certs = certs_dir(state.config_dir());
            // Delete the files first and propagate failures: removing
            // the record first would report success while the files
            // remain, with no retry path left.
            let _cert_guard = state.certs.lock().await;
            if let Err(error) = remove_leaf(&certs, &domain) {
                return internal_error(
                    request.id,
                    format!("cannot delete certificate files: {error}"),
                );
            }
            let mut store = state.store.write().await;
            match store.apply(|s| s.remove_domain(&domain)) {
                Ok(()) => {
                    Response::success(request.id, json!({"domain": domain, "secured": false}))
                }
                Err(error) => state_error(request.id, error),
            }
        }
        "domains.list" => {
            let store = state.store.read().await;
            let certs = certs_dir(state.config_dir());
            let known = known_projects(&store, 3);
            let mut domains: Vec<(String, String)> = Vec::new();
            for (project, domain, _) in &known {
                if let Some(domain) = domain {
                    let entry = (domain.clone(), path_string(&project.path));
                    if !domains.contains(&entry) {
                        domains.push(entry);
                    }
                }
            }
            domains.sort();
            let out: Vec<Value> = domains
                .iter()
                .map(|(domain, project)| {
                    let record = store.domain_cert(domain);
                    let secured = record.is_some_and(|_| leaf_exists(&certs, domain));
                    json!({
                        "domain": domain, "project": project,
                        "secured": secured,
                        "expires_at": if secured { record.map(|r| r.expires_at) } else { None },
                        "issuer": if secured { record.map(|r| r.issuer.clone()) } else { None },
                    })
                })
                .collect();
            Response::success(request.id, json!({"domains": out}))
        }
        "services.list" => {
            let defs = {
                let store = state.store.read().await;
                store.state.services.clone()
            };
            let mut supervisor = state.supervisor.lock().await;
            let mut out = Vec::new();
            for def in &defs {
                let status = supervisor.status(&def.name);
                let healthy = supervisor.healthy(def).await;
                out.push(json!({
                    "name": def.name, "command": def.command, "args": def.args,
                    "port": def.port, "workdir": def.workdir, "kind": def.kind,
                    "status": status,
                    "pid": supervisor.pid(&def.name),
                    "started_at": supervisor.started_at(&def.name).map(unix_secs),
                    "healthy": healthy,
                    "last_exit_code": supervisor.last_exit_code(&def.name),
                }));
            }
            Response::success(request.id, json!({"services": out}))
        }
        "services.create" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            if let Err(error) = validate_service_name(&name) {
                return state_error(request.id, error);
            }
            let command =
                request.params.get("command").and_then(|v| v.as_str()).map(str::to_string);
            let catalog = catalog_default(&name);
            let (command, kind) = match (command, catalog) {
                (Some(command), _) => (command, None),
                (None, Some((binary, _))) => (binary.to_string(), Some(name.clone())),
                (None, None) => {
                    return invalid_params(
                        request.id,
                        "params.command is required (or use a known service name: postgres, mysql, redis, mailpit)",
                    );
                }
            };
            let args: Vec<String> = request
                .params
                .get("args")
                .and_then(|v| v.as_array())
                .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let port = match request.params.get("port") {
                Some(Value::Null) | None => catalog.map(|(_, port)| port),
                Some(value) => match value.as_u64() {
                    Some(port) if (1..=65535).contains(&port) => Some(port as u16),
                    _ => return invalid_params(request.id, "params.port must be 1-65535"),
                },
            };
            let workdir = match request.params.get("workdir").and_then(|v| v.as_str()) {
                Some(dir) => {
                    let path = PathBuf::from(dir);
                    if !path.is_dir() {
                        return invalid_params(
                            request.id,
                            format!("params.workdir is not a directory: {dir}"),
                        );
                    }
                    // Canonicalize at the boundary, like `projects.link`:
                    // a spelling such as `/srv/web/.` must match the
                    // linked project `/srv/web` in route computation.
                    Some(path_string(&stored_dir(&path)))
                }
                None => None,
            };
            let def = ServiceDef { name, command, args, port, workdir, kind };
            let mut store = state.store.write().await;
            match store.apply(|s| s.add_service(def.clone())) {
                Ok(()) => Response::success(request.id, serde_json::to_value(def).unwrap()),
                Err(error) => state_error(request.id, error),
            }
        }
        "services.start" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            // Hold the store read guard across the supervisor lock and
            // the spawn: `services.remove` needs the write lock to
            // delete a definition, so it cannot interleave between this
            // lookup and the spawn. Lock order everywhere is
            // store-then-supervisor; never the reverse.
            let store = state.store.read().await;
            let def = match store.service(&name) {
                Some(def) => def.clone(),
                None => {
                    return invalid_params(request.id, format!("service {name:?} does not exist"));
                }
            };
            let mut supervisor = state.supervisor.lock().await;
            match supervisor.start(&def).await {
                Ok(pid) => Response::success(
                    request.id,
                    json!({"name": name, "pid": pid, "status": "running"}),
                ),
                Err(message) if message.contains("already running") => {
                    invalid_params(request.id, message)
                }
                Err(message) => internal_error(request.id, message),
            }
        }
        "services.stop" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            let mut supervisor = state.supervisor.lock().await;
            match supervisor.stop(&name).await {
                Ok(code) => Response::success(
                    request.id,
                    json!({"name": name, "status": "stopped", "exit_code": code}),
                ),
                Err(message) => invalid_params(request.id, message),
            }
        }
        "services.remove" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            // Same lock order as `services.start` (store, then
            // supervisor): the running check and the definition removal
            // are atomic with respect to a concurrent start, so a
            // removed service can never keep running without a
            // definition.
            let log_path = {
                let mut store = state.store.write().await;
                let mut supervisor = state.supervisor.lock().await;
                if supervisor.is_running(&name) {
                    return invalid_params(
                        request.id,
                        format!("service {name:?} is running; stop it first"),
                    );
                }
                if let Err(error) = store.apply(|s| s.remove_service(&name)) {
                    return state_error(request.id, error);
                }
                supervisor.log_path(&name)
            };
            let _ = fs::remove_file(log_path);
            Response::success(request.id, json!({"name": name, "removed": true}))
        }
        "services.logs" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            let store = state.store.read().await;
            if store.service(&name).is_none() {
                return invalid_params(request.id, format!("service {name:?} does not exist"));
            }
            let lines =
                request.params.get("lines").and_then(|v| v.as_u64()).unwrap_or(100) as usize;
            let lines = lines.clamp(1, 1000);
            let supervisor = state.supervisor.lock().await;
            match supervisor.logs(&name, lines) {
                Ok(lines) => Response::success(request.id, json!({"name": name, "lines": lines})),
                Err(error) => internal_error(request.id, format!("cannot read logs: {error}")),
            }
        }
        "proxy.routes" => {
            let routes = proxy::current_routes(&state).await;
            Response::success(request.id, json!({"routes": routes}))
        }
        "proxy.start" => {
            let port = match request.params.get("port") {
                Some(Value::Null) | None => proxy::DEFAULT_PROXY_PORT,
                Some(value) => match value.as_u64() {
                    Some(port) if (1..=65535).contains(&port) => port as u16,
                    _ => return invalid_params(request.id, "params.port must be 1-65535"),
                },
            };
            let mut slot = state.proxy.lock().await;
            if let Some(handle) = slot.as_ref() {
                if handle.is_terminated() {
                    // The listener gave up after repeated accept
                    // errors: reap the dead handle so a fresh proxy
                    // can start instead of refusing forever.
                    let old = slot.take().unwrap();
                    old.stop().await;
                } else {
                    return internal_error(
                        request.id,
                        "proxy is already running; stop it first with proxy.stop",
                    );
                }
            }
            match proxy::start_proxy(Arc::clone(&state), port).await {
                Ok(handle) => {
                    let started_at = unix_secs(handle.started_at);
                    *slot = Some(handle);
                    Response::success(
                        request.id,
                        json!({"running": true, "port": port, "started_at": started_at}),
                    )
                }
                Err(message) => internal_error(request.id, message),
            }
        }
        "proxy.stop" => {
            let mut slot = state.proxy.lock().await;
            let Some(handle) = slot.take() else {
                return invalid_params(request.id, "proxy is not running");
            };
            let port = handle.port;
            handle.stop().await;
            Response::success(request.id, json!({"running": false, "port": port}))
        }
        "proxy.status" => {
            let slot = state.proxy.lock().await;
            match slot.as_ref() {
                // A terminated listener is reported as not running:
                // the old code kept reporting running: true after the
                // accept loop had died.
                Some(handle) if !handle.is_terminated() => Response::success(
                    request.id,
                    json!({"running": true, "port": handle.port, "started_at": unix_secs(handle.started_at)}),
                ),
                _ => Response::success(
                    request.id,
                    json!({"running": false, "port": null, "started_at": null}),
                ),
            }
        }
        _ => Response::error(request.id, -32601, "method not found"),
    }
}

/// All projects the daemon knows about: every project found by
/// scanning the parked paths, plus every linked project. Each entry
/// carries its domain (when linked, or auto-derived when parked) and
/// its source. Shared by `projects.list`, the G2 pin validation, and
/// the G3 known-domain check, so the three can never disagree about
/// what "known" means.
fn known_projects(
    store: &StateStore,
    depth: usize,
) -> Vec<(ProjectCandidate, Option<String>, &'static str)> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut enriched: Vec<(ProjectCandidate, Option<String>, &'static str)> = Vec::new();
    for root in &store.state.parked_paths {
        let root_path = PathBuf::from(root);
        if let Ok(projects) = scan_projects(&root_path, depth) {
            for project in projects {
                if found.contains(&project.path) {
                    continue;
                }
                let (domain, source) = enrich(&project, store);
                found.push(project.path.clone());
                enriched.push((project, domain, source));
            }
        }
    }
    for link in &store.state.links {
        let link_path = PathBuf::from(&link.path);
        if found.contains(&link_path) {
            continue;
        }
        let project = detect_project(&link_path).unwrap_or(ProjectCandidate {
            name: link_path.file_name().and_then(|v| v.to_str()).unwrap_or("project").to_string(),
            path: link_path.clone(),
            kind: "Unknown".into(),
            manifest: String::new(),
        });
        found.push(link_path);
        enriched.push((project, Some(link.domain.clone()), "Linked"));
    }
    enriched.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    enriched
}

/// Every domain the daemon knows about: linked domains plus the
/// auto-domains of parked projects.
fn known_domains(store: &StateStore, depth: usize) -> Vec<String> {
    let mut domains: Vec<String> =
        known_projects(store, depth).into_iter().filter_map(|(_, domain, _)| domain).collect();
    domains.sort();
    domains.dedup();
    domains
}

/// Domain and source for a scanned project: an exact link wins, then a
/// project sitting directly in (or equal to) a parked path gets an
/// auto-domain and source `Parked`, otherwise `Scanned` with no domain.
fn enrich(project: &ProjectCandidate, store: &StateStore) -> (Option<String>, &'static str) {
    let path = path_string(&project.path);
    if let Some(link) = store.state.links.iter().find(|l| l.path == path) {
        return (Some(link.domain.clone()), "Linked");
    }
    for root in &store.state.parked_paths {
        let root_path = PathBuf::from(root);
        let direct = project.path == root_path
            || project.path.parent().is_some_and(|parent| parent == root_path);
        if direct {
            return (auto_domain(&project.name), "Parked");
        }
    }
    (None, "Scanned")
}

pub fn token_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("daemon.token"))
}

pub fn read_token() -> io::Result<String> {
    Ok(fs::read_to_string(token_path().map_err(io::Error::other)?)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_dir_canonicalizes_when_present_and_deleted() {
        let base =
            std::env::temp_dir().join(format!("herd-stored-dir-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let target = base.join("sub").join("proj");
        fs::create_dir_all(&target).unwrap();
        // Existing directory: canonicalized (resolves `.` segments and
        // symlinks, e.g. /var on macOS is a symlink to /private/var),
        // so the expectation is canonicalized too.
        let target_canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(stored_dir(&base.join("sub").join(".").join("proj")), target_canonical);
        // Deleted directory: the surviving prefix still resolves, so
        // the deleted path matches what was stored when it existed.
        // The expectation is the canonical base (identical to `base`
        // unless the temp dir itself is reached through a symlink).
        let base_canonical = fs::canonicalize(&base).unwrap();
        let deleted_target = base_canonical.join("sub").join("proj");
        fs::remove_dir_all(&base).unwrap();
        assert_eq!(stored_dir(&target), deleted_target);
        assert_eq!(
            stored_dir(&base.join("sub").join("..").join("sub").join("proj")),
            deleted_target
        );
        // A relative path is resolved against the current directory.
        let cwd = std::env::current_dir().unwrap();
        let relative = target.strip_prefix(&cwd).ok().map(|p| p.to_path_buf());
        if let Some(relative) = relative {
            assert_eq!(stored_dir(&relative), target);
        }
        // Deleted directory under a symlinked parent: the symlink in
        // the surviving prefix still resolves, so the deleted path
        // matches what was stored when it existed (the macOS TMPDIR
        // case: /var is a symlink to /private/var).
        let link_base =
            std::env::temp_dir().join(format!("herd-stored-dir-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&link_base);
        let real = link_base.join("real");
        let target = real.join("proj");
        fs::create_dir_all(&target).unwrap();
        let target_canonical = fs::canonicalize(&target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, link_base.join("link")).unwrap();
        #[cfg(unix)]
        {
            let via_link = link_base.join("link").join("proj");
            assert_eq!(stored_dir(&via_link), target_canonical);
            fs::remove_dir_all(&target).unwrap();
            assert_eq!(stored_dir(&via_link), target_canonical);
        }
        let _ = fs::remove_dir_all(&link_base);
    }
}
