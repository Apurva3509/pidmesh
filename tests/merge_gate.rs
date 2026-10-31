//! The merge gate: the one operation in pidmesh that writes to the user's real repository.
//!
//! `IdeManager::approve` commits a managed run's worktree and merges it into the primary
//! checkout with `--no-ff`. `reject` is its counterpart, and `review` is the evidence both are
//! decided on. Everything else in the product reports; this mutates. So nothing here settles for
//! "approve returned 200": every test asks git what actually happened — which commits exist, how
//! many parents `HEAD` has, where each branch points, what is in the working tree, and whether a
//! merge is in progress.
//!
//! **Refusals are assertions about non-events.** A gate that refuses is only useful if it refuses
//! *before* touching anything, so each refusal test captures a [`fingerprint`] of the primary
//! checkout — `HEAD`, the checked-out ref, the index, the porcelain status, the full commit graph,
//! every file byte — and asserts it is identical afterwards. A gate that refused but had already
//! committed, moved a branch or started a merge would fail that comparison.
//!
//! **Isolation.** `IdeManager` cuts its worktrees under `$HOME/.pidmesh`, so every test gets its
//! own `HOME` inside its own tempdir, alongside its own port, database, git workspace and `PATH`.
//! A test that reached a launch with the real `HOME` would commit into the developer's own tree —
//! and this is the file where that would then be *merged*. [`Harness::run`] asserts on every
//! launch that the worktree landed inside the tempdir, so the check is not something an individual
//! test can forget. The provider is reachable only because the throwaway `PATH` holds a `codex`
//! that is really `tests/support/fake_provider.rs`; nothing on the developer's `PATH` is launched.
//!
//! **Bounded waits.** Every poll carries a deadline and every socket a read timeout, so no test
//! here can hang.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tempfile::TempDir;

/// How long any single poll may run before the test fails instead of hanging.
const DEADLINE: Duration = Duration::from_secs(20);

/// What `tests/support/fake_provider.rs` writes into every file it is told to create.
const WRITTEN: &str = "written by the fake provider\n";

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

/// One managed run, reduced to the four facts every assertion below is written against.
struct Run {
    base_commit: String,
    branch: String,
    id: String,
    worktree: PathBuf,
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

    /// The repository the dashboard was pointed at: the *primary checkout* every gate protects.
    fn workspace(&self) -> PathBuf {
        self.directory.path().join("workspace")
    }

