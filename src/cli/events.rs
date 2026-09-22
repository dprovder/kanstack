//! `kanstack events` — read, and optionally follow, the append-only events log
//! (`crate::events`), so a caller doesn't have to poll `kanstack status` in a loop to notice a
//! report or lifecycle change. No `but`/pane interaction, like `prune` — just reads local
//! state, and (unlike every other subcommand) never touches the workstream registry either.
//!
//! Always prints raw JSONL — whatever `crate::events::EventLog` wrote, byte for byte — since
//! that already is the machine-readable shape this exists to hand a caller; `--json` here only
//! changes how a *failure* is reported (the same generic envelope every other subcommand
//! uses), not how success looks.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::workstream::events_path;

/// How often `--follow` checks the file for new bytes. A plain poll loop, not a filesystem
/// watch: this is a short-lived process the caller keeps running (like `tail -f`), not a
/// daemon, so simplicity wins over the latency a watch would save.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(super) fn run(since: u64, follow: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    let Some(path) = events_path(cwd) else {
        anyhow::bail!("no home directory to keep events in");
    };
    let mut offset = emit_new(&path, since, out)?;
    if !follow {
        return Ok(());
    }
    loop {
        std::thread::sleep(POLL_INTERVAL);
        offset = emit_new(&path, offset, out)?;
    }
}

/// Writes whatever `path` has past byte `since` to `out`, and returns the offset to resume
/// from next time — the file's current length, or `since` unchanged if the file doesn't exist
/// yet (nothing has been logged, not an error: the log is created lazily on its first append)
/// or hasn't grown past it.
fn emit_new(path: &Path, since: u64, out: &mut impl Write) -> Result<u64> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(since),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let len = file.metadata().with_context(|| format!("reading {}", path.display()))?.len();
    if len <= since {
        return Ok(since);
    }
    file.seek(SeekFrom::Start(since)).with_context(|| format!("reading {}", path.display()))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).with_context(|| format!("reading {}", path.display()))?;
    out.write_all(&buf)?;
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Command;

    fn with_state(tag: &str, body: impl FnOnce(&Path)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-cli-events-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        body(Path::new("/repo/events-cmd"));
        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_log_prints_nothing_and_is_not_an_error() {
        with_state("missing", |repo| {
            let mut out = Vec::new();
            crate::cli::run(Command::Events { since: 0, follow: false, json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty());
        });
    }

    #[test]
    fn events_prints_everything_logged_so_far_from_the_start() {
        with_state("full", |repo| {
            crate::report::Reports::for_repo(repo).write("fix-login", crate::report::Reported::Busy, std::time::SystemTime::UNIX_EPOCH).unwrap();
            crate::report::Reports::for_repo(repo).write("fix-login", crate::report::Reported::Idle, std::time::SystemTime::UNIX_EPOCH).unwrap();

            let mut out = Vec::new();
            crate::cli::run(Command::Events { since: 0, follow: false, json: false }, repo, &mut out).unwrap();
            let printed = String::from_utf8(out).unwrap();
            assert_eq!(printed.lines().count(), 2);
            assert!(printed.lines().next().unwrap().contains(r#""state":"busy""#));
            assert!(printed.lines().nth(1).unwrap().contains(r#""state":"idle""#));
        });
    }

    #[test]
    fn since_a_byte_offset_only_what_was_appended_after_it_comes_back() {
        with_state("since", |repo| {
            crate::report::Reports::for_repo(repo).write("fix-login", crate::report::Reported::Busy, std::time::SystemTime::UNIX_EPOCH).unwrap();
            let first_len = std::fs::metadata(events_path(repo).unwrap()).unwrap().len();
            crate::report::Reports::for_repo(repo).write("fix-login", crate::report::Reported::Idle, std::time::SystemTime::UNIX_EPOCH).unwrap();

            let mut out = Vec::new();
            crate::cli::run(Command::Events { since: first_len, follow: false, json: false }, repo, &mut out).unwrap();
            let printed = String::from_utf8(out).unwrap();
            assert_eq!(printed.lines().count(), 1, "{printed}");
            assert!(printed.contains(r#""state":"idle""#), "{printed}");
        });
    }

    /// A cursor past the file's current length — e.g. the log was truncated or replaced
    /// underneath a caller holding a stale offset — must read as "nothing new" rather than
    /// erroring on a seek past the end.
    #[test]
    fn a_since_offset_past_the_end_of_the_file_prints_nothing() {
        with_state("since-too-far", |repo| {
            crate::report::Reports::for_repo(repo).write("fix-login", crate::report::Reported::Busy, std::time::SystemTime::UNIX_EPOCH).unwrap();
            let mut out = Vec::new();
            crate::cli::run(Command::Events { since: 1_000_000, follow: false, json: false }, repo, &mut out).unwrap();
            assert!(out.is_empty());
        });
    }
}
