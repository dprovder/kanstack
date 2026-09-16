use super::*;
use ratatui::crossterm::event::KeyCode as K;

impl App {

    /// Opens the diff for the selected card.
    ///
    /// A working-tree file resolves to its hunks, each individually stageable. A commit
    /// resolves to its own diff, which is read-only — `but` gives committed changes no ids.
    pub(super) fn open_diff(&mut self) {
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
    pub(super) fn move_hunk_from_diff(&mut self) {
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
}

impl App {
    pub(super) fn handle_key_diff(&mut self, key: ratatui::crossterm::event::KeyEvent) {
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
    }
}
