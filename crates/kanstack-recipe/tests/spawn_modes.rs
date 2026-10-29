//! Spawn modes (`continue`, `handoff`, `stack`, `advisory` and the default between the first
//! two) and the completion rule (`done_when: commits|note|either`), driven against the
//! stand-in `kanstack` in `common`. Each test checks the exact `kanstack` calls a mode makes,
//! and what the run state records.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{actions, calls_log, fast_opts, git, graced_opts, init_repo, tip, write_fake_kanstack};
use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::recipe::{self, DoneWhen, Recipe};
use kanstack_recipe::run::{resume_run, start_run, RunOptions, RunOutcome};
use kanstack_recipe::state::{RunState, RunStore, StepRecord, StepStatus};

struct Lab {
    _tmp: tempfile::TempDir,
    bin_dir: PathBuf,
    repo: PathBuf,
    store: RunStore,
}

impl Lab {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (bin_dir, repo) = (root.join("bin"), root.join("repo"));
        fs::create_dir_all(&bin_dir).unwrap();
        write_fake_kanstack(&bin_dir);
        init_repo(&repo, &bin_dir);
        Lab { _tmp: tmp, bin_dir, repo, store: RunStore::at(root.join("state")) }
    }

    fn kanstack_bin(&self) -> PathBuf {
        self.bin_dir.join("kanstack")
    }

    fn client(&self) -> KanstackClient {
        KanstackClient::new(self.kanstack_bin())
    }

    fn knob(&self, name: &str, value: &str) {
        fs::write(self.bin_dir.join(name), value).unwrap();
    }

    fn new_run(&self, recipe: &Recipe) -> RunState {
        RunState::new("1-r", recipe, &self.repo.join("r.md"), String::new(), self.repo.clone())
    }

    /// Runs `source` from scratch, persisted, with the fake copying each snapshot it sees.
    fn run(&self, source: &str) -> (RunOutcome, RunState) {
        self.run_with(source, &fast_opts())
    }

    /// [`Lab::run`] with `opts`.
    fn run_with(&self, source: &str, opts: &RunOptions) -> (RunOutcome, RunState) {
        let recipe = recipe::parse(source).unwrap();
        let mut run = self.new_run(&recipe);
        self.knob("snapshot_path", self.store.state_path(&run.id).to_str().unwrap());
        let outcome = start_run(&recipe, &self.client(), opts, &self.store, &mut run);
        (outcome, self.store.load(&run.id).unwrap())
    }

    /// The run state the fake saw at its Nth `status` call.
    fn snapshot(&self, n: u32) -> RunState {
        serde_json::from_str(&fs::read_to_string(self.bin_dir.join(format!("snapshot.{n}.json"))).unwrap()).unwrap()
    }

    fn status_reads(&self) -> usize {
        calls_log(&self.bin_dir).lines().filter(|l| *l == "status").count()
    }
}

/// `implement` (codex) then `review` on its branch, with `review_agent` and `review_extra`
/// lines spliced into `review`.
fn implement_then_review(review_agent: &str, review_extra: &str) -> String {
    format!(
        "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: {review_agent}\n    needs: [implement]\n    on: implement\n{review_extra}    prompt: Review\n---\nctx\n"
    )
}

fn assert_actions(lab: &Lab, expected_prefixes: &[&str]) {
    let actions = actions(&lab.bin_dir);
    assert_eq!(actions.len(), expected_prefixes.len(), "{actions:#?}");
    for (action, prefix) in actions.iter().zip(expected_prefixes) {
        assert!(action.starts_with(prefix), "expected `{prefix}…`, got `{action}`\n{actions:#?}");
    }
}

#[test]
fn continue_sends_into_the_branchs_existing_pane() {
    let lab = Lab::new();
    let (outcome, run) = lab.run(&implement_then_review("codex", "    spawn: continue\n"));
    assert!(!outcome.failed);
    assert_actions(&lab, &["spawn implement --agent codex --prompt Implement", "send implement Review"]);
    assert_eq!(run.steps["review"].branch.as_deref(), Some("implement"));
}

