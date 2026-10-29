//! Durable runs: snapshots on disk, resume after a crash, the orchestrator lock and the
//! recipe-hash check. Like `verification_gating.rs`, these drive the stand-in `kanstack` in
//! `common`, never a real one. Crashes are real: the stand-in's `spawn` can `kill -9` the
//! `kanstack-recipe` process that called it, before or after the lane comes up.

mod common;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use common::{acquire_lock, calls_log, fast_opts, git, init_repo, lane_status, tip, write_fake_kanstack};

use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::lock::{LockError, OrchestratorLock};
use kanstack_recipe::recipe::{self, DoneWhen};
use kanstack_recipe::resume::{resume, ResumeError};
use kanstack_recipe::run::start_run;
use kanstack_recipe::state::{recipe_hash, RunState, RunStatus, RunStore, StepRecord, StepStatus};

const TWO_STEPS: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: Do B\n---\nctx\n";
const ONE_STEP: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n";

/// A scratch layout: the fake kanstack, a recipe file, a run store and a working directory
/// that's a git repository, where the fake's agents commit.
struct Scratch {
    _tmp: tempfile::TempDir,
    bin_dir: PathBuf,
    recipe_path: PathBuf,
    state_path: PathBuf,
    workdir: PathBuf,
}

impl Scratch {
    fn new(recipe_source: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (bin_dir, state_path, workdir) = (root.join("bin"), root.join("state"), root.join("work"));
        for dir in [&bin_dir, &state_path, &workdir] {
            fs::create_dir_all(dir).unwrap();
        }
        write_fake_kanstack(&bin_dir);
        init_repo(&workdir, &bin_dir);
        let recipe_path = root.join("land.md");
        fs::write(&recipe_path, recipe_source).unwrap();
        Scratch { _tmp: tmp, bin_dir, recipe_path, state_path, workdir }
    }

    fn client(&self) -> KanstackClient {
        KanstackClient::new(self.bin_dir.join("kanstack"))
    }

    /// The library-level store; the binary's own lives under `state_path` too, keyed by
    /// `workdir`, and is reached through [`Scratch::cli`].
    fn store(&self) -> RunStore {
        RunStore::at(self.state_path.join("lib"))
    }

    /// A run of the recipe on disk, as `run` would start it, with `steps` overriding records.
    fn saved_run(&self, steps: &[(&str, StepRecord)]) -> RunState {
        let source = fs::read_to_string(&self.recipe_path).unwrap();
        let recipe = recipe::parse(&source).unwrap();
        let mut run = RunState::new("1-land", &recipe, &self.recipe_path, recipe_hash(&source), self.workdir.clone());
        for (id, rec) in steps {
            run.steps.insert(id.to_string(), rec.clone());
        }
        self.store().save(&run).unwrap();
        run
    }

    /// Runs the `kanstack-recipe` binary in `workdir`, with its state under `state_path`.
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanstack-recipe"))
            .args(args)
            .current_dir(&self.workdir)
            .env("KANSTACK_STATE_PATH", &self.state_path)
            .output()
            .unwrap()
    }

    /// An open lane on `branch`, created at `HEAD`. Returns that commit, a step's `base`.
    fn lane(&self, branch: &str) -> String {
        git(&self.workdir, &["branch", branch]);
        fs::write(self.bin_dir.join("lanes"), format!("{branch}\n")).unwrap();
        git(&self.workdir, &["rev-parse", "HEAD"])
    }

    /// [`Scratch::lane`], with the agent's commit not landed yet: the next `status` lands it.
    fn lane_mid_turn(&self, branch: &str) -> String {
        let base = self.lane(branch);
        fs::write(self.bin_dir.join("pending_commits"), format!("{branch}\n")).unwrap();
        base
    }

    fn kanstack_bin(&self) -> String {
        self.bin_dir.join("kanstack").to_string_lossy().into_owned()
    }

    /// `runs --json`, parsed.
    fn cli_runs(&self) -> Vec<RunState> {
        let output = self.cli(&["runs", "--json"]);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        serde_json::from_value(value["runs"].clone()).unwrap()
    }
}

