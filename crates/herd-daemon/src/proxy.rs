//! HTTP proxy + static file server for linked `.test` domains (PR-1,
//! protocol 1.3). Routes are computed from `state.json` (links,
//! services) every time they are needed; no route table is stored.
//!
//! Route order for one domain (plan section 3):
//! 1. A Herd service whose `workdir` is the linked project, which is
//!    running and declares a port: reverse proxy to 127.0.0.1:port.
//! 2. Otherwise the first docroot under the project (`public/`,
//!    `dist/`, `build/`, `out/`, `static/`, then the project root)
//!    containing an `index.html`: serve static files from it.
//! 3. Otherwise the Herd placeholder page (linked, but nothing serves
//!    it yet).
//! 4. A host that is not linked at all: the Herd 404 page, not a
//!    connection error. Subdomains `<sub>.name.test` go to the same
//!    site as `name.test`.
//!
//! The HTTP server listens on 127.0.0.1 only. HTTP first; TLS is PR-3,
//! DNS is PR-2. The route computation here is a port of the proven
//! spike code (`spike-pr0/src/route.rs`, 8 tests) adapted to the
//! daemon's `State` and `ServiceDef` types; the HTTP plumbing is a
//! port of `spike-pr0/src/http_server.rs` (HTTP part only, no TLS).
//! API usage was verified against hyper 1.13.0 / hyper-util 0.1.21 /
//! http-body-util 0.1.5 docs.rs and crate sources (see the spike
//! report, section 0); the versions are pinned exactly in the
//! workspace `Cargo.toml`.

use crate::{DaemonState, state::State};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::Serialize;
use std::{
    collections::HashSet,
    convert::Infallible,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

/// Default HTTP listen port. `proxy.start` accepts an explicit port
/// for tests and for machines where 80 is taken; production use is
/// port 80.
pub const DEFAULT_PROXY_PORT: u16 = 80;

/// Docroot candidates under a project, in order. The empty candidate
/// is the project root itself.
pub const DOCROOT_CANDIDATES: [&str; 6] = ["public", "dist", "build", "out", "static", ""];

/// What serves one linked domain, computed from state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "target", rename_all = "lowercase")]
pub enum RouteTarget {
    /// Reverse proxy to a running service.
    Proxy { service: String, port: u16 },
    /// Serve static files from this docroot (per domain, never shared).
    Static { docroot: PathBuf },
    /// Linked, but nothing serves it yet.
    Placeholder,
}

/// One computed route: a linked domain and what serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RouteInfo {
    pub domain: String,
    pub project: String,
    #[serde(flatten)]
    pub target: RouteTarget,
}

/// The per-request resolution of a Host + path against computed routes.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolvedRoute {
    Proxy { port: u16 },
    Static { file: PathBuf },
    Placeholder { domain: String },
    Unknown,
}

