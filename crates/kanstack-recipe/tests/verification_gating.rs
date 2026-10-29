//! End-to-end tests of `run_recipe` against the stand-in `kanstack` in `common`, which reports
//! every lane it has opened as `idle` from the very first `status --json` poll and commits each
//! agent's turn on that poll, so these tests run in milliseconds and never touch a real
//! `kanstack`, a real multiplexer, or a real coding agent.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{calls_log, fast_opts};
use kanstack_recipe::graph::StepState;
use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::recipe;
use kanstack_recipe::run::run_recipe;

/// The stand-in `kanstack` from `common` in `dir`, with its agents committing to a repository
/// at `dir/work` (the run's workdir, see [`run`]). `fail_spawn_branch`, when set, makes `spawn`
/// on that exact branch return `ok:false` (a `workstream_exists` conflict); `status_fail_count`
/// makes the first N `status --json` calls fail with `ok:false` before status starts
/// succeeding, to simulate a transient (or persistent, if N is large) failure reading status.
///
/// Every call is logged in `calls.log`, `status` included, so a test can see how many polls
/// happened between two other calls — that ordering is what proves two steps sharing a
/// workstream were serialized rather than both launched in the same tick.
fn write_fake_kanstack(dir: &Path, fail_spawn_branch: Option<&str>, status_fail_count: u32) -> PathBuf {
    let bin = common::write_fake_kanstack(dir);
    if let Some(branch) = fail_spawn_branch {
        fs::write(dir.join("fail_spawn_branch"), branch).unwrap();
    }
    if status_fail_count > 0 {
        fs::write(dir.join("status_fail_remaining"), status_fail_count.to_string()).unwrap();
    }
    common::init_repo(&dir.join("work"), dir);
    bin
}

/// `run_recipe` in the repository [`write_fake_kanstack`] set up in `dir`.
fn run(recipe: &recipe::Recipe, client: &KanstackClient, dir: &Path) -> kanstack_recipe::run::RunOutcome {
    run_recipe(recipe, client, &fast_opts(), &dir.join("work"))
}

#[test]
fn no_verify_completes_as_soon_as_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None, 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Complete);
    assert!(calls_log(tmp.path()).contains("spawn a "));
}

#[test]
fn passing_verify_completes_the_step() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None, 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"true\"\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Complete);
}

#[test]
fn failing_verify_fails_the_step_and_blocks_its_dependents() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None, 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"false\"\n  b:\n    agent: codex\n    needs: [a]\n    prompt: Do B\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Failed);
    assert_eq!(outcome.states["b"], StepState::Pending);

    let calls = calls_log(tmp.path());
    assert!(calls.contains("spawn a "));
    assert!(
        !calls.contains("spawn b "),
        "b must never be spawned once its dependency a failed verification:\n{calls}"
    );
}

#[test]
fn spawn_failure_fails_the_step_and_independent_sibling_still_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), Some("a"), 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n  b:\n    agent: codex\n    prompt: Do B\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Failed);
    // b is independent of a (no `needs`), so it still runs to completion.
    assert_eq!(outcome.states["b"], StepState::Complete);
}

/// With the same agent, an `on` step with no `spawn` continues in the existing pane (see
/// `spawn_modes.rs` for a different agent, which hands off instead).
#[test]
fn on_places_dependent_step_on_the_same_branch_via_send() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None, 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: codex\n    needs: [implement]\n    on: implement\n    prompt: Review\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["implement"], StepState::Complete);
    assert_eq!(outcome.states["review"], StepState::Complete);

    let calls = calls_log(tmp.path());
    assert!(calls.contains("spawn implement "));
    assert!(
        calls.contains("send implement "),
        "review should be sent to the implement workstream, not spawned as a new one:\n{calls}"
    );
    assert!(!calls.contains("spawn review"));
}

