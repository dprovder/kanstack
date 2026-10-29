//! The stand-in `kanstack` every integration test drives: a shell script that records each
//! call, keeps a list of open lanes, and plays the agents' part in a scratch git repository,
//! so the tests run in milliseconds and never touch a real `kanstack`, multiplexer or agent.

#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use kanstack_recipe::run::RunOptions;

/// Writes an executable fake `kanstack` into `dir`. Every knob is a file next to it:
///
/// - Every call is appended to `calls.log`, one line each, a prompt's newlines flattened to
///   spaces. `status` is logged as exactly `status`.
/// - `spawn <branch>` fails with `workstream_exists` when a lane is already open on `<branch>`
///   or `fail_spawn_branch` names it. Otherwise it creates `refs/heads/<branch>` in the
///   repository named by `repo_path` (at `--above`'s branch, else at `HEAD`) if it's missing,
///   opens a lane (a line in `lanes`) and queues the agent's turn: its commit (a line in
///   `pending_commits`) and its note (a line in `pending_notes`).
///   `crash_before_spawn`/`crash_after_spawn` are removed and the caller is killed with
///   `kill -9`, before or after the lane comes up.
/// - `send <branch>` queues the agent's turn; `stop <branch>` closes the lane, or fails with
///   `unknown_workstream` when there's none.
/// - The Nth `status` call fails while `status_fail_remaining` counts down. Otherwise it first
///   copies the file `snapshot_path` names to `snapshot.N.json`. Once N reaches `commit_at`
///   (default 1) it lands the queued commits, one on each branch, unless `no_commits` exists;
///   once N reaches `note_at` (default 1) it appends a `kind: "note"` event about each queued
///   branch to `events.jsonl`, if `notes` exists. Then it prints `status.N.json` if the test
///   wrote one, else every lane `idle` on pane `%7`.
/// - `events [--since <offset>]` prints `events.jsonl` from that byte offset, as kanstack does.
pub fn write_fake_kanstack(dir: &Path) -> PathBuf {
    let script_path = dir.join("kanstack");
    let script = r#"#!/bin/sh
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cmd="$1"
shift

flat() { printf '%s' "$*" | tr '\n' ' '; }
g() { git -C "$(cat "$DIR/repo_path")" -c user.name=t -c user.email=t@t -c commit.gpgsign=false "$@"; }

# The agent's commit on branch $1.
commit() {
  if [ -f "$DIR/repo_path" ] && [ ! -f "$DIR/no_commits" ]; then
    if tip=$(g rev-parse --verify --quiet "refs/heads/$1"); then
      new=$(g commit-tree -p "$tip" -m "work on $1" "$tip^{tree}")
      g update-ref "refs/heads/$1" "$new"
    fi
  fi
}

# The agent's `kanstack note` about branch $1.
note() {
  if [ -f "$DIR/notes" ]; then
    printf '{"schema":1,"ts":"2026-10-28T00:00:00Z","branch":"%s","kind":"note","text":"report from %s"}\n' "$1" "$1" >> "$DIR/events.jsonl"
  fi
}

# Plays queue $1 with function $2 once this is status call $3 >= threshold file $4's (or 1).
play() {
  if [ "$3" -ge "$(cat "$DIR/$4" 2>/dev/null || echo 1)" ] && [ -f "$DIR/$1" ]; then
    while IFS= read -r b; do "$2" "$b" < /dev/null; done < "$DIR/$1"
    rm "$DIR/$1"
  fi
}

queue() {
  echo "$1" >> "$DIR/pending_commits"
  echo "$1" >> "$DIR/pending_notes"
}

case "$cmd" in
  spawn)
    branch="$1"
    echo "spawn $(flat "$*")" >> "$DIR/calls.log"
    above=""
    agent=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --above) above="$2"; shift ;;
        --agent) agent="$2"; shift ;;
      esac
      shift
    done
    if [ "$(cat "$DIR/fail_spawn_branch" 2>/dev/null)" = "$branch" ] || grep -qx "$branch" "$DIR/lanes" 2>/dev/null; then
      printf '{"schema":1,"ok":false,"command":"spawn","error":{"code":"workstream_exists","message":"a lane is already open on %s"}}\n' "$branch"
      exit 4
    fi
    if [ -f "$DIR/crash_before_spawn" ]; then
      rm "$DIR/crash_before_spawn"
      kill -9 "$PPID"
      exit 1
    fi
    if [ -f "$DIR/repo_path" ] && ! g rev-parse --verify --quiet "refs/heads/$branch" >/dev/null; then
      from=HEAD
      if [ -n "$above" ]; then from="refs/heads/$above"; fi
      g update-ref "refs/heads/$branch" "$(g rev-parse "$from")"
    fi
    echo "$branch" >> "$DIR/lanes"
    queue "$branch"
    if [ -f "$DIR/crash_after_spawn" ]; then
      rm "$DIR/crash_after_spawn"
      kill -9 "$PPID"
      exit 1
    fi
    printf '{"schema":1,"ok":true,"command":"spawn","workstream":"%s","result":{"created":true,"pane":"%%7","agent":"%s","workspace":null,"model":null,"effort":null,"advisory":false}}\n' "$branch" "$agent"
    ;;
  send)
    echo "send $(flat "$*")" >> "$DIR/calls.log"
    queue "$1"
    printf '{"schema":1,"ok":true,"command":"send","workstream":"%s","result":{"pane":"%%7"}}\n' "$1"
    ;;
  stop)
    echo "stop $*" >> "$DIR/calls.log"
    if ! grep -qx "$1" "$DIR/lanes" 2>/dev/null; then
      printf '{"schema":1,"ok":false,"command":"stop","error":{"code":"unknown_workstream","message":"no workstream %s"}}\n' "$1"
      exit 3
    fi
    grep -vx "$1" "$DIR/lanes" > "$DIR/lanes.tmp"
    mv "$DIR/lanes.tmp" "$DIR/lanes"
    printf '{"schema":1,"ok":true,"command":"stop","workstream":"%s","result":{"pane":"%%7"}}\n' "$1"
    ;;
  status)
    echo "status" >> "$DIR/calls.log"
    remaining=$(cat "$DIR/status_fail_remaining" 2>/dev/null || echo 0)
    if [ "$remaining" -gt 0 ]; then
      echo $((remaining - 1)) > "$DIR/status_fail_remaining"
      printf '{"schema":1,"ok":false,"command":"status","error":{"code":"internal","message":"forced transient failure for test"}}\n'
      exit 1
    fi
    n=$(( $(cat "$DIR/status_count" 2>/dev/null || echo 0) + 1 ))
    echo "$n" > "$DIR/status_count"
    if [ -f "$DIR/snapshot_path" ]; then
      cp "$(cat "$DIR/snapshot_path")" "$DIR/snapshot.$n.json" 2>/dev/null
    fi
    play pending_commits commit "$n" commit_at
    play pending_notes note "$n" note_at
    if [ -f "$DIR/status.$n.json" ]; then
      cat "$DIR/status.$n.json"
      exit 0
    fi
    printf '{"schema":1,"workstreams":['
    first=1
    if [ -f "$DIR/lanes" ]; then
      while IFS= read -r b; do
        if [ "$first" != "1" ]; then printf ','; fi
        first=0
        printf '{"branch":"%s","pane":"%%7","agent":"codex","item":null,"status":"idle","lane":null}' "$b"
      done < "$DIR/lanes"
    fi
    printf '],"workspace":null,"workspace_blocked":null}\n'
    ;;
  events)
    echo "events $*" >> "$DIR/calls.log"
    since=0
    while [ $# -gt 0 ]; do
      case "$1" in
        --since) since="$2"; shift ;;
      esac
      shift
    done
    if [ -f "$DIR/events.jsonl" ]; then
      tail -c +$((since + 1)) "$DIR/events.jsonl"
    fi
    ;;
  *)
    echo "fake kanstack: unsupported command: $cmd" >&2
    exit 2
    ;;
