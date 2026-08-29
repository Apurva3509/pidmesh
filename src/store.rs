use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS workspaces (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    pid INTEGER NOT NULL,
    parent_pid INTEGER,
    provider TEXT NOT NULL,
    capabilities_json TEXT NOT NULL DEFAULT '[]',
    started_at INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    stopped_at INTEGER,
    status TEXT NOT NULL DEFAULT 'running'
);

CREATE INDEX IF NOT EXISTS agents_workspace_status
ON agents(workspace_id, status, heartbeat_at DESC);

CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    key TEXT,
    content TEXT NOT NULL,
    importance REAL NOT NULL DEFAULT 0.5,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS memories_workspace_created
ON memories(workspace_id, created_at DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    content,
    key,
    kind,
    content='memories',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS memories_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memory_fts(rowid, content, key, kind)
    VALUES (new.id, new.content, coalesce(new.key, ''), new.kind);
END;

CREATE TRIGGER IF NOT EXISTS memories_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, content, key, kind)
    VALUES ('delete', old.id, old.content, coalesce(old.key, ''), old.kind);
END;

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sender_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    recipient_id TEXT REFERENCES agents(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    correlation_id TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS messages_workspace_created
ON messages(workspace_id, created_at DESC);

CREATE TABLE IF NOT EXISTS message_receipts (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    acknowledged_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, agent_id)
);

CREATE TABLE IF NOT EXISTS claims (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    task_key TEXT NOT NULL,
    detail TEXT,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    claimed_at INTEGER NOT NULL,
    lease_expires_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, task_key)
);

CREATE INDEX IF NOT EXISTS claims_lease
ON claims(workspace_id, lease_expires_at);

CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL,
    event_type TEXT NOT NULL,
    subject TEXT,
    data_json TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS events_workspace_sequence
ON events(workspace_id, sequence DESC);
";

#[derive(Clone, Debug)]
pub struct MeshStore {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
}

#[derive(Debug)]
struct AgentRow {
    workspace_id: String,
    started_at: i64,
}

