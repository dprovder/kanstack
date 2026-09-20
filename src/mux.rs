//! The seam between kanstack and a terminal multiplexer.
//!
//! A [`Multiplexer`] is the small, stateless set of things a multiplexer can do to a pane:
//! open one running a command line, type a line into one, focus it, close it, and say
//! whether it is still alive and busy. Nothing in it knows about branches, lanes or
//! harnesses. Everything that is the same whichever multiplexer is in use — which pane
//! belongs to which branch, where the next one splits off, what a pane's last known status
//! was — lives once in [`crate::splitter::Splitter`], so a new backend is one `impl` plus
//! one entry in its discovery order.
//!
//! Panes are addressed by an opaque id string the backend defines: tmux's `%3`, cmux's
//! `surface:12`, Orca's terminal handle.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::pane_status::PaneStatus;

/// Everything [`Multiplexer::open_pane`] needs to place and start one pane.
pub struct OpenRequest<'a> {
    /// The directory the harness runs in.
    pub cwd: &'a Path,
    /// What to call the pane, where the multiplexer has such a thing: the branch name.
    pub title: &'a str,
    /// The complete command line to run in it, typed as if at a shell (see
    /// `crate::harness::HarnessConfig::launch_line`).
    pub launch: &'a str,
    /// The pane the previous lane opened, if there is one. `Some` means split it, in
    /// `chain_direction`; `None` means this is the first lane, and the backend chooses what
    /// to split — normally kanstack's own pane, in `first_direction`.
    pub after: Option<&'a str>,
    /// Where the first lane goes relative to what it splits: `left`, `right`, `up` or `down`
    /// (see [`normalize_direction`]). A backend that can't place a pane that way says so in
    /// its own docs and does the nearest thing.
    pub first_direction: &'a str,
    /// Where every later lane goes relative to the one before it.
    pub chain_direction: &'a str,
}

/// One terminal multiplexer's view of panes. See the module doc for what is deliberately not
/// in here.
pub trait Multiplexer: Send + Sync {
    /// What to call it in the UI, and the middle of its environment variables:
    /// `KANSTACK_<NAME>_DIRECTION` and `KANSTACK_<NAME>_CHAIN_DIRECTION` (see
    /// [`configured_directions`]). Lowercase: `tmux`.
    fn name(&self) -> &'static str;

    /// Whether kanstack itself looks like it is running in one of this multiplexer's panes
    /// right now. Cheap — an environment check, never a round trip — because it gates
    /// whether polling is worth starting at all.
    fn running_inside(&self) -> bool;

    /// The first-lane and chained directions when nothing configures them. Most multiplexers
    /// put the first lane above kanstack and every later one beside the last.
    fn default_directions(&self) -> (&'static str, &'static str) {
        ("up", "right")
    }

    /// Opens a pane running `req.launch` and returns its id. A failed launch must not leave
    /// the caller believing a pane exists.
    fn open_pane(&self, req: &OpenRequest<'_>) -> Result<String>;

    /// Types `text` into `pane` as literal input, then submits it. The submit is a separate
    /// key press rather than a trailing newline in the same burst, which a TUI reads as part
    /// of a paste and never submits.
    fn type_line(&self, pane: &str, text: &str) -> Result<()>;

    /// Brings `pane` to the front and gives it keyboard focus.
    fn focus(&self, pane: &str) -> Result<()>;

    /// Closes `pane`, ending whatever runs in it. A pane that is already gone counts as
    /// closed.
    fn close(&self, pane: &str) -> Result<()>;

    /// Re-derives the status of each of `panes`. Read-only, and called from a background
    /// thread against a clone of the caller's state.
    ///
    /// The map is keyed by pane id and holds only the panes this call could classify: one
    /// that is gone is [`PaneStatus::Dead`], one that exists is `Busy` or `Idle` if the
    /// backend can tell and otherwise absent. Absent means "no news", and the caller keeps
    /// whatever it knew before.
    fn probe(&self, panes: &[&str]) -> Result<HashMap<String, PaneStatus>>;

    /// Pins the multiplexer-level grouping new panes open in (cmux's workspace). Only
    /// backends with such a notion do anything.
    fn set_scope(&self, _scope: Option<&str>) {}

    /// The grouping new panes open in, if this multiplexer has one and one is known.
    fn scope(&self) -> Option<String> {
        None
    }
}

/// Accepts the more readable `above`/`below` alongside `up`/`down`/`left`/`right`.
pub fn normalize_direction(raw: &str) -> String {
    match raw {
        "above" => "up",
        "below" => "down",
        other => other,
    }
    .to_string()
}

/// The first-lane and chained directions for `mux`: `KANSTACK_<NAME>_DIRECTION` and
/// `KANSTACK_<NAME>_CHAIN_DIRECTION` if set, else its [`Multiplexer::default_directions`].
pub fn configured_directions(mux: &dyn Multiplexer) -> (String, String) {
    let var = |suffix: &str| std::env::var(format!("KANSTACK_{}_{suffix}", mux.name().to_ascii_uppercase()));
    let (first, chain) = mux.default_directions();
    (
        var("DIRECTION").map(|raw| normalize_direction(&raw)).unwrap_or_else(|_| first.to_string()),
        var("CHAIN_DIRECTION").map(|raw| normalize_direction(&raw)).unwrap_or_else(|_| chain.to_string()),
    )
}

/// Whether `bin` names a file on `PATH`.
pub(crate) fn command_exists(bin: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin).is_file())
}