    /// Where this test's `HOME` is, which is where `IdeManager` must cut every worktree.
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
    ///
    /// The worktree assertion is here rather than in each test on purpose: it is the one check
    /// that must never be omitted, because the alternative to a tempdir worktree is the
    /// developer's own repository.
    fn run(&self, name: &str, scope: &str, boot: &[&str]) -> Result<Run> {
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
        let view = self.json("POST", "/api/v1/ide/sessions", &body)?;
        let worktree = PathBuf::from(view["worktree"].as_str().context("worktree")?);
        ensure!(
            worktree.starts_with(self.home()),
            "a managed worktree was cut outside this test's HOME: {}",
            worktree.display()
        );
        Ok(Run {
            base_commit: view["base_commit"]
                .as_str()
                .context("base_commit")?
                .to_owned(),
            branch: view["branch"].as_str().context("branch")?.to_owned(),
            id: view["id"].as_str().context("id")?.to_owned(),
            worktree,
        })
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

    fn review(&self, id: &str) -> Result<Value> {
        self.json("GET", &format!("/api/v1/ide/sessions/{id}/review"), "")
    }

    fn approve(&self, id: &str, message: &str) -> Result<Reply> {
        let body = json!({ "commit_message": message }).to_string();
        self.send("POST", &format!("/api/v1/ide/sessions/{id}/approve"), &body)
    }

    fn reject(&self, id: &str) -> Result<Reply> {
        self.send("POST", &format!("/api/v1/ide/sessions/{id}/reject"), "")
    }

    fn stop(&self, id: &str) -> Result<Reply> {
        self.send("DELETE", &format!("/api/v1/ide/sessions/{id}"), "")
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

    /// Poll until the provider has written `needle`, or fail rather than wait forever.
    fn await_output(&self, id: &str, needle: &str) -> Result<()> {
        let deadline = Instant::now() + DEADLINE;
        let mut seen = String::new();
        while Instant::now() < deadline {
            seen = self.output(id)?;
            if seen.contains(needle) {
                return Ok(());
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

    /// Launch a run that writes `files` and exits cleanly, and wait until it is reviewable.
    fn finished_run(&self, name: &str, scope: &str, files: &[&str]) -> Result<Run> {
        let mut boot: Vec<String> = files.iter().map(|path| format!("write {path}")).collect();
        boot.push("say finished".to_owned());
        boot.push("exit 0".to_owned());
        let boot: Vec<&str> = boot.iter().map(String::as_str).collect();
        let run = self.run(name, scope, &boot)?;
        self.await_status(&run.id, &["ready_for_review"])?;
        Ok(run)
    }
}

/// Shut the dashboard down the way a user does, so its own cleanup runs.
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

// ---------------------------------------------------------------------------------------------
// Asking git what really happened
// ---------------------------------------------------------------------------------------------

fn git_out(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()?;
    ensure!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
}

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    git_out(directory, arguments).map(|_| ())
}

/// Is a merge half-applied right now?
///
/// `MERGE_HEAD` exists for exactly as long as a merge is in progress and no longer, so this is
/// the direct question: did an approval leave the user's repository mid-merge? Nothing else in
/// the checkout answers it — a conflicted tree and a tree someone edited by hand look alike.
fn merge_in_progress(workspace: &Path) -> bool {
    git_out(
        workspace,
        &["rev-parse", "--verify", "--quiet", "MERGE_HEAD"],
    )
    .is_ok()
}

/// Everything a merge could change about the primary checkout, as one comparable value.
///
/// `HEAD` and the checked-out ref catch a branch that moved, the index and porcelain status catch
/// a staged or conflicted merge, the file walk catches content written into the user's tree — a
/// conflict marker is a content change like any other — and `MERGE_HEAD` catches a merge left
/// open. Failure prints both sides, so a difference names itself.
fn checkout_state(workspace: &Path) -> Result<String> {
    let mut state = String::new();
    writeln!(
        state,
        "head {}",
        git_out(workspace, &["rev-parse", "HEAD"])?
    )?;
    writeln!(
        state,
        "ref {}",
        git_out(workspace, &["symbolic-ref", "--quiet", "HEAD"])
            .unwrap_or_else(|_| "detached".to_owned())
    )?;
    writeln!(
        state,
        "status [{}]",
        git_out(
            workspace,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?
    )?;
    writeln!(
        state,
        "index [{}]",
        git_out(workspace, &["ls-files", "--stage"])?
    )?;
    writeln!(state, "merging {}", merge_in_progress(workspace))?;
    let mut files = BTreeMap::new();
    collect_files(workspace, workspace, &mut files)?;
    for (path, content) in files {
        writeln!(state, "file {path} {content}")?;
    }
    Ok(state)
}

/// [`checkout_state`] plus the whole commit graph, so "nothing was mutated" also means no commit
/// was created anywhere — including on the agent's own branch, which `approve` writes to first.
fn fingerprint(workspace: &Path) -> Result<String> {
    let mut state = checkout_state(workspace)?;
    writeln!(
        state,
        "graph [{}]",
        git_out(workspace, &["log", "--all", "--pretty=%H %P %s"])?
    )?;
    Ok(state)
}

/// Every file in the checkout, by path and by byte, with git's own database left out.
fn collect_files(root: &Path, directory: &Path, into: &mut BTreeMap<String, String>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, into)?;
        } else {
            let bytes = fs::read(&path)?;
            into.insert(
                path.strip_prefix(root)?.to_string_lossy().into_owned(),
                format!("{} {:?}", bytes.len(), String::from_utf8_lossy(&bytes)),
            );
        }
    }
    Ok(())
}

/// Where a branch points, asked from the primary checkout because refs are shared across
/// worktrees.
fn tip(workspace: &Path, reference: &str) -> Result<String> {
    git_out(workspace, &["rev-parse", reference])
}

fn assert_refusal(reply: &Reply, needle: &str) {
    assert_eq!(reply.status, 409, "expected a refusal, got {reply:?}");
    assert!(
        reply.body.contains(needle),
        "refusal did not mention {needle:?}: {reply:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The success path
// ---------------------------------------------------------------------------------------------

/// What approving a run actually does to git.
///
/// Four separate claims, each asked of git rather than of the API: the run's work became one
/// commit on its own branch carrying the given message and the file's bytes; the primary
/// checkout's branch advanced to a commit with *two* parents — the old tip and that branch tip,
/// which is what `--no-ff` means and what a fast-forward would not produce; the checkout is still
/// on its base branch with nothing in progress; and the user is left with a clean tree that
/// contains the agent's file and still contains their own.
#[test]
fn approve_commits_the_run_and_merges_it_into_the_primary_checkout() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("merge-happy", "src/a", &["src/a/allowed.txt"])?;

    let base = tip(&workspace, "HEAD")?;
    assert_eq!(base, run.base_commit, "the run recorded a different base");
    assert_eq!(
        tip(&workspace, &run.branch)?,
        base,
        "the agent branch already carried a commit before approval"
    );

    let approved = harness.json(
        "POST",
        &format!("/api/v1/ide/sessions/{}/approve", run.id),
        &json!({"commit_message": "feat: the agent's change"}).to_string(),
    )?;
    assert_eq!(approved["status"], "merged");
    assert!(approved["approved_at"].is_i64(), "approved_at not recorded");
    assert!(approved["merged_at"].is_i64(), "merged_at not recorded");
    assert_eq!(harness.status(&run.id)?, "merged");

    // 1. The run's work is one commit on its own branch, with the message and the content asked
    //    for. `git add -A` means an untracked file counts, which is all the fake provider makes.
    let agent = tip(&workspace, &run.branch)?;
    assert_ne!(agent, base, "the agent branch never advanced");
    assert_eq!(
        git_out(
            &workspace,
            &["rev-list", "--count", &format!("{base}..{agent}")]
        )?,
        "1",
        "approval should add exactly one commit to the agent branch"
    );
    assert_eq!(
        git_out(&workspace, &["log", "-1", "--pretty=%s", &agent])?,
        "feat: the agent's change"
    );
    assert_eq!(
        git_out(&workspace, &["show", &format!("{agent}:src/a/allowed.txt")])?,
        WRITTEN.trim_end()
    );

    // 2. The primary checkout advanced, and it advanced onto a real merge commit: two parents,
    //    the first its own previous tip and the second the agent branch.
    let head = tip(&workspace, "HEAD")?;
    assert_ne!(head, base, "the primary checkout never advanced");
    assert_eq!(
        git_out(&workspace, &["rev-list", "--parents", "-n", "1", "HEAD"])?,
        format!("{head} {base} {agent}"),
        "HEAD is not a no-fast-forward merge of the base and the agent branch"
    );
    assert_eq!(
        git_out(&workspace, &["log", "-1", "--pretty=%s"])?,
        "feat: the agent's change"
    );

    // 3. Still on the base branch, with no merge left open.
    assert_eq!(git_out(&workspace, &["branch", "--show-current"])?, "main");
    assert!(
        !merge_in_progress(&workspace),
        "a merge was left in progress"
    );

    // 4. The tree the user is left holding.
    assert_eq!(
        git_out(
            &workspace,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?,
        "",
        "the merge left the primary checkout dirty"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("src/a/allowed.txt"))?,
        WRITTEN
    );
    assert_eq!(fs::read_to_string(workspace.join("src/a/keep.txt"))?, "a\n");
    assert_eq!(fs::read_to_string(workspace.join("src/b/keep.txt"))?, "b\n");

    // 5. The run's worktree survives its own approval, committed and clean.
    assert!(run.worktree.join("src/a/allowed.txt").is_file());
    assert_eq!(harness.session(&run.id)?["changed_files"], json!([]));
    Ok(())
}

/// The same success path for the change an agent actually makes most often: editing a file that
/// already exists.
///
/// This is a separate test from the one above because the two take different paths through
/// `git status --porcelain`. A brand-new file is reported as `?? path`; a modified tracked file
/// as ` M path`, with a leading space for the staged column it has nothing in. That leading space
/// was being trimmed off the first line of the status output before the paths were parsed out of
/// it, so the first modified file came back one byte short — `rc/a/keep.txt` for
/// `src/a/keep.txt`. A name like that matches no declared scope, so the scope gate called an
/// in-scope edit out of scope and `approve` refused it. The assertion that matters here is the
/// one on `changed_files`: a path reported wrong is a merge that cannot happen.
#[test]
fn approve_merges_a_modification_to_an_existing_tracked_file() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("tracked-edit", "src/a", &["src/a/keep.txt"])?;

    let review = harness.review(&run.id)?;
    assert_eq!(
        review["changed_files"],
        json!(["src/a/keep.txt"]),
        "the modified path was not reported under its own name"
    );
    assert_eq!(review["out_of_scope"], json!([]));
    assert_eq!(review["scope_valid"], true);

    let base = tip(&workspace, "HEAD")?;
    let approved = harness.approve(&run.id, "fix: edit a tracked file")?;
    assert_eq!(approved.status, 200, "{approved:?}");

    let agent = tip(&workspace, &run.branch)?;
    let head = tip(&workspace, "HEAD")?;
    assert_eq!(
        git_out(&workspace, &["rev-list", "--parents", "-n", "1", "HEAD"])?,
        format!("{head} {base} {agent}")
    );
    assert_eq!(
        fs::read_to_string(workspace.join("src/a/keep.txt"))?,
        WRITTEN
    );
    assert_eq!(
        git_out(
            &workspace,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?,
        ""
    );
    assert!(!merge_in_progress(&workspace));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Every gate, refusing on its own, mutating nothing
// ---------------------------------------------------------------------------------------------

/// Process state: a run that is still going cannot be merged.
#[test]
fn approve_refuses_while_the_agent_is_still_running() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    // No `exit`, so the provider sits reading its PTY and the run stays `running`.
    let run = harness.run(
        "still-running",
        "src/a",
        &["write src/a/allowed.txt", "say working"],
    )?;
    harness.await_output(&run.id, "write:src/a/allowed.txt")?;
    assert_eq!(harness.status(&run.id)?, "running");

    let before = fingerprint(&workspace)?;
    assert_refusal(
        &harness.approve(&run.id, "feat: too early")?,
        "stop the agent",
    );
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    assert_eq!(harness.status(&run.id)?, "running");
    Ok(())
}

/// Scope validity: a run that wrote outside its reserved paths cannot be merged.
#[test]
fn approve_refuses_changes_outside_the_reserved_scope() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run(
        "scope-break",
        "src/a",
        &["src/a/allowed.txt", "src/b/sneaky.txt"],
    )?;
    let review = harness.review(&run.id)?;
    assert_eq!(review["scope_valid"], false);
    assert_eq!(review["out_of_scope"], json!(["src/b/sneaky.txt"]));

    let before = fingerprint(&workspace)?;
    assert_refusal(&harness.approve(&run.id, "feat: sneaky")?, "out-of-scope");
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    Ok(())
}

/// Primary-checkout cleanliness, in both of the ways a checkout can be dirty.
#[test]
fn approve_refuses_a_dirty_primary_checkout() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("dirty-base", "src/a", &["src/a/allowed.txt"])?;

    // A tracked file the user has edited and not committed.
    fs::write(workspace.join("src/b/keep.txt"), "the user was mid-edit\n")?;
    let before = fingerprint(&workspace)?;
    assert_refusal(&harness.approve(&run.id, "feat: onto dirt")?, "dirty");
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);

