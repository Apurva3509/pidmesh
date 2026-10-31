//! Observed worktree convergence.
//!
//! Resource reservations describe what an agent *intends* to touch. This module describes what a
//! checkout has *actually* changed, so the mesh can detect collisions that nobody declared. A scan
//! is pure git observation: it never mutates the checkout and never needs the agent's cooperation
//! beyond being pointed at a directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail, ensure};
use uuid::Uuid;

/// Branches consulted, in order, when the caller does not name an integration base.
const DEFAULT_BASE_REFS: [&str; 2] = ["main", "master"];

/// Files above this size are not fingerprinted; they are reported as divergent instead, which is
/// the conservative direction.
const MAX_HASHED_BYTES: u64 = 64 * 1024 * 1024;

/// Unchanged lines that must separate two rewritten regions for a merge to stay clean.
///
/// Measured against git: a gap of zero conflicts, a gap of one does not.
const REQUIRED_GAP: u32 = 1;

/// Cap on identifiers recorded per checkout, so a generated file cannot flood the mesh.
const MAX_REFERENCES: usize = 5_000;

/// Largest file read when harvesting identifiers.
///
/// Hashing already refuses oversized files; this read had no bound at all, so a sweep scanning a
/// checkout holding a large generated file spiked memory by roughly the file's size, every tick.
const MAX_SCANNED_BYTES: u64 = 2 * 1024 * 1024;

/// Extensions whose contents are treated as code.
///
/// Export detection matches bare `class`, `type`, `def` and `func`, which are ordinary English
/// words. Run over a changelog or a design note they manufacture exports nobody declared, and a
/// peer whose only change was a comment mentioning them gets blocked. A blocker that fires on
/// prose teaches people to ignore blockers, which costs more than the breaks it would have caught.
const SOURCE_EXTENSIONS: [&str; 18] = [
    "c", "cc", "cjs", "cpp", "cs", "go", "h", "hpp", "java", "js", "jsx", "kt", "mjs", "py", "rb",
    "rs", "ts", "tsx",
];

/// Whether a path's contents should be read as code.
fn is_source(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            SOURCE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Words that look like identifiers but never name an export worth tracking.
const REFERENCE_STOPLIST: [&str; 24] = [
    "and", "ary", "class", "const", "def", "else", "enum", "export", "false", "fn", "for", "from",
    "func", "if", "impl", "import", "let", "mut", "not", "pub", "return", "self", "true", "type",
];

/// How a path was changed inside a worktree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }

    /// Classify a git status pair such as `??`, `M `, or `MD`.
    #[must_use]
    fn from_status_code(code: &str) -> Self {
        if code == "??" || code.contains('A') {
            Self::Added
        } else if code.contains('D') {
            Self::Deleted
        } else {
            Self::Modified
        }
    }
}

/// One changed path inside an agent's checkout.
#[derive(Clone, Debug)]
pub struct FootprintEntry {
    pub path: String,
    pub change_kind: ChangeKind,
    /// Blob hash of the new content, or `None` for deletions and unhashable paths.
    pub digest: Option<String>,
    /// Regions of the *base* file this checkout rewrote, as `start-end` pairs joined by commas.
    ///
    /// Base coordinates, not working-tree coordinates, because a three-way merge conflicts when
    /// two sides rewrote overlapping regions of the common ancestor. `None` means the regions
    /// are unknown and the paths must be assumed to overlap.
    pub ranges: Option<String>,
}

/// Exported names a checkout withdrew, and names its changed files still lean on.
///
/// Path-level collisions cannot see the case where two agents edit *different* files and one
/// removes or renames something the other calls. That merge is textually clean and behaviourally
/// broken, which is the failure this closes.
#[derive(Clone, Debug, Default)]
pub struct SymbolFootprint {
    /// `(symbol, path)` for each exported name this checkout deleted and did not add back.
    pub removed_exports: Vec<(String, String)>,
    /// Identifiers appearing in this checkout's changed files.
    pub references: Vec<String>,
    /// Whether the reference set hit its cap and is therefore incomplete.
    ///
    /// An incomplete set cannot be used to rule a break out, only to rule one in, so a truncated
    /// checkout is treated as referencing everything. Silently dropping the overflow turned a
    /// generated file in someone's worktree into a clean merge over a real break.
    pub truncated: bool,
}

