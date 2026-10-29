//! Drives a [`Recipe`] to completion by shelling out to `kanstack` through [`KanstackClient`]
//! and polling `status --json` until each step's workstream goes idle and its `done_when` is
//! met (see this crate's README, "Spawn modes" and "Completion model").
//!
//! **How a step reaches its branch** is decided at launch ([`Driver::plan_launch`]): a step
//! with no `on` (or an advisory one) spawns its own branch, `stack` spawns `<target>-<step>`
//! `--above` its target, `continue` sends into the target branch's pane, and `handoff` stops
//! that pane's agent and spawns the step's own on the same branch. An `on` step with no
//! `spawn` continues when its agent is the branch's current one ([`RunState::branch_agents`])
//! and hands off when it isn't. Every branch carries the run's [`RunState::branch_prefix`], so
//! two runs of one recipe never meet on a lane.
//!
//! **Idle is necessary, not sufficient.** A trusted idle only ends a step once its `done_when`
//! holds: its branch tip moved since its `base` (`commits`), or a `kanstack note` about its
//! branch arrived in the events log after it launched (`note`), or either. Until then the step
//! keeps polling with `waiting_on` set, so `runs` shows why it isn't done.
//!
//! Failure policy: a step whose `verify` commands don't all exit 0, or whose pane dies, is
//! retried while it has budget left ([`StepRecord::retry_budget`], 0 unless the run was given
//! one): a failed verify sends the failing command and the tail of its output into the same
//! pane ([`Retry::FixForward`]), and a dead or gone pane respawns the step's agent on its
//! branch with its prompt and a note about what happened ([`Retry::Respawn`]). A `spawn` or
//! `send` that fails in transport (kanstack unreachable, the multiplexer down) is retried with
//! backoff and doesn't use up the budget. Out of budget, the step is marked `failed` and none
//! of its dependents ever become runnable (their `needs` can never all be `complete`).
//! Independent sibling steps that are already running are left to finish on their own — see
//! the README for why.
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
//!
//! **One tick at a time.** A run advances in ticks: launch whatever is runnable, take one
//! `status --json` read, apply it to every in-flight step. [`start_run`] and [`resume_run`]
//! loop those ticks until the run ends; [`ActiveRun`] exposes them one by one, so `serve` can
//! interleave several runs in one synchronous process and share one status read between them.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::graph::{resolve_workstream, runnable_steps, StepState};
use crate::kanstack::{KanstackClient, KanstackError, WorkstreamStatus};
use crate::recipe::{Recipe, SpawnMode, Step};
use crate::state::{now_secs, Retry, RunState, RunStatus, RunStore, StepRecord, StepStatus};

/// How many consecutive `status --json` failures in a row are tolerated as transient before
/// every currently-running step is failed. See the module doc.
const MAX_CONSECUTIVE_STATUS_FAILURES: u32 = 3;

/// How many transport errors in a row one launch survives before its step fails. They wait
/// `poll_interval`, then twice that, and so on.
const MAX_TRANSPORT_RETRIES: u32 = 5;

/// How much of a failed `verify` command's output is sent back to the agent: its last lines,
/// at most this many, and at most [`VERIFY_TAIL_BYTES`] of them.
const VERIFY_TAIL_LINES: usize = 40;
const VERIFY_TAIL_BYTES: usize = 4000;

/// kanstack's error codes for a launch that failed on the way, not on its merits: retrying
/// the same `spawn`/`send` later can succeed. A failure with no code at all (kanstack didn't
/// run, or printed no JSON) counts too.
const TRANSPORT_ERROR_CODES: &[&str] = &["internal", "multiplexer_unavailable", "harness_unavailable", "but_failed", "delivery_failed"];

pub struct RunOptions {
    pub poll_interval: Duration,
    /// Print progress lines to stderr instead of stdout, which `serve --json` keeps for JSON.
    pub progress_to_stderr: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            poll_interval: Duration::from_millis(1500),
            progress_to_stderr: false,
        }
    }
}

pub struct RunOutcome {
    pub states: HashMap<String, StepState>,
    /// A step failed, or the run stopped early because its state couldn't be saved or its
    /// lanes couldn't be read (it's still `running` on disk then, for a later resume).
    pub failed: bool,
}

/// Runs `recipe` from scratch, keeping its state in memory only. `workdir` is where `verify`
/// commands run and git is asked for branch tips.
pub fn run_recipe(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions, workdir: &Path) -> RunOutcome {
    let mut run = RunState::new("in-memory", recipe, Path::new(""), String::new(), workdir.to_path_buf());
    let mut live = Live::default();
    Driver::new(recipe, client, opts, None, &mut live).drive(&mut run)
}

