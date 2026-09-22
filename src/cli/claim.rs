//! `kanstack claim` — the `PreToolUse` hook Claude Code runs before an `Edit`/`Write`/
//! `MultiEdit` (see `crate::harness::Claude::status_hooks`), reading its JSON payload from
//! stdin and deciding whether the tool call may proceed. The preventive half of the
//! cross-lane collision problem docs/automation.md's "Concurrency guarantees" section
//! describes — vetoing a colliding edit *before* it lands, rather than reconciling the fused
//! hunks after the fact (issue #9's job). See `crate::claims` for the on-disk claim itself;
//! this module is just the decision Claude Code's hook contract wraps around it.
//!
//! ## The hook payload
//!
//! Confirmed against Claude Code's own hooks documentation: `PreToolUse` gets a JSON object on
//! stdin with (among other fields this ignores) `tool_name` and `tool_input`, and for
//! `Edit`/`Write`/`MultiEdit`, `tool_input.file_path` is the file being touched — the one
//! field [`file_path_from_hook_payload`] reads.
//!
//! ## The decision
//!
//! To deny, this prints Claude's own `PreToolUse` decision JSON on stdout and exits `0`:
//!
//! ```text
//! {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",
//!  "permissionDecisionReason":"..."}}
//! ```
//!
//! Deliberately the JSON form, not exit code `2`: Claude Code's docs are explicit that exit
//! code `2` blocks a `PreToolUse` tool call *unconditionally*, "whether or not you print JSON"
//! — the one exit code that can't be second-guessed by anything else the process does. That
//! makes it exactly the wrong tool here: everything in [`run`] is built to fail open (see
//! below), and a codepath that can turn an unrelated bug into an unconditional block is the
//! one thing fail-open cannot tolerate. The JSON form has no such override — malformed JSON,
//! a timeout, or any exit code other than `2` is *always* non-blocking, so a broken decision
//! can only ever fail toward allowing the edit, never toward blocking one that shouldn't be.
//!
//! To allow, this prints nothing and exits `0` — Claude Code's documented behavior for "the
//! hook has no decision" is to fall through to the normal permission flow, i.e. the edit
//! proceeds exactly as if this hook didn't exist. Same convention `crate::report`'s hooks
//! already use.
//!
//! ## Fail open, unconditionally
//!
//! [`run`] never returns an `Err`. Every other headless subcommand's failure goes through
//! [`super::exit::dispatch`]'s generic `--json` envelope and exit-code table — but
//! `ErrorCode::InvalidArguments` maps to exit code `2`, and (per the above) that is the one
//! exit code Claude Code's `PreToolUse` handling treats as an unconditional block, regardless
//! of what this process actually printed. Letting *any* internal condition here — a missing
//! branch, unreadable stdin, a broken claims directory — surface as a normal error would risk
//! that error's exit code accidentally colliding with the one code that blocks no matter what.
//! So every failure path below is absorbed locally into "allow" instead: the same
//! "best-effort, never break the thing it's attached to" spirit as `crate::report`/
//! `crate::events`, just enforced by construction rather than by convention, because the
//! consequence of getting it wrong here is not a missed status update — it's every future edit
//! silently refusing to land.
//!
//! ## Exact file path only
//!
//! Matching is on `file_path` alone — no line-range or region-level granularity. Two lanes
//! editing different parts of the same large file are still blocked from each other, a false
//! positive this v1 accepts: a file mid-edit has no stable notion of "region" to diff against
//! another lane's in-flight edit, so getting narrower than "the whole file" is a much harder
//! problem, out of scope here.

use std::io::{Read, Write};
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;

use crate::claims::Claims;
use crate::mux::pane_status::PaneStatus;
use crate::report::Reports;

pub(super) fn run(branch: Option<String>, cwd: &Path, out: &mut impl Write) -> Result<()> {
    // See the module doc's "Fail open, unconditionally" — nothing below this point ever
    // becomes an `Err`; a condition this subcommand can't make sense of just means there is
    // nothing to check, so the tool call proceeds.
    let Some(branch) = own_branch(branch) else { return Ok(()) };
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        return Ok(());
    }
    let Some(file_path) = file_path_from_hook_payload(&raw) else { return Ok(()) };
    decide(&branch, &file_path, &Claims::for_repo(cwd), &Reports::for_repo(cwd), SystemTime::now(), out);
    Ok(())
}

