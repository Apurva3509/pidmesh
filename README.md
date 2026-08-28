# PidMesh

Local process-aware memory and coordination for concurrent AI agents.

Run Codex, Claude Code, Cursor, local models, and custom workers in separate terminals without
making them work blind. PidMesh gives every process a workspace-scoped identity, shared durable
memory, an inbox, an ordered event stream, and atomic task leases through one private SQLite file.

No daemon. No cloud account. No API key.

## Why this exists

Long-term memory systems are good at retrieving old context. Agent message buses are good at chat.
Neither primitive answers the operating-system questions that matter when many local agents work at
once:

- Which sessions are actually alive?
- Which PID owns the task right now?
- Can another worker safely take over after a crash?
- Did two agents start the same edit?
- What decisions and handoffs happened in order?

PidMesh is a small coordination kernel for those questions. It uses SQLite WAL, short transactions,
and expiring leases so concurrent processes coordinate without a broker.

## Install

```bash
uv tool install 'pidmesh[mcp]'
```

Until a package release is available, install directly from GitHub:

```bash
uv tool install 'pidmesh[mcp] @ git+https://github.com/Apurva3509/pidmesh.git'
```

## Five-minute demo

Register two live processes in the same repository:

```bash
pidmesh join --name planner --provider codex --pid $$
pidmesh join --name implementer --provider claude --pid $$
```

Set the returned IDs in their respective shells, then coordinate:

```bash
export PIDMESH_AGENT_ID=planner-1234-abcd1234
pidmesh remember "Use SQLite WAL; no daemon" --kind decision --key architecture
pidmesh send "Implement the claim transaction" --to implementer

export PIDMESH_AGENT_ID=implementer-5678-efgh5678
pidmesh inbox --ack
pidmesh claim store.claim --lease-seconds 900
pidmesh recall "architecture SQLite"
```

Inspect the mesh from another terminal:

```bash
pidmesh status
pidmesh events --agent "$PIDMESH_AGENT_ID"
pidmesh wait --agent "$PIDMESH_AGENT_ID" --after 42 --timeout-seconds 30
pidmesh gc
```

Every command emits JSON so both humans and agents can consume it reliably.

## MCP setup

PidMesh exposes nine tools: status, remember, recall, send, inbox, claim, release, event stream, and
bounded event waiting.
Each MCP server process registers its real PID, pulses its heartbeat every five seconds, and marks
itself stopped during a clean shutdown.

Claude Code:

```bash
claude mcp add --scope user pidmesh -- uvx --from 'pidmesh[mcp]' pidmesh-mcp
```

Codex:

```toml
[mcp_servers.pidmesh]
command = "uvx"
args = ["--from", "pidmesh[mcp]", "pidmesh-mcp"]
env = { PIDMESH_AGENT_NAME = "codex", PIDMESH_PROVIDER = "codex" }
```

Set `PIDMESH_WORKSPACE` when the host does not launch the server from the project directory. Set
`PIDMESH_DB` to share a different database; the default is `~/.pidmesh/pidmesh.db`.

## Concurrency guarantees

- WAL mode allows readers while other agents write.
- Writes use bounded retries and `BEGIN IMMEDIATE` transactions.
- A task claim has exactly one owner until its lease expires or the owner releases it.
- Stopped or dead sessions release their claims.
- Broadcast acknowledgements are tracked independently for every agent.
- Workspaces are isolated even though all projects can share one database.
- Long polling wakes an agent on the next event without a tight database polling loop.

The test suite launches eight simultaneous processes to verify write integrity and prove that an
atomic claim has one winner.

## Architecture

```text
Codex PID 4101 ─┐
Claude PID 4102 ├── CLI / MCP ── SQLite WAL ── memory + inbox + claims + events
Worker PID 4103 ┘                    │
                                  pid liveness
```

See [docs/protocol.md](docs/protocol.md) for the storage and lifecycle contract.

## Development

```bash
uv venv
source .venv/bin/activate
uv sync --group dev
ruff check .
ruff format --check .
pytest --cov=pidmesh
```

## License

MIT
