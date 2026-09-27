use super::*;
use crate::harness::Effort;
use ratatui::crossterm::event::KeyCode as K;

/// A branch named in `Mode::Branch` but not yet created, waiting on
/// `Mode::HarnessMessage`'s prompt — see `App::advance_to_harness_message`. Carries the
/// harness/model/effort/advisory choice too, gathered on the name step same as the branch name and
/// stack target, so `App::confirm_harness_message` has them once the branch is actually
/// created.
pub(super) struct PendingBranch {
    pub(super) name: String,
    pub(super) anchor: Option<String>,
    pub(super) harness: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effort: Option<Effort>,
    pub(super) advisory: bool,
}

/// Which row of the branch-creation modal `Left`/`Right`/`Up`/`Down` currently act on —
/// see `App::branch_modal_row_down`/`_up`. Top-to-bottom order matches the modal's own
/// layout, `Split`/`Harness`/`Model`/`Effort`/`Advisory` included only when
/// `App::branch_modal_split_row_visible` says they're shown — all of them are meaningless
/// without a split backend to hand a model or an effort hint to (or, for `Advisory`, a pane
/// whose workstream there'd be anything to record about) in the first place, the same
/// reasoning `Split` already followed alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchModalRow {
    Name,
    Action,
    Split,
    /// A hybrid of the two other row shapes, same as `crate::setup`'s own harness field:
    /// `Left`/`Right` cycles through whatever's actually on `PATH` (see
    /// `App::cycle_harness_choice`, `crate::setup::detect_harnesses`), and typing overrides
    /// it with anything else — a harness kanstack doesn't know about, or one not on `PATH`
    /// in this shell. Blank means "no `--agent` override", same as `kanstack spawn` without
    /// the flag.
    Harness,
    /// A free-text field, same shape as `Name` — the model name is an open string, not a
    /// fixed set of choices in general, so typing is always how you can set one. When the
    /// picked harness happens to have real suggestions to offer (today: Codex, via its own
    /// locally cached `models_cache.json` — see `App::model_choices`), `Left`/`Right` cycles
    /// through those instead of moving the text cursor, the same hybrid `Harness` already
    /// uses; with nothing to suggest, `Left`/`Right` falls back to ordinary cursor movement.
    Model,
    /// A small fixed-choice picker, cycled with `Left`/`Right` through no hint at all, then
    /// `low` → `medium` → `high` — the same two-key cycling `Action`/`Split` already use,
    /// just with four states instead of two.
    Effort,
    /// A checkbox, flipped with `Left`/`Right` or `space` — the same two-state toggle
    /// `Split` is, for `kanstack spawn --advisory`: whether the workstream is recorded as
    /// one not expected to produce commits (see `crate::workstream::Workstream::advisory`).
    /// Changes nothing about how the pane is spawned; only how the board renders the lane.
    Advisory,
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

/// A text field's value, trimmed, or `None` if that leaves nothing — the modal's own "blank
/// means no override given" rule, shared by every optional free-text field it collects
/// (`model_input`, `harness_input`) so they can't drift on what counts as blank.
fn trimmed_or_none(input: &crate::text_input::TextInput) -> Option<String> {
    Some(input.trimmed()).filter(|s| !s.is_empty())
}

impl App {