/// `branch`, or `$KANSTACK_BRANCH` when the hook command (see `Harness::status_hooks`) didn't
/// bake one in — same fallback `kanstack report` uses, except a missing branch here just means
/// "allow", not an error: see the module doc.
fn own_branch(branch: Option<String>) -> Option<String> {
    branch.or_else(|| std::env::var("KANSTACK_BRANCH").ok()).filter(|b| !b.is_empty())
}

/// The `tool_input.file_path` a `PreToolUse` payload names, or `None` for anything that isn't
/// shaped like one — malformed JSON, or a tool call with no `file_path` (shouldn't happen,
/// since the hook's own matcher is `Edit|Write|MultiEdit`, but this is read from a live
/// process's stdin, not trusted input).
fn file_path_from_hook_payload(raw: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value.get("tool_input")?.get("file_path")?.as_str().map(str::to_string)
}

/// The actual claim/collision logic, split out from [`run`] so it can be exercised directly
/// against a `Claims`/`Reports` pair instead of real stdin and `$KANSTACK_BRANCH` — see the
/// tests below.
///
/// Blocks only when *all* of: another branch holds a claim on `file_path`, that claim is still
/// fresh (see `crate::claims::FRESH_FOR`), and that branch's own current report state (via
/// `crate::report::Reports`, not merely the claim's own freshness) is `Busy` — a fresh claim
/// from a branch that has since gone idle, or whose report has itself expired, is not a live
/// collision. Otherwise, takes (or renews) `branch`'s own claim and allows.
fn decide(branch: &str, file_path: &str, claims: &Claims, reports: &Reports, now: SystemTime, out: &mut impl Write) {
    if let Some(holder) = claims.holder(file_path, now) {
        if holder != branch && reports.status(&holder, now) == Some(PaneStatus::Busy) {
            deny(&holder, file_path, out);
            return;
        }
    }
    // Best-effort, like every other write here: a claim that fails to record just means the
    // next call re-derives it from scratch, not that this edit is refused.
    let _ = claims.claim(file_path, branch, now);
}

