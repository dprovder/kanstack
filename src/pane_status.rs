//! [`PaneStatus`], shared by every harness-split backend (`crate::mux::cmux`, `crate::mux::tmux`,
//! `crate::mux::orca`) — see `crate::splitter` for how a caller picks between them.

/// Last known liveness/activity of a lane's split pane, however the backend derives it:
/// `crate::mux::cmux::Cmux` from `cmux top`'s CPU reading, `crate::mux::tmux::Tmux` from a `ps` walk of
/// the pane's process tree, `crate::mux::orca::Orca` from Orca's own idle detection (no CPU
/// threshold involved) — each in its `Multiplexer::probe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    /// Present in the backend's pane listing, with CPU usage above
    /// [`CPU_BUSY_THRESHOLD_PERCENT`] for it.
    Busy,
    /// Present in the backend's pane listing; CPU usage is at or below the busy threshold.
    /// May mean the harness returned to a resting prompt, or never got a chance to start —
    /// a single poll cannot tell those apart from this alone.
    Idle,
    /// Stopped in the middle of a turn, waiting for the user — a permission prompt. Only an
    /// agent can say so (see `crate::report`): from outside, a pane blocked on a dialog looks
    /// exactly like one that is idle.
    Waiting,
    /// No longer present in the backend's pane listing at all — the tab/pane was closed,
    /// by the user or by the terminal multiplexer itself.
    Dead,
    /// Tracked, but no poll has completed yet (or the last one couldn't classify it).
    Unknown,
}

/// CPU usage above this, for a tracked pane, counts as `Busy` rather than `Idle`. Chosen
/// (by `cmux.rs`, empirically — see its own doc comment) to sit comfortably above what a
/// resting shell or an idle harness prompt shows while still catching real generation
/// work; reused as-is for `tmux.rs`'s `ps`-sourced CPU percentages, which are the same
/// units.
pub const CPU_BUSY_THRESHOLD_PERCENT: f64 = 3.0;
