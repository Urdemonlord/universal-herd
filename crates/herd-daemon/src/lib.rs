use anyhow::{Context, Result};
use directories::ProjectDirs;
use herd_core::{ProjectCandidate, detect_project, scan_projects};
use herd_protocol::{PROTOCOL_VERSION, Request, Response};
use serde_json::{Value, json};
use state::{
    ServiceDef, StateError, StateStore, auto_domain, catalog_default, normalize_domain,
    validate_service_name,
};
use std::{
    fs, io,
    path::{Path, PathBuf},
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
}

impl DaemonState {
    pub fn load(dir: &std::path::Path) -> Result<Arc<Self>, StateError> {
        let store = StateStore::load(dir)?;
        let supervisor = Supervisor::new(dir.join("logs")).map_err(StateError::Io)?;
        Ok(Arc::new(Self { store: RwLock::new(store), supervisor: Mutex::new(supervisor) }))
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
                dispatch(request, &state).await
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

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn unix_secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[allow(clippy::too_many_lines)]
async fn dispatch(request: Request, state: &DaemonState) -> Response {
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
            if let Err(error) = store.park(&path) {
                return state_error(request.id, error);
            }
            match store.save() {
                Ok(()) => Response::success(request.id, json!({"path": path, "parked": true})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.unpark" => {
            let path = match canonical_dir(&request, "path") {
                Ok(path) => path,
                Err(response) => return response,
            };
            let path = path_string(&path);
            let mut store = state.store.write().await;
            if let Err(error) = store.unpark(&path) {
                return state_error(request.id, error);
            }
            match store.save() {
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
            if let Err(error) = store.link(&path, domain.clone()) {
                return state_error(request.id, error);
            }
            match store.save() {
                Ok(()) => Response::success(request.id, json!({"path": path, "domain": domain})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.unlink" => {
            let path = match canonical_dir(&request, "path") {
                Ok(path) => path,
                Err(response) => return response,
            };
            let path = path_string(&path);
            let mut store = state.store.write().await;
            if let Err(error) = store.unlink(&path) {
                return state_error(request.id, error);
            }
            match store.save() {
                Ok(()) => Response::success(request.id, json!({"path": path, "linked": false})),
                Err(error) => state_error(request.id, error),
            }
        }
        "projects.list" => {
            let depth =
                request.params.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(3) as usize;
            let store = state.store.read().await;
            let mut found: Vec<ProjectCandidate> = Vec::new();
            let mut sources: Vec<(String, Option<String>, &'static str)> = Vec::new();
            for root in &store.state.parked_paths {
                let root_path = PathBuf::from(root);
                if let Ok(projects) = scan_projects(&root_path, depth) {
                    for project in projects {
                        if found.iter().any(|p| p.path == project.path) {
                            continue;
                        }
                        let (domain, source) = enrich(&project, &store);
                        sources.push((path_string(&project.path), domain, source));
                        found.push(project);
                    }
                }
            }
            for link in &store.state.links {
                let link_path = PathBuf::from(&link.path);
                if found.iter().any(|p| p.path == link_path) {
                    continue;
                }
                let project = detect_project(&link_path).unwrap_or(ProjectCandidate {
                    name: link_path
                        .file_name()
                        .and_then(|v| v.to_str())
                        .unwrap_or("project")
                        .to_string(),
                    path: link_path.clone(),
                    kind: "Unknown".into(),
                    manifest: String::new(),
                });
                sources.push((path_string(&project.path), Some(link.domain.clone()), "Linked"));
                found.push(project);
            }
            found.sort_by(|a, b| a.path.cmp(&b.path));
            let projects: Vec<Value> = found
                .iter()
                .map(|p| {
                    let (domain, source) = sources
                        .iter()
                        .find(|(path, _, _)| path == &path_string(&p.path))
                        .map(|(_, domain, source)| (domain.clone(), *source))
                        .unwrap_or((None, "Scanned"));
                    json!({
                        "name": p.name, "path": p.path, "kind": p.kind, "manifest": p.manifest,
                        "domain": domain, "source": source,
                    })
                })
                .collect();
            Response::success(request.id, json!({"projects": projects}))
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
                    Some(path_string(&path))
                }
                None => None,
            };
            let def = ServiceDef { name, command, args, port, workdir, kind };
            let mut store = state.store.write().await;
            if let Err(error) = store.add_service(def.clone()) {
                return state_error(request.id, error);
            }
            match store.save() {
                Ok(()) => Response::success(request.id, serde_json::to_value(def).unwrap()),
                Err(error) => state_error(request.id, error),
            }
        }
        "services.start" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            let def = {
                let store = state.store.read().await;
                match store.service(&name) {
                    Some(def) => def.clone(),
                    None => {
                        return invalid_params(
                            request.id,
                            format!("service {name:?} does not exist"),
                        );
                    }
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
            {
                let mut supervisor = state.supervisor.lock().await;
                if supervisor.is_running(&name) {
                    return invalid_params(
                        request.id,
                        format!("service {name:?} is running; stop it first"),
                    );
                }
            }
            let mut store = state.store.write().await;
            if let Err(error) = store.remove_service(&name) {
                return state_error(request.id, error);
            }
            if let Err(error) = store.save() {
                return state_error(request.id, error);
            }
            let log_path = state.supervisor.lock().await.log_path(&name);
            let _ = fs::remove_file(log_path);
            Response::success(request.id, json!({"name": name, "removed": true}))
        }
        "services.logs" => {
            let name = match required_str(&request, "name") {
                Ok(name) => name.to_string(),
                Err(response) => return response,
            };
            {
                let store = state.store.read().await;
                if store.service(&name).is_none() {
                    return invalid_params(request.id, format!("service {name:?} does not exist"));
                }
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
        _ => Response::error(request.id, -32601, "method not found"),
    }
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
