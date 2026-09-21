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
use crate::procs::{read_ps_table, subtree_cpu, PsRow};

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

/// One line for the setup wizard: whether tmux is usable here and, if not, what to do.
pub fn detection() -> String {
    match Tmux::discover() {
        Some(_) => "✓ tmux found, and this pane is inside one".to_string(),
        None if std::env::var_os("TMUX_PANE").is_none() => {
            "✗ tmux not usable here — not running inside a tmux pane".to_string()
        }
        None => "✗ tmux binary not found on PATH".to_string(),
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

    // What follows drives the real spawn/send/focus/stop/poll paths, through a `Splitter` as
    // kanstack does, against a stand-in `tmux` that logs its arguments. Its replies are what
    // a real tmux 3.6 printed for the same commands.

    use std::path::Path;
    use std::sync::Arc;

    use crate::harness::HarnessConfig;
    use crate::mux::stand_in;
    use crate::splitter::Splitter;

    fn with_fake_tmux(tag: &str, body: &str, extra: &[(&str, Option<&str>)], test: impl FnOnce(Splitter, &Path)) {
        let (bin, log) = stand_in::install(tag, "tmux", body);
        let mut vars = vec![
            ("KANSTACK_TMUX_BIN", Some(bin.to_str().unwrap())),
            ("TMUX_PANE", Some("%0")),
            ("KANSTACK_TMUX_DIRECTION", None),
            ("KANSTACK_TMUX_CHAIN_DIRECTION", None),
        ];
        vars.extend_from_slice(extra);
        stand_in::with_env(&vars, || {
            let tmux = Tmux::discover().unwrap();
            test(Splitter::new(Arc::new(tmux), HarnessConfig::new("claude")), &log)
        });
        stand_in::remove(&bin);
    }

    /// Answers a split with an id derived from the pane it was asked to split, so a chain of
    /// splits can be told apart.
    const SPLITS: &str = r#"case "$1" in split-window) echo "%9_after_$3" ;; esac"#;

    #[test]
    fn the_first_lane_splits_kanstacks_own_pane_and_is_typed_into_and_titled() {
        with_fake_tmux("first", SPLITS, &[], |mut splitter, log| {
            let pane = splitter.spawn_harness(Path::new("/repo"), "feat-a", Some("go")).unwrap();
            assert_eq!(pane, "%9_after_%0");
            let lines = stand_in::log_lines(log);
            assert_eq!(lines[0], "split-window -t %0 -c /repo -v -b -P -F #{pane_id}", "{lines:#?}");
            assert!(lines[1].starts_with("send-keys -t %9_after_%0 -l -- cd '/repo' && claude "), "{}", lines[1]);
            assert_eq!(lines[2], "send-keys -t %9_after_%0 Enter", "Enter is its own key press");
            assert_eq!(lines[3], "select-pane -t %9_after_%0 -T feat-a");
            assert_eq!(lines.len(), 4, "{lines:#?}");
        });
    }

    #[test]
    fn a_later_lane_chains_off_the_previous_one_in_the_chain_direction() {
        with_fake_tmux("chain", SPLITS, &[], |mut splitter, log| {
            splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap();
            splitter.spawn_harness(Path::new("/repo"), "feat-b", None).unwrap();
            let splits: Vec<_> = stand_in::log_lines(log).into_iter().filter(|l| l.starts_with("split-window")).collect();
            assert_eq!(splits[1], "split-window -t %9_after_%0 -c /repo -h -P -F #{pane_id}");
        });
    }

    #[test]
    fn the_directions_come_from_the_tmux_variables() {
        let vars = [("KANSTACK_TMUX_DIRECTION", Some("below")), ("KANSTACK_TMUX_CHAIN_DIRECTION", Some("left"))];
        with_fake_tmux("directions", SPLITS, &vars, |mut splitter, log| {
            splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap();
            splitter.spawn_harness(Path::new("/repo"), "feat-b", None).unwrap();
            let splits: Vec<_> = stand_in::log_lines(log).into_iter().filter(|l| l.starts_with("split-window")).collect();
            assert_eq!(splits[0], "split-window -t %0 -c /repo -v -P -F #{pane_id}");
            assert_eq!(splits[1], "split-window -t %9_after_%0 -c /repo -h -b -P -F #{pane_id}");
        });
    }

    #[test]
    fn a_failed_split_reports_tmuxs_own_message_and_tracks_nothing() {
        let body = r#"case "$1" in split-window) echo "no space for new pane" >&2; exit 1 ;; esac"#;
        with_fake_tmux("split-fails", body, &[], |mut splitter, log| {
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("no space for new pane"), "{err}");
            assert!(!splitter.has_pane("feat-a"));
            assert_eq!(stand_in::log_lines(log).len(), 1, "nothing may be typed into a pane that never opened");
        });
    }

    #[test]
    fn a_split_that_reports_no_pane_id_is_an_error() {
        with_fake_tmux("no-id", r#"case "$1" in split-window) ;; esac"#, &[], |mut splitter, _| {
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("did not report a pane id"), "{err}");
            assert!(!splitter.has_pane("feat-a"));
        });
    }

    /// The text goes in as literal keystrokes, so a message that happens to be the name of a
    /// key is typed rather than pressed, and the submit is a separate key.
    #[test]
    fn a_message_is_typed_literally_and_submitted_with_its_own_enter() {
        with_fake_tmux("send", "", &[], |mut splitter, log| {
            splitter.adopt("feat-a", "%5");
            splitter.send_task("feat-a", "Enter").unwrap();
            splitter.send_task("feat-a", "--fix the tests").unwrap();
            assert_eq!(
                stand_in::log_lines(log),
                ["send-keys -t %5 -l -- Enter", "send-keys -t %5 Enter", "send-keys -t %5 -l -- --fix the tests", "send-keys -t %5 Enter"]
            );
        });
    }

    /// `select-window` first: `select-pane` alone leaves a pane in another window out of sight.
    #[test]
    fn focus_selects_the_window_then_the_pane() {
        with_fake_tmux("focus", "", &[], |mut splitter, log| {
            splitter.adopt("feat-a", "%5");
            splitter.focus("feat-a").unwrap();
            assert_eq!(stand_in::log_lines(log), ["select-window -t %5", "select-pane -t %5"]);
        });
    }

    #[test]
    fn stopping_kills_the_pane_and_a_pane_already_gone_counts_as_stopped() {
        with_fake_tmux("stop", "", &[], |mut splitter, log| {
            splitter.adopt("feat-a", "%5");
            splitter.stop("feat-a").unwrap();
            assert_eq!(stand_in::log_lines(log), ["kill-pane -t %5"]);
            assert!(!splitter.has_pane("feat-a"));
        });
        let gone = r#"case "$1" in kill-pane) echo "can't find pane: %5" >&2; exit 1 ;; esac"#;
        with_fake_tmux("stop-gone", gone, &[], |mut splitter, _| {
            splitter.adopt("feat-a", "%5");
            splitter.stop("feat-a").expect("an already-closed pane is what stop is after anyway");
        });
        let broken = r#"case "$1" in kill-pane) echo "server exited unexpectedly" >&2; exit 1 ;; esac"#;
        with_fake_tmux("stop-broken", broken, &[], |mut splitter, _| {
            splitter.adopt("feat-a", "%5");
            let err = splitter.stop("feat-a").unwrap_err().to_string();
            assert!(err.contains("server exited unexpectedly"), "{err}");
        });
    }

    /// A pane absent from `list-panes` is dead; one that is listed reads idle when nothing
    /// under it is using CPU. The pid here is one no process has.
    #[test]
    fn polling_reads_present_panes_from_the_listing_and_absent_ones_as_dead() {
        let body = r#"case "$1" in list-panes) printf '%%5 2000000000\n' ;; esac"#;
        with_fake_tmux("poll", body, &[], |mut splitter, log| {
            splitter.adopt("here", "%5");
            splitter.adopt("gone", "%6");
            let statuses = splitter.poll_statuses().unwrap();
            assert_eq!(statuses["here"], PaneStatus::Idle);
            assert_eq!(statuses["gone"], PaneStatus::Dead);
            splitter.apply_statuses(statuses);
            assert_eq!(splitter.pane_status("gone"), Some(PaneStatus::Dead));
            assert_eq!(stand_in::log_lines(log), ["list-panes -a -F #{pane_id} #{pane_pid}"]);
        });
    }

    #[test]
    fn a_failed_listing_fails_the_poll_rather_than_calling_every_pane_dead() {
        let body = r#"case "$1" in list-panes) echo "no server running" >&2; exit 1 ;; esac"#;
        with_fake_tmux("poll-fails", body, &[], |mut splitter, _| {
            splitter.adopt("here", "%5");
            assert!(splitter.poll_statuses().unwrap_err().to_string().contains("no server running"));
        });
    }
}
