//! The WebSocket terminal: the part of pidmesh that allocates a pseudo-terminal, runs a provider
//! binary inside it, and relays its bytes to a browser.
//!
//! Nothing here mocks the terminal. Every test starts the real `pidmesh dashboard` binary, which
//! launches a real provider process on a real PTY, and then talks to it over a real WebSocket
//! handshake and real RFC 6455 frames written by hand. The provider is
//! `tests/support/fake_provider.rs`, which is deterministic and does only what a test tells it to.
//!
//! **Isolation.** `IdeManager` cuts its worktrees under `$HOME/.pidmesh`, so every test gets its
//! own `HOME` inside its own tempdir, alongside its own port, database, git workspace and `PATH`.
//! A test that reached a launch with the real `HOME` would write into the developer's own tree.
//! The provider is reachable only because the throwaway `PATH` holds a `codex` that is really
//! `fake_provider`; nothing on the developer's `PATH` is ever launched.
//!
//! **Bounded waits.** A PTY read that blocks forever hangs CI, so every socket carries a read
//! timeout and every poll carries a deadline. No test here can wait indefinitely for anything.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tempfile::TempDir;

/// How long any single poll may run before the test fails instead of hanging.
const DEADLINE: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------------------------

/// A dashboard of its own: own port, own database, own git workspace, own `HOME`, own `PATH`.
struct Harness {
    child: Child,
    address: String,
    token: String,
    directory: TempDir,
}

