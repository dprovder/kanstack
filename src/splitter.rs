//! The harness-split state kanstack keeps, whichever multiplexer it is in — `cmux` if
//! present, else plain `tmux`, else `orca` (see `crate::cmux`, `crate::tmux`, `crate::orca`),
//! paired with the harness it launches (`crate::harness`).
//!
//! A [`Multiplexer`] only opens, types into, focuses, closes and probes panes. Which branch a
//! pane belongs to, where the next one splits off, and what a pane's last known status was
//! are the same for every multiplexer, so they live here once, and `app/`'s call sites
//! needn't care which one they hold.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, bail, Result};

use crate::cmux::Cmux;
use crate::harness::HarnessConfig;
use crate::mux::{configured_directions, normalize_direction, Multiplexer, OpenRequest};
use crate::orca::Orca;
use crate::pane_status::PaneStatus;
use crate::report::Reports;
use crate::tmux::Tmux;

/// One lane's pane, as this process knows it.
#[derive(Debug, Clone)]
struct Pane {
    /// The multiplexer's own id for it.
    id: String,
    status: PaneStatus,
}

#[derive(Clone)]
pub struct Splitter {
    mux: Arc<dyn Multiplexer>,
    harness: HarnessConfig,
    /// Where the first lane goes relative to what it splits.
    first_direction: String,
    /// Where every later lane goes relative to the one before it.
    chain_direction: String,
    /// The pane the most recent spawn opened, so the next lane splits off it instead of
    /// kanstack's own pane. `None` for the first lane.
    last_anchor: Option<String>,
    /// One entry per lane a harness was opened for, keyed by the branch name passed to
    /// `spawn_harness` — the lane's *original* parallel branch, which stays the key even if
    /// other branches later stack on top of it (stacking never opens a second pane).
    panes: HashMap<String, Pane>,
    /// What each lane's agent has said about itself, where it can — see `crate::report`.
    reports: Reports,
}

impl Splitter {
    /// A splitter over `mux` launching `harness`, with the directions `mux` is configured
    /// for (see `crate::mux::configured_directions`) and no panes yet.
    pub fn new(mux: Arc<dyn Multiplexer>, harness: HarnessConfig) -> Self {
        let (first_direction, chain_direction) = configured_directions(mux.as_ref());
        Splitter {
            mux,
            harness,
            first_direction,
            chain_direction,
            last_anchor: None,
            panes: HashMap::new(),
            reports: Reports::default(),
        }
    }

    /// Tries `Cmux::discover` first, then `Tmux::discover`, then `Orca::discover` — see each
    /// for what makes a backend usable at all. `None` means none is available, same as any
    /// alone.
    ///
    /// `KANSTACK_SPLIT_BACKEND=cmux`/`tmux`/`orca` skips the other backends' detection
    /// entirely, rather than just reordering the fallback: `Cmux::discover` only checks
    /// whether the `cmux` binary is on `PATH`, not whether kanstack is actually running
    /// inside a cmux pane (unlike `Tmux::discover`, which requires `$TMUX_PANE`, and
    /// `Orca::discover`, which requires `$ORCA_TERMINAL_HANDLE`), so a machine with both
    /// binaries installed — cmux for unrelated reasons, tmux the one actually in use right
    /// now — would otherwise have cmux win by default and fail at spawn time instead of
    /// falling through. The override exists for exactly that ambiguity; an unrecognized
    /// value falls back to plain auto-detection, same as `KANSTACK_BRANCH_UI` does.
    ///
    /// One case the override needn't be reached for: running inside an Orca terminal, and
    /// not inside a cmux pane (`$CMUX_SURFACE_ID`) or tmux pane (`$TMUX_PANE`). That is the
    /// same cmux-installed-but-not-in-use ambiguity, and here there *is* a positive signal
    /// that Orca is the one in use, so it is tried ahead of cmux's bare `PATH` check. Inside
    /// a tmux or cmux pane in an Orca terminal, the default order is left as it was.
    ///
    /// The harness launched into new panes is `$KANSTACK_HARNESS` (default `claude`), read
    /// here once rather than by each backend.
    pub fn discover() -> Option<Self> {
        Some(Splitter::new(Self::discover_mux()?, HarnessConfig::from_env()))
    }