/// Runs a fresh `run` (see [`RunState::new`]) of `recipe`, saving every transition to `store`.
pub fn start_run(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions, store: &RunStore, run: &mut RunState) -> RunOutcome {
    let mut live = Live::default();
    let mut driver = Driver::new(recipe, client, opts, Some(store), &mut live);
    driver.persist(run);
    driver.drive(run)
}

/// Picks a persisted `run` of `recipe` back up where it left off: reconciles its in-flight
/// steps with their lanes, then carries on with the rest as [`start_run`] would. Checking that
/// `recipe` is still the one the run started with is the caller's job (see
/// [`crate::resume::resume`]).
pub fn resume_run(recipe: &Recipe, client: &KanstackClient, opts: &RunOptions, store: &RunStore, run: &mut RunState) -> RunOutcome {
    let mut live = Live::default();
    let mut driver = Driver::new(recipe, client, opts, Some(store), &mut live);
    driver.reconcile(run);
    driver.drive(run)
}

/// What a run's driver keeps in memory between ticks. None of it has to survive a restart:
/// a resumed run starts these afresh.
#[derive(Default)]
struct Live {
    // step id -> how many status reads have been taken since it started running (or was
    // reattached). A step needs at least 2 before an `idle` reading is trusted — see the
    // module doc.
    status_reads_since_launch: HashMap<String, u32>,
    consecutive_status_failures: u32,
    // A snapshot failed to save. Nothing more is launched or finished after that: the run
    // stops where it is, still `running` on disk, for a later resume to reconcile.
    broken: bool,
    // step id -> transport errors in a row, and when its launch may be tried again.
    backoff: HashMap<String, (u32, Instant)>,
}

/// One persisted run, driven a tick at a time by its caller (see the module doc's "One tick
/// at a time"): [`ActiveRun::launch`], then one [`KanstackClient::status`] read shared with
/// every other run handed to [`ActiveRun::poll`], until [`ActiveRun::ended`]; then
/// [`ActiveRun::finish`].
pub struct ActiveRun {
    pub recipe: Recipe,
    pub run: RunState,
    live: Live,
}

impl ActiveRun {
    /// A fresh `run` of `recipe`, its first snapshot saved.
    pub fn start(recipe: Recipe, run: RunState, client: &KanstackClient, opts: &RunOptions, store: &RunStore) -> Self {
        let mut active = ActiveRun { recipe, run, live: Live::default() };
        let (mut driver, run) = active.driver(client, opts, store);
        driver.prepare(run);
        driver.persist(run);
        active
    }

    /// A persisted `run` picked back up: its in-flight steps reconciled with their lanes, as
    /// [`resume_run`] does. Checking the recipe is still the run's is the caller's job.
    pub fn resume(recipe: Recipe, run: RunState, client: &KanstackClient, opts: &RunOptions, store: &RunStore) -> Self {
        let mut active = ActiveRun { recipe, run, live: Live::default() };
        let (mut driver, run) = active.driver(client, opts, store);
        driver.prepare(run);
        driver.reconcile(run);
        active
    }

    fn driver<'a>(&'a mut self, client: &'a KanstackClient, opts: &'a RunOptions, store: &'a RunStore) -> (Driver<'a>, &'a mut RunState) {
        (Driver::new(&self.recipe, client, opts, Some(store), &mut self.live), &mut self.run)
    }

    /// Launches every step that's runnable now.
    pub fn launch(&mut self, client: &KanstackClient, opts: &RunOptions, store: &RunStore) {
        let (mut driver, run) = self.driver(client, opts, store);
        driver.launch_runnable(run);
    }

    /// Whether any step is in flight, so the next tick needs a status read.
    pub fn in_flight(&self) -> bool {
        self.recipe.steps.keys().any(|id| self.run.steps.get(id).is_some_and(|rec| rec.status.in_flight()))
    }

    /// Applies one `status --json` read, taken after this tick's [`ActiveRun::launch`], to
    /// every in-flight step.
    pub fn poll(&mut self, client: &KanstackClient, opts: &RunOptions, store: &RunStore, statuses: Result<&[WorkstreamStatus], &str>) {
        let (mut driver, run) = self.driver(client, opts, store);
        let running = driver.in_flight(run);
        if !running.is_empty() {
            driver.poll(run, &running, statuses);
        }
    }

    /// Nothing is in flight and nothing more can run (or the run's state can't be saved).
    pub fn ended(&self) -> bool {
        self.live.broken || (!self.in_flight() && runnable_steps(&self.recipe, &self.run.graph_states()).is_empty())
    }

    /// Records how an [`ended`](ActiveRun::ended) run came out.
    pub fn finish(&mut self, client: &KanstackClient, opts: &RunOptions, store: &RunStore) -> RunOutcome {
        let (mut driver, run) = self.driver(client, opts, store);
        driver.conclude(run)
    }
}

