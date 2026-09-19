use super::*;
use ratatui::crossterm::event::KeyCode as K;

/// A branch named in `Mode::Branch` but not yet created, waiting on
/// `Mode::HarnessMessage`'s prompt — see `App::advance_to_harness_message`.
pub(super) struct PendingBranch {
    pub(super) name: String,
    pub(super) anchor: Option<String>,
}

/// Which row of the branch-creation modal `Left`/`Right`/`Up`/`Down` currently act on —
/// see `App::branch_modal_row_down`/`_up`. Top-to-bottom order matches the modal's own
/// layout, `Split` included only when `App::branch_modal_split_row_visible` says it's
/// shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchModalRow {
    Name,
    Action,
    Split,
}

/// How the branch-naming/harness-message flow is presented: a dedicated modal (the
/// default) with the name, the pending action, and the message all visible together,
/// closer to a confirm dialog — or squeezed into the one-line footer everything else in
/// the app uses, for `KANSTACK_BRANCH_UI=footer`. Configured once at startup — see
/// `main.rs`'s `--help` — rather than a runtime toggle, on the theory that this is a
/// standing preference about how you like to work rather than something worth switching
/// mid-session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchUi {
    Footer,
    Modal,
}

impl BranchUi {
    pub(super) fn from_env() -> Self {
        match std::env::var("KANSTACK_BRANCH_UI").as_deref() {
            Ok("footer") => BranchUi::Footer,
            _ => BranchUi::Modal,
        }
    }
}

impl App {