    // An untracked file alone is dirt too: `--porcelain` reports it, and a merge could clobber it.
    fs::write(workspace.join("src/b/keep.txt"), "b\n")?;
    fs::write(workspace.join("scratch.txt"), "not committed\n")?;
    let before = fingerprint(&workspace)?;
    assert_refusal(&harness.approve(&run.id, "feat: onto dirt")?, "dirty");
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    Ok(())
}

/// The expected base branch: merging into whatever the user happens to have checked out now is
/// exactly the mistake this gate exists to prevent.
#[test]
fn approve_refuses_when_the_primary_checkout_left_the_base_branch() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("branch-moved", "src/a", &["src/a/allowed.txt"])?;

    // Another branch, at the same commit — so only the *name* differs, and only the gate's own
    // comparison can catch it.
    git(&workspace, &["checkout", "--quiet", "-b", "side"])?;
    let before = fingerprint(&workspace)?;
    assert_refusal(
        &harness.approve(&run.id, "feat: onto the wrong branch")?,
        "no longer on main",
    );
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);

    // A detached HEAD has no current branch at all, which must refuse rather than compare equal
    // to an empty expectation.
    git(&workspace, &["checkout", "--quiet", "main"])?;
    git(&workspace, &["checkout", "--quiet", "--detach"])?;
    let before = fingerprint(&workspace)?;
    assert_refusal(
        &harness.approve(&run.id, "feat: onto a detached head")?,
        "no longer on main",
    );
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    Ok(())
}

