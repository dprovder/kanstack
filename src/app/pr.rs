use super::*;
use crate::but::PrOptions;
use ratatui::crossterm::event::KeyCode as K;

/// Which row of the PR modal `Up`/`Down` currently act on, and `Left`/`Right`/`Tab` toggle
/// on the `Draft` row — same shape as `BranchModalRow`, just for the three fields `but pr
/// new` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrModalRow {
    Title,
    /// The description, typed as one field that wraps downward across as many lines as it
    /// needs — the same treatment `draw_branch_modal` already gives its own
    /// `harness_message_input`, reusing `wrap_ranges` rather than a new multi-line-aware
    /// input: there is still no real newline in either one, just word-wrapped display over
    /// one flat string.
    Message,
    Draft,
}

/// A `but pr new` call running on a background thread, started by `confirm_pr`.
///
/// Opening a review pushes the branch first if it needs pushing, then calls out to
/// whatever forge is configured — a real network round trip, same reasoning as
/// [`PendingLand`] for why this doesn't run on the main thread.
pub struct PendingPr {
    pub title: String,
    pub(crate) rx: mpsc::Receiver<Result<WorkspaceStatus>>,
    /// Advanced once per poll so the UI can animate a spinner without its own clock.
    pub spinner: usize,
}

impl App {

    /// Starts filling out a PR for the selected lane's branch: clears the title and
    /// description fields and the draft toggle back to their defaults and opens the modal.
    /// Nothing touches `but` until `confirm_pr` — this only gathers what it needs.
    pub(super) fn begin_pr(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to open a PR for — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        if self.but.is_none() {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        }
        self.pr_target = Some((branch, title));
        self.pr_title_input.clear();
        self.pr_message_input.clear();
        self.pr_draft = false;
        self.pr_modal_row = PrModalRow::Title;
        self.mode = Mode::PrModal;
    }

    /// The lane title the PR modal is open for, valid while `mode == PrModal` — for the
    /// modal's own heading.
    pub fn pr_target_label(&self) -> Option<&str> {
        self.pr_target.as_ref().map(|(_, title)| title.as_str())
    }

    pub(super) fn toggle_pr_draft(&mut self) {
        self.pr_draft = !self.pr_draft;
    }

    /// `Down` in the PR modal: Title → Message → Draft, the same top-to-bottom order the
    /// modal draws in — a no-op already on `Draft`.
    pub(super) fn pr_modal_row_down(&mut self) {
        self.pr_modal_row = match self.pr_modal_row {
            PrModalRow::Title => PrModalRow::Message,
            PrModalRow::Message | PrModalRow::Draft => PrModalRow::Draft,
        };
    }

    /// `Up` in the PR modal: the reverse of `pr_modal_row_down`.
    pub(super) fn pr_modal_row_up(&mut self) {
        self.pr_modal_row = match self.pr_modal_row {
            PrModalRow::Draft => PrModalRow::Message,
            PrModalRow::Message | PrModalRow::Title => PrModalRow::Title,
        };
    }

    /// `Left`/`Right` on whichever row is focused: moves that field's text cursor, or
    /// toggles the draft row exactly as `Tab` does — direction doesn't matter for a
    /// two-state toggle, so both keys reach the same handler.
    pub(super) fn pr_modal_row_left_right(&mut self, left: bool) {
        match self.pr_modal_row {
            PrModalRow::Title if left => self.pr_title_input.move_left(),
            PrModalRow::Title => self.pr_title_input.move_right(),
            PrModalRow::Message if left => self.pr_message_input.move_left(),
            PrModalRow::Message => self.pr_message_input.move_right(),
            PrModalRow::Draft => self.toggle_pr_draft(),
        }
    }

