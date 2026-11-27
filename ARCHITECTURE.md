# Architecture

## Principles

1. The Rust core is independent of the desktop UI.
2. The daemon is the single owner of machine-level state.
3. CLI and desktop clients communicate through a versioned local protocol.
4. Platform-specific operations belong behind platform adapters.
5. Plugin execution and permissions will be introduced only after core lifecycle contracts stabilize.

## v0.1 topology

```text
Desktop UI (Tauri/Svelte)      CLI (Rust)
          |                       |
          +---- JSON-RPC --------+
                      |
             Herd Daemon (Rust)
                      |
           +----------+----------+
           |                     |
     Herd Protocol          Herd Core
     request/response       project discovery
```

## Local control channel

The initial implementation uses newline-delimited JSON-RPC 2.0 over TCP bound exclusively to `127.0.0.1:47831`. Requests require a per-user random token stored in the OS application config directory. The daemon must never bind to `0.0.0.0`.

Initial methods:

- `system.status`
- `projects.scan`

The protocol is versioned separately from the application version. Future transports may replace TCP with Unix domain sockets and Windows named pipes without changing method semantics.

## Project discovery

The scanner identifies projects by common manifest files: `package.json`, `Cargo.toml`, `composer.json`, `pyproject.toml`, `go.mod`, and `Gemfile`. It skips generated and dependency directories. Scan depth is configurable and capped.

## Future plugin boundary

The plugin manager is deliberately not part of the first executable slice. The intended boundary is a capability-checked host API with WASM components for sandboxed extensions, sidecar processes for native integrations, and OCI containers for infrastructure services. Plugin API contracts must be versioned and documented before community plugins are considered stable.
