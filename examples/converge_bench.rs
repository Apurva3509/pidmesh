//! Convergence-layer benchmark: how a `pidmesh watch` sweep scales and where its time goes.
//!
//! `examples/benchmark.rs` covers the storage hot paths — memory writes and FTS recalls — against
//! a database and nothing else. The convergence layer cannot be measured that way: a sweep is
//! mostly git subprocesses and file I/O against real checkouts, so the fixture *is* the
//! experiment. That is why this is a second binary rather than more cases inside the first: the
//! existing benchmark stays a sub-second smoke test anybody can run, while this one builds
//! hundreds of git worktrees and takes minutes.
//!
//! # What is measured
//!
//! One sweep here is the same work `watch_pass` does, phase by phase:
//!
//! 1. `live_checkouts` — one `SQLite` read, plus a liveness check per registered agent.
//! 2. scan — `converge::scan_worktree` per *distinct* checkout, on a thread pool sized exactly as
//!    the watcher sizes it. This is git subprocesses, file reads, digests and symbol harvesting.
//! 3. `publish_footprints` — one `SQLite` write transaction for the whole sweep.
//! 4. `collect_stale` — the reap the watcher performs every tick.
//! 5. `collisions` — the report the watcher prints.
//!
//! # How git is separated from `SQLite`
//!
//! Nothing inside `converge` is instrumented, because this benchmark is not allowed to modify it
//! and an instrumented build would not be the build users run. Instead the split is measured from
//! the outside, by subtraction:
//!
//! * The `SQLite` share is phases 1, 3, 4 and 5, which contain no git at all.
//! * Within phase 2, the git share is measured by replaying the exact argv list `scan_worktree`
//!   issues — same arguments, same `-c core.quotePath=false`, same order — against the same
//!   checkout and summing the wall time. Whatever is left of a serial `scan_worktree` call is
//!   in-process work: diff parsing, `digest_of`, and `referenced_symbols`.
//! * A spawn floor is measured separately with the cheapest possible git invocation, so the fixed
//!   cost of starting a process can be told apart from the work git then does.
//!
//! The replay is an estimate, not an instrumented trace: it pays cache effects the real scan has
//! already paid, so it slightly *under*-reports git. Treat the git share as a lower bound.
//!
//! # Running it
//!
//! ```text
//! cargo run --release --example converge_bench -- --scratch /tmp/pm-bench --suite all --reps 5
//! ```
//!
//! Suites are `scale`, `phases`, `growth`, `pathological`, or `all`. Every number printed is the
//! median of `--reps` repetitions after one discarded warm-up sweep; the report says how many.
//! Fixtures are built under `--scratch` and removed afterwards unless `--keep` is passed.

// Durations and byte counts are reported as approximate milliseconds and megabytes. Precision
// loss converting a u64 count of nanoseconds or bytes to f64 is the intended rounding, not a bug.
#![allow(clippy::cast_precision_loss)]

use std::collections::BTreeMap;
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use pidmesh::converge::{WorktreeScan, scan_worktree};
use pidmesh::store::MeshStore;
use serde_json::{Value, json};

/// Words reused across generated files, so the identifier vocabulary overlaps between checkouts
/// the way a real codebase's does. A benchmark whose every file invents unique names would make
/// `referenced_symbols` look far cheaper than it is.
const VOCABULARY: [&str; 24] = [
    "buffer", "cursor", "handle", "record", "session", "token", "window", "payload", "channel",
    "digest", "policy", "segment", "ledger", "anchor", "bundle", "matrix", "parser", "router",
    "socket", "stream", "vector", "worker", "cache", "index",
];

/// Files every checkout touches, standing in for the shared router or module index that a real
/// fleet all edits at once. Without one, no scenario would produce a single collision and the
/// collision query would never be exercised.
const HUB_FILES: usize = 2;

/// Lines per generated source file.
const FILE_LINES: usize = 40;

fn main() -> Result<()> {
    let options = Options::parse(env::args().skip(1))?;
    fs::create_dir_all(&options.scratch)
        .with_context(|| format!("failed to create {}", options.scratch.display()))?;
    let scratch = options
        .scratch
        .canonicalize()
        .unwrap_or_else(|_| options.scratch.clone());

    let mut report = json!({
        "repetitions": options.reps,
        "workers": thread::available_parallelism().map_or(4, NonZeroUsize::get),
        "note": "every duration is a median over `repetitions` sweeps after one discarded warm-up"
    });

    for suite in &options.suites {
        let outcome = match suite.as_str() {
            "scale" => scale_suite(&scratch, &options)?,
            "phases" => phase_suite(&scratch, &options)?,
            "growth" => growth_suite(&scratch, &options)?,
            "pathological" => pathological_suite(&scratch, &options)?,
            other => bail!("unknown suite: {other}"),
        };
        report[suite.as_str()] = outcome;
    }

    println!("{}", serde_json::to_string_pretty(&report)?);
    if !options.keep {
        let _ = fs::remove_dir_all(&scratch);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------------------------

struct Options {
    scratch: PathBuf,
    suites: Vec<String>,
    reps: usize,
    /// Sweeps per phase in the growth suite.
    ticks: usize,
    keep: bool,
}

impl Options {
    fn parse(arguments: impl Iterator<Item = String>) -> Result<Self> {
        let mut scratch = None;
        let mut suite = "all".to_owned();
        let mut reps = 5usize;
        let mut ticks = 120usize;
        let mut keep = false;
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--scratch" => {
                    scratch = Some(PathBuf::from(next_value(&mut arguments, "--scratch")?));
                }
                "--suite" => suite = next_value(&mut arguments, "--suite")?,
                "--reps" => reps = next_value(&mut arguments, "--reps")?.parse()?,
                "--ticks" => ticks = next_value(&mut arguments, "--ticks")?.parse()?,
                "--keep" => keep = true,
                other => bail!("unknown argument: {other}"),
            }
        }
        let scratch = scratch.context("--scratch <directory> is required")?;
        ensure!(reps >= 1, "--reps must be at least 1");
        ensure!(ticks >= 1, "--ticks must be at least 1");
        let suites = if suite == "all" {
            ["scale", "phases", "growth", "pathological"]
                .iter()
                .map(|name| (*name).to_owned())
                .collect()
        } else {
            suite.split(',').map(ToOwned::to_owned).collect()
        };
        Ok(Self {
            scratch,
            suites,
            reps,
            ticks,
            keep,
        })
    }
}

