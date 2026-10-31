use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::converge::{integration_head, scan_worktree};
use crate::store::{INTEGRATION_TASK_KEY, MeshStore, Refusal, checkout_directory};

#[derive(Clone)]
pub struct PidMeshMcp {
    store: MeshStore,
    agent_id: String,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberParams {
    content: String,
    #[serde(default = "default_kind")]
    kind: String,
    key: Option<String>,
    #[serde(default = "default_importance")]
    importance: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecallParams {
    query: String,
    #[serde(default = "default_recall_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendParams {
    message: String,
    #[serde(default = "broadcast")]
    recipient: String,
    correlation_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InboxParams {
    #[serde(default)]
    acknowledge: bool,
    #[serde(default = "default_inbox_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SyncParams {
    /// Integration branch to measure against; defaults to main, then master.
    base: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MergeableParams {
    /// Integration branch to measure against; defaults to main, then master.
    base: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClaimParams {
    task: String,
    #[serde(default = "default_lease")]
    lease_seconds: u64,
    detail: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReleaseParams {
    task: String,
}

/// The integration lease has a fixed task key, so the only thing a caller chooses is how long to
/// hold it.
///
/// Reusing `ClaimParams` here advertised a required `task` the tool then threw away, which made
/// the obvious call — `acquire_integration_lease` with no arguments — fail to deserialize.
#[derive(Debug, Deserialize, JsonSchema)]
struct IntegrationLeaseParams {
    #[serde(default = "default_lease")]
    lease_seconds: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReserveResourcesParams {
    resources: Vec<String>,
    task: Option<String>,
    detail: Option<String>,
    #[serde(default = "default_lease")]
    lease_seconds: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReleaseResourcesParams {
    resources: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EventParams {
    #[serde(default)]
    after_sequence: i64,
    #[serde(default = "default_event_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitParams {
    #[serde(default)]
    after_sequence: i64,
    #[serde(default = "default_wait")]
    timeout_seconds: f64,
    #[serde(default = "default_event_limit")]
    limit: u32,
}

#[tool_router]
impl PidMeshMcp {
    pub fn from_environment() -> Result<Self> {
        let store = MeshStore::from_environment()?;
        let name = env::var("PIDMESH_AGENT_NAME").unwrap_or_else(|_| "mcp-agent".to_owned());
        let provider = env::var("PIDMESH_PROVIDER").unwrap_or_else(|_| "mcp".to_owned());
        let workspace = env::var_os("PIDMESH_WORKSPACE").map(PathBuf::from);
        let capabilities = [
            "memory",
            "messaging",
            "claims",
            "resources",
            "events",
            "wait",
            "convergence",
        ]
        .map(str::to_owned);
        let managed_agent = env::var("PIDMESH_AGENT_ID").ok();
        let agent_id = if let Some(agent_id) = managed_agent {
            if !store.heartbeat(&agent_id)? {
                store.register_agent(
                    &name,
                    std::process::id(),
                    workspace.as_deref(),
                    &provider,
                    &capabilities,
                    Some(&agent_id),
                )?;
            }
            agent_id
        } else {
            store
                .register_agent(
                    &name,
                    std::process::id(),
                    workspace.as_deref(),
                    &provider,
                    &capabilities,
                    None,
                )?
                .get("agent_id")
                .and_then(Value::as_str)
                .context("registration did not return an agent id")?
                .to_owned()
        };
        Ok(Self {
            store,
            agent_id,
            tool_router: Self::tool_router(),
        })
    }

    #[must_use]
    pub fn store(&self) -> MeshStore {
        self.store.clone()
    }

    #[must_use]
    pub fn agent_id(&self) -> String {
        self.agent_id.clone()
    }

    #[tool(description = "List local agents, process liveness, task claims, and resource leases")]
    fn mesh_status(&self) -> Result<CallToolResult, McpError> {
        let result = self
            .store
            .heartbeat(&self.agent_id)
            .and_then(|_| self.store.status(Some(&self.agent_id), None));
        tool_result(result)
    }

    #[tool(description = "Append a durable memory visible to agents in this workspace")]
    fn remember(
        &self,
        Parameters(parameters): Parameters<RememberParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.remember(
            &self.agent_id,
            &parameters.content,
            &parameters.kind,
            parameters.key.as_deref(),
            parameters.importance,
        ))
    }

    #[tool(description = "Search durable memories written by any agent in this workspace")]
    fn recall(
        &self,
        Parameters(parameters): Parameters<RecallParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .recall(&self.agent_id, &parameters.query, parameters.limit),
        )
    }

    #[tool(description = "Send a message to an active agent by name/id or broadcast with '*'")]
    fn send(
        &self,
        Parameters(parameters): Parameters<SendParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.send(
            &self.agent_id,
            &parameters.message,
            &parameters.recipient,
            parameters.correlation_id.as_deref(),
        ))
    }

    #[tool(description = "Read unread direct and broadcast messages for this agent")]
    fn inbox(
        &self,
        Parameters(parameters): Parameters<InboxParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .store
            .inbox(&self.agent_id, true, parameters.limit)
            .and_then(|messages| {
                let acknowledged = if parameters.acknowledge {
                    let ids = messages
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|message| message["id"].as_i64())
                        .collect::<Vec<_>>();
                    self.store.acknowledge(&self.agent_id, &ids)?
                } else {
                    0
                };
                Ok(json!({"messages": messages, "acknowledged": acknowledged}))
            });
        tool_result(result)
    }

    #[tool(description = "Atomically acquire or renew a task lease")]
    fn claim(
        &self,
        Parameters(parameters): Parameters<ClaimParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.claim(
            &self.agent_id,
            &parameters.task,
            parameters.lease_seconds,
            parameters.detail.as_deref(),
        ))
    }

    #[tool(description = "Release a task lease owned by this agent")]
    fn release(
        &self,
        Parameters(parameters): Parameters<ReleaseParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .release(&self.agent_id, &parameters.task)
                .map(|released| json!({"released": released})),
        )
    }

    #[tool(description = "Atomically reserve paths or other resources before using them")]
    fn reserve_resources(
        &self,
        Parameters(parameters): Parameters<ReserveResourcesParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.reserve_resources(
            &self.agent_id,
            &parameters.resources,
            parameters.task.as_deref(),
            parameters.detail.as_deref(),
            parameters.lease_seconds,
        ))
    }

    #[tool(description = "Release resource leases owned by this agent")]
    fn release_resources(
        &self,
        Parameters(parameters): Parameters<ReleaseResourcesParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .release_resources(&self.agent_id, &parameters.resources)
                .map(|released| json!({"released": released})),
        )
    }

    #[tool(description = "Read ordered coordination events after a sequence number")]
    fn event_stream(
        &self,
        Parameters(parameters): Parameters<EventParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .events(&self.agent_id, parameters.after_sequence, parameters.limit),
        )
    }

