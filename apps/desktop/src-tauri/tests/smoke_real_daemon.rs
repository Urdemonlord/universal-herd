//! End-to-end smoke test: the Tauri command functions against a real
//! daemon process spawned by `start_daemon`.
//!
//! Run with: `cargo test --test smoke_real_daemon`
//!
//! The test is self-contained and cleans up after itself: it parks and
//! links one throwaway project under `target/test-workspace/demo-app`,
//! creates one `sleep` service, then unlinks, unparks, removes the
//! service, and kills the daemon it started. The daemon must NOT be
//! running before this test starts; the test fails loudly if it is.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use universal_herd_desktop::client;
use universal_herd_desktop::commands::{
    daemon_status, link_project, list_domains, list_projects, list_runtimes, list_services,
    park_project, scan_projects, start_daemon, start_service, stop_service,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn projects_of(value: &Value) -> Vec<Value> {
    value["projects"].as_array().cloned().unwrap_or_default()
}

async fn wait_port_free() {
    for _ in 0..25 {
        if tokio::net::TcpStream::connect("127.0.0.1:47831").await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("port 47831 still listening after killing the daemon");
}

#[tokio::test]
async fn desktop_commands_against_real_daemon() {
    // No daemon may be running when this starts.
    assert!(
        daemon_status().await.is_err(),
        "a daemon is already running on 127.0.0.1:47831; stop it before running this test"
    );

    // Throwaway Rust project for scan/park/link.
    let demo = repo_root().join("target").join("test-workspace").join("demo-app");
    std::fs::create_dir_all(&demo).unwrap();
    std::fs::write(demo.join("Cargo.toml"), "[package]\nname = \"demo-app\"\n").unwrap();
    let demo_str = demo.to_string_lossy().to_string();

    // Best-effort cleanup of leftovers from an earlier aborted run.
    let _ = client::call("projects.unlink", json!({"path": demo_str})).await;
    let _ = client::call("projects.unpark", json!({"path": demo_str})).await;
    let _ = client::call("services.remove", json!({"name": "smoke-sleep"})).await;

    // Start the daemon through the Tauri command under test.
    let started = start_daemon().await.expect("start_daemon failed");
    assert_eq!(started["started"], true, "{started}");
    let pid = started["pid"].as_u64().expect("start_daemon returned no pid");
    let status = daemon_status().await.expect("daemon did not answer after start");
    assert_eq!(status["protocol"], "1.2");
    assert_eq!(status["status"], "running");

    // Scan finds the throwaway project. A bare scan does not put the
    // project in projects.list: the daemon only lists projects under
    // parked paths plus linked projects, so park it first.
    let scanned = scan_projects(demo_str.clone()).await.expect("scan failed");
    let found = projects_of(&scanned);
    assert!(found.iter().any(|p| p["name"] == "demo-app" && p["kind"] == "Rust"), "{scanned}");

    park_project(demo_str.clone()).await.expect("park failed");
    let listed = list_projects().await.expect("list failed");
    let entry = projects_of(&listed)
        .into_iter()
        .find(|p| p["name"] == "demo-app")
        .expect("demo-app missing from projects.list after park");
    assert_eq!(entry["source"], "Parked");

    link_project(demo_str.clone(), "demo-app".to_string()).await.expect("link failed");
    let listed = list_projects().await.expect("list failed");
    let entry = projects_of(&listed)
        .into_iter()
        .find(|p| p["name"] == "demo-app")
        .expect("demo-app missing after link");
    assert_eq!(entry["domain"], "demo-app.test");

    // Domains list shows the linked domain, unsecured.
    let domains = list_domains().await.expect("domains list failed");
    let domains = domains["domains"].as_array().cloned().unwrap_or_default();
    let entry = domains
        .iter()
        .find(|d| d["domain"] == "demo-app.test")
        .expect("demo-app.test missing from domains.list");
    assert_eq!(entry["secured"], false);

    // Runtimes: the sandbox has real ones (node, python, rust).
    let runtimes = list_runtimes().await.expect("runtimes list failed");
    let runtimes = runtimes["runtimes"].as_array().cloned().unwrap_or_default();
    assert!(!runtimes.is_empty(), "no runtimes detected, expected at least one");
    assert!(
        runtimes.iter().any(|r| r["name"] == "rust" || r["name"] == "node"),
        "expected rust or node in {runtimes:?}"
    );

    // Service lifecycle through the Tauri commands (create/remove go
    // through the client directly; the desktop has no create UI yet).
    client::call(
        "services.create",
        json!({"name": "smoke-sleep", "command": "sleep", "args": ["120"]}),
    )
    .await
    .expect("services.create failed");
    start_service("smoke-sleep".to_string()).await.expect("service start failed");
    let services = list_services().await.expect("services list failed");
    let services = services["services"].as_array().cloned().unwrap_or_default();
    let entry = services
        .iter()
        .find(|s| s["name"] == "smoke-sleep")
        .expect("smoke-sleep missing from services.list");
    assert_eq!(entry["status"], "running", "{entry}");
    assert!(entry["pid"].as_u64().is_some(), "{entry}");

    stop_service("smoke-sleep".to_string()).await.expect("service stop failed");
    let services = list_services().await.expect("services list failed");
    let services = services["services"].as_array().cloned().unwrap_or_default();
    let entry = services
        .iter()
        .find(|s| s["name"] == "smoke-sleep")
        .expect("smoke-sleep missing after stop");
    assert_eq!(entry["status"], "stopped", "{entry}");

    // Cleanup: leave no trace in daemon state.
    client::call("services.remove", json!({"name": "smoke-sleep"}))
        .await
        .expect("services.remove failed");
    client::call("projects.unlink", json!({"path": demo_str})).await.expect("unlink failed");
    client::call("projects.unpark", json!({"path": demo_str})).await.expect("unpark failed");
    let listed = list_projects().await.expect("final list failed");
    assert!(
        !projects_of(&listed).iter().any(|p| p["name"] == "demo-app"),
        "demo-app still listed after unpark: {listed}"
    );

    // Kill the daemon we started and prove the port is free.
    let killed = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .expect("failed to run kill");
    assert!(killed.success(), "kill {pid} failed");
    wait_port_free().await;
    assert!(daemon_status().await.is_err(), "daemon still answers after kill");
}
