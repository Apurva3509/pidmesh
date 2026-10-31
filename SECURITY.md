# Security policy

## Reporting

Report vulnerabilities privately through GitHub's security advisory form for this repository. Do not
include secrets or private session content in a public issue.

## What PidMesh is, in security terms

PidMesh is a local tool. It opens no outbound connections, sends no telemetry, and requires no
account. Everything it records stays in a SQLite database on the machine that produced it.

Two components accept input, and they have different boundaries.

**The CLI and the MCP server** read git state and a local database. The MCP server speaks stdio to
the agent that launched it, inside that agent's own process tree.

**The dashboard** is a real HTTP server, and it is the surface worth scrutinising. It binds
`127.0.0.1` only. Every `/api/v1/*` route requires a bearer token, checked before the request body
is read, and compared in constant time. Requests whose `Host` is not the loopback authority the
server is actually bound to are refused, which is what stops a DNS-rebinding attack from reaching
it through a browser. Mutating routes additionally require a matching `Origin`, so a cross-origin
page cannot act on an agent's behalf even if the token leaks to it.

The one exception is the terminal WebSocket, which carries a single-use, session-bound ticket with
a 30-second lifetime instead of the token, because a browser cannot set an `Authorization` header on
a WebSocket handshake.

## What the trust boundary does *not* cover

These are real limits, stated plainly rather than left for someone to discover.

**A path scope is a merge-gate invariant, not a sandbox.** Managed agents run in isolated git
worktrees, and PidMesh refuses to approve changes outside an agent's declared scope. But the agent
is an ordinary process: a provider capable of writing absolute paths can still reach the rest of the
host. PidMesh reports and gates; it does not contain. Operating-system containment is future work.

**Anyone who can reach the dashboard token can do what the dashboard can do** — read files inside a
managed checkout, launch an allowlisted provider, and drive its terminal. Treat the token as
equivalent to shell access in that checkout.

**Stored data is not encrypted.** Agent messages, memories, and observed file footprints sit in
plain SQLite. The database and its default directory are created owner-only, but device security
and the choice of a custom `PIDMESH_DB` path remain yours.

**The git directory is deliberately excluded from the dashboard's file routes**, because
`.git/config` routinely holds credentials and `.git/objects` holds every version of every file ever
committed. A *hard* link to a file under `.git` would defeat that rule, since a path-based check
cannot see it; creating one requires write access to the checkout, which already implies the ability
to read `.git` directly.

## Dependencies

Dependency advisories are audited in CI on every pull request and again weekly on a schedule, since
advisories are published against dependencies nobody has touched. The configuration is `deny.toml`
and it carries no exceptions.
