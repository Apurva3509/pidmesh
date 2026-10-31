use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use pidmesh::converge::{Participant, classify, integration_head, scan_worktree};
use pidmesh::render::{Shape, render};
use pidmesh::store::MeshStore;
use serde_json::{Value, json};
use tempfile::TempDir;

fn git(directory: &Path, arguments: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {:?} failed: {}",
        arguments,
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

struct Fleet {
    _directory: TempDir,
    repository: PathBuf,
    store: MeshStore,
}

impl Fleet {
    /// A repository with one commit and a mesh scoped to its root.
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository)?;
        git(&repository, &["init", "-b", "main"])?;
        git(
            &repository,
            &["config", "user.email", "pidmesh@example.com"],
        )?;
        git(&repository, &["config", "user.name", "PidMesh Test"])?;
        std::fs::create_dir(repository.join("src"))?;
        std::fs::write(repository.join("src/shared.rs"), "fn shared() {}\n")?;
        std::fs::write(repository.join("src/legacy.rs"), "legacy\n")?;
        git(&repository, &["add", "-A"])?;
        git(&repository, &["commit", "-m", "initial"])?;
        let store = MeshStore::new(directory.path().join("mesh.db"))?;
        Ok(Self {
            _directory: directory,
            repository,
            store,
        })
    }

    /// Add a linked worktree on its own branch and register an agent for it.
    fn agent(&self, name: &str) -> Result<(String, PathBuf)> {
        let worktree = self.repository.join("..").join(format!("wt-{name}"));
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "-b",
                &format!("feat/{name}"),
                worktree.to_str().context("worktree path")?,
            ],
        )?;
        let registration = self.store.register_agent(
            name,
            std::process::id(),
            Some(&self.repository),
            "test",
            &[],
            None,
        )?;
        let agent_id = registration["agent_id"]
            .as_str()
            .context("missing agent id")?
            .to_owned();
        // The mesh is shared (one repository root) while each agent works in its own checkout,
        // which is what linked-worktree discovery produces for a real fleet.
        self.store
            .update_agent_checkout(&agent_id, &worktree, Some(&format!("feat/{name}")))?;
        Ok((agent_id, worktree))
    }

    /// Observe a checkout and publish the result, exactly as `pidmesh sync` does.
    fn sync(&self, agent_id: &str, worktree: &Path) -> Result<Value> {
        let scan = scan_worktree(worktree, Some("main"))?;
        self.store.publish_footprint(agent_id, &scan)
    }

    fn collisions(&self, agent_id: &str) -> Result<Vec<Value>> {
        Ok(self.store.collisions(agent_id)?["collisions"]
            .as_array()
            .context("missing collisions")?
            .clone())
    }
}

/// Find the report entry for one path.
fn at<'a>(collisions: &'a [Value], path: &str) -> Option<&'a Value> {
    collisions
        .iter()
        .find(|collision| collision["path"] == path)
}

#[test]
fn overlapping_worktrees_collide_without_anyone_reserving_a_path() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;

    fleet.sync(&alpha, &alpha_tree)?;
    let report = fleet.sync(&bravo, &bravo_tree)?;

    // Neither agent called reserve; the overlap is observed, not declared.
    assert_eq!(report["collision_count"], 1);
    let collisions = fleet.collisions(&alpha)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");
    assert_eq!(shared["involves_me"], true);
    assert_eq!(shared["participants"].as_array().map(Vec::len), Some(2));
    Ok(())
}

#[test]
fn byte_identical_edits_are_duplicated_effort_rather_than_a_conflict() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(alpha_tree.join("src/shared.rs"), "fn shared() { same() }\n")?;
    std::fs::write(bravo_tree.join("src/shared.rs"), "fn shared() { same() }\n")?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let shared = fleet.collisions(&alpha)?;
    let shared = at(&shared, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "identical");
    Ok(())
}

#[test]
fn deleting_a_path_another_agent_is_editing_is_flagged() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/legacy.rs"),
        "legacy but still needed\n",
    )?;
    std::fs::remove_file(bravo_tree.join("src/legacy.rs"))?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&bravo)?;
    let legacy = at(&collisions, "src/legacy.rs").context("expected a collision")?;
    assert_eq!(legacy["severity"], "delete_edit");
    Ok(())
}

#[test]
fn work_on_separate_paths_never_collides() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(alpha_tree.join("src/only-alpha.rs"), "a\n")?;
    std::fs::write(bravo_tree.join("src/only-bravo.rs"), "b\n")?;

    fleet.sync(&alpha, &alpha_tree)?;
    let report = fleet.sync(&bravo, &bravo_tree)?;

    assert_eq!(report["collision_count"], 0);
    let collisions = fleet.collisions(&alpha)?;
    assert_eq!(
        collisions.len(),
        0,
        "expected no contention, got {collisions:?}"
    );
    Ok(())
}

#[test]
fn a_worktree_cut_from_an_older_base_is_reported_as_divergent() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;

    // Someone lands work on main, so every existing worktree is now stale.
    std::fs::write(
        fleet.repository.join("src/shared.rs"),
        "fn shared() { /* landed */ }\n",
    )?;
    git(&fleet.repository, &["commit", "-am", "land upstream"])?;

    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* landed + b */ }\n",
    )?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&bravo)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");
    assert_eq!(
        shared["base_divergent"], true,
        "the two checkouts were cut from different commits"
    );
    Ok(())
}

#[test]
fn a_footprint_is_authoritative_so_abandoned_work_clears_its_collision() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    assert_eq!(fleet.collisions(&alpha)?.len(), 1);

    // Alpha drops its change; the next scan withdraws it from the contested path.
    git(&alpha_tree, &["checkout", "--", "src/shared.rs"])?;
    fleet.sync(&alpha, &alpha_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    assert_eq!(
        collisions.len(),
        0,
        "expected no contention, got {collisions:?}"
    );
    let cleared = fleet.store.events(&alpha, 0, 500)?;
    let cleared = cleared
        .as_array()
        .context("missing events")?
        .iter()
        .filter(|event| event["event_type"] == "collision.cleared")
        .count();
    assert_eq!(cleared, 1);
    Ok(())
}

#[test]
fn a_collision_wakes_peers_once_rather_than_on_every_scan() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let detected = |store: &MeshStore| -> Result<usize> {
        Ok(store
            .events(&alpha, 0, 500)?
            .as_array()
            .context("missing events")?
            .iter()
            .filter(|event| event["event_type"] == "collision.detected")
            .count())
    };
    let first = detected(&fleet.store)?;
    assert_eq!(first, 1, "the overlap is announced when it becomes real");

    // A polling fleet re-scans constantly; that must not flood the event stream.
    for _ in 0..5 {
        fleet.sync(&alpha, &alpha_tree)?;
        fleet.sync(&bravo, &bravo_tree)?;
    }
    assert_eq!(
        detected(&fleet.store)?,
        first,
        "unchanged state must not re-announce"
    );
    Ok(())
}

#[test]
fn committed_and_uncommitted_work_both_count_toward_the_footprint() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Alpha commits its change; bravo leaves its own dirty in the working tree.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha work"])?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");
    Ok(())
}

#[test]
fn two_sessions_sharing_a_checkout_are_one_editor_not_a_collision() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;

    // A worker commonly runs a CLI session and an MCP session against the same worktree.
    let second = fleet.store.register_agent(
        "alpha-mcp",
        std::process::id(),
        Some(&fleet.repository),
        "test",
        &[],
        None,
    )?;
    let second = second["agent_id"]
        .as_str()
        .context("missing agent id")?
        .to_owned();
    fleet
        .store
        .update_agent_checkout(&second, &alpha_tree, Some("feat/alpha"))?;

    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    let report = fleet.sync(&second, &alpha_tree)?;

    assert_eq!(
        report["collision_count"], 0,
        "one checkout cannot collide with itself"
    );
    Ok(())
}

/// A repository driven through the CLI, with one linked worktree and joined agent per worker.
///
/// Agents join from inside their checkout so linked-worktree discovery unifies the mesh, which is
/// what a real fleet does and what an explicit `--workspace` would bypass.
struct CliFleet {
    repository: PathBuf,
    database: PathBuf,
    workers: Vec<(String, PathBuf)>,
}

impl CliFleet {
    fn new(root: &Path, workers: usize, pid: Option<u32>) -> Result<Self> {
        let repository = root.join("repository");
        let database = root.join("fleet.db");
        std::fs::create_dir(&repository)?;
        git(&repository, &["init", "-b", "main"])?;
        git(
            &repository,
            &["config", "user.email", "pidmesh@example.com"],
        )?;
        git(&repository, &["config", "user.name", "PidMesh Test"])?;
        std::fs::create_dir(repository.join("src"))?;
        std::fs::write(repository.join("src/shared.rs"), "fn shared() {}\n")?;
        git(&repository, &["add", "-A"])?;
        git(&repository, &["commit", "-m", "initial"])?;

        let mut joined = Vec::new();
        for worker in 0..workers {
            let worktree = root.join(format!("wt-{worker}"));
            git(
                &repository,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &format!("feat/{worker}"),
                    worktree.to_str().context("worktree path")?,
                ],
            )?;
            // Every worktree edits the same path differently, plus one path of its own.
            std::fs::write(
                worktree.join("src/shared.rs"),
                format!("fn shared() {{ /* {worker} */ }}\n"),
            )?;
            std::fs::write(worktree.join(format!("src/only-{worker}.rs")), "private\n")?;
            let mut arguments = vec![
                "--db".to_owned(),
                database.to_string_lossy().into_owned(),
                "join".to_owned(),
                "--name".to_owned(),
                format!("worker-{worker}"),
            ];
            if let Some(pid) = pid {
                arguments.push("--pid".to_owned());
                arguments.push(pid.to_string());
            }
            let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
                .current_dir(&worktree)
                .env_remove("PIDMESH_WORKSPACE")
                .args(&arguments)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let registration: Value = serde_json::from_slice(&output.stdout)?;
            joined.push((
                registration["agent_id"]
                    .as_str()
                    .context("agent id")?
                    .to_owned(),
                worktree,
            ));
        }
        Ok(Self {
            repository,
            database,
            workers: joined,
        })
    }

    /// Run a pidmesh subcommand from a directory and parse its JSON.
    fn run(&self, directory: &Path, arguments: &[&str]) -> Result<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
            .current_dir(directory)
            .env_remove("PIDMESH_WORKSPACE")
            .args(["--db", self.database.to_str().context("database path")?])
            .args(arguments)
            .output()?;
        assert!(
            output.status.success(),
            "pidmesh {arguments:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

/// Contributing guide: transaction changes need multiprocess coverage.
///
/// Eight independent processes scan eight worktrees and publish footprints concurrently against one
/// `SQLite` database. Every write must land, and the contested path must end up with exactly eight
/// participants rather than a torn or partially-overwritten set.
#[test]
fn eight_processes_publish_footprints_concurrently() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let fleet = CliFleet::new(directory.path(), 8, None)?;
    let (database, workers) = (&fleet.database, &fleet.workers);

    let mut children = Vec::new();
    for (agent_id, worktree) in workers {
        children.push(
            Command::new(env!("CARGO_BIN_EXE_pidmesh"))
                .current_dir(worktree)
                .env_remove("PIDMESH_WORKSPACE")
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "sync",
                    "--agent",
                    agent_id,
                    "--base",
                    "main",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    for child in children {
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "concurrent sync failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let report = fleet.run(
        &fleet.repository,
        &["collisions", "--agent", workers[0].0.as_str()],
    )?;
    let collisions = report["collisions"].as_array().context("collisions")?;
    assert_eq!(
        collisions.len(),
        1,
        "only the shared path is contested: {report}"
    );
    assert_eq!(collisions[0]["path"], "src/shared.rs");
    assert_eq!(
        collisions[0]["participants"].as_array().map(Vec::len),
        Some(8),
        "every concurrent write must survive"
    );
    assert_eq!(collisions[0]["severity"], "divergent");
    Ok(())
}

/// Regression: a large change set used to deadlock.
///
/// Hashing shelled out to `git hash-object --stdin-paths` and wrote every path to its stdin
/// before reading any stdout. Once the child's unread output filled the pipe it stopped reading,
/// the parent filled the stdin pipe, and both blocked forever with no timeout. `pidmesh sync`
/// hung unkillable, and a `watch` sweep froze convergence for the whole fleet.
#[test]
fn a_large_change_set_does_not_hang_the_scan() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let bulk = alpha_tree.join("bulk");
    std::fs::create_dir(&bulk)?;
    // Comfortably past the observed stall threshold in both bytes and count.
    for index in 0..9_000 {
        std::fs::write(
            bulk.join(format!("file-with-a-fairly-long-name-{index:06}.txt")),
            "x".repeat(40),
        )?;
    }
    let report = fleet.sync(&alpha, &alpha_tree)?;
    assert_eq!(report["paths"], 9_000);
    Ok(())
}

/// Regression: one unhashable path used to abort the whole scan.
///
/// `git hash-object` dies on the first path it cannot read, and that failure was turned into a
/// scan-wide error. A staged-then-deleted file, a dirty submodule, or an untracked nested
/// repository was enough to make a checkout permanently unscannable.
#[test]
fn an_unreadable_path_degrades_to_no_digest_instead_of_failing() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;

    // Staged, then removed from the worktree: git reports `AD`, the file is not there.
    std::fs::write(alpha_tree.join("src/staged.rs"), "staged\n")?;
    git(&alpha_tree, &["add", "src/staged.rs"])?;
    std::fs::remove_file(alpha_tree.join("src/staged.rs"))?;

    // An untracked nested repository, which `-uall` reports as a single directory entry.
    let nested = alpha_tree.join("vendor");
    std::fs::create_dir(&nested)?;
    git(&nested, &["init", "-b", "main"])?;
    std::fs::write(nested.join("inner.txt"), "inner\n")?;

    // A real edit alongside them must still be observed.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;

    let scan = scan_worktree(&alpha_tree, Some("main"))?;
    let shared = scan
        .entries
        .iter()
        .find(|entry| entry.path == "src/shared.rs")
        .context("the ordinary edit must survive")?;
    assert!(shared.digest.is_some(), "a readable file is still hashed");

    let staged = scan
        .entries
        .iter()
        .find(|entry| entry.path == "src/staged.rs")
        .context("the staged-then-deleted path is still reported")?;
    assert_eq!(
        staged.change_kind.as_str(),
        "deleted",
        "the filesystem decides, not the git status letter"
    );
    assert!(staged.digest.is_none());

    fleet.sync(&alpha, &alpha_tree)?;
    Ok(())
}