impl Harness {
    fn start() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        let home = directory.path().join("home");
        let bin = directory.path().join("bin");
        fs::create_dir_all(workspace.join("src/a"))?;
        fs::create_dir_all(workspace.join("src/b"))?;
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&bin)?;
        fs::write(workspace.join("src/a/keep.txt"), "a\n")?;
        fs::write(workspace.join("src/b/keep.txt"), "b\n")?;
        git(&workspace, &["init", "--quiet", "--initial-branch", "main"])?;
        git(&workspace, &["config", "user.email", "probe@pidmesh.test"])?;
        git(&workspace, &["config", "user.name", "probe"])?;
        git(&workspace, &["add", "--all"])?;
        git(&workspace, &["commit", "--quiet", "--message", "probe"])?;
        // `IdeManager` resolves a provider by looking for an executable literally named `codex`
        // or `claude` on `PATH`. This is the only reason a managed run can start here, and the
        // only thing it can ever start.
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_pidmesh-fake-provider"),
            bin.join("codex"),
        )?;
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );

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
            .env("PATH", path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut startup = String::new();
        BufReader::new(child.stdout.take().context("dashboard stdout")?)
            .read_line(&mut startup)
            .context("dashboard never announced itself")?;
        let startup: Value = serde_json::from_str(&startup)
            .with_context(|| format!("dashboard startup line was not JSON: {startup}"))?;
        Ok(Self {
            address: startup["listening"]
                .as_str()
                .context("listening address")?
                .to_owned(),
            token: startup["dashboard"]
                .as_str()
                .and_then(|url| url.split("#token=").nth(1))
                .context("dashboard token")?
                .to_owned(),
            child,
            directory,
        })
    }

    /// Where this test's `$HOME` is, which is where `IdeManager` must cut every worktree.
    fn home(&self) -> PathBuf {
        self.directory.path().join("home")
    }

    fn origin(&self) -> String {
        format!("http://{}", self.address)
    }

    /// A request carrying the token and the dashboard's own origin: the control tier.
    fn send(&self, method: &str, path: &str, body: &str) -> Result<Reply> {
        let authorization = format!("Bearer {}", self.token);
        let origin = self.origin();
        let mut stream = TcpStream::connect(&self.address)?;
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        stream.set_write_timeout(Some(Duration::from_secs(30)))?;
        let mut head = format!("{method} {path} HTTP/1.1\r\n");
        write!(head, "Host: {}\r\n", self.address)?;
        write!(head, "Authorization: {authorization}\r\n")?;
        write!(head, "Origin: {origin}\r\n")?;
        write!(head, "Content-Length: {}\r\n", body.len())?;
        head.push_str("Content-Type: application/json\r\nConnection: close\r\n\r\n");
        stream.write_all(head.as_bytes())?;
        let _ = stream.write_all(body.as_bytes());
        let _ = stream.flush();
        let mut raw = Vec::new();
        if let Err(error) = stream.read_to_end(&mut raw)
            && raw.is_empty()
        {
            return Err(error).context("the dashboard answered nothing");
        }
        Reply::parse(&String::from_utf8_lossy(&raw))
    }

    fn json(&self, method: &str, path: &str, body: &str) -> Result<Value> {
        let reply = self.send(method, path, body)?;
        ensure!(reply.status == 200, "{method} {path} answered {reply:?}");
        serde_json::from_str(&reply.body).context("response was not JSON")
    }

    /// Start a managed run whose provider is the fake one, scripted by `boot`.
    fn launch(&self, name: &str, scope: &str, boot: &[&str]) -> Result<String> {
        let mut prompt = format!("drive the fake provider for {name}\n");
        for command in boot {
            writeln!(prompt, "@@ {command}")?;
        }
        let body = json!({
            "name": name,
            "provider": "codex",
            "workstream": name,
            "task": name,
            "prompt": prompt,
            "scopes": [scope],
        })
        .to_string();
        Ok(self.json("POST", "/api/v1/ide/sessions", &body)?["id"]
            .as_str()
            .context("launch returned no session id")?
            .to_owned())
    }

    fn session(&self, id: &str) -> Result<Value> {
        self.json("GET", "/api/v1/ide/sessions", "")?
            .as_array()
            .context("sessions is not an array")?
            .iter()
            .find(|session| session["id"] == id)
            .cloned()
            .with_context(|| format!("session {id} is gone"))
    }

    fn status(&self, id: &str) -> Result<String> {
        Ok(self.session(id)?["status"]
            .as_str()
            .context("session has no status")?
            .to_owned())
    }

    fn input(&self, id: &str, text: &str) -> Result<Reply> {
        let body = json!({ "text": format!("{text}\n") }).to_string();
        self.send("POST", &format!("/api/v1/ide/sessions/{id}/input"), &body)
    }

    fn output(&self, id: &str) -> Result<String> {
        let value = self.json(
            "GET",
            &format!("/api/v1/ide/sessions/{id}/output?after=0"),
            "",
        )?;
        Ok(value["chunks"]
            .as_array()
            .context("output has no chunks")?
            .iter()
            .filter_map(|chunk| chunk["text"].as_str())
            .collect())
    }

    fn ticket(&self, id: &str) -> Result<Value> {
        self.json(
            "POST",
            &format!("/api/v1/ide/sessions/{id}/attach-ticket"),
            "",
        )
    }

    fn ticket_value(&self, id: &str) -> Result<String> {
        Ok(self.ticket(id)?["ticket"]
            .as_str()
            .context("ticket response has no ticket")?
            .to_owned())
    }

    fn stop(&self, id: &str) -> Result<Reply> {
        self.send("DELETE", &format!("/api/v1/ide/sessions/{id}"), "")
    }

    /// Poll until the provider has written `needle`, or fail rather than wait forever.
    fn await_output(&self, id: &str, needle: &str) -> Result<String> {
        let deadline = Instant::now() + DEADLINE;
        let mut seen = String::new();
        while Instant::now() < deadline {
            seen = self.output(id)?;
            if seen.contains(needle) {
                return Ok(seen);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        bail!("{needle:?} never appeared in session output: {seen:?}")
    }

    fn await_status(&self, id: &str, wanted: &[&str]) -> Result<String> {
        let deadline = Instant::now() + DEADLINE;
        let mut status = String::new();
        while Instant::now() < deadline {
            status = self.status(id)?;
            if wanted.contains(&status.as_str()) {
                return Ok(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        bail!("session {id} stayed {status:?}, never reached one of {wanted:?}")
    }
}

/// Shut the dashboard down the way a user does, so its own cleanup runs.
///
/// `SIGTERM` reaches `shutdown_signal`, which calls `stop_all` and so kills every provider
/// process group. Killing the dashboard outright instead would orphan them.
impl Drop for Harness {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn parse(response: &str) -> Result<Self> {
        let (head, body) = response
            .split_once("\r\n\r\n")
            .context("response had no header terminator")?;
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .context("response had no status code")?;
        Ok(Self {
            status,
            body: body.to_owned(),
        })
    }
}

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "git {arguments:?} failed");
    Ok(())
}

/// Whether a pid names a process that still exists and is not a reaped corpse.
///
/// `kill(pid, 0)` alone would call a zombie alive, and a provider's `sleep` grandchild is exactly
/// the kind of process that becomes one — which would turn "the group was killed" into a pass
/// when nothing had been killed at all.
fn process_alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|result| {
            let state = String::from_utf8_lossy(&result.stdout).trim().to_owned();
            !state.is_empty() && !state.starts_with('Z')
        })
}