    #[tool(description = "Wait up to 60 seconds for the next coordination event")]
    async fn wait_for_events(
        &self,
        Parameters(parameters): Parameters<WaitParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.store.clone();
        let agent_id = self.agent_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            // `Duration::from_secs_f64` panics on a negative, non-finite, or too-large value,
            // and a panic inside a tool call reaches the calling agent as an opaque
            // "task N panicked" internal error it cannot act on. Converting fallibly turns
            // every one of those into an ordinary tool failure naming the argument at fault.
            let timeout =
                Duration::try_from_secs_f64(parameters.timeout_seconds).map_err(|_| {
                    anyhow::anyhow!("timeout_seconds must be a finite non-negative number")
                })?;
            store.wait_for_events(
                &agent_id,
                parameters.after_sequence,
                timeout,
                Duration::from_millis(100),
                parameters.limit,
            )
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        tool_result(result)
    }

    #[tool(
        description = "Publish what this checkout actually changed and report paths contested by other agents"
    )]
    fn sync_footprint(
        &self,
        Parameters(parameters): Parameters<SyncParams>,
    ) -> Result<CallToolResult, McpError> {
        // The checkout, not the mesh root: this server runs inside an agent that was started
        // in its own worktree and pointed at the repository the mesh is keyed by.
        let result = checkout_directory(None).and_then(|directory| {
            let scan = scan_worktree(&directory, parameters.base.as_deref())?;
            let mut published = self.store.publish_footprint(&self.agent_id, &scan)?;
            // Named in the answer so an agent can see which directory it just published, and
            // so a wrong one is visible rather than silently plausible.
            published["checkout_path"] = json!(directory.to_string_lossy());
            Ok(published)
        });
        tool_result(result)
    }

    #[tool(description = "Report paths contested by more than one agent's observed footprint")]
    fn collisions(&self) -> Result<CallToolResult, McpError> {
        tool_result(self.store.collisions(&self.agent_id))
    }

    #[tool(description = "Withdraw this agent's footprint when its work is merged or abandoned")]
    fn release_footprint(&self) -> Result<CallToolResult, McpError> {
        tool_result(self.store.release_footprint(&self.agent_id))
    }

    #[tool(
        description = "Check whether this checkout can merge without breaking another agent's work"
    )]
    fn mergeable(
        &self,
        Parameters(parameters): Parameters<MergeableParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = checkout_directory(None)
            .and_then(|directory| integration_head(&directory, parameters.base.as_deref()))
            .and_then(|(_, head)| self.store.merge_readiness(&self.agent_id, &head));
        tool_result(result)
    }

    #[tool(description = "Take the workspace-wide integration lease so merges do not race")]
    fn acquire_integration_lease(
        &self,
        Parameters(parameters): Parameters<IntegrationLeaseParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.claim(
            &self.agent_id,
            INTEGRATION_TASK_KEY,
            parameters.lease_seconds,
            Some("merging into the integration branch"),
        ))
    }

    #[tool(description = "Release the workspace-wide integration lease after merging")]
    fn release_integration_lease(&self) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .release(&self.agent_id, INTEGRATION_TASK_KEY)
                .map(|released| json!({"released": released})),
        )
    }
}

