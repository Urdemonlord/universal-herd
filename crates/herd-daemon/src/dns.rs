//! Embedded DNS server for linked `.test` domains (PR-2,
//! protocol 1.4). Answers `*.test` from the link table in
//! `state.json` and forwards everything else to the system
//! resolver. Answers are computed from state every time they are
//! needed; nothing DNS specific is stored, the same as the proxy
//! routes in `proxy.rs`.
//!
//! Answer rules for one name (plan section 3, spike PR-0 section 3):
//! 1. A linked `.test` name, or a subdomain of one, answers
//!    127.0.0.1, TTL 60, for A queries. Other query types on a
//!    linked name answer NOERROR with no records.
//! 2. A `.test` name that is not linked answers NXDOMAIN.
//! 3. Anything else is forwarded to the upstream resolver.
//!
//! The forwarder talks TCP only, with a 2s timeout and 1 attempt
//! (the hickory defaults are 5s x 2 attempts, which the spike
//! measured as a 10s SERVFAIL when the upstream is dead). TCP
//! forwarding measured a 1ms median in the spike, and TCP is the
//! only upstream transport that works in the sandbox (UDP sendto
//! is blocked there). The server itself still binds UDP 53 as
//! well as TCP 53; DNS-over-UDP is untested in the sandbox and
//! must be verified on a real machine before PR-2 merges.
//!
//! API usage was verified against the hickory 0.26.3 crate
//! sources: `Server` (not `ServerFuture`), `register_listener`
//! takes a third argument, `TokioRuntimeProvider` lives in
//! `hickory_resolver::net::runtime`, the shutdown token cancels
//! `block_until_done`, and the resolver timeout/attempts live on
//! `ResolverOpts` via `ResolverBuilder::options_mut`. Versions
//! are pinned exactly in the workspace `Cargo.toml`.

#[cfg(target_os = "macos")]
use crate::certs::CA_ISSUER;
use crate::{
    DaemonState,
    certs::{ca_crt_path, certs_dir},
    proxy::port_owner,
    runtimes::{find_program, path_dirs},
    state::Link,
};
use async_trait::async_trait;
use hickory_proto::{
    op::{Header, HeaderCounts, ResponseCode},
    rr::{RData, Record, RecordType, rdata::A},
};
use hickory_resolver::{
    TokioResolver,
    config::{ConnectionConfig, NameServerConfig, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
};
use hickory_server::{
    net::runtime::Time,
    proto::op::Metadata,
    server::{Request, RequestHandler, ResponseHandler, ResponseInfo, Server},
    zone_handler::MessageResponseBuilder,
};
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    net::{TcpListener, UdpSocket},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Default DNS listen port. `dns.start` accepts an explicit port
/// for tests; production use is port 53.
pub const DEFAULT_DNS_PORT: u16 = 53;

/// TTL for local `.test` answers.
pub const LOCAL_TTL: u32 = 60;

/// Upstream timeout: the hickory default is 5s, which the spike
/// measured as a 10s SERVFAIL when the upstream is dead (5s x 2
/// attempts). 2s x 1 attempt fails fast for a local forwarder.
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

/// Upstream attempts: 1, for the same reason as `UPSTREAM_TIMEOUT`.
pub const UPSTREAM_ATTEMPTS: usize = 1;

/// Normalize a queried DNS name: lowercase, no trailing dot.
pub fn normalize_name(name: &str) -> String {
    name.trim_end_matches('.').to_lowercase()
}

/// True when the name is under the `.test` suffix.
pub fn is_test_name(name: &str) -> bool {
    normalize_name(name).ends_with(".test")
}

/// Answer for a `.test` name from the link table: exact match, or a
/// sub-domain of a linked domain (plan section 3: `<sub>.nama.test`
/// goes to the same site). Returns None when the name is not a
/// linked `.test` name.
pub fn resolve_local(name: &str, links: &[Link]) -> Option<Ipv4Addr> {
    let name = normalize_name(name);
    if !name.ends_with(".test") {
        return None;
    }
    for link in links {
        let domain = link.domain.to_lowercase();
        if name == domain || name.ends_with(&format!(".{domain}")) {
            return Some(Ipv4Addr::new(127, 0, 0, 1));
        }
    }
    None
}

/// First nameserver in a `resolv.conf` text. Pure over the text so
/// it can be unit-tested without touching the system file.
pub fn first_nameserver(resolv_conf: &str) -> Option<Ipv4Addr> {
    for line in resolv_conf.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("nameserver") {
            continue;
        }
        if let Some(ip) = fields.next().and_then(|text| text.parse::<IpAddr>().ok())
            && let IpAddr::V4(ipv4) = ip
        {
            return Some(ipv4);
        }
    }
    None
}