    /// Starts naming a new branch.
    ///
    /// Defaults to stacking when a lane is selected and to a parallel lane otherwise,
    /// because that is what pressing the key *there* most likely means. Tab overrides it,
    /// so neither choice requires navigating somewhere else first.
    pub(super) fn begin_branch(&mut self) {
        self.branch_input.clear();
        self.harness_message_input.clear();
        self.stack_onto = self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.clone());
        self.open_harness = true;
        self.branch_modal_row = BranchModalRow::Name;
        self.mode = Mode::Branch;
    }

    /// Flips between stacking on the selected lane and creating a parallel one.
    pub(super) fn toggle_stack_onto(&mut self) {
        if self.stack_onto.is_some() {
            self.stack_onto = None;
        } else {
            self.stack_onto = self
                .board
                .columns
                .get(self.col)
                .and_then(|c| c.branch_name.clone());
            if self.stack_onto.is_none() {
                self.notify("the backlog has no branch to stack on", Notice::Info);
            }
        }
    }

    /// Opts a parallel lane out of its harness split for this one branch, without touching
    /// the standing `KANSTACK_CMUX_*`/`KANSTACK_TMUX_*` config. No-op while stacking, since
    /// a stacked branch never opens one to opt out of.
    pub(super) fn toggle_open_harness(&mut self) {
        if self.splitter.is_none() {
            self.notify("no harness-split backend found (cmux or tmux)", Notice::Info);
            return;
        }
        if self.stack_onto.is_some() {
            return;
        }
        self.open_harness = !self.open_harness;
    }

    /// Whether the modal's split-checkbox row is shown at all right now — a stacked branch
    /// never opens its own split (see `toggle_open_harness`), so the row that would toggle
    /// it is left out entirely rather than shown disabled. Shared between the row
    /// navigation below and `draw_branch_modal`'s own layout, so the two can't drift apart.
    pub fn branch_modal_split_row_visible(&self) -> bool {
        self.splitter_available() && self.stack_onto.is_none()
    }

    /// `Down` in the name step: moves the row cursor along Name → Action → Split (skipping
    /// Split when `branch_modal_split_row_visible` says it isn't shown) → on to the
    /// optional message step, the same top-to-bottom order the modal itself renders in.
    pub(super) fn branch_modal_row_down(&mut self) {
        self.branch_modal_row = match self.branch_modal_row {
            BranchModalRow::Name => BranchModalRow::Action,
            BranchModalRow::Action if self.branch_modal_split_row_visible() => BranchModalRow::Split,
            BranchModalRow::Action | BranchModalRow::Split => {
                self.advance_to_harness_message();
                return;
            }
        };
    }

    /// `Up` in the name step: the reverse of `branch_modal_row_down`, one row at a time —
    /// a no-op already at the top row.
    pub(super) fn branch_modal_row_up(&mut self) {
        self.branch_modal_row = match self.branch_modal_row {
            BranchModalRow::Split => BranchModalRow::Action,
            BranchModalRow::Action | BranchModalRow::Name => BranchModalRow::Name,
        };
    }

    /// `Left`/`Right` on whichever row is focused: moves the name field's text cursor, or
    /// toggles the action/split row exactly as `Tab`/`Shift-Tab` already do — direction
    /// doesn't matter for a two-state toggle, so both keys reach the same handler.
    pub(super) fn branch_modal_row_left_right(&mut self, left: bool) {
        match self.branch_modal_row {
            BranchModalRow::Name if left => self.branch_input.move_left(),
            BranchModalRow::Name => self.branch_input.move_right(),
            BranchModalRow::Action => self.toggle_stack_onto(),
            BranchModalRow::Split => self.toggle_open_harness(),
        }
    }

    /// What `b` will do, in the same spirit as the move footer: say it before doing it.
    pub fn pending_branch_action(&self) -> String {
        match &self.stack_onto {
            Some(anchor) => format!("stack on {anchor}"),
            None if self.splitter.is_some() && self.open_harness => {
                format!("new parallel lane · opens {}", self.splitter.as_ref().unwrap().label())
            }
            None if self.splitter.is_some() => "new parallel lane · no split".to_string(),
            None => "new parallel lane".to_string(),
        }
    }

    /// The stack-vs-parallel half of `pending_branch_action`, without the split clause —
    /// for the modal, which shows that clause as its own checkbox row instead of folding
    /// it into this sentence the way the single-line footer prompt has to.
    pub fn pending_branch_target(&self) -> String {
        match &self.stack_onto {
            Some(anchor) => format!("stack on {anchor}"),
            None => "new parallel lane".to_string(),
        }
    }

    /// Whether naming a branch right now has an optional initial-harness-message step to
    /// offer (`Down` on the name field, see `advance_to_harness_message`) — and, from
    /// `confirm_branch`, whether Enter there should open the harness too even without one.
    /// True only for a parallel lane (a stacked one shares its base's tab) that hasn't
    /// opted out of a split with shift-tab, and only when a split backend is actually
    /// configured at all.
    pub fn will_prompt_for_harness_message(&self) -> bool {
        self.stack_onto.is_none() && self.open_harness && self.splitter.is_some()
    }

    /// The branch name to show in the branch-creation modal: the live `branch_input`
    /// while still typing it, or the name already handed off to `pending_branch` once
    /// past that step and prompting for a harness message instead.
    pub fn branch_modal_name(&self) -> &str {
        match &self.pending_branch {
            Some(p) => p.name.as_str(),
            None => self.branch_input.as_str(),
        }
    }

    /// Enter's one job on the name field: create the branch right now. `Down`
    /// (`advance_to_harness_message`) is the way to write an initial harness message —
    /// keeping Enter here from ever meaning "navigate" instead of "create" is the reason
    /// that split exists.
    pub(super) fn confirm_branch(&mut self) {
        let name = self.branch_input.trimmed();
        if name.is_empty() {
            self.notify("a branch needs a name", Notice::Info);
            return;
        }
        let anchor = self.stack_onto.clone();
        let open_harness = self.will_prompt_for_harness_message();
        // The modal shows a message kept from an earlier visit to the message step (see
        // `back_to_branch_name`) right there on the name step, so Enter uses it rather than
        // silently dropping what's on screen. The footer has nowhere to show it, so there
        // Enter stays "create with no message".
        let message = (open_harness && self.branch_ui == BranchUi::Modal)
            .then(|| self.harness_message_input.trimmed())
            .filter(|m| !m.is_empty());
        self.branch_input.clear();
        self.harness_message_input.clear();
        self.mode = Mode::Normal;
        self.create_branch(&name, anchor.as_deref(), message.as_deref(), open_harness);
    }

    /// The last step of `branch_modal_row_down`'s descent through the name step's rows:
    /// moves on to the optional initial-message step instead of creating the branch. A
    /// no-op when there's no such step (stacking, or no split backend to open) — nothing to
    /// navigate to.
    ///
    /// Nothing is created yet — `but branch new` doesn't run until
    /// `confirm_harness_message` or `confirm_branch`. Deferring it means `Esc` on the
    /// message prompt is a real, no-side-effect cancel of the *whole* branch instead of a
    /// branch that already exists needing to be dealt with one way or another.
    pub(super) fn advance_to_harness_message(&mut self) {
        if !self.will_prompt_for_harness_message() {
            return;
        }
        let name = self.branch_input.trimmed();
        if name.is_empty() {
            // Drop the row cursor back on the name field — that's what the notice is
            // asking the user to go fix.
            self.branch_modal_row = BranchModalRow::Name;
            self.notify("a branch needs a name", Notice::Info);
            return;
        }
        let anchor = self.stack_onto.clone();
        self.branch_input.clear();
        self.pending_branch = Some(PendingBranch { name, anchor });
        self.mode = Mode::HarnessMessage;
    }

    /// `Up` on the message step: the reverse of `advance_to_harness_message` — restores
    /// the name and stack target to the name field and goes back to `Mode::Branch`, row
    /// cursor on the last row before the message step, without creating anything (unlike
    /// `Esc` here, which cancels the branch outright). Whatever was typed as the message
    /// is kept, so `Down` again finds it where it was left — navigating between fields
    /// must never throw away what's in them; `begin_branch` and the cancel/confirm paths
    /// are what reset it.
    pub(super) fn back_to_branch_name(&mut self) {
        let Some(pending) = self.pending_branch.take() else { return };
        self.branch_input.set(pending.name);
        self.stack_onto = pending.anchor;
        self.mode = Mode::Branch;
        self.branch_modal_row = if self.branch_modal_split_row_visible() {
            BranchModalRow::Split
        } else {
            BranchModalRow::Action
        };
    }

    /// Finishes the harness-message prompt `advance_to_harness_message` hands off to for a
    /// parallel lane that wants a harness: creates the branch (nothing exists until now —
    /// see `advance_to_harness_message`) and spawns its harness, folding in `text` as its
    /// initial message when non-empty.
    pub(super) fn confirm_harness_message(&mut self) {
        let text = self.harness_message_input.trimmed();
        self.harness_message_input.clear();
        self.mode = Mode::Normal;
        let Some(pending) = self.pending_branch.take() else {
            return;
        };
        let message = if text.is_empty() { None } else { Some(text.as_str()) };
        self.create_branch(&pending.name, pending.anchor.as_deref(), message, true);
    }

    /// Creates `name` via `but branch new`, rebuilds the board, and — when `open_harness`
    /// is set — spawns its split pane, optionally seeded with `initial_message`. Shared by
    /// `confirm_branch` (Enter on the name field — a stacked branch, or a parallel one
    /// created with no message) and `confirm_harness_message` (Enter after `Down` opted
    /// into typing one).
    pub(super) fn create_branch(&mut self, name: &str, anchor: Option<&str>, initial_message: Option<&str>, open_harness: bool) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        // Captured up front, as an owned path, so `but` (borrowed from `self.but`) does
        // not need to stay alive past `board_from` below — its last use there is what
        // lets the borrow checker treat the `&mut self` calls after it as fine.
        let cwd = but.cwd().to_path_buf();
        match but.branch_new(name, anchor) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(
                    match anchor {
                        Some(a) => format!("created {name} on top of {a}"),
                        None => format!("created {name}"),
                    },
                    Notice::Success,
                );
                // Put the cursor on what was just made, so it can be worked with at once.
                if let Some(i) = self
                    .board
                    .columns
                    .iter()
                    .position(|c| c.branch_name.as_deref() == Some(name))
                {
                    self.col = i;
                    self.card = 0;
                }
                if open_harness {
                    if let Some(splitter) = &mut self.splitter {
                        let label = splitter.label();
                        match splitter.spawn_harness(&cwd, name, initial_message) {
                            Ok(()) => self.notify(format!("created {name} — harness open"), Notice::Success),
                            Err(e) => self.notify(format!("{label}: {e}"), Notice::Error),
                        }
                    }
                }
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }
}

