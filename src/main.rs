use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use pidmesh::store::{MeshStore, default_database_path, workspace_root};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "pidmesh", version, about)]
struct Cli {
    #[arg(long, global = true, env = "PIDMESH_DB")]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Join {
        #[arg(long)]
        name: String,
        #[arg(long)]
        pid: Option<u32>,
        #[arg(long, default_value = "unknown")]
        provider: String,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Heartbeat {
        #[arg(long)]
        agent: Option<String>,
    },
    Leave {
        #[arg(long)]
        agent: Option<String>,
    },
    Remember {
        text: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = "note")]
        kind: String,
        #[arg(long)]
        key: Option<String>,
        #[arg(long, default_value_t = 0.5)]
        importance: f64,
    },
    Recall {
        query: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Send {
        text: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long = "to", default_value = "*")]
        recipient: String,
        #[arg(long)]
        correlation_id: Option<String>,
    },
    Inbox {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        ack: bool,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Claim {
        task: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 300)]
        lease_seconds: u64,
        #[arg(long)]
        detail: Option<String>,
    },
    Release {
        task: String,
        #[arg(long)]
        agent: Option<String>,
    },
    Status {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    Events {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Wait {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Gc {
        #[arg(long, default_value_t = 30)]
        stale_seconds: u64,
    },
    Run {
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "unknown")]
        provider: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value_t = 5.0)]
        heartbeat_seconds: f64,
        #[arg(last = true, required = true)]
        child_command: Vec<OsString>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("{}", json!({"error": format!("{error:#}")}));
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> Result<u8> {
    let cli = Cli::parse();
    let database = match cli.db {
        Some(path) => path,
        None => default_database_path()?,
    };
    let store = MeshStore::new(&database)?;
    match cli.command {
        Commands::Init { workspace } => emit(&json!({
            "database": database,
            "workspace": workspace_root(workspace.as_deref())?
        }))?,
        Commands::Join {
            name,
            pid,
            provider,
            capabilities,
            workspace,
        } => emit(&store.register_agent(
            &name,
            pid.unwrap_or_else(std::process::id),
            workspace.as_deref(),
            &provider,
            &capabilities,
            None,
        )?)?,
        Commands::Heartbeat { agent } => emit(&json!({
            "updated": store.heartbeat(&agent_id(agent.as_deref())?)?
        }))?,
        Commands::Leave { agent } => emit(&json!({
            "stopped": store.stop_agent(&agent_id(agent.as_deref())?)?
        }))?,
        Commands::Remember {
            text,
            agent,
            kind,
            key,
            importance,
        } => emit(&store.remember(
            &agent_id(agent.as_deref())?,
            &text,
            &kind,
            key.as_deref(),
            importance,
        )?)?,
        Commands::Recall {
            query,
            agent,
            limit,
        } => emit(&store.recall(&agent_id(agent.as_deref())?, &query, limit)?)?,
        Commands::Send {
            text,
            agent,
            recipient,
            correlation_id,
        } => emit(&store.send(
            &agent_id(agent.as_deref())?,
            &text,
            &recipient,
            correlation_id.as_deref(),
        )?)?,
        Commands::Inbox {
            agent,
            all,
            ack,
            limit,
        } => {
            let agent_id = agent_id(agent.as_deref())?;
            let messages = store.inbox(&agent_id, !all, limit)?;
            let acknowledged = if ack {
                let ids = messages
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|message| message["id"].as_i64())
                    .collect::<Vec<_>>();
                store.acknowledge(&agent_id, &ids)?
            } else {
                0
            };
            emit(&json!({"messages": messages, "acknowledged": acknowledged}))?;
        }
        Commands::Claim {
            task,
            agent,
            lease_seconds,
            detail,
        } => emit(&store.claim(
            &agent_id(agent.as_deref())?,
            &task,
            lease_seconds,
            detail.as_deref(),
        )?)?,
        Commands::Release { task, agent } => emit(&json!({
            "released": store.release(&agent_id(agent.as_deref())?, &task)?
        }))?,
        Commands::Status { agent, workspace } => {
            emit(&store.status(agent.as_deref(), workspace.as_deref())?)?;
        }
        Commands::Events {
            agent,
            after,
            limit,
        } => emit(&store.events(&agent_id(agent.as_deref())?, after, limit)?)?,
        Commands::Wait {
            agent,
            after,
            timeout_seconds,
            limit,
        } => emit(&store.wait_for_events(
            &agent_id(agent.as_deref())?,
            after,
            duration_from_seconds(timeout_seconds)?,
            Duration::from_millis(100),
            limit,
        )?)?,
        Commands::Gc { stale_seconds } => {
            emit(&store.collect_stale(Duration::from_secs(stale_seconds))?)?;
        }
        Commands::Run {
            name,
            provider,
            workspace,
            heartbeat_seconds,
            child_command,
        } => {
            return run_supervised(
                &store,
                &name,
                &provider,
                workspace.as_deref(),
                duration_from_seconds(heartbeat_seconds)?,
                &child_command,
            );
        }
    }
    Ok(0)
}