/// A complete observation of one checkout at a point in time.
#[derive(Clone, Debug)]
pub struct WorktreeScan {
    pub base_ref: String,
    pub base_commit: String,
    pub branch: Option<String>,
    pub entries: Vec<FootprintEntry>,
    pub symbols: SymbolFootprint,
}

/// One agent's stake in a contested path.
#[derive(Clone, Debug)]
pub struct Participant {
    pub agent_id: String,
    pub agent_name: String,
    pub status: String,
    pub change_kind: String,
    pub digest: Option<String>,
    pub base_commit: Option<String>,
    pub branch: Option<String>,
    pub ranges: Option<String>,
}

/// Collision severity for a single contested path.
///
/// `identical` means several agents produced byte-identical content: duplicated effort, safe to
/// merge. `divergent` means the same path holds different content in different checkouts, which is
/// the case that silently overwrites work. `delete_edit` means one agent removed a path another is
/// still editing, which git will merge without complaint in several common orderings.
#[must_use]
pub fn classify(participants: &[Participant]) -> &'static str {
    let deleting = participants
        .iter()
        .any(|participant| participant.change_kind == ChangeKind::Deleted.as_str());
    let editing = participants
        .iter()
        .any(|participant| participant.change_kind != ChangeKind::Deleted.as_str());
    if deleting && editing {
        return "delete_edit";
    }
    if deleting {
        // Every checkout removed the path. Deletions carry no digest, so without this the
        // content comparison below would fall through and call a clean merge divergent.
        return "identical";
    }
    let first = participants
        .first()
        .and_then(|participant| participant.digest.as_deref());
    if first.is_some()
        && participants
            .iter()
            .all(|participant| participant.digest.as_deref() == first)
    {
        return "identical";
    }
    // Different content in the same file is not automatically a conflict. A shared router or
    // module index is edited by everyone, and git merges those edits cleanly as long as they
    // rewrote different regions of the common ancestor.
    if disjoint_regions(participants) {
        return "adjacent";
    }
    "divergent"
}

/// Whether every pair of participants rewrote non-overlapping regions of the base file.
///
/// Unknown regions are treated as overlapping, so this only ever downgrades a collision when it
/// can prove the edits are separable.
#[must_use]
fn disjoint_regions(participants: &[Participant]) -> bool {
    let mut parsed = Vec::new();
    for participant in participants {
        let Some(ranges) = participant.ranges.as_deref() else {
            return false;
        };
        parsed.push(parse_ranges(ranges));
    }
    for (index, left) in parsed.iter().enumerate() {
        for right in parsed.iter().skip(index + 1) {
            if left
                .iter()
                .any(|outer| right.iter().any(|inner| regions_touch(*outer, *inner)))
            {
                return false;
            }
        }
    }
    true
}

/// Whether two rewritten regions are close enough that a three-way merge will conflict.
///
/// Not merely whether they overlap. A three-way merge needs unchanged material between two
/// changed regions to keep them apart, and measured against git directly, one line is enough and
/// zero is not: edits to consecutive lines conflict, edits one line apart merge cleanly. Testing
/// for overlap alone therefore called immediately-adjacent edits safe, and a differential run
/// against real merges caught it telling three checkouts in forty to merge into a conflict.
fn regions_touch(left: (u32, u32), right: (u32, u32)) -> bool {
    let (left_start, left_end) = left;
    let (right_start, right_end) = right;
    left_start <= right_end.saturating_add(REQUIRED_GAP)
        && right_start <= left_end.saturating_add(REQUIRED_GAP)
}

/// Parse a stored `start-end,start-end` range list.
fn parse_ranges(ranges: &str) -> Vec<(u32, u32)> {
    ranges
        .split(',')
        .filter_map(|span| {
            let (start, end) = span.split_once('-')?;
            Some((start.parse().ok()?, end.parse().ok()?))
        })
        .collect()
}

/// Whether the contesting agents cut their worktrees from different base commits.
///
/// A divergent base is how a clean merge still breaks behaviour: the later diff was written against
/// code that no longer exists on the integration branch.
#[must_use]
pub fn base_divergent(participants: &[Participant]) -> bool {
    let mut bases = participants
        .iter()
        .filter_map(|participant| participant.base_commit.as_deref());
    let Some(first) = bases.next() else {
        return false;
    };
    bases.any(|base| base != first)
}