#[tool_handler(
    name = "PidMesh",
    version = "1.5.0",
    instructions = "Check status and inbox, claim a task, reserve intended paths before editing, and record decisions as memories. Call sync_footprint after each round of edits so other agents can see what this checkout actually changed, and read collisions before continuing work on a contested path. Before merging, call mergeable and resolve every blocker, then hold the integration lease for the merge itself."
)]
impl ServerHandler for PidMeshMcp {}

/// Answer a tool call, saying which *kind* of failure a failure was.
///
/// An agent reading only `error` has the same problem the dashboard had: it cannot tell "you
/// addressed nobody", which it should fix and retry differently, from "the database broke",
/// which retrying the same way is the only sensible response to. `kind` is that distinction,
/// taken from the store's type rather than from the text of the message.
///
/// The message itself is unchanged, including for a fault. Unlike the dashboard this is a stdio
/// transport inside the agent's own process, with no remote audience to disclose to, and the
/// detail in a failure here — which base ref git would not read, which directory is not a
/// repository — is what makes a tool error actionable.
fn tool_result(result: Result<Value>) -> Result<CallToolResult, McpError> {
    Ok(match result {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::structured_error(json!({
            "error": format!("{error:#}"),
            "kind": Refusal::of(&error).map_or("fault", |refusal| refusal.kind().as_str()),
        })),
    })
}

fn default_kind() -> String {
    "note".to_owned()
}

const fn default_importance() -> f64 {
    0.5
}

const fn default_recall_limit() -> u32 {
    10
}

const fn default_inbox_limit() -> u32 {
    50
}

const fn default_event_limit() -> u32 {
    100
}

const fn default_lease() -> u64 {
    300
}

const fn default_wait() -> f64 {
    30.0
}

fn broadcast() -> String {
    "*".to_owned()
}
