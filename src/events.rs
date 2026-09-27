//! An append-only, tailable log of things happening to workstreams — a report state change,
//! a lifecycle change (spawn/stop/prune), and an orchestrator's note, ask or answer — so an external orchestrator driving kanstack
//! doesn't have to poll `kanstack status` in a loop to notice one. No daemon, no server, no
//! push channel: just a JSONL file another process can `tail -f`, or poll from a byte offset
//! with `kanstack events --since <offset>` — or skip the backlog entirely and start from the
//! log's current end with `kanstack events --new` (see `crate::cli::events`). One file per
//! repository (see [`crate::workstream::events_path`]), the same convention as
//! `reports_dir`/`pids_dir`/`state_path`.
//!
//! Each line is one JSON object, schema-versioned independently of `STATUS_SCHEMA`/
//! `PRUNE_SCHEMA`/`RESULT_SCHEMA` (`crate::cli::exit`) since none of those describe this
//! shape:
//!
//! ```text
//! {"schema":1,"ts":"2026-09-22T13:04:05Z","branch":"fix-login","kind":"report","state":"busy"}
//! {"schema":1,"ts":"2026-09-22T13:05:10Z","kind":"lifecycle","command":"prune"}
//! {"schema":1,"ts":"2026-09-22T13:06:00Z","branch":"fix-login","kind":"note","text":"tests pass, moving on to docs"}
//! {"schema":1,"ts":"2026-09-22T13:07:00Z","branch":"fix-login","kind":"ask","question":"ship it?"}
//! {"schema":1,"ts":"2026-09-22T13:08:00Z","branch":"fix-login","kind":"answer","text":"yes"}
//! ```
//!
//! `kind: "report"` carries `state` (`busy`/`idle`/`waiting`, [`Reported`]'s own wire form)
//! and always has a `branch`. `kind: "lifecycle"` carries `command` (`spawn`/`stop`/`prune` —
//! whichever subcommand `crate::cli::exit::dispatch` just ran successfully) and `branch` when
//! the command named one: `spawn`'s branch, or whatever `<branch|session>` `stop` was given
//! (not necessarily resolved to a branch name — `kanstack status --json` is where the precise
//! answer lives). `prune` can touch several workstreams or none, so it logs one event with no
//! `branch` rather than guessing which ones.
//!
//! `kind: "note"`, `"ask"` and `"answer"` come from an orchestrator talking about a lane —
//! `kanstack note`/`ask`/`answer` (see `crate::cli`) — and always have a `branch`. A `note`
//! exists *only* here: it's a remark for the record, with no latched state behind it. An
//! `ask` and its `answer` also set and clear a branch's pending ask (see
//! `crate::orchestration`), which is what `status --json` reads; they're logged too because
//! a question someone had to answer is worth a permanent record, not just a current value.
//! Unlike the other kinds these do carry their text, since for a note the text *is* the
//! whole event. `kanstack intent` logs nothing: an intent is a transient current value,
//! overwritten often, and belongs in `status --json` alone.
//!
//! Deliberately thin — this is a doorbell, not the payload: seeing a line is a cue to go read
//! `kanstack status --json`, not something to parse for the actual state.
//!
//! Appends are best-effort, like [`crate::report::Reports::write`]: nothing here ever returns
//! an error to its caller, so a write failure never breaks the command it's attached to.
//!
//! No rotation or size limit — a known limitation, not an oversight. Add one if a repo's log
//! ever actually grows enough to matter; nothing reads the whole file into memory except
//! `kanstack events` itself, which only reads what's new since its cursor, and the board's
//! [`EventLog::recent_notes`] (see its own doc for why that's fine for now).

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::report::Reported;
use crate::workstream::events_path;

/// The `"schema"` of every line in the events log, versioned independently of
/// `STATUS_SCHEMA`/`PRUNE_SCHEMA`/`RESULT_SCHEMA` — see the module doc.
pub const EVENTS_SCHEMA: u32 = 1;

/// Where one repository's events log lives. Cheap to clone; a default one (no path) writes
/// nowhere, same as [`crate::report::Reports`] with nowhere to keep reports.
#[derive(Debug, Clone, Default)]
pub struct EventLog {
    path: Option<PathBuf>,
}

impl EventLog {
    pub fn for_repo(repo: &Path) -> Self {
        EventLog { path: events_path(repo) }
    }

