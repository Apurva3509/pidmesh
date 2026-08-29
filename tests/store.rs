use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use pidmesh::store::MeshStore;
use serde_json::Value;
use tempfile::TempDir;

fn setup() -> Result<(TempDir, MeshStore)> {
    let directory = tempfile::tempdir()?;
    let store = MeshStore::new(directory.path().join("mesh.db"))?;
    Ok((directory, store))
}

fn register(store: &MeshStore, name: &str, workspace: &std::path::Path) -> Result<String> {
    let value =
        store.register_agent(name, std::process::id(), Some(workspace), "test", &[], None)?;
    Ok(value["agent_id"]
        .as_str()
        .context("missing agent id")?
        .to_owned())
}

#[test]
fn memory_is_shared_and_workspace_scoped() -> Result<()> {
    let (directory, store) = setup()?;
    let alpha = directory.path().join("alpha");
    let beta = directory.path().join("beta");
    std::fs::create_dir_all(&alpha)?;
    std::fs::create_dir_all(&beta)?;
    let codex = register(&store, "codex", &alpha)?;
    let claude = register(&store, "claude", &alpha)?;
    let outsider = register(&store, "other", &beta)?;
    store.remember(
        &codex,
        "Use SQLite WAL for concurrent writers",
        "decision",
        Some("storage"),
        0.9,
    )?;
    store.remember(&outsider, "Use a hosted database", "decision", None, 0.5)?;

    let results = store.recall(&claude, "SQLite concurrent", 10)?;
    let memories = results
        .as_array()
        .context("recall did not return an array")?;
    assert_eq!(memories.len(), 1);
    assert_eq!(memories[0]["agent_name"], "codex");
    Ok(())
}

#[test]
fn messages_have_independent_receipts() -> Result<()> {
    let (directory, store) = setup()?;
    let sender = register(&store, "planner", directory.path())?;
    let worker = register(&store, "worker", directory.path())?;
    let observer = register(&store, "observer", directory.path())?;
    let direct = store.send(&sender, "implement parser", "worker", None)?;
    let broadcast = store.send(&sender, "tests are green", "*", None)?;

    let worker_inbox = store.inbox(&worker, true, 50)?;
    let observer_inbox = store.inbox(&observer, true, 50)?;
    assert_eq!(worker_inbox.as_array().context("worker inbox")?.len(), 2);
    assert_eq!(
        observer_inbox.as_array().context("observer inbox")?.len(),
        1
    );
    let ids = [
        direct["message_id"].as_i64().context("direct id")?,
        broadcast["message_id"].as_i64().context("broadcast id")?,
    ];
    assert_eq!(store.acknowledge(&worker, &ids)?, 2);
    assert_eq!(store.inbox(&worker, true, 50)?, serde_json::json!([]));
    assert_eq!(
        store
            .inbox(&observer, true, 50)?
            .as_array()
            .context("observer inbox")?
            .len(),
        1
    );
    Ok(())
}

#[test]
fn claims_are_exclusive_and_recover_after_expiry() -> Result<()> {
    let (directory, store) = setup()?;
    let first = register(&store, "first", directory.path())?;
    let second = register(&store, "second", directory.path())?;
    assert_eq!(store.claim(&first, "task", 1, None)?["acquired"], true);
    assert_eq!(store.claim(&second, "task", 10, None)?["acquired"], false);
    thread::sleep(Duration::from_millis(1_050));
    let takeover = store.claim(&second, "task", 10, None)?;
    assert_eq!(takeover["acquired"], true);
    assert_eq!(takeover["agent_id"], second);
    Ok(())
}

#[test]
fn wait_wakes_when_another_agent_sends() -> Result<()> {
    let (directory, store) = setup()?;
    let receiver = register(&store, "receiver", directory.path())?;
    let sender = register(&store, "sender", directory.path())?;
    let existing = store.events(&receiver, 0, 100)?;
    let after = existing
        .as_array()
        .and_then(|events| events.last())
        .and_then(|event| event["sequence"].as_i64())
        .context("missing sequence")?;
    let sending_store = store.clone();
    let sending_sender = sender.clone();
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        sending_store.send(&sending_sender, "wake up", "receiver", None)
    });
    let events = store.wait_for_events(
        &receiver,
        after,
        Duration::from_secs(1),
        Duration::from_millis(10),
        100,
    )?;
    handle.join().expect("sender panicked")?;
    assert_eq!(events[0]["event_type"], "message.sent");
    Ok(())
}