#[test]
fn handoff_stops_the_branchs_agent_then_spawns_the_steps_own_on_it() {
    let lab = Lab::new();
    let (outcome, run) = lab.run(&implement_then_review("claude", "    spawn: handoff\n    model: opus\n    effort: high\n"));
    assert!(!outcome.failed);
    assert_actions(
        &lab,
        &[
            "spawn implement --agent codex --prompt Implement",
            "stop implement --json",
            "spawn implement --agent claude --model opus --effort high --prompt Review",
        ],
    );
    let (implement, review) = (&run.steps["implement"], &run.steps["review"]);
    assert_eq!(review.branch.as_deref(), Some("implement"));
    assert_eq!(review.base, implement.tip, "review's commits are counted from where implement left the branch");
    assert_ne!(review.tip, review.base);
    assert_eq!(run.branch_agents["implement"], "claude");
}

#[test]
fn stack_spawns_a_new_branch_above_the_target() {
    let lab = Lab::new();
    let (outcome, run) = lab.run(&implement_then_review("claude", "    spawn: stack\n"));
    assert!(!outcome.failed);
    assert_actions(
        &lab,
        &["spawn implement --agent codex --prompt Implement", "spawn implement-review --agent claude --above implement --prompt Review"],
    );
    let review = &run.steps["review"];
    assert_eq!(review.branch.as_deref(), Some("implement-review"));
    assert_eq!(review.base, run.steps["implement"].tip, "the stacked branch starts at implement's tip");
    assert_eq!(review.tip.as_deref(), Some(tip(&lab.repo, "implement-review").as_str()));
    assert_eq!(
        (run.branch_agents["implement"].as_str(), run.branch_agents["implement-review"].as_str()),
        ("codex", "claude")
    );
}

/// An advisory step spawns its own branch with `--advisory`, and by default is done on a note
/// alone, never needing a commit.
#[test]
fn advisory_spawns_its_own_branch_and_finishes_on_a_note() {
    let lab = Lab::new();
    lab.knob("notes", "");
    lab.knob("no_commits", "");
    let (outcome, run) = lab.run("---\nversion: 1\nsteps:\n  survey:\n    agent: claude\n    spawn: advisory\n    prompt: Survey\n---\nctx\n");
    assert!(!outcome.failed);
    assert_actions(&lab, &["spawn survey --agent claude --advisory --prompt Survey"]);
    assert!(actions(&lab.bin_dir)[0].contains(&format!("{} note survey \"<report>\"", lab.kanstack_bin().display())), "the prompt asks for the note");
    let survey = &run.steps["survey"];
    assert_eq!((survey.status, survey.report.as_deref()), (StepStatus::Done, Some("report from survey")));
}

/// The default for an `on` step: `handoff` when its agent differs from the branch's current
/// one, `continue` when it's the same. Here the branch goes codex → claude → codex → codex.
#[test]
fn the_default_hands_off_to_a_different_agent_and_continues_the_same_one() {
    let lab = Lab::new();
    let (outcome, run) = lab.run(
        "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: Review\n  docs:\n    agent: codex\n    needs: [review]\n    on: implement\n    prompt: Docs\n  polish:\n    agent: codex\n    needs: [docs]\n    on: implement\n    prompt: Polish\n---\nctx\n",
    );
    assert!(!outcome.failed);
    assert_actions(
        &lab,
        &[
            "spawn implement --agent codex",
            "stop implement",
            "spawn implement --agent claude --prompt Review",
            "stop implement",
            "spawn implement --agent codex --prompt Docs",
            "send implement Polish",
        ],
    );
    assert_eq!(run.branch_agents["implement"], "codex");
}

/// The acceptance recipe's `review` (claude) used to be typed into `implement`'s codex pane.
/// With `spawn: handoff` it gets a claude pane, and `docs` (codex, no `spawn`) hands back.
#[test]
fn the_acceptance_recipe_runs_review_in_its_own_agents_pane() {
    let lab = Lab::new();
    lab.knob("no_commits", "");
    lab.knob("notes", "");
    let source = include_str!("../../../examples/recipes/implement-review-docs.md").replace("cargo test", "true");
    // Nothing commits here, so give implement and docs the note `review` uses.
    let source = source.replace("    prompt: Add", "    done_when: note\n    prompt: Add").replace("    prompt: Update", "    done_when: note\n    prompt: Update");
    let (outcome, run) = lab.run(&source);
    assert!(!outcome.failed);
    assert_actions(
        &lab,
        &[
            "spawn implement --agent codex --model gpt-5.6 --effort high",
            "stop implement",
            "spawn implement --agent claude --effort high --prompt Review",
            "stop implement",
            "spawn implement --agent codex --effort low --prompt Update",
        ],
    );
    assert_eq!(run.steps["review"].report.as_deref(), Some("report from implement"));
}

