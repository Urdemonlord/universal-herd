//! Supervision of managed service processes.
//!
//! A service definition lives in the persistent state file; a running
//! process lives only in memory (PID, start time, exit code). Nothing
//! about a running process is written to `state.json`, so a daemon
//! restart never resurrects a service: services must be started again
//! explicitly. That is deliberate: no auto-start until the daemon
//! itself can run as an OS user service (IPD D-7).
//!
//! On Unix each child is spawned as its own process-group leader, so
//! `stop` can signal the whole group (child + grandchildren) instead
//! of only the direct child.

use crate::state::ServiceDef;
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime},
};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    time::timeout,
};

const STOP_GRACE: Duration = Duration::from_secs(3);

struct RunningEntry {
    child: Child,
    pid: u32,
    started_at: SystemTime,
}

#[derive(Debug, Clone)]
pub struct ExitedInfo {
    pub code: Option<i32>,
    pub at: SystemTime,
    /// True when the exit followed an explicit `stop` by the supervisor
    /// (SIGTERM/SIGKILL from us). Such an exit is "stopped", not "failed",
    /// even though a signal kill reports no exit code.
    pub stopped: bool,
}

pub struct Supervisor {
    running: HashMap<String, RunningEntry>,
    exited: HashMap<String, ExitedInfo>,
    logs_dir: PathBuf,
}

