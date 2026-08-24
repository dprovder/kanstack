//! Application state and input handling.
//!
//! Navigation is pure in-memory so arrow keys never wait on a subprocess. Only mutations
//! shell out, and `rub`/`commit`/`move` fold their refresh into the same call by embedding
//! a status in their reply, so a card move costs one round trip rather than a mutation plus
//! a reload.

use anyhow::Result;

use crate::board::{Board, Card, CardKind, ColumnKind, Unapplied};
use crate::but::But;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use crate::cmux::Cmux;
use crate::diff::DiffView;
use crate::model::{MergeCheck, PullPreview, PushPreview, WorkspaceStatus};
use crate::text_input::TextInput;
use crate::tutorial::Tutorial;

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

/// Wall-clock now, in epoch milliseconds, for rendering branch ages.
///
/// Saturates to 0 rather than panicking if the system clock is set before 1970 — a broken
/// clock should cost an age column, not the whole drawer.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    /// Typing a task description to send into the selected lane's cmux pane.
    Task,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Info,
    Success,
    Error,
}

/// A `but land` call running on a background thread, started by `confirm_land`.
///
/// Landing pushes directly to the target — sometimes a real remote — so it can take
/// long enough (and fail in ways worth explaining) that running it on the main thread
/// would freeze the whole UI with no feedback for however long the network takes.
pub struct PendingLand {
    pub title: String,
    /// How many branches this land call covers — 1 for an ordinary land, more on a
    /// stacked lane (see `confirm_land`). Lets the completion notification say "landed 3
    /// branches" instead of just "landed", which would undersell what actually happened.
    pub branch_count: usize,
    // `pub(crate)` rather than private: `ui`'s tests build one directly to exercise the
    // spinner overlay without a real `but` and thread.
    pub(crate) rx: mpsc::Receiver<Result<WorkspaceStatus>>,
    /// Advanced once per poll so the UI can animate a spinner without its own clock.
    pub spinner: usize,
}

pub struct App {
    /// `None` in snapshot mode, where a captured status is rendered read-only.
    /// `Arc` so `confirm_land` can hand a handle to the background thread it spawns.
    but: Option<Arc<But>>,
    /// `None` outside cmux, or when `cmux-tui` is not installed. See [`crate::cmux`].
    cmux: Option<Cmux>,
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
    /// While naming a branch: `Some(branch)` stacks the new one on top of it, `None` makes
    /// a parallel lane. Seeded from the selected lane and toggled with tab.
    pub stack_onto: Option<String>,
    /// While naming a branch: whether a parallel lane (`stack_onto.is_none()`) should get
    /// its own cmux split. Only meaningful when cmux is configured at all; irrelevant for a
    /// stacked branch, which never opens one regardless. Reset to `true` each time branch
    /// naming starts and toggled with shift-tab.
    pub open_harness: bool,
    /// What a push would do, valid while `mode == PushConfirm`.
    pub push_preview: Option<PushPreview>,
    /// What landing the selected lane onto the target would do, valid while
    /// `mode == LandConfirm`.
    pub land_check: Option<MergeCheck>,
    /// The `but land` call in flight, valid while `mode == Landing`.
    pub landing: Option<PendingLand>,
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
    /// A cmux pane-liveness poll running on a background thread; see `maybe_begin_cmux_poll`.
    cmux_poll: Option<PendingCmuxPoll>,
    /// Wall-clock time of the last poll kickoff, so most ticks cost one comparison and
    /// nothing else. `None` until the first poll ever starts.
    cmux_poll_at: Option<std::time::Instant>,
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
}

type RefreshResult = Result<(Board, HashMap<String, (usize, usize)>)>;

/// A watcher-triggered refresh in flight on a background thread. Unlike [`PendingLand`]
/// this has no UI of its own — it's invisible on success, the same as `refresh_quietly`
/// always was, just no longer blocking while it runs.
struct PendingRefresh {
    rx: mpsc::Receiver<RefreshResult>,
    /// The board generation at the moment this refresh started; see `board_generation`.
    generation: u64,
}

/// A cmux pane-liveness poll in flight on a background thread; see
/// `App::maybe_begin_cmux_poll`.
struct PendingCmuxPoll {
    rx: mpsc::Receiver<Result<HashMap<String, crate::cmux::PaneStatus>>>,
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
    pub fn new(but: But, cmux: Option<Cmux>) -> Result<Self> {
        let mut commit_stats = HashMap::new();
        // A blocked workspace starts the app rather than aborting it. Bailing here printed
        // one line of prose and vanished, which reads as a crash and leaves the user to
        // work out the recovery themselves — the modal can explain and offer to run it.
        let (board, blocked) = match but.status() {
            Ok(status) => (Self::board_from(&but, &mut commit_stats, &status), None),
            Err(e) if crate::but::is_workspace_block(&e.to_string()) => (
                Board::empty(),
                Some(but.diagnose_workspace_block(e.to_string())),
            ),
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
            cmux,
            board,
            col: 0,
            card: 0,
            mode: if blocked.is_some() {
                Mode::Blocked
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
            stack_onto: None,
            open_harness: true,
            push_preview: None,
            land_check: None,
            landing: None,
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
            cmux_poll: None,
            cmux_poll_at: None,
            unapplied: Unapplied::default(),
            branch_sel: 0,
        })
    }

    /// Read-only view over a captured status. Used by `--snapshot` and by tests, so the
    /// exact rendering can be reproduced without anyone's repository.
    pub fn from_board(board: Board) -> Self {
        App {
            but: None,
            cmux: None,
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
            stack_onto: None,
            open_harness: true,
            push_preview: None,
            land_check: None,
            landing: None,
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
            cmux_poll: None,
            cmux_poll_at: None,
            unapplied: Unapplied::default(),
            branch_sel: 0,
        }
    }

