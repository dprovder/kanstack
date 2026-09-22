//! `kanstack report` — the one subcommand that is run *by* a harness rather than for one, to
//! say what it's doing so `status` and the board can show it. Deliberately does not touch the
//! workstream registry at all: this runs from a harness's hooks, on every turn, and a registry
//! problem must not make it noisy or slow. And without `--json` it writes nothing to `out` —
//! Claude adds a `UserPromptSubmit` hook's stdout to what the model sees.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde::Serialize;

use crate::report::{Reported, Reports};

use super::exit::{tag, ErrorCode::InvalidArguments};
use super::json_result;

#[derive(Debug, Serialize)]
struct ReportResult {
    state: Reported,
}

pub(super) fn run(state: Reported, branch: Option<String>, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    let branch = branch
        .or_else(|| std::env::var("KANSTACK_BRANCH").ok())
        .filter(|b| !b.is_empty())
        .ok_or_else(|| {
            tag(
                InvalidArguments,
                anyhow::anyhow!(
                    "`kanstack report` needs a branch: name one, or run it where $KANSTACK_BRANCH is set \
                     (in a pane kanstack launched)"
                ),
            )
        })?;
    Reports::for_repo(cwd).write(&branch, state, SystemTime::now())?;
    if json {
        writeln!(out, "{}", json_result("report", branch, ReportResult { state })?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use crate::cli::Command;
    use crate::mux::pane_status::PaneStatus;
    use crate::report::{Reported, Reports};
    use crate::workstream::Registry;

    /// Runs `body` with `KANSTACK_STATE_PATH` pointing at a fresh directory, and `extra` set
    /// too, restoring everything afterwards. Serialized with the other tests that touch the
    /// environment.
    fn with_state(tag: &str, extra: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut vars = vec![("KANSTACK_STATE_PATH", Some(dir.to_str().unwrap()))];
        vars.extend_from_slice(extra);
        let old: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in &vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        body();
        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Claude adds a `UserPromptSubmit` hook's stdout to what the model sees, so a
    /// confirmation line here would end up in every prompt.
    #[test]
    fn report_records_what_it_was_told_and_prints_nothing() {
        with_state("report", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report");
            let mut out = Vec::new();
            crate::cli::run(Command::Report { state: Reported::Busy, branch: Some("fix-login".into()), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "stdout must stay empty: {:?}", String::from_utf8_lossy(&out));
            assert_eq!(Reports::for_repo(repo).status("fix-login", SystemTime::now()), Some(PaneStatus::Busy));

            crate::cli::run(Command::Report { state: Reported::Idle, branch: Some("fix-login".into()), json: false }, repo, &mut out).unwrap();
            assert_eq!(Reports::for_repo(repo).status("fix-login", SystemTime::now()), Some(PaneStatus::Idle));
        });
    }

    /// The hooks kanstack hands a harness are the same for every lane; which lane is speaking
    /// comes from the environment it was launched with.
    #[test]
    fn report_names_its_lane_from_the_environment_unless_told_otherwise() {
        with_state("report-env", &[("KANSTACK_BRANCH", Some("from-env"))], || {
            let repo = Path::new("/repo/report-env");
            crate::cli::run(Command::Report { state: Reported::Busy, branch: None, json: false }, repo, &mut Vec::new()).unwrap();
            crate::cli::run(Command::Report { state: Reported::Idle, branch: Some("named".into()), json: false }, repo, &mut Vec::new()).unwrap();
            let now = SystemTime::now();
            assert_eq!(Reports::for_repo(repo).status("from-env", now), Some(PaneStatus::Busy));
            assert_eq!(Reports::for_repo(repo).status("named", now), Some(PaneStatus::Idle));
        });
    }

    #[test]
    fn report_without_a_branch_anywhere_says_what_it_needs() {
        for unset in [None, Some("")] {
            with_state("report-nobranch", &[("KANSTACK_BRANCH", unset)], || {
                let err = crate::cli::run(Command::Report { state: Reported::Busy, branch: None, json: false }, Path::new("/repo/x"), &mut Vec::new())
                    .unwrap_err()
                    .to_string();
                assert!(err.contains("KANSTACK_BRANCH"), "{err}");
            });
        }
    }

    /// It runs on every turn, so a registry that has gone bad — which stops the other
    /// subcommands — must not get in its way.
    #[test]
    fn report_does_not_read_the_workstream_registry() {
        with_state("report-registry", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report-registry");
            let registry = crate::workstream::state_path(repo).unwrap();
            std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
            std::fs::write(&registry, "{ not a registry").unwrap();
            assert!(Registry::load(repo).is_err(), "the fixture must really be broken");
            crate::cli::run(Command::Report { state: Reported::Busy, branch: Some("a".into()), json: false }, repo, &mut Vec::new()).unwrap();
        });
    }

    #[test]
    fn report_json_prints_the_envelope_and_plain_report_prints_nothing() {
        with_state("report-json", &[("KANSTACK_BRANCH", None)], || {
            let repo = Path::new("/repo/report-json");
            let mut out = Vec::new();
            crate::cli::run(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()), json: true }, repo, &mut out).unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":true,\"command\":\"report\",\"workstream\":\"fix-login\",\"result\":{\"state\":\"waiting\"}}\n"
            );

            let mut out = Vec::new();
            crate::cli::run(Command::Report { state: Reported::Waiting, branch: Some("fix-login".into()), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "without --json, report still prints nothing");
        });
    }
}