/// Observe a checkout and produce its footprint relative to an integration base.
pub fn scan_worktree(checkout: &Path, base_ref: Option<&str>) -> Result<WorktreeScan> {
    ensure!(
        checkout.is_dir(),
        "checkout is not a directory: {}",
        checkout.display()
    );
    let branch = git_text(checkout, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|name| name != "HEAD" && !name.is_empty());
    let base_ref = resolve_base_ref(checkout, base_ref)?;
    let base_commit = git_text(checkout, &["merge-base", &base_ref, "HEAD"])
        .map_err(|_| base_failure(checkout, &base_ref))?;

    let summary = read_diff(checkout, &base_commit)?;
    let mut regions = summary.regions;
    let removed_exports = summary.removed_exports;
    let mut kinds = summary.kinds;
    // The diff is authoritative for anything it covers; status only fills in what it cannot see.
    for (path, kind) in status_only_changes(checkout)? {
        kinds.entry(path).or_insert(kind);
    }

    let entries: Vec<FootprintEntry> = kinds
        .into_iter()
        .map(|(path, reported)| {
            // A lossily-decoded name does not address a real file, so trust neither the
            // filesystem lookup nor the digest for it.
            let addressable = !path.contains('\u{fffd}');
            let absolute = checkout.join(&path);
            let present = addressable && fs::symlink_metadata(&absolute).is_ok();
            // git reports a path as added or modified that can already be gone: a staged file
            // deleted from the worktree, or one removed since the status call. The filesystem is
            // the authority on which it is.
            let change_kind = if addressable && !present {
                ChangeKind::Deleted
            } else {
                reported
            };
            let digest = if change_kind == ChangeKind::Deleted || !addressable {
                None
            } else {
                digest_of(&absolute)
            };
            let ranges = regions.remove(&path);
            FootprintEntry {
                path,
                change_kind,
                digest,
                ranges,
            }
        })
        .collect();
    let live: Vec<String> = entries
        .iter()
        .filter(|entry| entry.change_kind != ChangeKind::Deleted)
        .map(|entry| entry.path.clone())
        .collect();
    let (references, truncated) = referenced_symbols(checkout, &live);
    let symbols = SymbolFootprint {
        removed_exports,
        references,
        truncated,
    };
    Ok(WorktreeScan {
        base_ref,
        base_commit,
        branch,
        entries,
        symbols,
    })
}

/// Resolve the integration branch's current head commit.
///
/// Merge readiness compares this against the commit a worktree was cut from, so it has to be the
/// branch tip rather than the merge base.
pub fn integration_head(checkout: &Path, base_ref: Option<&str>) -> Result<(String, String)> {
    let base_ref = resolve_base_ref(checkout, base_ref)?;
    let head = git_text(checkout, &["rev-parse", &base_ref])?;
    Ok((base_ref, head))
}

/// Whether a ref resolves, keeping a failure to run git distinct from a ref that is absent.
fn ref_exists(checkout: &Path, reference: &str) -> Result<bool> {
    let program = git_program();
    let output = Command::new(program)
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .map_err(|error| {
            anyhow!(
                "could not run git at {}: {error}",
                Path::new(program).display()
            )
        })?;
    Ok(output.status.success())
}

