//! What an agent says about itself: `kanstack report busy` / `idle` / `waiting`.
//!
//! A multiplexer can only guess at whether a harness is working, from CPU or from whatever
//! idle detection it happens to have — and some can't guess at all. A harness that can run a
//! command when a turn starts and ends can just say so. This module is the channel for that:
//! one small file per branch, written by `kanstack report` (which a harness's hooks run) and
//! read when the board or `kanstack status` polls. It is deliberately not part of the
//! workstream registry: the registry is rewritten whole, and hooks fire often and from
//! several processes at once, so sharing it would lose updates. Each write here replaces one
//! branch's file atomically and touches nothing else.
//!
//! A report is only ever a hint that can go stale — an agent that crashes mid-turn never
//! says it stopped — so it counts for [`FRESH_FOR`] and no longer, after which the
//! multiplexer's own reading is used again. See `crate::splitter::Splitter::poll_statuses`
//! for how the two are combined.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::claims::Claims;
use crate::events::EventLog;
use crate::pane_status::PaneStatus;
use crate::workstream::{fnv1a, reports_dir};

/// How long a `busy` or `idle` report is believed. Long enough to span a turn that is busy
/// the whole time (harnesses that can also report per tool call keep it fresh), short enough
/// that a crashed agent stops reading as busy soon after.
pub const FRESH_FOR: Duration = Duration::from_secs(10 * 60);

/// How long a `waiting` report is believed. Much longer, because the point of it is an agent
/// that has been stuck on a prompt overnight: expiring it after ten minutes would report that
/// agent as fine just when it most needs someone. What clears a stale one is the agent
/// saying something else, not time.
pub const WAITING_FRESH_FOR: Duration = Duration::from_secs(12 * 60 * 60);

/// What an agent can say about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reported {
    /// A turn is running: it is generating, or running tools.
    Busy,
    /// A turn has ended and it is waiting for the next prompt.
    Idle,
    /// A turn is stopped partway, waiting for the user to answer something — a permission
    /// prompt. Unlike `Idle`, nothing will happen until they do.
    Waiting,
}

impl Reported {
    pub fn parse(word: &str) -> Option<Reported> {
        match word {
            "busy" => Some(Reported::Busy),
            "idle" => Some(Reported::Idle),
            "waiting" => Some(Reported::Waiting),
            _ => None,
        }
    }

    /// How long a report of this state is believed.
    fn fresh_for(self) -> Duration {
        match self {
            Reported::Busy | Reported::Idle => FRESH_FOR,
            Reported::Waiting => WAITING_FRESH_FOR,
        }
    }

    fn status(self) -> PaneStatus {
        match self {
            Reported::Busy => PaneStatus::Busy,
            Reported::Idle => PaneStatus::Idle,
            Reported::Waiting => PaneStatus::Waiting,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    /// Only for whoever is looking at the file; the name of the file is a hash.
    branch: String,
    state: Reported,
    /// Seconds since the Unix epoch.
    at: u64,
}

/// What an agent's last report is worth right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// Recent enough to believe: this old, and written at this second (seconds since the
    /// epoch), which identifies it for [`Reports::retract_busy`].
    Fresh { status: PaneStatus, age: Duration, at: u64 },
    /// There is one, but it has run out. Different from nothing having been said: the agent
    /// reported, and then went quiet without saying it had stopped.
    Stale,
}

/// Where one repository's reports live. Cheap to clone; a default one (no directory) reads
/// nothing and writes nowhere, which is what tests and a machine with no home directory get.
#[derive(Debug, Clone, Default)]
pub struct Reports {
    dir: Option<PathBuf>,
    /// Where a report [`Self::write`]s also rings the events-log doorbell — see
    /// `crate::events`. Best-effort, like the report itself.
    events: EventLog,
    /// What `crate::claims::Claims::release_all` a branch going idle, or being forgotten
    /// entirely, drops — see [`Self::write`] and [`Self::forget`]. Best-effort, like `events`.
    claims: Claims,
}

impl Reports {
    pub fn for_repo(repo: &Path) -> Self {
        Reports { dir: reports_dir(repo), events: EventLog::for_repo(repo), claims: Claims::for_repo(repo) }
    }

