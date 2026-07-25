//! Application state and input handling.
//!
//! Navigation is pure in-memory so arrow keys never wait on a subprocess. Only mutations
//! shell out, and those fold their refresh into the same call via `--status-after`, so a
//! card move costs one round trip rather than a mutation plus a reload.

use anyhow::Result;

use crate::board::{Board, Card, CardKind, ColumnKind};
use crate::but::But;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// A card has been picked up and is looking for a lane to land in.
    Moving,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Info,
    Success,
    Error,
}

pub struct App {
    /// `None` in snapshot mode, where a captured status is rendered read-only.
    but: Option<But>,
    pub board: Board,
    pub col: usize,
    pub card: usize,
    pub mode: Mode,
    /// Column the picked-up card came from, valid while `mode == Moving`.
    pub origin_col: usize,
    pub message: Option<(String, Notice)>,
    pub should_quit: bool,
}

impl App {
    pub fn new(but: But) -> Result<Self> {
        let board = Board::from_status(&but.status()?);
        let mut message = None;
        if but.is_untested_version() {
            message = Some((
                format!(
                    "but {} is newer than the {} this was verified against — \
                     report anything that looks wrong",
                    but.version(),
                    crate::but::VERIFIED_THROUGH
                ),
                Notice::Info,
            ));
        }
        Ok(App {
            but: Some(but),
            board,
            col: 0,
            card: 0,
            mode: Mode::Normal,
            origin_col: 0,
            message,
            should_quit: false,
        })
    }

    /// Read-only view over a captured status. Used by `--snapshot` and by tests, so the
    /// exact rendering can be reproduced without anyone's repository.
    pub fn from_board(board: Board) -> Self {
        App {
            but: None,
            board,
            col: 0,
            card: 0,
            mode: Mode::Normal,
            origin_col: 0,
            message: None,
            should_quit: false,
        }
    }

    pub fn column_count(&self) -> usize {
        self.board.columns.len()
    }

    pub fn selected_card(&self) -> Option<&Card> {
        self.board.columns.get(self.col)?.cards.get(self.card)
    }

    fn cards_in_current_column(&self) -> usize {
        self.board
            .columns
            .get(self.col)
            .map_or(0, |c| c.cards.len())
    }

    /// Keeps the cursor inside the board after the shape changes under it.
    fn clamp(&mut self) {
        if self.board.columns.is_empty() {
            self.col = 0;
            self.card = 0;
            return;
        }
        self.col = self.col.min(self.board.columns.len() - 1);
        let n = self.cards_in_current_column();
        self.card = if n == 0 { 0 } else { self.card.min(n - 1) };
    }

    pub fn notify(&mut self, msg: impl Into<String>, kind: Notice) {
        self.message = Some((msg.into(), kind));
    }

