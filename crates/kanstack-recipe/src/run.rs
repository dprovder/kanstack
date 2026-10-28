//! Drives a [`Recipe`] to completion by shelling out to `kanstack` through [`KanstackClient`]
//! and polling `status --json` until each step's workstream goes idle (see this crate's README,
//! "Completion model" and "Why polling, not `events`").
//!
//! Failure policy: a step that fails to spawn/send, or whose `verify` commands don't all exit
//! 0, is marked `failed` and none of its dependents ever become runnable (their `needs` can
//! never all be `complete`). Independent sibling steps that are already running are left to
//! finish on their own — see the README for why.
//!
//! **At most one in-flight operation per workstream branch.** Two steps that resolve to the
//! same branch (an `on` chain, or two siblings both `on` the same target) are never both
//! `spawn`/`send` in the same tick, and a step whose target branch is already claimed by
//! another currently-`running` step is deferred to a later tick instead. Without this, two
//! sends to one pane in the same tick race: kanstack's `status --json` reports one workstream
//! `idle`/`busy` state, not "did *this* message's turn finish," so a single idle observation
//! could otherwise be read as both steps' completion.
//!
//! **A transient `status --json` failure never fails a running step outright.** Reading status
//! shells out to `kanstack` itself and can hiccup independently of any step's own work; a
//! single failed poll is logged and retried next tick. Only [`MAX_CONSECUTIVE_STATUS_FAILURES`]
//! in a row — a `kanstack` that is actually unreachable, not a blip — fails every step still
//! running.
//!
//! **`idle` on the very first status read after a spawn/send is never trusted.** kanstack's own
//! `PaneStatus::Idle` doc comment (`src/mux/pane_status.rs` in the parent crate) says a single
//! poll can't tell "the harness returned to rest" apart from "the harness never got a chance to
//! start" — the pane can look idle for a moment before the harness has actually begun the turn
//! we just sent it. A step only finishes on its *second* (or later) status read since launch;
//! the first idle reading is discounted and polling continues. This can't hang forever the way
//! waiting for a "busy" sighting first could (a step whose whole turn finishes between two
//! polls would never be seen busy at all) — it costs at most one extra `poll_interval` per step.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::graph::{resolve_workstream, runnable_steps, StepState};
use crate::kanstack::KanstackClient;
use crate::recipe::Recipe;

/// How many consecutive `status --json` failures in a row are tolerated as transient before
/// every currently-running step is failed. See the module doc.
const MAX_CONSECUTIVE_STATUS_FAILURES: u32 = 3;

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
    // step id -> how many status reads have been taken since it started running. A step needs
    // at least 2 before an `idle` reading is trusted — see the module doc.
    let mut status_reads_since_launch: HashMap<String, u32> = HashMap::new();
    let mut consecutive_status_failures: u32 = 0;

    loop {
        let runnable = runnable_steps(recipe, &states);

        // Branches already claimed by a currently-running step — never spawn/send onto one of
        // these again until it frees up. Updated in-line below too, so two runnable steps that
        // resolve to the same branch in this same tick don't both launch onto it.
        let mut busy_branches: HashSet<String> = waiting_on_branch
            .iter()
            .filter(|(id, _)| states.get(id.as_str()) == Some(&StepState::Running))
            .map(|(_, branch)| branch.clone())
            .collect();

        let mut launched_this_tick = false;

        for step_id in &runnable {
            let step = &recipe.steps[step_id];
            let branch = resolve_workstream(recipe, step_id);
            if busy_branches.contains(&branch) {
                // Another step already owns this workstream right now; try again next tick
                // once it frees up, rather than racing a second spawn/send onto it.
                continue;
            }

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

            launched_this_tick = true;
            match launch_result {
                Ok(()) => {
                    states.insert(step_id.clone(), StepState::Running);
                    waiting_on_branch.insert(step_id.clone(), branch.clone());
                    status_reads_since_launch.insert(step_id.clone(), 0);
                    busy_branches.insert(branch);
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
            if !launched_this_tick {
                // Nothing running and nothing launched: either everything is done, or every
                // remaining pending step has a failed dependency and can never run.
                break;
            }
            // Everything launched this tick already failed at spawn/send time; loop again so
            // any still-independent pending steps get a chance.
            continue;
        }

        match client.status() {
            Ok(statuses) => {
                consecutive_status_failures = 0;
                let status_by_branch: HashMap<&str, &str> = statuses
                    .iter()
                    .map(|w| (w.branch.as_str(), w.status.as_str()))
                    .collect();

                for step_id in &running {
                    let branch = &waiting_on_branch[step_id];
                    let reads = status_reads_since_launch.entry(step_id.clone()).or_insert(0);
                    *reads += 1;
                    match status_by_branch.get(branch.as_str()).copied() {
                        Some("idle") if *reads >= 2 => {
                            finish_step(recipe, step_id, &mut states);
                            status_reads_since_launch.remove(step_id);
                        }
                        Some("idle") => {
                            // First read since launch — too early to trust: the harness may
                            // not have started the turn we just sent it yet. Keep polling.
                        }
                        Some("dead") => {
                            eprintln!(
                                "✗ {step_id:<15} failed    workstream \"{branch}\" died before finishing"
                            );
                            states.insert(step_id.clone(), StepState::Failed);
                            status_reads_since_launch.remove(step_id);
                        }
                        _ => {} // busy, waiting, unknown, no-pane, or not yet reported — keep polling
                    }
                }
            }
            Err(e) => {
                consecutive_status_failures += 1;
                if consecutive_status_failures >= MAX_CONSECUTIVE_STATUS_FAILURES {
                    eprintln!(
                        "✗ kanstack status failed {consecutive_status_failures} times in a row, giving up: {e}"
                    );
                    for step_id in &running {
                        states.insert(step_id.clone(), StepState::Failed);
                        status_reads_since_launch.remove(step_id);
                    }
                } else {
                    eprintln!(
                        "! kanstack status failed (attempt {consecutive_status_failures}/{MAX_CONSECUTIVE_STATUS_FAILURES}), retrying: {e}"
                    );
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
