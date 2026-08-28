from __future__ import annotations

import multiprocessing
import os
import sqlite3
import tempfile
import time
from pathlib import Path

import pytest

from pidmesh.store import MeshStore


def _write_memories(database: str, workspace: str, worker: int, count: int) -> None:
    store = MeshStore(database)
    agent = store.register_agent(f"worker-{worker}", os.getpid(), root=workspace)
    for index in range(count):
        store.remember(
            agent["agent_id"],
            f"worker {worker} completed unit {index}",
            kind="progress",
            key=f"{worker}:{index}",
        )


def _compete_for_claim(
    database: str,
    workspace: str,
    worker: int,
    start: multiprocessing.Event,
    output: multiprocessing.Queue,
) -> None:
    store = MeshStore(database)
    agent = store.register_agent(f"worker-{worker}", os.getpid(), root=workspace)
    start.wait()
    result = store.claim(agent["agent_id"], "exclusive-task", lease_seconds=60)
    output.put(result["acquired"])


@pytest.fixture
def store(tmp_path: Path) -> MeshStore:
    return MeshStore(tmp_path / "mesh.db")


def test_memory_search_is_shared_and_workspace_scoped(store: MeshStore, tmp_path: Path) -> None:
    first = store.register_agent("codex", os.getpid(), root=tmp_path / "alpha")
    second = store.register_agent("claude", os.getpid(), root=tmp_path / "alpha")
    outsider = store.register_agent("other", os.getpid(), root=tmp_path / "beta")

    store.remember(
        first["agent_id"],
        "Use SQLite WAL to support concurrent writers",
        kind="decision",
        key="storage",
        importance=0.9,
    )
    store.remember(outsider["agent_id"], "Use a hosted database", kind="decision")

    results = store.recall(second["agent_id"], "SQLite concurrent")

    assert [item["content"] for item in results] == ["Use SQLite WAL to support concurrent writers"]
    assert results[0]["agent_name"] == "codex"


def test_direct_and_broadcast_messages_have_per_agent_receipts(
    store: MeshStore, tmp_path: Path
) -> None:
    root = tmp_path / "project"
    sender = store.register_agent("planner", os.getpid(), root=root)
    worker = store.register_agent("worker", os.getpid(), root=root)
    observer = store.register_agent("observer", os.getpid(), root=root)

    direct = store.send(sender["agent_id"], "implement parser", recipient="worker")
    broadcast = store.send(sender["agent_id"], "tests are green")

    worker_inbox = store.inbox(worker["agent_id"])
    observer_inbox = store.inbox(observer["agent_id"])
    assert [message["id"] for message in worker_inbox] == [
        direct["message_id"],
        broadcast["message_id"],
    ]
    assert [message["id"] for message in observer_inbox] == [broadcast["message_id"]]

    assert store.acknowledge(worker["agent_id"], [message["id"] for message in worker_inbox]) == 2
    assert store.inbox(worker["agent_id"]) == []
    assert len(store.inbox(observer["agent_id"])) == 1


def test_claim_has_one_winner_across_eight_processes(tmp_path: Path) -> None:
    database = str(tmp_path / "claims.db")
    workspace = str(tmp_path / "workspace")
    context = multiprocessing.get_context("spawn")
    start = context.Event()
    output = context.Queue()
    processes = [
        context.Process(target=_compete_for_claim, args=(database, workspace, index, start, output))
        for index in range(8)
    ]
    for process in processes:
        process.start()
    start.set()
    for process in processes:
        process.join(timeout=20)
        assert process.exitcode == 0

    assert sum(output.get(timeout=1) for _ in processes) == 1


def test_eight_processes_can_write_without_data_loss(tmp_path: Path) -> None:
    database = str(tmp_path / "concurrent.db")
    workspace = str(tmp_path / "workspace")
    context = multiprocessing.get_context("spawn")
    processes = [
        context.Process(target=_write_memories, args=(database, workspace, index, 25))
        for index in range(8)
    ]
    for process in processes:
        process.start()
    for process in processes:
        process.join(timeout=30)
        assert process.exitcode == 0

    connection = sqlite3.connect(database)
    try:
        assert connection.execute("SELECT count(*) FROM memories").fetchone()[0] == 200
        assert connection.execute("PRAGMA journal_mode").fetchone()[0] == "wal"
    finally:
        connection.close()


