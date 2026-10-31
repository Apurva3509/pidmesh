# Coordination protocol

PidMesh scopes all state to a logical project path. With no explicit override, linked git worktrees
resolve through their common git directory and share the primary repository root as that identity.
Each agent also records its actual checkout path and branch. `PIDMESH_WORKSPACE` or an explicit CLI
workspace bypasses discovery. It names the mesh root only: the checkout an agent observes is its
process directory whenever that directory belongs to the same repository, or `PIDMESH_CHECKOUT`
when a spawner names it outright. Agent sessions get immutable IDs containing their human-readable name,
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

A registration records the checkout it was made from, as `checkout_path`, separately from the mesh
root. The two are different questions: the root is the project every agent shares, while the
checkout is the worktree this agent is editing. A spawner that starts an agent somewhere other than
the directory it will work in names the checkout outright; otherwise it is the process directory
whenever that directory belongs to the same repository. Conflating them publishes a footprint for a
directory nobody is editing, which reports no collision at all.

A session is only observable while its process is alive, so a registration says where its PID came
from. `pid_source` is `given` when the caller named one and `assumed` when it was taken from the
parent, and a registration whose PID is not a running process is refused rather than recorded. An
assumed PID is the common mistake: a shell that captures the agent id through command substitution
exits as soon as the call returns, so the registration is dead on arrival. Where that is possible
the answer carries a caution in `warnings`, and a later publish reports `pid_alive` so a footprint
is never mistaken for one the mesh can still watch.

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

## Footprints and collisions

A reservation is a declaration of intent. A footprint is an observation of fact: the set of paths a
checkout has actually changed relative to an integration base, derived entirely from git and
requiring no cooperation from the agent beyond pointing the scan at a directory.

A scan resolves the integration base (`main`, then `master`, unless one is named), takes the merge
base against `HEAD`, and unions committed changes since that base with uncommitted changes including
untracked files. Uncommitted state wins where both describe the same path.

The filesystem, not the git status letter, decides whether a path still exists: git reports a
staged file that has since been deleted as added, and a path can vanish between the status call and
the scan, so a reported path that is absent is recorded as deleted.

Each surviving path is fingerprinted from its bytes so identical content is distinguishable from
divergent content. The digest is mesh-internal and is not a git object id; it only ever has to
answer whether two checkouts hold the same content. Anything without comparable content carries no
digest — a deletion, a directory, a submodule, a nested repository, a symlink, an unreadable or
oversized file, or a path whose name is not valid UTF-8 — and a missing digest is treated as
divergent, which is the conservative direction. One unreadable path never fails a scan.

Publishing a footprint replaces every row previously recorded for that agent in one immediate
transaction. The footprint is therefore authoritative: withdrawing a change removes the agent from
that path's contention on its next scan, and an agent that merges and cleans its checkout leaves
every collision automatically.

Contention is keyed on checkout rather than agent, in the participant list as well as in the test
for whether a path is contested. A worker that runs both a CLI session and an MCP session against
one worktree is a single editor of that path, and only its most recent observation counts;
otherwise a stale footprint from a co-located session would argue with its own checkout. A path
changed in more than one checkout is classified by merge outcome rather than by lock
ownership. `identical` means every participant reached the same outcome: the same content hash, or every
checkout deleting the path. `delete_edit` means at least one participant removed the path while
another still edits it, and outranks content comparison.

Differing content in one path is not automatically a conflict. A scan also records which regions
of the *base* file each checkout rewrote, in base-file line numbers rather than working-tree ones,
because a three-way merge conflicts when two sides rewrote overlapping regions of the common
ancestor. When every pair of participants rewrote disjoint regions the collision is `adjacent`:
real contention on a shared file that git will nonetheless merge cleanly. Everything else is
`divergent`. Regions are taken from a single diff of the base commit against the working tree,
which already unions committed and uncommitted work. A pure insertion is recorded as the single
base line it sits against, so two insertions at the same point still register as overlapping, and
a path whose regions are unknown is assumed to overlap — a downgrade must be provable. Collisions separately report whether the participants
cut their checkouts from different base commits, because a stale base is how a textually clean merge
still produces incorrect behaviour.

