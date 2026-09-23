//! Drives a [`Recipe`] to completion by shelling out to `kanstack` through [`KanstackClient`]
//! and polling `status --json` until each step's workstream goes idle (see this crate's README,
//! "Completion model" and "Why polling, not `events`").
//!
//! Failure policy: a step that fails to spawn/send, or whose `verify` commands don't all exit
//! 0, is marked `failed` and none of its dependents ever become runnable (their `needs` can
//! never all be `complete`). Independent sibling steps that are already running are left to
//! finish on their own — see the README for why.

use std::collections::HashMap;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::graph::{resolve_workstream, runnable_steps, StepState};
use crate::kanstack::KanstackClient;
use crate::recipe::Recipe;

pub struct RunOptions {
    pub poll_interval: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            poll_interval: Duration::from_millis(1500),
        }
    }
}

pub struct RunOutcome {
    pub states: HashMap<String, StepState>,
    pub failed: bool,
}

pub fn run_recipe(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions) -> RunOutcome {
    let mut states: HashMap<String, StepState> = recipe
        .steps
        .keys()
        .map(|id| (id.clone(), StepState::Pending))
        .collect();
    // step id -> the workstream branch it's waiting to go idle on.
    let mut waiting_on_branch: HashMap<String, String> = HashMap::new();

    loop {
        let runnable = runnable_steps(recipe, &states);
        let launched_any = !runnable.is_empty();

        for step_id in &runnable {
            let step = &recipe.steps[step_id];
            let branch = resolve_workstream(recipe, step_id);
            let effective_prompt = recipe.effective_prompt(step_id);
            let placed_on_existing = step.on.is_some();

            println!(
                "→ {:<15} running   {} / {} / {}{}",
                step_id,
                step.agent,
                step.model.as_deref().unwrap_or("default"),
                step.effort.map(|e| e.as_str()).unwrap_or("default"),
                if placed_on_existing {
                    format!(" (on {branch})")
                } else {
                    String::new()
                }
            );

            let launch_result = if placed_on_existing {
                client.send(&branch, &effective_prompt).map(|_| ())
            } else {
                client.spawn(&branch, step, &effective_prompt).map(|_| ())
            };

            match launch_result {
                Ok(()) => {
                    states.insert(step_id.clone(), StepState::Running);
                    waiting_on_branch.insert(step_id.clone(), branch);
                }
                Err(e) => {
                    eprintln!("✗ {step_id:<15} failed    {e}");
                    states.insert(step_id.clone(), StepState::Failed);
                }
            }
        }

        let running: Vec<String> = states
            .iter()
            .filter(|(_, s)| **s == StepState::Running)
            .map(|(id, _)| id.clone())
            .collect();

        if running.is_empty() {
            if !launched_any {
                // Nothing running and nothing newly launchable: either everything is done, or
                // every remaining pending step has a failed dependency and can never run.
                break;
            }
            // Every step launched this tick already failed at spawn/send time; loop again so
            // any still-independent pending steps get a chance.
            continue;
        }

        match client.status() {
            Ok(statuses) => {
                let status_by_branch: HashMap<&str, &str> = statuses
                    .iter()
                    .map(|w| (w.branch.as_str(), w.status.as_str()))
                    .collect();

                for step_id in &running {
                    let branch = &waiting_on_branch[step_id];
                    match status_by_branch.get(branch.as_str()).copied() {
                        Some("idle") => finish_step(recipe, step_id, &mut states),
                        Some("dead") => {
                            eprintln!(
                                "✗ {step_id:<15} failed    workstream \"{branch}\" died before finishing"
                            );
                            states.insert(step_id.clone(), StepState::Failed);
                        }
                        _ => {} // busy, waiting, unknown, no-pane, or not yet reported — keep polling
                    }
                }
            }
            Err(e) => {
                eprintln!("✗ could not read kanstack status: {e}");
                for step_id in &running {
                    states.insert(step_id.clone(), StepState::Failed);
                }
            }
        }

        if states.values().any(|s| *s == StepState::Running) {
            thread::sleep(opts.poll_interval);
        }
    }

    let failed = states.values().any(|s| *s == StepState::Failed);
    RunOutcome { states, failed }
}

/// A step's workstream has gone idle: run its `verify` commands (if any) and mark it
/// `complete` or `failed`. No `verify` at all means idle alone is sufficient.
fn finish_step(recipe: &Recipe, step_id: &str, states: &mut HashMap<String, StepState>) {
    let step = &recipe.steps[step_id];
    if step.verify.is_empty() {
        println!("✓ {step_id:<15} complete");
        states.insert(step_id.to_string(), StepState::Complete);
        return;
    }

    for cmd in &step.verify {
        match Command::new("sh").arg("-c").arg(cmd).status() {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!(
                    "✗ {step_id:<15} verify failed: `{cmd}` exited {}",
                    status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "via signal".to_string())
                );
                states.insert(step_id.to_string(), StepState::Failed);
                return;
            }
            Err(e) => {
                eprintln!("✗ {step_id:<15} verify failed to run `{cmd}`: {e}");
                states.insert(step_id.to_string(), StepState::Failed);
                return;
            }
        }
    }

    println!("✓ {step_id:<15} verified  {}", step.verify.join(" && "));
    states.insert(step_id.to_string(), StepState::Complete);
}
