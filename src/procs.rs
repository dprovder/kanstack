//! What can be learned about a pane from the process table alone, for a multiplexer that
//! can't say for itself whether a pane's harness is busy, idle or still there — see
//! [`crate::mux::Multiplexer::tracks_pids`]. `crate::mux::tmux` reads the same table for the same
//! CPU sum, so the helpers live here rather than in either.
//!
//! One `ps -A -o pid=,ppid=,pcpu=` call yields every process's parent and CPU%, and from
//! that the CPU of any process's whole subtree, or whether it is there at all.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};

use crate::harness_launch::shell_quote;
use crate::pane_status::{PaneStatus, CPU_BUSY_THRESHOLD_PERCENT};
use crate::workstream::{fnv1a, pids_dir};

/// One row of `ps -A -o pid=,ppid=,pcpu=` output: a process, its parent, and its CPU%.
pub type PsRow = (u32, u32, f64);

/// Parses `ps -A -o pid=,ppid=,pcpu=` output (the `=` suffixes suppress the header row,
/// portable across macOS/BSD and GNU `ps`) into `(pid, ppid, cpu_percent)` rows.
pub fn parse_ps_table(text: &str) -> Vec<PsRow> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid = cols.next()?.parse().ok()?;
            let ppid = cols.next()?.parse().ok()?;
            let cpu = cols.next()?.parse().ok()?;
            Some((pid, ppid, cpu))
        })
        .collect()
}

pub fn read_ps_table() -> Result<Vec<PsRow>> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pcpu="])
        .output()
        .context("failed to spawn `ps`")?;
    if !out.status.success() {
        bail!("`ps` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse_ps_table(&String::from_utf8_lossy(&out.stdout)))
}

/// Sums CPU% over `root_pid` and every one of its descendants in `table` — a harness
/// process (e.g. `claude`) that spends its CPU in child processes rather than itself would
/// otherwise read as idle even mid-generation.
pub fn subtree_cpu(root_pid: u32, table: &[PsRow]) -> f64 {
    let mut total = 0.0;
    let mut frontier = vec![root_pid];
    while let Some(pid) = frontier.pop() {
        for &(row_pid, ppid, cpu) in table {
            if row_pid == pid {
                total += cpu;
            }
            if ppid == pid && row_pid != pid {
                frontier.push(row_pid);
            }
        }
    }
    total
}

/// Whether `pid` is in `table` at all — the process is still running, or at least still has
/// an entry (a zombie awaiting its parent's `wait` does, and so reads as present).
pub fn pid_present(pid: u32, table: &[PsRow]) -> bool {
    table.iter().any(|&(row_pid, _, _)| row_pid == pid)
}

/// What the process table says about a pane whose shell recorded `pid`: `Dead` if that shell
/// is gone from it, otherwise `Busy` when the shell and everything under it together use more
/// than [`CPU_BUSY_THRESHOLD_PERCENT`], else `Idle`.
///
/// It is the pane's *shell* that is followed, not the harness: on a multiplexer whose panes end
/// with the harness there is no difference, but where the shell outlives it a harness that has
/// finished reads `Idle`, not `Dead`.
pub fn reading_from_pid(pid: u32, table: &[PsRow]) -> PaneStatus {
    if !pid_present(pid, table) {
        PaneStatus::Dead
    } else if subtree_cpu(pid, table) > CPU_BUSY_THRESHOLD_PERCENT {
        PaneStatus::Busy
    } else {
        PaneStatus::Idle
    }
}

/// Where a [`crate::splitter::Splitter`] gets the process table. The real one runs `ps`; a
/// test hands it a fixed table, so nothing under test ever depends on what is running.
pub type PsReader = Arc<dyn Fn() -> Result<Vec<PsRow>> + Send + Sync>;

/// A [`PsReader`] that runs `ps`.
pub fn real_ps() -> PsReader {
    Arc::new(read_ps_table)
}

/// `root` and every process below it in `table`, each once: what has to be ended to end a
/// harness that has children of its own. Empty if `root` isn't in the table. A table whose
/// parent links loop (a stale or torn read) still terminates.
pub fn subtree(root: u32, table: &[PsRow]) -> Vec<u32> {
    if !pid_present(root, table) {
        return Vec::new();
    }
    let mut found = vec![root];
    let mut frontier = vec![root];
    while let Some(pid) = frontier.pop() {
        for &(row_pid, ppid, _) in table {
            if ppid == pid && row_pid != pid && !found.contains(&row_pid) {
                found.push(row_pid);
                frontier.push(row_pid);
            }
        }
    }
    found
}

/// How long a process has been running, from `ps -o etime=`, whose output is
/// `[[dd-]hh:]mm:ss`. `None` for anything else.
pub fn parse_etime(text: &str) -> Option<Duration> {
    let text = text.trim();
    let (days, clock) = match text.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, text),
    };
    let parts: Vec<u64> = clock.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let seconds = match parts.as_slice() {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(Duration::from_secs(days * 86_400 + seconds))
}