fn in_flight(status: StepStatus, attempt: u32) -> StepRecord {
    StepRecord {
        status,
        attempt,
        branch: Some("a".to_string()),
        pane: Some("%7".to_string()),
        ..StepRecord::default()
    }
}

#[test]
fn run_writes_a_snapshot_that_runs_lists() {
    let s = Scratch::new(ONE_STEP);
    let output = s.cli(&["run", s.recipe_path.to_str().unwrap(), "--kanstack-bin", &s.kanstack_bin()]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let runs = s.cli_runs();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert_eq!(run.status, RunStatus::Done);
    assert_eq!(run.recipe, "land");
    assert_eq!(run.recipe_hash, recipe_hash(ONE_STEP));
    let a = &run.steps["a"];
    assert_eq!((a.status, a.attempt), (StepStatus::Done, 1));
    assert_eq!(a.pane.as_deref(), Some("%7"));
    assert!(a.started_at.is_some());

    let human = String::from_utf8_lossy(&s.cli(&["runs"]).stdout).into_owned();
    assert!(human.contains(&run.id) && human.contains("done"), "{human}");
}

/// The orchestrator is killed inside `kanstack spawn`, before the lane came up. `launching`
/// was saved with the attempt counted before the spawn ran, so the crash used that attempt
/// up; resume finds no lane, counts it failed, and never spawns again.
#[test]
fn crash_between_launching_and_running_uses_up_an_attempt() {
    let s = Scratch::new(ONE_STEP);
    fs::write(s.bin_dir.join("crash_before_spawn"), "").unwrap();
    let output = s.cli(&["run", s.recipe_path.to_str().unwrap(), "--kanstack-bin", &s.kanstack_bin()]);
    assert!(!output.status.success(), "the orchestrator was killed mid-spawn");

    let runs = s.cli_runs();
    let a = &runs[0].steps["a"];
    assert_eq!((runs[0].status, a.status, a.attempt), (RunStatus::Running, StepStatus::Launching, 1), "{a:?}");

    let output = s.cli(&["resume", "--kanstack-bin", &s.kanstack_bin()]);
    assert!(!output.status.success(), "a step whose lane is gone fails the run");

    let runs = s.cli_runs();
    let a = &runs[0].steps["a"];
    assert_eq!((runs[0].status, a.status, a.attempt), (RunStatus::Failed, StepStatus::Failed, 1), "{a:?}");
    assert!(a.last_error.as_deref().unwrap_or_default().contains("gone"), "{a:?}");
    assert_eq!(calls_log(&s.bin_dir).matches("spawn ").count(), 1, "{}", calls_log(&s.bin_dir));
}

/// Killed after the lane came up but before `running` was saved: the lane is still working,
/// so resume reattaches to it and waits it out instead of sending the prompt again.
#[test]
fn resume_reattaches_to_a_busy_lane() {
    let s = Scratch::new(ONE_STEP);
    fs::write(s.bin_dir.join("crash_after_spawn"), "").unwrap();
    s.cli(&["run", s.recipe_path.to_str().unwrap(), "--kanstack-bin", &s.kanstack_bin()]);
    assert_eq!(s.cli_runs()[0].steps["a"].status, StepStatus::Launching);

    // Resume's first status read sees the lane busy; the next ones see it idle.
    lane_status(&s.bin_dir, 1, "a", "busy");
    let output = s.cli(&["resume", "--kanstack-bin", &s.kanstack_bin()]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
    assert!(stdout.contains("reattached busy"), "{stdout}");

    let runs = s.cli_runs();
    let a = &runs[0].steps["a"];
    assert_eq!((runs[0].status, a.status, a.attempt), (RunStatus::Done, StepStatus::Done, 1), "{a:?}");
    assert_eq!(a.pane.as_deref(), Some("%7"), "the pane is taken from status on reattach");
    let calls = calls_log(&s.bin_dir);
    assert_eq!(calls.matches("spawn ").count(), 1, "{calls}");
    assert!(!calls.contains("send "), "a live lane is never sent the prompt again:\n{calls}");
}

/// The same, at the library level and with a step that reached `running`: an idle on the
/// read resume takes is not trusted (it may be the first read since the send), the next is.
#[test]
fn resume_does_not_trust_idle_on_its_first_read_of_a_running_step() {
    let s = Scratch::new(ONE_STEP);
    let base = s.lane_mid_turn("a");
    s.saved_run(&[("a", StepRecord { base: Some(base), ..in_flight(StepStatus::Running, 1) })]);

    let outcome = resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).unwrap();
    assert!(!outcome.failed);

    let calls = calls_log(&s.bin_dir);
    assert_eq!(calls.lines().filter(|l| *l == "status").count(), 2, "{calls}");
    assert!(!calls.contains("spawn ") && !calls.contains("send "), "{calls}");
    assert_eq!(s.store().load("1-land").unwrap().steps["a"].status, StepStatus::Done);
}

#[test]
fn resume_skips_done_steps() {
    let s = Scratch::new(TWO_STEPS);
    let done = StepRecord {
        status: StepStatus::Done,
        attempt: 1,
        branch: Some("a".to_string()),
        pane: Some("%3".to_string()),
        base: Some("a1b2c3d".to_string()),
        tip: Some("9c8d7e6".to_string()),
        started_at: Some(1),
        ..StepRecord::default()
    };
    s.saved_run(&[("a", done.clone())]);

    let outcome = resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).unwrap();
    assert!(!outcome.failed);

    let calls = calls_log(&s.bin_dir);
    assert!(!calls.contains("spawn a "), "a done step is never re-run:\n{calls}");
    assert!(calls.contains("spawn b "), "{calls}");
    let run = s.store().load("1-land").unwrap();
    assert_eq!(run.steps["a"], done, "a done step's record is left exactly as it was");
    assert_eq!((run.steps["b"].status, run.status), (StepStatus::Done, RunStatus::Done));
}

