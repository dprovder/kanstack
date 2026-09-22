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

mod exit;
pub use exit::{dispatch, error_code, invalid_arguments, report_error, ErrorCode};
use exit::{tag, ErrorCode::*, RESULT_SCHEMA};

pub const SUBCOMMANDS: &[&str] = &["spawn", "send", "status", "focus", "stop", "report", "prune"];

pub const HELP: &str = "\
kanstack spawn <branch> [--agent <name>] [--prompt \"...\"] [--item <ref>] [--json]
    open a harness pane on <branch>, creating the branch first if it doesn't exist.
    --agent runs that harness (e.g. codex) instead of $KANSTACK_HARNESS; --prompt is
    the harness's first message; --item attaches an opaque work-item reference (e.g.
    github:#42) to the workstream, carried in `status`/`status --json` and never
    interpreted or fetched by kanstack itself
kanstack send <branch|session> \"...\" [--json]
    type a message into a pane and submit it
kanstack status [--json]
    list every workstream and whether its pane is busy, idle, waiting on you, or dead. --json prints one
    JSON document instead (schema in the README) and, unlike the table, still lists every
    workstream when no multiplexer is reachable, with their panes' status \"unknown\"
kanstack focus <branch|session> [--json]
    bring a pane to the front
kanstack stop <branch|session> [--json]
    close a pane, ending its harness, and forget the workstream
kanstack report <busy|idle|waiting> [<branch>] [--json]
    say what the agent on <branch> is doing, for `status` and the board to show; waiting
    means stopped on a permission prompt. This is
    what the hooks kanstack gives a harness run (claude's, today); anything else can call it
    too. <branch> defaults to $KANSTACK_BRANCH, which kanstack sets in a pane it launches.
    Prints nothing unless --json is given (never anything else — see README), and needs no
    multiplexer. KANSTACK_STATUS_HOOKS=off stops kanstack handing harnesses those hooks
kanstack prune [--json]
    forget workstreams whose pane is confirmed gone (closed outside kanstack, the process
    died). Only removes ones the poll came back and said were dead — never ones it
    couldn't ask, which stay registered. --json prints one JSON document instead of lines

<session> is a pane id as `kanstack status` prints it. These need to run inside the
multiplexer the panes live in (see README).

Every subcommand's --json prints one JSON document to stdout and nothing else, whether it
succeeds or fails; without --json, a failure is reported on stderr as always. Exit codes and
the `error.code` a failed --json document carries are documented in the README.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Spawn { branch: String, agent: Option<String>, prompt: Option<String>, item: Option<String>, json: bool },
    Send { target: String, text: String, json: bool },
    Status { json: bool },
    Focus { target: String, json: bool },
    Stop { target: String, json: bool },
    Report { state: Reported, branch: Option<String>, json: bool },
    Prune { json: bool },
}

impl Command {
    /// The subcommand name, for a `--json` envelope's `"command"` field.
    fn name(&self) -> &'static str {
        match self {
            Command::Spawn { .. } => "spawn",
            Command::Send { .. } => "send",
            Command::Status { .. } => "status",
            Command::Focus { .. } => "focus",
            Command::Stop { .. } => "stop",
            Command::Report { .. } => "report",
            Command::Prune { .. } => "prune",
        }
    }

    fn wants_json(&self) -> bool {
        match self {
            Command::Spawn { json, .. }
            | Command::Send { json, .. }
            | Command::Status { json }
            | Command::Focus { json, .. }
            | Command::Stop { json, .. }
            | Command::Report { json, .. }
            | Command::Prune { json } => *json,
        }
    }
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
            "--json" => {
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
        "spawn" => Command::Spawn { branch: one("<branch>")?, agent, prompt, item, json },
        "send" => {
            let target = one("<branch|session>")?;
            let text = positional.collect::<Vec<_>>().join(" ");
            if text.trim().is_empty() {
                bail!("`kanstack send` needs a message\n\n{HELP}");
            }
            return Ok(Some(Command::Send { target, text, json }));
        }
        "status" => Command::Status { json },
        "focus" => Command::Focus { target: one("<branch|session>")?, json },
        "stop" => Command::Stop { target: one("<branch|session>")?, json },
        "report" => {
            let word = one("<busy|idle|waiting>")?;
            let state = Reported::parse(&word).ok_or_else(|| {
                anyhow::anyhow!("`kanstack report` takes busy, idle or waiting, not {word:?}\n\n{HELP}")
            })?;
            Command::Report { state, branch: positional.next(), json }
        }
        "prune" => Command::Prune { json },
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
        tag(
            MultiplexerUnavailable,
            anyhow::anyhow!(
                "no harness-split backend found — run this from inside a {} pane \
                 (see KANSTACK_SPLIT_BACKEND)",
                crate::splitter::describe_backends()
            ),
        )
    })?;
    registry.adopt_into(&mut splitter);
    // Every pane goes in the workspace the others did, not wherever this shell's
    // environment happens to say.
    splitter.set_workspace(registry.workspace.as_deref());
    Ok(splitter)
}