fn next_value(arguments: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    arguments
        .next()
        .with_context(|| format!("{flag} needs a value"))
}

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

/// A synthetic repository plus a set of linked worktrees, one per simulated agent.
struct Fixture {
    root: PathBuf,
    repo: PathBuf,
    checkouts: Vec<PathBuf>,
    repo_files: usize,
}

impl Fixture {
    /// Build a repository of `repo_files` source files with `checkouts` linked worktrees.
    ///
    /// Linked worktrees, not clones: that is what the convergence layer is pointed at in practice,
    /// and they share one object store, so the fixture measures the layout users actually have.
    fn build(root: &Path, repo_files: usize, checkouts: usize) -> Result<Self> {
        Self::build_with_api(root, repo_files, checkouts, 0)
    }

    /// As `build`, but with a shared `src/api.rs` exporting `api_0..api_n` in the base commit.
    ///
    /// Nothing in the scaling matrix withdraws an export, so without this the symbol-break join —
    /// the one part of a sweep that is quadratic in fleet size — is never given any work to do.
    fn build_with_api(
        root: &Path,
        repo_files: usize,
        checkouts: usize,
        api_exports: usize,
    ) -> Result<Self> {
        let _ = fs::remove_dir_all(root);
        let repo = root.join("repo");
        fs::create_dir_all(&repo)?;
        git(&repo, &["init", "--quiet", "--initial-branch=main", "."])?;
        git(&repo, &["config", "user.email", "bench@pidmesh.invalid"])?;
        git(&repo, &["config", "user.name", "pidmesh bench"])?;
        git(&repo, &["config", "commit.gpgsign", "false"])?;
        for index in 0..repo_files {
            write_source(&repo, index, 0)?;
        }
        if api_exports > 0 {
            fs::write(repo.join("src").join("api.rs"), api_source(0, api_exports))?;
        }
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "--quiet", "-m", "base"])?;

        let mut paths = Vec::with_capacity(checkouts);
        for index in 0..checkouts {
            let path = root.join(format!("wt-{index}"));
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    &format!("bench-{index}"),
                    &path.to_string_lossy(),
                    "main",
                ],
            )?;
            paths.push(path);
        }
        Ok(Self {
            root: root.to_path_buf(),
            repo,
            checkouts: paths,
            repo_files,
        })
    }

    /// Give the first `checkouts` worktrees `changed` modified files each.
    ///
    /// Every checkout rewrites the same `HUB_FILES` shared files, which is what produces
    /// collisions; the rest of its window is disjoint from its peers', which is what a fleet
    /// working on separate features looks like.
    fn dirty(&self, checkouts: usize, changed: usize, revision: usize) -> Result<()> {
        ensure!(
            changed <= self.repo_files,
            "cannot change more files than exist"
        );
        for (index, checkout) in self.checkouts.iter().take(checkouts).enumerate() {
            for hub in 0..HUB_FILES.min(changed) {
                write_source(checkout, hub, revision * 1_000 + index + 1)?;
            }
            let own = changed.saturating_sub(HUB_FILES);
            let span = self.repo_files.saturating_sub(HUB_FILES).max(1);
            for offset in 0..own {
                let file = HUB_FILES + (index * own + offset) % span;
                write_source(checkout, file, revision * 1_000 + index + 1)?;
            }
        }
        Ok(())
    }

    /// Register one agent per checkout and return their ids.
    ///
    /// The working directory is moved into each worktree first, because registering with an
    /// explicit root bypasses linked-worktree discovery and would hand every agent its own
    /// private workspace — exactly the split `pidmesh doctor` warns about.
    fn register(&self, store: &MeshStore, checkouts: usize) -> Result<Vec<String>> {
        let previous = env::current_dir()?;
        let mut ids = Vec::with_capacity(checkouts);
        for (index, checkout) in self.checkouts.iter().take(checkouts).enumerate() {
            env::set_current_dir(checkout)?;
            let registration = store.register_agent(
                &format!("bench-{index}"),
                std::process::id(),
                None,
                "bench",
                &[],
                None,
            )?;
            ids.push(
                registration["agent_id"]
                    .as_str()
                    .context("agent id")?
                    .to_owned(),
            );
        }
        env::set_current_dir(previous)?;
        Ok(ids)
    }

    fn database(&self) -> PathBuf {
        self.root.join("mesh.db")
    }
}