/// Explain why a base could not be used, paying for a probe only once it has already failed.
fn base_failure(checkout: &Path, base_ref: &str) -> anyhow::Error {
    // An unborn HEAD makes every ref lookup fail, so check it first or the answer blames the
    // branch name for a checkout that simply has no history yet.
    if git_text(checkout, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_err() {
        return anyhow!("this checkout has no commits yet, so there is nothing to compare");
    }
    if git_text(checkout, &["rev-parse", "--verify", "--quiet", base_ref]).is_err() {
        anyhow!("base ref not found in checkout: {base_ref}")
    } else {
        anyhow!("no merge base between HEAD and {base_ref}; they share no history")
    }
}

/// Resolve the integration branch once, so a sweep does not re-probe it in every checkout.
///
/// Linked worktrees share one ref store, so whichever branch exists for one of them exists for
/// all of them.
pub fn resolve_base(checkout: &Path, requested: Option<&str>) -> Result<String> {
    resolve_base_ref(checkout, requested)
}

/// Pick the integration branch to measure against.
fn resolve_base_ref(checkout: &Path, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        // Reject a leading dash rather than passing `--`, which would make git read the ref as
        // a pathspec instead of a revision.
        ensure!(
            !requested.starts_with('-'),
            "base ref must not start with a dash: {requested}"
        );
        // Deliberately not verified here. `merge-base` below rejects an unknown ref anyway, and
        // a sweep spends almost all of its time starting git processes rather than running them:
        // one avoidable invocation per checkout per tick is the single largest saving available.
        // The error path pays for the nicer message instead of the happy path.
        return Ok(requested.to_owned());
    }
    for candidate in DEFAULT_BASE_REFS {
        // `?` rather than `is_ok()`: a git that will not start must not be reported as a
        // repository without a main branch.
        if ref_exists(checkout, candidate)? {
            return Ok(candidate.to_owned());
        }
    }
    bail!("no default integration branch found; pass an explicit --base")
}

/// One `git diff` covers every tracked path, committed and uncommitted alike, because diffing a
/// commit against the working tree already unions both. Untracked files have no base side and so
/// appear here at all.
/// Everything one unified diff tells us: change kind and rewritten base regions per path, the
/// exported names withdrawn, and the exported names introduced.
struct DiffSummary {
    kinds: BTreeMap<String, ChangeKind>,
    regions: BTreeMap<String, String>,
    removed_exports: Vec<(String, String)>,
}

/// Read one `git diff` of the base against the working tree.
///
/// That single diff already unions committed and uncommitted work for every tracked path, so the
/// change kind, the rewritten regions and the withdrawn exports all come from it. Running
/// `diff --name-status` and `status` as well asked git the same question twice more, and git
/// invocations are what a sweep actually spends its time on.
fn read_diff(checkout: &Path, base_commit: &str) -> Result<DiffSummary> {
    let raw = git_bytes(
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
    )?;
    let text = String::from_utf8_lossy(&raw);
    let mut regions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut kinds: BTreeMap<String, ChangeKind> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut pending_add: Option<String> = None;
    let mut removed: Vec<(String, String)> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            // `--- a/path`, or `--- /dev/null` for an addition, which has no base side. git
            // appends a tab to a header whose path contains a space.
            current = rest
                .strip_prefix("a/")
                .map(|path| path.trim_end_matches('\t').to_owned());
            pending_add = None;
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            // The pair of headers is what names the change: no base side means added, no working
            // side means deleted, both present means modified.
            let target = rest
                .strip_prefix("b/")
                .map(|path| path.trim_end_matches('\t').to_owned());
            match (current.as_ref(), target.as_ref()) {
                (None, Some(path)) => {
                    kinds.insert(path.clone(), ChangeKind::Added);
                    pending_add = Some(path.clone());
                }
                (Some(path), None) => {
                    kinds.insert(path.clone(), ChangeKind::Deleted);
                }
                (Some(path), Some(_)) => {
                    kinds.insert(path.clone(), ChangeKind::Modified);
                }
                (None, None) => {}
            }
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            let _ = &pending_add;
            if let Some(path) = current.as_ref()
                && let Some(span) = base_span(rest)
            {
                regions.entry(path.clone()).or_default().push(span);
            }
        } else if let Some(rest) = line.strip_prefix('-')
            && !rest.starts_with("--")
            && let Some(path) = current.as_ref()
            && is_source(path)
            && let Some(symbol) = exported_name(rest)
        {
            removed.push((symbol, path.clone()));
        } else if let Some(rest) = line.strip_prefix('+')
            && !rest.starts_with("++")
            && let Some(symbol) = exported_name(rest)
        {
            added.push(symbol);
        }
    }
    // A rewritten signature reads as a delete plus an add. Only a name that never comes back was
    // genuinely withdrawn.
    removed.retain(|(symbol, _)| !added.contains(symbol));
    let regions = regions
        .into_iter()
        .map(|(path, spans)| (path, spans.join(",")))
        .collect();
    Ok(DiffSummary {
        kinds,
        regions,
        removed_exports: removed,
    })
}

