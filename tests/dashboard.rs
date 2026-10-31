//! The dashboard is the only part of pidmesh that listens on a socket, and the only part that
//! can read a file or drive a PTY on behalf of whoever is talking to it. These tests treat it the
//! way an attacker on the same machine would: by request, never by reading the handler.
//!
//! Every test drives the real binary, bound to port 0 in its own temporary workspace, database,
//! and `HOME`, so nothing here depends on — or disturbs — anything outside its own directory.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tempfile::TempDir;

/// The dashboard source, read at compile time so the route inventory below cannot drift from the
/// router without this file failing to compile or failing an assertion.
const SOURCE: &str = include_str!("../src/dashboard.rs");

/// What a caller has to prove before a route will run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    /// The session token, and a matching `Origin` only if the caller volunteers one.
    Read,
    /// The session token *and* the dashboard's own `Origin`.
    Control,
    /// The dashboard's own `Origin` plus a single-use ticket a control route minted.
    Ticketed,
}

impl Tier {
    /// The name of the helper that wraps this tier's routes in `api_router`.
    fn helper(self) -> &'static str {
        match self {
            Self::Read => "read(",
            Self::Control => "control(",
            Self::Ticketed => "ticketed(",
        }
    }
}

/// One reachable request per method the router exposes.
///
/// `pattern` is the literal the router registers, which is what ties this table to the source;
/// `path` is a concrete request that gets past axum's extractors and into the guard. The bodies
/// are deliberately well formed: a request that would be refused for its shape anyway proves
/// nothing about the guard in front of it.
struct Route {
    pattern: &'static str,
    method: &'static str,
    path: &'static str,
    body: &'static str,
    tier: Tier,
}

const fn route(
    pattern: &'static str,
    method: &'static str,
    path: &'static str,
    body: &'static str,
    tier: Tier,
) -> Route {
    Route {
        pattern,
        method,
        path,
        body,
        tier,
    }
}

/// A launch body naming a provider that does not exist.
///
/// A control route has to be *reached* to prove its guard let the request through, but reaching
/// `POST /api/v1/ide/sessions` for real would start an agent and cut a git worktree. An unknown
/// provider gets the request all the way into `IdeManager` and stops it there.
const UNLAUNCHABLE: &str = r#"{"name":"guard-probe","provider":"not-a-provider","workstream":"runtime","task":"guard-probe","prompt":"guard-probe","scopes":["src"]}"#;

const ROUTES: &[Route] = &[
    route(
        "/api/v1/snapshot",
        "GET",
        "/api/v1/snapshot",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/memories",
        "GET",
        "/api/v1/memories?q=probe",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/memories",
        "POST",
        "/api/v1/memories",
        r#"{"text":"probe"}"#,
        Tier::Read,
    ),
    route(
        "/api/v1/messages",
        "POST",
        "/api/v1/messages",
        r#"{"text":"probe"}"#,
        Tier::Read,
    ),
    route(
        "/api/v1/claims",
        "POST",
        "/api/v1/claims",
        r#"{"task":"probe"}"#,
        Tier::Read,
    ),
    route(
        "/api/v1/claims/{task}",
        "DELETE",
        "/api/v1/claims/probe",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/resources",
        "GET",
        "/api/v1/resources",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/resources",
        "POST",
        "/api/v1/resources",
        r#"{"resources":["path:probe"]}"#,
        Tier::Read,
    ),
    route(
        "/api/v1/resources",
        "DELETE",
        "/api/v1/resources",
        r#"{"resources":["path:probe"]}"#,
        Tier::Read,
    ),
    route(
        "/api/v1/ide/providers",
        "GET",
        "/api/v1/ide/providers",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/sessions",
        "GET",
        "/api/v1/ide/sessions",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/sessions",
        "POST",
        "/api/v1/ide/sessions",
        UNLAUNCHABLE,
        Tier::Control,
    ),
    route(
        "/api/v1/ide/sessions/{id}",
        "DELETE",
        "/api/v1/ide/sessions/probe",
        "",
        Tier::Control,
    ),
    route(
        "/api/v1/ide/sessions/{id}/output",
        "GET",
        "/api/v1/ide/sessions/probe/output",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/sessions/{id}/input",
        "POST",
        "/api/v1/ide/sessions/probe/input",
        r#"{"text":"probe"}"#,
        Tier::Control,
    ),
    route(
        "/api/v1/ide/sessions/{id}/review",
        "GET",
        "/api/v1/ide/sessions/probe/review",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/sessions/{id}/approve",
        "POST",
        "/api/v1/ide/sessions/probe/approve",
        r#"{"commit_message":"probe"}"#,
        Tier::Control,
    ),
    route(
        "/api/v1/ide/sessions/{id}/reject",
        "POST",
        "/api/v1/ide/sessions/probe/reject",
        "",
        Tier::Control,
    ),
    route(
        "/api/v1/ide/sessions/{id}/attach-ticket",
        "POST",
        "/api/v1/ide/sessions/probe/attach-ticket",
        "",
        Tier::Control,
    ),
    route(
        "/api/v1/ide/sessions/{id}/terminal",
        "GET",
        "/api/v1/ide/sessions/probe/terminal?ticket=probe",
        "",
        Tier::Ticketed,
    ),
    route(
        "/api/v1/ide/files",
        "GET",
        "/api/v1/ide/files",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/file",
        "GET",
        "/api/v1/ide/file?path=probe.txt",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/worktrees",
        "GET",
        "/api/v1/ide/worktrees",
        "",
        Tier::Read,
    ),
    route(
        "/api/v1/ide/worktrees",
        "DELETE",
        "/api/v1/ide/worktrees",
        "",
        Tier::Control,
    ),
    route("/api/{*rest}", "GET", "/api/v1/not-a-route", "", Tier::Read),
];

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A dashboard of its own: own port, own database, own git workspace, own `HOME`.
///
/// `HOME` matters as much as the database. `IdeManager` cuts its worktrees under `$HOME/.pidmesh`,
/// so a test that reached a launch or a reclaim with the real `HOME` would write to — or delete
/// from — the developer's actual working tree.
struct Dashboard {
    child: Child,
    address: String,
    token: String,
    directory: TempDir,
}

impl Dashboard {
    fn start() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        let home = directory.path().join("home");
        std::fs::create_dir_all(&workspace)?;
        std::fs::create_dir_all(&home)?;
        std::fs::write(workspace.join("probe.txt"), "in-workspace probe\n")?;
        git(&workspace, &["init", "--quiet", "--initial-branch", "main"])?;
        git(&workspace, &["config", "user.email", "probe@pidmesh.test"])?;
        git(&workspace, &["config", "user.name", "probe"])?;
        git(&workspace, &["add", "--all"])?;
        git(&workspace, &["commit", "--quiet", "--message", "probe"])?;