/// Write one generated source file into a checkout.
fn write_source(checkout: &Path, index: usize, revision: usize) -> Result<()> {
    let directory = checkout.join("src").join(format!("m{}", index / 50));
    fs::create_dir_all(&directory)?;
    fs::write(
        directory.join(format!("f{index}.rs")),
        source(index, revision),
    )?;
    Ok(())
}

/// Generated Rust-shaped content: a few exports, many shared identifiers, one revision marker.
fn source(index: usize, revision: usize) -> String {
    let mut text = format!("// module {index} revision {revision}\n");
    text.push_str("use crate::shared::{buffer, cursor, policy};\n\n");
    let _ = write!(text, "pub struct Node{index} {{ depth: usize }}\n\n");
    let _ = writeln!(text, "pub fn handle_{index}(cursor: usize) -> usize {{");
    for line in 0..FILE_LINES {
        let first = VOCABULARY[(index + line) % VOCABULARY.len()];
        let second = VOCABULARY[(index + line * 7 + revision) % VOCABULARY.len()];
        let _ = writeln!(
            text,
            "    let value_{line} = {first}(cursor + {line}) + {second}(depth);"
        );
    }
    text.push_str("    cursor\n}\n");
    text
}

/// A shared API module exporting `api_{from}..api_{to}`.
fn api_source(from: usize, to: usize) -> String {
    let mut text = String::from("// shared api\n");
    for name in from..to {
        let _ = writeln!(text, "pub fn api_{name}(depth: usize) -> usize {{ depth }}");
    }
    text
}

/// Run git in a directory, mirroring the configuration neutralisation `converge` applies.
fn git(directory: &Path, arguments: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "core.quotePath=false"])
        .args(arguments)
        .output()?;
    ensure!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        arguments,
        directory.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

// ---------------------------------------------------------------------------------------------
// One sweep
// ---------------------------------------------------------------------------------------------

/// Wall time of one sweep, broken into the phases `watch_pass` performs.
#[derive(Clone, Copy, Default)]
struct Sweep {
    total: f64,
    live: f64,
    scan: f64,
    publish: f64,
    reap: f64,
    report: f64,
    published: usize,
    collisions: usize,
    paths: usize,
}

/// Replay of `watch_pass`, phase by phase.
///
/// This duplicates the watcher's structure deliberately rather than shelling out to
/// `pidmesh watch --once`: a subprocess can only be timed as a whole, and the whole is the one
/// number everybody already has.
fn sweep(store: &MeshStore, workspace: &Path, base: Option<&str>) -> Result<Sweep> {
    let whole = Instant::now();

    let started = Instant::now();
    let live = store.live_checkouts(Some(workspace))?;
    let live_elapsed = millis(started.elapsed());

    let mut grouped: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for checkout in live.checkouts {
        grouped
            .entry(checkout.checkout_path.clone())
            .or_default()
            .push(checkout.agent_id);
    }
    let checkouts: Vec<(&PathBuf, &Vec<String>)> = grouped.iter().collect();

    let started = Instant::now();
    let cursor = AtomicUsize::new(0);
    let scans: Mutex<Vec<(usize, Result<WorktreeScan>)>> = Mutex::new(Vec::new());
    let workers = thread::available_parallelism()
        .map_or(4, NonZeroUsize::get)
        .min(checkouts.len().max(1));
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((path, _)) = checkouts.get(index) else {
                        return;
                    };
                    let scanned = scan_worktree(path, base);
                    if let Ok(mut guard) = scans.lock() {
                        guard.push((index, scanned));
                    }
                }
            });
        }
    });
    let scan_elapsed = millis(started.elapsed());

    let mut scanned = scans.into_inner().unwrap_or_default();
    scanned.sort_by_key(|(index, _)| *index);

    let mut batch: Vec<(String, WorktreeScan)> = Vec::new();
    let mut paths = 0usize;
    let mut witness = None;
    for (index, result) in scanned {
        let Some((_, agents)) = checkouts.get(index) else {
            continue;
        };
        let Ok(scan) = result else { continue };
        paths += scan.entries.len();
        for agent in *agents {
            witness.get_or_insert_with(|| agent.clone());
            batch.push((agent.clone(), scan.clone()));
        }
    }

    let started = Instant::now();
    let published = store.publish_footprints(&batch)?;
    let publish_elapsed = millis(started.elapsed());

    let started = Instant::now();
    store.collect_stale(Duration::from_secs(30))?;
    let reap_elapsed = millis(started.elapsed());

    let started = Instant::now();
    let collisions = match witness {
        Some(agent) => store.collisions(&agent)?["collision_count"]
            .as_u64()
            .unwrap_or(0),
        None => 0,
    };
    let report_elapsed = millis(started.elapsed());

    Ok(Sweep {
        total: millis(whole.elapsed()),
        live: live_elapsed,
        scan: scan_elapsed,
        publish: publish_elapsed,
        reap: reap_elapsed,
        report: report_elapsed,
        published: usize::try_from(published["published"].as_u64().unwrap_or(0)).unwrap_or(0),
        collisions: usize::try_from(collisions).unwrap_or(0),
        paths,
    })
}