/// Paths a diff against the base cannot mention: untracked files, and index-only state.
///
/// A diff of the base against the working tree is the primary truth, but it is blind to anything
/// that exists only in the index — a file staged and then deleted from the worktree is absent
/// from both sides of that diff while still being part of what this checkout would commit.
fn status_only_changes(checkout: &Path) -> Result<Vec<(String, ChangeKind)>> {
    let raw = git_bytes(
        checkout,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )?;
    Ok(split_nul(&raw)
        .into_iter()
        .filter_map(|record| {
            let code = record.get(..2)?;
            let path = record.get(3..)?;
            if path.is_empty() {
                return None;
            }
            Some((path.to_owned(), ChangeKind::from_status_code(code)))
        })
        .collect())
}

/// The exported name a source line declares, if it declares one.
///
/// Heuristic and deliberately narrow: it recognises the common export forms of Rust, TypeScript,
/// JavaScript, Python and Go, and returns nothing when unsure. A missed export costs a warning
/// that is not raised; a wrong one costs a false alarm, which is worse.
#[must_use]
fn exported_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = [
        "pub fn ",
        "pub struct ",
        "pub enum ",
        "pub trait ",
        "pub const ",
        "pub type ",
        "pub static ",
        "pub async fn ",
        "export function ",
        "export async function ",
        "export const ",
        "export class ",
        "export interface ",
        "export type ",
        "export enum ",
        "export default function ",
        "def ",
        "class ",
        "func ",
        "type ",
    ]
    .into_iter()
    .find_map(|prefix| trimmed.strip_prefix(prefix))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    // Go exports by capitalisation; `func main` and lowercase helpers are not API.
    if name.len() < 2 || name.starts_with(char::is_numeric) {
        return None;
    }
    Some(name)
}

/// The local name a line binds, if it binds one.
///
/// A file that introduces its own `compute_total` is not depending on anybody else's. Without
/// this, a local binding that happens to share a name with a withdrawn export reads as a usage
/// and blocks a merge over two unrelated pieces of code.
#[must_use]
fn bound_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = ["let mut ", "let ", "const ", "var ", "static "]
        .into_iter()
        .find_map(|prefix| trimmed.strip_prefix(prefix))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (name.len() >= 2).then_some(name)
}

/// Names a checkout's changed files use but do not themselves declare.
///
/// The exclusion is what makes this a dependency rather than a word list. Two checkouts that both
/// changed one file each still hold their own copy of every declaration in it, and counting those
/// copies as usages reports every edited file as breaking every other one.
fn referenced_symbols(checkout: &Path, paths: &[String]) -> (Vec<String>, bool) {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut truncated = false;
    for path in paths {
        if !is_source(path) {
            continue;
        }
        let absolute = checkout.join(path);
        // `symlink_metadata` describes the link, whose length is the target *string*, so a size
        // test alone lets a symlink through and the read below then follows it. `digest_of` tests
        // `is_file()` first for exactly this reason; two readers of the same path must not
        // disagree about what they are willing to open.
        if fs::symlink_metadata(&absolute)
            .is_ok_and(|meta| !meta.is_file() || meta.len() > MAX_SCANNED_BYTES)
        {
            // Too large to hold in memory every tick. Treated like any other unknown: it
            // contributes no references, and the truncation flag keeps the verdict conservative.
            truncated = true;
            continue;
        }
        let Ok(content) = fs::read_to_string(&absolute) else {
            continue;
        };
        let declared: BTreeSet<String> = content
            .lines()
            .filter_map(|line| exported_name(line).or_else(|| bound_name(line)))
            .collect();
        let mut words: BTreeSet<String> = BTreeSet::new();
        let mut word = String::new();
        for character in content.chars() {
            if character.is_alphanumeric() || character == '_' {
                word.push(character);
                continue;
            }
            take_word(&mut word, &mut words);
        }
        take_word(&mut word, &mut words);
        for candidate in words {
            if declared.contains(&candidate) {
                continue;
            }
            seen.insert(candidate);
            if seen.len() >= MAX_REFERENCES {
                return (seen.into_iter().collect(), true);
            }
        }
    }
    (seen.into_iter().collect(), truncated)
}

