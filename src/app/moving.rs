use super::*;

impl App {

    /// What a move will rub, and how to name it. Normally the picked-up card; when a move
    /// began from the diff pane, the hunk chosen there.
    pub(super) fn move_source_ref(&self) -> Option<(String, String)> {
        if let Some((id, label)) = &self.move_source {
            return Some((id.clone(), label.clone()));
        }
        let c = self.source_card()?;
        Some((c.rub_id.clone(), c.cli_id.clone()))
    }

    /// The card currently picked up, valid while moving.
    pub(super) fn source_card(&self) -> Option<&Card> {
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
    pub(super) fn resolve_target(&self) -> Option<(String, String)> {
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
    pub(super) fn pending_op(&self) -> Option<MoveOp> {
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
    pub(super) fn toggle_selected(&mut self) {
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
    pub(super) fn begin_move(&mut self) {
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
    pub(super) fn move_target(&mut self, delta: isize) {
        let n = self.cards_in_current_column() as isize;
        // -1 represents the lane header, giving n + 1 positions to cycle through.
        let cur = self.target_card.map_or(-1, |i| i as isize);
        let next = (cur + 1 + delta).rem_euclid(n + 1) - 1;
        self.target_card = if next < 0 { None } else { Some(next as usize) };
    }

    pub(super) fn confirm_move(&mut self) {
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
    pub(super) fn confirm_bulk_move(&mut self) {
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
}
