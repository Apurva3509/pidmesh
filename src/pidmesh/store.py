from __future__ import annotations

import json
import os
import re
import sqlite3
import time
import uuid
from collections.abc import Callable
from pathlib import Path
from typing import Any, TypeVar

T = TypeVar("T")

SCHEMA = """
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
"""


def default_database_path() -> Path:
    configured = os.environ.get("PIDMESH_DB")
    if configured:
        return Path(configured).expanduser().resolve()
    return Path.home() / ".pidmesh" / "pidmesh.db"


def workspace_root(value: str | Path | None = None) -> str:
    configured = value or os.environ.get("PIDMESH_WORKSPACE") or Path.cwd()
    return str(Path(configured).expanduser().resolve())


def process_is_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


class MeshStore:
    def __init__(self, path: str | Path | None = None) -> None:
        self.path = Path(path or default_database_path()).expanduser().resolve()
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self._initialize()

    def _connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.path, timeout=30, isolation_level=None)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys = ON")
        connection.execute("PRAGMA busy_timeout = 30000")
        connection.execute("PRAGMA synchronous = NORMAL")
        return connection

    def _initialize(self) -> None:
        delay = 0.01
        for attempt in range(8):
            connection = self._connect()
            try:
                connection.execute("PRAGMA journal_mode = WAL")
                connection.executescript(SCHEMA)
                connection.execute("PRAGMA user_version = 1")
                break
            except sqlite3.OperationalError as error:
                if "locked" not in str(error).lower() or attempt == 7:
                    raise
            finally:
                connection.close()
            time.sleep(delay)
            delay *= 2
        self.path.chmod(0o600)

    def _write(self, operation: Callable[[sqlite3.Connection], T]) -> T:
        delay = 0.01
        for attempt in range(8):
            connection = self._connect()
            try:
                connection.execute("BEGIN IMMEDIATE")
                result = operation(connection)
                connection.commit()
                return result
            except sqlite3.OperationalError as error:
                connection.rollback()
                if "locked" not in str(error).lower() or attempt == 7:
                    raise
            finally:
                connection.close()
            time.sleep(delay)
            delay *= 2
        raise RuntimeError("database write retries exhausted")

    def _read(self, operation: Callable[[sqlite3.Connection], T]) -> T:
        connection = self._connect()
        try:
            return operation(connection)
        finally:
            connection.close()

    @staticmethod
    def _now() -> int:
        return time.time_ns() // 1_000_000

    @staticmethod
    def _workspace_id(root: str) -> str:
        return uuid.uuid5(uuid.NAMESPACE_URL, f"pidmesh:{root}").hex

    def _ensure_workspace(self, connection: sqlite3.Connection, root: str) -> str:
        identifier = self._workspace_id(root)
        connection.execute(
            "INSERT OR IGNORE INTO workspaces(id, root, created_at) VALUES (?, ?, ?)",
            (identifier, root, self._now()),
        )
        return identifier

    def _event(
        self,
        connection: sqlite3.Connection,
        workspace_id: str,
        agent_id: str | None,
        event_type: str,
        subject: str | None = None,
        data: dict[str, Any] | None = None,
    ) -> None:
        connection.execute(
            """
            INSERT INTO events(workspace_id, agent_id, event_type, subject, data_json, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            """,
            (workspace_id, agent_id, event_type, subject, json.dumps(data or {}), self._now()),
        )

    def register_agent(
        self,
        name: str,
        pid: int,
        *,
        root: str | Path | None = None,
        provider: str = "unknown",
        capabilities: list[str] | None = None,
        agent_id: str | None = None,
    ) -> dict[str, Any]:
        normalized_root = workspace_root(root)
        identifier = agent_id or f"{name}-{pid}-{uuid.uuid4().hex[:8]}"
        now = self._now()

        def operation(connection: sqlite3.Connection) -> dict[str, Any]:
            workspace_id = self._ensure_workspace(connection, normalized_root)
            connection.execute(
                """
                INSERT INTO agents(
                    id, workspace_id, name, pid, parent_pid, provider,
                    capabilities_json, started_at, heartbeat_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    identifier,
                    workspace_id,
                    name,
                    pid,
                    os.getppid(),
                    provider,
                    json.dumps(capabilities or []),
                    now,
                    now,
                ),
            )
            self._event(connection, workspace_id, identifier, "agent.joined", identifier)
            return {
                "agent_id": identifier,
                "name": name,
                "pid": pid,
                "provider": provider,
                "workspace": normalized_root,
                "workspace_id": workspace_id,
            }

        return self._write(operation)

    def heartbeat(self, agent_id: str) -> bool:
        now = self._now()

        def operation(connection: sqlite3.Connection) -> bool:
            cursor = connection.execute(
                """
                UPDATE agents SET heartbeat_at = ?, status = 'running', stopped_at = NULL
                WHERE id = ?
                """,
                (now, agent_id),
            )
            return cursor.rowcount == 1

        return self._write(operation)

    def update_agent_pid(self, agent_id: str, pid: int) -> bool:
        def operation(connection: sqlite3.Connection) -> bool:
            cursor = connection.execute(
                "UPDATE agents SET pid = ?, heartbeat_at = ? WHERE id = ?",
                (pid, self._now(), agent_id),
            )
            return cursor.rowcount == 1

        return self._write(operation)

    def stop_agent(self, agent_id: str) -> bool:
        now = self._now()

        def operation(connection: sqlite3.Connection) -> bool:
            row = connection.execute(
                "SELECT workspace_id FROM agents WHERE id = ?", (agent_id,)
            ).fetchone()
            if row is None:
                return False
            connection.execute(
                "UPDATE agents SET status = 'stopped', stopped_at = ? WHERE id = ?",
                (now, agent_id),
            )
            connection.execute("DELETE FROM claims WHERE agent_id = ?", (agent_id,))
            self._event(connection, row["workspace_id"], agent_id, "agent.stopped", agent_id)
            return True

        return self._write(operation)

    def remember(
        self,
        agent_id: str,
        content: str,
        *,
        kind: str = "note",
        key: str | None = None,
        importance: float = 0.5,
    ) -> dict[str, Any]:
        if not 0 <= importance <= 1:
            raise ValueError("importance must be between 0 and 1")

        def operation(connection: sqlite3.Connection) -> dict[str, Any]:
            agent = self._agent_row(connection, agent_id)
            cursor = connection.execute(
                """
                INSERT INTO memories(
                    workspace_id, agent_id, kind, key, content, importance, created_at
                )
                VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    agent["workspace_id"],
                    agent_id,
                    kind,
                    key,
                    content,
                    importance,
                    self._now(),
                ),
            )
            memory_id = int(cursor.lastrowid)
            self._event(
                connection, agent["workspace_id"], agent_id, "memory.created", str(memory_id)
            )
            return {"memory_id": memory_id, "kind": kind, "key": key}

        return self._write(operation)

    def recall(self, agent_id: str, query: str, limit: int = 10) -> list[dict[str, Any]]:
        def operation(connection: sqlite3.Connection) -> list[dict[str, Any]]:
            agent = self._agent_row(connection, agent_id)
            terms = re.findall(r"[\w.-]+", query, flags=re.UNICODE)
            if not terms:
                rows = connection.execute(
                    """
                    SELECT m.*, a.name AS agent_name
                    FROM memories m LEFT JOIN agents a ON a.id = m.agent_id
                    WHERE m.workspace_id = ?
                    ORDER BY m.importance DESC, m.created_at DESC LIMIT ?
                    """,
                    (agent["workspace_id"], limit),
                ).fetchall()
            else:
                expression = " AND ".join(f'"{term}"*' for term in terms)
                rows = connection.execute(
                    """
                    SELECT m.*, a.name AS agent_name, bm25(memory_fts) AS rank
                    FROM memory_fts
                    JOIN memories m ON m.id = memory_fts.rowid
                    LEFT JOIN agents a ON a.id = m.agent_id
                    WHERE memory_fts MATCH ? AND m.workspace_id = ?
                    ORDER BY rank, m.importance DESC, m.created_at DESC LIMIT ?
                    """,
                    (expression, agent["workspace_id"], limit),
                ).fetchall()
            return [dict(row) for row in rows]

        return self._read(operation)

    def send(
        self,
        agent_id: str,
        body: str,
        *,
        recipient: str = "*",
        correlation_id: str | None = None,
    ) -> dict[str, Any]:
        def operation(connection: sqlite3.Connection) -> dict[str, Any]:
            sender = self._agent_row(connection, agent_id)
            recipient_id = None
            if recipient != "*":
                recipient_row = connection.execute(
                    """
                    SELECT * FROM agents
                    WHERE workspace_id = ? AND (id = ? OR name = ?) AND status = 'running'
                    ORDER BY heartbeat_at DESC LIMIT 1
                    """,
                    (sender["workspace_id"], recipient, recipient),
                ).fetchone()
                if recipient_row is None:
                    raise ValueError(f"active recipient not found: {recipient}")
                recipient_id = recipient_row["id"]
            cursor = connection.execute(
                """
                INSERT INTO messages(
                    workspace_id, sender_id, recipient_id, body, correlation_id, created_at
                ) VALUES (?, ?, ?, ?, ?, ?)
                """,
                (
                    sender["workspace_id"],
                    agent_id,
                    recipient_id,
                    body,
                    correlation_id,
                    self._now(),
                ),
            )
            message_id = int(cursor.lastrowid)
            self._event(
                connection,
                sender["workspace_id"],
                agent_id,
                "message.sent",
                str(message_id),
                {"recipient": recipient_id or "*"},
            )
            return {"message_id": message_id, "recipient": recipient_id or "*"}

        return self._write(operation)

    def inbox(
        self, agent_id: str, *, unread_only: bool = True, limit: int = 50
    ) -> list[dict[str, Any]]:
        def operation(connection: sqlite3.Connection) -> list[dict[str, Any]]:
            agent = self._agent_row(connection, agent_id)
            unread_clause = "AND r.message_id IS NULL" if unread_only else ""
            rows = connection.execute(
                f"""
                SELECT m.id, m.body, m.correlation_id, m.created_at,
                       m.recipient_id, s.id AS sender_id, s.name AS sender_name,
                       r.acknowledged_at
                FROM messages m
                JOIN agents s ON s.id = m.sender_id
                LEFT JOIN message_receipts r
                  ON r.message_id = m.id AND r.agent_id = ?
                WHERE m.workspace_id = ?
                  AND m.sender_id != ?
                  AND (m.recipient_id = ? OR (m.recipient_id IS NULL AND m.created_at >= ?))
                  {unread_clause}
                ORDER BY m.created_at, m.id LIMIT ?
                """,
                (
                    agent_id,
                    agent["workspace_id"],
                    agent_id,
                    agent_id,
                    agent["started_at"],
                    limit,
                ),
            ).fetchall()
            return [dict(row) for row in rows]

        return self._read(operation)

    def acknowledge(self, agent_id: str, message_ids: list[int]) -> int:
        if not message_ids:
            return 0

        def operation(connection: sqlite3.Connection) -> int:
            agent = self._agent_row(connection, agent_id)
            placeholders = ",".join("?" for _ in message_ids)
            visible = connection.execute(
                f"""
                SELECT id FROM messages
                WHERE workspace_id = ? AND id IN ({placeholders})
                  AND (recipient_id = ? OR recipient_id IS NULL)
                """,
                (agent["workspace_id"], *message_ids, agent_id),
            ).fetchall()
            now = self._now()
            connection.executemany(
                """
                INSERT OR IGNORE INTO message_receipts(message_id, agent_id, acknowledged_at)
                VALUES (?, ?, ?)
                """,
                [(row["id"], agent_id, now) for row in visible],
            )
            return len(visible)

        return self._write(operation)

    def claim(
        self,
        agent_id: str,
        task_key: str,
        *,
        lease_seconds: int = 300,
        detail: str | None = None,
    ) -> dict[str, Any]:
        if lease_seconds < 1:
            raise ValueError("lease_seconds must be positive")
        now = self._now()
        lease_expires_at = now + lease_seconds * 1000

        def operation(connection: sqlite3.Connection) -> dict[str, Any]:
            agent = self._agent_row(connection, agent_id)
            connection.execute(
                """
                INSERT INTO claims(
                    workspace_id, task_key, detail, agent_id,
                    claimed_at, lease_expires_at, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(workspace_id, task_key) DO UPDATE SET
                    detail = excluded.detail,
                    agent_id = excluded.agent_id,
                    claimed_at = excluded.claimed_at,
                    lease_expires_at = excluded.lease_expires_at,
                    updated_at = excluded.updated_at
                WHERE claims.lease_expires_at <= ? OR claims.agent_id = excluded.agent_id
                """,
                (
                    agent["workspace_id"],
                    task_key,
                    detail,
                    agent_id,
                    now,
                    lease_expires_at,
                    now,
                    now,
                ),
            )
            claim = connection.execute(
                """
                SELECT c.*, a.name AS agent_name FROM claims c
                JOIN agents a ON a.id = c.agent_id
                WHERE c.workspace_id = ? AND c.task_key = ?
                """,
                (agent["workspace_id"], task_key),
            ).fetchone()
            acquired = claim["agent_id"] == agent_id
            if acquired:
                self._event(connection, agent["workspace_id"], agent_id, "task.claimed", task_key)
            result = dict(claim)
            result["acquired"] = acquired
            return result

        return self._write(operation)

    def release(self, agent_id: str, task_key: str) -> bool:
        def operation(connection: sqlite3.Connection) -> bool:
            agent = self._agent_row(connection, agent_id)
            cursor = connection.execute(
                "DELETE FROM claims WHERE workspace_id = ? AND task_key = ? AND agent_id = ?",
                (agent["workspace_id"], task_key, agent_id),
            )
            if cursor.rowcount:
                self._event(connection, agent["workspace_id"], agent_id, "task.released", task_key)
            return cursor.rowcount == 1

        return self._write(operation)

    def status(self, agent_id: str | None = None, root: str | Path | None = None) -> dict[str, Any]:
        def operation(connection: sqlite3.Connection) -> dict[str, Any]:
            if agent_id:
                agent = self._agent_row(connection, agent_id)
                workspace = connection.execute(
                    "SELECT * FROM workspaces WHERE id = ?", (agent["workspace_id"],)
                ).fetchone()
            else:
                normalized_root = workspace_root(root)
                workspace = connection.execute(
                    "SELECT * FROM workspaces WHERE root = ?", (normalized_root,)
                ).fetchone()
                if workspace is None:
                    return {"workspace": normalized_root, "agents": [], "claims": []}
            agents = connection.execute(
                """
                SELECT * FROM agents WHERE workspace_id = ?
                ORDER BY status = 'running' DESC, heartbeat_at DESC
                """,
                (workspace["id"],),
            ).fetchall()
            claims = connection.execute(
                """
                SELECT c.*, a.name AS agent_name FROM claims c
                JOIN agents a ON a.id = c.agent_id
                WHERE c.workspace_id = ? ORDER BY c.task_key
                """,
                (workspace["id"],),
            ).fetchall()
            now = self._now()
            agent_results = []
            for row in agents:
                item = dict(row)
                item["capabilities"] = json.loads(item.pop("capabilities_json"))
                item["pid_alive"] = process_is_alive(item["pid"])
                item["heartbeat_age_ms"] = now - item["heartbeat_at"]
                agent_results.append(item)
            return {
                "workspace": workspace["root"],
                "workspace_id": workspace["id"],
                "agents": agent_results,
                "claims": [dict(row) for row in claims],
            }

        return self._read(operation)

    def events(self, agent_id: str, after: int = 0, limit: int = 100) -> list[dict[str, Any]]:
        def operation(connection: sqlite3.Connection) -> list[dict[str, Any]]:
            agent = self._agent_row(connection, agent_id)
            rows = connection.execute(
                """
                SELECT e.*, a.name AS agent_name FROM events e
                LEFT JOIN agents a ON a.id = e.agent_id
                WHERE e.workspace_id = ? AND e.sequence > ?
                ORDER BY e.sequence LIMIT ?
                """,
                (agent["workspace_id"], after, limit),
            ).fetchall()
            results = []
            for row in rows:
                item = dict(row)
                item["data"] = json.loads(item.pop("data_json"))
                results.append(item)
            return results

        return self._read(operation)

    def wait_for_events(
        self,
        agent_id: str,
        *,
        after: int = 0,
        timeout_seconds: float = 30,
        poll_interval: float = 0.1,
        limit: int = 100,
    ) -> list[dict[str, Any]]:
        if not 0 <= timeout_seconds <= 60:
            raise ValueError("timeout_seconds must be between 0 and 60")
        if not 0.01 <= poll_interval <= 5:
            raise ValueError("poll_interval must be between 0.01 and 5")
        deadline = time.monotonic() + timeout_seconds
        while True:
            events = self.events(agent_id, after, limit)
            if events:
                return events
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return []
            time.sleep(min(poll_interval, remaining))

    def collect_stale(self, stale_seconds: int = 30) -> dict[str, int]:
        cutoff = self._now() - stale_seconds * 1000

        def operation(connection: sqlite3.Connection) -> dict[str, int]:
            candidates = connection.execute(
                """
                SELECT id, pid, workspace_id FROM agents
                WHERE status = 'running' AND heartbeat_at < ?
                """,
                (cutoff,),
            ).fetchall()
            stale = [row for row in candidates if not process_is_alive(row["pid"])]
            now = self._now()
            for row in stale:
                connection.execute(
                    "UPDATE agents SET status = 'dead', stopped_at = ? WHERE id = ?",
                    (now, row["id"]),
                )
                connection.execute("DELETE FROM claims WHERE agent_id = ?", (row["id"],))
                self._event(connection, row["workspace_id"], row["id"], "agent.dead", row["id"])
            expired = connection.execute(
                "DELETE FROM claims WHERE lease_expires_at <= ?", (now,)
            ).rowcount
            return {"dead_agents": len(stale), "expired_claims": expired}

        return self._write(operation)

    @staticmethod
    def _agent_row(connection: sqlite3.Connection, agent_id: str) -> sqlite3.Row:
        row = connection.execute("SELECT * FROM agents WHERE id = ?", (agent_id,)).fetchone()
        if row is None:
            raise ValueError(f"agent not found: {agent_id}")
        return row