#[test]
fn check_smoke_test_on_acceptance_recipe_via_binary() {
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let acceptance = Path::new(repo_root).join("../../examples/recipes/implement-review-docs.md");
    assert!(
        acceptance.exists(),
        "expected {} to exist",
        acceptance.display()
    );

    let bin_path = env!("CARGO_BIN_EXE_kanstack-recipe");
    let output = std::process::Command::new(bin_path)
        .arg("check")
        .arg(&acceptance)
        .output()
        .expect("failed to run kanstack-recipe check");

    assert!(
        output.status.success(),
        "check should succeed on the acceptance recipe: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("recipe valid"));
}

/// A step's workstream reading `idle` on the very first status poll after its spawn/send must
/// not be trusted as done — kanstack's own `PaneStatus::Idle` doc comment
/// (`src/mux/pane_status.rs` in the parent crate) says a single poll can't tell "returned to
/// rest" apart from "never got a chance to start." The fake kanstack here reports every
/// spawned branch idle from its very first status call (same as the other tests), so this
/// asserts on `calls.log` directly: the step must not complete after only one status read
/// since launch — exactly two are required.
#[test]
fn idle_is_not_trusted_on_the_first_status_read_after_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None, 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Complete);

    let calls = calls_log(tmp.path());
    let status_reads = calls.lines().filter(|l| *l == "status").count();
    assert_eq!(
        status_reads, 2,
        "a should complete on its second status read, not its first, since a single idle \
         reading can't tell \"finished\" apart from \"never started\":\n{calls}"
    );
}

/// Two independent siblings (`b`, `c`) both placed `on: implement` become runnable in the same
/// tick once `implement` completes. Before the fix, both were `send` in that same tick and
/// then both marked complete off a single `status --json` observation of `implement` idle —
/// indistinguishable from either message's turn actually finishing. The fix requires a `send`
/// to a branch that's already claimed by a running step to wait for a later tick, so the two
/// sends must be separated by at least one `status` poll (the poll that observes `b`'s turn
/// finish and frees the branch for `c`).
#[test]
fn siblings_sharing_one_on_target_complete_one_at_a_time() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None, 0);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  b:\n    agent: codex\n    needs: [implement]\n    on: implement\n    prompt: SIBLING_B_MARKER\n  c:\n    agent: codex\n    needs: [implement]\n    on: implement\n    prompt: SIBLING_C_MARKER\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["implement"], StepState::Complete);
    assert_eq!(outcome.states["b"], StepState::Complete);
    assert_eq!(outcome.states["c"], StepState::Complete);

    let calls = calls_log(tmp.path());
    // Both b and c send to the *same* branch (implement), never spawning a workstream of
    // their own.
    assert_eq!(calls.matches("spawn implement ").count(), 1);
    assert!(!calls.contains("spawn b"));
    assert!(!calls.contains("spawn c"));

    let lines: Vec<&str> = calls.lines().collect();
    let b_send = lines
        .iter()
        .position(|l| l.starts_with("send implement") && l.contains("SIBLING_B_MARKER"))
        .expect("b's send call should be logged");
    let c_send = lines
        .iter()
        .position(|l| l.starts_with("send implement") && l.contains("SIBLING_C_MARKER"))
        .expect("c's send call should be logged");

    let (first_send, second_send) = if b_send < c_send {
        (b_send, c_send)
    } else {
        (c_send, b_send)
    };
    let status_calls_between = lines[first_send + 1..second_send]
        .iter()
        .filter(|l| **l == "status")
        .count();
    assert!(
        status_calls_between >= 1,
        "the two sibling `send` calls must be separated by at least one `status` poll \
         (the one that observes the first sibling finish and frees the branch), not fired \
         back-to-back in the same tick:\n{calls}"
    );
}

/// A single failed `status --json` call is a transient hiccup in reading state, not a failure
/// of the step itself — it must not fail every currently-running step outright.
#[test]
fn single_transient_status_failure_does_not_fail_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    // Fails only the very first `status --json` call; every later one succeeds.
    let bin = write_fake_kanstack(tmp.path(), None, 1);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(
        !outcome.failed,
        "one transient status failure should be retried, not treated as a step failure"
    );
    assert_eq!(outcome.states["a"], StepState::Complete);
}

/// A `status --json` that keeps failing forever is not transient — a `kanstack` that is
/// actually unreachable must still surface as a failure instead of the runner retrying
/// indefinitely and hanging.
#[test]
fn persistent_status_failures_eventually_fail_running_steps() {
    let tmp = tempfile::tempdir().unwrap();
    // Fails far more times than any reasonable retry budget, so it never recovers within
    // this test's run.
    let bin = write_fake_kanstack(tmp.path(), None, 1_000);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run(&recipe, &client, tmp.path());

    assert!(outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Failed);
}
