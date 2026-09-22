//! The headless subcommands — `kanstack spawn/send/status/focus/stop`, and `report` for the
//! panes to talk back — for driving harness panes from a script or from an agent in another
//! pane, without the board.
//!
//! Each invocation is its own process, so none of them can see the pane handles another
//! one opened. They share state through [`Registry`] instead: every command loads it, seeds
//! a fresh [`Splitter`] with it, acts, and writes back whatever changed.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::but::But;
use crate::model::{BranchStatus, MergeStatus, WorkspaceStatus};
use crate::pane_status::PaneStatus;
use crate::report::{Reported, Reports};
use crate::splitter::Splitter;
use crate::workstream::{AgentId, BranchId, PaneId, Registry, WorkItemRef, Workstream};

pub const SUBCOMMANDS: &[&str] = &["spawn", "send", "status", "focus", "stop", "report"];

pub const HELP: &str = "\
kanstack spawn <branch> [--agent <name>] [--prompt \"...\"] [--item <ref>]
    open a harness pane on <branch>, creating the branch first if it doesn't exist.
    --agent runs that harness (e.g. codex) instead of $KANSTACK_HARNESS; --prompt is
    the harness's first message; --item attaches an opaque work-item reference (e.g.
    github:#42) to the workstream, carried in `status`/`status --json` and never
    interpreted or fetched by kanstack itself
kanstack send <branch|session> \"...\"
    type a message into a pane and submit it
kanstack status [--json]
    list every workstream and whether its pane is busy, idle, waiting on you, or dead. --json prints one
    JSON document instead (schema in the README) and, unlike the table, still lists every
    workstream when no multiplexer is reachable, with their panes' status \"unknown\"
kanstack focus <branch|session>
    bring a pane to the front
kanstack stop <branch|session>
    close a pane, ending its harness, and forget the workstream
kanstack report <busy|idle|waiting> [<branch>]
    say what the agent on <branch> is doing, for `status` and the board to show; waiting
    means stopped on a permission prompt. This is
    what the hooks kanstack gives a harness run (claude's, today); anything else can call it
    too. <branch> defaults to $KANSTACK_BRANCH, which kanstack sets in a pane it launches.
    Prints nothing, and needs no multiplexer. KANSTACK_STATUS_HOOKS=off stops kanstack
    handing harnesses those hooks

<session> is a pane id as `kanstack status` prints it. These need to run inside the
multiplexer the panes live in (see README).
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Spawn { branch: String, agent: Option<String>, prompt: Option<String>, item: Option<String> },
    Send { target: String, text: String },
    Status { json: bool },
    Focus { target: String },
    Stop { target: String },
    Report { state: Reported, branch: Option<String> },
}

/// Parses the arguments after subcommand `name`. `Ok(None)` means help was asked for and
/// has been handled by the caller printing [`HELP`].
pub fn parse(name: &str, args: Vec<String>) -> Result<Option<Command>> {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(None);
    }
    let mut positional = Vec::new();
    let mut agent = None;
    let mut prompt = None;
    let mut item = None;
    let mut json = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let flag = |long: &str, value: Option<&str>, rest: &mut std::vec::IntoIter<String>| -> Result<String> {
            match value {
                Some(v) => Ok(v.to_string()),
                None => rest.next().ok_or_else(|| anyhow::anyhow!("{long} requires a value")),
            }
        };
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        match key.as_str() {
            "--agent" if name == "spawn" => agent = Some(flag("--agent", inline.as_deref(), &mut args)?),
            "--prompt" if name == "spawn" => prompt = Some(flag("--prompt", inline.as_deref(), &mut args)?),
            "--item" if name == "spawn" => item = Some(flag("--item", inline.as_deref(), &mut args)?),
            "--json" if name == "status" => {
                if inline.is_some() {
                    bail!("--json takes no value\n\n{HELP}");
                }
                json = true;
            }
            k if k.starts_with("--") => bail!("unknown option {k:?} for `kanstack {name}`\n\n{HELP}"),
            _ => positional.push(arg),
        }
    }

    let mut positional = positional.into_iter();
    let mut one = |what: &str| {
        positional
            .next()
            .ok_or_else(|| anyhow::anyhow!("`kanstack {name}` needs a {what}\n\n{HELP}"))
    };
    let command = match name {
        "spawn" => Command::Spawn { branch: one("<branch>")?, agent, prompt, item },
        "send" => {
            let target = one("<branch|session>")?;
            let text = positional.collect::<Vec<_>>().join(" ");
            if text.trim().is_empty() {
                bail!("`kanstack send` needs a message\n\n{HELP}");
            }
            return Ok(Some(Command::Send { target, text }));
        }
        "status" => Command::Status { json },
        "focus" => Command::Focus { target: one("<branch|session>")? },
        "stop" => Command::Stop { target: one("<branch|session>")? },
        "report" => {
            let word = one("<busy|idle|waiting>")?;
            let state = Reported::parse(&word).ok_or_else(|| {
                anyhow::anyhow!("`kanstack report` takes busy, idle or waiting, not {word:?}\n\n{HELP}")
            })?;
            Command::Report { state, branch: positional.next() }
        }
        other => bail!("unknown subcommand {other:?}"),
    };
    if positional.next().is_some() {
        bail!("too many arguments for `kanstack {name}`\n\n{HELP}");
    }
    Ok(Some(command))
}

/// Where `spawn` splits off the calling pane: `raw` (`KANSTACK_SPAWN_DIRECTION`) if set,
/// else `right`. Checked here, unlike the board's own direction settings, because a typo
/// would otherwise surface as the multiplexer's own error about a flag it never heard of.
fn spawn_direction(raw: Option<&str>) -> Result<String> {
    match raw.map(str::trim).filter(|d| !d.is_empty()) {
        None => Ok("right".to_string()),
        Some(d) if ["left", "right", "above", "below", "up", "down"].contains(&d) => Ok(d.to_string()),
        Some(d) => bail!("KANSTACK_SPAWN_DIRECTION={d:?} is not one of left, right, above, below"),
    }
}

