//! Application state and input handling.
//!
//! Navigation is pure in-memory so arrow keys never wait on a subprocess. Only mutations
//! shell out, and `rub`/`commit`/`move` fold their refresh into the same call by embedding
//! a status in their reply, so a card move costs one round trip rather than a mutation plus
//! a reload.

use anyhow::Result;

use crate::board::{Board, Card, CardKind, ColumnKind};
use crate::but::But;
use std::collections::HashMap;

use crate::cmux::Cmux;
use crate::diff::DiffView;
use crate::model::{MergeCheck, PullPreview, PushPreview};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// A card has been picked up and is looking for a lane to land in.
    Moving,
    /// Typing a commit message.
    Commit,
    /// Typing a new branch name.
    Branch,
    /// A whole lane has been picked up, looking for a lane to stack onto.
    Restacking,
    /// Looking at what a push would do, before doing it.
    PushConfirm,
    /// Looking at what landing a lane onto the target would do, before doing it.
    LandConfirm,
    /// Confirming a lane deletion.
    DeleteConfirm,
    /// Looking at what rebasing onto the updated target would do.
    RebaseConfirm,
    /// Reading a diff, hunk by hunk.
    Diff,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Info,
    Success,
    Error,
}

pub struct App {
    /// `None` in snapshot mode, where a captured status is rendered read-only.
    but: Option<But>,
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
    pub commit_input: String,
    /// Branch name being typed, valid while `mode == Branch`.
    pub branch_input: String,
    /// While naming a branch: `Some(branch)` stacks the new one on top of it, `None` makes
    /// a parallel lane. Seeded from the selected lane and toggled with tab.
    pub stack_onto: Option<String>,
    /// What a push would do, valid while `mode == PushConfirm`.
    pub push_preview: Option<PushPreview>,
    /// What landing the selected lane onto the target would do, valid while
    /// `mode == LandConfirm`.
    pub land_check: Option<MergeCheck>,
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
}

