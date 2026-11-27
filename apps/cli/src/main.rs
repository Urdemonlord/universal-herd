use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use herd_protocol::{Request, Response};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};

#[derive(Parser)]
#[command(name = "herd", version, about = "Universal Herd developer environment manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the background daemon in the foreground.
    Daemon,
    /// Show daemon status.
    Status,
    /// Discover projects under a directory.
    Projects {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage services supervised by the daemon.
    Services {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Detect runtimes installed on this machine and install new ones.
    Runtimes {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Manage local domains and their HTTPS certificates.
    Domains {
        #[command(subcommand)]
        command: DomainCommand,
    },
}

#[derive(Subcommand)]
enum ProjectCommand {
    /// Scan a directory for common project manifests.
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 3)]
        depth: usize,
    },
    /// Park a directory: every project directly inside it is served as `<name>.test`.
    Park { path: PathBuf },
    /// Stop serving a parked directory.
    Unpark { path: PathBuf },
    /// Link a project directory to a local domain.
    Link { path: PathBuf, domain: String },
    /// Remove the domain link of a project directory.
    Unlink { path: PathBuf },
    /// List projects from parked paths and links, with domain and source.
    List {
        #[arg(long, default_value_t = 3)]
        depth: usize,
    },
    /// Pin a project to one installed runtime version.
    SetRuntime {
        path: PathBuf,
        runtime: String,
        #[arg(long)]
        version: Option<String>,
    },
    /// Remove a project's runtime pin.
    ClearRuntime { path: PathBuf },
}

#[derive(Subcommand)]
enum RuntimeCommand {
    /// List runtimes detected on this machine.
    List,
    /// Install a runtime version. Only Rust (via rustup) is installed
    /// by the daemon; anything else fails with a manual-install hint.
    Install {
        runtime: String,
        #[arg(long)]
        version: Option<String>,
    },
}

