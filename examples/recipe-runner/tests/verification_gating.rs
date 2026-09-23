//! End-to-end tests of `run_recipe` against a stand-in `kanstack` binary — a small shell
//! script that records every call it receives and reports every branch it has seen `spawn`ed
//! as `idle` on the very first `status --json` poll, so these tests run in milliseconds and
//! never touch a real `kanstack`, a real multiplexer, or a real coding agent.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kanstack_recipe_runner::graph::StepState;
use kanstack_recipe_runner::kanstack::KanstackClient;
use kanstack_recipe_runner::recipe;
use kanstack_recipe_runner::run::{run_recipe, RunOptions};

/// Writes an executable fake `kanstack` into `dir`. `fail_spawn_branch`, when set, makes
/// `spawn` on that exact branch return `ok:false` (a `workstream_exists` conflict) instead of
/// succeeding; every other branch spawns/sends successfully. Every invocation is appended,
/// one line per call, to `calls.log` next to the script, and every branch that was `spawn`ed
/// successfully is remembered in `branches.log` so `status --json` can report it `idle`.
fn write_fake_kanstack(dir: &Path, fail_spawn_branch: Option<&str>) -> PathBuf {
    let script_path = dir.join("kanstack");
    let fail_check = match fail_spawn_branch {
        Some(b) => format!(
            r#"if [ "$branch" = "{b}" ]; then
    printf '{{"schema":1,"ok":false,"command":"spawn","error":{{"code":"workstream_exists","message":"forced failure for test"}}}}\n'
    exit 4
  fi
"#
        ),
        None => String::new(),
    };

    let script = format!(
        r#"#!/bin/sh
set -e
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cmd="$1"
shift

case "$cmd" in
  spawn)
    branch="$1"
    echo "spawn $*" >> "$DIR/calls.log"
    {fail_check}
    echo "$branch" >> "$DIR/branches.log"
    printf '{{"schema":1,"ok":true,"command":"spawn","workstream":"%s","result":{{"created":true,"pane":"%%1","agent":"codex","workspace":null,"model":null,"effort":null}}}}\n' "$branch"
    ;;
  send)
    target="$1"
    echo "send $*" >> "$DIR/calls.log"
    printf '{{"schema":1,"ok":true,"command":"send","workstream":"%s","result":{{"pane":"%%1"}}}}\n' "$target"
    ;;
  status)
    printf '{{"schema":1,"workstreams":['
    first=1
    if [ -f "$DIR/branches.log" ]; then
      while IFS= read -r b; do
        if [ "$first" != "1" ]; then printf ','; fi
        first=0
        printf '{{"branch":"%s","pane":"%%1","agent":"codex","item":null,"status":"idle","lane":null}}' "$b"
      done < "$DIR/branches.log"
    fi
    printf '],"workspace":null,"workspace_blocked":null}}\n'
    ;;
  *)
    echo "fake kanstack: unsupported command: $cmd" >&2
    exit 2
    ;;
esac
"#,
        fail_check = fail_check,
    );

    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

fn calls_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

fn fast_opts() -> RunOptions {
    RunOptions {
        poll_interval: Duration::from_millis(5),
    }
}

#[test]
fn no_verify_completes_as_soon_as_idle() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run_recipe(&recipe, &client, &fast_opts());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Complete);
    assert!(calls_log(tmp.path()).contains("spawn a "));
}

#[test]
fn passing_verify_completes_the_step() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"true\"\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run_recipe(&recipe, &client, &fast_opts());

    assert!(!outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Complete);
}

#[test]
fn failing_verify_fails_the_step_and_blocks_its_dependents() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"false\"\n  b:\n    agent: codex\n    needs: [a]\n    prompt: Do B\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run_recipe(&recipe, &client, &fast_opts());

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
    let bin = write_fake_kanstack(tmp.path(), Some("a"));
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n  b:\n    agent: codex\n    prompt: Do B\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run_recipe(&recipe, &client, &fast_opts());

    assert!(outcome.failed);
    assert_eq!(outcome.states["a"], StepState::Failed);
    // b is independent of a (no `needs`), so it still runs to completion.
    assert_eq!(outcome.states["b"], StepState::Complete);
}

#[test]
fn on_places_dependent_step_on_the_same_branch_via_send() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_kanstack(tmp.path(), None);
    let recipe = recipe::parse(
        "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: Review\n---\nctx\n",
    )
    .unwrap();

    let client = KanstackClient::new(bin);
    let outcome = run_recipe(&recipe, &client, &fast_opts());

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
    let acceptance = Path::new(repo_root).join("../recipes/implement-review-docs.md");
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
