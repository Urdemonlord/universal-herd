# Universal Herd

**One environment. Infinite extensions.**

Universal Herd is an open, cross-platform local development environment. Its long-term architecture is built around a Rust core, a standalone daemon, a CLI, a Tauri desktop application, and an open plugin ecosystem.

> v0.1 is the project foundation. It includes a JSON-RPC daemon, a CLI client, basic project discovery, and an initial Svelte desktop shell. Runtime installation, service orchestration, domain management, and third-party plugins are planned but are not implemented yet.

## Workspace

```text
crates/
  herd-protocol/  Shared JSON-RPC request and response types
  herd-core/      Platform-independent project discovery
  herd-daemon/    Local daemon and authenticated loopback API
apps/
  cli/            `herd` command-line application
  desktop/        Tauri 2 + Svelte 5 desktop shell
```

## Requirements

- Rust stable (edition 2024)
- Node.js 22+
- npm
- Tauri 2 platform prerequisites for your operating system

See the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) before building the desktop app.

## Run the CLI

```bash
cargo run -p herd -- --help
cargo run -p herd -- daemon
```

In another terminal:

```bash
cargo run -p herd -- status
cargo run -p herd -- projects scan . --depth 3
```

The daemon binds to `127.0.0.1:47831`. A random token is created in the operating system's application config directory. On Unix, the token file is created with mode `0600`. Do not expose this endpoint beyond loopback.

## Run the desktop shell

```bash
cd apps/desktop/ui
npm install
npm run dev
```

For a full Tauri build, install the OS prerequisites and run the Tauri CLI from `apps/desktop/src-tauri`.

## Current limitations

- The desktop shell is currently a visual foundation and is not yet connected to the daemon.
- The daemon currently supports `system.status` and `projects.scan`.
- Project discovery is manifest-based and intentionally shallow by default.
- Runtime installation and service lifecycle operations are not implemented.
- The plugin manager, SDK, sandbox, and marketplace are architectural targets, not v0.1 features.
- Windows and macOS compilation must be verified in native CI runners; this environment does not include the Rust toolchain.

## License

MIT. See [LICENSE](LICENSE).