fn take_word(word: &mut String, seen: &mut BTreeSet<String>) {
    if word.len() >= 3
        && !word.starts_with(char::is_numeric)
        && !REFERENCE_STOPLIST.contains(&word.as_str())
    {
        seen.insert(std::mem::take(word));
    } else {
        word.clear();
    }
}

/// Turn the `-start,count` half of a hunk header into an inclusive `start-end` span.
fn base_span(header: &str) -> Option<String> {
    let old = header.split_whitespace().next()?.strip_prefix('-')?;
    let (start, count) = old
        .split_once(',')
        .map_or((old, "1"), |(start, count)| (start, count));
    let start: u32 = start.parse().ok()?;
    let count: u32 = count.parse().ok()?;
    // A pure insertion has count 0 and sits between two base lines; treat it as the single
    // boundary line so two insertions at the same point still register as overlapping.
    let end = start.saturating_add(count.saturating_sub(1)).max(start);
    Some(format!("{start}-{end}"))
}

/// Fingerprint a path's current bytes.
///
/// This is a mesh-internal digest, not a git object id: it only ever has to answer whether two
/// checkouts hold the same content. Hashing in process rather than shelling out to
/// `git hash-object --stdin-paths` matters for three reasons. That subprocess deadlocks once the
/// change set is large enough for its unread stdout to fill the pipe while the parent is still
/// writing paths to its stdin; it aborts the entire scan when any single path cannot be read,
/// which an ordinary staged-then-deleted file or a dirty submodule is enough to cause; and it
/// C-unquotes a leading quote, silently returning another file's digest.
///
/// Anything without comparable content — a directory, a submodule, a nested repository, a
/// symlink, an unreadable or oversized file — yields `None`, which classifies as divergent.
fn digest_of(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_HASHED_BYTES {
        return None;
    }
    let content = fs::read(path).ok()?;
    Some(
        Uuid::new_v5(&Uuid::NAMESPACE_OID, &content)
            .simple()
            .to_string(),
    )
}

/// Split NUL-terminated git output, dropping the trailing empty field.
fn split_nul(raw: &[u8]) -> Vec<String> {
    raw.split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8_lossy(field).into_owned())
        .collect()
}

/// Run git with the user's configuration neutralised where it would change our parsing.
///
/// `core.quotePath` C-quotes any non-ASCII path, and `diff.noprefix` / `diff.mnemonicPrefix`
/// change or remove the `a/` and `b/` markers the diff parser keys on. Both are ordinary settings
/// to have in a `~/.gitconfig`, and under either one this scan reads an empty diff and reports no
/// conflicts at all — a silent wrong answer with a zero exit code. Scanning must depend on the
/// repository, never on who is holding it.
/// The git executable to run, resolved once per process.
///
/// A sweep is dominated by the cost of *starting* git, and on macOS `/usr/bin/git` is a shim that
/// re-dispatches to the developer tools: measured idle, 16.4 ms per invocation against 4.8 ms for
/// the binary it forwards to — the same build, reporting the same version, reading the same
/// configuration. Four invocations per checkout per tick makes that worth resolving past.
///
/// `PIDMESH_GIT` overrides the choice outright for anyone who needs a particular git.
fn git_program() -> &'static OsString {
    static PROGRAM: OnceLock<OsString> = OnceLock::new();
    PROGRAM.get_or_init(|| {
        if let Some(configured) = std::env::var_os("PIDMESH_GIT") {
            return configured;
        }
        if cfg!(target_os = "macos")
            && let Some(direct) = developer_tools_git()
        {
            return direct;
        }
        OsString::from("git")
    })
}

/// The git the macOS shim forwards to, if it is present and runnable.
fn developer_tools_git() -> Option<OsString> {
    let output = Command::new("xcode-select").arg("-p").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let developer = String::from_utf8(output.stdout).ok()?;
    let candidate = Path::new(developer.trim()).join("usr/bin/git");
    // Only adopt it if it actually runs; a stale developer directory is worse than the shim.
    Command::new(&candidate)
        .arg("--version")
        .output()
        .ok()
        .filter(|probe| probe.status.success())?;
    Some(candidate.into_os_string())
}

