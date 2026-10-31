//! What an agent actually gets back when it calls a pidmesh MCP tool.
//!
//! `tests/mcp.rs` proves the server starts, negotiates a session, and lists its tool names. It
//! says nothing about what the tools do. This file covers the behaviour, because the MCP surface
//! is the interface agents use unattended: nobody reads the output, so a field that changes name
//! or type, a tool that refuses a call it documents, or a panic that returns an opaque internal
//! error are all silent breakages.
//!
//! Four things are asserted here. The response shape of every tool, field by field. The
//! collision story the product is built on, driven end to end through MCP over two real linked
//! worktrees rather than through the library. The failure modes, which must come back as tool
//! errors the caller can read and must leave the session usable. And two sessions sharing one
//! database, where the integration lease has to be exclusive and neither side may fall over from
//! SQLite contention.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tempfile::TempDir;

/// Nothing here legitimately waits this long. A stdio protocol read with nothing to read blocks
/// forever, so every wait is bounded and a stuck server fails the test instead of hanging the
/// suite behind it.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------------------------
// Session harness
// ---------------------------------------------------------------------------------------------

/// Build a JSON object, moving the values in rather than copying them.
fn object<const N: usize>(fields: [(&str, Value); N]) -> Value {
    Value::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

/// One live MCP server, talked to over its real stdio transport.
struct Session {
    label: String,
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<String>,
    diagnostics: Arc<Mutex<String>>,
    next_id: i64,
}

impl Session {
    /// Start a server and complete the handshake.
    ///
    /// `checkout` becomes the process directory, which is what the server observes when
    /// `PIDMESH_WORKSPACE` is absent. Leaving it absent is what lets several linked worktrees
    /// share one mesh, so the collision tests depend on it.
    fn start(
        label: &str,
        database: &Path,
        checkout: &Path,
        workspace: Option<&Path>,
    ) -> Result<Self> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pidmesh-mcp"));
        command
            .current_dir(checkout)
            .env("PIDMESH_DB", database)
            .env("PIDMESH_AGENT_NAME", label)
            // An inherited value from the surrounding run would silently repoint the mesh.
            .env_remove("PIDMESH_WORKSPACE")
            .env_remove("PIDMESH_AGENT_ID")
            .env_remove("PIDMESH_PROVIDER")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(workspace) = workspace {
            command.env("PIDMESH_WORKSPACE", workspace);
        }
        let mut child = command.spawn()?;

        let stdin = child.stdin.take().context("missing MCP stdin")?;
        let stdout = child.stdout.take().context("missing MCP stdout")?;
        let (sender, replies) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        });

        // Drained on its own thread: an undrained stderr pipe is how this project deadlocked
        // once already, and a panic message is exactly the thing worth being able to read back.
        let mut stderr = child.stderr.take().context("missing MCP stderr")?;
        let diagnostics = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&diagnostics);
        thread::spawn(move || {
            let mut buffer = String::new();
            let _ = stderr.read_to_string(&mut buffer);
            if let Ok(mut sink) = sink.lock() {
                sink.push_str(&buffer);
            }
        });

        let mut session = Self {
            label: label.to_owned(),
            child,
            stdin: Some(stdin),
            replies,
            diagnostics,
            next_id: 1,
        };

        let initialized = session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "pidmesh-tool-test", "version": "1.0.0"}
            }),
        )?;
        ensure!(
            initialized["result"]["serverInfo"]["name"] == "PidMesh",
            "unexpected server identity: {initialized}"
        );
        session.notify("notifications/initialized", json!({}))?;
        Ok(session)
    }

    /// Everything the server wrote to stderr so far.
    fn diagnostics(&self) -> String {
        self.diagnostics
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn write_line(&mut self, message: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("session already closed")?;
        writeln!(stdin, "{message}")?;
        stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_line(&object([
            ("jsonrpc", json!("2.0")),
            ("method", json!(method)),
            ("params", params),
        ]))
    }

    /// Send one request and wait for the reply carrying its id.
    ///
    /// The server answers requests concurrently, so replies are matched by id rather than by
    /// arrival order; anything else that turns up (notifications, a stray frame) is skipped.
    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write_line(&object([
            ("jsonrpc", json!("2.0")),
            ("id", json!(id)),
            ("method", json!(method)),
            ("params", params),
        ]))?;
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let line = match self.replies.recv_timeout(remaining) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => bail!(
                    "{} did not answer {method} within {REPLY_TIMEOUT:?}; stderr: {}",
                    self.label,
                    self.diagnostics()
                ),
                Err(RecvTimeoutError::Disconnected) => bail!(
                    "{} closed its stdout before answering {method}; stderr: {}",
                    self.label,
                    self.diagnostics()
                ),
            };
            let message: Value = serde_json::from_str(&line)
                .with_context(|| format!("invalid MCP frame: {line}"))?;
            if message["id"] == json!(id) {
                return Ok(message);
            }
        }
    }

    fn call(&mut self, tool: &str, arguments: Value) -> Result<Value> {
        self.request(
            "tools/call",
            object([("name", json!(tool)), ("arguments", arguments)]),
        )
    }

    /// Call a tool that is expected to succeed and return its structured content.
    fn ok(&mut self, tool: &str, arguments: Value) -> Result<Value> {
        let response = self.call(tool, arguments)?;
        ensure!(
            response.get("error").is_none(),
            "{tool} answered with a protocol error: {response}"
        );
        let result = &response["result"];
        ensure!(result["isError"] == json!(false), "{tool} failed: {result}");
        ensure!(
            !result["structuredContent"].is_null(),
            "{tool} returned no structured content: {result}"
        );
        Ok(result["structuredContent"].clone())
    }

    /// Call a tool that is expected to fail and return the message the agent would read.
    ///
    /// Both failure shapes count: a tool that reported an error through its structured result,
    /// and a call rejected before the tool ran. Neither may be a protocol-level error, because
    /// the point is that the session stays usable.
    fn failure(&mut self, tool: &str, arguments: Value) -> Result<String> {
        let response = self.call(tool, arguments)?;
        ensure!(
            response.get("error").is_none(),
            "{tool} answered with a protocol error rather than a tool error: {response}"
        );
        let result = &response["result"];
        ensure!(
            result["isError"] == json!(true),
            "{tool} unexpectedly succeeded: {result}"
        );
        let message = result["structuredContent"]["error"]
            .as_str()
            .or_else(|| result["content"][0]["text"].as_str())
            .with_context(|| format!("{tool} reported a failure with no message: {result}"))?;
        Ok(message.to_owned())
    }

    /// This session's own agent id, as the store knows it.
    fn agent_id(&mut self) -> Result<String> {
        let status = self.ok("mesh_status", json!({}))?;
        let id = status["agents"]
            .as_array()
            .context("mesh_status returned no agent list")?
            .iter()
            .find(|agent| agent["name"] == json!(self.label.as_str()))
            .and_then(|agent| agent["id"].as_str())
            .with_context(|| format!("{} is not in its own roster", self.label))?;
        Ok(id.to_owned())
    }

    /// Close stdin and require a clean exit, the way a well-behaved client disconnects.
    fn shut_down(mut self) -> Result<()> {
        self.stdin = None;
        let status = self.child.wait()?;
        ensure!(
            status.success(),
            "{} exited with {status}; stderr: {}",
            self.label,
            self.diagnostics()
        );
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct Workspace {
    directory: TempDir,
    root: PathBuf,
    database: PathBuf,
}

impl Workspace {
    /// A plain directory with no repository, for the tools that do not need git.
    fn bare() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("workspace");
        fs::create_dir(&root)?;
        // The store canonicalizes every path it records, and a macOS temporary directory lives
        // behind a symlink, so an uncanonicalized root would never match what comes back.
        let root = root.canonicalize()?;
        let database = directory.path().join("mesh.db");
        Ok(Self {
            directory,
            root,
            database,
        })
    }

    /// A repository on `main` with one commit, ready for linked worktrees.
    fn repository() -> Result<Self> {
        let workspace = Self::bare()?;
        let root = &workspace.root;
        git(root, &["init", "-b", "main"])?;
        git(root, &["config", "user.email", "pidmesh@example.com"])?;
        git(root, &["config", "user.name", "PidMesh Test"])?;
        let shared = (1..=12)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(root.join("shared.txt"), shared + "\n")?;
        fs::write(root.join("legacy.txt"), "legacy\n")?;
        git(root, &["add", "-A"])?;
        git(root, &["commit", "-m", "initial"])?;
        Ok(workspace)
    }

    /// A linked worktree on its own branch, sharing the primary repository's ref store.
    fn worktree(&self, name: &str) -> Result<PathBuf> {
        let path = self
            .directory
            .path()
            .join(format!("worktree-{name}"))
            .to_string_lossy()
            .into_owned();
        git(
            &self.root,
            &["worktree", "add", "-b", &format!("feat/{name}"), &path],
        )?;
        Ok(PathBuf::from(path).canonicalize()?)
    }

    fn advance_main(&self, note: &str) -> Result<()> {
        fs::write(self.root.join("advanced.txt"), format!("{note}\n"))?;
        git(&self.root, &["add", "-A"])?;
        git(&self.root, &["commit", "-m", note])?;
        Ok(())
    }
}