/// The branch `target` (a branch name or a pane id) names in `registry`.
fn resolve_branch(registry: &Registry, target: &str) -> Result<String> {
    registry
        .resolve(target)
        .map(|w| w.branch_id.0.clone())
        .ok_or_else(|| tag(UnknownWorkstream, anyhow::anyhow!("no workstream for {target:?} — `kanstack status` lists them")))
}

/// The workstream `target` names, that has a pane to act on.
fn target_with_pane<'a>(registry: &'a Registry, target: &str) -> Result<(&'a Workstream, &'a PaneId)> {
    let w = registry
        .resolve(target)
        .ok_or_else(|| tag(UnknownWorkstream, anyhow::anyhow!("no workstream for {target:?} — `kanstack status` lists them")))?;
    let pane = w
        .pane_id
        .as_ref()
        .ok_or_else(|| tag(NoPane, anyhow::anyhow!("{} has no pane open", w.branch_id)))?;
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
    /// The workspace as a whole. `null` when `but` could not be reached, including while
    /// `workspace_blocked` is set — a blocked workspace refuses `but status` too.
    workspace: Option<WorkspaceReport>,
    /// Set, with `but`'s own explanation, when the workspace is locked by a stray commit on
    /// `gitbutler/workspace` (see `docs/ARCHITECTURE.md`, "A commit on the workspace head
    /// locks everything") — `but` refuses every subcommand until it's fixed, which is worth
    /// telling apart from ordinary unreachability (`but` not installed, not a repo, a
    /// transient failure): it names one specific, fixable cause instead of "try again",
    /// `null` otherwise.
    workspace_blocked: Option<String>,
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
    workspace_blocked: Option<&str>,
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
    StatusReport {
        schema: STATUS_SCHEMA,
        workstreams,
        workspace: git.map(workspace_report),
        workspace_blocked: workspace_blocked.map(str::to_string),
    }
}

/// The workspace's git state: `but` reachable, refusing because of a workspace-commit lock
/// (see `StatusReport::workspace_blocked`), or neither — `but` not installed, not a repo, or
/// some other, transient failure, all of which collapse into the same "unreachable" `status
/// --json` reported before this distinction existed. One `but status` call, so it is only
/// made when there is a workstream to report on.
enum GitState {
    // Boxed: `WorkspaceStatus` is the biggest thing in this enum by far, and this is
    // constructed once per `status --json` call, nowhere near a hot path.
    Ok(Box<WorkspaceStatus>),
    Blocked(String),
    Unreachable,
}

impl GitState {
    fn status(&self) -> Option<&WorkspaceStatus> {
        match self {
            GitState::Ok(status) => Some(status),
            GitState::Blocked(_) | GitState::Unreachable => None,
        }
    }

    fn blocked_message(&self) -> Option<&str> {
        match self {
            GitState::Blocked(message) => Some(message),
            GitState::Ok(_) | GitState::Unreachable => None,
        }
    }
}

fn git_state(registry: &Registry, cwd: &Path) -> GitState {
    if registry.workstreams.is_empty() {
        return GitState::Unreachable;
    }
    let Ok(but) = But::discover(cwd) else {
        return GitState::Unreachable;
    };
    match but.status_with_upstream() {
        Ok(status) => GitState::Ok(Box::new(status)),
        Err(e) if crate::but::is_workspace_block(&e.to_string()) => GitState::Blocked(e.to_string()),
        Err(_) => GitState::Unreachable,
    }
}

/// Pane statuses by branch, or nothing at all if there's no multiplexer to ask or the poll
/// fails — `status --json` reports what's registered either way.
fn poll_or_nothing(registry: &Registry) -> HashMap<String, PaneStatus> {
    if registry.workstreams.iter().all(|w| w.pane_id.is_none()) {
        return HashMap::new();
    }
    seeded_splitter(registry).and_then(|s| s.poll_statuses()).unwrap_or_default()
}

