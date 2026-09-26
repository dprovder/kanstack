//! `kanstack intent` — set a lane's one-line intent: why it looks the way it does right now
//! ("waiting for cargo test to finish"), for `status --json`'s `"intent"` to show. Latched and
//! last-write-wins, not logged — see `crate::orchestration` for why, and for when it's dropped.
//!
//! Same shape as `report`: never reads the workstream registry, never checks that `<branch>`
//! is a registered workstream (an intent for a lane nobody tracks is just never read), and
//! prints nothing without `--json`.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde::Serialize;

use crate::orchestration::Orchestration;

use super::json_result;

/// `intent --json`'s `result`: the intent now set, echoed back.
#[derive(Debug, Serialize)]
struct IntentResult {
    intent: String,
}

pub(super) fn run(branch: String, text: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    Orchestration::for_repo(cwd).set_intent(&branch, &text, SystemTime::now())?;
    if json {
        writeln!(out, "{}", json_result("intent", branch, IntentResult { intent: text })?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::cli::test_support::*;
    use crate::cli::{parse, Command};
    use crate::orchestration::Orchestration;

    fn with_state(tag: &str, body: impl FnOnce(&Path)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("KANSTACK_STATE_PATH");
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        body(Path::new("/repo/intent"));
        match old {
            Some(v) => std::env::set_var("KANSTACK_STATE_PATH", v),
            None => std::env::remove_var("KANSTACK_STATE_PATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn intent_takes_a_branch_and_joins_the_rest_into_one_line() {
        assert_eq!(
            parse("intent", args(&["fix-login", "waiting", "for", "cargo", "test"])).unwrap(),
            Some(Command::Intent { branch: "fix-login".into(), text: "waiting for cargo test".into(), json: false })
        );
        assert_eq!(
            parse("intent", args(&["--json", "fix-login", "waiting"])).unwrap(),
            Some(Command::Intent { branch: "fix-login".into(), text: "waiting".into(), json: true })
        );
        let err = parse("intent", args(&["fix-login"])).unwrap_err().to_string();
        assert!(err.contains("needs an intent"), "{err}");
        let err = parse("intent", args(&[])).unwrap_err().to_string();
        assert!(err.contains("<branch>"), "{err}");
    }

    #[test]
    fn intent_sets_the_latched_value_replacing_the_last_and_prints_nothing() {
        with_state("intent", |repo| {
            let mut out = Vec::new();
            crate::cli::run(Command::Intent { branch: "fix-login".into(), text: "running tests".into(), json: false }, repo, &mut out).unwrap();
            crate::cli::run(Command::Intent { branch: "fix-login".into(), text: "reviewing".into(), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "stdout must stay empty: {:?}", String::from_utf8_lossy(&out));
            assert_eq!(Orchestration::for_repo(repo).intent("fix-login").as_deref(), Some("reviewing"));
            assert!(
                crate::workstream::events_path(repo).is_none_or(|p| !p.exists()),
                "an intent is a current value, not an event"
            );
        });
    }

    #[test]
    fn intent_json_echoes_the_intent_in_the_generic_envelope() {
        with_state("intent-json", |repo| {
            let mut out = Vec::new();
            crate::cli::run(Command::Intent { branch: "fix-login".into(), text: "waiting on CI".into(), json: true }, repo, &mut out).unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":true,\"command\":\"intent\",\"workstream\":\"fix-login\",\"result\":{\"intent\":\"waiting on CI\"}}\n"
            );
        });
    }
}