/// Regression: a filename starting with a quote used to take another file's digest.
///
/// `git hash-object --stdin-paths` C-unquotes any line beginning with `"`, so the footprint for
/// `"quoted.txt"` silently recorded the digest of `quoted.txt`. Two checkouts holding different
/// bytes could then be reported `identical`, which reads as safe to merge.
#[test]
fn a_quoted_filename_does_not_borrow_another_files_digest() -> Result<()> {
    let fleet = Fleet::new()?;
    let (_alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(alpha_tree.join("quoted.txt"), "plain contents\n")?;
    std::fs::write(alpha_tree.join("\"quoted.txt\""), "different contents\n")?;

    let scan = scan_worktree(&alpha_tree, Some("main"))?;
    let plain = scan
        .entries
        .iter()
        .find(|entry| entry.path == "quoted.txt")
        .and_then(|entry| entry.digest.clone());
    let quoted = scan
        .entries
        .iter()
        .find(|entry| entry.path == "\"quoted.txt\"")
        .and_then(|entry| entry.digest.clone());
    assert!(plain.is_some() && quoted.is_some(), "both are hashed");
    assert_ne!(plain, quoted, "different bytes must never share a digest");
    Ok(())
}

/// Regression: a stale footprint from a co-located session used to argue with its own checkout.
///
/// The contested-path test collapsed agents by checkout but the participant query did not, so a
/// second session in the same worktree contributed an independent opinion. Two byte-identical
/// checkouts were reported `divergent`, the severity that means work is being overwritten.
#[test]
fn a_stale_session_in_one_checkout_does_not_fabricate_divergence() -> Result<()> {
    let fleet = Fleet::new()?;
    let (first, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    let second = fleet.store.register_agent(
        "alpha-mcp",
        std::process::id(),
        Some(&fleet.repository),
        "test",
        &[],
        None,
    )?;
    let second = second["agent_id"]
        .as_str()
        .context("missing agent id")?
        .to_owned();
    fleet
        .store
        .update_agent_checkout(&second, &alpha_tree, Some("feat/alpha"))?;

    // The first session records one version, then the file changes underneath it.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* v1 */ }\n",
    )?;
    fleet.sync(&first, &alpha_tree)?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* v2 */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* v2 */ }\n",
    )?;
    fleet.sync(&second, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&bravo)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(
        shared["severity"], "identical",
        "both worktrees hold the same bytes: {shared}"
    );
    assert_eq!(
        shared["participants"].as_array().map(Vec::len),
        Some(2),
        "one opinion per checkout, not per session"
    );
    Ok(())
}

/// Regression: two agents deleting the same file were reported as divergent.
///
/// Deletions carry no digest, so delete/delete fell past the `delete_edit` branch and past the
/// content comparison into `divergent`. Removing the same dead file on two branches merges
/// cleanly; flagging it as the top severity is a false positive in ordinary cleanup work.
#[test]
fn two_agents_deleting_the_same_file_do_not_conflict() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::remove_file(alpha_tree.join("src/legacy.rs"))?;
    std::fs::remove_file(bravo_tree.join("src/legacy.rs"))?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let legacy = at(&collisions, "src/legacy.rs").context("expected a collision")?;
    assert_eq!(legacy["severity"], "identical", "{legacy}");
    Ok(())
}

/// Build a repository with two linked worktrees and join an agent in each.
///
/// When `explicit_workspace` is set, each agent pins its own checkout as the workspace, which is
/// the mistake that silently splits one repository into unrelated meshes.
fn two_worktree_repository(root: &Path, explicit_workspace: bool) -> Result<(PathBuf, PathBuf)> {
    let repository = root.join("repository");
    let database = root.join("doctor.db");
    std::fs::create_dir(&repository)?;
    git(&repository, &["init", "-b", "main"])?;
    git(
        &repository,
        &["config", "user.email", "pidmesh@example.com"],
    )?;
    git(&repository, &["config", "user.name", "PidMesh Test"])?;
    std::fs::write(repository.join("file.txt"), "mesh\n")?;
    git(&repository, &["add", "-A"])?;
    git(&repository, &["commit", "-m", "initial"])?;
    for worker in 0..2 {
        let worktree = root.join(format!("wt-{worker}"));
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                &format!("feat/{worker}"),
                worktree.to_str().context("worktree path")?,
            ],
        )?;
        let mut arguments = vec![
            "--db".to_owned(),
            database.to_string_lossy().into_owned(),
            "join".to_owned(),
            "--name".to_owned(),
            format!("worker-{worker}"),
            "--pid".to_owned(),
            std::process::id().to_string(),
        ];
        if explicit_workspace {
            arguments.push("--workspace".to_owned());
            arguments.push(worktree.to_string_lossy().into_owned());
        }
        let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
            .current_dir(&worktree)
            .env_remove("PIDMESH_WORKSPACE")
            .args(&arguments)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok((database, root.join("wt-0")))
}

fn doctor(database: &Path, from: &Path) -> Result<(Value, bool)> {
    doctor_with_workspace(database, from, None)
}

fn doctor_with_workspace(
    database: &Path,
    from: &Path,
    workspace: Option<&Path>,
) -> Result<(Value, bool)> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pidmesh"));
    command
        .current_dir(from)
        .env_remove("PIDMESH_WORKSPACE")
        .env_remove("PIDMESH_CHECKOUT")
        .args([
            "--db",
            database.to_str().context("database path")?,
            "doctor",
        ]);
    if let Some(workspace) = workspace {
        command.env("PIDMESH_WORKSPACE", workspace);
    }
    let output = command.output()?;
    Ok((
        serde_json::from_slice(&output.stdout)?,
        output.status.success(),
    ))
}

/// The watcher is what makes convergence autonomic.
///
/// Agents register — any harness does that — but never call sync themselves. A single supervisor
/// pass must still discover every overlap, because registration already recorded each checkout.
#[test]
fn a_watcher_observes_a_fleet_that_never_calls_sync() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repository = directory.path().join("repository");
    let database = directory.path().join("watch.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    std::fs::create_dir(&repository)?;
    git(&repository, &["init", "-b", "main"])?;
    git(
        &repository,
        &["config", "user.email", "pidmesh@example.com"],
    )?;
    git(&repository, &["config", "user.name", "PidMesh Test"])?;
    std::fs::create_dir(repository.join("src"))?;
    std::fs::write(repository.join("src/shared.rs"), "fn shared() {}\n")?;
    git(&repository, &["add", "-A"])?;
    git(&repository, &["commit", "-m", "initial"])?;

    for worker in 0..3 {
        let worktree = directory.path().join(format!("wt-{worker}"));
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                &format!("feat/{worker}"),
                worktree.to_str().context("worktree path")?,
            ],
        )?;
        std::fs::write(
            worktree.join("src/shared.rs"),
            format!("fn shared() {{ /* {worker} */ }}\n"),
        )?;
        let output = Command::new(binary)
            .current_dir(&worktree)
            .env_remove("PIDMESH_WORKSPACE")
            .args([
                "--db",
                database.to_str().context("database path")?,
                "join",
                "--name",
                &format!("worker-{worker}"),
                // A live PID, since the watcher skips checkouts whose process is gone.
                "--pid",
                &std::process::id().to_string(),
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = Command::new(binary)
        .current_dir(&repository)
        .env_remove("PIDMESH_WORKSPACE")
        .args([
            "--db",
            database.to_str().context("database path")?,
            "watch",
            "--once",
            "--base",
            "main",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pass: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(pass["checkouts"], 3, "each worktree is scanned once");
    assert_eq!(pass["published"], 3);
    assert_eq!(pass["skipped"].as_array().map(Vec::len), Some(0));
    assert_eq!(pass["collision_count"], 1, "{pass}");
    assert_eq!(pass["collisions"][0]["path"], "src/shared.rs");
    assert_eq!(
        pass["collisions"][0]["participants"]
            .as_array()
            .map(Vec::len),
        Some(3)
    );
    Ok(())
}

/// A repository whose checkouts register and then never cooperate, swept by the real CLI.
///
/// The watcher is only meaningful end to end. `watch_pass` assembles its answer from a live
/// sweep and hands it to the renderer, so a store-level test proves nothing about what the
/// supervisor actually reports.
struct Supervised {
    _directory: TempDir,
    repository: PathBuf,
    database: PathBuf,
}

impl Supervised {
    fn new(files: &[(&str, &str)]) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository)?;
        git(&repository, &["init", "-b", "main"])?;
        git(
            &repository,
            &["config", "user.email", "pidmesh@example.com"],
        )?;
        git(&repository, &["config", "user.name", "PidMesh Test"])?;
        std::fs::create_dir(repository.join("src"))?;
        for (path, body) in files {
            std::fs::write(repository.join(path), body)?;
        }
        git(&repository, &["add", "-A"])?;
        git(&repository, &["commit", "-m", "initial"])?;
        let database = directory.path().join("watch.db");
        Ok(Self {
            _directory: directory,
            repository,
            database,
        })
    }

    /// A linked worktree joined with a live PID, exactly as any harness registers one.
    ///
    /// A sweep groups checkouts by path, so the name chosen here decides which agent the sweep
    /// happens to reach first — which is the whole question these tests are about.
    fn checkout(&self, name: &str) -> Result<PathBuf> {
        let worktree = self.repository.join("..").join(format!("wt-{name}"));
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "-b",
                &format!("feat/{name}"),
                worktree.to_str().context("worktree path")?,
            ],
        )?;
        let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
            .current_dir(&worktree)
            .env_remove("PIDMESH_WORKSPACE")
            .args([
                "--db",
                self.database.to_str().context("database path")?,
                "join",
                "--name",
                name,
                "--pid",
                &std::process::id().to_string(),
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "join failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(worktree)
    }

    /// One `pidmesh watch --once`, as the JSON agents parse and the text a person reads.
    fn sweep(&self) -> Result<(Value, String)> {
        let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
            .current_dir(&self.repository)
            .env_remove("PIDMESH_WORKSPACE")
            .args([
                "--db",
                self.database.to_str().context("database path")?,
                "watch",
                "--once",
                "--base",
                "main",
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let pass: Value = serde_json::from_slice(&output.stdout)?;
        let rendered = render(Shape::Watch, &pass).context("a sweep has a rendering")?;
        Ok((pass, rendered))
    }
}

/// The supervisor has to see the conflict that contests no path.
///
/// `watch` is the one surface built for agents that never cooperate, and a symbol break is the
/// one conflict `git merge` reports as clean. The sweep published the footprints that detect it,
/// reaped on their behalf, and then printed contested paths and nothing else — so the fleet-wide
/// view was blind to exactly the class it exists to catch.
#[test]
fn a_sweep_reports_the_symbol_break_that_contests_no_path() -> Result<()> {
    let fleet = Supervised::new(&[
        ("src/api.rs", "pub fn compute_total() -> u32 {\n    7\n}\n"),
        ("src/caller.rs", "fn unused() {}\n"),
        ("src/notes.rs", "fn notes() {}\n"),
    ])?;
    // Checkouts are swept in path order, so the first one reached is a bystander to the break
    // below: it neither withdraws the name nor calls it. A watcher reporting only its witness's
    // own breaks finds nothing at all here.
    let bystander = fleet.checkout("aaa")?;
    let remover = fleet.checkout("mmm")?;
    let caller = fleet.checkout("zzz")?;
    std::fs::write(
        bystander.join("src/notes.rs"),
        "fn notes() { /* idle */ }\n",
    )?;
    std::fs::write(
        remover.join("src/api.rs"),
        "pub fn compute_subtotal() -> u32 {\n    7\n}\n",
    )?;
    std::fs::write(
        caller.join("src/caller.rs"),
        "fn report() -> u32 {\n    compute_total()\n}\n",
    )?;

    let (pass, rendered) = fleet.sweep()?;
    assert_eq!(pass["checkouts"], 3, "{pass}");
    // Three checkouts, three different files: path comparison alone reports a healthy fleet.
    assert_eq!(pass["collision_count"], 0, "{pass}");

    assert_eq!(pass["symbol_break_count"], 1, "{pass}");
    let breaks = pass["symbol_breaks"].as_array().context("symbol breaks")?;
    assert_eq!(breaks.len(), 1, "{pass}");
    assert_eq!(breaks[0]["symbol"], "compute_total");
    assert_eq!(breaks[0]["removed_from"], "src/api.rs");
    assert_eq!(breaks[0]["used_by"]["agent_name"], "zzz");
    assert!(
        breaks[0]["removed_by"]
            .as_str()
            .is_some_and(|agent| agent.starts_with("mmm-")),
        "{pass}"
    );
    // The break belongs to neither side of the agent the sweep reached first, which is the
    // point: a supervisor reports the workspace's breaks, not one session's.
    assert_eq!(breaks[0]["involves_me"], false, "{pass}");
    assert_eq!(pass["symbol_breaks_truncated"], false, "{pass}");

    // And it reaches the person watching, in the same words `collisions` uses.
    assert!(rendered.contains("1 symbol break"), "{rendered}");
    assert!(
        rendered.contains("compute_total() removed from src/api.rs, still used by zzz"),
        "{rendered}"
    );
    Ok(())
}

/// A fleet that withdraws nothing must not grow a symbol-break section.
///
/// The clean case is half the contract: a report that always finds something is as useless as
/// one that never does.
#[test]
fn a_sweep_without_a_withdrawn_export_reports_no_symbol_break() -> Result<()> {
    let fleet = Supervised::new(&[
        ("src/api.rs", "pub fn compute_total() -> u32 {\n    7\n}\n"),
        ("src/shared.rs", "fn shared() {}\n"),
    ])?;
    let alpha = fleet.checkout("alpha")?;
    let bravo = fleet.checkout("bravo")?;
    // A real contested path, so the sweep is demonstrably reporting *something*. Both keep
    // calling the export; neither withdraws it.
    std::fs::write(
        alpha.join("src/shared.rs"),
        "fn shared() { compute_total(); /* a */ }\n",
    )?;
    std::fs::write(
        bravo.join("src/shared.rs"),
        "fn shared() { compute_total(); /* b */ }\n",
    )?;

    let (pass, rendered) = fleet.sweep()?;
    assert_eq!(pass["collision_count"], 1, "{pass}");
    assert_eq!(pass["symbol_break_count"], 0, "{pass}");
    assert_eq!(
        pass["symbol_breaks"].as_array().map(Vec::len),
        Some(0),
        "{pass}"
    );
    assert_eq!(pass["symbol_breaks_truncated"], false, "{pass}");
    assert!(!rendered.contains("symbol break"), "{rendered}");
    Ok(())
}

#[test]
fn a_missing_checkout_is_skipped_rather_than_failing_the_sweep() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;

    // Scanning a checkout that has been removed is an error, not a panic.
    let removed = alpha_tree.join("does-not-exist");
    assert!(scan_worktree(&removed, Some("main")).is_err());
    Ok(())
}

#[test]
fn withdrawing_a_footprint_clears_contention_without_a_scan() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    assert_eq!(fleet.collisions(&alpha)?.len(), 1);

    // An agent shutting down cannot always produce a scan, so withdrawal is explicit.
    let released = fleet.store.release_footprint(&alpha)?;
    assert_eq!(released["released"], 1);
    let collisions = fleet.collisions(&bravo)?;
    assert_eq!(
        collisions.len(),
        0,
        "expected no contention, got {collisions:?}"
    );

    let cleared = fleet.store.events(&bravo, 0, 500)?;
    let cleared = cleared
        .as_array()
        .context("missing events")?
        .iter()
        .filter(|event| event["event_type"] == "collision.cleared")
        .count();
    assert_eq!(cleared, 1);
    Ok(())
}

#[test]
fn doctor_reports_a_healthy_mesh_when_discovery_groups_the_worktrees() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (database, worktree) = two_worktree_repository(directory.path(), false)?;
    let (report, succeeded) = doctor(&database, &worktree)?;
    assert!(succeeded, "a healthy mesh exits zero: {report}");
    assert_eq!(report["healthy"], true, "{report}");
    assert_eq!(report["linked_worktree"], true);
    assert_eq!(report["warnings"].as_array().map(Vec::len), Some(0));
    Ok(())
}

