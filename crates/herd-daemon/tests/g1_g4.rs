//! End-to-end test of the G1 (park/link) and G4 (services) methods
//! against a real daemon served on a port 0 listener, with the state
//! file pointed at a temp dir via `HERD_CONFIG_DIR` semantics (the
//! test builds the `DaemonState` directly, so no env override is needed
//! here; the env override exists for manual testing).
//!
//! One test function runs the whole flow sequentially so the spawned
//! services are always stopped again at the end.

use herd_daemon::{DaemonState, serve};
use herd_protocol::{Request, Response};
use serde_json::{Value, json};
use std::{fs, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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

    async fn err_code(&mut self, method: &str, params: Value) -> i32 {
        let response = self.call(method, params).await;
        response.error.expect("expected an error").code
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn g1_park_link_list_and_g4_service_lifecycle() {
    let base = std::env::temp_dir().join(format!("herd-g1g4-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let root = base.join("parked");
    let web = root.join("web");
    let api = root.join("api");
    fs::create_dir_all(&web).unwrap();
    fs::create_dir_all(&api).unwrap();
    fs::write(web.join("package.json"), "{}").unwrap();
    fs::write(api.join("Cargo.toml"), "[package]").unwrap();
    let outside = base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("package.json"), "{}").unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    // Old methods still behave.
    let status = client.ok("system.status", json!({})).await;
    assert_eq!(status["protocol"], "1.1");
    assert_eq!(status["status"], "running");

    // G1: park, link, list.
    assert_eq!(client.err_code("projects.park", json!({"path": base.join("nope")})).await, -32602);
    let parked = client.ok("projects.park", json!({"path": root})).await;
    assert_eq!(parked["parked"], true);
    assert_eq!(client.err_code("projects.park", json!({"path": root})).await, -32602);
    let linked = client.ok("projects.link", json!({"path": outside, "domain": "myapp"})).await;
    assert_eq!(linked["domain"], "myapp.test");
    assert_eq!(
        client.err_code("projects.link", json!({"path": outside, "domain": "bad_domain"})).await,
        -32602
    );
    assert_eq!(
        client.err_code("projects.link", json!({"path": outside, "domain": "other.test"})).await,
        -32602
    );

    let listed = client.ok("projects.list", json!({})).await;
    let projects = listed["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 3, "{projects:?}");
    let by_name = |name: &str| projects.iter().find(|p| p["name"] == name).unwrap().clone();
    assert_eq!(by_name("web")["domain"], "web.test");
    assert_eq!(by_name("web")["source"], "Parked");
    assert_eq!(by_name("api")["domain"], "api.test");
    assert_eq!(by_name("outside")["domain"], "myapp.test");
    assert_eq!(by_name("outside")["source"], "Linked");

    // State survives a reload from disk.
    let reloaded = DaemonState::load(&config_dir).unwrap();
    drop(reloaded);
    let state_file = config_dir.join("state.json");
    assert!(state_file.exists());
    let text = fs::read_to_string(&state_file).unwrap();
    assert!(text.contains("myapp.test"));

    // G4: create, start, health, logs, stop, remove.
    // Registering a service does not check the binary; starting does.
    let ghost = client
        .ok(
            "services.create",
            json!({"name": "ghost", "command": "herd-definitely-not-a-binary-xyz"}),
        )
        .await;
    assert_eq!(ghost["name"], "ghost");
    assert_eq!(client.err_code("services.start", json!({"name": "ghost"})).await, -32010);
    assert_eq!(
        client.err_code("services.create", json!({"name": "ghost", "command": "sleep"})).await,
        -32602
    );
    assert_eq!(
        client.err_code("services.create", json!({"name": "Bad Name", "command": "sleep"})).await,
        -32602
    );
    // Catalog default: redis without a command fills in redis-server:6379.
    let redis = client.ok("services.create", json!({"name": "redis"})).await;
    assert_eq!(redis["command"], "redis-server");
    assert_eq!(redis["port"], 6379);

    let port = free_port();
    let web = client
        .ok(
            "services.create",
            json!({"name": "websvc", "command": "python3", "args": ["-m", "http.server", port.to_string()], "port": port}),
        )
        .await;
    assert_eq!(web["port"], port);
    let started = client.ok("services.start", json!({"name": "websvc"})).await;
    assert_eq!(started["status"], "running");
    assert!(started["pid"].as_u64().unwrap() > 0);
    assert_eq!(client.err_code("services.start", json!({"name": "websvc"})).await, -32602);
    // Health: the http server answers on its port, so healthy becomes
    // true. Binding takes a moment after spawn, so poll briefly.
    let mut healthy = false;
    for _ in 0..30 {
        let listed = client.ok("services.list", json!({})).await;
        let services = listed["services"].as_array().unwrap();
        let websvc = services.iter().find(|s| s["name"] == "websvc").unwrap();
        assert_eq!(websvc["status"], "running");
        if websvc["healthy"] == true {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(healthy, "python http.server never became healthy");
    assert_eq!(client.err_code("services.remove", json!({"name": "websvc"})).await, -32602);

    // Logs: a service that prints on start leaves the line in its log.
    client
        .ok(
            "services.create",
            json!({"name": "logger", "command": "sh", "args": ["-c", "echo herd-log-line; sleep 60"]}),
        )
        .await;
    client.ok("services.start", json!({"name": "logger"})).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let logs = client.ok("services.logs", json!({"name": "logger", "lines": 10})).await;
    let lines = logs["lines"].as_array().unwrap();
    assert!(lines.iter().any(|line| line.as_str().unwrap().contains("herd-log-line")), "{lines:?}");

    // Stop everything, remove, unlink, unpark, list is empty again.
    let stopped = client.ok("services.stop", json!({"name": "websvc"})).await;
    assert_eq!(stopped["status"], "stopped");
    client.ok("services.stop", json!({"name": "logger"})).await;
    let listed = client.ok("services.list", json!({})).await;
    let services = listed["services"].as_array().unwrap();
    assert!(services.iter().find(|s| s["name"] == "websvc").unwrap()["status"] == "stopped");
    assert_eq!(client.ok("services.remove", json!({"name": "websvc"})).await["removed"], true);
    assert_eq!(client.ok("services.remove", json!({"name": "logger"})).await["removed"], true);
    assert_eq!(client.ok("services.remove", json!({"name": "ghost"})).await["removed"], true);
    assert_eq!(client.ok("services.remove", json!({"name": "redis"})).await["removed"], true);
    assert_eq!(client.err_code("services.stop", json!({"name": "ghost"})).await, -32602);
    assert_eq!(client.err_code("services.logs", json!({"name": "ghost"})).await, -32602);

    assert_eq!(client.ok("projects.unlink", json!({"path": outside})).await["linked"], false);
    assert_eq!(client.ok("projects.unpark", json!({"path": root})).await["parked"], false);
    let listed = client.ok("projects.list", json!({})).await;
    assert_eq!(listed["projects"].as_array().unwrap().len(), 0);

    fs::remove_dir_all(&base).unwrap();
}

/// Unpark/unlink must work after the directory was deleted: the stored
/// registration is matched by its canonical path, not by re-validating
/// that the directory still exists.
#[tokio::test]
async fn unpark_unlink_deleted_directories() {
    let base = std::env::temp_dir().join(format!("herd-del-dir-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let root = base.join("parked");
    let outside = base.join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("package.json"), "{}").unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    client.ok("projects.park", json!({"path": root})).await;
    client.ok("projects.link", json!({"path": outside, "domain": "gone"})).await;
    let listed = client.ok("projects.list", json!({})).await;
    assert_eq!(listed["projects"].as_array().unwrap().len(), 1);

    // Delete both directories from the filesystem, then unregister.
    fs::remove_dir_all(&root).unwrap();
    fs::remove_dir_all(&outside).unwrap();
    assert_eq!(client.ok("projects.unlink", json!({"path": outside})).await["linked"], false);
    assert_eq!(client.ok("projects.unpark", json!({"path": root})).await["parked"], false);
    // The domain is released: linking another path to it works again.
    let other = base.join("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("package.json"), "{}").unwrap();
    let linked = client.ok("projects.link", json!({"path": other, "domain": "gone"})).await;
    assert_eq!(linked["domain"], "gone.test");
    // And the deleted paths are gone from the list (only `other` left).
    let listed = client.ok("projects.list", json!({})).await;
    let projects = listed["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1, "{projects:?}");
    assert_eq!(projects[0]["name"], "other");

    fs::remove_dir_all(&base).unwrap();
}

/// Racing `services.start` against `services.remove` must never leave
/// a process running without a definition: either start wins (the
/// service stays registered and running) or remove wins (the start
/// fails). A leaked process is caught by the port it would still hold.
#[tokio::test]
async fn start_remove_race_leaves_no_orphan() {
    let base = std::env::temp_dir().join(format!("herd-race-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });

    for round in 0..20 {
        let name = format!("racer-{round}");
        let svc_port = free_port();
        let mut client = Client::connect(port).await;
        client
            .ok(
                "services.create",
                json!({"name": name, "command": "python3", "args": ["-m", "http.server", svc_port.to_string()], "port": svc_port}),
            )
            .await;
        // Two connections racing: one starts, one removes.
        let mut starter = Client::connect(port).await;
        let mut remover = Client::connect(port).await;
        let name_for_start = name.clone();
        let name_for_remove = name.clone();
        let (start_result, remove_result) = tokio::join!(
            async move { starter.call("services.start", json!({"name": name_for_start})).await },
            async move { remover.call("services.remove", json!({"name": name_for_remove})).await },
        );
        // Exactly one of the two may observe success; a start that
        // succeeded must find the definition still present, and a
        // remove that succeeded must mean no process was spawned.
        let start_ok = start_result.error.is_none();
        let remove_ok = remove_result.error.is_none();
        assert!(
            !(start_ok && remove_ok),
            "round {round}: both start and remove succeeded; definition removed under a running service"
        );
        // Clean up whatever state the race left behind.
        let listed = client.ok("services.list", json!({})).await;
        let services = listed["services"].as_array().unwrap();
        if let Some(entry) = services.iter().find(|s| s["name"] == name) {
            if entry["status"] == "running" {
                client.ok("services.stop", json!({"name": name})).await;
            }
            client.ok("services.remove", json!({"name": name})).await;
        }
        // No leaked process may still hold the port.
        assert!(
            std::net::TcpListener::bind(("127.0.0.1", svc_port)).is_ok(),
            "round {round}: port {svc_port} still held after start/remove race"
        );
    }

    fs::remove_dir_all(&base).unwrap();
}