    fn discover_mux() -> Option<Arc<dyn Multiplexer>> {
        fn shared(mux: impl Multiplexer + 'static) -> Arc<dyn Multiplexer> {
            Arc::new(mux)
        }
        match std::env::var("KANSTACK_SPLIT_BACKEND").as_deref() {
            Ok("cmux") => return Cmux::discover().map(shared),
            Ok("tmux") => return Tmux::discover().map(shared),
            Ok("orca") => return Orca::discover().map(shared),
            _ => {}
        }
        let in_cmux_or_tmux =
            std::env::var_os("CMUX_SURFACE_ID").is_some() || std::env::var_os("TMUX_PANE").is_some();
        if Orca::running_inside() && !in_cmux_or_tmux {
            if let Some(orca) = Orca::discover() {
                return Some(shared(orca));
            }
        }
        Cmux::discover()
            .map(shared)
            .or_else(|| Tmux::discover().map(shared))
            .or_else(|| Orca::discover().map(shared))
    }

    /// Which backend this is, for the one place UI copy needs to name it: the branch
    /// modal's checkbox row label.
    pub fn label(&self) -> &'static str {
        self.mux.name()
    }

    /// Whether kanstack itself looks like it's still running inside this backend's own
    /// pane right now — checked before starting a background poll, since polling only
    /// makes sense while that's true.
    pub fn running_inside_host(&self) -> bool {
        self.mux.running_inside()
    }

    /// Whether kanstack has ever opened a pane for `branch` (regardless of its current
    /// status) — used to decide whether task dispatch needs to spawn one first.
    pub fn has_pane(&self, branch: &str) -> bool {
        self.panes.contains_key(branch)
    }

    /// Last known status of `branch`'s pane. `None` if kanstack has never tracked one for
    /// it at all (as opposed to `Some(PaneStatus::Dead)`, which means one existed and has
    /// since disappeared).
    pub fn pane_status(&self, branch: &str) -> Option<PaneStatus> {
        self.panes.get(branch).map(|p| p.status)
    }

    /// No panes tracked at all — nothing worth polling.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn spawn_harness(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<String> {
        self.spawn_harness_with(cwd, name, initial_message, None)
    }

    /// Opens a pane running the harness on lane `name`, with `cwd` as its working directory,
    /// and starts tracking it. `harness` runs in place of the configured one, for just this
    /// pane. Returns the new pane's backend-specific id, for a caller that needs to find it
    /// again from another process.
    ///
    /// `initial_message` and the note that `name` is a GitButler virtual branch go out on
    /// the launch line itself rather than as a second message afterwards: the harness needs
    /// a moment to start before it can receive typed input, and there is no "wait until
    /// ready" primitive to lean on, so folding them in sidesteps the race instead of racing
    /// it. See `crate::harness::HarnessConfig::launch_line`.
    pub fn spawn_harness_with(
        &mut self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
    ) -> Result<String> {
        let launch = self.harness.launch_line(cwd, name, initial_message, harness)?;
        // Before the harness starts, not after: a fast one could report before we got back,
        // and the last word of a previous pane on this branch must not outlive it.
        self.reports.forget(name);
        let id = self.mux.open_pane(&OpenRequest {
            cwd,
            title: name,
            launch: &launch,
            after: self.last_anchor.as_deref(),
            first_direction: &self.first_direction,
            chain_direction: &self.chain_direction,
        })?;
        self.panes.insert(name.to_string(), Pane { id: id.clone(), status: PaneStatus::Unknown });
        self.last_anchor = Some(id.clone());
        Ok(id)
    }

    /// Where to read what agents report about themselves. Without it, statuses come from the
    /// multiplexer alone.
    pub fn set_reports(&mut self, reports: Reports) {
        self.reports = reports;
    }

    /// Starts tracking a pane another process opened — see `crate::workstream::Registry`.
    pub fn adopt(&mut self, branch: &str, pane_id: &str) {
        self.panes.insert(branch.to_string(), Pane { id: pane_id.to_string(), status: PaneStatus::Unknown });
    }

    /// Pins the multiplexer-level grouping new panes open in (cmux's workspace); a no-op
    /// for the others.
    pub fn set_workspace(&mut self, workspace: Option<&str>) {
        self.mux.set_scope(workspace);
    }

    /// The workspace new panes go in, if this backend has such a thing.
    pub fn workspace(&self) -> Option<String> {
        self.mux.scope()
    }

    /// Overrides the first-lane split direction. For a caller with its own setting — the
    /// `kanstack spawn` subcommand has `KANSTACK_SPAWN_DIRECTION`, since the board's `above`
    /// puts lanes over kanstack's own pane and an agent's pane is usually somewhere else.
    pub fn set_first_direction(&mut self, direction: &str) {
        self.first_direction = normalize_direction(direction);
    }

    /// Makes the next spawn split off `pane_id` instead of the caller's own pane, the way
    /// consecutive spawns within one process already chain.
    pub fn set_anchor(&mut self, pane_id: &str) {
        self.last_anchor = Some(pane_id.to_string());
    }

    pub fn pane_id(&self, branch: &str) -> Option<String> {
        self.panes.get(branch).map(|p| p.id.clone())
    }

    /// `branch`'s tracked pane, or the error every action on an untracked lane reports.
    fn pane(&self, branch: &str) -> Result<&Pane> {
        self.panes.get(branch).ok_or_else(|| anyhow!("no {} pane open for {branch} yet", self.mux.name()))
    }

    pub fn focus(&self, branch: &str) -> Result<()> {
        self.mux.focus(&self.pane(branch)?.id)
    }

    /// Closes `branch`'s pane, ending whatever harness is running in it, and stops tracking
    /// it. Already gone counts as stopped.
    pub fn stop(&mut self, branch: &str) -> Result<()> {
        let Some(pane) = self.panes.remove(branch) else {
            bail!("no {} pane open for {branch} yet", self.mux.name());
        };
        if self.last_anchor.as_deref() == Some(pane.id.as_str()) {
            self.last_anchor = None;
        }
        self.reports.forget(branch);
        self.mux.close(&pane.id)
    }

    /// Sends `text` followed by Enter into `branch`'s tracked pane — the same
    /// literal-then-Enter sequence spawning uses to type the launch command, just aimed at a
    /// pane recorded earlier.
    pub fn send_task(&self, branch: &str, text: &str) -> Result<()> {
        self.mux.type_line(&self.pane(branch)?.id, text).map_err(|e| anyhow!("{branch}: {e:#}"))
    }

    /// Re-derives the status of every tracked pane, keyed by branch, from two sources: the
    /// multiplexer's own reading of the pane, and what the agent in it last said about itself
    /// (see `crate::report`).
    ///
    /// A pane the multiplexer says is gone is `Dead`, whatever the agent last said — a
    /// crashed one can't have said it stopped. Otherwise a fresh report wins: it comes from
    /// the harness itself, where the multiplexer only infers from CPU, or from nothing at
    /// all. Otherwise the multiplexer's reading stands. A pane with neither is left out, so
    /// [`Self::apply_statuses`] keeps what was known.
    ///
    /// Read-only — safe to call from a background thread against a cloned snapshot.
    pub fn poll_statuses(&self) -> Result<HashMap<String, PaneStatus>> {
        self.poll_statuses_at(SystemTime::now())
    }

    fn poll_statuses_at(&self, now: SystemTime) -> Result<HashMap<String, PaneStatus>> {
        if self.panes.is_empty() {
            return Ok(HashMap::new());
        }
        let ids: Vec<&str> = self.panes.values().map(|p| p.id.as_str()).collect();
        let by_pane = match self.mux.probe(&ids) {
            Ok(by_pane) => by_pane,
            // The multiplexer couldn't be read — no server, a permission refused — but what
            // agents said about themselves is still good. Only whether a pane is gone is
            // unknowable, so that is all that is lost. The error surfaces only when there is
            // nothing at all to report instead.
            Err(err) => {
                let reported = self.reported_statuses(now);
                return if reported.is_empty() { Err(err) } else { Ok(reported) };
            }
        };
        Ok(self
            .panes
            .iter()
            .filter_map(|(branch, pane)| {
                let native = by_pane.get(&pane.id).copied();
                let status = match native {
                    Some(PaneStatus::Dead) => PaneStatus::Dead,
                    _ => self.reports.status(branch, now).or(native)?,
                };
                Some((branch.clone(), status))
            })
            .collect())
    }

    /// Every lane whose agent has said something recent about itself, keyed by branch.
    fn reported_statuses(&self, now: SystemTime) -> HashMap<String, PaneStatus> {
        self.panes.keys().filter_map(|branch| Some((branch.clone(), self.reports.status(branch, now)?))).collect()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `body` with each of `vars` swapped out and restored afterwards — same pattern
    /// as `cmux.rs`'s and `tmux.rs`'s own `with_env` helpers, just handling several vars at
    /// once since `discover` here spans both backends' env surface. Held for the whole
    /// call via `SPLIT_BACKEND_ENV_LOCK`, not just the swap, so this can't interleave with
    /// either of those other two files' own env-mutating tests.
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

    /// Both backends "found" (via trusted explicit-bin overrides, so no real `cmux`/`tmux`
    /// binary is needed) and no `KANSTACK_SPLIT_BACKEND` set: cmux wins, matching
    /// `discover`'s documented default order.
    #[test]
    fn discover_prefers_cmux_by_default_when_both_qualify() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", Some("%3")),
                ("ORCA_TERMINAL_HANDLE", None),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux"));
            },
        );
    }

    /// `KANSTACK_SPLIT_BACKEND=tmux` skips cmux detection entirely, even though cmux would
    /// otherwise qualify — the whole point of the override.
    #[test]
    fn discover_honors_an_explicit_tmux_override_over_a_qualifying_cmux() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", Some("%3")),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("tmux"));
            },
        );
    }

    /// `KANSTACK_SPLIT_BACKEND=cmux` does not fall through to tmux when cmux itself isn't
    /// actually usable — the override pins the backend, it doesn't just reorder the
    /// fallback.
    #[test]
    fn discover_honors_an_explicit_cmux_override_without_falling_back_to_tmux() {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("cmux")),
                ("PATH", Some(dir.to_str().unwrap())),
                ("KANSTACK_CMUX_BIN", None),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", Some("%3")),
            ],
            || {
                assert!(
                    Splitter::discover().is_none(),
                    "cmux was pinned but isn't usable, so this must not fall back to tmux"
                );
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unrecognized `KANSTACK_SPLIT_BACKEND` value falls back to plain auto-detection
    /// rather than treating it as "neither backend," same as `KANSTACK_BRANCH_UI` ignores
    /// a value it doesn't recognize.
    #[test]
    fn discover_falls_back_to_auto_detect_on_an_unrecognized_override_value() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("bogus")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("KANSTACK_TMUX_BIN", None),
                ("TMUX_PANE", None),
                ("ORCA_TERMINAL_HANDLE", None),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux"));
            },
        );
    }

    /// `KANSTACK_SPLIT_BACKEND=orca` pins Orca, and — like the other two pins — doesn't fall
    /// through to a backend that would otherwise qualify when Orca itself isn't usable.
    #[test]
    fn discover_honors_an_explicit_orca_override_without_falling_back() {
        let usable = [
            ("KANSTACK_SPLIT_BACKEND", Some("orca")),
            ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
            ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
            ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
            ("TMUX_PANE", Some("%3")),
        ];
        let mut with_handle = usable.to_vec();
        with_handle.push(("ORCA_TERMINAL_HANDLE", Some("term_1")));
        with_env(&with_handle, || {
            assert_eq!(Splitter::discover().map(|s| s.label()), Some("orca"));
        });

        let mut without_handle = usable.to_vec();
        without_handle.push(("ORCA_TERMINAL_HANDLE", None));
        with_env(&without_handle, || {
            assert!(
                Splitter::discover().is_none(),
                "orca was pinned but this isn't an Orca terminal, so this must not fall back to cmux or tmux"
            );
        });
    }

    /// Inside an Orca terminal with nothing saying kanstack is also inside a cmux or tmux
    /// pane, Orca goes ahead of cmux's bare "is the binary on PATH" check — the whole
    /// cmux-installed-but-not-in-use ambiguity, with a positive signal to settle it.
    #[test]
    fn discover_prefers_orca_over_a_merely_installed_cmux_inside_an_orca_terminal() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
                ("ORCA_TERMINAL_HANDLE", Some("term_1")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("CMUX_SURFACE_ID", None),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", None),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("orca"));
            },
        );
    }

    /// The same Orca terminal, but kanstack is in a cmux pane or a tmux pane within it: the
    /// positive signal points elsewhere, so the default order stands.
    #[test]
    fn discover_leaves_the_default_order_alone_inside_a_cmux_or_tmux_pane_in_orca() {
        let base = [
            ("KANSTACK_SPLIT_BACKEND", None),
            ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
            ("ORCA_TERMINAL_HANDLE", Some("term_1")),
            ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
            ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
        ];
        let mut in_cmux = base.to_vec();
        in_cmux.extend([("CMUX_SURFACE_ID", Some("ABC")), ("TMUX_PANE", None)]);
        with_env(&in_cmux, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux")));

        let mut in_tmux = base.to_vec();
        in_tmux.extend([("CMUX_SURFACE_ID", None), ("TMUX_PANE", Some("%3"))]);
        with_env(&in_tmux, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux")));
    }

    /// Orca is last in the chain: a tmux pane whose `tmux` binary can't be found (no cmux
    /// either) falls through to Orca rather than to nothing.
    #[test]
    fn discover_falls_through_to_orca_when_cmux_and_tmux_are_both_unusable() {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-orca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("PATH", Some(dir.to_str().unwrap())),
                ("KANSTACK_CMUX_BIN", None),
                ("KANSTACK_TMUX_BIN", None),
                ("TMUX_PANE", Some("%3")),
                ("CMUX_SURFACE_ID", None),
                ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
                ("ORCA_TERMINAL_HANDLE", Some("term_1")),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("orca"));
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `running_inside_host` gates the board's background poll, and for Orca it is read off
    /// `$ORCA_TERMINAL_HANDLE` at the moment of asking, not remembered from `discover`.
    #[test]
    fn an_orca_splitter_names_itself_and_asks_the_environment_whether_to_poll() {
        with_env(
            &[("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")), ("ORCA_TERMINAL_HANDLE", Some("term_1"))],
            || {
                let splitter = Splitter::new(Arc::new(Orca::discover().unwrap()), HarnessConfig::new("claude"));
                assert_eq!(splitter.label(), "orca");
                assert!(splitter.running_inside_host());
                assert_eq!(splitter.workspace(), None, "there is no workspace to pin");

                std::env::remove_var("ORCA_TERMINAL_HANDLE");
                assert!(!splitter.running_inside_host());
            },
        );
    }

    // Everything below runs against `FakeMux`, so what it pins is the layer every backend
    // shares — which pane belongs to which lane, where the next one goes, what a pane's last
    // known status was — once, rather than once per backend.

    use crate::mux::fake::FakeMux;

    fn fake_splitter() -> (Splitter, Arc<FakeMux>) {
        let mux = FakeMux::new();
        (Splitter::new(mux.clone(), HarnessConfig::new("claude")), mux)
    }

    fn cwd() -> &'static Path {
        Path::new("/repo")
    }

    #[test]
    fn the_first_lane_splits_kanstacks_own_pane_and_each_later_one_chains_off_the_last() {
        let (mut splitter, mux) = fake_splitter();
        assert_eq!(splitter.spawn_harness(cwd(), "feat-a", None).unwrap(), "p1");
        assert_eq!(splitter.spawn_harness(cwd(), "feat-b", None).unwrap(), "p2");
        assert_eq!(splitter.spawn_harness(cwd(), "feat-c", None).unwrap(), "p3");
        let lines = mux.lines();
        assert!(lines[0].starts_with("open p1 up of own in /repo as feat-a: cd '/repo' && claude"), "{lines:#?}");
        assert!(lines[1].starts_with("open p2 right of p1 in /repo as feat-b:"), "{lines:#?}");
        assert!(lines[2].starts_with("open p3 right of p2 in /repo as feat-c:"), "{lines:#?}");
    }

    #[test]
    fn a_spawned_lane_is_tracked_with_an_unknown_status_until_polled() {
        let (mut splitter, _) = fake_splitter();
        assert!(splitter.is_empty());
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(!splitter.is_empty());
        assert!(splitter.has_pane("feat-a"));
        assert_eq!(splitter.pane_id("feat-a").as_deref(), Some("p1"));
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Unknown));
        assert_eq!(splitter.pane_status("never-spawned"), None);
    }

    /// A launch that failed must not leave a phantom lane, or move the anchor the next
    /// lane splits off.
    #[test]
    fn a_failed_open_tracks_nothing_and_leaves_the_anchor_alone() {
        let (mut splitter, mux) = fake_splitter();
        *mux.fail_open.lock().unwrap() = true;
        assert!(splitter.spawn_harness(cwd(), "feat-a", None).is_err());
        assert!(!splitter.has_pane("feat-a"));
        *mux.fail_open.lock().unwrap() = false;
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(mux.lines()[0].starts_with("open p1 up of own"), "{:#?}", mux.lines());
    }

    #[test]
    fn stopping_the_newest_lane_sends_the_next_one_back_to_kanstacks_own_pane() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.stop("feat-a").unwrap();
        assert!(!splitter.has_pane("feat-a"));
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        let lines = mux.lines();
        assert_eq!(lines[1], "close p1", "{lines:#?}");
        assert!(lines[2].starts_with("open p2 up of own"), "the next lane must not split a closed pane: {lines:#?}");
    }

    #[test]
    fn stopping_an_older_lane_keeps_chaining_off_the_newest() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        splitter.stop("feat-a").unwrap();
        splitter.spawn_harness(cwd(), "feat-c", None).unwrap();
        assert!(mux.lines().last().unwrap().starts_with("open p3 right of p2"), "{:#?}", mux.lines());
    }

    #[test]
    fn acting_on_a_lane_with_no_pane_says_which_backend_and_which_lane() {
        let (mut splitter, mux) = fake_splitter();
        for err in [
            splitter.focus("nope").unwrap_err(),
            splitter.send_task("nope", "x").unwrap_err(),
            splitter.stop("nope").unwrap_err(),
        ] {
            assert_eq!(err.to_string(), "no fake pane open for nope yet");
        }
        assert!(mux.lines().is_empty(), "nothing should reach the multiplexer: {:#?}", mux.lines());
    }

    /// The subcommands run in a process that never opened the pane, so they adopt one from
    /// the registry and then act on it exactly as if they had.
    #[test]
    fn an_adopted_pane_can_be_focused_typed_into_and_stopped() {
        let (mut splitter, mux) = fake_splitter();
        splitter.adopt("feat-a", "p9");
        splitter.focus("feat-a").unwrap();
        splitter.send_task("feat-a", "run the tests").unwrap();
        splitter.stop("feat-a").unwrap();
        assert_eq!(mux.lines(), ["focus p9", "type p9: run the tests", "close p9"]);
    }

    #[test]
    fn set_anchor_makes_the_next_lane_split_that_pane_in_the_chain_direction() {
        let (mut splitter, mux) = fake_splitter();
        splitter.set_anchor("p42");
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(mux.lines()[0].starts_with("open p1 right of p42"), "{:#?}", mux.lines());
    }

    #[test]
    fn a_first_direction_override_is_normalized() {
        let (mut splitter, mux) = fake_splitter();
        splitter.set_first_direction("below");
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(mux.lines()[0].starts_with("open p1 down of own"), "{:#?}", mux.lines());
    }

    #[test]
    fn a_harness_override_changes_only_that_lanes_launch_line() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness_with(cwd(), "feat-a", Some("go"), Some("codex")).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", Some("go")).unwrap();
        let lines = mux.lines();
        assert!(lines[0].contains("&& codex "), "{lines:#?}");
        assert!(lines[1].contains("&& claude "), "{lines:#?}");
    }

    /// Statuses come back keyed by branch, and a pane the backend had no news about is left
    /// out — so applying them keeps what was already known instead of overwriting it.
    #[test]
    fn polling_reports_by_branch_and_no_news_keeps_the_last_known_status() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();

        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Busy);
        let polled = splitter.poll_statuses().unwrap();
        assert_eq!(polled, HashMap::from([("feat-a".to_string(), PaneStatus::Busy)]));
        splitter.apply_statuses(polled);
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Busy));
        assert_eq!(splitter.pane_status("feat-b"), Some(PaneStatus::Unknown));

        mux.statuses.lock().unwrap().clear();
        let polled = splitter.poll_statuses().unwrap();
        assert!(polled.is_empty());
        splitter.apply_statuses(polled);
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Busy), "no news must not erase it");

        mux.statuses.lock().unwrap().insert("p2".to_string(), PaneStatus::Dead);
        splitter.apply_statuses(splitter.poll_statuses().unwrap());
        assert_eq!(splitter.pane_status("feat-b"), Some(PaneStatus::Dead));
    }

    /// A lane deleted while a poll was in flight is simply not there to update.
    #[test]
    fn applying_statuses_for_a_lane_that_has_since_gone_is_a_no_op() {
        let (mut splitter, _) = fake_splitter();
        splitter.apply_statuses(HashMap::from([("gone".to_string(), PaneStatus::Busy)]));
        assert!(!splitter.has_pane("gone"));
    }

    #[test]
    fn polling_with_no_panes_never_asks_the_multiplexer() {
        let (splitter, mux) = fake_splitter();
        assert!(splitter.poll_statuses().unwrap().is_empty());
        assert!(mux.lines().is_empty());
    }

    #[test]
    fn the_workspace_is_the_multiplexers_scope() {
        let (mut splitter, _) = fake_splitter();
        assert_eq!(splitter.workspace(), None);
        splitter.set_workspace(Some("workspace:2"));
        assert_eq!(splitter.workspace().as_deref(), Some("workspace:2"));
        assert_eq!(splitter.label(), "fake");
        assert!(splitter.running_inside_host());
    }

    // What an agent says about itself, against what the multiplexer infers.

    use crate::report::{Reported, Reports, FRESH_FOR};
    use std::time::{Duration, SystemTime};

    /// A splitter with one lane, `feat-a`, in pane `p1`, reading reports from a fresh
    /// directory.
    fn splitter_with_reports(tag: &str) -> (Splitter, Arc<FakeMux>, Reports) {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-reports-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reports = Reports::in_dir(dir);
        let (mut splitter, mux) = fake_splitter();
        splitter.set_reports(reports.clone());
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        (splitter, mux, reports)
    }

    fn polled(splitter: &Splitter, now: SystemTime) -> Option<PaneStatus> {
        splitter.poll_statuses_at(now).unwrap().get("feat-a").copied()
    }

    /// The multiplexer only infers from CPU; the harness knows. So where both speak, the
    /// harness is believed.
    #[test]
    fn a_fresh_report_wins_over_the_multiplexers_reading() {
        let (splitter, mux, reports) = splitter_with_reports("wins");
        let now = SystemTime::now();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        reports.write("feat-a", Reported::Busy, now).unwrap();
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Busy));
    }

    /// A multiplexer with no idea whether its panes are busy — Ghostty's scripting can't
    /// say — is where reports matter most.
    #[test]
    fn a_report_is_the_whole_answer_when_the_multiplexer_has_no_reading() {
        let (splitter, _mux, reports) = splitter_with_reports("alone");
        let now = SystemTime::now();
        assert_eq!(polled(&splitter, now), None, "nobody has said anything yet");
        reports.write("feat-a", Reported::Idle, now).unwrap();
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Idle));
    }

    /// A crashed harness never reports that it stopped.
    #[test]
    fn a_closed_pane_is_dead_whatever_the_agent_last_said() {
        let (splitter, mux, reports) = splitter_with_reports("dead");
        let now = SystemTime::now();
        reports.write("feat-a", Reported::Busy, now).unwrap();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Dead);
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Dead));
    }

    #[test]
    fn a_stale_report_gives_way_to_the_multiplexers_reading() {
        let (splitter, mux, reports) = splitter_with_reports("stale");
        let then = SystemTime::now();
        reports.write("feat-a", Reported::Busy, then).unwrap();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        let later = then + FRESH_FOR + Duration::from_secs(1);
        assert_eq!(polled(&splitter, later), Some(PaneStatus::Idle));
        mux.statuses.lock().unwrap().clear();
        assert_eq!(polled(&splitter, later), None, "stale and no other reading: no news");
    }

    #[test]
    fn a_new_pane_on_a_branch_does_not_inherit_the_old_ones_last_word() {
        let (mut splitter, _mux, reports) = splitter_with_reports("respawn");
        let now = SystemTime::now();
        reports.write("feat-a", Reported::Busy, now).unwrap();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert_eq!(polled(&splitter, now), None);
    }

    #[test]
    fn stopping_a_lane_forgets_what_its_agent_said() {
        let (mut splitter, _mux, reports) = splitter_with_reports("stop");
        let now = SystemTime::now();
        reports.write("feat-a", Reported::Busy, now).unwrap();
        splitter.stop("feat-a").unwrap();
        assert_eq!(reports.status("feat-a", now), None);
    }

    /// If the multiplexer can't be read, an agent's own word is still the best there is —
    /// and a permission the user refused once shouldn't blank every status.
    #[test]
    fn a_failed_probe_still_yields_what_agents_reported() {
        let (splitter, mux, reports) = splitter_with_reports("probe-fails");
        let now = SystemTime::now();
        *mux.fail_probe.lock().unwrap() = true;
        reports.write("feat-a", Reported::Busy, now).unwrap();
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Busy));
    }

    #[test]
    fn a_failed_probe_with_nothing_reported_is_still_an_error() {
        let (splitter, mux, _reports) = splitter_with_reports("probe-fails-silent");
        *mux.fail_probe.lock().unwrap() = true;
        let err = splitter.poll_statuses_at(SystemTime::now()).unwrap_err().to_string();
        assert!(err.contains("fake probe failed"), "{err}");
    }
}