/// How a step reaches its branch this attempt.
enum Launch {
    /// `kanstack spawn` a new lane: the step's own branch, or a `stack` branch `--above` its
    /// target's.
    Spawn { above: Option<String> },
    /// `kanstack send` into the branch's existing pane.
    Continue,
    /// `kanstack stop` the branch's lane, then `kanstack spawn` this step's agent on it. Also
    /// how a dead pane is respawned.
    Handoff,
}

struct Driver<'a> {
    recipe: &'a Recipe,
    client: &'a KanstackClient,
    opts: &'a RunOptions,
    store: Option<&'a RunStore>,
    live: &'a mut Live,
}

impl<'a> Driver<'a> {
    fn new(recipe: &'a Recipe, client: &'a KanstackClient, opts: &'a RunOptions, store: Option<&'a RunStore>, live: &'a mut Live) -> Self {
        Driver { recipe, client, opts, store, live }
    }

    /// A progress line, tagged with the run's item when it has one.
    fn say(&self, run: &RunState, line: std::fmt::Arguments) {
        let tag = run.item.as_deref().map(|item| format!("[item {item}] ")).unwrap_or_default();
        if self.opts.progress_to_stderr {
            eprintln!("{tag}{line}");
        } else {
            println!("{tag}{line}");
        }
    }

    /// A problem line, on stderr, tagged like [`Driver::say`]'s.
    fn warn(&self, run: &RunState, line: std::fmt::Arguments) {
        let tag = run.item.as_deref().map(|item| format!("[item {item}] ")).unwrap_or_default();
        eprintln!("{tag}{line}");
    }

    /// Saves `run`'s snapshot, if this run is persisted. `false` (and the run stops) if it
    /// couldn't be.
    fn persist(&mut self, run: &RunState) -> bool {
        let Some(store) = self.store else { return true };
        match store.save(run) {
            Ok(()) => true,
            Err(e) => {
                self.warn(run, format_args!("✗ could not save run {}'s state, stopping: {e}", run.id));
                self.live.broken = true;
                false
            }
        }
    }

    /// The branch `step_id` works on in this run.
    fn branch_for(&self, run: &RunState, step_id: &str) -> String {
        format!("{}{}", run.branch_prefix, resolve_workstream(self.recipe, step_id))
    }

    /// A forced resume of an edited recipe can bring steps the run has no record of yet.
    fn prepare(&mut self, run: &mut RunState) {
        for id in self.recipe.steps.keys() {
            run.steps.entry(id.clone()).or_default();
        }
    }

    fn drive(&mut self, run: &mut RunState) -> RunOutcome {
        self.prepare(run);
        while !self.ended(run) {
            self.launch_runnable(run);
            if self.live.broken {
                break;
            }

            let running = self.in_flight(run);
            if running.is_empty() {
                // Every launch this tick already failed at spawn/send time (loop again, so any
                // still-independent pending steps get their chance), or is waiting out a
                // transport error's backoff.
                if !self.live.backoff.is_empty() {
                    thread::sleep(self.opts.poll_interval);
                }
                continue;
            }

            let statuses = self.client.status().map_err(|e| e.to_string());
            self.poll(run, &running, statuses.as_deref().map_err(String::as_str));

            if !self.in_flight(run).is_empty() {
                thread::sleep(self.opts.poll_interval);
            }
        }
        self.conclude(run)
    }

    /// Nothing is in flight and nothing more can run: everything is done, or every remaining
    /// pending step has a failed dependency. Or the run's state can't be saved.
    fn ended(&self, run: &RunState) -> bool {
        self.live.broken || (self.in_flight(run).is_empty() && runnable_steps(self.recipe, &run.graph_states()).is_empty())
    }

    fn conclude(&mut self, run: &mut RunState) -> RunOutcome {
        let failed = self.recipe.steps.keys().any(|id| run.steps[id].status == StepStatus::Failed);
        if !self.live.broken {
            let all_done = self.recipe.steps.keys().all(|id| run.steps[id].status == StepStatus::Done);
            run.status = if all_done && !failed { RunStatus::Done } else { RunStatus::Failed };
            self.persist(run);
        }
        RunOutcome {
            states: self.recipe.steps.keys().map(|id| (id.clone(), run.steps[id].status.graph_state())).collect(),
            failed: failed || self.live.broken,
        }
    }