#[test]
fn resume_treats_a_gone_lane_as_a_failed_attempt() {
    let s = Scratch::new(TWO_STEPS);
    s.saved_run(&[("a", in_flight(StepStatus::Running, 1))]);
    // No `lanes` file: status reports no workstreams at all.

    let outcome = resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).unwrap();
    assert!(outcome.failed);

    let run = s.store().load("1-land").unwrap();
    let a = &run.steps["a"];
    assert_eq!((a.status, a.attempt), (StepStatus::Failed, 1), "{a:?}");
    assert!(a.last_error.as_deref().unwrap_or_default().contains("gone"), "{a:?}");
    assert_eq!(run.steps["b"].status, StepStatus::Pending, "a failed step's dependents never run");
    assert_eq!(run.status, RunStatus::Failed);
    let calls = calls_log(&s.bin_dir);
    assert!(!calls.contains("spawn ") && !calls.contains("send "), "{calls}");
}

/// A dead pane is as gone as a missing one.
#[test]
fn resume_treats_a_dead_lane_as_gone() {
    let s = Scratch::new(ONE_STEP);
    s.saved_run(&[("a", in_flight(StepStatus::Launching, 2))]);
    lane_status(&s.bin_dir, 1, "a", "dead");

    resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).unwrap();
    let a = &s.store().load("1-land").unwrap().steps["a"];
    assert_eq!((a.status, a.attempt), (StepStatus::Failed, 2), "{a:?}");
}

