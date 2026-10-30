use std::collections::BTreeMap;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use pidmesh::converge::{WorktreeScan, integration_head, scan_worktree};
use pidmesh::ide::IdeManager;
use pidmesh::render::{self, Shape};
use pidmesh::store::{
    AgentCheckout, INTEGRATION_TASK_KEY, MeshStore, default_database_path, diagnose, workspace_root,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// Local process-aware memory and coordination for concurrent AI agents.
///
/// PidMesh gives every agent process working in one repository a shared SQLite mesh: an identity
/// tied to a real PID, durable memory, an inbox, task leases, resource reservations, and a
/// convergence guard that observes what each git checkout actually changed and refuses a merge
/// that would overwrite a live peer or withdraw a name it still calls.
///
/// Nothing leaves the machine. There is no daemon, no account and no network call.
///
/// Commands print prose at a terminal and JSON everywhere else, so a person reads sentences while
/// a pipe, a redirect or an agent still gets the object it parses. Use --json to force JSON.
///
/// Start with: quickstart, join, watch, mergeable.
// clap prints these doc comments verbatim in --help, so they are prose for a reader rather than
// rustdoc; backticking the command names the lint wants would put the backticks on screen.
#[allow(clippy::doc_markdown)]
#[derive(Parser)]
#[command(
    name = "pidmesh",
    version,
    after_help = "Run 'pidmesh quickstart' for the three commands that take a fresh checkout all \
                  the way to a guarded merge."
)]
struct Cli {
    /// Emit JSON even when stdout is a terminal.
    #[arg(long, global = true)]
    json: bool,

