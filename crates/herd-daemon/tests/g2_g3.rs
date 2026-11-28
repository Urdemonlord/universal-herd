//! End-to-end test of the G2 (isolate) and G3 (secure) methods against
//! a real daemon served on a port 0 listener, with the state file
//! pointed at a temp dir. Follows the `g1_g4.rs` pattern: one test
//! function runs the whole flow sequentially.
//!
//! The G3 certificate flow runs only when `openssl` is on PATH; when
//! it is not, the test asserts the clear -32010 failure instead.

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

fn looks_like_version(v: &str) -> bool {
    v.contains('.') && v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn openssl_present() -> bool {
    std::process::Command::new("openssl").arg("version").output().is_ok()
}

#[tokio::test]
async fn g2_runtime_pin_and_g3_local_https() {
    let base = std::env::temp_dir().join(format!("herd-g2g3-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let web = base.join("web");
    fs::create_dir_all(&web).unwrap();
    fs::write(web.join("package.json"), "{}").unwrap();

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

    // G2: runtimes.list reports the real runtimes on this machine.
    let listed = client.ok("runtimes.list", json!({})).await;
    let runtimes = listed["runtimes"].as_array().unwrap();
    assert!(!runtimes.is_empty(), "no runtimes detected at all");
    let by_name = |name: &str| runtimes.iter().find(|r| r["name"] == name).cloned();
    // node and python3 are detected on this machine; assert only that
    // the reported versions look like real versions, not a fixed
    // release, so the test passes on any machine. Rust is only
    // present when ~/.cargo/bin is on PATH, so it is not asserted here.
    let node = by_name("node").expect("node should be detected");
    assert!(looks_like_version(node["version"].as_str().unwrap()), "{}", node["version"]);
    let python = by_name("python").expect("python should be detected");
    assert!(looks_like_version(python["version"].as_str().unwrap()), "{}", python["version"]);
    let node_version = node["version"].as_str().unwrap().to_string();

    // Pinning requires a known project: an unscanned path is refused.
    assert_eq!(
        client
            .err_code(
                "projects.set_runtime",
                json!({"path": web, "runtime": "node", "version": node_version})
            )
            .await,
        -32602
    );
    client.ok("projects.link", json!({"path": web, "domain": "myapp"})).await;

    // Unknown runtime, wrong version, and uninstalled runtime are -32602.
    assert_eq!(
        client.err_code("projects.set_runtime", json!({"path": web, "runtime": "zig"})).await,
        -32602
    );
    assert_eq!(
        client
            .err_code(
                "projects.set_runtime",
                json!({"path": web, "runtime": "node", "version": "0.0.1"})
            )
            .await,
        -32602
    );
    let pinned = client
        .ok(
            "projects.set_runtime",
            json!({"path": web, "runtime": "node", "version": node_version}),
        )
        .await;
    assert_eq!(pinned["runtime"], "node");
    assert_eq!(pinned["version"], node_version);

    // The pin shows up in projects.list.
    let listed = client.ok("projects.list", json!({})).await;
    let projects = listed["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["runtime"]["runtime"], "node");
    assert_eq!(projects[0]["runtime"]["version"], node_version);
    // Re-pinning without a version keeps the pin but drops the version.
    let pinned = client.ok("projects.set_runtime", json!({"path": web, "runtime": "python"})).await;
    assert_eq!(pinned["runtime"], "python");
    assert!(pinned["version"].is_null());
    assert_eq!(
        client.err_code("projects.clear_runtime", json!({"path": base.join("nope")})).await,
        -32602
    );
    assert_eq!(client.ok("projects.clear_runtime", json!({"path": web})).await["pinned"], false);
    let listed = client.ok("projects.list", json!({})).await;
    assert!(listed["projects"][0]["runtime"].is_null());
    assert_eq!(client.err_code("projects.clear_runtime", json!({"path": web})).await, -32602);

    // runtimes.install: only rust via rustup is attempted; anything
    // else fails with -32010 and a manual-install hint.
    assert_eq!(client.err_code("runtimes.install", json!({"runtime": "node"})).await, -32010);
    assert_eq!(client.err_code("runtimes.install", json!({"runtime": "zig"})).await, -32010);

    // G3: domains.list shows the linked domain as not secured.
    let listed = client.ok("domains.list", json!({})).await;
    let domains = listed["domains"].as_array().unwrap();
    assert_eq!(domains.len(), 1);
    assert_eq!(domains[0]["domain"], "myapp.test");
    assert_eq!(domains[0]["secured"], false);
    assert!(domains[0]["expires_at"].is_null());

    // Securing an unknown or malformed domain is -32602.
    assert_eq!(client.err_code("domains.secure", json!({"domain": "ghost.test"})).await, -32602);
    assert_eq!(client.err_code("domains.secure", json!({"domain": "bad_domain"})).await, -32602);
    assert_eq!(client.err_code("domains.unsecure", json!({"domain": "myapp.test"})).await, -32602);

    if !openssl_present() {
        // Without openssl, securing must fail clearly, not silently.
        assert_eq!(
            client.err_code("domains.secure", json!({"domain": "myapp.test"})).await,
            -32010
        );
        fs::remove_dir_all(&base).unwrap();
        return;
    }

    let secured = client.ok("domains.secure", json!({"domain": "myapp.test"})).await;
    assert_eq!(secured["secured"], true);
    assert_eq!(secured["issuer"], "Herd Local CA");
    let expires_at = secured["expires_at"].as_u64().unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // 825 days from now, within a small tolerance.
    assert!(expires_at > now + 825 * 86400 - 300, "{expires_at} vs {now}");
    assert!(expires_at <= now + 825 * 86400 + 300, "{expires_at} vs {now}");

    // Cert files live in the config dir, keys restricted to 0600.
    let certs = config_dir.join("certs");
    assert!(certs.join("ca.crt").is_file());
    assert!(certs.join("ca.key").is_file());
    assert!(certs.join("myapp.test.crt").is_file());
    assert!(certs.join("myapp.test.key").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(certs.join("myapp.test.key")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // domains.list now reports the domain as secured with an expiry.
    let listed = client.ok("domains.list", json!({})).await;
    let domains = listed["domains"].as_array().unwrap();
    assert_eq!(domains[0]["secured"], true);
    assert_eq!(domains[0]["expires_at"], expires_at);
    assert_eq!(domains[0]["issuer"], "Herd Local CA");

    // Securing again regenerates (same record, new expiry window) and
    // survives a state reload from disk.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let secured2 = client.ok("domains.secure", json!({"domain": "myapp.test"})).await;
    assert!(secured2["expires_at"].as_u64().unwrap() >= expires_at);
    // After the second secure, key and certificate still match: the
    // re-secure generated the pair in temp files and installed it
    // whole, instead of overwriting the live key before signing.
    let key_modulus = std::process::Command::new("openssl")
        .args(["rsa", "-in", certs.join("myapp.test.key").to_str().unwrap(), "-noout", "-modulus"])
        .output()
        .unwrap();
    let crt_modulus = std::process::Command::new("openssl")
        .args(["x509", "-in", certs.join("myapp.test.crt").to_str().unwrap(), "-noout", "-modulus"])
        .output()
        .unwrap();
    assert!(key_modulus.status.success(), "{}", String::from_utf8_lossy(&key_modulus.stderr));
    assert_eq!(key_modulus.stdout, crt_modulus.stdout);
    let reloaded = DaemonState::load(&config_dir).unwrap();
    drop(reloaded);
    let text = fs::read_to_string(config_dir.join("state.json")).unwrap();
    assert!(text.contains("myapp.test"));
    assert!(text.contains("Herd Local CA"));

    // Unsecure removes the record and the files.
    assert_eq!(
        client.ok("domains.unsecure", json!({"domain": "myapp.test"})).await["secured"],
        false
    );
    assert!(!certs.join("myapp.test.crt").exists());
    assert!(!certs.join("myapp.test.key").exists());
    let listed = client.ok("domains.list", json!({})).await;
    assert_eq!(listed["domains"][0]["secured"], false);
    assert_eq!(client.err_code("domains.unsecure", json!({"domain": "myapp.test"})).await, -32602);

    client.ok("projects.unlink", json!({"path": web})).await;
    fs::remove_dir_all(&base).unwrap();
}

/// A pin can be cleared after its project directory is deleted:
/// clear_runtime resolves the stored path instead of requiring the
/// directory to exist.
#[tokio::test]
async fn clear_runtime_after_directory_deleted() {
    let base = std::env::temp_dir().join(format!("herd-clear-pin-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let web = base.join("web");
    fs::create_dir_all(&web).unwrap();
    fs::write(web.join("package.json"), "{}").unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    client.ok("projects.link", json!({"path": web, "domain": "gone"})).await;
    let listed = client.ok("runtimes.list", json!({})).await;
    let node_version = listed["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "node")
        .expect("node should be detected")["version"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .ok(
            "projects.set_runtime",
            json!({"path": web, "runtime": "node", "version": node_version}),
        )
        .await;
    let listed = client.ok("projects.list", json!({})).await;
    assert_eq!(listed["projects"][0]["runtime"]["runtime"], "node");

    fs::remove_dir_all(&web).unwrap();
    assert_eq!(client.ok("projects.clear_runtime", json!({"path": web})).await["pinned"], false);
    let listed = client.ok("projects.list", json!({})).await;
    assert!(listed["projects"][0]["runtime"].is_null());
    // Clearing again now fails: no pin left.
    assert_eq!(client.err_code("projects.clear_runtime", json!({"path": web})).await, -32602);
    fs::remove_dir_all(&base).unwrap();
}

/// A failed save must not leave the pin in memory: with the config
/// dir deleted, set_runtime fails and projects.list shows no pin;
/// once the dir is back, pinning works again.
#[tokio::test]
async fn failed_save_does_not_leave_pin_in_memory() {
    let base = std::env::temp_dir().join(format!("herd-save-fail-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let web = base.join("web");
    fs::create_dir_all(&web).unwrap();
    fs::write(web.join("package.json"), "{}").unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    client.ok("projects.link", json!({"path": web, "domain": "gone"})).await;
    // Deleting the config dir makes save fail (no place for the temp
    // file), so the mutation must not stick in memory either.
    fs::remove_dir_all(&config_dir).unwrap();
    assert_eq!(
        client.err_code("projects.set_runtime", json!({"path": web, "runtime": "node"})).await,
        -32010
    );
    let listed = client.ok("projects.list", json!({})).await;
    assert!(listed["projects"][0]["runtime"].is_null());
    fs::create_dir_all(&config_dir).unwrap();
    client.ok("projects.set_runtime", json!({"path": web, "runtime": "node"})).await;
    let listed = client.ok("projects.list", json!({})).await;
    assert_eq!(listed["projects"][0]["runtime"]["runtime"], "node");
    fs::remove_dir_all(&base).unwrap();
}

/// When the certificate files cannot be deleted, unsecure fails and
/// the record stays, so the operation can be retried.
#[tokio::test]
async fn unsecure_failure_keeps_the_record() {
    if !openssl_present() {
        return;
    }
    let base = std::env::temp_dir().join(format!("herd-unsecure-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();
    let web = base.join("web");
    fs::create_dir_all(&web).unwrap();
    fs::write(web.join("package.json"), "{}").unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    client.ok("projects.link", json!({"path": web, "domain": "myapp"})).await;
    client.ok("domains.secure", json!({"domain": "myapp.test"})).await;
    // Replace the key file with a directory: remove_file on it fails,
    // which is how an undeletable file is simulated.
    let certs = config_dir.join("certs");
    fs::remove_file(certs.join("myapp.test.key")).unwrap();
    fs::create_dir(certs.join("myapp.test.key")).unwrap();
    assert_eq!(client.err_code("domains.unsecure", json!({"domain": "myapp.test"})).await, -32010);
    // The record is still in the state file...
    let text = fs::read_to_string(config_dir.join("state.json")).unwrap();
    assert!(text.contains("myapp.test"));
    // ...and the retry works once the blocker is gone.
    fs::remove_dir(certs.join("myapp.test.key")).unwrap();
    assert_eq!(
        client.ok("domains.unsecure", json!({"domain": "myapp.test"})).await["secured"],
        false
    );
    fs::remove_dir_all(&base).unwrap();
}