/// The branch's current agent is read from run state, so the default decides the same way
/// after a resume as it would have in the first run.
#[test]
fn the_default_mode_comes_from_the_recorded_branch_agent_on_resume() {
    let source = "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: Review\n  docs:\n    agent: codex\n    needs: [review]\n    on: implement\n    prompt: Docs\n---\nctx\n";
    let recipe = recipe::parse(source).unwrap();

    for (branch_agent, expected) in [("claude", vec!["stop implement", "spawn implement --agent codex --prompt Docs"]), ("codex", vec!["send implement Docs"])] {
        let lab = Lab::new();
        common::git(&lab.repo, &["branch", "implement"]);
        lab.knob("lanes", "implement\n");
        let done = StepRecord { status: StepStatus::Done, attempt: 1, branch: Some("implement".to_string()), ..StepRecord::default() };
        let mut run = lab.new_run(&recipe);
        run.steps.insert("implement".to_string(), done.clone());
        run.steps.insert("review".to_string(), done);
        run.branch_agents.insert("implement".to_string(), branch_agent.to_string());
        lab.store.save(&run).unwrap();

        let mut run = lab.store.load(&run.id).unwrap();
        let outcome = resume_run(&recipe, &lab.client(), &fast_opts(), &lab.store, &mut run);
        assert!(!outcome.failed);
        assert_actions(&lab, &expected);
        assert_eq!(lab.store.load(&run.id).unwrap().branch_agents["implement"], "codex");
    }
}

#[test]
fn check_rejects_continue_with_a_different_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("bad.md");
    fs::write(&path, implement_then_review("claude", "    spawn: continue\n")).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kanstack-recipe")).arg("check").arg(&path).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("step \"review\" has `spawn: continue`") && stderr.contains("use `spawn: handoff`"), "{stderr}");
}

/// `commits`: a trusted idle with no new commits doesn't fail the step or finish it. It waits,
/// with `waiting_on` saying why, until the branch tip moves.
#[test]
fn done_when_commits_waits_for_the_branch_tip_to_move() {
    let lab = Lab::new();
    lab.knob("commit_at", "4");
    let (outcome, run) = lab.run("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n");
    assert!(!outcome.failed);

    // Read 1 is too early to trust; read 2 is a trusted idle with no commits yet.
    let a = &lab.snapshot(3).steps["a"];
    assert_eq!((a.status, a.waiting_on), (StepStatus::Running, Some(DoneWhen::Commits)), "{a:?}");
    assert_eq!(lab.status_reads(), 4, "done on the read the commit landed");
    let a = &run.steps["a"];
    assert_eq!((a.status, a.waiting_on, a.report.as_deref()), (StepStatus::Done, None, None));
    assert!(actions(&lab.bin_dir)[0].contains(&format!("{} note a \"<report>\"", lab.kanstack_bin().display())), "a commits step is asked for a report too");
    assert!(a.notes_from.is_some(), "and its notes are read from its launch");
}

const COMMITS_STEP: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n";

/// A `commits` step that committed with no note yet waits out the report's grace: a note that
/// lands during it is the step's report.
#[test]
fn a_commits_step_picks_up_a_note_that_lands_during_its_grace() {
    let lab = Lab::new();
    lab.knob("notes", "");
    lab.knob("note_at", "3");
    let (outcome, run) = lab.run_with(COMMITS_STEP, &graced_opts());
    assert!(!outcome.failed);

    // Read 2 is the first trusted idle: the commit is there, the note isn't yet.
    let a = &lab.snapshot(3).steps["a"];
    assert_eq!((a.status, a.waiting_on, a.report.as_deref()), (StepStatus::Running, None, None), "{a:?}");
    assert_eq!(lab.status_reads(), 3, "done on the read the note landed");
    let a = &run.steps["a"];
    assert_eq!((a.status, a.report.as_deref()), (StepStatus::Done, Some("report from a")));
    assert_ne!(a.base, a.tip);
}

/// The grace is bounded: with no note at all, the step finishes on its commits after
/// `note_grace_reads` more trusted idle reads, with no report.
#[test]
fn a_commits_step_with_no_note_finishes_after_its_grace() {
    let lab = Lab::new();
    let (outcome, run) = lab.run_with(COMMITS_STEP, &graced_opts());
    assert!(!outcome.failed);
    let grace = RunOptions::default().note_grace_reads as usize;
    assert_eq!(lab.status_reads(), 2 + grace, "the first trusted idle, then the grace");
    let a = &run.steps["a"];
    assert_eq!((a.status, a.report.as_deref(), a.retries), (StepStatus::Done, None, 0));
}

