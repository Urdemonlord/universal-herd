# Daemon DNS PR-2 (protocol 1.4)

Embedded DNS server for linked `.test` domains, built into the
daemon on hickory-server 0.26.3. Listens on 127.0.0.1:53 (UDP and
TCP). Answers `.test` from the link table, forwards everything
else to the system resolver. No TLS (PR-3), no proxy changes
(PR-1 already serves HTTP on port 80).

## Answer model

Answers are computed from `state.json` every time they are needed
(on every DNS request); nothing DNS specific is stored. For one
name, in order:

1. A linked `.test` name, or a subdomain of one, answers
   127.0.0.1, TTL 60, for A queries. Other query types on a
   linked name answer NOERROR with no records (there is no IPv6
   address to give). A link made after the server started is
   answered at once; the handler reads the store per request.
2. A `.test` name that is not linked answers NXDOMAIN.
3. Anything else is forwarded to the upstream resolver.

The forwarder talks TCP only, with a 2s timeout and 1 attempt.
The hickory defaults are 5s x 2 attempts, which the PR-0 spike
measured as a 10s SERVFAIL when the upstream is dead; 2s x 1
fails fast for a local forwarder. TCP forwarding measured a 1ms
median in the spike. The server itself still binds UDP 53 as
well as TCP 53, but DNS-over-UDP could not be tested in the
sandbox (see Limits).

## Methods

Protocol version is `1.4` (`system.status` reports it).

### `dns.start`

Params: `{"port": 5353, "upstream": "127.0.0.1",
"upstream_port": 53531}`, all optional. Defaults: port 53,
upstream is the first nameserver in `/etc/resolv.conf`
(Unix), upstream port 53. Listens on 127.0.0.1 only. Returns
`{"running": true, "port": 53, "started_at": <unix>,
"upstream": "198.19.0.1", "upstream_port": 53}`. Errors:

- `-32010` when the DNS server is already running.
- `-32010` when a socket cannot bind, naming the owning
  process when it can be detected on Linux (via `/proc/net/tcp`
  + `/proc/<pid>/fd`, shared with the proxy).
- `-32010` when no upstream can be determined and none was
  passed (no `/etc/resolv.conf` nameserver, e.g. Windows):
  the message says to pass `params.upstream` explicitly.
- `-32602` for an out-of-range port or an upstream that is not
  an IP address.

`--port` and `--upstream-port` are test/dev escape hatches;
resolvers query port 53, so production use is port 53.

### `dns.stop`

Params: none. Cancels the server shutdown token and waits for
`block_until_done` to finish, so in-flight queries are not cut
off by dropping the task. Returns `{"running": false, "port":
53}`. `-32602` when the DNS server is not running.

### `dns.status`

Params: none. Returns `{"running": true, "port": 53,
"started_at": <unix>, "upstream": "198.19.0.1",
"upstream_port": 53, "domains": 2}` or `{"running": false,
"port": null, "started_at": null, "upstream": null,
"upstream_port": null, "domains": 2}`. `domains` is the number
of linked domains currently answered, whether the server is
running or not. A server task that died on its own is reported
as not running, and `dns.start` may replace it without an
explicit stop first.

### `dns.install`

Params: none. Registers the OS resolver so `*.test` queries
reach this daemon. Linux: `resolvectl dns lo 127.0.0.1` +
`resolvectl domain lo ~test`. macOS: writes
`/etc/resolver/test` (`nameserver 127.0.0.1`) and nudges
mDNSResponder. Windows: `Add-DnsClientNrptRule -Namespace
".test" -NameServers "127.0.0.1" -DisplayName "Herd"` in an
elevated PowerShell. Returns `{"installed": true, "method":
"resolvectl", "detail": "..."}`. Errors:

- `-32010` when the OS mechanism is not available (no
  `resolvectl` on a system without systemd-resolved) or the
  command fails. Registration needs privilege, so as a normal
  user the error names the exact privileged command to run
  instead of pretending it worked.

`dns.install` only registers the resolver; it does not start
the DNS server (run `herd dns start` for that) and it does not
install any helper. There is no setuid/sudo helper in this PR;
that is still an open product decision (plan section 10).

### `dns.uninstall`

Params: none. Removes the registration: Linux `resolvectl
revert lo`, macOS removes `/etc/resolver/test`, Windows
removes the Herd NRPT rule. Returns `{"installed": false,
"method": "resolvectl", "detail": "..."}`. Idempotent: a
missing registration is not an error on macOS (the file is
simply absent).

### `system.doctor`

Params: none. Checks the three layers and reports each one.
Every probe reports what it actually found; anything that
cannot be detected is `unknown`, never a guessed yes.

```json
{"dns": {
   "daemon_running": true, "port": 53, "domains_answered": 2,
   "direct_answer": "ok",
   "direct_detail": "api.test resolves to 127.0.0.1 via the Herd DNS server",
   "resolver_registered": "no",
   "registration_detail": "resolvectl: lo does not route ~test to 127.0.0.1; run `herd dns install`",
   "system_resolves": "no",
   "system_detail": "api.test does not resolve through the system resolver"
 },
 "proxy": {"running": false, "port": null},
 "ca": {"exists": false, "trusted": "no",
        "detail": "no Herd CA yet; run `herd domains secure <domain>` first"}}
```