/// `PIDMESH_WORKSPACE` naming a mesh this process is not standing in.
///
/// Same family as the split mesh above, and the shape #37 was reported for: the workspace and
/// the checkout are two different things, so a process pointed at one while sitting outside it
/// observes a directory nobody is editing. Nothing fails, which is exactly why doctor has to
/// say it.
#[test]
fn doctor_names_a_workspace_that_disagrees_with_the_checkout() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (database, worktree) = two_worktree_repository(directory.path(), false)?;
    let repository = worktree
        .parent()
        .context("worktree parent")?
        .join("repository");
    let elsewhere = directory.path().join("elsewhere");
    std::fs::create_dir(&elsewhere)?;

    // Standing inside the mesh is healthy, whether or not the workspace is named explicitly:
    // a managed agent is started exactly like this, cwd in its worktree and the mesh root set.
    let (report, _) = doctor_with_workspace(&database, &worktree, Some(&repository))?;
    let codes = warning_codes(&report);
    assert!(
        !codes.contains(&"checkout_mismatch"),
        "a checkout of the named mesh is not a mismatch: {report}"
    );
    assert_eq!(
        report["resolved_checkout"],
        json!(worktree.canonicalize()?.to_string_lossy()),
        "the checkout is where the process is, not the mesh root: {report}"
    );

    // A subdirectory of that worktree is still that worktree, and must not be reported as a
    // disagreement: working from `src/` is what people actually do.
    let inside = worktree.join("nested");
    std::fs::create_dir(&inside)?;
    let (report, _) = doctor_with_workspace(&database, &inside, Some(&repository))?;
    assert!(
        !warning_codes(&report).contains(&"checkout_mismatch"),
        "a subdirectory of the checkout is the checkout: {report}"
    );

    // Standing outside it is not.
    let (report, succeeded) = doctor_with_workspace(&database, &elsewhere, Some(&repository))?;
    assert!(
        !succeeded,
        "a mismatched checkout must exit non-zero: {report}"
    );
    let warnings = report["warnings"].as_array().context("warnings")?;
    let mismatch = warnings
        .iter()
        .find(|warning| warning["code"] == "checkout_mismatch")
        .context("doctor missed a workspace that disagrees with the checkout")?;
    let detail = mismatch["detail"].as_str().context("detail")?;
    assert!(
        detail.contains("PIDMESH_CHECKOUT"),
        "the warning has to say what to set: {detail}"
    );
    assert_eq!(
        mismatch["checkout"],
        json!(repository.canonicalize()?.to_string_lossy()),
        "the directory actually observed is named: {mismatch}"
    );
    Ok(())
}

fn warning_codes(report: &Value) -> Vec<&str> {
    report["warnings"]
        .as_array()
        .map(|warnings| {
            warnings
                .iter()
                .filter_map(|warning| warning["code"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// The worst failure mode this system has: every command succeeds while nothing coordinates.
#[test]
fn doctor_catches_a_mesh_silently_split_by_an_explicit_workspace() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let (database, worktree) = two_worktree_repository(directory.path(), true)?;
    let (report, succeeded) = doctor(&database, &worktree)?;
    assert!(!succeeded, "a split mesh must exit non-zero: {report}");
    assert_eq!(report["healthy"], false);
    let warnings = report["warnings"].as_array().context("warnings")?;
    assert_eq!(warnings.len(), 1, "{report}");
    assert_eq!(warnings[0]["code"], "split_mesh");
    assert_eq!(
        warnings[0]["roots"].as_array().map(Vec::len),
        Some(2),
        "both orphaned roots are named so the operator can see the damage"
    );
    Ok(())
}

/// Regression: a dead agent's footprint could never be collected.
///
/// The reaper only removed footprints while transitioning an agent from running to dead, and it
/// selects only running sessions, so a worktree removed after the agent died was never
/// revisited. The phantom collision was permanent and the dead agent could not run `unsync`.
#[test]
fn a_worktree_removed_after_its_agent_died_stops_contesting() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    assert_eq!(fleet.collisions(&bravo)?.len(), 1);

    // Alpha exits cleanly; its preserved worktree still holds the uncommitted work.
    fleet.store.stop_agent(&alpha)?;
    fleet.store.collect_stale(Duration::from_secs(0))?;
    assert_eq!(
        fleet.collisions(&bravo)?.len(),
        1,
        "work still on disk still contests the path"
    );

    // Once the checkout is gone the contention cannot be real.
    git(
        &fleet.repository,
        &[
            "worktree",
            "remove",
            "--force",
            alpha_tree.to_str().context("worktree path")?,
        ],
    )?;
    let swept = fleet.store.collect_stale(Duration::from_secs(0))?;
    assert_eq!(swept["abandoned_footprints"], 1, "{swept}");
    let collisions = fleet.collisions(&bravo)?;
    assert_eq!(
        collisions.len(),
        0,
        "expected no contention, got {collisions:?}"
    );

    // Anything parked on `wait` has to learn the collision ended.
    let events = fleet.store.events(&bravo, 0, 500)?;
    let cleared = events
        .as_array()
        .context("missing events")?
        .iter()
        .filter(|event| event["event_type"] == "collision.cleared")
        .count();
    assert_eq!(cleared, 1, "the reaper announces what it cleared");
    Ok(())
}

/// Merge readiness for one agent, with the integration head resolved from git.
fn readiness(fleet: &Fleet, agent_id: &str, worktree: &Path) -> Result<Value> {
    let (_, head) = integration_head(worktree, Some("main"))?;
    fleet.store.merge_readiness(agent_id, &head)
}

fn blocker_codes(readiness: &Value) -> Vec<String> {
    readiness["blockers"]
        .as_array()
        .map(|blockers| {
            blockers
                .iter()
                .filter_map(|blocker| blocker["code"].as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_path_contested_by_a_live_peer_blocks_a_merge() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], false, "{report}");
    assert!(blocker_codes(&report).contains(&"contested_path".to_owned()));
    Ok(())
}

#[test]
fn duplicated_work_does_not_block_a_merge() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    // Byte-identical edits collide, but merging either one is safe.
    std::fs::write(alpha_tree.join("src/shared.rs"), "fn shared() { same() }\n")?;
    std::fs::write(bravo_tree.join("src/shared.rs"), "fn shared() { same() }\n")?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], true, "{report}");
    Ok(())
}

/// A stopped peer's preserved worktree is still a reported collision, but it cannot rebase and
/// must not deadlock the queue behind it.
#[test]
fn a_stopped_peer_does_not_block_a_merge() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    assert_eq!(readiness(&fleet, &alpha, &alpha_tree)?["mergeable"], false);

    fleet.store.stop_agent(&bravo)?;
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], true, "{report}");
    assert!(
        !fleet.collisions(&alpha)?.is_empty(),
        "the collision is still reported, it just no longer blocks"
    );
    Ok(())
}

#[test]
fn a_worktree_cut_before_the_branch_moved_cannot_merge_until_rebased() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha work"])?;
    fleet.sync(&alpha, &alpha_tree)?;
    assert_eq!(readiness(&fleet, &alpha, &alpha_tree)?["mergeable"], true);

    // Someone lands on main. Alpha's diff may still apply cleanly and be wrong.
    std::fs::write(fleet.repository.join("src/legacy.rs"), "landed\n")?;
    git(&fleet.repository, &["commit", "-am", "land upstream"])?;

    let stale = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(stale["mergeable"], false, "{stale}");
    assert!(blocker_codes(&stale).contains(&"stale_base".to_owned()));

    // Rebasing resolves it, which is the whole point of reporting it.
    git(&alpha_tree, &["rebase", "main"])?;
    fleet.sync(&alpha, &alpha_tree)?;
    let rebased = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(rebased["mergeable"], true, "{rebased}");
    Ok(())
}

#[test]
fn the_integration_lease_admits_one_merge_at_a_time() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    // Disjoint work: nothing but the lease can separate them.
    std::fs::write(alpha_tree.join("src/only-alpha.rs"), "a\n")?;
    std::fs::write(bravo_tree.join("src/only-bravo.rs"), "b\n")?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    assert_eq!(readiness(&fleet, &alpha, &alpha_tree)?["mergeable"], true);
    assert_eq!(readiness(&fleet, &bravo, &bravo_tree)?["mergeable"], true);

    let taken = fleet
        .store
        .claim(&bravo, pidmesh::store::INTEGRATION_TASK_KEY, 300, None)?;
    assert_eq!(taken["acquired"], true);

    let blocked = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(blocked["mergeable"], false, "{blocked}");
    assert!(blocker_codes(&blocked).contains(&"integration_held".to_owned()));

    let holder = readiness(&fleet, &bravo, &bravo_tree)?;
    assert_eq!(
        holder["mergeable"], true,
        "the holder is not blocked by itself"
    );
    assert_eq!(holder["holds_integration_lease"], true);

    fleet
        .store
        .release(&bravo, pidmesh::store::INTEGRATION_TASK_KEY)?;
    assert_eq!(readiness(&fleet, &alpha, &alpha_tree)?["mergeable"], true);
    Ok(())
}

