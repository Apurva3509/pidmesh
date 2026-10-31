# Convergence performance

## Differential validation against git

Severity is a claim about what a merge will do, so it can be checked against one. Eighty generated
two-agent scenarios, each rewriting a random band of lines in a shared file: ask the mesh for a
verdict, then actually perform the merge and see.

The first run disagreed three times in forty, always in the dangerous direction — reported
`adjacent`, meaning safe to merge, and then conflicted. Every case had the two bands on
consecutive lines.

The cause was that severity asked whether the rewritten regions *overlap*, which is not the
question a three-way merge asks. It needs unchanged material between two changed regions to keep
them apart. Measured directly against git:

| unchanged lines between the edits | result |
| ---: | --- |
| 0 | conflict |
| 1 or more | merges cleanly |

With one line of separation required, eighty trials agree with git exactly: no verdict of safe
that conflicts, and no verdict of conflict that merges. The asymmetry matters more than the
symmetry — a false alarm costs a rebase nobody needed, while a false clean sends an agent into a
conflict the tool promised was not there.

## Validation on a real repository

Every figure elsewhere in this note comes from generated repositories: uniform files, trivial
history, one language. Those hide whatever real structure does. Repeated against a working
polyglot application — 253 tracked files, 105 JavaScript, 54 TSX, 30 PNG, 20 JSON, 9 Python, a
5.8 MB tracked asset, and a committed build directory — with four agents in linked worktrees:

- A sweep costs 0.280 s, in line with the synthetic numbers at that fleet size.
- Binary files are recorded with a content digest and are not harvested for identifiers, because
  they are not source extensions. A PNG rewritten with random bytes behaves like any other change.
- A renamed Python `def` is recorded as a withdrawn export. It raises no break, correctly, because
  no other checkout references it.
- Two agents appending to the same TSX file are reported `divergent`; nothing else collides.
- No false positives of any kind appeared across the four checkouts.

### Soak

150 consecutive sweeps against that repository, with a real edit introduced every tenth sweep:

| after | elapsed | database | events | footprints | symbol rows |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 30 sweeps | 2.0 s | 1.18 MB | 14 | 7 | 1955 |
| 60 | 4.0 s | 1.18 MB | 17 | 7 | 1957 |
| 90 | 6.0 s | 1.18 MB | 20 | 7 | 1957 |
| 120 | 8.1 s | 1.18 MB | 23 | 7 | 1957 |
| 150 | 10.1 s | 1.18 MB | 26 | 7 | 1957 |

Nothing drifts. The database does not grow between real changes, row counts stay flat, and
`integrity_check` passes with no foreign-key violations and no duplicate footprint rows
afterwards. Announcement is exactly one to one: six changes introduced one per sweep produced six
`footprint.published` events, which is the property a waiting agent depends on.

## Update: the first-sweep penalty no longer reproduces

The measurement below recorded a first sweep costing 2.5–4x a steady one. Re-measured after the
batching, git-resolution and idle-write work, at 48 checkouts of a 2,400-file repository:

| sweep | |
| --- | ---: |
| first | 0.332 s |
| second | 0.280 s |
| third | 0.281 s |
| fourth | 0.266 s |

About 1.2x, which is cold cache rather than anything structural. The earlier figure was taken
when a sweep wrote every footprint on every tick and resolved git through the macOS shim; both of
those are gone.

Re-measuring it did surface a reporting fault: a sweep counted the batch it submitted rather than
what the store wrote, so a tick that changed nothing reported forty-eight checkouts published.
`watch` now reports what actually moved.

## Update: the shim is now bypassed automatically

The measurement below identified `/usr/bin/git` on macOS as the largest free lever. It is no
longer something an operator has to know about: the git executable is resolved once per process,
and on macOS a shim is resolved past to the developer-tools binary it forwards to — the same
build, reporting the same version, reading the same configuration. `PIDMESH_GIT` overrides the
choice for anyone who needs a particular git.

Measured on an idle machine, 24 checkouts of a 600-file repository:

| git | one sweep |
| --- | ---: |
| `/usr/bin/git` (shim) | 0.448 s |
| resolved automatically | **0.188 s** |

