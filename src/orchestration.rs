//! What whoever is orchestrating a lane says about it: `kanstack intent` and `kanstack
//! ask`/`answer`.
//!
//! kanstack doesn't schedule or interpret anything — a human, an agent improvising turn by
//! turn, or a structured runner like `examples/recipe-runner` decides what happens next. But
//! a lane that has sat idle for ten minutes looks the same whether it's stuck or deliberately
//! waiting on something, and only the orchestrator knows which. This module is where it can
//! say so, in two conceptually separate latched values per branch:
//!
//! - an **intent** — one line of *why this lane looks the way it does right now* ("waiting
//!   for cargo test to finish"). Informational: nothing is waiting on anyone.
//! - a **pending ask** — a question that specifically wants a human's answer before the
//!   orchestrator will go on. Set by `ask`, cleared by `answer`, and kept apart from the intent
//!   so `status --json` (and, later, the board) can tell "FYI" from "needs you".
//!
//! Both are latched, last-write-wins — not appended — the same shape as `crate::report`'s
//! `Reports`: one small file per branch, named by a hash of it, replaced atomically, never
//! part of the workstream registry (which is rewritten whole, and must not be something a
//! chatty orchestrator can lose updates in or be slowed by). Unlike a report there's no
//! freshness window: an agent that crashes can't retract a `busy`, which is why reports expire,
//! but an intent or an ask is only ever set on purpose, so it holds until it's overwritten,
//! answered, or its lane is stopped or respawned (see [`Orchestration::forget`], called from
//! `crate::report::Reports::forget` — the one choke point both of those already go through).
//!
//! Nothing here checks that a branch is a registered workstream: a write for a branch nobody
//! is tracking is simply never read by anything, which is cheaper and less surprising for a
//! caller than an error would be. The permanent record — `ask` and `answer` events — lives in
//! `crate::events`, not here; this is only the current value.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::events::EventLog;
use crate::workstream::{asks_dir, fnv1a, intents_dir};

#[derive(Debug, Serialize, Deserialize)]
struct StoredIntent {
    /// Only for whoever is looking at the file; the name of the file is a hash.
    branch: String,
    text: String,
    /// Seconds since the Unix epoch. Not read back today — nothing expires an intent — but
    /// cheap to keep, and what anyone inspecting the file would want to know.
    at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredAsk {
    /// Only for whoever is looking at the file; the name of the file is a hash.
    branch: String,
    question: String,
    /// Seconds since the Unix epoch.
    at: u64,
}

/// A question a branch's orchestrator is waiting on someone to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingAsk {
    pub question: String,
    /// When it was asked.
    pub asked_at: SystemTime,
}

/// Where one repository's intents and pending asks live. Cheap to clone; a default one (no
/// directories) reads nothing and refuses to write, same as a default `crate::report::Reports`
/// — which is what tests and a machine with no home directory get.
#[derive(Debug, Clone, Default)]
pub struct Orchestration {
    intents: Option<PathBuf>,
    asks: Option<PathBuf>,
    /// Where [`Self::ask`] and [`Self::answer`] also leave their permanent record — see the
    /// module doc. Best-effort, like every other event.
    events: EventLog,
}

impl Orchestration {
    pub fn for_repo(repo: &Path) -> Self {
        Orchestration { intents: intents_dir(repo), asks: asks_dir(repo), events: EventLog::for_repo(repo) }
    }

    #[cfg(test)]
    pub(crate) fn in_dir(dir: PathBuf) -> Self {
        Orchestration { intents: Some(dir.join("intents")), asks: Some(dir.join("asks")), events: EventLog::default() }
    }

    /// Sets `branch`'s intent to `text`, replacing whatever it was.
    pub fn set_intent(&self, branch: &str, text: &str, now: SystemTime) -> Result<()> {
        let stored = StoredIntent { branch: branch.to_string(), text: text.to_string(), at: epoch_secs(now) };
        write_atomically(file(&self.intents, branch), &stored)
    }

    /// `branch`'s current intent, if one is set. A missing, unreadable or malformed file is
    /// "none" — this is only ever shown, never acted on, so a bad file must not fail the
    /// `status` that reads it.
    pub fn intent(&self, branch: &str) -> Option<String> {
        read::<StoredIntent>(file(&self.intents, branch)?).map(|s| s.text)
    }

    /// Sets `branch`'s pending ask to `question`, replacing any that was already pending (an
    /// orchestrator that asks again has changed its mind about what it needs to know), and
    /// logs it to the events log.
    pub fn ask(&self, branch: &str, question: &str, now: SystemTime) -> Result<()> {
        let stored = StoredAsk { branch: branch.to_string(), question: question.to_string(), at: epoch_secs(now) };
        write_atomically(file(&self.asks, branch), &stored)?;
        self.events.record_ask(branch, question, now);
        Ok(())
    }

    /// `branch`'s pending ask, if it has one. Unreadable is "none", same as [`Self::intent`].
    pub fn pending_ask(&self, branch: &str) -> Option<PendingAsk> {
        let stored = read::<StoredAsk>(file(&self.asks, branch)?)?;
        Some(PendingAsk { question: stored.question, asked_at: UNIX_EPOCH + Duration::from_secs(stored.at) })
    }