/// A splitter seeded with every pane in `registry`, or an explanation of why there isn't one.
fn seeded_splitter(registry: &Registry) -> Result<Splitter> {
    let mut splitter = Splitter::discover().ok_or_else(|| {
        anyhow::anyhow!(
            "no harness-split backend found — run this from inside a {} pane \
             (see KANSTACK_SPLIT_BACKEND)",
            crate::splitter::describe_backends()
        )
    })?;
    registry.adopt_into(&mut splitter);
    // Every pane goes in the workspace the others did, not wherever this shell's
    // environment happens to say.
    splitter.set_workspace(registry.workspace.as_deref());
    Ok(splitter)
}

/// The workstream `target` names, that has a pane to act on.
fn target_with_pane<'a>(registry: &'a Registry, target: &str) -> Result<(&'a Workstream, &'a PaneId)> {
    let w = registry
        .resolve(target)
        .ok_or_else(|| anyhow::anyhow!("no workstream for {target:?} — `kanstack status` lists them"))?;
    let pane = w
        .pane_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("{} has no pane open", w.branch_id))?;
    Ok((w, pane))
}

fn label(status: Option<PaneStatus>) -> &'static str {
    match status {
        Some(PaneStatus::Busy) => "busy",
        Some(PaneStatus::Idle) => "idle",
        Some(PaneStatus::Waiting) => "waiting",
        Some(PaneStatus::Dead) => "dead",
        Some(PaneStatus::Unknown) | None => "unknown",
    }
}

/// The `"schema"` of `kanstack status --json`. Fields are only ever added within a version;
/// bump this when one is renamed, removed or changes meaning.
pub const STATUS_SCHEMA: u32 = 1;

/// The document `kanstack status --json` prints. Every field of every entry is always
/// present (`null`, never omitted), so consumers needn't guard, and later steps add fields
/// rather than reshaping these.
#[derive(Debug, Serialize)]
struct StatusReport {
    schema: u32,
    workstreams: Vec<WorkstreamReport>,
    /// The workspace as a whole. `null` when `but` could not be reached.
    workspace: Option<WorkspaceReport>,
}

#[derive(Debug, Serialize)]
struct WorkstreamReport {
    branch: String,
    pane: Option<String>,
    agent: Option<String>,
    item: Option<String>,
    status: ReportStatus,
    /// The lane's git state. `null` when `but` could not be reached, or the branch is not in
    /// the workspace (deleted, or unapplied).
    lane: Option<LaneReport>,
}

/// The state of the workspace itself, for an agent deciding whether to pull.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct WorkspaceReport {
    /// How many commits the target branch has that the workspace doesn't: what `but pull`
    /// would bring in.
    behind: usize,
    /// Changes in the working tree that no lane owns yet.
    uncommitted: usize,
    /// When the workspace last fetched from the remote, as `but` reports it. `behind`, `landed`
    /// and a lane's own `behind` only change on a fetch — `but status` never fetches, and
    /// neither does this — so this is how stale they might be. `null` if never fetched.
    fetched: Option<String>,
}

/// One lane's git state, from `but status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct LaneReport {
    commits: usize,
    /// A commit on the lane is conflicted right now, and needs `but resolve`.
    conflicted: bool,
    /// Commits on the lane's remote branch that the lane doesn't have.
    behind: usize,
    /// What updating the lane from upstream would do; `null` when there is nothing to say —
    /// which includes right after `but pull`, when the update has already happened.
    rebase: Option<Rebase>,
    /// The lane has landed upstream, and `but pull` will remove it (after which the lane is no
    /// longer in the workspace and its `lane` is `null`). Commits on it can't be changed any
    /// more. Only visible between a fetch and that pull.
    landed: bool,
    /// `needs-force` means a plain push would be refused: either the lane's pushed history was
    /// rewritten, or the remote branch has commits the lane doesn't (it has diverged).
    push: PushState,
    /// Uncommitted files assigned to the lane's stack.
    uncommitted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Rebase {
    Clean,
    /// Rebasing the lane onto upstream would conflict.
    Conflicts,
    Integrated,
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum PushState {
    Pushed,
    Unpushed,
    NeedsForce,
    LocalOnly,
    Integrated,
    Unknown,
}

/// `branch`'s git state in `status`, or `None` if the workspace has no such branch.
fn lane_report(status: &WorkspaceStatus, branch: &str) -> Option<LaneReport> {
    let (stack, lane) = status
        .stacks
        .iter()
        .find_map(|stack| stack.branches.iter().find(|b| b.name == branch).map(|b| (stack, b)))?;
    Some(LaneReport {
        commits: lane.commits.len(),
        conflicted: lane.commits.iter().any(|c| c.conflicted == Some(true)),
        behind: lane.upstream_commits.len(),
        rebase: lane.merge_status.map(|m| match m {
            MergeStatus::Clean => Rebase::Clean,
            MergeStatus::Conflicted { .. } => Rebase::Conflicts,
            MergeStatus::Integrated => Rebase::Integrated,
            MergeStatus::Empty => Rebase::Empty,
        }),
        landed: lane.branch_status == BranchStatus::Integrated || lane.merge_status == Some(MergeStatus::Integrated),
        push: match lane.branch_status {
            BranchStatus::NothingToPush => PushState::Pushed,
            BranchStatus::UnpushedCommits => PushState::Unpushed,
            BranchStatus::UnpushedCommitsRequiringForce => PushState::NeedsForce,
            BranchStatus::CompletelyUnpushed => PushState::LocalOnly,
            BranchStatus::Integrated => PushState::Integrated,
            BranchStatus::Unknown => PushState::Unknown,
        },
        uncommitted: stack.assigned_changes.len(),
    })
}

fn workspace_report(status: &WorkspaceStatus) -> WorkspaceReport {
    WorkspaceReport {
        behind: status.upstream_state.behind,
        uncommitted: status.uncommitted_changes.len(),
        fetched: status.upstream_state.last_fetched.clone(),
    }
}

/// [`PaneStatus`] plus `NoPane`, which is a fact about the registry rather than about a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ReportStatus {
    Busy,
    Idle,
    Waiting,
    Dead,
    Unknown,
    NoPane,
}

impl From<Option<PaneStatus>> for ReportStatus {
    fn from(status: Option<PaneStatus>) -> Self {
        match status {
            Some(PaneStatus::Busy) => ReportStatus::Busy,
            Some(PaneStatus::Idle) => ReportStatus::Idle,
            Some(PaneStatus::Waiting) => ReportStatus::Waiting,
            Some(PaneStatus::Dead) => ReportStatus::Dead,
            Some(PaneStatus::Unknown) | None => ReportStatus::Unknown,
        }
    }
}