/// `done_when` still decides completion: a note alone doesn't finish a `commits` step, but it's
/// kept as the report, and the step finishes on the read its commit lands with no grace.
#[test]
fn a_note_alone_doesnt_finish_a_commits_step_but_is_its_report() {
    let lab = Lab::new();
    lab.knob("notes", "");
    lab.knob("commit_at", "4");
    let (outcome, run) = lab.run_with(COMMITS_STEP, &graced_opts());
    assert!(!outcome.failed);

    let a = &lab.snapshot(3).steps["a"];
    assert_eq!((a.status, a.waiting_on, a.report.as_deref()), (StepStatus::Running, Some(DoneWhen::Commits), Some("report from a")), "{a:?}");
    assert_eq!(lab.status_reads(), 4, "done on the read the commit landed");
    assert_eq!((run.steps["a"].status, run.steps["a"].report.as_deref()), (StepStatus::Done, Some("report from a")));
}

/// A lane that dies while its step waits on a report doesn't undo the commits it finished on:
/// the step is done, not respawned.
#[test]
fn a_lane_that_dies_during_the_grace_still_finishes_its_step() {
    let lab = Lab::new();
    lab.knob(
        "status.3.json",
        "{\"schema\":1,\"workstreams\":[{\"branch\":\"a\",\"pane\":\"%7\",\"agent\":\"codex\",\"item\":null,\"status\":\"dead\",\"lane\":null}],\"workspace\":null,\"workspace_blocked\":null}\n",
    );
    let (outcome, run) = lab.run_with(COMMITS_STEP, &graced_opts());
    assert!(!outcome.failed);
    assert_eq!(lab.status_reads(), 3);
    let a = &run.steps["a"];
    assert_eq!((a.status, a.attempt, a.retries, a.report.as_deref()), (StepStatus::Done, 1, 0, None));
    assert_eq!(actions(&lab.bin_dir).len(), 1, "no respawn");
}

/// `note`: commits alone don't finish it, and a note from before the step launched doesn't
/// count; only one written after does, and its text is stored as the step's report.
#[test]
fn done_when_note_waits_for_a_note_written_after_launch() {
    let lab = Lab::new();
    lab.knob("notes", "");
    lab.knob("note_at", "4");
    lab.knob(
        "events.jsonl",
        "{\"schema\":1,\"ts\":\"2026-10-27T00:00:00Z\",\"branch\":\"a\",\"kind\":\"note\",\"text\":\"stale\"}\n\
         {\"schema\":1,\"ts\":\"2026-10-27T00:00:00Z\",\"kind\":\"lifecycle\",\"command\":\"prune\"}\n",
    );
    let (outcome, run) = lab.run("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    done_when: note\n    prompt: Do A\n---\nctx\n");
    assert!(!outcome.failed);

    let a = &lab.snapshot(3).steps["a"];
    assert_eq!((a.status, a.waiting_on), (StepStatus::Running, Some(DoneWhen::Note)), "{a:?}");
    assert_ne!(a.base, Some(tip(&lab.repo, "a")), "the commit landed already, and wasn't enough");
    assert_eq!(lab.status_reads(), 4);
    let a = &run.steps["a"];
    assert_eq!((a.status, a.report.as_deref()), (StepStatus::Done, Some("report from a")));
    assert!(a.notes_from.unwrap() > 0, "notes are read from past the stale one");
    assert!(actions(&lab.bin_dir)[0].contains(&format!("{} note a \"<report>\"", lab.kanstack_bin().display())), "the prompt asks for the note");
}

/// `either`: a note with no commits finishes it, and so do commits with no note.
#[test]
fn done_when_either_accepts_a_note_or_commits() {
    let source = "---\nversion: 1\nsteps:\n  review:\n    agent: claude\n    done_when: either\n    prompt: Review\n---\nctx\n";

    let lab = Lab::new();
    lab.knob("notes", "");
    lab.knob("no_commits", "");
    let (outcome, run) = lab.run(source);
    assert!(!outcome.failed);
    let review = &run.steps["review"];
    assert_eq!((review.status, review.report.as_deref()), (StepStatus::Done, Some("report from review")));
    assert_eq!(review.base, review.tip, "no commits");
    assert!(actions(&lab.bin_dir)[0].contains(&format!("{} note review \"<report>\"", lab.kanstack_bin().display())));

    let lab = Lab::new();
    let (outcome, run) = lab.run(source);
    assert!(!outcome.failed);
    let review = &run.steps["review"];
    assert_eq!((review.status, review.report.as_deref()), (StepStatus::Done, None));
    assert_ne!(review.base, review.tip);
    assert_eq!(lab.status_reads(), 2, "no waiting");
}