impl MeshStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            let created = !parent.exists();
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            if created {
                restrict_directory(parent)?;
            }
        }
        let connection = Self::open_connection(&path)?;
        let store = Self {
            path,
            connection: Arc::new(Mutex::new(connection)),
        };
        store.initialize()?;
        store.restrict_permissions()?;
        Ok(store)
    }

    pub fn from_environment() -> Result<Self> {
        Self::new(default_database_path()?)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn register_agent(
        &self,
        name: &str,
        pid: u32,
        root: Option<&Path>,
        provider: &str,
        capabilities: &[String],
        agent_id: Option<&str>,
    ) -> Result<Value> {
        let root = workspace_root(root)?;
        let identifier = agent_id.map_or_else(
            || format!("{name}-{pid}-{}", &Uuid::new_v4().simple().to_string()[..8]),
            ToOwned::to_owned,
        );
        let now = now_ms()?;
        self.write(|transaction| {
            let workspace_id = Self::ensure_workspace(transaction, &root)?;
            transaction.execute(
                "INSERT INTO agents(
                    id, workspace_id, name, pid, parent_pid, provider,
                    capabilities_json, started_at, heartbeat_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    identifier,
                    workspace_id,
                    name,
                    i64::from(pid),
                    parent_pid(),
                    provider,
                    serde_json::to_string(capabilities)?,
                    now,
                    now
                ],
            )?;
            Self::event(
                transaction,
                &workspace_id,
                Some(&identifier),
                "agent.joined",
                Some(&identifier),
                &json!({}),
            )?;
            Ok(json!({
                "agent_id": identifier,
                "name": name,
                "pid": pid,
                "provider": provider,
                "workspace": root,
                "workspace_id": workspace_id
            }))
        })
    }

    pub fn heartbeat(&self, agent_id: &str) -> Result<bool> {
        let now = now_ms()?;
        self.write(|transaction| {
            let changed = transaction.execute(
                "UPDATE agents SET heartbeat_at = ?, status = 'running', stopped_at = NULL
                 WHERE id = ?",
                params![now, agent_id],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn update_agent_pid(&self, agent_id: &str, pid: u32) -> Result<bool> {
        let now = now_ms()?;
        self.write(|transaction| {
            let changed = transaction.execute(
                "UPDATE agents SET pid = ?, heartbeat_at = ? WHERE id = ?",
                params![i64::from(pid), now, agent_id],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn stop_agent(&self, agent_id: &str) -> Result<bool> {
        let now = now_ms()?;
        self.write(|transaction| {
            let workspace_id: Option<String> = transaction
                .query_row(
                    "SELECT workspace_id FROM agents WHERE id = ?",
                    [agent_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(workspace_id) = workspace_id else {
                return Ok(false);
            };
            transaction.execute(
                "UPDATE agents SET status = 'stopped', stopped_at = ? WHERE id = ?",
                params![now, agent_id],
            )?;
            transaction.execute("DELETE FROM claims WHERE agent_id = ?", [agent_id])?;
            Self::event(
                transaction,
                &workspace_id,
                Some(agent_id),
                "agent.stopped",
                Some(agent_id),
                &json!({}),
            )?;
            Ok(true)
        })
    }

    pub fn remember(
        &self,
        agent_id: &str,
        content: &str,
        kind: &str,
        key: Option<&str>,
        importance: f64,
    ) -> Result<Value> {
        if !(0.0..=1.0).contains(&importance) {
            bail!("importance must be between 0 and 1");
        }
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            transaction.execute(
                "INSERT INTO memories(
                    workspace_id, agent_id, kind, key, content, importance, created_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    agent.workspace_id,
                    agent_id,
                    kind,
                    key,
                    content,
                    importance,
                    now_ms()?
                ],
            )?;
            let memory_id = transaction.last_insert_rowid();
            Self::event(
                transaction,
                &agent.workspace_id,
                Some(agent_id),
                "memory.created",
                Some(&memory_id.to_string()),
                &json!({}),
            )?;
            Ok(json!({"memory_id": memory_id, "kind": kind, "key": key}))
        })
    }

    pub fn recall(&self, agent_id: &str, query: &str, limit: u32) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let terms: Vec<_> = query
                .split(|character: char| {
                    !(character.is_alphanumeric() || matches!(character, '.' | '-' | '_'))
                })
                .filter(|term| !term.is_empty())
                .collect();
            let mut results = Vec::new();
            if terms.is_empty() {
                let mut statement = connection.prepare(
                    "SELECT m.id, m.agent_id, m.kind, m.key, m.content, m.importance,
                            m.created_at, a.name
                     FROM memories m LEFT JOIN agents a ON a.id = m.agent_id
                     WHERE m.workspace_id = ?
                     ORDER BY m.importance DESC, m.created_at DESC LIMIT ?",
                )?;
                let rows = statement.query_map(params![agent.workspace_id, limit], memory_json)?;
                for row in rows {
                    results.push(row?);
                }
            } else {
                let expression = terms
                    .iter()
                    .map(|term| format!("\"{term}\"*"))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let mut statement = connection.prepare(
                    "SELECT m.id, m.agent_id, m.kind, m.key, m.content, m.importance,
                            m.created_at, a.name
                     FROM memory_fts
                     JOIN memories m ON m.id = memory_fts.rowid
                     LEFT JOIN agents a ON a.id = m.agent_id
                     WHERE memory_fts MATCH ? AND m.workspace_id = ?
                     ORDER BY bm25(memory_fts), m.importance DESC, m.created_at DESC LIMIT ?",
                )?;
                let rows = statement
                    .query_map(params![expression, agent.workspace_id, limit], memory_json)?;
                for row in rows {
                    results.push(row?);
                }
            }
            Ok(Value::Array(results))
        })
    }

    pub fn send(
        &self,
        agent_id: &str,
        body: &str,
        recipient: &str,
        correlation_id: Option<&str>,
    ) -> Result<Value> {
        self.write(|transaction| {
            let sender = Self::agent_row(transaction, agent_id)?;
            let recipient_id = if recipient == "*" {
                None
            } else {
                transaction
                    .query_row(
                        "SELECT id FROM agents
                         WHERE workspace_id = ? AND (id = ? OR name = ?) AND status = 'running'
                         ORDER BY heartbeat_at DESC LIMIT 1",
                        params![sender.workspace_id, recipient, recipient],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow!("active recipient not found: {recipient}"))?
                    .into()
            };
            transaction.execute(
                "INSERT INTO messages(
                    workspace_id, sender_id, recipient_id, body, correlation_id, created_at
                 ) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    sender.workspace_id,
                    agent_id,
                    recipient_id,
                    body,
                    correlation_id,
                    now_ms()?
                ],
            )?;
            let message_id = transaction.last_insert_rowid();
            let routed_to = recipient_id.as_deref().unwrap_or("*");
            Self::event(
                transaction,
                &sender.workspace_id,
                Some(agent_id),
                "message.sent",
                Some(&message_id.to_string()),
                &json!({"recipient": routed_to}),
            )?;
            Ok(json!({"message_id": message_id, "recipient": routed_to}))
        })
    }

    pub fn inbox(&self, agent_id: &str, unread_only: bool, limit: u32) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let unread = if unread_only {
                "AND r.message_id IS NULL"
            } else {
                ""
            };
            let sql = format!(
                "SELECT m.id, m.body, m.correlation_id, m.created_at, m.recipient_id,
                        s.id, s.name, r.acknowledged_at
                 FROM messages m
                 JOIN agents s ON s.id = m.sender_id
                 LEFT JOIN message_receipts r
                   ON r.message_id = m.id AND r.agent_id = ?
                 WHERE m.workspace_id = ? AND m.sender_id != ?
                   AND (m.recipient_id = ? OR (m.recipient_id IS NULL AND m.created_at >= ?))
                   {unread}
                 ORDER BY m.created_at, m.id LIMIT ?"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    agent_id,
                    agent.workspace_id,
                    agent_id,
                    agent_id,
                    agent.started_at,
                    limit
                ],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "body": row.get::<_, String>(1)?,
                        "correlation_id": row.get::<_, Option<String>>(2)?,
                        "created_at": row.get::<_, i64>(3)?,
                        "recipient_id": row.get::<_, Option<String>>(4)?,
                        "sender_id": row.get::<_, String>(5)?,
                        "sender_name": row.get::<_, String>(6)?,
                        "acknowledged_at": row.get::<_, Option<i64>>(7)?
                    }))
                },
            )?;
            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(Value::Array(messages))
        })
    }

    pub fn acknowledge(&self, agent_id: &str, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let now = now_ms()?;
            let mut acknowledged = 0;
            for message_id in message_ids {
                let visible: bool = transaction.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM messages
                        WHERE workspace_id = ? AND id = ?
                          AND (recipient_id = ? OR recipient_id IS NULL)
                     )",
                    params![agent.workspace_id, message_id, agent_id],
                    |row| row.get(0),
                )?;
                if visible {
                    transaction.execute(
                        "INSERT OR IGNORE INTO message_receipts(
                            message_id, agent_id, acknowledged_at
                         ) VALUES (?, ?, ?)",
                        params![message_id, agent_id, now],
                    )?;
                    acknowledged += 1;
                }
            }
            Ok(acknowledged)
        })
    }

    pub fn claim(
        &self,
        agent_id: &str,
        task_key: &str,
        lease_seconds: u64,
        detail: Option<&str>,
    ) -> Result<Value> {
        if lease_seconds == 0 {
            bail!("lease_seconds must be positive");
        }
        let now = now_ms()?;
        let lease_expires_at = now
            .checked_add(i64::try_from(lease_seconds)?.saturating_mul(1000))
            .ok_or_else(|| anyhow!("lease duration is too large"))?;
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            transaction.execute(
                "INSERT INTO claims(
                    workspace_id, task_key, detail, agent_id,
                    claimed_at, lease_expires_at, updated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(workspace_id, task_key) DO UPDATE SET
                    detail = excluded.detail,
                    agent_id = excluded.agent_id,
                    claimed_at = excluded.claimed_at,
                    lease_expires_at = excluded.lease_expires_at,
                    updated_at = excluded.updated_at
                 WHERE claims.lease_expires_at <= ? OR claims.agent_id = excluded.agent_id",
                params![
                    agent.workspace_id,
                    task_key,
                    detail,
                    agent_id,
                    now,
                    lease_expires_at,
                    now,
                    now
                ],
            )?;
            let claim = transaction.query_row(
                "SELECT c.task_key, c.detail, c.agent_id, c.claimed_at,
                        c.lease_expires_at, c.updated_at, a.name
                 FROM claims c JOIN agents a ON a.id = c.agent_id
                 WHERE c.workspace_id = ? AND c.task_key = ?",
                params![agent.workspace_id, task_key],
                |row| {
                    Ok(json!({
                        "task_key": row.get::<_, String>(0)?,
                        "detail": row.get::<_, Option<String>>(1)?,
                        "agent_id": row.get::<_, String>(2)?,
                        "claimed_at": row.get::<_, i64>(3)?,
                        "lease_expires_at": row.get::<_, i64>(4)?,
                        "updated_at": row.get::<_, i64>(5)?,
                        "agent_name": row.get::<_, String>(6)?
                    }))
                },
            )?;
            let acquired = claim["agent_id"] == agent_id;
            if acquired {
                Self::event(
                    transaction,
                    &agent.workspace_id,
                    Some(agent_id),
                    "task.claimed",
                    Some(task_key),
                    &json!({}),
                )?;
            }
            let mut claim = claim;
            claim["acquired"] = Value::Bool(acquired);
            Ok(claim)
        })
    }

    pub fn release(&self, agent_id: &str, task_key: &str) -> Result<bool> {
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let changed = transaction.execute(
                "DELETE FROM claims WHERE workspace_id = ? AND task_key = ? AND agent_id = ?",
                params![agent.workspace_id, task_key, agent_id],
            )?;
            if changed == 1 {
                Self::event(
                    transaction,
                    &agent.workspace_id,
                    Some(agent_id),
                    "task.released",
                    Some(task_key),
                    &json!({}),
                )?;
            }
            Ok(changed == 1)
        })
    }

    pub fn status(&self, agent_id: Option<&str>, root: Option<&Path>) -> Result<Value> {
        self.read(|connection| {
            let workspace = if let Some(agent_id) = agent_id {
                connection
                    .query_row(
                        "SELECT w.id, w.root FROM workspaces w
                         JOIN agents a ON a.workspace_id = w.id WHERE a.id = ?",
                        [agent_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?
            } else {
                let root = workspace_root(root)?;
                let found = connection
                    .query_row(
                        "SELECT id, root FROM workspaces WHERE root = ?",
                        [&root],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?;
                let Some(found) = found else {
                    return Ok(json!({"workspace": root, "agents": [], "claims": []}));
                };
                found
            };
            let mut agents_statement = connection.prepare(
                "SELECT id, name, pid, parent_pid, provider, capabilities_json,
                        started_at, heartbeat_at, stopped_at, status
                 FROM agents WHERE workspace_id = ?
                 ORDER BY status = 'running' DESC, heartbeat_at DESC",
            )?;
            let current_time = now_ms()?;
            let agent_rows = agents_statement.query_map([&workspace.0], |row| {
                let pid = row.get::<_, u32>(2)?;
                let capabilities: String = row.get(5)?;
                let heartbeat_at = row.get::<_, i64>(7)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "pid": pid,
                    "parent_pid": row.get::<_, Option<i64>>(3)?,
                    "provider": row.get::<_, String>(4)?,
                    "capabilities": serde_json::from_str::<Value>(&capabilities)
                        .unwrap_or_else(|_| json!([])),
                    "started_at": row.get::<_, i64>(6)?,
                    "heartbeat_at": heartbeat_at,
                    "stopped_at": row.get::<_, Option<i64>>(8)?,
                    "status": row.get::<_, String>(9)?,
                    "pid_alive": process_is_alive(pid),
                    "heartbeat_age_ms": current_time.saturating_sub(heartbeat_at)
                }))
            })?;
            let mut agents = Vec::new();
            for row in agent_rows {
                agents.push(row?);
            }
            let mut claims_statement = connection.prepare(
                "SELECT c.task_key, c.detail, c.agent_id, c.claimed_at,
                        c.lease_expires_at, c.updated_at, a.name
                 FROM claims c JOIN agents a ON a.id = c.agent_id
                 WHERE c.workspace_id = ? ORDER BY c.task_key",
            )?;
            let claim_rows = claims_statement.query_map([&workspace.0], |row| {
                Ok(json!({
                    "task_key": row.get::<_, String>(0)?,
                    "detail": row.get::<_, Option<String>>(1)?,
                    "agent_id": row.get::<_, String>(2)?,
                    "claimed_at": row.get::<_, i64>(3)?,
                    "lease_expires_at": row.get::<_, i64>(4)?,
                    "updated_at": row.get::<_, i64>(5)?,
                    "agent_name": row.get::<_, String>(6)?
                }))
            })?;
            let mut claims = Vec::new();
            for row in claim_rows {
                claims.push(row?);
            }
            Ok(json!({
                "workspace": workspace.1,
                "workspace_id": workspace.0,
                "agents": agents,
                "claims": claims
            }))
        })
    }

    pub fn events(&self, agent_id: &str, after: i64, limit: u32) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let mut statement = connection.prepare(
                "SELECT e.sequence, e.agent_id, e.event_type, e.subject,
                        e.data_json, e.created_at, a.name
                 FROM events e LEFT JOIN agents a ON a.id = e.agent_id
                 WHERE e.workspace_id = ? AND e.sequence > ?
                 ORDER BY e.sequence LIMIT ?",
            )?;
            let rows = statement.query_map(params![agent.workspace_id, after, limit], |row| {
                let data: String = row.get(4)?;
                Ok(json!({
                    "sequence": row.get::<_, i64>(0)?,
                    "agent_id": row.get::<_, Option<String>>(1)?,
                    "event_type": row.get::<_, String>(2)?,
                    "subject": row.get::<_, Option<String>>(3)?,
                    "data": serde_json::from_str::<Value>(&data).unwrap_or_else(|_| json!({})),
                    "created_at": row.get::<_, i64>(5)?,
                    "agent_name": row.get::<_, Option<String>>(6)?
                }))
            })?;
            let mut events = Vec::new();
            for row in rows {
                events.push(row?);
            }
            Ok(Value::Array(events))
        })
    }

    pub fn wait_for_events(
        &self,
        agent_id: &str,
        after: i64,
        timeout: Duration,
        poll_interval: Duration,
        limit: u32,
    ) -> Result<Value> {
        if timeout > Duration::from_secs(60) {
            bail!("timeout must be at most 60 seconds");
        }
        if !(Duration::from_millis(10)..=Duration::from_secs(5)).contains(&poll_interval) {
            bail!("poll interval must be between 10 milliseconds and 5 seconds");
        }
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events(agent_id, after, limit)?;
            if events.as_array().is_some_and(|items| !items.is_empty()) {
                return Ok(events);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(json!([]));
            }
            thread::sleep(poll_interval.min(deadline.saturating_duration_since(now)));
        }
    }

    pub fn collect_stale(&self, stale_after: Duration) -> Result<Value> {
        let cutoff = now_ms()?.saturating_sub(i64::try_from(stale_after.as_millis())?);
        self.write(|transaction| {
            let mut statement = transaction.prepare(
                "SELECT id, pid, workspace_id FROM agents
                 WHERE status = 'running' AND heartbeat_at < ?",
            )?;
            let candidates = statement
                .query_map([cutoff], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let now = now_ms()?;
            let dead: Vec<_> = candidates
                .into_iter()
                .filter(|(_, pid, _)| !process_is_alive(*pid))
                .collect();
            for (agent_id, _, workspace_id) in &dead {
                transaction.execute(
                    "UPDATE agents SET status = 'dead', stopped_at = ? WHERE id = ?",
                    params![now, agent_id],
                )?;
                transaction.execute("DELETE FROM claims WHERE agent_id = ?", [agent_id])?;
                Self::event(
                    transaction,
                    workspace_id,
                    Some(agent_id),
                    "agent.dead",
                    Some(agent_id),
                    &json!({}),
                )?;
            }
            let expired =
                transaction.execute("DELETE FROM claims WHERE lease_expires_at <= ?", [now])?;
            Ok(json!({"dead_agents": dead.len(), "expired_claims": expired}))
        })
    }

    fn initialize(&self) -> Result<()> {
        let mut delay = Duration::from_millis(10);
        let connection = self.lock_connection()?;
        for attempt in 0..8 {
            match connection.execute_batch(SCHEMA) {
                Ok(()) => {
                    connection.pragma_update(None, "user_version", 1)?;
                    return Ok(());
                }
                Err(error) if is_busy(&error) && attempt < 7 => {
                    thread::sleep(delay);
                    delay *= 2;
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("database initialization retries exhausted")
    }

    fn open_connection(path: &Path) -> Result<Connection> {
        let connection =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(connection)
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("database connection lock is poisoned"))
    }

    fn read<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self.lock_connection()?;
        operation(&connection)
    }

    fn write<T>(&self, mut operation: impl FnMut(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut delay = Duration::from_millis(10);
        let mut connection = self.lock_connection()?;
        for attempt in 0..8 {
            let transaction =
                match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
                    Ok(transaction) => transaction,
                    Err(error) if is_busy(&error) && attempt < 7 => {
                        thread::sleep(delay);
                        delay *= 2;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
            let result = operation(&transaction)?;
            match transaction.commit() {
                Ok(()) => return Ok(result),
                Err(error) if is_busy(&error) && attempt < 7 => {
                    thread::sleep(delay);
                    delay *= 2;
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("database write retries exhausted")
    }

    fn ensure_workspace(transaction: &Transaction<'_>, root: &str) -> Result<String> {
        let workspace_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("pidmesh:{root}").as_bytes())
            .simple()
            .to_string();
        transaction.execute(
            "INSERT OR IGNORE INTO workspaces(id, root, created_at) VALUES (?, ?, ?)",
            params![workspace_id, root, now_ms()?],
        )?;
        Ok(workspace_id)
    }

    fn agent_row(connection: &Connection, agent_id: &str) -> Result<AgentRow> {
        connection
            .query_row(
                "SELECT workspace_id, started_at FROM agents WHERE id = ?",
                [agent_id],
                |row| {
                    Ok(AgentRow {
                        workspace_id: row.get(0)?,
                        started_at: row.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))
    }

    fn event(
        transaction: &Transaction<'_>,
        workspace_id: &str,
        agent_id: Option<&str>,
        event_type: &str,
        subject: Option<&str>,
        data: &Value,
    ) -> Result<()> {
        transaction.execute(
            "INSERT INTO events(
                workspace_id, agent_id, event_type, subject, data_json, created_at
             ) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                workspace_id,
                agent_id,
                event_type,
                subject,
                serde_json::to_string(data)?,
                now_ms()?
            ],
        )?;
        Ok(())
    }

    #[cfg(unix)]
    fn restrict_permissions(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn restrict_permissions(&self) -> Result<()> {
        Ok(())
    }
}

pub fn default_database_path() -> Result<PathBuf> {
    if let Some(configured) = env::var_os("PIDMESH_DB") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".pidmesh").join("pidmesh.db"))
}

pub fn workspace_root(root: Option<&Path>) -> Result<String> {
    let selected = root
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("PIDMESH_WORKSPACE").map(PathBuf::from))
        .unwrap_or(env::current_dir()?);
    let absolute = if selected.is_absolute() {
        selected
    } else {
        env::current_dir()?.join(selected)
    };
    let normalized = absolute.canonicalize().unwrap_or(absolute);
    Ok(normalized.to_string_lossy().into_owned())
}

#[cfg(unix)]
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    i32::try_from(pid).is_ok_and(|pid| match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    })
}

#[cfg(not(unix))]
#[must_use]
pub const fn process_is_alive(pid: u32) -> bool {
    pid > 0
}

#[allow(clippy::unnecessary_wraps)]
fn parent_pid() -> Option<i64> {
    #[cfg(unix)]
    {
        Some(i64::from(nix::unistd::getppid().as_raw()))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn now_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    i64::try_from(duration.as_millis()).context("timestamp exceeds i64")
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn memory_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "agent_id": row.get::<_, Option<String>>(1)?,
        "kind": row.get::<_, String>(2)?,
        "key": row.get::<_, Option<String>>(3)?,
        "content": row.get::<_, String>(4)?,
        "importance": row.get::<_, f64>(5)?,
        "created_at": row.get::<_, i64>(6)?,
        "agent_name": row.get::<_, Option<String>>(7)?
    }))
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory(_: &Path) -> Result<()> {
    Ok(())
}
