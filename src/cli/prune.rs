//! `kanstack prune` — forget workstreams whose pane is confirmed gone.
//!
//! Idempotent by construction: `stale_branches` only acts on a poll that came back
//! [`PaneStatus::Dead`], so once a branch is gone from the registry there is nothing left for
//! a second run to find.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::pane_status::PaneStatus;
use crate::workstream::Registry;

use super::status::poll_or_nothing;

/// The `"schema"` of `kanstack prune --json`, versioned independently of `STATUS_SCHEMA`.
pub const PRUNE_SCHEMA: u32 = 1;

#[derive(Debug, Serialize)]
struct PruneReport {
    schema: u32,
    pruned: Vec<String>,
}

/// Branches in `registry` whose pane the poll has confirmed gone — present in the registry
/// with a pane id, but reported [`PaneStatus::Dead`]. Anything the poll has nothing to say
/// about (no multiplexer reachable, the branch missing from its answer, `Unknown`) is left
/// alone: absence of a reading is never grounds to forget a workstream, only a reading of
/// `Dead` is — see `poll_or_nothing`, whose empty map when no backend is found makes this
/// naturally prune nothing rather than everything.
fn stale_branches(registry: &Registry, statuses: &HashMap<String, PaneStatus>) -> Vec<String> {
    registry
        .workstreams
        .iter()
        .filter(|w| w.pane_id.is_some() && statuses.get(&w.branch_id.0) == Some(&PaneStatus::Dead))
        .map(|w| w.branch_id.0.clone())
        .collect()
}