impl App {
    pub fn new(but: But, cmux: Option<Cmux>) -> Result<Self> {
        let status = but.status()?;
        let mut commit_stats = HashMap::new();
        let board = Self::board_from(&but, &mut commit_stats, &status);
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
            but: Some(but),
            cmux,
            board,
            col: 0,
            card: 0,
            mode: Mode::Normal,
            origin_col: 0,
            origin_card: 0,
            target_card: None,
            commit_input: String::new(),
            branch_input: String::new(),
            stack_onto: None,
            push_preview: None,
            land_check: None,
            pull_preview: None,
            diff: None,
            diff_full: false,
            move_source: None,
            commit_stats,
            message,
            should_quit: false,
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
            commit_input: String::new(),
            branch_input: String::new(),
            stack_onto: None,
            push_preview: None,
            land_check: None,
            pull_preview: None,
            diff: None,
            diff_full: false,
            move_source: None,
            commit_stats: HashMap::new(),
            message: None,
            should_quit: false,
        }
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
        if self.board.columns.is_empty() {
            self.col = 0;
            self.card = 0;
            return;
        }
        self.col = self.col.min(self.board.columns.len() - 1);
        let n = self.cards_in_current_column();
        self.card = if n == 0 { 0 } else { self.card.min(n - 1) };
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
            Err(e) => self.notify(format!("refresh failed: {e}"), Notice::Error),
        }
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

    /// Resolves the drop target into the CLI id to rub onto, plus a label for the footer.
    ///
    /// Targeting the lane header rubs onto its branch; targeting a card rubs onto that
    /// commit. Both are the same `but rub` call — only the target id differs, which is why
    /// squash and amend need no code of their own.
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

    /// Describes what `but rub` will do, derived from its documented operations matrix.
    /// Shown before committing to it, because "commit onto unassigned" is an uncommit and
    /// "commit onto commit" is a squash, and neither should be a surprise.
    pub fn pending_action(&self) -> Option<String> {
        if self.mode != Mode::Moving {
            return None;
        }
        let (_, source_label) = self.move_source_ref()?;
        let (_, target_label) = self.resolve_target()?;
        let verb = self.pending_verb()?;
        Some(format!("{verb} {source_label} → {target_label}"))
    }

    /// `None` when the combination is not a supported rub.
    fn pending_verb(&self) -> Option<&'static str> {
        // A hunk picked from the diff pane behaves like any other working-tree change.
        let source_kind = if self.move_source.is_some() {
            CardKind::Change
        } else {
            self.source_card()?.kind
        };
        let col = self.board.columns.get(self.col)?;
        match self.target_card {
            // Dropping on a lane: move or stage, per the matrix's Branch column.
            None => Some(match (source_kind, col.kind) {
                (CardKind::Change, ColumnKind::Unassigned) => "unstage",
                (CardKind::Change, ColumnKind::Stack) => "stage to",
                (CardKind::Commit, ColumnKind::Unassigned) => "uncommit into",
                (CardKind::Commit, ColumnKind::Stack) => "move to",
            }),
            // Dropping on a card: only commits are valid targets. The matrix has no
            // file-onto-file operation, so that combination is refused rather than guessed.
            Some(i) => match (source_kind, col.cards.get(i)?.kind) {
                (CardKind::Commit, CardKind::Commit) => Some("squash into"),
                (CardKind::Change, CardKind::Commit) => Some("amend into"),
                (_, CardKind::Change) => None,
            },
        }
    }

    fn begin_move(&mut self) {
        if self.selected_card().is_none() {
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

        let Some(verb) = self.pending_verb() else {
            self.notify("that combination isn't a supported operation", Notice::Error);
            return;
        };
        let verb = verb.to_string();
        let Some((target_id, target_label)) = self.resolve_target() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;
        self.move_source = None;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };

        match but.rub(&source_id, &target_id) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(format!("{verb} {source_label} → {target_label}"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
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

    fn begin_commit(&mut self) {
        let Some((_, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to commit to — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        let has_staged = self
            .board
            .columns
            .get(self.col)
            .is_some_and(|c| c.cards.iter().any(|k| k.kind == CardKind::Change));
        if !has_staged {
            self.notify(
                format!("nothing staged to {title} — move a file there first"),
                Notice::Info,
            );
            return;
        }
        self.commit_input.clear();
        self.mode = Mode::Commit;
    }

    fn confirm_commit(&mut self) {
        let message = self.commit_input.trim().to_string();
        if message.is_empty() {
            self.notify("a commit needs a message", Notice::Info);
            return;
        }
        let Some((branch, title)) = self.selected_branch() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.commit(&branch, &message) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(format!("committed to {title}"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
        self.commit_input.clear();
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

    /// What `b` will do, in the same spirit as the move footer: say it before doing it.
    pub fn pending_branch_action(&self) -> String {
        match &self.stack_onto {
            Some(anchor) => format!("stack on {anchor}"),
            None => "new parallel lane".to_string(),
        }
    }

    fn confirm_branch(&mut self) {
        let name = self.branch_input.trim().to_string();
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
                // worth a harness of its own.
                if anchor.is_none() {
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
        let verb = match card.kind {
            CardKind::Commit => "uncommitted",
            CardKind::Change => "unstaged",
        };

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.rub(&card.rub_id, crate::board::UNASSIGNED_TARGET) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(format!("{verb} {}", card.cli_id), Notice::Success);
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

    fn confirm_land(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;
        self.land_check = None;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.land(&branch) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(format!("landed {title} onto the target"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
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

    pub fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode as K;

        // Typing a commit message swallows ordinary keys, so navigation bindings do not
        // eat the letters being typed.
        if self.mode == Mode::Commit {
            match key.code {
                K::Esc => {
                    self.mode = Mode::Normal;
                    self.commit_input.clear();
                    self.notify("commit cancelled", Notice::Info);
                }
                K::Enter => self.confirm_commit(),
                K::Backspace => {
                    self.commit_input.pop();
                }
                K::Char(c) => self.commit_input.push(c),
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
                K::Backspace => {
                    self.branch_input.pop();
                }
                K::Char(c) => self.branch_input.push(c),
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
                _ => self.should_quit = true,
            },
            K::Char('?') => self.mode = Mode::Help,
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
            K::Char('m') if self.mode == Mode::Normal => self.begin_move(),
            K::Char('c') if self.mode == Mode::Normal => self.begin_commit(),
            K::Char('p') if self.mode == Mode::Normal => self.begin_push(),
            K::Char('M') if self.mode == Mode::Normal => self.begin_land(),
            K::Char('z') if self.mode == Mode::Normal => self.undo(),
            K::Char('Z') if self.mode == Mode::Normal => self.redo(),
            K::Char('b') if self.mode == Mode::Normal => self.begin_branch(),
            K::Char('s') if self.mode == Mode::Normal => self.begin_restack(),
            K::Char('u') if self.mode == Mode::Normal => self.send_to_backlog(),
            K::Char('d') if self.mode == Mode::Normal => self.begin_delete(),
            K::Char('r') if self.mode == Mode::Normal => self.begin_rebase(),
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
}