def test_expired_claim_can_be_taken_over(store: MeshStore, tmp_path: Path) -> None:
    first = store.register_agent("first", os.getpid(), root=tmp_path)
    second = store.register_agent("second", os.getpid(), root=tmp_path)
    assert store.claim(first["agent_id"], "task", lease_seconds=1)["acquired"]
    assert not store.claim(second["agent_id"], "task", lease_seconds=10)["acquired"]

    time.sleep(1.05)

    takeover = store.claim(second["agent_id"], "task", lease_seconds=10)
    assert takeover["acquired"]
    assert takeover["agent_id"] == second["agent_id"]


def test_stop_releases_claims(store: MeshStore, tmp_path: Path) -> None:
    agent = store.register_agent("worker", os.getpid(), root=tmp_path)
    store.claim(agent["agent_id"], "task")

    assert store.stop_agent(agent["agent_id"])
    status = store.status(root=tmp_path)
    assert status["claims"] == []
    assert status["agents"][0]["status"] == "stopped"


def test_database_permissions_are_private(tmp_path: Path) -> None:
    database = tmp_path / "private" / "mesh.db"
    MeshStore(database)
    assert database.stat().st_mode & 0o777 == 0o600


def test_invalid_importance_is_rejected(store: MeshStore, tmp_path: Path) -> None:
    agent = store.register_agent("worker", os.getpid(), root=tmp_path)
    with pytest.raises(ValueError, match="importance"):
        store.remember(agent["agent_id"], "bad", importance=1.5)


def test_status_before_workspace_exists(store: MeshStore, tmp_path: Path) -> None:
    status = store.status(root=tmp_path / "missing")
    assert status["agents"] == []
    assert status["claims"] == []


def test_empty_query_returns_important_recent_memories(store: MeshStore, tmp_path: Path) -> None:
    agent = store.register_agent("worker", os.getpid(), root=tmp_path)
    store.remember(agent["agent_id"], "low", importance=0.1)
    store.remember(agent["agent_id"], "high", importance=0.9)
    assert [item["content"] for item in store.recall(agent["agent_id"], "")] == [
        "high",
        "low",
    ]


def test_missing_recipient_and_agent_are_rejected(store: MeshStore, tmp_path: Path) -> None:
    agent = store.register_agent("worker", os.getpid(), root=tmp_path)
    with pytest.raises(ValueError, match="recipient"):
        store.send(agent["agent_id"], "hello", recipient="missing")
    assert not store.heartbeat("missing")


def test_collect_stale_dead_agent_and_expired_claim(store: MeshStore, tmp_path: Path) -> None:
    dead = store.register_agent("dead", 99_999_999, root=tmp_path)
    live = store.register_agent("live", os.getpid(), root=tmp_path)
    store.claim(live["agent_id"], "expired", lease_seconds=1)
    connection = sqlite3.connect(store.path)
    try:
        connection.execute("UPDATE agents SET heartbeat_at = 0 WHERE id = ?", (dead["agent_id"],))
        connection.execute("UPDATE claims SET lease_expires_at = 0")
        connection.commit()
    finally:
        connection.close()

    result = store.collect_stale(stale_seconds=1)
    assert result == {"dead_agents": 1, "expired_claims": 1}
    statuses = {agent["name"]: agent["status"] for agent in store.status(root=tmp_path)["agents"]}
    assert statuses["dead"] == "dead"


def test_release_and_acknowledge_noops(store: MeshStore, tmp_path: Path) -> None:
    agent = store.register_agent("worker", os.getpid(), root=tmp_path)
    assert not store.release(agent["agent_id"], "missing")
    assert store.acknowledge(agent["agent_id"], []) == 0


def test_temporary_database_can_be_reopened() -> None:
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "mesh.db"
        MeshStore(path)
        MeshStore(path)