- DNS, direct: the doctor builds a resolver pointed at the
  daemon's own DNS port and looks up the first linked domain
  (sorted). `ok` / `fail` / `not_running` / `no_domain`.
- DNS, registration: Linux reads the live `resolvectl domain
  lo` / `resolvectl dns lo` settings; macOS reads
  `/etc/resolver/test`; Windows reports `unknown` (NRPT
  detection is not implemented yet).
- DNS, end to end: the first linked domain is resolved through
  the system resolver (`getaddrinfo`). `yes` only when it
  returns 127.0.0.1.
- Proxy: `proxy.status` (running, port).
- CA: `exists` is whether `certs/ca.crt` exists. `trusted` on
  Linux checks whether the Debian/Ubuntu bundle
  `/etc/ssl/certs/ca-certificates.crt` contains the CA text;
  on macOS it asks the System keychain; on Windows it is
  `unknown`. Trust installation itself is PR-3.

## CLI

```sh
herd dns start [--port 5353] [--upstream 127.0.0.1] [--upstream-port 53531]
herd dns stop
herd dns status
herd dns install
herd dns uninstall
herd doctor
```

## Testing

- Unit tests in `crates/herd-daemon/src/dns.rs` (10 tests):
  local answers (exact, subdomain, child link beats parent),
  NXDOMAIN for unlinked `.test` names, non-`.test` names not
  answered locally, case-insensitive and trailing-dot matching,
  suffix lookalikes, `resolv.conf` first-nameserver parsing,
  and the forwarder timeout (2s) and attempts (1).
- Integration test `crates/herd-daemon/tests/dns_pr2.rs`: a
  real daemon on a port 0 listener, a fake upstream DNS server
  (a second hickory server in the test) answering
  `example.com` with a fixed IP, and queries over TCP through
  a hickory client plus `dig +tcp` when the binary exists:
  local answers, subdomain, AAAA NOERROR with no records,
  NXDOMAIN, forwarding, a link made after start answered at
  once, start/stop/restart, the -32010 cases, bad params, the
  doctor report, and install/uninstall shape.
- Injected-bug RED proof: changing the local answer IP from
  127.0.0.1 to 127.0.0.2 fails 5 unit tests; restoring the
  hickory defaults (5s timeout, 2 attempts) fails the
  forwarder tuning test. Both reverted green.
- Smoke test (manual, real daemon + CLI + dig, no resolver
  registration): `herd dns start --port 15353` then `dig
  @127.0.0.1 -p 15353 web.test A +tcp` answers 127.0.0.1.
  In the sandbox, curl to localhost must use `--noproxy`, or
  it talks to the sandbox egress proxy (MITM) instead.

### Test harness note: blocking calls starve the test runtime

`#[tokio::test]` defaults to a current-thread runtime: the
test future, the daemon serve task, and the DNS server task
all share one thread. The first version of the integration
test called `dig` with blocking `std::process::Command`, which
blocked that thread while dig waited for an answer the starved
server could not produce: dig timed out 3x5s and the test took
30s. The fix is `tokio::process::Command` (async) for the dig
calls. Any subprocess that talks to a server in the same test
must be spawned async, never with a blocking wait.

## Limits (by design, PR-2)

- DNS-over-UDP is bound but untested: the sandbox blocks UDP
  sendto for every binary except dig to the gateway, so no UDP
  packet ever reaches the loopback server. UDP must be verified
  on a real machine before PR-2 merges (spike section 3.4).
- Resolver registration per OS is implemented but not verified
  live: the sandbox has no `resolvectl` and no systemd-resolved,
  and there is no macOS or Windows here. `dns.install` on this
  machine fails -32010 with a clear message, as designed.
- No setuid/sudo helper: install/uninstall run the OS command
  directly and fail clearly without privilege. The helper is an
  open product decision (plan section 10).
- CA trust install is PR-3; `system.doctor` only reports
  whether the CA is trusted, it never installs trust.
- Windows NRPT detection in the doctor reports `unknown`;
  the install/uninstall commands exist but are untested.
- The macOS 26 `/etc/resolver/` risk from the spike (a single
  bug report that the mechanism silently breaks for TLDs
  outside the IANA root zone, which includes `.test`) is still
  open and needs a live Mac test.
- Firefox uses its own NSS store and will not trust the Herd
  CA from the system store; that needs the enterprise-roots
  step documented in the spike (PR-3 scope).
- The desktop Tauri tests still assert protocol `"1.3"`
  (`apps/desktop/src-tauri/src/client.rs` mock and
  `tests/smoke_real_daemon.rs`); they are test-only staleness
  (the client passes the version through, it does not enforce
  it) and need a one-line update each to `"1.4"` with the
  desktop touch. The workspace test suite does not run them
  (`apps/desktop/src-tauri` is excluded from the workspace).
