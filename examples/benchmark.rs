use std::time::Instant;

use anyhow::{Context, Result};
use pidmesh::store::MeshStore;
use serde_json::json;
use uuid::Uuid;

fn main() -> Result<()> {
    let directory = std::env::temp_dir().join(format!("pidmesh-bench-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&directory)?;
    let store = MeshStore::new(directory.join("mesh.db"))?;
    let registration = store.register_agent(
        "benchmark",
        std::process::id(),
        Some(&directory),
        "rust",
        &[],
        None,
    )?;
    let agent_id = registration["agent_id"].as_str().context("agent id")?;

    let started = Instant::now();
    for index in 0..2_000 {
        store.remember(
            agent_id,
            &format!("completed benchmark unit {index}"),
            "benchmark",
            Some(&index.to_string()),
            0.5,
        )?;
    }
    let writes_per_second = 2_000.0 / started.elapsed().as_secs_f64();

    let started = Instant::now();
    for _ in 0..500 {
        store.recall(agent_id, "benchmark completed", 10)?;
    }
    let recalls_per_second = 500.0 / started.elapsed().as_secs_f64();

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "runtime": "rust",
            "writes_per_second": writes_per_second.round(),
            "recalls_per_second": recalls_per_second.round()
        }))?
    );
    std::fs::remove_dir_all(directory)?;
    Ok(())
}