/// True when `host` is `domain` or a subdomain of it. Case-insensitive;
/// a `:port` suffix on the host is ignored.
fn host_matches(host: &str, domain: &str) -> bool {
    let host = host.to_lowercase();
    let host = host.split(':').next().unwrap_or(&host);
    let domain = domain.to_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The bare host of a Host header value: lowercased, port stripped.
fn bare_host(host: &str) -> String {
    host.split(':').next().unwrap_or(host).to_lowercase()
}

/// First docroot candidate under `project` that contains an
/// `index.html`. `None` when the project serves no static site.
pub fn find_docroot(project: &Path) -> Option<PathBuf> {
    for candidate in DOCROOT_CANDIDATES {
        let dir =
            if candidate.is_empty() { project.to_path_buf() } else { project.join(candidate) };
        if dir.join("index.html").is_file() {
            return Some(dir);
        }
    }
    None
}

/// Compute the routes for every linked domain in `state`. `running`
/// holds the names of services that are currently running; only a
/// running service that declares a port and whose `workdir` is the
/// linked project becomes a proxy target. When several services match
/// one project, the first in state order wins.
pub fn compute_routes(state: &State, running: &HashSet<String>) -> Vec<RouteInfo> {
    let mut routes: Vec<RouteInfo> = state
        .links
        .iter()
        .map(|link| {
            let project = PathBuf::from(&link.path);
            let target = state
                .services
                .iter()
                .find(|def| {
                    def.workdir.as_deref() == Some(link.path.as_str())
                        && def.port.is_some()
                        && running.contains(&def.name)
                })
                .map(|def| RouteTarget::Proxy {
                    service: def.name.clone(),
                    port: def.port.expect("port checked above"),
                })
                .or_else(|| find_docroot(&project).map(|docroot| RouteTarget::Static { docroot }))
                .unwrap_or(RouteTarget::Placeholder);
            RouteInfo { domain: link.domain.clone(), project: link.path.clone(), target }
        })
        .collect();
    routes.sort_by(|a, b| a.domain.cmp(&b.domain));
    routes
}

/// Resolve a static file under `docroot` for `path`. Returns `None`
/// when the file does not exist or the path tries to escape the
/// docroot. A directory request serves that directory's `index.html`.
fn resolve_static(docroot: &Path, path: &str) -> Option<PathBuf> {
    if path.split('/').any(|segment| segment == "..") {
        return None;
    }
    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let candidate = docroot.join(rel);
    if candidate.is_file() {
        return Some(candidate);
    }
    if candidate.is_dir() {
        let index = candidate.join("index.html");
        if index.is_file() {
            return Some(index);
        }
    }
    None
}

/// Resolve one request against the computed routes.
pub fn resolve_route(host: &str, path: &str, routes: &[RouteInfo]) -> ResolvedRoute {
    let host = bare_host(host);
    for route in routes {
        if !host_matches(&host, &route.domain) {
            continue;
        }
        return match &route.target {
            RouteTarget::Proxy { port, .. } => ResolvedRoute::Proxy { port: *port },
            RouteTarget::Static { docroot } => match resolve_static(docroot, path) {
                Some(file) => ResolvedRoute::Static { file },
                None => ResolvedRoute::Placeholder { domain: route.domain.clone() },
            },
            RouteTarget::Placeholder => ResolvedRoute::Placeholder { domain: route.domain.clone() },
        };
    }
    ResolvedRoute::Unknown
}

/// Compute the current routes from the daemon state: links and service
/// definitions from the store, running services from the supervisor.
/// Lock order is store-then-supervisor, the same as `services.list`;
/// never the reverse.
pub async fn current_routes(state: &DaemonState) -> Vec<RouteInfo> {
    let (links, services) = {
        let store = state.store.read().await;
        (store.state.links.clone(), store.state.services.clone())
    };
    let mut supervisor = state.supervisor.lock().await;
    let running: HashSet<String> = services
        .iter()
        .filter(|def| def.port.is_some() && supervisor.is_running(&def.name))
        .map(|def| def.name.clone())
        .collect();
    let snapshot = State { links, services, ..Default::default() };
    compute_routes(&snapshot, &running)
}

/// A running proxy server: its port, start time, and the shutdown
/// signal plus task that stop it.
pub struct ProxyHandle {
    pub port: u16,
    pub started_at: SystemTime,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl ProxyHandle {
    /// Signal the accept loop to stop and wait for it to finish.
    pub async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = self.task.await;
    }
}

/// Start the HTTP proxy on 127.0.0.1:`port`. Fails -32010-style with a
/// clear message when the port is already taken, naming the owning
/// process when it can be detected.
pub async fn start_proxy(state: Arc<DaemonState>, port: u16) -> Result<ProxyHandle, String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|error| {
        let owner = port_owner(port).map(|name| format!(" (owned by {name})")).unwrap_or_default();
        format!("cannot bind 127.0.0.1:{port}: {error}{owner}")
    })?;
    let client: Client<HttpConnector, Full<Bytes>> =
        Client::builder(TokioExecutor::new()).build_http();
    let http = Arc::new(HttpState { client, daemon: state });
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _)) => {
                            let http = Arc::clone(&http);
                            tokio::spawn(async move {
                                let io = TokioIo::new(stream);
                                if let Err(error) = http1::Builder::new()
                                    .serve_connection(
                                        io,
                                        service_fn(move |req| handle_request(Arc::clone(&http), req)),
                                    )
                                    .await
                                {
                                    tracing::warn!(%error, "proxy connection error");
                                }
                            });
                        }
                        Err(error) => {
                            tracing::warn!(%error, "proxy accept error");
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(ProxyHandle { port, started_at: SystemTime::now(), shutdown: shutdown_tx, task })
}

struct HttpState {
    client: Client<HttpConnector, Full<Bytes>>,
    daemon: Arc<DaemonState>,
}

/// Hop-by-hop headers that must not be copied through the proxy.
const HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn text_response(status: StatusCode, body: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

/// The Herd placeholder page: a linked domain with nothing serving it
/// yet. The 404 page for an unlinked domain lives in `unknown_page`.
pub fn placeholder_page(domain: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{domain} - Herd</title></head>\
        <body><h1>{domain}</h1>\
        <p>This domain is linked in Herd, but no service is running for it yet.</p>\
        <p>Start a service for this project with <code>herd services start &lt;name&gt;</code>, \
        or add an <code>index.html</code> to its <code>public/</code> folder.</p></body></html>"
    )
}

