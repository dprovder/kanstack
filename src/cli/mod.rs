//! The headless subcommands — `kanstack spawn/send/status/focus/stop`, and `report` for the
//! panes to talk back — for driving harness panes from a script or from an agent in another
//! pane, without the board.
//!
//! Each invocation is its own process, so none of them can see the pane handles another
//! one opened. They share state through [`Registry`] instead: every command loads it, seeds
//! a fresh [`Splitter`] with it, acts, and writes back whatever changed.
//!
//! Each subcommand's argument parsing and dispatch logic lives in its own sibling module
//! (`spawn`, `send`, `status`, `focus`, `stop`, `report_cmd`, `prune`, `claim`), so work on one
//! subcommand touches one file. What's shared across more than one of them — the [`Command`]
//! enum and its [`parse`], the generic `--json` result envelope, and small helpers like
//! [`seeded_splitter`] — stays here.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::report::Reported;
use crate::splitter::Splitter;
use crate::workstream::{PaneId, Registry, Workstream};

mod exit;
pub use exit::{dispatch, error_code, invalid_arguments, report_error, ErrorCode};
use exit::{tag, ErrorCode::*, RESULT_SCHEMA};

mod claim;
mod events;
mod focus;
mod prune;
mod report_cmd;
mod send;
mod spawn;
mod status;
mod stop;

pub use prune::PRUNE_SCHEMA;
pub use status::STATUS_SCHEMA;

pub const SUBCOMMANDS: &[&str] = &["spawn", "send", "status", "focus", "stop", "report", "prune", "events", "claim"];

