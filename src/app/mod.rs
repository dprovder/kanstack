//! Application state and input handling.
//!
//! Navigation is pure in-memory so arrow keys never wait on a subprocess. Only mutations
//! shell out, and `rub`/`commit`/`move` fold their refresh into the same call by embedding
//! a status in their reply, so a card move costs one round trip rather than a mutation plus
//! a reload.
//!
//! Split across files by feature area so two features in flight touch different files —
//! see `docs/ARCHITECTURE.md`. `App`'s own definition, `Mode`, and the handful of methods
//! every feature shares (refresh/lifecycle, board-cursor navigation, the key dispatcher)
//! stay here; everything else lives in a sibling module with its own `impl App` block.
//! Rust privacy makes this free: a private item defined here is visible to every submodule
//! below, so `self.some_private_field` and `Self::some_private_helper` work unchanged from
//! any of them without widening visibility.

use anyhow::Result;

use crate::board::{Board, Card, CardKind, ColumnKind, Unapplied};
use crate::but::But;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use crate::diff::DiffView;
use crate::model::{MergeCheck, PullPreview, PushPreview, WorkspaceStatus};
use crate::splitter::Splitter;
use crate::text_input::TextInput;
use crate::tutorial::Tutorial;

mod blocked;
mod branch_modal;
mod commit;
mod delete;
mod diff;
mod drawer;
mod moving;
mod pr;
mod push_land;
mod rebase;
mod restack;
mod setup_required;
mod task_dispatch;
mod undo;

// Re-exported so `crate::app::X` keeps working for types that used to live directly in this
// file — `ui`, `snapshot`, `tutorial` and the integration tests all reach them that way.
pub use branch_modal::{BranchModalRow, BranchUi};
pub use drawer::BranchPreview;
pub use pr::{PendingPr, PrModalRow};
pub use push_land::PendingLand;

use branch_modal::PendingBranch;
use delete::DeleteTarget;

/// Board-layout constants shared with `ui::visible_columns`: how wide a lane is allowed to
/// get, and the gap between lanes. Living here rather than in `ui` is what lets
/// `columns_that_fit` answer "how many lanes fit on screen" for `Shift+←/→` paging without
/// `App` needing to know anything about `ratatui::Rect`.
pub(crate) const MIN_COL_WIDTH: u16 = 26;
pub(crate) const MAX_COL_WIDTH: u16 = 46;
pub(crate) const COL_GAP: u16 = 2;

/// The lane width and how many lanes fit at once, for `n` lanes in a terminal `area_width`
/// columns wide. Pure so it can be shared between actual layout (`ui::visible_columns`,
/// which also needs the width to draw with) and `Shift+←/→` paging (which only needs the
/// count) without duplicating the same clamped-width formula in two places to drift apart.
pub(crate) fn columns_that_fit(n: usize, area_width: u16) -> (u16, usize) {
    let n = n.max(1) as u16;
    let usable = area_width.saturating_sub(2);
    let width = (usable / n)
        .saturating_sub(COL_GAP)
        .clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
    let per = width + COL_GAP;
    let fit = (usable / per).max(1) as usize;
    (width, fit)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// A card has been picked up and is looking for a lane to land in.
    Moving,
    /// Typing a commit message.
    Commit,
    /// Typing a new branch name.
    Branch,
    /// Typing a task description to send into the selected lane's split pane.
    Task,
    /// Typing an initial message to seed the harness pane of a branch that does not exist
    /// yet, in `b`'s create-parallel-lane flow — see `App::advance_to_harness_message`.
    /// Distinct from `Task`: that one targets an already-running pane, while this one
    /// folds the message into the harness's own launch command — see
    /// [`crate::splitter::Splitter::spawn_harness`].
    HarnessMessage,
    /// A whole lane has been picked up, looking for a lane to stack onto.
    Restacking,
    /// Looking at what a push would do, before doing it.
    PushConfirm,
    /// Looking at what landing a lane onto the target would do, before doing it.
    LandConfirm,
    /// `but land` is running on a background thread; see [`PendingLand`]. Input is
    /// swallowed here — a land in flight is not a state to navigate the board out from
    /// under, and there is nothing left to cancel once it has started pushing.
    Landing,
    /// Filling out a PR's title and draft toggle before `but pr new` runs — see
    /// [`PrModalRow`].
    PrModal,
    /// `but pr new` is running on a background thread; see [`PendingPr`]. Same reasoning
    /// as `Landing`: input is swallowed, and there is no cancel once it may already be
    /// pushing.
    PrRunning,
    /// Confirming a lane deletion.
    DeleteConfirm,
    /// The unapplied-branches drawer is open beside the board, and has the keys.
    Branches,
    /// Confirming that a lane — and the whole stack it belongs to — leaves the workspace.
    UnapplyConfirm,
    /// Looking at what rebasing onto the updated target would do.
    RebaseConfirm,
    /// Reading a diff, hunk by hunk.
    Diff,
    Help,
    /// The workspace is broken in the one way that stops `but` answering anything at all
    /// (see [`crate::but::is_workspace_block`]). Every key is swallowed except the two
    /// recoveries and quit — the board behind this is a snapshot of a repository that has
    /// since moved on, so acting on it would mean acting on a lie.
    Blocked,
    /// This directory isn't a GitButler project yet — not broken, just never set up (see
    /// [`crate::but::is_setup_required`]). Unlike `Blocked`, there's exactly one safe
    /// recovery, so the only choices are running it or quitting.
    SetupRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Info,
    Success,
    Error,
}

pub struct App {
    /// `None` in snapshot mode, where a captured status is rendered read-only.
    /// `Arc` so `confirm_land` can hand a handle to the background thread it spawns.
    but: Option<Arc<But>>,
    /// `None` when neither `cmux` nor `tmux` is usable — see [`crate::splitter::Splitter::discover`].
    splitter: Option<Splitter>,
    pub board: Board,
    pub col: usize,
    pub card: usize,
    pub mode: Mode,
    /// Where the picked-up card came from, valid while `mode == Moving`.
    pub origin_col: usize,
    pub origin_card: usize,
    /// Drop position within the target lane: `None` targets the lane itself (a move or
    /// stage), `Some(i)` targets that card (a squash or amend).
    pub target_card: Option<usize>,
    /// Commit message being typed, valid while `mode == Commit`.
    pub commit_input: TextInput,
    /// A move that needs a message before it can run — dragging an uncommitted change onto
    /// a lane with no commits yet. Set by `confirm_move`/`confirm_bulk_move`, consumed by
    /// `confirm_commit`, valid while `mode == Commit`.
    pending_commit_move: Option<PendingCommitMove>,
    /// Branch name being typed, valid while `mode == Branch`.
    pub branch_input: TextInput,
    /// Task description being typed, valid while `mode == Task`.
    pub task_input: TextInput,
    /// Branch key of the pane being typed into, valid while `mode == Task`. Resolved once
    /// in `begin_task_dispatch` rather than re-derived in `confirm_task_dispatch`, so it
    /// can't drift if the board reshuffles while the user is typing.
    task_target: Option<String>,
    /// Initial message being typed to seed a not-yet-created branch's harness pane, valid
    /// while `mode == HarnessMessage`.
    pub harness_message_input: TextInput,
    /// A branch whose creation is deferred until the harness-message prompt confirms (or
    /// cancels), valid while `mode == HarnessMessage`. Set by `confirm_branch`, consumed
    /// by `confirm_harness_message`. Nothing is created with `but` until then — see
    /// `confirm_branch`'s doc comment — so an `Esc` here is a real, no-side-effect cancel.
    pending_branch: Option<PendingBranch>,
    /// While naming a branch: `Some(branch)` stacks the new one on top of it, `None` makes
    /// a parallel lane. Seeded from the selected lane and toggled with tab.
    pub stack_onto: Option<String>,
    /// While naming a branch: whether a parallel lane (`stack_onto.is_none()`) should get
    /// its own harness split (cmux or tmux). Only meaningful when a split backend is
    /// configured at all; irrelevant for a stacked branch, which never opens one
    /// regardless. Reset to `true` each time branch naming starts and toggled with
    /// shift-tab.
    pub open_harness: bool,
    /// Which row of the branch-creation modal `Left`/`Right`/`Up`/`Down` currently act on,
    /// valid while `mode == Branch` — see `BranchModalRow` and
    /// `App::branch_modal_row_down`/`_up`. Reset to `Name` each time branch naming starts.
    /// Meaningless (and unused) under the footer presentation (`BranchUi::Footer`), which
    /// has no rows to move a cursor between — that's the whole reason the modal exists.
    pub branch_modal_row: BranchModalRow,
    /// Footer or modal presentation for `Branch`/`HarnessMessage` — see [`BranchUi`].
    pub branch_ui: BranchUi,
    /// What a push would do, valid while `mode == PushConfirm`.
    pub push_preview: Option<PushPreview>,
    /// What landing the selected lane onto the target would do, valid while
    /// `mode == LandConfirm`.
    pub land_check: Option<MergeCheck>,
    /// The `but land` call in flight, valid while `mode == Landing`.
    pub landing: Option<PendingLand>,
    /// PR title being typed, valid while `mode == PrModal`.
    pub pr_title_input: TextInput,
    /// PR description being typed, valid while `mode == PrModal` — see [`PrModalRow::Message`].
    pub pr_message_input: TextInput,
    /// Whether the PR modal's draft toggle is on, valid while `mode == PrModal`. Reset to
    /// `false` each time the modal opens.
    pub pr_draft: bool,
    /// Which row of the PR modal `Up`/`Down`/`Left`/`Right`/`Tab` currently act on, valid
    /// while `mode == PrModal`. Reset to `Title` each time the modal opens.
    pub pr_modal_row: PrModalRow,
    /// The branch (and its lane title) a pending PR targets — set by `begin_pr`, consumed
    /// by `confirm_pr`. Valid while `mode == PrModal`.
    pr_target: Option<(String, String)>,
    /// The `but pr new` call in flight, valid while `mode == PrRunning`.
    pub pr_running: Option<PendingPr>,
    /// What a rebase onto the updated target would do, valid while `mode == RebaseConfirm`.
    pub pull_preview: Option<PullPreview>,
    /// The diff being read, valid while `mode == Diff`.
    pub diff: Option<DiffView>,
    /// Whether the diff takes the whole width. Split by default so the board stays
    /// visible — reading a diff should not cost you your place on the board.
    pub diff_full: bool,
    /// Line counts per commit hash. A commit's diff never changes, so this only ever
    /// grows and a refresh costs no extra subprocesses once warm.
    commit_stats: HashMap<String, (usize, usize)>,
    /// Set when a move was started from a hunk in the diff pane rather than from a card.
    /// Lets hunk staging reuse the ordinary lane-targeting flow unchanged.
    pub move_source: Option<(String, String)>,
    pub message: Option<(String, Notice)>,
    pub should_quit: bool,
    /// The workspace-wide block being shown, valid while `mode == Blocked`.
    pub blocked: Option<crate::but::WorkspaceBlock>,
    /// Printed on stdout after the terminal is restored, on the way out. Used by the
    /// teardown recovery, whose whole point is to leave GitButler mode — there is no board
    /// to come back to afterwards, so what it did has to survive the alternate screen.
    pub exit_note: Option<String>,
    /// Present only under `kanstack --tutorial`. Checked after every keystroke; absent for
    /// ordinary runs, so the check costs nothing outside that mode.
    pub tutorial: Option<Tutorial>,
    /// Bumped every time `self.board` is replaced, from any source. `poll_background_refresh`
    /// captures this before starting a background refresh and checks it again when the
    /// result comes back — if something else (a mutation, another refresh) has already
    /// replaced the board in the meantime, the background result is stale and is discarded
    /// rather than clobbering newer state with older.
    board_generation: u64,
    /// A watcher-driven refresh running on a background thread, so a file save elsewhere
    /// never blocks navigation while it's picked up. See `begin_background_refresh`.
    background_refresh: Option<PendingRefresh>,
    /// Whether the unassigned lane's cards are grouped by directory. GitHub issue #6: a
    /// flat list of loose files stops being navigable by eye once there are a lot of them.
    /// Toggled with `tab` while that lane is selected; reapplied in `clamp` so it survives
    /// every refresh rather than resetting the moment the board rebuilds.
    pub unassigned_grouped_by_folder: bool,
    /// The terminal's width as of the last frame drawn, kept here (not just in `ui`) so
    /// `Shift+←/→` paging knows how many lanes are actually on screen. `ui::draw` never
    /// mutates `App` — this is set from `main`'s event loop instead, right after each
    /// `terminal.draw`, which is the one place both the fresh size and the next keystroke
    /// are available before the other.
    pub terminal_width: u16,
    /// Cards picked for a bulk move, keyed by `rub_id` (stable across a refresh, unlike a
    /// column/index position). Toggled with `space`; `m` with a non-empty selection moves
    /// all of it in one action instead of just the card under the cursor. A `HashSet`
    /// rather than scoped to one lane on purpose — nothing about `but rub` requires the
    /// sources to share a column, so there is no reason to make kanstack pretend they must.
    pub selected: HashSet<String>,
    /// A split-pane-liveness poll running on a background thread; see
    /// `maybe_begin_split_poll`.
    split_poll: Option<PendingSplitPoll>,
    /// Wall-clock time of the last poll kickoff, so most ticks cost one comparison and
    /// nothing else. `None` until the first poll ever starts.
    split_poll_at: Option<std::time::Instant>,
    /// Branches that exist but are not in the workspace, for the drawer.
    ///
    /// Empty until the drawer is first opened, and refetched every time it opens or after
    /// an apply/unapply — never on the ordinary refresh path. `but status` cannot see these
    /// at all, so they cost a second subprocess, and one that does per-branch merge checks;
    /// paying that on every file save to populate a panel that is usually closed would be
    /// a poor trade. The consequence is that a branch someone creates in another terminal
    /// shows up when the drawer is next opened rather than instantly, which is the right
    /// side of that trade for a panel you deliberately open.
    pub unapplied: Unapplied,
    /// Cursor within the drawer, valid while `mode == Branches`.
    pub branch_sel: usize,
    /// Which branch a pending `Mode::DeleteConfirm` targets. See [`DeleteTarget`].
    delete_target: DeleteTarget,
    /// The drawer's detail view, when one is open. See [`BranchPreview`].
    pub branch_preview: Option<BranchPreview>,
    /// Where on screen everything from the last frame landed, for resolving the next mouse
    /// event. Rebuilt every draw (see `ui::draw`) rather than kept in sync incrementally.
    pub hit_map: crate::hit::HitMap,
    /// The card under a left-button press, while it's still down and hasn't (yet) dragged
    /// onto a different card or lane. `None` once a plain click is done, or once the press
    /// has turned into a `Moving` drag (see `on_mouse`).
    mouse_down_card: Option<(usize, usize)>,
    /// Whatever the pointer is over right now, from the last `MouseEventKind::Moved` — kept
    /// separate from `col`/`card` because hovering is where the mouse happens to sit, not
    /// where the keyboard cursor is; `ui::draw` only paints it while `mode == Normal` or
    /// `Branches`, wherever it was recorded from.
    pub hover: Option<crate::hit::HitTarget>,
    /// The card and moment of the last click, so a second click on the same one within
    /// `DOUBLE_CLICK_WINDOW` opens its diff instead of registering as two ordinary selects
    /// — crossterm reports each click as its own `Down`, with no double-click event of its
    /// own to key off.
    last_card_click: Option<((usize, usize), std::time::Instant)>,
}

