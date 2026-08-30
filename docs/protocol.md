# Coordination protocol

PidMesh scopes all state to a logical project path. With no explicit override, linked git worktrees
resolve through their common git directory and share the primary repository root as that identity.
Each agent also records its actual checkout path and branch. `PIDMESH_WORKSPACE` or an explicit CLI
workspace bypasses discovery. Agent sessions get immutable IDs containing their human-readable name,
operating-system PID, and a random suffix. Names are convenient routing aliases; IDs are the stable
address.

Each Rust process holds one configured SQLite connection for its lifetime. Clones inside that process
share the connection through a mutex; independent PIDs coordinate through SQLite WAL and immediate
write transactions. The schema remains compatible with databases created by Python v0.1 and v0.2.

## Session lifecycle

1. A process joins with its PID, provider, and capabilities.
2. Activity refreshes its heartbeat.
3. A clean exit marks the session stopped and releases its claims and resource reservations.
4. Garbage collection marks a stale session dead only when its heartbeat is old and its PID no
   longer exists.

The native swarm supervisor registers every child as a separate session, updates its registration
with the real child PID, and heartbeats only workers that have not exited. A supervisor interruption
sends a termination request to every live child, waits for the configured grace period, then forces
remaining processes to exit and releases their claims and resource reservations.

The PID check avoids declaring a quiet but live local process dead. Leases still expire independently,
so abandoned work can be recovered even before garbage collection runs.

## Memory

Memories are append-only records with a kind, optional key, importance, author, and timestamp. FTS5
provides local lexical retrieval without model downloads or external calls. Later memories do not
silently rewrite an earlier agent's record.

## Messaging

Messages are either direct to an immutable agent ID or broadcast to the workspace. A name resolves to
the most recently active matching session. Broadcasts are visible only to sessions that existed when
the message was sent, and acknowledgements are stored per recipient.

## Claims

Claims use `(workspace, task key)` as their unique identity. Acquisition is one SQLite transaction:
the insert succeeds when the key is free, while takeover succeeds only after expiration. The current
owner can renew a lease. Another agent receives the existing owner instead of a false success.

## Resource reservations

Resources use a namespaced identity such as `path:src/store.rs`, `port:4399`, or
`service:test-postgres`. A reservation request may include up to 64 resources and is acquired
all-or-nothing in one immediate SQLite transaction.

Path ownership is hierarchical. `path:src` conflicts with `path:src/store.rs`, but not with
`path:src2`. Paths are normalized lexically relative to the logical project, and absolute paths or
parent traversal are rejected. Other resource kinds conflict only on an exact normalized key.

The current owner can renew a reservation. Another agent receives the conflicting owners and expiry
times without acquiring any part of its requested set. Expired rows do not appear in reads and may be
taken over immediately. Clean exits, dead-process collection, and garbage collection release or
expire reservations.

Resource reservations are cooperative collision prevention, not filesystem or network enforcement.
Participating agents must reserve intended resources before modifying or binding them. Symlink aliases
that point to the same target can still appear as distinct lexical paths.

## Event stream

Every coordination mutation appends an event with a monotonically increasing sequence. Consumers can
checkpoint a sequence and request only later events, which makes polling deterministic and cheap.
The bounded wait operation long-polls this sequence for up to 60 seconds, allowing an agent to sleep
until another session creates a memory, message, claim, resource reservation, handoff, or lifecycle
event.
