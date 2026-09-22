//! An append-only, tailable log of things happening to workstreams — a report state change
//! and a lifecycle change (spawn/stop/prune) — so an external orchestrator driving kanstack
//! doesn't have to poll `kanstack status` in a loop to notice one. No daemon, no server, no
//! push channel: just a JSONL file another process can `tail -f`, or poll from a byte offset
//! with `kanstack events --since <offset>` (see `crate::cli::events`). One file per
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
//! Deliberately thin — this is a doorbell, not the payload: seeing a line is a cue to go read
//! `kanstack status --json`, not something to parse for the actual state.
//!
//! Appends are best-effort, like [`crate::report::Reports::write`]: nothing here ever returns
//! an error to its caller, so a write failure never breaks the command it's attached to.
//!
//! No rotation or size limit — a known limitation, not an oversight. Add one if a repo's log
//! ever actually grows enough to matter; nothing here reads the whole file into memory except
//! `kanstack events` itself, which only reads what's new since its cursor.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

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
}

/// `now` formatted as RFC3339 in UTC, e.g. `2026-09-22T13:04:05Z`. Always UTC — kanstack has
/// no reason to know the caller's local zone, and UTC sidesteps an offset that could be wrong
/// if the host's own zone is misconfigured. Hand-rolled rather than pulling in a date/time
/// crate for the one thing this needs.
fn rfc3339(now: SystemTime) -> String {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO).as_secs();
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;
    let (h, m, s) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
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

    /// A doorbell that can't ring must never be the reason the command it's attached to fails
    /// — see the module doc.
    #[test]
    fn with_nowhere_to_keep_events_appending_is_a_silent_no_op() {
        let log = EventLog::default();
        log.record_report("fix-login", Reported::Idle, SystemTime::now());
        log.record_lifecycle(Some("fix-login"), "stop", SystemTime::now());
    }
}