    /// Answers `branch`'s pending ask with `text`: clears it and logs the answer. Returns the
    /// ask it answered, or `None` if there was none pending — which the caller reports as an
    /// error, since an answer to nothing is almost certainly a mistake (a typo'd branch, or a
    /// question someone else already answered).
    ///
    /// The ask is taken by renaming its file away before reading it, rather than reading then
    /// removing, so two `answer`s racing on the same ask can't both succeed: only one rename
    /// finds the file, and the other sees nothing pending.
    pub fn answer(&self, branch: &str, text: &str, now: SystemTime) -> Result<Option<PendingAsk>> {
        let Some(path) = file(&self.asks, branch) else {
            anyhow::bail!("no home directory to keep asks in");
        };
        let taken = path.with_extension(format!("json.{}.answering", std::process::id()));
        match std::fs::rename(&path, &taken) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("taking {}", path.display())),
        }
        let stored = read::<StoredAsk>(taken.clone());
        let _ = std::fs::remove_file(&taken);
        // A mangled file is cleared all the same, but it's answered as nothing pending — the
        // same thing `pending_ask`, and so `status --json`, already said about it.
        let Some(stored) = stored else { return Ok(None) };
        let answered = PendingAsk { question: stored.question, asked_at: UNIX_EPOCH + Duration::from_secs(stored.at) };
        self.events.record_answer(branch, text, now);
        Ok(Some(answered))
    }

    /// Drops `branch`'s intent and pending ask, because they were about a pane that no longer
    /// exists: a stale "waiting for cargo test" or an unanswered question from a finished task
    /// must not haunt the next agent spawned on the same branch name. Called from
    /// `crate::report::Reports::forget`, so it happens exactly when a report is forgotten — on
    /// `stop`, and on a respawn onto the same branch. Best-effort: nothing to remove is not an
    /// error. Logs no `answer` — a question dropped with its lane wasn't answered.
    pub fn forget(&self, branch: &str) {
        for path in [file(&self.intents, branch), file(&self.asks, branch)].into_iter().flatten() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// One file per branch in `dir`, named by a hash of it so any branch name — slashes and all —
/// is a valid file name and none can collide with another. Same scheme as `Reports::file`.
fn file(dir: &Option<PathBuf>, branch: &str) -> Option<PathBuf> {
    Some(dir.as_ref()?.join(format!("{:016x}.json", fnv1a(branch.as_bytes()))))
}

fn epoch_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Writes `value` to `path` via a temporary file and a rename, so a reader never sees half of
/// one — same as `Reports::write`.
fn write_atomically(path: Option<PathBuf>, value: &impl Serialize) -> Result<()> {
    let Some(path) = path else {
        anyhow::bail!("no home directory to keep orchestration state in");
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string(value)? + "\n")?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

fn read<T: DeserializeOwned>(path: PathBuf) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> Orchestration {
        let dir = std::env::temp_dir().join(format!("kanstack-orchestration-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Orchestration::in_dir(dir)
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn an_intent_reads_back_and_the_latest_one_replaces_the_one_before() {
        let o = scratch("intent");
        assert_eq!(o.intent("feat-a"), None, "nothing set yet");
        o.set_intent("feat-a", "waiting for cargo test", at(1000)).unwrap();
        assert_eq!(o.intent("feat-a").as_deref(), Some("waiting for cargo test"));
        o.set_intent("feat-a", "reviewing the diff", at(1001)).unwrap();
        assert_eq!(o.intent("feat-a").as_deref(), Some("reviewing the diff"));
        assert_eq!(o.intent("feat-b"), None, "another branch is untouched");
    }

    /// Unlike a report, an intent doesn't go stale — it's only ever set on purpose.
    #[test]
    fn an_intent_does_not_expire() {
        let o = scratch("intent-old");
        o.set_intent("feat-a", "parked until Monday", at(0)).unwrap();
        assert_eq!(o.intent("feat-a").as_deref(), Some("parked until Monday"));
    }

    #[test]
    fn an_ask_is_pending_until_answered_and_answering_returns_what_was_asked() {
        let o = scratch("ask");
        assert_eq!(o.pending_ask("feat-a"), None);
        o.ask("feat-a", "ship it?", at(1000)).unwrap();
        assert_eq!(o.pending_ask("feat-a"), Some(PendingAsk { question: "ship it?".into(), asked_at: at(1000) }));

        let answered = o.answer("feat-a", "yes", at(1005)).unwrap();
        assert_eq!(answered, Some(PendingAsk { question: "ship it?".into(), asked_at: at(1000) }));
        assert_eq!(o.pending_ask("feat-a"), None, "answering clears it");
    }

    #[test]
    fn asking_again_replaces_the_pending_question() {
        let o = scratch("ask-again");
        o.ask("feat-a", "ship it?", at(1000)).unwrap();
        o.ask("feat-a", "ship it to staging first?", at(1001)).unwrap();
        assert_eq!(o.pending_ask("feat-a").map(|a| a.question).as_deref(), Some("ship it to staging first?"));
    }

    /// Answering nothing is the caller's error to report (`no_pending_ask`), not a silent
    /// success — and a second answer to an ask the first already cleared is exactly that.
    #[test]
    fn answering_with_nothing_pending_says_so() {
        let o = scratch("answer-none");
        assert_eq!(o.answer("feat-a", "yes", at(1000)).unwrap(), None);
        o.ask("feat-a", "ship it?", at(1000)).unwrap();
        assert!(o.answer("feat-a", "yes", at(1001)).unwrap().is_some());
        assert_eq!(o.answer("feat-a", "yes again", at(1002)).unwrap(), None, "already answered");
    }

    /// An intent and a pending ask are separate values: setting or clearing one leaves the
    /// other alone.
    #[test]
    fn an_intent_and_an_ask_are_independent() {
        let o = scratch("independent");
        o.set_intent("feat-a", "waiting on you", at(1000)).unwrap();
        o.ask("feat-a", "ship it?", at(1000)).unwrap();
        o.answer("feat-a", "yes", at(1001)).unwrap();
        assert_eq!(o.intent("feat-a").as_deref(), Some("waiting on you"), "answering doesn't clear the intent");
        o.ask("feat-a", "and the docs?", at(1002)).unwrap();
        o.set_intent("feat-a", "writing docs", at(1003)).unwrap();
        assert!(o.pending_ask("feat-a").is_some(), "a new intent doesn't clear the ask");
    }

    #[test]
    fn forgetting_drops_the_intent_and_the_ask_for_that_branch_only() {
        let o = scratch("forget");
        o.set_intent("feat-a", "a", at(1000)).unwrap();
        o.ask("feat-a", "a?", at(1000)).unwrap();
        o.set_intent("feat-b", "b", at(1000)).unwrap();
        o.ask("feat-b", "b?", at(1000)).unwrap();
        o.forget("feat-a");
        assert_eq!((o.intent("feat-a"), o.pending_ask("feat-a")), (None, None));
        assert_eq!(o.intent("feat-b").as_deref(), Some("b"));
        assert!(o.pending_ask("feat-b").is_some());
        o.forget("never-set"); // nothing to remove is not an error
    }

    #[test]
    fn branch_names_with_slashes_and_odd_characters_are_all_distinct_files() {
        let o = scratch("names");
        for branch in ["feat/login", "feat-login", "feat/login/", "ünïcode branch", "a b"] {
            o.set_intent(branch, &format!("intent of {branch}"), at(1000)).unwrap();
        }
        for branch in ["feat/login", "feat-login", "feat/login/", "ünïcode branch", "a b"] {
            assert_eq!(o.intent(branch), Some(format!("intent of {branch}")));
        }
    }

    #[test]
    fn a_corrupt_file_reads_as_nothing_set() {
        let o = scratch("corrupt");
        o.set_intent("feat-a", "x", at(1000)).unwrap();
        o.ask("feat-a", "x?", at(1000)).unwrap();
        std::fs::write(file(&o.intents, "feat-a").unwrap(), "{ not json").unwrap();
        std::fs::write(file(&o.asks, "feat-a").unwrap(), "{ not json").unwrap();
        assert_eq!(o.intent("feat-a"), None);
        assert_eq!(o.pending_ask("feat-a"), None);
    }

    #[test]
    fn with_nowhere_to_keep_state_nothing_is_read_and_writing_says_so() {
        let o = Orchestration::default();
        assert_eq!(o.intent("feat-a"), None);
        assert_eq!(o.pending_ask("feat-a"), None);
        assert!(o.set_intent("feat-a", "x", at(1)).is_err());
        assert!(o.ask("feat-a", "x?", at(1)).is_err());
        assert!(o.answer("feat-a", "x", at(1)).is_err());
        o.forget("feat-a");
    }

    /// `ask` and `answer` leave a permanent record in the events log; `set_intent` doesn't —
    /// see the module doc. `Orchestration::in_dir` has nowhere to keep an events log, so this
    /// goes through `for_repo`, the constructor the subcommands actually use.
    #[test]
    fn asking_and_answering_are_logged_but_setting_an_intent_is_not() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-orchestration-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let repo = Path::new("/repo/orchestration-events");

        let o = Orchestration::for_repo(repo);
        o.set_intent("fix-login", "thinking", at(1000)).unwrap();
        o.ask("fix-login", "ship it?", at(1001)).unwrap();
        o.answer("fix-login", "yes", at(1002)).unwrap();
        o.answer("fix-login", "nothing pending, so not logged", at(1003)).unwrap();

        let raw = std::fs::read_to_string(crate::workstream::events_path(repo).unwrap()).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(lines.len(), 2, "{raw}");
        assert!(lines[0].contains(r#""kind":"ask","question":"ship it?""#), "{}", lines[0]);
        assert!(lines[1].contains(r#""kind":"answer","text":"yes""#), "{}", lines[1]);
    }
}
