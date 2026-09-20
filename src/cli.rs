//! The headless subcommands — `kanstack spawn/send/status/focus/stop` — for driving
//! harness panes from a script or from an agent in another pane, without the board.
//!
//! Each invocation is its own process, so none of them can see the pane handles another
//! one opened. They share state through [`Registry`] instead: every command loads it, seeds
//! a fresh [`Splitter`] with it, acts, and writes back whatever changed.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{bail, Result};
use serde::Serialize;

use crate::but::But;
use crate::pane_status::PaneStatus;
use crate::splitter::Splitter;
use crate::workstream::{AgentId, BranchId, PaneId, Registry, Workstream};

pub const SUBCOMMANDS: &[&str] = &["spawn", "send", "status", "focus", "stop"];

pub const HELP: &str = "\
kanstack spawn <branch> [--agent <name>] [--prompt \"...\"]
    open a harness pane on <branch>, creating the branch first if it doesn't exist.
    --agent runs that harness (e.g. codex) instead of $KANSTACK_HARNESS; --prompt is
    the harness's first message
kanstack send <branch|session> \"...\"
    type a message into a pane and submit it
kanstack status [--json]
    list every workstream and whether its pane is busy, idle, or dead. --json prints one
    JSON document instead (schema in the README) and, unlike the table, still lists every
    workstream when no multiplexer is reachable, with their panes' status \"unknown\"
kanstack focus <branch|session>
    bring a pane to the front
kanstack stop <branch|session>
    close a pane, ending its harness, and forget the workstream

<session> is a pane id as `kanstack status` prints it. These need to run inside the cmux,
tmux or Orca the panes live in.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Spawn { branch: String, agent: Option<String>, prompt: Option<String> },
    Send { target: String, text: String },
    Status { json: bool },
    Focus { target: String },
    Stop { target: String },
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
        "spawn" => Command::Spawn { branch: one("<branch>")?, agent, prompt },
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
            "no harness-split backend found — run this from inside a cmux or tmux pane \
             (see KANSTACK_SPLIT_BACKEND)"
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
}

#[derive(Debug, Serialize)]
struct WorkstreamReport {
    branch: String,
    pane: Option<String>,
    agent: Option<String>,
    item: Option<String>,
    status: ReportStatus,
}

/// [`PaneStatus`] plus `NoPane`, which is a fact about the registry rather than about a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ReportStatus {
    Busy,
    Idle,
    Dead,
    Unknown,
    NoPane,
}

impl From<Option<PaneStatus>> for ReportStatus {
    fn from(status: Option<PaneStatus>) -> Self {
        match status {
            Some(PaneStatus::Busy) => ReportStatus::Busy,
            Some(PaneStatus::Idle) => ReportStatus::Idle,
            Some(PaneStatus::Dead) => ReportStatus::Dead,
            Some(PaneStatus::Unknown) | None => ReportStatus::Unknown,
        }
    }
}

/// Every workstream in `registry`, with `status_of` answering for the ones that have a pane
/// (by branch, as `Splitter::poll_statuses` keys them). Taking a lookup rather than a
/// splitter keeps this free of a real multiplexer; a lookup that knows nothing yields
/// `unknown` for every pane.
fn report(registry: &Registry, status_of: impl Fn(&str) -> Option<PaneStatus>) -> StatusReport {
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
        })
        .collect();
    StatusReport { schema: STATUS_SCHEMA, workstreams }
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
    let mut registry = Registry::load(cwd)?;
    match command {
        Command::Spawn { branch, agent, prompt } => {
            let but = But::discover(cwd)?;
            let mut splitter = seeded_splitter(&registry)?;
            // Its own setting, not the board's `KANSTACK_*_DIRECTION`: the board's `above`
            // assumes kanstack is the pane at the bottom, but here the caller is usually an
            // agent's pane, and a new lane belongs beside it.
            splitter.set_first_direction(&spawn_direction(std::env::var("KANSTACK_SPAWN_DIRECTION").ok().as_deref())?);

            // One poll answers both questions below: is the branch's old pane still alive,
            // and which live pane should the new one split off. A failed poll leaves every
            // pane "unknown", which counts as alive — refusing a spawn is the safer error.
            let statuses = splitter.poll_statuses().unwrap_or_default();
            let dead = |b: &BranchId| statuses.get(&b.0) == Some(&PaneStatus::Dead);

            if let Some(w) = registry.get(&branch) {
                if w.pane_id.is_some() && !dead(&w.branch_id) {
                    bail!("{branch} already has a pane open — use `kanstack send`, `focus` or `stop`");
                }
            }
            if let Some(last_live) = registry
                .workstreams
                .iter()
                .rev()
                .find(|w| w.branch_id.0 != branch && w.pane_id.is_some() && !dead(&w.branch_id))
            {
                splitter.set_anchor(&last_live.pane_id.as_ref().unwrap().0);
            }

            let status = but.status()?;
            let exists = status.stacks.iter().flat_map(|s| &s.branches).any(|b| b.name == branch);
            if !exists {
                but.branch_new(&branch, None)?;
                writeln!(out, "created {branch}")?;
            }

            let pane = splitter.spawn_harness_with(but.cwd(), &branch, prompt.as_deref(), agent.as_deref())?;
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
            let report = report(&registry, |branch| statuses.get(branch).copied());
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
            Some(Command::Spawn { branch: "fix-login".into(), agent: None, prompt: None })
        );
        assert_eq!(
            parse("spawn", args(&["--agent", "codex", "fix-login", "--prompt", "fix the flaky test"])).unwrap(),
            Some(Command::Spawn {
                branch: "fix-login".into(),
                agent: Some("codex".into()),
                prompt: Some("fix the flaky test".into()),
            })
        );
        assert_eq!(
            parse("spawn", args(&["b", "--agent=codex"])).unwrap(),
            Some(Command::Spawn { branch: "b".into(), agent: Some("codex".into()), prompt: None })
        );
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
        let json = serde_json::to_string(&report(&five_workstreams(), |b| statuses.get(b).copied())).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"schema":1,"workstreams":["#,
                r#"{"branch":"fix-login","pane":"%3","agent":"claude","item":"GH-4","status":"busy"},"#,
                r#"{"branch":"add-search","pane":"%4","agent":"codex","item":null,"status":"idle"},"#,
                r#"{"branch":"old-spike","pane":"%5","agent":null,"item":null,"status":"dead"},"#,
                r#"{"branch":"mystery","pane":"%6","agent":"claude","item":null,"status":"unknown"},"#,
                r#"{"branch":"planned","pane":null,"agent":null,"item":"GH-9","status":"no-pane"}"#,
                r#"]}"#
            )
        );
    }

    #[test]
    fn a_pane_the_poll_did_not_mention_is_unknown_and_a_paneless_workstream_stays_no_pane() {
        let nothing = report(&five_workstreams(), |_| None);
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
        let json = serde_json::to_string(&report(&Registry::default(), |_| None)).unwrap();
        assert_eq!(json, r#"{"schema":1,"workstreams":[]}"#);
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
                r#"{"branch":"fix-login","pane":"%3","agent":"claude","item":null,"status":"unknown"},"#,
                r#"{"branch":"planned","pane":null,"agent":null,"item":null,"status":"no-pane"}"#,
                "]}\n"
            )
        );
        assert!(human.is_err(), "the table still needs a backend");
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
}
