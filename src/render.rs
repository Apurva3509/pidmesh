//! Human-readable rendering of command output.
//!
//! Every command still emits JSON when stdout is not a terminal, because agents and scripts parse
//! it and that contract predates this module. A person at a prompt gets prose instead: the same
//! facts, ordered by what demands attention.

use std::fmt::Write as _;
use std::io::IsTerminal;

use serde_json::Value;

/// Which renderer to use when stdout is a terminal.
#[derive(Clone, Copy, Debug)]
pub enum Shape {
    Collisions,
    Doctor,
    Join,
    Mergeable,
    Status,
    Sync,
    Watch,
    Worktrees,
}

/// Severity ordering for display: what breaks a merge comes first.
fn rank(severity: &str) -> u8 {
    match severity {
        "delete_edit" => 0,
        "divergent" => 1,
        "adjacent" => 2,
        _ => 3,
    }
}

/// A leading glyph that reads at a glance without relying on colour.
fn glyph(severity: &str) -> &'static str {
    match severity {
        "delete_edit" | "divergent" => "x",
        "adjacent" => "~",
        _ => "·",
    }
}

struct Palette {
    bad: &'static str,
    warn: &'static str,
    good: &'static str,
    dim: &'static str,
    reset: &'static str,
}

impl Palette {
    /// Colour is opt-out through `NO_COLOR`, which is the convention these tools follow.
    fn new() -> Self {
        if std::env::var_os("NO_COLOR").is_some() {
            Self {
                bad: "",
                warn: "",
                good: "",
                dim: "",
                reset: "",
            }
        } else {
            Self {
                bad: "\u{1b}[31m",
                warn: "\u{1b}[33m",
                good: "\u{1b}[32m",
                dim: "\u{1b}[2m",
                reset: "\u{1b}[0m",
            }
        }
    }

    fn for_severity(&self, severity: &str) -> &'static str {
        match severity {
            "delete_edit" | "divergent" => self.bad,
            "adjacent" => self.warn,
            _ => self.dim,
        }
    }
}

/// Whether a person is reading this output.
#[must_use]
pub fn is_interactive() -> bool {
    std::io::stdout().is_terminal()
}

/// Render a failure for whoever is reading stderr.
///
/// A person gets a sentence; anything else keeps the `{"error": ...}` object that scripts match on.
#[must_use]
pub fn error(message: &str, force_json: bool) -> String {
    if force_json || !std::io::stderr().is_terminal() {
        return serde_json::json!({"error": message}).to_string();
    }
    let palette = Palette::new();
    format!("{}error:{} {message}", palette.bad, palette.reset)
}

/// Render a value for a terminal, or `None` when the shape does not apply to it.
#[must_use]
pub fn render(shape: Shape, value: &Value) -> Option<String> {
    let palette = Palette::new();
    match shape {
        Shape::Collisions => Some(collisions(value, &palette)),
        Shape::Doctor => Some(doctor(value, &palette)),
        Shape::Join => Some(join(value, &palette)),
        Shape::Mergeable => Some(mergeable(value, &palette)),
        Shape::Status => Some(status(value, &palette)),
        Shape::Sync => Some(sync(value, &palette)),
        Shape::Watch => Some(watch(value, &palette)),
        Shape::Worktrees => Some(worktrees(value, &palette)),
    }
}

/// What a severity actually means, in one line, for someone meeting the word for the first time.
///
/// These four names are the whole vocabulary of the tool and they appeared in output with no
/// gloss anywhere: a first run printed `delete_edit` and left the reader to guess.
fn gloss(severity: &str) -> &'static str {
    match severity {
        "delete_edit" => {
            "one checkout deleted this path while another kept editing it — git merges that \
             quietly in several orderings, and the edit is lost"
        }
        "divergent" => {
            "two checkouts rewrote overlapping regions of this path — merging either one \
             overwrites the other's work"
        }
        "adjacent" => {
            "the same path, but in separable regions — git three-way merges this cleanly, so \
             it does not block"
        }
        "identical" => {
            "every checkout reached the same content here — duplicated effort, safe to merge"
        }
        _ => "",
    }
}

