//! Runtime detection and install delegation (G2).
//!
//! Detection runs the real version command of each known binary
//! (`node --version`, `python3 --version`, ...) and parses the first
//! semver-looking token out of its output. Nothing is installed by
//! detection, and no project code is executed: only version flags.
//!
//! Install is deliberately small and honest: the daemon is not a
//! version manager. The only install it performs is delegating Rust to
//! `rustup` when that manager is already present. Everything else
//! fails with -32010 and a message that says how to install the
//! runtime manually. See `docs/daemon-g2-g3.md`.

use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    time::Duration,
};

/// A runtime the daemon knows how to detect. `version_args` is the
/// argument list passed to the binary to make it print its version.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeSpec {
    pub name: &'static str,
    pub display: &'static str,
    pub binary: &'static str,
    pub version_args: &'static [&'static str],
}

pub const KNOWN_RUNTIMES: &[RuntimeSpec] = &[
    RuntimeSpec { name: "node", display: "Node.js", binary: "node", version_args: &["--version"] },
    RuntimeSpec { name: "npm", display: "npm", binary: "npm", version_args: &["--version"] },
    RuntimeSpec {
        name: "python",
        display: "Python",
        binary: "python3",
        version_args: &["--version"],
    },
    RuntimeSpec { name: "rust", display: "Rust", binary: "rustc", version_args: &["--version"] },
    RuntimeSpec { name: "cargo", display: "Cargo", binary: "cargo", version_args: &["--version"] },
    RuntimeSpec { name: "go", display: "Go", binary: "go", version_args: &["version"] },
    RuntimeSpec { name: "php", display: "PHP", binary: "php", version_args: &["--version"] },
    RuntimeSpec { name: "ruby", display: "Ruby", binary: "ruby", version_args: &["--version"] },
    RuntimeSpec {
        name: "composer",
        display: "Composer",
        binary: "composer",
        version_args: &["--version"],
    },
];

pub fn known_runtime(name: &str) -> Option<&'static RuntimeSpec> {
    KNOWN_RUNTIMES.iter().find(|spec| spec.name == name)
}

pub fn known_runtime_names() -> Vec<&'static str> {
    KNOWN_RUNTIMES.iter().map(|spec| spec.name).collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeInfo {
    pub name: String,
    pub display: String,
    pub binary: String,
    pub version: String,
}

/// Find an executable program by name in the given directories, in
/// order. Returns the first match that is a file.
pub fn find_program(name: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in search_dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Directories of the current `PATH`, in order.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default()
}

/// Extract the first semver-looking token (`major.minor` or
/// `major.minor.patch`) from a version command's output.
///
/// Handles the usual shapes: `v24.22.1`, `Python 3.12.3`,
/// `rustc 1.100.0 (...)`, `go version go1.22.5 linux/amd64`,
/// `Composer version 2.7.1 ...`. Returns `None` when nothing parses.
pub fn extract_version(output: &str) -> Option<String> {
    for raw in output.split_whitespace() {
        let token = raw.trim_matches(|c: char| c == ',' || c == ';' || c == ')' || c == '(');
        let token = token.strip_prefix('v').unwrap_or(token);
        let token = token.strip_prefix("go").unwrap_or(token);
        let parts: Vec<&str> = token.split('.').collect();
        if (parts.len() == 2 || parts.len() == 3)
            && parts.iter().all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        {
            return Some(token.to_string());
        }
    }
    None
}

fn run_version(spec: &RuntimeSpec, binary: &Path) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    let binary = binary.to_path_buf();
    let args: Vec<String> = spec.version_args.iter().map(|a| a.to_string()).collect();
    std::thread::spawn(move || {
        let output = Command::new(&binary).args(&args).output();
        let _ = tx.send(output);
    });
    let output = rx.recv_timeout(Duration::from_secs(5)).ok()?.ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stderr).to_string();
    }
    extract_version(&text)
}

/// Detect installed runtimes by running each known binary's version
/// command. Only runtimes that are actually present are reported; a
/// missing or misbehaving binary is skipped silently.
pub fn detect_runtimes_in(search_dirs: &[PathBuf]) -> Vec<RuntimeInfo> {
    let mut found = Vec::new();
    for spec in KNOWN_RUNTIMES {
        let Some(binary) = find_program(spec.binary, search_dirs) else { continue };
        let Some(version) = run_version(spec, &binary) else { continue };
        found.push(RuntimeInfo {
            name: spec.name.to_string(),
            display: spec.display.to_string(),
            binary: spec.binary.to_string(),
            version,
        });
    }
    found
}

pub fn detect_runtimes() -> Vec<RuntimeInfo> {
    detect_runtimes_in(&path_dirs())
}

/// What `runtimes.install` decided to do, before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAction {
    /// Delegate to rustup with this toolchain name.
    ViaRustup { toolchain: String },
}

