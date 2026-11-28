# Daemon Proxy PR-1 (protocol 1.3)

HTTP proxy + static file server for linked `.test` domains, built
into the daemon. HTTP on port 80 only. No TLS (PR-3), no DNS (PR-2),
no PHP-FPM.

## Route model

Routes are computed from `state.json` every time they are needed
(`proxy.routes` and on every HTTP request); no route table is stored.
For one domain, in order:

1. A Herd service whose `workdir` is the linked project, which is
   running and declares a port: reverse proxy to 127.0.0.1:port,
   with `Host` rewritten to the upstream, plus
   `X-Forwarded-For: 127.0.0.1` and `X-Forwarded-Proto: http`.
   Hop-by-hop headers are stripped both ways. Upstream failure is a
   502 page, not a dropped connection.
2. Otherwise the first docroot under the project containing an
   `index.html`: `public/`, `dist/`, `build/`, `out/`, `static/`,
   then the project root. Static files are served from that
   domain's own docroot only; a shared docroot would leak one
   project's files to another domain (regression caught by TDD in
   the PR-0 spike). A directory request serves its `index.html`.
   Paths with `..` segments are rejected. Request paths are
   percent-decoded before the traversal check, so `/hello%20world.html`
   finds the real file and `/%2e%2e/` is caught like plain `..`.
   The resolved file is verified to stay inside the canonical
   docroot, so a symlink inside the docroot pointing outside is
   never served.
3. Otherwise the Herd placeholder page (HTTP 200): the domain is
   linked but nothing serves it yet, with instructions to start a
   service or add an `index.html`.
4. A host that is not linked at all: the Herd 404 page ("domain
   belum di-link"), not a connection error.

Subdomains `<sub>.name.test` go to the same site as `name.test`.
When several linked domains match a host (an explicitly linked
child like `z.app.test` and its parent `app.test`), the longest
(most specific) domain wins. Host matching is case-insensitive
and ignores any `:port` suffix.
A service only counts when it is running: stopping it falls back
to the static docroot or the placeholder on the next request.
A service workdir is canonicalized when the service is created,
and route comparison normalizes path spellings, so a workdir
stored as `/srv/web/.` still matches the linked `/srv/web`.

## Methods

Protocol version is `1.3` (`system.status` reports it).

### `proxy.routes`

Params: none. Returns the computed routes, sorted by domain:

```json
{"routes": [
  {"domain": "api.test", "project": "/srv/api", "target": "proxy",
   "service": "api", "port": 18081},
  {"domain": "blog.test", "project": "/srv/blog", "target": "placeholder"},
  {"domain": "web.test", "project": "/srv/web", "target": "static",
   "docroot": "/srv/web/public"}
]}
```

### `proxy.start`

Params: `{"port": 8080}` optional, default 80. Listens on
127.0.0.1 only. Returns `{"running": true, "port": 80,
"started_at": <unix>}`. Errors:

- `-32010` when the proxy is already running.
- `-32010` when the port is already taken, naming the owning
  process when it can be detected on Linux (via `/proc/net/tcp`
  + `/proc/<pid>/fd`): e.g. `cannot bind 127.0.0.1:80: Address
  already in use (os error 98) (owned by python3 (pid 15871))`.
  When the owner cannot be detected the bind error is reported
  without it, never as a failure of its own.
- `-32602` for an out-of-range port.

### `proxy.stop`

Params: none. Stops the accept loop and waits for it to finish,
then waits for every accepted connection to finish its graceful
shutdown. `stop` does not return while a connection is still
serving. Returns `{"running": false, "port": 80}`. `-32602` when
the proxy is not running.

### `proxy.status`

Params: none. Returns `{"running": true, "port": 80,
"started_at": <unix>}` or `{"running": false, "port": null,
"started_at": null}`. A listener that gave up after repeated
accept errors (see below) is reported as not running, and
`proxy.start` may replace it without an explicit stop first.

### Accept errors

An accept error on a bound listener is usually transient, so the
accept loop retries with backoff (100ms, doubling to a 1s cap).
After 10 consecutive failures the listener is declared
terminated: the accept loop ends, open connections are shut down
gracefully, `proxy.status` reports not running, and `proxy.start`
starts a fresh proxy. `proxy.start` still refuses while a healthy
proxy is running, so starts stay serialized.

## CLI

```sh
herd proxy start [--port 8080]   # default port 80
herd proxy stop
herd proxy status
herd proxy routes
```

`--port` is a test/dev escape hatch; browsers reach `.test`
domains on port 80, so production use is port 80.

## Testing

- Unit tests in `crates/herd-daemon/src/proxy.rs` (17 tests):
  route order, docroot order and per-domain isolation, subdomain
  matching, longest-match child routing, workdir spelling
  normalization, percent-decoded static paths, symlink
  containment, accept-error backoff, traversal rejection, suffix
  lookalikes, placeholder and 404 pages, and the `/proc/net/tcp`
  parser.
- Integration test `crates/herd-daemon/tests/proxy_pr1.rs`: a real
  daemon on a port 0 listener, a Python upstream service, and raw
  HTTP requests with Host headers covering proxy, subdomain proxy,
  static, percent-encoded static paths, symlink escape rejection,
  placeholder, 404, start/stop, restart after stop, keep-alive
  connection closed by stop, workdir canonicalization at
  `services.create`, and the -32010 cases. The `port_owner`
  assertion is Linux-only; other targets still assert the -32010
  error code and the port conflict.
- Smoke test (manual, real daemon + CLI + curl, no DNS):
  `curl --noproxy '*' -H "Host: api.test" http://127.0.0.1/`.
  In the sandbox, curl to localhost must use `--noproxy`, or it
  talks to the sandbox egress proxy (MITM) instead.

## Limits (by design, PR-1)

- HTTP only; a secured domain still serves HTTP on 80 until PR-3.
- The whole upstream response body is buffered in memory before
  it is sent to the browser (dev proxy; upstream is local).
- A service is routed when its `workdir` names the linked project
  after path normalization. A service without `workdir`, or one
  whose workdir is a parent of the project, is not routed.
- Route changes apply on the next request; there is no push or
  reload step.
- `proxy.stop` waits for in-flight connections to finish their
  graceful shutdown, so a client holding a request open can delay
  the stop.