/// The case that decides whether this signal is usable at fleet scale.
///
/// A shared router or module index is edited by every agent. Flagging it `divergent` every time
/// trains an operator to ignore collisions. Git merges edits to separable regions of one file
/// cleanly, so the mesh must say so.
#[test]
fn separate_regions_of_a_shared_file_do_not_conflict() -> Result<()> {
    let fleet = Fleet::new()?;
    let routes = (1..=60).fold(String::new(), |mut routes, line| {
        let _ = writeln!(routes, "route_{line}();");
        routes
    });
    std::fs::write(fleet.repository.join("src/routes.rs"), &routes)?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "routes"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Each agent registers its own route, far apart in the same file.
    let top: String = routes.replace("route_3();", "route_3_owned_by_alpha();");
    let bottom: String = routes.replace("route_55();", "route_55_owned_by_bravo();");
    std::fs::write(alpha_tree.join("src/routes.rs"), &top)?;
    std::fs::write(bravo_tree.join("src/routes.rs"), &bottom)?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let routes_collision = at(&collisions, "src/routes.rs").context("expected a collision")?;
    assert_eq!(
        routes_collision["severity"], "adjacent",
        "separable edits must not read as overwriting: {routes_collision}"
    );

    // And an adjacent collision must not block the merge either.
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], true, "{report}");
    Ok(())
}

#[test]
fn edits_to_the_same_region_of_a_shared_file_still_conflict() -> Result<()> {
    let fleet = Fleet::new()?;
    let routes = (1..=60).fold(String::new(), |mut routes, line| {
        let _ = writeln!(routes, "route_{line}();");
        routes
    });
    std::fs::write(fleet.repository.join("src/routes.rs"), &routes)?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "routes"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Both rewrite the same line.
    std::fs::write(
        alpha_tree.join("src/routes.rs"),
        routes.replace("route_30();", "route_30_alpha();"),
    )?;
    std::fs::write(
        bravo_tree.join("src/routes.rs"),
        routes.replace("route_30();", "route_30_bravo();"),
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let routes_collision = at(&collisions, "src/routes.rs").context("expected a collision")?;
    assert_eq!(routes_collision["severity"], "divergent");

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], false, "{report}");
    Ok(())
}

/// Severity is a property of a pair, not of a path.
///
/// Four agents can share one file while only two of them overlap. Blocking the other two because
/// the path as a whole is contested would serialize a fleet that has no actual conflict — which
/// is exactly what an end-to-end run of eight agents exposed.
#[test]
fn one_overlapping_pair_does_not_block_everyone_on_a_shared_file() -> Result<()> {
    let fleet = Fleet::new()?;
    let routes = (1..=80).fold(String::new(), |mut routes, line| {
        let _ = writeln!(routes, "route_{line}();");
        routes
    });
    std::fs::write(fleet.repository.join("src/routes.rs"), &routes)?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "routes"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    let (charlie, charlie_tree) = fleet.agent("charlie")?;
    let (delta, delta_tree) = fleet.agent("delta")?;

    // alpha and bravo edit far-apart regions; charlie and delta fight over one line.
    std::fs::write(
        alpha_tree.join("src/routes.rs"),
        routes.replace("route_5();", "route_5_alpha();"),
    )?;
    std::fs::write(
        bravo_tree.join("src/routes.rs"),
        routes.replace("route_70();", "route_70_bravo();"),
    )?;
    std::fs::write(
        charlie_tree.join("src/routes.rs"),
        routes.replace("route_40();", "route_40_charlie();"),
    )?;
    std::fs::write(
        delta_tree.join("src/routes.rs"),
        routes.replace("route_40();", "route_40_delta();"),
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    fleet.sync(&charlie, &charlie_tree)?;
    fleet.sync(&delta, &delta_tree)?;

    // The path as a whole is contested by four agents and is divergent overall.
    let collisions = fleet.collisions(&alpha)?;
    let routes_collision = at(&collisions, "src/routes.rs").context("expected a collision")?;
    assert_eq!(
        routes_collision["participants"].as_array().map(Vec::len),
        Some(4)
    );

    // But only the pair that actually overlaps is blocked.
    assert_eq!(
        readiness(&fleet, &alpha, &alpha_tree)?["mergeable"],
        true,
        "alpha overlaps nobody"
    );
    assert_eq!(
        readiness(&fleet, &bravo, &bravo_tree)?["mergeable"],
        true,
        "bravo overlaps nobody"
    );

    let blocked = readiness(&fleet, &charlie, &charlie_tree)?;
    assert_eq!(blocked["mergeable"], false, "{blocked}");
    let peers = blocked["blockers"][0]["peers"]
        .as_array()
        .context("peers")?;
    assert_eq!(
        peers.len(),
        1,
        "only the overlapping peer is named: {blocked}"
    );
    assert_eq!(peers[0]["agent_name"], "delta");
    assert_eq!(peers[0]["severity"], "divergent");
    Ok(())
}

/// The conflict path-level comparison structurally cannot see.
///
/// Two agents edit entirely different files. One withdraws an exported function; the other writes
/// new code that calls it. Git merges both without complaint and the result does not build.
#[test]
fn removing_an_export_another_checkout_calls_is_caught() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn compute_total() -> u32 {\n    7\n}\n",
    )?;
    std::fs::write(fleet.repository.join("src/caller.rs"), "fn unused() {}\n")?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // alpha retires the export, touching only src/api.rs.
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn compute_subtotal() -> u32 {\n    7\n}\n",
    )?;
    // bravo writes a new caller, touching only src/caller.rs.
    std::fs::write(
        bravo_tree.join("src/caller.rs"),
        "fn report() -> u32 {\n    compute_total()\n}\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    // No path is shared, so path-level detection sees nothing at all.
    assert!(
        fleet.collisions(&alpha)?.is_empty(),
        "the two checkouts share no path"
    );

    let report = fleet.store.collisions(&alpha)?;
    let breaks = report["symbol_breaks"].as_array().context("breaks")?;
    assert_eq!(breaks.len(), 1, "{report}");
    assert_eq!(breaks[0]["symbol"], "compute_total");
    assert_eq!(breaks[0]["removed_from"], "src/api.rs");
    assert_eq!(breaks[0]["used_by"]["agent_name"], "bravo");

    // And it must block alpha's merge, since alpha is the one breaking bravo.
    let alpha_readiness = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(alpha_readiness["mergeable"], false, "{alpha_readiness}");
    assert!(
        blocker_codes(&alpha_readiness).contains(&"removed_export_in_use".to_owned()),
        "{alpha_readiness}"
    );
    // bravo is not the one removing anything, so bravo is free to land.
    assert_eq!(readiness(&fleet, &bravo, &bravo_tree)?["mergeable"], true);
    Ok(())
}

#[test]
fn a_renamed_export_that_is_added_back_is_not_a_break() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn compute_total() -> u32 {\n    7\n}\n",
    )?;
    std::fs::write(fleet.repository.join("src/caller.rs"), "fn unused() {}\n")?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Reformatting the signature deletes and re-adds the same name; nothing was withdrawn.
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn compute_total(scale: u32) -> u32 {\n    7 * scale\n}\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/caller.rs"),
        "fn report() -> u32 {\n    compute_total()\n}\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let report = fleet.store.collisions(&alpha)?;
    assert_eq!(
        report["symbol_break_count"], 0,
        "the name still exists: {report}"
    );
    Ok(())
}

#[test]
fn a_withdrawn_export_nobody_uses_does_not_block() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn never_called() -> u32 {\n    7\n}\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn kept() -> u32 {\n    7\n}\n",
    )?;
    std::fs::write(bravo_tree.join("src/only-bravo.rs"), "fn other() {}\n")?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    assert_eq!(fleet.store.collisions(&alpha)?["symbol_break_count"], 0);
    assert_eq!(readiness(&fleet, &alpha, &alpha_tree)?["mergeable"], true);
    Ok(())
}

/// Regression: another checkout's copy of a declaration is not a usage of it.
///
/// References were every identifier in a changed file, so two agents who each edited one shared
/// module both "referenced" every name that module declares. At 24 agents that produced 8,336
/// phantom breaks and a 2.8s query — the false-alarm flood that teaches an operator to stop
/// reading the output.
#[test]
fn a_copy_of_a_declaration_is_not_a_reference_to_it() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn alpha_owned() {}\npub fn shared_helper() {}\npub fn bravo_owned() {}\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // alpha retires shared_helper. bravo edits the same file but only its own function, so its
    // copy still *declares* shared_helper without ever calling it.
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn alpha_owned() {}\npub fn bravo_owned() {}\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/api.rs"),
        "pub fn alpha_owned() {}\npub fn shared_helper() {}\npub fn bravo_owned_v2() {}\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    assert_eq!(
        fleet.store.collisions(&alpha)?["symbol_break_count"],
        0,
        "holding a copy of a declaration is not depending on it"
    );
    Ok(())
}

/// A sweep is one logical observation, so it publishes in one transaction.
///
/// Publishing agent by agent recomputes the workspace collision set twice per agent, which is
/// most of a tick once a fleet is large. Batching must not change what peers observe: the same
/// collisions, and the same transition events they would have been woken by.
#[test]
fn a_batched_sweep_reports_what_serial_publishing_would() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;

    let batch = vec![
        (alpha.clone(), scan_worktree(&alpha_tree, Some("main"))?),
        (bravo.clone(), scan_worktree(&bravo_tree, Some("main"))?),
    ];
    let report = fleet.store.publish_footprints(&batch)?;
    assert_eq!(report["published"], 2);
    assert_eq!(report["collision_count"], 1, "{report}");

    let collisions = fleet.collisions(&alpha)?;
    let shared = at(&collisions, "src/shared.rs").context("expected a collision")?;
    assert_eq!(shared["severity"], "divergent");

    // The overlap must have been announced exactly once, to a workspace-scoped listener.
    let detected = fleet.store.events(&bravo, 0, 500)?;
    let detected = detected
        .as_array()
        .context("events")?
        .iter()
        .filter(|event| event["event_type"] == "collision.detected")
        .count();
    assert_eq!(detected, 1, "one transition, one announcement");

    // Re-publishing unchanged state must stay silent, exactly as the serial path does.
    fleet.store.publish_footprints(&batch)?;
    let again = fleet.store.events(&bravo, 0, 500)?;
    let again = again
        .as_array()
        .context("events")?
        .iter()
        .filter(|event| event["event_type"] == "collision.detected")
        .count();
    assert_eq!(again, detected, "steady state must not re-announce");
    Ok(())
}

#[test]
fn an_empty_sweep_is_not_an_error() -> Result<()> {
    let fleet = Fleet::new()?;
    let report = fleet.store.publish_footprints(&[])?;
    assert_eq!(report["published"], 0);
    Ok(())
}

/// Regression: a repository with no commits is still a repository.
///
/// `git rev-parse` cannot resolve an unborn HEAD and exits non-zero even though it printed the
/// toplevel and git directory it did resolve. Treating that as "not a repository" dropped
/// linked-worktree discovery, which silently splits one repository into unrelated meshes — the
/// exact failure `pidmesh doctor` exists to catch.
#[test]
fn a_repository_with_no_commits_is_still_discovered() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repository = directory.path().join("unborn");
    std::fs::create_dir(&repository)?;
    git(&repository, &["init", "-b", "main"])?;

    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(&repository)
        .env_remove("PIDMESH_WORKSPACE")
        .args([
            "--db",
            directory
                .path()
                .join("mesh.db")
                .to_str()
                .context("database path")?,
            "--json",
            "doctor",
        ])
        .output()?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    let codes: Vec<&str> = report["warnings"]
        .as_array()
        .context("warnings")?
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect();
    assert!(
        !codes.contains(&"not_a_repository"),
        "an empty repository is still a repository: {report}"
    );
    assert!(
        report["primary_repository"].is_string(),
        "the repository root must resolve: {report}"
    );
    Ok(())
}

/// Regression: scanning must depend on the repository, not on who is holding it.
///
/// `diff.noprefix` and `diff.mnemonicPrefix` change or drop the `a/` and `b/` markers the diff
/// parser keys on, and `core.quotePath` C-quotes any non-ASCII path. All three are ordinary
/// `~/.gitconfig` settings, and under any of them the scan read an empty diff and reported no
/// conflicts — a silent wrong answer with a zero exit code.
#[test]
fn user_git_configuration_cannot_switch_detection_off() -> Result<()> {
    for (key, value) in [
        ("diff.noprefix", "true"),
        ("diff.mnemonicPrefix", "true"),
        ("core.quotePath", "true"),
    ] {
        let fleet = Fleet::new()?;
        git(&fleet.repository, &["config", key, value])?;
        std::fs::write(
            fleet.repository.join("src/api.rs"),
            "pub fn compute_total() -> u32 {\n    7\n}\n",
        )?;
        git(&fleet.repository, &["add", "-A"])?;
        git(&fleet.repository, &["commit", "-m", "api"])?;

        let (alpha, alpha_tree) = fleet.agent("alpha")?;
        let (bravo, bravo_tree) = fleet.agent("bravo")?;
        git(&alpha_tree, &["config", key, value])?;
        git(&bravo_tree, &["config", key, value])?;

        std::fs::write(
            alpha_tree.join("src/api.rs"),
            "pub fn compute_subtotal() -> u32 {\n    7\n}\n",
        )?;
        std::fs::write(
            bravo_tree.join("src/shared.rs"),
            "fn report() -> u32 {\n    compute_total()\n}\n",
        )?;
        fleet.sync(&alpha, &alpha_tree)?;
        fleet.sync(&bravo, &bravo_tree)?;

        let report = readiness(&fleet, &alpha, &alpha_tree)?;
        assert_eq!(
            report["mergeable"], false,
            "{key}={value} must not hide the break: {report}"
        );
    }
    Ok(())
}