    /// Records that `branch`'s agent reported `state`, as of `now`. Best-effort — see the
    /// module doc.
    pub fn record_report(&self, branch: &str, state: Reported, now: SystemTime) {
        self.append(&Event {
            schema: EVENTS_SCHEMA,
            ts: rfc3339(now),
            branch: Some(branch.to_string()),
            kind: Kind::Report { state },
        });
    }

    /// Records that `command` (`spawn`/`stop`/`prune`) succeeded, for `branch` when it named
    /// one, as of `now`. Best-effort — see the module doc.
    pub fn record_lifecycle(&self, branch: Option<&str>, command: &'static str, now: SystemTime) {
        self.append(&Event {
            schema: EVENTS_SCHEMA,
            ts: rfc3339(now),
            branch: branch.map(str::to_string),
            kind: Kind::Lifecycle { command },
        });
    }

    /// Records a free-text remark about `branch`, as of `now` — `kanstack note`, which has no
    /// latched state of its own, so this line is all there is of it. Best-effort — see the
    /// module doc.
    pub fn record_note(&self, branch: &str, text: &str, now: SystemTime) {
        self.append(&Event {
            schema: EVENTS_SCHEMA,
            ts: rfc3339(now),
            branch: Some(branch.to_string()),
            kind: Kind::Note { text: text.to_string() },
        });
    }

    /// Records that `branch` now has `question` pending, as of `now` — the permanent record
    /// beside the latched pending ask `crate::orchestration::Orchestration::ask` sets.
    /// Best-effort — see the module doc.
    pub fn record_ask(&self, branch: &str, question: &str, now: SystemTime) {
        self.append(&Event {
            schema: EVENTS_SCHEMA,
            ts: rfc3339(now),
            branch: Some(branch.to_string()),
            kind: Kind::Ask { question: question.to_string() },
        });
    }

    /// Records that `branch`'s pending ask was answered with `text`, as of `now`. Best-effort
    /// — see the module doc.
    pub fn record_answer(&self, branch: &str, text: &str, now: SystemTime) {
        self.append(&Event {
            schema: EVENTS_SCHEMA,
            ts: rfc3339(now),
            branch: Some(branch.to_string()),
            kind: Kind::Answer { text: text.to_string() },
        });
    }

