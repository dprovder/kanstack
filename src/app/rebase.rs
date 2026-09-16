use super::*;
use ratatui::crossterm::event::KeyCode as K;

impl App {


    /// Fetches and previews a rebase onto the updated target.
    ///
    /// The header already reports how far behind upstream the workspace is; without this
    /// that number is a dead end. `--check` is read-only, so nothing moves until the
    /// preview has been seen and confirmed.
    pub(super) fn begin_rebase(&mut self) {
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

    pub(super) fn confirm_rebase(&mut self) {
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
}

impl App {
    pub(super) fn handle_key_rebase_confirm(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Enter | K::Char('y') => self.confirm_rebase(),
            K::Esc | K::Char('n') | K::Char('q') => {
                self.mode = Mode::Normal;
                self.pull_preview = None;
                self.notify("rebase cancelled", Notice::Info);
            }
            _ => {}
        }
    }
}