fn millis(elapsed: Duration) -> f64 {
    elapsed.as_secs_f64() * 1_000.0
}

fn median(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    let value = if sorted.len().is_multiple_of(2) {
        f64::midpoint(sorted[middle - 1], sorted[middle])
    } else {
        sorted[middle]
    };
    (value * 100.0).round() / 100.0
}

/// Median of each phase across repeated sweeps.
fn medians(samples: &[Sweep]) -> Value {
    let pick =
        |extract: fn(&Sweep) -> f64| median(&samples.iter().map(extract).collect::<Vec<_>>());
    json!({
        "total_ms": pick(|s| s.total),
        "live_checkouts_ms": pick(|s| s.live),
        "scan_ms": pick(|s| s.scan),
        "publish_ms": pick(|s| s.publish),
        "reap_ms": pick(|s| s.reap),
        "collisions_ms": pick(|s| s.report),
        "sqlite_ms": pick(|s| s.live + s.publish + s.reap + s.report),
        "published": samples.last().map_or(0, |s| s.published),
        "collisions": samples.last().map_or(0, |s| s.collisions),
        "footprint_paths": samples.last().map_or(0, |s| s.paths)
    })
}

// ---------------------------------------------------------------------------------------------
// Suite: scale
// ---------------------------------------------------------------------------------------------

/// One point in the scaling matrix.
struct Scenario {
    label: &'static str,
    repo_files: usize,
    checkouts: usize,
    changed: usize,
}

/// The matrix: vary one axis at a time around a 32-checkout, 2000-file, 10-changed baseline.
const SCENARIOS: [Scenario; 7] = [
    Scenario {
        label: "checkouts=8",
        repo_files: 2_000,
        checkouts: 8,
        changed: 10,
    },
    Scenario {
        label: "checkouts=32",
        repo_files: 2_000,
        checkouts: 32,
        changed: 10,
    },
    Scenario {
        label: "checkouts=128",
        repo_files: 2_000,
        checkouts: 128,
        changed: 10,
    },
    Scenario {
        label: "changed=100",
        repo_files: 2_000,
        checkouts: 32,
        changed: 100,
    },
    Scenario {
        label: "changed=1000",
        repo_files: 2_000,
        checkouts: 32,
        changed: 1_000,
    },
    Scenario {
        label: "repo=500",
        repo_files: 500,
        checkouts: 32,
        changed: 10,
    },
    Scenario {
        label: "repo=8000",
        repo_files: 8_000,
        checkouts: 32,
        changed: 10,
    },
];

fn scale_suite(scratch: &Path, options: &Options) -> Result<Value> {
    let mut results = Vec::new();
    for scenario in &SCENARIOS {
        eprintln!(
            "[scale] {} (repo={} checkouts={} changed={})",
            scenario.label, scenario.repo_files, scenario.checkouts, scenario.changed
        );
        let root = scratch.join("scale");
        let built = Instant::now();
        let fixture = Fixture::build(&root, scenario.repo_files, scenario.checkouts)?;
        fixture.dirty(scenario.checkouts, scenario.changed, 1)?;
        let fixture_ms = millis(built.elapsed());

        let store = MeshStore::new(fixture.database())?;
        fixture.register(&store, scenario.checkouts)?;

        // The first sweep writes every footprint; later sweeps observe no change and skip the
        // write entirely. Both are reported, because they are different operations and only the
        // second one is what a watcher spends its day doing.
        let cold = sweep(&store, &fixture.repo, Some("main"))?;
        let mut warm = Vec::with_capacity(options.reps);
        for _ in 0..options.reps {
            warm.push(sweep(&store, &fixture.repo, Some("main"))?);
        }

        // One checkout changes between ticks: the realistic steady state, where a sweep still has
        // to write something.
        let mut churn = Vec::with_capacity(options.reps);
        for revision in 0..options.reps {
            fixture.dirty(1, scenario.changed, revision + 2)?;
            churn.push(sweep(&store, &fixture.repo, Some("main"))?);
        }

        let git_share = git_share(&fixture.checkouts[0], options.reps)?;
        drop(store);
        results.push(json!({
            "scenario": scenario.label,
            "repo_files": scenario.repo_files,
            "checkouts": scenario.checkouts,
            "changed_files": scenario.changed,
            "fixture_build_ms": fixture_ms,
            "cold_sweep_ms": (cold.total * 100.0).round() / 100.0,
            "idle_sweep": medians(&warm),
            "one_agent_changed_sweep": medians(&churn),
            "per_checkout_serial": git_share,
            "database_mb": megabytes(&fixture.database())
        }));
        let _ = fs::remove_dir_all(&root);
    }
    Ok(Value::Array(results))
}

// ---------------------------------------------------------------------------------------------
// Suite: phases (git versus everything else)
// ---------------------------------------------------------------------------------------------

