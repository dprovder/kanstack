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
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};

use crate::harness_launch::{
    launch_line, resolve_note_delivery, resolve_note_delivery_for_override, NoteDelivery,
};
use crate::pane_status::{PaneStatus, CPU_BUSY_THRESHOLD_PERCENT};

#[derive(Debug, Clone)]
struct PaneHandle {
    /// tmux's own pane identifier, e.g. `"%3"` — stable across resizes and reflows, unlike
    /// a position-based `-t session:window.pane` target.
    pane_id: String,
    status: PaneStatus,
}

#[derive(Clone)]
pub struct Tmux {
    bin: PathBuf,
    /// Shell command typed into the new pane, e.g. `"claude"` or `"codex"`.
    harness: String,
    note_delivery: NoteDelivery,
    /// kanstack's own pane (`$TMUX_PANE`), read once at `discover` time — the anchor the
    /// *first* lane splits off; later lanes chain off `last_anchor` instead.
    own_pane: String,
    /// Split direction for the first lane, off `own_pane`.
    direction: String,
    /// Split direction for every lane after the first, off the previous lane.
    chain_direction: String,
    last_anchor: Option<String>,
    panes: HashMap<String, PaneHandle>,
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
        let harness = std::env::var("KANSTACK_HARNESS").unwrap_or_else(|_| "claude".to_string());
        let note_delivery = resolve_note_delivery(&harness);
        let direction = std::env::var("KANSTACK_TMUX_DIRECTION")
            .map(|raw| normalize_direction(&raw))
            .unwrap_or_else(|_| "up".to_string());
        let chain_direction = std::env::var("KANSTACK_TMUX_CHAIN_DIRECTION")
            .map(|raw| normalize_direction(&raw))
            .unwrap_or_else(|_| "right".to_string());
        Some(Tmux {
            bin,
            harness,
            note_delivery,
            own_pane,
            direction,
            chain_direction,
            last_anchor: None,
            panes: HashMap::new(),
        })
    }

    /// Whether kanstack has ever opened a pane for `branch` (regardless of its current
    /// status) — used to decide whether task dispatch needs to spawn one first.
    pub fn has_pane(&self, branch: &str) -> bool {
        self.panes.contains_key(branch)
    }

    /// Last known status of `branch`'s pane. `None` if kanstack has never tracked one for
    /// it at all (as opposed to `Some(PaneStatus::Dead)`, which means one existed and has
    /// since disappeared from `list-panes`).
    pub fn pane_status(&self, branch: &str) -> Option<PaneStatus> {
        self.panes.get(branch).map(|p| p.status)
    }

    /// No panes tracked at all — nothing worth polling.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Splits off the previous lane's pane (or kanstack's own, for the first lane), types
    /// the configured harness command into the fresh pane with `cwd` as its working
    /// directory, then titles it `name` (e.g. the branch name) — best-effort, since pane
    /// titles need `set -g pane-border-status` to actually be visible and an old tmux
    /// without title support shouldn't fail the whole spawn over it.
    ///
    /// See `build_launch_command`/[`crate::harness_launch`] for `initial_message` and the
    /// branch-context note, identical to `Cmux::spawn_harness`'s own handling of both.
    pub fn spawn_harness(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<String> {
        self.spawn_harness_with(cwd, name, initial_message, None)
    }

    /// [`Self::spawn_harness`] with `harness` overriding the configured one for just this
    /// pane (`kanstack spawn --agent`), including which delivery the branch-context note
    /// uses — that depends on the harness, not on what was configured. Returns the new
    /// pane's id, for a caller that needs to find it again from another process.
    pub fn spawn_harness_with(
        &mut self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
    ) -> Result<String> {
        let (direction, anchor) = match &self.last_anchor {
            Some(anchor) => (self.chain_direction.as_str(), anchor.as_str()),
            None => (self.direction.as_str(), self.own_pane.as_str()),
        };

        let cwd_str = cwd.to_string_lossy().into_owned();
        let mut split_args = vec!["split-window", "-t", anchor, "-c", cwd_str.as_str()];
        split_args.extend(split_flags(direction));
        split_args.extend(["-P", "-F", "#{pane_id}"]);
        let pane_id = self.run(&split_args)?.trim().to_string();
        if pane_id.is_empty() {
            bail!("`tmux split-window` did not report a pane id");
        }

        let (harness, note_delivery) = match harness {
            Some(h) if h != self.harness => (h, resolve_note_delivery_for_override(h)),
            Some(_) | None => (self.harness.as_str(), self.note_delivery.clone()),
        };
        let launch = launch_line(cwd, harness, &note_delivery, name, initial_message)?;
        self.type_and_submit(&pane_id, launch.trim_end_matches('\n'))?;

        let _ = self.run(&["select-pane", "-t", &pane_id, "-T", name]);

        self.panes.insert(name.to_string(), PaneHandle { pane_id: pane_id.clone(), status: PaneStatus::Unknown });
        self.last_anchor = Some(pane_id.clone());
        Ok(pane_id)
    }

    /// Starts tracking a pane some other process opened, so `send_task`, `poll_statuses`,
    /// `focus` and `stop` work on it.
    pub fn adopt(&mut self, branch: &str, pane_id: &str) {
        self.panes.insert(branch.to_string(), PaneHandle { pane_id: pane_id.to_string(), status: PaneStatus::Unknown });
    }

    /// tmux pane ids are global and `$TMUX_PANE` names the caller's own, so there's no
    /// workspace to pin — kept so `Splitter` needn't know which backend it holds.
    pub fn set_workspace(&mut self, _workspace: Option<&str>) {}

    pub fn workspace(&self) -> Option<String> {
        None
    }

    /// Overrides the first-lane split direction, which `discover` read from `KANSTACK_TMUX_DIRECTION`.
    /// For a caller with its own setting — the `kanstack spawn` subcommand has
    /// `KANSTACK_SPAWN_DIRECTION`, since the board's `above` puts lanes over kanstack's own
    /// pane and an agent's pane is usually somewhere else.
    pub fn set_first_direction(&mut self, direction: &str) {
        self.direction = normalize_direction(direction);
    }

    /// Makes the next spawn split off `pane_id` rather than kanstack's own pane, the way
    /// consecutive spawns within one process already chain.
    pub fn set_anchor(&mut self, pane_id: &str) {
        self.last_anchor = Some(pane_id.to_string());
    }

    /// The id of `branch`'s tracked pane.
    pub fn pane_id(&self, branch: &str) -> Option<String> {
        self.panes.get(branch).map(|p| p.pane_id.clone())
    }

    /// Brings `branch`'s pane to the front and gives it keyboard focus. `select-window`
    /// first, since `select-pane` alone leaves a pane in another window out of sight.
    pub fn focus(&self, branch: &str) -> Result<()> {
        let Some(pane) = self.panes.get(branch) else {
            bail!("no tmux pane open for {branch} yet");
        };
        self.run(&["select-window", "-t", &pane.pane_id])?;
        self.run(&["select-pane", "-t", &pane.pane_id])?;
        Ok(())
    }

    /// Closes `branch`'s pane, ending whatever harness is running in it, and stops
    /// tracking it. Already gone counts as stopped.
    pub fn stop(&mut self, branch: &str) -> Result<()> {
        let Some(pane) = self.panes.remove(branch) else {
            bail!("no tmux pane open for {branch} yet");
        };
        if self.last_anchor.as_deref() == Some(pane.pane_id.as_str()) {
            self.last_anchor = None;
        }
        match self.run(&["kill-pane", "-t", &pane.pane_id]) {
            Err(e) if !e.to_string().contains("can't find pane") => Err(e),
            _ => Ok(()),
        }
    }

    /// Sends `text` followed by Enter into `branch`'s tracked pane — the same
    /// literal-then-Enter sequence `spawn_harness` already uses to type the launch command,
    /// just generalized to target a pane recorded earlier rather than the one just created.
    pub fn send_task(&self, branch: &str, text: &str) -> Result<()> {
        let Some(pane) = self.panes.get(branch) else {
            bail!("no tmux pane open for {branch} yet");
        };
        self.type_and_submit(&pane.pane_id, text)
    }

    /// Re-derives every tracked pane's status: `tmux list-panes -a` for whether the pane
    /// still exists at all and, for those that do, its `pane_pid`; then a single `ps` call
    /// to sum CPU% over each pane_pid's whole descendant tree (see the module doc comment
    /// for why the pane's own foreground-command name can't tell busy from idle).
    ///
    /// Pure and read-only on `self` — safe to call from a background thread against a
    /// cloned snapshot.
    pub fn poll_statuses(&self) -> Result<HashMap<String, PaneStatus>> {
        if self.panes.is_empty() {
            return Ok(HashMap::new());
        }
        let list_out = self.run(&["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])?;
        let (present, pane_pids) = parse_pane_list(&list_out);

        let any_alive = self.panes.values().any(|p| present.contains(&p.pane_id));
        let ps_table = if any_alive { read_ps_table()? } else { Vec::new() };

        Ok(classify_statuses(&self.panes, &present, &pane_pids, &ps_table))
    }

    /// Merges a `poll_statuses` result back in, keyed by branch. Entries for lanes deleted
    /// since the poll started are simply absent from `self.panes` and are ignored.
    pub fn apply_statuses(&mut self, statuses: HashMap<String, PaneStatus>) {
        for (branch, status) in statuses {
            if let Some(pane) = self.panes.get_mut(&branch) {
                pane.status = status;
            }
        }
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

/// Pure classification step of [`Tmux::poll_statuses`], split out so it can be unit tested
/// against synthetic `list-panes`/`ps` data without shelling out to either.
fn classify_statuses(
    panes: &HashMap<String, PaneHandle>,
    present: &HashSet<String>,
    pane_pids: &HashMap<String, u32>,
    ps_table: &[PsRow],
) -> HashMap<String, PaneStatus> {
    panes
        .iter()
        .map(|(branch, pane)| {
            let status = if !present.contains(&pane.pane_id) {
                PaneStatus::Dead
            } else {
                match pane_pids.get(&pane.pane_id) {
                    Some(&pid) => {
                        if subtree_cpu(pid, ps_table) > CPU_BUSY_THRESHOLD_PERCENT {
                            PaneStatus::Busy
                        } else {
                            PaneStatus::Idle
                        }
                    }
                    // Pane present but its pid didn't come back this round: keep whatever
                    // was already known rather than guessing.
                    None => pane.status,
                }
            };
            (branch.clone(), status)
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

/// Accepts the more readable `above`/`below` alongside tmux's own `up`/`down`/`left`/
/// `right` framing, matching `cmux.rs`'s `normalize_direction`.
fn normalize_direction(raw: &str) -> String {
    match raw {
        "above" => "up",
        "below" => "down",
        other => other,
    }
    .to_string()
}

fn command_exists(bin: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin).is_file())
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
    fn normalize_direction_accepts_above_and_below() {
        assert_eq!(normalize_direction("above"), "up");
        assert_eq!(normalize_direction("below"), "down");
        assert_eq!(normalize_direction("left"), "left");
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
        let panes = HashMap::from([(
            "feature-a".to_string(),
            PaneHandle { pane_id: "%9".to_string(), status: PaneStatus::Busy },
        )]);
        let present = HashSet::new();
        let statuses = classify_statuses(&panes, &present, &HashMap::new(), &[]);
        assert_eq!(statuses["feature-a"], PaneStatus::Dead);
    }

    #[test]
    fn classify_statuses_marks_a_high_cpu_pane_busy() {
        let panes = HashMap::from([(
            "feature-a".to_string(),
            PaneHandle { pane_id: "%1".to_string(), status: PaneStatus::Unknown },
        )]);
        let present = HashSet::from(["%1".to_string()]);
        let pane_pids = HashMap::from([("%1".to_string(), 100)]);
        let ps_table = vec![(100, 1, 35.0)];
        let statuses = classify_statuses(&panes, &present, &pane_pids, &ps_table);
        assert_eq!(statuses["feature-a"], PaneStatus::Busy);
    }

    #[test]
    fn classify_statuses_marks_a_low_cpu_pane_idle() {
        let panes = HashMap::from([(
            "feature-b".to_string(),
            PaneHandle { pane_id: "%2".to_string(), status: PaneStatus::Unknown },
        )]);
        let present = HashSet::from(["%2".to_string()]);
        let pane_pids = HashMap::from([("%2".to_string(), 200)]);
        let ps_table = vec![(200, 1, 0.0)];
        let statuses = classify_statuses(&panes, &present, &pane_pids, &ps_table);
        assert_eq!(statuses["feature-b"], PaneStatus::Idle);
    }

    /// Present in `list-panes` but its pid didn't come back this round (a race between the
    /// two `tmux`/`ps` calls): keep whatever was already known rather than guessing.
    #[test]
    fn classify_statuses_keeps_prior_status_without_a_pid() {
        let panes = HashMap::from([(
            "feature-c".to_string(),
            PaneHandle { pane_id: "%3".to_string(), status: PaneStatus::Busy },
        )]);
        let present = HashSet::from(["%3".to_string()]);
        let statuses = classify_statuses(&panes, &present, &HashMap::new(), &[]);
        assert_eq!(statuses["feature-c"], PaneStatus::Busy);
    }
}