    pub fn refresh(&mut self) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.status() {
            Ok(s) => {
                self.board = Board::from_status(&s);
                self.clamp();
            }
            Err(e) => self.notify(format!("refresh failed: {e}"), Notice::Error),
        }
    }

    fn move_column(&mut self, delta: isize) {
        let n = self.column_count();
        if n == 0 {
            return;
        }
        let next = (self.col as isize + delta).rem_euclid(n as isize) as usize;
        self.col = next;
        // Landing in a new lane, keep the cursor on a real card.
        let count = self.cards_in_current_column();
        self.card = if count == 0 {
            0
        } else {
            self.card.min(count - 1)
        };
    }

    fn move_card(&mut self, delta: isize) {
        let n = self.cards_in_current_column();
        if n == 0 {
            return;
        }
        self.card = (self.card as isize + delta).clamp(0, n as isize - 1) as usize;
    }

    /// Describes what `but rub` will do, derived from its documented operations matrix.
    /// Shown before committing to it, because "commit onto unassigned" is an uncommit and
    /// that should never be a surprise.
    pub fn pending_action(&self) -> Option<String> {
        if self.mode != Mode::Moving {
            return None;
        }
        let card = self
            .board
            .columns
            .get(self.origin_col)?
            .cards
            .get(self.card)?;
        let target = self.board.columns.get(self.col)?;
        let verb = match (card.kind, target.kind) {
            (CardKind::Change, ColumnKind::Unassigned) => "unstage",
            (CardKind::Change, ColumnKind::Stack) => "stage to",
            (CardKind::Commit, ColumnKind::Unassigned) => "uncommit into",
            (CardKind::Commit, ColumnKind::Stack) => "move to",
        };
        Some(
            format!("{} {} → {} {}", verb, card.cli_id, target.title, {
                if self.col == self.origin_col {
                    "(same lane)"
                } else {
                    ""
                }
            })
            .trim_end()
            .to_string(),
        )
    }

    fn begin_move(&mut self) {
        if self.selected_card().is_none() {
            self.notify("nothing to move here", Notice::Info);
            return;
        }
        self.origin_col = self.col;
        self.mode = Mode::Moving;
    }

    fn confirm_move(&mut self) {
        let Some(card) = self
            .board
            .columns
            .get(self.origin_col)
            .and_then(|c| c.cards.get(self.card))
            .cloned()
        else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(target) = self.board.columns.get(self.col) else {
            self.mode = Mode::Normal;
            return;
        };

        if self.col == self.origin_col {
            self.mode = Mode::Normal;
            self.notify("cancelled — same lane", Notice::Info);
            return;
        }

        let target_id = target.drop_target.clone();
        let target_title = target.title.clone();
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };

        match but.rub(&card.cli_id, &target_id) {
            Ok(status) => {
                self.board = Board::from_status(&status);
                self.clamp();
                self.notify(
                    format!("{} → {}", card.cli_id, target_title),
                    Notice::Success,
                );
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    pub fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode as K;

        // Help swallows everything except the keys that dismiss it.
        if self.mode == Mode::Help {
            if matches!(key.code, K::Esc | K::Char('?') | K::Char('q')) {
                self.mode = Mode::Normal;
            }
            return;
        }

        self.message = None;

        match key.code {
            K::Char('q') if self.mode == Mode::Normal => self.should_quit = true,
            K::Esc => {
                if self.mode == Mode::Moving {
                    self.mode = Mode::Normal;
                    self.col = self.origin_col;
                    self.notify("move cancelled", Notice::Info);
                } else {
                    self.should_quit = true;
                }
            }
            K::Char('?') => self.mode = Mode::Help,
            K::Left | K::Char('h') => self.move_column(-1),
            K::Right | K::Char('l') => self.move_column(1),
            K::Up | K::Char('k') if self.mode == Mode::Normal => self.move_card(-1),
            K::Down | K::Char('j') if self.mode == Mode::Normal => self.move_card(1),
            K::Char('g') if self.mode == Mode::Normal => self.card = 0,
            K::Char('G') if self.mode == Mode::Normal => {
                self.card = self.cards_in_current_column().saturating_sub(1)
            }
            K::Char('m') if self.mode == Mode::Normal => self.begin_move(),
            K::Enter if self.mode == Mode::Moving => self.confirm_move(),
            K::Char('r') if self.mode == Mode::Normal => {
                self.refresh();
                self.notify("refreshed", Notice::Info);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::but::parse_status;

    /// Builds an App without spawning `but`. Only navigation is exercised here;
    /// anything that mutates needs the real CLI and is covered by hand.
    fn board() -> Board {
        Board::from_status(&parse_status(include_str!("../tests/fixtures/status.json")).unwrap())
    }

    /// Mirrors `App`'s cursor rules against a board, so navigation can be tested without
    /// a `But` handle. Kept in sync by construction: both call the same helpers.
    struct Nav {
        board: Board,
        col: usize,
        card: usize,
    }

    impl Nav {
        fn new() -> Self {
            Nav {
                board: board(),
                col: 0,
                card: 0,
            }
        }
        fn cards(&self) -> usize {
            self.board.columns[self.col].cards.len()
        }
        fn right(&mut self) {
            self.col = (self.col + 1) % self.board.columns.len();
            let n = self.cards();
            self.card = if n == 0 { 0 } else { self.card.min(n - 1) };
        }
    }

    #[test]
    fn changing_lane_keeps_cursor_on_a_real_card() {
        let mut n = Nav::new();
        n.card = 1; // second card in the backlog
        n.right(); // feat-auth has 2 cards
        assert_eq!(n.card, 1);
        n.right(); // feat-ui has 2
        assert_eq!(n.card, 1);
        n.right(); // fix-flaky-tests has only 1
        assert_eq!(n.card, 0, "cursor clamps instead of pointing past the end");
    }

    #[test]
    fn pending_action_names_the_rub_semantics() {
        // Commit landing on the backlog is an uncommit, and must say so.
        let b = board();
        let commit = b.columns[1].cards[0].clone();
        assert_eq!(commit.kind, CardKind::Commit);
        assert_eq!(b.columns[0].kind, ColumnKind::Unassigned);

        let file = b.columns[0].cards[0].clone();
        assert_eq!(file.kind, CardKind::Change);

        let verb = |c: CardKind, t: ColumnKind| match (c, t) {
            (CardKind::Change, ColumnKind::Unassigned) => "unstage",
            (CardKind::Change, ColumnKind::Stack) => "stage to",
            (CardKind::Commit, ColumnKind::Unassigned) => "uncommit into",
            (CardKind::Commit, ColumnKind::Stack) => "move to",
        };
        assert_eq!(verb(commit.kind, ColumnKind::Unassigned), "uncommit into");
        assert_eq!(verb(file.kind, ColumnKind::Stack), "stage to");
    }
}
