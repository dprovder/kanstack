//! The ask inbox: one place, reachable from anywhere on the board with `i`, listing every
//! lane whose orchestrator is waiting on a human (`kanstack ask`) — see `Mode::Asks`.
//!
//! Deliberately one global list rather than something per lane: a question that needs you
//! is exactly the thing that shouldn't depend on having scrolled to the right lane to
//! notice, which is why the lane itself only carries a glyph (`ui::board`) and the header a
//! count (`ui::header`). An intent, by contrast, is informational — nobody is waiting on
//! it — so it never appears here at all.
//!
//! The board only relays: `Enter` jumps to the lane the same way ordinary lane navigation
//! would, and `a` answers inline by calling `Orchestration::answer` directly (in-process —
//! this is the TUI, not a wrapper around the CLI), exactly as `kanstack answer` would. What
//! the answer *means*, and what happens next, is entirely the orchestrator's business.

use super::*;
use crate::orchestration::{Orchestration, PendingAsk};
use ratatui::crossterm::event::KeyCode as K;

impl App {
    /// Every branch on the board with a pending ask, in board order — lanes left to right,
    /// and within a stacked lane tip first — as `(column index, branch, ask)`. What the inbox
    /// lists and the header counts. Read off the board's own sections (filled in by
    /// `sync_orchestration`) rather than the registry, so every entry is somewhere `Enter`
    /// can actually jump to; an ask raised on a branch that isn't applied in the workspace
    /// has no lane to show it in, and is left to `kanstack status` to report.
    pub fn pending_asks(&self) -> Vec<(usize, &str, &PendingAsk)> {
        self.board
            .columns
            .iter()
            .enumerate()
            .flat_map(|(i, col)| {
                col.sections
                    .iter()
                    .filter_map(move |s| Some((i, s.name.as_str(), s.pending_ask.as_ref()?)))
            })
            .collect()
    }

    /// `i`: opens the inbox, re-reading every ask first so it never shows a list older than
    /// the keypress (asks arrive from other processes, and nothing else on the board need
    /// have changed since the last sync). With nothing pending there's nothing to open — a
    /// notice says so instead of an empty dialog.
    pub(super) fn begin_asks(&mut self) {
        self.sync_orchestration();
        if self.pending_asks().is_empty() {
            self.notify("no pending asks", Notice::Info);
            return;
        }
        self.ask_sel = 0;
        self.ask_answering = None;
        self.ask_answer_input.clear();
        self.mode = Mode::Asks;
    }

    /// `↑`/`↓` in the inbox, wrapping at either end like lane and card movement do.
    fn move_ask_selection(&mut self, delta: isize) {
        let n = self.pending_asks().len() as isize;
        if n == 0 {
            return;
        }
        self.ask_sel = (self.ask_sel as isize + delta).rem_euclid(n) as usize;
    }

    /// `Enter` in the inbox: closes it and puts the board cursor on the selected ask's lane —
    /// the same `col`/`card` an ordinary `←`/`→` would land on, so everything that acts on
    /// "this lane" (`t`, `p`, `⏎`…) acts on it next. For a branch stacked below a lane's tip,
    /// the card cursor goes to that branch's own first commit, so the lane scrolls to where
    /// its `○` header (and glyph) actually are rather than leaving it off-screen.
    pub(super) fn jump_to_ask(&mut self) {
        let Some((col, branch)) = self.pending_asks().get(self.ask_sel).map(|(c, b, _)| (*c, b.to_string())) else {
            return;
        };
        let column = &self.board.columns[col];
        let is_tip = column.sections.first().is_some_and(|s| s.name == branch);
        self.col = col;
        self.card = if is_tip {
            0
        } else {
            column.cards.iter().position(|c| c.group.as_deref() == Some(branch.as_str())).unwrap_or(0)
        };
        self.target_card = None;
        self.mode = Mode::Normal;
    }

    /// `a` in the inbox: starts typing an answer to the selected ask, in place.
    pub(super) fn begin_answer(&mut self) {
        let Some(branch) = self.pending_asks().get(self.ask_sel).map(|(_, b, _)| b.to_string()) else {
            return;
        };
        self.ask_answer_input.clear();
        self.ask_answering = Some(branch);
    }

    /// `Enter` while answering: `Orchestration::answer(branch, text, now)`, exactly what
    /// `kanstack answer` does, then a re-sync so the answered ask leaves the list (and its
    /// lane's glyph, and the header count) straight away. An empty answer is refused rather
    /// than sent, same as the CLI requiring its text argument. The inbox stays open while
    /// anything is still pending, so a run of questions can be worked through in one go, and
    /// closes itself once the last one is answered.
    ///
    /// An ask that turns out to be gone already — answered from another terminal, or its
    /// lane stopped — while this one was being typed is reported as such, not as an error:
    /// the outcome someone wanted (nothing pending) is already true.
    pub(super) fn confirm_answer(&mut self) {
        let text = self.ask_answer_input.trimmed();
        if text.is_empty() {
            self.notify("an answer needs some text", Notice::Info);
            return;
        }
        let Some(branch) = self.ask_answering.take() else { return };
        self.ask_answer_input.clear();
        match &self.repo {
            None => self.notify("snapshot is read-only", Notice::Info),
            Some(repo) => match Orchestration::for_repo(repo).answer(&branch, &text, std::time::SystemTime::now()) {
                Ok(Some(_)) => self.notify(format!("answered {branch}"), Notice::Success),
                Ok(None) => self.notify(format!("{branch} had no pending ask — already answered?"), Notice::Info),
                Err(e) => self.notify(format!("answering {branch}: {e}"), Notice::Error),
            },
        }
        self.sync_orchestration();
        let left = self.pending_asks().len();
        if left == 0 {
            self.mode = Mode::Normal;
        } else {
            self.ask_sel = self.ask_sel.min(left - 1);
        }
    }
}

impl App {
    pub(super) fn handle_key_asks(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        // Typing an answer: every key but the two that leave the field goes into it, so `a`,
        // `i`, `j`, `k` and `q` are all just letters here.
        if self.ask_answering.is_some() {
            match key.code {
                // Back to the list, not out of the inbox — nothing has been sent yet.
                K::Esc => {
                    self.ask_answering = None;
                    self.ask_answer_input.clear();
                }
                K::Enter => self.confirm_answer(),
                _ => {
                    self.ask_answer_input.handle_key(key);
                }
            }
            return;
        }
        self.message = None;
        match key.code {
            K::Esc | K::Char('i') | K::Char('q') => self.mode = Mode::Normal,
            K::Up | K::Char('k') => self.move_ask_selection(-1),
            K::Down | K::Char('j') => self.move_ask_selection(1),
            K::Enter => self.jump_to_ask(),
            K::Char('a') => self.begin_answer(),
            _ => {}
        }
    }
}
