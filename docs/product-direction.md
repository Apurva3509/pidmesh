# PidMesh product direction

## The honest category view

PidMesh is not the first multi-agent workspace. The category already includes:

- [Shire](https://www.agents-shire.sh/), which provides persistent agent teams, shared files, direct messaging, reusable skills, schedules, and sleep/resume.
- [Intent](https://www.augmentcode.com/blog/intent-a-workspace-for-agent-orchestration), which combines isolated git worktrees, a living specification, coordinator/implementor/verifier roles, diffs, browsers, and git workflow.
- [Superset](https://docs.superset.sh/overview), which provides local-first worktree isolation, terminals, diffs, ports, pull requests, automations, and agent orchestration.
- [Orca](https://www.onorca.dev/), which combines worktrees, agent terminals, a browser, editor, tasks, notes, diff review, and mobile supervision.

Shared memory, chat, a task board, or parallel terminals are therefore not sufficient differentiation.

## The problem PidMesh owns

When five or more independent agent processes run locally, the missing layer is a shared coordination kernel underneath their chosen terminals and workspaces:

- Which operating-system processes are actually alive?
- Which logical project does each checkout and git worktree belong to?
- Which agent owns a task, path, port, or local service right now?
- Which ownership can be recovered safely after a crash?
- Which agent attempted conflicting work, and what context led to the handoff?
- Can Codex, Claude Code, Cursor, local models, and custom workers coordinate without adopting one vendor's agent runtime?

PidMesh answers these questions through a local Rust binary, a private SQLite WAL database, and a vendor-neutral CLI/MCP protocol.

## Positioning

PidMesh is the local coordination kernel for agent fleets.

It should complement agent development environments rather than copy them. Superset, Intent, or Orca can own terminals, worktrees, browsers, and diff review while PidMesh supplies project identity, process liveness, shared memory, messages, causal events, and atomic ownership across every participating process.

## Product principles

1. **Kernel before canvas.** Correct concurrency, recovery, and isolation come before visual workflow builders.
2. **Any harness.** The protocol must work from CLI and MCP without requiring one model provider or terminal app.
3. **One project across worktrees.** Linked git worktrees share one mesh automatically while retaining their own checkout and branch identity.
4. **Cooperative collision prevention.** Agents reserve hierarchical paths and exact resources before acting; reservations are atomic and tied to live PIDs.
5. **Local by default.** Operational data and controls remain on loopback and never require a cloud account.
6. **Observable decisions.** Ownership, contention, messages, and memory changes produce an ordered event trail.
7. **A useful cockpit, not a decorative dashboard.** The UI prioritizes attention, active work, collisions, handoffs, and shared context.

## Version 1.3: Worktree-aware collision guard

The first differentiated release adds:

- automatic logical-project discovery across linked git worktrees;
- per-agent checkout and branch visibility;
- atomic resource reservations for paths, ports, and local services;
- hierarchical path conflicts such as `path:src` versus `path:src/store.rs`;
- all-or-nothing acquisition for multi-resource work;
- lease expiry and automatic cleanup when the owning PID stops or dies;
- collision and expiring-lease visibility in the local cockpit;
- CLI and MCP operations so existing coding agents can use the guard before editing.

Resource reservations are cooperative coordination, not an operating-system sandbox. Agents must call the protocol before modifying a resource.

## Cockpit information architecture

The local cockpit answers four questions in order:

1. **What needs attention?** Dead or stale processes, expiring ownership, and failed reservations.
2. **Who is doing what?** Agent, PID, provider, checkout, branch, task claim, and resource leases.
3. **Where will work collide?** Hierarchical path reservations and exact port/service ownership.
4. **What does the fleet know?** Shared memory, messages, and the causal event timeline.

It deliberately does not execute arbitrary shell commands over HTTP. Agent launching and terminal ownership require a separately threat-modeled capability layer.

## Roadmap after 1.3

### 1.4: Causal handoffs

- Structured handoff bundles linking task, resources, memories, messages, branch, and verification evidence.
- Attention states such as blocked, review requested, and human decision required.
- Event replay that shows what an agent knew when it made a decision.

### 1.5: Resource-aware scheduler

- Named, allowlisted launch profiles.
- CPU, memory, GPU, and token-budget admission control.
- Queueing and backpressure instead of blindly starting more agents.
- Recovery policies for crashed or rate-limited workers.

### 2.0: Desktop agent operations workspace

- Native packaging around the same local Rust kernel.
- Terminal attachment, branch/diff inspection, approvals, and notifications.
- Repository and fleet switching without merging operational databases.
- Optional encrypted remote observation, disabled by default.

## Explicit non-goals

- Copying Superset's source-available desktop application.
- Rebuilding a full IDE before the coordination kernel is defensible.
- Pretending an agent's private context is shared unless the agent writes it to PidMesh.
- Generating large volumes of artificial pull requests for profile activity.
- Exposing arbitrary local command execution from the browser dashboard.
