//! End-to-end test of the PR-1 proxy methods against a real daemon
//! served on a port 0 listener, with the state file pointed at a temp
//! dir. Follows the `g2_g3.rs` pattern: one test function runs the
//! whole flow sequentially.
//!
//! A tiny Python HTTP server is the upstream service (it answers a
//! fixed body, so a proxied response is provably not the daemon's own
//! static serving). Static files, placeholder, and 404 are exercised
//! through the same proxy with different Host headers.

use herd_daemon::{DaemonState, serve};
use herd_protocol::{Request, Response};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

struct Client {
    stream: tokio::net::tcp::OwnedWriteHalf,
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    next_id: u64,
}

impl Client {
    async fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (read, write) = stream.into_split();
        Self { stream: write, reader: BufReader::new(read), next_id: 1 }
    }

    async fn call(&mut self, method: &str, params: Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            jsonrpc: "2.0".into(),
            id: json!(id),
            method: method.into(),
            params,
            token: Some("test-token".into()),
        };
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        self.stream.write_all(&bytes).await.unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        assert!(response.error.is_none(), "{method} failed: {:?}", response.error);
        response.result.unwrap()
    }

    async fn err(&mut self, method: &str, params: Value) -> (i32, String) {
        let response = self.call(method, params).await;
        let error = response.error.expect("expected an error");
        (error.code, error.message)
    }
}

/// A free loopback port (bind port 0, read, release).
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// Wait until something accepts TCP on `port` (an upstream that is
/// still starting up), up to ~2s.
async fn wait_for_tcp(port: u16) {
    for _ in 0..40 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("nothing listening on 127.0.0.1:{port} after 2s");
}