/// A step that crashed mid-`verify` was already trusted idle, so an idle lane on resume
/// re-runs its verify commands at once.
#[test]
fn resume_reruns_verify_for_a_verifying_step_on_an_idle_lane() {
    let s = Scratch::new("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - touch verified\n---\nctx\n");
    s.saved_run(&[("a", in_flight(StepStatus::Verifying, 1))]);
    fs::write(s.bin_dir.join("lanes"), "a\n").unwrap();

    let outcome = resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).unwrap();
    assert!(!outcome.failed);
    assert!(s.workdir.join("verified").exists(), "verify runs in the run's workdir");
    assert_eq!(calls_log(&s.bin_dir).lines().filter(|l| *l == "status").count(), 1);
}

#[test]
fn a_second_orchestrator_is_refused_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let store = RunStore::at(tmp.path().join("recipe-0"));

    let held = OrchestratorLock::try_acquire(&store).unwrap();
    assert!(matches!(OrchestratorLock::try_acquire(&store), Err(LockError::Held(_))));
    drop(held);
    acquire_lock(&store); // dropping the holder releases the lock
}

#[test]
fn the_binary_refuses_to_run_while_another_orchestrator_holds_the_lock() {
    let s = Scratch::new(ONE_STEP);
    // The binary keys its store by its working directory, under KANSTACK_STATE_PATH (setting
    // that here would race the other tests), so a first run shows where it is.
    let output = s.cli(&["run", s.recipe_path.to_str().unwrap(), "--kanstack-bin", &s.kanstack_bin()]);
    assert!(output.status.success());
    let mut key_dirs: Vec<PathBuf> = fs::read_dir(&s.state_path)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("recipe-"))
        .collect();
    assert_eq!(key_dirs.len(), 1, "{key_dirs:?}");
    let store = RunStore::at(key_dirs.pop().unwrap());

    let _held = OrchestratorLock::try_acquire(&store).unwrap();
    let bin = s.kanstack_bin();
    let recipe_path = s.recipe_path.to_str().unwrap();
    for args in [vec!["resume", "--kanstack-bin", &bin], vec!["run", recipe_path, "--kanstack-bin", &bin]] {
        let output = s.cli(&args);
        assert!(!output.status.success(), "{args:?} must refuse to start");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("another orchestrator"), "{stderr}");
    }
    assert_eq!(calls_log(&s.bin_dir).matches("spawn ").count(), 1, "the refused run never launched anything");
}

#[test]
fn resume_refuses_a_run_whose_recipe_changed_unless_forced() {
    let s = Scratch::new(ONE_STEP);
    let before = s.saved_run(&[]);
    let edited = ONE_STEP.replace("Do A", "Do A differently");
    fs::write(&s.recipe_path, &edited).unwrap();

    let err = resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).err().expect("a changed recipe is refused");
    assert!(matches!(err, ResumeError::RecipeChanged(_)), "{err}");
    assert_eq!(s.store().load("1-land").unwrap(), before, "a refused resume changes nothing");
    assert!(calls_log(&s.bin_dir).is_empty());

    let outcome = resume(&s.store(), &s.client(), &fast_opts(), "1-land", true).unwrap();
    assert!(!outcome.failed);
    let run = s.store().load("1-land").unwrap();
    assert_eq!(run.recipe_hash, recipe_hash(&edited), "--force records the new hash");
    assert!(calls_log(&s.bin_dir).contains("Do A differently"));
}

#[test]
fn resume_refuses_a_finished_run() {
    let s = Scratch::new(ONE_STEP);
    let mut run = s.saved_run(&[]);
    run.status = RunStatus::Done;
    s.store().save(&run).unwrap();
    assert!(matches!(resume(&s.store(), &s.client(), &fast_opts(), "1-land", false), Err(ResumeError::Finished(RunStatus::Done))));
}