impl App {
    pub(super) fn handle_key_branch(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        // Row navigation (`Up`/`Down` between name/action/split, `Left`/`Right` toggling
        // whichever row is focused) only makes sense where a row cursor is actually
        // drawn — the modal. The footer is one line with nowhere to show it, so there
        // `Left`/`Right` stay plain text-cursor movement and `Down` goes straight to
        // the message step the way it always has, same as before row navigation
        // existed.
        let modal = self.branch_ui == BranchUi::Modal;
        let on_name_row = !modal || self.branch_modal_row == BranchModalRow::Name;
        match key.code {
            K::Esc => {
                self.mode = Mode::Normal;
                self.branch_input.clear();
                self.notify("branch cancelled", Notice::Info);
            }
            K::Enter => self.confirm_branch(),
            K::Down if modal => self.branch_modal_row_down(),
            K::Down => self.advance_to_harness_message(),
            K::Up if modal => self.branch_modal_row_up(),
            K::Left if modal => self.branch_modal_row_left_right(true),
            K::Right if modal => self.branch_modal_row_left_right(false),
            K::Left => self.branch_input.move_left(),
            K::Right => self.branch_input.move_right(),
            K::Tab => self.toggle_stack_onto(),
            K::BackTab => self.toggle_open_harness(),
            _ if on_name_row => {
                self.branch_input.handle_key(key);
            }
            _ => {}
        }
    }

    pub(super) fn handle_key_harness_message(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            // Nothing has touched `but` yet at this point — see
            // `advance_to_harness_message` — so this cancels branch creation outright,
            // the same as `Esc` does from `Mode::Branch` itself, rather than creating
            // the branch anyway with no message. `Up` is the non-cancelling way back —
            // it restores the name field instead of dropping it.
            K::Esc => {
                self.pending_branch = None;
                self.harness_message_input.clear();
                self.mode = Mode::Normal;
                self.notify("branch cancelled", Notice::Info);
            }
            K::Enter => self.confirm_harness_message(),
            K::Up => self.back_to_branch_name(),
            _ => {
                self.harness_message_input.handle_key(key);
            }
        }
    }
}
