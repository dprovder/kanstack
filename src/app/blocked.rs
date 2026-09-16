use super::*;
use ratatui::crossterm::event::KeyCode as K;

impl App {
    /// Checked before everything else, including the other modals: while the workspace
    /// is blocked there is no navigation, no editing and no cancelling back to a board
    /// worth trusting. Only the two recoveries and the way out. Neither runs on its own
    /// — both rewrite history, and a modal that acted before it was read would be a
    /// worse failure than the one it exists to report.
    pub(super) fn handle_key_blocked(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Char('r') => self.recover_reset(),
            K::Char('t') => self.recover_teardown(),
            K::Char('q') | K::Esc => self.should_quit = true,
            _ => {}
        }
    }
}