#[test]
fn eight_threads_write_without_data_loss() -> Result<()> {
    let (directory, store) = setup()?;
    let workspace = directory.path().to_path_buf();
    let mut handles = Vec::new();
    for worker in 0..8 {
        let worker_store = store.clone();
        let worker_workspace = workspace.clone();
        handles.push(thread::spawn(move || -> Result<()> {
            let agent = register(
                &worker_store,
                &format!("worker-{worker}"),
                &worker_workspace,
            )?;
            for unit in 0..100 {
                worker_store.remember(
                    &agent,
                    &format!("worker {worker} completed native unit {unit}"),
                    "progress",
                    Some(&format!("{worker}:{unit}")),
                    0.5,
                )?;
            }
            Ok(())
        }));
    }
    for handle in handles {
        handle.join().expect("writer panicked")?;
    }
    let reader = register(&store, "reader", &workspace)?;
    let memories = store.recall(&reader, "native unit", 1_000)?;
    assert_eq!(memories.as_array().context("recall array")?.len(), 800);
    Ok(())
}

#[test]
fn eight_processes_produce_one_claim_winner() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("claims.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let mut agent_ids = Vec::new();
    for worker in 0..8 {
        let output = Command::new(binary)
            .args([
                "--db",
                database.to_str().context("database path")?,
                "join",
                "--name",
                &format!("worker-{worker}"),
                "--workspace",
                directory.path().to_str().context("workspace path")?,
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let registration: Value = serde_json::from_slice(&output.stdout)?;
        agent_ids.push(
            registration["agent_id"]
                .as_str()
                .context("agent id")?
                .to_owned(),
        );
    }
    let mut children = Vec::new();
    for agent_id in agent_ids {
        children.push(
            Command::new(binary)
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "claim",
                    "exclusive-task",
                    "--agent",
                    &agent_id,
                    "--lease-seconds",
                    "60",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    let mut winners = 0;
    for child in children {
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let claim: Value = serde_json::from_slice(&output.stdout)?;
        winners += usize::from(claim["acquired"] == true);
    }
    assert_eq!(winners, 1);
    Ok(())
}

#[test]
fn eight_processes_write_to_one_database() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("writes.db");
    let binary = env!("CARGO_BIN_EXE_pidmesh");
    let mut agent_ids = Vec::new();
    for worker in 0..8 {
        let output = Command::new(binary)
            .args([
                "--db",
                database.to_str().context("database path")?,
                "join",
                "--name",
                &format!("writer-{worker}"),
                "--workspace",
                directory.path().to_str().context("workspace path")?,
            ])
            .output()?;
        let registration: Value = serde_json::from_slice(&output.stdout)?;
        agent_ids.push(
            registration["agent_id"]
                .as_str()
                .context("agent id")?
                .to_owned(),
        );
    }
    let mut children = Vec::new();
    for (worker, agent_id) in agent_ids.iter().enumerate() {
        children.push(
            Command::new(binary)
                .args([
                    "--db",
                    database.to_str().context("database path")?,
                    "remember",
                    &format!("native process write {worker}"),
                    "--agent",
                    agent_id,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    for child in children {
        assert!(child.wait_with_output()?.status.success());
    }
    let store = MeshStore::new(&database)?;
    let reader = register(&store, "reader", directory.path())?;
    assert_eq!(
        store
            .recall(&reader, "native process write", 100)?
            .as_array()
            .context("recall")?
            .len(),
        8
    );
    Ok(())
}

#[test]
fn supervised_command_receives_identity_and_stops_cleanly() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("run.db");
    let marker = directory.path().join("agent-id.txt");
    let output = Command::new(env!("CARGO_BIN_EXE_pidmesh"))
        .args([
            "--db",
            database.to_str().context("database path")?,
            "run",
            "--name",
            "child",
            "--workspace",
            directory.path().to_str().context("workspace path")?,
            "--heartbeat-seconds",
            "0.01",
            "--",
            "/bin/sh",
            "-c",
            &format!("printf %s \"$PIDMESH_AGENT_ID\" > {}", marker.display()),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let registration: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(std::fs::read_to_string(marker)?, registration["agent_id"]);
    let store = MeshStore::new(database)?;
    let status = store.status(None, Some(directory.path()))?;
    assert_eq!(status["agents"][0]["status"], "stopped");
    Ok(())
}
