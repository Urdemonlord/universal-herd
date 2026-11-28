//! End-to-end test of the PR-2 DNS methods against a real daemon
//! served on a port 0 listener, with the state file pointed at a
//! temp dir. Follows the `proxy_pr1.rs` pattern: one test function
//! runs the whole flow sequentially.
//!
//! Queries go over TCP only, through a hickory client pointed at
//! the daemon's DNS port. DNS-over-UDP is NOT tested here: the
//! sandbox blocks UDP sendto for every binary except dig to the
//! gateway, so loopback UDP always times out (see the spike
//! report, section 3.4). The server still binds the UDP socket;
//! UDP must be verified on a real machine before PR-2 merges.
//!
//! Forwarding is tested against a fake upstream DNS server (a
//! second hickory server in this test) that answers `example.com`
//! with a fixed IP, so the test needs no internet and no real
//! upstream. `dig +tcp` is used as a second client when the
//! binary exists; when it does not, that part is skipped and the
//! hickory client assertions still cover the behavior.

use async_trait::async_trait;
use herd_daemon::{DaemonState, serve};
use herd_protocol::{Request, Response};
use hickory_proto::{
    op::ResponseCode,
    rr::{Name, RData, Record, RecordType, rdata::A},
};
use hickory_resolver::{
    TokioResolver,
    config::{ConnectionConfig, NameServerConfig, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
};
use hickory_server::{
    net::runtime::Time,
    proto::op::Metadata,
    server::{Request as DnsRequest, RequestHandler, ResponseHandler, ResponseInfo, Server},
    zone_handler::MessageResponseBuilder,
};
use serde_json::{Value, json};
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};

struct Client {
    stream: tokio::net::tcp::OwnedWriteHalf,
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    next_id: u64,
}

impl Client {
    async fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (read, write) = stream.into_split();
        Self { stream: write, reader: BufReader::new(read), next_id: 1 }
    }

    async fn call(&mut self, method: &str, params: Value) -> Response {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request {
            jsonrpc: "2.0".into(),
            id: json!(id),
            method: method.into(),
            params,
            token: Some("test-token".into()),
        };
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        self.stream.write_all(&bytes).await.unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        let response = self.call(method, params).await;
        assert!(response.error.is_none(), "{method} failed: {:?}", response.error);
        response.result.unwrap()
    }

    async fn err(&mut self, method: &str, params: Value) -> (i32, String) {
        let response = self.call(method, params).await;
        let error = response.error.expect("expected an error");
        (error.code, error.message)
    }
}

/// A free loopback port (bind port 0, read, release).
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// A DNS client pointed at 127.0.0.1:`port`, TCP only: the sandbox
/// blocks UDP sendto, so a UDP client would always time out here.
fn dns_client(port: u16) -> TokioResolver {
    let mut connection = ConnectionConfig::tcp();
    connection.port = port;
    let config = ResolverConfig::from_name_servers(vec![NameServerConfig::new(
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        true,
        vec![connection],
    )]);
    TokioResolver::builder_with_config(config, TokioRuntimeProvider::default()).build().unwrap()
}

/// The fake upstream: answers `example.com` A with a fixed IP,
/// NOERROR with no records for other types on that name, NXDOMAIN
/// for everything else. TCP only; the daemon's forwarder is TCP.
struct FakeUpstream;

#[async_trait]
impl RequestHandler for FakeUpstream {
    async fn handle_request<R: ResponseHandler, T: Time>(
        &self,
        request: &DnsRequest,
        mut response_handler: R,
    ) -> ResponseInfo {
        let builder = MessageResponseBuilder::from_message_request(request);
        let metadata = Metadata::response_from_request(&request.metadata);
        let Some(query) = request.queries.queries().first() else {
            let response = builder.error_msg(&request.metadata, ResponseCode::FormErr);
            return response_handler.send_response(response).await.expect("fake upstream send");
        };
        let name = query.name().to_string();
        if name == "example.com." && query.query_type() == RecordType::A {
            let record = Record::from_rdata(
                query.name().clone().into(),
                60,
                RData::A(A(Ipv4Addr::new(93, 184, 216, 34))),
            );
            let response = builder.build(metadata, std::iter::once(&record), &[], &[], &[]);
            response_handler.send_response(response).await.expect("fake upstream send")
        } else if name == "example.com." {
            let response = builder.build_no_records(metadata);
            response_handler.send_response(response).await.expect("fake upstream send")
        } else {
            let response = builder.error_msg(&request.metadata, ResponseCode::NXDomain);
            response_handler.send_response(response).await.expect("fake upstream send")
        }
    }
}

async fn start_fake_upstream(port: u16) -> tokio_util::sync::CancellationToken {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    let mut server = Server::new(FakeUpstream);
    server.register_listener(listener, Duration::from_secs(5), 4096);
    let token = server.shutdown_token().clone();
    tokio::spawn(async move {
        let _ = server.block_until_done().await;
    });
    token
}