impl Supervisor {
    pub fn new(logs_dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&logs_dir)?;
        Ok(Self { running: HashMap::new(), exited: HashMap::new(), logs_dir })
    }

    pub fn log_path(&self, name: &str) -> PathBuf {
        self.logs_dir.join(format!("{name}.log"))
    }

    /// Find `command` in PATH (or verify it directly when it contains a
    /// path separator). Returns `None` when the binary cannot be executed.
    pub fn resolve_binary(command: &str) -> Option<PathBuf> {
        if command.contains('/') || command.contains('\\') {
            let path = PathBuf::from(command);
            return is_executable(&path).then_some(path);
        }
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(command);
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Move freshly exited children out of `running` into `exited`.
    /// Call before reading any status so a crashed service shows up as
    /// `failed` instead of `running` forever.
    pub fn reap(&mut self) {
        let mut done = Vec::new();
        for (name, entry) in self.running.iter_mut() {
            match entry.child.try_wait() {
                Ok(Some(status)) => done.push((name.clone(), status.code())),
                Ok(None) => {}
                Err(_) => done.push((name.clone(), None)),
            }
        }
        for (name, code) in done {
            self.running.remove(&name);
            self.exited.insert(name, ExitedInfo { code, at: SystemTime::now(), stopped: false });
        }
    }

    pub fn is_running(&mut self, name: &str) -> bool {
        self.reap();
        self.running.contains_key(name)
    }

    pub fn pid(&mut self, name: &str) -> Option<u32> {
        self.reap();
        self.running.get(name).map(|entry| entry.pid)
    }

    pub fn started_at(&mut self, name: &str) -> Option<SystemTime> {
        self.reap();
        self.running.get(name).map(|entry| entry.started_at)
    }

    pub fn status(&mut self, name: &str) -> &'static str {
        self.reap();
        if self.running.contains_key(name) {
            return "running";
        }
        match self.exited.get(name) {
            Some(info) if info.stopped => "stopped",
            Some(info) if info.code != Some(0) => "failed",
            _ => "stopped",
        }
    }

    pub fn last_exit_code(&self, name: &str) -> Option<i32> {
        self.exited.get(name).and_then(|info| info.code)
    }

    /// Spawn a service. Fails clearly when the binary is not in PATH,
    /// and refuses to start a service that is already running.
    pub async fn start(&mut self, def: &ServiceDef) -> Result<u32, String> {
        self.reap();
        if self.running.contains_key(&def.name) {
            return Err(format!("service {:?} is already running", def.name));
        }
        let binary = Self::resolve_binary(&def.command).ok_or_else(|| {
            format!(
                "service binary {:?} not found in PATH; install it or create the service with an explicit command path",
                def.command
            )
        })?;
        let log_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(&def.name))
            .map_err(|error| format!("cannot open log file: {error}"))?;
        let stdout = Stdio::from(
            log_file.try_clone().map_err(|error| format!("cannot open log file: {error}"))?,
        );
        let stderr = Stdio::from(log_file);
        let mut command = Command::new(binary);
        command
            .args(&def.args)
            .stdout(stdout)
            .stderr(stderr)
            .stdin(Stdio::null())
            .kill_on_drop(false);
        if let Some(workdir) = &def.workdir {
            command.current_dir(workdir);
        }
        #[cfg(unix)]
        {
            // tokio's Command already exposes process_group on Unix.
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot start service {:?}: {error}", def.name))?;
        let pid = child.id().ok_or_else(|| format!("service {:?} has no PID", def.name))?;
        // If the process exits immediately (bad flags, missing config),
        // surface that instead of reporting a successful start.
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(Some(status)) = child.try_wait() {
            // Reap before dropping the Child so no zombie is left behind.
            let _ = child.wait().await;
            self.exited.insert(
                def.name.clone(),
                ExitedInfo { code: status.code(), at: SystemTime::now(), stopped: false },
            );
            return Err(format!(
                "service {:?} exited immediately with {}",
                def.name,
                status.code().map_or("unknown status".to_string(), |code| format!("code {code}"))
            ));
        }
        self.running
            .insert(def.name.clone(), RunningEntry { child, pid, started_at: SystemTime::now() });
        Ok(pid)
    }

    /// Stop a running service: SIGTERM to the whole process group, a
    /// grace period, then SIGKILL, then a final kill of the direct child.
    /// Returns the process exit code when known.
    pub async fn stop(&mut self, name: &str) -> Result<Option<i32>, String> {
        self.reap();
        let mut entry = match self.running.remove(name) {
            Some(entry) => entry,
            None => return Err(format!("service {name:?} is not running")),
        };
        #[cfg(unix)]
        {
            signal_group(entry.pid, libc::SIGTERM);
            let deadline = tokio::time::Instant::now() + STOP_GRACE;
            loop {
                match entry.child.try_wait() {
                    Ok(Some(status)) => {
                        self.exited.insert(
                            name.to_string(),
                            ExitedInfo {
                                code: status.code(),
                                at: SystemTime::now(),
                                stopped: true,
                            },
                        );
                        return Ok(status.code());
                    }
                    Ok(None) => {
                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    Err(_) => break,
                }
            }
            signal_group(entry.pid, libc::SIGKILL);
        }
        let _ = entry.child.kill().await;
        match entry.child.wait().await {
            Ok(status) => {
                let code = status.code();
                self.exited.insert(
                    name.to_string(),
                    ExitedInfo { code, at: SystemTime::now(), stopped: true },
                );
                Ok(code)
            }
            Err(error) => Err(format!("cannot stop service {name:?}: {error}")),
        }
    }

    /// Health: when the service declares a port, a TCP connect to
    /// 127.0.0.1:port must succeed; otherwise "alive" is healthy.
    /// Returns `None` when the service is not running.
    pub async fn healthy(&mut self, def: &ServiceDef) -> Option<bool> {
        if !self.is_running(&def.name) {
            return None;
        }
        match def.port {
            Some(port) => {
                let target = format!("127.0.0.1:{port}");
                // Both layers must succeed: a refused connect is Ok(Err),
                // not an error of the timeout itself.
                Some(matches!(
                    timeout(Duration::from_millis(500), TcpStream::connect(target)).await,
                    Ok(Ok(_))
                ))
            }
            None => Some(true),
        }
    }

    /// Last `lines` lines of the service log file (empty when the service
    /// never wrote anything). `lines` is clamped to 1..=1000 by the caller.
    pub fn logs(&self, name: &str, lines: usize) -> io::Result<Vec<String>> {
        let path = self.log_path(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let all: Vec<&str> = text.lines().collect();
        let skip = all.len().saturating_sub(lines);
        Ok(all[skip..].iter().map(|line| (*line).to_string()).collect())
    }
}

#[cfg(unix)]
fn signal_group(pgid: u32, signal: i32) {
    // Signal the whole process group with the kill syscall directly.
    // Do NOT shell out to the `kill` binary: it silently fails for
    // negative PIDs (process groups) in some sandboxed environments
    // while still exiting 0, which used to leak grandchildren. The
    // direct-child kill in `stop` remains the fallback when the group
    // is already gone.
    //
    // SAFETY: `kill` with a negative pid targets a process group; both
    // arguments are plain integers, no pointers are involved.
    unsafe {
        libc::kill(-(pgid as libc::pid_t), signal);
    }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ServiceDef;

    fn tmp_logs(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herd-sup-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn def(name: &str, command: &str, args: Vec<&str>, port: Option<u16>) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            command: command.into(),
            args: args.into_iter().map(str::to_string).collect(),
            port,
            workdir: None,
            kind: None,
        }
    }

    #[test]
    fn resolve_binary_finds_path_entries_and_rejects_missing() {
        assert!(Supervisor::resolve_binary("sh").is_some());
        assert!(Supervisor::resolve_binary("herd-definitely-not-a-binary-xyz").is_none());
        assert!(Supervisor::resolve_binary("/bin/sh").is_some());
        assert!(Supervisor::resolve_binary("/bin/no-such-binary-xyz").is_none());
    }

    #[tokio::test]
    async fn start_missing_binary_fails_clearly() {
        let dir = tmp_logs("missing");
        let mut sup = Supervisor::new(dir.clone()).unwrap();
        let error = sup
            .start(&def("ghost", "herd-definitely-not-a-binary-xyz", vec![], None))
            .await
            .unwrap_err();
        assert!(error.contains("not found in PATH"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn start_stop_sleep_and_read_logs() {
        let dir = tmp_logs("sleep");
        let mut sup = Supervisor::new(dir.clone()).unwrap();
        // The shell backgrounds a `sleep` grandchild and records its PID,
        // so the test can prove `stop` kills the whole process group,
        // not just the direct child. (Regression test: an earlier version
        // shelled out to the `kill` binary, which silently failed for
        // negative PIDs in this sandbox and leaked the grandchild.)
        let pid_file = dir.join("grandchild.pid");
        let script =
            format!("echo herd-log-line; sleep 300 & echo $! > {}; wait", pid_file.display());
        let service = def("sleeper", "sh", vec!["-c", &script], None);
        let pid = sup.start(&service).await.unwrap();
        assert!(pid > 0);
        assert_eq!(sup.status("sleeper"), "running");
        assert!(sup.start(&service).await.is_err(), "double start must fail");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let logs = sup.logs("sleeper", 10).unwrap();
        assert!(logs.iter().any(|line| line.contains("herd-log-line")), "{logs:?}");
        // Wait for the grandchild PID file to appear.
        let mut grandchild = None;
        for _ in 0..20 {
            if let Ok(text) = fs::read_to_string(&pid_file) {
                grandchild = text.trim().parse::<i32>().ok();
                if grandchild.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let grandchild = grandchild.expect("grandchild pid file never appeared");
        let code = sup.stop("sleeper").await.unwrap();
        assert_eq!(sup.status("sleeper"), "stopped");
        // Killed by SIGTERM, so there is no exit code by design.
        assert!(code.is_none());
        // The grandchild must be gone too: kill(pid, 0) fails with ESRCH.
        let probe = unsafe { libc::kill(grandchild, 0) };
        assert_eq!(probe, -1, "grandchild {grandchild} survived stop");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert!(sup.stop("sleeper").await.is_err(), "double stop must fail");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn health_checks_port_when_declared() {
        let dir = tmp_logs("health");
        let mut sup = Supervisor::new(dir.clone()).unwrap();
        // A listener on a free port, then a service bound to nothing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let with_port = def("web", "sleep", vec!["300"], Some(port));
        sup.start(&with_port).await.unwrap();
        assert_eq!(sup.healthy(&with_port).await, Some(true));
        drop(listener);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(sup.healthy(&with_port).await, Some(false));
        sup.stop("web").await.unwrap();
        assert_eq!(sup.healthy(&with_port).await, None);
        fs::remove_dir_all(dir).unwrap();
    }
}