/// Spell out only the severities actually on screen, worst first, once each.
fn legend(value: &Value, palette: &Palette) -> Vec<String> {
    let mut seen: Vec<&str> = value["collisions"]
        .as_array()
        .map(|collisions| {
            collisions
                .iter()
                .filter_map(|collision| collision["severity"].as_str())
                .collect()
        })
        .unwrap_or_default();
    seen.sort_by_key(|severity| rank(severity));
    seen.dedup();
    seen.into_iter()
        .filter(|severity| !gloss(severity).is_empty())
        .map(|severity| {
            format!(
                "    {}{severity}  {}{}",
                palette.dim,
                gloss(severity),
                palette.reset
            )
        })
        .collect()
}

/// The one command that answers "so what do I do now?".
fn next_step(palette: &Palette, text: &str) -> String {
    format!("  {}Next  {text}{}", palette.dim, palette.reset)
}

/// How many checkouts git knows about that this sweep is not watching, and which ones.
fn unwatched(value: &Value) -> Vec<&str> {
    value["unwatched_worktrees"]
        .as_array()
        .map(|paths| paths.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The empty state, said so that it cannot be mistaken for a safe one.
///
/// A sweep counts registrations, so a repository whose agents never joined produced a clean
/// green report while several worktrees edited the same files. Nothing in that output
/// distinguished "nothing is wrong" from "I am not looking at anything".
fn unwatched_lines(value: &Value, palette: &Palette) -> Vec<String> {
    if let Some(problem) = value["workspace_problem"].as_str() {
        return vec![format!("  {}{problem}{}", palette.dim, palette.reset)];
    }
    let missing = unwatched(value);
    if missing.is_empty() {
        return vec![
            format!(
                "  {}No agent has joined this workspace, and git reports no other checkout \
                 here either.{}",
                palette.dim, palette.reset
            ),
            next_step(
                palette,
                "`pidmesh join --name <agent>` from inside each checkout registers one.",
            ),
        ];
    }
    let mut out = vec![format!(
        "  {}git knows {} here; none of them is registered with pidmesh, so no \
         collision between them can be seen.{}",
        palette.warn,
        plural(missing.len(), "checkout", "checkouts"),
        palette.reset
    )];
    out.extend(listed(&missing, palette));
    out.push(next_step(
        palette,
        "run `pidmesh join --name <agent>` inside each one — or `pidmesh run`, which joins for \
         the command it supervises.",
    ));
    out
}

/// A sweep that covers only some of the repository's checkouts is only partly authoritative.
fn partial_coverage(value: &Value, palette: &Palette) -> Option<String> {
    let missing = unwatched(value);
    if missing.is_empty() {
        return None;
    }
    let mut out = vec![format!(
        "  {}{} not watched, so collisions involving {} are invisible:{}",
        palette.warn,
        plural(missing.len(), "checkout is", "checkouts are"),
        if missing.len() == 1 { "it" } else { "them" },
        palette.reset
    )];
    out.extend(listed(&missing, palette));
    Some(out.join("\n"))
}

/// List paths, capped, so a repository with forty worktrees does not bury the message.
fn listed(paths: &[&str], palette: &Palette) -> Vec<String> {
    const SHOWN: usize = 5;
    let mut out: Vec<String> = paths
        .iter()
        .take(SHOWN)
        .map(|path| format!("    {}{path}{}", palette.dim, palette.reset))
        .collect();
    if paths.len() > SHOWN {
        out.push(format!(
            "    {}and {} more{}",
            palette.dim,
            paths.len() - SHOWN,
            palette.reset
        ));
    }
    out
}

fn plural(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Contested paths, worst first, then the symbol breaks.
fn contested_lines(value: &Value, palette: &Palette) -> Vec<String> {
    let mut rows: Vec<(u8, String)> = value["collisions"]
        .as_array()
        .map(|collisions| {
            collisions
                .iter()
                .map(|collision| {
                    let severity = collision["severity"].as_str().unwrap_or("divergent");
                    let path = collision["path"].as_str().unwrap_or("?");
                    let who: Vec<&str> = collision["participants"]
                        .as_array()
                        .map(|participants| {
                            participants
                                .iter()
                                .filter_map(|participant| participant["agent_name"].as_str())
                                .collect()
                        })
                        .unwrap_or_default();
                    let stale = if collision["base_divergent"] == true {
                        " (different bases)"
                    } else {
                        ""
                    };
                    (
                        rank(severity),
                        format!(
                            "  {}{} {:<28}{:<13}{}{}{}",
                            palette.for_severity(severity),
                            glyph(severity),
                            path,
                            severity,
                            who.join(", "),
                            stale,
                            palette.reset
                        ),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by_key(|(rank, _)| *rank);
    rows.into_iter().map(|(_, line)| line).collect()
}

fn symbol_lines(value: &Value, palette: &Palette) -> Vec<String> {
    value["symbol_breaks"]
        .as_array()
        .map(|breaks| {
            breaks
                .iter()
                .map(|entry| {
                    format!(
                        "  {}x {}() removed from {}, still used by {}{}",
                        palette.bad,
                        entry["symbol"].as_str().unwrap_or("?"),
                        entry["removed_from"].as_str().unwrap_or("?"),
                        entry["used_by"]["agent_name"].as_str().unwrap_or("?"),
                        palette.reset
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The contested-path block, shared by `collisions`, `sync` and `watch`.
///
/// `advice` is the line that tells a reader what to type next; a report they cannot act on is
/// where every one of these commands used to stop.
fn collision_block(value: &Value, palette: &Palette, advice: Option<&str>) -> String {
    let contested = contested_lines(value, palette);
    let symbols = symbol_lines(value, palette);
    if contested.is_empty() && symbols.is_empty() {
        let mut clear = format!(
            "{}No contested paths. Every checkout is working somewhere else.{}",
            palette.good, palette.reset
        );
        if let Some(advice) = advice {
            clear.push('\n');
            clear.push_str(&next_step(palette, advice));
        }
        return clear;
    }
    let mut out = Vec::new();
    if !contested.is_empty() {
        out.push(plural(contested.len(), "contested path", "contested paths"));
        out.extend(contested);
        out.extend(legend(value, palette));
    }
    if !symbols.is_empty() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(plural(symbols.len(), "symbol break", "symbol breaks"));
        out.extend(symbols);
    }
    if let Some(advice) = advice {
        out.push(next_step(palette, advice));
    }
    out.join("\n")
}

fn collisions(value: &Value, palette: &Palette) -> String {
    collision_block(
        value,
        palette,
        Some("`pidmesh mergeable` says whether your own branch can merge right now."),
    )
}

fn watch(value: &Value, palette: &Palette) -> String {
    let checkouts = value["checkouts"].as_u64().unwrap_or(0);
    let published = value["published"].as_u64().unwrap_or(0);
    let skipped = value["skipped"].as_array().map_or(0, Vec::len);
    // "nothing changed" is the common answer for a watcher, and it should read like one rather
    // than like a count of work performed.
    let moved = if published == 0 {
        "nothing changed".to_owned()
    } else {
        format!("{published} updated")
    };
    let mut head = format!(
        "{}{} scanned, {}{}",
        palette.dim,
        plural(
            usize::try_from(checkouts).unwrap_or(0),
            "checkout",
            "checkouts"
        ),
        moved,
        palette.reset
    );
    if skipped > 0 {
        let _ = write!(
            head,
            " {}({skipped} skipped){}",
            palette.warn, palette.reset
        );
    }
    // A bare "(1 skipped)" is a warning nobody can act on: it names neither the checkout nor the
    // reason, both of which the sweep already knows.
    for entry in value["skipped"].as_array().into_iter().flatten() {
        let subject = entry["checkout"]
            .as_str()
            .or_else(|| entry["publish"].as_str())
            .unwrap_or("a checkout");
        let reason = entry["error"].as_str().unwrap_or("no reason recorded");
        // The header already names the checkout, so drop the copy git or the scanner appended.
        let reason = reason
            .strip_suffix(subject)
            .map_or(reason, |head| head.trim_end().trim_end_matches(':'));
        let _ = write!(
            head,
            "\n  {}~ skipped {subject}{}\n    {}{reason}{}",
            palette.warn, palette.reset, palette.dim, palette.reset
        );
    }
    let stale = value["stale_agents"].as_u64().unwrap_or(0);
    if checkouts == 0 {
        // An empty sweep has several very different causes and only one of them is a quiet
        // workspace: nobody joined, everybody who joined is already gone, or git cannot read
        // this directory at all. Reporting any of them as a clean mesh is the failure, because
        // the empty state then looks exactly like the safe state.
        if stale > 0 {
            return format!(
                "{head}\n{}{} registered, but no live process.{}\n  {}They exited, or joined without a surviving --pid. Re-join from the shell that keeps working in each checkout.{}",
                palette.warn,
                plural(
                    usize::try_from(stale).unwrap_or(0),
                    "agent is",
                    "agents are"
                ),
                palette.reset,
                palette.dim,
                palette.reset
            );
        }
        return format!(
            "{head}\n{}Nothing is being watched.{}\n{}",
            palette.warn,
            palette.reset,
            unwatched_lines(value, palette).join("\n")
        );
    }
    // Before the coverage gap below, which is several lines: appended after it, this suffix
    // landed at the end of the last path rather than on the summary line it qualifies.
    if stale > 0 {
        let _ = write!(
            head,
            " {}({stale} registered without a live process){}",
            palette.warn, palette.reset
        );
    }
    // A partial view is the same failure in smaller print: the collision report below is
    // authoritative only for the checkouts that are actually in it.
    if let Some(gap) = partial_coverage(value, palette) {
        let _ = write!(head, "\n{gap}");
    }
    let body = collision_block(
        value,
        palette,
        Some("`pidmesh mergeable` says whether your own branch can merge right now."),
    );
    format!("{head}\n{body}")
}

fn sync(value: &Value, palette: &Palette) -> String {
    let paths = value["paths"].as_u64().unwrap_or(0);
    let branch = value["branch"].as_str().unwrap_or("detached");
    let head = format!(
        "{}{} changed on {}{}",
        palette.dim,
        plural(usize::try_from(paths).unwrap_or(0), "path", "paths"),
        branch,
        palette.reset
    );
    let body = collision_block(
        value,
        palette,
        Some("`pidmesh mergeable` says whether your own branch can merge right now."),
    );
    format!("{head}\n{body}")
}

/// Turn one blocker into the sentence a person needs to act on it.
fn blocker_line(blocker: &Value, palette: &Palette) -> String {
    let code = blocker["code"].as_str().unwrap_or("?");
    let detail = match code {
        "stale_base" => "the integration branch moved since this worktree was cut — rebase".into(),
        "never_synced" => {
            "this checkout has not been observed yet — run `pidmesh sync` first".into()
        }
        "integration_held" => format!(
            "{} is merging right now — wait for the lease",
            blocker["held_by"].as_str().unwrap_or("another agent")
        ),
        "removed_export_in_use" => format!(
            "removes {}() which {} still calls",
            blocker["symbol"].as_str().unwrap_or("?"),
            blocker["used_by"]["agent_name"].as_str().unwrap_or("?")
        ),
        "contested_path" => {
            let peers: Vec<String> = blocker["peers"]
                .as_array()
                .map(|peers| {
                    peers
                        .iter()
                        .map(|peer| {
                            format!(
                                "{} ({})",
                                peer["agent_name"].as_str().unwrap_or("?"),
                                peer["severity"].as_str().unwrap_or("?")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "{} overlaps {}",
                blocker["path"].as_str().unwrap_or("?"),
                peers.join(", ")
            )
        }
        other => other.to_owned(),
    };
    format!("  {}x {}{}", palette.bad, detail, palette.reset)
}

fn mergeable(value: &Value, palette: &Palette) -> String {
    let blockers = value["blockers"].as_array().cloned().unwrap_or_default();
    if blockers.is_empty() {
        return format!(
            "{}Ready to merge. Nothing else is touching this work.{}\n{}",
            palette.good,
            palette.reset,
            next_step(
                palette,
                "`pidmesh mergeable && git merge <branch>` keeps the check in front of the merge."
            )
        );
    }
    let mut out = vec![format!(
        "{}Cannot merge yet{} — {}",
        palette.bad,
        palette.reset,
        plural(blockers.len(), "blocker", "blockers")
    )];
    out.extend(
        blockers
            .iter()
            .map(|blocker| blocker_line(blocker, palette)),
    );
    // "Cannot merge yet" with no way forward is the dead end that matters most: it is the one
    // place the tool actively stops someone.
    out.push(next_step(
        palette,
        "`pidmesh collisions` shows every contested path and who holds it. Merging anyway is \
         always yours to decide — pidmesh never touches your branches.",
    ));
    out.join("\n")
}

/// What `join` prints, with the one line the quickstart tells people to copy.
///
/// This is the very first command a newcomer runs, and it used to answer with a raw JSON object
/// at a terminal while the quickstart said "export the id it prints".
fn join(value: &Value, palette: &Palette) -> String {
    let id = value["agent_id"].as_str().unwrap_or("?");
    let name = value["name"].as_str().unwrap_or("this agent");
    let branch = value["git_branch"].as_str();
    let checkout = value["checkout_path"].as_str().unwrap_or("?");
    let mut out = vec![format!(
        "{}Joined as {name}.{}",
        palette.good, palette.reset
    )];
    out.push(format!(
        "  {}checkout   {checkout}{}{}",
        palette.dim,
        branch.map_or_else(String::new, |branch| format!("  ({branch})")),
        palette.reset
    ));
    out.push(format!(
        "  {}workspace  {}{}",
        palette.dim,
        value["workspace"].as_str().unwrap_or("?"),
        palette.reset
    ));
    // Not dimmed: this is the line that has to be copied for anything else to work.
    out.push(String::new());
    out.push(format!("  export PIDMESH_AGENT_ID={id}"));
    out.push(next_step(
        palette,
        "`pidmesh watch --once` observes every checkout here and reports what is contested.",
    ));
    out.join("\n")
}

/// Who is alive, what they hold, and where — instead of the agent-shaped JSON a person got.
fn status(value: &Value, palette: &Palette) -> String {
    let agents = value["agents"].as_array().cloned().unwrap_or_default();
    let workspace = value["workspace"].as_str().unwrap_or("?");
    if agents.is_empty() {
        return format!(
            "{}No agents have joined this workspace yet.{}\n  {}workspace  {workspace}{}\n{}",
            palette.warn,
            palette.reset,
            palette.dim,
            palette.reset,
            next_step(
                palette,
                "`pidmesh join --name <agent>` from inside each checkout registers one."
            )
        );
    }
    let live = agents
        .iter()
        .filter(|agent| agent["pid_alive"] == true && agent["status"] == "running")
        .count();
    let mut out = vec![format!(
        "{}{} of {} in {workspace}{}",
        palette.dim,
        plural(live, "live agent", "live agents"),
        plural(agents.len(), "registration", "registrations"),
        palette.reset
    )];
    for agent in &agents {
        let alive = agent["pid_alive"] == true && agent["status"] == "running";
        let colour = if alive { palette.good } else { palette.dim };
        // The same glyph vocabulary the collision rows use: a filled mark is the live case.
        let mark = if alive { "*" } else { "·" };
        let branch = agent["git_branch"].as_str().unwrap_or("detached");
        let note = if alive {
            format!("pid {}", agent["pid"].as_u64().unwrap_or(0))
        } else {
            format!("pid {} is gone", agent["pid"].as_u64().unwrap_or(0))
        };
        out.push(format!(
            "  {colour}{mark} {:<14}{:<22}{:<10}{note}{}",
            agent["name"].as_str().unwrap_or("?"),
            branch,
            agent["provider"].as_str().unwrap_or("unknown"),
            palette.reset
        ));
    }
    let claims = value["claims"].as_array().map_or(0, Vec::len);
    let resources = value["resources"].as_array().map_or(0, Vec::len);
    if claims > 0 || resources > 0 {
        out.push(format!(
            "  {}{}, {}{}",
            palette.dim,
            plural(claims, "task claim", "task claims"),
            plural(resources, "reservation", "reservations"),
            palette.reset
        ));
    }
    if live < agents.len() {
        out.push(format!(
            "  {}Registrations whose process is gone still hold their share of every contested \
             path. `pidmesh gc` releases them.{}",
            palette.dim, palette.reset
        ));
    }
    out.push(next_step(
        palette,
        "`pidmesh collisions` shows which paths these agents are contesting.",
    ));
    out.join("\n")
}

/// Managed worktrees — and, when there are none, what the word means here.
///
/// Someone who created three checkouts with `git worktree add` runs this expecting to see them
/// and gets two empty arrays. The empty case has to say whose worktrees these are.
fn worktrees(value: &Value, palette: &Palette) -> String {
    let reclaimable = value["reclaimable"].as_array().cloned().unwrap_or_default();
    let retained = value["retained"].as_array().cloned().unwrap_or_default();
    let root = value["root"].as_str().unwrap_or("?");
    if reclaimable.is_empty() && retained.is_empty() {
        return format!(
            "{}No managed worktrees.{}\n  {}These are the checkouts pidmesh created itself, under\n  {root}\n  \
             Worktrees you made with `git worktree add` are watched but never listed here, \
             because pidmesh will not remove what it did not create.{}\n{}",
            palette.good,
            palette.reset,
            palette.dim,
            palette.reset,
            next_step(
                palette,
                "`pidmesh watch --once` reports on every checkout, managed or not."
            )
        );
    }
    let mut out = vec![format!(
        "{}{} held, {} reclaimable{}",
        palette.dim,
        retained.len(),
        reclaimable.len(),
        palette.reset
    )];
    for entry in retained.iter().chain(reclaimable.iter()) {
        let safe = entry["reclaimable"] == true;
        let colour = if safe { palette.dim } else { palette.warn };
        out.push(format!(
            "  {colour}{} {:<40}{}{}",
            if safe { "·" } else { "~" },
            entry["path"].as_str().unwrap_or("?"),
            entry["branch"].as_str().unwrap_or("detached"),
            palette.reset
        ));
        out.push(format!(
            "    {}{}{}",
            palette.dim,
            entry["reason"].as_str().unwrap_or("no reason recorded"),
            palette.reset
        ));
    }
    out.push(next_step(
        palette,
        if reclaimable.is_empty() {
            "nothing here is safe to remove yet; every one of these still holds work."
        } else {
            "`pidmesh worktrees --reclaim` removes only the ones proven spent. Work is never \
             discarded."
        },
    ));
    out.join("\n")
}

fn doctor(value: &Value, palette: &Palette) -> String {
    let warnings = value["warnings"].as_array().cloned().unwrap_or_default();
    let workspace = value["resolved_workspace"].as_str().unwrap_or("?");
    if warnings.is_empty() {
        return format!(
            "{}Mesh is healthy.{}\n  {}workspace  {}{}\n{}",
            palette.good,
            palette.reset,
            palette.dim,
            workspace,
            palette.reset,
            next_step(
                palette,
                "`pidmesh watch` keeps observing this workspace; `pidmesh status` says who is in it."
            )
        );
    }
    let mut out = vec![format!(
        "{}Mesh has {}{}",
        palette.bad,
        plural(warnings.len(), "problem", "problems"),
        palette.reset
    )];
    for warning in &warnings {
        out.push(format!(
            "  {}x {}{}",
            palette.bad,
            warning["detail"].as_str().unwrap_or("?").replace("  ", " "),
            palette.reset
        ));
        if let Some(roots) = warning["roots"].as_array() {
            for root in roots {
                out.push(format!(
                    "      {}{}  ({} live){}",
                    palette.dim,
                    root["workspace"].as_str().unwrap_or("?"),
                    root["live_agents"].as_u64().unwrap_or(0),
                    palette.reset
                ));
            }
        }
        // Naming a count of unwatched checkouts without naming them leaves the reader to
        // work out which ones, which is the dead end this whole pass is about.
        if let Some(paths) = warning["worktrees"].as_array() {
            let paths: Vec<&str> = paths.iter().filter_map(Value::as_str).collect();
            out.extend(
                listed(&paths, palette)
                    .into_iter()
                    .map(|line| format!("  {line}")),
            );
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plain() -> Palette {
        Palette {
            bad: "",
            warn: "",
            good: "",
            dim: "",
            reset: "",
        }
    }

    #[test]
    fn an_empty_mesh_says_so_rather_than_printing_nothing() {
        let rendered = collisions(&json!({"collisions": [], "symbol_breaks": []}), &plain());
        assert!(rendered.contains("No contested paths"));
    }

    #[test]
    fn blocking_severities_sort_above_benign_ones() {
        let value = json!({"collisions": [
            {"path": "a.rs", "severity": "identical", "participants": []},
            {"path": "b.rs", "severity": "delete_edit", "participants": []},
            {"path": "c.rs", "severity": "adjacent", "participants": []}
        ]});
        let rendered = collisions(&value, &plain());
        // Only the glyph rows are collisions. The header, the severity legend and the next
        // step are all indented too, so the glyph is what identifies a path row.
        let order: Vec<&str> = rendered
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .filter(|line| ["x ", "~ ", "· "].iter().any(|mark| line.starts_with(mark)))
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        assert_eq!(order, vec!["b.rs", "c.rs", "a.rs"], "{rendered}");
    }

    #[test]
    fn an_empty_workspace_explains_how_to_fill_it() {
        let rendered = watch(
            &json!({"checkouts": 0, "published": 0, "skipped": [], "collisions": []}),
            &plain(),
        );
        assert!(rendered.contains("Nothing is being watched"), "{rendered}");
        assert!(rendered.contains("pidmesh join"), "{rendered}");
        // The one thing it must never read as is a clean report.
        assert!(
            !rendered.contains("No contested paths"),
            "an empty sweep must not read as a clear one: {rendered}"
        );
    }

    #[test]
    fn an_unwatched_fleet_is_named_rather_than_reported_as_quiet() {
        // git can see checkouts pidmesh is not watching, and the gap between the two is the
        // difference between "nothing is wrong" and "I am not looking at anything".
        let rendered = watch(
            &json!({
                "checkouts": 0,
                "published": 0,
                "skipped": [],
                "collisions": [],
                "git_worktrees": 7,
                "unwatched_worktrees": ["/repo", "/repo/a", "/repo/b"]
            }),
            &plain(),
        );
        assert!(rendered.contains("Nothing is being watched"), "{rendered}");
        assert!(rendered.contains("git knows 3 checkouts"), "{rendered}");
        assert!(rendered.contains("/repo/a"), "{rendered}");
        assert!(
            !rendered.contains("No contested paths"),
            "a blind sweep must not read as a clear one: {rendered}"
        );
    }

    #[test]
    fn a_partly_watched_sweep_says_which_checkouts_it_cannot_see() {
        let rendered = watch(
            &json!({
                "checkouts": 2,
                "published": 0,
                "skipped": [],
                "collisions": [],
                "git_worktrees": 3,
                "unwatched_worktrees": ["/repo/c"]
            }),
            &plain(),
        );
        assert!(rendered.contains("/repo/c"), "{rendered}");
        assert!(rendered.contains("not watched"), "{rendered}");
        // The collision verdict still stands for what was scanned; it is just not the whole
        // repository, and the reader has to be able to tell.
        assert!(rendered.contains("No contested paths"), "{rendered}");
    }

    #[test]
    fn a_severity_is_explained_the_first_time_it_appears() {
        let rendered = collisions(
            &json!({"collisions": [
                {"path": "a.rs", "severity": "delete_edit", "participants": []}
            ]}),
            &plain(),
        );
        assert!(rendered.contains("delete_edit"), "{rendered}");
        assert!(
            rendered.contains("deleted this path while another kept editing it"),
            "the jargon is printed with no gloss: {rendered}"
        );
        // Only the severity on screen, so the legend does not become a wall of definitions.
        assert!(!rendered.contains("duplicated effort"), "{rendered}");
    }

    #[test]
    fn a_skipped_checkout_says_which_one_and_why() {
        let rendered = watch(
            &json!({
                "checkouts": 2,
                "published": 0,
                "collisions": [],
                "skipped": [{"checkout": "/repo/gone", "error": "checkout is not a directory: /repo/gone"}]
            }),
            &plain(),
        );
        assert!(rendered.contains("/repo/gone"), "{rendered}");
        assert!(rendered.contains("not a directory"), "{rendered}");
        // The header already names it; repeating the path inside the reason is noise.
        assert_eq!(
            rendered.matches("/repo/gone").count(),
            1,
            "the path is printed twice: {rendered}"
        );
    }

    #[test]
    fn every_report_ends_somewhere_a_reader_can_go_next() {
        // A report with no next step is a dead end, and these are the commands a newcomer
        // reaches first.
        for rendered in [
            collisions(&json!({"collisions": [], "symbol_breaks": []}), &plain()),
            mergeable(&json!({"blockers": []}), &plain()),
            mergeable(&json!({"blockers": [{"code": "stale_base"}]}), &plain()),
            doctor(
                &json!({"warnings": [], "resolved_workspace": "/repo"}),
                &plain(),
            ),
            status(&json!({"agents": [], "workspace": "/repo"}), &plain()),
            worktrees(
                &json!({"reclaimable": [], "retained": [], "root": "/r"}),
                &plain(),
            ),
            join(
                &json!({"agent_id": "a-1-b", "name": "a", "workspace": "/repo"}),
                &plain(),
            ),
        ] {
            assert!(
                rendered.contains("pidmesh "),
                "no command to run next: {rendered}"
            );
        }
    }

    #[test]
    fn joining_prints_the_line_the_quickstart_says_to_copy() {
        let rendered = join(
            &json!({
                "agent_id": "planner-42-abcd",
                "name": "planner",
                "checkout_path": "/repo/a",
                "git_branch": "feature",
                "workspace": "/repo"
            }),
            &plain(),
        );
        assert!(
            rendered.contains("export PIDMESH_AGENT_ID=planner-42-abcd"),
            "the id has to be pasteable, not transcribed: {rendered}"
        );
    }

    #[test]
    fn an_empty_worktree_listing_says_whose_worktrees_it_lists() {
        // Someone with three `git worktree add` checkouts runs this, sees two empty arrays,
        // and concludes the tool is broken. It lists only what pidmesh created itself.
        let rendered = worktrees(
            &json!({"reclaimable": [], "retained": [], "root": "/home/u/.pidmesh/worktrees/x"}),
            &plain(),
        );
        assert!(rendered.contains("git worktree add"), "{rendered}");
        assert!(rendered.contains("pidmesh created itself"), "{rendered}");
    }

    #[test]
    fn a_dead_registration_is_distinguished_from_a_live_agent() {
        let rendered = status(
            &json!({
                "workspace": "/repo",
                "agents": [
                    {"name": "alive", "pid": 1, "pid_alive": true, "status": "running",
                     "provider": "codex", "git_branch": "a"},
                    {"name": "ghost", "pid": 2, "pid_alive": false, "status": "running",
                     "provider": "codex", "git_branch": "b"}
                ]
            }),
            &plain(),
        );
        assert!(
            rendered.contains("1 live agent of 2 registrations"),
            "{rendered}"
        );
        assert!(
            rendered.contains("pidmesh gc"),
            "a stuck registration needs a way out: {rendered}"
        );
    }

    #[test]
    fn a_sweep_that_changed_nothing_says_so() {
        let quiet = watch(
            &json!({"checkouts": 48, "observed": 48, "published": 0, "skipped": [], "collisions": []}),
            &plain(),
        );
        assert!(quiet.contains("nothing changed"), "{quiet}");
        assert!(
            !quiet.contains("48 updated"),
            "submitting is not the same as writing: {quiet}"
        );

        let moved = watch(
            &json!({"checkouts": 48, "observed": 48, "published": 3, "skipped": [], "collisions": []}),
            &plain(),
        );
        assert!(moved.contains("3 updated"), "{moved}");
    }

    #[test]
    fn a_fleet_whose_processes_died_is_not_reported_as_healthy() {
        let rendered = watch(
            &json!({"checkouts": 0, "published": 0, "stale_agents": 3, "skipped": [], "collisions": []}),
            &plain(),
        );
        assert!(rendered.contains("no live process"), "{rendered}");
        assert!(
            !rendered.contains("No contested paths"),
            "a dead fleet must not read as a clean one: {rendered}"
        );
    }

    #[test]
    fn an_error_is_a_sentence_for_people_and_json_for_scripts() {
        let forced = error("agent id required", true);
        assert!(forced.starts_with('{'), "{forced}");
        assert!(forced.contains("agent id required"));
    }

    #[test]
    fn a_blocker_reads_as_an_instruction() {
        let value = json!({"blockers": [{"code": "stale_base"}]});
        let rendered = mergeable(&value, &plain());
        assert!(rendered.contains("rebase"), "{rendered}");
    }

    #[test]
    fn a_clear_merge_is_stated_plainly() {
        let rendered = mergeable(&json!({"blockers": []}), &plain());
        assert!(rendered.contains("Ready to merge"));
    }

    #[test]
    fn a_contested_path_blocker_names_the_peer_and_severity() {
        let value = json!({"blockers": [{
            "code": "contested_path",
            "path": "src/routes.rs",
            "peers": [{"agent_name": "bravo", "severity": "divergent"}]
        }]});
        let rendered = mergeable(&value, &plain());
        assert!(rendered.contains("src/routes.rs"), "{rendered}");
        assert!(rendered.contains("bravo (divergent)"), "{rendered}");
    }

    #[test]
    fn a_split_mesh_lists_the_orphaned_roots() {
        let value = json!({"warnings": [{
            "code": "split_mesh",
            "detail": "2 workspace roots belong to one repository",
            "roots": [{"workspace": "/a", "live_agents": 1}, {"workspace": "/b", "live_agents": 2}]
        }]});
        let rendered = doctor(&value, &plain());
        assert!(rendered.contains("/a"), "{rendered}");
        assert!(rendered.contains("/b"), "{rendered}");
    }
}
