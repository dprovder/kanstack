use super::*;
use ratatui::crossterm::event::KeyCode as K;

impl App {
    /// Checked before navigation/mutation, same as `Blocked`: there is no board here worth
    /// acting on yet, just the one recovery and the way out.
    pub(super) fn handle_key_setup_required(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Char('s') => self.recover_setup(),
            K::Char('q') | K::Esc => self.should_quit = true,
            _ => {}
        }
    }

    /// Runs `but setup --init`, safe to fire on a bare keypress (no y/n first) — unlike
    /// `Blocked`'s two recoveries, this never rewrites history: it only adds GitButler
    /// bookkeeping (and, at most, an initial empty commit) to a repo the user already chose
    /// to open kanstack against.
    fn recover_setup(&mut self) {
        let Some(but) = self.but.clone() else { return };
        if let Err(e) = but.run_setup() {
            let msg = e.to_string();
            if crate::but::is_missing_git_identity(&msg) {
                self.notify(
                    "git has no identity configured — run `git config --global user.name \
                     \"…\"` and `user.email \"…\"`, then press s again",
                    Notice::Error,
                );
            } else {
                self.notify(format!("`but setup` failed: {msg}"), Notice::Error);
            }
            return;
        }
        self.mode = Mode::Normal;
        // Same "trust but verify" shape `recover_reset` uses: go through the ordinary
        // refresh path, which re-enters `SetupRequired` on its own if this somehow didn't
        // take, rather than assuming success.
        self.refresh();
    }
}