pub(super) fn run(json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    // Best-effort, like `poll_or_nothing` itself: no multiplexer reachable, or a poll
    // that fails, must prune nothing rather than error out — an orchestrator calling
    // this routinely shouldn't have to special-case "not currently inside a pane". The
    // poll itself reads outside the lock (a plain `load`, same as `status`); only the
    // remove-and-save below needs it, same as every other mutating subcommand.
    let registry = Registry::load(cwd)?;
    let statuses = poll_or_nothing(&registry);
    let stale = stale_branches(&registry, &statuses);
    if !stale.is_empty() {
        Registry::with_lock(cwd, |registry| {
            for branch in &stale {
                registry.remove(branch);
            }
            Ok(())
        })?;
    }
    if json {
        writeln!(out, "{}", serde_json::to_string(&PruneReport { schema: PRUNE_SCHEMA, pruned: stale })?)?;
    } else if stale.is_empty() {
        writeln!(out, "nothing to prune")?;
    } else {
        for branch in &stale {
            writeln!(out, "pruned {branch}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use crate::cli::test_support::*;
    use crate::cli::Command;
    use crate::pane_status::PaneStatus;
    use crate::workstream::{Registry, Workstream};

    use super::stale_branches;

    /// A poll that confirms a pane gone (`Dead`) marks its branch stale; anything the poll
    /// merely has nothing to say about — busy, idle, waiting, unknown, or no entry at all —
    /// must not be, and neither should a workstream with no pane to poll in the first place.
    #[test]
    fn stale_branches_picks_only_a_confirmed_dead_pane() {
        let statuses = HashMap::from([
            ("fix-login".to_string(), PaneStatus::Busy),
            ("add-search".to_string(), PaneStatus::Idle),
            ("old-spike".to_string(), PaneStatus::Dead),
            ("mystery".to_string(), PaneStatus::Unknown),
            // No entry for "planned": it has no pane to poll in the first place.
        ]);
        assert_eq!(stale_branches(&five_workstreams(), &statuses), vec!["old-spike".to_string()]);
    }

    /// An empty status map — what `poll_or_nothing` returns when no multiplexer is reachable
    /// or the poll failed — must never be read as "every pane is gone".
    #[test]
    fn stale_branches_prunes_nothing_when_the_poll_has_nothing_to_say() {
        assert_eq!(stale_branches(&five_workstreams(), &HashMap::new()), Vec::<String>::new());
    }

    /// Runs `kanstack prune` against a stand-in `tmux` whose `list-panes` answers
    /// `list_panes`, with `workstreams` pre-loaded into the registry. Returns what it printed
    /// and the registry as `prune` left it on disk.
    fn prune_with_tmux(tag: &str, list_panes: &str, workstreams: Vec<Workstream>, json: bool) -> (String, Registry) {
        use crate::mux::stand_in;
        let (bin, _log) = stand_in::install(tag, "tmux", &format!(r#"case "$1" in list-panes) {list_panes} ;; esac"#));
        let state = std::env::temp_dir().join(format!("kanstack-cli-{tag}-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let (mut printed, mut after) = (String::new(), Registry::default());
        stand_in::with_env(
            &[
                ("KANSTACK_STATE_PATH", Some(state.to_str().unwrap())),
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_TMUX_BIN", Some(bin.to_str().unwrap())),
                ("TMUX_PANE", Some("%0")),
            ],
            || {
                let repo = Path::new("/repo/prune-backend");
                let mut registry = Registry::load(repo).unwrap();
                for w in workstreams {
                    registry.upsert(w);
                }
                registry.save().unwrap();
                let mut out = Vec::new();
                crate::cli::run(Command::Prune { json }, repo, &mut out).expect("prune must not fail here");
                printed = String::from_utf8(out).unwrap();
                after = Registry::load(repo).unwrap();
            },
        );
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&state);
        (printed, after)
    }

    #[test]
    fn prune_json_removes_only_the_pane_the_poll_confirms_gone() {
        let workstreams = vec![
            workstream("fix-login", Some("%3"), Some("claude"), None),
            workstream("old-spike", Some("%5"), Some("codex"), None),
            workstream("planned", None, None, None),
        ];
        // Only %3 is in the listing: fix-login is alive, old-spike is gone.
        let (printed, after) =
            prune_with_tmux("prune-json", r#"printf '%%3 2000000000\n'"#, workstreams, true);
        assert_eq!(printed, "{\"schema\":1,\"pruned\":[\"old-spike\"]}\n");
        assert_eq!(after.get("old-spike"), None, "the dead one is forgotten");
        assert!(after.get("fix-login").is_some(), "the live one stays");
        assert!(after.get("planned").is_some(), "a workstream with no pane is untouched");
    }

    /// `prune` is idempotent by construction (see the module doc): a repeat run just finds
    /// nothing left to prune. Runs `prune` twice against the same on-disk registry, in the
    /// same process, to pin that down directly rather than relying on it falling out of the
    /// single-run tests above. No code change needed — `stale_branches` already only acts on a
    /// poll that came back `Dead`, so once a branch is gone from the registry there is nothing
    /// left for a second run to find.
    #[test]
    fn running_prune_twice_prunes_nothing_the_second_time() {
        use crate::mux::stand_in;
        let tag = "prune-twice";
        let (bin, _log) = stand_in::install(tag, "tmux", r#"case "$1" in list-panes) printf '%%3 2000000000\n' ;; esac"#);
        let state = std::env::temp_dir().join(format!("kanstack-cli-{tag}-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        stand_in::with_env(
            &[
                ("KANSTACK_STATE_PATH", Some(state.to_str().unwrap())),
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_TMUX_BIN", Some(bin.to_str().unwrap())),
                ("TMUX_PANE", Some("%0")),
            ],
            || {
                let repo = Path::new("/repo/prune-twice");
                let mut registry = Registry::load(repo).unwrap();
                registry.upsert(workstream("fix-login", Some("%3"), Some("claude"), None));
                registry.upsert(workstream("old-spike", Some("%5"), Some("codex"), None));
                registry.save().unwrap();

                let mut first = Vec::new();
                crate::cli::run(Command::Prune { json: true }, repo, &mut first).unwrap();
                assert_eq!(String::from_utf8(first).unwrap(), "{\"schema\":1,\"pruned\":[\"old-spike\"]}\n");
                assert!(Registry::load(repo).unwrap().get("old-spike").is_none());

                let mut second = Vec::new();
                crate::cli::run(Command::Prune { json: true }, repo, &mut second).unwrap();
                assert_eq!(String::from_utf8(second).unwrap(), "{\"schema\":1,\"pruned\":[]}\n", "nothing left to prune the second time");
                assert!(Registry::load(repo).unwrap().get("fix-login").is_some(), "the live one is untouched by either run");
            },
        );
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn prune_table_prints_one_line_per_pruned_branch_or_says_there_is_nothing() {
        let dead = vec![workstream("old-spike", Some("%5"), Some("codex"), None)];
        let (printed, after) = prune_with_tmux("prune-table-some", "printf ''", dead, false);
        assert_eq!(printed, "pruned old-spike\n");
        assert_eq!(after.get("old-spike"), None);

        let alive = vec![workstream("fix-login", Some("%3"), Some("claude"), None)];
        let (printed, after) = prune_with_tmux("prune-table-none", r#"printf '%%3 2000000000\n'"#, alive, false);
        assert_eq!(printed, "nothing to prune\n");
        assert!(after.get("fix-login").is_some());
    }

    /// The one case this whole feature exists to get right: no multiplexer reachable must
    /// prune nothing, not everything, and must not error out either — see `poll_or_nothing`.
    #[test]
    fn prune_leaves_every_workstream_alone_when_no_backend_is_reachable() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-prune-no-backend-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vars = [
            ("KANSTACK_STATE_PATH", Some(dir.to_str().unwrap())),
            ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
            ("TMUX_PANE", None),
        ];
        let old: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }

        let repo = Path::new("/repo/prune-no-backend");
        let mut registry = Registry::load(repo).unwrap();
        registry.upsert(workstream("fix-login", Some("%3"), Some("claude"), None));
        registry.save().unwrap();

        let mut out = Vec::new();
        let result = crate::cli::run(Command::Prune { json: true }, repo, &mut out);
        let after = result.as_ref().ok().map(|()| Registry::load(repo).unwrap());

        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        result.expect("prune must not error just because no multiplexer is reachable");
        assert_eq!(String::from_utf8(out).unwrap(), "{\"schema\":1,\"pruned\":[]}\n");
        assert!(after.unwrap().get("fix-login").is_some(), "unknown must never be treated as dead");
    }
}