/// Regression: a non-ASCII or space-containing filename must not hide a break.
#[test]
fn unusual_filenames_are_still_scanned() -> Result<()> {
    for name in ["café.rs", "my api.rs"] {
        let fleet = Fleet::new()?;
        std::fs::write(
            fleet.repository.join(format!("src/{name}")),
            "pub fn compute_total() -> u32 {\n    7\n}\n",
        )?;
        git(&fleet.repository, &["add", "-A"])?;
        git(&fleet.repository, &["commit", "-m", "api"])?;

        let (alpha, alpha_tree) = fleet.agent("alpha")?;
        let (bravo, bravo_tree) = fleet.agent("bravo")?;
        std::fs::write(
            alpha_tree.join(format!("src/{name}")),
            "pub fn compute_subtotal() -> u32 {\n    7\n}\n",
        )?;
        std::fs::write(
            bravo_tree.join("src/shared.rs"),
            "fn report() -> u32 {\n    compute_total()\n}\n",
        )?;
        fleet.sync(&alpha, &alpha_tree)?;
        fleet.sync(&bravo, &bravo_tree)?;

        let report = readiness(&fleet, &alpha, &alpha_tree)?;
        assert_eq!(
            report["mergeable"], false,
            "{name} must not hide the break: {report}"
        );
    }
    Ok(())
}

/// A watcher re-scans forever, so an unchanged sweep must cost nothing.
///
/// `footprint.published` fired every pass per agent whether or not anything moved, which is a row
/// per agent per tick in an append-only log: measured at roughly three gigabytes a month for a
/// twenty-agent machine that was merely left running.
#[test]
fn an_unchanged_sweep_writes_no_events() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    let batch = vec![
        (alpha.clone(), scan_worktree(&alpha_tree, Some("main"))?),
        (bravo.clone(), scan_worktree(&bravo_tree, Some("main"))?),
    ];
    fleet.store.publish_footprints(&batch)?;

    let count = |fleet: &Fleet| -> Result<usize> {
        Ok(fleet
            .store
            .events(&alpha, 0, 5000)?
            .as_array()
            .context("events")?
            .len())
    };
    let settled = count(&fleet)?;
    for _ in 0..10 {
        let report = fleet.store.publish_footprints(&batch)?;
        assert_eq!(report["published"], 0, "nothing moved: {report}");
    }
    assert_eq!(count(&fleet)?, settled, "an idle sweep must be silent");

    // A real change still announces itself.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a2 */ }\n",
    )?;
    let moved = vec![
        (alpha.clone(), scan_worktree(&alpha_tree, Some("main"))?),
        (bravo.clone(), scan_worktree(&bravo_tree, Some("main"))?),
    ];
    let report = fleet.store.publish_footprints(&moved)?;
    assert_eq!(
        report["published"], 1,
        "only the agent that moved: {report}"
    );
    assert!(count(&fleet)? > settled, "a real change is still announced");
    Ok(())
}

/// What the local dashboard has to be able to say without the operator opening a terminal.
///
/// A watcher holds no footprint of its own, so the snapshot must report the whole workspace,
/// rank it by severity, and name both sides of a symbol break rather than an opaque session id.
#[test]
fn the_dashboard_snapshot_carries_workspace_convergence() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn compute_total() -> u32 {\n    7\n}\n",
    )?;
    std::fs::write(fleet.repository.join("src/caller.rs"), "fn unused() {}\n")?;
    std::fs::write(
        fleet.repository.join("src/duplicate.rs"),
        "fn duplicate() {}\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // A real conflict on one path, duplicated effort on another, and a merge that is textually
    // clean and still broken.
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn shared() { /* b */ }\n",
    )?;
    std::fs::write(
        alpha_tree.join("src/duplicate.rs"),
        "fn duplicate() { same() }\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/duplicate.rs"),
        "fn duplicate() { same() }\n",
    )?;
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn compute_subtotal() -> u32 {\n    7\n}\n",
    )?;
    std::fs::write(
        bravo_tree.join("src/caller.rs"),
        "fn report() -> u32 {\n    compute_total()\n}\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let snapshot = fleet.store.dashboard_snapshot(&fleet.repository, 100)?;
    let convergence = &snapshot["convergence"];
    assert_eq!(snapshot["stats"]["contested_paths"], 2, "{convergence}");
    assert_eq!(snapshot["stats"]["symbol_breaks"], 1, "{convergence}");
    assert_eq!(snapshot["stats"]["blocked_agents"], 2, "{convergence}");
    assert_eq!(convergence["severity"]["divergent"], 1);
    assert_eq!(convergence["severity"]["identical"], 1);

    // A truncating reader must not lose the conflict behind the duplicated work.
    assert_eq!(convergence["collisions"][0]["path"], "src/shared.rs");
    assert_eq!(convergence["collisions"][0]["severity"], "divergent");
    assert_eq!(convergence["collisions"][1]["severity"], "identical");

    let breaks = convergence["symbol_breaks"].as_array().context("breaks")?;
    assert_eq!(breaks[0]["symbol"], "compute_total");
    assert_eq!(breaks[0]["removed_by"]["agent_name"], "alpha");
    assert_eq!(breaks[0]["used_by"]["agent_name"], "bravo");

    // Byte-identical work is never a blocker, and alpha carries both of the ones it does have.
    let blocked = convergence["blocked"].as_array().context("blocked")?;
    let entry = blocked
        .iter()
        .find(|entry| entry["agent_id"] == alpha.as_str())
        .context("alpha is blocked")?;
    let codes: Vec<&str> = entry["blockers"]
        .as_array()
        .context("blockers")?
        .iter()
        .filter_map(|blocker| blocker["code"].as_str())
        .collect();
    assert_eq!(
        codes,
        ["contested_path", "removed_export_in_use"],
        "{entry}"
    );
    assert_eq!(convergence["integration_lease"], Value::Null);

    // Whoever is mid-merge is reported once rather than against every agent waiting on them.
    fleet
        .store
        .claim(&bravo, pidmesh::store::INTEGRATION_TASK_KEY, 300, None)?;
    let held = fleet.store.dashboard_snapshot(&fleet.repository, 100)?;
    assert_eq!(
        held["convergence"]["integration_lease"]["agent_name"],
        "bravo"
    );
    Ok(())
}

/// Regression: prose must not manufacture blockers.
///
/// `class`, `type`, `def` and `func` are ordinary English words, and export detection matched
/// them in any file. Trimming a changelog whose lines began with them, against a peer whose only
/// change was a comment mentioning them, produced real blockers over work that was fine. A
/// blocker that fires on prose teaches people to ignore blockers.
#[test]
fn prose_does_not_manufacture_blockers() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("CHANGELOG.md"),
        "class Registry was added\ntype Widget was renamed\ndef process was removed\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "notes"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    // alpha trims the changelog; bravo only writes a comment naming those words.
    std::fs::write(alpha_tree.join("CHANGELOG.md"), "nothing to report\n")?;
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "// Registry, Widget and process are described in the changelog.\nfn shared() {}\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(
        report["mergeable"], true,
        "a changelog edit is not an API change: {report}"
    );
    Ok(())
}

/// Regression: an unobserved checkout is not a clear one.
///
/// With no footprint there is no base commit, so no staleness could be detected and `mergeable`
/// returned true with exit code 0 — a script doing `mergeable && git merge` would merge blind.
#[test]
fn a_checkout_that_was_never_synced_cannot_merge() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    std::fs::write(
        alpha_tree.join("src/shared.rs"),
        "fn shared() { /* a */ }\n",
    )?;
    // Deliberately no sync.
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], false, "{report}");
    assert!(blocker_codes(&report).contains(&"never_synced".to_owned()));

    // Observing it clears the blocker.
    fleet.sync(&alpha, &alpha_tree)?;
    let observed = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(observed["mergeable"], true, "{observed}");
    Ok(())
}

/// The event ledger is the last thing on a long-lived machine that grew without bound.
///
/// Consumers checkpoint on a sequence and long-poll, so they only ever read the recent tail.
/// Keeping an unbounded history serves nobody, and nothing else prunes it.
#[test]
fn garbage_collection_bounds_the_event_ledger() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;

    // Drive real changes so the ledger genuinely grows; idle sweeps are deliberately silent.
    for round in 0..60 {
        std::fs::write(
            alpha_tree.join("src/shared.rs"),
            format!("fn shared() {{ /* {round} */ }}\n"),
        )?;
        fleet.sync(&alpha, &alpha_tree)?;
    }
    let before = fleet.store.events(&alpha, 0, 100_000)?;
    let before = before.as_array().context("events")?.len();
    assert!(before > 0, "the ledger must actually have grown");

    let swept = fleet.store.collect_stale(Duration::from_secs(3600))?;
    assert!(swept["trimmed_events"].is_number(), "{swept}");

    // Under the retention ceiling nothing is discarded: pruning must never cost a live consumer
    // the tail it is about to read.
    let after = fleet.store.events(&alpha, 0, 100_000)?;
    assert_eq!(
        after.as_array().context("events")?.len(),
        before,
        "a small ledger is left alone: {swept}"
    );

    // Past the ceiling it prunes, keeps the newest, and does so without re-ranking the ledger
    // per row — the first version of this was quadratic and took minutes.
    fleet.store.seed_events_for_test(&alpha, 25_000)?;
    let started = std::time::Instant::now();
    let swept = fleet.store.collect_stale(Duration::from_secs(3600))?;
    let elapsed = started.elapsed();
    assert!(
        swept["trimmed_events"].as_u64().unwrap_or(0) > 0,
        "an over-sized ledger is trimmed: {swept}"
    );
    let kept = fleet.store.events(&alpha, 0, 100_000)?;
    let kept = kept.as_array().context("events")?.len();
    assert!(kept <= 20_000, "the ceiling is respected, kept {kept}");
    assert!(
        elapsed < Duration::from_secs(20),
        "pruning must not be quadratic, took {elapsed:?}"
    );
    Ok(())
}

/// Regression: a recycled pid must not keep a dead session alive forever.
///
/// Liveness was confirmed by pid alone, and the operating system reuses them. An unrelated
/// process inheriting a dead agent's pid kept that agent `running` indefinitely, holding its
/// claims and blocking every peer that shared a path with it.
#[test]
fn a_long_silent_session_dies_even_if_its_pid_was_reused() -> Result<()> {
    let fleet = Fleet::new()?;
    // pid 1 always exists, which is exactly the situation a recycled pid creates.
    let registration =
        fleet
            .store
            .register_agent("ghost", 1, Some(&fleet.repository), "test", &[], None)?;
    let ghost = registration["agent_id"]
        .as_str()
        .context("agent id")?
        .to_owned();

    // A recent heartbeat with a live pid is left alone.
    let fresh = fleet.store.collect_stale(Duration::from_secs(0))?;
    assert_eq!(fresh["dead_agents"], 0, "a live session is not reaped");

    // Age the heartbeat past the abandonment ceiling.
    fleet.store.backdate_heartbeat_for_test(&ghost, 7_200_000)?;
    let reaped = fleet.store.collect_stale(Duration::from_secs(0))?;
    assert_eq!(reaped["dead_agents"], 1, "{reaped}");
    Ok(())
}

/// Regression: a local variable sharing a name with a withdrawn export is not a dependency.
///
/// Reference matching is textual, so any file holding the word counted as using it. Two unrelated
/// pieces of code that happened to pick the same name blocked each other's merge.
#[test]
fn a_local_binding_is_not_a_dependency_on_someone_elses_export() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn compute_total() -> u32 {\n    7\n}\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn compute_subtotal() -> u32 {\n    7\n}\n",
    )?;
    // bravo binds its own `compute_total`; it never calls anyone else's.
    std::fs::write(
        bravo_tree.join("src/shared.rs"),
        "fn local() -> u32 {\n    let compute_total = 3;\n    compute_total\n}\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(
        report["mergeable"], true,
        "two unrelated uses of one name are not a conflict: {report}"
    );
    Ok(())
}

/// Regression: a symlink must not walk around the file-size ceiling.
///
/// `symlink_metadata` describes the link, whose length is the target *string*, so a size test
/// alone let any symlink through and the read then followed it. A benchmark reached 534 MiB of
/// resident memory through a linked generated file — paid on every tick, for every checkout
/// scanned in parallel. `digest_of` tests `is_file()` first; the two readers of the same path
/// must not disagree about what they are willing to open.
#[test]
fn a_symlink_cannot_bypass_the_scan_ceiling() -> Result<()> {
    let fleet = Fleet::new()?;
    let (_alpha, alpha_tree) = fleet.agent("alpha")?;

    // Comfortably past MAX_SCANNED_BYTES, reachable only through a link.
    let target = alpha_tree.join("..").join("generated.rs");
    std::fs::write(&target, "pub fn generated() {}\n".repeat(200_000))?;
    assert!(
        std::fs::metadata(&target)?.len() > 2 * 1024 * 1024,
        "the target must exceed the ceiling for this to prove anything"
    );
    std::os::unix::fs::symlink(&target, alpha_tree.join("src/linked.rs"))?;

    let scan = scan_worktree(&alpha_tree, Some("main"))?;
    let linked = scan
        .entries
        .iter()
        .find(|entry| entry.path == "src/linked.rs")
        .context("the link is still reported as a change")?;
    assert!(
        linked.digest.is_none(),
        "a symlink has no comparable content of its own"
    );
    assert!(
        scan.symbols.truncated,
        "skipping it must mark the scan incomplete rather than pass silently"
    );
    // The link's target declares `generated`; reading through it would have harvested that name.
    assert!(
        !scan
            .symbols
            .references
            .iter()
            .any(|name| name == "generated"),
        "the target's contents must never have been read"
    );
    Ok(())
}

