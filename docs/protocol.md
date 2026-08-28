# Coordination protocol

PidMesh scopes all state to a canonical workspace path. Agent sessions get immutable IDs containing
their human-readable name, operating-system PID, and a random suffix. Names are convenient routing
aliases; IDs are the stable address.

## Session lifecycle

1. A process joins with its PID, provider, and capabilities.
2. Activity refreshes its heartbeat.
3. A clean exit marks the session stopped and releases its claims.
4. Garbage collection marks a stale session dead only when its heartbeat is old and its PID no
   longer exists.

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

## Event stream

Every coordination mutation appends an event with a monotonically increasing sequence. Consumers can
checkpoint a sequence and request only later events, which makes polling deterministic and cheap.