pub const HELP: &str = "\
kanstack spawn <branch> [--agent <name>] [--prompt \"...\"] [--item <ref>]
    [--above <base>|--below <base>] [--json]
    open a harness pane on <branch>, creating the branch first if it doesn't exist.
    --agent runs that harness (e.g. codex) instead of $KANSTACK_HARNESS; --prompt is
    the harness's first message; --item attaches an opaque work-item reference (e.g.
    github:#42) to the workstream, carried in `status`/`status --json` and never
    interpreted or fetched by kanstack itself. --above/--below stack the new branch on
    <base> instead of giving it its own lane (mutually exclusive; only meaningful when
    <branch> doesn't exist yet — spawning on one that already does is `workstream_exists`
    regardless, and stacking an *existing* branch onto another is `but move`'s job, not
    spawn's)
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
kanstack events [--since <offset>|--new] [--follow] [--json]
    print the append-only events log (schema in the README): a report state change and a
    spawn/stop/prune lifecycle change, one JSON line each, so a caller doesn't have to poll
    `status` in a loop to notice one — a doorbell, not the payload; go read `status --json`
    for what actually happened. --since <offset> starts from that byte offset instead of the
    beginning (print the offset a prior run left off at, e.g. with `wc -c`, to resume where it
    left off); --new skips the entire backlog and starts from the log's current end, for
    `--follow` without replaying history (mutually exclusive with --since). --follow keeps
    printing new lines as they're appended (like `tail -f`) until killed, instead of exiting
    once caught up. Needs no multiplexer and never touches the workstream registry. Always
    prints raw JSON lines on success; --json only changes how a failure is reported, same as
    every other subcommand
kanstack claim [<branch>] [--json]
    read a PreToolUse/BeforeTool hook payload from stdin (Claude Code or Gemini CLI, told apart
    by the payload's own hook_event_name) and decide whether the edit it names may proceed:
    denies it (that harness's own decision JSON, on stdout) only when another branch holds a
    fresh, live claim on the exact same file (see docs/automation.md's \"Concurrency
    guarantees\"), otherwise records this branch's own claim and prints nothing. This is what
    the claim-check hook kanstack gives Claude Code and Gemini CLI runs, not something to type
    by hand; <branch> defaults to $KANSTACK_BRANCH, same as `report`. Exact file path only —
    two lanes editing different parts of the same file are still blocked from each other. Never
    fails outward: a broken claims file or malformed stdin always allows the edit rather than
    risk blocking one by mistake

<session> is a pane id as `kanstack status` prints it. These need to run inside the
multiplexer the panes live in (see README).

Every subcommand's --json prints one JSON document to stdout and nothing else, whether it
succeeds or fails; without --json, a failure is reported on stderr as always. Exit codes and
the `error.code` a failed --json document carries are documented in the README.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Spawn {
        branch: String,
        agent: Option<String>,
        prompt: Option<String>,
        item: Option<String>,
        above: Option<String>,
        below: Option<String>,
        json: bool,
    },
    Send { target: String, text: String, json: bool },
    Status { json: bool },
    Focus { target: String, json: bool },
    Stop { target: String, json: bool },
    Report { state: Reported, branch: Option<String>, json: bool },
    Prune { json: bool },
    Events { since: u64, follow: bool, new: bool, json: bool },
    Claim { branch: Option<String>, json: bool },
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
            Command::Events { .. } => "events",
            Command::Claim { .. } => "claim",
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
            | Command::Prune { json }
            | Command::Events { json, .. }
            | Command::Claim { json, .. } => *json,
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
    let mut since = None;
    let mut follow = false;
    let mut new = false;
    let mut agent = None;
    let mut prompt = None;
    let mut item = None;
    let mut above = None;
    let mut below = None;
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
            "--since" if name == "events" => {
                let raw = flag("--since", inline.as_deref(), &mut args)?;
                since = Some(raw.parse::<u64>().map_err(|_| anyhow::anyhow!("--since must be a byte offset, e.g. 4096\n\n{HELP}"))?);
            }
            "--follow" if name == "events" => {
                if inline.is_some() {
                    bail!("--follow takes no value\n\n{HELP}");
                }
                follow = true;
            }
            "--new" if name == "events" => {
                if inline.is_some() {
                    bail!("--new takes no value\n\n{HELP}");
                }
                new = true;
            }
            "--item" if name == "spawn" => item = Some(flag("--item", inline.as_deref(), &mut args)?),
            "--above" if name == "spawn" => above = Some(flag("--above", inline.as_deref(), &mut args)?),
            "--below" if name == "spawn" => below = Some(flag("--below", inline.as_deref(), &mut args)?),
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
        "spawn" => {
            if above.is_some() && below.is_some() {
                bail!("`kanstack spawn` takes --above or --below, not both\n\n{HELP}");
            }
            Command::Spawn { branch: one("<branch>")?, agent, prompt, item, above, below, json }
        }
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
        "events" => {
            if since.is_some() && new {
                bail!("`kanstack events` takes --since or --new, not both\n\n{HELP}");
            }
            Command::Events { since: since.unwrap_or(0), follow, new, json }
        }
        "claim" => Command::Claim { branch: positional.next(), json },
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

/// The result `send`, `focus` and `stop` share: which pane the command acted on, `null` for
/// `stop` on a workstream that had none to close.
#[derive(Debug, Serialize)]
struct PaneResult {
    pane: Option<String>,
}

pub fn run(command: Command, cwd: &Path, out: &mut impl Write) -> Result<()> {
    match command {
        Command::Spawn { branch, agent, prompt, item, above, below, json } => {
            spawn::run(branch, agent, prompt, item, above, below, json, cwd, out)
        }
        Command::Send { target, text, json } => send::run(target, text, json, cwd, out),
        Command::Status { json } => status::run(json, cwd, out),
        Command::Focus { target, json } => focus::run(target, json, cwd, out),
        Command::Stop { target, json } => stop::run(target, json, cwd, out),
        // Deliberately not routed through the registry-loading machinery the other arms
        // share: this runs from a harness's hooks, on every turn, so a registry problem must
        // not make it noisy or slow, and report_cmd::run never reads the registry at all.
        Command::Report { state, branch, json } => report_cmd::run(state, branch, json, cwd, out),
        Command::Prune { json } => prune::run(json, cwd, out),
        Command::Events { since, follow, new, json: _ } => events::run(since, follow, new, cwd, out),
        // Also not routed through the registry-loading machinery, for the same reason as
        // `Report`: this runs from a harness's `PreToolUse` hook, on every file-editing tool
        // call, so it must stay fast and must never fail outward — see `claim`'s module doc.
        Command::Claim { branch, json: _ } => claim::run(branch, cwd, out),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use crate::workstream::{AgentId, BranchId, PaneId, Registry, WorkItemRef, Workstream};

    pub fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    pub fn registry_of(workstreams: Vec<Workstream>) -> Registry {
        let mut registry = Registry::default();
        registry.workstreams = workstreams;
        registry
    }

    pub fn workstream(branch: &str, pane: Option<&str>, agent: Option<&str>, item: Option<&str>) -> Workstream {
        Workstream {
            branch_id: BranchId(branch.into()),
            pane_id: pane.map(|p| PaneId(p.into())),
            agent: agent.map(|a| AgentId(a.into())),
            item: item.map(|i| WorkItemRef(i.into())),
        }
    }

    pub fn five_workstreams() -> Registry {
        registry_of(vec![
            workstream("fix-login", Some("%3"), Some("claude"), Some("GH-4")),
            workstream("add-search", Some("%4"), Some("codex"), None),
            workstream("old-spike", Some("%5"), None, None),
            workstream("mystery", Some("%6"), Some("claude"), None),
            workstream("planned", None, None, Some("GH-9")),
        ])
    }

    /// Runs `body` against a repo pre-loaded with `workstreams` and a stand-in `tmux`
    /// answering `list-panes` with `list_panes`. Hands the repo path to the caller, for tests
    /// that exercise `send`/`focus`/`stop` (and their error paths) against it.
    pub fn with_tmux_registry(tag: &str, list_panes: &str, workstreams: Vec<Workstream>, body: impl FnOnce(&std::path::Path)) {
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
                let repo = std::path::Path::new("/repo/cli-json-with-tmux");
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
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn spawn_takes_a_branch_and_optional_agent_and_prompt() {
        assert_eq!(
            parse("spawn", args(&["fix-login"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-login".into(),
                agent: None,
                prompt: None,
                item: None,
                above: None,
                below: None,
                json: false,
            })
        );
        assert_eq!(
            parse("spawn", args(&["--agent", "codex", "fix-login", "--prompt", "fix the flaky test"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-login".into(),
                agent: Some("codex".into()),
                prompt: Some("fix the flaky test".into()),
                item: None,
                above: None,
                below: None,
                json: false,
            })
        );
        assert_eq!(
            parse("spawn", args(&["b", "--agent=codex"])).unwrap(),
            Some(Command::Spawn {
                branch: "b".into(),
                agent: Some("codex".into()),
                prompt: None,
                item: None,
                above: None,
                below: None,
                json: false,
            })
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
                above: None,
                below: None,
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
                above: None,
                below: None,
                json: false,
            })
        );
        assert!(parse("spawn", args(&["b", "--item"])).is_err(), "--item requires a value");
        assert!(parse("send", args(&["b", "--item", "GH-1"])).is_err(), "--item is spawn-only");
    }

    #[test]
    fn spawn_takes_above_or_below_but_not_both() {
        assert_eq!(
            parse("spawn", args(&["fix-parser", "--above", "main-feature"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-parser".into(),
                agent: None,
                prompt: None,
                item: None,
                above: Some("main-feature".into()),
                below: None,
                json: false,
            })
        );
        assert_eq!(
            parse("spawn", args(&["fix-parser", "--below=main-feature"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-parser".into(),
                agent: None,
                prompt: None,
                item: None,
                above: None,
                below: Some("main-feature".into()),
                json: false,
            })
        );
        assert!(parse("spawn", args(&["b", "--above"])).is_err(), "--above requires a value");
        assert!(
            parse("spawn", args(&["b", "--above", "x", "--below", "y"])).is_err(),
            "--above and --below are mutually exclusive"
        );
        assert!(parse("send", args(&["b", "--above", "x"])).is_err(), "--above is spawn-only");
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
            Some(Command::Spawn { branch: "b".into(), agent: None, prompt: None, item: None, above: None, below: None, json: true })
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

    #[test]
    fn report_accepts_waiting_and_names_all_three_words_when_given_another() {
        assert_eq!(
            parse("report", args(&["waiting", "fix-login"])).unwrap(),
            Some(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()), json: false })
        );
        let err = parse("report", args(&["asleep"])).unwrap_err().to_string();
        assert!(err.contains("busy, idle or waiting"), "{err}");
    }

    #[test]
    fn claim_takes_an_optional_branch_and_json_flag() {
        assert_eq!(parse("claim", args(&[])).unwrap(), Some(Command::Claim { branch: None, json: false }));
        assert_eq!(
            parse("claim", args(&["fix-login"])).unwrap(),
            Some(Command::Claim { branch: Some("fix-login".into()), json: false })
        );
        assert_eq!(
            parse("claim", args(&["fix-login", "--json"])).unwrap(),
            Some(Command::Claim { branch: Some("fix-login".into()), json: true })
        );
        assert!(parse("claim", args(&["a", "b"])).is_err(), "claim takes at most one positional argument");
    }

    #[test]
    fn parse_accepts_prune_with_an_optional_json_flag() {
        assert_eq!(parse("prune", args(&[])).unwrap(), Some(Command::Prune { json: false }));
        assert_eq!(parse("prune", args(&["--json"])).unwrap(), Some(Command::Prune { json: true }));
        assert!(parse("prune", args(&["extra"])).is_err(), "prune takes no positional arguments");
    }

    #[test]
    fn parse_accepts_events_with_since_follow_and_json() {
        assert_eq!(
            parse("events", args(&[])).unwrap(),
            Some(Command::Events { since: 0, follow: false, new: false, json: false })
        );
        assert_eq!(
            parse("events", args(&["--since", "4096"])).unwrap(),
            Some(Command::Events { since: 4096, follow: false, new: false, json: false })
        );
        assert_eq!(
            parse("events", args(&["--since=4096", "--follow", "--json"])).unwrap(),
            Some(Command::Events { since: 4096, follow: true, new: false, json: true })
        );
        assert!(parse("events", args(&["--since", "not-a-number"])).is_err());
        assert!(parse("events", args(&["extra"])).is_err(), "events takes no positional arguments");
        assert!(parse("status", args(&["--since", "1"])).is_err(), "--since is events-only");
        assert!(parse("status", args(&["--follow"])).is_err(), "--follow is events-only");
        assert!(parse("status", args(&["--new"])).is_err(), "--new is events-only");
    }

    #[test]
    fn parse_accepts_events_new_and_rejects_it_combined_with_since() {
        assert_eq!(
            parse("events", args(&["--new"])).unwrap(),
            Some(Command::Events { since: 0, follow: false, new: true, json: false })
        );
        assert_eq!(
            parse("events", args(&["--new", "--follow"])).unwrap(),
            Some(Command::Events { since: 0, follow: true, new: true, json: false })
        );
        assert!(parse("events", args(&["--new=x"])).is_err(), "--new takes no value");
        assert!(
            parse("events", args(&["--since", "4096", "--new"])).is_err(),
            "--since and --new are mutually exclusive"
        );
    }

    // Cross-cutting behavior of the shared `resolve_branch`/`target_with_pane` helpers above,
    // exercised through `dispatch` across the several subcommands that call them — not tied
    // to any one subcommand's own file.

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

    /// A registered workstream with no pane (e.g. `spawn --item` reserved it, or a previous
    /// `stop` already closed its pane while leaving the branch registered some other way) is a
    /// different condition from an unknown target entirely — `send`/`focus` tell them apart as
    /// `no_pane` vs `unknown_workstream`, both exit `3` but distinct `error.code`s, so a caller
    /// that wants to react differently (e.g. `spawn` a pane for it vs treat the branch as
    /// nonexistent) can. `send`/`focus` are not made idempotent by this task — a retry really
    /// does mean "do it again" — but the signal that decides *how* to react is unambiguous
    /// either way (see the module doc on `send`/`focus` above).
    #[test]
    fn a_workstream_with_no_pane_is_no_pane_not_unknown_workstream() {
        with_tmux_registry("no-pane-json", "printf ''", vec![workstream("planned", None, None, None)], |repo| {
            for command in
                [Command::Send { target: "planned".into(), text: "hi".into(), json: true }, Command::Focus { target: "planned".into(), json: true }]
            {
                let name = command.name();
                let mut out = Vec::new();
                let mut err_out = Vec::new();
                let code = dispatch(command, repo, &mut out, &mut err_out);
                assert_eq!(code, 3, "{name}: no_pane shares unknown_workstream's exit code");
                assert!(err_out.is_empty(), "{name}");
                let printed = String::from_utf8(out).unwrap();
                assert!(printed.contains(r#""code":"no_pane""#), "{name}: {printed}");
            }
        });
    }
}
