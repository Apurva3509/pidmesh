use std::time::Duration;

use anyhow::Result;
use pidmesh::mcp::PidMeshMcp;
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> Result<()> {
    let service = PidMeshMcp::from_environment()?;
    let store = service.store();
    let agent_id = service.agent_id();
    let heartbeat_store = store.clone();
    let heartbeat_agent = agent_id.clone();
    let heartbeat = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let store = heartbeat_store.clone();
            let agent = heartbeat_agent.clone();
            let _ = tokio::task::spawn_blocking(move || store.heartbeat(&agent)).await;
        }
    });
    let running = service.serve(stdio()).await?;
    let outcome = running.waiting().await;
    heartbeat.abort();
    let _ = tokio::task::spawn_blocking(move || store.stop_agent(&agent_id)).await;
    outcome?;
    Ok(())
}
