//! Dispatch over whichever harness-split backend `main.rs` discovered — `cmux` if present,
//! else plain `tmux`, else `orca` (see `crate::cmux`, `crate::tmux`, `crate::orca`), paired
//! with the harness it launches (`crate::harness`). The backend is a plain enum rather than a
//! trait object: there are exactly three, and the point is that `app.rs`'s call sites
//! shouldn't have to care which one they hold.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::cmux::Cmux;
use crate::harness::HarnessConfig;
use crate::orca::Orca;
use crate::pane_status::PaneStatus;
use crate::tmux::Tmux;

#[derive(Clone)]
enum Backend {
    Cmux(Cmux),
    Tmux(Tmux),
    Orca(Orca),
}

/// A harness-split backend plus the harness it launches into new panes. The backend only
/// knows how to open, type into, focus and close panes; which harness runs in them, and
/// what its launch line looks like, is [`HarnessConfig`]'s business (see `crate::harness`).
#[derive(Clone)]
pub struct Splitter {
    backend: Backend,
    harness: HarnessConfig,
}

impl Splitter {
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
        Some(Splitter { backend: Self::discover_backend()?, harness: HarnessConfig::from_env() })
    }

    fn discover_backend() -> Option<Backend> {
        match std::env::var("KANSTACK_SPLIT_BACKEND").as_deref() {
            Ok("cmux") => return Cmux::discover().map(Backend::Cmux),
            Ok("tmux") => return Tmux::discover().map(Backend::Tmux),
            Ok("orca") => return Orca::discover().map(Backend::Orca),
            _ => {}
        }
        let in_cmux_or_tmux =
            std::env::var_os("CMUX_SURFACE_ID").is_some() || std::env::var_os("TMUX_PANE").is_some();
        if Orca::running_inside() && !in_cmux_or_tmux {
            if let Some(orca) = Orca::discover() {
                return Some(Backend::Orca(orca));
            }
        }
        Cmux::discover()
            .map(Backend::Cmux)
            .or_else(|| Tmux::discover().map(Backend::Tmux))
            .or_else(|| Orca::discover().map(Backend::Orca))
    }

    /// Which backend this is, for the one place UI copy needs to name it: the branch
    /// modal's checkbox row label.
    pub fn label(&self) -> &'static str {
        match &self.backend {
            Backend::Cmux(_) => "cmux",
            Backend::Tmux(_) => "tmux",
            Backend::Orca(_) => "orca",
        }
    }

    /// Whether kanstack itself looks like it's still running inside this backend's own
    /// pane right now — checked before starting a background poll, since polling only
    /// makes sense while that's true. Cheap env-var checks, one per backend, rather than
    /// a round trip to the CLI.
    pub fn running_inside_host(&self) -> bool {
        match &self.backend {
            Backend::Cmux(_) => std::env::var_os("CMUX_SURFACE_ID").is_some(),
            Backend::Tmux(_) => std::env::var_os("TMUX_PANE").is_some(),
            Backend::Orca(_) => Orca::running_inside(),
        }
    }

    pub fn has_pane(&self, branch: &str) -> bool {
        match &self.backend {
            Backend::Cmux(c) => c.has_pane(branch),
            Backend::Tmux(t) => t.has_pane(branch),
            Backend::Orca(o) => o.has_pane(branch),
        }
    }

    pub fn pane_status(&self, branch: &str) -> Option<PaneStatus> {
        match &self.backend {
            Backend::Cmux(c) => c.pane_status(branch),
            Backend::Tmux(t) => t.pane_status(branch),
            Backend::Orca(o) => o.pane_status(branch),
        }
    }

    pub fn is_empty(&self) -> bool {
        match &self.backend {
            Backend::Cmux(c) => c.is_empty(),
            Backend::Tmux(t) => t.is_empty(),
            Backend::Orca(o) => o.is_empty(),
        }
    }

    pub fn spawn_harness(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<String> {
        self.spawn_harness_with(cwd, name, initial_message, None)
    }

    /// [`Self::spawn_harness`] running `harness` in place of the configured one. Returns the
    /// new pane's backend-specific id.
    pub fn spawn_harness_with(
        &mut self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
    ) -> Result<String> {
        let launch = self.harness.launch_line(cwd, name, initial_message, harness)?;
        match &mut self.backend {
            Backend::Cmux(c) => c.spawn_pane(cwd, name, &launch),
            Backend::Tmux(t) => t.spawn_pane(cwd, name, &launch),
            Backend::Orca(o) => o.spawn_pane(cwd, name, &launch),
        }
    }

    /// Starts tracking a pane another process opened — see `crate::workstream::Registry`.
    pub fn adopt(&mut self, branch: &str, pane_id: &str) {
        match &mut self.backend {
            Backend::Cmux(c) => c.adopt(branch, pane_id),
            Backend::Tmux(t) => t.adopt(branch, pane_id),
            Backend::Orca(o) => o.adopt(branch, pane_id),
        }
    }

    /// Pins the cmux workspace new panes are opened in; a no-op for tmux.
    pub fn set_workspace(&mut self, workspace: Option<&str>) {
        match &mut self.backend {
            Backend::Cmux(c) => c.set_workspace(workspace),
            Backend::Tmux(t) => t.set_workspace(workspace),
            Backend::Orca(o) => o.set_workspace(workspace),
        }
    }

    /// The workspace new panes go in, if this backend has such a thing.
    pub fn workspace(&self) -> Option<String> {
        match &self.backend {
            Backend::Cmux(c) => c.workspace(),
            Backend::Tmux(t) => t.workspace(),
            Backend::Orca(o) => o.workspace(),
        }
    }

    /// Overrides the first-lane split direction — see `Cmux::set_first_direction`.
    pub fn set_first_direction(&mut self, direction: &str) {
        match &mut self.backend {
            Backend::Cmux(c) => c.set_first_direction(direction),
            Backend::Tmux(t) => t.set_first_direction(direction),
            Backend::Orca(o) => o.set_first_direction(direction),
        }
    }

    /// Makes the next spawn split off `pane_id` instead of the caller's own pane.
    pub fn set_anchor(&mut self, pane_id: &str) {
        match &mut self.backend {
            Backend::Cmux(c) => c.set_anchor(pane_id),
            Backend::Tmux(t) => t.set_anchor(pane_id),
            Backend::Orca(o) => o.set_anchor(pane_id),
        }
    }

    pub fn pane_id(&self, branch: &str) -> Option<String> {
        match &self.backend {
            Backend::Cmux(c) => c.pane_id(branch),
            Backend::Tmux(t) => t.pane_id(branch),
            Backend::Orca(o) => o.pane_id(branch),
        }
    }

    pub fn focus(&self, branch: &str) -> Result<()> {
        match &self.backend {
            Backend::Cmux(c) => c.focus(branch),
            Backend::Tmux(t) => t.focus(branch),
            Backend::Orca(o) => o.focus(branch),
        }
    }

    pub fn stop(&mut self, branch: &str) -> Result<()> {
        match &mut self.backend {
            Backend::Cmux(c) => c.stop(branch),
            Backend::Tmux(t) => t.stop(branch),
            Backend::Orca(o) => o.stop(branch),
        }
    }

    pub fn send_task(&self, branch: &str, text: &str) -> Result<()> {
        match &self.backend {
            Backend::Cmux(c) => c.send_task(branch, text),
            Backend::Tmux(t) => t.send_task(branch, text),
            Backend::Orca(o) => o.send_task(branch, text),
        }
    }

    pub fn poll_statuses(&self) -> Result<HashMap<String, PaneStatus>> {
        match &self.backend {
            Backend::Cmux(c) => c.poll_statuses(),
            Backend::Tmux(t) => t.poll_statuses(),
            Backend::Orca(o) => o.poll_statuses(),
        }
    }

    pub fn apply_statuses(&mut self, statuses: HashMap<String, PaneStatus>) {
        match &mut self.backend {
            Backend::Cmux(c) => c.apply_statuses(statuses),
            Backend::Tmux(t) => t.apply_statuses(statuses),
            Backend::Orca(o) => o.apply_statuses(statuses),
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
                assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Cmux(_), .. })));
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
                assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Tmux(_), .. })));
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
                assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Cmux(_), .. })));
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
            assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Orca(_), .. })));
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
                assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Orca(_), .. })));
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
        with_env(&in_cmux, || assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Cmux(_), .. }))));

        let mut in_tmux = base.to_vec();
        in_tmux.extend([("CMUX_SURFACE_ID", None), ("TMUX_PANE", Some("%3"))]);
        with_env(&in_tmux, || assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Cmux(_), .. }))));
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
                assert!(matches!(Splitter::discover(), Some(Splitter { backend: Backend::Orca(_), .. })));
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
                let splitter = Splitter { backend: Backend::Orca(Orca::discover().unwrap()), harness: HarnessConfig::new("claude") };
                assert_eq!(splitter.label(), "orca");
                assert!(splitter.running_inside_host());
                assert_eq!(splitter.workspace(), None, "there is no workspace to pin");

                std::env::remove_var("ORCA_TERMINAL_HANDLE");
                assert!(!splitter.running_inside_host());
            },
        );
    }
}