A trivial invocation costs 16.4 ms through the shim against 4.8 ms direct, and a sweep makes four
per checkout per tick, which is where the difference comes from.


This note measures the convergence layer — the `pidmesh watch` sweep and the `pidmesh collisions`
report it produces — and says where it stops keeping up. It is deliberately unflattering: the
interesting number is the one where the thing breaks, not the one where it looks good.
`pidmesh mergeable` is *not* measured here; see "What this note does not cover".

Everything below is a median of repeated runs on one machine. The benchmark that produced it ships
in the repository, so the numbers can be re-measured rather than believed.

## In short

On an 8-core M1 laptop, a `pidmesh watch` sweep costs about **42 ms per agent checkout**, which puts
the default 5-second interval out of reach at roughly **120 agents** on a 2 000-file repository —
and at **70** on an 8 000-file one, or **32** if every worktree holds a thousand changed files.
Fleet size dominates; changed files and repository size are second-order.

About **96 % of that is git**, and most of the git time is not git doing work but git *starting*:
five processes per checkout per tick at a 16–19 ms floor each. SQLite is 1–10 % of a sweep and the
batched publish has left it there.

Idle sweeps write **zero** bytes, which is exactly as advertised. The event ledger's 20 000-row cap
holds exactly — but it bounds rows, not bytes, and only while a watcher is running.

The thing that breaks it is a large generated file in every worktree: that truncates each
checkout's reference set, which makes every checkout match every withdrawn export, which takes a
128-agent sweep from 8.6 s to 12.3 s and the database from 31 MB to **367 MB**.

## Method

`examples/converge_bench.rs` builds synthetic repositories and linked git worktrees under a
scratch directory, registers one agent session per worktree, and then replays the sweep
`pidmesh watch` performs — `live_checkouts`, a parallel `scan_worktree` per distinct checkout on a
pool sized the way the watcher sizes it, one batched `publish_footprints`, `collect_stale`, and
`collisions` — timing each phase separately.

It is a second example binary rather than more cases inside `examples/benchmark.rs` because the two
measure different things. The existing benchmark exercises storage hot paths against a database and
nothing else, and finishes in under a second; this one has to build hundreds of real git worktrees,
and takes tens of minutes. Folding them together would have made the quick one slow.

```
cargo build --release --example converge_bench
./target/release/examples/converge_bench --scratch /tmp/pm-bench --suite all --reps 5
```

Suites are `scale`, `phases`, `growth`, `pathological`, or a comma-separated subset; `--ticks` sets
the sweeps per phase in the growth suite, and `--keep` leaves the fixtures on disk. Each sweep
figure is the median of five repetitions after a discarded warm-up sweep; each pathological case is
the median of three. Fixtures are rebuilt from scratch for every scenario and deleted afterwards.
The whole matrix takes roughly forty minutes and about 1 GB of scratch space; the figures below
come from `--suite scale --reps 5` and `--suite phases,growth,pathological --reps 5 --ticks 60`.

### Measuring git against SQLite

Nothing inside `src/converge.rs` is instrumented — an instrumented build is not the build anybody
runs — so the split is measured from outside, two ways that have to agree:

1. **By phase.** `live_checkouts`, `publish_footprints`, `collect_stale` and `collisions` contain no
   git at all. Their summed wall time is the SQLite share of a sweep outright.
2. **By replay, inside the scan phase.** The benchmark re-issues the exact argv list
   `scan_worktree` uses — `rev-parse --abbrev-ref HEAD`, `rev-parse --verify --quiet <base>`,
   `merge-base`, the `diff --unified=0` and the `status --porcelain=v1 -z -u all`, each with
   `-c core.quotePath=false` — against the same checkout, and sums their wall time. Subtracting that
   from a serial `scan_worktree` on the same checkout leaves the in-process work: diff parsing,
   `digest_of`, and `referenced_symbols`.

The replay runs after the real scan has already warmed the page cache, so it under-reports git
slightly. Read the git share as a lower bound.

