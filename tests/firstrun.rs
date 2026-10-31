//! What a newcomer sees, and what everyone sees when something is wrong.
//!
//! Every assertion here is about a message rather than a value: that a failure names the thing
//! that failed, where it failed, and what to do next, and that no command answers a broken
//! situation with a raw SQLite string, a bare errno, or a cheerful report of a clean mesh.
//!
//! Prose rendering only happens at a terminal, so these tests read stderr (which `render::error`
//! formats the same way either side of a pipe) and the JSON that a pipe receives.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result};
use serde_json::Value;

/// Run the CLI in `directory` against `database`, with git available as usual.
fn run(directory: &Path, database: &Path, arguments: &[&str]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(directory)
        .env("PIDMESH_DB", database)
        .env("NO_COLOR", "1")
        .args(arguments)
        .output()
        .context("failed to run pidmesh")
}

/// Run the CLI with nothing on PATH, so git cannot be started at all.
fn run_without_git(directory: &Path, database: &Path, arguments: &[&str]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(directory)
        .env("PIDMESH_DB", database)
        .env("NO_COLOR", "1")
        .env("PATH", "/nonexistent")
        // The macOS fallback probes xcode-select and would otherwise find a real git behind a
        // PATH that has none, which is not the situation under test.
        .env("PIDMESH_GIT", "git")
        .args(arguments)
        .output()
        .context("failed to run pidmesh")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A repository with one commit, which is the least a checkout needs to be observable.
fn repository() -> Result<tempfile::TempDir> {
    let directory = tempfile::tempdir()?;
    let path = directory.path();
    for arguments in [
        &["init", "-q", "-b", "main", "."][..],
        &["config", "user.email", "test@example.invalid"][..],
        &["config", "user.name", "Test"][..],
    ] {
        let status = Command::new("git")
            .current_dir(path)
            .args(arguments)
            .output()?;
        anyhow::ensure!(status.status.success(), "git {arguments:?} failed");
    }
    fs::write(path.join("file.txt"), "one\n")?;
    for arguments in [&["add", "-A"][..], &["commit", "-qm", "initial"][..]] {
        let status = Command::new("git")
            .current_dir(path)
            .args(arguments)
            .output()?;
        anyhow::ensure!(status.status.success(), "git {arguments:?} failed");
    }
    Ok(directory)
}

fn database_in(directory: &Path) -> PathBuf {
    directory.join("mesh.db")
}

#[test]
fn a_directory_that_is_not_a_repository_is_named_as_the_problem() -> Result<()> {
    // Advising `--base` was the old answer to all of these, and no branch name has ever fixed
    // a directory git will not read.
    let directory = tempfile::tempdir()?;
    let database = database_in(directory.path());
    for arguments in [&["sync"][..], &["mergeable"][..], &["worktrees"][..]] {
        let output = run(directory.path(), &database, arguments)?;
        assert!(!output.status.success(), "{arguments:?} reported success");
        let message = stderr(&output);
        assert!(
            message.contains("not inside a git repository"),
            "{arguments:?} did not name the real problem: {message}"
        );
        assert!(
            !message.contains("--base"),
            "{arguments:?} still advises a branch name: {message}"
        );
    }
    Ok(())
}

#[test]
fn a_repository_with_no_commits_says_so_instead_of_blaming_the_branch() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let status = Command::new("git")
        .current_dir(directory.path())
        .args(["init", "-q", "-b", "main", "."])
        .output()?;
    assert!(status.status.success());
    let database = database_in(directory.path());
    for arguments in [&["sync"][..], &["mergeable"][..]] {
        let message = stderr(&run(directory.path(), &database, arguments)?);
        assert!(
            message.contains("no commits yet"),
            "{arguments:?}: {message}"
        );
        assert!(
            !message.contains("--base"),
            "{arguments:?} still advises a branch name: {message}"
        );
    }
    Ok(())
}

