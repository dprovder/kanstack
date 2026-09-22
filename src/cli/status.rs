//! `kanstack status` — list every workstream and whether its pane is busy, idle, waiting on
//! you, or dead, plus (under `--json`) each lane's git state.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::but::But;
use crate::model::{BranchStatus, MergeStatus, WorkspaceStatus};
use crate::pane_status::PaneStatus;
use crate::workstream::Registry;

use super::exit::{tag, ErrorCode::*};
use super::seeded_splitter;

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
/// fails — `status --json` reports what's registered either way. Also used by `prune`, whose
/// idempotency depends on the same "no reading is never grounds to treat a pane as dead" rule.
pub(super) fn poll_or_nothing(registry: &Registry) -> HashMap<String, PaneStatus> {
    if registry.workstreams.iter().all(|w| w.pane_id.is_none()) {
        return HashMap::new();
    }
    seeded_splitter(registry).and_then(|s| s.poll_statuses()).unwrap_or_default()
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

pub(super) fn run(json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    if json {
        let registry = Registry::load(cwd)?;
        let statuses = poll_or_nothing(&registry);
        let git = git_state(&registry, cwd);
        let report = report(&registry, |branch| statuses.get(branch).copied(), git.status(), git.blocked_message());
        writeln!(out, "{}", serde_json::to_string(&report)?)?;
        return Ok(());
    }

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
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use crate::cli::test_support::*;
    use crate::cli::Command;
    use crate::report::{Reported, Reports};
    use crate::workstream::Registry;

    use super::*;

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
        let result = crate::cli::run(Command::Status { json: true }, repo, &mut out);
        let human = crate::cli::run(Command::Status { json: false }, repo, &mut Vec::new());

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
            crate::cli::parse("spawn", args(&["fix-parser", "--item", "github:#42"])).unwrap()
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
        let result = crate::cli::run(Command::Status { json: true }, repo, &mut out);

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
                crate::cli::run(Command::Status { json: true }, repo, &mut out).expect("status --json must not fail here");
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

    const PUSHED: &str = include_str!("../../tests/fixtures/status_lane_pushed.json");
    const UNPUSHED: &str = include_str!("../../tests/fixtures/status_lane_unpushed.json");
    const NEEDS_FORCE: &str = include_str!("../../tests/fixtures/status_lane_needs_force.json");
    const BEHIND_REMOTE: &str = include_str!("../../tests/fixtures/status_lane_behind_remote.json");
    const LANDED_NO_FLAG: &str = include_str!("../../tests/fixtures/status_lane_landed_no_upstream_flag.json");
    const LANDED: &str = include_str!("../../tests/fixtures/status_lane_landed_upstream_flag.json");
    const CONFLICT_NO_FLAG: &str = include_str!("../../tests/fixtures/status_lane_conflict_expected_no_upstream_flag.json");
    const CONFLICT_EXPECTED: &str = include_str!("../../tests/fixtures/status_lane_conflict_expected_upstream_flag.json");
    const CONFLICTED: &str = include_str!("../../tests/fixtures/status_lane_conflicted.json");

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
        let before = lane_of(include_str!("../../tests/fixtures/status_lane_needs_force.json"), "lane-pushed").unwrap();
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

    const BASE: &str = include_str!("../../tests/fixtures/status.json");

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

    /// `but status`'s reply for these tests: a real capture with a `feat-ui` branch already in
    /// the workspace, so `spawn feat-ui` takes the "branch already exists" path rather than
    /// also needing a stand-in `branch_new` reply. Shared with `spawn`'s own tests via the
    /// same fixture path, duplicated here since each subcommand's test module is
    /// self-contained.
    fn but_status_cat() -> String {
        format!("cat '{}'", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/status.json"))
    }
}