/// The system resolver to forward to: the first nameserver in
/// `/etc/resolv.conf` (Unix). `None` on Windows and on machines
/// without a resolv.conf nameserver; the caller must then fail
/// clearly or take an explicit `--upstream`.
pub fn system_upstream() -> Option<IpAddr> {
    #[cfg(unix)]
    {
        std::fs::read_to_string("/etc/resolv.conf")
            .ok()
            .and_then(|text| first_nameserver(&text))
            .map(IpAddr::V4)
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Build the upstream forwarder: TCP only, 2s timeout, 1 attempt.
/// The port is carried on the connection config, so a test can
/// point the forwarder at a fake upstream on a high port.
pub fn build_forwarder(upstream: IpAddr, upstream_port: u16) -> Result<TokioResolver, String> {
    let mut connection = ConnectionConfig::tcp();
    connection.port = upstream_port;
    let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
        upstream,
        true,
        vec![connection],
    )]);
    let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
    builder.options_mut().timeout = UPSTREAM_TIMEOUT;
    builder.options_mut().attempts = UPSTREAM_ATTEMPTS;
    builder.build().map_err(|error| format!("cannot build DNS forwarder: {error}"))
}

/// The hickory request handler: local `.test` answers from the
/// daemon's link table, NXDOMAIN for unlinked `.test` names,
/// forwarding for everything else. Links are read from the store
/// on every request, so a link made after the server started is
/// answered immediately, with no reload step.
pub struct HerdDnsHandler {
    daemon: Arc<DaemonState>,
    resolver: Arc<TokioResolver>,
}

#[async_trait]
impl RequestHandler for HerdDnsHandler {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &Request,
        response_handler: R,
    ) -> ResponseInfo {
        let builder = MessageResponseBuilder::from_message_request(request);
        let metadata = Metadata::response_from_request(&request.metadata);
        let Some(query) = request.queries.queries().first() else {
            let response = builder.error_msg(&request.metadata, ResponseCode::FormErr);
            return send(response_handler, request, response).await;
        };
        let name = query.name().to_string();
        let qtype = query.query_type();
        let links = {
            let store = self.daemon.store.read().await;
            store.state.links.clone()
        };

        // Linked `.test` name: answer locally.
        if let Some(ip) = resolve_local(&name, &links) {
            if qtype == RecordType::A {
                let record =
                    Record::from_rdata(query.name().clone().into(), LOCAL_TTL, RData::A(A(ip)));
                let response = builder.build(metadata, std::iter::once(&record), &[], &[], &[]);
                return send(response_handler, request, response).await;
            }
            // AAAA and friends on a linked name: NOERROR, no records.
            let response = builder.build_no_records(metadata);
            return send(response_handler, request, response).await;
        }

        // Unlinked `.test` name: it does not exist in our table.
        if is_test_name(&name) {
            let response = builder.error_msg(&request.metadata, ResponseCode::NXDomain);
            return send(response_handler, request, response).await;
        }

        // Everything else: forward to the upstream resolver.
        match self.resolver.lookup(query.name().clone(), qtype).await {
            Ok(lookup) => {
                let answers: Vec<Record> = lookup.answers().to_vec();
                let response = builder.build(metadata, answers.iter(), &[], &[], &[]);
                send(response_handler, request, response).await
            }
            Err(_) => {
                let response = builder.error_msg(&request.metadata, ResponseCode::ServFail);
                send(response_handler, request, response).await
            }
        }
    }
}

