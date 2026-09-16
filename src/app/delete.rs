use super::*;
use ratatui::crossterm::event::KeyCode as K;

/// Which branch `Mode::DeleteConfirm` is about to delete: the current lane's tip branch,
/// or the drawer's selected unapplied branch. Both go through the same confirmation mode
/// and the same `but branch delete`, but the mode alone can't say which is which — this
/// is set when the confirm is entered and read back by `pending_delete`/`confirm_delete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeleteTarget {
    Lane,
    Unapplied,
}

impl App {

    /// Describes what deleting the selected lane would do.
    ///
    /// `d` always deletes the lane's tip branch. Whether that branch is alone or stacked
    /// on others, deleting it discards *its own* commits outright — there is no folding
    /// into the branch below (verified against 0.21.2; earlier `but` folded a mid-stack
    /// branch's commits upward and refused a lone branch with unpushed commits
    /// non-interactively, neither of which happens now). `but undo` is the safety net
    /// instead, so the confirmation says that rather than implying nothing can be lost.
    pub(super) fn pending_delete_lane(&self) -> Option<(String, String)> {
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

    /// Describes what deleting the drawer's selected unapplied branch would do. Unlike a
    /// lane, it isn't in the workspace at all, so there's no working-tree fallout — just
    /// the branch and whatever commits are unique to it going away.
    pub(super) fn pending_delete_unapplied(&self) -> Option<(String, String)> {
        let b = self.selected_unapplied()?;
        let detail = match b.commits_ahead {
            None | Some(0) => "it has no commits of its own".to_string(),
            Some(n) => format!(
                "discards {n} commit{} — recoverable with `but undo`",
                if n == 1 { "" } else { "s" }
            ),
        };
        Some((b.name.clone(), detail))
    }

    /// The name and consequence of whatever a pending `Mode::DeleteConfirm` is about to
    /// delete. Dispatches on `delete_target` since the mode alone doesn't say whether `d`
    /// was pressed on a lane or on the drawer's selected row.
    pub fn pending_delete(&self) -> Option<(String, String)> {
        match self.delete_target {
            DeleteTarget::Lane => self.pending_delete_lane(),
            DeleteTarget::Unapplied => self.pending_delete_unapplied(),
        }
    }

    /// Whether the pending delete confirmation is for the drawer's selected branch rather
    /// than the current lane — `ui::draw_delete_confirm` uses this to title the dialog.
    pub fn deleting_unapplied(&self) -> bool {
        self.delete_target == DeleteTarget::Unapplied
    }

    pub(super) fn begin_delete(&mut self) {
        if self.pending_delete_lane().is_none() {
            self.notify("the backlog is not a branch", Notice::Info);
            return;
        }
        self.delete_target = DeleteTarget::Lane;
        self.mode = Mode::DeleteConfirm;
    }

    /// Same confirmation, for the branch currently selected in the drawer rather than the
    /// board — reached with `d` while `mode == Branches`.
    pub(super) fn begin_delete_unapplied(&mut self) {
        if self.pending_delete_unapplied().is_none() {
            return;
        }
        self.delete_target = DeleteTarget::Unapplied;
        self.mode = Mode::DeleteConfirm;
    }

    pub(super) fn confirm_delete(&mut self) {
        let Some((name, _)) = self.pending_delete() else {
            self.mode = Mode::Normal;
            return;
        };
        // A lane-delete drops you back on the board, same as before; a drawer-delete
        // returns you to the drawer — you came from there, and deleting one stale branch
        // is often the start of deleting several.
        let from_drawer = self.deleting_unapplied();
        self.mode = if from_drawer { Mode::Branches } else { Mode::Normal };

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.branch_delete(&name) {
            Ok(()) => {
                if from_drawer {
                    self.refresh_branch_list();
                } else {
                    self.refresh_quietly();
                }
                self.notify(format!("deleted {name}"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }
}

impl App {
    pub(super) fn handle_key_delete_confirm(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Enter | K::Char('y') => self.confirm_delete(),
            K::Esc | K::Char('n') | K::Char('q') => {
                // A drawer-initiated delete returns to the drawer it came from, same
                // as confirming it does — cancelling shouldn't drop you somewhere else.
                self.mode = if self.deleting_unapplied() {
                    Mode::Branches
                } else {
                    Mode::Normal
                };
                self.notify("delete cancelled", Notice::Info);
            }
            _ => {}
        }
    }
}