/// A git that will not start must say so, not blame the repository.
///
/// The base-branch probe used to swallow the difference, so pointing `PIDMESH_GIT` at a missing
/// binary reported "no default integration branch found" — sending someone to look at their
/// branches over a path problem.
#[test]
fn a_git_that_cannot_run_is_reported_as_such() -> Result<()> {
    let fleet = Fleet::new()?;
    let (_alpha, alpha_tree) = fleet.agent("alpha")?;
    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(&alpha_tree)
        .env("PIDMESH_GIT", "/nonexistent/git")
        .env_remove("PIDMESH_WORKSPACE")
        .args([
            "--db",
            fleet
                .repository
                .join("..")
                .join("probe.db")
                .to_str()
                .context("database path")?,
            "--json",
            "sync",
            "--agent",
            "nobody",
        ])
        .output()?;
    let message = String::from_utf8_lossy(&output.stderr);
    // The wording may be "could not run git at X" or the fuller "git is not installed, or not
    // on PATH"; what matters is that git is named as the subject and the executable that was
    // tried is quoted, so the reader can see it is their override that is wrong.
    assert!(
        (message.contains("git") && message.contains("/nonexistent/git"))
            || message.contains("agent id"),
        "a missing git must name itself: {message}"
    );
    assert!(
        !message.contains("no default integration branch"),
        "a path problem must not read as a repository problem: {message}"
    );
    Ok(())
}

/// Trimming rows frees pages; it does not shrink the file.
///
/// A long-lived mesh was therefore a high-water mark: trimming 100,000 events left a 72 MB
/// database with 83% of its pages free. Reclaiming rewrites the database, so it belongs to `gc`
/// rather than to a watcher's tick.
#[test]
fn garbage_collection_returns_space_to_the_filesystem() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, _alpha_tree) = fleet.agent("alpha")?;

    // Comfortably past the retention ceiling, so trimming leaves a large free list.
    fleet.store.seed_events_for_test(&alpha, 60_000)?;
    fleet.store.collect_stale(Duration::from_secs(3600))?;

    let before = fleet.store.compact()?;
    assert_eq!(before["compacted"], true, "{before}");
    let pages_before = before["pages_before"].as_i64().context("pages_before")?;
    let pages_after = before["pages_after"].as_i64().context("pages_after")?;
    assert!(
        pages_after < pages_before,
        "the file must actually shrink: {before}"
    );

    // Running it again must decline rather than rewrite the database for nothing.
    let again = fleet.store.compact()?;
    assert_eq!(again["compacted"], false, "{again}");

    // And the mesh must still be intact and answerable afterwards.
    let events = fleet.store.events(&alpha, 0, 10)?;
    assert!(events.is_array(), "the ledger still reads after a rewrite");
    Ok(())
}

/// The verdict has to agree with what `git merge` actually does.
///
/// Severity was derived from whether rewritten regions overlap, which is not the question a
/// three-way merge asks. It needs unchanged material *between* two changed regions to keep them
/// apart: measured against git, one line is enough and zero is not. Edits to consecutive lines
/// were therefore reported `adjacent` — safe to merge — and then conflicted. A differential run
/// against real merges caught three such cases in forty.
#[test]
fn consecutive_edits_are_a_conflict_because_git_says_so() -> Result<()> {
    let fleet = Fleet::new()?;
    let body = (0..60).fold(String::new(), |mut rows, line| {
        let _ = writeln!(rows, "pub fn line_{line}() {{}}");
        rows
    });
    std::fs::write(fleet.repository.join("src/wide.rs"), &body)?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "wide"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Consecutive lines: no unchanged line separates them.
    std::fs::write(
        alpha_tree.join("src/wide.rs"),
        body.replace("pub fn line_20() {}", "pub fn line_20_alpha() {}"),
    )?;
    std::fs::write(
        bravo_tree.join("src/wide.rs"),
        body.replace("pub fn line_21() {}", "pub fn line_21_bravo() {}"),
    )?;
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let collisions = fleet.collisions(&alpha)?;
    let wide = at(&collisions, "src/wide.rs").context("expected a collision")?;
    assert_eq!(
        wide["severity"], "divergent",
        "consecutive edits conflict in a real merge: {wide}"
    );

    // One unchanged line apart is genuinely separable, and must not be over-reported.
    std::fs::write(
        bravo_tree.join("src/wide.rs"),
        body.replace("pub fn line_22() {}", "pub fn line_22_bravo() {}"),
    )?;
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    let wide = at(&collisions, "src/wide.rs").context("expected a collision")?;
    assert_eq!(
        wide["severity"], "adjacent",
        "a one-line gap merges cleanly, so it must not block: {wide}"
    );
    Ok(())
}

/// What a real three-way merge does with two checkouts' work.
///
/// Every severity claim below is checked against this rather than against our own reasoning about
/// what git ought to do. The merge is unwound afterwards so the checkout stays usable, and the
/// callers assert both outcomes of this oracle at least once, so an oracle that had stopped
/// working — a git that will not run, a branch name that does not resolve — cannot quietly report
/// "conflict" for everything and manufacture findings.
fn merge_conflicts(checkout: &Path, other_branch: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["merge", "--no-commit", "--no-ff", other_branch])
        .output()?;
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // "Already up to date" and an unknown ref both exit zero or non-zero for reasons that have
    // nothing to do with conflicts, and either would make the oracle lie.
    anyhow::ensure!(
        !combined.contains("Already up to date")
            && !combined.contains("not something we can merge"),
        "the merge oracle did not actually merge anything: {combined}"
    );
    let conflicted = !output.status.success();
    let _ = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["merge", "--abort"])
        .output();
    let _ = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["reset", "--hard", "HEAD"])
        .output();
    Ok(conflicted)
}

/// A line of a file is not a header, however much it looks like one.
///
/// `git diff --unified=0` prefixes every body line with one marker byte, so a deleted `-- note`
/// reaches the parser as `--- note` — byte for byte the shape of a `--- a/path` file header. SQL,
/// Lua, Haskell and Ada comments all start that way, and so does every line of a committed
/// `.patch` fixture. Reading one as a header detached the parser from the file and silently
/// dropped every hunk after it, and a file whose overlapping region went unrecorded compares as
/// having no overlap at all: `adjacent`, reported as safe to merge, against a merge that
/// conflicts. The fix counts each hunk's body lines from its own header instead of guessing.
#[test]
fn a_deleted_line_shaped_like_a_diff_header_cannot_hide_a_hunk() -> Result<()> {
    let fleet = Fleet::new()?;
    let body = (1..=40).fold(String::new(), |mut rows, line| {
        let _ = match line {
            5 => writeln!(rows, "-- the schema this file builds"),
            _ => writeln!(rows, "select col_{line};"),
        };
        rows
    });
    std::fs::write(fleet.repository.join("schema.sql"), &body)?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "schema"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Alpha rewrites the comment *and* line 30. The comment's hunk comes first in the diff, so
    // a parser that mistakes it for a header never sees the hunk that actually matters.
    std::fs::write(
        alpha_tree.join("schema.sql"),
        body.replace("-- the schema this file builds", "-- the schema, rewritten")
            .replace("select col_30;", "select col_30_alpha;"),
    )?;
    // Bravo rewrites only line 30, so the two genuinely collide there.
    std::fs::write(
        bravo_tree.join("schema.sql"),
        body.replace("select col_30;", "select col_30_bravo;"),
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    // The oracle first: this is what the tool has to agree with.
    assert!(
        merge_conflicts(&alpha_tree, "feat/bravo")?,
        "both checkouts rewrote line 30, so a real merge must conflict"
    );

    let collisions = fleet.collisions(&alpha)?;
    let schema = at(&collisions, "schema.sql").context("expected a collision")?;
    assert_eq!(
        schema["severity"], "divergent",
        "a comment that looks like a diff header must not hide the hunk below it: {schema}"
    );
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], false, "{report}");

    // The same oracle must report the other outcome too, or it is proving nothing. Bravo moves
    // its edit far away; now the two really are separable and git merges them.
    std::fs::write(
        bravo_tree.join("schema.sql"),
        body.replace("select col_38;", "select col_38_bravo;"),
    )?;
    git(&bravo_tree, &["commit", "-am", "bravo moves"])?;
    assert!(
        !merge_conflicts(&alpha_tree, "feat/bravo")?,
        "well-separated edits merge cleanly, so the oracle must say so"
    );
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    let schema = at(&collisions, "schema.sql").context("expected a collision")?;
    assert_eq!(
        schema["severity"], "adjacent",
        "and a separable edit must not be over-reported either: {schema}"
    );
    Ok(())
}

/// Rewritten regions have to be measured in the bytes git merges, not in a rendering of them.
///
/// A `diff=<driver>` attribute with a `textconv` makes `git diff` describe a converted document,
/// and `--no-ext-diff` does not turn that off. Pretty-printing a one-line config is the common
/// case — people configure it for minified JSON and for notebooks — and it reports two rewrites
/// of that single physical line as hunks several lines apart. Several lines apart is `adjacent`,
/// which does not block a merge; the merge then conflicts every time, because in the file git
/// actually merges both sides rewrote line one.
#[test]
fn a_textconv_driver_cannot_move_a_conflict_out_of_the_way() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("config.json"),
        "{\"alpha\":1,\"beta\":2,\"gamma\":3,\"delta\":4,\"epsilon\":5,\"zeta\":6}\n",
    )?;
    std::fs::write(
        fleet.repository.join(".gitattributes"),
        "*.json diff=json\n",
    )?;
    git(
        &fleet.repository,
        &["config", "diff.json.textconv", "python3 -m json.tool"],
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "config"])?;

    // Confirm the driver is actually in effect, or this test proves nothing about textconv.
    let rendered = Command::new("git")
        .arg("-C")
        .arg(&fleet.repository)
        .args(["show", "HEAD:config.json", "--textconv"])
        .output()?;
    anyhow::ensure!(
        String::from_utf8_lossy(&rendered.stdout).lines().count() > 1,
        "the textconv driver is not configured, so this test would pass for the wrong reason"
    );

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Two different keys, both on the file's single physical line.
    std::fs::write(
        alpha_tree.join("config.json"),
        "{\"alpha\":99,\"beta\":2,\"gamma\":3,\"delta\":4,\"epsilon\":5,\"zeta\":6}\n",
    )?;
    std::fs::write(
        bravo_tree.join("config.json"),
        "{\"alpha\":1,\"beta\":2,\"gamma\":3,\"delta\":4,\"epsilon\":5,\"zeta\":99}\n",
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    assert!(
        merge_conflicts(&alpha_tree, "feat/bravo")?,
        "both rewrote the file's only line, so a real merge must conflict"
    );
    let collisions = fleet.collisions(&alpha)?;
    let config = at(&collisions, "config.json").context("expected a collision")?;
    assert_eq!(
        config["severity"], "divergent",
        "regions must be measured in the merged bytes, not in a pretty-printed rendering: {config}"
    );
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], false, "{report}");
    Ok(())
}

/// The same header confusion in the other direction: an added line can read as `+++ ...`.
///
/// `(Some(path), None)` in the header parser means "a base side with no working side", which is a
/// deletion. An added line whose text begins `++ ` produced exactly that pair for a file sitting
/// right there on disk, so a live file was recorded as deleted — which reads as `delete_edit`
/// against any peer editing it, and strips the digest that would otherwise have proved the two
/// checkouts identical.
#[test]
fn an_added_line_shaped_like_a_diff_header_does_not_report_a_live_file_deleted() -> Result<()> {
    let fleet = Fleet::new()?;
    let (alpha, alpha_tree) = fleet.agent("alpha")?;

    // A changelog entry, a diff quoted in documentation, a `.patch` fixture: all produce lines
    // that begin with two plus signs and a space.
    std::fs::write(
        alpha_tree.join("src/legacy.rs"),
        "legacy\n++ added by the migration script\n",
    )?;
    fleet.sync(&alpha, &alpha_tree)?;

    let scan = scan_worktree(&alpha_tree, Some("main"))?;
    let legacy = scan
        .entries
        .iter()
        .find(|entry| entry.path == "src/legacy.rs")
        .context("the file is still part of the footprint")?;
    assert_eq!(
        legacy.change_kind.as_str(),
        "modified",
        "the file is on disk, so it was modified rather than deleted"
    );
    assert!(
        legacy.digest.is_some(),
        "a file that exists has content to compare, and without a digest two identical \
         checkouts cannot be told apart from two conflicting ones"
    );
    Ok(())
}