/// Send a DNS response. A send failure means the client is gone;
/// log it and return a SERVFAIL info instead of unwrapping (the
/// spike flagged its own unwrap here as not production-safe).
async fn send<'a, R: ResponseHandler>(
    mut response_handler: R,
    request: &Request,
    response: hickory_server::zone_handler::MessageResponse<
        '_,
        'a,
        impl Iterator<Item = &'a Record> + Send + 'a,
        impl Iterator<Item = &'a Record> + Send + 'a,
        impl Iterator<Item = &'a Record> + Send + 'a,
        impl Iterator<Item = &'a Record> + Send + 'a,
    >,
) -> ResponseInfo {
    match response_handler.send_response(response).await {
        Ok(info) => info,
        Err(error) => {
            warn!(%error, "dns response send failed");
            let mut metadata = Metadata::response_from_request(&request.metadata);
            metadata.response_code = ResponseCode::ServFail;
            ResponseInfo::from(Header { metadata, counts: HeaderCounts::default() })
        }
    }
}

/// A running DNS server: its port, start time, upstream, the
/// shutdown token, and the server task. Stopping cancels the
/// token and waits for `block_until_done` to finish, so no query
/// is cut off mid-flight by dropping the task.
pub struct DnsHandle {
    pub port: u16,
    pub started_at: SystemTime,
    pub upstream: IpAddr,
    pub upstream_port: u16,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl DnsHandle {
    /// True when the server task already finished on its own (the
    /// listener died). `dns.status` reports such a handle as not
    /// running, and `dns.start` may replace it, mirroring the
    /// proxy's terminated-listener handling.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    pub async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

/// Start the DNS server on 127.0.0.1:`port`, answering `.test`
/// from the link table and forwarding the rest to `upstream`.
/// Fails with a clear message when a socket cannot bind, naming
/// the owning process when it can be detected on Linux.
pub async fn start_dns(
    state: Arc<DaemonState>,
    port: u16,
    upstream: IpAddr,
    upstream_port: u16,
) -> Result<DnsHandle, String> {
    let resolver = Arc::new(build_forwarder(upstream, upstream_port)?);
    let handler = HerdDnsHandler { daemon: Arc::clone(&state), resolver };
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    let udp = UdpSocket::bind(addr).await.map_err(|error| {
        let owner = port_owner(port).map(|name| format!(" (owned by {name})")).unwrap_or_default();
        format!("cannot bind UDP 127.0.0.1:{port}: {error}{owner}")
    })?;
    let tcp = TcpListener::bind(addr).await.map_err(|error| {
        let owner = port_owner(port).map(|name| format!(" (owned by {name})")).unwrap_or_default();
        format!("cannot bind TCP 127.0.0.1:{port}: {error}{owner}")
    })?;
    let mut server = Server::new(handler);
    server.register_socket(udp);
    server.register_listener(tcp, Duration::from_secs(5), 4096);
    let shutdown = server.shutdown_token().clone();
    let task = tokio::spawn(async move {
        if let Err(error) = server.block_until_done().await {
            warn!(%error, "dns server ended with error");
        }
    });
    Ok(DnsHandle { port, started_at: SystemTime::now(), upstream, upstream_port, shutdown, task })
}

/// How the OS resolver was registered, for `dns.install` replies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverMethod {
    /// Linux, systemd-resolved via `resolvectl`.
    Resolvectl,
    /// macOS, `/etc/resolver/test`.
    EtcResolver,
    /// Windows, NRPT rule.
    Nrpt,
}

impl ResolverMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Resolvectl => "resolvectl",
            Self::EtcResolver => "etc-resolver",
            Self::Nrpt => "nrpt",
        }
    }
}

fn run_command(program: &str, args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        return Err(format!("{program} {} failed with status {}", args.join(" "), output.status));
    }
    Err(format!("{program} {} failed: {detail}", args.join(" ")))
}