/// Where a [`crate::splitter::Splitter`] learns how long a process has been running, so it can
/// tell the shell it recorded from a stranger that has since been given the same pid. `None`
/// means there is no such process (or `ps` couldn't say).
pub type AgeReader = Arc<dyn Fn(u32) -> Option<Duration> + Send + Sync>;

/// An [`AgeReader`] that runs `ps -o etime=`.
pub fn real_age() -> AgeReader {
    Arc::new(|pid| {
        let out = Command::new("ps").args(["-o", "etime=", "-p", &pid.to_string()]).output().ok()?;
        out.status.success().then(|| parse_etime(&String::from_utf8_lossy(&out.stdout))).flatten()
    })
}

/// Where a [`crate::splitter::Splitter`] ends processes. The real one sends `SIGTERM`; a test
/// records what it was asked to end instead.
pub type Killer = Arc<dyn Fn(&[u32]) + Send + Sync>;

/// A [`Killer`] that runs `kill -TERM`. Best effort by design: a process that has already gone
/// is the outcome wanted, so there is nothing to report.
pub fn real_killer() -> Killer {
    Arc::new(|pids| {
        if pids.is_empty() {
            return;
        }
        let _ = Command::new("kill")
            .arg("-TERM")
            .args(pids.iter().map(u32::to_string))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    })
}

/// Whether a process that has run for `age` (as of `now`) can be the shell that wrote a pid file
/// at `written`: it must have been running by then. The shell writes the file the moment it
/// starts, so a process that began after that is a different one that inherited the pid — the
/// operating system reuses them — and ending it would be ending a stranger. `etime` counts
/// whole seconds, so a couple are allowed.
pub fn started_before(age: Duration, now: SystemTime, written: SystemTime) -> bool {
    const SLACK: Duration = Duration::from_secs(2);
    match now.checked_sub(age) {
        Some(started) => started <= written + SLACK,
        None => true, // said to have run longer than the clock goes back: certainly older
    }
}

/// Whether process tracking applies, given what the multiplexer asks for and what
/// `KANSTACK_TRACK_PIDS` says. `1`, `true`, `on` and `yes` force it on, whichever multiplexer
/// this is (which is how it can be tried on one that needs no help); `0`, `false`, `off` and
/// `no` force it off; anything else, or nothing, leaves it to the multiplexer.
pub fn tracking_applies(mux_asks: bool, setting: Option<&str>) -> bool {
    match setting.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("1" | "true" | "on" | "yes") => true,
        Some("0" | "false" | "off" | "no") => false,
        _ => mux_asks,
    }
}

/// The text that goes in front of a launch line so the pane's own shell writes its pid to
/// `file` before running the harness: `sh -c 'printf %s "$PPID" > "$0"' '<file>' && `.
///
/// `$PPID` is the parent of the `sh` that runs it, which is the pane's shell — whichever
/// shell that is, fish included, unlike `$$`, which fish spells differently. The path is
/// passed as `sh`'s `$0` rather than written into the script, so it is quoted once, for the
/// shell the pane runs, and never has to survive being quoted inside a quote.
pub fn record_pid_prefix(file: &Path) -> String {
    format!("sh -c 'printf %s \"$PPID\" > \"$0\"' {} && ", shell_quote(&file.to_string_lossy()))
}

/// Where the shell pid of each branch's pane is kept, when it is being tracked: one small file
/// per branch, holding the pid in decimal. Like [`crate::report::Reports`], and for the same
/// reasons: it is written by another process (the pane's own shell, from the launch line), so
/// it can't live in the registry, which is rewritten whole. Cheap to clone; a default one
/// (no directory) reads nothing and records nowhere, which is what tests and a machine with no
/// home directory get.
#[derive(Debug, Clone, Default)]
pub struct Pids {
    dir: Option<PathBuf>,
}

impl Pids {
    pub fn for_repo(repo: &Path) -> Self {
        Pids { dir: pids_dir(repo) }
    }