A third measurement separates git's start-up cost from the work git then does, by timing the
cheapest possible invocation (`git rev-parse HEAD`) in the same loop. This matters more than
expected, and is the main result below.

### Machine

Apple M1, 8 cores, 8 GiB RAM, macOS 26.6.2, APFS on internal SSD, git 2.50.1 (Apple Git-155),
release profile (`lto = "thin"`, `codegen-units = 1`). Other work was running on the same machine
during parts of the run, which is one reason every figure is a median rather than a best-of.
Absolute milliseconds will differ on other hardware; the ratios are the portable part.

## How a sweep scales

Steady-state sweep, median of five, 8 worker threads. "Scan" is the parallel observation phase;
"SQLite" is every other phase summed.

| Scenario | Repo files | Checkouts | Changed each | Scan (ms) | SQLite (ms) | **Sweep (ms)** | First sweep (ms) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline, small fleet | 2 000 | 8 | 10 | 240 | 18 | **258** | 659 |
| baseline, medium fleet | 2 000 | 32 | 10 | 888 | 25 | **926** | 3 652 |
| baseline, large fleet | 2 000 | 128 | 10 | 4 902 | 61 | **4 970** | 14 775 |
| dirtier worktrees | 2 000 | 32 | 100 | 1 218 | 64 | **1 280** | 3 266 |
| very dirty worktrees | 2 000 | 32 | 1 000 | 4 538 | 494 | **5 022** | 5 843 |
| small repository | 500 | 32 | 10 | 820 | 42 | **870** | 1 273 |
| large repository | 8 000 | 32 | 10 | 2 233 | 26 | **2 255** | 8 771 |

**Checkout count dominates, and it is close to linear.** Going from 8 to 128 checkouts is a 16x
increase in input and a 19x increase in sweep time. The marginal cost of one more checkout is about
28 ms between 8 and 32, and about 42 ms between 32 and 128 — it gets slightly *worse* with fleet
size, because eight worker threads are already saturated and additional checkouts simply queue.

The other two axes are much weaker. A 100x increase in changed files per checkout (10 to 1 000)
costs 5.4x. A 16x increase in repository size (500 to 8 000 files) costs 2.6x. Neither is free, but
neither competes with fleet size.

Two things in that table deserve to be called out rather than buried.

**The first sweep is not the sweep you will live with.** A watcher starting against a fleet that is
already running pays 2.5x to 4x a steady sweep — 14.8 s for 128 checkouts against 5.0 s afterwards.
Part of that is writing every footprint for the first time; most of it is cold page cache across a
hundred-odd worktrees. It is a one-off, but it is a one-off that lands exactly when an operator is
watching, and `watch` gives no indication that the first tick is special.

**Repository size only hurts in parallel.** Scanning one 8 000-file checkout serially with a warm
cache costs 117 ms — statistically the same as a 2 000-file checkout (119 ms) or a 500-file one
(112 ms). Yet sweeping 32 of them costs 2 233 ms against 888 ms. The difference is not git getting
slower per checkout; it is eight workers walking 256 000 files at once and falling out of cache. The
parallel speed-up over serial scanning degrades from 4.3x at 2 000 files to 1.7x at 8 000. Measuring
one checkout tells you nothing about this, which is why the matrix varies the repository separately.

## Where the time actually goes

Prior profiling said git dominates after the batched-publish optimisation. That is confirmed, and
the margin is larger than "dominates" suggests. It is also, on macOS, mostly not git's fault.

### SQLite is a rounding error until the diffs get big

The phase split needs no estimation: `live_checkouts`, `publish_footprints`, `collect_stale` and
`collisions` contain no git at all, so their summed time *is* the SQLite share.

| Scenario | Scan (ms) | SQLite (ms) | SQLite share |
| --- | ---: | ---: | ---: |
| 2 000 files, 8 checkouts, 10 changed | 240 | 18 | 7.1 % |
| 2 000 files, 32 checkouts, 10 changed | 888 | 25 | 2.7 % |
| 2 000 files, 128 checkouts, 10 changed | 4 902 | 61 | 1.2 % |
| 2 000 files, 32 checkouts, 100 changed | 1 218 | 64 | 5.0 % |
| 2 000 files, 32 checkouts, 1 000 changed | 4 538 | 494 | 9.8 % |
| 8 000 files, 32 checkouts, 10 changed | 2 233 | 26 | 1.2 % |