/// The base branch moved since the run started, by a commit that would have merged cleanly.
///
/// `base_commit` is the position the worktree was cut at. While the primary checkout still points
/// there the agent's branch is a strict descendant and `--no-ff` can only produce a clean merge
/// commit; once it moves, the merge becomes a three-way merge against a base the run never saw.
/// The run's diff is no longer the change it would introduce, so the gate refuses even when git
/// could have resolved it.
#[test]
fn approve_refuses_when_the_base_branch_moved_under_the_run() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("stale-base", "src/a", &["src/a/allowed.txt"])?;

    fs::write(workspace.join("src/b/keep.txt"), "the user moved on\n")?;
    git(&workspace, &["add", "--all"])?;
    git(&workspace, &["commit", "--quiet", "-m", "chore: move on"])?;
    assert_ne!(tip(&workspace, "HEAD")?, run.base_commit);

    let before = fingerprint(&workspace)?;
    assert_refusal(
        &harness.approve(&run.id, "feat: onto a moved base")?,
        "moved",
    );
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    assert!(!merge_in_progress(&workspace));
    Ok(())
}

/// The base branch moved, and it moved onto the same file the run changed.
///
/// This is the regression test for the worst outcome this code path can produce. With no
/// base-commit gate, `approve` reached `git merge --no-ff` with a genuinely conflicting merge:
/// git does not fail that cleanly, it stops *half-applied* — `MERGE_HEAD` set, the index
/// unmerged, and conflict markers written into the user's own file — and then `approve` returned
/// an error, telling the operator it had refused while their real repository sat mid-merge. The
/// assertions below are therefore about the file's bytes and `MERGE_HEAD`, not just the status
/// code: a refusal that leaves `<<<<<<<` in a tracked file is not a refusal.
#[test]
fn approve_never_leaves_a_half_applied_merge_when_the_base_conflicts() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    // The run rewrites a file that already exists and is tracked.
    let run = harness.finished_run("conflicting-base", "src/a", &["src/a/keep.txt"])?;

    // The user rewrites the very same file on the base branch.
    fs::write(workspace.join("src/a/keep.txt"), "the user's own edit\n")?;
    git(&workspace, &["add", "--all"])?;
    git(&workspace, &["commit", "--quiet", "-m", "fix: my own edit"])?;

    let before = fingerprint(&workspace)?;
    let refused = harness.approve(&run.id, "feat: conflicting")?;
    assert_refusal(&refused, "moved");

    assert_eq!(fingerprint(&workspace)?, before);
    assert!(
        !merge_in_progress(&workspace),
        "approve left the primary checkout mid-merge"
    );
    let keep = fs::read_to_string(workspace.join("src/a/keep.txt"))?;
    assert_eq!(keep, "the user's own edit\n");
    assert!(
        !keep.contains("<<<<<<<"),
        "conflict markers in a tracked file"
    );
    assert_eq!(
        git_out(
            &workspace,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?,
        ""
    );
    // Not even the agent-side commit happened: every gate runs before the first mutation.
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    Ok(())
}

