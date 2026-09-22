//! `kanstack spawn` — open a harness pane on a branch, creating it first if needed.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::but::But;
use crate::pane_status::PaneStatus;
use crate::workstream::{AgentId, BranchId, PaneId, Registry, WorkItemRef, Workstream};

use super::exit::{tag, ErrorCode::*};
use super::{seeded_splitter, spawn_direction};

#[derive(Debug, Serialize)]
struct SpawnResult {
    /// Whether `branch` was newly created by this call, or already existed.
    created: bool,
    pane: String,
    agent: String,
    /// The multiplexer's workspace the pane opened in, when the backend has one (cmux does;
    /// tmux, Orca and Ghostty don't).
    workspace: Option<String>,
}

pub(super) fn run(
    branch: String,
    agent: Option<String>,
    prompt: Option<String>,
    item: Option<String>,
    json: bool,
    cwd: &Path,
    out: &mut impl Write,
) -> Result<()> {
    // Locked for the whole load-mutate-save, including the pane spawn itself: two
    // `kanstack spawn`s racing here must serialize rather than each load the same
    // registry and have the loser's `upsert` vanish under the winner's save.
    let (lines, spawn_result) = Registry::with_lock(cwd, |registry| {
        let mut lines = Vec::new();
        let but = But::discover(cwd).map_err(|e| tag(ButFailed, e))?;
        let mut splitter = seeded_splitter(registry)?;
        // Its own setting, not the board's `KANSTACK_*_DIRECTION`: the board's `above`
        // assumes kanstack is the pane at the bottom, but here the caller is usually an
        // agent's pane, and a new lane belongs beside it.
        splitter.set_first_direction(&spawn_direction(std::env::var("KANSTACK_SPAWN_DIRECTION").ok().as_deref())?);

        // Fetched before the poll below decides anything: whether `branch` is stacked on
        // top of a sibling that already has a pane open determines which anchor the new
        // pane groups with, not just whether the branch itself needs creating.
        let status = but.status().map_err(|e| tag(ButFailed, e))?;
        let exists = status.stacks.iter().flat_map(|s| &s.branches).any(|b| b.name == branch);
        let created = !exists;
        if created {
            but.branch_new(&branch, None).map_err(|e| tag(ButFailed, e))?;
            lines.push(format!("created {branch}"));
        }

        // One poll answers every question below: is the branch's old pane still alive,
        // is a stack sibling's, and which live pane should the new one split off absent
        // either. A failed poll leaves every pane "unknown", which counts as alive —
        // refusing a spawn is the safer error.
        let statuses = splitter.poll_statuses().unwrap_or_default();
        let dead = |b: &BranchId| statuses.get(&b.0) == Some(&PaneStatus::Dead);

        if let Some(w) = registry.get(&branch) {
            if w.pane_id.is_some() && !dead(&w.branch_id) {
                return Err(tag(
                    WorkstreamExists,
                    anyhow::anyhow!("{branch} already has a pane open — use `kanstack send`, `focus` or `stop`"),
                ));
            }
        }

        // A sibling in the same GitButler stack with a live pane already open — group
        // with it (`Splitter::spawn_stacked_harness_with`) rather than the generic
        // "last live pane" fallback below, which knows nothing about which branches are
        // actually stacked together. The first live sibling in stack order wins; a stack
        // rarely has more than one pane open at a time anyway.
        let group_with = status
            .stacks
            .iter()
            .find(|s| s.branches.iter().any(|b| b.name == branch))
            .and_then(|s| {
                s.branches.iter().filter(|b| b.name != branch).find_map(|b| {
                    let w = registry.get(&b.name)?;
                    let pane = w.pane_id.as_ref()?;
                    (!dead(&w.branch_id)).then(|| pane.clone())
                })
            });

        if group_with.is_none() {
            if let Some(last_live) = registry
                .workstreams
                .iter()
                .rev()
                .find(|w| w.branch_id.0 != branch && w.pane_id.is_some() && !dead(&w.branch_id))
            {
                splitter.set_anchor(&last_live.pane_id.as_ref().unwrap().0);
            }
        }

        let pane = match &group_with {
            Some(anchor) => splitter
                .spawn_stacked_harness_with(but.cwd(), &branch, prompt.as_deref(), agent.as_deref(), &anchor.0)
                .map_err(|e| tag(MultiplexerUnavailable, e))?,
            None => splitter
                .spawn_harness_with(but.cwd(), &branch, prompt.as_deref(), agent.as_deref())
                .map_err(|e| tag(MultiplexerUnavailable, e))?,
        };
        let agent = agent
            .or_else(|| std::env::var("KANSTACK_HARNESS").ok())
            .unwrap_or_else(|| "claude".to_string());
        if let Some(workspace) = splitter.workspace() {
            registry.workspace = Some(workspace);
        }
        let item = item
            .map(WorkItemRef)
            .or_else(|| registry.get(&branch).and_then(|w| w.item.clone()));
        registry.upsert(Workstream {
            branch_id: BranchId(branch.clone()),
            pane_id: Some(PaneId(pane.clone())),
            agent: Some(AgentId(agent.clone())),
            item,
        });
        lines.push(match &registry.workspace {
            Some(workspace) => format!("spawned {agent} on {branch} in {pane} ({workspace})"),
            None => format!("spawned {agent} on {branch} in {pane}"),
        });
        let result = SpawnResult { created, pane, agent, workspace: registry.workspace.clone() };
        Ok((lines, result))
    })?;
    if json {
        writeln!(out, "{}", super::json_result("spawn", branch, spawn_result)?)?;
    } else {
        for line in lines {
            writeln!(out, "{line}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::test_support::*;
    use crate::cli::{dispatch, Command};
    use crate::workstream::{Registry, Workstream};

    // `spawn --json` — the one command that also talks to `but`, so its stand-in needs both
    // `but` (for `status`/`branch_new`) and `tmux` (for the pane itself).

    /// Runs `body` against a real, empty temp directory (needed because `But::run` sets
    /// `current_dir` to it, which fails outright against the fake `/repo/...` paths every
    /// other test here uses — nothing under it ever needs to look like a real git repo, since
    /// both `but` and `tmux` are stand-ins) with `KANSTACK_BUT_BIN` answering `--version` and
    /// `status` from `but_status_body`, and `KANSTACK_TMUX_BIN` answering `list-panes` and
    /// `split-window` from `tmux_body`.
    fn with_but_and_tmux(tag: &str, but_status_body: &str, tmux_body: &str, workstreams: Vec<Workstream>, body: impl FnOnce(&std::path::Path)) {
        use crate::mux::stand_in;
        let (but_bin, _but_log) =
            stand_in::install(tag, "but", &format!("case \"$1\" in --version) echo 'but 0.22.3' ;; status) {but_status_body} ;; esac"));
        let (tmux_bin, _tmux_log) = stand_in::install(tag, "tmux", tmux_body);
        let repo = std::env::temp_dir().join(format!("kanstack-cli-{tag}-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        let state = std::env::temp_dir().join(format!("kanstack-cli-{tag}-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        stand_in::with_env(
            &[
                ("KANSTACK_STATE_PATH", Some(state.to_str().unwrap())),
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_TMUX_BIN", Some(tmux_bin.to_str().unwrap())),
                ("KANSTACK_BUT_BIN", Some(but_bin.to_str().unwrap())),
                ("TMUX_PANE", Some("%0")),
            ],
            || {
                let mut registry = Registry::load(&repo).unwrap();
                for w in workstreams {
                    registry.upsert(w);
                }
                registry.save().unwrap();
                body(&repo);
            },
        );
        stand_in::remove(&but_bin);
        stand_in::remove(&tmux_bin);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&state);
    }

    /// `but status`'s reply for these tests: a real capture with a `feat-ui` branch already in
    /// the workspace, so `spawn feat-ui` takes the "branch already exists" path rather than
    /// also needing a stand-in `branch_new` reply.
    fn but_status_cat() -> String {
        format!("cat '{}'", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/status.json"))
    }

    #[test]
    fn spawn_json_reports_the_pane_the_agent_and_whether_it_created_the_branch() {
        with_but_and_tmux(
            "spawn-json",
            &but_status_cat(),
            r#"case "$1" in list-panes) printf '' ;; split-window) echo '%9' ;; esac"#,
            vec![],
            |repo| {
                let mut out = Vec::new();
                crate::cli::run(
                    Command::Spawn { branch: "feat-ui".into(), agent: Some("codex".into()), prompt: None, item: None, json: true },
                    repo,
                    &mut out,
                )
                .unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"spawn\",\"workstream\":\"feat-ui\",\"result\":\
                     {\"created\":false,\"pane\":\"%9\",\"agent\":\"codex\",\"workspace\":null}}\n"
                );
            },
        );
    }

    /// `spawn` on a branch that already has a live pane is `workstream_exists` — exit `4`,
    /// distinct from every other condition here.
    #[test]
    fn spawn_on_an_already_running_workstream_is_a_workstream_exists_error() {
        with_but_and_tmux(
            "spawn-exists-json",
            &but_status_cat(),
            r#"case "$1" in list-panes) printf '%%3 2000000000\n' ;; esac"#,
            vec![workstream("feat-ui", Some("%3"), Some("claude"), None)],
            |repo| {
                let mut out = Vec::new();
                let mut err_out = Vec::new();
                let command = Command::Spawn { branch: "feat-ui".into(), agent: None, prompt: None, item: None, json: true };
                let code = dispatch(command, repo, &mut out, &mut err_out);
                assert_eq!(code, 4);
                assert!(err_out.is_empty());
                let printed = String::from_utf8(out).unwrap();
                assert!(printed.starts_with("{\"schema\":1,\"ok\":false,\"command\":\"spawn\",\"error\":{\"code\":\"workstream_exists\","), "{printed}");
            },
        );
    }

    /// `spawn` failing after `but branch new` succeeds but before the pane actually opens
    /// (e.g. the multiplexer refuses the split) must not leave a workstream registered with no
    /// pane for a retry to trip over: `registry.upsert` only runs after the pane spawn
    /// succeeds, and `Registry::with_lock` only saves once its closure returns `Ok`, so the
    /// failed attempt's in-memory mutations (there aren't any, here) never reach disk. A retry
    /// then sees the branch `but` already created — skipping `branch new` again, which a real
    /// `but` would refuse the second time — and succeeds. No code change needed: this pins
    /// down behavior the locking added for stage 1 (`registry-locking`) already gave for free.
    #[test]
    fn spawn_retried_after_the_pane_fails_to_open_does_not_leave_a_bogus_workstream() {
        let tag = "spawn-retry";
        let status_counter = std::env::temp_dir().join(format!("kanstack-cli-{tag}-status-count-{}", std::process::id()));
        let split_marker = std::env::temp_dir().join(format!("kanstack-cli-{tag}-split-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&status_counter);
        let _ = std::fs::remove_file(&split_marker);
        let before = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/status_no_feat_ui.json");
        let after = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/status.json");

        // `status` is asked at least once per spawn attempt. The very first call overall —
        // before `but branch new` has run — must say `feat-ui` does not exist yet; every call
        // after must say it does, exactly what a real `but` would report once the branch is
        // real, whether or not the attempt asking is the one that created it.
        let but_body = format!(
            "case \"$1\" in --version) echo 'but 0.22.3' ;; status) \
             n=$(( $(cat '{counter}' 2>/dev/null || echo 0) + 1 )); echo \"$n\" > '{counter}'; \
             if [ \"$n\" -le 1 ]; then cat '{before}'; else cat '{after}'; fi ;; esac",
            counter = status_counter.display(),
        );
        // The pane fails to open the first time `split-window` is asked; it succeeds the
        // second time, simulating a transient multiplexer failure a retry gets past.
        let tmux_body = format!(
            "case \"$1\" in list-panes) printf '' ;; split-window) \
             if [ -f '{marker}' ]; then echo '%9'; else touch '{marker}'; echo boom >&2; exit 1; fi ;; esac",
            marker = split_marker.display(),
        );
        with_but_and_tmux(tag, &but_body, &tmux_body, vec![], |repo| {
            let first = crate::cli::run(
                Command::Spawn { branch: "feat-ui".into(), agent: None, prompt: None, item: None, json: false },
                repo,
                &mut Vec::new(),
            );
            assert!(first.is_err(), "the pane never opened, so the attempt must fail");
            assert!(
                Registry::load(repo).unwrap().workstreams.is_empty(),
                "a failed spawn must not register a workstream with no pane"
            );

            let mut out = Vec::new();
            crate::cli::run(Command::Spawn { branch: "feat-ui".into(), agent: None, prompt: None, item: None, json: true }, repo, &mut out)
                .expect("retrying spawn on the same branch must now succeed");
            let printed = String::from_utf8(out).unwrap();
            assert!(
                printed.contains(r#""created":false"#),
                "the branch `but` already created on the failed attempt must not be created again: {printed}"
            );
            assert_eq!(Registry::load(repo).unwrap().get("feat-ui").map(|w| w.pane_id.is_some()), Some(true));
        });
        let _ = std::fs::remove_file(&status_counter);
        let _ = std::fs::remove_file(&split_marker);
    }
}
