//! Fallback bridge to plain `tmux`, for a harness split on machines without `cmux` (see
//! `crate::cmux`) but where kanstack is itself running inside a real tmux pane.
//!
//! `Tmux::discover` requires `$TMUX_PANE` — the pane id tmux sets for every process it
//! spawns — precisely because the whole scheme is "split off kanstack's own pane": with no
//! tmux pane to be kanstack's own, there's nothing to split. So unlike `Cmux::discover`
//! (silent whenever the binary just isn't on `PATH`), an explicit `KANSTACK_TMUX_BIN`
//! override still comes back `None` outside tmux — there's no session-less equivalent of
//! "join whatever pane is already there" worth building for that case.
//!
//! Splitting: tmux has no cross-pane pixel geometry the way `cmux rpc pane.list` does (see
//! `cmux.rs`'s `occupant_in_direction`), so there's no equivalent pre-check for a pane
//! already occupying kanstack's split slot. Every lane after the first just chains off the
//! previously spawned pane instead — the same graceful-degradation path `cmux.rs` already
//! falls back to whenever it can't determine geometry either.
//!
//! Busy/idle: tmux's `pane_current_command` only reports the foreground process name,
//! which for a long-lived harness like `claude` never changes between generating and
//! resting at a prompt — the same limitation that pushed `cmux.rs` off process counts and
//! onto CPU% (see its own doc comment). So this polls CPU% too, summed over the pane's
//! whole process tree via `ps` rather than `cmux top`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

use crate::mux::{command_exists, Multiplexer, OpenRequest};
use crate::pane_status::{PaneStatus, CPU_BUSY_THRESHOLD_PERCENT};

/// tmux, through its own CLI. Stateless: which pane belongs to which branch is
/// `crate::splitter::Splitter`'s business, and this only knows how to act on a pane id.
#[derive(Clone)]
pub struct Tmux {
    bin: PathBuf,
    /// kanstack's own pane (`$TMUX_PANE`), read once at `discover` time — the anchor the
    /// *first* lane splits off; later lanes chain off the previous lane instead.
    own_pane: String,
}

impl Tmux {
    /// Locates the `tmux` binary and kanstack's own pane id. Returns `None` (not an error)
    /// whenever either is missing — see the module doc comment for why an explicit
    /// `KANSTACK_TMUX_BIN` doesn't change that for a missing `$TMUX_PANE`.
    pub fn discover() -> Option<Self> {
        let bin = match std::env::var_os("KANSTACK_TMUX_BIN") {
            Some(path) => PathBuf::from(path),
            None => {
                let candidate = PathBuf::from("tmux");
                if !command_exists(&candidate) {
                    return None;
                }
                candidate
            }
        };
        let own_pane = std::env::var("TMUX_PANE").ok()?;
        Some(Tmux { bin, own_pane })
    }