/// The `"schema"` of `kanstack prune --json`, versioned independently of [`STATUS_SCHEMA`].
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

/// The generic `--json` success envelope for `spawn`/`send`/`focus`/`stop`/`report` — see
/// [`RESULT_SCHEMA`]. `status` and `prune` print their own, independently versioned shapes
/// instead of this, unchanged by this addition.
#[derive(Debug, Serialize)]
struct ResultEnvelope<T: Serialize> {
    schema: u32,
    ok: bool,
    command: &'static str,
    workstream: String,
    result: T,
}

fn json_result<T: Serialize>(command: &'static str, workstream: String, result: T) -> Result<String> {
    Ok(serde_json::to_string(&ResultEnvelope { schema: RESULT_SCHEMA, ok: true, command, workstream, result })?)
}

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

/// The result `send`, `focus` and `stop` share: which pane the command acted on, `null` for
/// `stop` on a workstream that had none to close.
#[derive(Debug, Serialize)]
struct PaneResult {
    pane: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReportResult {
    state: Reported,
}

pub fn run(command: Command, cwd: &Path, out: &mut impl Write) -> Result<()> {
    // Before the registry is even read: this runs from a harness's hooks, on every turn, and
    // a registry problem must not make it noisy or slow. And without --json it writes nothing
    // to `out` — Claude adds a `UserPromptSubmit` hook's stdout to what the model sees.
    if let Command::Report { state, branch, json } = &command {
        let branch = branch
            .clone()
            .or_else(|| std::env::var("KANSTACK_BRANCH").ok())
            .filter(|b| !b.is_empty())
            .ok_or_else(|| {
                tag(
                    InvalidArguments,
                    anyhow::anyhow!(
                        "`kanstack report` needs a branch: name one, or run it where $KANSTACK_BRANCH is set \
                         (in a pane kanstack launched)"
                    ),
                )
            })?;
        Reports::for_repo(cwd).write(&branch, *state, SystemTime::now())?;
        if *json {
            writeln!(out, "{}", json_result("report", branch, ReportResult { state: *state })?)?;
        }
        return Ok(());
    }

    match command {
        Command::Spawn { branch, agent, prompt, item, json } => {
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
                splitter
                    .set_first_direction(&spawn_direction(std::env::var("KANSTACK_SPAWN_DIRECTION").ok().as_deref())?);

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
                writeln!(out, "{}", json_result("spawn", branch, spawn_result)?)?;
            } else {
                for line in lines {
                    writeln!(out, "{line}")?;
                }
            }
        }
        Command::Send { target, text, json } => {
            let registry = Registry::load(cwd)?;
            let splitter = seeded_splitter(&registry)?;
            let (w, pane) = target_with_pane(&registry, &target)?;
            let branch = w.branch_id.0.clone();
            let pane = pane.0.clone();
            splitter.send_task(&branch, &text).map_err(|e| tag(DeliveryFailed, e))?;
            if json {
                writeln!(out, "{}", json_result("send", branch, PaneResult { pane: Some(pane) })?)?;
            } else {
                writeln!(out, "sent to {branch}")?;
            }
        }
        Command::Status { json: true } => {
            let registry = Registry::load(cwd)?;
            let statuses = poll_or_nothing(&registry);
            let git = git_state(&registry, cwd);
            let report = report(&registry, |branch| statuses.get(branch).copied(), git.status(), git.blocked_message());
            writeln!(out, "{}", serde_json::to_string(&report)?)?;
        }
        Command::Status { json: false } => {
            let registry = Registry::load(cwd)?;
            if registry.workstreams.is_empty() {
                writeln!(out, "no workstreams — `kanstack spawn <branch>` starts one")?;
                return Ok(());
            }
            let mut splitter = seeded_splitter(&registry)?;
            let statuses = splitter.poll_statuses().map_err(|e| tag(MultiplexerUnavailable, e))?;
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
        Command::Focus { target, json } => {
            let registry = Registry::load(cwd)?;
            let splitter = seeded_splitter(&registry)?;
            let (w, pane) = target_with_pane(&registry, &target)?;
            let branch = w.branch_id.0.clone();
            let pane = pane.0.clone();
            splitter.focus(&branch).map_err(|e| tag(MultiplexerUnavailable, e))?;
            if json {
                writeln!(out, "{}", json_result("focus", branch, PaneResult { pane: Some(pane) })?)?;
            } else {
                writeln!(out, "focused {branch}")?;
            }
        }
        Command::Report { .. } => unreachable!("handled before the registry is loaded"),
        Command::Stop { target, json } => {
            // Locked for the same reason as `Spawn`: resolving the target, stopping the pane
            // and forgetting the workstream must land as one save, not two processes' saves
            // interleaved.
            let (branch, pane) = Registry::with_lock(cwd, |registry| {
                let branch = resolve_branch(registry, &target)?;
                let pane = registry.get(&branch).and_then(|w| w.pane_id.clone());
                // A workstream with no pane has nothing to close; forgetting it is the stop.
                if pane.is_some() {
                    let mut splitter = seeded_splitter(registry)?;
                    splitter.stop(&branch).map_err(|e| tag(MultiplexerUnavailable, e))?;
                }
                registry.remove(&branch);
                Ok((branch, pane))
            })?;
            if json {
                writeln!(out, "{}", json_result("stop", branch, PaneResult { pane: pane.map(|p| p.0) })?)?;
            } else {
                writeln!(out, "stopped {branch}")?;
            }
        }
        Command::Prune { json } => {
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
            Some(Command::Spawn { branch: "fix-login".into(), agent: None, prompt: None, item: None, json: false })
        );
        assert_eq!(
            parse("spawn", args(&["--agent", "codex", "fix-login", "--prompt", "fix the flaky test"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-login".into(),
                agent: Some("codex".into()),
                prompt: Some("fix the flaky test".into()),
                item: None,
                json: false,
            })
        );
        assert_eq!(
            parse("spawn", args(&["b", "--agent=codex"])).unwrap(),
            Some(Command::Spawn { branch: "b".into(), agent: Some("codex".into()), prompt: None, item: None, json: false })
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
                json: false,
            })
        );
        assert_eq!(
            parse("spawn", args(&["fix-parser", "--item=linear:ENG-7"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-parser".into(),
                agent: None,
                prompt: None,
                item: Some("linear:ENG-7".into()),
                json: false,
            })
        );
        assert!(parse("spawn", args(&["b", "--item"])).is_err(), "--item requires a value");
        assert!(parse("send", args(&["b", "--item", "GH-1"])).is_err(), "--item is spawn-only");
    }

    #[test]
    fn send_joins_the_rest_into_one_message() {
        assert_eq!(
            parse("send", args(&["fix-login", "run", "the", "tests"])).unwrap(),
            Some(Command::Send { target: "fix-login".into(), text: "run the tests".into(), json: false })
        );
        assert!(parse("send", args(&["fix-login"])).is_err());
        assert!(parse("send", args(&[])).is_err());
    }

    #[test]
    fn the_single_target_commands_take_exactly_one_argument() {
        assert_eq!(
            parse("focus", args(&["%3"])).unwrap(),
            Some(Command::Focus { target: "%3".into(), json: false })
        );
        assert_eq!(parse("stop", args(&["a"])).unwrap(), Some(Command::Stop { target: "a".into(), json: false }));
        assert!(parse("stop", args(&[])).is_err());
        assert!(parse("stop", args(&["a", "b"])).is_err());
        assert_eq!(parse("status", args(&[])).unwrap(), Some(Command::Status { json: false }));
        assert!(parse("status", args(&["extra"])).is_err());
    }

    #[test]
    fn every_subcommand_takes_a_json_flag() {
        assert_eq!(parse("status", args(&["--json"])).unwrap(), Some(Command::Status { json: true }));
        assert!(parse("status", args(&["--json=true"])).is_err(), "it is a switch, not an option with a value");
        assert!(parse("status", args(&["--json", "extra"])).is_err());
        assert_eq!(
            parse("spawn", args(&["b", "--json"])).unwrap(),
            Some(Command::Spawn { branch: "b".into(), agent: None, prompt: None, item: None, json: true })
        );
        // `--json` is recognized as a flag wherever it falls among the arguments (flags are
        // split out before the remaining words are joined into the message), same as before
        // this test for any other unrecognized `--word` — which used to make it an error for
        // every command but `status`/`prune` (see `options_are_rejected_where_they_do_not_apply`).
        assert_eq!(
            parse("send", args(&["b", "hi", "--json"])).unwrap(),
            Some(Command::Send { target: "b".into(), text: "hi".into(), json: true })
        );
        assert_eq!(
            parse("focus", args(&["b", "--json"])).unwrap(),
            Some(Command::Focus { target: "b".into(), json: true })
        );
        assert_eq!(
            parse("stop", args(&["b", "--json"])).unwrap(),
            Some(Command::Stop { target: "b".into(), json: true })
        );
        assert_eq!(
            parse("report", args(&["busy", "--json"])).unwrap(),
            Some(Command::Report { state: Reported::Busy, branch: None, json: true })
        );
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
        let json = serde_json::to_string(&report(&five_workstreams(), |b| statuses.get(b).copied(), None, None)).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"schema":1,"workstreams":["#,
                r#"{"branch":"fix-login","pane":"%3","agent":"claude","item":"GH-4","status":"busy","lane":null},"#,
                r#"{"branch":"add-search","pane":"%4","agent":"codex","item":null,"status":"idle","lane":null},"#,
                r#"{"branch":"old-spike","pane":"%5","agent":null,"item":null,"status":"dead","lane":null},"#,
                r#"{"branch":"mystery","pane":"%6","agent":"claude","item":null,"status":"unknown","lane":null},"#,
                r#"{"branch":"planned","pane":null,"agent":null,"item":"GH-9","status":"no-pane","lane":null}"#,
                r#"],"workspace":null,"workspace_blocked":null}"#
            )
        );
    }

