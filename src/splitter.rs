//! Dispatch over whichever harness-split backend `main.rs` discovered — `cmux` if present,
//! else plain `tmux` as a fallback for machines without it (see `crate::cmux`,
//! `crate::tmux`). A plain enum rather than a trait object: there are exactly two backends,
//! and the point is that `app.rs`'s call sites shouldn't have to care which one they hold.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::cmux::Cmux;
use crate::pane_status::PaneStatus;
use crate::tmux::Tmux;

#[derive(Clone)]
pub enum Splitter {
    Cmux(Cmux),
    Tmux(Tmux),
}

impl Splitter {
    /// Tries `Cmux::discover` first, then `Tmux::discover` — see each for what makes a
    /// backend usable at all. `None` means neither is available, same as either alone.
    ///
    /// `KANSTACK_SPLIT_BACKEND=cmux`/`tmux` skips the other backend's detection entirely,
    /// rather than just reordering the fallback: `Cmux::discover` only checks whether the
    /// `cmux` binary is on `PATH`, not whether kanstack is actually running inside a cmux
    /// pane (unlike `Tmux::discover`, which requires `$TMUX_PANE`), so a machine with both
    /// binaries installed — cmux for unrelated reasons, tmux the one actually in use right
    /// now — would otherwise have cmux win by default and fail at spawn time instead of
    /// falling through. The override exists for exactly that ambiguity; an unrecognized
    /// value falls back to plain auto-detection, same as `KANSTACK_BRANCH_UI` does.
    pub fn discover() -> Option<Self> {
        match std::env::var("KANSTACK_SPLIT_BACKEND").as_deref() {
            Ok("cmux") => return Cmux::discover().map(Splitter::Cmux),
            Ok("tmux") => return Tmux::discover().map(Splitter::Tmux),
            _ => {}
        }
        Cmux::discover().map(Splitter::Cmux).or_else(|| Tmux::discover().map(Splitter::Tmux))
    }

    /// Which backend this is, for the one place UI copy needs to name it: the branch
    /// modal's checkbox row label.
    pub fn label(&self) -> &'static str {
        match self {
            Splitter::Cmux(_) => "cmux",
            Splitter::Tmux(_) => "tmux",
        }
    }

    /// Whether kanstack itself looks like it's still running inside this backend's own
    /// pane right now — checked before starting a background poll, since polling only
    /// makes sense while that's true. Cheap env-var checks, one per backend, rather than
    /// a round trip to the CLI.
    pub fn running_inside_host(&self) -> bool {
        match self {
            Splitter::Cmux(_) => std::env::var_os("CMUX_SURFACE_ID").is_some(),
            Splitter::Tmux(_) => std::env::var_os("TMUX_PANE").is_some(),
        }
    }

    pub fn has_pane(&self, branch: &str) -> bool {
        match self {
            Splitter::Cmux(c) => c.has_pane(branch),
            Splitter::Tmux(t) => t.has_pane(branch),
        }
    }

    pub fn pane_status(&self, branch: &str) -> Option<PaneStatus> {
        match self {
            Splitter::Cmux(c) => c.pane_status(branch),
            Splitter::Tmux(t) => t.pane_status(branch),
        }
    }

    pub fn is_empty(&self) -> bool {
        match self {
            Splitter::Cmux(c) => c.is_empty(),
            Splitter::Tmux(t) => t.is_empty(),
        }
    }

    pub fn spawn_harness(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<()> {
        match self {
            Splitter::Cmux(c) => c.spawn_harness(cwd, name, initial_message),
            Splitter::Tmux(t) => t.spawn_harness(cwd, name, initial_message),
        }
    }

    pub fn send_task(&self, branch: &str, text: &str) -> Result<()> {
        match self {
            Splitter::Cmux(c) => c.send_task(branch, text),
            Splitter::Tmux(t) => t.send_task(branch, text),
        }
    }

    pub fn poll_statuses(&self) -> Result<HashMap<String, PaneStatus>> {
        match self {
            Splitter::Cmux(c) => c.poll_statuses(),
            Splitter::Tmux(t) => t.poll_statuses(),
        }
    }

    pub fn apply_statuses(&mut self, statuses: HashMap<String, PaneStatus>) {
        match self {
            Splitter::Cmux(c) => c.apply_statuses(statuses),
            Splitter::Tmux(t) => t.apply_statuses(statuses),
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
            ],
            || {
                assert!(matches!(Splitter::discover(), Some(Splitter::Cmux(_))));
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
                assert!(matches!(Splitter::discover(), Some(Splitter::Tmux(_))));
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
            ],
            || {
                assert!(matches!(Splitter::discover(), Some(Splitter::Cmux(_))));
            },
        );
    }
}