    /// Types `text` into `pane_id` as literal keystrokes, then presses Enter as a separate
    /// key — the documented tmux pattern for sending arbitrary text (a `-l`-less `send-keys`
    /// would risk interpreting text that happens to match a key name, like a message that's
    /// literally the word `Enter`, as that key instead of literal characters).
    fn type_and_submit(&self, pane_id: &str, text: &str) -> Result<()> {
        self.run(&["send-keys", "-t", pane_id, "-l", "--", text])?;
        self.run(&["send-keys", "-t", pane_id, "Enter"])?;
        Ok(())
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out: Output = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn `{} {}`", self.bin.display(), args.join(" ")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim();
            bail!(
                "`tmux {}` failed{}",
                args.join(" "),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Multiplexer for Tmux {
    fn name(&self) -> &'static str {
        "tmux"
    }

    fn running_inside(&self) -> bool {
        std::env::var_os("TMUX_PANE").is_some()
    }

    /// Splits off the previous lane's pane (or kanstack's own, for the first lane) with
    /// `cwd` as its working directory, types `launch` into it, then titles it (e.g. the
    /// branch name) — best-effort, since pane titles need `set -g pane-border-status` to
    /// actually be visible and an old tmux without title support shouldn't fail the whole
    /// spawn over it.
    fn open_pane(&self, req: &OpenRequest<'_>) -> Result<String> {
        let (direction, anchor) = match req.after {
            Some(anchor) => (req.chain_direction, anchor),
            None => (req.first_direction, self.own_pane.as_str()),
        };

        let cwd_str = req.cwd.to_string_lossy().into_owned();
        let mut split_args = vec!["split-window", "-t", anchor, "-c", cwd_str.as_str()];
        split_args.extend(split_flags(direction));
        split_args.extend(["-P", "-F", "#{pane_id}"]);
        let pane_id = self.run(&split_args)?.trim().to_string();
        if pane_id.is_empty() {
            bail!("`tmux split-window` did not report a pane id");
        }

        self.type_and_submit(&pane_id, req.launch)?;

        let _ = self.run(&["select-pane", "-t", &pane_id, "-T", req.title]);
        Ok(pane_id)
    }

    fn type_line(&self, pane: &str, text: &str) -> Result<()> {
        self.type_and_submit(pane, text)
    }

    /// `select-window` first, since `select-pane` alone leaves a pane in another window out
    /// of sight.
    fn focus(&self, pane: &str) -> Result<()> {
        self.run(&["select-window", "-t", pane])?;
        self.run(&["select-pane", "-t", pane])?;
        Ok(())
    }

    fn close(&self, pane: &str) -> Result<()> {
        match self.run(&["kill-pane", "-t", pane]) {
            Err(e) if !e.to_string().contains("can't find pane") => Err(e),
            _ => Ok(()),
        }
    }

    /// `tmux list-panes -a` for whether each pane still exists at all and, for those that
    /// do, its `pane_pid`; then a single `ps` call to sum CPU% over each pane_pid's whole
    /// descendant tree (see the module doc comment for why the pane's own foreground-command
    /// name can't tell busy from idle).
    fn probe(&self, panes: &[&str]) -> Result<HashMap<String, PaneStatus>> {
        if panes.is_empty() {
            return Ok(HashMap::new());
        }
        let list_out = self.run(&["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])?;
        let (present, pane_pids) = parse_pane_list(&list_out);

        let any_alive = panes.iter().any(|p| present.contains(*p));
        let ps_table = if any_alive { read_ps_table()? } else { Vec::new() };

        Ok(classify_statuses(panes, &present, &pane_pids, &ps_table))
    }
}

/// Parses `tmux list-panes -F "#{pane_id} #{pane_pid}"` output into the set of pane ids
/// still present and a pane id → pid map, split out so it can be unit tested against a
/// captured string without shelling out to a real `tmux`.
fn parse_pane_list(list_out: &str) -> (HashSet<String>, HashMap<String, u32>) {
    let mut present = HashSet::new();
    let mut pane_pids = HashMap::new();
    for line in list_out.lines() {
        let mut parts = line.split_whitespace();
        let (Some(id), Some(pid)) = (parts.next(), parts.next()) else {
            continue;
        };
        present.insert(id.to_string());
        if let Ok(pid) = pid.parse::<u32>() {
            pane_pids.insert(id.to_string(), pid);
        }
    }
    (present, pane_pids)
}

/// One row of `ps -A -o pid=,ppid=,pcpu=` output: a process, its parent, and its CPU%.
type PsRow = (u32, u32, f64);

/// Parses `ps -A -o pid=,ppid=,pcpu=` output (the `=` suffixes suppress the header row,
/// portable across macOS/BSD and GNU `ps`) into `(pid, ppid, cpu_percent)` rows.
fn parse_ps_table(text: &str) -> Vec<PsRow> {
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

fn read_ps_table() -> Result<Vec<PsRow>> {
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
fn subtree_cpu(root_pid: u32, table: &[PsRow]) -> f64 {
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

/// Pure classification step of [`Tmux::probe`], split out so it can be unit tested against
/// synthetic `list-panes`/`ps` data without shelling out to either. A pane that is present
/// but whose pid didn't come back this round is left out — no news, not a guess.
fn classify_statuses(
    panes: &[&str],
    present: &HashSet<String>,
    pane_pids: &HashMap<String, u32>,
    ps_table: &[PsRow],
) -> HashMap<String, PaneStatus> {
    panes
        .iter()
        .filter_map(|&pane| {
            let status = if !present.contains(pane) {
                PaneStatus::Dead
            } else {
                let pid = *pane_pids.get(pane)?;
                if subtree_cpu(pid, ps_table) > CPU_BUSY_THRESHOLD_PERCENT {
                    PaneStatus::Busy
                } else {
                    PaneStatus::Idle
                }
            };
            Some((pane.to_string(), status))
        })
        .collect()
}

/// tmux's `split-window` flags for `direction`: `-v` splits into top/bottom (new pane
/// below unless `-b` puts it before/above); `-h` splits into left/right (new pane to the
/// right unless `-b` puts it before/to the left).
fn split_flags(direction: &str) -> Vec<&'static str> {
    match direction {
        "up" => vec!["-v", "-b"],
        "down" => vec!["-v"],
        "left" => vec!["-h", "-b"],
        _ => vec!["-h"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `body` with each of `vars` swapped out and restored afterwards. These are
    /// process-wide state also read by `cmux.rs`'s and `splitter.rs`'s own env-mutating
    /// tests — held for the whole call via `SPLIT_BACKEND_ENV_LOCK`, not just the swap, so
    /// none of them can interleave with each other either.
    fn with_env(vars: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in vars {
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
    }

    /// No `tmux` on `PATH`, no override: `discover` must come back `None`, not an error —
    /// same contract as `Cmux::discover`.
    #[test]
    fn discover_is_none_without_tmux_on_path_or_an_override() {
        let dir = std::env::temp_dir().join(format!("kanstack-tmux-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(
            &[("PATH", Some(dir.to_str().unwrap())), ("KANSTACK_TMUX_BIN", None), ("TMUX_PANE", Some("%0"))],
            || {
                assert!(Tmux::discover().is_none());
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Even an explicit `KANSTACK_TMUX_BIN` override can't make up for kanstack not
    /// running inside a tmux pane at all — there's no "own pane" to split for the first
    /// lane, so `discover` still comes back `None`. This is the one place Tmux is
    /// *stricter* than Cmux's "an explicit override is trusted without checking it
    /// exists" — see the module doc comment for why.
    #[test]
    fn discover_is_none_without_tmux_pane_even_with_an_explicit_override() {
        with_env(&[("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")), ("TMUX_PANE", None)], || {
            assert!(Tmux::discover().is_none());
        });
    }

    #[test]
    fn discover_finds_an_explicit_override_inside_a_tmux_pane() {
        with_env(
            &[("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")), ("TMUX_PANE", Some("%3"))],
            || {
                let tmux = Tmux::discover();
                assert!(tmux.is_some());
            },
        );
    }

    #[test]
    fn split_flags_map_readable_directions_onto_tmuxs_own() {
        assert_eq!(split_flags("up"), vec!["-v", "-b"]);
        assert_eq!(split_flags("down"), vec!["-v"]);
        assert_eq!(split_flags("left"), vec!["-h", "-b"]);
        assert_eq!(split_flags("right"), vec!["-h"]);
    }

    #[test]
    fn parse_pane_list_reads_ids_and_pids() {
        let (present, pids) = parse_pane_list("%0 111\n%1 222\n");
        assert_eq!(present, HashSet::from(["%0".to_string(), "%1".to_string()]));
        assert_eq!(pids.get("%0"), Some(&111));
        assert_eq!(pids.get("%1"), Some(&222));
    }

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

    /// A tracked pane absent from `list-panes` entirely classifies as `Dead`, regardless
    /// of anything `ps` might say — mirrors `cmux.rs`'s
    /// `classify_statuses_marks_a_missing_surface_dead`.
    #[test]
    fn classify_statuses_marks_a_missing_pane_dead() {
        let statuses = classify_statuses(&["%9"], &HashSet::new(), &HashMap::new(), &[]);
        assert_eq!(statuses["%9"], PaneStatus::Dead);
    }

    #[test]
    fn classify_statuses_marks_a_high_cpu_pane_busy() {
        let present = HashSet::from(["%1".to_string()]);
        let pane_pids = HashMap::from([("%1".to_string(), 100)]);
        let ps_table = vec![(100, 1, 35.0)];
        let statuses = classify_statuses(&["%1"], &present, &pane_pids, &ps_table);
        assert_eq!(statuses["%1"], PaneStatus::Busy);
    }

    #[test]
    fn classify_statuses_marks_a_low_cpu_pane_idle() {
        let present = HashSet::from(["%2".to_string()]);
        let pane_pids = HashMap::from([("%2".to_string(), 200)]);
        let ps_table = vec![(200, 1, 0.0)];
        let statuses = classify_statuses(&["%2"], &present, &pane_pids, &ps_table);
        assert_eq!(statuses["%2"], PaneStatus::Idle);
    }

    /// Present in `list-panes` but its pid didn't come back this round (a race between the
    /// two `tmux`/`ps` calls): no news, so the caller keeps whatever it already knew rather
    /// than being handed a guess.
    #[test]
    fn classify_statuses_leaves_out_a_pane_without_a_pid() {
        let present = HashSet::from(["%3".to_string()]);
        let statuses = classify_statuses(&["%3"], &present, &HashMap::new(), &[]);
        assert!(!statuses.contains_key("%3"), "{statuses:?}");
    }
}