fn await_exit(pid: u32) -> Result<()> {
    let deadline = Instant::now() + DEADLINE;
    while Instant::now() < deadline {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("process {pid} outlived the session that owned it")
}

// ---------------------------------------------------------------------------------------------
// A hand-written RFC 6455 client
// ---------------------------------------------------------------------------------------------

const TEXT: u8 = 0x1;
const BINARY: u8 = 0x2;
const CLOSE: u8 = 0x8;
const PING: u8 = 0x9;
const PONG: u8 = 0xa;

/// A WebSocket client built on a raw socket, because pidmesh has no client dependency and a test
/// that used one would stop testing the frames the dashboard actually emits.
struct Socket {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Socket {
    /// Attempt the upgrade. Returns the HTTP status, and the socket only when it was `101`, so a
    /// refused attach is an assertable value rather than an error.
    fn attach(harness: &Harness, session: &str, ticket: &str) -> Result<(u16, Option<Self>)> {
        let stream = TcpStream::connect(&harness.address)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut writer = stream.try_clone()?;
        let request = format!(
            "GET /api/v1/ide/sessions/{session}/terminal?ticket={ticket} HTTP/1.1\r\n\
             Host: {address}\r\n\
             Origin: {origin}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
            address = harness.address,
            origin = harness.origin(),
        );
        writer.write_all(request.as_bytes())?;
        writer.flush()?;

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status: u16 = line
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .with_context(|| format!("upgrade answered {line:?}"))?;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 || header == "\r\n" {
                break;
            }
        }
        if status == 101 {
            Ok((status, Some(Self { reader, writer })))
        } else {
            Ok((status, None))
        }
    }

    /// Send one frame. A client frame must be masked, so every one here is.
    fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        let mask = [0x37_u8, 0xfa, 0x21, 0x3d];
        let mut frame = vec![0x80 | opcode];
        let length = payload.len();
        if length < 126 {
            frame.push(0x80 | u8::try_from(length)?);
        } else if let Ok(short) = u16::try_from(length) {
            frame.push(0x80 | 0x7e);
            frame.extend_from_slice(&short.to_be_bytes());
        } else {
            frame.push(0x80 | 0x7f);
            frame.extend_from_slice(&u64::try_from(length)?.to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.writer.write_all(&frame)?;
        self.writer.flush()?;
        Ok(())
    }

    fn text(&mut self, payload: &str) -> Result<()> {
        self.send(TEXT, payload.as_bytes())
    }

    /// Read one frame, or `None` when the socket closed or went quiet for its read timeout.
    fn recv(&mut self) -> Result<Option<(u8, Vec<u8>)>> {
        let mut head = [0_u8; 2];
        if self.reader.read_exact(&mut head).is_err() {
            return Ok(None);
        }
        let opcode = head[0] & 0x0f;
        let mut length = usize::from(head[1] & 0x7f);
        if length == 126 {
            let mut extended = [0_u8; 2];
            self.reader.read_exact(&mut extended)?;
            length = usize::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0_u8; 8];
            self.reader.read_exact(&mut extended)?;
            length = usize::try_from(u64::from_be_bytes(extended))?;
        }
        let mut payload = vec![0_u8; length];
        self.reader.read_exact(&mut payload)?;
        Ok(Some((opcode, payload)))
    }

    /// Read frames until `wanted` holds, collecting every byte seen on the way.
    ///
    /// Bounded twice over: by the deadline and by the socket's own read timeout, so neither a
    /// silent server nor a chatty one can keep a test running.
    fn read_until(
        &mut self,
        label: &str,
        wanted: impl Fn(&Seen) -> bool,
        limit: Duration,
    ) -> Result<Seen> {
        let deadline = Instant::now() + limit;
        let mut seen = Seen::default();
        while Instant::now() < deadline {
            let Some((opcode, payload)) = self.recv()? else {
                break;
            };
            match opcode {
                BINARY => seen.bytes.extend_from_slice(&payload),
                TEXT => seen
                    .text
                    .push(String::from_utf8_lossy(&payload).into_owned()),
                PONG => seen.pongs += 1,
                CLOSE => {
                    seen.closed = true;
                    if wanted(&seen) {
                        return Ok(seen);
                    }
                    bail!("the server closed the socket before {label}: {seen:?}");
                }
                _ => {}
            }
            if wanted(&seen) {
                return Ok(seen);
            }
        }
        bail!("never saw {label}: {seen:?}")
    }
}

/// Everything a client has observed on a socket so far.
#[derive(Default)]
struct Seen {
    bytes: Vec<u8>,
    closed: bool,
    pongs: usize,
    text: Vec<String>,
}

impl Seen {
    fn stream(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

impl std::fmt::Debug for Seen {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Seen")
            .field("bytes", &self.bytes.len())
            .field(
                "tail",
                &self.stream().chars().rev().take(200).collect::<String>(),
            )
            .field("closed", &self.closed)
            .field("pongs", &self.pongs)
            .field("text", &self.text)
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Ticket security
// ---------------------------------------------------------------------------------------------

/// The terminal upgrade is the one `/api/v1/*` route the session token does not gate, so its
/// ticket is the entire boundary: spend it once and it is gone.
#[test]
fn a_terminal_ticket_cannot_be_redeemed_twice() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("single-use", "src/a", &["say hello"])?;
    let ticket = harness.ticket_value(&session)?;

    let (first, socket) = Socket::attach(&harness, &session, &ticket)?;
    assert_eq!(first, 101, "a fresh ticket was refused");
    drop(socket);

    let (second, _) = Socket::attach(&harness, &session, &ticket)?;
    assert_eq!(second, 409, "a spent ticket was accepted a second time");
    // And it is not merely the live socket that blocks the replay: the first one is long gone.
    let (third, _) = Socket::attach(&harness, &session, &ticket)?;
    assert_eq!(third, 409, "a spent ticket came back to life");

    let (unknown, _) = Socket::attach(&harness, &session, "0123456789abcdef")?;
    assert_eq!(unknown, 409, "a ticket nobody minted was accepted");
    Ok(())
}

/// A ticket names one session. Presenting it against another must not attach, must not leak that
/// the other session exists, and must still burn the ticket.
#[test]
fn a_ticket_minted_for_one_session_cannot_attach_to_another() -> Result<()> {
    let harness = Harness::start()?;
    let first = harness.launch("holder", "src/a", &["say first"])?;
    let second = harness.launch("target", "src/b", &["say second"])?;
    harness.await_output(&first, "say:first")?;
    harness.await_output(&second, "say:second")?;

    let ticket = harness.ticket_value(&first)?;
    let (crossed, _) = Socket::attach(&harness, &second, &ticket)?;
    assert_eq!(crossed, 409, "a ticket for another session attached");

    // The redemption attempt consumed it, so the holder cannot use it either.
    let (replayed, _) = Socket::attach(&harness, &first, &ticket)?;
    assert_eq!(replayed, 409, "a ticket survived being spent on a mismatch");

    // Nothing about the failed attach disturbed either session.
    let fresh = harness.ticket_value(&second)?;
    let (upgraded, socket) = Socket::attach(&harness, &second, &fresh)?;
    assert_eq!(upgraded, 101);
    let mut socket = socket.context("no socket")?;
    let seen = socket.read_until(
        "the second session's own backlog",
        |seen| seen.stream().contains("say:second"),
        DEADLINE,
    )?;
    assert!(
        !seen.stream().contains("say:first"),
        "sessions bled: {seen:?}"
    );
    Ok(())
}

/// The ticket the HTTP surface hands out really does carry the 30s lifetime `TICKET_TTL_MS`
/// declares. Expiry itself is proven against the clock in `src/ide.rs`, where a grant can be
/// dated into the past without a test sleeping for half a minute.
#[test]
fn a_ticket_is_minted_with_a_short_lifetime() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("ttl", "src/a", &["say ttl"])?;
    let minted = harness.ticket(&session)?;
    let expires = minted["expires_at"].as_i64().context("expires_at")?;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )?;
    assert!(
        (0..=30_000).contains(&(expires - now)),
        "a ticket minted now expires in {}ms",
        expires - now
    );
    assert_eq!(
        minted["ticket"].as_str().unwrap_or_default().len(),
        32,
        "a ticket must be a full 128-bit value, not something guessable"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------------------------

/// Launch, attach, type, stop — and then the two things a stop has to be: input refused, and no
/// process left behind. The provider starts a `sleep` grandchild precisely so the second claim is
/// about the process *group* rather than only the process pidmesh happens to hold a handle to.
#[test]
fn a_stopped_session_refuses_input_and_leaves_no_process_behind() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("lifecycle", "src/a", &["spawn 90"])?;
    let printed = harness.await_output(&session, "spawn:")?;
    let grandchild: u32 = printed
        .split("spawn:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .context("provider did not report its grandchild")?
        .parse()?;
    let view = harness.session(&session)?;
    let worktree = view["worktree"].as_str().context("worktree")?;
    assert!(
        worktree.starts_with(harness.home().to_str().context("home")?),
        "a managed worktree was cut outside this test's HOME: {worktree}"
    );
    let provider = u32::try_from(view["pid"].as_u64().context("session has no pid")?)?;
    assert!(process_alive(provider) && process_alive(grandchild));

    assert_eq!(harness.input(&session, "say alive")?.status, 200);
    harness.await_output(&session, "say:alive")?;

    assert_eq!(harness.stop(&session)?.status, 200);
    let settled = harness.await_status(&session, &["stopped", "failed"])?;
    assert_eq!(settled, "stopped", "a stop must be reported as a stop");

    let refused = harness.input(&session, "say zombie")?;
    assert_eq!(refused.status, 409, "a stopped session took input");
    assert!(
        refused.body.contains("not running"),
        "unexpected refusal: {refused:?}"
    );

    await_exit(provider).context("the provider process outlived its session")?;
    await_exit(grandchild).context("a grandchild outlived the process group it was in")?;
    Ok(())
}

/// A provider that is gone before anyone can attach must be reported, not hung on, and its
/// terminal must tell a client that it is looking at something closed.
#[test]
fn a_provider_that_exits_immediately_is_reported_and_closes_its_terminal() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("instant", "src/a", &["exit 3"])?;
    assert_eq!(harness.await_status(&session, &["failed"])?, "failed");
    assert_eq!(
        harness.session(&session)?["exit_code"],
        3,
        "the provider's exit status was not preserved"
    );

    // The run survives its agent on purpose: its worktree still backs the review panes.
    let ticket = harness.ticket_value(&session)?;
    let (status, socket) = Socket::attach(&harness, &session, &ticket)?;
    assert_eq!(status, 101, "a finished run must still be inspectable");
    let mut socket = socket.context("no socket")?;
    let seen = socket.read_until(
        "the closed notice",
        |seen| seen.text.iter().any(|frame| frame.contains("\"closed\"")),
        DEADLINE,
    )?;
    assert!(seen.text.iter().any(|frame| frame.contains("\"status\"")));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Backlog, ordering and the lag gap
// ---------------------------------------------------------------------------------------------

/// A client that attaches after the fact is handed the scrollback first, and then the live
/// stream, with no seam between them.
#[test]
fn a_late_client_is_handed_the_backlog_before_live_output() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("backlog", "src/a", &["say before-attach"])?;
    harness.await_output(&session, "say:before-attach")?;

    let ticket = harness.ticket_value(&session)?;
    let (status, socket) = Socket::attach(&harness, &session, &ticket)?;
    assert_eq!(status, 101);
    let mut socket = socket.context("no socket")?;
    let replayed = socket.read_until("the backlog", |seen| !seen.text.is_empty(), DEADLINE)?;
    assert!(
        replayed.stream().contains("say:before-attach"),
        "the backlog was not replayed: {replayed:?}"
    );

    harness.input(&session, "say after-attach")?;
    let live = socket.read_until(
        "live output",
        |seen| seen.stream().contains("say:after-attach"),
        DEADLINE,
    )?;
    assert!(!live.closed);
    Ok(())
}

/// A client that keeps up loses nothing: every chunk arrives, in order.
///
/// The counter is the assertion -- a dropped or reordered chunk shows up as a missing or
/// out-of-sequence `n:`, which no whitespace comparison would catch. The count stays under the
/// 256-slot broadcast ring on purpose, because overrunning it is a *different* behaviour with a
/// different contract: the client is told, and that is what the lag test asserts.
#[test]
fn a_client_that_keeps_up_loses_no_chunk() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("ordering", "src/a", &["say attached"])?;
    let ticket = harness.ticket_value(&session)?;
    let (_, socket) = Socket::attach(&harness, &session, &ticket)?;
    let mut socket = socket.context("no socket")?;
    socket.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;

    harness.input(&session, "count 200")?;
    let seen = socket.read_until(
        "the last counted line",
        |seen| seen.stream().contains("n:200\r"),
        DEADLINE,
    )?;
    assert!(
        !seen.text.iter().any(|frame| frame.contains("\"gap\"")),
        "a client that never stopped reading was told it fell behind: {seen:?}"
    );
    let stream = seen.stream();
    let counted = stream
        .split("n:")
        .skip(1)
        .filter_map(|rest| {
            rest.split(|character: char| !character.is_ascii_digit())
                .next()
                .and_then(|digits| digits.parse::<u64>().ok())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        counted,
        (1..=200).collect::<Vec<_>>(),
        "the relayed stream lost or reordered chunks"
    );
    Ok(())
}

/// A client too slow to drain its socket is told it missed bytes, rather than quietly handed a
/// stream with a hole in it — and the scrollback it could still ask for stays bounded.
#[test]
fn a_client_that_falls_behind_is_told_and_the_backlog_stays_bounded() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("lag", "src/a", &["say attached"])?;
    let ticket = harness.ticket_value(&session)?;
    let (_, socket) = Socket::attach(&harness, &session, &ticket)?;
    let mut socket = socket.context("no socket")?;
    socket.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;

    // Write far more than the 256-chunk broadcast ring while this client reads nothing, so the
    // socket task's own receiver is the thing that falls behind.
    harness.input(&session, "flood 12288")?;
    harness.await_output(&session, "flood:done")?;
    std::thread::sleep(Duration::from_millis(500));

    let seen = socket.read_until(
        "the gap notice",
        |seen| seen.text.iter().any(|frame| frame.contains("\"gap\"")),
        Duration::from_secs(60),
    )?;
    assert!(!seen.closed, "a lagging client was disconnected: {seen:?}");

    // 12 MiB went through a 1 MiB scrollback, so the buffer must have been trimmed rather than
    // grown: fewer chunks are retained than were ever produced.
    let output = harness.json(
        "GET",
        &format!("/api/v1/ide/sessions/{session}/output?after=0"),
        "",
    )?;
    let retained = output["chunks"].as_array().context("chunks")?.len();
    let produced = output["next_sequence"].as_u64().context("next_sequence")?;
    assert!(
        u64::try_from(retained)? < produced,
        "the scrollback kept every one of {produced} chunks: it is not bounded"
    );
    assert!(
        harness.output(&session)?.len() < 4 * 1024 * 1024,
        "the scrollback grew past its byte budget"
    );
    assert_eq!(harness.status(&session)?, "running");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// A resize must reach the kernel's idea of the window, not just a field somewhere, because that
/// is what the program inside the PTY actually reads.
#[test]
fn a_resize_reaches_the_window_the_provider_sees() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("resize", "src/a", &["size"])?;
    harness.await_output(&session, "size:30x120")?;

    let ticket = harness.ticket_value(&session)?;
    let (_, socket) = Socket::attach(&harness, &session, &ticket)?;
    let mut socket = socket.context("no socket")?;
    socket.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;

    socket.text(&json!({"type": "resize", "cols": 100, "rows": 40}).to_string())?;
    socket.send(BINARY, b"size\n")?;
    let seen = socket.read_until(
        "the resized window",
        |seen| seen.stream().contains("size:40x100"),
        DEADLINE,
    )?;
    assert!(!seen.closed);

    // Out-of-range resizes are refused by `IdeManager`, and refusing one may not cost the client
    // its socket or the session its window.
    socket.text(&json!({"type": "resize", "cols": 1, "rows": 1}).to_string())?;
    socket.text(&json!({"type": "resize", "cols": 9000, "rows": 9000}).to_string())?;
    socket.send(BINARY, b"size\n")?;
    // A fresh `Seen` per call, so this is the window reported *after* both refusals.
    socket.read_until(
        "the window after two refused resizes",
        |seen| seen.stream().contains("size:40x100"),
        DEADLINE,
    )?;
    Ok(())
}

/// Frames a browser would never send: junk text, a ping, a pong, a resize with the wrong shape.
/// None of them may panic the server, close the session, or stop the terminal working.
#[test]
fn junk_frames_do_not_disturb_the_session() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("junk", "src/a", &["say attached"])?;
    let ticket = harness.ticket_value(&session)?;
    let (_, socket) = Socket::attach(&harness, &session, &ticket)?;
    let mut socket = socket.context("no socket")?;
    socket.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;

    socket.text("not json at all")?;
    socket.text("{")?;
    socket.text(&json!({"type": "resize"}).to_string())?;
    socket.text(&json!({"type": "resize", "cols": "wide", "rows": 40}).to_string())?;
    socket.text(&json!({"type": "unheard-of"}).to_string())?;
    socket.text(&json!({"type": "resize", "cols": 1_000_000, "rows": 40}).to_string())?;
    socket.send(PING, b"are you there")?;
    socket.send(PONG, b"unsolicited")?;
    // Bytes that are not valid UTF-8 go to the PTY as a lossy string rather than as an error.
    socket.send(BINARY, &[0xff, 0xfe, b'\n'])?;
    socket.send(BINARY, b"say survivor\n")?;

    let seen = socket.read_until(
        "the terminal still working",
        |seen| seen.stream().contains("say:survivor"),
        DEADLINE,
    )?;
    assert_eq!(seen.pongs, 1, "a ping was not answered: {seen:?}");
    assert!(!seen.closed);
    assert_eq!(harness.status(&session)?, "running");
    Ok(())
}

