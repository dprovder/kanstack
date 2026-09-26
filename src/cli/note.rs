//! `kanstack note` — an orchestrator leaving a free-text remark about a lane in the events log
//! (`crate::events`, kind `"note"`), and nowhere else: unlike `intent` or `ask` there's no
//! latched value behind it, so nothing in `status --json` changes. It's for the record — "tests
//! pass, moving on to docs" — and for anything tailing `kanstack events` to see as it happens.
//!
//! Like `report`, it never reads the workstream registry and never checks that `<branch>` is a
//! registered workstream: a note about a lane nobody is tracking is still a line in the log,
//! just one nothing will match up with. And the append itself is best-effort (see
//! `crate::events`'s module doc), so this never fails once its arguments parsed. Prints
//! nothing without `--json`, the same convention as `report` and `claim` — an orchestrator
//! calling it from inside a harness's own turn shouldn't have a confirmation line added to
//! what that harness sees.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde::Serialize;

use crate::events::EventLog;

use super::json_result;

/// `note --json`'s `result`: empty, since the note itself is all there was, and the caller
/// already has it.
#[derive(Debug, Serialize)]
struct NoteResult {}

pub(super) fn run(branch: String, text: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    EventLog::for_repo(cwd).record_note(&branch, &text, SystemTime::now());
    if json {
        writeln!(out, "{}", json_result("note", branch, NoteResult {})?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::cli::test_support::*;
    use crate::cli::{dispatch, parse, Command};
    use crate::workstream::Registry;

    fn with_state(tag: &str, body: impl FnOnce(&Path)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("KANSTACK_STATE_PATH");
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        body(Path::new("/repo/note"));
        match old {
            Some(v) => std::env::set_var("KANSTACK_STATE_PATH", v),
            None => std::env::remove_var("KANSTACK_STATE_PATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn note_takes_a_branch_and_joins_the_rest_into_one_message() {
        assert_eq!(
            parse("note", args(&["fix-login", "tests", "pass"])).unwrap(),
            Some(Command::Note { branch: "fix-login".into(), text: "tests pass".into(), json: false })
        );
        assert_eq!(
            parse("note", args(&["fix-login", "tests pass", "--json"])).unwrap(),
            Some(Command::Note { branch: "fix-login".into(), text: "tests pass".into(), json: true })
        );
        let err = parse("note", args(&["fix-login"])).unwrap_err().to_string();
        assert!(err.contains("needs a note"), "{err}");
        assert!(parse("note", args(&["fix-login", "  "])).is_err(), "blank text is no text");
        let err = parse("note", args(&[])).unwrap_err().to_string();
        assert!(err.contains("<branch>"), "{err}");
        assert!(parse("note", args(&["fix-login", "x", "--advisory"])).is_err(), "--advisory is spawn-only");
    }

    #[test]
    fn note_appends_to_the_events_log_and_prints_nothing() {
        with_state("note", |repo| {
            let mut out = Vec::new();
            crate::cli::run(Command::Note { branch: "fix-login".into(), text: "tests pass".into(), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "stdout must stay empty: {:?}", String::from_utf8_lossy(&out));
            let raw = std::fs::read_to_string(crate::workstream::events_path(repo).unwrap()).unwrap();
            assert_eq!(raw.lines().count(), 1);
            assert!(raw.contains(r#""branch":"fix-login","kind":"note","text":"tests pass""#), "{raw}");
        });
    }

    /// Through `dispatch`, as `main` runs it: the generic envelope with an empty `result`, and
    /// no lifecycle event beside the note itself.
    #[test]
    fn note_json_prints_the_generic_envelope_with_an_empty_result() {
        with_state("note-json", |repo| {
            let mut out = Vec::new();
            let code = dispatch(Command::Note { branch: "fix-login".into(), text: "hi".into(), json: true }, repo, &mut out, &mut Vec::new());
            assert_eq!(code, 0);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":true,\"command\":\"note\",\"workstream\":\"fix-login\",\"result\":{}}\n"
            );
            let raw = std::fs::read_to_string(crate::workstream::events_path(repo).unwrap()).unwrap();
            assert_eq!(raw.lines().count(), 1, "only the note: {raw}");
        });
    }

    /// A note about a lane nobody registered is still written, not refused — and a broken
    /// registry doesn't get in its way, since it's never read.
    #[test]
    fn note_needs_neither_a_registered_workstream_nor_a_readable_registry() {
        with_state("note-registry", |repo| {
            let registry = crate::workstream::state_path(repo).unwrap();
            std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
            std::fs::write(&registry, "{ not a registry").unwrap();
            assert!(Registry::load(repo).is_err(), "the fixture must really be broken");
            crate::cli::run(Command::Note { branch: "nobody-tracks-this".into(), text: "hi".into(), json: false }, repo, &mut Vec::new()).unwrap();
        });
    }
}