fn run_supervised(
    store: &MeshStore,
    name: &str,
    provider: &str,
    workspace: Option<&std::path::Path>,
    heartbeat_interval: Duration,
    child_command: &[OsString],
) -> Result<u8> {
    if heartbeat_interval < Duration::from_millis(10) {
        return Err(anyhow!(
            "heartbeat interval must be at least 10 milliseconds"
        ));
    }
    let agent_id = format!("{name}-run-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let root = workspace_root(workspace)?;
    let mut registration = store.register_agent(
        name,
        std::process::id(),
        Some(std::path::Path::new(&root)),
        provider,
        &[],
        Some(&agent_id),
    )?;
    let environment = json!({
        "PIDMESH_AGENT_ID": agent_id,
        "PIDMESH_DB": store.path(),
        "PIDMESH_WORKSPACE": root
    });
    let mut command = Command::new(&child_command[0]);
    command.args(&child_command[1..]);
    for (key, value) in environment
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, value)| value.as_str().map(|value| (key, value)))
    {
        command.env(key, value);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            store.stop_agent(&agent_id)?;
            return Err(error).context("failed to start supervised command");
        }
    };
    store.update_agent_pid(&agent_id, child.id())?;
    store.remember(
        &agent_id,
        &json!({"command": child_command, "environment": environment}).to_string(),
        "session",
        Some("process.start"),
        0.2,
    )?;
    registration["environment"] = environment;
    emit(&registration)?;

    let running = Arc::new(AtomicBool::new(true));
    let heartbeat_running = Arc::clone(&running);
    let heartbeat_store = store.clone();
    let heartbeat_agent = agent_id.clone();
    let heartbeat = thread::spawn(move || {
        while heartbeat_running.load(Ordering::Relaxed) {
            thread::park_timeout(heartbeat_interval);
            if heartbeat_running.load(Ordering::Relaxed) {
                let _ = heartbeat_store.heartbeat(&heartbeat_agent);
            }
        }
    });
    let status = child
        .wait()
        .context("failed to wait for supervised command")?;
    running.store(false, Ordering::Relaxed);
    heartbeat.thread().unpark();
    heartbeat
        .join()
        .map_err(|_| anyhow!("heartbeat thread panicked"))?;
    store.stop_agent(&agent_id)?;
    Ok(status.code().unwrap_or(1).try_into().unwrap_or(1))
}

fn agent_id(explicit: Option<&str>) -> Result<String> {
    explicit
        .map(ToOwned::to_owned)
        .or_else(|| std::env::var("PIDMESH_AGENT_ID").ok())
        .ok_or_else(|| anyhow!("agent id required: pass --agent or set PIDMESH_AGENT_ID"))
}

fn duration_from_seconds(seconds: f64) -> Result<Duration> {
    if !seconds.is_finite() || seconds.is_sign_negative() {
        bail_duration()
    } else {
        Ok(Duration::from_secs_f64(seconds))
    }
}

fn bail_duration<T>() -> Result<T> {
    Err(anyhow!("duration must be a finite non-negative number"))
}

fn emit(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