/// The cheap gates: a message that is blank or oversized, and a run with nothing to merge.
#[test]
fn approve_refuses_an_unusable_message_or_an_empty_run() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("empty-run", "src/a", &[])?;
    assert_eq!(harness.review(&run.id)?["changed_files"], json!([]));

    let before = fingerprint(&workspace)?;
    assert_refusal(&harness.approve(&run.id, "   ")?, "commit message");
    assert_refusal(
        &harness.approve(&run.id, &"x".repeat(257))?,
        "commit message",
    );
    assert_refusal(
        &harness.approve(&run.id, "feat: nothing at all")?,
        "no changes to approve",
    );
    assert_eq!(fingerprint(&workspace)?, before);
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    Ok(())
}

/// A worktree deleted and a branch removed out from under a live run.
///
/// The run is still in the dashboard's session map, so `approve` is reachable and has to fail on
/// a checkout that is no longer there. The point is not the error, it is that failing to find the
/// work leaves the primary checkout exactly as it was.
#[test]
fn approve_refuses_cleanly_when_the_run_lost_its_worktree_and_branch() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("vanished", "src/a", &["src/a/allowed.txt"])?;

    fs::remove_dir_all(&run.worktree)?;
    git(&workspace, &["worktree", "prune"])?;
    git(&workspace, &["branch", "-D", &run.branch])?;

    let before = fingerprint(&workspace)?;
    let refused = harness.approve(&run.id, "feat: into the void")?;
    assert_eq!(refused.status, 409, "{refused:?}");
    assert_eq!(fingerprint(&workspace)?, before);
    assert!(!merge_in_progress(&workspace));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Rejection