    /// Launches every runnable step whose branch is free and that isn't waiting out a
    /// transport error's backoff.
    fn launch_runnable(&mut self, run: &mut RunState) {
        let runnable = runnable_steps(self.recipe, &run.graph_states());

        // Branches already claimed by an in-flight step — never spawn/send onto one of these
        // again until it frees up. Updated in-line below too, so two runnable steps that
        // resolve to the same branch in this same tick don't both launch onto it.
        let mut busy_branches: HashSet<String> = self
            .in_flight(run)
            .iter()
            .filter_map(|id| run.steps[id].branch.clone())
            .collect();

        let now = Instant::now();
        for step_id in &runnable {
            if self.live.backoff.get(step_id).is_some_and(|(_, at)| *at > now) {
                continue;
            }
            let branch = self.branch_for(run, step_id);
            if busy_branches.contains(&branch) {
                // Another step already owns this workstream right now; try again next tick
                // once it frees up, rather than racing a second spawn/send onto it.
                continue;
            }
            if self.launch(run, step_id, &branch) {
                busy_branches.insert(branch);
            }
            if self.live.broken {
                break;
            }
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

    /// How `step_id` reaches `branch`, from its `spawn` mode, or for an `on` step with none,
    /// from whether its agent is the branch's current one. `Err` when `continue` would hand the
    /// prompt to another agent: parse rejects the cases `needs` orders, and this catches a
    /// sibling on the same branch that ran in between.
    fn plan_launch(&self, run: &RunState, step_id: &str, branch: &str) -> Result<Launch, String> {
        let step = &self.recipe.steps[step_id];
        let on = match (&step.on, step.spawn) {
            (None, _) | (Some(_), Some(SpawnMode::Advisory)) => return Ok(Launch::Spawn { above: None }),
            (Some(on), _) => on,
        };
        // A run saved before branch agents were recorded only knows the target step's agent.
        let current = run.branch_agents.get(branch).unwrap_or(&self.recipe.steps[on].agent);
        match step.spawn {
            Some(SpawnMode::Stack) => Ok(Launch::Spawn {
                above: Some(self.branch_for(run, on)),
            }),
            Some(SpawnMode::Handoff) => Ok(Launch::Handoff),
            Some(SpawnMode::Continue) if *current != step.agent => Err(format!(
                "`spawn: continue` would send {}'s prompt to {current}, the agent on \"{branch}\"; use `spawn: handoff`",
                step.agent
            )),
            None if *current != step.agent => Ok(Launch::Handoff),
            _ => Ok(Launch::Continue),
        }
    }

    /// Spawns or sends `step_id` onto `branch`: its first attempt, or the retry its record
    /// holds. `true` if it's now running there.
    fn launch(&mut self, run: &mut RunState, step_id: &str, branch: &str) -> bool {
        let step = &self.recipe.steps[step_id];
        let retry = run.steps[step_id].retry.clone();
        let (plan, prompt) = match &retry {
            None => (self.plan_launch(run, step_id, branch), self.recipe.launch_prompt(step_id, branch)),
            Some(Retry::FixForward { message }) => (Ok(Launch::Continue), message.clone()),
            Some(Retry::Respawn { note }) => (Ok(Launch::Handoff), format!("{}\n\n{note}", self.recipe.launch_prompt(step_id, branch))),
        };

        let rec = &run.steps[step_id];
        self.say(
            run,
            format_args!(
                "→ {:<15} running   {} / {} / {}{}",
                step_id,
                step.agent,
                step.model.as_deref().unwrap_or("default"),
                step.effort.map(|e| e.as_str()).unwrap_or("default"),
                match (&retry, &plan) {
                    (Some(Retry::FixForward { .. }), _) => format!(" (fix forward on {branch}, retry {}/{})", rec.retries, rec.retry_budget),
                    (Some(Retry::Respawn { .. }), _) => format!(" (respawn on {branch}, retry {}/{})", rec.retries, rec.retry_budget),
                    (None, Ok(Launch::Spawn { above: Some(target) })) => format!(" (stack {branch} above {target})"),
                    (None, Ok(Launch::Spawn { above: None })) if step.spawn == Some(SpawnMode::Advisory) => " (advisory)".to_string(),
                    (None, Ok(Launch::Spawn { above: None })) if !run.branch_prefix.is_empty() => format!(" (on {branch})"),
                    (None, Ok(Launch::Continue)) => format!(" (continue on {branch})"),
                    (None, Ok(Launch::Handoff)) => format!(" (handoff on {branch})"),
                    _ => String::new(),
                }
            ),
        );

        // Read before the launch, so the agent's note can't land ahead of where this attempt's
        // notes are read from. A retry keeps reading from where the step's first attempt did.
        let notes_from = (retry.is_none() && step.done_when.allows_note()).then(|| self.client.events_end());

        // Record before acting: a launch that couldn't be recorded never happens, and one that
        // was recorded has used up its attempt even if this process dies mid-spawn. A retry
        // keeps the step's base, so the commits its earlier attempts made still count.
        let base = if retry.is_none() { branch_tip(&run.workdir, branch) } else { run.steps[step_id].base.clone() };
        let previous_agent = run.branch_agents.insert(branch.to_string(), step.agent.clone());
        let rec = record(run, step_id);
        rec.status = StepStatus::Launching;
        rec.attempt += 1;
        rec.branch = Some(branch.to_string());
        rec.pane = None;
        rec.base = base;
        rec.tip = None;
        rec.started_at = Some(now_secs());
        rec.waiting_on = None;
        if retry.is_none() {
            rec.last_error = None;
            rec.notes_from = notes_from.as_ref().and_then(|r| r.as_ref().ok().copied());
            rec.report = None;
        }
        if !self.persist(run) {
            return false;
        }

        let launch_result = match (plan, notes_from) {
            (Err(e), _) => Err(LaunchError::Fatal(e)),
            (_, Some(Err(e))) => Err(LaunchError::Transport(format!("could not read kanstack's events log: {e}"))),
            (Ok(plan), _) => self.act(plan, branch, step, &prompt),
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
                rec.retry = None;
                self.live.status_reads_since_launch.insert(step_id.to_string(), 0);
                self.live.backoff.remove(step_id);
                self.persist(run);
                true
            }
            Err(e) => {
                match previous_agent {
                    Some(agent) => run.branch_agents.insert(branch.to_string(), agent),
                    None => run.branch_agents.remove(branch),
                };
                match e {
                    LaunchError::Transport(e) => self.back_off(run, step_id, e),
                    LaunchError::Gone(e) => {
                        self.warn(run, format_args!("✗ {step_id:<15} failed    {e}"));
                        self.retry_or_fail(run, step_id, e, respawn_note);
                    }
                    LaunchError::Fatal(e) => {
                        self.warn(run, format_args!("✗ {step_id:<15} failed    {e}"));
                        self.fail(run, step_id, e);
                    }
                }
                false
            }
        }
    }

    /// Carries out `plan`, returning the pane the step now runs in.
    fn act(&self, plan: Launch, branch: &str, step: &Step, prompt: &str) -> Result<Option<String>, LaunchError> {
        match plan {
            Launch::Continue => self.client.send(branch, prompt).map(|r| r.pane).map_err(|e| match e.code.as_deref() {
                // The pane this was meant for is gone: as dead as a pane `status` says died.
                Some("unknown_workstream" | "no_pane") => LaunchError::Gone(format!("workstream \"{branch}\" was gone when sent to: {e}")),
                _ => LaunchError::from(e),
            }),
            Launch::Spawn { above } => self.client.spawn(branch, step, prompt, above.as_deref()).map(|r| r.pane).map_err(LaunchError::from),
            Launch::Handoff => self
                .client
                .stop(branch)
                .and_then(|()| self.client.spawn(branch, step, prompt, None))
                .map(|r| r.pane)
                .map_err(LaunchError::from),
        }
    }

    /// A launch failed in transport: put the step back to `pending` and try it again after a
    /// backoff, without touching its retry budget, until [`MAX_TRANSPORT_RETRIES`] in a row.
    fn back_off(&mut self, run: &mut RunState, step_id: &str, error: String) {
        let failures = self.live.backoff.get(step_id).map_or(0, |(n, _)| *n) + 1;
        if failures > MAX_TRANSPORT_RETRIES {
            let error = format!("{error} ({MAX_TRANSPORT_RETRIES} transport retries in a row failed)");
            self.warn(run, format_args!("✗ {step_id:<15} failed    {error}"));
            self.live.backoff.remove(step_id);
            self.fail(run, step_id, error);
            return;
        }
        let delay = self.opts.poll_interval * 2u32.pow(failures - 1);
        self.warn(
            run,
            format_args!("! {step_id:<15} launch failed in transport ({failures}/{MAX_TRANSPORT_RETRIES}), retrying in {delay:?}: {error}"),
        );
        self.live.backoff.insert(step_id.to_string(), (failures, Instant::now() + delay));
        let rec = record(run, step_id);
        rec.status = StepStatus::Pending;
        rec.last_error = Some(format!("transport error, retrying: {error}"));
        self.persist(run);
    }

    /// One `status --json` read, applied to every step in `running`.
    fn poll(&mut self, run: &mut RunState, running: &[String], statuses: Result<&[WorkstreamStatus], &str>) {
        match statuses {
            Ok(statuses) => {
                self.live.consecutive_status_failures = 0;
                let status_by_branch: HashMap<&str, &str> = statuses
                    .iter()
                    .map(|w| (w.branch.as_str(), w.status.as_str()))
                    .collect();

                for step_id in running {
                    let branch = run.steps[step_id].branch.clone().unwrap_or_default();
                    let reads = self.live.status_reads_since_launch.entry(step_id.clone()).or_insert(0);
                    *reads += 1;
                    let reads = *reads;
                    match status_by_branch.get(branch.as_str()).copied() {
                        Some("idle") if reads >= 2 => self.on_trusted_idle(run, step_id),
                        Some("idle") => {
                            // First read since launch — too early to trust: the harness may
                            // not have started the turn we just sent it yet. Keep polling.
                        }
                        Some(status @ ("dead" | "no-pane")) => {
                            let error = format!(
                                "workstream \"{branch}\" {} before finishing",
                                if status == "dead" { "died" } else { "lost its pane" }
                            );
                            self.warn(run, format_args!("✗ {step_id:<15} failed    {error}"));
                            self.retry_or_fail(run, step_id, error, respawn_note);
                        }
                        _ => {} // busy, waiting, unknown, or not yet reported — keep polling
                    }
                    if self.live.broken {
                        return;
                    }
                }
            }
            Err(e) => {
                self.live.consecutive_status_failures += 1;
                let failures = self.live.consecutive_status_failures;
                if failures >= MAX_CONSECUTIVE_STATUS_FAILURES {
                    self.warn(run, format_args!("✗ kanstack status failed {failures} times in a row, giving up: {e}"));
                    for step_id in running {
                        self.fail(run, step_id, format!("kanstack status failed {failures} times in a row: {e}"));
                    }
                } else {
                    self.warn(
                        run,
                        format_args!("! kanstack status failed (attempt {failures}/{MAX_CONSECUTIVE_STATUS_FAILURES}), retrying: {e}"),
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
    /// | gone (absent, dead, or no pane) | Count it as a failed attempt, and respawn if the budget allows. |
    ///
    /// A lane that still exists is never sent the step's prompt again, even when the step was
    /// only `launching` and the send may never have landed: a duplicate send garbles the
    /// agent's input, and a missing one is the lesser evil.
    ///
    /// The read taken here counts as the first since reattaching, so an `idle` on it is only
    /// acted on at once for a step that was already `verifying` (its lane was trusted idle and
    /// its `done_when` met before the crash). A `launching` or `running` step may have been
    /// sent its prompt a moment before the crash, which a first `idle` can't tell apart from a
    /// finished turn — the runner's usual rule — so it's judged on the next idle read instead,
    /// by its `done_when` like any other: a send that never landed leaves no commits and no
    /// note, so the step waits rather than passing.
    fn reconcile(&mut self, run: &mut RunState) {
        let in_flight = self.in_flight(run);
        if in_flight.is_empty() {
            return;
        }

        let statuses = loop {
            match self.client.status() {
                Ok(statuses) => break statuses,
                Err(e) => {
                    self.live.consecutive_status_failures += 1;
                    let failures = self.live.consecutive_status_failures;
                    if failures >= MAX_CONSECUTIVE_STATUS_FAILURES {
                        // Without the lanes there's no knowing what's safe, so leave the run
                        // exactly as it is on disk for the next resume.
                        self.warn(
                            run,
                            format_args!("✗ kanstack status failed {failures} times in a row, leaving run {} for a later resume: {e}", run.id),
                        );
                        self.live.broken = true;
                        return;
                    }
                    self.warn(
                        run,
                        format_args!("! kanstack status failed (attempt {failures}/{MAX_CONSECUTIVE_STATUS_FAILURES}), retrying: {e}"),
                    );
                    thread::sleep(self.opts.poll_interval);
                }
            }
        };
        self.live.consecutive_status_failures = 0;

        for step_id in &in_flight {
            let rec = &run.steps[step_id];
            let was = rec.status;
            let attempt = rec.attempt;
            let branch = rec.branch.clone().unwrap_or_else(|| self.branch_for(run, step_id));
            let lane = statuses.iter().find(|w| w.branch == branch);

            match lane.map(|w| w.status.as_str()) {
                None | Some("dead") | Some("no-pane") => {
                    let error = format!("workstream \"{branch}\" was gone on resume (attempt {attempt})");
                    self.warn(run, format_args!("✗ {step_id:<15} failed    {error}"));
                    record(run, step_id).branch = Some(branch);
                    self.retry_or_fail(run, step_id, error, respawn_note);
                }
                Some("idle") if was == StepStatus::Verifying => {
                    self.say(run, format_args!("↻ {step_id:<15} reattached idle, re-running verify"));
                    self.finish_step(run, step_id);
                }
                Some(status) => {
                    self.say(run, format_args!("↻ {step_id:<15} reattached {status} on {branch}"));
                    let pane = lane.and_then(|w| w.pane.clone());
                    let rec = record(run, step_id);
                    rec.status = StepStatus::Running;
                    rec.branch = Some(branch);
                    rec.retry = None;
                    if pane.is_some() {
                        rec.pane = pane;
                    }
                    self.live.status_reads_since_launch.insert(step_id.clone(), 1);
                    self.persist(run);
                }
            }
            if self.live.broken {
                return;
            }
        }
    }

    /// A step's lane is trusted idle. If its `done_when` is met, store the agent's report (if
    /// any) and finish it; if not, record what it's waiting on and keep polling. A failed read
    /// of the events log counts as no note yet, and is retried on the next idle read.
    fn on_trusted_idle(&mut self, run: &mut RunState, step_id: &str) {
        let done_when = self.recipe.steps[step_id].done_when;
        let rec = &run.steps[step_id];
        let branch = rec.branch.clone().unwrap_or_default();

        // A base of `null` means the branch didn't exist when the step started (or git
        // couldn't say), so any tip at all is movement.
        let committed = done_when.allows_commits()
            && branch_tip(&run.workdir, &branch).is_some_and(|tip| rec.base.as_deref() != Some(tip.as_str()));
        let report = if done_when.allows_note() {
            match self.client.notes_since(rec.notes_from.unwrap_or(0)) {
                Ok(notes) => notes.into_iter().rfind(|n| n.branch == branch).map(|n| n.text),
                Err(e) => {
                    self.warn(run, format_args!("! {step_id:<15} could not read kanstack's events log, retrying: {e}"));
                    None
                }
            }
        } else {
            None
        };

        if committed || report.is_some() {
            let rec = record(run, step_id);
            rec.waiting_on = None;
            if report.is_some() {
                rec.report = report;
            }
            self.finish_step(run, step_id);
        } else if run.steps[step_id].waiting_on != Some(done_when) {
            self.say(run, format_args!("… {step_id:<15} idle, waiting on {}", done_when.waiting_label()));
            record(run, step_id).waiting_on = Some(done_when);
            self.persist(run);
        }
    }

    /// A step's lane is trusted idle and its `done_when` is met: run its `verify` commands (if
    /// any) and mark it `done`, or retry or fail it. No `verify` at all means it's done now.
    fn finish_step(&mut self, run: &mut RunState, step_id: &str) {
        self.live.status_reads_since_launch.remove(step_id);
        let step = &self.recipe.steps[step_id];
        if step.verify.is_empty() {
            self.say(run, format_args!("✓ {step_id:<15} complete"));
            self.complete(run, step_id);
            return;
        }

        record(run, step_id).status = StepStatus::Verifying;
        if !self.persist(run) {
            return;
        }

        for cmd in &step.verify {
            match run_verify(&run.workdir, cmd) {
                Ok(()) => {}
                Err(VerifyFailure::Failed { error, tail }) => {
                    self.warn(run, format_args!("✗ {step_id:<15} verify failed: {error}"));
                    if !tail.is_empty() {
                        self.warn(run, format_args!("{tail}"));
                    }
                    self.retry_or_fail(run, step_id, format!("verify failed: {error}"), |rec, _| Retry::FixForward {
                        message: fix_forward_message(cmd, &error, &tail, rec.retries, rec.retry_budget),
                    });
                    return;
                }
                Err(VerifyFailure::CouldNotRun(e)) => {
                    self.warn(run, format_args!("✗ {step_id:<15} verify failed to run `{cmd}`: {e}"));
                    self.fail(run, step_id, format!("verify failed to run `{cmd}`: {e}"));
                    return;
                }
            }
        }

        self.say(run, format_args!("✓ {step_id:<15} verified  {}", step.verify.join(" && ")));
        self.complete(run, step_id);
    }

    /// `step_id`'s attempt failed with `error`. With budget left, count a retry and put the
    /// step back to `pending` with how to retry it (`retry`, given the record with the retry
    /// already counted, and the error), for the next tick to launch; out of budget, fail it.
    fn retry_or_fail(&mut self, run: &mut RunState, step_id: &str, error: String, retry: impl FnOnce(&StepRecord, &str) -> Retry) {
        let rec = &run.steps[step_id];
        if rec.retries >= rec.retry_budget {
            let error = if rec.retry_budget > 0 {
                format!("{error} (out of retries, {}/{})", rec.retries, rec.retry_budget)
            } else {
                error
            };
            self.fail(run, step_id, error);
            return;
        }
        self.live.status_reads_since_launch.remove(step_id);
        let tip = run.steps[step_id].branch.as_deref().and_then(|b| branch_tip(&run.workdir, b));
        let rec = record(run, step_id);
        rec.retries += 1;
        rec.retry = Some(retry(rec, &error));
        rec.status = StepStatus::Pending;
        rec.tip = tip;
        rec.last_error = Some(error);
        rec.waiting_on = None;
        let label = rec.retry_label().unwrap_or_default();
        self.say(run, format_args!("↻ {step_id:<15} retrying  ({label})"));
        self.persist(run);
    }

    fn complete(&mut self, run: &mut RunState, step_id: &str) {
        self.settle(run, step_id, StepStatus::Done, None);
    }

    fn fail(&mut self, run: &mut RunState, step_id: &str, error: String) {
        self.settle(run, step_id, StepStatus::Failed, Some(error));
    }

    /// Ends `step_id`'s current attempt as `status`, recording where its branch got to. A
    /// step that ends `done` keeps the error its last retry was for, so `runs` shows why it
    /// was retried.
    fn settle(&mut self, run: &mut RunState, step_id: &str, status: StepStatus, error: Option<String>) {
        self.live.status_reads_since_launch.remove(step_id);
        let tip = run.steps[step_id].branch.as_deref().and_then(|b| branch_tip(&run.workdir, b));
        let rec = record(run, step_id);
        rec.status = status;
        rec.tip = tip;
        if error.is_some() || rec.retries == 0 {
            rec.last_error = error;
        }
        rec.waiting_on = None;
        rec.retry = None;
        self.persist(run);
    }
}

/// Why a launch didn't happen.
enum LaunchError {
    /// kanstack couldn't be reached, or couldn't reach the multiplexer: try the same launch
    /// again later ([`Driver::back_off`]).
    Transport(String),
    /// The pane the launch was for is gone, as a dead one is: respawn, within the budget.
    Gone(String),
    /// Retrying the same launch can't help.
    Fatal(String),
}

impl From<KanstackError> for LaunchError {
    fn from(e: KanstackError) -> Self {
        match e.code.as_deref() {
            None => LaunchError::Transport(e.to_string()),
            Some(code) if TRANSPORT_ERROR_CODES.contains(&code) => LaunchError::Transport(e.to_string()),
            Some(_) => LaunchError::Fatal(e.to_string()),
        }
    }
}

/// The [`Retry::Respawn`] for a step whose pane died or was gone.
fn respawn_note(_: &StepRecord, error: &str) -> Retry {
    Retry::Respawn {
        note: format!(
            "Note: an earlier session on this step ended before it was finished ({error}). Whatever it committed is \
             still on this branch, so check `git log` and `git status` and carry on from there."
        ),
    }
}

/// What a failed `verify` sends back into the step's pane.
fn fix_forward_message(cmd: &str, error: &str, tail: &str, retry: u32, budget: u32) -> String {
    let output = if tail.is_empty() {
        "It printed nothing.".to_string()
    } else {
        format!("The end of its output:\n\n```\n{tail}\n```")
    };
    format!(
        "The step's check `{cmd}` failed: {error}. {output}\n\nFix it on this branch and commit. The check runs again \
         when you're done (retry {retry}/{budget})."
    )
}

enum VerifyFailure {
    /// It ran and didn't exit 0: `error` says how, `tail` is the end of what it printed.
    Failed { error: String, tail: String },
    CouldNotRun(std::io::Error),
}

/// Runs one `verify` command with `sh -c` in `workdir`, its stdout and stderr captured
/// together so a failure can be sent back to the agent.
fn run_verify(workdir: &Path, cmd: &str) -> Result<(), VerifyFailure> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(format!("exec 2>&1\n{cmd}")).stdin(Stdio::null());
    if !workdir.as_os_str().is_empty() {
        command.current_dir(workdir);
    }
    let output = command.output().map_err(VerifyFailure::CouldNotRun)?;
    if output.status.success() {
        return Ok(());
    }
    let error = format!(
        "`{cmd}` exited {}",
        output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "via signal".to_string())
    );
    Err(VerifyFailure::Failed { error, tail: tail(&String::from_utf8_lossy(&output.stdout)) })
}

/// The last [`VERIFY_TAIL_LINES`] lines of `output`, trimmed to [`VERIFY_TAIL_BYTES`] from the
/// end on a character boundary.
fn tail(output: &str) -> String {
    let lines: Vec<&str> = output.trim_end().lines().collect();
    let mut tail = lines[lines.len().saturating_sub(VERIFY_TAIL_LINES)..].join("\n");
    if tail.len() > VERIFY_TAIL_BYTES {
        let mut cut = tail.len() - VERIFY_TAIL_BYTES;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = format!("…{}", &tail[cut..]);
    }
    tail
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines_within_the_byte_cap() {
        let output: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let t = tail(&output);
        assert!(t.starts_with("line 61\n") && t.ends_with("line 100"), "{t}");

        let long = "é".repeat(VERIFY_TAIL_BYTES);
        let t = tail(&long);
        assert!(t.starts_with('…') && t.len() <= VERIFY_TAIL_BYTES + '…'.len_utf8() + 1, "{}", t.len());
    }

    #[test]
    fn transport_errors_are_told_apart_by_code() {
        let err = |code: Option<&str>| KanstackError { code: code.map(String::from), message: "x".to_string() };
        assert!(matches!(LaunchError::from(err(None)), LaunchError::Transport(_)));
        assert!(matches!(LaunchError::from(err(Some("multiplexer_unavailable"))), LaunchError::Transport(_)));
        assert!(matches!(LaunchError::from(err(Some("invalid_arguments"))), LaunchError::Fatal(_)));
        assert!(matches!(LaunchError::from(err(Some("workstream_exists"))), LaunchError::Fatal(_)));
    }
}