/// Pure decision step of `runtimes.install`: no command is run here.
/// Returns `Err(message)` for everything the daemon refuses to do,
/// with the message written for the -32010 error body.
pub fn install_plan(
    runtime: &str,
    version: Option<&str>,
    rustup_present: bool,
) -> Result<InstallAction, String> {
    let spec = known_runtime(runtime).ok_or_else(|| {
        format!("unknown runtime {runtime:?}; known runtimes: {}", known_runtime_names().join(", "))
    })?;
    if spec.name != "rust" {
        return Err(format!(
            "runtimes.install only supports rust (via rustup); install {} manually: {}",
            spec.display,
            manual_install_hint(spec.name)
        ));
    }
    if !rustup_present {
        return Err(
            "rustup is not installed; install it from https://rustup.rs, then retry `runtimes.install`"
                .to_string(),
        );
    }
    let toolchain = version.unwrap_or("stable");
    if toolchain != "stable"
        && toolchain != "nightly"
        && toolchain != "beta"
        && extract_version(toolchain).is_none()
    {
        return Err(format!(
            "invalid toolchain {toolchain:?}; use stable, nightly, beta, or a version like 1.85.0"
        ));
    }
    Ok(InstallAction::ViaRustup { toolchain: toolchain.to_string() })
}

/// Human install hint per runtime, used in the -32010 error when the
/// daemon cannot install it itself.
pub fn manual_install_hint(runtime: &str) -> &'static str {
    match runtime {
        "node" | "npm" => {
            "install Node.js from https://nodejs.org or via nvm (https://github.com/nvm-sh/nvm)"
        }
        "python" => {
            "use your OS package manager (e.g. apt install python3) or pyenv (https://github.com/pyenv/pyenv)"
        }
        "cargo" => "ships with Rust; install Rust via rustup (https://rustup.rs)",
        "go" => "install from https://go.dev/dl",
        "php" => "use your OS package manager (e.g. apt install php)",
        "ruby" => "use rbenv, rvm, or your OS package manager",
        "composer" => "install from https://getcomposer.org/download/",
        _ => "install it with your OS package manager",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fake_bin_dir(tag: &str, bins: &[(&str, &str)]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("herd-runtime-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, output) in bins {
            let path = dir.join(name);
            fs::write(&path, format!("#!/bin/sh\necho {output}\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    #[test]
    fn version_extraction_covers_real_outputs() {
        assert_eq!(extract_version("v24.22.1").as_deref(), Some("24.22.1"));
        assert_eq!(extract_version("10.9.4").as_deref(), Some("10.9.4"));
        assert_eq!(extract_version("Python 3.12.3").as_deref(), Some("3.12.3"));
        assert_eq!(extract_version("rustc 1.100.0 (abc 2025-01-01)").as_deref(), Some("1.100.0"));
        assert_eq!(extract_version("go version go1.22.5 linux/amd64").as_deref(), Some("1.22.5"));
        assert_eq!(
            extract_version("PHP 8.3.0 (cli) (built: Nov 23 2023)").as_deref(),
            Some("8.3.0")
        );
        assert_eq!(
            extract_version("ruby 3.3.0 (2023-12-25 revision 5124f9ac75) [x86_64-linux]")
                .as_deref(),
            Some("3.3.0")
        );
        assert_eq!(
            extract_version("Composer version 2.7.1 2024-02-09 14:31:34").as_deref(),
            Some("2.7.1")
        );
        assert_eq!(extract_version("nothing here").as_deref(), None);
        assert_eq!(extract_version("").as_deref(), None);
        assert_eq!(extract_version("version 1.2").as_deref(), Some("1.2"));
    }

    #[test]
    fn detection_runs_fake_binaries_and_skips_missing_ones() {
        let dir = fake_bin_dir(
            "detect",
            &[
                ("node", "v24.22.1"),
                ("python3", "Python 3.12.3"),
                ("go", "go version go1.22.5 linux/amd64"),
                ("php", "garbage with no version"),
            ],
        );
        let found = detect_runtimes_in(std::slice::from_ref(&dir));
        let by_name = |name: &str| found.iter().find(|r| r.name == name).cloned();
        assert_eq!(by_name("node").unwrap().version, "24.22.1");
        assert_eq!(by_name("python").unwrap().version, "3.12.3");
        assert_eq!(by_name("go").unwrap().version, "1.22.5");
        // php printed garbage: skipped. npm, rust, ruby, composer absent: skipped.
        assert!(by_name("php").is_none());
        assert!(by_name("npm").is_none());
        assert!(by_name("rust").is_none());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn install_plan_only_delegates_rust_to_rustup() {
        assert_eq!(
            install_plan("rust", Some("1.85.0"), true).unwrap(),
            InstallAction::ViaRustup { toolchain: "1.85.0".into() }
        );
        assert_eq!(
            install_plan("rust", None, true).unwrap(),
            InstallAction::ViaRustup { toolchain: "stable".into() }
        );
        assert_eq!(
            install_plan("rust", Some("nightly"), true).unwrap(),
            InstallAction::ViaRustup { toolchain: "nightly".into() }
        );
        // Rust without rustup: clear error, no attempt.
        let err = install_plan("rust", None, false).unwrap_err();
        assert!(err.contains("rustup is not installed"), "{err}");
        // Bad toolchain name.
        assert!(install_plan("rust", Some("banana"), true).is_err());
        // Everything else: unsupported, with a manual hint.
        let err = install_plan("node", Some("22"), true).unwrap_err();
        assert!(err.contains("only supports rust"), "{err}");
        assert!(err.contains("nodejs.org"), "{err}");
        assert!(install_plan("go", None, true).unwrap_err().contains("go.dev"));
        // Unknown runtime name.
        let err = install_plan("zig", None, true).unwrap_err();
        assert!(err.contains("unknown runtime"), "{err}");
    }
}
