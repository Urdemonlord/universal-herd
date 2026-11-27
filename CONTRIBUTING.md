# Contributing

Thanks for your interest in Universal Herd.

- Keep core logic independent from UI code.
- Do not add platform-specific behavior to shared crates.
- Add tests for protocol and project discovery changes.
- Treat plugin execution and local daemon access as security-sensitive.
- Do not introduce network listeners on non-loopback interfaces without an explicit security design.
- Run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` before submitting Rust changes.

For larger changes, open an issue describing the problem, proposed API, compatibility impact, and security implications.