// ---------------------------------------------------------------------------------------------

/// Rejection preserves the work and touches nothing else.
///
/// Destroying a dirty worktree after a stopped run is an explicit product non-goal, so the
/// interesting assertions are the ones about what *survives*: the files the run wrote are still
/// on disk, the review still describes them, and the branch is still where it was because no
/// commit was ever made. The primary checkout is untouched by construction — `reject` has no
/// business going near it — and that is asserted rather than assumed.
#[test]
fn reject_preserves_the_work_and_leaves_the_primary_checkout_untouched() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.run(
        "rejected",
        "src/a",
        &["write src/a/allowed.txt", "say working"],
    )?;
    harness.await_output(&run.id, "write:src/a/allowed.txt")?;

    // Process state gates rejection too: a running agent could still be writing.
    assert_refusal(&harness.reject(&run.id)?, "stop the agent");
    assert_eq!(harness.status(&run.id)?, "running");

    assert_eq!(harness.stop(&run.id)?.status, 200);
    harness.await_status(&run.id, &["stopped", "failed"])?;

    let before = fingerprint(&workspace)?;
    let rejected = harness.reject(&run.id)?;
    assert_eq!(rejected.status, 200, "{rejected:?}");
    assert_eq!(harness.status(&run.id)?, "rejected");

    assert_eq!(fingerprint(&workspace)?, before);
    assert!(!merge_in_progress(&workspace));

    // The work is kept, not swept up.
    assert!(
        run.worktree.join("src/a/allowed.txt").is_file(),
        "rejection deleted the run's work"
    );
    assert_eq!(
        fs::read_to_string(run.worktree.join("src/a/allowed.txt"))?,
        WRITTEN
    );
    assert_eq!(tip(&workspace, &run.branch)?, run.base_commit);
    assert_eq!(
        harness.review(&run.id)?["changed_files"],
        json!(["src/a/allowed.txt"]),
        "a rejected run can no longer explain itself"
    );
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The dangerous middles
// ---------------------------------------------------------------------------------------------

/// Approving the same run twice in a row merges it once.
#[test]
fn approving_twice_in_sequence_merges_once() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("double-approve", "src/a", &["src/a/allowed.txt"])?;

    let first = harness.approve(&run.id, "feat: the first time")?;
    assert_eq!(first.status, 200, "{first:?}");
    let settled = fingerprint(&workspace)?;
    let head = tip(&workspace, "HEAD")?;

    let second = harness.approve(&run.id, "feat: the second time")?;
    assert_refusal(&second, "no changes to approve");
    assert_eq!(fingerprint(&workspace)?, settled);
    assert_eq!(tip(&workspace, "HEAD")?, head);
    assert_eq!(
        git_out(
            &workspace,
            &[
                "rev-list",
                "--count",
                "--merges",
                &format!("{}..HEAD", run.base_commit)
            ]
        )?,
        "1",
        "the second approval added a second merge commit"
    );
    assert!(!merge_in_progress(&workspace));
    Ok(())
}