    /// Starts naming a new branch.
    ///
    /// Defaults to stacking when a lane is selected and to a parallel lane otherwise,
    /// because that is what pressing the key *there* most likely means. Tab overrides it,
    /// so neither choice requires navigating somewhere else first.
    ///
    /// The split checkbox defaults on for a parallel lane, same as always, but off for a
    /// stacked one — stacking still means "the same lane of work" far more often than "hand
    /// this off to another agent", so the long-standing "stacking never opens a pane"
    /// behavior stays the default; shift-tab opts in per branch.
    pub(super) fn begin_branch(&mut self) {
        self.branch_input.clear();
        self.harness_message_input.clear();
        self.harness_input.clear();
        self.harness_choices = crate::setup::detect_harnesses();
        self.model_input.clear();
        self.effort = None;
        self.advisory = false;
        self.stack_onto = self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.clone());
        self.open_harness = self.stack_onto.is_none();
        self.branch_modal_row = BranchModalRow::Name;
        self.branch_name_missing = false;
        self.mode = Mode::Branch;
    }

    /// Refuses to go on without a branch name: says so in the footer notice *and* flags
    /// the name field itself (see `branch_name_missing`), putting the row cursor back on it
    /// since that is what needs fixing.
    fn refuse_missing_name(&mut self) {
        self.branch_modal_row = BranchModalRow::Name;
        self.branch_name_missing = true;
        self.notify("a branch needs a name", Notice::Info);
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

    /// Opts a branch — stacked or parallel — in or out of opening its own harness pane,
    /// without touching the standing `KANSTACK_CMUX_*`/`KANSTACK_TMUX_*` config. A stacked
    /// branch whose base already has a pane open groups with it (see
    /// `Splitter::spawn_stacked_harness_with`) rather than sharing it outright the way it
    /// used to unconditionally; one whose base has none, or that isn't stacked at all, just
    /// opens a plain one, same as a parallel lane always has.
    pub(super) fn toggle_open_harness(&mut self) {
        if self.splitter.is_none() {
            self.notify("no harness-split backend found (cmux, tmux, orca or ghostty)", Notice::Info);
            return;
        }
        self.open_harness = !self.open_harness;
    }

    /// Whether the modal's split-checkbox row is shown at all right now. Shared between the
    /// row navigation below and `draw_branch_modal`'s own layout, so the two can't drift
    /// apart.
    pub fn branch_modal_split_row_visible(&self) -> bool {
        self.splitter_available()
    }

    /// `Down` in the name step: moves the row cursor along Name → Action → Split → Harness →
    /// Model → Effort → Advisory (skipping everything after Action together when
    /// `branch_modal_split_row_visible` says they aren't shown — there's no split backend to
    /// hand any of them to) → on to the optional message step, the same top-to-bottom order
    /// the modal itself renders in.
    pub(super) fn branch_modal_row_down(&mut self) {
        self.branch_modal_row = match self.branch_modal_row {
            BranchModalRow::Name => BranchModalRow::Action,
            BranchModalRow::Action if self.branch_modal_split_row_visible() => BranchModalRow::Split,
            BranchModalRow::Action => {
                self.advance_to_harness_message();
                return;
            }
            BranchModalRow::Split => BranchModalRow::Harness,
            BranchModalRow::Harness => BranchModalRow::Model,
            BranchModalRow::Model => BranchModalRow::Effort,
            BranchModalRow::Effort => BranchModalRow::Advisory,
            BranchModalRow::Advisory => {
                self.advance_to_harness_message();
                return;
            }
        };
    }

    /// `Up` in the name step: the reverse of `branch_modal_row_down`, one row at a time —
    /// a no-op already at the top row.
    pub(super) fn branch_modal_row_up(&mut self) {
        self.branch_modal_row = match self.branch_modal_row {
            BranchModalRow::Advisory => BranchModalRow::Effort,
            BranchModalRow::Effort => BranchModalRow::Model,
            BranchModalRow::Model => BranchModalRow::Harness,
            BranchModalRow::Harness => BranchModalRow::Split,
            BranchModalRow::Split => BranchModalRow::Action,
            BranchModalRow::Action | BranchModalRow::Name => BranchModalRow::Name,
        };
    }

    /// `Left`/`Right` on whichever row is focused: moves the name/model field's text cursor,
    /// toggles the action/split/advisory row exactly as `Tab`/`Shift-Tab`/`space` already do
    /// (direction doesn't matter for a two-state toggle, so both keys reach the same
    /// handler), or steps the harness/effort picker one way or the other (see
    /// `cycle_harness_choice`/`cycle_effort`, where direction does matter). `Harness` never
    /// moves a text cursor on `Left`/`Right` — same as `crate::setup`'s own harness field,
    /// those two keys are reserved for cycling there, and typing still edits the field by
    /// other keys. `Model` follows suit only when it actually has something to cycle through
    /// (`model_choices`) — with nothing to suggest, it's cursor movement like `Name`.
    pub(super) fn branch_modal_row_left_right(&mut self, left: bool) {
        match self.branch_modal_row {
            BranchModalRow::Name if left => self.branch_input.move_left(),
            BranchModalRow::Name => self.branch_input.move_right(),
            BranchModalRow::Action => self.toggle_stack_onto(),
            BranchModalRow::Split => self.toggle_open_harness(),
            BranchModalRow::Harness => self.cycle_harness_choice(left),
            BranchModalRow::Model if !self.model_choices().is_empty() => self.cycle_model_choice(left),
            BranchModalRow::Model if left => self.model_input.move_left(),
            BranchModalRow::Model => self.model_input.move_right(),
            BranchModalRow::Effort => self.cycle_effort(left),
            BranchModalRow::Advisory => self.advisory = !self.advisory,
        }
    }

    /// The harness whose model catalog the picker should consult: `harness_input`, trimmed,
    /// or the configured default when it's blank — the same fallback `kanstack spawn` itself
    /// uses when `--agent`/`harness` is `None` (`$KANSTACK_HARNESS`, else `claude`). Shared by
    /// `model_choices` and the effort row's own hint, so the two can't disagree about which
    /// harness is actually in play.
    fn resolved_harness(&self) -> String {
        let typed = self.harness_input.trimmed();
        if typed.is_empty() {
            std::env::var("KANSTACK_HARNESS").unwrap_or_else(|_| "claude".to_string())
        } else {
            typed
        }
    }

    /// Model-name suggestions for whichever harness is currently picked (see
    /// `resolved_harness`) — today, only Codex has anything to offer: its own locally cached
    /// `models_cache.json` (`crate::harness::codex::cached_models`), read fresh each call
    /// rather than cached on `App` since it depends on `harness_input`, which can change on
    /// every keystroke of the Harness row — a small local file read is cheaper than keeping
    /// the two in sync by hand. Empty for every other harness, and empty for Codex too when
    /// nothing is cached (never run, or never run online, on this machine) — the model field
    /// stays a plain text field in either case, exactly as it always has.
    pub fn model_choices(&self) -> Vec<String> {
        if crate::harness::for_command(&self.resolved_harness()).id() != "codex" {
            return Vec::new();
        }
        crate::harness::codex::cached_models().into_iter().map(|m| m.slug).collect()
    }

    /// `Left`/`Right` on the model row, reachable only once `model_choices` has something to
    /// offer (see `branch_modal_row_left_right`) — steps through those suggestions exactly
    /// the way `cycle_harness_choice` steps through detected harnesses.
    pub(super) fn cycle_model_choice(&mut self, left: bool) {
        let choices = self.model_choices();
        let len = choices.len();
        if len == 0 {
            return;
        }
        let current = choices.iter().position(|m| m == self.model_input.as_str());
        let next = match (current, left) {
            (Some(i), false) => (i + 1) % len,
            (Some(i), true) => (i + len - 1) % len,
            (None, false) => 0,
            (None, true) => len - 1,
        };
        self.model_input.set(choices[next].clone());
    }

    /// Whether Codex's own locally cached catalog positively says the currently typed model
    /// does *not* support the currently picked effort — `false` (never flags anything)
    /// whenever there is nothing to check against: no effort picked, no model typed, the
    /// picked harness isn't Codex, or the cache has nothing for this exact slug (a model too
    /// new for a stale cache, or no cache at all). A hint for the effort row to show, never
    /// something that blocks picking it — kanstack has no way to be sure an unlisted slug
    /// really doesn't support a given effort, only that a *listed* one doesn't.
    pub fn effort_unsupported_by_typed_model(&self) -> bool {
        let Some(effort) = self.effort else { return false };
        let model = self.model_input.trimmed();
        if model.is_empty() || crate::harness::for_command(&self.resolved_harness()).id() != "codex" {
            return false;
        }
        let wanted = effort.to_string();
        crate::harness::codex::cached_models()
            .into_iter()
            .find(|m| m.slug == model)
            .is_some_and(|m| !m.supported_reasoning_levels.iter().any(|l| l.effort == wanted))
    }

    /// `Left`/`Right` on the harness row: steps `harness_input` to the next (or previous)
    /// harness actually found on `PATH` (`self.harness_choices`, refreshed each time
    /// `begin_branch` starts). A no-op with nothing detected. When the current value isn't
    /// one of the detected choices at all (typed in by hand, or nothing found), `Right` lands
    /// on the first choice and `Left` on the last — the exact same rule
    /// `crate::setup::Wizard::cycle_harness` uses for its own harness field, which this
    /// mirrors rather than reinventing.
    pub(super) fn cycle_harness_choice(&mut self, left: bool) {
        let len = self.harness_choices.len();
        if len == 0 {
            return;
        }
        let current = self.harness_choices.iter().position(|h| *h == self.harness_input.as_str());
        let next = match (current, left) {
            (Some(i), false) => (i + 1) % len,
            (Some(i), true) => (i + len - 1) % len,
            (None, false) => 0,
            (None, true) => len - 1,
        };
        self.harness_input.set(self.harness_choices[next]);
    }

    /// `Left`/`Right` on the effort row: cycles through no hint at all, then `low` →
    /// `medium` → `high`, wrapping at either end — the same four values `kanstack spawn
    /// --effort` accepts (absent, or one of the three words `crate::harness::Effort::parse`
    /// takes), in the order low-to-high reads.
    pub(super) fn cycle_effort(&mut self, left: bool) {
        const STATES: [Option<Effort>; 4] = [None, Some(Effort::Low), Some(Effort::Medium), Some(Effort::High)];
        let at = STATES.iter().position(|e| *e == self.effort).unwrap_or(0);
        let len = STATES.len();
        self.effort = STATES[if left { (at + len - 1) % len } else { (at + 1) % len }];
    }

    /// What `b` will do, in the same spirit as the move footer: say it before doing it.
    pub fn pending_branch_action(&self) -> String {
        let target = match &self.stack_onto {
            Some(anchor) => format!("stack on {anchor}"),
            None => "new parallel lane".to_string(),
        };
        match &self.splitter {
            Some(splitter) if self.open_harness => format!("{target} · opens {}", splitter.label()),
            Some(_) if self.stack_onto.is_none() => format!("{target} · no split"),
            _ => target,
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
    /// True for a stacked branch as much as a parallel one now, as long as the split
    /// checkbox is on (see `toggle_open_harness`; unchecked by default for a stacked
    /// branch, checked by default for a parallel one — see `begin_branch`) and a split
    /// backend is actually configured at all.
    pub fn will_prompt_for_harness_message(&self) -> bool {
        self.open_harness && self.splitter.is_some()
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
            self.refuse_missing_name();
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
        let harness = trimmed_or_none(&self.harness_input);
        let model = trimmed_or_none(&self.model_input);
        let effort = self.effort;
        let advisory = self.advisory;
        self.branch_input.clear();
        self.harness_message_input.clear();
        self.harness_input.clear();
        self.model_input.clear();
        self.mode = Mode::Normal;
        self.create_branch(
            &name,
            anchor.as_deref(),
            message.as_deref(),
            open_harness,
            harness.as_deref(),
            model.as_deref(),
            effort,
            advisory,
        );
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
            self.refuse_missing_name();
            return;
        }
        let anchor = self.stack_onto.clone();
        let harness = trimmed_or_none(&self.harness_input);
        let model = trimmed_or_none(&self.model_input);
        let effort = self.effort;
        let advisory = self.advisory;
        self.branch_input.clear();
        self.harness_input.clear();
        self.model_input.clear();
        self.pending_branch = Some(PendingBranch { name, anchor, harness, model, effort, advisory });
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
        self.harness_input.set(pending.harness.unwrap_or_default());
        self.model_input.set(pending.model.unwrap_or_default());
        self.effort = pending.effort;
        self.advisory = pending.advisory;
        self.mode = Mode::Branch;
        self.branch_modal_row = if self.branch_modal_split_row_visible() {
            BranchModalRow::Advisory
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
        self.create_branch(
            &pending.name,
            pending.anchor.as_deref(),
            message,
            true,
            pending.harness.as_deref(),
            pending.model.as_deref(),
            pending.effort,
            pending.advisory,
        );
    }

    /// Creates `name` via `but branch new`, rebuilds the board, and — when `open_harness`
    /// is set — spawns its pane, optionally seeded with `initial_message`. Shared by
    /// `confirm_branch` (Enter on the name field — a stacked or parallel branch created
    /// with no message) and `confirm_harness_message` (Enter after `Down` opted into typing
    /// one).
    ///
    /// When `anchor` (the base being stacked onto) already has a pane open, the new one
    /// groups with it (`Splitter::spawn_stacked_harness_with` — a real tab by default, or a
    /// split orthogonal to the ordinary chain direction; see `KANSTACK_STACK_PANES`) rather
    /// than sharing it outright. A dead pane doesn't count as "already has one" — nothing
    /// to group with, so it opens a plain pane instead, same as `anchor` being `None` (a
    /// parallel lane, or a stacked branch whose base never opened one) always has.
    ///
    /// `harness` overrides the configured harness for just this pane, same as `kanstack spawn
    /// --agent` — `None` (the modal's default, and every call site before the harness row
    /// existed) leaves it to `$KANSTACK_HARNESS`/the configured default, exactly as before.
    /// `model`/`effort` are the modal's own Model/Effort rows, gathered the same way as
    /// `initial_message` and forwarded exactly as `kanstack spawn --model`/`--effort` would —
    /// see `crate::harness::Harness::model_effort_args`. `None` for all three (the modal's
    /// default, and every call site before this feature existed) reproduces the launch line
    /// byte for byte. `advisory` is the modal's Advisory checkbox, recorded against the new
    /// pane's workstream once it opens (`record_advisory`) — it never touches the launch line
    /// at all, only how the board renders the lane afterwards.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create_branch(
        &mut self,
        name: &str,
        anchor: Option<&str>,
        initial_message: Option<&str>,
        open_harness: bool,
        harness: Option<&str>,
        model: Option<&str>,
        effort: Option<Effort>,
        advisory: bool,
    ) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        // Captured up front, as an owned path, so `but` (borrowed from `self.but`) does
        // not need to stay alive past `board_from` below — its last use there is what
        // lets the borrow checker treat the `&mut self` calls after it as fine.
        let cwd = but.cwd().to_path_buf();
        match but.branch_new(name, anchor.map(Placement::Above)) {
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
                        // A dead pane isn't "already has one" — nothing to group with.
                        let group_anchor = anchor
                            .filter(|a| splitter.pane_status(a) != Some(crate::mux::pane_status::PaneStatus::Dead))
                            .and_then(|a| splitter.pane_id(a));
                        let spawned = match &group_anchor {
                            Some(pane_id) => {
                                splitter.spawn_stacked_harness_with(&cwd, name, initial_message, harness, model, effort, pane_id)
                            }
                            None => splitter.spawn_harness_with(&cwd, name, initial_message, harness, model, effort),
                        };
                        match spawned {
                            Ok(pane) => {
                                crate::workstream::record_spawn(&cwd, name, &pane, harness, splitter.workspace().as_deref());
                                record_advisory(&cwd, name, advisory);
                                self.notify(format!("created {name} — harness open"), Notice::Success);
                            }
                            Err(e) => self.notify(format!("{label}: {e}"), Notice::Error),
                        }
                    }
                }
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }
}