/// A merge gate must not run out of room to mention the break it is gating on.
///
/// Symbol breaks were read through one workspace-wide query capped at 200 rows, and the merge
/// gate then filtered those rows down to the asking agent's own. A peer withdrawing a few hundred
/// names of its own — deleting a generated module is enough — fills the window before anyone
/// else's break is reached. The agent that really is breaking a live peer is then handed
/// `mergeable: true` and an empty blocker list: not a truncated answer that says it stopped
/// looking, a clean one. `mergeable && git merge` merges it.
#[test]
fn a_flood_of_peer_symbol_breaks_cannot_clear_an_agents_own() -> Result<()> {
    let fleet = Fleet::new()?;

    // One file full of exports a peer will withdraw wholesale, and one lone export beside it.
    let bulk = (1..=250).fold(String::new(), |mut rows, index| {
        let _ = writeln!(rows, "pub fn bulk_{index:03}() {{}}");
        rows
    });
    std::fs::write(fleet.repository.join("src/bulk.rs"), &bulk)?;
    std::fs::write(
        fleet.repository.join("src/target.rs"),
        "pub fn zzz_lone_export() {}\n",
    )?;
    // A consumer that calls all of them, so withdrawing any one is a real break.
    let mut consumer = String::from("pub fn consume() {\n");
    for index in 1..=250 {
        let _ = writeln!(consumer, "    bulk_{index:03}();");
    }
    consumer.push_str("    zzz_lone_export();\n}\n");
    std::fs::write(fleet.repository.join("src/consumer.rs"), &consumer)?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    let (charlie, charlie_tree) = fleet.agent("charlie")?;

    // Alpha withdraws exactly one export. Its name sorts last, which is the whole point.
    std::fs::write(alpha_tree.join("src/target.rs"), "pub fn kept() {}\n")?;
    // Bravo withdraws two hundred and fifty, in a different file, contesting nothing.
    std::fs::write(bravo_tree.join("src/bulk.rs"), "pub fn nothing() {}\n")?;
    // Charlie still calls every one of them.
    std::fs::write(
        charlie_tree.join("src/consumer.rs"),
        format!("{consumer}// charlie is still working here\n"),
    )?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    fleet.sync(&charlie, &charlie_tree)?;

    // The premise: there really are more breaks in this workspace than the report will carry.
    let convergence = fleet.store.collisions(&alpha)?;
    assert!(
        convergence["symbol_break_count"].as_i64().unwrap_or(0) >= 200,
        "this test only means something if the cap is actually reached: {convergence}"
    );
    // And the report has to admit it stopped early. A capped list that reads as a complete one
    // is the same lie in a quieter voice: two hundred breaks and twenty thousand printed
    // identically.
    assert_eq!(
        convergence["symbol_breaks_truncated"], true,
        "a capped report must say it was capped: {convergence}"
    );

    // Alpha is genuinely withdrawing a name a live peer still calls, and must be told so.
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert!(
        blocker_codes(&report).contains(&"removed_export_in_use".to_owned()),
        "alpha's own break must survive a peer's flood: {report}"
    );
    assert_eq!(report["mergeable"], false, "{report}");

    // And bravo, whose breaks did fill the window, is still blocked on its own account.
    let bravo_report = readiness(&fleet, &bravo, &bravo_tree)?;
    assert!(
        blocker_codes(&bravo_report).contains(&"removed_export_in_use".to_owned()),
        "{bravo_report}"
    );

    // Charlie withdraws nothing, so it must stay mergeable — the gate is still discriminating,
    // not just blocking everyone.
    let charlie_report = readiness(&fleet, &charlie, &charlie_tree)?;
    assert!(
        !blocker_codes(&charlie_report).contains(&"removed_export_in_use".to_owned()),
        "a checkout that withdraws nothing must not be blocked: {charlie_report}"
    );
    Ok(())
}

/// A sentence cannot put back an export that code took away.
///
/// Both halves of "was this name withdrawn?" run the same declaration matcher, but only the
/// removed half was restricted to code. The matcher recognises a bare `def `, `class `, `func `
/// and `type ` — ordinary English — which is exactly why the removed half is restricted. An
/// added line anywhere in the checkout that happened to parse as a declaration therefore
/// cancelled a real withdrawal, and quoting the old signature in a migration note is the normal
/// way to document a removal. The cancellation has to be as trustworthy as the thing it cancels.
#[test]
fn documenting_a_removal_does_not_cancel_it() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/parser.py"),
        "def parse_header(raw):\n    return raw\n",
    )?;
    std::fs::write(
        fleet.repository.join("src/report.py"),
        "def build(raw):\n    return parse_header(raw)\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "parser"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Alpha withdraws the function and writes the migration note that quotes its old signature.
    std::fs::write(
        alpha_tree.join("src/parser.py"),
        "def parse_headers(raw):\n    return [raw]\n",
    )?;
    std::fs::create_dir_all(alpha_tree.join("docs"))?;
    std::fs::write(
        alpha_tree.join("docs/migration.md"),
        "# Migration\n\nThe old entry point\n\ndef parse_header(raw)\n\nis gone; call \
         `parse_headers` instead.\n",
    )?;
    // Bravo is still calling it.
    std::fs::write(
        bravo_tree.join("src/report.py"),
        "def build(raw):\n    return parse_header(raw).strip()\n",
    )?;

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert!(
        blocker_codes(&report).contains(&"removed_export_in_use".to_owned()),
        "a prose line that parses as a declaration must not cancel a real withdrawal: {report}"
    );
    Ok(())
}

/// A change git describes as binary still has to reach the mesh.
///
/// `git diff` answers a change to such a path with a `diff --git` header and `Binary files ...
/// differ` — no `---`/`+++` pair naming it, no hunks. Once the work is committed `git status` is
/// silent about it as well, so the path landed in no footprint at all: two checkouts both
/// rewriting the same image, the same fixture, or the same lockfile carrying `-diff` produced no
/// collision, no blocker and `mergeable: true`, while `git merge` conflicts on it every time.
/// That is the whole product claim failing quietly, so the scan now recovers those paths by name.
#[test]
fn a_committed_binary_change_is_still_a_collision() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("logo.png"),
        b"\x89PNG\r\n\x1a\n\x00base pixels\x00",
    )?;
    // The other common shape of the same problem: ordinary text a repository marks `-diff`.
    std::fs::write(fleet.repository.join(".gitattributes"), "*.lock -diff\n")?;
    std::fs::write(fleet.repository.join("deps.lock"), "dep a = 1\ndep b = 2\n")?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "assets"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    std::fs::write(
        alpha_tree.join("logo.png"),
        b"\x89PNG\r\n\x1a\n\x00alpha pixels\x00",
    )?;
    std::fs::write(alpha_tree.join("deps.lock"), "dep a = 9\ndep b = 2\n")?;
    std::fs::write(
        bravo_tree.join("logo.png"),
        b"\x89PNG\r\n\x1a\n\x00bravo pixels\x00",
    )?;
    std::fs::write(bravo_tree.join("deps.lock"), "dep a = 1\ndep b = 9\n")?;
    // Committing is what hides the change: it leaves the diff, and `status` never saw it.
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    assert!(
        merge_conflicts(&alpha_tree, "feat/bravo")?,
        "git cannot merge two rewrites of a binary path, so the oracle must say conflict"
    );

    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    for path in ["logo.png", "deps.lock"] {
        let collision = at(&collisions, path)
            .with_context(|| format!("{path} must be reported as contested"))?;
        assert_eq!(
            collision["severity"], "divergent",
            "a path git will not merge line by line cannot be adjacent: {collision}"
        );
    }
    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert_eq!(report["mergeable"], false, "{report}");
    Ok(())
}

/// Being shown as text does not make a file mergeable as text.
///
/// A bare `diff` attribute tells git to render a binary path as lines. The merge driver is not
/// listening: it still takes the file whole and conflicts on any two-sided change. So hunk
/// coordinates exist for such a path and describe nothing a merge will respect — two edits a
/// dozen lines apart read as `adjacent`, and the merge conflicts every time. Regions are
/// withheld from content that carries a NUL, which is the same test git uses.
#[test]
fn a_binary_file_rendered_as_text_is_not_merged_as_text() -> Result<()> {
    let fleet = Fleet::new()?;
    let base: Vec<u8> = (1..=12).fold(Vec::new(), |mut bytes, row| {
        bytes.extend_from_slice(format!("\0row_{row:02}\n").as_bytes());
        bytes
    });
    std::fs::write(fleet.repository.join("frames.dat"), &base)?;
    std::fs::write(fleet.repository.join(".gitattributes"), "*.dat diff\n")?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "frames"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Far apart in the rendering: row 1 against row 12.
    let mut mine = base.clone();
    mine[..9].copy_from_slice(b"\0row_01a\n");
    std::fs::write(alpha_tree.join("frames.dat"), &mine)?;
    let mut theirs = base.clone();
    let tail = theirs.len() - 9;
    theirs[tail..].copy_from_slice(b"\0row_12b\n");
    std::fs::write(bravo_tree.join("frames.dat"), &theirs)?;
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    // The premise: git really is rendering this as text, or the test proves nothing.
    let shown = Command::new("git")
        .arg("-C")
        .arg(&alpha_tree)
        .args(["diff", "--unified=0", "main", "--", "frames.dat"])
        .output()?;
    anyhow::ensure!(
        String::from_utf8_lossy(&shown.stdout).contains("@@"),
        "the `diff` attribute is not in effect, so this would pass for the wrong reason"
    );

    assert!(
        merge_conflicts(&alpha_tree, "feat/bravo")?,
        "a binary merge is all or nothing, so the oracle must say conflict"
    );
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    let frames = at(&collisions, "frames.dat").context("expected a collision")?;
    assert_eq!(
        frames["severity"], "divergent",
        "hunk coordinates on a binary path describe nothing a merge respects: {frames}"
    );
    Ok(())
}

/// `.gitattributes` `-merge` takes the line merge away, so no gap is wide enough.
///
/// `-merge` — the normal idiom for a lockfile or a generated bundle — marks a path unmergeable
/// while leaving it perfectly diffable. git produces ordinary line hunks for it and still refuses
/// to merge it a line at a time: any two-sided change conflicts, however far apart the edits are.
/// Two agents editing opposite ends of such a file used to be reported `adjacent` and told the
/// merge was clean, and it was not. The oracle below is the point: the merge is run for real
/// first, so the severity is checked against git rather than against our reading of git.
#[test]
fn a_path_marked_unmergeable_is_not_adjacent() -> Result<()> {
    let fleet = Fleet::new()?;
    let body = (1..=20).fold(String::new(), |mut rows, row| {
        let _ = writeln!(rows, "dep_{row} = \"1.0\"");
        rows
    });
    std::fs::write(fleet.repository.join("deps.lock"), &body)?;
    std::fs::write(fleet.repository.join(".gitattributes"), "*.lock -merge\n")?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "deps"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("deps.lock"),
        body.replace("dep_2 = \"1.0\"", "dep_2 = \"2.0\""),
    )?;
    std::fs::write(
        bravo_tree.join("deps.lock"),
        body.replace("dep_18 = \"1.0\"", "dep_18 = \"2.0\""),
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    assert!(
        merge_conflicts(&alpha_tree, "feat/bravo")?,
        "`-merge` makes any two-sided change a conflict"
    );
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    let deps = at(&collisions, "deps.lock").context("expected a collision")?;
    assert_eq!(
        deps["severity"], "divergent",
        "a path the repository declared unmergeable cannot be adjacent: {deps}"
    );
    Ok(())
}

