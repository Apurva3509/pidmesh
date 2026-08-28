from __future__ import annotations

import asyncio
import os
from contextlib import asynccontextmanager, suppress
from pathlib import Path
from typing import Any

from pidmesh.store import MeshStore, workspace_root


def build_server(
    *,
    database: str | Path | None = None,
    workspace: str | Path | None = None,
    name: str | None = None,
    provider: str | None = None,
) -> Any:
    try:
        from mcp.server import MCPServer
    except ImportError as error:
        raise RuntimeError("install MCP support with: uv tool install 'pidmesh[mcp]'") from error

    store = MeshStore(database)
    registration = store.register_agent(
        name or os.environ.get("PIDMESH_AGENT_NAME", "mcp-agent"),
        os.getpid(),
        root=workspace,
        provider=provider or os.environ.get("PIDMESH_PROVIDER", "mcp"),
        capabilities=["memory", "messaging", "claims", "events", "wait"],
    )
    agent_id = registration["agent_id"]

    @asynccontextmanager
    async def lifespan(_: Any):
        async def heartbeat_loop() -> None:
            while True:
                await asyncio.sleep(5)
                await asyncio.to_thread(store.heartbeat, agent_id)

        heartbeat_task = asyncio.create_task(heartbeat_loop())
        try:
            yield {}
        finally:
            heartbeat_task.cancel()
            with suppress(asyncio.CancelledError):
                await heartbeat_task
            await asyncio.to_thread(store.stop_agent, agent_id)

    server = MCPServer(
        "PidMesh",
        instructions=(
            "Use PidMesh to coordinate with other local agent processes. Check status and inbox "
            "before starting work, claim a task before editing, and record decisions as memories."
        ),
        lifespan=lifespan,
    )

    @server.tool()
    def mesh_status() -> dict[str, Any]:
        """List local agents, process liveness, and active task claims."""
        store.heartbeat(agent_id)
        return store.status(agent_id)

    @server.tool()
    def remember(
        content: str,
        kind: str = "note",
        key: str | None = None,
        importance: float = 0.5,
    ) -> dict[str, Any]:
        """Append a durable memory visible to agents in this workspace."""
        store.heartbeat(agent_id)
        return store.remember(agent_id, content, kind=kind, key=key, importance=importance)

    @server.tool()
    def recall(query: str, limit: int = 10) -> list[dict[str, Any]]:
        """Search durable memories written by any agent in this workspace."""
        store.heartbeat(agent_id)
        return store.recall(agent_id, query, limit)

    @server.tool()
    def send(
        message: str, recipient: str = "*", correlation_id: str | None = None
    ) -> dict[str, Any]:
        """Send a message to one active agent by name/id or broadcast with '*'."""
        store.heartbeat(agent_id)
        return store.send(agent_id, message, recipient=recipient, correlation_id=correlation_id)

    @server.tool()
    def inbox(acknowledge: bool = False, limit: int = 50) -> dict[str, Any]:
        """Read unread direct and broadcast messages for this agent."""
        store.heartbeat(agent_id)
        messages = store.inbox(agent_id, limit=limit)
        acknowledged = (
            store.acknowledge(agent_id, [message["id"] for message in messages])
            if acknowledge
            else 0
        )
        return {"messages": messages, "acknowledged": acknowledged}

    @server.tool()
    def claim(task: str, lease_seconds: int = 300, detail: str | None = None) -> dict[str, Any]:
        """Atomically acquire or renew a task lease to prevent duplicate agent work."""
        store.heartbeat(agent_id)
        return store.claim(agent_id, task, lease_seconds=lease_seconds, detail=detail)

    @server.tool()
    def release(task: str) -> dict[str, bool]:
        """Release a task lease owned by this agent."""
        store.heartbeat(agent_id)
        return {"released": store.release(agent_id, task)}

    @server.tool()
    def event_stream(after_sequence: int = 0, limit: int = 100) -> list[dict[str, Any]]:
        """Read ordered coordination events after a sequence number."""
        store.heartbeat(agent_id)
        return store.events(agent_id, after_sequence, limit)

    @server.tool()
    def wait_for_events(
        after_sequence: int = 0, timeout_seconds: float = 30, limit: int = 100
    ) -> list[dict[str, Any]]:
        """Wait up to 60 seconds for the next coordination event."""
        store.heartbeat(agent_id)
        return store.wait_for_events(
            agent_id,
            after=after_sequence,
            timeout_seconds=timeout_seconds,
            limit=limit,
        )

    return server


def main() -> None:
    server = build_server(
        database=os.environ.get("PIDMESH_DB"),
        workspace=os.environ.get("PIDMESH_WORKSPACE") or workspace_root(),
    )
    server.run()