/// Every workstream in `registry`, with `status_of` answering for the ones that have a pane
/// (by branch, as `Splitter::poll_statuses` keys them). Taking a lookup rather than a
/// splitter keeps this free of a real multiplexer; a lookup that knows nothing yields
/// `unknown` for every pane.
fn report(
    registry: &Registry,
    status_of: impl Fn(&str) -> Option<PaneStatus>,
    git: Option<&WorkspaceStatus>,
) -> StatusReport {
    let workstreams = registry
        .workstreams
        .iter()
        .map(|w| WorkstreamReport {
            branch: w.branch_id.0.clone(),
            pane: w.pane_id.as_ref().map(|p| p.0.clone()),
            agent: w.agent.as_ref().map(|a| a.0.clone()),
            item: w.item.as_ref().map(|i| i.0.clone()),
            status: match w.pane_id {
                Some(_) => status_of(&w.branch_id.0).into(),
                None => ReportStatus::NoPane,
            },
            lane: git.and_then(|status| lane_report(status, &w.branch_id.0)),
        })
        .collect();
    StatusReport { schema: STATUS_SCHEMA, workstreams, workspace: git.map(workspace_report) }
}

/// The workspace's git state, or nothing if `but` isn't reachable — `status --json` reports
/// what it can either way. One `but status` call, so it is only made when there is a
/// workstream to report on.
fn git_state(registry: &Registry, cwd: &Path) -> Option<WorkspaceStatus> {
    if registry.workstreams.is_empty() {
        return None;
    }
    But::discover(cwd).ok()?.status_with_upstream().ok()
}

/// Pane statuses by branch, or nothing at all if there's no multiplexer to ask or the poll
/// fails — `status --json` reports what's registered either way.
fn poll_or_nothing(registry: &Registry) -> HashMap<String, PaneStatus> {
    if registry.workstreams.iter().all(|w| w.pane_id.is_none()) {
        return HashMap::new();
    }
    seeded_splitter(registry).and_then(|s| s.poll_statuses()).unwrap_or_default()
}