    #[cfg(test)]
    pub(crate) fn in_dir(dir: PathBuf) -> Self {
        Reports { dir: Some(dir), events: EventLog::default(), claims: Claims::default() }
    }

    /// One file per branch, named by a hash of it so any branch name — slashes and all — is a
    /// valid file name and none can collide with another.
    fn file(&self, branch: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{:016x}.json", fnv1a(branch.as_bytes()))))
    }

    /// Records that `branch`'s agent is `state` as of `now`. Written to a temporary file and
    /// renamed, so a reader never sees half of one.
    pub fn write(&self, branch: &str, state: Reported, now: SystemTime) -> Result<()> {
        let Some(path) = self.file(branch) else {
            anyhow::bail!("no home directory to keep reports in");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let at = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let stored = Stored { branch: branch.to_string(), state, at };
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string(&stored)? + "\n")?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        self.events.record_report(branch, state, now);
        // A turn ending is the ordinary way a lane stops being "actively editing" anything —
        // see `crate::claims`'s module doc. Not for `Busy` (still going) or `Waiting` (stopped
        // mid-turn on a prompt, not done with the file it was touching) — only `Idle` means the
        // turn that held these claims has actually finished.
        if state == Reported::Idle {
            self.claims.release_all(branch);
        }
        Ok(())
    }

    /// What `branch`'s agent last said, and whether it can still be believed as of `now`. A
    /// missing, unreadable or malformed file is "nothing said" — a report is only a hint, so
    /// a bad one must never get in the way of the multiplexer's own reading.
    pub fn said(&self, branch: &str, now: SystemTime) -> Option<Said> {
        let raw = std::fs::read_to_string(self.file(branch)?).ok()?;
        let stored: Stored = serde_json::from_str(&raw).ok()?;
        let at = UNIX_EPOCH + Duration::from_secs(stored.at);
        // A report from the future (clock skew) is as fresh as it gets.
        let age = now.duration_since(at).unwrap_or_default();
        Some(if age <= stored.state.fresh_for() { Said::Fresh { status: stored.state.status(), age, at: stored.at } } else { Said::Stale })
    }

    /// [`Self::said`], but only what can still be believed.
    pub fn status(&self, branch: &str, now: SystemTime) -> Option<PaneStatus> {
        match self.said(branch, now)? {
            Said::Fresh { status, .. } => Some(status),
            Said::Stale => None,
        }
    }

    /// Takes back a `busy` report that the pane's own quiet has shown to be stale, by writing
    /// `idle` in its place — but only if the file still holds the very report the caller
    /// looked at (`expected_at`, the second it was written) and it still says `busy`. A hook
    /// that fired in between has written something newer, and that must win. Returns whether it
    /// did.
    ///
    /// Interrupting a turn with Escape fires no hook, so nothing else ever retracts that
    /// `busy`. Writing the correction back, rather than merely reading past it, is what keeps
    /// a later CPU blip from resurrecting it.
    pub fn retract_busy(&self, branch: &str, expected_at: u64, now: SystemTime) -> bool {
        let Some(path) = self.file(branch) else { return false };
        let Ok(raw) = std::fs::read_to_string(&path) else { return false };
        let Ok(stored) = serde_json::from_str::<Stored>(&raw) else { return false };
        if stored.state != Reported::Busy || stored.at != expected_at {
            return false;
        }
        self.write(branch, Reported::Idle, now).is_ok()
    }

