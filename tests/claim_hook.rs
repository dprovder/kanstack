//! End-to-end checks of `kanstack claim` — the `PreToolUse`/`BeforeTool` hook contract (see
//! `src/cli/claim.rs`'s module doc) — against the compiled binary, the same style
//! `tests/cli_exit.rs` uses for the rest of the headless subcommands. Real subprocesses, real
//! stdin, no `but` needed (like `report`, `claim` never touches the workstream registry or a
//! multiplexer), so this isn't `#[ignore]`d.
//!
//! This is the closest thing to the two-agents-collide scenario `docs/automation.md`'s
//! "Concurrency guarantees" section describes that's deterministic and scriptable: two real
//! `kanstack claim` invocations, standing in for two lanes' `PreToolUse`/`BeforeTool` hooks
//! firing against the same file, driven by real `kanstack report` calls in between rather than
//! a live harness. Most scenarios below are exercised once, with Claude Code's payload shape;
//! `a_gemini_lane_is_denied_in_geminis_own_decision_shape_while_the_first_is_busy` is the one
//! that also exercises Gemini's, since the two harnesses share every code path except the
//! final decision JSON (see `cli::claim::DecisionShape`).

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("kanstack-claim-hook-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Sandbox { root }
    }

    fn kanstack(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_kanstack"))
            .args(["-C", self.root.to_str().unwrap()])
            .args(args)
            .env("KANSTACK_STATE_PATH", self.root.join("state"))
            .output()
            .expect("kanstack must run")
    }

    /// `kanstack claim <branch>`, with `payload` piped in on stdin as a `PreToolUse` hook
    /// would send it.
    fn claim(&self, branch: &str, payload: &str) -> std::process::Output {
        let mut child = Command::new(env!("CARGO_BIN_EXE_kanstack"))
            .args(["-C", self.root.to_str().unwrap(), "claim", branch])
            .env("KANSTACK_STATE_PATH", self.root.join("state"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("kanstack claim must spawn");
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        child.wait_with_output().expect("kanstack claim must run")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn edit_payload(file_path: &str) -> String {
    format!(r#"{{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{{"file_path":"{file_path}","old_string":"a","new_string":"b"}}}}"#)
}

/// Gemini CLI's own `BeforeTool` shape — confirmed against its hooks reference, structurally
/// close to Claude's (`tool_name`/`tool_input.file_path`) but a different `hook_event_name`,
/// which is what `kanstack claim` actually keys its decision shape on.
fn gemini_edit_payload(file_path: &str) -> String {
    format!(r#"{{"hook_event_name":"BeforeTool","tool_name":"write_file","tool_input":{{"file_path":"{file_path}","content":"..."}}}}"#)
}

/// The core scenario: `feat-a` is busy and mid-edit of a file; `feat-b`'s hook for the exact
/// same file is denied, with Claude's own decision JSON on stdout and exit `0` (never exit
/// `2` — see `src/cli/claim.rs`'s module doc for why that would be dangerous here).
#[test]
fn a_second_lane_is_denied_while_the_first_is_busy_and_holds_the_claim() {
    let sandbox = Sandbox::new("deny");
    let file = "/repo/src/lib.rs";

    assert!(sandbox.kanstack(&["report", "busy", "feat-a"]).status.success());
    let first = sandbox.claim("feat-a", &edit_payload(file));
    assert!(first.status.success());
    assert!(first.stdout.is_empty(), "the first lane's own edit is allowed silently");

    let second = sandbox.claim("feat-b", &edit_payload(file));
    assert_eq!(second.status.code(), Some(0), "a deny is exit 0, never exit 2 — see the module doc");
    let stdout = String::from_utf8(second.stdout).unwrap();
    let decision: serde_json::Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(decision["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(decision["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().contains("feat-a"));
}

/// Once `feat-a` goes idle, its claim is released (`Reports::write` -> `Claims::release_all`,
/// see `src/report.rs`) — a lane picking up the same file afterwards is no longer blocked.
#[test]
fn releasing_the_claim_on_idle_unblocks_the_next_lane() {
    let sandbox = Sandbox::new("release-on-idle");
    let file = "/repo/src/lib.rs";

    sandbox.kanstack(&["report", "busy", "feat-a"]);
    sandbox.claim("feat-a", &edit_payload(file));
    let blocked = sandbox.claim("feat-b", &edit_payload(file));
    assert!(!blocked.stdout.is_empty(), "sanity: feat-b is blocked while feat-a is busy");

    assert!(sandbox.kanstack(&["report", "idle", "feat-a"]).status.success());

    let after_idle = sandbox.claim("feat-b", &edit_payload(file));
    assert!(after_idle.stdout.is_empty(), "feat-a's claim was released when it went idle");
}

/// Two different files never collide — matching is exact-path only.
#[test]
fn two_lanes_on_different_files_never_collide() {
    let sandbox = Sandbox::new("different-files");
    sandbox.kanstack(&["report", "busy", "feat-a"]);
    sandbox.kanstack(&["report", "busy", "feat-b"]);

    let a = sandbox.claim("feat-a", &edit_payload("/repo/src/one.rs"));
    let b = sandbox.claim("feat-b", &edit_payload("/repo/src/two.rs"));
    assert!(a.stdout.is_empty());
    assert!(b.stdout.is_empty());
}

/// Gemini's own payload shape denies with Gemini's own flat decision JSON — no
/// `hookSpecificOutput` wrapper — exercised against the real compiled binary, same as Claude's
/// equivalent scenario above.
#[test]
fn a_gemini_lane_is_denied_in_geminis_own_decision_shape_while_the_first_is_busy() {
    let sandbox = Sandbox::new("deny-gemini");
    let file = "/repo/src/lib.rs";

    assert!(sandbox.kanstack(&["report", "busy", "feat-a"]).status.success());
    let first = sandbox.claim("feat-a", &gemini_edit_payload(file));
    assert!(first.status.success());
    assert!(first.stdout.is_empty());

    let second = sandbox.claim("feat-b", &gemini_edit_payload(file));
    assert_eq!(second.status.code(), Some(0), "a deny is exit 0 for Gemini too, never exit 2");
    let stdout = String::from_utf8(second.stdout).unwrap();
    let decision: serde_json::Value = serde_json::from_str(stdout.trim_end()).unwrap();
    assert_eq!(decision["decision"], "deny");
    assert!(decision.get("hookSpecificOutput").is_none(), "flat, not Claude's wrapper: {decision:?}");
    assert!(decision["reason"].as_str().unwrap().contains("feat-a"));
}

/// A tool call with no recognizable `file_path` (malformed stdin, or a payload shaped nothing
/// like the documented one) must never block — fail open, see the module doc.
#[test]
fn malformed_stdin_fails_open() {
    let sandbox = Sandbox::new("malformed-stdin");
    let out = sandbox.claim("feat-a", "not json at all");
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

/// No branch anywhere (no argument, no `$KANSTACK_BRANCH`) must also fail open rather than
/// error — see the module doc's "Fail open, unconditionally".
#[test]
fn no_branch_anywhere_fails_open() {
    let sandbox = Sandbox::new("no-branch");
    let mut child = Command::new(env!("CARGO_BIN_EXE_kanstack"))
        .args(["-C", sandbox.root.to_str().unwrap(), "claim"])
        .env("KANSTACK_STATE_PATH", sandbox.root.join("state"))
        .env_remove("KANSTACK_BRANCH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(edit_payload("/repo/src/lib.rs").as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}
