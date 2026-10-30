use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use pidmesh::converge::{integration_head, scan_worktree};
use pidmesh::store::MeshStore;
use serde_json::Value;
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
    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(from)
        .env_remove("PIDMESH_WORKSPACE")
        .args([
            "--db",
            database.to_str().context("database path")?,
            "doctor",
        ])
        .output()?;
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