#[test]
fn a_missing_git_is_reported_as_a_missing_git() -> Result<()> {
    // `worktrees` used to answer this with `No such file or directory (os error 2)`: an errno
    // with no subject, no path and no remedy.
    let repository = repository()?;
    let database = database_in(repository.path());
    for arguments in [&["worktrees"][..], &["sync"][..], &["mergeable"][..]] {
        let output = run_without_git(repository.path(), &database, arguments)?;
        assert!(!output.status.success(), "{arguments:?} reported success");
        let message = stderr(&output);
        assert!(
            message.contains("git is not installed, or not on PATH"),
            "{arguments:?} did not name git: {message}"
        );
        assert!(
            message.contains("PIDMESH_GIT"),
            "{arguments:?} offered no remedy: {message}"
        );
        assert!(
            !message.contains("os error 2"),
            "{arguments:?} still leaks a bare errno: {message}"
        );
    }
    Ok(())
}

#[test]
fn doctor_does_not_mistake_a_missing_git_for_a_missing_repository() -> Result<()> {
    // The diagnostic command giving the wrong diagnosis is worse than it giving none: the
    // directory really is a checkout, and `git init` here would make things worse.
    let repository = repository()?;
    let database = database_in(repository.path());
    let output = run_without_git(repository.path(), &database, &["doctor"])?;
    let report: Value = serde_json::from_str(&stdout(&output))?;
    let codes: Vec<&str> = report["warnings"]
        .as_array()
        .context("doctor has no warnings array")?
        .iter()
        .filter_map(|warning| warning["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"git_unavailable"),
        "doctor missed the real cause: {report}"
    );
    assert!(
        !codes.contains(&"not_a_repository"),
        "doctor blamed the repository for a missing git: {report}"
    );
    Ok(())
}

#[test]
fn a_sweep_of_an_unreadable_workspace_is_not_reported_as_a_clean_one() -> Result<()> {
    // `watch` reporting a healthy empty mesh outside any repository is the single most
    // dangerous output this tool can produce: it is a guard saying "all clear" while guarding
    // nothing at all.
    let directory = tempfile::tempdir()?;
    let database = database_in(directory.path());
    let output = run(directory.path(), &database, &["watch", "--once"])?;
    let report: Value = serde_json::from_str(&stdout(&output))?;
    let problem = report["workspace_problem"]
        .as_str()
        .context("an empty sweep outside a repository claimed nothing was wrong")?;
    assert!(problem.contains("not inside a git repository"), "{problem}");

    // The same sweep inside a real repository must stay silent about it.
    let repository = repository()?;
    let clean = run(
        repository.path(),
        &database_in(repository.path()),
        &["watch", "--once"],
    )?;
    let report: Value = serde_json::from_str(&stdout(&clean))?;
    assert!(
        report["workspace_problem"].is_null(),
        "a real repository was reported as a problem: {report}"
    );
    Ok(())
}

#[test]
fn a_database_that_is_not_a_database_says_which_file_and_what_to_do() -> Result<()> {
    // The old answer was `file is not a database: Error code 26: file is not a database`:
    // doubled, numbered, and naming neither the path nor a way out.
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("not-a-mesh.db");
    fs::write(&database, b"this is a text file, not SQLite\n")?;
    let output = run(directory.path(), &database, &["status"])?;
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(
        message.contains("not-a-mesh.db"),
        "the file is not named: {message}"
    );
    assert!(
        message.contains("PIDMESH_DB"),
        "no remedy is offered: {message}"
    );
    assert!(
        !message.contains("Error code"),
        "a raw SQLite code still reaches the reader: {message}"
    );
    Ok(())
}

#[test]
fn a_database_path_that_cannot_be_opened_explains_the_path_not_the_errno() -> Result<()> {
    let directory = tempfile::tempdir()?;
    // A directory where a file is expected is the most common form of this mistake.
    let database = directory.path().join("mesh.db");
    fs::create_dir(&database)?;
    let output = run(directory.path(), &database, &["status"])?;
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("mesh.db"), "{message}");
    assert!(
        message.contains("PIDMESH_DB"),
        "no remedy is offered: {message}"
    );
    assert!(!message.contains("Error code"), "{message}");
    Ok(())
}

