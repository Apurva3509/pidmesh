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
    Mergeable,
    Sync,
    Watch,
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

/// Render a value for a terminal, or `None` when the shape does not apply to it.
#[must_use]
pub fn render(shape: Shape, value: &Value) -> Option<String> {
    let palette = Palette::new();
    match shape {
        Shape::Collisions => Some(collisions(value, &palette)),
        Shape::Doctor => Some(doctor(value, &palette)),
        Shape::Mergeable => Some(mergeable(value, &palette)),
        Shape::Sync => Some(sync(value, &palette)),
        Shape::Watch => Some(watch(value, &palette)),
    }
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

fn collisions(value: &Value, palette: &Palette) -> String {
    let contested = contested_lines(value, palette);
    let symbols = symbol_lines(value, palette);
    if contested.is_empty() && symbols.is_empty() {
        return format!(
            "{}No contested paths. Every checkout is working somewhere else.{}",
            palette.good, palette.reset
        );
    }
    let mut out = Vec::new();
    if !contested.is_empty() {
        out.push(plural(contested.len(), "contested path", "contested paths"));
        out.extend(contested);
    }
    if !symbols.is_empty() {
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(plural(symbols.len(), "symbol break", "symbol breaks"));
        out.extend(symbols);
    }
    out.join("\n")
}

fn watch(value: &Value, palette: &Palette) -> String {
    let checkouts = value["checkouts"].as_u64().unwrap_or(0);
    let published = value["published"].as_u64().unwrap_or(0);
    let skipped = value["skipped"].as_array().map_or(0, Vec::len);
    let mut head = format!(
        "{}{} scanned, {} published{}",
        palette.dim,
        plural(
            usize::try_from(checkouts).unwrap_or(0),
            "checkout",
            "checkouts"
        ),
        published,
        palette.reset
    );
    if skipped > 0 {
        let _ = write!(
            head,
            " {}({skipped} skipped){}",
            palette.warn, palette.reset
        );
    }
    let body = collisions(value, palette);
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
    format!("{head}\n{}", collisions(value, palette))
}

/// Turn one blocker into the sentence a person needs to act on it.
fn blocker_line(blocker: &Value, palette: &Palette) -> String {
    let code = blocker["code"].as_str().unwrap_or("?");
    let detail = match code {
        "stale_base" => "the integration branch moved since this worktree was cut — rebase".into(),
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
            "{}Ready to merge. Nothing else is touching this work.{}",
            palette.good, palette.reset
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
    out.join("\n")
}

fn doctor(value: &Value, palette: &Palette) -> String {
    let warnings = value["warnings"].as_array().cloned().unwrap_or_default();
    let workspace = value["resolved_workspace"].as_str().unwrap_or("?");
    if warnings.is_empty() {
        return format!(
            "{}Mesh is healthy.{}\n  {}workspace  {}{}",
            palette.good, palette.reset, palette.dim, workspace, palette.reset
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
        // Skip the count header; only the indented rows are collisions.
        let order: Vec<&str> = rendered
            .lines()
            .filter(|line| line.starts_with("  "))
            .filter_map(|line| line.split_whitespace().nth(1))
            .collect();
        assert_eq!(order, vec!["b.rs", "c.rs", "a.rs"], "{rendered}");
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