/// How close together two clicks on the same card have to land to count as opening its
/// diff (like a keyboard `Enter`) rather than two unrelated selects.
const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);

type RefreshResult = Result<(Board, HashMap<String, (usize, usize)>)>;

/// A watcher-triggered refresh in flight on a background thread. Unlike [`PendingLand`]
/// this has no UI of its own — it's invisible on success, the same as `refresh_quietly`
/// always was, just no longer blocking while it runs.
struct PendingRefresh {
    rx: mpsc::Receiver<RefreshResult>,
    /// The board generation at the moment this refresh started; see `board_generation`.
    generation: u64,
}

/// A split-pane-liveness poll in flight on a background thread; see
/// `App::maybe_begin_split_poll`.
struct PendingSplitPoll {
    rx: mpsc::Receiver<Result<HashMap<String, crate::pane_status::PaneStatus>>>,
}

/// What a move actually does now that `but rub` is gone and every combination is its own
/// command. Computed once by `pending_op` and used both to describe the move (`pending_action`)
/// and to dispatch it (`confirm_move`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveOp {
    /// Uncommitted change(s) onto a lane with no commits yet — needs a message; see
    /// `PendingCommitMove`.
    Commit,
    /// Uncommitted change(s) onto a lane's tip, or a specific commit — no message needed,
    /// the target's own message is kept.
    Amend,
    /// A whole commit onto a lane.
    MoveToLane,
    /// A commit onto another commit.
    Squash,
    /// A commit back to the backlog.
    Uncommit,
}

impl MoveOp {
    fn verb(self) -> &'static str {
        match self {
            MoveOp::Commit => "commit to",
            MoveOp::Amend => "amend into",
            MoveOp::MoveToLane => "move to",
            MoveOp::Squash => "squash into",
            MoveOp::Uncommit => "uncommit into",
        }
    }
}

/// A move parked mid-flight because it needs a commit message before it can run — see
/// `MoveOp::Commit`. Holds everything `confirm_commit` needs to finish it.
struct PendingCommitMove {
    /// Uncommitted change ids (files or hunks) to commit.
    changes: Vec<String>,
    /// Branch to commit onto.
    branch: String,
    /// Whole-commit sources from the same bulk move, if any — applied with `move_commits`
    /// right after the initial commit succeeds and the lane is no longer empty.
    also_move: Vec<String>,
    source_label: String,
    target_label: String,
}

impl App {
    pub fn new(but: But, splitter: Option<Splitter>) -> Result<Self> {
        let mut commit_stats = HashMap::new();
        // A blocked workspace, or one that was never set up at all, starts the app rather
        // than aborting it. Bailing here printed one line of prose and vanished, which
        // reads as a crash and leaves the user to work out the recovery themselves — the
        // modal can explain and offer to run it.
        let mut needs_setup = false;
        let (board, blocked) = match but.status() {
            Ok(status) => (Self::board_from(&but, &mut commit_stats, &status), None),
            Err(e) if crate::but::is_workspace_block(&e.to_string()) => (
                Board::empty(),
                Some(but.diagnose_workspace_block(e.to_string())),
            ),
            Err(e) if crate::but::is_setup_required(&e.to_string()) => {
                needs_setup = true;
                (Board::empty(), None)
            }
            Err(e) => return Err(e),
        };
        let mut message = None;
        if but.is_untested_version() {
            message = Some((
                format!(
                    "but {} is newer than the {} this was verified against — \
                     report anything that looks wrong",
                    but.version(),
                    crate::but::VERIFIED_THROUGH
                ),
                Notice::Info,
            ));
        }
        Ok(App {
            but: Some(Arc::new(but)),
            splitter,
            board,
            col: 0,
            card: 0,
            mode: if blocked.is_some() {
                Mode::Blocked
            } else if needs_setup {
                Mode::SetupRequired
            } else {
                Mode::Normal
            },
            origin_col: 0,
            origin_card: 0,
            target_card: None,
            commit_input: TextInput::default(),
            pending_commit_move: None,
            branch_input: TextInput::default(),
            task_input: TextInput::default(),
            task_target: None,
            harness_message_input: TextInput::default(),
            pending_branch: None,
            stack_onto: None,
            open_harness: true,
            branch_modal_row: BranchModalRow::Name,
            branch_ui: BranchUi::from_env(),
            push_preview: None,
            land_check: None,
            landing: None,
            pr_title_input: TextInput::default(),
            pr_message_input: TextInput::default(),
            pr_draft: false,
            pr_modal_row: PrModalRow::Title,
            pr_target: None,
            pr_running: None,
            pull_preview: None,
            diff: None,
            diff_full: false,
            move_source: None,
            commit_stats,
            message,
            should_quit: false,
            blocked,
            exit_note: None,
            tutorial: None,
            board_generation: 0,
            background_refresh: None,
            unassigned_grouped_by_folder: false,
            terminal_width: 80,
            selected: HashSet::new(),
            split_poll: None,
            split_poll_at: None,
            unapplied: Unapplied::default(),
            branch_sel: 0,
            delete_target: DeleteTarget::Lane,
            branch_preview: None,
            hit_map: crate::hit::HitMap::new(),
            mouse_down_card: None,
            hover: None,
            last_card_click: None,
        })
    }

    /// Read-only view over a captured status. Used by `--snapshot` and by tests, so the
    /// exact rendering can be reproduced without anyone's repository.
    pub fn from_board(board: Board) -> Self {
        App {
            but: None,
            splitter: None,
            board,
            col: 0,
            card: 0,
            mode: Mode::Normal,
            origin_col: 0,
            origin_card: 0,
            target_card: None,
            commit_input: TextInput::default(),
            pending_commit_move: None,
            branch_input: TextInput::default(),
            task_input: TextInput::default(),
            task_target: None,
            harness_message_input: TextInput::default(),
            pending_branch: None,
            stack_onto: None,
            open_harness: true,
            branch_modal_row: BranchModalRow::Name,
            branch_ui: BranchUi::from_env(),
            push_preview: None,
            land_check: None,
            landing: None,
            pr_title_input: TextInput::default(),
            pr_message_input: TextInput::default(),
            pr_draft: false,
            pr_modal_row: PrModalRow::Title,
            pr_target: None,
            pr_running: None,
            pull_preview: None,
            diff: None,
            diff_full: false,
            move_source: None,
            commit_stats: HashMap::new(),
            message: None,
            should_quit: false,
            blocked: None,
            exit_note: None,
            tutorial: None,
            board_generation: 0,
            background_refresh: None,
            unassigned_grouped_by_folder: false,
            terminal_width: 80,
            selected: HashSet::new(),
            split_poll: None,
            split_poll_at: None,
            unapplied: Unapplied::default(),
            branch_sel: 0,
            delete_target: DeleteTarget::Lane,
            branch_preview: None,
            hit_map: crate::hit::HitMap::new(),
            mouse_down_card: None,
            hover: None,
            last_card_click: None,
        }
    }

    /// Whether a harness-split backend (cmux or tmux) was found at startup, i.e. whether
    /// shift-tab in branch mode does anything. See [`crate::splitter::Splitter::discover`].
    pub fn splitter_available(&self) -> bool {
        self.splitter.is_some()
    }

