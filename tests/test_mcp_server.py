from __future__ import annotations

import asyncio
from pathlib import Path

from pidmesh.mcp_server import build_server


def test_mcp_server_exposes_coordination_tools(tmp_path: Path) -> None:
    server = build_server(database=tmp_path / "mesh.db", workspace=tmp_path, name="test-agent")
    tool_names = {tool.name for tool in asyncio.run(server.list_tools())}
    assert tool_names == {
        "claim",
        "event_stream",
        "inbox",
        "mesh_status",
        "recall",
        "release",
        "remember",
        "send",
        "wait_for_events",
    }


def test_mcp_tools_coordinate_two_servers(tmp_path: Path) -> None:
    database = tmp_path / "mesh.db"
    first = build_server(database=database, workspace=tmp_path, name="first")
    second = build_server(database=database, workspace=tmp_path, name="second")

    async def exercise() -> None:
        status = await first.call_tool("mesh_status", {})
        assert len(status.structured_content["agents"]) == 2
        await first.call_tool(
            "remember",
            {"content": "Shared API decision", "kind": "decision", "key": "api"},
        )
        recall = await second.call_tool("recall", {"query": "API decision"})
        assert recall.structured_content["result"][0]["key"] == "api"
        await first.call_tool("send", {"message": "hello", "recipient": "second"})
        inbox = await second.call_tool("inbox", {"acknowledge": True})
        assert inbox.structured_content["acknowledged"] == 1
        claim = await first.call_tool("claim", {"task": "task-1", "lease_seconds": 30})
        assert claim.structured_content["acquired"]
        events = await first.call_tool("event_stream", {"after_sequence": 0})
        assert events.structured_content["result"]
        waited = await first.call_tool(
            "wait_for_events",
            {
                "after_sequence": events.structured_content["result"][-1]["sequence"],
                "timeout_seconds": 0,
            },
        )
        assert waited.structured_content["result"] == []
        released = await first.call_tool("release", {"task": "task-1"})
        assert released.structured_content["released"]

    asyncio.run(exercise())