/// A `merge=<driver>` attribute is no safer than `-merge`, and for the same reason.
///
/// A named driver hands the merge to a program out of `merge.<driver>.driver`. It may refuse
/// every two-sided change the way `binary` does, it may concatenate both sides the way `union`
/// does, or it may be anything at all — and whatever it does, it does not combine the two sides
/// along the hunk coordinates a textual diff produced, so those coordinates stop describing the
/// merge. The driver here is `false`, which conflicts on every invocation, so the real merge
/// below proves the severity rather than our reading of what drivers do.
#[test]
fn a_path_with_a_custom_merge_driver_is_not_adjacent() -> Result<()> {
    let fleet = Fleet::new()?;
    let body = (1..=20).fold(String::new(), |mut rows, row| {
        let _ = writeln!(rows, "dep_{row} = \"1.0\"");
        rows
    });
    std::fs::write(fleet.repository.join("deps.lock"), &body)?;
    std::fs::write(
        fleet.repository.join(".gitattributes"),
        "*.lock merge=lockfile\n",
    )?;
    // Repository config is shared with every linked worktree, so one setting covers the fleet.
    git(
        &fleet.repository,
        &["config", "merge.lockfile.driver", "false"],
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "deps"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("deps.lock"),
        body.replace("dep_2 = \"1.0\"", "dep_2 = \"2.0\""),
    )?;
    std::fs::write(
        bravo_tree.join("deps.lock"),
        body.replace("dep_18 = \"1.0\"", "dep_18 = \"2.0\""),
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    assert!(
        merge_conflicts(&alpha_tree, "feat/bravo")?,
        "the driver under test must actually refuse the merge"
    );
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    let deps = at(&collisions, "deps.lock").context("expected a collision")?;
    assert_eq!(
        deps["severity"], "divergent",
        "a path handed to a merge driver cannot be adjacent: {deps}"
    );
    Ok(())
}

/// The attribute is read per path, not per repository.
///
/// The fix has to ask git about each contested path rather than giving up on a whole checkout the
/// moment a `.gitattributes` exists. Almost every repository that uses `*.lock -merge` has
/// ordinary source files alongside the lockfile, and blanket-blocking those would turn one narrow
/// false safe into a constant false alarm — which teaches people to ignore the blocker, and costs
/// more than the breaks it would catch. The same `-merge` rule is in force here; the path being
/// measured simply does not match it, and the merge really is clean.
#[test]
fn an_unmergeable_rule_leaves_other_paths_alone() -> Result<()> {
    let fleet = Fleet::new()?;
    let body = (0..60).fold(String::new(), |mut rows, line| {
        let _ = writeln!(rows, "pub fn line_{line}() {{}}");
        rows
    });
    std::fs::write(fleet.repository.join("src/wide.rs"), &body)?;
    std::fs::write(fleet.repository.join(".gitattributes"), "*.lock -merge\n")?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "wide"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;
    std::fs::write(
        alpha_tree.join("src/wide.rs"),
        body.replace("pub fn line_20() {}", "pub fn line_20_alpha() {}"),
    )?;
    std::fs::write(
        bravo_tree.join("src/wide.rs"),
        body.replace("pub fn line_40() {}", "pub fn line_40_bravo() {}"),
    )?;
    git(&alpha_tree, &["commit", "-am", "alpha"])?;
    git(&bravo_tree, &["commit", "-am", "bravo"])?;

    assert!(
        !merge_conflicts(&alpha_tree, "feat/bravo")?,
        "the oracle must agree this merge is clean, or the test proves nothing"
    );
    fleet.sync(&alpha, &alpha_tree)?;
    fleet.sync(&bravo, &bravo_tree)?;
    let collisions = fleet.collisions(&alpha)?;
    let wide = at(&collisions, "src/wide.rs").context("expected a collision")?;
    assert_eq!(
        wide["severity"], "adjacent",
        "an unrelated `-merge` rule must not block a path that merges cleanly: {wide}"
    );
    Ok(())
}

/// Every `merge` attribute state, checked against a real merge of the same shape.
///
/// The named tests above pin `-merge` and a driver. This walks the whole state space
/// `git check-attr merge` can report — nothing said, `merge` set plainly, `-merge`, a driver that
/// conflicts, and a driver that resolves — against the identical pair of far-apart edits each
/// time, so the only variable is the attribute. Each case asserts the classification against what
/// `git merge` actually did, and the sweep as a whole asserts that both outcomes occurred: a
/// classifier that had started reporting `divergent` for everything would pass every individual
/// case while proving nothing.
#[test]
fn every_merge_attribute_state_agrees_with_a_real_merge() -> Result<()> {
    // `union` resolves instead of conflicting, so a merge under it is clean — and the classifier
    // still refuses to call it adjacent, because a driver does not combine the sides along the
    // hunk coordinates we measured. That case is a tolerated false alarm, not a false safe, and
    // it is listed here so the trade is visible rather than implied.
    let cases: [(&str, &str, &str); 5] = [
        ("unspecified", "", "adjacent"),
        ("set", "*.lock merge\n", "adjacent"),
        ("unset", "*.lock -merge\n", "divergent"),
        ("driver-conflicts", "*.lock merge=refuses\n", "divergent"),
        ("driver-resolves", "*.lock merge=union\n", "divergent"),
    ];
    let mut clean = 0_usize;
    let mut conflicted = 0_usize;
    for (label, attributes, expected) in cases {
        let fleet = Fleet::new()?;
        let body = (1..=20).fold(String::new(), |mut rows, row| {
            let _ = writeln!(rows, "dep_{row} = \"1.0\"");
            rows
        });
        std::fs::write(fleet.repository.join("deps.lock"), &body)?;
        if !attributes.is_empty() {
            std::fs::write(fleet.repository.join(".gitattributes"), attributes)?;
        }
        git(
            &fleet.repository,
            &["config", "merge.refuses.driver", "false"],
        )?;
        git(&fleet.repository, &["add", "-A"])?;
        git(&fleet.repository, &["commit", "-m", "deps"])?;

        let (alpha, alpha_tree) = fleet.agent("alpha")?;
        let (bravo, bravo_tree) = fleet.agent("bravo")?;
        std::fs::write(
            alpha_tree.join("deps.lock"),
            body.replace("dep_2 = \"1.0\"", "dep_2 = \"2.0\""),
        )?;
        std::fs::write(
            bravo_tree.join("deps.lock"),
            body.replace("dep_18 = \"1.0\"", "dep_18 = \"2.0\""),
        )?;
        git(&alpha_tree, &["commit", "-am", "alpha"])?;
        git(&bravo_tree, &["commit", "-am", "bravo"])?;

        let conflicts = merge_conflicts(&alpha_tree, "feat/bravo")?;
        if conflicts {
            conflicted += 1;
        } else {
            clean += 1;
        }
        fleet.sync(&alpha, &alpha_tree)?;
        fleet.sync(&bravo, &bravo_tree)?;
        let collisions = fleet.collisions(&alpha)?;
        let deps = at(&collisions, "deps.lock").context("expected a collision")?;
        let severity = deps["severity"].as_str().context("severity")?;
        assert!(
            !(severity == "adjacent" && conflicts),
            "{label}: reported adjacent for a merge that conflicts: {deps}"
        );
        assert_eq!(
            severity, expected,
            "{label}: unexpected severity for `{attributes}`: {deps}"
        );
    }
    assert!(
        clean > 0 && conflicted > 0,
        "the oracle produced {clean} clean and {conflicted} conflicting merges; \
         it has to produce both or it is not discriminating"
    );
    Ok(())
}

/// A checkout that stopped reading has to say so, not read as having found nothing.
///
/// The reference harvester gives up on a file past its size ceiling, which leaves the checkout's
/// name set incomplete. An incomplete set can rule a break *in* but never *out*, so the scan is
/// supposed to mark itself truncated and match every withdrawn export. This asserts that the
/// safeguard is really wired through to the merge gate rather than merely recorded: the failure
/// it prevents — one oversized generated file quietly turning a real break into a clean merge —
/// is the exact shape of bug this whole mechanism exists to stop.
#[test]
fn a_checkout_that_stopped_reading_still_blocks_a_withdrawal() -> Result<()> {
    let fleet = Fleet::new()?;
    std::fs::write(
        fleet.repository.join("src/api.rs"),
        "pub fn widget_total() -> u32 { 0 }\n",
    )?;
    git(&fleet.repository, &["add", "-A"])?;
    git(&fleet.repository, &["commit", "-m", "api"])?;

    let (alpha, alpha_tree) = fleet.agent("alpha")?;
    let (bravo, bravo_tree) = fleet.agent("bravo")?;

    // Alpha withdraws the export.
    std::fs::write(
        alpha_tree.join("src/api.rs"),
        "pub fn widget_sum() -> u32 { 0 }\n",
    )?;
    // Bravo's only changed source file is past the harvester's ceiling, and — this is the point —
    // it does not mention `widget_total` anywhere. Nothing was read, so nothing can be ruled out.
    let generated = "pub fn generated_filler() {}\n".repeat(90_000);
    assert!(
        generated.len() as u64 > 2 * 1024 * 1024,
        "the file must exceed MAX_SCANNED_BYTES for this to prove anything"
    );
    std::fs::write(bravo_tree.join("src/generated.rs"), &generated)?;

    fleet.sync(&alpha, &alpha_tree)?;
    let scan = scan_worktree(&bravo_tree, Some("main"))?;
    assert!(
        scan.symbols.truncated,
        "the oversized file must mark the scan incomplete"
    );
    assert!(
        !scan.symbols.references.iter().any(|n| n == "widget_total"),
        "bravo genuinely never read the name, so only the truncation flag can save this"
    );
    fleet.sync(&bravo, &bravo_tree)?;

    let report = readiness(&fleet, &alpha, &alpha_tree)?;
    assert!(
        blocker_codes(&report).contains(&"removed_export_in_use".to_owned()),
        "a checkout that stopped reading must be assumed to reference everything: {report}"
    );
    Ok(())
}

/// Deterministic pseudo-randomness, so a failure here is a failure anyone can reproduce.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound.max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.next() % bound).unwrap_or(0)
    }

    /// A line index, weighted towards the edges of the file where off-by-ones live.
    fn line(&mut self, len: usize) -> usize {
        match self.below(6) {
            0 => 0,
            1 => 1.min(len.saturating_sub(1)),
            2 => len.saturating_sub(1),
            3 => len.saturating_sub(2),
            _ => self.below(len),
        }
    }
}

/// Rewrite, insert, or delete — the three things a diff can report.
fn mutate(base: &[String], rng: &mut Seeded, tag: &str) -> Vec<String> {
    let mut lines = base.to_vec();
    let mut edits: Vec<(usize, usize, usize)> = (0..=rng.below(3))
        .map(|_| (rng.line(base.len()), rng.below(3), 1 + rng.below(3)))
        .collect();
    // Apply back to front, so an earlier edit cannot move a later index.
    edits.sort_unstable_by_key(|(at, _, _)| std::cmp::Reverse(*at));
    for (at, kind, span) in edits {
        match kind {
            0 if at < lines.len() => lines[at] = format!("line_{at}_{tag}"),
            1 => {
                let at = at.min(lines.len());
                for step in 0..span {
                    lines.insert(at, format!("new_{at}_{step}_{tag}"));
                }
            }
            _ => {
                let end = (at + span).min(lines.len());
                if at < end {
                    lines.drain(at..end);
                }
            }
        }
    }
    lines
}

/// Severity, checked against real merges over a spread of generated edit pairs.
///
/// The named cases in this file each pin one shape of edit. This covers the space between them:
/// rewrites, insertions and deletions of random width, placed with a heavy bias toward the first
/// and last lines of the file, run against a real `git merge` every time. It is the check that
/// caught the adjacency rule being off by one line, and it is kept because the interesting
/// failures in this module have all been cases nobody thought to name.
///
/// Only one direction is an error. `divergent` over a merge that would have been clean is a
/// false alarm and is counted but tolerated; `identical` or `adjacent` over a merge that
/// conflicts is the product telling someone their work is safe to overwrite, and fails the test.
#[test]
fn generated_edit_pairs_agree_with_real_merges() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let repository = directory.path().join("repository");
    std::fs::create_dir(&repository)?;
    git(&repository, &["init", "-b", "main"])?;
    git(
        &repository,
        &["config", "user.email", "pidmesh@example.com"],
    )?;
    git(&repository, &["config", "user.name", "PidMesh Test"])?;
    std::fs::write(repository.join("f.txt"), "seed\n")?;
    git(&repository, &["add", "-A"])?;
    git(&repository, &["commit", "-m", "seed"])?;
    let alpha_tree = directory.path().join("wt-alpha");
    let bravo_tree = directory.path().join("wt-bravo");
    for (tree, branch) in [(&alpha_tree, "alpha"), (&bravo_tree, "bravo")] {
        git(
            &repository,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                tree.to_str().context("worktree path")?,
            ],
        )?;
    }

    let mut rng = Seeded(0x5eed_1234_abcd_0001);
    let mut checked = 0_usize;
    let mut false_alarms = 0_usize;
    for case in 0..40 {
        let len = 8 + rng.below(20);
        let base_lines: Vec<String> = (0..len).map(|line| format!("line_{line}")).collect();
        // Half the cases leave the final newline off, which is its own hunk shape.
        let terminator = if case % 2 == 0 { "\n" } else { "" };
        let render = |lines: &[String]| format!("{}{terminator}", lines.join("\n"));
        let base = render(&base_lines);
        let mine = render(&mutate(&base_lines, &mut rng, "alpha"));
        let theirs = render(&mutate(&base_lines, &mut rng, "bravo"));
        if mine == base || theirs == base || mine == theirs {
            continue;
        }

        std::fs::write(repository.join("f.txt"), &base)?;
        git(&repository, &["commit", "-am", "base"])?;
        for (tree, branch, body) in [
            (&alpha_tree, "alpha", &mine),
            (&bravo_tree, "bravo", &theirs),
        ] {
            git(tree, &["checkout", "-B", branch, "main"])?;
            std::fs::write(tree.join("f.txt"), body)?;
            git(tree, &["commit", "-am", "work"])?;
        }

        let observed = |tree: &Path, id: &str| -> Result<Participant> {
            let scan = scan_worktree(tree, Some("main"))?;
            let entry = scan
                .entries
                .iter()
                .find(|entry| entry.path == "f.txt")
                .context("the edited path must be part of the footprint")?;
            Ok(Participant {
                agent_id: id.to_owned(),
                agent_name: id.to_owned(),
                status: "running".to_owned(),
                change_kind: entry.change_kind.as_str().to_owned(),
                digest: entry.digest.clone(),
                base_commit: Some(scan.base_commit.clone()),
                branch: scan.branch.clone(),
                ranges: entry.ranges.clone(),
            })
        };
        let mine_observed = observed(&alpha_tree, "alpha")?;
        let theirs_observed = observed(&bravo_tree, "bravo")?;
        let severity = classify(&[mine_observed.clone(), theirs_observed.clone()]);
        let conflicts = merge_conflicts(&alpha_tree, "bravo")?;
        checked += 1;

        let called_safe = severity == "identical" || severity == "adjacent";
        assert!(
            !(called_safe && conflicts),
            "case {case}: reported {severity} for a merge that conflicts — \
             ranges {:?} against {:?}\nbase:\n{base}\nmine:\n{mine}\ntheirs:\n{theirs}",
            mine_observed.ranges,
            theirs_observed.ranges
        );
        if !called_safe && !conflicts {
            false_alarms += 1;
        }
    }
    // Guard the harness itself: a setup that silently stopped producing contested pairs would
    // pass this test without ever having compared anything.
    assert!(
        checked >= 25,
        "only {checked} cases actually produced a comparison"
    );
    assert!(
        false_alarms < checked,
        "every single case was reported as conflicting, which means the classifier is not \
         discriminating and this sweep is proving nothing"
    );
    Ok(())
}
