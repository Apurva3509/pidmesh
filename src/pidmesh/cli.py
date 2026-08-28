from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import threading
import uuid
from pathlib import Path
from typing import Any

from pidmesh import __version__
from pidmesh.store import MeshStore, default_database_path, workspace_root


def _emit(value: Any) -> None:
    sys.stdout.write(json.dumps(value, indent=2, sort_keys=True) + "\n")


def _agent_id(args: argparse.Namespace) -> str:
    value = getattr(args, "agent", None) or os.environ.get("PIDMESH_AGENT_ID")
    if not value:
        raise ValueError("agent id required: pass --agent or set PIDMESH_AGENT_ID")
    return value


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="pidmesh")
    parser.add_argument("--db", type=Path, default=default_database_path())
    parser.add_argument("--version", action="version", version=__version__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    init = subparsers.add_parser("init", help="initialize the shared store")
    init.add_argument("--workspace", type=Path)

    join = subparsers.add_parser("join", help="register an agent process")
    join.add_argument("--name", required=True)
    join.add_argument("--pid", type=int, default=os.getpid())
    join.add_argument("--provider", default="unknown")
    join.add_argument("--capability", action="append", default=[])
    join.add_argument("--workspace", type=Path)

    heartbeat = subparsers.add_parser("heartbeat", help="refresh process liveness")
    heartbeat.add_argument("--agent")

    leave = subparsers.add_parser("leave", help="stop an agent and release its claims")
    leave.add_argument("--agent")

    remember = subparsers.add_parser("remember", help="append a shared memory")
    remember.add_argument("text")
    remember.add_argument("--agent")
    remember.add_argument("--kind", default="note")
    remember.add_argument("--key")
    remember.add_argument("--importance", type=float, default=0.5)

    recall = subparsers.add_parser("recall", help="search shared memory")
    recall.add_argument("query")
    recall.add_argument("--agent")
    recall.add_argument("--limit", type=int, default=10)

    send = subparsers.add_parser("send", help="send a direct or broadcast message")
    send.add_argument("text")
    send.add_argument("--agent")
    send.add_argument("--to", default="*")
    send.add_argument("--correlation-id")

    inbox = subparsers.add_parser("inbox", help="read messages")
    inbox.add_argument("--agent")
    inbox.add_argument("--all", action="store_true")
    inbox.add_argument("--ack", action="store_true")
    inbox.add_argument("--limit", type=int, default=50)

    claim = subparsers.add_parser("claim", help="atomically claim a task lease")
    claim.add_argument("task")
    claim.add_argument("--agent")
    claim.add_argument("--lease-seconds", type=int, default=300)
    claim.add_argument("--detail")

    release = subparsers.add_parser("release", help="release a task lease")
    release.add_argument("task")
    release.add_argument("--agent")

    status = subparsers.add_parser("status", help="show agents and active claims")
    status.add_argument("--agent")
    status.add_argument("--workspace", type=Path)

    events = subparsers.add_parser("events", help="read the append-only event stream")
    events.add_argument("--agent")
    events.add_argument("--after", type=int, default=0)
    events.add_argument("--limit", type=int, default=100)

    collect = subparsers.add_parser("gc", help="collect dead agents and expired leases")
    collect.add_argument("--stale-seconds", type=int, default=30)

    run = subparsers.add_parser("run", help="run and supervise an agent command")
    run.add_argument("--name", required=True)
    run.add_argument("--provider", default="unknown")
    run.add_argument("--workspace", type=Path)
    run.add_argument("--heartbeat-seconds", type=float, default=5)
    run.add_argument("child_command", nargs=argparse.REMAINDER)
    return parser


def _run_supervised(store: MeshStore, args: argparse.Namespace) -> int:
    command = list(args.child_command)
    if command and command[0] == "--":
        command.pop(0)
    if not command:
        raise ValueError("run requires a command after --")
    agent_id = f"{args.name}-run-{uuid.uuid4().hex[:8]}"
    normalized_workspace = workspace_root(args.workspace)
    registration = store.register_agent(
        args.name,
        os.getpid(),
        root=normalized_workspace,
        provider=args.provider,
        agent_id=agent_id,
    )
    child_env = {
        "PIDMESH_AGENT_ID": agent_id,
        "PIDMESH_DB": str(store.path),
        "PIDMESH_WORKSPACE": registration["workspace"],
    }
    environment = os.environ.copy()
    environment.update(child_env)
    try:
        child = subprocess.Popen(command, env=environment)
    except OSError:
        store.stop_agent(agent_id)
        raise
    store.update_agent_pid(agent_id, child.pid)
    store.remember(
        agent_id,
        json.dumps({"command": command, "environment": child_env}),
        kind="session",
        key="process.start",
        importance=0.2,
    )
    stop = threading.Event()

    def beat() -> None:
        while not stop.wait(args.heartbeat_seconds):
            if child.poll() is not None:
                return
            store.heartbeat(agent_id)

    thread = threading.Thread(target=beat, name="pidmesh-heartbeat", daemon=True)
    thread.start()
    _emit({**registration, "environment": child_env})
    try:
        return child.wait()
    finally:
        stop.set()
        thread.join(timeout=args.heartbeat_seconds + 1)
        store.stop_agent(agent_id)


def dispatch(args: argparse.Namespace) -> int:
    store = MeshStore(args.db)
    if args.command == "init":
        _emit({"database": str(store.path), "workspace": workspace_root(args.workspace)})
    elif args.command == "join":
        _emit(
            store.register_agent(
                args.name,
                args.pid,
                root=args.workspace,
                provider=args.provider,
                capabilities=args.capability,
            )
        )
    elif args.command == "heartbeat":
        _emit({"updated": store.heartbeat(_agent_id(args))})
    elif args.command == "leave":
        _emit({"stopped": store.stop_agent(_agent_id(args))})
    elif args.command == "remember":
        _emit(
            store.remember(
                _agent_id(args),
                args.text,
                kind=args.kind,
                key=args.key,
                importance=args.importance,
            )
        )
    elif args.command == "recall":
        _emit(store.recall(_agent_id(args), args.query, args.limit))
    elif args.command == "send":
        _emit(
            store.send(
                _agent_id(args),
                args.text,
                recipient=args.to,
                correlation_id=args.correlation_id,
            )
        )
    elif args.command == "inbox":
        agent_id = _agent_id(args)
        messages = store.inbox(agent_id, unread_only=not args.all, limit=args.limit)
        acknowledged = (
            store.acknowledge(agent_id, [item["id"] for item in messages]) if args.ack else 0
        )
        _emit({"messages": messages, "acknowledged": acknowledged})
    elif args.command == "claim":
        _emit(
            store.claim(
                _agent_id(args),
                args.task,
                lease_seconds=args.lease_seconds,
                detail=args.detail,
            )
        )
    elif args.command == "release":
        _emit({"released": store.release(_agent_id(args), args.task)})
    elif args.command == "status":
        _emit(store.status(args.agent, args.workspace))
    elif args.command == "events":
        _emit(store.events(_agent_id(args), args.after, args.limit))
    elif args.command == "gc":
        _emit(store.collect_stale(args.stale_seconds))
    elif args.command == "run":
        return _run_supervised(store, args)
    return 0


def main() -> None:
    try:
        raise SystemExit(dispatch(build_parser().parse_args()))
    except (OSError, ValueError) as error:
        sys.stderr.write(json.dumps({"error": str(error)}) + "\n")
        raise SystemExit(2) from error