    /// `branch`'s most recent `limit` notes (`kanstack note`), newest first, each with the
    /// time it was recorded — what the board shows in an advisory lane's body, where a note
    /// is often the only sign of what a lane that never commits has been up to. The one thing
    /// in kanstack that reads this log back rather than only appending to it or handing its
    /// raw bytes to someone else (`kanstack events`).
    ///
    /// "Newest" means *last appended*, not "latest `ts`": appends land in the order they
    /// happened, so the two only disagree if two writers' clocks do, and file order is the
    /// one a reader can't get wrong. Every line that isn't a well-formed note for `branch` —
    /// another branch's, a report/lifecycle/ask/answer event, a malformed or half-written
    /// line, an unparseable `ts` — is skipped rather than failing the read: this is only
    /// ever shown, never acted on, same as `Orchestration::intent`. A missing log, or one with
    /// nowhere to live, is simply no notes.
    ///
    /// Reads the whole file on every call, then walks it from the end — a known limitation,
    /// in the same spirit as the module doc's "no rotation or size limit": fine for any log a
    /// repository realistically accumulates, and the board only asks for advisory lanes, and
    /// only when it rebuilds. A byte-range read backward from EOF is the fix if a log ever
    /// grows big enough for this to show up.
    pub fn recent_notes(&self, branch: &str, limit: usize) -> Vec<(SystemTime, String)> {
        let Some(path) = &self.path else { return Vec::new() };
        let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
        raw.lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<ReadBack>(line).ok())
            .filter(|e| e.kind == "note" && e.branch.as_deref() == Some(branch))
            .filter_map(|e| Some((parse_rfc3339(&e.ts)?, e.text?)))
            .take(limit)
            .collect()
    }

    /// Appends `event` as one line. Swallows every error — a missing home directory, a
    /// permissions problem, a full disk — because a doorbell that occasionally doesn't ring
    /// must never be the reason the command it's attached to fails.
    fn append(&self, event: &Event) {
        let Some(path) = &self.path else { return };
        let Ok(line) = serde_json::to_string(event) else { return };
        if let Some(parent) = path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[derive(Debug, Serialize)]
struct Event {
    schema: u32,
    ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(flatten)]
    kind: Kind,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum Kind {
    Report { state: Reported },
    Lifecycle { command: &'static str },
    Note { text: String },
    Ask { question: String },
    Answer { text: String },
}

/// Just the fields of a line [`EventLog::recent_notes`] needs, read back leniently — every
/// other field (`schema`, `state`, `command`, `question`) is ignored rather than modelled,
/// so a line of any kind parses and is then filtered on `kind`. Deliberately not
/// [`Event`]/[`Kind`] themselves: those are the write side's exact wire shape, and a reader
/// that insisted on it would fail on a line from a future schema instead of skipping it.
#[derive(Debug, Deserialize)]
struct ReadBack {
    ts: String,
    #[serde(default)]
    branch: Option<String>,
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

/// `now` formatted as RFC3339 in UTC, e.g. `2026-09-22T13:04:05Z`. Always UTC — kanstack has
/// no reason to know the caller's local zone, and UTC sidesteps an offset that could be wrong
/// if the host's own zone is misconfigured. Hand-rolled rather than pulling in a date/time
/// crate for the one thing this needs. Also how `status --json` prints a pending ask's
/// `asked_at`, so both surfaces agree on the format.
pub(crate) fn rfc3339(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (h, m, s) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// The inverse of [`rfc3339`], for exactly the shape it writes (`2026-09-22T13:04:05Z` —
/// always UTC, always `Z`, no fractional seconds), since that's the only shape this log
/// ever holds. Anything else, or a time before the epoch, is `None` rather than a guess.
fn parse_rfc3339(ts: &str) -> Option<SystemTime> {
    let b = ts.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let num = |range: std::ops::Range<usize>| ts.get(range)?.parse::<u32>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(y as i64, mo, d);
    let secs = days.checked_mul(86_400)? + (h * 3600 + mi * 60 + s) as i64;
    Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).ok()?))
}

/// A proleptic-Gregorian (year, month, day) to days since the Unix epoch — the inverse of
/// [`civil_from_days`], also Howard Hinnant's: http://howardhinnant.github.io/date_algorithms.html.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Days since the Unix epoch to a proleptic-Gregorian (year, month, day) — Howard Hinnant's
/// `civil_from_days`: http://howardhinnant.github.io/date_algorithms.html.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_calendar_boundaries() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
        assert_eq!(civil_from_days(365), (1971, 1, 1), "1970 is not a leap year");
        assert_eq!(civil_from_days(730), (1972, 1, 1));
        assert_eq!(civil_from_days(790), (1972, 3, 1), "1972 is a leap year, so Feb has 29 days");
    }

    #[test]
    fn rfc3339_formats_the_epoch_and_a_day_with_every_field_set() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(UNIX_EPOCH + Duration::from_secs(90_061)), "1970-01-02T01:01:01Z");
    }

    #[test]
    fn a_report_and_two_lifecycle_events_round_trip_as_one_line_each() {
        let dir = std::env::temp_dir().join(format!("kanstack-events-roundtrip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = EventLog { path: Some(dir.join("events.jsonl")) };

        log.record_report("fix-login", Reported::Busy, UNIX_EPOCH + Duration::from_secs(1000));
        log.record_lifecycle(Some("fix-login"), "spawn", UNIX_EPOCH + Duration::from_secs(999));
        log.record_lifecycle(None, "prune", UNIX_EPOCH + Duration::from_secs(1001));

        let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            r#"{"schema":1,"ts":"1970-01-01T00:16:40Z","branch":"fix-login","kind":"report","state":"busy"}"#
        );
        assert_eq!(
            lines[1],
            r#"{"schema":1,"ts":"1970-01-01T00:16:39Z","branch":"fix-login","kind":"lifecycle","command":"spawn"}"#
        );
        assert_eq!(lines[2], r#"{"schema":1,"ts":"1970-01-01T00:16:41Z","kind":"lifecycle","command":"prune"}"#, "prune logs no branch");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `note`/`ask`/`answer` carry their text, unlike `report`/`lifecycle` — for a note the
    /// text is the whole event. Quotes and newlines must survive as JSON escapes, never break
    /// the one-object-per-line framing.
    #[test]
    fn a_note_an_ask_and_an_answer_round_trip_as_one_line_each_with_their_text() {
        let dir = std::env::temp_dir().join(format!("kanstack-events-orchestration-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = EventLog { path: Some(dir.join("events.jsonl")) };

        log.record_note("fix-login", "tests pass, moving on", UNIX_EPOCH + Duration::from_secs(1000));
        log.record_ask("fix-login", "ship \"it\"?\nor wait", UNIX_EPOCH + Duration::from_secs(1001));
        log.record_answer("fix-login", "ship it", UNIX_EPOCH + Duration::from_secs(1002));

        let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "an embedded newline must not split an event: {raw}");
        assert_eq!(
            lines[0],
            r#"{"schema":1,"ts":"1970-01-01T00:16:40Z","branch":"fix-login","kind":"note","text":"tests pass, moving on"}"#
        );
        assert_eq!(
            lines[1],
            r#"{"schema":1,"ts":"1970-01-01T00:16:41Z","branch":"fix-login","kind":"ask","question":"ship \"it\"?\nor wait"}"#
        );
        assert_eq!(
            lines[2],
            r#"{"schema":1,"ts":"1970-01-01T00:16:42Z","branch":"fix-login","kind":"answer","text":"ship it"}"#
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch_log(tag: &str) -> (PathBuf, EventLog) {
        let dir = std::env::temp_dir().join(format!("kanstack-events-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = EventLog { path: Some(dir.join("events.jsonl")) };
        (dir, log)
    }

    #[test]
    fn parse_rfc3339_inverts_rfc3339_and_rejects_every_other_shape() {
        for secs in [0, 1, 59, 86_399, 86_400, 90_061, 951_782_400 /* 2000-02-29 */, 1_790_000_000] {
            let t = UNIX_EPOCH + Duration::from_secs(secs);
            assert_eq!(parse_rfc3339(&rfc3339(t)), Some(t), "{}", rfc3339(t));
        }
        for bad in ["", "1970-01-01", "1970-01-01T00:00:00", "1970-01-01T00:00:00+00:00", "1970-13-01T00:00:00Z", "19x0-01-01T00:00:00Z", "1969-12-31T23:59:59Z"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn recent_notes_round_trips_one_branchs_notes_newest_first_with_their_times() {
        let (dir, log) = scratch_log("notes-roundtrip");
        let at = |s| UNIX_EPOCH + Duration::from_secs(s);
        log.record_note("review-auth", "started on the middleware", at(1000));
        log.record_note("review-auth", "found two unchecked unwraps", at(1001));

        assert_eq!(
            log.recent_notes("review-auth", 10),
            [(at(1001), "found two unchecked unwraps".to_string()), (at(1000), "started on the middleware".to_string())]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only this branch's notes: another branch's note, and this branch's own report,
    /// lifecycle, ask and answer events — which also carry a `text` in the answer's case —
    /// are all skipped, as is a line that isn't JSON at all (a half-written append).
    #[test]
    fn recent_notes_ignores_other_branches_other_kinds_and_garbage_lines() {
        let (dir, log) = scratch_log("notes-filter");
        let now = UNIX_EPOCH + Duration::from_secs(1000);
        log.record_note("review-auth", "mine", now);
        log.record_note("feat-ui", "someone else's", now);
        log.record_report("review-auth", Reported::Busy, now);
        log.record_lifecycle(Some("review-auth"), "spawn", now);
        log.record_lifecycle(None, "prune", now);
        log.record_ask("review-auth", "ship it?", now);
        log.record_answer("review-auth", "an answer is not a note", now);
        let mut f = OpenOptions::new().append(true).open(dir.join("events.jsonl")).unwrap();
        writeln!(f, "{{\"schema\":1,\"ts\":\"1970-01-01T00:16:4").unwrap();

        let texts: Vec<String> = log.recent_notes("review-auth", 10).into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts, ["mine"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_notes_keeps_only_the_newest_limit() {
        let (dir, log) = scratch_log("notes-limit");
        for i in 0..5 {
            log.record_note("review-auth", &format!("note {i}"), UNIX_EPOCH + Duration::from_secs(1000 + i));
        }
        let texts: Vec<String> = log.recent_notes("review-auth", 2).into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts, ["note 4", "note 3"]);
        assert!(log.recent_notes("review-auth", 0).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_notes_with_no_log_or_nowhere_to_keep_one_is_empty() {
        let (dir, log) = scratch_log("notes-missing");
        assert!(log.recent_notes("review-auth", 5).is_empty(), "no file written yet");
        assert!(EventLog::default().recent_notes("review-auth", 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A doorbell that can't ring must never be the reason the command it's attached to fails
    /// — see the module doc.
    #[test]
    fn with_nowhere_to_keep_events_appending_is_a_silent_no_op() {
        let log = EventLog::default();
        log.record_report("fix-login", Reported::Idle, SystemTime::now());
        log.record_lifecycle(Some("fix-login"), "stop", SystemTime::now());
        log.record_note("fix-login", "hi", SystemTime::now());
        log.record_ask("fix-login", "ok?", SystemTime::now());
        log.record_answer("fix-login", "ok", SystemTime::now());
    }
}
