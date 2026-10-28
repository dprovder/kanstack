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
//!
//! **Durable when given a [`RunStore`].** Every step transition is saved as a fresh snapshot
//! of the run's `state.json` (see [`crate::state`]) — and `launching`, with the attempt
//! counted, is saved *before* `spawn`/`send` runs, so a launch that couldn't be recorded never
//! happens and a crash mid-launch still uses up its attempt. [`resume_run`] picks a run back
//! up: `done` steps are never touched, and every in-flight step is matched against its lane
//! first (see [`Driver::reconcile`]). A prompt is never sent twice to a lane that still
//! exists. [`run_recipe`] is the same loop with nothing persisted.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::graph::{resolve_workstream, runnable_steps, StepState};
use crate::kanstack::KanstackClient;
use crate::recipe::Recipe;
use crate::state::{now_secs, RunState, RunStatus, RunStore, StepRecord, StepStatus};

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
    /// A step failed, or the run stopped early because its state couldn't be saved or its
    /// lanes couldn't be read (it's still `running` on disk then, for a later resume).
    pub failed: bool,
}

/// Runs `recipe` from scratch, keeping its state in memory only.
pub fn run_recipe(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions) -> RunOutcome {
    let workdir = std::env::current_dir().unwrap_or_default();
    let mut run = RunState::new("in-memory", recipe, Path::new(""), String::new(), workdir);
    Driver::new(recipe, client, opts, None).drive(&mut run)
}

/// Runs a fresh `run` (see [`RunState::new`]) of `recipe`, saving every transition to `store`.
pub fn start_run(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions, store: &RunStore, run: &mut RunState) -> RunOutcome {
    let mut driver = Driver::new(recipe, client, opts, Some(store));
    driver.persist(run);
    driver.drive(run)
}

/// Picks a persisted `run` of `recipe` back up where it left off: reconciles its in-flight
/// steps with their lanes, then carries on with the rest as [`start_run`] would. Checking that
/// `recipe` is still the one the run started with is the caller's job (see
/// [`crate::resume::resume`]).
pub fn resume_run(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions, store: &RunStore, run: &mut RunState) -> RunOutcome {
    let mut driver = Driver::new(recipe, client, opts, Some(store));
    driver.reconcile(run);
    driver.drive(run)
}

struct Driver<'a> {
    recipe: &'a Recipe,
    client: &'a KanstackClient,
    opts: &'a RunOptions,
    store: Option<&'a RunStore>,
    // step id -> how many status reads have been taken since it started running (or was
    // reattached). A step needs at least 2 before an `idle` reading is trusted — see the
    // module doc.
    status_reads_since_launch: HashMap<String, u32>,
    consecutive_status_failures: u32,
    // A snapshot failed to save. Nothing more is launched or finished after that: the run
    // stops where it is, still `running` on disk, for a later resume to reconcile.
    broken: bool,
}

impl<'a> Driver<'a> {
    fn new(recipe: &'a Recipe, client: &'a KanstackClient, opts: &'a RunOptions, store: Option<&'a RunStore>) -> Self {
        Driver {
            recipe,
            client,
            opts,
            store,
            status_reads_since_launch: HashMap::new(),
            consecutive_status_failures: 0,
            broken: false,
        }
    }