/// Two panes on one terminal. Both see everything the provider writes, and neither one's input
/// is swallowed by the other — a shared terminal is shared, not stolen.
#[test]
fn two_clients_share_one_terminal_without_stealing_from_each_other() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("shared", "src/a", &["say attached"])?;
    let first_ticket = harness.ticket_value(&session)?;
    let second_ticket = harness.ticket_value(&session)?;
    assert_ne!(first_ticket, second_ticket, "two tickets collided");

    let (_, first) = Socket::attach(&harness, &session, &first_ticket)?;
    let (_, second) = Socket::attach(&harness, &session, &second_ticket)?;
    let mut first = first.context("no first socket")?;
    let mut second = second.context("no second socket")?;
    first.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;
    second.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;

    first.send(BINARY, b"say from-first\n")?;
    second.send(BINARY, b"say from-second\n")?;

    for (label, socket) in [("first", &mut first), ("second", &mut second)] {
        let seen = socket.read_until(
            "both clients' output",
            |seen| {
                seen.stream().contains("say:from-first")
                    && seen.stream().contains("say:from-second")
            },
            DEADLINE,
        )?;
        assert!(!seen.closed, "the {label} client was cut off: {seen:?}");
    }
    assert_eq!(harness.status(&session)?, "running");
    Ok(())
}