The batched publish did its job: a 128-checkout sweep spends 61 ms in the database, of which 15 ms
is the publish transaction itself. SQLite only becomes visible when footprints get large — at 1 000
changed files per checkout across 32 checkouts the store holds 32 000 footprint rows and writes
roughly 160 000 symbol rows, and the database share climbs to 10 % (494 ms, split 293 ms publish
and 169 ms collision reporting). Even there it is a fifth of the scan.

### Inside the scan, git is 73–99 %

Replaying `scan_worktree`'s exact git invocations serially against one checkout, and subtracting
from a serial `scan_worktree` on that same checkout:

| Scenario | Serial scan (ms) | git (ms) | In-process (ms) | git share |
| --- | ---: | ---: | ---: | ---: |
| 2 000 files, 10 changed | 119 | 116 | 3 | 97 % |
| 2 000 files, 100 changed | 123 | 117 | 6 | 96 % |
| 2 000 files, 1 000 changed | 362 | 284 | 78 | 79 % |
| 500 files, 10 changed | 112 | 106 | 5 | 95 % |
| 8 000 files, 10 changed | 117 | 115 | 1 | 99 % |

In-process work — diff parsing, `digest_of`, `referenced_symbols` — only becomes measurable when a
checkout changes a thousand files, and even then it is a fifth of the scan. For an ordinary dirty
worktree it is single-digit milliseconds.

### Most of the git time is not git working — it is git starting

This is the result worth acting on. Timing the cheapest possible invocation inside the same loop
puts the floor at 16–19 ms per git process, consistently, in every scenario of both suites.
`scan_worktree` issues five git commands per checkout, so roughly 80–95 ms of the 96–120 ms git cost
of scanning a quiet 2 000-file checkout is process start-up. git's actual work is the remaining
10–25 ms.

An independent probe outside the benchmark, comparing against a non-git process:

```
/usr/bin/true                             3.7 ms   (fork/exec floor)
/usr/bin/git --version                   19.5 ms   (no repository work at all)
/usr/bin/git rev-parse HEAD              23.6 ms
/usr/bin/git status --porcelain (2k)     60.5 ms
```

`git --version` opens no repository and still costs 19.5 ms against a 3.7 ms fork/exec floor. On
macOS, `/usr/bin/git` is not git: it is a stub that resolves the active developer directory and then
execs the real binary. Timing both:

```
                                  --version   rev-parse   status
/usr/bin/git (shim)                 57.2 ms     55.1 ms   68.0 ms
/Library/Developer/CommandLineTools/usr/bin/git
                                    15.7 ms     16.9 ms   32.6 ms
```

(Measured with other work on the machine, which is why both columns are higher than the idle
figures above; the ratio is the point. The shim is also markedly more sensitive to load than the
real binary.)

The in-benchmark floor is reported as `git_spawn_floor_ms` in every scenario, so it needs no
special procedure. The shim comparison is outside the benchmark and is reproduced by timing 40
runs of each of the two binaries in a loop — `/usr/bin/git` against
`/Library/Developer/CommandLineTools/usr/bin/git` (or `xcode-select -p` plus `/usr/bin/git`) —
with `/usr/bin/true` timed the same way as the fork/exec floor to subtract the harness.

`converge::git_bytes` runs `Command::new("git")`, which finds the shim through `PATH`. A macOS user
who puts a real git binary ahead of `/usr/bin` in `PATH` should see a materially cheaper sweep
without changing a line of pidmesh. That is worth saying plainly because it is the single largest
lever available and it costs nothing.

### Confirming it at fleet scale

A second suite forces a real write every tick — 50 changed files in every checkout, re-dirtied
between sweeps, so the publish transaction never short-circuits on an unchanged footprint:

| Checkouts | Sweep (ms) | Scan (ms) | SQLite (ms) | of which publish | Estimated git (ms) | git ÷ SQLite |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 8 | 264 | 240 | 23 | 5 | 234 | **10x** |
| 32 | 931 | 888 | 44 | 23 | 843 | **19x** |
| 128 | 4 674 | 4 509 | 177 | 124 | 4 369 | **25x** |

The ratio widens with fleet size: git is not merely dominant, it is pulling further ahead. The
128-checkout row carries 6 400 footprint paths and 2 000 contested paths and still spends 96 % of
the sweep in git. Prior profiling was right, and the batched publish is not the next thing to
optimise — the next thing is issuing fewer processes.

## The ceiling on a laptop

`pidmesh watch` defaults to a 5-second interval and sleeps *after* each sweep rather than on a
fixed schedule, so it does not fall behind in the queueing sense — it simply observes the fleet less
often than it claims to. The useful ceiling is the fleet size at which one sweep reaches 5 seconds,
because past that the advertised interval is a fiction.

Dividing each measured steady-state sweep by its number of checkouts gives the cost of one agent on
this machine, and 5 000 ms divided by that is the ceiling:

| Workload | Measured sweep | ÷ checkouts | **Agents at a 5 s interval** |
| --- | ---: | ---: | ---: |
| 2 000-file repo, ~10 changed files each | 4 970 ms / 128 | 39 ms | **≈ 128** |
| 2 000-file repo, ~100 changed files each | 1 280 ms / 32 | 40 ms | **≈ 125** |
| 8 000-file repo, ~10 changed files each | 2 255 ms / 32 | 70 ms | **≈ 71** |
| 2 000-file repo, ~1 000 changed files each | 5 022 ms / 32 | 157 ms | **≈ 32** |

**The headline number is about 120 agents**, and the top row is measured rather than extrapolated:
128 checkouts on a 2 000-file repository sweep in 4 970 ms, which is the 5-second interval almost
exactly. Call it 120 rather than 128 because the marginal cost rises with fleet size — 28 ms per
checkout between 8 and 32, 42 ms between 32 and 128 — so the last few agents cost more than the
average. That is also the optimistic end: a modest repository and modestly dirty worktrees.

The pessimistic end is more useful for planning. On an 8 000-file repository the ceiling is about
70 agents, and if the fleet keeps a thousand changed files per worktree it is about 32. A realistic
large repository with realistically messy agents will sit in the 30–70 range, not at 120. The
existing 24-and-96-agent figures sit comfortably inside that, which is presumably why nothing broke.

Three caveats, in the order they will bite:

* **This is eight cores.** Scanning parallelism is `available_parallelism()`, so the ceiling scales
  with core count until I/O takes over, and a four-core laptop halves it.
* **The watcher is not the only thing running.** These sweeps had the machine mostly to themselves.
  A hundred agent processes compiling and running tests do not leave eight cores free, and the
  watcher competes with the very fleet it is observing.
* **Raising the interval is the cheap fix.** At `--interval-seconds 15` the same measurements put
  the ceiling near 360 agents on a small repository. Nothing about convergence requires 5 seconds;
  it is the time before a collision is noticed, not a correctness bound.

## How large the database gets

Measured with 32 checkouts of a 2 000-file repository, 50 changed files each, 60 sweeps per phase.

**Idle sweeps really do write nothing.** Sixty consecutive sweeps over an unchanged fleet grew the
database by **0 bytes** — not "a little", zero. `footprint_is_current` short-circuits before the
write and no event is appended, so a watcher left running overnight against a quiet fleet costs
nothing but CPU. This is the single best-behaved thing in the convergence layer.

**Churn costs about 75 KB per sweep** at this fleet size, when every one of the 32 checkouts
rewrites 50 files between ticks. At a 5-second interval that is 54 MB/hour — but only while every
agent is editing continuously, which is not a thing that happens for an hour.

That rate does not continue forever, because the event ledger is capped at 20 000 rows per
workspace. The cap holds exactly:

| | |
| --- | --- |
| Events appended without a sweep | 61 986 (no trimming happened at all) |
| Events after one `collect_stale` | **20 000** |
| Cost of that one catch-up trim | 121 ms |
| Cost of trimming an at-ceiling ledger | 2.1 ms per tick |

