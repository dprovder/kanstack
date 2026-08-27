//! Mouse hit-testing.
//!
//! `ui::draw` has no mutable access to `App` (it takes `&App`), so it cannot record where
//! things landed on screen as it goes. Instead it builds a [`HitMap`] as a side output and
//! returns it; `main`'s event loop stashes that on `App` right after the draw call, so the
//! *next* mouse event can be resolved against exactly what's on screen right now. Rects are
//! recomputed every frame anyway (see `ui.rs`), so the map is simply rebuilt alongside them
//! rather than incrementally maintained.

use ratatui::layout::Rect;

/// What a screen position resolves to, for [`crate::app::App::on_mouse`] to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTarget {
    /// A card at `(column, card)` within it.
    Card(usize, usize),
    /// A lane's header, by column index.
    LaneHeader(usize),
    /// Anywhere else inside a lane's body (blank space below its cards, or a card's
    /// unclaimed padding line) — still enough to tell which lane was clicked.
    LaneBody(usize),
    /// A row in the unapplied-branches drawer.
    BranchRow(usize),
    /// The header's own entry point into the drawer — the mouse equivalent of `a`, for
    /// opening it from the main board rather than only closing it once already inside.
    OpenBranches,
    /// The branch modal's `[x]/[ ] open harness split` row — the mouse equivalent of
    /// shift-tab, since it's a real checkbox rather than half of a confirm/cancel line.
    BranchToggleCmux,
    /// The "confirm" half of a confirm dialog's `⏎/y ... esc/n ...` hint line.
    DialogConfirm,
    /// The "cancel" half of the same line.
    DialogCancel,
    /// Click-anywhere-to-dismiss, for a full-screen overlay like the help screen.
    Dismiss,
}

/// Regions recorded during one frame's draw, searched most-recently-pushed first so a
/// popup drawn over the board takes priority over whatever is hit-tested underneath it.
#[derive(Debug, Clone, Default)]
pub struct HitMap {
    regions: Vec<(Rect, HitTarget)>,
}

impl HitMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, rect: Rect, target: HitTarget) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.push((rect, target));
        }
    }

    pub fn hit_test(&self, x: u16, y: u16) -> Option<HitTarget> {
        self.regions
            .iter()
            .rev()
            .find(|(r, _)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
            .map(|(_, t)| *t)
    }
}