/// A provider writing bytes that are not UTF-8 is ordinary — curses redraws and progress spinners
/// do it constantly. Neither the polling route nor the socket may fail on them.
#[test]
fn output_that_is_not_utf8_reaches_both_the_socket_and_the_poll_route() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("bytes", "src/a", &["say attached"])?;
    let ticket = harness.ticket_value(&session)?;
    let (_, socket) = Socket::attach(&harness, &session, &ticket)?;
    let mut socket = socket.context("no socket")?;
    socket.read_until("the attach status", |seen| !seen.text.is_empty(), DEADLINE)?;

    socket.send(BINARY, b"binary\n")?;
    let seen = socket.read_until(
        "the binary marker",
        |seen| seen.stream().contains("binary:done"),
        DEADLINE,
    )?;
    assert!(
        seen.bytes.windows(2).any(|pair| pair == [0xff, 0xfe]),
        "the socket mangled bytes it was supposed to relay verbatim"
    );
    // The polling route is lossy by construction, which is fine; failing is not.
    assert!(
        harness
            .await_output(&session, "binary:done")?
            .contains('\u{fffd}')
    );
    assert_eq!(harness.status(&session)?, "running");
    Ok(())
}

/// A provider that does not die on `SIGTERM` must still be reaped.
///
/// `stop` is the only way an operator can end one run, and it sends nothing but `SIGTERM`. A
/// provider that ignores it — which a terminal program with its own interrupt handling very
/// plausibly does — would otherwise keep its PTY, its process group and its worktree forever,
/// while the dashboard showed the run as `stopping` and refused its input. `stop_all` escalates
/// at shutdown; `stop` has to do the same, or the leak just waits for the dashboard to exit.
#[test]
fn a_provider_that_ignores_sigterm_is_still_reaped() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch("stubborn", "src/a", &["ignore-term", "spawn 90"])?;
    let printed = harness.await_output(&session, "spawn:")?;
    assert!(printed.contains("ignore-term:on"));
    let grandchild: u32 = printed
        .split("spawn:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .context("provider did not report its grandchild")?
        .parse()?;
    let provider = u32::try_from(
        harness.session(&session)?["pid"]
            .as_u64()
            .context("session has no pid")?,
    )?;

    assert_eq!(harness.stop(&session)?.status, 200);
    assert_eq!(
        harness.await_status(&session, &["stopped", "failed"])?,
        "stopped"
    );
    await_exit(provider).context("a provider that ignored SIGTERM was never escalated on")?;
    await_exit(grandchild).context("the stubborn provider's group outlived it")?;
    Ok(())
}