    #[test]
    fn a_pane_the_poll_did_not_mention_is_unknown_and_a_paneless_workstream_stays_no_pane() {
        let nothing = report(&five_workstreams(), |_| None, None, None);
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
        let json = serde_json::to_string(&report(&Registry::default(), |_| None, None, None)).unwrap();
        assert_eq!(json, r#"{"schema":1,"workstreams":[],"workspace":null,"workspace_blocked":null}"#);
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
                "],\"workspace\":null,\"workspace_blocked\":null}\n"
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
            Some(Command::Report { state: Reported::Busy, branch: None, json: false })
        );
        assert_eq!(
            parse("report", args(&["idle", "fix-login"])).unwrap(),
            Some(Command::Report { state: Reported::Idle, branch: Some("fix-login".into()), json: false })
        );
        assert!(parse("report", args(&[])).is_err());
        assert!(parse("report", args(&["dead"])).is_err(), "only busy and idle can be reported");
        assert!(parse("report", args(&["busy", "a", "b"])).is_err());
    }

    /// Claude adds a `UserPromptSubmit` hook's stdout to what the model sees, so a
    /// confirmation line here would end up in every prompt.
    #[test]
    fn report_records_what_it_was_told_and_prints_nothing() {
        with_state("report", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report");
            let mut out = Vec::new();
            run(Command::Report { state: Reported::Busy, branch: Some("fix-login".into()), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "stdout must stay empty: {:?}", String::from_utf8_lossy(&out));
            assert_eq!(Reports::for_repo(repo).status("fix-login", SystemTime::now()), Some(PaneStatus::Busy));

            run(Command::Report { state: Reported::Idle, branch: Some("fix-login".into()), json: false }, repo, &mut out).unwrap();
            assert_eq!(Reports::for_repo(repo).status("fix-login", SystemTime::now()), Some(PaneStatus::Idle));
        });
    }