/// A multiplexer with no multiplexer behind it, for testing everything above the trait. It
/// records each call as one line in `log`, and answers `probe` from `statuses`.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    pub struct FakeMux {
        pub log: Mutex<Vec<String>>,
        /// What `probe` reports, by pane id. A pane not in here is left out of the answer.
        pub statuses: Mutex<HashMap<String, PaneStatus>>,
        pub scope: Mutex<Option<String>>,
        /// Fails the next `open_pane` when set.
        pub fail_open: Mutex<bool>,
        next: Mutex<u32>,
    }

    impl FakeMux {
        pub fn new() -> Arc<FakeMux> {
            Arc::new(FakeMux::default())
        }

        pub fn lines(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn record(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }
    }

    impl Multiplexer for FakeMux {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn running_inside(&self) -> bool {
            true
        }

        fn open_pane(&self, req: &OpenRequest<'_>) -> Result<String> {
            if *self.fail_open.lock().unwrap() {
                anyhow::bail!("fake open failed");
            }
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let id = format!("p{}", *next);
            let (from, direction) = match req.after {
                Some(after) => (after, req.chain_direction),
                None => ("own", req.first_direction),
            };
            self.record(format!("open {id} {direction} of {from} in {} as {}: {}", req.cwd.display(), req.title, req.launch));
            Ok(id)
        }

        fn type_line(&self, pane: &str, text: &str) -> Result<()> {
            self.record(format!("type {pane}: {text}"));
            Ok(())
        }

        fn focus(&self, pane: &str) -> Result<()> {
            self.record(format!("focus {pane}"));
            Ok(())
        }

        fn close(&self, pane: &str) -> Result<()> {
            self.record(format!("close {pane}"));
            Ok(())
        }

        fn probe(&self, panes: &[&str]) -> Result<HashMap<String, PaneStatus>> {
            let known = self.statuses.lock().unwrap();
            Ok(panes.iter().filter_map(|p| known.get(*p).map(|s| (p.to_string(), *s))).collect())
        }

        fn set_scope(&self, scope: Option<&str>) {
            *self.scope.lock().unwrap() = scope.map(str::to_string);
        }

        fn scope(&self) -> Option<String> {
            self.scope.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeMux;
    use super::*;

    #[test]
    fn normalize_direction_accepts_above_and_below() {
        assert_eq!(normalize_direction("above"), "up");
        assert_eq!(normalize_direction("below"), "down");
        assert_eq!(normalize_direction("left"), "left");
        assert_eq!(normalize_direction("right"), "right");
    }

    /// The variable names come from the backend's own name, so the ones people already set
    /// (`KANSTACK_TMUX_DIRECTION`) keep working without each backend spelling them out.
    #[test]
    fn configured_directions_reads_variables_named_after_the_backend() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mux = FakeMux::new();
        std::env::remove_var("KANSTACK_FAKE_DIRECTION");
        std::env::remove_var("KANSTACK_FAKE_CHAIN_DIRECTION");
        assert_eq!(configured_directions(mux.as_ref()), ("up".to_string(), "right".to_string()));

        std::env::set_var("KANSTACK_FAKE_DIRECTION", "below");
        std::env::set_var("KANSTACK_FAKE_CHAIN_DIRECTION", "left");
        assert_eq!(configured_directions(mux.as_ref()), ("down".to_string(), "left".to_string()));
        std::env::remove_var("KANSTACK_FAKE_DIRECTION");
        std::env::remove_var("KANSTACK_FAKE_CHAIN_DIRECTION");
    }
}