    #[cfg(test)]
    pub(crate) fn in_dir(dir: PathBuf) -> Self {
        Pids { dir: Some(dir) }
    }

    /// One file per branch, named by a hash of it so any branch name is a valid file name and
    /// none can collide with another.
    fn file(&self, branch: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{:016x}.pid", fnv1a(branch.as_bytes()))))
    }

    /// The file `branch`'s pane should write its shell's pid to, with its directory made — the
    /// shell that writes it would not create one. `None` when there is nowhere to keep it.
    pub fn prepare(&self, branch: &str) -> Result<Option<PathBuf>> {
        let Some(file) = self.file(branch) else { return Ok(None) };
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        Ok(Some(file))
    }

    /// The pid `branch`'s pane's shell recorded, if it has. A missing, empty or malformed file is
    /// "not recorded (yet)" — the shell writes it a moment after the pane opens, and a pid that
    /// can't be read must never stand in for one that can.
    pub fn read(&self, branch: &str) -> Option<u32> {
        let raw = std::fs::read_to_string(self.file(branch)?).ok()?;
        raw.trim().parse().ok().filter(|pid| *pid != 0)
    }

    /// When `branch`'s pid was recorded: when its file was last written.
    pub fn written_at(&self, branch: &str) -> Option<SystemTime> {
        std::fs::metadata(self.file(branch)?).ok()?.modified().ok()
    }