    /// Mesh database to open; defaults to pidmesh.db under your home directory.
    #[arg(long, global = true, env = "PIDMESH_DB")]
    db: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

// Same reason as Cli: this help text is read on a terminal, not rendered as rustdoc.
#[allow(clippy::doc_markdown)]
#[derive(Subcommand)]
enum Commands {
    /// Print the three commands that take a fresh checkout to a guarded merge.
    Quickstart,
    /// Create the mesh database and report which workspace it is bound to.
    Init {
        /// Repository to bind the mesh to; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Register this process as an agent and print the identity every other command needs.
    Join {
        /// Readable name for this agent, such as planner or implementer.
        #[arg(long)]
        name: String,
        /// Process to treat as the agent; defaults to the shell that invoked this command.
        #[arg(long)]
        pid: Option<u32>,
        /// Harness driving this agent, such as codex, claude or cursor.
        #[arg(long, default_value = "unknown")]
        provider: String,
        /// A capability this agent offers; repeat the flag to declare several.
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        /// Repository this agent works in; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Prove this agent is still alive so a sweep does not reap what it holds.
    Heartbeat {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Mark this agent stopped and release its claims and reservations.
    Leave {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Write a durable note into the memory every agent in the mesh can search.
    Remember {
        /// What to remember.
        text: String,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// What kind of note this is, such as note, decision or handoff.
        #[arg(long, default_value = "note")]
        kind: String,
        /// Stable key, so writing the same key again supersedes the earlier note.
        #[arg(long)]
        key: Option<String>,
        /// Weight between 0 and 1 used to rank this note during a recall.
        #[arg(long, default_value_t = 0.5)]
        importance: f64,
    },
    /// Search the shared memory, ranked by relevance and importance.
    Recall {
        /// Full-text query to rank memories against.
        query: String,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Most memories to return.
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    /// Deliver a message to one agent, or broadcast it to the whole mesh.
    Send {
        /// Message body.
        text: String,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Agent to deliver to; the default broadcasts to every agent.
        #[arg(long = "to", default_value = "*")]
        recipient: String,
        /// Thread identifier, so a reply can be matched back to its request.
        #[arg(long)]
        correlation_id: Option<String>,
    },
    /// Read the messages addressed to this agent, and optionally acknowledge them.
    Inbox {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Include messages this agent already acknowledged.
        #[arg(long)]
        all: bool,
        /// Acknowledge everything this call returns.
        #[arg(long)]
        ack: bool,
        /// Most messages to return.
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Take an exclusive lease on a task so no peer starts the same work.
    Claim {
        /// Task key to claim; any stable string the fleet agrees on.
        task: String,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// How long the claim survives without a heartbeat.
        #[arg(long, default_value_t = 300)]
        lease_seconds: u64,
        /// Note stored with the claim so peers can see what is being done.
        #[arg(long)]
        detail: Option<String>,
    },
    /// Give up a task lease before it expires.
    Release {
        /// Task key to release.
        task: String,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Declare the paths, ports and services this agent intends to touch.
    Reserve {
        /// Resources to reserve, each prefixed path:, port: or service:.
        #[arg(required = true)]
        resources: Vec<String>,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Task these reservations belong to.
        #[arg(long)]
        task: Option<String>,
        /// Note stored with the reservations so peers can see why they are held.
        #[arg(long)]
        detail: Option<String>,
        /// How long the reservations survive without a heartbeat.
        #[arg(long, default_value_t = 300)]
        lease_seconds: u64,
    },
    /// List the paths, ports and services every live agent currently owns.
    Resources {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Release reserved paths, ports or services.
    Unreserve {
        /// Resources to release, each prefixed path:, port: or service:.
        #[arg(required = true)]
        resources: Vec<String>,
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Observe this checkout and publish what it actually changed.
    ///
    /// Pure observation: it reads git, never writes to the checkout, and counts committed,
    /// uncommitted and untracked work alike. A footprint is authoritative per agent, so a
    /// checkout that merges and cleans up drops out of every collision on its next scan.
    Sync {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Integration branch to measure against; defaults to main, then master.
        #[arg(long)]
        base: Option<String>,
        /// Checkout to observe; defaults to the current directory.
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    /// Report paths contested by more than one agent's observed footprint.
    ///
    /// Each contested path is classified by what a merge would really do to it, and withdrawn
    /// exported names that another live checkout still calls are reported alongside.
    Collisions {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Decide whether this agent can merge without breaking another checkout.
    ///
    /// Exits 1 and names every blocker when it cannot, so 'pidmesh mergeable && git merge ...'
    /// is a safe thing to write.
    Mergeable {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Integration branch to measure against; defaults to main, then master.
        #[arg(long)]
        base: Option<String>,
        /// Checkout to judge; defaults to the current directory.
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    /// Take or release the workspace-wide integration lease that serializes merges.
    ///
    /// Exits 1 when another agent already holds it and is merging right now.
    Integrate {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Release the lease instead of taking it.
        #[arg(long)]
        release: bool,
        /// How long the lease survives without a heartbeat.
        #[arg(long, default_value_t = 900)]
        lease_seconds: u64,
    },
    /// List managed worktrees, showing which hold work and which can be reclaimed.
    Worktrees {
        /// Repository to inspect; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Remove every worktree that is provably spent. Work is never discarded.
        #[arg(long)]
        reclaim: bool,
    },
    /// Check that this workspace is actually one shared mesh.
    ///
    /// Exits 1 when two workspace roots belong to one repository, which silently gives every
    /// checkout its own mesh while every other command still reports success.
    Doctor {
        /// Repository to check; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Withdraw this agent's footprint from the mesh.
    Unsync {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
    },
    /// Observe every live checkout in this workspace on an interval.
    ///
    /// Needs no cooperation from anyone: registration already recorded every checkout, so one
    /// watcher covers agents that will never call sync themselves. It also reaps dead sessions.
    Watch {
        /// Repository to sweep; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Integration branch to measure against; defaults to main, then master.
        #[arg(long)]
        base: Option<String>,
        /// Seconds to wait between sweeps.
        #[arg(long, default_value_t = 5.0)]
        interval_seconds: f64,
        /// Run a single pass and exit.
        #[arg(long)]
        once: bool,
    },
    /// Show who is alive in the mesh, what they hold, and what they are working on.
    Status {
        /// Restrict the report to one agent.
        #[arg(long)]
        agent: Option<String>,
        /// Repository to report on; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Read this agent's event ledger forward from a cursor.
    Events {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Return only events after this ledger cursor.
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Most events to return.
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    /// Block until this agent's next event arrives, instead of polling for it.
    Wait {
        /// Agent to act as; defaults to the identity in the environment.
        #[arg(long)]
        agent: Option<String>,
        /// Wait only for events after this ledger cursor.
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Give up after this many seconds and return whatever arrived.
        #[arg(long, default_value_t = 30.0)]
        timeout_seconds: f64,
        /// Most events to return.
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    /// Reap sessions whose process is gone and release everything they held.
    Gc {
        /// How long a session may be silent before its process is checked.
        #[arg(long, default_value_t = 30)]
        stale_seconds: u64,
    },
    /// Run a command as a supervised agent, heartbeating for it until it exits.
    Run {
        /// Readable name for the agent this command runs as.
        #[arg(long)]
        name: String,
        /// Harness driving this agent, such as codex, claude or cursor.
        #[arg(long, default_value = "unknown")]
        provider: String,
        /// Repository this agent works in; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Seconds between heartbeats while the child runs.
        #[arg(long, default_value_t = 5.0)]
        heartbeat_seconds: f64,
        /// Command to supervise, after a double dash. Its exit code becomes this one.
        #[arg(last = true, required = true)]
        child_command: Vec<OsString>,
    },
    /// Run many copies of a command as independently addressable agent processes.
    Swarm {
        /// Name each worker with this prefix and its index.
        #[arg(long, default_value = "worker")]
        name_prefix: String,
        /// Harness driving these agents, such as codex, claude or cursor.
        #[arg(long, default_value = "unknown")]
        provider: String,
        /// Repository these agents work in; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// How many worker processes to start, between 1 and 64.
        #[arg(long, default_value_t = 5)]
        workers: u16,
        /// Seconds between heartbeats while the workers run.
        #[arg(long, default_value_t = 5.0)]
        heartbeat_seconds: f64,
        /// Seconds a worker gets to exit on its own before it is killed.
        #[arg(long, default_value_t = 2.0)]
        shutdown_grace_seconds: f64,
        /// Stop the remaining workers as soon as one of them fails.
        #[arg(long)]
        fail_fast: bool,
        /// Command to run in every worker, after a double dash.
        #[arg(last = true, required = true)]
        child_command: Vec<OsString>,
    },
    /// Serve the private browser IDE for this workspace on localhost.
    #[command(alias = "ui")]
    Dashboard {
        /// Repository to serve; defaults to the current directory.
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Port to bind on localhost; 0 picks a free one.
        #[arg(long, default_value_t = 4399)]
        port: u16,
    },
    /// Print a shell completion script on stdout.
    ///
    /// Install it once and every subcommand, flag and alias here completes by name. See the
    /// README for the one line each shell needs.
    Completions {
        /// Shell to generate the script for.
        shell: Shell,
    },
}

static FORCE_JSON: AtomicBool = AtomicBool::new(false);

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(failure) => {
            eprintln!(
                "{}",
                render::error(&format!("{failure:#}"), FORCE_JSON.load(Ordering::Relaxed))
            );
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<u8> {
    let cli = Cli::parse();
    FORCE_JSON.store(cli.json, Ordering::Relaxed);
    // These two are answered out of the binary itself. Opening the store first would create a
    // database as a side effect of printing a completion script into a shell startup file, and
    // nobody reading the quickstart has a mesh to open yet.
    match cli.command {
        Commands::Completions { shell } => {
            completions(shell);
            Ok(0)
        }
        Commands::Quickstart => {
            quickstart()?;
            Ok(0)
        }
        command => dispatch(cli.db, command),
    }
}

/// Everything that needs the mesh: open the database once, then act.
#[allow(clippy::too_many_lines)]
fn dispatch(db: Option<PathBuf>, command: Commands) -> Result<u8> {
    let database = match db {
        Some(path) => path,
        None => default_database_path()?,
    };
    let store = MeshStore::new(&database)?;
    match command {
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
            pid.unwrap_or_else(calling_pid),
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
        Commands::Reserve {
            resources,
            agent,
            task,
            detail,
            lease_seconds,
        } => emit(&store.reserve_resources(
            &agent_id(agent.as_deref())?,
            &resources,
            task.as_deref(),
            detail.as_deref(),
            lease_seconds,
        )?)?,
        Commands::Resources { agent } => {
            emit(&store.resources(&agent_id(agent.as_deref())?)?)?;
        }
        Commands::Unreserve { resources, agent } => emit(&json!({
            "released": store.release_resources(&agent_id(agent.as_deref())?, &resources)?
        }))?,
        Commands::Sync {
            agent,
            base,
            checkout,
        } => {
            let directory = match checkout {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let scan = scan_worktree(&directory, base.as_deref())?;
            present(
                Shape::Sync,
                &store.publish_footprint(&agent_id(agent.as_deref())?, &scan)?,
            )?;
        }
        Commands::Collisions { agent } => {
            present(
                Shape::Collisions,
                &store.collisions(&agent_id(agent.as_deref())?)?,
            )?;
        }
        Commands::Mergeable {
            agent,
            base,
            checkout,
        } => {
            let directory = match checkout {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let (_, head) = integration_head(&directory, base.as_deref())?;
            let readiness = store.merge_readiness(&agent_id(agent.as_deref())?, &head)?;
            let mergeable = readiness["mergeable"] == true;
            present(Shape::Mergeable, &readiness)?;
            if !mergeable {
                return Ok(1);
            }
        }
        Commands::Integrate {
            agent,
            release,
            lease_seconds,
        } => {
            let agent = agent_id(agent.as_deref())?;
            if release {
                emit(&json!({
                    "released": store.release(&agent, INTEGRATION_TASK_KEY)?
                }))?;
            } else {
                let claimed = store.claim(
                    &agent,
                    INTEGRATION_TASK_KEY,
                    lease_seconds,
                    Some("merging into the integration branch"),
                )?;
                let acquired = claimed["acquired"] == true;
                emit(&claimed)?;
                if !acquired {
                    return Ok(1);
                }
            }
        }
        Commands::Worktrees { workspace, reclaim } => {
            let root = PathBuf::from(workspace_root(workspace.as_deref())?);
            let ide = IdeManager::new(store.clone(), root);
            // Reporting is the default and has no side effects. Removal is opt-in and still
            // proves each worktree is spent before touching it, because preserving an agent's
            // uncommitted work matters more than reclaiming disk.
            emit(&if reclaim {
                ide.reclaim()?
            } else {
                ide.worktrees()?
            })?;
        }
        Commands::Doctor { workspace } => {
            let report = diagnose(&database, workspace.as_deref())?;
            let healthy = report["healthy"] == true;
            present(Shape::Doctor, &report)?;
            if !healthy {
                return Ok(1);
            }
        }
        Commands::Unsync { agent } => {
            emit(&store.release_footprint(&agent_id(agent.as_deref())?)?)?;
        }
        Commands::Watch {
            workspace,
            base,
            interval_seconds,
            once,
        } => watch(
            &store,
            workspace.as_deref(),
            base.as_deref(),
            duration_from_seconds(interval_seconds)?,
            once,
        )?,
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
            let mut collected = store.collect_stale(Duration::from_secs(stale_seconds))?;
            // gc is routine, so it reports managed worktrees without ever reclaiming them:
            // preserving uncommitted agent work is a product invariant, not a default to
            // trade away for disk. `pidmesh worktrees --reclaim` is the explicit opt-in.
            let root = PathBuf::from(workspace_root(None)?);
            if let Ok(worktrees) = IdeManager::new(store.clone(), root).worktrees() {
                collected["worktrees"] = worktrees;
            }
            emit(&collected)?;
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
        Commands::Swarm {
            name_prefix,
            provider,
            workspace,
            workers,
            heartbeat_seconds,
            shutdown_grace_seconds,
            fail_fast,
            child_command,
        } => {
            return run_swarm(
                &store,
                &name_prefix,
                &provider,
                workspace.as_deref(),
                workers,
                duration_from_seconds(heartbeat_seconds)?,
                duration_from_seconds(shutdown_grace_seconds)?,
                fail_fast,
                &child_command,
            );
        }
        Commands::Dashboard { workspace, port } => {
            let workspace = PathBuf::from(workspace_root(workspace.as_deref())?);
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to start dashboard runtime")?;
            runtime.block_on(pidmesh::dashboard::serve(store, workspace, port))?;
            return Ok(0);
        }
        // Both are answered in run() before the store is ever opened. They are repeated here so
        // the match stays total without a panic, and so either path behaves identically.
        Commands::Completions { shell } => completions(shell),
        Commands::Quickstart => quickstart()?,
    }
    Ok(0)
}

/// Write a completion script for `shell` to stdout.
///
/// This is the one command that does not answer to the JSON contract, because what lands on
/// stdout is sourced by a shell: wrapping it in an object would make it unusable. Completions are
/// generated from the live `Cli` definition, so a subcommand or flag added above completes without
/// anyone remembering to update a checked-in script.
fn completions(shell: Shell) {
    clap_complete::generate(
        shell,
        &mut Cli::command(),
        "pidmesh",
        &mut std::io::stdout(),
    );
}

/// The three commands that take a fresh checkout to a guarded merge.
///
/// Twenty-eight subcommands do not tell a newcomer which three actually matter, and a fleet only
/// converges once every checkout is registered, something is observing them, and merges are gated.
/// Kept in step with `QUICKSTART_PROSE` by hand: one is read by a person, the other parsed.
fn quickstart_steps() -> Value {
    json!({
        "steps": [
            {
                "step": 1,
                "run_in": "every checkout, once",
                "command": "pidmesh join --name planner --provider codex",
                "why": "Registers the process and prints the agent id. Export it as \
                        PIDMESH_AGENT_ID in that shell and every later command knows who it \
                        speaks for."
            },
            {
                "step": 2,
                "run_in": "the repository, once",
                "command": "pidmesh watch",
                "why": "Sweeps every live checkout from git alone, so agents that never call \
                        pidmesh themselves are still covered. It also reaps crashed sessions."
            },
            {
                "step": 3,
                "run_in": "before every merge",
                "command": "pidmesh mergeable && git merge my-branch",
                "why": "Exits non-zero and names the blocker when merging would overwrite a \
                        live peer or withdraw a name another checkout still calls."
            }
        ],
        "then": [
            {"command": "pidmesh status", "answers": "who is alive and what they hold"},
            {"command": "pidmesh collisions", "answers": "which paths are contested right now"},
            {"command": "pidmesh doctor", "answers": "whether this really is one shared mesh"}
        ]
    })
}

/// The same three steps, written for a person rather than a parser.
const QUICKSTART_PROSE: &str = "\
PidMesh in three commands.

  1. Join — once per checkout
       pidmesh join --name planner --provider codex

     Export the id it prints as PIDMESH_AGENT_ID in that shell. Every later
     command then knows which agent it is speaking for.

  2. Watch — once per repository
       pidmesh watch

     One sweep observes every live checkout from git alone, so agents that
     never call pidmesh themselves are still covered, and crashed sessions
     stop holding what their peers are waiting on.

  3. Gate — before every merge
       pidmesh mergeable && git merge my-branch

     mergeable exits non-zero and names the blocker when merging would
     overwrite a live peer or withdraw a name another checkout still calls.

Then, whenever you want to know more:
  pidmesh status        who is alive and what they hold
  pidmesh collisions    which paths are contested right now
  pidmesh doctor        whether this really is one shared mesh";

/// Print the quickstart: prose at a terminal, the same steps as JSON everywhere else.
fn quickstart() -> Result<()> {
    if FORCE_JSON.load(Ordering::Relaxed) || !render::is_interactive() {
        return emit(&quickstart_steps());
    }
    println!("{QUICKSTART_PROSE}");
    Ok(())
}

struct SwarmWorker {
    agent_id: String,
    name: String,
    child: Child,
    exit_code: Option<i32>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_swarm(
    store: &MeshStore,
    name_prefix: &str,
    provider: &str,
    workspace: Option<&std::path::Path>,
    worker_count: u16,
    heartbeat_interval: Duration,
    shutdown_grace: Duration,
    fail_fast: bool,
    child_command: &[OsString],
) -> Result<u8> {
    if !(1..=64).contains(&worker_count) {
        return Err(anyhow!("workers must be between 1 and 64"));
    }
    if heartbeat_interval < Duration::from_millis(10) {
        return Err(anyhow!(
            "heartbeat interval must be at least 10 milliseconds"
        ));
    }
    if shutdown_grace > Duration::from_secs(60) {
        return Err(anyhow!("shutdown grace must be at most 60 seconds"));
    }

    let root = workspace_root(workspace)?;
    let swarm_id = format!("swarm-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let stopping = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal_stopping = Arc::clone(&stopping);
    let signal_interrupted = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        signal_interrupted.store(true, Ordering::Relaxed);
        signal_stopping.store(true, Ordering::Relaxed);
    })
    .context("failed to install swarm shutdown handler")?;

    let mut workers = Vec::with_capacity(usize::from(worker_count));
    for index in 0..worker_count {
        let name = format!("{name_prefix}-{index}");
        let agent_id = format!(
            "{name}-{swarm_id}-{}",
            &Uuid::new_v4().simple().to_string()[..8]
        );
        let registration = store.register_agent(
            &name,
            std::process::id(),
            workspace,
            provider,
            &["swarm-worker".to_owned()],
            Some(&agent_id),
        )?;
        let environment = json!({
            "PIDMESH_AGENT_ID": agent_id,
            "PIDMESH_AGENT_INDEX": index,
            "PIDMESH_AGENT_NAME": name,
            "PIDMESH_DB": store.path(),
            "PIDMESH_PROVIDER": provider,
            "PIDMESH_SWARM_ID": swarm_id,
            "PIDMESH_SWARM_SIZE": worker_count,
            "PIDMESH_WORKSPACE": root
        });
        let mut command = Command::new(&child_command[0]);
        command.args(&child_command[1..]);
        command.process_group(0);
        for (key, value) in environment.as_object().into_iter().flatten() {
            command.env(key, environment_value(value)?);
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                store.stop_agent(&agent_id)?;
                force_stop_workers(store, &mut workers);
                return Err(error).context("failed to start swarm worker");
            }
        };
        let child_pid = child.id();
        workers.push(SwarmWorker {
            agent_id: agent_id.clone(),
            name,
            child,
            exit_code: None,
        });
        let registered = store.update_agent_pid(&agent_id, child_pid).and_then(|_| {
            store.remember(
                &agent_id,
                &json!({
                    "command": child_command,
                    "environment": environment,
                    "registration": registration,
                    "swarm_id": swarm_id
                })
                .to_string(),
                "session",
                Some("process.start"),
                0.2,
            )
        });
        if let Err(error) = registered {
            force_stop_workers(store, &mut workers);
            return Err(error).context("failed to register spawned swarm worker");
        }
    }

    let mut remaining = workers.len();
    let mut next_heartbeat = Instant::now() + heartbeat_interval;
    let mut shutdown_deadline = None;
    let mut first_failure = None;
    while remaining > 0 {
        if stopping.load(Ordering::Relaxed) && shutdown_deadline.is_none() {
            request_worker_shutdown(&mut workers);
            shutdown_deadline = Some(Instant::now() + shutdown_grace);
        }
        if shutdown_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            force_kill_running(&mut workers);
        }
        for worker in &mut workers {
            if worker.exit_code.is_some() {
                continue;
            }
            if let Some(status) = worker
                .child
                .try_wait()
                .context("failed to poll swarm worker")?
            {
                let code = status.code().unwrap_or(1);
                worker.exit_code = Some(code);
                remaining -= 1;
                store.stop_agent(&worker.agent_id)?;
                if code != 0 && first_failure.is_none() {
                    first_failure = Some(code);
                    if fail_fast {
                        stopping.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        if Instant::now() >= next_heartbeat {
            for worker in workers.iter().filter(|worker| worker.exit_code.is_none()) {
                store.heartbeat(&worker.agent_id)?;
            }
            next_heartbeat = Instant::now() + heartbeat_interval;
        }
        if remaining > 0 {
            thread::sleep(Duration::from_millis(25));
        }
    }

    emit(&json!({
        "event": "swarm.stopped",
        "interrupted": interrupted.load(Ordering::Relaxed),
        "swarm_id": swarm_id,
        "workers": workers.iter().map(|worker| json!({
            "agent_id": worker.agent_id,
            "exit_code": worker.exit_code,
            "name": worker.name,
            "pid": worker.child.id()
        })).collect::<Vec<_>>()
    }))?;
    if interrupted.load(Ordering::Relaxed) {
        Ok(130)
    } else {
        Ok(first_failure
            .and_then(|code| code.try_into().ok())
            .unwrap_or(0))
    }
}

fn environment_value(value: &Value) -> Result<String> {
    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => Err(anyhow!("unsupported child environment value")),
    }
}

fn request_worker_shutdown(workers: &mut [SwarmWorker]) {
    for worker in workers
        .iter_mut()
        .filter(|worker| worker.exit_code.is_none())
    {
        if let Ok(pid) = i32::try_from(worker.child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

fn force_kill_running(workers: &mut [SwarmWorker]) {
    for worker in workers
        .iter_mut()
        .filter(|worker| worker.exit_code.is_none())
    {
        if let Ok(pid) = i32::try_from(worker.child.id()) {
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

fn force_stop_workers(store: &MeshStore, workers: &mut [SwarmWorker]) {
    force_kill_running(workers);
    for worker in workers {
        let _ = worker.child.wait();
        let _ = store.stop_agent(&worker.agent_id);
    }
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
        workspace,
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

/// Observe every live checkout in a workspace and publish footprints on the fleet's behalf.
///
/// This is what makes convergence autonomic. `pidmesh sync` still has to be invoked by somebody,
/// so a fleet whose agents never call it stays invisible to each other. A watcher needs no
/// cooperation at all: registration already recorded every checkout.
fn watch(
    store: &MeshStore,
    workspace: Option<&Path>,
    base: Option<&str>,
    interval: Duration,
    once: bool,
) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || signal_running.store(false, Ordering::Relaxed))
        .context("failed to install interrupt handler")?;
    loop {
        present(Shape::Watch, &watch_pass(store, workspace, base)?)?;
        if once {
            break;
        }
        // Sleep in slices so an interrupt is observed promptly rather than at the next tick.
        let deadline = Instant::now() + interval;
        while running.load(Ordering::Relaxed) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100).min(interval));
        }
        if !running.load(Ordering::Relaxed) {
            break;
        }
    }
    Ok(())
}

/// One observation sweep over every distinct live checkout.
fn watch_pass(store: &MeshStore, workspace: Option<&Path>, base: Option<&str>) -> Result<Value> {
    // Several sessions can share one checkout, so scan each checkout once and reuse the result.
    let live = store.live_checkouts(workspace)?;
    let stale = live.stale;
    let mut grouped: BTreeMap<PathBuf, Vec<AgentCheckout>> = BTreeMap::new();
    for checkout in live.checkouts {
        grouped
            .entry(checkout.checkout_path.clone())
            .or_default()
            .push(checkout);
    }
    // Scanning is git and file I/O with no shared state, so checkouts are observed in parallel.
    // Publishing stays serial: it is one short transaction each and the store holds one
    // connection, so contending for it would only add lock traffic.
    let checkouts: Vec<(&PathBuf, &Vec<AgentCheckout>)> = grouped.iter().collect();
    let cursor = AtomicUsize::new(0);
    let scans: Mutex<Vec<(usize, Result<WorktreeScan>)>> = Mutex::new(Vec::new());
    let workers = thread::available_parallelism()
        .map_or(4, NonZeroUsize::get)
        .min(checkouts.len().max(1));
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((path, _)) = checkouts.get(index) else {
                        return;
                    };
                    let scanned = scan_worktree(path, base);
                    if let Ok(mut guard) = scans.lock() {
                        guard.push((index, scanned));
                    }
                }
            });
        }
    });

    let mut scanned = scans.into_inner().unwrap_or_default();
    scanned.sort_by_key(|(index, _)| *index);

    let mut published = 0usize;
    let mut skipped: Vec<Value> = Vec::new();
    let mut witness = None;
    let mut batch: Vec<(String, WorktreeScan)> = Vec::new();
    for (index, result) in scanned {
        let Some((path, agents)) = checkouts.get(index) else {
            continue;
        };
        let scan = match result {
            Ok(scan) => scan,
            Err(error) => {
                // A checkout can be removed or rewritten underneath us; that is not fatal.
                skipped.push(json!({
                    "checkout": path.to_string_lossy(),
                    "error": format!("{error:#}")
                }));
                continue;
            }
        };
        for agent in *agents {
            witness.get_or_insert_with(|| agent.agent_id.clone());
            batch.push((agent.agent_id.clone(), scan.clone()));
            published += 1;
        }
    }
    // One transaction for the whole sweep: publishing agent by agent recomputes the collision
    // set twice per agent, which is most of a tick once a fleet gets large.
    // A watcher runs for hours. A publish that fails — a locked database, a vanished agent — is
    // one bad tick, not a reason to stop observing the fleet, so it is reported like a failed
    // scan rather than propagated out of the loop.
    if let Err(error) = store.publish_footprints(&batch) {
        skipped.push(json!({"publish": "failed", "error": format!("{error:#}")}));
        published = 0;
    }

    // Nothing else reaps dead sessions, and a crashed agent holds its claims, its reservations
    // and its share of every contested path until something does — which blocks its peers from
    // merging indefinitely. A watcher is the one process reliably running, so it reaps.
    let reaped = store.collect_stale(Duration::from_secs(STALE_AFTER_SECONDS))?;
    let collisions = match witness {
        Some(agent_id) => store.collisions(&agent_id)?,
        None => json!({"collisions": [], "collision_count": 0}),
    };
    Ok(json!({
        "checkouts": grouped.len(),
        "published": published,
        "stale_agents": stale,
        "reaped": reaped,
        "skipped": skipped,
        "collision_count": collisions["collision_count"],
        "collisions": collisions["collisions"]
    }))
}

/// How long a session may be quiet before a sweep checks whether its process is still alive.
///
/// Liveness is confirmed by PID before anything is marked dead, so this only bounds how long a
/// genuinely crashed agent keeps blocking its peers.
const STALE_AFTER_SECONDS: u64 = 30;

/// The process to treat as the agent when `--pid` is not given.
///
/// This CLI exits the moment it registers, so recording its own pid marks the agent dead
/// immediately: `watch` skips checkouts whose process is gone, and a fleet registered with a plain
/// `pidmesh join` was reported as an empty, healthy workspace. The parent is the shell or agent
/// that will actually keep working in the checkout.
fn calling_pid() -> u32 {
    let parent = nix::unistd::getppid();
    u32::try_from(parent.as_raw()).unwrap_or_else(|_| std::process::id())
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

/// Print for whoever is reading: prose at a terminal, JSON everywhere else.
///
/// The JSON contract predates this and agents depend on it, so a pipe or a redirect always gets
/// JSON and only a human at a prompt sees the rendered form.
fn present(shape: Shape, value: &Value) -> Result<()> {
    if FORCE_JSON.load(Ordering::Relaxed) || !render::is_interactive() {
        return emit(value);
    }
    match render::render(shape, value) {
        Some(text) => {
            println!("{text}");
            Ok(())
        }
        None => emit(value),
    }
}