#[test]
fn the_first_command_prints_the_line_the_quickstart_tells_you_to_copy() -> Result<()> {
    // Step one of the quickstart says to set PIDMESH_AGENT_ID from what join prints, so the id
    // has to be in the JSON a script reads and in the export line a person pastes.
    let repository = repository()?;
    let database = database_in(repository.path());
    let output = run(repository.path(), &database, &["join", "--name", "planner"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(&stdout(&output))?;
    let id = value["agent_id"]
        .as_str()
        .context("join printed no agent id")?;
    assert!(id.starts_with("planner-"), "{id}");
    Ok(())
}

#[test]
fn the_quickstart_does_not_open_with_a_command_that_never_returns() -> Result<()> {
    // A newcomer following step two literally used to get a process that swept every five
    // seconds forever, with nothing on screen saying so or how to stop it.
    let directory = tempfile::tempdir()?;
    let output = run(
        directory.path(),
        &database_in(directory.path()),
        &["quickstart"],
    )?;
    let value: Value = serde_json::from_str(&stdout(&output))?;
    let steps = value["steps"].as_array().context("no steps")?;
    let watch = steps
        .iter()
        .find(|step| {
            step["command"]
                .as_str()
                .is_some_and(|command| command.contains("watch"))
        })
        .context("the quickstart never mentions watch")?;
    let command = watch["command"].as_str().unwrap_or_default();
    assert!(
        command.contains("--once"),
        "the quickstart still opens a loop: {command}"
    );
    assert!(
        watch["why"]
            .as_str()
            .is_some_and(|why| why.contains("once")),
        "nothing explains the looping form: {watch}"
    );
    Ok(())
}

#[test]
fn the_json_contract_survives_every_rendering_change() -> Result<()> {
    // Prose is for a terminal only. Agents and scripts parse these, and that contract predates
    // every renderer added here, so a pipe must still receive an object.
    let repository = repository()?;
    let database = database_in(repository.path());
    let joined = run(repository.path(), &database, &["join", "--name", "planner"])?;
    let agent = serde_json::from_str::<Value>(&stdout(&joined))?["agent_id"]
        .as_str()
        .context("no agent id")?
        .to_owned();
    for arguments in [
        &["status"][..],
        &["worktrees"][..],
        &["doctor"][..],
        &["watch", "--once"][..],
        &["collisions", "--agent", &agent][..],
    ] {
        let output = run(repository.path(), &database, arguments)?;
        let text = stdout(&output);
        serde_json::from_str::<Value>(&text)
            .with_context(|| format!("{arguments:?} did not emit JSON to a pipe: {text}"))?;
    }
    Ok(())
}

/// Add a linked worktree on a new branch, as anyone running parallel agents would.
fn add_worktree(repository: &Path, name: &str) -> Result<PathBuf> {
    let path = repository.join(name);
    let output = Command::new("git")
        .current_dir(repository)
        .args(["worktree", "add", "-q", "-b", name])
        .arg(&path)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(path)
}

#[test]
fn a_repository_whose_agents_never_joined_is_not_reported_as_watched() -> Result<()> {
    // The most dangerous output this tool can produce: a confident all-green report delivered
    // while several worktrees edit the same files. Registration is the only thing a sweep
    // counts, so "nothing is wrong" and "I am not looking at anything" produced identical
    // output — and git could have told them apart for the cost of one command.
    let repository = repository()?;
    for name in ["agent-one", "agent-two", "agent-three"] {
        add_worktree(repository.path(), name)?;
    }
    let database = database_in(repository.path());
    let output = run(repository.path(), &database, &["watch", "--once"])?;
    let report: Value = serde_json::from_str(&stdout(&output))?;

    assert_eq!(report["checkouts"], 0, "nothing should have been scanned");
    // The machine-readable signal, so an agent can act on this and not only a person.
    assert_eq!(
        report["git_worktrees"], 4,
        "the sweep did not count what git can see: {report}"
    );
    let unwatched = report["unwatched_worktrees"]
        .as_array()
        .context("no unwatched_worktrees field")?;
    assert_eq!(
        unwatched.len(),
        4,
        "an unwatched repository reported no gap: {report}"
    );

    // And doctor must not call this healthy, because every other check it makes passes
    // trivially when there is nothing being checked.
    let output = run(repository.path(), &database, &["doctor"])?;
    assert!(
        !output.status.success(),
        "doctor exited zero on a repository it is watching none of"
    );
    let report: Value = serde_json::from_str(&stdout(&output))?;
    assert_eq!(report["healthy"], false, "{report}");
    let warning = report["warnings"]
        .as_array()
        .context("no warnings")?
        .iter()
        .find(|warning| warning["code"] == "unwatched_worktrees")
        .context("doctor reported healthy while watching nothing")?;
    assert_eq!(
        warning["worktrees"].as_array().map(Vec::len),
        Some(4),
        "the warning does not say which checkouts: {warning}"
    );
    Ok(())
}

#[test]
fn a_fully_watched_repository_raises_no_coverage_warning() -> Result<()> {
    // The counterpart that keeps the warning worth reading. Running the watcher from the main
    // worktree is the documented arrangement, so an unregistered main checkout is not a gap
    // while the worktrees that hold work are all registered.
    let repository = repository()?;
    let database = database_in(repository.path());
    let worktree = add_worktree(repository.path(), "agent-one")?;

    // This process stays alive for the whole test, so the registration stays live.
    let pid = std::process::id().to_string();
    let joined = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(&worktree)
        .env("PIDMESH_DB", &database)
        .env("NO_COLOR", "1")
        .args(["join", "--name", "one", "--pid", &pid])
        .output()?;
    assert!(joined.status.success(), "{}", stderr(&joined));

    let output = run(repository.path(), &database, &["watch", "--once"])?;
    let report: Value = serde_json::from_str(&stdout(&output))?;
    assert_eq!(report["checkouts"], 1, "{report}");
    assert_eq!(
        report["unwatched_worktrees"].as_array().map(Vec::len),
        Some(0),
        "the main worktree was reported as a gap in the ordinary arrangement: {report}"
    );

    let output = run(repository.path(), &database, &["doctor"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output))?;
    assert_eq!(report["healthy"], true, "{report}");
    Ok(())
}

#[test]
fn a_worktree_nobody_is_watching_is_named_while_the_others_are_scanned() -> Result<()> {
    // Partial coverage is the same failure in smaller print: the collision report is
    // authoritative only for the checkouts actually in it, and it never said which those were.
    let repository = repository()?;
    let database = database_in(repository.path());
    let watched = add_worktree(repository.path(), "agent-one")?;
    add_worktree(repository.path(), "agent-two")?;

    let pid = std::process::id().to_string();
    let joined = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .current_dir(&watched)
        .env("PIDMESH_DB", &database)
        .env("NO_COLOR", "1")
        .args(["join", "--name", "one", "--pid", &pid])
        .output()?;
    assert!(joined.status.success(), "{}", stderr(&joined));

    let output = run(repository.path(), &database, &["watch", "--once"])?;
    let report: Value = serde_json::from_str(&stdout(&output))?;
    assert_eq!(report["checkouts"], 1, "{report}");
    let unwatched: Vec<&str> = report["unwatched_worktrees"]
        .as_array()
        .context("no unwatched_worktrees field")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        unwatched.len(),
        1,
        "the unwatched worktree was not reported: {report}"
    );
    assert!(
        unwatched[0].contains("agent-two"),
        "the wrong checkout was named: {unwatched:?}"
    );
    Ok(())
}

/// Capture the agent id the way the quickstart's output invites you to: in a subshell.
///
/// The shell that runs `join` here exits the moment the substitution closes, so the process
/// `join` bound the agent to is dead by the time anything looks for it. This is the
/// reproduction from issue #40, kept in the shape a person would actually type.
fn join_through_a_subshell(directory: &Path, database: &Path, name: &str) -> Result<Output> {
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let script = format!(
        r#"A=$("{binary}" join --name {name} --json | grep -o '"agent_id": "[^"]*"' | cut -d'"' -f4); printf '%s' "$A""#
    );
    Command::new("/bin/sh")
        .current_dir(directory)
        .env("PIDMESH_DB", database)
        .env("NO_COLOR", "1")
        .args(["-c", &script])
        .output()
        .context("failed to run the subshell reproduction")
}

#[test]
fn join_refuses_a_pid_that_is_not_a_running_process() -> Result<()> {
    // An agent bound to a process that is already gone is one nothing will ever observe:
    // live_checkouts filters on a surviving pid, so the footprint is published and then
    // skipped by every sweep. join used to answer this with a cheerful agent id.
    let repository = repository()?;
    let database = database_in(repository.path());
    // Above every platform's pid ceiling, so it cannot name a live process and cannot be
    // recycled into one while the test runs.
    let output = run(
        repository.path(),
        &database,
        &["join", "--name", "alpha", "--pid", "2147483646"],
    )?;
    assert!(
        !output.status.success(),
        "join registered a process that does not exist: {}",
        stdout(&output)
    );
    let message = stderr(&output);
    assert!(
        message.contains("2147483646"),
        "the refusal does not name the pid: {message}"
    );
    assert!(
        message.contains("--pid"),
        "the refusal does not say how to fix it: {message}"
    );

    // A refused join must also leave nothing behind, or the next sweep reports the very
    // registration join said it would not make.
    let report: Value =
        serde_json::from_str(&stdout(&run(repository.path(), &database, &["status"])?))?;
    assert_eq!(
        report["agents"].as_array().map(Vec::len),
        Some(0),
        "a refused join still registered an agent: {report}"
    );
    Ok(())
}

#[test]
fn an_explicitly_given_pid_is_taken_at_its_word() -> Result<()> {
    // The counterpart that keeps the caution below worth reading: when the caller names the
    // process, pidmesh has nothing to guess and must say nothing.
    let repository = repository()?;
    let database = database_in(repository.path());
    let pid = std::process::id().to_string();
    let output = run(
        repository.path(),
        &database,
        &["join", "--name", "alpha", "--pid", &pid],
    )?;
    assert!(output.status.success(), "{}", stderr(&output));
    let value: Value = serde_json::from_str(&stdout(&output))?;
    assert_eq!(value["pid_source"], "given", "{value}");
    assert!(
        value["warnings"]
            .as_array()
            .context("join reports no warnings field")?
            .is_empty(),
        "an explicit pid was second-guessed: {value}"
    );
    assert!(
        stderr(&output).is_empty(),
        "an explicit pid printed a caution: {}",
        stderr(&output)
    );
    Ok(())
}

#[test]
fn joining_through_a_command_substitution_is_reported_while_it_is_happening() -> Result<()> {
    // The failure this guards: both join and sync reported success, and the mistake only
    // surfaced later as an empty sweep. join cannot detect it by liveness — the subshell is
    // still running while join runs — so the honest answer is to say it had to guess, on
    // stderr, which is the one stream command substitution does not swallow.
    let repository = repository()?;
    let database = database_in(repository.path());
    let output = join_through_a_subshell(repository.path(), &database, "alpha")?;
    assert!(output.status.success(), "{}", stderr(&output));
    let agent = stdout(&output);
    assert!(agent.starts_with("alpha-"), "no agent id captured: {agent}");

    let caution = stderr(&output);
    assert!(
        caution.contains("--pid $$"),
        "join never named the incantation that survives a subshell: {caution}"
    );
    assert!(
        caution.contains("subshell"),
        "join never said what it had guessed: {caution}"
    );

    // And the registration really is dead on arrival, which is what the caution is about.
    let report: Value = serde_json::from_str(&stdout(&run(
        repository.path(),
        &database,
        &["watch", "--once"],
    )?))?;
    assert_eq!(report["checkouts"], 0, "{report}");
    assert_eq!(report["stale_agents"], 1, "{report}");
    Ok(())
}

#[test]
fn sync_does_not_report_plain_success_for_an_agent_with_no_live_process() -> Result<()> {
    // Second half of the same reproduction. The footprint is real and collisions still read
    // it, so sync publishes — but a watcher will never scan this checkout, and sync is the
    // last command that can say so before the mistake goes quiet.
    let repository = repository()?;
    let database = database_in(repository.path());
    let agent = stdout(&join_through_a_subshell(
        repository.path(),
        &database,
        "alpha",
    )?);
    assert!(agent.starts_with("alpha-"), "no agent id captured: {agent}");

    let output = run(repository.path(), &database, &["sync", "--agent", &agent])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output))?;
    assert_eq!(
        report["pid_alive"], false,
        "sync does not report whether anything can observe this agent: {report}"
    );
    let warnings = report["warnings"]
        .as_array()
        .context("sync reports no warnings field")?;
    assert!(
        !warnings.is_empty(),
        "sync reported plain success for an agent nothing will scan: {report}"
    );
    let caution = stderr(&output);
    assert!(
        caution.contains("watch"),
        "the caution does not say what will not happen: {caution}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_read_only_mesh_file_does_not_answer_two_commands_differently() -> Result<()> {
    // Observed during the first-run UX work and never chased: join returned an agent id
    // while status against the same file failed. The cause was ordering, not permissions.
    // MeshStore opened the connection before it normalised the file's mode, SQLite latched
    // the handle read-only, the command failed — and the chmod it had just performed made
    // the *next* command succeed. Same file, two answers, decided by which ran first.
    use std::os::unix::fs::PermissionsExt;

    let repository = repository()?;
    let database = database_in(repository.path());
    run(repository.path(), &database, &["init"])?;
    fs::set_permissions(&database, fs::Permissions::from_mode(0o444))?;

    let first = run(repository.path(), &database, &["status"])?;
    assert!(
        first.status.success(),
        "the first command against a read-only mesh failed: {}",
        stderr(&first)
    );
    let second = run(
        repository.path(),
        &database,
        &[
            "join",
            "--name",
            "alpha",
            "--pid",
            &std::process::id().to_string(),
        ],
    )?;
    assert!(
        second.status.success(),
        "the second command answered differently from the first: {}",
        stderr(&second)
    );
    assert_eq!(
        fs::metadata(&database)?.permissions().mode() & 0o777,
        0o600,
        "the mesh file was left readable by anyone"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_mesh_that_cannot_be_made_writable_fails_the_same_way_every_time() -> Result<()> {
    // The repair above must not swallow the real case: a database in a directory nobody can
    // write cannot be fixed by a chmod, and every command has to say the same thing about it.
    use std::os::unix::fs::PermissionsExt;

    let repository = repository()?;
    let enclosing = repository.path().join("mesh");
    fs::create_dir(&enclosing)?;
    let database = enclosing.join("mesh.db");
    run(repository.path(), &database, &["init"])?;
    fs::set_permissions(&enclosing, fs::Permissions::from_mode(0o555))?;
    // A process that can write anyway is running as root, where these permissions mean
    // nothing and the test would only assert something about the sandbox.
    let unprivileged = fs::File::create(enclosing.join("probe")).is_err();

    let outcomes: Vec<Output> = [&["status"][..], &["join", "--name", "alpha"][..]]
        .iter()
        .map(|arguments| run(repository.path(), &database, arguments))
        .collect::<Result<_>>()?;
    fs::set_permissions(&enclosing, fs::Permissions::from_mode(0o755))?;

    if unprivileged {
        for output in &outcomes {
            assert!(
                !output.status.success(),
                "a mesh that cannot be written reported success: {}",
                stdout(output)
            );
            assert!(
                stderr(output).contains("not writable"),
                "the failure does not name the problem: {}",
                stderr(output)
            );
        }
    }
    Ok(())
}
