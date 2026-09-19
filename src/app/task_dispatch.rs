use super::*;
use ratatui::crossterm::event::KeyCode as K;

impl App {

    /// The branch name identifying lane `col`'s split pane: whichever section already has
    /// one tracked (the lane's original parallel branch, even once other branches have
    /// stacked on top of it — stacking never opens a second pane), or the tip branch if
    /// none has been opened yet, so a first dispatch spawns one labelled the way the lane
    /// reads today.
    pub(super) fn pane_branch(&self, col: usize) -> Option<String> {
        let column = self.board.columns.get(col)?;
        if let Some(splitter) = &self.splitter {
            for section in &column.sections {
                if splitter.has_pane(&section.name) {
                    return Some(section.name.clone());
                }
            }
        }
        column.branch_name.clone()
    }

    /// Starts typing a task description to send into the selected lane's split pane.
    pub(super) fn begin_task_dispatch(&mut self) {
        if self.splitter.is_none() {
            self.notify("no harness-split backend found (cmux or tmux)", Notice::Info);
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

    pub(super) fn confirm_task_dispatch(&mut self) {
        let text = self.task_input.trimmed();
        self.mode = Mode::Normal;
        if text.is_empty() {
            self.notify("a task needs a description", Notice::Info);
            return;
        }
        let Some(branch) = self.task_target.take() else {
            return;
        };

        let has_pane = self.splitter.as_ref().is_some_and(|s| s.has_pane(&branch));
        if !has_pane {
            // Deliberately does not send the just-typed text in the same action: a fresh
            // pane's shell needs a moment to launch the harness before it can receive a
            // second line, and neither backend has a "wait until ready" primitive to lean
            // on. Spawning now and asking the user to press `t` again is simpler and safer
            // than guessing a delay or racing the harness's own startup.
            let Some(but) = &self.but else {
                self.notify("snapshot is read-only", Notice::Info);
                return;
            };
            let cwd = but.cwd().to_path_buf();
            let Some(splitter) = &mut self.splitter else { return };
            let label = splitter.label();
            match splitter.spawn_harness(&cwd, &branch, None) {
                Ok(()) => self.notify(
                    format!("opened a pane for {branch} — press t again once it's ready for the task"),
                    Notice::Info,
                ),
                Err(e) => self.notify(format!("{label}: {e}"), Notice::Error),
            }
            self.task_input.clear();
            return;
        }

        let Some(splitter) = &self.splitter else { return };
        let label = splitter.label();
        match splitter.send_task(&branch, &text) {
            Ok(()) => self.notify(format!("sent task to {branch}"), Notice::Success),
            Err(e) => self.notify(format!("{label}: {e}"), Notice::Error),
        }
        self.task_input.clear();
    }
}

impl App {
    pub(super) fn handle_key_task(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Esc => {
                self.mode = Mode::Normal;
                self.task_input.clear();
                self.task_target = None;
                self.notify("task cancelled", Notice::Info);
            }
            K::Enter => self.confirm_task_dispatch(),
            _ => {
                self.task_input.handle_key(key);
            }
        }
    }
}