    /// Saves `run`'s snapshot, if this run is persisted. `false` (and the run stops) if it
    /// couldn't be.
    fn persist(&mut self, run: &RunState) -> bool {
        let Some(store) = self.store else { return true };
        match store.save(run) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("✗ could not save run {}'s state, stopping: {e}", run.id);
                self.broken = true;
                false
            }
        }
    }

    fn drive(&mut self, run: &mut RunState) -> RunOutcome {
        // A forced resume of an edited recipe can bring steps the run has no record of yet.
        for id in self.recipe.steps.keys() {
            run.steps.entry(id.clone()).or_default();
        }

        while !self.broken {
            let runnable = runnable_steps(self.recipe, &run.graph_states());

            // Branches already claimed by an in-flight step — never spawn/send onto one of
            // these again until it frees up. Updated in-line below too, so two runnable steps
            // that resolve to the same branch in this same tick don't both launch onto it.
            let mut busy_branches: HashSet<String> = self
                .in_flight(run)
                .iter()
                .filter_map(|id| run.steps[id].branch.clone())
                .collect();

            let mut launched_this_tick = false;

            for step_id in &runnable {
                let branch = resolve_workstream(self.recipe, step_id);
                if busy_branches.contains(&branch) {
                    // Another step already owns this workstream right now; try again next tick
                    // once it frees up, rather than racing a second spawn/send onto it.
                    continue;
                }
                launched_this_tick = true;
                if self.launch(run, step_id, &branch) {
                    busy_branches.insert(branch);
                }
                if self.broken {
                    break;
                }
            }
            if self.broken {
                break;
            }

            let running = self.in_flight(run);
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

            self.poll(run, &running);

            if !self.in_flight(run).is_empty() {
                thread::sleep(self.opts.poll_interval);
            }
        }

        let failed = self.recipe.steps.keys().any(|id| run.steps[id].status == StepStatus::Failed);
        if !self.broken {
            let all_done = self.recipe.steps.keys().all(|id| run.steps[id].status == StepStatus::Done);
            run.status = if all_done && !failed { RunStatus::Done } else { RunStatus::Failed };
            self.persist(run);
        }
        RunOutcome {
            states: self.recipe.steps.keys().map(|id| (id.clone(), run.steps[id].status.graph_state())).collect(),
            failed: failed || self.broken,
        }
    }

    /// The recipe's steps that are in flight, in recipe order.
    fn in_flight(&self, run: &RunState) -> Vec<String> {
        self.recipe
            .steps
            .keys()
            .filter(|id| run.steps.get(*id).is_some_and(|rec| rec.status.in_flight()))
            .cloned()
            .collect()
    }

    /// Spawns or sends `step_id` onto `branch`. `true` if it's now running there.
    fn launch(&mut self, run: &mut RunState, step_id: &str, branch: &str) -> bool {
        let step = &self.recipe.steps[step_id];
        let effective_prompt = self.recipe.effective_prompt(step_id);
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

        // Record before acting: a launch that couldn't be recorded never happens, and one that
        // was recorded has used up its attempt even if this process dies mid-spawn.
        let base = branch_tip(&run.workdir, branch);
        let rec = record(run, step_id);
        rec.status = StepStatus::Launching;
        rec.attempt += 1;
        rec.branch = Some(branch.to_string());
        rec.pane = None;
        rec.base = base;
        rec.tip = None;
        rec.started_at = Some(now_secs());
        rec.last_error = None;
        if !self.persist(run) {
            return false;
        }

        let launch_result = if placed_on_existing {
            self.client.send(branch, &effective_prompt).map(|r| r.pane)
        } else {
            self.client.spawn(branch, step, &effective_prompt).map(|r| r.pane)
        };

        match launch_result {
            Ok(pane) => {
                // A spawned branch doesn't exist until the spawn creates it, so its base is
                // only readable now.
                let base = match &run.steps[step_id].base {
                    Some(base) => Some(base.clone()),
                    None => branch_tip(&run.workdir, branch),
                };
                let rec = record(run, step_id);
                rec.status = StepStatus::Running;
                rec.pane = pane;
                rec.base = base;
                self.status_reads_since_launch.insert(step_id.to_string(), 0);
                self.persist(run);
                true
            }
            Err(e) => {
                eprintln!("✗ {step_id:<15} failed    {e}");
                self.fail(run, step_id, e.to_string());
                false
            }
        }
    }

    /// One `status --json` read, applied to every step in `running`.
    fn poll(&mut self, run: &mut RunState, running: &[String]) {
        match self.client.status() {
            Ok(statuses) => {
                self.consecutive_status_failures = 0;
                let status_by_branch: HashMap<&str, &str> = statuses
                    .iter()
                    .map(|w| (w.branch.as_str(), w.status.as_str()))
                    .collect();

                for step_id in running {
                    let branch = run.steps[step_id].branch.clone().unwrap_or_default();
                    let reads = self.status_reads_since_launch.entry(step_id.clone()).or_insert(0);
                    *reads += 1;
                    let reads = *reads;
                    match status_by_branch.get(branch.as_str()).copied() {
                        Some("idle") if reads >= 2 => self.finish_step(run, step_id),
                        Some("idle") => {
                            // First read since launch — too early to trust: the harness may
                            // not have started the turn we just sent it yet. Keep polling.
                        }
                        Some("dead") => {
                            let error = format!("workstream \"{branch}\" died before finishing");
                            eprintln!("✗ {step_id:<15} failed    {error}");
                            self.fail(run, step_id, error);
                        }
                        _ => {} // busy, waiting, unknown, no-pane, or not yet reported — keep polling
                    }
                    if self.broken {
                        return;
                    }
                }
            }
            Err(e) => {
                self.consecutive_status_failures += 1;
                let failures = self.consecutive_status_failures;
                if failures >= MAX_CONSECUTIVE_STATUS_FAILURES {
                    eprintln!("✗ kanstack status failed {failures} times in a row, giving up: {e}");
                    for step_id in running {
                        self.fail(run, step_id, format!("kanstack status failed {failures} times in a row: {e}"));
                    }
                } else {
                    eprintln!(
                        "! kanstack status failed (attempt {failures}/{MAX_CONSECUTIVE_STATUS_FAILURES}), retrying: {e}"
                    );
                }
            }
        }
    }

    /// Resume's first move: match every in-flight step against its lane in one
    /// `status --json` read, before anything is launched.
    ///
    /// | The lane is | Resume does |
    /// | --- | --- |
    /// | busy (or waiting, or unknown) | Reattach and keep polling. |
    /// | idle | Run the step's `verify` commands (which must be safe to re-run). |
    /// | gone (absent, dead, or no pane) | Count it as a failed attempt. |
    ///
    /// A lane that still exists is never sent the step's prompt again, even when the step was
    /// only `launching` and the send may never have landed: a duplicate send garbles the
    /// agent's input, and a missing one is the lesser evil.
    ///
    /// The read taken here counts as the first since reattaching, so an `idle` on it is only
    /// acted on at once for a step that was already `verifying` (its lane was trusted idle
    /// before the crash). A `launching` or `running` step may have been sent its prompt a
    /// moment before the crash, which a first `idle` can't tell apart from a finished turn —
    /// the runner's usual rule — so it runs `verify` on the next idle read instead.
    fn reconcile(&mut self, run: &mut RunState) {
        let in_flight = self.in_flight(run);
        if in_flight.is_empty() {
            return;
        }

        let statuses = loop {
            match self.client.status() {
                Ok(statuses) => break statuses,
                Err(e) => {
                    self.consecutive_status_failures += 1;
                    let failures = self.consecutive_status_failures;
                    if failures >= MAX_CONSECUTIVE_STATUS_FAILURES {
                        // Without the lanes there's no knowing what's safe, so leave the run
                        // exactly as it is on disk for the next resume.
                        eprintln!("✗ kanstack status failed {failures} times in a row, leaving run {} for a later resume: {e}", run.id);
                        self.broken = true;
                        return;
                    }
                    eprintln!(
                        "! kanstack status failed (attempt {failures}/{MAX_CONSECUTIVE_STATUS_FAILURES}), retrying: {e}"
                    );
                    thread::sleep(self.opts.poll_interval);
                }
            }
        };
        self.consecutive_status_failures = 0;

        for step_id in &in_flight {
            let rec = &run.steps[step_id];
            let was = rec.status;
            let attempt = rec.attempt;
            let branch = rec.branch.clone().unwrap_or_else(|| resolve_workstream(self.recipe, step_id));
            let lane = statuses.iter().find(|w| w.branch == branch);

            match lane.map(|w| w.status.as_str()) {
                None | Some("dead") | Some("no-pane") => {
                    let error = format!("workstream \"{branch}\" was gone on resume (attempt {attempt})");
                    eprintln!("✗ {step_id:<15} failed    {error}");
                    self.fail(run, step_id, error);
                }
                Some("idle") if was == StepStatus::Verifying => {
                    println!("↻ {step_id:<15} reattached idle, re-running verify");
                    self.finish_step(run, step_id);
                }
                Some(status) => {
                    println!("↻ {step_id:<15} reattached {status} on {branch}");
                    let pane = lane.and_then(|w| w.pane.clone());
                    let rec = record(run, step_id);
                    rec.status = StepStatus::Running;
                    rec.branch = Some(branch);
                    if pane.is_some() {
                        rec.pane = pane;
                    }
                    self.status_reads_since_launch.insert(step_id.clone(), 1);
                    self.persist(run);
                }
            }
            if self.broken {
                return;
            }
        }
    }

    /// A step's workstream has gone idle: run its `verify` commands (if any) and mark it
    /// `done` or `failed`. No `verify` at all means idle alone is sufficient.
    fn finish_step(&mut self, run: &mut RunState, step_id: &str) {
        self.status_reads_since_launch.remove(step_id);
        let step = &self.recipe.steps[step_id];
        if step.verify.is_empty() {
            println!("✓ {step_id:<15} complete");
            self.complete(run, step_id);
            return;
        }

        record(run, step_id).status = StepStatus::Verifying;
        if !self.persist(run) {
            return;
        }

        for cmd in &step.verify {
            let mut command = Command::new("sh");
            command.arg("-c").arg(cmd);
            if !run.workdir.as_os_str().is_empty() {
                command.current_dir(&run.workdir);
            }
            match command.status() {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    let error = format!(
                        "`{cmd}` exited {}",
                        status
                            .code()
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "via signal".to_string())
                    );
                    eprintln!("✗ {step_id:<15} verify failed: {error}");
                    self.fail(run, step_id, format!("verify failed: {error}"));
                    return;
                }
                Err(e) => {
                    eprintln!("✗ {step_id:<15} verify failed to run `{cmd}`: {e}");
                    self.fail(run, step_id, format!("verify failed to run `{cmd}`: {e}"));
                    return;
                }
            }
        }

        println!("✓ {step_id:<15} verified  {}", step.verify.join(" && "));
        self.complete(run, step_id);
    }

    fn complete(&mut self, run: &mut RunState, step_id: &str) {
        self.settle(run, step_id, StepStatus::Done, None);
    }

    fn fail(&mut self, run: &mut RunState, step_id: &str, error: String) {
        self.settle(run, step_id, StepStatus::Failed, Some(error));
    }

    /// Ends `step_id`'s current attempt as `status`, recording where its branch got to.
    fn settle(&mut self, run: &mut RunState, step_id: &str, status: StepStatus, error: Option<String>) {
        self.status_reads_since_launch.remove(step_id);
        let tip = run.steps[step_id].branch.as_deref().and_then(|b| branch_tip(&run.workdir, b));
        let rec = record(run, step_id);
        rec.status = status;
        rec.tip = tip;
        rec.last_error = error;
        self.persist(run);
    }
}

fn record<'r>(run: &'r mut RunState, step_id: &str) -> &'r mut StepRecord {
    run.steps.entry(step_id.to_string()).or_default()
}

/// The commit `branch` points at, asked of git in `workdir` (GitButler keeps a real
/// `refs/heads/<branch>` for every applied branch). `None` when the branch doesn't exist yet
/// or git can't be asked.
fn branch_tip(workdir: &Path, branch: &str) -> Option<String> {
    let mut command = Command::new("git");
    if !workdir.as_os_str().is_empty() {
        command.arg("-C").arg(workdir);
    }
    let output = command
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}^{{commit}}"))
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !sha.is_empty()).then_some(sha)
}