/// Serial cost of one checkout, split into the git subprocesses and what is left.
fn git_share(checkout: &Path, reps: usize) -> Result<Value> {
    let base_commit = git(checkout, &["merge-base", "main", "HEAD"])?;
    let mut scans = Vec::with_capacity(reps);
    let mut gits = Vec::with_capacity(reps);
    let mut spawns = Vec::with_capacity(reps);
    // Warm-up: the first call pays for cold caches in both directions.
    let _ = scan_worktree(checkout, Some("main"))?;
    let _ = replay_git(checkout, &base_commit)?;
    for _ in 0..reps {
        let started = Instant::now();
        scan_worktree(checkout, Some("main"))?;
        scans.push(millis(started.elapsed()));
        gits.push(replay_git(checkout, &base_commit)?);
        let started = Instant::now();
        git(checkout, &["rev-parse", "HEAD"])?;
        spawns.push(millis(started.elapsed()));
    }
    let scan = median(&scans);
    let git_time = median(&gits);
    Ok(json!({
        "scan_ms": scan,
        "git_subprocess_ms": git_time,
        "in_process_ms": ((scan - git_time) * 100.0).round() / 100.0,
        "git_share_percent": if scan > 0.0 { (git_time / scan * 1_000.0).round() / 10.0 } else { 0.0 },
        "git_spawn_floor_ms": median(&spawns),
        "git_calls_per_scan": 5
    }))
}