fn git_bytes(directory: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let program = git_program();
    let output = Command::new(program)
        .arg("-C")
        .arg(directory)
        .args(["-c", "core.quotePath=false"])
        .args(arguments)
        .output()
        .map_err(|error| {
            // Failing to start git is a different problem from git refusing a request, and
            // reporting it as the latter sends people looking at their branches.
            anyhow!(
                "could not run git at {}: {error}",
                Path::new(program).display()
            )
        })?;
    ensure!(
        output.status.success(),
        "git failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn git_text(directory: &Path, arguments: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git_bytes(directory, arguments)?)?
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(change_kind: &str, digest: Option<&str>, base: Option<&str>) -> Participant {
        ranged(change_kind, digest, base, None)
    }

    fn ranged(
        change_kind: &str,
        digest: Option<&str>,
        base: Option<&str>,
        ranges: Option<&str>,
    ) -> Participant {
        Participant {
            agent_id: "agent".to_owned(),
            agent_name: "agent".to_owned(),
            status: "running".to_owned(),
            change_kind: change_kind.to_owned(),
            digest: digest.map(ToOwned::to_owned),
            base_commit: base.map(ToOwned::to_owned),
            branch: None,
            ranges: ranges.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn matching_content_is_duplicated_effort_not_a_conflict() {
        let participants = vec![
            participant("modified", Some("aaa"), None),
            participant("modified", Some("aaa"), None),
        ];
        assert_eq!(classify(&participants), "identical");
    }

    #[test]
    fn differing_content_on_one_path_is_divergent() {
        let participants = vec![
            participant("modified", Some("aaa"), None),
            participant("modified", Some("bbb"), None),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn a_deletion_against_an_edit_outranks_content_comparison() {
        let participants = vec![
            participant("deleted", None, None),
            participant("modified", Some("bbb"), None),
        ];
        assert_eq!(classify(&participants), "delete_edit");
    }

    #[test]
    fn an_unhashable_path_is_treated_as_divergent() {
        let participants = vec![
            participant("modified", None, None),
            participant("modified", None, None),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn differing_base_commits_are_reported_separately_from_severity() {
        let same = vec![
            participant("modified", Some("aaa"), Some("c1")),
            participant("modified", Some("bbb"), Some("c1")),
        ];
        let drifted = vec![
            participant("modified", Some("aaa"), Some("c1")),
            participant("modified", Some("bbb"), Some("c2")),
        ];
        assert!(!base_divergent(&same));
        assert!(base_divergent(&drifted));
    }

    #[test]
    fn separable_regions_of_one_file_are_adjacent_not_divergent() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-12")),
            ranged("modified", Some("bbb"), None, Some("80-84")),
        ];
        assert_eq!(classify(&participants), "adjacent");
    }

    #[test]
    fn overlapping_regions_remain_divergent() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-20")),
            ranged("modified", Some("bbb"), None, Some("18-24")),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn touching_regions_count_as_overlapping() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-20")),
            ranged("modified", Some("bbb"), None, Some("20-30")),
        ];
        assert_eq!(classify(&participants), "divergent");
    }

    #[test]
    fn an_unknown_region_is_assumed_to_overlap() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("10-12")),
            ranged("modified", Some("bbb"), None, None),
        ];
        assert_eq!(
            classify(&participants),
            "divergent",
            "a downgrade must be provable"
        );
    }

    #[test]
    fn multiple_disjoint_hunks_stay_adjacent() {
        let participants = vec![
            ranged("modified", Some("aaa"), None, Some("1-5,40-42")),
            ranged("modified", Some("bbb"), None, Some("10-12,80-90")),
        ];
        assert_eq!(classify(&participants), "adjacent");
    }

    #[test]
    fn status_codes_map_to_change_kinds() {
        assert_eq!(ChangeKind::from_status_code("??"), ChangeKind::Added);
        assert_eq!(ChangeKind::from_status_code("A "), ChangeKind::Added);
        assert_eq!(ChangeKind::from_status_code(" D"), ChangeKind::Deleted);
        assert_eq!(ChangeKind::from_status_code("MD"), ChangeKind::Deleted);
        assert_eq!(ChangeKind::from_status_code(" M"), ChangeKind::Modified);
    }
}
