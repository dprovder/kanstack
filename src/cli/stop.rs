//! `kanstack stop` — close a pane, ending its harness, and forget the workstream.
//!
//! Idempotent by construction: a pane already gone (closed outside kanstack, or by a `stop`
//! that raced this one and won the registry lock) is treated as success by every backend's own
//! `close` (see `tmux`/`cmux`/`ghostty`/`orca`'s `close`, and `Splitter::stop`'s "already gone
//! counts as stopped"), and a second `stop` on an already-forgotten workstream reports
//! `unknown_workstream` — the same condition a typo'd branch name would, not a silent success —
//! which an orchestrator retrying a `stop` it's unsure landed can treat as "already done".

use std::io::Write;
use std::path::Path;

use anyhow::Result;

use super::exit::{tag, ErrorCode::*};
use super::{resolve_branch, seeded_splitter, PaneResult};
use crate::report::Reports;
use crate::workstream::Registry;

pub(super) fn run(target: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    // Locked for the same reason as `Spawn`: resolving the target, stopping the pane
    // and forgetting the workstream must land as one save, not two processes' saves
    // interleaved.
    let (branch, pane) = Registry::with_lock(cwd, |registry| {
        let branch = resolve_branch(registry, &target)?;
        let pane = registry.get(&branch).and_then(|w| w.pane_id.clone());
        // A workstream with no pane has nothing to close; forgetting it is the stop. Its
        // per-branch state goes too — `Splitter::stop` forgets that for a lane with a pane,
        // but a paneless one can still have had an intent or ask set on it (see
        // `crate::orchestration`), and it mustn't outlive the workstream either.
        if pane.is_some() {
            let mut splitter = seeded_splitter(registry)?;
            splitter.stop(&branch).map_err(|e| tag(MultiplexerUnavailable, e))?;
        } else {
            Reports::for_repo(cwd).forget(&branch);
        }
        registry.remove(&branch);
        Ok((branch, pane))
    })?;
    if json {
        writeln!(out, "{}", super::json_result("stop", branch, PaneResult { pane: pane.map(|p| p.0) })?)?;
    } else {
        writeln!(out, "stopped {branch}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::test_support::*;
    use crate::cli::{dispatch, Command};
    use crate::orchestration::Orchestration;
    use crate::workstream::Registry;

    #[test]
    fn stop_json_reports_the_pane_it_closed_and_null_when_there_was_none() {
        with_tmux_registry(
            "stop-json",
            r#"printf '%%3 2000000000\n'"#,
            vec![workstream("fix-login", Some("%3"), Some("claude"), None), workstream("planned", None, None, None)],
            |repo| {
                let mut out = Vec::new();
                crate::cli::run(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"stop\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\"}}\n"
                );

                let mut out = Vec::new();
                crate::cli::run(Command::Stop { target: "planned".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"stop\",\"workstream\":\"planned\",\"result\":{\"pane\":null}}\n"
                );
            },
        );
    }

    /// A pane already gone — closed outside kanstack, or by a `stop` that raced this one and
    /// won the registry lock — is what every backend's own `close` already treats as success
    /// (see `tmux`/`cmux`/`ghostty`/`orca`'s `close`, and `Splitter::stop`'s "already gone
    /// counts as stopped"), so `stop` must not surface the multiplexer's "no such pane" as a
    /// failure: the workstream ends up forgotten either way, which is what the caller asked
    /// for. No code change needed here — this pins down behavior that was already correct.
    #[test]
    fn stop_on_a_pane_already_closed_outside_kanstack_succeeds_quietly() {
        use crate::mux::stand_in;
        let (bin, _log) = stand_in::install(
            "stop-already-dead",
            "tmux",
            r#"case "$1" in list-panes) printf '' ;; kill-pane) echo "can't find pane %3" >&2; exit 1 ;; esac"#,
        );
        let state = std::env::temp_dir().join(format!("kanstack-cli-stop-already-dead-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        stand_in::with_env(
            &[
                ("KANSTACK_STATE_PATH", Some(state.to_str().unwrap())),
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_TMUX_BIN", Some(bin.to_str().unwrap())),
                ("TMUX_PANE", Some("%0")),
            ],
            || {
                let repo = std::path::Path::new("/repo/stop-already-dead");
                let mut registry = Registry::load(repo).unwrap();
                registry.upsert(workstream("fix-login", Some("%3"), Some("claude"), None));
                registry.save().unwrap();

                let mut out = Vec::new();
                crate::cli::run(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut out)
                    .expect("a pane already gone must not fail the stop");
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"stop\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\"}}\n"
                );
                assert!(Registry::load(repo).unwrap().get("fix-login").is_none(), "the workstream is forgotten either way");
            },
        );
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A second `stop` on a workstream the first one already forgot is indistinguishable, by
    /// construction, from a target that never existed — `stop` removes the registry entry, so
    /// there is nothing left recording that it was ever there. It reports the same
    /// `unknown_workstream` a typo'd branch name would (exit `3`): a specific,
    /// machine-readable condition, not a silent success and not `internal` — an orchestrator
    /// retrying a `stop` it's unsure landed can treat this exact code as "already done".
    #[test]
    fn stopping_an_already_stopped_workstream_is_unknown_workstream_not_a_silent_success() {
        with_tmux_registry(
            "stop-retry",
            r#"printf '%%3 2000000000\n'"#,
            vec![workstream("fix-login", Some("%3"), Some("claude"), None)],
            |repo| {
                crate::cli::run(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut Vec::new()).unwrap();
                assert!(Registry::load(repo).unwrap().get("fix-login").is_none());

                let mut out = Vec::new();
                let mut err_out = Vec::new();
                let code = dispatch(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut out, &mut err_out);
                assert_eq!(code, 3, "unknown_workstream's exit code");
                let printed = String::from_utf8(out).unwrap();
                assert!(
                    printed.contains(r#""code":"unknown_workstream""#),
                    "a retried stop must report a specific, non-ambiguous condition: {printed}"
                );
            },
        );
    }

    /// `stop` drops the lane's intent and pending ask along with its report — with a pane
    /// (through `Splitter::stop`'s `Reports::forget`) and without one (directly) — so neither
    /// haunts whatever is spawned on the branch next. Another lane's are left alone.
    #[test]
    fn stop_forgets_the_intent_and_pending_ask_with_or_without_a_pane() {
        with_tmux_registry(
            "stop-forgets-orchestration",
            r#"printf '%%3 2000000000\n'"#,
            vec![
                workstream("fix-login", Some("%3"), Some("claude"), None),
                workstream("planned", None, None, None),
                workstream("add-search", None, None, None),
            ],
            |repo| {
                let orchestration = Orchestration::for_repo(repo);
                let now = std::time::SystemTime::now();
                for branch in ["fix-login", "planned", "add-search"] {
                    orchestration.set_intent(branch, "busy with something", now).unwrap();
                    orchestration.ask(branch, "ship it?", now).unwrap();
                }

                crate::cli::run(Command::Stop { target: "fix-login".into(), json: false }, repo, &mut Vec::new()).unwrap();
                crate::cli::run(Command::Stop { target: "planned".into(), json: false }, repo, &mut Vec::new()).unwrap();

                for branch in ["fix-login", "planned"] {
                    assert_eq!(orchestration.intent(branch), None, "{branch}");
                    assert_eq!(orchestration.pending_ask(branch), None, "{branch}");
                }
                assert!(orchestration.intent("add-search").is_some(), "a lane that wasn't stopped keeps its intent");
                assert!(orchestration.pending_ask("add-search").is_some());
            },
        );
    }
}