    /// Starts `but pr new` on a background thread — see [`PendingPr`] — using whatever the
    /// modal gathered. An empty title with a non-empty description is refused rather than
    /// silently dropping what was typed: `but pr new -m` needs a first line to be the title,
    /// so there is nowhere for a title-less description to go. Empty title *and* empty
    /// description falls back to `None`, which `But::pr_new` turns into `--default`.
    pub(super) fn confirm_pr(&mut self) {
        let title = self.pr_title_input.trimmed();
        let message = self.pr_message_input.trimmed();
        if title.is_empty() && !message.is_empty() {
            self.pr_modal_row = PrModalRow::Title;
            self.notify("a PR needs a title before a description", Notice::Info);
            return;
        }

        let Some((branch, lane_title)) = self.pr_target.take() else {
            self.mode = Mode::Normal;
            return;
        };
        let Some(but) = self.but.clone() else {
            self.mode = Mode::Normal;
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        let full_message = match (title.is_empty(), message.is_empty()) {
            (true, true) => None,
            (false, true) => Some(title),
            (false, false) => Some(format!("{title}\n\n{message}")),
            (true, false) => unreachable!("checked above"),
        };
        let opts = PrOptions {
            title: full_message,
            draft: self.pr_draft,
        };
        self.pr_title_input.clear();
        self.pr_message_input.clear();

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(but.pr_new(&branch, &opts));
        });
        self.pr_running = Some(PendingPr { title: lane_title, rx, spinner: 0 });
        self.mode = Mode::PrRunning;
    }

    /// Non-blocking check on a `but pr new` running on a background thread. Call this every
    /// tick of the event loop, the same as `poll_land`.
    pub fn poll_pr(&mut self) {
        let Some(pending) = self.pr_running.as_mut() else {
            return;
        };
        pending.spinner = pending.spinner.wrapping_add(1);
        match pending.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pr_running = None;
                self.mode = Mode::Normal;
                self.notify("PR thread vanished unexpectedly", Notice::Error);
            }
            Ok(Ok(status)) => {
                let title = pending.title.clone();
                self.pr_running = None;
                self.mode = Mode::Normal;
                if let Some(but) = self.but.clone() {
                    self.board = Self::board_from(&but, &mut self.commit_stats, &status);
                }
                self.clamp();
                self.notify(format!("opened a PR for {title}"), Notice::Success);
            }
            Ok(Err(e)) => {
                self.pr_running = None;
                self.mode = Mode::Normal;
                self.notify(format!("{e}"), Notice::Error);
            }
        }
        self.check_tutorial_advance();
    }
}

impl App {
    pub(super) fn handle_key_pr_modal(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Esc => {
                self.mode = Mode::Normal;
                self.pr_title_input.clear();
                self.pr_message_input.clear();
                self.pr_target = None;
                self.notify("PR cancelled", Notice::Info);
            }
            K::Enter => self.confirm_pr(),
            K::Down => self.pr_modal_row_down(),
            K::Up => self.pr_modal_row_up(),
            K::Left => self.pr_modal_row_left_right(true),
            K::Right => self.pr_modal_row_left_right(false),
            K::Tab => self.toggle_pr_draft(),
            K::Backspace if self.pr_modal_row == PrModalRow::Title => self.pr_title_input.backspace(),
            K::Delete if self.pr_modal_row == PrModalRow::Title => self.pr_title_input.delete_forward(),
            K::Home if self.pr_modal_row == PrModalRow::Title => self.pr_title_input.move_home(),
            K::End if self.pr_modal_row == PrModalRow::Title => self.pr_title_input.move_end(),
            K::Char(c) if self.pr_modal_row == PrModalRow::Title => self.pr_title_input.insert(c),
            K::Backspace if self.pr_modal_row == PrModalRow::Message => self.pr_message_input.backspace(),
            K::Delete if self.pr_modal_row == PrModalRow::Message => self.pr_message_input.delete_forward(),
            K::Home if self.pr_modal_row == PrModalRow::Message => self.pr_message_input.move_home(),
            K::End if self.pr_modal_row == PrModalRow::Message => self.pr_message_input.move_end(),
            K::Char(c) if self.pr_modal_row == PrModalRow::Message => self.pr_message_input.insert(c),
            _ => {}
        }
    }

    /// Nothing to do here but wait, same as `handle_key_landing` — `poll_pr` (driven by the
    /// event loop, not a key) is what moves this out of `Landing`. No cancel: by the time
    /// the spinner is up, `but pr new` may already be pushing.
    pub(super) fn handle_key_pr_running(&mut self, _key: ratatui::crossterm::event::KeyEvent) {
    }
}