/// One raw HTTP GET through the proxy. Returns (status, body).
async fn http_get(port: u16, host: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

const UPSTREAM_PY: &str = r#"
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"hello-from-upstream"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
"#;

#[tokio::test]
async fn proxy_http_routes_start_stop_and_serve() {
    let base = std::env::temp_dir().join(format!("herd-proxy-pr1-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();

    // Three linked projects: web has a static site, api gets a running
    // service, blog has neither (placeholder). Directories are created
    // before canonicalizing, so the stored link path and the service
    // workdir match on machines where /tmp is a symlink.
    for name in ["web", "api", "blog"] {
        fs::create_dir_all(base.join(name)).unwrap();
        fs::write(base.join(name).join("package.json"), "{}").unwrap();
    }
    let web = fs::canonicalize(base.join("web")).unwrap();
    let api = fs::canonicalize(base.join("api")).unwrap();
    let blog = fs::canonicalize(base.join("blog")).unwrap();
    fs::create_dir_all(web.join("public")).unwrap();
    fs::write(web.join("public").join("index.html"), "static-marker-web").unwrap();
    fs::write(api.join("upstream.py"), UPSTREAM_PY).unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    let status = client.ok("system.status", json!({})).await;
    assert_eq!(status["protocol"], "1.3");

    // Proxy starts stopped; stopping a stopped proxy is -32602.
    let proxy_status = client.ok("proxy.status", json!({})).await;
    assert_eq!(proxy_status["running"], false);
    assert_eq!(client.err("proxy.stop", json!({})).await.0, -32602);

    client.ok("projects.link", json!({"path": web, "domain": "web"})).await;
    client.ok("projects.link", json!({"path": api, "domain": "api"})).await;
    client.ok("projects.link", json!({"path": blog, "domain": "blog"})).await;

    // Routes before any service runs: web static, api + blog placeholder.
    let routes = client.ok("proxy.routes", json!({})).await;
    let by_domain = |domain: &str| {
        routes["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["domain"] == domain)
            .cloned()
            .unwrap_or_else(|| panic!("no route for {domain}: {routes}"))
    };
    assert_eq!(by_domain("web.test")["target"], "static");
    assert!(by_domain("web.test")["docroot"].as_str().unwrap().ends_with("public"));
    assert_eq!(by_domain("api.test")["target"], "placeholder");
    assert_eq!(by_domain("blog.test")["target"], "placeholder");

    // Start the upstream service, then api.test routes to it.
    let upstream_port = free_port().await;
    client
        .ok(
            "services.create",
            json!({
                "name": "api",
                "command": "python3",
                "args": ["upstream.py", upstream_port.to_string()],
                "port": upstream_port,
                "workdir": api,
            }),
        )
        .await;
    let started = client.ok("services.start", json!({"name": "api"})).await;
    assert_eq!(started["status"], "running");
    wait_for_tcp(upstream_port).await;
    let routes = client.ok("proxy.routes", json!({})).await;
    let api_route = routes["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["domain"] == "api.test")
        .cloned()
        .unwrap();
    assert_eq!(api_route["target"], "proxy");
    assert_eq!(api_route["service"], "api");
    assert_eq!(api_route["port"], upstream_port);

    // Port 80 is the default, but tests use a free port.
    let proxy_port = free_port().await;
    let started = client.ok("proxy.start", json!({"port": proxy_port})).await;
    assert_eq!(started["running"], true);
    assert_eq!(started["port"], proxy_port);
    // A second start is -32010.
    let (code, message) = client.err("proxy.start", json!({"port": free_port().await})).await;
    assert_eq!(code, -32010, "{message}");
    assert!(message.contains("already running"), "{message}");

    // All four serving branches, plus subdomain routing.
    let (code, body) = http_get(proxy_port, "api.test", "/").await;
    assert_eq!(code, 200);
    assert_eq!(body, "hello-from-upstream");
    let (code, body) = http_get(proxy_port, "sub.api.test", "/foo?x=1").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, "hello-from-upstream");
    let (code, body) = http_get(proxy_port, "web.test", "/").await;
    assert_eq!(code, 200);
    assert_eq!(body, "static-marker-web");
    let (code, body) = http_get(proxy_port, "web.test", "/missing.html").await;
    assert_eq!(code, 200);
    assert!(body.contains("web.test"), "{body}");
    assert!(body.contains("no service is running"), "{body}");
    let (code, body) = http_get(proxy_port, "blog.test", "/").await;
    assert_eq!(code, 200);
    assert!(body.contains("blog.test"), "{body}");
    let (code, body) = http_get(proxy_port, "stranger.test", "/").await;
    assert_eq!(code, 404);
    assert!(body.contains("not linked"), "{body}");

    // Status while running, then stop: the port closes.
    let proxy_status = client.ok("proxy.status", json!({})).await;
    assert_eq!(proxy_status["running"], true);
    assert_eq!(proxy_status["port"], proxy_port);
    let stopped = client.ok("proxy.stop", json!({})).await;
    assert_eq!(stopped["running"], false);
    assert_eq!(stopped["port"], proxy_port);
    assert!(TcpStream::connect(("127.0.0.1", proxy_port)).await.is_err());
    let proxy_status = client.ok("proxy.status", json!({})).await;
    assert_eq!(proxy_status["running"], false);

    // -32010 when the port is already taken, naming the owner when
    // it can be detected (the test process owns this listener).
    let taken_port = free_port().await;
    let _holder = TcpListener::bind(("127.0.0.1", taken_port)).await.unwrap();
    let (code, message) = client.err("proxy.start", json!({"port": taken_port})).await;
    assert_eq!(code, -32010, "{message}");
    assert!(message.contains(&taken_port.to_string()), "{message}");
    assert!(message.contains("Address already in use"), "{message}");
    let owner = herd_daemon::proxy::port_owner(taken_port);
    assert!(owner.is_some(), "port_owner should find the test process");
    drop(_holder);

    // Bad port values are -32602.
    assert_eq!(client.err("proxy.start", json!({"port": 0})).await.0, -32602);
    assert_eq!(client.err("proxy.start", json!({"port": 70000})).await.0, -32602);

    client.ok("services.stop", json!({"name": "api"})).await;
    fs::remove_dir_all(base).unwrap();
}