Two honest caveats about what that cap does and does not buy.

**The cap bounds rows, not bytes.** Trimming 100 000 events made the files *larger*, not smaller:
30.3 MB before the trim, 35.0 MB after. SQLite frees pages for reuse rather than returning them,
the delete transaction inflates the write-ahead log, and nothing in pidmesh ever runs `VACUUM`. The
database is a high-water mark. It stops growing; it does not come back down.

**The cap is only enforced while something is sweeping.** Trimming lives inside `collect_stale`,
which `watch` calls once per tick. A workspace generating events through `sync`, `claim` and
`remember` with no watcher running accumulated 62 000 rows before anything pruned them. The ledger
is bounded in practice because a watcher is normally running, not because appending is bounded.

For a realistic 32-agent fleet, extrapolating the measured 75 KB/tick against the measured 20 000
row ceiling, the plateau is in the region of 45–50 MB, reached after roughly an hour of
continuous maximum churn. That last figure is an extrapolation, not a measurement: running to the
plateau takes an hour of synthetic thrashing and the benchmark does not do it. The measured parts
are the per-tick rate, the zero idle rate, and the row ceiling.

## Pathological inputs

Each case is one checkout unless stated, median of three.

| Case | Scan (ms) | What happens |
| --- | ---: | --- |
| One untracked 32 MiB file | 133 | Read and digested in full, every tick |
| One untracked 80 MiB file | 92 | *Cheaper*, because 64 MiB is the hashing ceiling |
| 20 000 tiny untracked files | **1 796** | Plus 173 ms to publish and 16 MB of database, for one checkout |
| 50 files at directory depth 174 | 117 | Nothing. `PATH_MAX` caps the depth before pidmesh notices |
| One 1.9 MiB source file, 114 000 identifiers | 118 | Fast, but the reference set truncates (see below) |
| 3 000-commit history, branch cut at the root | 215 | `merge-base` alone costs 128 ms, roughly doubling the scan |

Three of these deserve comment.

**The size ceilings work, and the 80 MiB file is the proof.** It is faster to scan than the 32 MiB
one precisely because it is over the limit: `digest_of` refuses it, so it is reported as divergent
without being read. The conservative direction also happens to be the fast one.

**A deep directory tree is not a pathology**, because the operating system gets there first. The
benchmark asks for an unbounded tree and `PATH_MAX` stops it at 174 levels, which costs nothing
measurable. This one is a non-finding and is recorded as such.

**A long history is a real but modest cost.** A worktree cut from the root of a 3 000-commit branch
makes `merge-base` walk the whole history, 128 ms against a 16 ms process floor. It roughly doubles
a scan and is paid every tick, because nothing caches the merge base between sweeps.

### The one that actually blows up

Path-level collision detection stays cheap at every fleet size. The symbol layer does not. With
128 checkouts all depending on a shared 400-export module and one of them withdrawing half of it:

| | Normal references | Reference set truncated |
| --- | ---: | ---: |
| Scan | 8 064 ms | 7 743 ms |
| Publish | 516 ms | **3 466 ms** |
| `collisions` report | 34 ms | **1 083 ms** |
| Whole sweep | 8 618 ms | **12 316 ms** |
| Database | 31 MB | **367 MB** |

Read across the two columns, not against the scaling table earlier: both were measured on the same
fixture shape back to back, so the difference between them is the effect. The absolute scan figures
are higher than the equivalent scaling scenario because this fixture gives every checkout a large
changed file, which is the whole point of the case.

The trigger is `MAX_REFERENCES`. A checkout whose changed files contain more than 5 000 distinct
identifiers has its reference set truncated, and a truncated checkout is deliberately treated as
referencing *everything*, so it matches every withdrawn export in the workspace. Do that across a
whole fleet and three things happen at once: each checkout stores 5 000 symbol rows (640 000 rows
for the fleet, hence 367 MB), the publish transaction takes 3.5 seconds, and the symbol-break query
— a self-join whose `reference_truncated` branch cannot use the index — takes a second on its own.