pub fn unknown_page(host: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>404 - Herd</title></head>\
        <body><h1>404</h1><p>Domain {host} is not linked in Herd yet. \
        Link a project with <code>herd projects link &lt;path&gt; &lt;domain&gt;</code>.</p></body></html>"
    )
}

async fn handle_request(
    http: Arc<HttpState>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let host_bare = bare_host(&host);
    let path = req.uri().path().to_string();
    let routes = current_routes(&http.daemon).await;
    let route = resolve_route(&host, &path, &routes);
    match route {
        ResolvedRoute::Proxy { port } => match proxy_forward(&http, req, port).await {
            Ok(response) => Ok(response),
            Err(error) => Ok(text_response(
                StatusCode::BAD_GATEWAY,
                format!("<h1>502 Bad Gateway</h1><p>upstream 127.0.0.1:{port} failed: {error}</p>"),
            )),
        },
        ResolvedRoute::Static { file } => match tokio::fs::read(&file).await {
            Ok(bytes) => Ok(Response::builder()
                .status(StatusCode::OK)
                .header("content-type", content_type(&file))
                .body(Full::new(Bytes::from(bytes)))
                .unwrap()),
            Err(_) => Ok(text_response(StatusCode::NOT_FOUND, "<h1>404</h1>".into())),
        },
        ResolvedRoute::Placeholder { domain } => {
            Ok(text_response(StatusCode::OK, placeholder_page(&domain)))
        }
        ResolvedRoute::Unknown => {
            Ok(text_response(StatusCode::NOT_FOUND, unknown_page(&host_bare)))
        }
    }
}