        let mut child = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
            .args([
                "--db",
                directory.path().join("mesh.db").to_str().context("db")?,
                "dashboard",
                "--port",
                "0",
                "--workspace",
                workspace.to_str().context("workspace")?,
            ])
            .env("HOME", &home)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut startup = String::new();
        BufReader::new(child.stdout.take().context("dashboard stdout")?)
            .read_line(&mut startup)
            .context("dashboard never announced itself")?;
        let startup: Value = serde_json::from_str(&startup)
            .with_context(|| format!("dashboard startup line was not JSON: {startup}"))?;
        let address = startup["listening"]
            .as_str()
            .context("listening address")?
            .to_owned();
        let token = startup["dashboard"]
            .as_str()
            .and_then(|url| url.split("#token=").nth(1))
            .context("dashboard token")?
            .to_owned();
        Ok(Self {
            child,
            address,
            token,
            directory,
        })
    }

    fn workspace(&self) -> std::path::PathBuf {
        self.directory.path().join("workspace")
    }

    /// The database this dashboard is running on, so a test can break it on purpose.
    fn database(&self) -> std::path::PathBuf {
        self.directory.path().join("mesh.db")
    }

    fn authorization(&self) -> String {
        format!("Bearer {}", self.token)
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Send a request with the honest `Host` and read the whole response.
    fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<Reply> {
        self.send_with_host(&self.address.clone(), method, path, headers, body)
    }

    fn send_with_host(
        &self,
        host: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Result<Reply> {
        let mut stream = TcpStream::connect(&self.address)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
        if !host.is_empty() {
            write!(head, "Host: {host}\r\n")?;
        }
        write!(head, "Content-Length: {}\r\n", body.len())?;
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        for (name, value) in headers {
            write!(head, "{name}: {value}\r\n")?;
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        // A refused request is answered before its body is drained, so the server may reset the
        // connection while these bytes are still in flight. That is the behaviour under test, not
        // a transport failure: keep whatever the server already said.
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
        Reply::read(&mut stream)
    }

    /// Announce a body of `declared` bytes and send only `sent` of them.
    ///
    /// The dashboard drains a refused request before answering, but only ever up to its own body
    /// budget — so sending a little past that budget is enough to get an answer, and the
    /// remaining `declared - sent` bytes never leave this process. A server that buffered the
    /// whole declared body before deciding anything would still be waiting here.
    fn send_oversized_body(
        &self,
        headers: &[(&str, &str)],
        declared: usize,
        sent: usize,
    ) -> Result<Reply> {
        let mut stream = TcpStream::connect(&self.address)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut head = format!(
            "POST /api/v1/memories HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {declared}\r\n",
            self.address
        );
        for (name, value) in headers {
            write!(head, "{name}: {value}\r\n")?;
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        let _ = stream.write_all(&vec![b'A'; sent]);
        let _ = stream.flush();
        Reply::read(&mut stream)
    }

    fn assert_alive(&self) -> Result<()> {
        let health = self.send("GET", "/healthz", &[], "")?;
        anyhow::ensure!(
            health.status == 200 && health.body.contains("pidmesh-dashboard"),
            "dashboard stopped answering: {health:?}"
        );
        Ok(())
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn read(stream: &mut TcpStream) -> Result<Self> {
        let mut raw = Vec::new();
        // A reset arriving after the response was written is normal here; what was already read
        // is the response, so only a read that produced nothing at all is a failure.
        if let Err(error) = stream.read_to_end(&mut raw)
            && raw.is_empty()
        {
            return Err(error).context("the dashboard answered nothing");
        }
        let response = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = response
            .split_once("\r\n\r\n")
            .context("response had no header terminator")?;
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .context("response had no status line")?
            .parse()?;
        Ok(Self {
            status,
            head: head.to_owned(),
            body: body.to_owned(),
        })
    }

    fn header(&self, name: &str) -> bool {
        self.head
            .to_ascii_lowercase()
            .contains(&format!("\r\n{}:", name.to_ascii_lowercase()))
    }
}

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 1. The route table cannot fall behind the router
// ---------------------------------------------------------------------------------------------

/// Pull every `.route("…", …)` registration out of the named function, paired with its own body.
fn routes_declared_in(function: &str) -> Vec<(String, String)> {
    let start = SOURCE
        .find(function)
        .unwrap_or_else(|| panic!("{function} is still a router"));
    let body = &SOURCE[start..];
    let end = body.find("\n}\n").expect("the router has an end");
    let body = &body[..end];
    let mut found = Vec::new();
    for chunk in body.split(".route(").skip(1) {
        let rest = chunk
            .trim_start()
            .strip_prefix('"')
            .expect("a route registers a string literal first");
        let (pattern, remainder) = rest.split_once('"').expect("the literal is terminated");
        found.push((pattern.to_owned(), remainder.to_owned()));
    }
    found
}

/// Authorization is applied per route *inside* `api_router`, through `read`, `control` or
/// `ticketed` -- it is not a blanket layer over the whole service. The outer `router` carries
/// only the body limit, the host check and the security headers, so an `/api/` path registered
/// out there is served to anyone who asks.
///
/// `registered_routes` only reads `api_router`, so such a route would be invisible to every
/// other test in this file as well as unguarded. Planting
/// `.route("/api/v1/backdoor", get(health))` beside `/healthz` is enough to reproduce it: the
/// whole suite passes. This closes that gap.
#[test]
fn the_public_router_registers_no_api_route() {
    let stray: Vec<String> = routes_declared_in("fn router(")
        .into_iter()
        .map(|(pattern, _)| pattern)
        .filter(|pattern| pattern.starts_with("/api"))
        .collect();
    assert!(
        stray.is_empty(),
        "these /api routes are registered in `router` rather than `api_router`, so no \
         read()/control()/ticketed() guard applies and they are served unauthenticated: {stray:?}"
    );
}

/// Pull every `.route("…", …)` registration out of `api_router`, paired with its own body.
fn registered_routes() -> Vec<(String, String)> {
    let start = SOURCE
        .find("fn api_router(")
        .expect("api_router is still the API router");
    let body = &SOURCE[start..];
    let end = body.find("\n}\n").expect("api_router has an end");
    let body = &body[..end];
    let mut found = Vec::new();
    for chunk in body.split(".route(").skip(1) {
        let rest = chunk
            .trim_start()
            .strip_prefix('"')
            .expect("a route registers a string literal first");
        let (pattern, remainder) = rest.split_once('"').expect("the literal is terminated");
        found.push((pattern.to_owned(), remainder.to_owned()));
    }
    found
}

/// The guard tests below are only as good as the table they iterate. If a route is added to the
/// router and not to `ROUTES`, every other test in this file would keep passing while the new
/// route went unexercised — so the table is compared against the source itself, and each entry's
/// tier is checked against the helper the router actually wrapped it in.
#[test]
fn the_route_table_accounts_for_every_route_the_router_registers() {
    let registered = registered_routes();
    assert!(
        registered.len() >= 20,
        "parsed too few routes out of api_router; the parser has gone stale: {registered:?}"
    );

    let mut declared: Vec<&str> = ROUTES.iter().map(|route| route.pattern).collect();
    declared.sort_unstable();
    declared.dedup();
    let mut actual: Vec<&str> = registered
        .iter()
        .map(|(pattern, _)| pattern.as_str())
        .collect();
    actual.sort_unstable();
    actual.dedup();
    assert_eq!(
        actual, declared,
        "api_router and the ROUTES table disagree; add the new route to ROUTES so it is exercised"
    );

    for (pattern, registration) in &registered {
        let mut expected: Vec<&str> = ROUTES
            .iter()
            .filter(|route| route.pattern == pattern)
            .map(|route| route.tier.helper())
            .collect();
        expected.sort_unstable();
        expected.dedup();
        let mut present: Vec<&str> = [Tier::Read, Tier::Control, Tier::Ticketed]
            .iter()
            .map(|tier| tier.helper())
            .filter(|helper| registration.contains(helper))
            .collect();
        present.sort_unstable();
        assert!(
            !present.is_empty(),
            "{pattern} is registered without read(), control(), or ticketed(): it has no guard"
        );
        assert_eq!(
            present, expected,
            "{pattern} is guarded by {present:?} but ROUTES claims {expected:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 2. Authentication
// ---------------------------------------------------------------------------------------------

/// The whole API surface, refused for an absent, wrong, or malformed token.
///
/// `/healthz` is checked in the same breath as a negative control: it is deliberately open, and
/// if it answered 401 the probe below would be measuring something other than the guard.
#[test]
fn every_api_route_refuses_an_absent_wrong_or_malformed_token() -> Result<()> {
    let dashboard = Dashboard::start()?;
    assert_eq!(
        dashboard.send("GET", "/healthz", &[], "")?.status,
        200,
        "the probe cannot tell a guarded route from an open one"
    );

    let token = &dashboard.token;
    let flipped = |index: usize| {
        let mut bytes = token.clone().into_bytes();
        bytes[index] = if bytes[index] == b'0' { b'1' } else { b'0' };
        format!("Bearer {}", String::from_utf8(bytes).expect("ascii token"))
    };
    let attempts: Vec<(&str, String)> = vec![
        ("no header at all", String::new()),
        ("empty value", String::new()),
        ("scheme only", "Bearer".to_owned()),
        ("scheme and nothing", "Bearer ".to_owned()),
        ("wrong scheme", format!("Basic {token}")),
        ("bare token", token.clone()),
        ("lowercase scheme", format!("bearer {token}")),
        ("first byte flipped", flipped(0)),
        ("middle byte flipped", flipped(token.len() / 2)),
        ("last byte flipped", flipped(token.len() - 1)),
        ("truncated", format!("Bearer {}", &token[..token.len() - 1])),
        ("extended", format!("Bearer {token}0")),
        // Not "trailing space": HTTP defines a field value as excluding the whitespace around
        // it, so `Bearer <token> ` is the same credential and is meant to be accepted.
        ("doubled separator", format!("Bearer  {token}")),
        (
            "reversed",
            format!("Bearer {}", token.chars().rev().collect::<String>()),
        ),
        (
            "another token entirely",
            format!("Bearer {}", "f".repeat(32)),
        ),
    ];

    for route in ROUTES {
        if route.tier == Tier::Ticketed {
            // Covered on its own below: a browser cannot put a header on a WebSocket handshake,
            // so this one route is credentialed differently and refuses differently.
            continue;
        }
        for (label, value) in &attempts {
            let headers: Vec<(&str, &str)> = if value.is_empty() && *label == "no header at all" {
                Vec::new()
            } else {
                vec![("Authorization", value.as_str())]
            };
            let reply = dashboard.send(route.method, route.path, &headers, route.body)?;
            assert_eq!(
                reply.status, 401,
                "{} {} let through a {label} token: {reply:?}",
                route.method, route.path
            );
            assert_eq!(
                reply.body, r#"{"error":"authorization required"}"#,
                "{} {} said more than it should to a {label} token",
                route.method, route.path
            );
        }
    }
    dashboard.assert_alive()
}

/// The terminal upgrade is the one API route the token does not open, because a browser cannot
/// set `Authorization` on a WebSocket handshake. What stands in its place is the dashboard's own
/// `Origin` plus a single-use ticket a control route minted — so a tokenless caller is refused,
/// and the refusal is checked here rather than assumed.
#[test]
fn the_terminal_upgrade_refuses_a_caller_without_a_ticket() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let path = "/api/v1/ide/sessions/probe/terminal?ticket=00000000000000000000000000000000";
    let authorization = dashboard.authorization();
    let origin = dashboard.origin();

    for headers in [
        vec![],
        vec![("Authorization", authorization.as_str())],
        vec![("Origin", "https://attacker.example")],
        vec![
            ("Authorization", authorization.as_str()),
            ("Origin", "https://attacker.example"),
        ],
    ] {
        let reply = dashboard.send("GET", path, &headers, "")?;
        assert_eq!(reply.status, 403, "terminal upgrade accepted {headers:?}");
        assert_eq!(reply.body, r#"{"error":"request origin rejected"}"#);
    }

    // With the right origin but no upgrade handshake the extractors stop the request before
    // `IdeManager::attach` ever sees the ticket, so a guessed ticket cannot even be tried this
    // way. The route is still distinguishable from a route that does not exist, which is a
    // smaller surface than the token tier offers and is recorded here as the known exception.
    let probed = dashboard.send("GET", path, &[("Origin", origin.as_str())], "")?;
    assert!(
        (400..500).contains(&probed.status),
        "terminal upgrade answered a bare GET with {probed:?}"
    );
    assert!(
        !probed.body.contains("ticket is"),
        "the refusal described the ticket state to a tokenless caller: {probed:?}"
    );
    dashboard.assert_alive()
}

/// A refused request must cost the server nothing: the token is checked before axum is asked to
/// buffer or parse a byte of the body.
///
/// The proof is the status code rather than a stopwatch. The same request — a body announced as
/// 64 MiB, of which only 128 KiB is ever sent — comes back 401 without a token and 413 with one.
/// The body limit is what produces the 413, so an unauthenticated 401 means the token was
/// decided *first*, with 63.9 MiB of the announced body never leaving this process.
#[test]
fn authorization_is_decided_before_the_request_body_is_read() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let declared = 64 * 1024 * 1024;
    let sent = 128 * 1024;

    for (label, headers) in [
        ("no token", Vec::new()),
        (
            "a wrong token",
            vec![("Authorization", "Bearer 00000000000000000000000000000000")],
        ),
        ("a malformed token", vec![("Authorization", "Bearer")]),
    ] {
        let refused = dashboard.send_oversized_body(&headers, declared, sent)?;
        assert_eq!(
            refused.status, 401,
            "a 64MiB body with {label} was not refused up front: {refused:?}"
        );
        assert_eq!(refused.body, r#"{"error":"authorization required"}"#);
    }

    let authorization = dashboard.authorization();
    let limited = dashboard.send_oversized_body(
        &[("Authorization", authorization.as_str())],
        declared,
        sent,
    )?;
    assert_eq!(
        limited.status, 413,
        "the body limit is what stops an authenticated caller, so this must differ from the 401 \
         above or the test above proves nothing: {limited:?}"
    );

    dashboard.assert_alive()
}

/// Nothing a request can be refused for may tell an unauthenticated caller which routes exist.
#[test]
fn an_unauthenticated_caller_cannot_map_the_api_surface() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let expected = r#"{"error":"authorization required"}"#;
    let probes: &[(&str, &str, &str)] = &[
        ("GET", "/api/v1/snapshot", ""),
        ("GET", "/api/v1/not-a-route-at-all", ""),
        ("GET", "/api/v1/ide/sessions/x/not-a-route", ""),
        ("POST", "/api/v1/ide/sessions", r#"{"name": "#),
        ("POST", "/api/v1/ide/sessions", "not json at all"),
        ("DELETE", "/api/v1/snapshot", ""),
        ("PATCH", "/api/v1/memories", ""),
        ("POST", "/api/v1/ide/providers", ""),
        ("GET", "/api/v1/ide/file?path=../../etc/passwd", ""),
    ];
    for (method, path, body) in probes {
        let reply = dashboard.send(method, path, &[], body)?;
        assert_eq!(reply.status, 401, "{method} {path} -> {reply:?}");
        assert_eq!(reply.body, expected, "{method} {path} answered differently");
    }
    dashboard.assert_alive()
}

/// Every answer, including every refusal, carries the headers that keep a browser from treating
/// the dashboard as ordinary content.
#[test]
fn every_answer_carries_the_security_headers() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let replies = [
        dashboard.send("GET", "/healthz", &[], "")?,
        dashboard.send("GET", "/", &[], "")?,
        dashboard.send("GET", "/api/v1/snapshot", &[], "")?,
        dashboard.send_with_host("evil.example", "GET", "/healthz", &[], "")?,
        dashboard.send(
            "GET",
            "/api/v1/snapshot?limit=0",
            &[("Authorization", authorization.as_str())],
            "",
        )?,
    ];
    for reply in &replies {
        for header in [
            "cache-control",
            "content-security-policy",
            "referrer-policy",
            "x-content-type-options",
        ] {
            assert!(reply.header(header), "{header} missing from {reply:?}");
        }
        assert!(reply.head.contains("frame-ancestors 'none'"), "{reply:?}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 3. Host and Origin
// ---------------------------------------------------------------------------------------------

/// A page that rebinds a name it controls to 127.0.0.1 arrives with that name in `Host`. The
/// dashboard has to refuse it even when the caller somehow holds the token.
#[test]
fn a_host_that_is_not_this_dashboard_is_refused() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let port = dashboard
        .address
        .rsplit(':')
        .next()
        .context("bound port")?
        .to_owned();

    let refused = [
        "evil.example".to_owned(),
        format!("evil.example:{port}"),
        // The classic rebinding name: a prefix that reads as loopback and resolves to anything.
        format!("127.0.0.1.evil.com:{port}"),
        "127.0.0.1.evil.com".to_owned(),
        format!("localhost.evil.com:{port}"),
        format!("127.0.0.1:{port}.evil.com"),
        // Right name, wrong server.
        format!("localhost:{}", port.parse::<u16>()? ^ 1),
        format!("127.0.0.1:{}", port.parse::<u16>()? ^ 1),
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "[::1]".to_owned(),
        String::new(),
    ];
    for host in &refused {
        for (path, headers) in [
            ("/healthz", Vec::new()),
            (
                "/api/v1/snapshot",
                vec![("Authorization", authorization.as_str())],
            ),
        ] {
            let reply = dashboard.send_with_host(host, "GET", path, &headers, "")?;
            assert_eq!(reply.status, 403, "Host {host:?} reached {path}: {reply:?}");
            assert_eq!(reply.body, r#"{"error":"request host rejected"}"#);
        }
    }

    // The aliases a person actually types for this machine on this port are the same machine.
    for host in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ] {
        let reply = dashboard.send_with_host(&host, "GET", "/healthz", &[], "")?;
        assert_eq!(reply.status, 200, "Host {host} was refused: {reply:?}");
    }
    dashboard.assert_alive()
}

/// The token tier rejects a *mismatched* origin; the control tier demands a matching one.
///
/// That is the whole guarantee the `read` / `control` split buys: a cross-origin page cannot
/// reach anything that starts, stops, writes to, or deletes on behalf of an agent, even if the
/// token leaks into it. Both halves are asserted for every route, so a route that moves between
/// tiers has to move in this table too.
#[test]
fn the_read_and_control_tiers_differ_exactly_in_demanding_an_origin() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let origin = dashboard.origin();
    let forbidden = r#"{"error":"request origin rejected"}"#;

    for route in ROUTES {
        if route.tier == Tier::Ticketed {
            continue;
        }
        // A foreign origin is refused at both tiers, token or not.
        let foreign = dashboard.send(
            route.method,
            route.path,
            &[
                ("Authorization", authorization.as_str()),
                ("Origin", "https://attacker.example"),
            ],
            route.body,
        )?;
        assert_eq!(
            foreign.status, 403,
            "{} {} accepted a foreign origin: {foreign:?}",
            route.method, route.path
        );
        assert_eq!(foreign.body, forbidden);

        // No origin at all: the dividing line between the tiers.
        let headerless = dashboard.send(
            route.method,
            route.path,
            &[("Authorization", authorization.as_str())],
            route.body,
        )?;
        match route.tier {
            Tier::Control => {
                assert_eq!(
                    headerless.status, 403,
                    "control route {} {} ran without an origin: {headerless:?}",
                    route.method, route.path
                );
                assert_eq!(headerless.body, forbidden);
            }
            Tier::Read => assert_ne!(
                headerless.status, 403,
                "read route {} {} refused a caller that sent no origin: {headerless:?}",
                route.method, route.path
            ),
            Tier::Ticketed => unreachable!(),
        }

        // With the dashboard's own origin every route reaches its handler. "Reached" is the
        // claim, not "succeeded": an unknown session or provider is the handler talking back.
        let matching = dashboard.send(
            route.method,
            route.path,
            &[
                ("Authorization", authorization.as_str()),
                ("Origin", origin.as_str()),
            ],
            route.body,
        )?;
        assert_ne!(
            matching.status, 401,
            "{} {} refused the token it was given: {matching:?}",
            route.method, route.path
        );
        assert_ne!(
            matching.status, 403,
            "{} {} refused its own origin: {matching:?}",
            route.method, route.path
        );
        assert_ne!(
            matching.status, 500,
            "{} {} answered a well-formed request with an internal error: {matching:?}",
            route.method, route.path
        );
    }
    dashboard.assert_alive()
}

// ---------------------------------------------------------------------------------------------
// 4. Path traversal
// ---------------------------------------------------------------------------------------------

/// The file routes are the reason the rest of this file exists: they read bytes off disk for
/// whoever asks. Nothing outside the workspace may come back, by any spelling.
#[test]
fn the_ide_file_routes_cannot_reach_outside_the_workspace() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let secret = "PIDMESH-OUTSIDE-THE-WORKSPACE";
    let outside = dashboard.directory.path().join("outside.txt");
    std::fs::write(&outside, format!("{secret}\n"))?;
    // A symlink is the case canonicalisation exists for: the name is innocent, the target is not.
    std::os::unix::fs::symlink(&outside, dashboard.workspace().join("escape.txt"))?;
    std::os::unix::fs::symlink(
        dashboard.directory.path(),
        dashboard.workspace().join("escape-dir"),
    )?;

    let escapes = [
        "../outside.txt",
        "../../etc/passwd",
        "../../../../../../etc/passwd",
        "/etc/passwd",
        "//etc/passwd",
        "probe.txt/../../outside.txt",
        "./../outside.txt",
        "escape.txt",
        "escape-dir/outside.txt",
        // Percent-encoded, which axum decodes before the handler sees it.
        "..%2Foutside.txt",
        "%2e%2e%2foutside.txt",
        "%2E%2E%2F%2E%2E%2Fetc%2Fpasswd",
        "%2fetc%2fpasswd",
        // Doubly encoded, so the handler sees the literal `%2e` sequence.
        "%252e%252e%252foutside.txt",
        // NUL, which no path may contain.
        "probe.txt%00",
        "probe.txt%00.png",
        "%00",
        // Separator tricks.
        "....//outside.txt",
        "..;/outside.txt",
        "..\\..\\outside.txt",
    ];
    for escape in escapes {
        for endpoint in ["file", "files"] {
            let reply = dashboard.send(
                "GET",
                &format!("/api/v1/ide/{endpoint}?path={escape}"),
                &[("Authorization", authorization.as_str())],
                "",
            )?;
            assert_ne!(
                reply.status, 200,
                "/api/v1/ide/{endpoint}?path={escape} was served: {reply:?}"
            );
            assert!(
                !reply.body.contains(secret),
                "/api/v1/ide/{endpoint}?path={escape} leaked a file outside the workspace: {reply:?}"
            );
            assert!(
                !reply.body.contains("root:x:"),
                "/api/v1/ide/{endpoint}?path={escape} leaked /etc/passwd: {reply:?}"
            );
        }
    }

    // The same route reads a file that really is in the workspace, so none of the above passed
    // because the endpoint is simply broken.
    let allowed = dashboard.send(
        "GET",
        "/api/v1/ide/file?path=probe.txt",
        &[("Authorization", authorization.as_str())],
        "",
    )?;
    assert_eq!(allowed.status, 200, "{allowed:?}");
    assert!(allowed.body.contains("in-workspace probe"), "{allowed:?}");

    let listing = dashboard.send(
        "GET",
        "/api/v1/ide/files?path=.",
        &[("Authorization", authorization.as_str())],
        "",
    )?;
    assert_eq!(listing.status, 200, "{listing:?}");
    assert!(listing.body.contains("probe.txt"), "{listing:?}");
    dashboard.assert_alive()
}

/// Credentials planted in the two `.git/config` files the probe below creates. Finding either
/// one in a response is the leak itself, not a proxy for it.
const GIT_CONFIG_SECRET: &str = "PIDMESH-CREDENTIAL-IN-GIT-CONFIG";
const NESTED_GIT_CONFIG_SECRET: &str = "PIDMESH-CREDENTIAL-IN-NESTED-GIT-CONFIG";

/// Every way of naming something at or under a git directory that resolves to real bytes in the
/// probe workspace. They are listed rather than generated because each one is a distinct way a
/// rule could be wrong: a bare name, a trailing separator, an interior `.`, a nested repository,
/// a symlink whose name is innocent, and an encoding axum unwraps before the handler runs.
const GIT_SPELLINGS: &[&str] = &[
    ".git",
    ".git/",
    ".git/.",
    ".git/config",
    "./.git/config",
    ".git/./config",
    ".git/HEAD",
    ".git/COMMIT_EDITMSG",
    ".git/objects",
    ".git/hooks",
    ".git/logs/HEAD",
    "vendor/.git",
    "vendor/.git/config",
    "./vendor/.git/config",
    "gitlink",
    "gitlink/config",
    "innocent.txt",
    "%2egit/config",
    ".git%2fconfig",
    ".git%2Fconfig",
    "%2egit%2fconfig",
];

/// Spellings that only reach the git directory where the filesystem folds case.
const GIT_SPELLINGS_WHEN_CASE_INSENSITIVE: &[&str] =
    &[".GIT/config", ".Git/config", ".GIT", "vendor/.GIT/config"];

/// Plant, in a started dashboard's workspace, everything the git-directory probe needs: a
/// credential in `.git/config`, a second repository nested inside the workspace, two symlinks
/// whose names say nothing about where they land, and tracked content whose name merely begins
/// with `.git`.
fn seed_git_exposure_probe(workspace: &Path) -> Result<()> {
    git(
        workspace,
        &[
            "remote",
            "add",
            "origin",
            &format!("https://user:{GIT_CONFIG_SECRET}@github.com/acme/private.git"),
        ],
    )?;
    let nested = workspace.join("vendor");
    std::fs::create_dir_all(&nested)?;
    git(&nested, &["init", "--quiet", "--initial-branch", "main"])?;
    git(
        &nested,
        &[
            "remote",
            "add",
            "origin",
            &format!("https://user:{NESTED_GIT_CONFIG_SECRET}@github.com/acme/vendor.git"),
        ],
    )?;
    std::fs::write(nested.join("vendored.txt"), "vendored content\n")?;
    std::os::unix::fs::symlink(workspace.join(".git"), workspace.join("gitlink"))?;
    std::os::unix::fs::symlink(
        workspace.join(".git/config"),
        workspace.join("innocent.txt"),
    )?;
    std::fs::write(workspace.join(".gitignore"), "target/\n")?;
    std::fs::create_dir_all(workspace.join(".github"))?;
    std::fs::write(workspace.join(".github/ci.yml"), "name: ci\n")?;
    Ok(())
}

/// `.git` is inside the workspace, so nothing above has anything to say about it -- and it was
/// served. `/api/v1/ide/file?path=.git/config` came back `200` with the remote URL, credentials
/// and all; `/api/v1/ide/files?path=.git` listed the object database. `.git` was filtered out of
/// its *parent's* listing, which is the clearest evidence this was an oversight rather than a
/// decision: the browser already declined to show it and then served it to anyone who typed the
/// name.
///
/// What a token holder got was not a file in the workspace. It was `.git/config`, which in a
/// cloned repository routinely carries `https://user:token@host/...`; `.git/objects`, which is
/// every version of every file ever committed, including content long since deleted from the
/// working tree; and `.git/hooks`, which is code that runs on the developer's machine at commit
/// time. The token is a real boundary, but the dashboard is a workspace file browser, and this
/// turned it into a credential and history reader.
///
/// The spellings below are not variations for their own sake: each one is a different way of
/// arriving at the same bytes, and a rule that matched on the string the caller typed would
/// fall to one of them.
#[test]
fn the_git_directory_is_not_served_by_any_spelling() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let workspace = dashboard.workspace();
    seed_git_exposure_probe(&workspace)?;

    let mut refused = GIT_SPELLINGS.to_vec();
    // A case-insensitive filesystem -- macOS's default -- opens `.git/config` for `.GIT/config`,
    // so the rule has to ignore case there. Asserting it on a case-sensitive filesystem would
    // assert only that a path which does not exist cannot be read, so it is probed for first.
    if workspace.join(".GIT/config").exists() {
        refused.extend(GIT_SPELLINGS_WHEN_CASE_INSENSITIVE);
    }

    for path in &refused {
        for endpoint in ["file", "files"] {
            let reply = dashboard.send(
                "GET",
                &format!("/api/v1/ide/{endpoint}?path={path}"),
                &[("Authorization", authorization.as_str())],
                "",
            )?;
            assert_ne!(
                reply.status, 200,
                "/api/v1/ide/{endpoint}?path={path} was served: {reply:?}"
            );
            assert!(
                !reply.body.contains(GIT_CONFIG_SECRET)
                    && !reply.body.contains(NESTED_GIT_CONFIG_SECRET),
                "/api/v1/ide/{endpoint}?path={path} leaked a credential from .git/config: {reply:?}"
            );
            // `.git/HEAD` and `refs/heads/main` are the shape of the object database; nothing
            // that names a ref should come back from either route.
            assert!(
                !reply.body.contains("refs/heads/"),
                "/api/v1/ide/{endpoint}?path={path} leaked the repository's refs: {reply:?}"
            );
        }
    }

    // A listing must not offer a name the file route will then refuse, and must not hide one it
    // would serve. `.git` is absent from both the workspace root and the nested checkout.
    for (directory, present, absent) in [
        (".", "probe.txt", "\".git\""),
        ("vendor", "vendored.txt", "\".git\""),
    ] {
        let listing = dashboard.send(
            "GET",
            &format!("/api/v1/ide/files?path={directory}"),
            &[("Authorization", authorization.as_str())],
            "",
        )?;
        assert_eq!(listing.status, 200, "{directory}: {listing:?}");
        assert!(listing.body.contains(present), "{directory}: {listing:?}");
        assert!(!listing.body.contains(absent), "{directory}: {listing:?}");
    }

    // The refusal is narrow on purpose. `.gitignore`, `.gitattributes` and `.github` are tracked
    // workspace content that people read in the browser, and hiding them would cost a real
    // feature for no security at all.
    for (path, expected) in [
        (".gitignore", "target/"),
        (".github/ci.yml", "name: ci"),
        ("vendor/vendored.txt", "vendored content"),
        ("probe.txt", "in-workspace probe"),
    ] {
        let reply = dashboard.send(
            "GET",
            &format!("/api/v1/ide/file?path={path}"),
            &[("Authorization", authorization.as_str())],
            "",
        )?;
        assert_eq!(reply.status, 200, "{path}: {reply:?}");
        assert!(reply.body.contains(expected), "{path}: {reply:?}");
    }
    dashboard.assert_alive()
}

/// The session identifier is the other caller-supplied path segment the IDE routes take; it
/// names a worktree, so it must not be usable as a path either.
#[test]
fn a_session_identifier_cannot_be_used_as_a_path() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    for session in [
        "../../etc",
        "..%2f..%2fetc",
        "%2e%2e%2f%2e%2e",
        "/etc/passwd",
        "a%00b",
    ] {
        for path in [
            format!("/api/v1/ide/files?session={session}"),
            format!("/api/v1/ide/file?session={session}&path=probe.txt"),
            format!("/api/v1/ide/sessions/{session}/output"),
            format!("/api/v1/ide/sessions/{session}/review"),
        ] {
            let reply = dashboard.send(
                "GET",
                &path,
                &[("Authorization", authorization.as_str())],
                "",
            )?;
            assert!(
                (400..500).contains(&reply.status),
                "{path} answered {reply:?}"
            );
            assert!(!reply.body.contains("root:x:"), "{path} leaked: {reply:?}");
        }
    }
    dashboard.assert_alive()
}

// ---------------------------------------------------------------------------------------------
// 5. Input robustness
// ---------------------------------------------------------------------------------------------

/// Everything a client can get wrong, kept out of the test body so the table can grow without
/// the test becoming unreadable.
///
/// Each entry is a method, a path, and a body that is wrong in exactly one way: malformed JSON,
/// the right syntax with the wrong shape, a number past the edge of its range, a resource
/// identifier the store cannot parse, a query parameter of the wrong type, or a route that is not
/// there at all.
const MALFORMED: &[(&str, &str, &str)] = &[
    // Malformed JSON.
    ("POST", "/api/v1/memories", "{"),
    ("POST", "/api/v1/memories", "not json"),
    ("POST", "/api/v1/memories", r#"{"text": "#),
    ("POST", "/api/v1/memories", "\u{feff}{}"),
    // Right syntax, wrong shape.
    ("POST", "/api/v1/memories", "[]"),
    ("POST", "/api/v1/memories", "null"),
    ("POST", "/api/v1/memories", "42"),
    ("POST", "/api/v1/memories", "{}"),
    ("POST", "/api/v1/memories", r#"{"text":123}"#),
    ("POST", "/api/v1/memories", r#"{"text":[]}"#),
    ("POST", "/api/v1/memories", r#"{"text":""}"#),
    ("POST", "/api/v1/memories", r#"{"text":"   "}"#),
    // Numbers at and past the edges.
    (
        "POST",
        "/api/v1/memories",
        r#"{"text":"t","importance":1.0001}"#,
    ),
    (
        "POST",
        "/api/v1/memories",
        r#"{"text":"t","importance":-1}"#,
    ),
    (
        "POST",
        "/api/v1/memories",
        r#"{"text":"t","importance":1e400}"#,
    ),
    (
        "POST",
        "/api/v1/claims",
        r#"{"task":"t","lease_seconds":0}"#,
    ),
    (
        "POST",
        "/api/v1/claims",
        r#"{"task":"t","lease_seconds":86401}"#,
    ),
    (
        "POST",
        "/api/v1/claims",
        r#"{"task":"t","lease_seconds":18446744073709551615}"#,
    ),
    (
        "POST",
        "/api/v1/claims",
        r#"{"task":"t","lease_seconds":-1}"#,
    ),
    ("POST", "/api/v1/claims", r#"{"task":"   "}"#),
    // Resource identifiers the store will not parse. Every one of these was a 500 before the
    // dashboard learned the `kind:key` grammar the store enforces.
    ("POST", "/api/v1/resources", r#"{"resources":["src"]}"#),
    (
        "POST",
        "/api/v1/resources",
        r#"{"resources":["path:/etc"]}"#,
    ),
    (
        "POST",
        "/api/v1/resources",
        r#"{"resources":["path:../../etc"]}"#,
    ),
    ("POST", "/api/v1/resources", r#"{"resources":["PATH:src"]}"#),
    ("POST", "/api/v1/resources", r#"{"resources":[":src"]}"#),
    ("POST", "/api/v1/resources", r#"{"resources":["path:"]}"#),
    ("POST", "/api/v1/resources", r#"{"resources":["path:."]}"#),
    (
        "POST",
        "/api/v1/resources",
        r#"{"resources":["port:\u0001"]}"#,
    ),
    ("DELETE", "/api/v1/resources", r#"{"resources":["src"]}"#),
    (
        "DELETE",
        "/api/v1/resources",
        r#"{"resources":["path:.."]}"#,
    ),
    ("POST", "/api/v1/resources", r#"{"resources":[]}"#),
    ("POST", "/api/v1/resources", r#"{"resources":"path:src"}"#),
    // Query strings.
    ("GET", "/api/v1/snapshot?limit=0", ""),
    ("GET", "/api/v1/snapshot?limit=501", ""),
    ("GET", "/api/v1/snapshot?limit=-1", ""),
    ("GET", "/api/v1/snapshot?limit=abc", ""),
    ("GET", "/api/v1/snapshot?limit=99999999999999999999", ""),
    ("GET", "/api/v1/memories", ""),
    ("GET", "/api/v1/memories?q=", ""),
    ("GET", "/api/v1/memories?q=a&limit=0", ""),
    ("GET", "/api/v1/memories?q=a&limit=101", ""),
    ("GET", "/api/v1/ide/file", ""),
    ("GET", "/api/v1/ide/sessions/x/output?after=nope", ""),
    ("DELETE", "/api/v1/claims/%20", ""),
    // Routes and methods that are not there.
    ("GET", "/api/v1/nowhere", ""),
    ("GET", "/api/v1/ide/sessions/x/nowhere", ""),
    ("DELETE", "/api/v1/snapshot", ""),
    ("PATCH", "/api/v1/memories", ""),
];

/// Everything a client can get wrong gets a 4xx, and the dashboard is still answering afterwards.
#[test]
fn malformed_input_earns_a_clean_4xx_and_the_server_survives() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let origin = dashboard.origin();
    let credentials = [
        ("Authorization", authorization.as_str()),
        ("Origin", origin.as_str()),
    ];

    // A body larger than the limit and a body nested deeper than any parser wants to recurse:
    // both are sized at run time, so they sit beside the table rather than in it.
    let oversized = format!(r#"{{"text":"{}"}}"#, "A".repeat(200 * 1024));
    let deep = format!("{}{}", "[".repeat(2000), "]".repeat(2000));
    let dynamic = [
        ("POST", "/api/v1/memories", oversized.as_str()),
        ("POST", "/api/v1/memories", deep.as_str()),
    ];

    for (method, path, body) in MALFORMED.iter().copied().chain(dynamic) {
        let reply = dashboard.send(method, path, &credentials, body)?;
        assert!(
            (400..500).contains(&reply.status),
            "{method} {path} with {:.60} answered {} rather than a client error: {reply:?}",
            body,
            reply.status
        );
        // The dashboard's own errors are JSON; axum's extractor rejections are plain text. Either
        // way nothing may come back that looks like a panic or a stack trace.
        assert!(
            !reply.body.contains("panicked") && !reply.body.contains("RUST_BACKTRACE"),
            "{method} {path} leaked a panic: {reply:?}"
        );
    }
    dashboard.assert_alive()
}

/// Requests that are refused must not cost the connection, and the server has to keep serving
/// afterwards — a refusal that wedges a worker is a denial of service with extra steps.
#[test]
fn a_run_of_refusals_does_not_wedge_the_server() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    for index in 0..32 {
        let refused = dashboard.send_oversized_body(&[], 32 * 1024 * 1024, 128 * 1024)?;
        assert_eq!(refused.status, 401, "refusal {index}: {refused:?}");
    }
    for index in 0..64 {
        let refused = dashboard.send(
            "GET",
            "/api/v1/snapshot",
            &[("Authorization", "Bearer x")],
            "",
        )?;
        assert_eq!(refused.status, 401, "refusal {index}: {refused:?}");
    }
    let snapshot = dashboard.send(
        "GET",
        "/api/v1/snapshot",
        &[("Authorization", authorization.as_str())],
        "",
    )?;
    assert_eq!(snapshot.status, 200, "{snapshot:?}");
    dashboard.assert_alive()
}

// ---------------------------------------------------------------------------------------------
// 6. What the store refuses, and what actually broke
// ---------------------------------------------------------------------------------------------

/// Naming a recipient that is not a running agent is an ordinary thing for a caller to do, and
/// the dashboard used to answer it with `500 dashboard operation failed`.
///
/// `MeshStore::send` refused an unknown recipient through `anyhow`, and `blocking` flattened
/// every `anyhow` error into `ApiError::Internal`, so a typo and a broken database were the same
/// answer. The store now says which it is with a type — `store::Refusal` — and `blocking` reads
/// the type rather than the text of the message. An unknown recipient is a 404 naming what was
/// not found; see `a_database_fault_is_a_500_that_describes_nothing` for the other half, which
/// is that this did not come at the cost of telling a caller about sqlite.
#[test]
fn sending_to_an_agent_that_is_not_running_is_a_client_error() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let reply = dashboard.send(
        "POST",
        "/api/v1/messages",
        &[("Authorization", authorization.as_str())],
        r#"{"text":"hello","recipient":"no-such-agent"}"#,
    )?;
    assert!(
        (400..500).contains(&reply.status),
        "an unknown recipient should be a client error, not {reply:?}"
    );
    assert_eq!(
        reply.status, 404,
        "an unknown recipient is a thing that is not there: {reply:?}"
    );
    // The point of the status is that the caller can act on it, which needs the reason too.
    assert!(
        reply.body.contains("no-such-agent"),
        "the refusal should name the recipient that was not found: {reply:?}"
    );
    // ... and must stay distinguishable from the router saying the endpoint does not exist,
    // which is the other thing this dashboard answers 404 with.
    assert!(
        !reply.body.contains("no such endpoint"),
        "an unknown recipient was reported as an unknown route: {reply:?}"
    );

    // Broadcasting still works, so the 404 is about the recipient and not about the route.
    let broadcast = dashboard.send(
        "POST",
        "/api/v1/messages",
        &[("Authorization", authorization.as_str())],
        r#"{"text":"hello"}"#,
    )?;
    assert_eq!(broadcast.status, 200, "{broadcast:?}");
    dashboard.assert_alive()
}

/// The other half of the decision: a refusal carries its reason, a fault still carries nothing.
///
/// Carrying the store's reason back for everything was the cheap fix, and it would have started
/// handing an authenticated caller sqlite's own words — table names, file paths, schema. The
/// fault here is induced for real rather than simulated: a second connection renames the
/// `messages` table out from under the running dashboard, so the next insert fails inside sqlite
/// exactly the way a corrupted or half-upgraded database would.
#[test]
fn a_database_fault_is_a_500_that_describes_nothing() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let credentials = [("Authorization", authorization.as_str())];

    // The route works before the table is taken away, so what follows is the fault and not a
    // dashboard that never answered this request in the first place.
    let healthy = dashboard.send("POST", "/api/v1/messages", &credentials, r#"{"text":"a"}"#)?;
    assert_eq!(healthy.status, 200, "{healthy:?}");

    let connection = rusqlite::Connection::open(dashboard.database())?;
    connection.busy_timeout(Duration::from_secs(30))?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.execute_batch("ALTER TABLE messages RENAME TO messages_taken_away")?;
    drop(connection);

    let reply = dashboard.send("POST", "/api/v1/messages", &credentials, r#"{"text":"b"}"#)?;
    assert_eq!(
        reply.status, 500,
        "a database that cannot answer is not the caller's mistake: {reply:?}"
    );
    assert_eq!(
        reply.body, r#"{"error":"dashboard operation failed"}"#,
        "a fault must say exactly what it always said: {reply:?}"
    );
    let served = format!("{}{}", reply.head, reply.body);
    for leak in [
        "messages_taken_away",
        "no such table",
        "sqlite",
        "SQLite",
        "SQL",
        "mesh.db",
        "Error code",
        "rusqlite",
    ] {
        assert!(
            !served.contains(leak),
            "the database fault leaked {leak:?} to the caller: {reply:?}"
        );
    }

    // A refusal on the same broken database is still a refusal: the classification is the store's
    // type, so it does not depend on which failure happens to come first.
    let refused = dashboard.send(
        "POST",
        "/api/v1/messages",
        &credentials,
        r#"{"text":"c","recipient":"no-such-agent"}"#,
    )?;
    assert_eq!(refused.status, 404, "{refused:?}");

    dashboard.assert_alive()
}

/// Every store-backed route still answers a well-formed request, now that `blocking` classifies
/// instead of flattening.
///
/// `ApiError::from_store` is one function behind eight routes, so a mistake in it would be a
/// mistake in all of them. The route inventory above proves they are reachable; this proves the
/// new error path did not turn any of them into a refusal of ordinary work.
#[test]
fn the_store_backed_routes_still_answer_ordinary_requests() -> Result<()> {
    let dashboard = Dashboard::start()?;
    let authorization = dashboard.authorization();
    let origin = dashboard.origin();
    let credentials = [
        ("Authorization", authorization.as_str()),
        ("Origin", origin.as_str()),
    ];
    for (method, path, body) in [
        ("GET", "/api/v1/snapshot", ""),
        ("POST", "/api/v1/memories", r#"{"text":"a memory"}"#),
        ("GET", "/api/v1/memories?q=memory", ""),
        ("POST", "/api/v1/messages", r#"{"text":"a broadcast"}"#),
        ("POST", "/api/v1/claims", r#"{"task":"a-task"}"#),
        ("DELETE", "/api/v1/claims/a-task", ""),
        ("GET", "/api/v1/resources", ""),
        ("POST", "/api/v1/resources", r#"{"resources":["path:src"]}"#),
        (
            "DELETE",
            "/api/v1/resources",
            r#"{"resources":["path:src"]}"#,
        ),
    ] {
        let reply = dashboard.send(method, path, &credentials, body)?;
        assert_eq!(reply.status, 200, "{method} {path} answered {reply:?}");
    }
    dashboard.assert_alive()
}
