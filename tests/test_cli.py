from __future__ import annotations

import json
import os
import sys
from pathlib import Path

from pidmesh.cli import build_parser, dispatch


def test_cli_agent_lifecycle(tmp_path: Path, capsys) -> None:
    database = tmp_path / "mesh.db"
    parser = build_parser()
    dispatch(
        parser.parse_args(
            [
                "--db",
                str(database),
                "join",
                "--name",
                "codex",
                "--pid",
                str(os.getpid()),
                "--workspace",
                str(tmp_path),
            ]
        )
    )
    registration = json.loads(capsys.readouterr().out)

    dispatch(
        parser.parse_args(
            [
                "--db",
                str(database),
                "remember",
                "shared decision",
                "--agent",
                registration["agent_id"],
                "--kind",
                "decision",
            ]
        )
    )
    capsys.readouterr()
    dispatch(
        parser.parse_args(
            [
                "--db",
                str(database),
                "recall",
                "shared",
                "--agent",
                registration["agent_id"],
            ]
        )
    )
    results = json.loads(capsys.readouterr().out)
    assert results[0]["content"] == "shared decision"


def test_cli_coordination_commands(tmp_path: Path, capsys, monkeypatch) -> None:
    database = tmp_path / "mesh.db"
    parser = build_parser()

    def invoke(*arguments: str):
        result = dispatch(parser.parse_args(["--db", str(database), *arguments]))
        output = capsys.readouterr().out
        return result, json.loads(output) if output else None

    _, initialized = invoke("init", "--workspace", str(tmp_path))
    assert initialized["database"] == str(database)

    _, sender = invoke(
        "join",
        "--name",
        "planner",
        "--pid",
        str(os.getpid()),
        "--provider",
        "codex",
        "--capability",
        "planning",
        "--workspace",
        str(tmp_path),
    )
    _, receiver = invoke(
        "join",
        "--name",
        "worker",
        "--pid",
        str(os.getpid()),
        "--workspace",
        str(tmp_path),
    )
    monkeypatch.setenv("PIDMESH_AGENT_ID", sender["agent_id"])
    _, heartbeat = invoke("heartbeat")
    assert heartbeat["updated"]

    _, sent = invoke(
        "send",
        "take this task",
        "--to",
        "worker",
        "--correlation-id",
        "work-1",
    )
    assert sent["recipient"] == receiver["agent_id"]

    _, inbox = invoke("inbox", "--agent", receiver["agent_id"], "--ack")
    assert inbox["acknowledged"] == 1
    _, all_messages = invoke("inbox", "--agent", receiver["agent_id"], "--all")
    assert all_messages["messages"][0]["correlation_id"] == "work-1"

    _, claim = invoke("claim", "parser", "--lease-seconds", "10", "--detail", "parser.py")
    assert claim["acquired"]
    _, status = invoke("status", "--agent", sender["agent_id"])
    assert status["claims"][0]["task_key"] == "parser"
    _, events = invoke("events", "--after", "0", "--limit", "20")
    assert any(event["event_type"] == "task.claimed" for event in events)
    _, released = invoke("release", "parser")
    assert released["released"]
    _, stopped = invoke("leave")
    assert stopped["stopped"]
    _, collected = invoke("gc", "--stale-seconds", "0")
    assert collected["expired_claims"] == 0


def test_cli_run_injects_mesh_environment(tmp_path: Path, capsys) -> None:
    database = tmp_path / "mesh.db"
    marker = tmp_path / "agent-id.txt"
    parser = build_parser()
    result = dispatch(
        parser.parse_args(
            [
                "--db",
                str(database),
                "run",
                "--name",
                "child",
                "--provider",
                "test",
                "--workspace",
                str(tmp_path),
                "--heartbeat-seconds",
                "0.01",
                "--",
                sys.executable,
                "-c",
                f"import os; open({str(marker)!r}, 'w').write(os.environ['PIDMESH_AGENT_ID'])",
            ]
        )
    )
    registration = json.loads(capsys.readouterr().out)
    assert result == 0
    assert marker.read_text() == registration["agent_id"]