/// Forward one request to the upstream service on 127.0.0.1:`port`.
/// The listener is loopback-only, so `X-Forwarded-For` is always
/// 127.0.0.1. The whole upstream body is buffered in memory (dev
/// proxy; upstream is a local dev server, not the open internet).
async fn proxy_forward(
    http: &HttpState,
    req: Request<Incoming>,
    port: u16,
) -> Result<Response<Full<Bytes>>, anyhow::Error> {
    let path_and_query = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let uri = format!("http://127.0.0.1:{port}{path_and_query}");
    let method = req.method().clone();
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in req.headers() {
        if HOP_HEADERS.contains(&name.as_str()) || name.as_str() == "host" {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder.header("host", format!("127.0.0.1:{port}"));
    builder = builder.header("x-forwarded-for", "127.0.0.1");
    builder = builder.header("x-forwarded-proto", "http");
    let body_bytes = req.into_body().collect().await?.to_bytes();
    let out_req = builder.body(Full::new(body_bytes))?;
    let response = http.client.request(out_req).await?;
    let (parts, body) = response.into_parts();
    let bytes = body.collect().await?.to_bytes();
    let mut out = Response::builder().status(parts.status);
    for (name, value) in parts.headers.iter() {
        if HOP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        out = out.header(name, value);
    }
    Ok(out.body(Full::new(bytes))?)
}

/// Name of the process listening on `port`, best effort (Linux only):
/// `"<comm> (pid <pid>)"`, or `None` when it cannot be determined.
/// Anything else (non-Linux, no /proc access, no listener) is `None`,
/// never an error: the bind failure is reported either way.
#[cfg(target_os = "linux")]
pub fn port_owner(port: u16) -> Option<String> {
    let mut inodes = Vec::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        if let Ok(text) = std::fs::read_to_string(file) {
            inodes.extend(listening_inodes(&text, port));
        }
    }
    if inodes.is_empty() {
        return None;
    }
    let procs = std::fs::read_dir("/proc").ok()?;
    for entry in procs.flatten() {
        // Non-numeric entries (self, net, sys, ...) are skipped, not
        // fatal: a `?` here would abort the whole scan on the first
        // one. Same for fds that vanish mid-scan.
        let Ok(pid) = entry.file_name().to_str().unwrap_or("").parse::<u32>() else { continue };
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else { continue };
            let target = target.to_string_lossy();
            if inodes.iter().any(|inode| target == format!("socket:[{inode}]")) {
                let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                    .map(|c| c.trim().to_string())
                    .unwrap_or_default();
                if comm.is_empty() {
                    return Some(format!("pid {pid}"));
                }
                return Some(format!("{comm} (pid {pid})"));
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
pub fn port_owner(_port: u16) -> Option<String> {
    None
}

/// Inodes of LISTEN sockets bound to `port` in one `/proc/net/tcp`
/// (or tcp6) dump. Pure function, unit-tested below.
#[cfg(target_os = "linux")]
fn listening_inodes(proc_net_tcp: &str, port: u16) -> Vec<u64> {
    let mut inodes = Vec::new();
    for line in proc_net_tcp.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        // fields[1] is local_address "IP:PORT" in hex, fields[3] is
        // the state (0A = LISTEN), fields[9] is the socket inode.
        let Some((_, hex_port)) = fields[1].split_once(':') else { continue };
        let Ok(bound) = u16::from_str_radix(hex_port, 16) else { continue };
        if bound != port || fields[3] != "0A" {
            continue;
        }
        if let Ok(inode) = fields[9].parse::<u64>() {
            inodes.push(inode);
        }
    }
    inodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Link, ServiceDef};
    use std::fs;

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herd-proxy-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state_with(links: Vec<(&str, &str)>, services: Vec<ServiceDef>) -> State {
        State {
            version: 1,
            links: links
                .into_iter()
                .map(|(path, domain)| Link { path: path.into(), domain: domain.into() })
                .collect(),
            services,
            ..Default::default()
        }
    }

    fn service(name: &str, workdir: Option<&str>, port: Option<u16>) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            command: "python3".into(),
            args: vec![],
            port,
            workdir: workdir.map(str::to_string),
            kind: None,
        }
    }

    #[test]
    fn running_service_with_port_wins_over_static() {
        let dir = tmp_dir("proxy-wins");
        let web = dir.join("web");
        fs::create_dir_all(web.join("public")).unwrap();
        fs::write(web.join("public").join("index.html"), "static").unwrap();
        let state = state_with(
            vec![(web.to_str().unwrap(), "myapp.test")],
            vec![service("web", Some(web.to_str().unwrap()), Some(18081))],
        );
        let running: HashSet<String> = ["web".to_string()].into_iter().collect();
        let routes = compute_routes(&state, &running);
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].target, RouteTarget::Proxy { service: "web".into(), port: 18081 });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stopped_service_falls_back_to_static() {
        let dir = tmp_dir("stopped-static");
        let web = dir.join("web");
        fs::create_dir_all(web.join("public")).unwrap();
        fs::write(web.join("public").join("index.html"), "static").unwrap();
        let state = state_with(
            vec![(web.to_str().unwrap(), "myapp.test")],
            vec![service("web", Some(web.to_str().unwrap()), Some(18081))],
        );
        let routes = compute_routes(&state, &HashSet::new());
        assert_eq!(routes[0].target, RouteTarget::Static { docroot: web.join("public") });
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn service_without_port_or_workdir_never_proxies() {
        let dir = tmp_dir("no-port");
        let web = dir.join("web");
        fs::create_dir_all(&web).unwrap();
        let state = state_with(
            vec![(web.to_str().unwrap(), "myapp.test")],
            vec![
                service("noport", Some(web.to_str().unwrap()), None),
                service("noworkdir", None, Some(18081)),
            ],
        );
        let running: HashSet<String> =
            ["noport".to_string(), "noworkdir".to_string()].into_iter().collect();
        let routes = compute_routes(&state, &running);
        assert_eq!(routes[0].target, RouteTarget::Placeholder);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn service_for_another_project_does_not_capture_domain() {
        let dir = tmp_dir("other-project");
        let web = dir.join("web");
        let api = dir.join("api");
        fs::create_dir_all(&web).unwrap();
        fs::create_dir_all(&api).unwrap();
        let state = state_with(
            vec![(web.to_str().unwrap(), "myapp.test")],
            vec![service("api", Some(api.to_str().unwrap()), Some(18081))],
        );
        let running: HashSet<String> = ["api".to_string()].into_iter().collect();
        let routes = compute_routes(&state, &running);
        assert_eq!(routes[0].target, RouteTarget::Placeholder);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn docroot_order_prefers_public_then_dist_then_root() {
        let dir = tmp_dir("docroot-order");
        let web = dir.join("web");
        for sub in ["public", "dist", "build", "out", "static"] {
            fs::create_dir_all(web.join(sub)).unwrap();
            fs::write(web.join(sub).join("index.html"), sub).unwrap();
        }
        assert_eq!(find_docroot(&web), Some(web.join("public")));
        fs::remove_file(web.join("public").join("index.html")).unwrap();
        assert_eq!(find_docroot(&web), Some(web.join("dist")));
        for sub in ["dist", "build", "out", "static"] {
            fs::remove_file(web.join(sub).join("index.html")).unwrap();
        }
        assert_eq!(find_docroot(&web), None);
        fs::write(web.join("index.html"), "root").unwrap();
        assert_eq!(find_docroot(&web), Some(web.clone()));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn docroot_is_per_domain_not_shared() {
        let dir = tmp_dir("per-domain");
        let a = dir.join("a");
        let b = dir.join("b");
        fs::create_dir_all(a.join("public")).unwrap();
        fs::write(a.join("public").join("index.html"), "a").unwrap();
        fs::create_dir_all(&b).unwrap();
        let state = state_with(
            vec![(a.to_str().unwrap(), "a.test"), (b.to_str().unwrap(), "b.test")],
            vec![],
        );
        let routes = compute_routes(&state, &HashSet::new());
        assert_eq!(routes[0].domain, "a.test");
        assert_eq!(routes[0].target, RouteTarget::Static { docroot: a.join("public") });
        assert_eq!(routes[1].domain, "b.test");
        assert_eq!(routes[1].target, RouteTarget::Placeholder);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn subdomain_resolves_to_the_same_site() {
        let routes = vec![RouteInfo {
            domain: "myapp.test".into(),
            project: "/srv/web".into(),
            target: RouteTarget::Proxy { service: "web".into(), port: 18081 },
        }];
        assert_eq!(
            resolve_route("api.myapp.test", "/foo", &routes),
            ResolvedRoute::Proxy { port: 18081 }
        );
        assert_eq!(
            resolve_route("MyApp.TEST:80", "/", &routes),
            ResolvedRoute::Proxy { port: 18081 }
        );
    }

    #[test]
    fn static_file_directory_and_traversal() {
        let dir = tmp_dir("static-resolve");
        let docroot = dir.join("public");
        fs::create_dir_all(docroot.join("docs")).unwrap();
        fs::write(docroot.join("index.html"), "home").unwrap();
        fs::write(docroot.join("docs").join("index.html"), "docs").unwrap();
        let routes = vec![RouteInfo {
            domain: "static.test".into(),
            project: dir.to_str().unwrap().into(),
            target: RouteTarget::Static { docroot: docroot.clone() },
        }];
        assert_eq!(
            resolve_route("static.test", "/", &routes),
            ResolvedRoute::Static { file: docroot.join("index.html") }
        );
        assert_eq!(
            resolve_route("static.test", "/docs/", &routes),
            ResolvedRoute::Static { file: docroot.join("docs").join("index.html") }
        );
        // Traversal escapes the docroot: placeholder, never the file.
        assert_eq!(
            resolve_route("static.test", "/../Cargo.toml", &routes),
            ResolvedRoute::Placeholder { domain: "static.test".into() }
        );
        assert_eq!(
            resolve_route("static.test", "/nope.html", &routes),
            ResolvedRoute::Placeholder { domain: "static.test".into() }
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unlinked_host_is_unknown() {
        let routes = vec![RouteInfo {
            domain: "myapp.test".into(),
            project: "/srv/web".into(),
            target: RouteTarget::Placeholder,
        }];
        assert_eq!(resolve_route("stranger.test", "/", &routes), ResolvedRoute::Unknown);
        assert_eq!(resolve_route("", "/", &routes), ResolvedRoute::Unknown);
        // A suffix lookalike is not a subdomain: "notmyapp.test" must
        // not match "myapp.test".
        assert_eq!(resolve_route("notmyapp.test", "/", &routes), ResolvedRoute::Unknown);
    }

    #[test]
    fn placeholder_page_names_the_domain() {
        let page = placeholder_page("blog.test");
        assert!(page.contains("blog.test"));
        assert!(page.contains("herd services start"));
        let page = unknown_page("stranger.test");
        assert!(page.contains("stranger.test"));
        assert!(page.contains("404"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_net_tcp_parser_finds_listen_inode() {
        let sample = "  sl  local_address rem_address   st tx_queue:rx_queue tr:tm->when retrnsmt   uid  timeout inode\n\
             0: 00000000:0050 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 12345 1 0000000000000000 100 0 0 10 0\n\
             1: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99999 1 0000000000000000 100 0 0 10 0\n\
             2: 00000000:0050 00000000:0000 01 00000000:00000000 00:00000000 00000000     0        0 77777 1 0000000000000000 100 0 0 10 0\n";
        // Port 80 (0x50) LISTEN: inodes 12345 only; 99999 is port 8080,
        // 77777 is port 80 but state 01, not LISTEN.
        assert_eq!(listening_inodes(sample, 80), vec![12345]);
        assert_eq!(listening_inodes(sample, 8080), vec![99999]);
        assert!(listening_inodes(sample, 443).is_empty());
    }
}