Collisions are fingerprinted by severity and participant set. A fingerprint that appears or changes
appends `collision.detected`; one that disappears appends `collision.cleared`. Unchanged state
appends neither, so agents polling the mesh in a loop generate no event churn while waking
immediately on a real overlap.

Footprints survive an agent being marked stopped, because PidMesh deliberately preserves worktrees
and their uncommitted work. Collision reports carry each participant's session status so a reader
can tell live contention from abandoned contention. Garbage collection removes a dead agent's
footprint only once its checkout no longer exists on disk, at which point the contention it
described cannot be real. An agent may also withdraw its footprint explicitly, which is the only
option available to a session that is shutting down and can no longer produce a scan.

Because every checkout is recorded at registration, a supervisor can publish footprints on behalf
of a fleet that never calls the protocol itself. Observation therefore requires no agent
participation at all, unlike a reservation, which is the property that makes convergence hold for
agents launched by an arbitrary harness. A sweep scans each distinct checkout once regardless of
how many sessions occupy it, skips checkouts that have been removed, and ignores sessions whose
process is gone.

## Sweeps

An agent publishes its own footprint when it calls the protocol. A sweep publishes for a fleet that
never will: every checkout recorded at registration is scanned by one supervisor, which is what lets
the guard hold for agents launched from a bare terminal or another vendor's harness.

Because a sweep answers for checkouts it does not control, it reports how much of its answer it
actually observed. `observation` is `complete` when every checkout scanned, `partial` when some did
not, `blind` when none did, and `idle` when there was nothing registered to scan. A sweep that
observed nothing does not report an empty workspace: collisions are computed from stored footprints
rather than from the scan, so the mesh can still say what it last knew, and `last_observed_at` dates
that answer. What it must not do is present a recalled answer as a current one, so the clean result
is withheld entirely when nothing was observed.

`observed` counts sessions and `observed_checkouts` counts checkouts; two agents sharing one
worktree are two of the first and one of the second. A checkout that could not be scanned is named
in `skipped` with its reason, and a scan result that cannot be accounted for at all is recorded
there too rather than silently dropped.

A sweep that finds no checkouts at all asks why before answering. An empty mesh and a directory
git will never read produce the same silence otherwise, so `workspace_problem` carries the
diagnosis — no git on the path, not a repository, or a repository with no commits. The probe costs
one `git rev-parse` and runs only on the empty path, so a busy fleet never pays for it.

A sweep also compares what git knows against what the mesh watches. `git_worktrees` counts the
checkouts git reports, and `unwatched_worktrees` names those with no agent registered — the gap
between the two being the most useful thing the guard can tell an operator who has not joined
anything yet. A probe that cannot ask git is recorded in `skipped` rather than reported as an
absence of worktrees.

## Symbol footprints

Paths answer who is touching what. They cannot answer whether two agents working on different
files have broken each other, which happens when one withdraws an exported name the other calls:
the merge is textually clean and the result does not build.

A scan therefore also records two sets per checkout. The withdrawn set holds exported names
declared on a line the diff removed and not reintroduced on a line it added, so an edited
signature — deleted and re-added under the same name — is not a withdrawal. The reference set
holds identifiers appearing in the checkout's changed files, capped so a generated file cannot
flood the mesh, and filtered by a short stoplist of language keywords.

Recognising an export is a heuristic covering the common declaration forms of Rust, TypeScript,
JavaScript, Python and Go. It yields nothing when unsure. The asymmetry is deliberate: a missed
warning costs an unreported break, while a wrong one costs a false alarm against work that was
fine, and false alarms are what teach an operator to stop reading the output.

References are harvested from a checkout's changed files only. Measured: when the usage lives in
a file neither agent touched, the removing agent's own branch already fails to compile, so a build
finds it without any coordination. When the usage lives in a file the peer changed, both branches
compile, the merge is clean, and the result does not build — which is the case nothing but the
mesh can see. Scanning the whole tree would re-find the first and not improve the second.

A withdrawn name that another checkout references is a symbol break. It is attributed to the
checkout doing the removing rather than the one doing the calling, because that is the side whose
merge introduces the failure, and it is matched across checkouts rather than agents so two
sessions in one worktree never break each other.