const ONE_COMMITS_STEP: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n";

/// Branch `a` off `main` with one commit of its own on it, as an earlier step would leave it.
fn branch_with_work(lab: &Lab) -> String {
    git(&lab.repo, &["checkout", "-q", "-b", "a"]);
    fs::write(lab.repo.join("earlier.txt"), "earlier work\n").unwrap();
    git(&lab.repo, &["add", "earlier.txt"]);
    git(&lab.repo, &["commit", "-q", "-m", "earlier work"]);
    git(&lab.repo, &["checkout", "-q", "main"]);
    tip(&lab.repo, "a")
}

fn contains_commit(lab: &Lab, branch: &str, sha: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(&lab.repo)
        .args(["merge-base", "--is-ancestor", sha, &format!("refs/heads/{branch}")])
        .status()
        .unwrap()
        .success()
}

/// GitButler restacks branches routinely, which moves a branch's tip with nothing new on it. A
/// `commits` step isn't finished by that: it keeps waiting until a commit with a patch id the
/// branch didn't have when the step started lands.
#[test]
fn a_rebase_with_no_new_commit_keeps_a_commits_step_waiting() {
    let lab = Lab::new();
    let before = branch_with_work(&lab);
    lab.knob("rebase_at", "2");
    lab.knob("commit_at", "4");
    let (outcome, run) = lab.run(ONE_COMMITS_STEP);
    assert!(!outcome.failed);

    // Read 2 rebased the branch and was a trusted idle; it didn't count as commits.
    let a = &lab.snapshot(3).steps["a"];
    assert_eq!((a.status, a.waiting_on), (StepStatus::Running, Some(DoneWhen::Commits)), "{a:?}");
    let a = &lab.snapshot(4).steps["a"];
    assert_eq!((a.status, a.waiting_on), (StepStatus::Running, Some(DoneWhen::Commits)), "still waiting: {a:?}");
    assert!(!contains_commit(&lab, "a", &before), "the earlier commit was rewritten by the rebase");
    assert_eq!(lab.status_reads(), 4, "done on the read the real commit landed");

    let a = &run.steps["a"];
    assert_eq!(a.status, StepStatus::Done);
    assert_eq!(a.base.as_deref(), Some(before.as_str()));
    assert_eq!(a.base_patches.as_ref().map(Vec::len), Some(1), "the earlier commit's patch id: {a:?}");
}

/// A rebase that carries a real new commit along with the old ones finishes the step on the
/// read it happens.
#[test]
fn a_rebase_with_a_new_commit_finishes_a_commits_step() {
    let lab = Lab::new();
    let before = branch_with_work(&lab);
    lab.knob("rebase_at", "2");
    lab.knob("commit_at", "2");
    let (outcome, run) = lab.run(ONE_COMMITS_STEP);
    assert!(!outcome.failed);
    assert!(!contains_commit(&lab, "a", &before), "the branch was rebased");
    assert_eq!(lab.status_reads(), 2, "done on the first trusted idle");
    assert_eq!(run.steps["a"].status, StepStatus::Done);
}

/// With no workspace target to take a merge base with (and so no patch ids recorded, as in a
/// run from before them), a step falls back to its tip having moved from its `base`, which a
/// rebase alone satisfies.
#[test]
fn without_patch_ids_a_step_falls_back_to_comparing_tips() {
    let lab = Lab::new();
    git(&lab.repo, &["config", "--unset", "gitbutler.project.targetref"]);
    branch_with_work(&lab);
    lab.knob("rebase_at", "2");
    lab.knob("no_commits", "");
    let (outcome, run) = lab.run(ONE_COMMITS_STEP);
    assert!(!outcome.failed);
    assert_eq!(lab.status_reads(), 2);
    let a = &run.steps["a"];
    assert_eq!((a.status, a.base_patches.clone()), (StepStatus::Done, None));
}
