use super::*;
use ratatui::crossterm::event::KeyCode as K;

impl App {

    /// Finishes a move parked by `confirm_move`/`confirm_bulk_move` because it needed a
    /// message first — see `MoveOp::Commit`. There is no longer a standalone "commit
    /// whatever's staged" key: nothing is staged ahead of a commit anymore, so every commit
    /// now originates from a move that just happened to need a message.
    pub(super) fn confirm_commit(&mut self) {
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
}

impl App {
    /// Typing a commit message swallows ordinary keys, so navigation bindings do not
    /// eat the letters being typed.
    pub(super) fn handle_key_commit(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Esc => {
                self.mode = Mode::Normal;
                self.commit_input.clear();
                self.pending_commit_move = None;
                self.notify("commit cancelled", Notice::Info);
            }
            K::Enter => self.confirm_commit(),
            _ => {
                self.commit_input.handle_key(key);
            }
        }
    }
}