#[derive(Subcommand)]
enum DomainCommand {
    /// List known domains with their certificate status.
    List,
    /// Create a local CA (once) and a certificate for a linked domain.
    Secure { domain: String },
    /// Delete a domain's certificate.
    Unsecure { domain: String },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// List registered services with status and health.
    List,
    /// Register a service. Known names (postgres, mysql, redis, mailpit)
    /// fill in the default binary and port when --command is omitted.
    Create {
        name: String,
        #[arg(long)]
        command: Option<String>,
        /// Args are passed through verbatim; values starting with `-`
        /// (e.g. `-m`, `--port`) are accepted, not parsed as flags.
        #[arg(long = "arg", allow_hyphen_values = true)]
        args: Vec<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        workdir: Option<PathBuf>,
    },
    /// Start a registered service.
    Start { name: String },
    /// Stop a running service.
    Stop { name: String },
    /// Remove a registered (stopped) service.
    Remove { name: String },
    /// Show the last log lines of a service.
    Logs {
        name: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Daemon => herd_daemon::run().await,
        Command::Status => {
            let response = call("system.status", json!({})).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
        Command::Projects { command: ProjectCommand::Scan { path, depth } } => {
            let absolute = std::fs::canonicalize(&path)
                .with_context(|| format!("cannot access {}", path.display()))?;
            let response =
                call("projects.scan", json!({"path": absolute, "max_depth": depth})).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
        Command::Projects { command: ProjectCommand::Park { path } } => {
            let absolute = canonical_dir(&path)?;
            print_call("projects.park", json!({"path": absolute})).await
        }
        Command::Projects { command: ProjectCommand::Unpark { path } } => {
            // Unpark must also work after the directory was deleted, so
            // resolve the stored path without requiring it to exist.
            let absolute = herd_daemon::stored_dir(&path);
            print_call("projects.unpark", json!({"path": absolute})).await
        }
        Command::Projects { command: ProjectCommand::Link { path, domain } } => {
            let absolute = canonical_dir(&path)?;
            print_call("projects.link", json!({"path": absolute, "domain": domain})).await
        }
        Command::Projects { command: ProjectCommand::Unlink { path } } => {
            let absolute = herd_daemon::stored_dir(&path);
            print_call("projects.unlink", json!({"path": absolute})).await
        }
        Command::Projects { command: ProjectCommand::List { depth } } => {
            print_call("projects.list", json!({"max_depth": depth})).await
        }
        Command::Projects { command: ProjectCommand::SetRuntime { path, runtime, version } } => {
            let absolute = canonical_dir(&path)?;
            let mut params = json!({"path": absolute, "runtime": runtime});
            if let Some(version) = version {
                params["version"] = json!(version);
            }
            print_call("projects.set_runtime", params).await
        }
        Command::Projects { command: ProjectCommand::ClearRuntime { path } } => {
            let absolute = canonical_dir(&path)?;
            print_call("projects.clear_runtime", json!({"path": absolute})).await
        }
        Command::Runtimes { command: RuntimeCommand::List } => {
            print_call("runtimes.list", json!({})).await
        }
        Command::Runtimes { command: RuntimeCommand::Install { runtime, version } } => {
            let mut params = json!({"runtime": runtime});
            if let Some(version) = version {
                params["version"] = json!(version);
            }
            print_call("runtimes.install", params).await
        }
        Command::Domains { command: DomainCommand::List } => {
            print_call("domains.list", json!({})).await
        }
        Command::Domains { command: DomainCommand::Secure { domain } } => {
            print_call("domains.secure", json!({"domain": domain})).await
        }
        Command::Domains { command: DomainCommand::Unsecure { domain } } => {
            print_call("domains.unsecure", json!({"domain": domain})).await
        }
        Command::Services { command: ServiceCommand::List } => {
            print_call("services.list", json!({})).await
        }
        Command::Services {
            command: ServiceCommand::Create { name, command, args, port, workdir },
        } => {
            let mut params = json!({"name": name, "args": args});
            if let Some(command) = command {
                params["command"] = json!(command);
            }
            if let Some(port) = port {
                params["port"] = json!(port);
            }
            if let Some(workdir) = workdir {
                // Canonicalize so the daemon receives the absolute
                // directory the user meant, not a path relative to
                // wherever the daemon was launched.
                let absolute = canonical_dir(&workdir)?;
                params["workdir"] = json!(absolute);
            }
            print_call("services.create", params).await
        }
        Command::Services { command: ServiceCommand::Start { name } } => {
            print_call("services.start", json!({"name": name})).await
        }
        Command::Services { command: ServiceCommand::Stop { name } } => {
            print_call("services.stop", json!({"name": name})).await
        }
        Command::Services { command: ServiceCommand::Remove { name } } => {
            print_call("services.remove", json!({"name": name})).await
        }
        Command::Services { command: ServiceCommand::Logs { name, lines } } => {
            print_call("services.logs", json!({"name": name, "lines": lines})).await
        }
    }
}

fn canonical_dir(path: &PathBuf) -> Result<PathBuf> {
    let absolute =
        std::fs::canonicalize(path).with_context(|| format!("cannot access {}", path.display()))?;
    if !absolute.is_dir() {
        anyhow::bail!("{} is not a directory", absolute.display());
    }
    Ok(absolute)
}

async fn print_call(method: &str, params: Value) -> Result<()> {
    let response = call(method, params).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn call(method: &str, params: Value) -> Result<Value> {
    let token = herd_daemon::read_token()
        .context("daemon token not found; start the daemon with `herd daemon`")?;
    let mut stream = timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", herd_daemon::DEFAULT_PORT)),
    )
    .await
    .context("timed out connecting to Herd daemon")??;
    let request = Request {
        jsonrpc: "2.0".into(),
        id: json!(1),
        method: method.into(),
        params,
        token: Some(token),
    };
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let response: Response = serde_json::from_str(&line)?;
    if let Some(error) = response.error {
        anyhow::bail!("daemon error {}: {}", error.code, error.message);
    }
    Ok(response.result.unwrap_or(Value::Null))
}