/// Replace one zero-indexed line of a file, keeping the rest byte for byte.
fn rewrite_line(path: &Path, index: usize, text: &str) -> Result<()> {
    let mut lines: Vec<String> = fs::read_to_string(path)?
        .lines()
        .map(ToOwned::to_owned)
        .collect();
    text.clone_into(lines.get_mut(index).context("line index out of range")?);
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(path, body)?;
    Ok(())
}

/// The reported collision for one path, if it is contested at all.
fn contested<'a>(report: &'a Value, path: &str) -> Option<&'a Value> {
    report
        .as_array()?
        .iter()
        .find(|entry| entry["path"] == json!(path))
}

/// The blockers of one kind in a merge-readiness answer.
fn blockers<'a>(readiness: &'a Value, code: &str) -> Vec<&'a Value> {
    readiness["blockers"]
        .as_array()
        .map(|blockers| {
            blockers
                .iter()
                .filter(|blocker| blocker["code"] == json!(code))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// 1. The contract agents parse
// ---------------------------------------------------------------------------------------------

/// Every memory, messaging, lease, and event tool, asserted field by field.
///
/// An agent reads these responses programmatically. A renamed key or a number that becomes a
/// string is a breaking change, and before this test nothing in the suite would have noticed one.
#[test]
fn coordination_tools_answer_with_the_shape_agents_parse() -> Result<()> {
    let workspace = Workspace::bare()?;
    let mut writer = Session::start(
        "shape-writer",
        &workspace.database,
        &workspace.root,
        Some(&workspace.root),
    )?;
    let mut reader = Session::start(
        "shape-reader",
        &workspace.database,
        &workspace.root,
        Some(&workspace.root),
    )?;
    let writer_id = writer.agent_id()?;

    assert_roster_shape(&mut writer, &workspace, &writer_id)?;
    assert_memory_shape(&mut writer, &mut reader, &writer_id)?;
    assert_messaging_shape(&mut writer, &mut reader, &writer_id)?;
    assert_task_lease_shape(&mut writer, &mut reader, &writer_id)?;
    assert_resource_lease_shape(&mut writer, &mut reader, &writer_id)?;
    assert_event_shape(&mut writer)?;

    reader.shut_down()?;
    writer.shut_down()?;
    Ok(())
}

/// `mesh_status`: who is in this mesh, and what the caller may assume about each entry.
fn assert_roster_shape(writer: &mut Session, workspace: &Workspace, writer_id: &str) -> Result<()> {
    let status = writer.ok("mesh_status", json!({}))?;
    assert_eq!(status["workspace"], json!(workspace.root.to_string_lossy()));
    assert!(status["workspace_id"].is_string());
    assert_eq!(
        status["agents"]
            .as_array()
            .context("mesh_status lost its agent list")?
            .len(),
        2,
        "both live sessions belong to one mesh: {status}"
    );
    let me = status["agents"]
        .as_array()
        .context("mesh_status lost its agent list")?
        .iter()
        .find(|agent| agent["id"] == json!(writer_id))
        .context("mesh_status omitted the calling agent")?;
    assert_eq!(me["status"], json!("running"));
    assert_eq!(me["provider"], json!("mcp"));
    assert_eq!(me["pid_alive"], json!(true));
    assert!(me["pid"].is_number());
    assert!(
        me["capabilities"]
            .as_array()
            .context("missing capabilities")?
            .contains(&json!("convergence")),
        "the convergence capability is how a client knows these tools exist: {me}"
    );
    Ok(())
}

/// `remember` and `recall`: a memory one agent writes, read back by another.
fn assert_memory_shape(writer: &mut Session, reader: &mut Session, writer_id: &str) -> Result<()> {
    let remembered = writer.ok(
        "remember",
        json!({
            "content": "the integration branch is main",
            "kind": "decision",
            "key": "integration-branch",
            "importance": 0.9
        }),
    )?;
    assert!(remembered["memory_id"].is_number());
    assert_eq!(remembered["kind"], json!("decision"));
    assert_eq!(remembered["key"], json!("integration-branch"));

    let recalled = reader.ok("recall", json!({"query": "integration", "limit": 5}))?;
    let recalled = recalled.as_array().context("recall is an array")?;
    assert_eq!(recalled.len(), 1, "one memory matches: {recalled:?}");
    let memory = &recalled[0];
    assert_eq!(memory["content"], json!("the integration branch is main"));
    assert_eq!(memory["kind"], json!("decision"));
    assert_eq!(memory["key"], json!("integration-branch"));
    assert_eq!(memory["agent_id"], json!(writer_id));
    assert_eq!(memory["agent_name"], json!("shape-writer"));
    assert_eq!(memory["importance"], json!(0.9));
    assert!(memory["id"].is_number());
    assert!(memory["created_at"].is_number());
    Ok(())
}

/// `send` and `inbox`: a broadcast, its delivery, and its acknowledgement.
fn assert_messaging_shape(
    writer: &mut Session,
    reader: &mut Session,
    writer_id: &str,
) -> Result<()> {
    let sent = writer.ok(
        "send",
        json!({"message": "rebase before merging", "correlation_id": "merge-1"}),
    )?;
    assert!(sent["message_id"].is_number());
    assert_eq!(sent["recipient"], json!("*"), "the default is a broadcast");

    let inbox = reader.ok("inbox", json!({"acknowledge": true}))?;
    assert_eq!(inbox["acknowledged"], json!(1));
    let messages = inbox["messages"]
        .as_array()
        .context("inbox messages is an array")?;
    assert_eq!(
        messages.len(),
        1,
        "the broadcast arrived once: {messages:?}"
    );
    // `send` takes `message` and `inbox` hands it back as `body`. The asymmetry is the shipped
    // contract, so it is pinned here rather than quietly assumed either way.
    assert_eq!(messages[0]["body"], json!("rebase before merging"));
    assert_eq!(messages[0]["correlation_id"], json!("merge-1"));
    assert_eq!(messages[0]["sender_id"], json!(writer_id));
    assert_eq!(messages[0]["sender_name"], json!("shape-writer"));
    assert_eq!(
        messages[0]["recipient_id"],
        Value::Null,
        "a broadcast is addressed to nobody in particular: {messages:?}"
    );
    assert!(messages[0]["id"].is_number());
    assert_eq!(
        reader.ok("inbox", json!({}))?["messages"],
        json!([]),
        "acknowledging must actually clear the message"
    );
    Ok(())
}

/// `claim` and `release`: the task lease, and the fact that it is not stealable.
fn assert_task_lease_shape(
    writer: &mut Session,
    reader: &mut Session,
    writer_id: &str,
) -> Result<()> {
    let claim = writer.ok(
        "claim",
        json!({"task": "render-pass", "lease_seconds": 120, "detail": "owning the renderer"}),
    )?;
    assert_eq!(claim["acquired"], json!(true));
    assert_eq!(claim["task_key"], json!("render-pass"));
    assert_eq!(claim["agent_id"], json!(writer_id));
    assert_eq!(claim["agent_name"], json!("shape-writer"));
    assert_eq!(claim["detail"], json!("owning the renderer"));
    let claimed_at = claim["claimed_at"]
        .as_i64()
        .context("claimed_at is a time")?;
    let expires_at = claim["lease_expires_at"]
        .as_i64()
        .context("lease_expires_at is a time")?;
    assert_eq!(
        expires_at - claimed_at,
        120_000,
        "the lease window is the one that was asked for"
    );

    let contended = reader.ok(
        "claim",
        json!({"task": "render-pass", "lease_seconds": 120}),
    )?;
    assert_eq!(
        contended["acquired"],
        json!(false),
        "a live lease is not stealable: {contended}"
    );
    assert_eq!(contended["agent_id"], json!(writer_id));

    assert_eq!(
        reader.ok("release", json!({"task": "render-pass"}))?,
        json!({"released": false}),
        "releasing somebody else's lease must be a no-op, not a theft"
    );
    assert_eq!(
        writer.ok("release", json!({"task": "render-pass"}))?,
        json!({"released": true})
    );
    Ok(())
}

/// `reserve_resources` and `release_resources`: exclusive paths, and what a loser is told.
fn assert_resource_lease_shape(
    writer: &mut Session,
    reader: &mut Session,
    writer_id: &str,
) -> Result<()> {
    let reserved = writer.ok(
        "reserve_resources",
        json!({"resources": ["path:src/mcp.rs", "port:4399"], "lease_seconds": 60}),
    )?;
    assert_eq!(reserved["acquired"], json!(true));
    assert_eq!(reserved["conflicts"], json!([]));
    assert_eq!(
        reserved["resources"],
        json!(["path:src/mcp.rs", "port:4399"])
    );
    assert!(reserved["lease_expires_at"].is_number());

    let refused = reader.ok(
        "reserve_resources",
        json!({"resources": ["path:src/mcp.rs"], "lease_seconds": 60}),
    )?;
    assert_eq!(
        refused["acquired"],
        json!(false),
        "a reserved path is exclusive: {refused}"
    );
    let conflict = &refused["conflicts"][0];
    assert_eq!(conflict["resource_kind"], json!("path"));
    assert_eq!(conflict["resource_key"], json!("src/mcp.rs"));
    assert_eq!(conflict["agent_id"], json!(writer_id));
    assert_eq!(conflict["agent_name"], json!("shape-writer"));
    assert!(
        conflict["lease_expires_at"].is_number(),
        "the loser is told when the holder's lease runs out: {conflict}"
    );
    assert_eq!(
        refused["resources"],
        json!([]),
        "a refused reservation acquires nothing at all: {refused}"
    );

    assert_eq!(
        writer.ok(
            "release_resources",
            json!({"resources": ["path:src/mcp.rs", "port:4399"]})
        )?,
        json!({"released": 2})
    );
    Ok(())
}

/// `event_stream` and `wait_for_events`: the ordered log everything above wrote into.
fn assert_event_shape(writer: &mut Session) -> Result<()> {
    let events = writer.ok("event_stream", json!({"after_sequence": 0, "limit": 100}))?;
    let events = events.as_array().context("event_stream is an array")?;
    let kinds: Vec<&str> = events
        .iter()
        .filter_map(|event| event["event_type"].as_str())
        .collect();
    assert!(kinds.contains(&"agent.joined"), "{kinds:?}");
    assert!(kinds.contains(&"memory.created"), "{kinds:?}");
    assert!(kinds.contains(&"task.claimed"), "{kinds:?}");
    assert!(kinds.contains(&"resource.reserved"), "{kinds:?}");
    let first = events.first().context("at least one event")?;
    assert_eq!(first["sequence"], json!(1), "sequencing starts at one");
    assert!(first["created_at"].is_number());
    assert!(first["data"].is_object());

    let last = events
        .last()
        .and_then(|event| event["sequence"].as_i64())
        .context("events carry a sequence")?;
    assert_eq!(
        writer.ok(
            "wait_for_events",
            json!({"after_sequence": last, "timeout_seconds": 0.2})
        )?,
        json!([]),
        "waiting past the end of the stream times out empty rather than blocking"
    );
    let replayed = writer.ok(
        "wait_for_events",
        json!({"after_sequence": 0, "timeout_seconds": 5, "limit": 2}),
    )?;
    assert_eq!(
        replayed
            .as_array()
            .context("wait_for_events is an array")?
            .len(),
        2,
        "an event already in the log returns at once, honouring the limit: {replayed}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 2. The end-to-end collision story, over MCP
// ---------------------------------------------------------------------------------------------

/// The product's central claim, driven entirely through MCP.
///
/// Two agents in real linked worktrees make genuinely conflicting edits, publish what they
/// changed, and the mesh has to say so: the contested paths with the right severity, and a
/// `mergeable` that refuses. Then one agent withdraws and the other becomes free to merge.
#[test]
fn two_agents_contesting_a_file_collide_and_cannot_merge() -> Result<()> {
    let workspace = Workspace::repository()?;
    let alpha_tree = workspace.worktree("alpha")?;
    let beta_tree = workspace.worktree("beta")?;

    // The same region of one file, rewritten differently: the case that silently overwrites work.
    rewrite_line(&alpha_tree.join("shared.txt"), 5, "alpha rewrote this")?;
    rewrite_line(&beta_tree.join("shared.txt"), 5, "beta rewrote this")?;
    // And a delete against an edit, which git will merge without complaint in some orderings.
    fs::remove_file(alpha_tree.join("legacy.txt"))?;
    fs::write(beta_tree.join("legacy.txt"), "legacy, still needed\n")?;
    // One path only alpha touches, so a report that lumps everything together is visible.
    fs::write(alpha_tree.join("alpha-only.txt"), "mine alone\n")?;

    // No PIDMESH_WORKSPACE: linked-worktree discovery is what puts both agents in one mesh.
    let mut alpha = Session::start("alpha", &workspace.database, &alpha_tree, None)?;
    let mut beta = Session::start("beta", &workspace.database, &beta_tree, None)?;
    let alpha_id = alpha.agent_id()?;
    let beta_id = beta.agent_id()?;
    assert_ne!(alpha_id, beta_id);

    let published = alpha.ok("sync_footprint", json!({}))?;
    assert_eq!(published["agent_id"], json!(alpha_id.as_str()));
    assert_eq!(published["base_ref"], json!("main"));
    assert_eq!(published["branch"], json!("feat/alpha"));
    assert_eq!(published["paths"], json!(3));
    assert!(
        published["base_commit"]
            .as_str()
            .is_some_and(|commit| commit.len() == 40),
        "the footprint records the commit it was measured against: {published}"
    );
    assert_eq!(
        published["collision_count"],
        json!(0),
        "the first agent to sync contests nothing: {published}"
    );

    let published = beta.ok("sync_footprint", json!({}))?;
    assert_eq!(published["branch"], json!("feat/beta"));
    assert_eq!(
        published["collision_count"],
        json!(2),
        "publishing against a peer's footprint reports the overlap immediately: {published}"
    );

    // Severity, read from the agent that did not publish last.
    assert_collision_report(&alpha.ok("collisions", json!({}))?, &alpha_id)?;

    // The refusal. Both peers are live, so neither may merge over the other.
    let readiness = alpha.ok("mergeable", json!({}))?;
    assert_eq!(readiness["agent_id"], json!(alpha_id.as_str()));
    assert_eq!(
        readiness["mergeable"],
        json!(false),
        "a contested path must block the merge: {readiness}"
    );
    assert_eq!(readiness["holds_integration_lease"], json!(false));
    assert_eq!(
        readiness["base_commit"], readiness["integration_head"],
        "the base has not moved, so staleness is not what is blocking: {readiness}"
    );
    let contested_paths: Vec<&str> = blockers(&readiness, "contested_path")
        .iter()
        .filter_map(|blocker| blocker["path"].as_str())
        .collect();
    assert!(
        contested_paths.contains(&"shared.txt") && contested_paths.contains(&"legacy.txt"),
        "both contested paths must be named: {readiness}"
    );
    let shared_blocker = blockers(&readiness, "contested_path")
        .into_iter()
        .find(|blocker| blocker["path"] == json!("shared.txt"))
        .context("shared.txt must be a blocker")?;
    assert_eq!(
        shared_blocker["peers"][0]["agent_id"],
        json!(beta_id.as_str())
    );
    assert_eq!(shared_blocker["peers"][0]["agent_name"], json!("beta"));
    assert_eq!(shared_blocker["peers"][0]["severity"], json!("divergent"));
    assert!(
        beta.ok("mergeable", json!({}))?["mergeable"] == json!(false),
        "the refusal is symmetric; neither agent may go first"
    );

    // Withdrawing a footprint is how an agent that merged or gave up stops blocking everyone.
    let released = beta.ok("release_footprint", json!({}))?;
    assert_eq!(
        released["released"],
        json!(2),
        "beta withdraws both of the paths it had published: {released}"
    );
    let after = alpha.ok("collisions", json!({}))?;
    assert_eq!(
        after["collision_count"],
        json!(0),
        "with the peer withdrawn nothing is contested: {after}"
    );
    let readiness = alpha.ok("mergeable", json!({}))?;
    assert_eq!(
        readiness["mergeable"],
        json!(true),
        "alpha is now free to merge: {readiness}"
    );

    beta.shut_down()?;
    alpha.shut_down()?;
    Ok(())
}

/// What `collisions` has to say once both checkouts of the fixture have published.
///
/// Two contested paths with different severities, one untouched path that is not contested, and
/// enough detail per participant for a client to explain the conflict without asking again.
fn assert_collision_report(report: &Value, viewer: &str) -> Result<()> {
    assert_eq!(report["agent_id"], json!(viewer));
    assert_eq!(report["collision_count"], json!(2));
    assert_eq!(
        report["symbol_break_count"],
        json!(0),
        "plain text files withdraw no exported symbol: {report}"
    );
    assert!(
        contested(&report["collisions"], "alpha-only.txt").is_none(),
        "a path only one agent touched is not a collision: {report}"
    );

    let shared = contested(&report["collisions"], "shared.txt")
        .context("shared.txt is contested and must be reported")?;
    assert_eq!(
        shared["severity"],
        json!("divergent"),
        "different content in the same region is the overwrite case: {shared}"
    );
    assert_eq!(shared["involves_me"], json!(true));
    assert_eq!(
        shared["base_divergent"],
        json!(false),
        "both worktrees were cut from the same commit: {shared}"
    );
    let participants = shared["participants"]
        .as_array()
        .context("a collision names its participants")?;
    assert_eq!(participants.len(), 2);
    let names: Vec<&str> = participants
        .iter()
        .filter_map(|participant| participant["agent_name"].as_str())
        .collect();
    assert!(
        names.contains(&"alpha") && names.contains(&"beta"),
        "{names:?}"
    );
    for participant in participants {
        assert_eq!(participant["status"], json!("running"));
        assert_eq!(participant["change_kind"], json!("modified"));
        assert_eq!(
            participant["ranges"],
            json!("6-6"),
            "the rewritten region is reported in base coordinates: {participant}"
        );
        assert!(participant["digest"].is_string());
        assert!(participant["agent_id"].is_string());
        assert!(participant["branch"].is_string());
    }
    assert_ne!(
        participants[0]["digest"], participants[1]["digest"],
        "divergent means the content genuinely differs: {participants:?}"
    );

    let legacy = contested(&report["collisions"], "legacy.txt")
        .context("legacy.txt is contested and must be reported")?;
    assert_eq!(
        legacy["severity"],
        json!("delete_edit"),
        "one agent removed a path the other is still editing: {legacy}"
    );
    Ok(())
}

/// A checkout nobody contests still has to be observed before it may merge, and has to stop
/// being mergeable the moment the integration branch moves underneath it.
#[test]
fn merge_readiness_tracks_observation_and_the_moving_base() -> Result<()> {
    let workspace = Workspace::repository()?;
    let tree = workspace.worktree("solo")?;
    rewrite_line(&tree.join("shared.txt"), 2, "solo rewrote this")?;
    let mut solo = Session::start("solo", &workspace.database, &tree, None)?;

    let unobserved = solo.ok("mergeable", json!({}))?;
    assert_eq!(
        unobserved["mergeable"],
        json!(false),
        "never having been scanned is not the same as having been found clear: {unobserved}"
    );
    assert_eq!(
        blockers(&unobserved, "never_synced").len(),
        1,
        "{unobserved}"
    );
    assert_eq!(unobserved["base_commit"], Value::Null);

    solo.ok("sync_footprint", json!({}))?;
    let ready = solo.ok("mergeable", json!({}))?;
    assert_eq!(
        ready["mergeable"],
        json!(true),
        "an observed, uncontested, up-to-date checkout may merge: {ready}"
    );
    assert_eq!(ready["blockers"], json!([]));

    let lease = solo.ok("acquire_integration_lease", json!({"lease_seconds": 120}))?;
    assert_eq!(lease["acquired"], json!(true));
    assert_eq!(lease["task_key"], json!("pidmesh:integration"));
    assert_eq!(
        solo.ok("mergeable", json!({}))?["holds_integration_lease"],
        json!(true),
        "holding the lease is reported back, so a merge script knows it may proceed"
    );

    // Somebody else merged. The stored base is now behind the branch tip.
    workspace.advance_main("another agent landed first")?;
    let stale = solo.ok("mergeable", json!({}))?;
    assert_eq!(
        stale["mergeable"],
        json!(false),
        "a worktree cut from a commit main has moved past must rebase: {stale}"
    );
    let stale_base = blockers(&stale, "stale_base");
    assert_eq!(stale_base.len(), 1, "{stale}");
    assert_eq!(stale_base[0]["base_commit"], stale["base_commit"]);
    assert_eq!(stale_base[0]["integration_head"], stale["integration_head"]);
    assert_ne!(stale["base_commit"], stale["integration_head"]);

    assert_eq!(
        solo.ok("release_integration_lease", json!({}))?,
        json!({"released": true})
    );
    assert_eq!(
        solo.ok("mergeable", json!({}))?["holds_integration_lease"],
        json!(false)
    );

    solo.shut_down()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 3. Error behaviour
// ---------------------------------------------------------------------------------------------

/// Every way a caller can get a tool call wrong, and the session surviving all of them.
///
/// An unattended agent cannot recover from a dead session. A bad argument has to come back as a
/// readable tool error on a connection that still works for the next call.
#[test]
fn bad_calls_fail_as_tool_errors_and_leave_the_session_usable() -> Result<()> {
    let repository = Workspace::repository()?;
    let plain = Workspace::bare()?;
    let mut inside = Session::start("inside", &repository.database, &repository.root, None)?;
    let mut outside = Session::start("outside", &plain.database, &plain.root, Some(&plain.root))?;

    // Outside a repository there is no integration branch to measure against.
    for tool in ["sync_footprint", "mergeable"] {
        let message = outside.failure(tool, json!({}))?;
        assert!(
            message.contains("no default integration branch"),
            "{tool} outside a repository should explain itself: {message}"
        );
    }

    // A base ref that does not exist, and one git would read as a flag.
    let message = inside.failure("sync_footprint", json!({"base": "refs/heads/nope"}))?;
    assert!(
        message.contains("refs/heads/nope"),
        "a missing base ref should name the ref: {message}"
    );
    let message = inside.failure("mergeable", json!({"base": "--output=/tmp/pwned"}))?;
    assert!(
        message.contains("must not start with a dash"),
        "a ref that git would read as an option is refused: {message}"
    );

    // Malformed parameters: wrong type, and a required field left out.
    let message = inside.failure("remember", json!({"content": 42}))?;
    assert!(
        message.contains("expected a string"),
        "a type error names the type: {message}"
    );
    let message = inside.failure("claim", json!({}))?;
    assert!(
        message.contains("task"),
        "a missing required field names the field: {message}"
    );
    let message = inside.failure("reserve_resources", json!({"resources": []}))?;
    assert!(message.contains("between 1 and 64"), "{message}");

    // Addressing an agent that is not there.
    let message = inside.failure("send", json!({"message": "hi", "recipient": "nobody-here"}))?;
    assert!(
        message.contains("nobody-here"),
        "an unknown recipient is named: {message}"
    );

    // A tool that does not exist is the one case that is a protocol error, and must stay one
    // rather than becoming a crash.
    let unknown = inside.call("no_such_tool", json!({}))?;
    assert_eq!(unknown["error"]["code"], json!(-32602), "{unknown}");

    // Beyond what the event waiter will serve.
    let message = inside.failure("wait_for_events", json!({"timeout_seconds": 500}))?;
    assert!(message.contains("60 seconds"), "{message}");

    // The point of all of the above: both sessions still work.
    assert_eq!(
        inside.ok("mesh_status", json!({}))?["agents"]
            .as_array()
            .context("missing agents")?
            .len(),
        1
    );
    assert!(outside.ok("remember", json!({"content": "still alive"}))?["memory_id"].is_number());
    assert!(
        !inside.diagnostics().contains("panicked"),
        "a bad argument must not panic the server: {}",
        inside.diagnostics()
    );

    outside.shut_down()?;
    inside.shut_down()?;
    Ok(())
}

/// Regression: a finite, positive `timeout_seconds` too large for a `Duration` panicked the
/// worker thread, and the agent got `task N panicked with message ...` as an internal error
/// instead of something it could act on.
#[test]
fn wait_for_events_rejects_an_unrepresentable_timeout_without_panicking() -> Result<()> {
    let workspace = Workspace::bare()?;
    let mut session = Session::start(
        "waiter",
        &workspace.database,
        &workspace.root,
        Some(&workspace.root),
    )?;

    for timeout in [json!(1e300), json!(-1), json!(1e308)] {
        let message = session.failure("wait_for_events", json!({"timeout_seconds": timeout}))?;
        assert!(
            message.contains("timeout_seconds"),
            "the caller is told which argument was wrong: {message}"
        );
        assert!(
            !message.contains("panicked"),
            "a bad timeout is a validation failure, not a crash: {message}"
        );
    }

    assert!(
        !session.diagnostics().contains("panicked"),
        "nothing panicked on the server: {}",
        session.diagnostics()
    );
    // The session is still good for ordinary work afterwards.
    assert_eq!(
        session
            .ok(
                "wait_for_events",
                json!({"after_sequence": 0, "timeout_seconds": 1, "limit": 1})
            )?
            .as_array()
            .context("wait_for_events is an array")?
            .len(),
        1
    );

    session.shut_down()?;
    Ok(())
}

/// Regression: `acquire_integration_lease` advertised `task` as a required parameter and then
/// ignored it, so the obvious call -- the one the server's own instructions describe -- failed
/// to deserialize.
#[test]
fn acquire_integration_lease_does_not_demand_a_task_it_ignores() -> Result<()> {
    let workspace = Workspace::bare()?;
    let mut session = Session::start(
        "integrator",
        &workspace.database,
        &workspace.root,
        Some(&workspace.root),
    )?;

    let schema = session.request("tools/list", json!({}))?["result"]["tools"]
        .as_array()
        .context("missing tool list")?
        .iter()
        .find(|tool| tool["name"] == json!("acquire_integration_lease"))
        .context("acquire_integration_lease is missing")?["inputSchema"]
        .clone();
    assert!(
        schema["required"].is_null(),
        "the lease tool takes no required argument: {schema}"
    );
    assert!(
        schema["properties"]["task"].is_null(),
        "the task key is fixed, so advertising one would be a lie: {schema}"
    );
    assert_eq!(schema["properties"]["lease_seconds"]["default"], json!(300));

    let lease = session.ok("acquire_integration_lease", json!({}))?;
    assert_eq!(lease["acquired"], json!(true));
    assert_eq!(lease["task_key"], json!("pidmesh:integration"));
    assert_eq!(
        lease["detail"],
        json!("merging into the integration branch"),
        "the detail is the server's, not the caller's"
    );
    let window = lease["lease_expires_at"]
        .as_i64()
        .context("lease_expires_at")?
        - lease["claimed_at"].as_i64().context("claimed_at")?;
    assert_eq!(
        window, 300_000,
        "the documented default is the real default"
    );

    session.shut_down()?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 4. Concurrency
// ---------------------------------------------------------------------------------------------

/// Two sessions, one database. The integration lease is the mutual exclusion the whole merge
/// protocol rests on, and neither session may fall over from SQLite contention while they trade.
#[test]
fn the_integration_lease_is_exclusive_across_concurrent_sessions() -> Result<()> {
    let workspace = Workspace::repository()?;
    let first_tree = workspace.worktree("first")?;
    let second_tree = workspace.worktree("second")?;
    let mut first = Session::start("first", &workspace.database, &first_tree, None)?;
    let mut second = Session::start("second", &workspace.database, &second_tree, None)?;
    let first_id = first.agent_id()?;
    let second_id = second.agent_id()?;

    let held = first.ok("acquire_integration_lease", json!({"lease_seconds": 300}))?;
    assert_eq!(held["acquired"], json!(true));
    assert_eq!(held["agent_id"], json!(first_id.as_str()));

    let refused = second.ok("acquire_integration_lease", json!({"lease_seconds": 300}))?;
    assert_eq!(
        refused["acquired"],
        json!(false),
        "two agents must never both believe they are merging: {refused}"
    );
    assert_eq!(
        refused["agent_id"],
        json!(first_id.as_str()),
        "the loser is told who holds it: {refused}"
    );
    assert_eq!(
        second.ok("release_integration_lease", json!({}))?,
        json!({"released": false}),
        "the lease cannot be released out from under its holder"
    );

    // The waiting agent is told, through mergeable, that somebody else is mid-merge.
    second.ok("sync_footprint", json!({}))?;
    let readiness = second.ok("mergeable", json!({}))?;
    let held_blockers = blockers(&readiness, "integration_held");
    assert_eq!(held_blockers.len(), 1, "{readiness}");
    assert_eq!(held_blockers[0]["held_by"], json!(first_id.as_str()));

    // Interleaved writes from both sessions: SQLite contention must be invisible to the caller.
    for round in 0..12 {
        let note = format!("round {round}");
        assert!(first.ok("remember", json!({"content": note.clone()}))?["memory_id"].is_number());
        assert!(second.ok("remember", json!({"content": note}))?["memory_id"].is_number());
        first.ok("claim", json!({"task": format!("first-{round}")}))?;
        second.ok("claim", json!({"task": format!("second-{round}")}))?;
        second.ok("event_stream", json!({"after_sequence": 0, "limit": 5}))?;
    }
    assert_eq!(
        second
            .ok("recall", json!({"query": "round 11", "limit": 10}))?
            .as_array()
            .context("recall is an array")?
            .len(),
        2,
        "both sessions' writes survived the contention"
    );

    // Handing the lease over works, and only after the holder lets go.
    assert_eq!(
        first.ok("release_integration_lease", json!({}))?,
        json!({"released": true})
    );
    let taken = second.ok("acquire_integration_lease", json!({}))?;
    assert_eq!(taken["acquired"], json!(true));
    assert_eq!(taken["agent_id"], json!(second_id.as_str()));
    assert!(
        blockers(&second.ok("mergeable", json!({}))?, "integration_held").is_empty(),
        "holding the lease yourself is not a blocker"
    );

    for session in [&first, &second] {
        assert!(
            !session.diagnostics().contains("panicked"),
            "{} panicked: {}",
            session.label,
            session.diagnostics()
        );
    }
    second.shut_down()?;
    first.shut_down()?;
    Ok(())
}