    /// Drops the pid recorded for `branch`, because it was about a pane that no longer exists:
    /// a new pane must not be read through the old one's shell.
    pub fn forget(&self, branch: &str) {
        if let Some(file) = self.file(branch) {
            let _ = std::fs::remove_file(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_table_reads_pid_ppid_cpu() {
        let table = parse_ps_table("  111   1   0.0\n  222 111  35.4\n");
        assert_eq!(table, vec![(111, 1, 0.0), (222, 111, 35.4)]);
    }

    /// The harness's own CPU can sit near zero while it's actually busy generating, if all
    /// the work happens in a child process — a real shape for e.g. a harness that shells
    /// out to a language server or a build. Summing over the whole subtree catches that;
    /// looking at `root_pid` alone would miss it.
    #[test]
    fn subtree_cpu_sums_a_harnesss_child_processes_too() {
        // shell(pid 100) -> claude(pid 200) -> ripgrep(pid 300), all descendants of 100.
        let table = vec![(100, 1, 0.1), (200, 100, 0.5), (300, 200, 40.0)];
        assert_eq!(subtree_cpu(100, &table), 40.6);
    }

    #[test]
    fn subtree_cpu_ignores_unrelated_processes() {
        let table = vec![(100, 1, 5.0), (999, 1, 90.0)];
        assert_eq!(subtree_cpu(100, &table), 5.0);
    }

    #[test]
    fn pid_present_is_whether_the_table_lists_it_as_a_process_not_a_parent() {
        let table = vec![(100, 1, 0.0), (200, 100, 0.0)];
        assert!(pid_present(100, &table));
        assert!(pid_present(200, &table));
        assert!(!pid_present(1, &table), "init is only named as a parent here, it has no row");
        assert!(!pid_present(300, &table));
        assert!(!pid_present(100, &[]));
    }

    // The pid file and the launch prefix that fills it.

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kanstack-procs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_pid_a_shell_wrote_reads_back_and_forgetting_it_leaves_nothing() {
        let pids = Pids::in_dir(scratch("roundtrip"));
        assert_eq!(pids.read("feat-a"), None, "nothing recorded yet");
        let file = pids.prepare("feat-a").unwrap().unwrap();
        assert_eq!(pids.read("feat-a"), None, "the shell has not written it yet");
        std::fs::write(&file, "4242").unwrap();
        assert_eq!(pids.read("feat-a"), Some(4242), "printf %s leaves no newline, but one would be fine too");
        std::fs::write(&file, "4243\n").unwrap();
        assert_eq!(pids.read("feat-a"), Some(4243));
        assert_eq!(pids.read("feat-b"), None, "one file per branch");
        pids.forget("feat-a");
        assert_eq!(pids.read("feat-a"), None);
        assert!(!file.exists());
        pids.forget("feat-a"); // already gone is fine
    }

    /// A pid that can't be read must never stand in for one that can: a shell writes the file a
    /// moment after the pane opens, so a reader can see it empty.
    #[test]
    fn an_empty_or_malformed_pid_file_reads_as_not_recorded() {
        let pids = Pids::in_dir(scratch("malformed"));
        let file = pids.prepare("feat-a").unwrap().unwrap();
        for junk in ["", "\n", "abc", "-5", "12 34", "0"] {
            std::fs::write(&file, junk).unwrap();
            assert_eq!(pids.read("feat-a"), None, "{junk:?}");
        }
    }

    #[test]
    fn a_branch_name_with_slashes_still_gets_a_file_of_its_own() {
        let pids = Pids::in_dir(scratch("slashes"));
        let (a, b) = (pids.prepare("feat/a").unwrap().unwrap(), pids.prepare("feat/b").unwrap().unwrap());
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent(), "both directly in the directory: {a:?}");
        assert!(a.to_string_lossy().ends_with(".pid"));
    }

    #[test]
    fn with_nowhere_to_keep_them_nothing_is_recorded_and_nothing_fails() {
        let pids = Pids::default();
        assert_eq!(pids.prepare("feat-a").unwrap(), None);
        assert_eq!(pids.read("feat-a"), None);
        pids.forget("feat-a");
    }

    #[test]
    fn preparing_a_file_makes_its_directory_and_replaces_nothing() {
        let dir = scratch("prepare");
        let pids = Pids::in_dir(dir.join("deep").join("er"));
        let file = pids.prepare("feat-a").unwrap().unwrap();
        assert!(file.parent().unwrap().is_dir());
        assert!(!file.exists(), "it is for the pane's shell to write");
    }

    #[test]
    fn tracking_follows_the_multiplexer_unless_the_setting_says_otherwise() {
        for on in ["1", "true", "on", "yes", "ON", " Yes ", "True"] {
            assert!(tracking_applies(false, Some(on)), "{on:?} forces it on");
            assert!(tracking_applies(true, Some(on)), "{on:?}");
        }
        for off in ["0", "false", "off", "no", "OFF", " No "] {
            assert!(!tracking_applies(true, Some(off)), "{off:?} forces it off, even for a multiplexer that asks");
            assert!(!tracking_applies(false, Some(off)), "{off:?}");
        }
        for neither in [None, Some(""), Some("maybe"), Some("2")] {
            assert!(tracking_applies(true, neither), "{neither:?} leaves it to the multiplexer");
            assert!(!tracking_applies(false, neither), "{neither:?}");
        }
    }

    #[test]
    fn the_launch_prefix_passes_the_path_as_a_word_of_its_own() {
        assert_eq!(
            record_pid_prefix(Path::new("/state/pids-1/ab.pid")),
            r#"sh -c 'printf %s "$PPID" > "$0"' '/state/pids-1/ab.pid' && "#
        );
        assert_eq!(
            record_pid_prefix(Path::new("/tmp/a b/it's.pid")),
            r#"sh -c 'printf %s "$PPID" > "$0"' '/tmp/a b/it'\''s.pid' && "#
        );
    }

    /// Run by a real shell: the file gets the pid of the shell that ran the prefix — what
    /// `$PPID` inside `sh -c` is — even when the path has a space and a quote in it, and the
    /// command after the prefix still runs.
    #[test]
    fn the_launch_prefix_records_the_pid_of_the_shell_that_ran_it() {
        let file = scratch("real-shell").join("it's a dir").join("x.pid");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{}echo $$", record_pid_prefix(&file)))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let shell_pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(shell_pid.parse::<u32>().is_ok(), "{shell_pid:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), shell_pid);
    }

    /// If the file can't be written, the launch stops there rather than running the harness with
    /// nothing tracking it — the shell says why, in the pane.
    #[test]
    fn the_launch_prefix_stops_the_launch_when_the_file_cannot_be_written() {
        let file = scratch("no-dir").join("missing").join("x.pid");
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{}echo ran", record_pid_prefix(&file)))
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    }

    /// The three readings, and what each is made of.
    #[test]
    fn a_pid_reads_dead_when_absent_and_otherwise_busy_above_the_threshold_and_idle_at_or_below() {
        let table = vec![(100, 1, 0.0), (200, 100, CPU_BUSY_THRESHOLD_PERCENT)];
        assert_eq!(reading_from_pid(100, &table), PaneStatus::Idle, "exactly the threshold");
        let table = vec![(100, 1, 0.0), (200, 100, CPU_BUSY_THRESHOLD_PERCENT + 0.01)];
        assert_eq!(reading_from_pid(100, &table), PaneStatus::Busy, "the child's CPU counts");
        assert_eq!(reading_from_pid(200, &table), PaneStatus::Busy);
        assert_eq!(reading_from_pid(300, &table), PaneStatus::Dead);
        assert_eq!(reading_from_pid(100, &[]), PaneStatus::Dead, "an empty table has no shell in it");
    }

    // Ending a pane's processes.

    #[test]
    fn subtree_is_the_root_and_everything_below_it_and_nothing_else() {
        //   100 ── 101 ── 102        200 (unrelated)
        //     └─── 103               99  (100's parent)
        let table: Vec<PsRow> = vec![(99, 1, 0.0), (100, 99, 0.0), (101, 100, 0.0), (102, 101, 0.0), (103, 100, 0.0), (200, 1, 0.0)];
        let mut found = subtree(100, &table);
        found.sort_unstable();
        assert_eq!(found, [100, 101, 102, 103], "not its parent, not the unrelated process");
        assert_eq!(subtree(102, &table), [102], "a leaf is just itself");
        assert!(subtree(4242, &table).is_empty(), "a process that isn't there has nothing to end");
    }

    #[test]
    fn subtree_terminates_on_a_table_whose_parent_links_loop() {
        let table: Vec<PsRow> = vec![(1, 2, 0.0), (2, 1, 0.0), (3, 3, 0.0)];
        let mut found = subtree(1, &table);
        found.sort_unstable();
        assert_eq!(found, [1, 2]);
        assert_eq!(subtree(3, &table), [3], "a process that is its own parent");
    }

    #[test]
    fn etime_parses_every_shape_ps_prints() {
        assert_eq!(parse_etime("   05:23\n"), Some(Duration::from_secs(323)));
        assert_eq!(parse_etime("01:05:23"), Some(Duration::from_secs(3923)));
        assert_eq!(parse_etime("2-03:04:05"), Some(Duration::from_secs(2 * 86_400 + 3 * 3600 + 4 * 60 + 5)));
        assert_eq!(parse_etime("00:00"), Some(Duration::ZERO));
        for bad in ["", "   ", "abc", "12", "1:2:3:4", "x-01:02"] {
            assert_eq!(parse_etime(bad), None, "{bad:?}");
        }
    }

    /// The reason for the check: pids are reused, and a recorded one that now names an unrelated
    /// process must not be ended.
    #[test]
    fn a_process_that_started_after_the_pid_was_recorded_is_a_stranger() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let written = now - Duration::from_secs(3600); // the pane opened an hour ago
        assert!(started_before(Duration::from_secs(3600), now, written), "the shell that wrote it");
        assert!(started_before(Duration::from_secs(3601), now, written), "or one started slightly earlier");
        assert!(started_before(Duration::from_secs(3599), now, written), "etime counts whole seconds");
        assert!(!started_before(Duration::from_secs(3590), now, written), "started ten seconds after: not it");
        assert!(!started_before(Duration::from_secs(5), now, written), "started five seconds ago: a stranger holding a reused pid");
        assert!(started_before(Duration::from_secs(u64::MAX / 2), now, written), "older than the clock: certainly earlier");
    }