/// Two approvals of the same run racing each other.
///
/// `approve` holds no lock across its gates and its git commands, so both requests can clear the
/// gates before either mutates anything. What must not happen is the compound outcome: two
/// commits on the agent branch, two merge commits, or a merge left open because one request tore
/// through another's.
///
/// What actually serialises them is git, not pidmesh: the loser is turned away by `index.lock`
/// in the agent worktree, before it can commit anything. That is a real guarantee and worth
/// pinning, but it is git's and could be reached by a different interleaving, so the assertions
/// below are written as invariants over the outcome rather than as one expected ordering.
#[test]
fn concurrent_approvals_cannot_merge_twice_or_leave_a_merge_open() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("racing", "src/a", &["src/a/allowed.txt"])?;

    let replies = std::thread::scope(|scope| -> Result<Vec<Reply>> {
        let left = scope.spawn(|| harness.approve(&run.id, "feat: racer one"));
        let right = scope.spawn(|| harness.approve(&run.id, "feat: racer two"));
        let left = left
            .join()
            .map_err(|_| anyhow::anyhow!("racer one panicked"))??;
        let right = right
            .join()
            .map_err(|_| anyhow::anyhow!("racer two panicked"))??;
        Ok(vec![left, right])
    })?;
    let winners = replies.iter().filter(|reply| reply.status == 200).count();
    assert!(
        winners <= 1,
        "both approvals claimed to have merged: {replies:?}"
    );
    for reply in &replies {
        assert!(
            reply.status == 200 || reply.status == 409,
            "an approval failed in an unexpected way: {reply:?}"
        );
    }

    assert!(
        !merge_in_progress(&workspace),
        "a racing approval left the primary checkout mid-merge"
    );
    assert_eq!(
        git_out(
            &workspace,
            &["status", "--porcelain=v1", "--untracked-files=all"]
        )?,
        "",
        "a racing approval left the primary checkout dirty"
    );
    let agent_commits: u32 = git_out(
        &workspace,
        &[
            "rev-list",
            "--count",
            &format!("{}..{}", run.base_commit, run.branch),
        ],
    )?
    .parse()?;
    assert!(
        agent_commits <= 1,
        "the agent branch picked up {agent_commits} commits from two approvals"
    );
    let merges: u32 = git_out(
        &workspace,
        &[
            "rev-list",
            "--count",
            "--merges",
            &format!("{}..HEAD", run.base_commit),
        ],
    )?
    .parse()?;
    assert_eq!(
        u32::try_from(winners)?,
        merges,
        "merge commits in the primary checkout did not match successful approvals: {replies:?}"
    );
    Ok(())
}

/// A merge that fails *after* git has already started applying it.
///
/// Every gate runs before the first mutation, so the merge is the one step that can fail
/// part-way. A repository hook is the realistic way to reach that state — a declined
/// `pre-merge-commit` leaves the merge staged in the index and written into the working tree with
/// `MERGE_HEAD` set, which is the same half-applied shape a conflict produces. The error is
/// expected; what is asserted is that the primary checkout is handed back exactly as it was,
/// rather than left for the user to discover mid-merge.
#[test]
fn a_merge_that_fails_part_way_is_unwound_from_the_primary_checkout() -> Result<()> {
    let harness = Harness::start()?;
    let workspace = harness.workspace();
    let run = harness.finished_run("declined", "src/a", &["src/a/allowed.txt"])?;

    let hook = workspace.join(".git/hooks/pre-merge-commit");
    fs::create_dir_all(workspace.join(".git/hooks"))?;
    fs::write(&hook, "#!/bin/sh\nexit 1\n")?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;

    // The commit graph will change — the agent-side commit happens before the merge and is
    // deliberately kept — so this compares the primary checkout rather than the whole repository.
    let before = checkout_state(&workspace)?;
    let refused = harness.approve(&run.id, "feat: declined by a hook")?;
    assert_eq!(refused.status, 409, "{refused:?}");
    assert!(
        refused.body.contains("left as it was"),
        "the refusal did not say the checkout was restored: {refused:?}"
    );

    assert!(
        !merge_in_progress(&workspace),
        "a failed merge was left in progress in the primary checkout"
    );
    assert_eq!(
        checkout_state(&workspace)?,
        before,
        "a failed merge changed the primary checkout"
    );
    assert_eq!(tip(&workspace, "HEAD")?, run.base_commit);
    assert!(
        !workspace.join("src/a/allowed.txt").exists(),
        "a failed merge left the agent's file in the primary checkout"
    );
    assert_eq!(git_out(&workspace, &["branch", "--show-current"])?, "main");
    Ok(())
}