A single generated file is enough to cause this. One 1 MiB file of generated bindings in each
worktree took a 8.6-second sweep to 12.3 seconds and a 31 MB database to 367 MB.

The reported blocker count is capped at 200 (`MAX_SYMBOL_BREAKS`) in every case above, so the
operator sees the same 200 rows whether the join examined two thousand candidates or two million.
The cost is paid regardless of what is shown.

## What to do, and the one thing not to do

**Do not let generated files into agent worktrees.** This is the specific recommendation. A fleet
with one large generated or vendored source file per checkout pays for it three times over: the
reference set truncates, every checkout starts matching every withdrawn export, the symbol table
grows to hundreds of thousands of rows, and a sweep that was 8 seconds becomes 12. Add the paths to
`.gitignore`, or generate into a directory with an extension `SOURCE_EXTENSIONS` does not list.
Twenty thousand untracked files in one checkout is the same mistake in a different shape: 1.8
seconds of sweep and 16 MB of database for a single agent, for files nobody is coordinating on.

Worth doing, in rough order of payoff:

1. **On macOS, get the git shim out of the way.** `/usr/bin/git` costs 15–40 ms per invocation over
   the real binary, and a sweep issues five per checkout per tick. This is the largest available
   win and it is a `PATH` change.
2. **Raise `--interval-seconds` before raising anything else.** The default 5 seconds is a latency
   choice, not a correctness one. At 15 seconds the measured ceiling moves from ~120 agents to
   ~360 on a small repository.
3. **Keep worktrees clean.** A thousand changed files per checkout costs 5x a sweep over ten, and
   is also where SQLite finally becomes visible at 10 % of the sweep.
4. **Expect the first sweep to be slow.** 14.8 seconds against 5.0 steady at 128 checkouts.
   Nothing in `watch`'s output says the first tick is different, so it reads as a hang.

## What this note does not cover

* `pidmesh mergeable` is not benchmarked. It recomputes the workspace collision set and then does
  pairwise `classify` work per contested path, which is the one shape in the convergence layer that
  looks quadratic and has no measurement behind it. That is a gap, not a clean bill of health.
* Everything here is one machine, one filesystem, one git build. The ratios should travel; the
  milliseconds will not.
* The fleet in these measurements does nothing but hold dirty files. Real agents compile and run
  tests, and the watcher competes with them for the same eight cores.

## A bug found while measuring

The 2 MiB read ceiling in `converge::referenced_symbols` is bypassed by a symlink, so a symlinked
source file of any size is read into memory in full on every sweep.

The guard is `fs::symlink_metadata(&absolute).is_ok_and(|meta| meta.len() > MAX_SCANNED_BYTES)`.
For a symlink, `symlink_metadata().len()` is the length of the link's *target string*, not of the
file it points at, so the check passes trivially. The following `fs::read_to_string` then follows
the link. `digest_of` in the same module does not have this problem, because it rejects anything
that is not a regular file; the two readers of the same path disagree.

To reproduce: make a 300 MiB `.rs` file outside a repository. In one fresh checkout copy it to
`src/linked.rs`; in another, symlink it there. In each, `pidmesh join` then `pidmesh sync`, and
record the peak resident set of the `sync` process (`/usr/bin/time -l`, or `ru_maxrss` for the
child). Run the copy first — peak-RSS accounting over children is a high-water mark, so a cheap run
measured after an expensive one reads as zero whatever it used.

```
copied in place      lstat size 300.4 MiB   peak RSS of `pidmesh sync`:   0.2 MiB
symlinked            lstat size 133 bytes   peak RSS of `pidmesh sync`: 534.7 MiB
```

Both exit zero. The control shows the ceiling working as intended; the symlink shows it skipped
entirely. In a `watch` sweep this is paid every tick, and the scan phase runs eight checkouts in
parallel. The fix belongs in `src/converge.rs`, which this note's author was not permitted to
touch; `referenced_symbols` needs the same `metadata.is_file()` test `digest_of` already applies,
or to measure the resolved target rather than the link.