esac
"#;
    fs::write(&script_path, script).unwrap();
    let mut perms = fs::metadata(&script_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script_path, perms).unwrap();
    script_path
}

/// Runs git in `repo` and returns its trimmed stdout, failing the test if git does.
pub fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Makes `repo` a git repository with one commit on `main`, and points the fake in `bin_dir`
/// at it.
pub fn init_repo(repo: &Path, bin_dir: &Path) {
    fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    fs::write(bin_dir.join("repo_path"), repo.to_str().unwrap()).unwrap();
}

/// `refs/heads/<branch>`'s commit in `repo`.
pub fn tip(repo: &Path, branch: &str) -> String {
    git(repo, &["rev-parse", &format!("refs/heads/{branch}")])
}

/// A `status.N.json` reporting `branch` with lane status `status`.
pub fn lane_status(dir: &Path, n: u32, branch: &str, status: &str) {
    fs::write(
        dir.join(format!("status.{n}.json")),
        format!(r#"{{"schema":1,"workstreams":[{{"branch":"{branch}","pane":"%7","agent":"codex","item":null,"status":"{status}","lane":null}}],"workspace":null,"workspace_blocked":null}}"#),
    )
    .unwrap();
}

pub fn calls_log(dir: &Path) -> String {
    fs::read_to_string(dir.join("calls.log")).unwrap_or_default()
}

/// `calls.log`'s lines that aren't `status` or `events` reads: just the launches and stops.
pub fn actions(dir: &Path) -> Vec<String> {
    calls_log(dir)
        .lines()
        .filter(|l| *l != "status" && !l.starts_with("events"))
        .map(String::from)
        .collect()
}

pub fn fast_opts() -> RunOptions {
    RunOptions {
        poll_interval: Duration::from_millis(5),
    }
}