/// Replay the exact git invocations `scan_worktree` makes and sum their wall time.
fn replay_git(checkout: &Path, base_commit: &str) -> Result<f64> {
    let started = Instant::now();
    let _ = git(checkout, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let _ = git(checkout, &["rev-parse", "--verify", "--quiet", "main"]);
    let _ = git(checkout, &["merge-base", "main", "HEAD"]);
    let _ = git(
        checkout,
        &[
            "diff",
            "--unified=0",
            "--no-renames",
            "--no-ext-diff",
            "--no-color",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            base_commit,
        ],
    );
    let _ = git(
        checkout,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    );
    Ok(millis(started.elapsed()))
}

/// Sweep the git/`SQLite` split across fleet sizes, so the crossover (if any) is visible.
fn phase_suite(scratch: &Path, options: &Options) -> Result<Value> {
    let mut results = Vec::new();
    for checkouts in [8usize, 32, 128] {
        eprintln!("[phases] checkouts={checkouts}");
        let root = scratch.join("phases");
        let fixture = Fixture::build(&root, 2_000, checkouts)?;
        fixture.dirty(checkouts, 50, 1)?;
        let store = MeshStore::new(fixture.database())?;
        fixture.register(&store, checkouts)?;
        sweep(&store, &fixture.repo, Some("main"))?;

        let mut samples = Vec::with_capacity(options.reps);
        for revision in 0..options.reps {
            // Force a real write every tick, so the publish phase is measured doing work rather
            // than short-circuiting on an unchanged footprint.
            fixture.dirty(checkouts, 50, revision + 2)?;
            samples.push(sweep(&store, &fixture.repo, Some("main"))?);
        }
        let phases = medians(&samples);
        let per_checkout = git_share(&fixture.checkouts[0], options.reps)?;
        let scan_ms = phases["scan_ms"].as_f64().unwrap_or(0.0);
        let sqlite_ms = phases["sqlite_ms"].as_f64().unwrap_or(0.0);
        let git_percent = per_checkout["git_share_percent"].as_f64().unwrap_or(0.0);
        let estimated_git = scan_ms * git_percent / 100.0;
        drop(store);
        results.push(json!({
            "checkouts": checkouts,
            "changed_files": 50,
            "sweep": phases,
            "per_checkout_serial": per_checkout,
            "estimated_git_ms": (estimated_git * 100.0).round() / 100.0,
            "sqlite_ms": sqlite_ms,
            "git_over_sqlite": if sqlite_ms > 0.0 {
                (estimated_git / sqlite_ms * 100.0).round() / 100.0
            } else { 0.0 }
        }));
        let _ = fs::remove_dir_all(&root);
    }
    Ok(Value::Array(results))
}

// ---------------------------------------------------------------------------------------------
// Suite: database growth
// ---------------------------------------------------------------------------------------------

fn megabytes(database: &Path) -> f64 {
    let mut total = 0u64;
    for suffix in ["", "-wal", "-shm"] {
        let mut path = database.as_os_str().to_os_string();
        path.push(suffix);
        if let Ok(metadata) = fs::metadata(PathBuf::from(path)) {
            total += metadata.len();
        }
    }
    (total as f64 / 1_048_576.0 * 1_000.0).round() / 1_000.0
}

/// How the database grows under idle ticks, under churn, and against the event ledger's cap.
fn growth_suite(scratch: &Path, options: &Options) -> Result<Value> {
    let ticks = options.ticks;
    let checkouts = 32usize;
    eprintln!("[growth] {checkouts} checkouts, {ticks} ticks idle then {ticks} ticks churning");
    let root = scratch.join("growth");
    let fixture = Fixture::build(&root, 2_000, checkouts)?;
    fixture.dirty(checkouts, 50, 1)?;
    let store = MeshStore::new(fixture.database())?;
    let agents = fixture.register(&store, checkouts)?;

    sweep(&store, &fixture.repo, Some("main"))?;
    let after_first = megabytes(&fixture.database());

    for _ in 0..ticks {
        sweep(&store, &fixture.repo, Some("main"))?;
    }
    let after_idle = megabytes(&fixture.database());

    for revision in 0..ticks {
        fixture.dirty(checkouts, 50, revision + 2)?;
        sweep(&store, &fixture.repo, Some("main"))?;
    }
    let after_churn = megabytes(&fixture.database());

    // The ledger cap is a claim about a bound, so test the bound directly rather than inferring
    // it: push far past 20,000 events, then do what a watcher does every tick and see what is
    // left. Trimming happens inside `collect_stale`, so the ceiling only holds while something
    // is sweeping — which is worth measuring separately from the ceiling itself.
    let before_flood = megabytes(&fixture.database());
    store.seed_events_for_test(&agents[0], 60_000)?;
    let after_flood = megabytes(&fixture.database());
    let untrimmed = store.events(&agents[0], 0, 200_000)?;
    let untrimmed = untrimmed.as_array().map_or(0, Vec::len);
    store.seed_events_for_test(&agents[0], 60_000)?;
    let after_second_flood = megabytes(&fixture.database());
    let trim_started = Instant::now();
    store.collect_stale(Duration::from_secs(30))?;
    let trim_ms = millis(trim_started.elapsed());
    let after_trim = megabytes(&fixture.database());
    let retained = store.events(&agents[0], 0, 200_000)?;
    let retained = retained.as_array().map_or(0, Vec::len);
    // A second sweep on a ledger already at the ceiling: this cost is paid every single tick.
    let steady_started = Instant::now();
    store.collect_stale(Duration::from_secs(30))?;
    let steady_trim_ms = millis(steady_started.elapsed());

    let result = json!({
        "checkouts": checkouts,
        "changed_files_per_checkout": 50,
        "ticks_per_phase": ticks,
        "after_first_sweep_mb": after_first,
        "after_idle_ticks_mb": after_idle,
        "idle_growth_kb_per_tick":
            ((after_idle - after_first) * 1_048_576.0 / 1_024.0 / ticks as f64 * 100.0).round() / 100.0,
        "after_churn_ticks_mb": after_churn,
        "churn_growth_kb_per_tick":
            ((after_churn - after_idle) * 1_048_576.0 / 1_024.0 / ticks as f64 * 100.0).round() / 100.0,
        "ledger_cap": {
            "before_flood_mb": before_flood,
            "after_60k_events_mb": after_flood,
            "events_before_any_trim": untrimmed,
            "after_120k_events_mb": after_second_flood,
            "after_trim_mb": after_trim,
            "trim_ms": (trim_ms * 100.0).round() / 100.0,
            "steady_trim_ms": (steady_trim_ms * 100.0).round() / 100.0,
            "events_retained": retained
        }
    });
    drop(store);
    let _ = fs::remove_dir_all(&root);
    Ok(result)
}

// ---------------------------------------------------------------------------------------------
// Suite: pathological inputs
// ---------------------------------------------------------------------------------------------

/// One pathological case, boxed so the suite can run each behind its own error boundary.
type PathologicalCase<'a> = Box<dyn Fn() -> Result<Value> + 'a>;

fn pathological_suite(scratch: &Path, options: &Options) -> Result<Value> {
    // Each case is reported rather than propagated. These cases exist to find the edges, so one
    // of them hitting a real limit is a result, not a reason to discard every other measurement
    // in a run that takes twenty minutes to produce.
    let cases: Vec<(&str, PathologicalCase<'_>)> = vec![
        (
            "huge-file-32",
            Box::new(|| case_huge_file(scratch, options, 32)),
        ),
        (
            "huge-file-80",
            Box::new(|| case_huge_file(scratch, options, 80)),
        ),
        ("many-tiny", Box::new(|| case_many_tiny(scratch, options))),
        ("deep-tree", Box::new(|| case_deep_tree(scratch, options))),
        (
            "wide-source",
            Box::new(|| case_wide_source(scratch, options)),
        ),
        (
            "long-history",
            Box::new(|| case_long_history(scratch, options)),
        ),
        (
            "symbols-8",
            Box::new(|| case_symbol_fanout(scratch, options, 8, false)),
        ),
        (
            "symbols-32",
            Box::new(|| case_symbol_fanout(scratch, options, 32, false)),
        ),
        (
            "symbols-128",
            Box::new(|| case_symbol_fanout(scratch, options, 128, false)),
        ),
        (
            "symbols-128-truncated",
            Box::new(|| case_symbol_fanout(scratch, options, 128, true)),
        ),
    ];
    let mut results = Vec::with_capacity(cases.len());
    for (name, run) in cases {
        match run() {
            Ok(value) => results.push(value),
            Err(error) => {
                eprintln!("[pathological] {name} failed: {error:#}");
                results.push(json!({"case": name, "failed": format!("{error:#}")}));
            }
        }
    }
    Ok(Value::Array(results))
}

/// Time one checkout's scan, repeated, with the fixture already dirty.
fn scan_median(checkout: &Path, reps: usize) -> Result<(f64, usize, bool)> {
    let first = scan_worktree(checkout, Some("main"))?;
    let mut samples = Vec::with_capacity(reps);
    for _ in 0..reps {
        let started = Instant::now();
        scan_worktree(checkout, Some("main"))?;
        samples.push(millis(started.elapsed()));
    }
    Ok((
        median(&samples),
        first.entries.len(),
        first.symbols.truncated,
    ))
}

/// A single enormous changed file, below and above the 64 MiB hashing ceiling.
fn case_huge_file(scratch: &Path, options: &Options, megs: usize) -> Result<Value> {
    eprintln!("[pathological] single {megs} MiB file");
    let root = scratch.join("patho-huge");
    let fixture = Fixture::build(&root, 20, 1)?;
    let checkout = &fixture.checkouts[0];
    let blob = vec![b'x'; 1_048_576];
    let path = checkout.join("huge.bin");
    let mut bytes = Vec::with_capacity(megs * 1_048_576);
    for _ in 0..megs {
        bytes.extend_from_slice(&blob);
    }
    fs::write(&path, &bytes)?;
    drop(bytes);
    let (ms, entries, truncated) = scan_median(checkout, options.reps.min(3))?;
    let _ = fs::remove_dir_all(&root);
    Ok(json!({
        "case": format!("single untracked {megs} MiB file"),
        "scan_ms": ms,
        "changed_paths": entries,
        "references_truncated": truncated,
        "note": "64 MiB is the hashing ceiling; above it the path is reported with no digest"
    }))
}

/// Tens of thousands of tiny untracked files in one checkout.
fn case_many_tiny(scratch: &Path, options: &Options) -> Result<Value> {
    let count = 20_000usize;
    eprintln!("[pathological] {count} tiny untracked files");
    let root = scratch.join("patho-tiny");
    let fixture = Fixture::build(&root, 20, 1)?;
    let checkout = &fixture.checkouts[0];
    for index in 0..count {
        let directory = checkout.join("gen").join(format!("d{}", index / 500));
        fs::create_dir_all(&directory)?;
        fs::write(directory.join(format!("t{index}.rs")), "pub fn tiny() {}\n")?;
    }
    let (ms, entries, truncated) = scan_median(checkout, options.reps.min(3))?;
    let store = MeshStore::new(fixture.database())?;
    let agents = fixture.register(&store, 1)?;
    let scan = scan_worktree(checkout, Some("main"))?;
    let started = Instant::now();
    store.publish_footprints(&[(agents[0].clone(), scan)])?;
    let publish = millis(started.elapsed());
    let database = megabytes(&fixture.database());
    drop(store);
    let _ = fs::remove_dir_all(&root);
    Ok(json!({
        "case": format!("{count} tiny untracked files in one checkout"),
        "scan_ms": ms,
        "publish_ms": (publish * 100.0).round() / 100.0,
        "changed_paths": entries,
        "references_truncated": truncated,
        "database_mb": database
    }))
}

/// A very deep directory tree with a changed file at the bottom.
fn case_deep_tree(scratch: &Path, options: &Options) -> Result<Value> {
    // `PATH_MAX` is 1024 bytes on macOS, so a tree cannot be nested arbitrarily deep no matter
    // what the benchmark asks for: go as deep as the budget allows and report the depth reached.
    const PATH_BUDGET: usize = 900;
    eprintln!("[pathological] directory tree as deep as PATH_MAX allows");
    let root = scratch.join("patho-deep");
    let fixture = Fixture::build(&root, 20, 1)?;
    let checkout = &fixture.checkouts[0];
    let mut directory = checkout.join("deep");
    let mut depth = 0usize;
    loop {
        let next = directory.join(format!("l{depth}"));
        if next.as_os_str().len() > PATH_BUDGET {
            break;
        }
        directory = next;
        depth += 1;
    }
    fs::create_dir_all(&directory)?;
    for index in 0..50 {
        fs::write(directory.join(format!("d{index}.rs")), source(index, 1))?;
    }
    let (ms, entries, truncated) = scan_median(checkout, options.reps.min(3))?;
    let _ = fs::remove_dir_all(&root);
    Ok(json!({
        "case": format!("50 changed files at directory depth {depth}"),
        "scan_ms": ms,
        "changed_paths": entries,
        "references_truncated": truncated
    }))
}

/// One source file just under the 2 MiB scanning ceiling, dense with unique identifiers.
fn case_wide_source(scratch: &Path, options: &Options) -> Result<Value> {
    eprintln!("[pathological] one dense 2 MiB source file");
    let root = scratch.join("patho-wide");
    let fixture = Fixture::build(&root, 20, 1)?;
    let checkout = &fixture.checkouts[0];
    let mut text = String::with_capacity(2_000_000);
    let mut index = 0usize;
    while text.len() < 1_900_000 {
        let _ = writeln!(
            text,
            "    symbol_{index}(argument_{index}, parameter_{index});"
        );
        index += 1;
    }
    fs::write(checkout.join("src").join("dense.rs"), &text)?;
    let (ms, entries, truncated) = scan_median(checkout, options.reps.min(3))?;
    let _ = fs::remove_dir_all(&root);
    Ok(json!({
        "case": "one 1.9 MiB source file with ~3 unique identifiers per line",
        "distinct_identifiers_written": index * 3,
        "scan_ms": ms,
        "changed_paths": entries,
        "references_truncated": truncated,
        "note": "MAX_REFERENCES is 5000; a truncated reference set makes the checkout match every withdrawn export"
    }))
}

/// A long history where the worktree branched near the root, forcing `merge-base` to walk.
fn case_long_history(scratch: &Path, options: &Options) -> Result<Value> {
    let commits = 3_000usize;
    eprintln!("[pathological] {commits}-commit history, branch cut at the root");
    let root = scratch.join("patho-history");
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("repo");
    fs::create_dir_all(&repo)?;
    git(&repo, &["init", "--quiet", "--initial-branch=main", "."])?;
    git(&repo, &["config", "user.email", "bench@pidmesh.invalid"])?;
    git(&repo, &["config", "user.name", "pidmesh bench"])?;
    for index in 0..20 {
        write_source(&repo, index, 0)?;
    }
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "--quiet", "-m", "base"])?;
    let origin = git(&repo, &["rev-parse", "HEAD"])?;
    let built = Instant::now();
    for index in 0..commits {
        git(
            &repo,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                &format!("step {index}"),
            ],
        )?;
    }
    let history_ms = millis(built.elapsed());
    let checkout = root.join("wt-0");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "bench-0",
            &checkout.to_string_lossy(),
            &origin,
        ],
    )?;
    for index in 0..10 {
        write_source(&checkout, index, 7)?;
    }
    let (ms, entries, truncated) = scan_median(&checkout, options.reps.min(3))?;
    let base_time = {
        let mut samples = Vec::new();
        for _ in 0..options.reps.min(3) {
            let started = Instant::now();
            git(&checkout, &["merge-base", "main", "HEAD"])?;
            samples.push(millis(started.elapsed()));
        }
        median(&samples)
    };
    let _ = fs::remove_dir_all(&root);
    Ok(json!({
        "case": format!("{commits} commits on main, worktree cut from the root commit"),
        "history_build_ms": history_ms,
        "scan_ms": ms,
        "merge_base_ms": base_time,
        "changed_paths": entries,
        "references_truncated": truncated
    }))
}