    /// Drops whatever `branch`'s agent said, because it was about a pane that no longer
    /// exists: a new pane must not inherit the old one's last word. Also drops every file
    /// claim `branch` holds (see `crate::claims`) for the same reason — a pane that's gone,
    /// or about to be replaced, can't still be mid-edit of anything.
    pub fn forget(&self, branch: &str) {
        if let Some(path) = self.file(branch) {
            let _ = std::fs::remove_file(path);
        }
        self.claims.release_all(branch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> Reports {
        let dir = std::env::temp_dir().join(format!("kanstack-reports-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Reports::in_dir(dir)
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_report_reads_back_as_the_status_it_named() {
        let reports = scratch("roundtrip");
        reports.write("feat-a", Reported::Busy, at(1000)).unwrap();
        reports.write("feat-b", Reported::Idle, at(1000)).unwrap();
        reports.write("feat-w", Reported::Waiting, at(1000)).unwrap();
        assert_eq!(reports.status("feat-a", at(1001)), Some(PaneStatus::Busy));
        assert_eq!(reports.status("feat-b", at(1001)), Some(PaneStatus::Idle));
        assert_eq!(reports.status("feat-w", at(1001)), Some(PaneStatus::Waiting));
        assert_eq!(reports.status("feat-c", at(1001)), None, "nobody said anything for feat-c");
    }

    #[test]
    fn the_latest_report_replaces_the_one_before() {
        let reports = scratch("replace");
        reports.write("feat-a", Reported::Busy, at(1000)).unwrap();
        reports.write("feat-a", Reported::Idle, at(1005)).unwrap();
        assert_eq!(reports.status("feat-a", at(1006)), Some(PaneStatus::Idle));
    }

    /// A crashed agent never reports that it stopped, so a busy report can't be believed
    /// forever.
    #[test]
    fn a_report_is_believed_for_a_while_and_then_not() {
        let reports = scratch("stale");
        reports.write("feat-a", Reported::Busy, at(1000)).unwrap();
        let limit = 1000 + FRESH_FOR.as_secs();
        assert_eq!(reports.status("feat-a", at(limit)), Some(PaneStatus::Busy), "the limit itself is still fresh");
        assert_eq!(reports.status("feat-a", at(limit + 1)), None);
    }

    #[test]
    fn a_report_from_the_future_counts_as_fresh() {
        let reports = scratch("skew");
        reports.write("feat-a", Reported::Busy, at(5000)).unwrap();
        assert_eq!(reports.status("feat-a", at(1000)), Some(PaneStatus::Busy));
    }

    #[test]
    fn forgetting_drops_only_that_branch() {
        let reports = scratch("forget");
        reports.write("feat-a", Reported::Busy, at(1000)).unwrap();
        reports.write("feat-b", Reported::Busy, at(1000)).unwrap();
        reports.forget("feat-a");
        assert_eq!(reports.status("feat-a", at(1001)), None);
        assert_eq!(reports.status("feat-b", at(1001)), Some(PaneStatus::Busy));
        reports.forget("never-reported"); // nothing to remove is not an error
    }

    #[test]
    fn branch_names_with_slashes_and_odd_characters_are_all_distinct_files() {
        let reports = scratch("names");
        for (i, branch) in ["feat/login", "feat-login", "feat/login/", "ünïcode branch", "a b"].iter().enumerate() {
            let state = if i % 2 == 0 { Reported::Busy } else { Reported::Idle };
            reports.write(branch, state, at(1000)).unwrap();
        }
        assert_eq!(reports.status("feat/login", at(1001)), Some(PaneStatus::Busy));
        assert_eq!(reports.status("feat-login", at(1001)), Some(PaneStatus::Idle));
        assert_eq!(reports.status("feat/login/", at(1001)), Some(PaneStatus::Busy));
        assert_eq!(reports.status("ünïcode branch", at(1001)), Some(PaneStatus::Idle));
    }

    /// A report is only a hint; a mangled file must read as "nothing said", not fail the
    /// poll that found it.
    #[test]
    fn a_corrupt_report_reads_as_nothing_said() {
        let reports = scratch("corrupt");
        reports.write("feat-a", Reported::Busy, at(1000)).unwrap();
        std::fs::write(reports.file("feat-a").unwrap(), "{ not json").unwrap();
        assert_eq!(reports.status("feat-a", at(1001)), None);
    }

    #[test]
    fn with_nowhere_to_keep_reports_nothing_is_read_and_writing_says_so() {
        let reports = Reports::default();
        assert_eq!(reports.status("feat-a", at(1)), None);
        assert!(reports.write("feat-a", Reported::Busy, at(1)).is_err());
        reports.forget("feat-a");
    }

    #[test]
    fn only_busy_idle_and_waiting_can_be_reported() {
        assert_eq!(Reported::parse("busy"), Some(Reported::Busy));
        assert_eq!(Reported::parse("idle"), Some(Reported::Idle));
        assert_eq!(Reported::parse("waiting"), Some(Reported::Waiting));
        assert_eq!(Reported::parse("dead"), None);
        assert_eq!(Reported::parse(""), None);
    }

    /// A prompt someone hasn't got to yet is not a stale report; a busy agent that stopped
    /// talking is.
    #[test]
    fn a_waiting_report_outlives_a_busy_one() {
        let reports = scratch("lifetimes");
        reports.write("busy", Reported::Busy, at(1000)).unwrap();
        reports.write("waiting", Reported::Waiting, at(1000)).unwrap();
        let later = at(1000 + FRESH_FOR.as_secs() + 1);
        assert_eq!(reports.said("busy", later), Some(Said::Stale));
        assert!(matches!(reports.said("waiting", later), Some(Said::Fresh { status: PaneStatus::Waiting, .. })));
        let much_later = at(1000 + WAITING_FRESH_FOR.as_secs() + 1);
        assert_eq!(reports.said("waiting", much_later), Some(Said::Stale));
    }

    #[test]
    fn a_stale_report_is_not_the_same_as_nothing_said() {
        let reports = scratch("stale-vs-none");
        reports.write("feat-a", Reported::Idle, at(1000)).unwrap();
        let later = at(1000 + FRESH_FOR.as_secs() + 1);
        assert_eq!(reports.said("feat-a", later), Some(Said::Stale));
        assert_eq!(reports.status("feat-a", later), None, "a stale report is not believed");
        assert_eq!(reports.said("feat-b", later), None, "nobody ever said anything for feat-b");
    }

    #[test]
    fn retracting_a_busy_report_writes_idle_but_only_for_the_report_that_was_looked_at() {
        let reports = scratch("retract");
        reports.write("feat-a", Reported::Busy, at(1000)).unwrap();
        // A report written since — a hook fired between the decision and now — must win.
        assert!(!reports.retract_busy("feat-a", 999, at(1030)), "not the report that was examined");
        assert!(matches!(reports.said("feat-a", at(1031)), Some(Said::Fresh { status: PaneStatus::Busy, .. })));

        assert!(reports.retract_busy("feat-a", 1000, at(1030)));
        assert_eq!(reports.status("feat-a", at(1031)), Some(PaneStatus::Idle));
        assert!(!reports.retract_busy("feat-a", 1030, at(1040)), "nothing busy is left to retract");
    }

    #[test]
    fn only_a_busy_report_can_be_retracted() {
        let reports = scratch("retract-kinds");
        reports.write("waiting", Reported::Waiting, at(1000)).unwrap();
        reports.write("idle", Reported::Idle, at(1000)).unwrap();
        assert!(!reports.retract_busy("waiting", 1000, at(1030)), "a prompt is not a stale busy");
        assert!(!reports.retract_busy("idle", 1000, at(1030)));
        assert!(!reports.retract_busy("never-reported", 1000, at(1030)));
        assert_eq!(reports.status("waiting", at(1031)), Some(PaneStatus::Waiting));
    }

    /// `write` is the one place every `kanstack report <state>` call goes through, so it's
    /// where the events-log doorbell rings too — see `crate::events`. `Reports::in_dir` (what
    /// `scratch` above uses) has nowhere to keep an events log, so this goes through
    /// `Reports::for_repo` instead, the same constructor `kanstack report` actually uses.
    #[test]
    fn writing_a_report_also_appends_it_to_the_events_log() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-reports-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let repo = Path::new("/repo/reports-events");

        Reports::for_repo(repo).write("fix-login", Reported::Busy, at(1000)).unwrap();

        let events_path = crate::workstream::events_path(repo).unwrap();
        let raw = std::fs::read_to_string(&events_path).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert!(raw.contains(r#""branch":"fix-login""#), "{raw}");
        assert!(raw.contains(r#""kind":"report""#), "{raw}");
        assert!(raw.contains(r#""state":"busy""#), "{raw}");

        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same reasoning as the events-log test above: `Claims::release_all` only does anything
    /// with somewhere to look, so this goes through `Reports::for_repo` rather than `scratch`'s
    /// `Reports::in_dir`.
    fn with_claims_state(tag: &str, body: impl FnOnce(&Path, &crate::claims::Claims)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-reports-claims-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let repo = Path::new("/repo/reports-claims");
        body(repo, &crate::claims::Claims::for_repo(repo));
        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A turn ending is the ordinary way a lane stops being mid-edit of anything — see
    /// `crate::claims`'s module doc — so writing `idle` must drop whatever files this branch
    /// had claimed, but leave another branch's claims alone.
    #[test]
    fn writing_idle_releases_the_branchs_own_claims_only() {
        with_claims_state("idle", |repo, claims| {
            claims.claim("/repo/src/lib.rs", "fix-login", at(1000)).unwrap();
            claims.claim("/repo/src/other.rs", "add-search", at(1000)).unwrap();

            Reports::for_repo(repo).write("fix-login", Reported::Idle, at(1001)).unwrap();

            assert_eq!(claims.holder("/repo/src/lib.rs", at(1002)), None, "fix-login's own claim is gone");
            assert_eq!(claims.holder("/repo/src/other.rs", at(1002)), Some("add-search".to_string()), "another branch's claim is untouched");
        });
    }

    /// A turn merely running (`Busy`) or stopped on a prompt (`Waiting`) is not "done with the
    /// file" — only `Idle` releases claims.
    #[test]
    fn writing_busy_or_waiting_does_not_release_claims() {
        with_claims_state("busy-waiting", |repo, claims| {
            claims.claim("/repo/src/lib.rs", "fix-login", at(1000)).unwrap();
            let reports = Reports::for_repo(repo);
            reports.write("fix-login", Reported::Busy, at(1001)).unwrap();
            assert_eq!(claims.holder("/repo/src/lib.rs", at(1002)), Some("fix-login".to_string()));
            reports.write("fix-login", Reported::Waiting, at(1002)).unwrap();
            assert_eq!(claims.holder("/repo/src/lib.rs", at(1003)), Some("fix-login".to_string()));
        });
    }

    /// `forget` — called when a pane is stopped, or reused for a fresh spawn (see
    /// `crate::splitter::Splitter::stop`/`prepare_launch`) — drops the branch's claims too: a
    /// pane that's gone, or about to be replaced, can't still be mid-edit of anything.
    #[test]
    fn forgetting_a_branch_also_releases_its_claims() {
        with_claims_state("forget", |repo, claims| {
            claims.claim("/repo/src/lib.rs", "fix-login", at(1000)).unwrap();
            claims.claim("/repo/src/other.rs", "add-search", at(1000)).unwrap();

            Reports::for_repo(repo).forget("fix-login");

            assert_eq!(claims.holder("/repo/src/lib.rs", at(1001)), None);
            assert_eq!(claims.holder("/repo/src/other.rs", at(1001)), Some("add-search".to_string()));
        });
    }
}