    /// The active split backend's name (`"cmux"` or `"tmux"`), for UI copy that needs to
    /// name it. Only meaningful when `splitter_available()` says there is one — falls back
    /// to the generic "split" otherwise, though every caller already guards on that first.
    pub fn splitter_label(&self) -> &'static str {
        self.splitter.as_ref().map(Splitter::label).unwrap_or("split")
    }

    pub fn column_count(&self) -> usize {
        self.board.columns.len()
    }

    pub fn selected_card(&self) -> Option<&Card> {
        self.board.columns.get(self.col)?.cards.get(self.card)
    }

    fn cards_in_current_column(&self) -> usize {
        self.board
            .columns
            .get(self.col)
            .map_or(0, |c| c.cards.len())
    }

    /// Keeps the cursor inside the board after the shape changes under it.
    fn clamp(&mut self) {
        self.board_generation = self.board_generation.wrapping_add(1);
        if self.unassigned_grouped_by_folder {
            if let Some(col) = self.board.columns.first_mut() {
                crate::board::group_unassigned_by_folder(col);
            }
        }
        if self.board.columns.is_empty() {
            self.col = 0;
            self.card = 0;
            return;
        }
        self.col = self.col.min(self.board.columns.len() - 1);
        let n = self.cards_in_current_column();
        self.card = if n == 0 { 0 } else { self.card.min(n - 1) };
        self.sync_pane_statuses();
    }

    /// Copies each tracked split pane's last known status onto the matching
    /// column/section, so `Board::build` never needs to know a split backend exists.
    /// Called from `clamp` after every board rebuild, and again on its own after a
    /// background poll resolves (`poll_split`), since a poll updates `self.splitter`'s
    /// cache without rebuilding the board at all.
    fn sync_pane_statuses(&mut self) {
        let Some(splitter) = &self.splitter else { return };
        for col in &mut self.board.columns {
            for section in &mut col.sections {
                section.pane_status = splitter.pane_status(&section.name);
            }
            col.pane_status = col.sections.first().and_then(|s| s.pane_status);
        }
    }

    pub fn notify(&mut self, msg: impl Into<String>, kind: Notice) {
        self.message = Some((msg.into(), kind));
    }

    /// Builds a board from a status, attaching line counts everywhere they belong.
    ///
    /// One `but diff` covers every uncommitted change at once. Commits need one
    /// `but diff <sha>` each, which would be ruinous per refresh — except that a commit's
    /// content is fixed by its hash, so the result caches forever. Only hashes never seen
    /// before cost anything, which in steady state is none of them: a refresh after
    /// editing a file re-reads the worktree diff and nothing else.
    fn board_from(
        but: &But,
        cache: &mut HashMap<String, (usize, usize)>,
        s: &crate::model::WorkspaceStatus,
    ) -> Board {
        for stack in &s.stacks {
            for branch in &stack.branches {
                for commit in &branch.commits {
                    if cache.contains_key(&commit.commit_id) {
                        continue;
                    }
                    // A commit whose diff cannot be read simply goes uncounted.
                    if let Ok(d) = but.diff_target(&commit.commit_id) {
                        let totals = crate::board::stats_by_path(&d)
                            .into_values()
                            .fold((0, 0), |(a, r), (ca, cr)| (a + ca, r + cr));
                        cache.insert(commit.commit_id.clone(), totals);
                    }
                }
            }
        }
        match but.diff_uncommitted() {
            Ok(d) => Board::from_status_diff_and_commits(s, &d, cache),
            // Counts are a nicety; a board without them beats no board.
            Err(_) => Board::from_status(s),
        }
    }

    pub fn refresh(&mut self) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.status() {
            Ok(s) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &s);
                self.clamp();
            }
            Err(e) => self.note_refresh_failure(e),
        }
    }

    /// Routes a failed status read. Ordinary failures are a passing notice — the board is
    /// still broadly true and the next refresh will probably work. The workspace-wide block
    /// is not: nothing will work again until it is resolved, so it gets a mode of its own
    /// rather than a message that scrolls away over a board quietly frozen in the past.
    fn note_refresh_failure(&mut self, e: anyhow::Error) {
        let msg = e.to_string();
        if crate::but::is_workspace_block(&msg) {
            self.enter_blocked(msg);
        } else if crate::but::is_setup_required(&msg) {
            self.mode = Mode::SetupRequired;
        } else {
            self.notify(format!("refresh failed: {msg}"), Notice::Error);
        }
    }

    /// Stops the board and explains itself. Idempotent, because every refresh from here on
    /// will keep failing the same way until the user acts.
    fn enter_blocked(&mut self, message: String) {
        if self.mode == Mode::Blocked {
            return;
        }
        self.blocked = Some(match &self.but {
            Some(but) => but.diagnose_workspace_block(message),
            None => crate::but::WorkspaceBlock {
                message,
                workspace_sha: None,
                stray: Vec::new(),
            },
        });
        self.mode = Mode::Blocked;
        // Drop anything half-finished. All of it targets a board that no longer describes
        // the repository, so resuming it after a recovery would be acting on stale ids.
        self.message = None;
        self.diff = None;
        self.push_preview = None;
        self.land_check = None;
        self.pull_preview = None;
        self.pr_target = None;
        self.move_source = None;
        self.selected.clear();
    }

    /// Refresh driven by the filesystem watcher rather than by the user.
    ///
    /// Deliberately silent on success — the whole point is that following the repository
    /// is background behaviour, not something worth a notification each time you save a
    /// file. Failures still surface, since a board that has quietly stopped updating is
    /// worse than one that says so.
    pub fn refresh_quietly(&mut self) {
        let Some(but) = &self.but else { return };
        match but.status() {
            Ok(s) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &s);
                self.clamp();
            }
            Err(e) => self.note_refresh_failure(e),
        }
    }

    /// Starts a watcher-driven refresh on a background thread, if one isn't already in
    /// flight. `but status` plus `but diff` together cost around 90ms of fixed subprocess
    /// overhead regardless of repository size — small in absolute terms, but enough to
    /// stutter navigation if it runs on the thread that also reads keys, since a save in
    /// another window can trigger this at any moment, including mid-navigation. Unlike
    /// `refresh_quietly`, the caller (the watcher path in `main`) never blocks on this.
    pub fn begin_background_refresh(&mut self) {
        if self.background_refresh.is_some() {
            return;
        }
        let Some(but) = self.but.clone() else {
            return;
        };
        let cache = self.commit_stats.clone();
        let generation = self.board_generation;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = but.status().map(|s| {
                let mut cache = cache;
                let board = Self::board_from(&but, &mut cache, &s);
                (board, cache)
            });
            let _ = tx.send(result);
        });
        self.background_refresh = Some(PendingRefresh { rx, generation });
    }

    /// Whether a background refresh is still in flight. Exposed for tests that need to
    /// poll until one resolves; the UI itself has nothing to show while this is `true` —
    /// see `refresh_quietly`'s doc comment for why that's deliberate.
    pub fn background_refresh_is_pending(&self) -> bool {
        self.background_refresh.is_some()
    }

    /// Non-blocking check on a background refresh started by `begin_background_refresh`.
    /// Call this every tick of the event loop, the same as `poll_land`.
    pub fn poll_background_refresh(&mut self) {
        let Some(pending) = &self.background_refresh else {
            return;
        };
        match pending.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.background_refresh = None;
            }
            Ok(Ok((board, cache))) => {
                // Something else already replaced the board since this refresh started
                // (a mutation, another refresh) — that result is newer than this one, so
                // applying this would go backwards. Just drop it; the watcher will fire
                // again if anything is still actually out of date.
                let stale = pending.generation != self.board_generation;
                self.background_refresh = None;
                if !stale {
                    self.board = board;
                    self.commit_stats = cache;
                    self.clamp();
                }
            }
            Ok(Err(e)) => {
                self.background_refresh = None;
                self.note_refresh_failure(e);
            }
        }
    }

    /// Moves the branch ref back onto the workspace commit and picks the board back up.
    ///
    /// The surgical route, and the default one: it keeps GitButler mode on, so the board
    /// comes straight back, and it discards nothing — the stray commits' changes return as
    /// uncommitted work, ready to be committed properly, and the commits themselves stay in
    /// the reflog.
    fn recover_reset(&mut self) {
        let Some(sha) = self.blocked.as_ref().and_then(|b| b.workspace_sha.clone()) else {
            self.notify(
                "could not identify the workspace commit here — use teardown instead",
                Notice::Error,
            );
            return;
        };
        let Some(but) = self.but.clone() else { return };
        if let Err(e) = but.reset_soft(&sha) {
            self.notify(format!("reset failed: {e}"), Notice::Error);
            return;
        }
        let strays = self.blocked.as_ref().map(|b| b.stray.len()).unwrap_or(0);
        self.blocked = None;
        self.mode = Mode::Normal;
        // Straight back through the ordinary path, which re-enters `Blocked` by itself if
        // the reset somehow did not take — better than assuming it worked.
        self.refresh();
        if self.mode == Mode::Normal {
            self.notify(
                format!(
                    "workspace restored — {strays} commit{} back as uncommitted changes",
                    if strays == 1 { "" } else { "s" }
                ),
                Notice::Success,
            );
        }
    }

    /// Runs `but teardown` and leaves.
    ///
    /// Quitting is not incidental here: teardown's whole purpose is to exit GitButler mode
    /// and check out an ordinary branch, so there is deliberately no workspace left for the
    /// board to draw afterwards. `but setup` is left to the user rather than chained on
    /// automatically — re-entering GitButler mode is a second decision, and hiding it
    /// behind the same keypress would be doing more than was asked.
    fn recover_teardown(&mut self) {
        let Some(but) = self.but.clone() else { return };
        match but.teardown() {
            Ok(out) => {
                self.exit_note = Some(format!(
                    "{}\n\
                     GitButler mode is off, so there is no board to draw. To come back:\n\
                     \n    but setup && kanstack\n",
                    out.trim_end()
                ));
                self.should_quit = true;
            }
            Err(e) => self.notify(format!("teardown failed: {e}"), Notice::Error),
        }
    }

    /// How often `maybe_begin_split_poll` is willing to start a new poll.
    const SPLIT_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

    /// Kicks off a background split-pane liveness poll if one isn't already in flight,
    /// enough time has passed since the last one, and there's at least one pane worth
    /// asking about. Gated on `Splitter::running_inside_host` — kanstack itself still
    /// looking like it's running inside that backend's own pane — the same kind of guard
    /// `Cmux::occupant_in_direction` uses for its own env-var check, rather than querying
    /// an ambiguous default scope when it isn't. Call this every tick, the same as
    /// `begin_background_refresh`.
    pub fn maybe_begin_split_poll(&mut self) {
        let Some(splitter) = &self.splitter else { return };
        if splitter.is_empty() || self.split_poll.is_some() {
            return;
        }
        if !splitter.running_inside_host() {
            return;
        }
        if self
            .split_poll_at
            .is_some_and(|t| t.elapsed() < Self::SPLIT_POLL_INTERVAL)
        {
            return;
        }
        self.split_poll_at = Some(std::time::Instant::now());
        let snapshot = splitter.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(snapshot.poll_statuses());
        });
        self.split_poll = Some(PendingSplitPoll { rx });
    }

    /// Non-blocking check on a poll started by `maybe_begin_split_poll`. Call every tick,
    /// the same as `poll_background_refresh`.
    pub fn poll_split(&mut self) {
        let Some(pending) = &self.split_poll else { return };
        match pending.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.split_poll = None,
            Ok(Ok(statuses)) => {
                self.split_poll = None;
                if let Some(splitter) = &mut self.splitter {
                    splitter.apply_statuses(statuses);
                }
                self.sync_pane_statuses();
            }
            // Transient — a hiccup every few seconds shouldn't spam the footer the way a
            // user-triggered action's failure should.
            Ok(Err(_)) => self.split_poll = None,
        }
    }

    /// Toggles whether the unassigned lane's cards are grouped by directory (GitHub issue
    /// #6: a flat list of loose files stops being navigable by eye once there are a lot of
    /// them). Scoped to that lane on purpose — `tab` does nothing anywhere else in Normal
    /// mode today, and toggling a view option for a lane you are not looking at would be a
    /// surprising thing for a key to do.
    ///
    /// Re-fetches rather than un-sorting the in-memory board in place: sorting is
    /// destructive to the original order, so turning grouping back off needs a real source
    /// of truth to restore from, not just an inverse of `group_unassigned_by_folder`.
    fn toggle_unassigned_grouping(&mut self) {
        let on_unassigned = self
            .board
            .columns
            .get(self.col)
            .is_some_and(|c| c.kind == crate::board::ColumnKind::Unassigned);
        if !on_unassigned {
            self.notify(
                "tab groups the unassigned lane by folder — select it first",
                Notice::Info,
            );
            return;
        }

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        self.unassigned_grouped_by_folder = !self.unassigned_grouped_by_folder;
        let grouped = self.unassigned_grouped_by_folder;
        match but.status() {
            Ok(s) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &s);
                self.clamp();
                self.notify(
                    if grouped {
                        "unassigned grouped by folder"
                    } else {
                        "unassigned back to a flat list"
                    },
                    Notice::Info,
                );
            }
            Err(e) => self.notify(format!("refresh failed: {e}"), Notice::Error),
        }
    }

    fn move_column(&mut self, delta: isize) {
        let n = self.column_count();
        if n == 0 {
            return;
        }
        let next = (self.col as isize + delta).rem_euclid(n as isize) as usize;
        self.col = next;
        // Landing in a new lane, keep the cursor on a real card.
        let count = self.cards_in_current_column();
        self.card = if count == 0 {
            0
        } else {
            self.card.min(count - 1)
        };
    }

    /// Moves the card cursor, wrapping at the ends the way lane movement does.
    fn move_card(&mut self, delta: isize) {
        let n = self.cards_in_current_column();
        if n == 0 {
            return;
        }
        self.card = (self.card as isize + delta).rem_euclid(n as isize) as usize;
    }

    /// `Shift+←/→`: jumps by however many lanes are actually on screen at once, rather
    /// than one at a time — the board equivalent of Page Up/Down. Reuses `move_column`,
    /// which already wraps and keeps the card cursor on a real card in the new lane.
    fn move_column_page(&mut self, direction: isize) {
        let (_, fit) = columns_that_fit(self.column_count(), self.terminal_width);
        self.move_column(fit.max(1) as isize * direction);
    }

    /// `Shift+↑/↓`: jumps to the start of the next/previous group of cards within the
    /// lane — a stacked branch's own commits, or (when grouped) a folder in the
    /// unassigned lane — rather than moving one card at a time. Falls back to an ordinary
    /// single-card move in a lane with nothing to skip between, so the key never just does
    /// nothing.
    fn move_card_by_group(&mut self, forward: bool) {
        let Some(col) = self.board.columns.get(self.col) else {
            return;
        };
        if col.cards.is_empty() {
            return;
        }

        // The index of the first card in each run of cards sharing the same `group`.
        let mut starts = Vec::new();
        let mut last: Option<&str> = None;
        for (i, card) in col.cards.iter().enumerate() {
            let group = card.group.as_deref();
            if group.is_some() && group != last {
                starts.push(i);
            }
            last = group;
        }
        if starts.len() <= 1 {
            // Nothing to skip *between*: either no groups at all (a plain lane), or —
            // edge case, but real: unassigned grouped by folder with everything in one
            // folder — exactly one. Stepping "to the next group" in either case would
            // just land back where the cursor already is, which looks exactly like the
            // key doing nothing. Fall back to an ordinary move instead.
            self.move_card(if forward { 1 } else { -1 });
            return;
        }

        // Where the cursor currently sits among the group starts, then step one group in
        // the requested direction, wrapping the same way every other move here does.
        let pos = starts.iter().rposition(|&s| s <= self.card).unwrap_or(0);
        let next = if forward {
            (pos + 1) % starts.len()
        } else {
            (pos + starts.len() - 1) % starts.len()
        };
        self.card = starts[next];
    }

    /// Handles a keystroke, then — under `--tutorial` — checks whether it satisfied the
    /// current step. Wrapping rather than checking inline: `handle_key` returns early from
    /// many different branches depending on mode, and the check needs to run after all of
    /// them, not just whichever one happens to fall through to the end.
    pub fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        self.handle_key(key);
        self.check_tutorial_advance();
    }

    /// Under `--tutorial`, checks whether the current step is now satisfied.
    ///
    /// Not folded into `on_key` alone: a land now finishes on a background thread (see
    /// `poll_land`), so the state a step is waiting on can change between keystrokes rather
    /// than only in direct response to one. Both call sites need this, or the "press M to
    /// land" step would never advance once the land it started actually completes.
    fn check_tutorial_advance(&mut self) {
        if let Some(mut t) = self.tutorial.take() {
            t.advance(self);
            self.tutorial = Some(t);
        }
    }

    /// Handles a mouse event against `hit_map`, which reflects the frame that was on
    /// screen when the event arrived (see `main::run`, which fills it in right after each
    /// `terminal.draw`).
    ///
    /// Where an action already exists as a keystroke — confirming or cancelling a dialog,
    /// dismissing help, scrolling — this synthesizes that keystroke through `handle_key`
    /// rather than re-implementing per-mode dispatch a second time. Only clicking and
    /// dragging on the board itself, which has no keyboard equivalent for jumping straight
    /// to an arbitrary card, are handled directly.
    pub fn on_mouse(&mut self, ev: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};

        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.mouse_down_card = None;
                match self.hit_map.hit_test(ev.column, ev.row) {
                    Some(crate::hit::HitTarget::DialogConfirm) => {
                        self.handle_key(KeyEvent::from(KeyCode::Enter));
                    }
                    Some(crate::hit::HitTarget::BranchToggleSplit) => {
                        self.handle_key(KeyEvent::from(KeyCode::BackTab));
                    }
                    Some(crate::hit::HitTarget::DialogCancel) => {
                        self.handle_key(KeyEvent::from(KeyCode::Esc));
                    }
                    Some(crate::hit::HitTarget::Dismiss) => {
                        self.handle_key(KeyEvent::from(KeyCode::Esc));
                    }
                    Some(crate::hit::HitTarget::OpenBranches) if self.mode == Mode::Normal => {
                        self.handle_key(KeyEvent::from(KeyCode::Char('a')));
                    }
                    Some(crate::hit::HitTarget::BranchRow(i))
                        if self.mode == Mode::Branches && i < self.unapplied.branches.len() =>
                    {
                        self.branch_sel = i;
                    }
                    Some(crate::hit::HitTarget::Card(col, card)) if self.mode == Mode::Normal => {
                        let double_click = self.last_card_click.is_some_and(|(at_card, at)| {
                            at_card == (col, card) && at.elapsed() < DOUBLE_CLICK_WINDOW
                        });
                        self.last_card_click = Some(((col, card), std::time::Instant::now()));
                        self.col = col;
                        self.card = card;
                        if double_click {
                            self.handle_key(KeyEvent::from(KeyCode::Enter));
                        } else {
                            self.mouse_down_card = Some((col, card));
                            // The unassigned lane is the one place a click doubles as
                            // `space`: its cards are loose files rather than commits, so
                            // building up a bulk move by clicking through them is the
                            // whole point of a mouse there.
                            if self.board.columns.get(col).is_some_and(|c| c.kind == ColumnKind::Unassigned)
                            {
                                self.toggle_selected();
                            }
                        }
                    }
                    Some(
                        crate::hit::HitTarget::LaneHeader(col) | crate::hit::HitTarget::LaneBody(col),
                    ) if self.mode == Mode::Normal => {
                        self.col = col;
                        let count = self.cards_in_current_column();
                        self.card = if count == 0 { 0 } else { self.card.min(count - 1) };
                    }
                    _ => {}
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(origin) = self.mouse_down_card else {
                    return;
                };
                let target = self.hit_map.hit_test(ev.column, ev.row);
                if self.mode == Mode::Normal {
                    let moved_off = match target {
                        Some(crate::hit::HitTarget::Card(c, ci)) => (c, Some(ci)) != (origin.0, Some(origin.1)),
                        Some(
                            crate::hit::HitTarget::LaneHeader(c) | crate::hit::HitTarget::LaneBody(c),
                        ) => c != origin.0,
                        _ => false,
                    };
                    if !moved_off {
                        return;
                    }
                    // `begin_move` picks up whatever's under `self.col`/`self.card`, which
                    // the `Down` handler already set to `origin` — same trick the keyboard
                    // `m` binding relies on.
                    self.begin_move();
                    if self.mode != Mode::Moving {
                        // Nothing to move (see `begin_move`'s own check) — nothing dragging.
                        self.mouse_down_card = None;
                        return;
                    }
                }
                if self.mode == Mode::Moving {
                    match target {
                        Some(crate::hit::HitTarget::Card(c, ci)) => {
                            self.col = c;
                            self.target_card = Some(ci);
                        }
                        Some(
                            crate::hit::HitTarget::LaneHeader(c) | crate::hit::HitTarget::LaneBody(c),
                        ) => {
                            self.col = c;
                            self.target_card = None;
                        }
                        _ => {}
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left)
                if self.mouse_down_card.take().is_some() && self.mode == Mode::Moving =>
            {
                self.confirm_move();
            }
            MouseEventKind::Moved => {
                self.hover = self.hit_map.hit_test(ev.column, ev.row);
            }
            MouseEventKind::ScrollDown => self.handle_key(KeyEvent::from(KeyCode::Down)),
            MouseEventKind::ScrollUp => self.handle_key(KeyEvent::from(KeyCode::Up)),
            _ => {}
        }

        self.check_tutorial_advance();
    }
    fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode as K;
        use ratatui::crossterm::event::KeyModifiers;

        // Each mode with its own key set delegates to a `handle_key_*` method living
        // alongside that feature's state and mutations — see the module doc comment.
        // Mutually exclusive on `self.mode`, so unlike the old sequential if-chain this
        // order carries no meaning; each mode still documents on its own handler why it
        // swallows what it swallows.
        match self.mode {
            Mode::Blocked => return self.handle_key_blocked(key),
            Mode::SetupRequired => return self.handle_key_setup_required(key),
            Mode::Commit => return self.handle_key_commit(key),
            Mode::Branch => return self.handle_key_branch(key),
            Mode::Task => return self.handle_key_task(key),
            Mode::HarnessMessage => return self.handle_key_harness_message(key),
            Mode::PushConfirm => return self.handle_key_push_confirm(key),
            Mode::LandConfirm => return self.handle_key_land_confirm(key),
            Mode::Landing => return self.handle_key_landing(key),
            Mode::PrModal => return self.handle_key_pr_modal(key),
            Mode::PrRunning => return self.handle_key_pr_running(key),
            Mode::Diff => return self.handle_key_diff(key),
            Mode::RebaseConfirm => return self.handle_key_rebase_confirm(key),
            Mode::DeleteConfirm => return self.handle_key_delete_confirm(key),
            Mode::UnapplyConfirm => return self.handle_key_unapply_confirm(key),
            Mode::Branches => return self.handle_key_branches(key),
            // Help swallows everything except the keys that dismiss it. Small enough (and
            // general enough — it isn't really "a feature") to leave right here rather
            // than in a file of its own.
            Mode::Help => {
                if matches!(key.code, K::Esc | K::Char('?') | K::Char('q')) {
                    self.mode = Mode::Normal;
                }
                return;
            }
            Mode::Normal | Mode::Moving | Mode::Restacking => {}
        }

        self.message = None;

        match key.code {
            K::Char('q') if self.mode == Mode::Normal => self.should_quit = true,
            // Esc means "cancel, go back" everywhere else in this app — Commit, Branch,
            // every confirm dialog, Diff, Help. It quitting outright from Normal mode with
            // no confirmation was the one inconsistent case, and the exact key most people
            // reach for on reflex to back out of something: reported (GitHub issue #2) as
            // an unexplained "crash" — the app just vanished, no message, because that is
            // what an instant, silent quit looks like. `q` is the documented, deliberate
            // quit key; Esc at the top level now does nothing, matching "nothing left to
            // cancel back to" rather than "quit".
            K::Esc => match self.mode {
                Mode::Moving => {
                    self.mode = Mode::Normal;
                    self.move_source = None;
                    self.col = self.origin_col;
                    self.notify("move cancelled", Notice::Info);
                }
                Mode::Restacking => {
                    self.mode = Mode::Normal;
                    self.col = self.origin_col;
                    self.notify("restack cancelled", Notice::Info);
                }
                // Esc cancelling a move deliberately leaves a selection alone (see
                // `begin_move`'s doc comment) so a bulk move can be retried at a
                // different target; this is the "actually give it up" gesture instead.
                Mode::Normal if !self.selected.is_empty() => {
                    let n = self.selected.len();
                    self.selected.clear();
                    self.notify(format!("cleared {n} selected"), Notice::Info);
                }
                _ => {}
            },
            K::Char('?') => self.mode = Mode::Help,
            // Shift+←/→ pages by however many lanes are actually on screen, rather than
            // one at a time — the same jump `‹`/`›` already advertise is there to make.
            // Shift+↑/↓ skips to the next group boundary within the lane instead: the next
            // stacked branch's own commits, or (when grouped) the next folder in the
            // unassigned lane. Scoped to Normal mode so it doesn't fight the Moving/
            // Restacking arrows' own special-cased meaning for the same keys.
            K::Left if key.modifiers.contains(KeyModifiers::SHIFT) && self.mode == Mode::Normal => {
                self.move_column_page(-1);
            }
            K::Right if key.modifiers.contains(KeyModifiers::SHIFT) && self.mode == Mode::Normal => {
                self.move_column_page(1);
            }
            K::Up if key.modifiers.contains(KeyModifiers::SHIFT) && self.mode == Mode::Normal => {
                self.move_card_by_group(false);
            }
            K::Down if key.modifiers.contains(KeyModifiers::SHIFT) && self.mode == Mode::Normal => {
                self.move_card_by_group(true);
            }
            // Changing lane while moving resets the drop position to the lane header.
            // While restacking, only branch lanes are valid targets, so skip the rest.
            K::Left | K::Char('h') => {
                if self.mode == Mode::Restacking {
                    if let Some(i) = self.prev_stack_column(self.col) {
                        self.col = i;
                    }
                } else {
                    self.move_column(-1);
                    self.target_card = None;
                }
            }
            K::Right | K::Char('l') => {
                if self.mode == Mode::Restacking {
                    if let Some(i) = self.next_stack_column(self.col) {
                        self.col = i;
                    }
                } else {
                    self.move_column(1);
                    self.target_card = None;
                }
            }
            K::Up | K::Char('k') => {
                if self.mode == Mode::Moving {
                    self.move_target(-1)
                } else {
                    self.move_card(-1)
                }
            }
            K::Down | K::Char('j') => {
                if self.mode == Mode::Moving {
                    self.move_target(1)
                } else {
                    self.move_card(1)
                }
            }
            K::Char('g') if self.mode == Mode::Normal => self.card = 0,
            K::Char('G') if self.mode == Mode::Normal => {
                self.card = self.cards_in_current_column().saturating_sub(1)
            }
            K::Char(' ') if self.mode == Mode::Normal => self.toggle_selected(),
            K::Char('m') if self.mode == Mode::Normal => self.begin_move(),
            K::Char('p') if self.mode == Mode::Normal => self.begin_push(),
            K::Char('L') if self.mode == Mode::Normal => self.begin_land(),
            K::Char('M') if self.mode == Mode::Normal => self.begin_pr(),
            K::Char('z') if self.mode == Mode::Normal => self.undo(),
            K::Char('Z') if self.mode == Mode::Normal => self.redo(),
            K::Char('b') if self.mode == Mode::Normal => self.begin_branch(),
            K::Char('t') if self.mode == Mode::Normal => self.begin_task_dispatch(),
            K::Char('s') if self.mode == Mode::Normal => self.begin_restack(),
            K::Char('u') if self.mode == Mode::Normal => self.send_to_backlog(),
            K::Char('d') if self.mode == Mode::Normal => self.begin_delete(),
            K::Char('a') if self.mode == Mode::Normal => self.toggle_branch_drawer(),
            K::Char('U') if self.mode == Mode::Normal => self.begin_unapply(),
            K::Char('r') if self.mode == Mode::Normal => self.begin_rebase(),
            K::Tab if self.mode == Mode::Normal => self.toggle_unassigned_grouping(),
            K::Enter if self.mode == Mode::Normal => self.open_diff(),
            K::Enter if self.mode == Mode::Restacking => self.confirm_restack(),
            K::Enter if self.mode == Mode::Moving => self.confirm_move(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::but::parse_status;
    use crate::hit::{HitMap, HitTarget};
    use ratatui::layout::Rect;

    /// Builds an App without spawning `but`. Only navigation is exercised here;
    /// anything that mutates needs the real CLI and is covered by hand.
    fn board() -> Board {
        Board::from_status(&parse_status(include_str!("../../tests/fixtures/status.json")).unwrap())
    }

    /// Mirrors `App`'s cursor rules against a board, so navigation can be tested without
    /// a `But` handle. Kept in sync by construction: both call the same helpers.
    struct Nav {
        board: Board,
        col: usize,
        card: usize,
    }

    impl Nav {
        fn new() -> Self {
            Nav {
                board: board(),
                col: 0,
                card: 0,
            }
        }
        fn cards(&self) -> usize {
            self.board.columns[self.col].cards.len()
        }
        fn right(&mut self) {
            self.col = (self.col + 1) % self.board.columns.len();
            let n = self.cards();
            self.card = if n == 0 { 0 } else { self.card.min(n - 1) };
        }
    }

    #[test]
    fn changing_lane_keeps_cursor_on_a_real_card() {
        let mut n = Nav::new();
        n.card = 1; // second card in the backlog
        n.right(); // feat-auth has 2 cards
        assert_eq!(n.card, 1);
        n.right(); // feat-ui has 2
        assert_eq!(n.card, 1);
        n.right(); // fix-flaky-tests has only 1
        assert_eq!(n.card, 0, "cursor clamps instead of pointing past the end");
    }

    /// Mirrors `pending_verb`'s card-target arm. Dropping onto a card is the whole of
    /// squash and amend, so the combinations are worth pinning down explicitly.
    fn verb_onto_card(source: CardKind, target: CardKind) -> Option<&'static str> {
        match (source, target) {
            (CardKind::Commit, CardKind::Commit) => Some("squash into"),
            (CardKind::Change, CardKind::Commit) => Some("amend into"),
            (_, CardKind::Change) => None,
        }
    }

    #[test]
    fn dropping_onto_a_card_is_squash_or_amend() {
        assert_eq!(
            verb_onto_card(CardKind::Commit, CardKind::Commit),
            Some("squash into")
        );
        assert_eq!(
            verb_onto_card(CardKind::Change, CardKind::Commit),
            Some("amend into")
        );
    }

    #[test]
    fn dropping_onto_a_file_card_is_refused_not_guessed() {
        // The rub matrix has no file-onto-file operation; better to refuse than invent one.
        assert_eq!(verb_onto_card(CardKind::Commit, CardKind::Change), None);
        assert_eq!(verb_onto_card(CardKind::Change, CardKind::Change), None);
    }

    #[test]
    fn the_drop_cursor_cycles_through_the_header_and_cards() {
        // -1 is the lane header, so there are n + 1 positions and they wrap, matching how
        // lane and card movement behave everywhere else.
        let n: isize = 2;
        let step = |cur: Option<usize>, delta: isize| -> Option<usize> {
            let c = cur.map_or(-1, |i| i as isize);
            let next = (c + 1 + delta).rem_euclid(n + 1) - 1;
            if next < 0 {
                None
            } else {
                Some(next as usize)
            }
        };
        assert_eq!(step(None, 1), Some(0), "down from the header lands on card 0");
        assert_eq!(step(Some(0), 1), Some(1));
        assert_eq!(step(Some(1), 1), None, "past the last card, back to the header");
        assert_eq!(step(Some(0), -1), None, "up from card 0 returns to the header");
        assert_eq!(step(None, -1), Some(1), "up from the header wraps to the last card");
    }

    #[test]
    fn shift_left_right_pages_by_however_many_lanes_fit() {
        let mut app = App::from_board(board()); // [unassigned, feat-auth, feat-ui, fix-flaky-tests]
        app.terminal_width = 60;
        let (_, fit) = columns_that_fit(app.column_count(), app.terminal_width);
        assert_eq!(fit, 2, "fixture for this test assumes a width where 2 of 4 lanes fit");

        app.move_column_page(1);
        assert_eq!(app.col, 2, "one page forward lands exactly `fit` lanes over");

        app.move_column_page(1);
        assert_eq!(app.col, 0, "another page wraps back around, same as ordinary lane movement");
    }

    #[test]
    fn shift_left_right_wraps_backward_too() {
        let mut app = App::from_board(board());
        app.terminal_width = 60; // fit == 2, see the test above
        app.col = 0;
        app.move_column_page(-1);
        assert_eq!(app.col, 2, "one page back from the start wraps to the far side");
    }

    /// Builds a lane with two stacked branches (feat-ui folded into feat-auth), so
    /// Shift+↑/↓'s "skip to the next branch" behaviour has something real to skip between.
    fn board_with_a_stacked_lane() -> Board {
        let mut s = parse_status(include_str!("../../tests/fixtures/status.json")).unwrap();
        let extra = s.stacks.remove(2).branches.remove(0);
        s.stacks[0].branches.push(extra);
        Board::from_status(&s)
    }

    #[test]
    fn shift_up_down_skips_to_the_next_stacked_branchs_commits() {
        let mut app = App::from_board(board_with_a_stacked_lane());
        app.col = 1; // "feat-auth +1", the stacked lane
        let groups: Vec<Option<&str>> = app.board.columns[1]
            .cards
            .iter()
            .map(|c| c.group.as_deref())
            .collect();
        assert!(
            groups.iter().any(|g| g.is_some()) && groups.iter().any(|g| g != &groups[0]),
            "test fixture must actually have more than one group to skip between: {groups:?}"
        );

        app.card = 0;
        app.move_card_by_group(true);
        let landed_group = app.board.columns[1].cards[app.card].group.clone();
        assert_ne!(
            landed_group,
            app.board.columns[1].cards[0].group,
            "moved into a different group, not just the next card"
        );

        // And back down should return to the start of the first group.
        app.move_card_by_group(false);
        assert_eq!(app.card, 0);
    }

    #[test]
    fn shift_up_down_falls_back_to_a_plain_move_with_nothing_to_skip() {
        let mut app = App::from_board(board()); // no stacked lanes, nothing grouped
        app.col = 2; // "feat-ui", two commits, no groups
        app.card = 0;
        app.move_card_by_group(true);
        assert_eq!(app.card, 1, "same as an ordinary ↓ with no group boundary to jump to");
    }

    /// Regression: unassigned grouped by folder with everything landing in the same
    /// folder has exactly one group, not zero — every card's `group` is `Some`, so the
    /// earlier `starts.is_empty()` fallback check missed this case entirely. Stepping "to
    /// the next group" landed back on the one group's own start, which if the cursor was
    /// already there (or already past it) looked exactly like the key doing nothing.
    #[test]
    fn shift_up_down_falls_back_when_theres_only_one_group() {
        let mut app = App::from_board(board());
        app.col = 0; // unassigned
        for card in &mut app.board.columns[0].cards {
            card.group = Some("src".into());
        }
        let n = app.board.columns[0].cards.len();
        assert!(n > 1, "test fixture needs more than one card in the backlog");

        app.card = 0;
        app.move_card_by_group(true);
        assert_eq!(
            app.card, 1,
            "falls back to an ordinary move instead of landing back on the same group start"
        );
    }

    #[test]
    fn the_card_cursor_wraps_like_the_lane_cursor() {
        let n = 3isize;
        let step = |cur: usize, delta: isize| (cur as isize + delta).rem_euclid(n) as usize;
        assert_eq!(step(2, 1), 0, "past the last card, back to the first");
        assert_eq!(step(0, -1), 2, "before the first, round to the last");
        assert_eq!(step(1, 1), 2);
    }

    /// Regression: the restack cursor used to just step one column, which from the last
    /// lane wraps onto the backlog — a lane with no branch, so confirming silently did
    /// nothing. Valid targets are branch lanes other than the origin.
    #[test]
    fn restack_targeting_skips_the_backlog_and_the_origin() {
        let b = board(); // [unassigned, feat-auth, feat-ui, fix-flaky-tests]
        let origin = 3usize;
        let n = b.columns.len();
        let next = |from: usize| -> Option<usize> {
            (1..=n)
                .map(|step| (from + step) % n)
                .find(|&i| i != origin && b.columns[i].branch_name.is_some())
        };

        // From the last lane, wrapping must land on a branch lane, not the backlog at 0.
        assert_eq!(next(origin), Some(1));
        assert_eq!(next(1), Some(2));
        assert_eq!(next(2), Some(1), "wraps past both the origin and the backlog");
        assert!(b.columns[0].branch_name.is_none(), "index 0 is the backlog");
    }

    #[test]
    fn pending_action_names_the_rub_semantics() {
        // Commit landing on the backlog is an uncommit, and must say so.
        let b = board();
        let commit = b.columns[1].cards[0].clone();
        assert_eq!(commit.kind, CardKind::Commit);
        assert_eq!(b.columns[0].kind, ColumnKind::Unassigned);

        let file = b.columns[0].cards[0].clone();
        assert_eq!(file.kind, CardKind::Change);

        let verb = |c: CardKind, t: ColumnKind| match (c, t) {
            (CardKind::Change, ColumnKind::Unassigned) => "unstage",
            (CardKind::Change, ColumnKind::Stack) => "stage to",
            (CardKind::Commit, ColumnKind::Unassigned) => "uncommit into",
            (CardKind::Commit, ColumnKind::Stack) => "move to",
        };
        assert_eq!(verb(commit.kind, ColumnKind::Unassigned), "uncommit into");
        assert_eq!(verb(file.kind, ColumnKind::Stack), "stage to");
    }

    fn key(code: ratatui::crossterm::event::KeyCode) -> ratatui::crossterm::event::KeyEvent {
        ratatui::crossterm::event::KeyEvent::from(code)
    }

    /// Puts an app into the blocked state without needing a broken repository behind it.
    fn blocked_app() -> App {
        let mut app = App::from_board(board());
        app.blocked = Some(crate::but::WorkspaceBlock {
            message: "Error: GitButler mode exit required: please run `but teardown`".into(),
            workspace_sha: Some("f6b543ef29e7a069c1b85c8039ac90d6c1130f8c".into()),
            stray: vec![crate::but::StrayCommit {
                sha: "ec92a93".into(),
                subject: "agent commit on the virtual head".into(),
            }],
        });
        app.mode = Mode::Blocked;
        app
    }

    /// The whole point of the mode. Every key that would navigate, mutate or dismiss has to
    /// bounce, because the board underneath describes a repository that has moved on.
    #[test]
    fn blocked_swallows_navigation_and_mutation_keys() {
        use ratatui::crossterm::event::KeyCode as K;
        let mut app = blocked_app();
        app.col = 0;
        app.card = 0;

        for k in [
            K::Down,
            K::Char('j'),
            K::Right,
            K::Char('m'),
            K::Char('c'),
            K::Char('b'),
            K::Char('d'),
            K::Char('p'),
            K::Char('z'),
            K::Char(' '),
            K::Enter,
            K::Char('?'),
        ] {
            app.on_key(key(k));
            assert_eq!(app.mode, Mode::Blocked, "{k:?} must not leave the blocked mode");
        }
        assert_eq!((app.col, app.card), (0, 0), "the cursor must not have moved");
        assert!(app.selected.is_empty(), "nothing can be selected while blocked");
        assert!(!app.should_quit, "none of those keys mean quit");
    }

    /// Esc means "cancel, go back" everywhere else, but there is nothing to go back to
    /// here — so it leaves, and it leaves without touching the repository.
    #[test]
    fn blocked_quit_keys_exit_without_recovering() {
        use ratatui::crossterm::event::KeyCode as K;
        for k in [K::Char('q'), K::Esc] {
            let mut app = blocked_app();
            app.on_key(key(k));
            assert!(app.should_quit, "{k:?} quits");
            assert!(app.exit_note.is_none(), "{k:?} changed nothing, so it reports nothing");
        }
    }

    /// Neither recovery may fire on its own: both rewrite history, and a modal that acted
    /// before it was read would be a worse failure than the one it exists to report.
    #[test]
    fn blocked_never_recovers_without_a_keypress() {
        let app = blocked_app();
        assert_eq!(app.mode, Mode::Blocked);
        assert!(!app.should_quit);
        assert!(app.exit_note.is_none());
    }

    /// Puts an app into the not-set-up state without needing a real, unconfigured
    /// repository behind it.
    fn setup_required_app() -> App {
        let mut app = App::from_board(board());
        app.mode = Mode::SetupRequired;
        app
    }

    /// Same point as `blocked_swallows_navigation_and_mutation_keys`: there is no board
    /// here worth navigating or acting on yet.
    #[test]
    fn setup_required_swallows_navigation_and_mutation_keys() {
        use ratatui::crossterm::event::KeyCode as K;
        let mut app = setup_required_app();
        app.col = 0;
        app.card = 0;

        for k in [
            K::Down,
            K::Char('j'),
            K::Right,
            K::Char('m'),
            K::Char('c'),
            K::Char('b'),
            K::Char('d'),
            K::Char('p'),
            K::Char('z'),
            K::Char(' '),
            K::Enter,
            K::Char('?'),
        ] {
            app.on_key(key(k));
            assert_eq!(app.mode, Mode::SetupRequired, "{k:?} must not leave the mode");
        }
        assert_eq!((app.col, app.card), (0, 0), "the cursor must not have moved");
        assert!(app.selected.is_empty(), "nothing can be selected here");
        assert!(!app.should_quit, "none of those keys mean quit");
    }

    /// Esc means "cancel, go back" everywhere else, but there is nothing to go back to
    /// here — so it leaves, and it leaves without touching the repository.
    #[test]
    fn setup_required_quit_keys_exit_without_recovering() {
        use ratatui::crossterm::event::KeyCode as K;
        for k in [K::Char('q'), K::Esc] {
            let mut app = setup_required_app();
            app.on_key(key(k));
            assert!(app.should_quit, "{k:?} quits");
            assert!(app.exit_note.is_none(), "{k:?} changed nothing, so it reports nothing");
        }
    }

    /// The recovery may not fire on its own: it shells out to `but setup`, and a modal that
    /// acted before it was read would be a worse surprise than the one it exists to report.
    #[test]
    fn setup_required_never_recovers_without_a_keypress() {
        let app = setup_required_app();
        assert_eq!(app.mode, Mode::SetupRequired);
        assert!(!app.should_quit);
        assert!(app.exit_note.is_none());
    }

    #[test]
    fn space_toggles_a_cards_selection() {
        use ratatui::crossterm::event::KeyCode as K;
        let mut app = App::from_board(board());
        app.col = 0; // unassigned, two files
        app.card = 0;
        let id = app.board.columns[0].cards[0].rub_id.clone();

        app.on_key(key(K::Char(' ')));
        assert!(app.selected.contains(&id), "space selects the card under the cursor");

        app.on_key(key(K::Char(' ')));
        assert!(!app.selected.contains(&id), "space again deselects it");
    }

    /// `m` with a non-empty selection must pick up the *selection*, not just the card the
    /// cursor happens to be sitting on — the whole point of `space`-ing several cards
    /// first is that the cursor doesn't need to still be on one of them.
    #[test]
    fn m_with_a_selection_picks_up_all_of_it_not_just_the_cursor() {
        use ratatui::crossterm::event::KeyCode as K;
        let mut app = App::from_board(board());
        app.col = 0;
        app.card = 0;
        let first_id = app.board.columns[0].cards[0].rub_id.clone();
        let second_id = app.board.columns[0].cards[1].rub_id.clone();

        app.on_key(key(K::Char(' '))); // select card 0
        app.on_key(key(K::Down)); // cursor moves off it, onto card 1
        app.on_key(key(K::Char('m')));

        assert_eq!(app.mode, Mode::Moving);
        assert_eq!(app.selected.len(), 1, "only the spaced card, not the one under the cursor too");
        assert!(app.selected.contains(&first_id));
        assert!(!app.selected.contains(&second_id));
        assert!(
            app.pending_action().unwrap().starts_with("move 1 selected"),
            "bulk phrasing, not a single-card verb: {:?}",
            app.pending_action()
        );
    }

    /// Esc means two different things depending on what's pending: cancel the move but
    /// keep the selection (so a bulk move can be retried at a different target), or —
    /// with nothing else to cancel — clear the selection itself.
    #[test]
    fn esc_clears_a_selection_only_when_theres_nothing_else_to_cancel() {
        use ratatui::crossterm::event::KeyCode as K;
        let mut app = App::from_board(board());
        app.col = 0;
        app.card = 0;
        app.on_key(key(K::Char(' ')));
        assert_eq!(app.selected.len(), 1);

        app.on_key(key(K::Char('m')));
        assert_eq!(app.mode, Mode::Moving);
        app.on_key(key(K::Esc));
        assert_eq!(app.mode, Mode::Normal, "cancelled the move");
        assert_eq!(app.selected.len(), 1, "but the selection survives, for a retry");

        app.on_key(key(K::Esc));
        assert!(app.selected.is_empty(), "esc with nothing else pending clears it");
    }

    /// GitHub issue #2, "crash on adding a branch on the unassigned stack": the actual
    /// cause turned out to be `Esc` quitting outright from Normal mode with no
    /// confirmation — reflexively pressed to "back out" of typing a branch name (or
    /// anything else), it looked exactly like a crash: the app just vanished. Esc means
    /// "cancel, go back" in every other mode; at the top level, with nothing left to
    /// cancel back to, it must do nothing. `q` is the one documented, deliberate quit key.
    #[test]
    fn esc_does_not_quit_from_normal_mode_only_q_does() {
        use ratatui::crossterm::event::KeyCode as K;

        let mut app = App::from_board(board());
        assert_eq!(app.mode, Mode::Normal);

        app.on_key(key(K::Esc));
        assert!(!app.should_quit, "Esc at the top level must not quit");
        assert_eq!(app.mode, Mode::Normal);

        app.on_key(key(K::Char('q')));
        assert!(app.should_quit, "q is still the documented quit key");
    }

    /// Esc must still cancel out of the states that press it as "go back" — this pins
    /// those against ever regressing into the old "everything else quits" fallback too.
    #[test]
    fn esc_still_cancels_moving_and_restacking() {
        use ratatui::crossterm::event::KeyCode as K;

        let mut app = App::from_board(board());
        app.mode = Mode::Moving;
        app.on_key(key(K::Esc));
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Normal);

        app.mode = Mode::Restacking;
        app.on_key(key(K::Esc));
        assert!(!app.should_quit);
        assert_eq!(app.mode, Mode::Normal);
    }

    fn mouse(
        kind: ratatui::crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> ratatui::crossterm::event::MouseEvent {
        ratatui::crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: ratatui::crossterm::event::KeyModifiers::empty(),
        }
    }

    /// Hand-built rather than produced by `ui::draw`, so these tests pin `on_mouse`'s
    /// interpretation of a hit map against known coordinates without depending on the
    /// board's actual layout.
    fn hits(regions: &[(Rect, HitTarget)]) -> HitMap {
        let mut map = HitMap::new();
        for (rect, target) in regions {
            map.push(*rect, *target);
        }
        map
    }

    fn rect(x: u16, y: u16) -> Rect {
        Rect { x, y, width: 4, height: 1 }
    }

    #[test]
    fn clicking_a_card_selects_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Card(1, 0))]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert_eq!((app.col, app.card), (1, 0));
    }

    #[test]
    fn clicking_a_lane_header_selects_the_lane_and_clamps_the_card_cursor() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.col = 1;
        app.card = 1; // valid in feat-auth (2 cards), out of range in fix-flaky-tests (1)
        app.hit_map = hits(&[(rect(0, 0), HitTarget::LaneHeader(3))]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert_eq!(app.col, 3);
        assert_eq!(app.card, 0, "the click must clamp, not point past the lane's last card");
    }

    /// The mouse equivalent of `m` (`begin_move`) followed by dropping on the target lane:
    /// press on a card, drag onto a different lane's header, release. `App::from_board`
    /// leaves `but` unset, so the drop resolves through the same "snapshot is read-only"
    /// path a real single-card move takes once `but` itself is missing — which is enough
    /// to prove the release actually reached `confirm_move`, without needing a real CLI.
    #[test]
    fn dragging_a_card_onto_another_lane_enters_moving_mode_and_confirms_on_release() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.col = 0;
        app.card = 0; // a working-tree file in the unassigned/backlog lane

        let card_rect = rect(0, 0);
        let header_rect = rect(20, 0);
        app.hit_map = hits(&[
            (card_rect, HitTarget::Card(0, 0)),
            (header_rect, HitTarget::LaneHeader(1)),
        ]);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        app.on_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 21, 0));
        assert_eq!(app.mode, Mode::Moving, "dragging onto a different lane must pick the card up");
        assert_eq!((app.origin_col, app.origin_card), (0, 0));
        assert_eq!(app.col, 1, "the drop cursor follows the pointer, not just the drag start");
        assert_eq!(app.target_card, None, "dropped on the header, not on one of its cards");

        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 21, 0));
        assert_eq!(app.mode, Mode::Normal, "releasing must resolve the move, not leave it pending");
    }

    /// A plain click — no drag in between — must not pick anything up; that would turn
    /// ordinary selection into an accidental move the moment the pointer twitches.
    #[test]
    fn clicking_a_card_without_dragging_never_enters_moving_mode() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Card(0, 0))]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        app.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0));

        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn scrolling_moves_the_card_cursor_like_the_arrow_keys() {
        use ratatui::crossterm::event::MouseEventKind;

        let mut app = App::from_board(board());
        app.col = 0;
        app.card = 0;

        app.on_mouse(mouse(MouseEventKind::ScrollDown, 0, 0));
        assert_eq!(app.card, 1, "scroll down must be Down, not left unhandled");

        app.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(app.card, 0);
    }

    /// Clicking either half of a confirm dialog's hint line must take exactly the path its
    /// keyboard equivalent (`Esc`/`Enter`) would — reusing `handle_key` rather than a
    /// second, parallel dispatch that could drift from it.
    #[test]
    fn clicking_a_confirm_dialogs_buttons_takes_the_same_paths_as_its_keys() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.col = 1; // feat-auth: a real branch, so `pending_delete` is `Some`
        let confirm_rect = rect(0, 0);
        let cancel_rect = rect(20, 0);
        app.hit_map = hits(&[
            (confirm_rect, HitTarget::DialogConfirm),
            (cancel_rect, HitTarget::DialogCancel),
        ]);

        app.begin_delete();
        assert_eq!(app.mode, Mode::DeleteConfirm);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 21, 0));
        assert_eq!(app.mode, Mode::Normal, "the cancel half must act like Esc");
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("cancelled")));

        app.begin_delete();
        assert_eq!(app.mode, Mode::DeleteConfirm);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert_eq!(app.mode, Mode::Normal, "the confirm half must act like Enter");
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")));
    }

    /// Same wiring, for `draw_branch_modal` (`ui::draw`'s `KANSTACK_BRANCH_UI=modal` dialog):
    /// its hint line gets the same `confirm_hitboxes`/`Dialog{Confirm,Cancel}` treatment as
    /// every other confirm dialog, so `Mode::Branch`'s own `Enter`/`Esc` handling — already
    /// exercised by keyboard tests — is all a click needs to reach.
    #[test]
    fn clicking_the_branch_modals_buttons_takes_the_same_paths_as_its_keys() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        let confirm_rect = rect(0, 0);
        let cancel_rect = rect(20, 0);
        app.hit_map = hits(&[
            (confirm_rect, HitTarget::DialogConfirm),
            (cancel_rect, HitTarget::DialogCancel),
        ]);

        app.mode = Mode::Branch;
        for c in "feature".chars() {
            app.branch_input.insert(c);
        }
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 21, 0));
        assert_eq!(app.mode, Mode::Normal, "the cancel half must act like Esc");
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("cancelled")));

        app.mode = Mode::Branch;
        for c in "feature".chars() {
            app.branch_input.insert(c);
        }
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        // `App::from_board` has no split backend, so this never detours through
        // `HarnessMessage` — it goes straight to `create_branch`, which (with no `but`
        // either) is read-only.
        assert_eq!(app.mode, Mode::Normal, "the confirm half must act like Enter");
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")));
    }

    /// Unlike the confirm/cancel halves above, the modal's split row is a real checkbox,
    /// so its click target synthesizes shift-tab (`toggle_open_harness`'s own key) rather
    /// than Enter/Esc — same "reach the existing key handling" wiring, different key.
    #[test]
    fn clicking_the_branch_modals_split_row_acts_like_shift_tab() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Branch;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::BranchToggleSplit)]);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        // `App::from_board` has no split backend, so `toggle_open_harness` takes its
        // not-configured branch — same as pressing shift-tab directly would.
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("no harness-split backend found")));
    }

    /// `Down` in the modal moves the row cursor one row at a time — name, then action —
    /// rather than jumping straight past it to the message step.
    #[test]
    fn down_moves_the_row_cursor_from_name_to_action_before_anything_else() {
        let mut app = App::from_board(board());
        app.branch_ui = BranchUi::Modal;
        app.mode = Mode::Branch;
        for c in "feature".chars() {
            app.branch_input.insert(c);
        }

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Down));

        assert_eq!(app.mode, Mode::Branch, "still naming — no row to reach past action yet");
        assert_eq!(app.branch_modal_row, BranchModalRow::Action);
        assert_eq!(app.branch_input.as_str(), "feature", "Down must not touch the input");
    }

    /// `App::from_board` has no split backend, so past the action row there's no split row
    /// and no message step — a second `Down` must be a quiet no-op, not something that
    /// silently wraps back to the top or drops into a mode with nothing to show for it.
    #[test]
    fn down_from_the_action_row_is_a_no_op_with_nothing_further_to_reach() {
        let mut app = App::from_board(board());
        app.branch_ui = BranchUi::Modal;
        app.mode = Mode::Branch;
        app.branch_modal_row = BranchModalRow::Action;

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Down));

        assert_eq!(app.mode, Mode::Branch, "there's nowhere further for Down to go");
        assert_eq!(app.branch_modal_row, BranchModalRow::Action, "the row cursor must stay put");
    }

    /// `Up` is the exact reverse of `Down` through the same rows.
    #[test]
    fn up_moves_the_row_cursor_from_action_back_to_name() {
        let mut app = App::from_board(board());
        app.branch_ui = BranchUi::Modal;
        app.mode = Mode::Branch;
        app.branch_modal_row = BranchModalRow::Action;

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Up));

        assert_eq!(app.branch_modal_row, BranchModalRow::Name);
    }

    /// Once the row cursor has moved off the name field, `Left`/`Right` must act on
    /// whichever row it's actually on — here, flipping the stack/parallel choice exactly
    /// as `Tab` already does — rather than moving a text cursor the user can no longer see.
    #[test]
    fn left_right_on_the_action_row_toggles_the_stack_choice_not_the_text_cursor() {
        let mut app = App::from_board(board());
        app.branch_ui = BranchUi::Modal;
        app.mode = Mode::Branch;
        app.branch_modal_row = BranchModalRow::Action;
        app.stack_onto = Some("main".to_string());
        for c in "feature".chars() {
            app.branch_input.insert(c);
        }

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Left));
        assert_eq!(app.stack_onto, None, "Left toggled the action row");
        app.handle_key(key(ratatui::crossterm::event::KeyCode::Right));
        assert_eq!(
            app.stack_onto, None,
            "the backlog (col 0) has no branch to stack on, so this toggle fails — but it must \
             still have gone to the action row, not the text field"
        );
        assert_eq!(app.branch_input.as_str(), "feature", "the name field itself is untouched");
    }

    /// Typing while focused on a row other than the name field must not silently corrupt
    /// the name the user can no longer see they'd be editing.
    #[test]
    fn typing_while_focused_on_the_action_row_does_not_touch_the_branch_name() {
        let mut app = App::from_board(board());
        app.branch_ui = BranchUi::Modal;
        app.mode = Mode::Branch;
        app.branch_modal_row = BranchModalRow::Action;
        for c in "feature".chars() {
            app.branch_input.insert(c);
        }

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Char('x')));
        app.handle_key(key(ratatui::crossterm::event::KeyCode::Backspace));

        assert_eq!(app.branch_input.as_str(), "feature");
    }

    /// The footer presentation (`KANSTACK_BRANCH_UI=footer`) has no rows to show a cursor
    /// moving between, so it keeps the pre-row-navigation behavior: `Left`/`Right` always
    /// edit the text field, `Down` always goes straight to the message step, regardless of
    /// `branch_modal_row` (which the footer never renders and so never sets deliberately).
    #[test]
    fn footer_mode_keeps_left_right_on_the_text_field_regardless_of_row_state() {
        let mut app = App::from_board(board());
        app.branch_ui = BranchUi::Footer;
        app.mode = Mode::Branch;
        app.branch_modal_row = BranchModalRow::Action;
        for c in "feature".chars() {
            app.branch_input.insert(c);
        }

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Left));
        app.handle_key(key(ratatui::crossterm::event::KeyCode::Char('!')));

        assert_eq!(app.branch_input.as_str(), "featur!e", "Left moved the text cursor, not a row");
    }

    /// `Up` from the harness-message step is the non-cancelling way back to the name
    /// field — the name and stack target it restores must be exactly what
    /// `advance_to_harness_message` handed off, and nothing must be created in the
    /// process (unlike `Esc`, which drops the whole branch).
    #[test]
    fn up_from_the_harness_message_step_restores_the_branch_name_without_creating_anything() {
        let mut app = App::from_board(board());
        app.mode = Mode::HarnessMessage;
        app.pending_branch =
            Some(PendingBranch { name: "feature".to_string(), anchor: Some("main".to_string()) });
        for c in "an initial message".chars() {
            app.harness_message_input.insert(c);
        }

        app.handle_key(key(ratatui::crossterm::event::KeyCode::Up));

        assert_eq!(app.mode, Mode::Branch, "Up goes back to the name field");
        assert_eq!(app.branch_input.as_str(), "feature", "the name must come back exactly");
        assert_eq!(app.stack_onto.as_deref(), Some("main"), "the stack target must come back too");
        assert_eq!(
            app.harness_message_input.as_str(),
            "an initial message",
            "moving between fields must not throw away what was typed"
        );
        assert!(app.pending_branch.is_none(), "nothing should still be pending — it's back on the name field");
        assert!(app.message.is_none(), "going back is not a cancel — no notice should fire");
        assert_eq!(
            app.branch_modal_row,
            BranchModalRow::Action,
            "no split row to land on without a split backend configured, so the row before it"
        );
    }

    /// `Ctrl-U` clears the focused field only — and must not type a `u`.
    #[test]
    fn ctrl_u_clears_only_the_focused_field_in_the_pr_modal() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::from_board(board());
        app.mode = Mode::PrModal;
        app.pr_modal_row = PrModalRow::Message;
        app.pr_title_input.set("title");
        app.pr_message_input.set("body");

        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));

        assert_eq!(app.pr_message_input.as_str(), "");
        assert_eq!(app.pr_title_input.as_str(), "title");
    }

    /// The unassigned lane holds loose files, not commits — clicking through several of
    /// them to build up a bulk move is the point of a mouse there, so a click doubles as
    /// `space` only in that one lane.
    #[test]
    fn clicking_a_card_in_the_unassigned_lane_toggles_its_bulk_selection_checkbox() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        let id = app.board.columns[0].cards[0].rub_id.clone();
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Card(0, 0))]);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert!(app.selected.contains(&id), "the click must check the card, like space would");
    }

    /// A branch lane holds commits, not loose files — clicking through them must not build
    /// up a bulk-move selection the way it does in the unassigned lane.
    #[test]
    fn clicking_a_card_outside_the_unassigned_lane_does_not_touch_bulk_selection() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Card(1, 0))]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert!(app.selected.is_empty());
    }

    /// Two rapid clicks on the same card open its diff, the same as pressing `Enter` —
    /// checked here by the fact that `open_diff` takes the "snapshot is read-only" path
    /// with no real `but` behind this app, which only happens if it actually ran. A single
    /// click must not take that path at all.
    #[test]
    fn double_clicking_a_card_opens_its_diff() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.col = 1;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Card(1, 0))]);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert!(
            app.message.is_none(),
            "a single click must not have tried to open anything yet"
        );

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        assert!(
            app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")),
            "the second click must have gone through open_diff: {:?}",
            app.message
        );
    }

    /// A click on a different card, even a fast one, is two ordinary selects — not a
    /// double-click on either card.
    #[test]
    fn clicking_two_different_cards_quickly_is_not_a_double_click() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.col = 1;
        app.hit_map = hits(&[
            (rect(0, 0), HitTarget::Card(1, 0)),
            (rect(20, 0), HitTarget::Card(1, 1)),
        ]);

        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 21, 0));

        assert!(app.message.is_none(), "neither click was a repeat, so neither opens a diff");
        assert_eq!((app.col, app.card), (1, 1));
    }

    #[test]
    fn moving_the_mouse_updates_hover_and_leaving_every_region_clears_it() {
        use ratatui::crossterm::event::MouseEventKind;

        let mut app = App::from_board(board());
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Card(2, 0))]);

        app.on_mouse(mouse(MouseEventKind::Moved, 1, 0));
        assert_eq!(app.hover, Some(HitTarget::Card(2, 0)));

        app.on_mouse(mouse(MouseEventKind::Moved, 50, 50));
        assert_eq!(app.hover, None, "moving off every hit region must clear the hover");
    }

    /// `a` already opens the drawer from the keyboard; before this there was nothing to
    /// click for it anywhere in the main board view — only its own back control, once
    /// already inside. `‹` on the unassigned lane's own dot (see `draw_column`) is that
    /// door for a mouse.
    #[test]
    fn clicking_the_unassigned_lanes_open_drawer_control_opens_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.hit_map = hits(&[(rect(0, 0), HitTarget::OpenBranches)]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        // `App::from_board` has no `but` behind it, so `toggle_branch_drawer` can't
        // actually fetch the list and switch modes — reaching this same "read-only"
        // message is what proves the click took the `a` path at all, rather than a no-op.
        assert!(
            app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")),
            "the click must have gone through toggle_branch_drawer: {:?}",
            app.message
        );
    }

    /// The control only exists on the unassigned lane's dot while `mode == Normal` (see
    /// `draw_column`), but the guard belongs to `on_mouse` too — a stale hit shouldn't
    /// reopen the drawer, or anything else, out from under an unrelated mode.
    #[test]
    fn the_open_drawer_control_is_inert_outside_normal_mode() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Help;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::OpenBranches)]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert_eq!(app.mode, Mode::Help);
        assert!(app.message.is_none());
    }

    /// `ui::draw` gives the branches drawer the same `‹` back control as the diff pane,
    /// mapped to the same `Dismiss` target — so it closes the drawer without a click on a
    /// row (selecting it) being swallowed by an anywhere-closes rule.
    #[test]
    fn clicking_the_drawers_back_control_closes_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Branches;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Dismiss)]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert_eq!(app.mode, Mode::Normal);
    }

    /// Clicking a row in the drawer selects it, the same as any other row click — it must
    /// not also close the drawer out from under the click.
    #[test]
    fn clicking_a_branch_row_leaves_the_drawer_open() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Branches;
        let branch = |name: &str| crate::board::UnappliedBranch {
            name: name.into(),
            commits_ahead: None,
            merges_cleanly: None,
            author: None,
            age: None,
            has_local: true,
            stale: false,
        };
        app.unapplied = crate::board::Unapplied {
            branches: vec![branch("a"), branch("b")],
            truncated: false,
        };
        app.hit_map = hits(&[(rect(0, 5), HitTarget::BranchRow(1))]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 5));

        assert_eq!(app.mode, Mode::Branches);
        assert_eq!(app.branch_sel, 1);
    }

    /// `d` in the drawer confirms deleting the selected unapplied branch rather than the
    /// current lane — and, since you came from the drawer, both confirming and cancelling
    /// return you to it instead of dropping you on the board.
    #[test]
    fn d_in_the_drawer_targets_the_selected_branch_and_returns_to_it() {
        use ratatui::crossterm::event::KeyCode as K;

        let mut app = App::from_board(board());
        app.mode = Mode::Branches;
        let branch = |name: &str| crate::board::UnappliedBranch {
            name: name.into(),
            commits_ahead: Some(3),
            merges_cleanly: None,
            author: None,
            age: None,
            has_local: true,
            stale: false,
        };
        app.unapplied = crate::board::Unapplied {
            branches: vec![branch("a"), branch("stale-branch")],
            truncated: false,
        };
        app.branch_sel = 1;

        app.handle_key(key(K::Char('d')));
        assert_eq!(app.mode, Mode::DeleteConfirm);
        assert!(app.deleting_unapplied());
        let (name, _) = app.pending_delete().expect("a branch is selected");
        assert_eq!(name, "stale-branch");

        app.handle_key(key(K::Esc));
        assert_eq!(
            app.mode,
            Mode::Branches,
            "cancelling a drawer-initiated delete should return to the drawer, not the board"
        );

        app.handle_key(key(K::Char('d')));
        app.handle_key(key(K::Enter));
        assert_eq!(
            app.mode,
            Mode::Branches,
            "confirming a drawer-initiated delete should return to the drawer, not the board"
        );
    }

    fn fake_check(clean: bool) -> crate::model::MergeCheck {
        crate::model::MergeCheck {
            commits_ahead: 1,
            commits: vec![crate::model::MergeCheckCommit {
                short_sha: "abc1234".into(),
                message: "a commit".into(),
                insertions: None,
                deletions: None,
            }],
            merge_check: crate::model::MergeCheckResult {
                merges_cleanly: clean,
                conflicting_files: if clean {
                    Vec::new()
                } else {
                    vec![crate::model::ConflictingFile {
                        path: "src/app.rs".into(),
                        branch_commits: Vec::new(),
                        upstream_commits: Vec::new(),
                    }]
                },
            },
        }
    }

    /// `⏎` in the drawer's list opens the detail view rather than applying — applying is
    /// `a`'s job now, and it has to work whether or not a preview happens to be open, the
    /// same way `m` still moves a hunk while the diff pane is open.
    #[test]
    fn enter_previews_instead_of_applying_and_a_applies_from_either_view() {
        use ratatui::crossterm::event::KeyCode as K;

        let mut app = App::from_board(board());
        app.mode = Mode::Branches;
        app.unapplied = crate::board::Unapplied {
            branches: vec![crate::board::UnappliedBranch {
                name: "feat-x".into(),
                commits_ahead: Some(1),
                merges_cleanly: Some(false),
                author: None,
                age: None,
                has_local: true,
                stale: false,
            }],
            truncated: false,
        };

        // No live `but` (snapshot mode), so the fetch behind `⏎` fails — but it must have
        // tried to preview, not applied outright.
        app.handle_key(key(K::Enter));
        assert_eq!(app.mode, Mode::Branches, "still in the drawer, not applied");
        assert!(app.branch_preview.is_none(), "the fetch has no `but` to run against");
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")));

        // Fake a successful preview directly, bypassing the fetch, to exercise the detail
        // view's own key handling.
        app.branch_preview = Some(crate::app::BranchPreview {
            name: "feat-x".into(),
            check: fake_check(false),
            scroll: 0,
            stale: false,
        });

        app.handle_key(key(K::Esc));
        assert_eq!(
            app.mode,
            Mode::Branches,
            "esc from the detail view goes back to the list, not out of the drawer"
        );
        assert!(app.branch_preview.is_none());

        // Re-open it and apply from inside it — `a` must work here too, not just from the
        // list, and (since `but` is absent) at least attempts the apply rather than no-op.
        app.branch_preview = Some(crate::app::BranchPreview {
            name: "feat-x".into(),
            check: fake_check(false),
            scroll: 0,
            stale: false,
        });
        app.handle_key(key(K::Char('a')));
        assert!(
            app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")),
            "a from the detail view should have attempted to apply"
        );
    }

    /// `←` backs out of the drawer one level at a time too, the same as `esc` — mirroring
    /// `Mode::Diff`, where the diff similarly sits off to one side of the board and either
    /// arrow already means "back" rather than "move within this view".
    #[test]
    fn left_arrow_backs_out_of_the_drawer_like_esc() {
        use ratatui::crossterm::event::KeyCode as K;

        let mut app = App::from_board(board());
        app.mode = Mode::Branches;
        app.branch_preview = Some(crate::app::BranchPreview {
            name: "feat-x".into(),
            check: fake_check(true),
            scroll: 0,
            stale: false,
        });

        app.handle_key(key(K::Left));
        assert_eq!(
            app.mode,
            Mode::Branches,
            "left from the detail view goes back to the list, not out of the drawer"
        );
        assert!(app.branch_preview.is_none(), "left should have closed the preview");

        app.handle_key(key(K::Left));
        assert_eq!(app.mode, Mode::Normal, "left from the list closes the drawer");
    }

    /// `A` only does anything when the list is actually truncated — otherwise it's a
    /// no-op, not a redundant re-fetch of what's already fully shown.
    #[test]
    fn a_shift_loads_all_branches_only_when_truncated() {
        use ratatui::crossterm::event::KeyCode as K;

        let mut app = App::from_board(board());
        app.mode = Mode::Branches;
        app.unapplied.truncated = false;

        app.handle_key(key(K::Char('A')));
        assert!(
            app.message.is_none(),
            "A should be a no-op when the list isn't truncated: {:?}",
            app.message
        );

        app.unapplied.truncated = true;
        app.handle_key(key(K::Char('A')));
        assert!(
            app.message.as_ref().is_some_and(|(m, _)| m.contains("read-only")),
            "truncated should have attempted the --all re-fetch (and hit the no-`but` guard \
             in this snapshot-mode app): {:?}",
            app.message
        );
    }

    #[test]
    fn clicking_anywhere_dismisses_help() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Help;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Dismiss)]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert_eq!(app.mode, Mode::Normal);
    }

    /// `ui::draw` puts a `Dismiss` region on the `‹` control in the diff pane's own header
    /// — not over the whole pane, so clicking to read (or just clicking around) never
    /// closes it out from under you the way an anywhere-click did at first.
    #[test]
    fn clicking_the_diffs_back_control_leaves_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Diff;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Dismiss)]);
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0));

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.diff.is_none());
    }

    /// The complement of the test above: reading the diff — clicking anywhere that isn't
    /// the back control — must leave it open.
    #[test]
    fn clicking_inside_the_diff_but_not_its_back_control_leaves_it_open() {
        use ratatui::crossterm::event::{MouseButton, MouseEventKind};

        let mut app = App::from_board(board());
        app.mode = Mode::Diff;
        app.hit_map = hits(&[(rect(0, 0), HitTarget::Dismiss)]); // the back control, at x0-3
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 30, 5));

        assert_eq!(app.mode, Mode::Diff, "a click well away from the back control must not close it");
    }
}
