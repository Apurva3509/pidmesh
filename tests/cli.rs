//! The surface a newcomer meets first: help, quickstart and shell completions.
//!
//! These are the only commands that answer without a mesh, so they are also the only ones whose
//! correctness includes *not* touching the database.

use std::path::Path;
use std::process::{Command, Output};

use anyhow::{Context, Result};
use serde_json::Value;

/// Run the CLI with a database path that must stay unused.
fn run(database: &Path, arguments: &[&str]) -> Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .env("PIDMESH_DB", database)
        .args(arguments)
        .output()
        .context("failed to run pidmesh")
}

fn stdout(output: &Output) -> Result<String> {
    String::from_utf8(output.stdout.clone()).context("pidmesh printed invalid UTF-8")
}

#[test]
fn completions_are_generated_for_every_documented_shell() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("mesh.db");
    for shell in ["bash", "zsh", "fish"] {
        let output = run(&database, &["completions", shell])?;
        assert!(output.status.success(), "{shell} completions failed");
        let script = stdout(&output)?;
        assert!(script.contains("pidmesh"), "{shell} script names no binary");
        // Generated from the live definition rather than a checked-in copy, so a subcommand
        // added to the parser has to appear here without anyone maintaining a second list.
        assert!(
            script.contains("mergeable"),
            "{shell} script misses a subcommand"
        );
        assert!(
            script.contains("quickstart"),
            "{shell} script misses a subcommand"
        );
    }
    Ok(())
}

#[test]
fn an_unknown_shell_is_refused_rather_than_guessed() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let output = run(
        &directory.path().join("mesh.db"),
        &["completions", "seashell"],
    )?;
    assert!(!output.status.success());
    Ok(())
}

#[test]
fn quickstart_keeps_the_json_contract_when_stdout_is_not_a_terminal() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("mesh.db");
    for arguments in [&["quickstart"][..], &["quickstart", "--json"][..]] {
        let output = run(&database, arguments)?;
        assert!(output.status.success());
        let value: Value = serde_json::from_str(&stdout(&output)?)
            .context("quickstart did not emit JSON to a pipe")?;
        let steps = value["steps"]
            .as_array()
            .context("quickstart has no steps")?;
        assert_eq!(steps.len(), 3, "the path to value is three commands");
        for (index, step) in steps.iter().enumerate() {
            assert_eq!(step["step"], index + 1);
            assert!(
                step["command"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("pidmesh "))
            );
            assert!(step["why"].as_str().is_some_and(|why| !why.is_empty()));
        }
    }
    Ok(())
}

#[test]
fn neither_command_creates_a_mesh_as_a_side_effect() -> Result<()> {
    // A completion script is sourced from a shell startup file, so this runs on every new
    // terminal. Creating a database there would be a surprise, and so would doing it for help.
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("mesh.db");
    for arguments in [
        &["completions", "bash"][..],
        &["quickstart"][..],
        &["--help"][..],
    ] {
        let output = run(&database, arguments)?;
        assert!(output.status.success(), "{arguments:?} failed");
        assert!(!database.exists(), "{arguments:?} opened the mesh");
    }
    Ok(())
}

#[test]
fn every_subcommand_explains_itself_in_the_top_level_help() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let help = stdout(&run(&directory.path().join("mesh.db"), &["--help"])?)?;
    // A summary line per command is what makes the list readable top to bottom; a bare name is
    // the failure this guards against.
    for command in [
        "quickstart",
        "init",
        "join",
        "heartbeat",
        "leave",
        "remember",
        "recall",
        "send",
        "inbox",
        "claim",
        "release",
        "reserve",
        "resources",
        "unreserve",
        "sync",
        "collisions",
        "mergeable",
        "integrate",
        "worktrees",
        "doctor",
        "unsync",
        "watch",
        "status",
        "events",
        "wait",
        "gc",
        "run",
        "swarm",
        "dashboard",
        "completions",
    ] {
        let line = help
            .lines()
            .find(|line| line.trim_start().starts_with(&format!("{command} ")))
            .with_context(|| format!("{command} is missing from the help"))?;
        let summary = line.trim_start().trim_start_matches(command).trim();
        assert!(
            summary.len() > 20,
            "{command} has no real summary: {summary:?}"
        );
    }
    Ok(())
}

/// A pipeline ending early is an ordinary end, not a crash.
///
/// Rust ignores `SIGPIPE`, so a write to a reader that has gone away surfaces as an error and the
/// print macros turn it into a panic with a backtrace. `pidmesh collisions | head` is a reasonable
/// thing to type, and every other tool in the pipeline exits quietly when it happens.
#[test]
fn a_closed_pipe_is_not_a_crash() -> Result<()> {
    use std::io::Read as _;
    use std::process::Stdio;

    // Completions are comfortably larger than a pipe buffer, so the write genuinely fails
    // rather than being absorbed and discarded after the reader leaves.
    let mut child = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args(["completions", "zsh"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drop the read end immediately; the child is still writing.
    drop(child.stdout.take());
    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        handle.read_to_string(&mut stderr)?;
    }
    child.wait()?;
    assert!(
        !stderr.contains("panicked"),
        "a closed pipe must not panic: {stderr}"
    );
    Ok(())
}
