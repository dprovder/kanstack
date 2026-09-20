//! The headless subcommands — `kanstack spawn/send/status/focus/stop` — for driving
//! harness panes from a script or from an agent in another pane, without the board.
//!
//! Each invocation is its own process, so none of them can see the pane handles another
//! one opened. They share state through [`Registry`] instead: every command loads it, seeds
//! a fresh [`Splitter`] with it, acts, and writes back whatever changed.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Result};

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
kanstack status
    list every workstream and whether its pane is busy, idle, or dead
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
    Status,
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
        "status" => Command::Status,
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
        Command::Status => {
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
        assert_eq!(parse("status", args(&[])).unwrap(), Some(Command::Status));
        assert!(parse("status", args(&["extra"])).is_err());
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