    /// The hooks kanstack hands a harness are the same for every lane; which lane is speaking
    /// comes from the environment it was launched with.
    #[test]
    fn report_names_its_lane_from_the_environment_unless_told_otherwise() {
        with_state("report-env", &[("KANSTACK_BRANCH", Some("from-env"))], || {
            let repo = Path::new("/repo/report-env");
            run(Command::Report { state: Reported::Busy, branch: None, json: false }, repo, &mut Vec::new()).unwrap();
            run(Command::Report { state: Reported::Idle, branch: Some("named".into()), json: false }, repo, &mut Vec::new()).unwrap();
            let now = SystemTime::now();
            assert_eq!(Reports::for_repo(repo).status("from-env", now), Some(PaneStatus::Busy));
            assert_eq!(Reports::for_repo(repo).status("named", now), Some(PaneStatus::Idle));
        });
    }

    #[test]
    fn report_without_a_branch_anywhere_says_what_it_needs() {
        for unset in [None, Some("")] {
            with_state("report-nobranch", &[("KANSTACK_BRANCH", unset)], || {
                let err = run(Command::Report { state: Reported::Busy, branch: None, json: false }, Path::new("/repo/x"), &mut Vec::new())
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
            run(Command::Report { state: Reported::Busy, branch: Some("a".into()), json: false }, repo, &mut Vec::new()).unwrap();
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
        format!(r#"{{"schema":1,"workstreams":[{{"branch":"fix-login","pane":"%3","agent":"claude","item":null,"status":"{status}","lane":null}}],"workspace":null,"workspace_blocked":null}}{}"#, "\n")
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

    // `--json` on `send`/`focus`/`stop`/`report`, and the generic error envelope/exit codes
    // `dispatch` gives every subcommand — see `cli::exit`.

    /// Runs `body` against a repo pre-loaded with `workstreams` and a stand-in `tmux`
    /// answering `list-panes` with `list_panes`. Mirrors `status_json_with_tmux`, but hands
    /// the repo path to the caller instead of running one fixed command, for tests that
    /// exercise `send`/`focus`/`stop` (and their error paths) against it.
    fn with_tmux_registry(tag: &str, list_panes: &str, workstreams: Vec<Workstream>, body: impl FnOnce(&Path)) {
        use crate::mux::stand_in;
        let (bin, _log) = stand_in::install(tag, "tmux", &format!(r#"case "$1" in list-panes) {list_panes} ;; esac"#));
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
                let repo = Path::new("/repo/cli-json-with-tmux");
                let mut registry = Registry::load(repo).unwrap();
                for w in workstreams {
                    registry.upsert(w);
                }
                registry.save().unwrap();
                body(repo);
            },
        );
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn send_json_reports_the_pane_it_typed_into() {
        with_tmux_registry(
            "send-json",
            r#"printf '%%3 2000000000\n'"#,
            vec![workstream("fix-login", Some("%3"), Some("claude"), None)],
            |repo| {
                let mut out = Vec::new();
                run(Command::Send { target: "fix-login".into(), text: "run tests".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"send\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\"}}\n"
                );
            },
        );
    }

    #[test]
    fn focus_json_reports_the_pane_it_focused() {
        with_tmux_registry(
            "focus-json",
            r#"printf '%%3 2000000000\n'"#,
            vec![workstream("fix-login", Some("%3"), Some("claude"), None)],
            |repo| {
                let mut out = Vec::new();
                run(Command::Focus { target: "fix-login".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"focus\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\"}}\n"
                );
            },
        );
    }

    #[test]
    fn stop_json_reports_the_pane_it_closed_and_null_when_there_was_none() {
        with_tmux_registry(
            "stop-json",
            r#"printf '%%3 2000000000\n'"#,
            vec![workstream("fix-login", Some("%3"), Some("claude"), None), workstream("planned", None, None, None)],
            |repo| {
                let mut out = Vec::new();
                run(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"stop\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\"}}\n"
                );

                let mut out = Vec::new();
                run(Command::Stop { target: "planned".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"stop\",\"workstream\":\"planned\",\"result\":{\"pane\":null}}\n"
                );
            },
        );
    }

    // Idempotency: what a retried `stop`/`spawn`/`prune` actually does. `send`/`focus` are
    // deliberately not made idempotent (see the module doc and README) — only their signal on
    // failure is checked, alongside the others', in the tests below.

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
                let repo = Path::new("/repo/stop-already-dead");
                let mut registry = Registry::load(repo).unwrap();
                registry.upsert(workstream("fix-login", Some("%3"), Some("claude"), None));
                registry.save().unwrap();

                let mut out = Vec::new();
                run(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut out)
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
                run(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut Vec::new()).unwrap();
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

    #[test]
    fn report_json_prints_the_envelope_and_plain_report_prints_nothing() {
        with_state("report-json", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report-json");
            let mut out = Vec::new();
            run(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()), json: true }, repo, &mut out).unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":true,\"command\":\"report\",\"workstream\":\"fix-login\",\"result\":{\"state\":\"waiting\"}}\n"
            );

            let mut out = Vec::new();
            run(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "without --json, report still prints nothing");
        });
    }

    /// `dispatch` is what `main` actually calls: it turns a failure into `--json`'s error
    /// envelope (or, without `--json`, `anyhow`'s own text on `err_out`) and a matching exit
    /// code — see `cli::exit`. `unknown_workstream` is shared by every command that resolves
    /// a `<branch|session>` target.
    #[test]
    fn an_unknown_target_is_an_unknown_workstream_error_everywhere_that_resolves_one() {
        with_tmux_registry("unknown-json", "printf ''", vec![], |repo| {
            for command in [
                Command::Send { target: "ghost".into(), text: "hi".into(), json: true },
                Command::Focus { target: "ghost".into(), json: true },
                Command::Stop { target: "ghost".into(), json: true },
            ] {
                let name = command.name();
                let mut out = Vec::new();
                let mut err_out = Vec::new();
                let code = dispatch(command, repo, &mut out, &mut err_out);
                assert_eq!(code, 3, "{name}");
                assert!(err_out.is_empty(), "{name}: json mode writes nothing to the error stream");
                let printed = String::from_utf8(out).unwrap();
                assert_eq!(
                    printed,
                    format!(
                        "{{\"schema\":1,\"ok\":false,\"command\":\"{name}\",\"error\":{{\"code\":\"unknown_workstream\",\
                         \"message\":\"no workstream for \\\"ghost\\\" — `kanstack status` lists them\"}}}}\n"
                    ),
                    "{name}: {printed}"
                );
            }
        });
    }

    /// Without `--json`, `dispatch` reports the same failure the old flat-exit-1 `main` did —
    /// text on `err_out`, nothing on `out` — but with the more specific exit code.
    #[test]
    fn a_human_mode_failure_still_goes_to_err_out_with_the_sharper_exit_code() {
        with_tmux_registry("unknown-human", "printf ''", vec![], |repo| {
            let mut out = Vec::new();
            let mut err_out = Vec::new();
            let code = dispatch(Command::Focus { target: "ghost".into(), json: false }, repo, &mut out, &mut err_out);
            assert_eq!(code, 3);
            assert!(out.is_empty());
            assert_eq!(
                String::from_utf8(err_out).unwrap(),
                "Error: no workstream for \"ghost\" — `kanstack status` lists them\n"
            );
        });
    }

    // `spawn --json` — the one command that also talks to `but`, so its stand-in needs both
    // `but` (for `status`/`branch_new`) and `tmux` (for the pane itself).

    /// Runs `body` against a real, empty temp directory (needed because `But::run` sets
    /// `current_dir` to it, which fails outright against the fake `/repo/...` paths every
    /// other test here uses — nothing under it ever needs to look like a real git repo, since
    /// both `but` and `tmux` are stand-ins) with `KANSTACK_BUT_BIN` answering `--version` and
    /// `status` from `but_status_body`, and `KANSTACK_TMUX_BIN` answering `list-panes` and
    /// `split-window` from `tmux_body`.
    fn with_but_and_tmux(tag: &str, but_status_body: &str, tmux_body: &str, workstreams: Vec<Workstream>, body: impl FnOnce(&Path)) {
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
                run(
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

    // `status --json`'s `workspace_blocked` — see `StatusReport::workspace_blocked` and
    // `GitState`. `docs/ARCHITECTURE.md` ("A commit on the workspace head locks everything")
    // has the background on why this is worth telling apart from ordinary unreachability.

    #[test]
    fn report_json_carries_the_workspace_blocked_reason_when_given_one() {
        let json = serde_json::to_string(&report(&Registry::default(), |_| None, None, Some("run `but teardown`"))).unwrap();
        assert_eq!(
            json,
            r#"{"schema":1,"workstreams":[],"workspace":null,"workspace_blocked":"run `but teardown`"}"#
        );
    }

    /// `git_state` itself, against a stand-in `but` whose `status` fails the way a locked
    /// workspace really does (prose on stderr, matched by `but::is_workspace_block`) —
    /// distinct from a `but` that is merely unreachable, which the next test covers.
    #[test]
    fn git_state_reports_blocked_when_but_refuses_over_a_stray_workspace_commit() {
        use crate::mux::stand_in;
        let (bin, _log) = stand_in::install(
            "git-state-blocked",
            "but",
            "case \"$1\" in --version) echo 'but 0.22.3' ;; \
             status) echo 'Error: GitButler mode exit required: please run but teardown to preserve your work.' >&2; exit 1 ;; esac",
        );
        // A real, existing directory: `But::run` (unlike `But::discover`'s own `--version`
        // check) sets `current_dir` to it, which fails outright against the fake `/repo/...`
        // paths every other test here uses.
        let cwd = std::env::temp_dir().join(format!("kanstack-cli-git-state-blocked-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let mut registry = Registry::default();
        registry.upsert(workstream("feat-ui", None, None, None));
        stand_in::with_env(&[("KANSTACK_BUT_BIN", Some(bin.to_str().unwrap()))], || {
            let state = git_state(&registry, &cwd);
            assert!(state.status().is_none());
            let message = state.blocked_message().expect("a workspace block must be recognized");
            assert!(message.contains("GitButler mode exit required"), "{message}");
        });
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// `but` genuinely reachable and happy: `Ok`, and neither `status` nor `blocked_message`
    /// collapse it into "unreachable".
    #[test]
    fn git_state_reads_a_reachable_but() {
        use crate::mux::stand_in;
        let (bin, _log) =
            stand_in::install("git-state-ok", "but", &format!("case \"$1\" in --version) echo 'but 0.22.3' ;; status) {} ;; esac", but_status_cat()));
        let cwd = std::env::temp_dir().join(format!("kanstack-cli-git-state-ok-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).unwrap();
        let mut registry = Registry::default();
        registry.upsert(workstream("feat-ui", None, None, None));
        stand_in::with_env(&[("KANSTACK_BUT_BIN", Some(bin.to_str().unwrap()))], || {
            let state = git_state(&registry, &cwd);
            assert!(state.status().is_some());
            assert!(state.blocked_message().is_none());
        });
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    /// `but` not installed at all (an empty `KANSTACK_BUT_BIN` pointing nowhere) is
    /// "unreachable", not "blocked" — the two must not be conflated.
    #[test]
    fn git_state_reads_unreachable_when_but_cannot_even_be_run() {
        let mut registry = Registry::default();
        registry.upsert(workstream("feat-ui", None, None, None));
        crate::mux::stand_in::with_env(&[("KANSTACK_BUT_BIN", Some("/nonexistent/but-does-not-exist"))], || {
            let state = git_state(&registry, Path::new("/repo/git-state-unreachable"));
            assert!(state.status().is_none());
            assert!(state.blocked_message().is_none());
        });
    }

    #[test]
    fn report_accepts_waiting_and_names_all_three_words_when_given_another() {
        assert_eq!(
            parse("report", args(&["waiting", "fix-login"])).unwrap(),
            Some(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()), json: false })
        );
        let err = parse("report", args(&["asleep"])).unwrap_err().to_string();
        assert!(err.contains("busy, idle or waiting"), "{err}");
    }

    /// A pane blocked on a permission prompt is its own status, not `idle` — it will not
    /// move until the user does.
    #[test]
    fn a_waiting_pane_is_waiting_in_both_the_table_and_the_json() {
        assert_eq!(label(Some(PaneStatus::Waiting)), "waiting");
        let json = serde_json::to_string(&report(&five_workstreams(), |b| (b == "fix-login").then_some(PaneStatus::Waiting), None, None)).unwrap();
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
        let json = serde_json::to_string(&report(&registry, |_| Some(PaneStatus::Idle), Some(&status), None)).unwrap();
        assert_eq!(
            json,
            format!(
                concat!(
                    r#"{{"schema":1,"workstreams":["#,
                    r#"{{"branch":"lane-conflict","pane":"%3","agent":"claude","item":null,"status":"idle","lane":"#,
                    r#"{{"commits":1,"conflicted":false,"behind":0,"rebase":"conflicts","landed":false,"push":"local-only","uncommitted":0}}}},"#,
                    r#"{{"branch":"gone","pane":"%4","agent":null,"item":null,"status":"idle","lane":null}}"#,
                    r#"],"workspace":{{"behind":3,"uncommitted":0,"fetched":"{}"}},"workspace_blocked":null}}"#
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
        assert!(
            git_state(&Registry::default(), Path::new("/nonexistent")).status().is_none(),
            "no workstreams: no reason to ask but"
        );
        assert!(git_state(&registry, Path::new("/nonexistent/not-a-repo")).status().is_none());
        let json = serde_json::to_string(&report(&registry, |_| Some(PaneStatus::Busy), None, None)).unwrap();
        assert_eq!(
            json,
            r#"{"schema":1,"workstreams":[{"branch":"feat-ui","pane":"%3","agent":"claude","item":null,"status":"busy","lane":null}],"workspace":null,"workspace_blocked":null}"#
        );
    }

    // `kanstack prune` — forgetting workstreams whose pane is confirmed gone.

    #[test]
    fn parse_accepts_prune_with_an_optional_json_flag() {
        assert_eq!(parse("prune", args(&[])).unwrap(), Some(Command::Prune { json: false }));
        assert_eq!(parse("prune", args(&["--json"])).unwrap(), Some(Command::Prune { json: true }));
        assert!(parse("prune", args(&["extra"])).is_err(), "prune takes no positional arguments");
    }

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
                run(Command::Prune { json }, repo, &mut out).expect("prune must not fail here");
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
        let result = run(Command::Prune { json: true }, repo, &mut out);
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
