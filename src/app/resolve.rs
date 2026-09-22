use super::*;
use crate::model::ResolveConflicts;
use ratatui::crossterm::event::KeyCode as K;

/// State for `Mode::ResolveConflict`, entered by `App::begin_resolve`.
pub struct ResolveView {
    /// The lane being resolved — see `But::resolve_conflicts` for why this outlives any
    /// one commit id.
    pub branch: String,
    pub title: String,
    pub data: ResolveConflicts,
    /// Index into `data.files` then `data.manual`, treated as one flattened list — see
    /// `ResolveView::row_count`.
    pub selected: usize,
}

impl ResolveView {
    pub fn row_count(&self) -> usize {
        self.data.files.len() + self.data.manual.len()
    }
}

impl App {
    /// Lists conflicts on the selected lane's branch and opens the picker.
    ///
    /// `but` itself refuses a branch with no conflicted commits, so that failure doubles as
    /// the "nothing to resolve here" notice rather than needing a check of its own.
    pub(super) fn begin_resolve(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to resolve conflicts on — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.resolve_conflicts(&branch) {
            Ok(data) => {
                self.resolve_view = Some(ResolveView { branch, title, data, selected: 0 });
                self.mode = Mode::ResolveConflict;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Re-lists the same branch's conflicts after a mutation, so the picker always shows
    /// what `but` actually has left. Closes the picker and reports success once nothing
    /// remains — `but resolve conflicts` on a fully-resolved branch is itself the signal,
    /// since it errors the same way a never-conflicted branch would (see `begin_resolve`).
    fn refresh_resolve(&mut self) {
        if let Some(view) = &self.resolve_view {
            let branch = view.branch.clone();
            let title = view.title.clone();
            match self.but.as_ref().map(|but| but.resolve_conflicts(&branch)) {
                Some(Ok(data)) => {
                    self.resolve_view = Some(ResolveView { branch, title, data, selected: 0 });
                }
                _ => {
                    self.resolve_view = None;
                    self.mode = Mode::Normal;
                    self.notify(format!("{title} is resolved"), Notice::Success);
                }
            }
        }
        self.refresh_quietly();
    }

    /// Resolves the selected row's file by taking one side — refused for a manual-only
    /// conflict, which `apply_side` never targets in the first place (see `row_count`'s
    /// flattening: an index past `data.files` lands here with no file to look up).
    fn apply_side(&mut self, ours: bool) {
        let Some(view) = &self.resolve_view else { return };
        let Some(file) = view.data.files.get(view.selected) else {
            self.notify(
                "that conflict needs a manual edit — run `but resolve` from a shell",
                Notice::Info,
            );
            return;
        };
        let path = file.path.clone();
        let branch = view.branch.clone();
        let Some(but) = &self.but else { return };
        match but.resolve_apply_side(&branch, &path, ours) {
            Ok(()) => self.refresh_resolve(),
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Resolves every remaining conflict in the commit with AI, in one shot — not just the
    /// selected row, since `but resolve <commit-id> --ai` works commit-wide with no
    /// per-file targeting of its own.
    fn apply_ai(&mut self) {
        let Some(view) = &self.resolve_view else { return };
        let commit_id = view.data.commit_id.clone();
        let title = view.title.clone();
        let Some(but) = self.but.clone() else { return };
        self.notify(format!("resolving {title} with AI…"), Notice::Info);
        match but.resolve_ai(&commit_id) {
            Ok(()) => self.refresh_resolve(),
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }
}

impl App {
    pub(super) fn handle_key_resolve_confirm(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let Some(view) = &self.resolve_view else {
            self.mode = Mode::Normal;
            return;
        };
        let rows = view.row_count();
        match key.code {
            K::Up | K::Char('k') if rows > 0 => {
                let view = self.resolve_view.as_mut().expect("checked above");
                view.selected = view.selected.checked_sub(1).unwrap_or(rows - 1);
            }
            K::Down | K::Char('j') if rows > 0 => {
                let view = self.resolve_view.as_mut().expect("checked above");
                view.selected = (view.selected + 1) % rows;
            }
            K::Char('o') => self.apply_side(true),
            K::Char('t') => self.apply_side(false),
            K::Char('A') => self.apply_ai(),
            K::Esc | K::Char('q') => {
                self.resolve_view = None;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }
}