/// A fleet that all depends on one module, and one checkout that withdraws half of it.
///
/// Path-level collision detection is a grouped scan and stays cheap. The symbol-break report is a
/// self-join of `symbol_footprints` against itself across every agent in the workspace, and it is
/// the only part of a sweep whose cost grows with the square of the fleet. It does no work at all
/// until an export is actually withdrawn, which is why no scaling scenario above reaches it.
fn case_symbol_fanout(
    scratch: &Path,
    options: &Options,
    checkouts: usize,
    dense: bool,
) -> Result<Value> {
    let exports = 400usize;
    let withdrawn = 200usize;
    eprintln!(
        "[pathological] {checkouts} checkouts, one withdraws {withdrawn} shared exports (dense={dense})"
    );
    let root = scratch.join("patho-symbols");
    let fixture = Fixture::build_with_api(&root, 1_000, checkouts, exports)?;

    // Every checkout calls the shared API, so every one of them holds a reference row per name.
    for (index, checkout) in fixture.checkouts.iter().enumerate() {
        let mut text = format!("// caller {index}\n");
        for name in 0..exports {
            let _ = writeln!(text, "    api_{name}(cursor + {index});");
        }
        fs::write(checkout.join("src").join("caller.rs"), &text)?;
        if dense {
            // Push every checkout past MAX_REFERENCES, so each one stores the
            // `reference_truncated` sentinel instead of a reference set. The sentinel matches
            // every withdrawn export by design, which is the fleet-wide worst case for the
            // symbol-break join and for how many blockers a user is shown.
            let mut filler = String::with_capacity(900_000);
            for name in 0..40_000 {
                let _ = writeln!(filler, "    unique_{index}_{name}(depth);");
            }
            fs::write(checkout.join("src").join("filler.rs"), &filler)?;
        }
    }
    // One of them withdraws the first half of the API.
    fs::write(
        fixture.checkouts[0].join("src").join("api.rs"),
        api_source(withdrawn, exports),
    )?;

    let store = MeshStore::new(fixture.database())?;
    let agents = fixture.register(&store, checkouts)?;
    let swept = sweep(&store, &fixture.repo, Some("main"))?;

    let viewer = agents.last().context("at least one agent")?;
    let mut samples = Vec::with_capacity(options.reps);
    for _ in 0..options.reps {
        let started = Instant::now();
        store.collisions(viewer)?;
        samples.push(millis(started.elapsed()));
    }
    let report = store.collisions(viewer)?;
    let database = megabytes(&fixture.database());
    drop(store);
    let _ = fs::remove_dir_all(&root);
    Ok(json!({
        "case": format!("{checkouts} checkouts depend on 400 exports; one withdraws {withdrawn}"),
        "checkouts": checkouts,
        "references_truncated": dense,
        "collisions_ms": median(&samples),
        "publish_ms": (swept.publish * 100.0).round() / 100.0,
        "scan_ms": (swept.scan * 100.0).round() / 100.0,
        "sweep_total_ms": (swept.total * 100.0).round() / 100.0,
        "symbol_break_count": report["symbol_break_count"].as_u64().unwrap_or(0),
        "symbol_breaks_reported": report["symbol_breaks"].as_array().map_or(0, Vec::len),
        "database_mb": database
    }))
}
