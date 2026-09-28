//! `kanstack ask` / `kanstack answer` — an orchestrator setting a question on a lane that
//! wants a human's answer, and that answer clearing it. The pending ask is a latched value
//! (`crate::orchestration`), shown by `status --json` as `"pending_ask"`; both halves are also
//! logged to the events log (`crate::events`, kinds `"ask"` and `"answer"`), since a question
//! someone had to answer is worth a permanent record, not just a current value.
//!
//! Both live here, not in a file each: they're one exchange, and neither makes sense without
//! the other. Same shape as `report` otherwise — never reads the workstream registry, never
//! checks that `<branch>` is a registered workstream, prints nothing without `--json` — with
//! one deliberate exception: `answer` with nothing pending is an error (`no_pending_ask`, see
//! `super::exit`), because answering nothing is almost certainly a mistake worth saying so
//! about — a typo'd branch, or a question someone else already answered.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use serde::Serialize;

use crate::orchestration::Orchestration;

use super::exit::{tag, ErrorCode::NoPendingAsk};
use super::json_result;

/// `ask --json`'s `result`: the question now pending, echoed back.
#[derive(Debug, Serialize)]
struct AskResult {
    question: String,
}

/// `answer --json`'s `result`: the question that was answered, and the answer — so a caller
/// that answered "whatever is pending" learns what it actually answered.
#[derive(Debug, Serialize)]
struct AnswerResult {
    question: String,
    answer: String,
}

pub(super) fn run_ask(branch: String, question: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    Orchestration::for_repo(cwd).ask(&branch, &question, SystemTime::now())?;
    if json {
        writeln!(out, "{}", json_result("ask", branch, AskResult { question })?)?;
    }
    Ok(())
}

pub(super) fn run_answer(branch: String, text: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    let answered = Orchestration::for_repo(cwd)
        .answer(&branch, &text, SystemTime::now())?
        .ok_or_else(|| tag(NoPendingAsk, anyhow::anyhow!("{branch} has no pending ask to answer")))?;
    if json {
        writeln!(out, "{}", json_result("answer", branch, AnswerResult { question: answered.question, answer: text })?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::cli::test_support::*;
    use crate::cli::{dispatch, parse, Command};
    use crate::orchestration::Orchestration;

    fn with_state(tag: &str, body: impl FnOnce(&Path)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("KANSTACK_STATE_PATH");
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        body(Path::new("/repo/ask"));
        match old {
            Some(v) => std::env::set_var("KANSTACK_STATE_PATH", v),
            None => std::env::remove_var("KANSTACK_STATE_PATH"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn events(repo: &Path) -> String {
        std::fs::read_to_string(crate::workstream::events_path(repo).unwrap()).unwrap_or_default()
    }

    #[test]
    fn ask_and_answer_take_a_branch_and_join_the_rest_into_one_message() {
        assert_eq!(
            parse("ask", args(&["fix-login", "ship", "it?"])).unwrap(),
            Some(Command::Ask { branch: "fix-login".into(), question: "ship it?".into(), json: false })
        );
        assert_eq!(
            parse("answer", args(&["fix-login", "yes,", "ship", "it", "--json"])).unwrap(),
            Some(Command::Answer { branch: "fix-login".into(), text: "yes, ship it".into(), json: true })
        );
        let err = parse("ask", args(&["fix-login"])).unwrap_err().to_string();
        assert!(err.contains("needs a question"), "{err}");
        let err = parse("answer", args(&["fix-login"])).unwrap_err().to_string();
        assert!(err.contains("needs an answer"), "{err}");
        for name in ["ask", "answer"] {
            let err = parse(name, args(&[])).unwrap_err().to_string();
            assert!(err.contains("<branch>"), "{name}: {err}");
        }
    }

    #[test]
    fn ask_sets_a_pending_ask_and_logs_it_and_answer_clears_it_and_logs_that() {
        with_state("ask-answer", |repo| {
            let mut out = Vec::new();
            crate::cli::run(Command::Ask { branch: "fix-login".into(), question: "ship it?".into(), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty(), "stdout must stay empty: {:?}", String::from_utf8_lossy(&out));
            assert_eq!(Orchestration::for_repo(repo).pending_ask("fix-login").map(|a| a.question).as_deref(), Some("ship it?"));

            crate::cli::run(Command::Answer { branch: "fix-login".into(), text: "yes".into(), json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty());
            assert_eq!(Orchestration::for_repo(repo).pending_ask("fix-login"), None);

            let raw = events(repo);
            let lines: Vec<&str> = raw.lines().collect();
            assert_eq!(lines.len(), 2, "{raw}");
            assert!(lines[0].contains(r#""branch":"fix-login","kind":"ask","question":"ship it?""#), "{}", lines[0]);
            assert!(lines[1].contains(r#""branch":"fix-login","kind":"answer","text":"yes""#), "{}", lines[1]);
        });
    }

    #[test]
    fn ask_and_answer_json_print_the_generic_envelope() {
        with_state("ask-answer-json", |repo| {
            let mut out = Vec::new();
            assert_eq!(dispatch(Command::Ask { branch: "fix-login".into(), question: "ship it?".into(), json: true }, repo, &mut out, &mut Vec::new()), 0);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":true,\"command\":\"ask\",\"workstream\":\"fix-login\",\"result\":{\"question\":\"ship it?\"}}\n"
            );
            let mut out = Vec::new();
            assert_eq!(dispatch(Command::Answer { branch: "fix-login".into(), text: "yes".into(), json: true }, repo, &mut out, &mut Vec::new()), 0);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":true,\"command\":\"answer\",\"workstream\":\"fix-login\",\"result\":{\"question\":\"ship it?\",\"answer\":\"yes\"}}\n"
            );
        });
    }

    /// Answering with nothing pending — never asked, or already answered — is `no_pending_ask`,
    /// exit `3`, the "nothing to act on" bucket `no_pane` is also in; and it logs no answer.
    #[test]
    fn answer_with_no_pending_ask_is_a_no_pending_ask_error() {
        with_state("answer-none", |repo| {
            let mut out = Vec::new();
            let mut err_out = Vec::new();
            let code = dispatch(Command::Answer { branch: "fix-login".into(), text: "yes".into(), json: true }, repo, &mut out, &mut err_out);
            assert_eq!(code, 3);
            assert!(err_out.is_empty(), "json mode writes nothing to the error stream");
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "{\"schema\":1,\"ok\":false,\"command\":\"answer\",\"error\":{\"code\":\"no_pending_ask\",\
                 \"message\":\"fix-login has no pending ask to answer\"}}\n"
            );

            // Asked and answered once: a second answer finds nothing, in human mode too.
            crate::cli::run(Command::Ask { branch: "fix-login".into(), question: "ship it?".into(), json: false }, repo, &mut Vec::new()).unwrap();
            crate::cli::run(Command::Answer { branch: "fix-login".into(), text: "yes".into(), json: false }, repo, &mut Vec::new()).unwrap();
            let mut out = Vec::new();
            let mut err_out = Vec::new();
            let code = dispatch(Command::Answer { branch: "fix-login".into(), text: "yes again".into(), json: false }, repo, &mut out, &mut err_out);
            assert_eq!(code, 3);
            assert!(out.is_empty());
            assert_eq!(String::from_utf8(err_out).unwrap(), "Error: fix-login has no pending ask to answer\n");
            assert_eq!(events(repo).lines().filter(|l| l.contains(r#""kind":"answer""#)).count(), 1, "only the real answer is logged");
        });
    }
}