    #[test]
    fn a_pid_files_age_is_when_it_was_written() {
        let dir = std::env::temp_dir().join(format!("kanstack-pids-age-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pids = Pids::in_dir(dir.clone());
        assert_eq!(pids.written_at("feat-a"), None, "nothing recorded");
        let file = pids.prepare("feat-a").unwrap().unwrap();
        std::fs::write(&file, "1234").unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(long_ago).unwrap();
        let written = pids.written_at("feat-a").unwrap();
        assert!(written.duration_since(long_ago).unwrap_or_default() < Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real functions, against a real process: `ps` reports how long it has run, `kill` ends
    /// it, and afterwards `ps` no longer knows it. Nothing here is a stand-in.
    #[test]
    fn the_real_age_reader_and_killer_end_a_real_process() {
        let mut child = Command::new("sleep").arg("300").spawn().expect("sleep starts");
        let pid = child.id();
        let age = real_age()(pid).expect("ps knows a process that is running");
        assert!(age < Duration::from_secs(30), "a process started a moment ago: {age:?}");
        assert!(started_before(age, SystemTime::now(), SystemTime::now()), "it started before now");

        real_killer()(&[pid]);
        let status = child.wait().expect("the child ends");
        assert!(!status.success(), "it was ended by the signal, not by finishing: {status:?}");
        assert_eq!(real_age()(pid), None, "and ps no longer knows it");
        real_killer()(&[pid]); // ending what is already gone is not an error
        real_killer()(&[]);
    }
}
