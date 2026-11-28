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

#[cfg(unix)]
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

    /// Stop a running service: SIGTERM to the whole process group,
    /// then wait for the *group* to empty (not just the direct child)
    /// through the grace period, then SIGKILL any remaining members.
    /// Returns the direct child's exit code when known. The fast path
    /// is preserved: when the group is already gone we return at once.
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
            let mut status = None;
            loop {
                if status.is_none() {
                    match entry.child.try_wait() {
                        Ok(Some(s)) => status = Some(s),
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
                if group_gone(entry.pid) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if !group_gone(entry.pid) {
                signal_group(entry.pid, libc::SIGKILL);
                let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
                while !group_gone(entry.pid) && tokio::time::Instant::now() < kill_deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            let status = match status {
                Some(status) => status,
                None => {
                    let _ = entry.child.kill().await;
                    match entry.child.wait().await {
                        Ok(status) => status,
                        Err(error) => {
                            return Err(format!("cannot stop service {name:?}: {error}"));
                        }
                    }
                }
            };
            let code = status.code();
            self.exited.insert(
                name.to_string(),
                ExitedInfo { code, at: SystemTime::now(), stopped: true },
            );
            Ok(code)
        }
        #[cfg(not(unix))]
        {
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
    /// Reads backwards from the end of the file in bounded chunks, so a
    /// gigabyte append-only log costs at most ~1 MiB of memory, and a
    /// single pathological line is truncated to 64 KiB.
    pub fn logs(&self, name: &str, lines: usize) -> io::Result<Vec<String>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.log_path(name);
        let mut file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let len = file.metadata()?.len();
        const CHUNK: usize = 8192;
        const MAX_TAIL: usize = 1024 * 1024;
        const MAX_LINE: usize = 64 * 1024;
        let mut buf: Vec<u8> = Vec::new();
        let mut newlines = 0usize;
        let mut pos = len;
        while pos > 0 && buf.len() < MAX_TAIL {
            let take = (pos as usize).min(CHUNK).min(MAX_TAIL - buf.len());
            pos -= take as u64;
            let mut chunk = vec![0u8; take];
            file.seek(SeekFrom::Start(pos))?;
            file.read_exact(&mut chunk)?;
            // Count only the new chunk: rescanning the whole buffer
            // every iteration is quadratic in the tail size.
            newlines += chunk.iter().filter(|b| **b == b'\n').count();
            chunk.extend_from_slice(&buf);
            buf = chunk;
            if newlines > lines {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let all: Vec<&str> = text.lines().collect();
        // When enough newlines were found, the first line in the buffer
        // is cut mid-line by the chunk boundary; `skip` below drops it,
        // so every returned line is complete. When the tail cap stopped
        // the scan early, the first line is a partial tail and is
        // returned truncated to MAX_LINE instead of being dropped.
        let skip = all.len().saturating_sub(lines);
        Ok(all[skip..].iter().map(|line| line.chars().take(MAX_LINE).collect()).collect())
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

/// True when no member of the process group remains: `kill(pgid, 0)`
/// fails once the last member (or zombie) is gone.
#[cfg(unix)]
fn group_gone(pgid: u32) -> bool {
    // SAFETY: signal 0 performs only an existence check; both
    // arguments are plain integers, no pointers are involved.
    unsafe { libc::kill(-(pgid as libc::pid_t), 0) != 0 }
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
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

    /// A command that sleeps for a while without doing anything:
    /// `sleep` on Unix, `ping.exe` on Windows (there is no `sleep`
    /// binary there, and `ping.exe` is the direct child, so `stop`
    /// kills it without leaving a grandchild behind).
    #[cfg(unix)]
    fn sleeper_command() -> (&'static str, Vec<&'static str>) {
        ("sleep", vec!["300"])
    }

    #[cfg(windows)]
    fn sleeper_command() -> (&'static str, Vec<&'static str>) {
        ("ping.exe", vec!["-n", "30", "127.0.0.1"])
    }

    #[test]
    #[cfg(unix)]
    fn resolve_binary_finds_path_entries_and_rejects_missing() {
        assert!(Supervisor::resolve_binary("sh").is_some());
        assert!(Supervisor::resolve_binary("herd-definitely-not-a-binary-xyz").is_none());
        assert!(Supervisor::resolve_binary("/bin/sh").is_some());
        assert!(Supervisor::resolve_binary("/bin/no-such-binary-xyz").is_none());
    }

    #[test]
    #[cfg(windows)]
    fn resolve_binary_finds_windows_executables() {
        assert!(Supervisor::resolve_binary("cmd.exe").is_some());
        assert!(Supervisor::resolve_binary("ping.exe").is_some());
        assert!(Supervisor::resolve_binary("herd-definitely-not-a-binary-xyz").is_none());
        assert!(Supervisor::resolve_binary("C:\\Windows\\System32\\cmd.exe").is_some());
        assert!(
            Supervisor::resolve_binary("C:\\Windows\\System32\\no-such-binary-xyz.exe").is_none()
        );
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
    #[cfg(unix)]
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
    #[cfg(unix)]
    async fn stop_kills_group_after_leader_exits() {
        let dir = tmp_logs("group-after-leader");
        let mut sup = Supervisor::new(dir.clone()).unwrap();
        // The leader (sh) has the default SIGTERM disposition and exits
        // on it, but the grandchild inherits an ignored SIGTERM via the
        // subshell trap, so it survives the group's SIGTERM. `stop`
        // must not return when the leader exits: it has to wait out the
        // grace period, SIGKILL the group, and only then return.
        // (Regression test: the old code returned as soon as the direct
        // child exited, leaking the grandchild.)
        let pid_file = dir.join("grandchild.pid");
        let script =
            format!("(trap '' TERM; exec sleep 300) & echo $! > {}; wait", pid_file.display());
        let service = def("ignorer", "sh", vec!["-c", &script], None);
        let pid = sup.start(&service).await.unwrap();
        assert!(pid > 0);
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
        let code = sup.stop("ignorer").await.unwrap();
        assert_eq!(sup.status("ignorer"), "stopped");
        // Killed by SIGKILL, so there is no exit code by design.
        assert!(code.is_none());
        let probe = unsafe { libc::kill(grandchild, 0) };
        assert_eq!(probe, -1, "grandchild {grandchild} survived stop");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        fs::remove_dir_all(dir).unwrap();
    }

    /// On Windows there is no process group to signal: `stop` kills
    /// the direct child only. A grandchild (a shell background job,
    /// for example) can survive; that limitation is documented in
    /// docs/daemon-g1-g4.md.
    #[tokio::test]
    #[cfg(windows)]
    async fn stop_kills_direct_child_on_windows() {
        let dir = tmp_logs("win-stop");
        let mut sup = Supervisor::new(dir.clone()).unwrap();
        let (cmd, args) = sleeper_command();
        let service = def("pinger", cmd, args, None);
        let pid = sup.start(&service).await.unwrap();
        assert!(pid > 0);
        assert_eq!(sup.status("pinger"), "running");
        let code = sup.stop("pinger").await.unwrap();
        assert_eq!(sup.status("pinger"), "stopped");
        // TerminateProcess reports exit code 1, unlike a Unix signal
        // kill which reports no code.
        assert_eq!(code, Some(1));
        assert!(sup.stop("pinger").await.is_err(), "double stop must fail");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn logs_reads_bounded_tail_not_the_whole_file() {
        let dir = tmp_logs("logs-tail");
        let sup = Supervisor::new(dir.clone()).unwrap();
        // Case 1: 100_000 short lines (~2 MB). The tail is found after
        // a few 8 KiB chunks, not by loading the whole file.
        let path = sup.log_path("big");
        {
            let mut file = fs::File::create(&path).unwrap();
            use std::io::Write;
            for i in 0..100_000 {
                writeln!(file, "line-{i:06}").unwrap();
            }
        }
        let lines = sup.logs("big", 10).unwrap();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], "line-099990");
        assert_eq!(lines[9], "line-099999");

        // Case 2: one 2 MB line with no newline, then "last-line". The
        // 1 MiB tail cap means the short lines before the giant line
        // are out of reach, so only complete lines inside the cap come
        // back: the giant line truncated to 64 KiB, then "last-line".
        {
            let mut file = fs::File::create(&path).unwrap();
            use std::io::Write;
            let giant = "g".repeat(2 * 1024 * 1024);
            file.write_all(giant.as_bytes()).unwrap();
            file.write_all(b"\nlast-line\n").unwrap();
        }
        let lines = sup.logs("big", 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), 64 * 1024, "giant line not truncated");
        assert!(lines[0].chars().all(|c| c == 'g'));
        assert_eq!(lines[1], "last-line");
        // Missing log file reads as empty, as before.
        assert!(sup.logs("never-existed", 10).unwrap().is_empty());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn health_checks_port_when_declared() {
        let dir = tmp_logs("health");
        let mut sup = Supervisor::new(dir.clone()).unwrap();
        // A listener on a free port, then a service bound to nothing.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sleeper_cmd, sleeper_args) = sleeper_command();
        let with_port = def("web", sleeper_cmd, sleeper_args, Some(port));
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