/// What a finished run leaves behind: a review that names the files it touched, and a refusal to
/// merge the ones it was not allowed to touch.
///
/// This replaces the in-crate test that proved the same thing by launching a provider with the
/// developer's own `$HOME`, which is where `IdeManager` cuts its worktrees. Here the worktree is
/// inside the tempdir, and the assertion below says so.
#[test]
fn a_finished_run_reviews_its_changes_and_refuses_an_out_of_scope_merge() -> Result<()> {
    let harness = Harness::start()?;
    let session = harness.launch(
        "scope-check",
        "src/a",
        &[
            "write src/a/allowed.txt",
            "write README.generated",
            "say finished",
            "exit 0",
        ],
    )?;
    assert_eq!(
        harness.await_status(&session, &["ready_for_review"])?,
        "ready_for_review"
    );

    let worktree = harness.session(&session)?["worktree"]
        .as_str()
        .context("worktree")?
        .to_owned();
    assert!(
        worktree.starts_with(harness.home().to_str().context("home")?),
        "a managed worktree was cut outside this test's HOME: {worktree}"
    );

    let review = harness.json("GET", &format!("/api/v1/ide/sessions/{session}/review"), "")?;
    assert_eq!(review["scope_valid"], false);
    assert_eq!(review["out_of_scope"], json!(["README.generated"]));

    let refused = harness.send(
        "POST",
        &format!("/api/v1/ide/sessions/{session}/approve"),
        &json!({"commit_message": "test: managed change"}).to_string(),
    )?;
    assert_eq!(refused.status, 409);
    assert!(
        refused.body.contains("out-of-scope"),
        "unexpected refusal: {refused:?}"
    );
    Ok(())
}