pub fn run(command: Command, cwd: &Path, out: &mut impl Write) -> Result<()> {
    // Before the registry is even read: this runs from a harness's hooks, on every turn, and
    // a registry problem must not make it noisy or slow. And it writes nothing to `out` —
    // Claude adds a `UserPromptSubmit` hook's stdout to what the model sees.
    if let Command::Report { state, branch } = &command {
        let branch = branch
            .clone()
            .or_else(|| std::env::var("KANSTACK_BRANCH").ok())
            .filter(|b| !b.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "`kanstack report` needs a branch: name one, or run it where $KANSTACK_BRANCH is set \
                     (in a pane kanstack launched)"
                )
            })?;
        return Reports::for_repo(cwd).write(&branch, *state, SystemTime::now());
    }

    let mut registry = Registry::load(cwd)?;
    match command {
        Command::Spawn { branch, agent, prompt } => {
            let but = But::discover(cwd)?;
            let mut splitter = seeded_splitter(&registry)?;
            // Its own setting, not the board's `KANSTACK_*_DIRECTION`: the board's `above`
            // assumes kanstack is the pane at the bottom, but here the caller is usually an
            // agent's pane, and a new lane belongs beside it.
            splitter.set_first_direction(&spawn_direction(std::env::var("KANSTACK_SPAWN_DIRECTION").ok().as_deref())?);

            // Fetched before the poll below decides anything: whether `branch` is stacked on
            // top of a sibling that already has a pane open determines which anchor the new
            // pane groups with, not just whether the branch itself needs creating.
            let status = but.status()?;
            let exists = status.stacks.iter().flat_map(|s| &s.branches).any(|b| b.name == branch);
            if !exists {
                but.branch_new(&branch, None)?;
                writeln!(out, "created {branch}")?;
            }

            // One poll answers every question below: is the branch's old pane still alive,
            // is a stack sibling's, and which live pane should the new one split off absent
            // either. A failed poll leaves every pane "unknown", which counts as alive —
            // refusing a spawn is the safer error.
            let statuses = splitter.poll_statuses().unwrap_or_default();
            let dead = |b: &BranchId| statuses.get(&b.0) == Some(&PaneStatus::Dead);

            if let Some(w) = registry.get(&branch) {
                if w.pane_id.is_some() && !dead(&w.branch_id) {
                    bail!("{branch} already has a pane open — use `kanstack send`, `focus` or `stop`");
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
                Some(anchor) => {
                    splitter.spawn_stacked_harness_with(but.cwd(), &branch, prompt.as_deref(), agent.as_deref(), &anchor.0)?
                }
                None => splitter.spawn_harness_with(but.cwd(), &branch, prompt.as_deref(), agent.as_deref())?,
            };
            let agent = agent
                .or_else(|| std::env::var("KANSTACK_HARNESS").ok())
                .unwrap_or_else(|| "claude".to_string());
            if let Some(workspace) = splitter.workspace() {
                registry.workspace = Some(workspace);
            }
            let item = registry.get(&branch).and_then(|w| w.item.clone());
            registry.upsert(Workstream {
                branch_id: BranchId(branch.clone()),
                pane_id: Some(PaneId(pane.clone())),
                agent: Some(AgentId(agent.clone())),
                item,
            });
            registry.save()?;
            match &registry.workspace {
                Some(workspace) => writeln!(out, "spawned {agent} on {branch} in {pane} ({workspace})")?,
                None => writeln!(out, "spawned {agent} on {branch} in {pane}")?,
            }
        }
        Command::Send { target, text } => {
            let splitter = seeded_splitter(&registry)?;
            let (w, _) = target_with_pane(&registry, &target)?;
            splitter.send_task(&w.branch_id.0, &text)?;
            writeln!(out, "sent to {}", w.branch_id)?;
        }
        Command::Status { json: true } => {
            let statuses = poll_or_nothing(&registry);
            let git = git_state(&registry, cwd);
            let report = report(&registry, |branch| statuses.get(branch).copied(), git.as_ref());
            writeln!(out, "{}", serde_json::to_string(&report)?)?;
        }
        Command::Status { json: false } => {
            if registry.workstreams.is_empty() {
                writeln!(out, "no workstreams — `kanstack spawn <branch>` starts one")?;
                return Ok(());
            }
            let mut splitter = seeded_splitter(&registry)?;
            let statuses = splitter.poll_statuses()?;
            splitter.apply_statuses(statuses);
            let width = registry.workstreams.iter().map(|w| w.branch_id.0.len()).max().unwrap_or(0);
            for w in &registry.workstreams {
                let status = match w.pane_id {
                    Some(_) => label(splitter.pane_status(&w.branch_id.0)),
                    None => "no pane",
                };
                writeln!(
                    out,
                    "{:<width$}  {:<7}  {:<10}  {}{}",
                    w.branch_id.0,
                    status,
                    w.pane_id.as_ref().map_or_else(String::new, |p| p.0.clone()),
                    w.agent.as_ref().map_or_else(String::new, |a| a.0.clone()),
                    w.item.as_ref().map_or_else(String::new, |i| format!("  [{i}]")),
                )?;
            }
        }
        Command::Focus { target } => {
            let splitter = seeded_splitter(&registry)?;
            let (w, _) = target_with_pane(&registry, &target)?;
            splitter.focus(&w.branch_id.0)?;
            writeln!(out, "focused {}", w.branch_id)?;
        }
        Command::Report { .. } => unreachable!("handled before the registry is loaded"),
        Command::Stop { target } => {
            let branch = registry
                .resolve(&target)
                .map(|w| w.branch_id.0.clone())
                .ok_or_else(|| anyhow::anyhow!("no workstream for {target:?} — `kanstack status` lists them"))?;
            // A workstream with no pane has nothing to close; forgetting it is the stop.
            if registry.get(&branch).is_some_and(|w| w.pane_id.is_some()) {
                let mut splitter = seeded_splitter(&registry)?;
                splitter.stop(&branch)?;
            }
            registry.remove(&branch);
            registry.save()?;
            writeln!(out, "stopped {branch}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workstream::WorkItemRef;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn spawn_takes_a_branch_and_optional_agent_and_prompt() {
        assert_eq!(
            parse("spawn", args(&["fix-login"])).unwrap(),
            Some(Command::Spawn { branch: "fix-login".into(), agent: None, prompt: None, item: None })
        );
        assert_eq!(
            parse("spawn", args(&["--agent", "codex", "fix-login", "--prompt", "fix the flaky test"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-login".into(),
                agent: Some("codex".into()),
                prompt: Some("fix the flaky test".into()),
                item: None,
            })
        );
        assert_eq!(
            parse("spawn", args(&["b", "--agent=codex"])).unwrap(),
            Some(Command::Spawn { branch: "b".into(), agent: Some("codex".into()), prompt: None, item: None })
        );
    }

    #[test]
    fn spawn_takes_an_opaque_item_reference() {
        assert_eq!(
            parse("spawn", args(&["fix-parser", "--item", "github:#42"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-parser".into(),
                agent: None,
                prompt: None,
                item: Some("github:#42".into()),
            })
        );
        assert_eq!(
            parse("spawn", args(&["fix-parser", "--item=linear:ENG-7"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-parser".into(),
                agent: None,
                prompt: None,
                item: Some("linear:ENG-7".into()),
            })
        );
        assert!(parse("spawn", args(&["b", "--item"])).is_err(), "--item requires a value");
        assert!(parse("send", args(&["b", "--item", "GH-1"])).is_err(), "--item is spawn-only");
    }

    #[test]
    fn send_joins_the_rest_into_one_message() {
        assert_eq!(
            parse("send", args(&["fix-login", "run", "the", "tests"])).unwrap(),
            Some(Command::Send { target: "fix-login".into(), text: "run the tests".into() })
        );
        assert!(parse("send", args(&["fix-login"])).is_err());
        assert!(parse("send", args(&[])).is_err());
    }

    #[test]
    fn the_single_target_commands_take_exactly_one_argument() {
        assert_eq!(parse("focus", args(&["%3"])).unwrap(), Some(Command::Focus { target: "%3".into() }));
        assert_eq!(parse("stop", args(&["a"])).unwrap(), Some(Command::Stop { target: "a".into() }));
        assert!(parse("stop", args(&[])).is_err());
        assert!(parse("stop", args(&["a", "b"])).is_err());
        assert_eq!(parse("status", args(&[])).unwrap(), Some(Command::Status { json: false }));
        assert!(parse("status", args(&["extra"])).is_err());
    }

    #[test]
    fn status_takes_a_json_flag_and_no_other_command_does() {
        assert_eq!(parse("status", args(&["--json"])).unwrap(), Some(Command::Status { json: true }));
        assert!(parse("status", args(&["--json=true"])).is_err(), "it is a switch, not an option with a value");
        assert!(parse("status", args(&["--json", "extra"])).is_err());
        assert!(parse("spawn", args(&["b", "--json"])).is_err());
        assert!(parse("send", args(&["b", "hi", "--json"])).is_err());
        assert!(parse("focus", args(&["b", "--json"])).is_err());
        assert!(parse("stop", args(&["b", "--json"])).is_err());
    }

    fn registry_of(workstreams: Vec<Workstream>) -> Registry {
        let mut registry = Registry::default();
        registry.workstreams = workstreams;
        registry
    }

    fn workstream(branch: &str, pane: Option<&str>, agent: Option<&str>, item: Option<&str>) -> Workstream {
        Workstream {
            branch_id: BranchId(branch.into()),
            pane_id: pane.map(|p| PaneId(p.into())),
            agent: agent.map(|a| AgentId(a.into())),
            item: item.map(|i| WorkItemRef(i.into())),
        }
    }

    fn five_workstreams() -> Registry {
        registry_of(vec![
            workstream("fix-login", Some("%3"), Some("claude"), Some("GH-4")),
            workstream("add-search", Some("%4"), Some("codex"), None),
            workstream("old-spike", Some("%5"), None, None),
            workstream("mystery", Some("%6"), Some("claude"), None),
            workstream("planned", None, None, Some("GH-9")),
        ])
    }

    #[test]
    fn the_status_json_shape_is_pinned() {
        let statuses = HashMap::from([
            ("fix-login".to_string(), PaneStatus::Busy),
            ("add-search".to_string(), PaneStatus::Idle),
            ("old-spike".to_string(), PaneStatus::Dead),
            ("mystery".to_string(), PaneStatus::Unknown),
            // No entry for "planned": it has no pane, so nobody is asked.
            ("stray".to_string(), PaneStatus::Busy),
        ]);
        let json = serde_json::to_string(&report(&five_workstreams(), |b| statuses.get(b).copied(), None)).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"schema":1,"workstreams":["#,
                r#"{"branch":"fix-login","pane":"%3","agent":"claude","item":"GH-4","status":"busy","lane":null},"#,
                r#"{"branch":"add-search","pane":"%4","agent":"codex","item":null,"status":"idle","lane":null},"#,
                r#"{"branch":"old-spike","pane":"%5","agent":null,"item":null,"status":"dead","lane":null},"#,
                r#"{"branch":"mystery","pane":"%6","agent":"claude","item":null,"status":"unknown","lane":null},"#,
                r#"{"branch":"planned","pane":null,"agent":null,"item":"GH-9","status":"no-pane","lane":null}"#,
                r#"],"workspace":null}"#
            )
        );
    }

    #[test]
    fn a_pane_the_poll_did_not_mention_is_unknown_and_a_paneless_workstream_stays_no_pane() {
        let nothing = report(&five_workstreams(), |_| None, None);
        let statuses: Vec<_> = nothing.workstreams.iter().map(|w| w.status).collect();
        assert_eq!(
            statuses,
            [
                ReportStatus::Unknown,
                ReportStatus::Unknown,
                ReportStatus::Unknown,
                ReportStatus::Unknown,
                ReportStatus::NoPane
            ]
        );
    }

    #[test]
    fn an_empty_registry_is_an_empty_list_not_prose() {
        let json = serde_json::to_string(&report(&Registry::default(), |_| None, None)).unwrap();
        assert_eq!(json, r#"{"schema":1,"workstreams":[],"workspace":null}"#);
    }

    /// With no multiplexer to ask, `status --json` still lists everything and exits 0. Forcing
    /// the tmux backend with no `TMUX_PANE` makes `discover` come back `None` wherever the
    /// test runs.
    #[test]
    fn status_json_degrades_to_unknown_when_no_backend_is_reachable() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-status-json-{}", std::process::id()));
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

        let repo = Path::new("/repo/status-json");
        let mut registry = Registry::load(repo).unwrap();
        registry.upsert(workstream("fix-login", Some("%3"), Some("claude"), None));
        registry.upsert(workstream("planned", None, None, None));
        registry.save().unwrap();

        let mut out = Vec::new();
        let result = run(Command::Status { json: true }, repo, &mut out);
        let human = run(Command::Status { json: false }, repo, &mut Vec::new());

        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        result.unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                r#"{"schema":1,"workstreams":["#,
                r#"{"branch":"fix-login","pane":"%3","agent":"claude","item":null,"status":"unknown","lane":null},"#,
                r#"{"branch":"planned","pane":null,"agent":null,"item":null,"status":"no-pane","lane":null}"#,
                "],\"workspace\":null}\n"
            )
        );
        assert!(human.is_err(), "the table still needs a backend");
    }

    /// `--item` is opaque end to end: whatever string `spawn` is given is exactly what
    /// `status --json` reports back, with no parsing, provider special-casing or fetching
    /// in between.
    #[test]
    fn spawn_item_round_trips_through_status_json() {
        let Some(Command::Spawn { item, .. }) =
            parse("spawn", args(&["fix-parser", "--item", "github:#42"])).unwrap()
        else {
            panic!("expected a Spawn command");
        };
        assert_eq!(item, Some("github:#42".to_string()));

        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-spawn-item-{}", std::process::id()));
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

        let repo = Path::new("/repo/spawn-item");
        let mut registry = Registry::load(repo).unwrap();
        // Mirrors what `run`'s `Command::Spawn` arm does with the parsed `--item`: fold it
        // into the `Workstream` that gets upserted, without ever interpreting the string.
        registry.upsert(workstream("fix-parser", Some("%3"), Some("claude"), item.as_deref()));
        registry.save().unwrap();

        let mut out = Vec::new();
        let result = run(Command::Status { json: true }, repo, &mut out);

        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);

        result.unwrap();
        let json = String::from_utf8(out).unwrap();
        assert!(json.contains(r#""branch":"fix-parser","pane":"%3","agent":"claude","item":"github:#42""#), "{json}");
    }

    #[test]
    fn options_are_rejected_where_they_do_not_apply() {
        assert!(parse("spawn", args(&["b", "--bogus"])).is_err());
        assert!(parse("send", args(&["b", "--agent", "codex"])).is_err());
        assert!(parse("spawn", args(&["b", "--agent"])).is_err());
    }

    #[test]
    fn spawn_direction_defaults_to_right_and_rejects_unknown_values() {
        assert_eq!(spawn_direction(None).unwrap(), "right");
        assert_eq!(spawn_direction(Some("")).unwrap(), "right");
        assert_eq!(spawn_direction(Some("below")).unwrap(), "below");
        assert_eq!(spawn_direction(Some(" left ")).unwrap(), "left");
        assert!(spawn_direction(Some("sideways")).is_err());
    }

    #[test]
    fn help_is_not_an_error() {
        assert_eq!(parse("spawn", args(&["--help"])).unwrap(), None);
    }

    // `kanstack report`, the one subcommand that is run *by* a harness rather than for one.

    /// Runs `body` with `KANSTACK_STATE_PATH` pointing at a fresh directory, and `extra` set
    /// too, restoring everything afterwards. Serialized with the other tests that touch the
    /// environment.
    fn with_state(tag: &str, extra: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut vars = vec![("KANSTACK_STATE_PATH", Some(dir.to_str().unwrap()))];
        vars.extend_from_slice(extra);
        let old: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in &vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        body();
        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn report_takes_busy_or_idle_and_an_optional_branch() {
        assert_eq!(
            parse("report", args(&["busy"])).unwrap(),
            Some(Command::Report { state: Reported::Busy, branch: None })
        );
        assert_eq!(
            parse("report", args(&["idle", "fix-login"])).unwrap(),
            Some(Command::Report { state: Reported::Idle, branch: Some("fix-login".into()) })
        );
        assert!(parse("report", args(&[])).is_err());
        assert!(parse("report", args(&["dead"])).is_err(), "only busy and idle can be reported");
        assert!(parse("report", args(&["busy", "a", "b"])).is_err());
        assert!(parse("report", args(&["busy", "--json"])).is_err());
    }

    /// Claude adds a `UserPromptSubmit` hook's stdout to what the model sees, so a
    /// confirmation line here would end up in every prompt.
    #[test]
    fn report_records_what_it_was_told_and_prints_nothing() {
        with_state("report", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report");
            let mut out = Vec::new();
            run(Command::Report { state: Reported::Busy, branch: Some("fix-login".into()) }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "stdout must stay empty: {:?}", String::from_utf8_lossy(&out));
            assert_eq!(Reports::for_repo(repo).status("fix-login", SystemTime::now()), Some(PaneStatus::Busy));

            run(Command::Report { state: Reported::Idle, branch: Some("fix-login".into()) }, repo, &mut out).unwrap();
            assert_eq!(Reports::for_repo(repo).status("fix-login", SystemTime::now()), Some(PaneStatus::Idle));
        });
    }

    /// The hooks kanstack hands a harness are the same for every lane; which lane is speaking
    /// comes from the environment it was launched with.
    #[test]
    fn report_names_its_lane_from_the_environment_unless_told_otherwise() {
        with_state("report-env", &[("KANSTACK_BRANCH", Some("from-env"))], || {
            let repo = Path::new("/repo/report-env");
            run(Command::Report { state: Reported::Busy, branch: None }, repo, &mut Vec::new()).unwrap();
            run(Command::Report { state: Reported::Idle, branch: Some("named".into()) }, repo, &mut Vec::new()).unwrap();
            let now = SystemTime::now();
            assert_eq!(Reports::for_repo(repo).status("from-env", now), Some(PaneStatus::Busy));
            assert_eq!(Reports::for_repo(repo).status("named", now), Some(PaneStatus::Idle));
        });
    }

    #[test]
    fn report_without_a_branch_anywhere_says_what_it_needs() {
        for unset in [None, Some("")] {
            with_state("report-nobranch", &[("KANSTACK_BRANCH", unset)], || {
                let err = run(Command::Report { state: Reported::Busy, branch: None }, Path::new("/repo/x"), &mut Vec::new())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("KANSTACK_BRANCH"), "{err}");
            });
        }
    }

    /// It runs on every turn, so a registry that has gone bad — which stops the other
    /// subcommands — must not get in its way.
    #[test]
    fn report_does_not_read_the_workstream_registry() {
        with_state("report-registry", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report-registry");
            let registry = crate::workstream::state_path(repo).unwrap();
            std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
            std::fs::write(&registry, "{ not a registry").unwrap();
            assert!(Registry::load(repo).is_err(), "the fixture must really be broken");
            run(Command::Report { state: Reported::Busy, branch: Some("a".into()) }, repo, &mut Vec::new()).unwrap();
        });
    }

    // `status --json` where a backend IS found, against a stand-in `tmux`. The neighbouring
    // test covers no backend at all; these cover the poll itself succeeding and failing.

    /// The JSON `status --json` prints for one registered workstream, `fix-login` in `%3`,
    /// with `tmux list-panes` answering `list_panes` (a shell fragment), and an agent report
    /// of `reported` written first if given.
    fn status_json_with_tmux(tag: &str, list_panes: &str, reported: Option<Reported>) -> String {
        use crate::mux::stand_in;
        let (bin, _log) = stand_in::install(tag, "tmux", &format!(r#"case "$1" in list-panes) {list_panes} ;; esac"#));
        let state = std::env::temp_dir().join(format!("kanstack-cli-{tag}-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let mut json = String::new();
        stand_in::with_env(
            &[
                ("KANSTACK_STATE_PATH", Some(state.to_str().unwrap())),
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_TMUX_BIN", Some(bin.to_str().unwrap())),
                ("TMUX_PANE", Some("%0")),
            ],
            || {
                let repo = Path::new("/repo/status-json-backend");
                let mut registry = Registry::load(repo).unwrap();
                registry.upsert(workstream("fix-login", Some("%3"), Some("claude"), None));
                registry.save().unwrap();
                if let Some(state) = reported {
                    Reports::for_repo(repo).write("fix-login", state, SystemTime::now()).unwrap();
                }
                let mut out = Vec::new();
                run(Command::Status { json: true }, repo, &mut out).expect("status --json must not fail here");
                json = String::from_utf8(out).unwrap();
            },
        );
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&state);
        json
    }

    fn fix_login(status: &str) -> String {
        format!(r#"{{"schema":1,"workstreams":[{{"branch":"fix-login","pane":"%3","agent":"claude","item":null,"status":"{status}","lane":null}}],"workspace":null}}{}"#, "\n")
    }

    #[test]
    fn status_json_reads_a_reachable_backend() {
        // A pane that is listed, with a pid nothing is running under: idle.
        assert_eq!(status_json_with_tmux("json-ok", r#"printf '%%3 2000000000\n'"#, None), fix_login("idle"));
        // A pane that is not listed: dead.
        assert_eq!(status_json_with_tmux("json-dead", r#"printf '%%9 2000000000\n'"#, None), fix_login("dead"));
    }

    /// A backend that is found but cannot answer degrades exactly as an absent one does.
    #[test]
    fn status_json_degrades_to_unknown_when_the_backend_is_found_but_the_poll_fails() {
        let failing = r#"echo "no server running" >&2; exit 1"#;
        assert_eq!(status_json_with_tmux("json-poll-fails", failing, None), fix_login("unknown"));
    }

    /// The multiplexer being unreadable must not hide what the agent itself said.
    #[test]
    fn status_json_keeps_a_fresh_agent_report_when_the_poll_fails() {
        let failing = r#"echo "no server running" >&2; exit 1"#;
        assert_eq!(status_json_with_tmux("json-report", failing, Some(Reported::Busy)), fix_login("busy"));
    }

    #[test]
    fn report_accepts_waiting_and_names_all_three_words_when_given_another() {
        assert_eq!(
            parse("report", args(&["waiting", "fix-login"])).unwrap(),
            Some(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()) })
        );
        let err = parse("report", args(&["asleep"])).unwrap_err().to_string();
        assert!(err.contains("busy, idle or waiting"), "{err}");
    }

    /// A pane blocked on a permission prompt is its own status, not `idle` — it will not
    /// move until the user does.
    #[test]
    fn a_waiting_pane_is_waiting_in_both_the_table_and_the_json() {
        assert_eq!(label(Some(PaneStatus::Waiting)), "waiting");
        let json = serde_json::to_string(&report(&five_workstreams(), |b| (b == "fix-login").then_some(PaneStatus::Waiting), None)).unwrap();
        assert!(json.contains(r#""branch":"fix-login","pane":"%3","agent":"claude","item":"GH-4","status":"waiting""#), "{json}");
    }

    // Lane state. These read REAL captures of `but status -f --json` (tests/fixtures/
    // status_lane_*.json), taken from a scratch GitButler repo with a local remote by driving
    // it into each state — and for the upstream ones, fetching first, since `but status`
    // never does. Only what `but` 0.22.0 can't be made to produce from the command line is
    // hand-set below, and says so.

    fn capture(raw: &str) -> WorkspaceStatus {
        crate::but::parse_status(raw).unwrap()
    }

    fn lane_of(raw: &str, branch: &str) -> Option<LaneReport> {
        lane_report(&capture(raw), branch)
    }

    const PUSHED: &str = include_str!("../tests/fixtures/status_lane_pushed.json");
    const UNPUSHED: &str = include_str!("../tests/fixtures/status_lane_unpushed.json");
    const NEEDS_FORCE: &str = include_str!("../tests/fixtures/status_lane_needs_force.json");
    const BEHIND_REMOTE: &str = include_str!("../tests/fixtures/status_lane_behind_remote.json");
    const LANDED_NO_FLAG: &str = include_str!("../tests/fixtures/status_lane_landed_no_upstream_flag.json");
    const LANDED: &str = include_str!("../tests/fixtures/status_lane_landed_upstream_flag.json");
    const CONFLICT_NO_FLAG: &str = include_str!("../tests/fixtures/status_lane_conflict_expected_no_upstream_flag.json");
    const CONFLICT_EXPECTED: &str = include_str!("../tests/fixtures/status_lane_conflict_expected_upstream_flag.json");
    const CONFLICTED: &str = include_str!("../tests/fixtures/status_lane_conflicted.json");

    fn clean(commits: usize, push: PushState) -> LaneReport {
        LaneReport { commits, conflicted: false, behind: 0, rebase: None, landed: false, push, uncommitted: 0 }
    }

    #[test]
    fn a_pushed_lane_and_one_with_a_local_commit_after_the_push() {
        assert_eq!(lane_of(PUSHED, "lane-pushed"), Some(clean(1, PushState::Pushed)));
        assert_eq!(lane_of(UNPUSHED, "lane-pushed"), Some(clean(2, PushState::Unpushed)));
    }

    /// Rewriting a commit that was already pushed.
    #[test]
    fn a_lane_whose_pushed_history_was_rewritten_needs_a_force_push() {
        assert_eq!(lane_of(NEEDS_FORCE, "lane-force"), Some(clean(1, PushState::NeedsForce)));
        assert_eq!(lane_of(NEEDS_FORCE, "lane-pushed"), Some(clean(2, PushState::Pushed)));
    }

    /// Nothing was rewritten here: someone else pushed to the lane's remote branch. `but`
    /// still calls it "requiring force", because the lane and its remote have diverged, so
    /// `needs-force` means a plain push would be refused, not necessarily that history changed.
    #[test]
    fn a_lane_behind_its_own_remote_branch_is_diverged_and_also_needs_force() {
        let lane = lane_of(BEHIND_REMOTE, "lane-pushed").unwrap();
        assert_eq!((lane.commits, lane.behind, lane.push), (2, 1, PushState::NeedsForce));
        assert!(!lane.conflicted && !lane.landed);
    }

    /// The same lane in the state just before the fetch that revealed it. `but status` reads
    /// remote-tracking refs, so until something fetches, the remote looks unchanged.
    #[test]
    fn behind_only_appears_after_a_fetch() {
        let before = lane_of(include_str!("../tests/fixtures/status_lane_needs_force.json"), "lane-pushed").unwrap();
        assert_eq!((before.behind, before.push), (0, PushState::Pushed));
        assert_eq!(lane_of(BEHIND_REMOTE, "lane-pushed").unwrap().behind, 1);
    }

    #[test]
    fn the_workspace_reports_how_far_behind_the_target_it_is_and_when_it_last_fetched() {
        let workspace = workspace_report(&capture(LANDED));
        assert_eq!((workspace.behind, workspace.uncommitted), (2, 0));
        let raw: serde_json::Value = serde_json::from_str(LANDED).unwrap();
        assert_eq!(workspace.fetched.as_deref(), raw["upstreamState"]["lastFetched"].as_str());
        assert!(workspace.fetched.is_some());
    }

    /// A lane that was merged into the target upstream, seen after a fetch and before
    /// `but pull` removes it — captured with `-u`, which is how kanstack asks.
    #[test]
    fn a_lane_that_landed_upstream_is_flagged_and_the_others_can_rebase_cleanly() {
        let landed = lane_of(LANDED, "lane-land").unwrap();
        assert_eq!(
            landed,
            LaneReport {
                commits: 1,
                conflicted: false,
                behind: 0,
                rebase: Some(Rebase::Integrated),
                landed: true,
                push: PushState::Integrated,
                uncommitted: 0,
            }
        );
        let force = lane_of(LANDED, "lane-force").unwrap();
        assert_eq!((force.rebase, force.landed), (Some(Rebase::Clean), false));
        let pushed = lane_of(LANDED, "lane-pushed").unwrap();
        assert_eq!((pushed.rebase, pushed.behind), (Some(Rebase::Clean), 1));
    }

    /// `branchStatus: integrated` alone says a lane has landed, so that survives without `-u`.
    #[test]
    fn landed_is_still_known_without_the_upstream_flag_but_rebase_is_not() {
        let lane = lane_of(LANDED_NO_FLAG, "lane-land").unwrap();
        assert!(lane.landed);
        assert_eq!((lane.push, lane.rebase), (PushState::Integrated, None));
    }

    /// This is why `status --json` asks `but` for `-u`: without it `mergeStatus` is absent
    /// entirely, so `rebase` would be null for every lane, including one about to conflict.
    #[test]
    fn a_lane_that_would_conflict_on_update_says_so_only_when_asked_with_the_upstream_flag() {
        assert_eq!(lane_of(CONFLICT_NO_FLAG, "lane-conflict").unwrap().rebase, None);
        let lane = lane_of(CONFLICT_EXPECTED, "lane-conflict").unwrap();
        assert_eq!(lane.rebase, Some(Rebase::Conflicts));
        assert!(!lane.conflicted, "nothing is conflicted until the pull actually rebases it");
        assert_eq!(workspace_report(&capture(CONFLICT_EXPECTED)).behind, 3);
    }

    /// After `but pull` the conflict is real and the landed lane has been removed from the
    /// workspace, so it has no lane to report.
    #[test]
    fn after_the_pull_the_commit_is_conflicted_and_the_landed_lane_is_gone() {
        let conflicted = lane_of(CONFLICTED, "lane-conflict").unwrap();
        assert_eq!((conflicted.conflicted, conflicted.push, conflicted.commits), (true, PushState::LocalOnly, 1));
        assert_eq!(lane_of(CONFLICTED, "lane-land"), None);
        assert_eq!(workspace_report(&capture(CONFLICTED)).behind, 0);
    }

    // Hand-set from here: what this `but` cannot be made to produce.
    //
    // Assigned changes: 0.22.0's command line has no way to assign an uncommitted file to a
    // stack (`but rub` is retired, `but move` refuses uncommitted sources, and editing a file a
    // lane committed did not auto-assign it), so a real capture with `assignedChanges` filled
    // in couldn't be taken. This is the real fixture with three files put in by hand.

    const BASE: &str = include_str!("../tests/fixtures/status.json");

    fn altered(change: impl FnOnce(&mut serde_json::Value)) -> WorkspaceStatus {
        let mut json: serde_json::Value = serde_json::from_str(BASE).unwrap();
        change(&mut json);
        capture(&json.to_string())
    }

    fn branch<'a>(json: &'a mut serde_json::Value, name: &str) -> &'a mut serde_json::Value {
        json["stacks"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .flat_map(|stack| stack["branches"].as_array_mut().unwrap().iter_mut())
            .find(|b| b["name"] == name)
            .unwrap()
    }

    #[test]
    fn files_assigned_to_a_lanes_stack_are_counted_on_the_lane() {
        let status = altered(|json| {
            json["stacks"][2]["assignedChanges"] = serde_json::json!([
                {"cliId": "a", "filePath": "x.rs", "changeType": "modified"},
                {"cliId": "b", "filePath": "y.rs", "changeType": "added"},
                {"cliId": "c", "filePath": "z.rs", "changeType": "removed"},
            ]);
        });
        assert_eq!(lane_report(&status, "feat-auth").unwrap().uncommitted, 3);
        assert_eq!(lane_report(&status, "feat-ui").unwrap().uncommitted, 0);
        assert_eq!(workspace_report(&status).uncommitted, 2, "unassigned changes are counted on the workspace, not a lane");
    }

    /// Every wire word `but` can send maps to its own word, including one a newer `but` might
    /// add — none of these is reachable from a real capture without inventing a state.
    #[test]
    fn every_upstream_merge_status_and_push_status_has_its_own_word() {
        for (wire, expected) in [("clean", Rebase::Clean), ("integrated", Rebase::Integrated), ("empty", Rebase::Empty)] {
            let status = altered(|json| branch(json, "feat-ui")["mergeStatus"] = wire.into());
            assert_eq!(lane_report(&status, "feat-ui").unwrap().rebase, Some(expected), "{wire}");
        }
        let status = altered(|json| branch(json, "feat-ui")["mergeStatus"] = serde_json::json!({"conflicted": {"rebasable": true}}));
        assert_eq!(lane_report(&status, "feat-ui").unwrap().rebase, Some(Rebase::Conflicts), "rebasable is ignored");
        for (wire, expected) in [
            ("nothingToPush", PushState::Pushed),
            ("unpushedCommits", PushState::Unpushed),
            ("unpushedCommitsRequiringForce", PushState::NeedsForce),
            ("completelyUnpushed", PushState::LocalOnly),
            ("integrated", PushState::Integrated),
            ("somethingNewerThanThisClient", PushState::Unknown),
        ] {
            let status = altered(|json| branch(json, "feat-ui")["branchStatus"] = wire.into());
            assert_eq!(lane_report(&status, "feat-ui").unwrap().push, expected, "{wire}");
        }
    }

    #[test]
    fn a_branch_the_workspace_does_not_have_has_no_lane() {
        assert_eq!(lane_report(&altered(|_| {}), "deleted-long-ago"), None);
    }

    /// The shape an agent parses: the lane object inside its workstream, the workspace beside
    /// the list, kebab-case words, and `null` for what `but` had nothing to say about.
    #[test]
    fn the_json_carries_the_lane_and_the_workspace() {
        let mut registry = Registry::default();
        registry.upsert(workstream("lane-conflict", Some("%3"), Some("claude"), None));
        registry.upsert(workstream("gone", Some("%4"), None, None));
        let status = capture(CONFLICT_EXPECTED);
        let fetched = workspace_report(&status).fetched.unwrap();
        let json = serde_json::to_string(&report(&registry, |_| Some(PaneStatus::Idle), Some(&status))).unwrap();
        assert_eq!(
            json,
            format!(
                concat!(
                    r#"{{"schema":1,"workstreams":["#,
                    r#"{{"branch":"lane-conflict","pane":"%3","agent":"claude","item":null,"status":"idle","lane":"#,
                    r#"{{"commits":1,"conflicted":false,"behind":0,"rebase":"conflicts","landed":false,"push":"local-only","uncommitted":0}}}},"#,
                    r#"{{"branch":"gone","pane":"%4","agent":null,"item":null,"status":"idle","lane":null}}"#,
                    r#"],"workspace":{{"behind":3,"uncommitted":0,"fetched":"{}"}}}}"#
                ),
                fetched
            )
        );
    }

    /// `but` being unreachable costs the lane and workspace objects, nothing else.
    #[test]
    fn without_but_the_lane_and_workspace_are_null_and_the_rest_is_unchanged() {
        let mut registry = Registry::default();
        registry.upsert(workstream("feat-ui", Some("%3"), Some("claude"), None));
        assert!(git_state(&Registry::default(), Path::new("/nonexistent")).is_none(), "no workstreams: no reason to ask but");
        assert!(git_state(&registry, Path::new("/nonexistent/not-a-repo")).is_none());
        let json = serde_json::to_string(&report(&registry, |_| Some(PaneStatus::Busy), None)).unwrap();
        assert_eq!(
            json,
            r#"{"schema":1,"workstreams":[{"branch":"feat-ui","pane":"%3","agent":"claude","item":null,"status":"busy","lane":null}],"workspace":null}"#
        );
    }
}