/// Records `advisory` against `branch`'s workstream, which `record_spawn` has just written.
/// Separate from it because `record_spawn` deliberately carries over whatever an earlier
/// spawn on the same branch name recorded — the right call for a spawn that has no advisory
/// opinion of its own (a `t` dispatch's first pane) — while the modal's checkbox, like
/// `kanstack spawn --advisory`, *is* this spawn's opinion: a respawn is a new agent that may
/// well be doing a different kind of work (see `crate::cli::spawn`'s own note on this).
/// Best-effort, same as `record_spawn`: a registry that can't be written must not undo a
/// pane that did open, it just leaves the lane rendered as whatever it was.
pub(super) fn record_advisory(cwd: &std::path::Path, branch: &str, advisory: bool) {
    let _ = crate::workstream::Registry::with_lock(cwd, |registry| {
        if let Some(w) = registry.workstreams.iter_mut().find(|w| w.branch_id.0 == branch) {
            w.advisory = advisory;
        }
        Ok(())
    });
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
        let on_harness_row = modal && self.branch_modal_row == BranchModalRow::Harness;
        let on_model_row = modal && self.branch_modal_row == BranchModalRow::Model;
        let on_advisory_row = modal && self.branch_modal_row == BranchModalRow::Advisory;
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
            K::Char(' ') if on_advisory_row => self.advisory = !self.advisory,
            _ if on_name_row => {
                if self.branch_input.handle_key(key) {
                    self.branch_name_missing = false;
                }
            }
            _ if on_harness_row => {
                self.harness_input.handle_key(key);
            }
            _ if on_model_row => {
                self.model_input.handle_key(key);
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