fn deny(holder: &str, file_path: &str, out: &mut impl Write) {
    let decision = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!(
                "kanstack: {holder} is already editing {file_path} in another lane. Retry \
                 once it moves on, ask it to hand the file off, or edit something else."
            ),
        }
    });
    if let Ok(line) = serde_json::to_string(&decision) {
        let _ = writeln!(out, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn scratch(tag: &str) -> (Claims, Reports) {
        let dir = std::env::temp_dir().join(format!("kanstack-claim-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (Claims::in_dir(dir.join("claims")), Reports::in_dir(dir.join("reports")))
    }

    // Hook-payload parsing.

    #[test]
    fn file_path_from_hook_payload_reads_the_documented_field() {
        let raw = r#"{
            "session_id": "abc123",
            "transcript_path": "/home/user/.claude/projects/x/y.jsonl",
            "cwd": "/home/user/my-project",
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": {"file_path": "/path/to/file.ts", "old_string": "a", "new_string": "b"},
            "tool_use_id": "toolu_01ABC123"
        }"#;
        assert_eq!(file_path_from_hook_payload(raw).as_deref(), Some("/path/to/file.ts"));
    }

    #[test]
    fn file_path_from_hook_payload_handles_write_and_multiedit_shapes_too() {
        let write = r#"{"tool_name":"Write","tool_input":{"file_path":"/a/b.rs","content":"..."}}"#;
        assert_eq!(file_path_from_hook_payload(write).as_deref(), Some("/a/b.rs"));
        let multi = r#"{"tool_name":"MultiEdit","tool_input":{"file_path":"/a/c.rs","edits":[]}}"#;
        assert_eq!(file_path_from_hook_payload(multi).as_deref(), Some("/a/c.rs"));
    }

    #[test]
    fn file_path_from_hook_payload_is_none_for_anything_unrecognized() {
        assert_eq!(file_path_from_hook_payload("{ not json"), None);
        assert_eq!(file_path_from_hook_payload(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#), None, "no file_path");
        assert_eq!(file_path_from_hook_payload(r#"{"tool_input":{"file_path": 5}}"#), None, "file_path must be a string");
        assert_eq!(file_path_from_hook_payload(""), None);
    }

    // The decision itself.

    #[test]
    fn an_unclaimed_file_is_allowed_and_claimed() {
        let (claims, reports) = scratch("unclaimed");
        let mut out = Vec::new();
        decide("feat-a", "/repo/src/lib.rs", &claims, &reports, at(1000), &mut out);
        assert!(out.is_empty(), "allow prints nothing");
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1001)), Some("feat-a".to_string()));
    }

    #[test]
    fn a_second_tool_call_from_the_same_branch_is_always_allowed() {
        let (claims, reports) = scratch("same-branch");
        reports.write("feat-a", crate::report::Reported::Busy, at(999)).unwrap();
        decide("feat-a", "/repo/src/lib.rs", &claims, &reports, at(1000), &mut Vec::new());
        let mut out = Vec::new();
        decide("feat-a", "/repo/src/lib.rs", &claims, &reports, at(1001), &mut out);
        assert!(out.is_empty(), "a branch never blocks its own claim");
    }

    /// The core scenario: another lane holds a fresh claim on the exact file, and is reporting
    /// itself busy — the tool call must be denied, with Claude's own decision JSON.
    #[test]
    fn a_fresh_claim_from_a_busy_branch_denies_the_call() {
        let (claims, reports) = scratch("deny");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        reports.write("feat-a", crate::report::Reported::Busy, at(1000)).unwrap();

        let mut out = Vec::new();
        decide("feat-b", "/repo/src/lib.rs", &claims, &reports, at(1001), &mut out);

        let printed = String::from_utf8(out).unwrap();
        let decision: serde_json::Value = serde_json::from_str(printed.trim_end()).unwrap();
        assert_eq!(decision["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(decision["hookSpecificOutput"]["permissionDecision"], "deny");
        let reason = decision["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains("feat-a") && reason.contains("/repo/src/lib.rs"), "{reason}");
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1002)), Some("feat-a".to_string()), "the denied branch never took the claim");
    }

    /// A fresh claim from a branch that has since gone idle (or was never busy) is not a live
    /// collision — the report state, not just the claim's own freshness, is what makes this a
    /// veto rather than a permanent lock on a file someone touched once.
    #[test]
    fn a_fresh_claim_from_a_branch_that_is_not_busy_does_not_block() {
        let (claims, reports) = scratch("idle-holder");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        reports.write("feat-a", crate::report::Reported::Idle, at(1000)).unwrap();

        let mut out = Vec::new();
        decide("feat-b", "/repo/src/lib.rs", &claims, &reports, at(1001), &mut out);
        assert!(out.is_empty(), "an idle holder's claim does not block");
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1002)), Some("feat-b".to_string()), "feat-b took the claim instead");
    }

    /// Same idea when the holder never reported anything at all.
    #[test]
    fn a_fresh_claim_from_a_branch_with_no_report_does_not_block() {
        let (claims, reports) = scratch("no-report");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        let mut out = Vec::new();
        decide("feat-b", "/repo/src/lib.rs", &claims, &reports, at(1001), &mut out);
        assert!(out.is_empty());
    }

    /// Once a claim ages out (`crate::claims::FRESH_FOR`), it stops blocking even a genuinely
    /// busy holder — the lane is presumed to have moved on to a different file.
    #[test]
    fn an_expired_claim_does_not_block_even_a_busy_holder() {
        let (claims, reports) = scratch("expired");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        reports.write("feat-a", crate::report::Reported::Busy, at(1000)).unwrap();

        let past = at(1000 + crate::claims::FRESH_FOR.as_secs() + 1);
        let mut out = Vec::new();
        decide("feat-b", "/repo/src/lib.rs", &claims, &reports, past, &mut out);
        assert!(out.is_empty());
        assert_eq!(claims.holder("/repo/src/lib.rs", past), Some("feat-b".to_string()));
    }

    /// A claim on a different file never blocks — matching is exact-path only (see the module
    /// doc's "Exact file path only").
    #[test]
    fn a_claim_on_a_different_file_never_blocks() {
        let (claims, reports) = scratch("different-file");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        reports.write("feat-a", crate::report::Reported::Busy, at(1000)).unwrap();
        let mut out = Vec::new();
        decide("feat-b", "/repo/src/other.rs", &claims, &reports, at(1001), &mut out);
        assert!(out.is_empty());
    }

    // Fail-open behavior of `own_branch` and the payload parser feeding `run`.

    #[test]
    fn own_branch_falls_back_to_the_environment_and_treats_empty_as_missing() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("KANSTACK_BRANCH");
        assert_eq!(own_branch(Some("fix-login".into())), Some("fix-login".to_string()));
        assert_eq!(own_branch(None), None, "no branch anywhere");
        std::env::set_var("KANSTACK_BRANCH", "from-env");
        assert_eq!(own_branch(None), Some("from-env".to_string()));
        std::env::set_var("KANSTACK_BRANCH", "");
        assert_eq!(own_branch(None), None, "an empty environment variable is treated as unset");
        std::env::remove_var("KANSTACK_BRANCH");
    }
}