    /// Whether cmux was found at startup, i.e. whether shift-tab in branch mode does
    /// anything. See [`crate::cmux`].
    pub fn cmux_available(&self) -> bool {
        self.cmux.is_some()
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

    /// Copies each tracked cmux pane's last known status onto the matching column/section,
    /// so `Board::build` never needs to know cmux exists. Called from `clamp` after every
    /// board rebuild, and again on its own after a background poll resolves (`poll_cmux`),
    /// since a poll updates `self.cmux`'s cache without rebuilding the board at all.
    fn sync_pane_statuses(&mut self) {
        let Some(cmux) = &self.cmux else { return };
        for col in &mut self.board.columns {
            for section in &mut col.sections {
                section.pane_status = cmux.pane_status(&section.name);
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

    /// How often `maybe_begin_cmux_poll` is willing to start a new poll.
    const CMUX_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

    /// Kicks off a background cmux liveness poll if one isn't already in flight, enough
    /// time has passed since the last one, and there's at least one pane worth asking
    /// about. Gated on `CMUX_SURFACE_ID` being present (kanstack itself running inside a
    /// cmux pane) — the same guard `Cmux::occupant_in_direction` already uses, rather than
    /// querying an ambiguous default scope when it isn't. Call this every tick, the same
    /// as `begin_background_refresh`.
    pub fn maybe_begin_cmux_poll(&mut self) {
        let Some(cmux) = &self.cmux else { return };
        if cmux.is_empty() || self.cmux_poll.is_some() {
            return;
        }
        if std::env::var_os("CMUX_SURFACE_ID").is_none() {
            return;
        }
        if self
            .cmux_poll_at
            .is_some_and(|t| t.elapsed() < Self::CMUX_POLL_INTERVAL)
        {
            return;
        }
        self.cmux_poll_at = Some(std::time::Instant::now());
        let snapshot = cmux.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(snapshot.poll_statuses());
        });
        self.cmux_poll = Some(PendingCmuxPoll { rx });
    }

    /// Non-blocking check on a poll started by `maybe_begin_cmux_poll`. Call every tick,
    /// the same as `poll_background_refresh`.
    pub fn poll_cmux(&mut self) {
        let Some(pending) = &self.cmux_poll else { return };
        match pending.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => self.cmux_poll = None,
            Ok(Ok(statuses)) => {
                self.cmux_poll = None;
                if let Some(cmux) = &mut self.cmux {
                    cmux.apply_statuses(statuses);
                }
                self.sync_pane_statuses();
            }
            // Transient — a `cmux` hiccup every few seconds shouldn't spam the footer the
            // way a user-triggered action's failure should.
            Ok(Err(_)) => self.cmux_poll = None,
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

    /// What a move will rub, and how to name it. Normally the picked-up card; when a move
    /// began from the diff pane, the hunk chosen there.
    fn move_source_ref(&self) -> Option<(String, String)> {
        if let Some((id, label)) = &self.move_source {
            return Some((id.clone(), label.clone()));
        }
        let c = self.source_card()?;
        Some((c.rub_id.clone(), c.cli_id.clone()))
    }

    /// The card currently picked up, valid while moving.
    fn source_card(&self) -> Option<&Card> {
        self.board
            .columns
            .get(self.origin_col)?
            .cards
            .get(self.origin_card)
    }

    /// Resolves the drop target into the CLI id to act on, plus a label for the footer.
    ///
    /// Targeting the lane header names its branch (the `-b`/`-t` argument for a commit,
    /// amend, or move); targeting a card names that commit (the `-t` argument for an amend
    /// or squash).
    fn resolve_target(&self) -> Option<(String, String)> {
        let col = self.board.columns.get(self.col)?;
        match self.target_card {
            None => Some((col.drop_target.clone(), col.title.clone())),
            Some(i) => {
                let card = col.cards.get(i)?;
                Some((card.rub_id.clone(), card.title.clone()))
            }
        }
    }

    /// Describes what confirming the move will do. Shown before committing to it, because
    /// "commit onto unassigned" is an uncommit and "commit onto commit" is a squash, and
    /// neither should be a surprise.
    pub fn pending_action(&self) -> Option<String> {
        if self.mode != Mode::Moving {
            return None;
        }
        let (_, target_label) = self.resolve_target()?;
        // A bulk selection can mix commits and files, which take different verbs (amend
        // vs. move, say) — rather than pick one that would be wrong for half the
        // selection, this just says how many and where, same for a lane or a card target.
        if !self.selected.is_empty() {
            return Some(format!("move {} selected → {target_label}", self.selected.len()));
        }
        let (_, source_label) = self.move_source_ref()?;
        let op = self.pending_op()?;
        Some(format!("{} {source_label} → {target_label}", op.verb()))
    }

    /// `None` when the combination is not a supported move.
    fn pending_op(&self) -> Option<MoveOp> {
        // A hunk picked from the diff pane behaves like any other working-tree change.
        let source_kind = if self.move_source.is_some() {
            CardKind::Change
        } else {
            self.source_card()?.kind
        };
        let col = self.board.columns.get(self.col)?;
        match self.target_card {
            None => match (source_kind, col.kind) {
                // Nothing is ever assigned to a lane pre-commit anymore, so a Change card
                // only ever lives in the unassigned lane already — there is nothing to
                // un-assign it from.
                (CardKind::Change, ColumnKind::Unassigned) => None,
                (CardKind::Change, ColumnKind::Stack) => Some(
                    if col.state == Some(crate::board::LaneState::Empty) {
                        MoveOp::Commit
                    } else {
                        MoveOp::Amend
                    },
                ),
                (CardKind::Commit, ColumnKind::Unassigned) => Some(MoveOp::Uncommit),
                (CardKind::Commit, ColumnKind::Stack) => Some(MoveOp::MoveToLane),
            },
            // Dropping on a card: only commits are valid targets. The matrix has no
            // file-onto-file operation, so that combination is refused rather than guessed.
            Some(i) => match (source_kind, col.cards.get(i)?.kind) {
                (CardKind::Commit, CardKind::Commit) => Some(MoveOp::Squash),
                (CardKind::Change, CardKind::Commit) => Some(MoveOp::Amend),
                (_, CardKind::Change) => None,
            },
        }
    }

    /// Toggles the card under the cursor in/out of the bulk-move selection. A no-op with
    /// nothing under the cursor — an empty lane has no card to select.
    fn toggle_selected(&mut self) {
        let Some(card) = self.selected_card() else {
            return;
        };
        let id = card.rub_id.clone();
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
    }

    /// Picks up the selection (if non-empty) or just the card under the cursor. A
    /// non-empty selection always wins over the single card under the cursor — `space` a
    /// few cards from wherever, then `m` anywhere on the board, rather than needing the
    /// cursor to still be sitting on one of them.
    ///
    /// Cancelling (`esc`) deliberately leaves the selection alone, so a bulk move can be
    /// retried at a different target without re-selecting everything; `esc` from Normal
    /// mode with nothing else to cancel is what actually clears it.
    fn begin_move(&mut self) {
        if self.selected.is_empty() && self.selected_card().is_none() {
            self.notify("nothing to move here", Notice::Info);
            return;
        }
        self.origin_col = self.col;
        self.origin_card = self.card;
        self.target_card = None;
        self.mode = Mode::Moving;
    }

    /// Moves the drop cursor through a lane: the header, then each card in turn, wrapping
    /// back round to the header past the last card.
    fn move_target(&mut self, delta: isize) {
        let n = self.cards_in_current_column() as isize;
        // -1 represents the lane header, giving n + 1 positions to cycle through.
        let cur = self.target_card.map_or(-1, |i| i as isize);
        let next = (cur + 1 + delta).rem_euclid(n + 1) - 1;
        self.target_card = if next < 0 { None } else { Some(next as usize) };
    }

    fn confirm_move(&mut self) {
        if !self.selected.is_empty() {
            self.confirm_bulk_move();
            return;
        }

        let Some((source_id, source_label)) = self.move_source_ref() else {
            self.mode = Mode::Normal;
            self.move_source = None;
            return;
        };
        let from_hunk = self.move_source.is_some();

        // Dropping a card onto itself is a no-op, not an error worth calling `but` for.
        // A hunk has no "own lane" to land back in, so the check only applies to cards.
        if !from_hunk && self.col == self.origin_col && self.target_card == Some(self.origin_card) {
            self.mode = Mode::Normal;
            self.notify("cancelled — same card", Notice::Info);
            return;
        }
        if !from_hunk && self.col == self.origin_col && self.target_card.is_none() {
            self.mode = Mode::Normal;
            self.notify("cancelled — same lane", Notice::Info);
            return;
        }

        let Some(op) = self.pending_op() else {
            self.notify("that combination isn't a supported operation", Notice::Error);
            return;
        };
        let Some((target_id, target_label)) = self.resolve_target() else {
            self.mode = Mode::Normal;
            return;
        };

        // A fresh commit needs a message before it can run — park it and switch to typing
        // one, the same text-entry flow the old standalone commit key used.
        if op == MoveOp::Commit {
            self.pending_commit_move = Some(PendingCommitMove {
                changes: vec![source_id],
                branch: target_id,
                also_move: Vec::new(),
                source_label,
                target_label,
            });
            self.move_source = None;
            self.commit_input.clear();
            self.mode = Mode::Commit;
            return;
        }

        self.mode = Mode::Normal;
        self.move_source = None;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };

        let result = match op {
            MoveOp::Amend => but.amend(&[source_id], &target_id),
            MoveOp::MoveToLane => but.move_commits(&[source_id], &target_id),
            MoveOp::Squash => but.squash(&[source_id], &target_id),
            MoveOp::Uncommit => but.uncommit(&[source_id]),
            MoveOp::Commit => unreachable!("handled above"),
        };
        match result {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(
                    format!("{} {source_label} → {target_label}", op.verb()),
                    Notice::Success,
                );
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Moves the whole selection in one action — `space` a few cards, `m`, pick a target,
    /// `⏎`. Sources are grouped by kind (a selection can mix commits and uncommitted
    /// changes) so each group becomes one combined call rather than one call per source —
    /// `but commit`/`amend`/`squash`/`move`/`uncommit` all accept several sources at once.
    /// See `pending_action` for why this doesn't try to name one exact verb the way a
    /// single-card move does.
    fn confirm_bulk_move(&mut self) {
        let Some((target_id, target_label)) = self.resolve_target() else {
            self.mode = Mode::Normal;
            return;
        };
        let sources: Vec<String> = self.selected.drain().collect();
        let n = sources.len();

        let mut changes = Vec::new();
        let mut commits = Vec::new();
        for id in &sources {
            let kind = self
                .board
                .columns
                .iter()
                .flat_map(|c| &c.cards)
                .find(|c| &c.rub_id == id)
                .map(|c| c.kind);
            match kind {
                Some(CardKind::Change) => changes.push(id.clone()),
                Some(CardKind::Commit) => commits.push(id.clone()),
                // A stale id from before the last refresh — nothing to do with it.
                None => {}
            }
        }

        let Some(col) = self.board.columns.get(self.col) else {
            self.mode = Mode::Normal;
            return;
        };
        let target_is_card = self.target_card.is_some();
        let col_kind = col.kind;
        let lane_empty = col.state == Some(crate::board::LaneState::Empty);
        self.mode = Mode::Normal;

        // Handled before `but` is borrowed below: a batch with only changes dropped on the
        // backlog has nothing to call `but` for at all — there's no un-assign anymore.
        if !target_is_card && col_kind == ColumnKind::Unassigned {
            if !changes.is_empty() {
                self.notify(
                    "files aren't assigned to lanes to unassign anymore",
                    Notice::Info,
                );
            }
            if commits.is_empty() {
                return;
            }
        }

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };

        if target_is_card {
            // Dropped on a commit card: changes amend into it, commits squash into it.
            let result = match (changes.is_empty(), commits.is_empty()) {
                (true, true) => {
                    self.notify("nothing to move", Notice::Info);
                    return;
                }
                (false, true) => but.amend(&changes, &target_id),
                (true, false) => but.squash(&commits, &target_id),
                (false, false) => but
                    .amend(&changes, &target_id)
                    .and_then(|_| but.squash(&commits, &target_id)),
            };
            match result {
                Ok(status) => {
                    self.board = Self::board_from(but, &mut self.commit_stats, &status);
                    self.clamp();
                    self.notify(format!("moved {n} → {target_label}"), Notice::Success);
                }
                Err(e) => self.notify(format!("{e}"), Notice::Error),
            }
            return;
        }

        match col_kind {
            ColumnKind::Unassigned => match but.uncommit(&commits) {
                Ok(status) => {
                    self.board = Self::board_from(but, &mut self.commit_stats, &status);
                    self.clamp();
                    self.notify(
                        format!("uncommitted {} → {target_label}", commits.len()),
                        Notice::Success,
                    );
                }
                Err(e) => self.notify(format!("{e}"), Notice::Error),
            },
            ColumnKind::Stack if changes.is_empty() => match but.move_commits(&commits, &target_id) {
                Ok(status) => {
                    self.board = Self::board_from(but, &mut self.commit_stats, &status);
                    self.clamp();
                    self.notify(format!("moved {n} → {target_label}"), Notice::Success);
                }
                Err(e) => self.notify(format!("{e}"), Notice::Error),
            },
            // Changes onto an empty lane need a message before the first commit can be
            // made — park the whole batch (including any commit sources, applied right
            // after) and switch to typing one.
            ColumnKind::Stack if lane_empty => {
                self.pending_commit_move = Some(PendingCommitMove {
                    changes,
                    branch: target_id,
                    also_move: commits,
                    source_label: format!("{n} selected"),
                    target_label,
                });
                self.commit_input.clear();
                self.mode = Mode::Commit;
            }
            ColumnKind::Stack => {
                let result = but.amend(&changes, &target_id).and_then(|status| {
                    if commits.is_empty() {
                        Ok(status)
                    } else {
                        but.move_commits(&commits, &target_id)
                    }
                });
                match result {
                    Ok(status) => {
                        self.board = Self::board_from(but, &mut self.commit_stats, &status);
                        self.clamp();
                        self.notify(format!("moved {n} → {target_label}"), Notice::Success);
                    }
                    Err(e) => self.notify(format!("{e}"), Notice::Error),
                }
            }
        }
    }

    /// The branch behind the selected lane, which is what commit and push act on.
    /// `None` for the backlog lane, which has no branch of its own.
    fn selected_branch(&self) -> Option<(String, String)> {
        let col = self.board.columns.get(self.col)?;
        if col.kind != ColumnKind::Stack {
            return None;
        }
        Some((col.drop_target.clone(), col.title.clone()))
    }

    /// Finishes a move parked by `confirm_move`/`confirm_bulk_move` because it needed a
    /// message first — see `MoveOp::Commit`. There is no longer a standalone "commit
    /// whatever's staged" key: nothing is staged ahead of a commit anymore, so every commit
    /// now originates from a move that just happened to need a message.
    fn confirm_commit(&mut self) {
        let message = self.commit_input.trimmed();
        if message.is_empty() {
            self.notify("a commit needs a message", Notice::Info);
            return;
        }
        let Some(pending) = self.pending_commit_move.take() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;
        self.commit_input.clear();

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        let result = but
            .commit(&pending.changes, &message, &pending.branch)
            .and_then(|status| {
                if pending.also_move.is_empty() {
                    Ok(status)
                } else {
                    but.move_commits(&pending.also_move, &pending.branch)
                }
            });
        match result {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(
                    format!("committed {} → {}", pending.source_label, pending.target_label),
                    Notice::Success,
                );
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Starts naming a new branch.
    ///
    /// Defaults to stacking when a lane is selected and to a parallel lane otherwise,
    /// because that is what pressing the key *there* most likely means. Tab overrides it,
    /// so neither choice requires navigating somewhere else first.
    fn begin_branch(&mut self) {
        self.branch_input.clear();
        self.stack_onto = self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.clone());
        self.open_harness = true;
        self.mode = Mode::Branch;
    }

    /// Flips between stacking on the selected lane and creating a parallel one.
    fn toggle_stack_onto(&mut self) {
        if self.stack_onto.is_some() {
            self.stack_onto = None;
        } else {
            self.stack_onto = self
                .board
                .columns
                .get(self.col)
                .and_then(|c| c.branch_name.clone());
            if self.stack_onto.is_none() {
                self.notify("the backlog has no branch to stack on", Notice::Info);
            }
        }
    }

    /// Opts a parallel lane out of its cmux split for this one branch, without touching the
    /// standing `KANSTACK_CMUX_*` config. No-op while stacking, since a stacked branch never
    /// opens one to opt out of.
    fn toggle_open_harness(&mut self) {
        if self.cmux.is_none() {
            self.notify("cmux is not configured", Notice::Info);
            return;
        }
        if self.stack_onto.is_some() {
            return;
        }
        self.open_harness = !self.open_harness;
    }

    /// What `b` will do, in the same spirit as the move footer: say it before doing it.
    pub fn pending_branch_action(&self) -> String {
        match &self.stack_onto {
            Some(anchor) => format!("stack on {anchor}"),
            None if self.cmux.is_some() && self.open_harness => {
                "new parallel lane · opens cmux".to_string()
            }
            None if self.cmux.is_some() => "new parallel lane · no cmux".to_string(),
            None => "new parallel lane".to_string(),
        }
    }

    fn confirm_branch(&mut self) {
        let name = self.branch_input.trimmed();
        if name.is_empty() {
            self.notify("a branch needs a name", Notice::Info);
            return;
        }
        let anchor = self.stack_onto.clone();
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        let cwd = but.cwd().to_path_buf();
        match but.branch_new(&name, anchor.as_deref()) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(
                    match &anchor {
                        Some(a) => format!("created {name} on top of {a}"),
                        None => format!("created {name}"),
                    },
                    Notice::Success,
                );
                // Put the cursor on what was just made, so it can be worked with at once.
                if let Some(i) = self
                    .board
                    .columns
                    .iter()
                    .position(|c| c.branch_name.as_deref() == Some(name.as_str()))
                {
                    self.col = i;
                    self.card = 0;
                }
                // A stacked lane shares its base's tab; only a parallel lane is new work
                // worth a harness of its own, and only when that wasn't opted out of with
                // shift-tab.
                if anchor.is_none() && self.open_harness {
                    if let Some(cmux) = &mut self.cmux {
                        if let Err(e) = cmux.spawn_harness(&cwd, &name) {
                            self.notify(format!("cmux: {e}"), Notice::Error);
                        }
                    }
                }
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
        self.branch_input.clear();
    }

    /// The branch name identifying lane `col`'s cmux pane: whichever section already has
    /// one tracked (the lane's original parallel branch, even once other branches have
    /// stacked on top of it — stacking never opens a second pane), or the tip branch if
    /// none has been opened yet, so a first dispatch spawns one labelled the way the lane
    /// reads today.
    fn pane_branch(&self, col: usize) -> Option<String> {
        let column = self.board.columns.get(col)?;
        if let Some(cmux) = &self.cmux {
            for section in &column.sections {
                if cmux.has_pane(&section.name) {
                    return Some(section.name.clone());
                }
            }
        }
        column.branch_name.clone()
    }

    /// Starts typing a task description to send into the selected lane's cmux pane.
    fn begin_task_dispatch(&mut self) {
        if self.cmux.is_none() {
            self.notify("cmux is not configured", Notice::Info);
            return;
        }
        let Some(branch) = self.pane_branch(self.col) else {
            self.notify("pick a lane with a branch — the backlog has no pane", Notice::Info);
            return;
        };
        self.task_target = Some(branch);
        self.task_input.clear();
        self.mode = Mode::Task;
    }

    fn confirm_task_dispatch(&mut self) {
        let text = self.task_input.trimmed();
        self.mode = Mode::Normal;
        if text.is_empty() {
            self.notify("a task needs a description", Notice::Info);
            return;
        }
        let Some(branch) = self.task_target.take() else {
            return;
        };

        let has_pane = self.cmux.as_ref().is_some_and(|c| c.has_pane(&branch));
        if !has_pane {
            // Deliberately does not send the just-typed text in the same action: a fresh
            // pane's shell needs a moment to launch the harness before it can receive a
            // second line, and `cmux send` has no "wait until ready" primitive to lean on.
            // Spawning now and asking the user to press `t` again is simpler and safer
            // than guessing a delay or racing the harness's own startup.
            let Some(but) = &self.but else {
                self.notify("snapshot is read-only", Notice::Info);
                return;
            };
            let cwd = but.cwd().to_path_buf();
            let Some(cmux) = &mut self.cmux else { return };
            match cmux.spawn_harness(&cwd, &branch) {
                Ok(()) => self.notify(
                    format!("opened a pane for {branch} — press t again once it's ready for the task"),
                    Notice::Info,
                ),
                Err(e) => self.notify(format!("cmux: {e}"), Notice::Error),
            }
            self.task_input.clear();
            return;
        }

        let Some(cmux) = &self.cmux else { return };
        match cmux.send_task(&branch, &text) {
            Ok(()) => self.notify(format!("sent task to {branch}"), Notice::Success),
            Err(e) => self.notify(format!("cmux: {e}"), Notice::Error),
        }
        self.task_input.clear();
    }

    /// Opens the diff for the selected card.
    ///
    /// A working-tree file resolves to its hunks, each individually stageable. A commit
    /// resolves to its own diff, which is read-only — `but` gives committed changes no ids.
    fn open_diff(&mut self) {
        let Some(card) = self.selected_card().cloned() else {
            self.notify("nothing selected", Notice::Info);
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        let result = match card.kind {
            CardKind::Change => but.diff_uncommitted().map(|out| {
                // `but diff` covers the whole worktree; keep the file we asked about.
                let filtered = crate::model::DiffOutput {
                    changes: out
                        .changes
                        .into_iter()
                        .filter(|c| c.path == card.title)
                        .collect(),
                };
                DiffView::from_output(card.title.clone(), &filtered)
            }),
            CardKind::Commit => but
                .diff_target(&card.rub_id)
                .map(|out| DiffView::from_output(card.title.clone(), &out)),
        };
        match result {
            Ok(view) if view.entries.is_empty() => {
                self.diff = None;
                self.notify("no diff to show", Notice::Info);
            }
            Ok(view) => {
                self.diff = Some(view);
                self.mode = Mode::Diff;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Picks up the hunk under the diff cursor and hands off to the ordinary move flow,
    /// so staging a hunk uses the same lane targeting as everything else.
    ///
    /// Closing the pane here is load-bearing, not tidiness: hunk ids describe the current
    /// state, and staging one renumbers whatever is left. Holding the old list open would
    /// leave every remaining id pointing at the wrong hunk. Reopening re-queries.
    fn move_hunk_from_diff(&mut self) {
        let Some(view) = &self.diff else { return };
        let Some(entry) = view.selected() else { return };
        let Some(id) = entry.rub_id.clone() else {
            self.notify("this hunk is already committed", Notice::Info);
            return;
        };
        let label = format!("hunk in {}", entry.path);
        self.diff = None;
        self.move_source = Some((id, label));
        self.origin_col = self.col;
        self.origin_card = self.card;
        self.target_card = None;
        self.mode = Mode::Moving;
    }


    /// Fetches and previews a rebase onto the updated target.
    ///
    /// The header already reports how far behind upstream the workspace is; without this
    /// that number is a dead end. `--check` is read-only, so nothing moves until the
    /// preview has been seen and confirmed.
    fn begin_rebase(&mut self) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.pull_check() {
            Ok(preview) => {
                if preview.up_to_date {
                    self.notify("already up to date with the target", Notice::Info);
                    return;
                }
                self.pull_preview = Some(preview);
                self.mode = Mode::RebaseConfirm;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    fn confirm_rebase(&mut self) {
        self.mode = Mode::Normal;
        self.pull_preview = None;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.pull() {
            Ok(()) => {
                // `pull` ignores `--status-after`, like `push`.
                self.refresh_quietly();
                self.notify("rebased onto the updated target", Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Describes what deleting the selected lane would do.
    ///
    /// `d` always deletes the lane's tip branch. Whether that branch is alone or stacked
    /// on others, deleting it discards *its own* commits outright — there is no folding
    /// into the branch below (verified against 0.21.2; earlier `but` folded a mid-stack
    /// branch's commits upward and refused a lone branch with unpushed commits
    /// non-interactively, neither of which happens now). `but undo` is the safety net
    /// instead, so the confirmation says that rather than implying nothing can be lost.
    pub fn pending_delete(&self) -> Option<(String, String)> {
        let col = self.board.columns.get(self.col)?;
        let name = col.branch_name.clone()?;
        let commits = col
            .sections
            .first()
            .map(|s| s.commits)
            .unwrap_or(col.cards.len());
        let detail = if commits == 0 {
            "it is empty".to_string()
        } else {
            format!(
                "discards {commits} commit{} — recoverable with `but undo`",
                if commits == 1 { "" } else { "s" }
            )
        };
        Some((name, detail))
    }

    fn begin_delete(&mut self) {
        if self.pending_delete().is_none() {
            self.notify("the backlog is not a branch", Notice::Info);
            return;
        }
        self.mode = Mode::DeleteConfirm;
    }

    fn confirm_delete(&mut self) {
        let Some((name, _)) = self.pending_delete() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.branch_delete(&name) {
            Ok(()) => {
                self.refresh_quietly();
                self.notify(format!("deleted {name}"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    // ---- unapplied branches: the drawer, apply, and unapply ----

    /// Opens the drawer, fetching the branch list, or closes it if it is already open.
    ///
    /// The fetch is synchronous, unlike `land`'s background thread: `but branch list`
    /// without `--review` is local work only — a merge check and a rev walk per branch,
    /// measured at ~40ms — so it lands well inside a frame. `--review` is what would make
    /// this a network call, and it is deliberately not passed (see `But::branch_list`).
    fn toggle_branch_drawer(&mut self) {
        if self.mode == Mode::Branches {
            self.mode = Mode::Normal;
            return;
        }
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.branch_list() {
            Ok(list) => {
                self.unapplied = Unapplied::from_list(&list, now_ms());
                // Keep the cursor in range across a refetch that shrank the list, the same
                // way `clamp` does for the board.
                self.branch_sel = self
                    .branch_sel
                    .min(self.unapplied.branches.len().saturating_sub(1));
                self.mode = Mode::Branches;
                if self.unapplied.is_empty() {
                    self.notify("every branch is already applied", Notice::Info);
                }
            }
            Err(e) => self.notify(format!("could not list branches: {e}"), Notice::Error),
        }
    }

    /// Re-reads the branch list in place, without touching the mode.
    ///
    /// Called after an apply or unapply, both of which move a branch from one side of the
    /// list to the other. Failing here is a notice rather than a mode change: the mutation
    /// itself already succeeded, so a stale drawer is a cosmetic problem, not a wrong one.
    fn refresh_branch_list(&mut self) {
        let Some(but) = &self.but else { return };
        match but.branch_list() {
            Ok(list) => {
                self.unapplied = Unapplied::from_list(&list, now_ms());
                self.branch_sel = self
                    .branch_sel
                    .min(self.unapplied.branches.len().saturating_sub(1));
            }
            Err(e) => self.notify(format!("could not list branches: {e}"), Notice::Error),
        }
    }

    pub fn selected_unapplied(&self) -> Option<&crate::board::UnappliedBranch> {
        self.unapplied.branches.get(self.branch_sel)
    }

    /// Applies the highlighted branch, bringing it in as a new lane.
    ///
    /// Applies even when `mergesCleanly` is false rather than refusing: a conflicted apply
    /// is a legitimate thing to want — it is how you find out what conflicts — and `but`
    /// leaves the result recoverable. The row says so beforehand and the notice says so
    /// afterwards, which is the honest version of a guardrail that would otherwise just
    /// block a supported operation.
    fn apply_selected_branch(&mut self) {
        let Some(branch) = self.selected_unapplied().cloned() else {
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.apply(&branch.name) {
            Ok(()) => {
                let name = branch.name.clone();
                self.refresh_quietly();
                self.refresh_branch_list();
                // Land the cursor on what was just applied, so the lane you asked for is
                // the one selected when the drawer closes — otherwise applying a branch
                // leaves you looking at whichever lane you happened to be on before.
                if let Some(i) = self
                    .board
                    .columns
                    .iter()
                    .position(|c| c.branch_name.as_deref() == Some(name.as_str()))
                {
                    self.col = i;
                    self.card = 0;
                }
                // Not an unconditional assignment: the refresh above can discover a
                // workspace-wide block and switch to `Blocked`, and that must win — it
                // means the board behind this drawer no longer describes the repository.
                if self.mode == Mode::Branches {
                    self.mode = Mode::Normal;
                }
                if branch.merges_cleanly == Some(false) {
                    self.notify(
                        format!("applied {name} — it reported conflicts, check the lane"),
                        Notice::Info,
                    );
                } else {
                    self.notify(format!("applied {name}"), Notice::Success);
                }
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Describes what unapplying the selected lane would do: the branch to name it by, and
    /// what leaves the workspace with it.
    ///
    /// The detail spells out the whole-stack behaviour because that is the one part a user
    /// cannot see coming — `but unapply` takes the stack containing the named branch, so on
    /// a lane of three stacked branches, pressing `U` on the tip removes all three. Naming
    /// them is the difference between a confirmation and a surprise.
    pub fn pending_unapply(&self) -> Option<(String, String)> {
        let col = self.board.columns.get(self.col)?;
        let name = col.branch_name.clone()?;
        // A lane with no commits anywhere in it is the one case where `a` will not bring it
        // back: `but branch list` omits empty branches, so the drawer never offers it. The
        // branch itself survives and `but apply` still works, but promising the drawer
        // would be a lie — and this is reachable, since `b` creates exactly such a lane.
        let commits: usize = col.sections.iter().map(|s| s.commits).sum();
        let detail = if commits == 0 {
            "it has no commits, so the drawer won't list it — `but apply` still brings it back"
                .to_string()
        } else {
            match col.sections.len() {
                0 | 1 => "leaves the workspace; re-apply it any time with `a`".to_string(),
                n => format!(
                    "unapplies the whole stack — all {n} branches: {}",
                    col.sections
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        };
        Some((name, detail))
    }

    fn begin_unapply(&mut self) {
        if self.pending_unapply().is_none() {
            self.notify("the backlog is not a branch", Notice::Info);
            return;
        }
        self.mode = Mode::UnapplyConfirm;
    }

    /// Unapplies the selected lane.
    ///
    /// Confirmed rather than immediate, unlike `u`/send-to-backlog: this reaches into the
    /// working directory and takes the branch's changes off disk. Nothing is destroyed —
    /// the branch survives unapplied, which is exactly what the drawer then lists — but a
    /// working tree emptying out without warning is alarming enough to be worth a keypress.
    fn confirm_unapply(&mut self) {
        let Some((name, _)) = self.pending_unapply() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.unapply(&name) {
            Ok(()) => {
                self.refresh_quietly();
                // Only refetch a drawer that has already been populated: an unapply from
                // the board should not pay for a branch list nobody has asked to see.
                if !self.unapplied.is_empty() {
                    self.refresh_branch_list();
                }
                self.notify(format!("unapplied {name} — `a` brings it back"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Sends the selected card back to the backlog.
    ///
    /// One key for what the rub matrix treats as one operation against the `zz` target: a
    /// commit is uncommitted, a staged file is unstaged. Both preserve content — verified
    /// against the real CLI, which reports "Uncommitted <sha>" and leaves the worktree
    /// intact — so this needs no confirmation step.
    ///
    /// It is reachable by dragging a card onto the backlog lane too; this exists because
    /// that can mean three or four keypresses of `←` from a distant lane.
    fn send_to_backlog(&mut self) {
        let Some(card) = self.selected_card().cloned() else {
            self.notify("nothing selected", Notice::Info);
            return;
        };
        if self
            .board
            .columns
            .get(self.col)
            .is_some_and(|c| c.kind == ColumnKind::Unassigned)
        {
            self.notify("already unassigned", Notice::Info);
            return;
        }
        // A Change card only ever lives in the unassigned lane itself under the current
        // model (nothing is "assigned" pre-commit anymore), so this is reached only for a
        // Commit card in practice — the guard above already refuses an already-unassigned
        // selection.
        if card.kind == CardKind::Change {
            self.notify("nothing to unassign", Notice::Info);
            return;
        }

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.uncommit(std::slice::from_ref(&card.rub_id)) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(format!("uncommitted {}", card.cli_id), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Picks up the whole lane, to stack it onto another one.
    fn begin_restack(&mut self) {
        if self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.as_ref())
            .is_none()
        {
            self.notify("the backlog is not a branch to stack", Notice::Info);
            return;
        }
        if self.board.columns.iter().filter(|c| c.branch_name.is_some()).count() < 2 {
            self.notify("nothing to stack onto — there is only one lane", Notice::Info);
            return;
        }
        self.origin_col = self.col;
        self.mode = Mode::Restacking;
        // Land on a lane that is actually a valid target. Stepping one column blindly can
        // wrap onto the backlog, which has no branch and so can never accept a stack.
        if let Some(i) = self.next_stack_column(self.col) {
            self.col = i;
        }
    }

    /// Next column after `from` that is a branch lane other than the origin.
    fn next_stack_column(&self, from: usize) -> Option<usize> {
        let n = self.board.columns.len();
        (1..=n)
            .map(|step| (from + step) % n)
            .find(|&i| i != self.origin_col && self.board.columns[i].branch_name.is_some())
    }

    /// As [`Self::next_stack_column`], walking the other way.
    fn prev_stack_column(&self, from: usize) -> Option<usize> {
        let n = self.board.columns.len();
        (1..=n)
            .map(|step| (from + n - (step % n)) % n)
            .find(|&i| i != self.origin_col && self.board.columns[i].branch_name.is_some())
    }

    /// What `s` will do, named before it happens — this rewrites history.
    pub fn pending_restack(&self) -> Option<String> {
        if self.mode != Mode::Restacking {
            return None;
        }
        let source = self.board.columns.get(self.origin_col)?.branch_name.as_ref()?;
        let target = self.board.columns.get(self.col)?;
        match &target.branch_name {
            Some(t) if t != source => Some(format!("stack {source} onto {t}")),
            Some(_) => Some(format!("{source} — pick a different lane")),
            None => Some(format!("{source} — the backlog cannot hold a stack")),
        }
    }

    fn confirm_restack(&mut self) {
        let source = self
            .board
            .columns
            .get(self.origin_col)
            .and_then(|c| c.branch_name.clone());
        let target = self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.clone());
        let (Some(source), Some(target)) = (source, target) else {
            self.notify("pick a lane to stack onto", Notice::Info);
            return;
        };
        if source == target {
            self.notify("pick a different lane", Notice::Info);
            return;
        }
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.restack_branch(&source, &target) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(
                    format!("stacked {source} onto {target} — commits were rewritten"),
                    Notice::Success,
                );
                if let Some(i) = self
                    .board
                    .columns
                    .iter()
                    .position(|c| c.branch_name.as_deref() == Some(source.as_str()))
                {
                    self.col = i;
                    self.card = 0;
                }
            }
            Err(e) => self.notify(format!("{e:#}"), Notice::Error),
        }
    }

    /// Asks `but` what a push would do and shows it, rather than pushing straight away.
    ///
    /// This is not ceremony: `but push` force-pushes by default, and with GitHub native
    /// stacking a stack push can transiently retarget open PRs. Seeing the destination and
    /// whether a force is involved before confirming is the thing that makes one keystroke
    /// safe to bind at all.
    fn begin_push(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to push — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.push_preview(&branch) {
            Ok(preview) => {
                if preview.branches.iter().all(|b| b.unpushed_commits == 0) {
                    self.notify(format!("{title} has nothing to push"), Notice::Info);
                    return;
                }
                self.push_preview = Some(preview);
                self.mode = Mode::PushConfirm;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    fn confirm_push(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;
        self.push_preview = None;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.push(&branch) {
            Ok(()) => {
                // `push` ignores `--status-after`, so the board has to be reloaded here.
                self.refresh_quietly();
                self.notify(format!("pushed {title}"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Asks `but` whether landing the selected lane onto the target would be clean, and
    /// shows that before landing.
    ///
    /// `but land` has no dry-run of its own, so this is `branch show --check` instead —
    /// read-only, so nothing moves until the preview has been seen and confirmed. Landing
    /// rewrites the target branch, which is a shared one, so the same "see it before it
    /// happens" rule as push applies here, more so — landing pushes directly to a real
    /// remote's target, bypassing any pull-request review entirely.
    fn begin_land(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to land — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.merge_check(&branch) {
            Ok(check) => {
                if check.commits_ahead == 0 {
                    self.notify(format!("{title} has nothing to land"), Notice::Info);
                    return;
                }
                self.land_check = Some(check);
                self.mode = Mode::LandConfirm;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Starts landing on a background thread rather than blocking the UI for however long
    /// the push takes — `but land` can reach a real remote, and a frozen terminal with no
    /// feedback looks indistinguishable from a hang. `poll_land` picks up the result.
    ///
    /// `but land` refuses a non-base branch outright when the lane is a stack ("it is
    /// stacked on top of ... other segment(s)"), with no flag or stack-id argument that
    /// lands the whole thing for you — so on a stacked lane this lands every branch base
    /// first, one `but land` call each, rather than surfacing that refusal to press `M`
    /// again N times with the CLI open to work out the right order.
    fn confirm_land(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.mode = Mode::Normal;
            return;
        };
        self.land_check = None;

        let Some(but) = self.but.clone() else {
            self.mode = Mode::Normal;
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        // `sections` is tip-first (see `Column::sections`'s own doc comment); landing needs
        // the reverse, base first, or `but land` refuses the first call.
        let branches: Vec<String> = match self.board.columns.get(self.col) {
            Some(col) if col.sections.len() > 1 => {
                col.sections.iter().rev().map(|s| s.name.clone()).collect()
            }
            _ => vec![branch],
        };

        let branch_count = branches.len();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(but.land_stack(&branches));
        });
        self.landing = Some(PendingLand {
            title,
            branch_count,
            rx,
            spinner: 0,
        });
        self.mode = Mode::Landing;
    }

    /// Non-blocking check on a `but land` running on a background thread. Call this every
    /// tick of the event loop — it costs nothing when nothing is landing, and is what
    /// advances the spinner and applies the result the moment it arrives.
    pub fn poll_land(&mut self) {
        let Some(pending) = self.landing.as_mut() else {
            return;
        };
        pending.spinner = pending.spinner.wrapping_add(1);
        match pending.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.landing = None;
                self.mode = Mode::Normal;
                self.notify("landing thread vanished unexpectedly", Notice::Error);
            }
            Ok(Ok(status)) => {
                let title = pending.title.clone();
                let branch_count = pending.branch_count;
                self.landing = None;
                self.mode = Mode::Normal;
                if let Some(but) = self.but.clone() {
                    self.board = Self::board_from(&but, &mut self.commit_stats, &status);
                }
                self.clamp();
                let message = if branch_count > 1 {
                    format!("landed {branch_count} branches onto the target, base first")
                } else {
                    format!("landed {title} onto the target")
                };
                self.notify(message, Notice::Success);
            }
            Ok(Err(e)) => {
                self.landing = None;
                self.mode = Mode::Normal;
                self.notify(format!("{e}"), Notice::Error);
            }
        }
        self.check_tutorial_advance();
    }

    /// Undoes the last operation. Fires immediately rather than asking first: undo is
    /// itself the safety net for every other action here (`d` in particular leans on it
    /// now that `but` no longer refuses a destructive delete), and `Z` reverses it if `z`
    /// was thrown one too many times — the same "no confirmation" contract an editor's
    /// own undo/redo has.
    fn undo(&mut self) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.undo() {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify("undid the last operation", Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    fn redo(&mut self) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.redo() {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify("redid the last undone operation", Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
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

    fn handle_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode as K;
        use ratatui::crossterm::event::KeyModifiers;

        // Checked before everything else, including the other modals: while the workspace
        // is blocked there is no navigation, no editing and no cancelling back to a board
        // worth trusting. Only the two recoveries and the way out. Neither runs on its own
        // — both rewrite history, and a modal that acted before it was read would be a
        // worse failure than the one it exists to report.
        if self.mode == Mode::Blocked {
            match key.code {
                K::Char('r') => self.recover_reset(),
                K::Char('t') => self.recover_teardown(),
                K::Char('q') | K::Esc => self.should_quit = true,
                _ => {}
            }
            return;
        }

        // Typing a commit message swallows ordinary keys, so navigation bindings do not
        // eat the letters being typed.
        if self.mode == Mode::Commit {
            match key.code {
                K::Esc => {
                    self.mode = Mode::Normal;
                    self.commit_input.clear();
                    self.pending_commit_move = None;
                    self.notify("commit cancelled", Notice::Info);
                }
                K::Enter => self.confirm_commit(),
                K::Backspace => self.commit_input.backspace(),
                K::Delete => self.commit_input.delete_forward(),
                K::Left => self.commit_input.move_left(),
                K::Right => self.commit_input.move_right(),
                K::Home => self.commit_input.move_home(),
                K::End => self.commit_input.move_end(),
                K::Char(c) => self.commit_input.insert(c),
                _ => {}
            }
            return;
        }

        if self.mode == Mode::Branch {
            match key.code {
                K::Esc => {
                    self.mode = Mode::Normal;
                    self.branch_input.clear();
                    self.notify("branch cancelled", Notice::Info);
                }
                K::Enter => self.confirm_branch(),
                K::Tab => self.toggle_stack_onto(),
                K::BackTab => self.toggle_open_harness(),
                K::Backspace => self.branch_input.backspace(),
                K::Delete => self.branch_input.delete_forward(),
                K::Left => self.branch_input.move_left(),
                K::Right => self.branch_input.move_right(),
                K::Home => self.branch_input.move_home(),
                K::End => self.branch_input.move_end(),
                K::Char(c) => self.branch_input.insert(c),
                _ => {}
            }
            return;
        }

        if self.mode == Mode::Task {
            match key.code {
                K::Esc => {
                    self.mode = Mode::Normal;
                    self.task_input.clear();
                    self.task_target = None;
                    self.notify("task cancelled", Notice::Info);
                }
                K::Enter => self.confirm_task_dispatch(),
                K::Backspace => self.task_input.backspace(),
                K::Delete => self.task_input.delete_forward(),
                K::Left => self.task_input.move_left(),
                K::Right => self.task_input.move_right(),
                K::Home => self.task_input.move_home(),
                K::End => self.task_input.move_end(),
                K::Char(c) => self.task_input.insert(c),
                _ => {}
            }
            return;
        }

        if self.mode == Mode::PushConfirm {
            match key.code {
                K::Enter | K::Char('y') => self.confirm_push(),
                K::Esc | K::Char('n') | K::Char('q') => {
                    self.mode = Mode::Normal;
                    self.push_preview = None;
                    self.notify("push cancelled", Notice::Info);
                }
                _ => {}
            }
            return;
        }

        if self.mode == Mode::LandConfirm {
            match key.code {
                K::Enter | K::Char('y') => self.confirm_land(),
                K::Esc | K::Char('n') | K::Char('q') => {
                    self.mode = Mode::Normal;
                    self.land_check = None;
                    self.notify("land cancelled", Notice::Info);
                }
                _ => {}
            }
            return;
        }

        // Nothing to do here but wait: `poll_land` (driven by the event loop, not by a
        // key) is what moves this out of `Landing`. There is deliberately no cancel —
        // once `but land` has started it may already be pushing to a real remote, and a
        // key that looked like it stopped that would be a lie.
        if self.mode == Mode::Landing {
            return;
        }

        if self.mode == Mode::Diff {
            match key.code {
                // Left exits, because the diff sits to the right of the board and going
                // back is a direction, not a toggle. Enter deliberately does nothing here:
                // one key that both opens and closes reads as a surprise.
                // Either arrow leaves. The diff is a detour off the board, not a place with
                // its own left and right, so both horizontal keys mean "back".
                K::Esc | K::Char('q') | K::Left | K::Char('h') | K::Right | K::Char('l') => {
                    self.mode = Mode::Normal;
                    self.diff = None;
                }
                K::Down | K::Char('j') => {
                    if let Some(v) = self.diff.as_mut() {
                        v.move_cursor(1)
                    }
                }
                K::Up | K::Char('k') => {
                    if let Some(v) = self.diff.as_mut() {
                        v.move_cursor(-1)
                    }
                }
                K::Char('m') => self.move_hunk_from_diff(),
                K::Tab => self.diff_full = !self.diff_full,
                _ => {}
            }
            return;
        }

        if self.mode == Mode::RebaseConfirm {
            match key.code {
                K::Enter | K::Char('y') => self.confirm_rebase(),
                K::Esc | K::Char('n') | K::Char('q') => {
                    self.mode = Mode::Normal;
                    self.pull_preview = None;
                    self.notify("rebase cancelled", Notice::Info);
                }
                _ => {}
            }
            return;
        }

        if self.mode == Mode::DeleteConfirm {
            match key.code {
                K::Enter | K::Char('y') => self.confirm_delete(),
                K::Esc | K::Char('n') | K::Char('q') => {
                    self.mode = Mode::Normal;
                    self.notify("delete cancelled", Notice::Info);
                }
                _ => {}
            }
            return;
        }

        if self.mode == Mode::UnapplyConfirm {
            match key.code {
                K::Enter | K::Char('y') => self.confirm_unapply(),
                K::Esc | K::Char('n') | K::Char('q') => {
                    self.mode = Mode::Normal;
                    self.notify("unapply cancelled", Notice::Info);
                }
                _ => {}
            }
            return;
        }

        // The drawer takes the keys while it is open, so j/k walk the branch list rather
        // than the cards behind it. The board stays drawn and stays where it was — this is
        // a focus change, not a screen change.
        if self.mode == Mode::Branches {
            let n = self.unapplied.branches.len();
            match key.code {
                K::Esc | K::Char('q') | K::Char('a') => self.mode = Mode::Normal,
                K::Down | K::Char('j') if n > 0 => {
                    self.branch_sel = (self.branch_sel + 1).min(n - 1);
                }
                K::Up | K::Char('k') => self.branch_sel = self.branch_sel.saturating_sub(1),
                K::Char('g') if n > 0 => self.branch_sel = 0,
                K::Char('G') if n > 0 => self.branch_sel = n - 1,
                K::Enter if n > 0 => self.apply_selected_branch(),
                _ => {}
            }
            return;
        }

        // Help swallows everything except the keys that dismiss it.
        if self.mode == Mode::Help {
            if matches!(key.code, K::Esc | K::Char('?') | K::Char('q')) {
                self.mode = Mode::Normal;
            }
            return;
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

    /// Builds an App without spawning `but`. Only navigation is exercised here;
    /// anything that mutates needs the real CLI and is covered by hand.
    fn board() -> Board {
        Board::from_status(&parse_status(include_str!("../tests/fixtures/status.json")).unwrap())
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
        let mut s = parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
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
}
