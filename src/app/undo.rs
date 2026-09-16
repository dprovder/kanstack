use super::*;

impl App {

    /// Undoes the last operation. Fires immediately rather than asking first: undo is
    /// itself the safety net for every other action here (`d` in particular leans on it
    /// now that `but` no longer refuses a destructive delete), and `Z` reverses it if `z`
    /// was thrown one too many times — the same "no confirmation" contract an editor's
    /// own undo/redo has.
    pub(super) fn undo(&mut self) {
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

    pub(super) fn redo(&mut self) {
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
}