/// Each step's branch tip is recorded when it starts (`base`) and when it finishes (`tip`).
#[test]
fn records_each_steps_base_and_tip() {
    let s = Scratch::new(ONE_STEP);
    let head = git(&s.workdir, &["rev-parse", "HEAD"]);

    let source = fs::read_to_string(&s.recipe_path).unwrap();
    let recipe = recipe::parse(&source).unwrap();
    let mut run = RunState::new("2-land", &recipe, &s.recipe_path, recipe_hash(&source), s.workdir.clone());
    let outcome = start_run(&recipe, &s.client(), &fast_opts(), &s.store(), &mut run);
    assert!(!outcome.failed);

    let a = &s.store().load("2-land").unwrap().steps["a"];
    assert_eq!(a.base.as_deref(), Some(head.as_str()), "the spawn created `a` at HEAD");
    assert_eq!(a.tip.as_deref(), Some(tip(&s.workdir, "a").as_str()));
    assert_ne!(a.base, a.tip, "the stand-in agent committed during the step");
}

/// The agent's `kanstack note` report is stored in the step's state, so it survives a crash:
/// here the orchestrator is killed for real by the step's own `verify`, after the note was
/// read, and the report is still there once resume re-runs verify and finishes the step.
#[test]
fn a_steps_report_survives_a_resume() {
    let s = Scratch::new("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    done_when: note\n    prompt: Do A\n    verify:\n      - \"[ -f crashed ] || { touch crashed; kill -9 $PPID; }\"\n---\nctx\n");
    fs::write(s.bin_dir.join("notes"), "").unwrap();
    fs::write(s.bin_dir.join("no_commits"), "").unwrap();
    let output = s.cli(&["run", s.recipe_path.to_str().unwrap(), "--kanstack-bin", &s.kanstack_bin()]);
    assert!(!output.status.success(), "the orchestrator was killed mid-verify");

    let a = &s.cli_runs()[0].steps["a"];
    assert_eq!((a.status, a.report.as_deref()), (StepStatus::Verifying, Some("report from a")), "{a:?}");

    let output = s.cli(&["resume", "--kanstack-bin", &s.kanstack_bin()]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let runs = s.cli_runs();
    let a = &runs[0].steps["a"];
    assert_eq!((runs[0].status, a.status, a.report.as_deref()), (RunStatus::Done, StepStatus::Done, Some("report from a")), "{a:?}");
    assert_eq!(calls_log(&s.bin_dir).matches("spawn ").count(), 1);
}

/// The lost-send case: the orchestrator crashed while `launching` a step whose lane exists
/// but is idle, so the prompt may never have arrived. Resume doesn't re-send it, and no longer
/// marks the step done either: with no commits since its base it waits (`waiting_on`), and
/// here fails once the lane dies.
#[test]
fn resume_judges_a_launching_step_on_an_idle_lane_by_done_when() {
    let s = Scratch::new(ONE_STEP);
    let base = s.lane("a");
    let run = s.saved_run(&[("a", StepRecord { base: Some(base), ..in_flight(StepStatus::Launching, 1) })]);
    fs::write(s.bin_dir.join("snapshot_path"), s.store().state_path(&run.id).to_str().unwrap()).unwrap();
    lane_status(&s.bin_dir, 5, "a", "dead");

    let outcome = resume(&s.store(), &s.client(), &fast_opts(), "1-land", false).unwrap();
    assert!(outcome.failed);

    let waiting: RunState = serde_json::from_str(&fs::read_to_string(s.bin_dir.join("snapshot.3.json")).unwrap()).unwrap();
    let a = &waiting.steps["a"];
    assert_eq!((a.status, a.waiting_on), (StepStatus::Running, Some(DoneWhen::Commits)), "{a:?}");

    let a = &s.store().load("1-land").unwrap().steps["a"];
    assert_eq!((a.status, a.waiting_on), (StepStatus::Failed, None), "{a:?}");
    assert!(a.last_error.as_deref().unwrap_or_default().contains("died"), "{a:?}");
    let calls = calls_log(&s.bin_dir);
    assert!(!calls.contains("spawn ") && !calls.contains("send "), "{calls}");
}
