//! Universal Herd desktop: Tauri shell connected to the Herd daemon.
//!
//! The desktop talks to the daemon over the same JSON-RPC NDJSON
//! protocol as the CLI (see `client`). Every command returns
//! `Result<Value, String>` so the frontend always gets a human
//! readable error instead of a stack trace.
//!
//! Commands live in the `commands` submodule, not at the crate root:
//! `#[tauri::command]` on a `pub` function emits the wrapper macro
//! with `#[macro_export]`, which collides with the textual definition
//! when the function sits at the crate root (E0255). One module down
//! the export and the definition land in different namespaces.

pub mod client;
pub mod commands;

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            commands::daemon_status,
            commands::start_daemon,
            commands::scan_projects,
            commands::list_projects,
            commands::list_runtimes,
            commands::list_services,
            commands::list_domains,
            commands::park_project,
            commands::link_project,
            commands::start_service,
            commands::stop_service
        ])
        .run(tauri::generate_context!())
        .expect("error while running Universal Herd desktop application");
}