A report of breaks is capped. The cap bounds the rows returned, not the question asked: the query
reads one row past it, so `symbol_breaks_truncated` distinguishes a report that stopped early from
one that found everything. `symbol_break_count` counts the rows returned, which is why it cannot be
read as a total once that flag is set. Reporting surfaces carry every break in the workspace; the merge gate
narrows to the asking agent, and it narrows inside the query rather than after the cap, so a peer
with many breaks of its own cannot push a real blocker out of the window.

## Merge ordering

A collision report states that two checkouts disagree. Merge readiness decides whether one of them
may land. The caller supplies the integration branch's current head, because resolving a ref is a
git question rather than a mesh one, and the mesh compares it against the commit the worktree was
cut from.

Three conditions block a merge. A stale base means the integration branch advanced after this
worktree was cut, so the diff may apply cleanly and still be wrong. A contested path means another
live checkout rewrote an overlapping region of a path this one changed. A held integration lease
means another agent is merging at this moment.

Contention is judged per pair rather than per path. Four checkouts can share one file while only
two of them overlap, and the severity of the path as a whole says nothing about whether any
particular agent may land. Each blocker therefore names the specific peers that conflict with the
asking agent, and an agent that overlaps nobody merges even while the path it touched is
contested by others.

Two things deliberately do not block. Byte-identical content is duplicated effort, so merging
either copy is safe, and so is an `adjacent` collision, which is precisely the case three-way
merging exists to resolve. A peer that is no longer running does not block either: its preserved worktree
is still reported as a collision, but it cannot be asked to rebase, and treating it as a blocker
would deadlock every later merge behind an agent that has already exited.

The integration lease is an ordinary claim under a reserved task key rather than a new mechanism.
It therefore inherits exactly-one-owner semantics, lease expiry so a crashed holder cannot wedge
the queue permanently, and release through the existing agent lifecycle.

## Reclaiming space

Deleting rows marks pages free inside the database; it does not return them to the filesystem, so
a mesh that has trimmed a large ledger stays at its high-water mark. Reclaiming rewrites the whole
database, which is far too expensive for a watcher to do on a tick, so it belongs to garbage
collection: occasional, explicit, and skipped entirely unless enough of the file is free to be
worth the rewrite. Owner-only permissions are reapplied afterwards, because a rewrite creates a
new file.

## Event stream

Every coordination mutation appends an event with a monotonically increasing sequence. Consumers can
checkpoint a sequence and request only later events, which makes polling deterministic and cheap.
The bounded wait operation long-polls this sequence for up to 60 seconds, allowing an agent to sleep
until another session creates a memory, message, claim, resource reservation, handoff, or lifecycle
event.

## Managed worktree reclamation

Every managed run gets its own checkout and branch, and preserving dirty or failed worktrees is a
stated product invariant: cleanup on `stop`, `reject`, `approve`, or shutdown would destroy
uncommitted agent work, which is an explicit non-goal. The cost of that invariant is that disk use
grows with every run forever, including runs whose worktree a later dashboard process knows nothing
about.

The protocol splits the problem in two. Reporting is continuous and free of side effects: the
managed root is inventoried from `git worktree list`, not from the session table, so a worktree
orphaned by an earlier process is listed on the same footing as a live one, each with the reason it
is being kept. Removal is a separate, explicit operation that nothing in the run lifecycle calls.

A worktree is reclaimable only when both places it can hold work are provably redundant: its
working tree has no modified or untracked files, and every commit on its branch is already an
ancestor of the primary checkout's `HEAD`. The test is observational rather than declarative, so it
never trusts a recorded run status; an approved and merged run simply satisfies both halves on its
own. Removal then runs `git worktree remove` and `git branch -d` without their force flags, so git
independently re-checks cleanliness and merge status and refuses anything that still holds work.
That second opinion matters because a worktree is classified a moment before it is removed, and a
stray process may write into it in between.

A worktree an in-memory run still owns is retained regardless of how clean it looks, because it
backs a Files, Diff, and Terminal pane the operator may be about to open. Only `merged` and
`rejected` count as decided; `failed` and `stopped` do not, because no human has looked at them yet.