fn dig_available() -> bool {
    std::process::Command::new("dig").arg("-v").output().is_ok()
}

/// One `dig +tcp` query. Returns stdout. Async: `dig` must not
/// block the test's current-thread runtime, or the DNS server
/// task (same thread) starves and dig times out waiting for an
/// answer that cannot be produced until dig exits.
async fn dig(port: u16, name: &str, qtype: &str) -> String {
    let output = tokio::process::Command::new("dig")
        .args(["@127.0.0.1", "-p", &port.to_string(), name, qtype, "+tcp", "+short"])
        .output()
        .await
        .expect("dig failed to run");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// One `dig +tcp` query with the header comments (status line).
async fn dig_comments(port: u16, name: &str, qtype: &str) -> String {
    let output = tokio::process::Command::new("dig")
        .args(["@127.0.0.1", "-p", &port.to_string(), name, qtype, "+tcp", "+noall", "+comments"])
        .output()
        .await
        .expect("dig failed to run");
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[tokio::test]
async fn dns_answers_forward_and_doctor() {
    let base = std::env::temp_dir().join(format!("herd-dns-pr2-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let config_dir = base.join("config");
    fs::create_dir_all(&config_dir).unwrap();

    for name in ["web", "api", "blog"] {
        fs::create_dir_all(base.join(name)).unwrap();
        fs::write(base.join(name).join("package.json"), "{}").unwrap();
    }
    let web = fs::canonicalize(base.join("web")).unwrap();
    let api = fs::canonicalize(base.join("api")).unwrap();
    let blog = fs::canonicalize(base.join("blog")).unwrap();

    let state = DaemonState::load(&config_dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serve_state = Arc::clone(&state);
    tokio::spawn(async move {
        serve(listener, serve_state, "test-token".to_string()).await.unwrap();
    });
    let mut client = Client::connect(port).await;

    let status = client.ok("system.status", json!({})).await;
    assert_eq!(status["protocol"], "1.4");

    // DNS starts stopped; stopping a stopped DNS server is -32602.
    let dns_status = client.ok("dns.status", json!({})).await;
    assert_eq!(dns_status["running"], false);
    assert_eq!(dns_status["domains"], 0);
    assert_eq!(client.err("dns.stop", json!({})).await.0, -32602);

    client.ok("projects.link", json!({"path": web, "domain": "web"})).await;
    client.ok("projects.link", json!({"path": api, "domain": "api"})).await;
    let dns_status = client.ok("dns.status", json!({})).await;
    assert_eq!(dns_status["domains"], 2);

    // The fake upstream answers example.com; nothing else.
    let upstream_port = free_port().await;
    let upstream_token = start_fake_upstream(upstream_port).await;

    // Port 53 is the default, but tests use a free port and a fake
    // upstream on a high port.
    let dns_port = free_port().await;
    let started = client
        .ok(
            "dns.start",
            json!({
                "port": dns_port,
                "upstream": "127.0.0.1",
                "upstream_port": upstream_port,
            }),
        )
        .await;
    assert_eq!(started["running"], true);
    assert_eq!(started["port"], dns_port);
    assert_eq!(started["upstream"], "127.0.0.1");
    assert_eq!(started["upstream_port"], upstream_port);
    // A second start is -32010.
    let (code, message) = client.err("dns.start", json!({"port": free_port().await})).await;
    assert_eq!(code, -32010, "{message}");
    assert!(message.contains("already running"), "{message}");

    let resolver = dns_client(dns_port);
    let lookup = |name: &str| resolver.lookup_ip(name.parse::<Name>().unwrap());

    // Linked names answer 127.0.0.1, including subdomains.
    let ips: Vec<IpAddr> = lookup("web.test").await.unwrap().iter().collect();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);
    let ips: Vec<IpAddr> = lookup("api.web.test").await.unwrap().iter().collect();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);
    // AAAA on a linked name: NOERROR, no records. hickory's
    // `lookup` reports an empty answer set as NoRecordsFound, so
    // the error itself is the proof: its response code is NoError,
    // not NXDOMAIN or ServFail.
    let name: Name = "web.test".parse().unwrap();
    let error = resolver.lookup(name, RecordType::AAAA).await.unwrap_err();
    let debug = format!("{error:?}");
    assert!(debug.contains("NoError"), "{debug}");
    assert!(!debug.contains("NXDomain"), "{debug}");
    assert!(!debug.contains("ServFail"), "{debug}");
    // Unlinked .test names are NXDOMAIN, not a silent failure.
    assert!(lookup("other.test").await.is_err());
    assert!(lookup("notweb.test").await.is_err());
    // Non-.test names are forwarded to the fake upstream.
    let ips: Vec<IpAddr> = lookup("example.com").await.unwrap().iter().collect();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]);
    assert!(lookup("unknown.example").await.is_err());

    // A link made after the server started is answered at once:
    // no reload step, the handler reads the store per request.
    client.ok("projects.link", json!({"path": blog, "domain": "blog"})).await;
    let ips: Vec<IpAddr> = lookup("blog.test").await.unwrap().iter().collect();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);
    let dns_status = client.ok("dns.status", json!({})).await;
    assert_eq!(dns_status["domains"], 3);

    // dig +tcp as a second, independent client, when available.
    if dig_available() {
        assert_eq!(dig(dns_port, "web.test", "A").await, "127.0.0.1");
        assert_eq!(dig(dns_port, "api.web.test", "A").await, "127.0.0.1");
        assert_eq!(dig(dns_port, "example.com", "A").await, "93.184.216.34");
        assert_eq!(dig(dns_port, "other.test", "A").await, "");
        let comments = dig_comments(dns_port, "web.test", "AAAA").await;
        assert!(comments.contains("status: NOERROR"), "{comments}");
        let comments = dig_comments(dns_port, "other.test", "A").await;
        assert!(comments.contains("status: NXDOMAIN"), "{comments}");
    }

    // Doctor: the daemon answers directly, the proxy is down, and
    // there is no CA yet. Registration and system resolution depend
    // on the machine, so only their shape is asserted, not the
    // value: this test runs unregistered in the sandbox.
    let doctor = client.ok("system.doctor", json!({})).await;
    assert_eq!(doctor["dns"]["daemon_running"], true);
    assert_eq!(doctor["dns"]["port"], dns_port);
    assert_eq!(doctor["dns"]["domains_answered"], 3);
    assert_eq!(doctor["dns"]["direct_answer"], "ok");
    assert!(
        doctor["dns"]["direct_detail"]
            .as_str()
            .unwrap()
            .contains("resolves to 127.0.0.1 via the Herd DNS server")
    );
    assert!(
        ["yes", "no", "unknown"].contains(&doctor["dns"]["resolver_registered"].as_str().unwrap())
    );
    assert!(["yes", "no", "unknown"].contains(&doctor["dns"]["system_resolves"].as_str().unwrap()));
    assert_eq!(doctor["proxy"]["running"], false);
    assert_eq!(doctor["ca"]["exists"], false);
    assert_eq!(doctor["ca"]["trusted"], "no");

    // Bad params are -32602.
    assert_eq!(client.err("dns.start", json!({"port": 0})).await.0, -32602);
    assert_eq!(client.err("dns.start", json!({"port": 70000})).await.0, -32602);
    assert_eq!(client.err("dns.start", json!({"upstream": "not-an-ip"})).await.0, -32602);
    assert_eq!(client.err("dns.start", json!({"upstream_port": 70000})).await.0, -32602);

    // Stop: the port closes.
    let stopped = client.ok("dns.stop", json!({})).await;
    assert_eq!(stopped["running"], false);
    assert_eq!(stopped["port"], dns_port);
    assert!(TcpStream::connect(("127.0.0.1", dns_port)).await.is_err());
    let dns_status = client.ok("dns.status", json!({})).await;
    assert_eq!(dns_status["running"], false);

    // -32010 when the port is already taken (checked while the
    // DNS server is stopped, so the bind is the only failure).
    let taken_port = free_port().await;
    let _holder = TcpListener::bind(("127.0.0.1", taken_port)).await.unwrap();
    let (code, message) = client
        .err(
            "dns.start",
            json!({"port": taken_port, "upstream": "127.0.0.1", "upstream_port": upstream_port}),
        )
        .await;
    assert_eq!(code, -32010, "{message}");
    assert!(message.contains(&taken_port.to_string()), "{message}");
    assert!(message.contains("Address already in use"), "{message}");
    drop(_holder);

    // Restart after stop works.
    let restarted = client
        .ok(
            "dns.start",
            json!({
                "port": dns_port,
                "upstream": "127.0.0.1",
                "upstream_port": upstream_port,
            }),
        )
        .await;
    assert_eq!(restarted["running"], true);
    let ips: Vec<IpAddr> = lookup("web.test").await.unwrap().iter().collect();
    assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);

    // Install/uninstall shape: in the sandbox there is no
    // resolvectl, so install fails -32010 with a clear message.
    // On a machine with systemd-resolved it may succeed instead;
    // either way the result is a shaped answer, never a crash.
    let install = client.call("dns.install", json!({})).await;
    match install.error {
        Some(error) => {
            assert_eq!(error.code, -32010, "{}", error.message);
            assert!(
                error.message.contains("resolvectl") || error.message.contains("privilege"),
                "{}",
                error.message
            );
        }
        None => {
            assert_eq!(install.result.unwrap()["installed"], true);
        }
    }
    let uninstall = client.call("dns.uninstall", json!({})).await;
    match uninstall.error {
        Some(error) => assert_eq!(error.code, -32010, "{}", error.message),
        None => {
            assert_eq!(uninstall.result.unwrap()["installed"], false);
        }
    }

    client.ok("dns.stop", json!({})).await;
    upstream_token.cancel();
    fs::remove_dir_all(base).unwrap();
}