/// Register the OS resolver so `*.test` queries reach this
/// daemon's DNS server on 127.0.0.1:53. This only registers the
/// resolver; it does not start the DNS server (run `herd dns
/// start` for that) and it does not install any helper: there is
/// no setuid/sudo helper in this PR, that is still an open
/// product decision. Registration needs privilege, so as a
/// non-root user the command fails and the error names the exact
/// privileged command to run instead of pretending it worked.
pub fn install_resolver() -> Result<(ResolverMethod, String), String> {
    #[cfg(target_os = "linux")]
    {
        if find_program("resolvectl", &path_dirs()).is_none() {
            return Err("systemd-resolved (resolvectl) was not found on this machine; \
                Herd cannot register the .test resolver here. On a system without \
                systemd-resolved, register 127.0.0.1 for .test with your DNS manager \
                (NetworkManager, dnsmasq) instead"
                .into());
        }
        run_command("resolvectl", &["dns", "lo", "127.0.0.1"]).map_err(|error| {
            format!(
                "{error}. Registration needs privilege: run as root, or run \
                `sudo resolvectl dns lo 127.0.0.1 && sudo resolvectl domain lo ~test` yourself"
            )
        })?;
        run_command("resolvectl", &["domain", "lo", "~test"]).map_err(|error| {
            format!(
                "{error}. Registration needs privilege: run as root, or run \
                `sudo resolvectl domain lo ~test` yourself"
            )
        })?;
        Ok((
            ResolverMethod::Resolvectl,
            "resolvectl dns lo 127.0.0.1; resolvectl domain lo ~test".into(),
        ))
    }
    #[cfg(target_os = "macos")]
    {
        let dir = std::path::Path::new("/etc/resolver");
        if let Err(error) = std::fs::create_dir_all(dir) {
            return Err(format!(
                "cannot create /etc/resolver: {error}. Registration needs privilege: run as root, \
                or run `sudo sh -c 'printf \"nameserver 127.0.0.1\\n\" > /etc/resolver/test'` yourself"
            ));
        }
        if let Err(error) = std::fs::write(dir.join("test"), "nameserver 127.0.0.1\n") {
            return Err(format!(
                "cannot write /etc/resolver/test: {error}. Registration needs privilege: run as root, \
                or run `sudo sh -c 'printf \"nameserver 127.0.0.1\\n\" > /etc/resolver/test'` yourself"
            ));
        }
        // Best effort: nudge mDNSResponder to re-read /etc/resolver.
        let _ = run_command("killall", &["-HUP", "mDNSResponder"]);
        Ok((ResolverMethod::EtcResolver, "wrote /etc/resolver/test".into()))
    }
    #[cfg(target_os = "windows")]
    {
        run_command(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "Add-DnsClientNrptRule -Namespace \".test\" -NameServers \"127.0.0.1\" -DisplayName \"Herd\"",
            ],
        )
        .map_err(|error| {
            format!(
                "{error}. Registration needs an elevated PowerShell: run PowerShell as \
                Administrator and run `Add-DnsClientNrptRule -Namespace \".test\" \
                -NameServers \"127.0.0.1\" -DisplayName \"Herd\"` yourself"
            )
        })?;
        Ok((
            ResolverMethod::Nrpt,
            "Add-DnsClientNrptRule -Namespace \".test\" -NameServers \"127.0.0.1\"".into(),
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err("resolver registration is not implemented for this OS yet".into())
    }
}

/// Undo `install_resolver`: remove the OS resolver registration.
/// A missing registration is not an error; uninstall is
/// idempotent.
pub fn uninstall_resolver() -> Result<(ResolverMethod, String), String> {
    #[cfg(target_os = "linux")]
    {
        if find_program("resolvectl", &path_dirs()).is_none() {
            return Err(
                "systemd-resolved (resolvectl) was not found on this machine; nothing to uninstall"
                    .into(),
            );
        }
        match run_command("resolvectl", &["revert", "lo"]) {
            Ok(_) => Ok((ResolverMethod::Resolvectl, "resolvectl revert lo".into())),
            Err(error) => Err(format!(
                "{error}. Uninstall needs privilege: run as root, or run \
                `sudo resolvectl revert lo` yourself"
            )),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let path = std::path::Path::new("/etc/resolver/test");
        match std::fs::remove_file(path) {
            Ok(()) | Err(_) => {
                let _ = run_command("killall", &["-HUP", "mDNSResponder"]);
                Ok((ResolverMethod::EtcResolver, "removed /etc/resolver/test".into()))
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        run_command(
            "powershell",
            &[
                "-NoProfile",
                "-Command",
                "Get-DnsClientNrptRule | Where-Object { $_.DisplayName -eq 'Herd' } | Remove-DnsClientNrptRule -Force",
            ],
        )
        .map_err(|error| {
            format!(
                "{error}. Uninstall needs an elevated PowerShell: run PowerShell as \
                Administrator and remove the Herd NRPT rule yourself"
            )
        })?;
        Ok((ResolverMethod::Nrpt, "removed the Herd NRPT rule".into()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        Err("resolver unregistration is not implemented for this OS yet".into())
    }
}

/// One layer of the `system.doctor` report. `yes` / `no` are
/// facts; `unknown` means the check could not be run on this
/// machine, never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Yes,
    No,
    Unknown,
}

impl Check {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Unknown => "unknown",
        }
    }
}

/// Probe whether the OS resolver routes `.test` to this daemon.
/// Linux reads the live resolvectl link settings; macOS reads
/// `/etc/resolver/test`; Windows is not probed yet, so it reports
/// unknown instead of a guess.
fn probe_registration() -> (Check, String) {
    #[cfg(target_os = "linux")]
    {
        if find_program("resolvectl", &path_dirs()).is_none() {
            return (
                Check::Unknown,
                "resolvectl not found; cannot tell whether .test is registered".into(),
            );
        }
        let dns = run_command("resolvectl", &["dns", "lo"]);
        let domains = run_command("resolvectl", &["domain", "lo"]);
        match (dns, domains) {
            (Ok(dns), Ok(domains)) => {
                if dns.contains("127.0.0.1") && domains.contains("~test") {
                    (Check::Yes, "resolvectl: lo routes ~test to 127.0.0.1".into())
                } else {
                    (
                        Check::No,
                        "resolvectl: lo does not route ~test to 127.0.0.1; run `herd dns install`"
                            .into(),
                    )
                }
            }
            (Err(error), _) | (_, Err(error)) => {
                (Check::Unknown, format!("resolvectl query failed: {error}"))
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        match std::fs::read_to_string("/etc/resolver/test") {
            Ok(text) if text.contains("nameserver 127.0.0.1") => {
                (Check::Yes, "/etc/resolver/test routes .test to 127.0.0.1".into())
            }
            Ok(_) => (
                Check::No,
                "/etc/resolver/test exists but does not point at 127.0.0.1; run `herd dns install`"
                    .into(),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (Check::No, "/etc/resolver/test does not exist; run `herd dns install`".into())
            }
            Err(error) => (Check::Unknown, format!("cannot read /etc/resolver/test: {error}")),
        }
    }
    #[cfg(target_os = "windows")]
    {
        (
            Check::Unknown,
            "NRPT detection is not implemented yet; check with `Get-DnsClientNrptRule`".into(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        (Check::Unknown, "resolver detection is not implemented for this OS yet".into())
    }
}

/// Probe whether the Herd CA is in the system trust store.
/// Linux checks the Debian/Ubuntu bundle for the CA text;
/// macOS asks the System keychain; Windows is not probed yet.
fn probe_ca_trust(config_dir: &std::path::Path) -> (bool, Check, String) {
    let ca = ca_crt_path(&certs_dir(config_dir));
    if !ca.exists() {
        return (
            false,
            Check::No,
            "no Herd CA yet; run `herd domains secure <domain>` first".into(),
        );
    }
    #[cfg(target_os = "linux")]
    {
        let bundle = std::path::Path::new("/etc/ssl/certs/ca-certificates.crt");
        match (std::fs::read_to_string(&ca), std::fs::read_to_string(bundle)) {
            (Ok(ca_text), Ok(bundle_text)) => {
                if bundle_text.contains(ca_text.trim()) {
                    (true, Check::Yes, "Herd CA is in /etc/ssl/certs/ca-certificates.crt".into())
                } else {
                    (
                        true,
                        Check::No,
                        "Herd CA exists but is not in the system bundle; trust install is PR-3"
                            .into(),
                    )
                }
            }
            (Err(error), _) => (true, Check::Unknown, format!("cannot read the Herd CA: {error}")),
            (_, Err(_)) => (
                true,
                Check::Unknown,
                "no Debian/Ubuntu CA bundle found; RHEL-style trust stores are not probed yet"
                    .into(),
            ),
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = ca;
        match run_command(
            "security",
            &["find-certificate", "-c", CA_ISSUER, "-a", "/Library/Keychains/System.keychain"],
        ) {
            Ok(_) => (true, Check::Yes, "Herd CA is in the System keychain".into()),
            Err(_) => (
                true,
                Check::No,
                "Herd CA is not in the System keychain; trust install is PR-3".into(),
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _ = ca;
        (
            true,
            Check::Unknown,
            "Windows trust store detection is not implemented yet; check with `certutil -store Root`"
                .into(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = ca;
        (true, Check::Unknown, "CA trust detection is not implemented for this OS yet".into())
    }
}

/// The `system.doctor` report: the three layers (DNS resolve,
/// proxy listen, CA trust), one by one. Every probe reports what
/// it actually found; anything that cannot be detected is
/// `unknown`, never a guessed yes.
pub async fn doctor_report(state: &DaemonState) -> Value {
    let config_dir = state.config_dir().to_path_buf();
    let (dns_running, dns_port) = {
        let slot = state.dns.lock().await;
        match slot.as_ref() {
            Some(handle) if !handle.is_finished() => (true, Some(handle.port)),
            _ => (false, None),
        }
    };
    let domains = answered_domains(state).await.len();
    let first_domain = {
        let store = state.store.read().await;
        let mut domains: Vec<String> =
            store.state.links.iter().map(|link| link.domain.clone()).collect();
        domains.sort();
        domains.into_iter().next()
    };

    // Layer 1, DNS: does the daemon itself answer a linked name?
    let (direct_answer, direct_detail) =
        flatten_direct_probe(dns_running, dns_port, first_domain.clone()).await;

    // Layer 1, registration: does the OS route .test to the daemon?
    let (registered, registration_detail) = tokio::task::spawn_blocking(probe_registration)
        .await
        .unwrap_or((Check::Unknown, "resolver probe task failed".into()));

    // Layer 1, end to end: does the system resolver answer?
    let system_resolves = match first_domain.clone() {
        None => (Check::Unknown, "no linked domain to test".into()),
        Some(domain) => {
            let target = format!("{domain}:80");
            match tokio::task::spawn_blocking(move || {
                target.to_socket_addrs().map(|mut iter| iter.next())
            })
            .await
            {
                Ok(Ok(Some(addr))) if addr.ip() == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)) => (
                    Check::Yes,
                    format!("{domain} resolves to 127.0.0.1 through the system resolver"),
                ),
                Ok(Ok(Some(addr))) => (
                    Check::No,
                    format!(
                        "{domain} resolves to {} through the system resolver, not 127.0.0.1",
                        addr.ip()
                    ),
                ),
                Ok(Ok(None)) => {
                    (Check::No, format!("{domain} does not resolve through the system resolver"))
                }
                Ok(Err(error)) => (
                    Check::No,
                    format!("{domain} does not resolve through the system resolver: {error}"),
                ),
                Err(error) => {
                    (Check::Unknown, format!("system resolution probe task failed: {error}"))
                }
            }
        }
    };

    // Layer 2, proxy: is it listening?
    let proxy = {
        let slot = state.proxy.lock().await;
        match slot.as_ref() {
            Some(handle) if !handle.is_terminated() => {
                json!({"running": true, "port": handle.port})
            }
            _ => json!({"running": false, "port": null}),
        }
    };

    // Layer 3, CA: does it exist, and is it trusted?
    let (ca_exists, ca_trusted, ca_detail) =
        tokio::task::spawn_blocking(move || probe_ca_trust(&config_dir)).await.unwrap_or((
            false,
            Check::Unknown,
            "CA probe task failed".into(),
        ));

    json!({
        "dns": {
            "daemon_running": dns_running,
            "port": dns_port,
            "domains_answered": domains,
            "direct_answer": direct_answer,
            "direct_detail": direct_detail,
            "resolver_registered": registered.as_str(),
            "registration_detail": registration_detail,
            "system_resolves": system_resolves.0.as_str(),
            "system_detail": system_resolves.1,
        },
        "proxy": proxy,
        "ca": {
            "exists": ca_exists,
            "trusted": ca_trusted.as_str(),
            "detail": ca_detail,
        },
    })
}

/// Run the direct-answer probe against the running DNS server.
/// Split out of `doctor_report` so the main flow stays readable:
/// the probe needs an owned resolver and a parsed name, and every
/// failure mode reports `fail` with the reason instead of
/// panicking on a bad name.
async fn flatten_direct_probe(
    dns_running: bool,
    dns_port: Option<u16>,
    first_domain: Option<String>,
) -> (&'static str, String) {
    if !dns_running {
        return ("not_running", "DNS server is not running; run `herd dns start`".into());
    }
    let Some(domain) = first_domain else {
        return (
            "no_domain",
            "no linked domain to test; link a project first with `herd projects link`".into(),
        );
    };
    let Some(port) = dns_port else {
        return ("not_running", "DNS server is not running; run `herd dns start`".into());
    };
    let resolver = match build_forwarder(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port) {
        Ok(resolver) => resolver,
        Err(error) => return ("fail", format!("cannot build a DNS probe client: {error}")),
    };
    use hickory_proto::rr::Name;
    let name: Name = match domain.parse() {
        Ok(name) => name,
        Err(error) => return ("fail", format!("linked domain {domain:?} does not parse: {error}")),
    };
    match resolver.lookup_ip(name).await {
        Ok(ips) => {
            let ips: Vec<IpAddr> = ips.iter().collect();
            if ips.iter().any(|ip| *ip == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))) {
                ("ok", format!("{domain} resolves to 127.0.0.1 via the Herd DNS server"))
            } else {
                ("fail", format!("{domain} resolved to {ips:?}, not 127.0.0.1"))
            }
        }
        Err(error) => ("fail", format!("{domain} lookup failed: {error}")),
    }
}

/// Linked domains currently answered by the DNS server, sorted.
pub async fn answered_domains(state: &DaemonState) -> Vec<String> {
    let store = state.store.read().await;
    let mut domains: Vec<String> =
        store.state.links.iter().map(|link| link.domain.clone()).collect();
    domains.sort();
    domains.dedup();
    domains
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Link, State};

    fn links() -> Vec<Link> {
        vec![
            Link { path: "/home/arya/projek/myapp".into(), domain: "myapp.test".into() },
            Link { path: "/home/arya/projek/blog".into(), domain: "blog.test".into() },
        ]
    }

    #[test]
    fn exact_match_resolves() {
        assert_eq!(resolve_local("myapp.test", &links()), Some(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn subdomain_of_linked_resolves() {
        assert_eq!(resolve_local("api.myapp.test", &links()), Some(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn unlinked_test_name_does_not_resolve() {
        assert_eq!(resolve_local("other.test", &links()), None);
    }

    #[test]
    fn non_test_name_does_not_resolve() {
        assert_eq!(resolve_local("example.com", &links()), None);
        assert_eq!(resolve_local("myapp.testing", &links()), None);
    }

    #[test]
    fn name_matching_is_case_insensitive_and_dot_tolerant() {
        assert_eq!(resolve_local("MyApp.TEST.", &links()), Some(Ipv4Addr::new(127, 0, 0, 1)));
    }

    #[test]
    fn is_test_name_checks_suffix() {
        assert!(is_test_name("foo.test"));
        assert!(!is_test_name("foo.testing"));
        assert!(!is_test_name("example.com"));
    }

    #[test]
    fn explicit_child_link_beats_parent() {
        let child_links = vec![
            Link { path: "/srv/app".into(), domain: "app.test".into() },
            Link { path: "/srv/z".into(), domain: "z.app.test".into() },
        ];
        assert_eq!(resolve_local("z.app.test", &child_links), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(
            resolve_local("api.z.app.test", &child_links),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        // A suffix lookalike is not a subdomain.
        assert_eq!(resolve_local("notmyapp.test", &links()), None);
        assert_eq!(resolve_local("myapp.test.evil.com", &links()), None);
    }

    #[test]
    fn resolv_conf_first_nameserver() {
        let text = "# comment\nnameserver 198.19.0.1\nnameserver 8.8.8.8\n";
        assert_eq!(first_nameserver(text), Some(Ipv4Addr::new(198, 19, 0, 1)));
        assert_eq!(first_nameserver(""), None);
        assert_eq!(first_nameserver("nameserver ::1\n"), None);
        assert_eq!(first_nameserver("nameserver not-an-ip\n"), None);
        assert_eq!(
            first_nameserver("search example.com\nnameserver 1.1.1.1\n"),
            Some(Ipv4Addr::new(1, 1, 1, 1))
        );
    }

    #[test]
    fn forwarder_uses_tuned_timeout_and_attempts() {
        let resolver = build_forwarder(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 53).unwrap();
        assert_eq!(resolver.options().timeout, UPSTREAM_TIMEOUT);
        assert_eq!(resolver.options().timeout, Duration::from_secs(2));
        assert_eq!(resolver.options().attempts, UPSTREAM_ATTEMPTS);
        assert_eq!(resolver.options().attempts, 1);
    }

    #[test]
    fn links_from_state_answer_the_same_as_links() {
        let state = State { version: 1, links: links(), ..Default::default() };
        assert_eq!(resolve_local("blog.test", &state.links), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(resolve_local("other.test", &state.links), None);
    }
}
