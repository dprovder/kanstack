//! The issues view: the third position of the unassigned lane's `tab` toggle, which turns
//! that lane into the tracker's item list (`kanstack_tracker`'s [`Tracker`]).
//!
//! Every tracker call runs on a background thread, the same pattern as the split-pane poll:
//! a `Receiver` parked on [`Issues`], checked once per tick by [`App::poll_issues`]. A slow
//! `gh` or `curl` (or a broken one) only ever means the list is older, never a stalled frame.
//! The tracker is opened on that thread too, through [`Issues`]'s opener, since a
//! `Box<dyn Tracker>` isn't `Send`.
//!
//! An item's lanes are the board columns with a branch named `item-<id>-…`, the naming
//! convention the orchestrator spawns under. Nothing here reads the orchestrator's own run
//! state: kanstack sits below it, so the branch names are the whole contract.

use super::*;
use kanstack_tracker::{Draft, Item, ItemFilter, ItemId, ItemState, Tracker};
use ratatui::crossterm::event::{KeyCode as K, KeyModifiers};

/// What `tab` on the unassigned lane cycles through, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnassignedView {
    /// The loose files, as `but status` lists them.
    #[default]
    Flat,
    /// The same files under a divider per directory (GitHub issue #6).
    Folders,
    /// The tracker's items instead of files.
    Issues,
}

impl UnassignedView {
    /// Every position, in `tab` order: what the lane header lists with the active one marked.
    pub const ALL: [UnassignedView; 3] = [UnassignedView::Flat, UnassignedView::Folders, UnassignedView::Issues];

    pub fn next(self) -> UnassignedView {
        match self {
            UnassignedView::Flat => UnassignedView::Folders,
            UnassignedView::Folders => UnassignedView::Issues,
            UnassignedView::Issues => UnassignedView::Flat,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            UnassignedView::Flat => "flat",
            UnassignedView::Folders => "folders",
            UnassignedView::Issues => "issues",
        }
    }
}

/// Opens the tracker on whichever thread calls it. `kanstack_tracker::select` for the real
/// app; a stub or `LocalTracker::in_dir` in tests (see [`App::with_tracker`]).
pub type OpenTracker = Arc<dyn Fn() -> Result<Box<dyn Tracker>> + Send + Sync>;

/// Which field of the draft modal has the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftRow {
    Title,
    /// Wraps downward like the PR modal's description, and keeps pasted line breaks.
    Body,
    Recipe,
}

/// A tracker write in flight, and what to say when it lands.
enum WriteDone {
    Created(Result<ItemId>),
    Promoted(ItemId, Result<()>),
    RecipeSet(ItemId, String, Result<()>),
}

/// How often the list is refetched while the issues view is showing.
const ISSUES_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

type ListResult = Result<(&'static str, Vec<Item>)>;

/// Everything the issues view keeps between frames.
pub struct Issues {
    /// `None` in snapshot mode, which has no repository to open a tracker for.
    open: Option<OpenTracker>,
    /// The id of the tracker the last successful list came from (`local`, `github`).
    pub tracker: Option<&'static str>,
    /// The last list that came back, oldest first, as the tracker returned it. Kept when a
    /// later fetch fails, so a flaky tracker shows a stale list and an error, not nothing.
    pub items: Vec<Item>,
    /// Whether any list has come back yet: before that, the lane says it's loading.
    pub loaded: bool,
    /// Why the last fetch failed, until one succeeds.
    pub error: Option<String>,
    fetch: Option<mpsc::Receiver<ListResult>>,
    fetched_at: Option<std::time::Instant>,
    write: Option<mpsc::Receiver<WriteDone>>,
    /// Put the cursor on this item once the next list has it: a draft just created.
    select_after_fetch: Option<ItemId>,
    /// Cursor in the list. `items.len()` is the "+ draft an issue" row after the last item.
    pub sel: usize,
    /// The item `Mode::IssueDetail` shows, and how far down it's scrolled.
    pub detail: Option<ItemId>,
    pub detail_scroll: u16,
    pub draft_title: TextInput,
    pub draft_body: TextInput,
    pub draft_recipe: TextInput,
    pub draft_row: DraftRow,
    /// Set when save was refused for an empty title, so the modal can mark the field.
    pub draft_title_missing: bool,
}

impl Issues {
    pub(super) fn new(open: Option<OpenTracker>) -> Issues {
        Issues {
            open,
            tracker: None,
            items: Vec::new(),
            loaded: false,
            error: None,
            fetch: None,
            fetched_at: None,
            write: None,
            select_after_fetch: None,
            sel: 0,
            detail: None,
            detail_scroll: 0,
            draft_title: TextInput::default(),
            draft_body: TextInput::default(),
            draft_recipe: TextInput::default(),
            draft_row: DraftRow::Title,
            draft_title_missing: false,
        }
    }

    /// The item under the cursor, or `None` on the draft row (or an empty list).
    pub fn selected(&self) -> Option<&Item> {
        self.items.get(self.sel)
    }

    /// Whether a fetch is in flight. For tests polling until one resolves.
    pub fn fetching(&self) -> bool {
        self.fetch.is_some()
    }

    /// Whether a create or promote is in flight. For tests polling until one resolves.
    pub fn writing(&self) -> bool {
        self.write.is_some()
    }
}

/// `item-<id>-`: the prefix the orchestrator puts on every branch an item's run works on.
/// Anything but letters, digits, `-`, `_` and `.` in the id becomes `-`, so `#44` and `44`
/// both give `item-44-`. Mirrors `kanstack_recipe::serve::branch_prefix`, which kanstack
/// can't depend on (the orchestrator sits above it); the naming is the contract.
pub fn item_branch_prefix(id: &ItemId) -> String {
    let id: String =
        id.0.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    format!("item-{}-", id.trim_matches('-'))
}

/// How an id reads in a list: `#44` for a bare number (the local tracker's), anything else
/// as the tracker gave it (`#44` already, `LIN-123`).
pub fn item_label(id: &ItemId) -> String {
    if id.0.starts_with(|c: char| c.is_ascii_digit()) {
        format!("#{id}")
    } else {
        id.0.clone()
    }
}

impl App {
    /// Swaps the tracker the issues view opens, for tests and anything else without a
    /// repository behind it. Clears whatever the previous tracker had listed.
    pub fn with_tracker(mut self, open: impl Fn() -> Result<Box<dyn Tracker>> + Send + Sync + 'static) -> App {
        self.issues = Issues::new(Some(Arc::new(open)));
        self
    }

    /// Whether the unassigned lane is showing the issue list.
    pub fn showing_issues(&self) -> bool {
        self.unassigned_view == UnassignedView::Issues
    }

    /// Whether the cursor is on the unassigned lane while it shows the issue list, which is
    /// when the issue keys (`n`, `p`, `⏎`, `↑/↓`) act on items instead of cards.
    pub fn on_issues(&self) -> bool {
        self.showing_issues()
            && self.board.columns.get(self.col).is_some_and(|c| c.kind == ColumnKind::Unassigned)
    }

    /// Board columns with a branch (tip or stacked) named for `id`'s run.
    pub fn item_lanes(&self, id: &ItemId) -> Vec<usize> {
        let prefix = item_branch_prefix(id);
        self.board
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.sections.iter().any(|s| s.name.starts_with(&prefix)))
            .map(|(i, _)| i)
            .collect()
    }

    /// The item whose lanes the board highlights: the one under the cursor while the cursor
    /// is on the issue list (or reading that item's detail).
    pub fn highlighted_item(&self) -> Option<&Item> {
        if !(self.on_issues() && matches!(self.mode, Mode::Normal | Mode::IssueDetail)) {
            return None;
        }
        self.issues.selected()
    }

    /// The item `Mode::IssueDetail` is showing.
    pub fn issue_detail(&self) -> Option<&Item> {
        let id = self.issues.detail.as_ref()?;
        self.issues.items.iter().find(|i| &i.id == id)
    }

    pub fn set_unassigned_view(&mut self, view: UnassignedView) {
        self.unassigned_view = view;
        self.unassigned_grouped_by_folder = view == UnassignedView::Folders;
    }

    /// `tab` on the unassigned lane: flat → folders → issues → flat. Scoped to that lane on
    /// purpose; toggling a view option for a lane the cursor isn't on would be a surprising
    /// thing for a key to do.
    ///
    /// Back to flat re-fetches rather than un-sorting the board in place: grouping sorts, so
    /// the original order needs a real source of truth. Snapshot mode has none, so there it
    /// only drops the dividers.
    pub(super) fn cycle_unassigned_view(&mut self) {
        let on_unassigned = self.board.columns.get(self.col).is_some_and(|c| c.kind == ColumnKind::Unassigned);
        if !on_unassigned {
            self.notify("tab switches the unassigned lane's view — select it first", Notice::Info);
            return;
        }
        let next = self.unassigned_view.next();
        self.set_unassigned_view(next);
        match next {
            UnassignedView::Issues => {
                // The files aren't drawn here, so nothing to re-fetch; dropping the folder
                // dividers keeps the board saying what `unassigned_grouped_by_folder` says.
                if let Some(col) = self.board.columns.first_mut() {
                    col.cards.iter_mut().for_each(|c| c.group = None);
                }
                self.issues.fetched_at = None;
                self.maybe_begin_issues_fetch();
                self.notify("unassigned shows the tracker's issues", Notice::Info);
                return;
            }
            UnassignedView::Folders | UnassignedView::Flat => {}
        }
        let grouped = next == UnassignedView::Folders;
        let notice = if grouped { "unassigned grouped by folder" } else { "unassigned back to a flat list" };
        let Some(but) = &self.but else {
            if let Some(col) = self.board.columns.first_mut() {
                if grouped {
                    crate::board::group_unassigned_by_folder(col);
                } else {
                    col.cards.iter_mut().for_each(|c| c.group = None);
                }
            }
            self.notify(notice, Notice::Info);
            return;
        };
        match but.status() {
            Ok(s) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &s);
                self.clamp();
                self.notify(notice, Notice::Info);
            }
            Err(e) => self.notify(format!("refresh failed: {e}"), Notice::Error),
        }
    }

    /// Applies whatever tracker work has finished, then starts the next list if the issues
    /// view is showing and the last one is old enough. Never waits: call every tick.
    pub fn poll_issues(&mut self) {
        if let Some(rx) = &self.issues.fetch {
            match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.issues.fetch = None;
                    self.issues.error = Some("the tracker stopped before answering".into());
                }
                Ok(Ok((tracker, items))) => {
                    self.issues.fetch = None;
                    self.issues.tracker = Some(tracker);
                    self.issues.items = items;
                    self.issues.loaded = true;
                    self.issues.error = None;
                    // Kept until a list has it: one already in flight when the create landed
                    // can come back without it, and the next one won't.
                    let wanted = self.issues.select_after_fetch.as_ref();
                    if let Some(i) = wanted.and_then(|id| self.issues.items.iter().position(|i| &i.id == id)) {
                        self.issues.sel = i;
                        self.issues.select_after_fetch = None;
                    }
                    self.issues.sel = self.issues.sel.min(self.issues.items.len());
                }
                Ok(Err(e)) => {
                    self.issues.fetch = None;
                    self.issues.error = Some(format!("{e:#}"));
                }
            }
        }
        if let Some(rx) = &self.issues.write {
            let done = match rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("the tracker stopped before answering".to_string()))
                }
                Ok(WriteDone::Created(Ok(id))) => {
                    let msg = format!("created {} in backlog", item_label(&id));
                    self.issues.select_after_fetch = Some(id);
                    Some(Ok(msg))
                }
                Ok(WriteDone::Created(Err(e))) => Some(Err(format!("could not create the issue: {e:#}"))),
                Ok(WriteDone::Promoted(id, Ok(()))) => Some(Ok(format!("promoted {} to backlog", item_label(&id)))),
                Ok(WriteDone::Promoted(id, Err(e))) => {
                    Some(Err(format!("could not promote {}: {e:#}", item_label(&id))))
                }
                Ok(WriteDone::RecipeSet(id, name, Ok(()))) => Some(Ok(format!("{} runs {name} now", item_label(&id)))),
                Ok(WriteDone::RecipeSet(id, name, Err(e))) => {
                    Some(Err(format!("could not set {}'s recipe to {name}: {e:#}", item_label(&id))))
                }
            };
            if let Some(done) = done {
                self.issues.write = None;
                // Due now, whatever the interval says: the list is known to be out of date.
                self.issues.fetched_at = None;
                match done {
                    Ok(msg) => self.notify(msg, Notice::Success),
                    Err(msg) => self.notify(msg, Notice::Error),
                }
            }
        }
        self.maybe_begin_issues_fetch();
    }

    fn maybe_begin_issues_fetch(&mut self) {
        if !self.showing_issues() || self.issues.fetch.is_some() {
            return;
        }
        let Some(open) = self.issues.open.clone() else { return };
        if self.issues.fetched_at.is_some_and(|t| t.elapsed() < ISSUES_POLL_INTERVAL) {
            return;
        }
        self.issues.fetched_at = Some(std::time::Instant::now());
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = open().and_then(|tracker| Ok((tracker.id(), tracker.list(&ItemFilter::default())?)));
            let _ = tx.send(result);
        });
        self.issues.fetch = Some(rx);
    }

    /// Runs one tracker write on a background thread; [`App::poll_issues`] reports it.
    fn begin_issues_write(
        &mut self,
        work: impl FnOnce(Result<Box<dyn Tracker>>) -> WriteDone + Send + 'static,
    ) -> bool {
        if self.issues.write.is_some() {
            self.notify("still saving the last change to the tracker", Notice::Info);
            return false;
        }
        let Some(open) = self.issues.open.clone() else {
            self.notify("snapshot is read-only", Notice::Info);
            return false;
        };
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(work(open()));
        });
        self.issues.write = Some(rx);
        true
    }

    fn move_issue_selection(&mut self, delta: isize) {
        // One more than the items: the "+ draft an issue" row.
        let n = self.issues.items.len() as isize + 1;
        self.issues.sel = (self.issues.sel as isize + delta).rem_euclid(n) as usize;
    }

    /// Keys on the unassigned lane while it shows issues. Returns whether the key was the
    /// view's own; anything else falls through to the board's ordinary keys (`←/→`, `tab`,
    /// `?`, `q`…).
    pub(super) fn handle_key_issues_view(&mut self, key: ratatui::crossterm::event::KeyEvent) -> bool {
        match key.code {
            K::Up | K::Char('k') => self.move_issue_selection(-1),
            K::Down | K::Char('j') => self.move_issue_selection(1),
            K::Char('g') => self.issues.sel = 0,
            K::Char('G') => self.issues.sel = self.issues.items.len().saturating_sub(1),
            K::Char('n') => self.begin_issue_draft(),
            K::Char('p') => self.promote_selected_issue(),
            K::Char('r') => self.pick_recipe_for_selected_issue(),
            K::Enter if self.issues.selected().is_none() => self.begin_issue_draft(),
            K::Enter => self.open_issue_detail(),
            // Every one of these acts on a file card, and none is on screen to act on.
            K::Char(' ' | 'm' | 'u' | 'c') => {
                self.notify("tab to the flat or folders view to work with files", Notice::Info)
            }
            _ => return false,
        }
        true
    }

    fn open_issue_detail(&mut self) {
        let Some(item) = self.issues.selected() else { return };
        self.issues.detail = Some(item.id.clone());
        self.issues.detail_scroll = 0;
        self.mode = Mode::IssueDetail;
    }

    pub(super) fn handle_key_issue_detail(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let lines = self
            .issue_detail()
            .map_or(0, |item| crate::ui::issue_detail_line_count(item, self.terminal_width));
        let last = lines.saturating_sub(1).min(u16::MAX as usize) as u16;
        let scroll = &mut self.issues.detail_scroll;
        match key.code {
            K::Esc | K::Enter | K::Left | K::Char('q' | 'h') => {
                self.mode = Mode::Normal;
                self.issues.detail = None;
            }
            K::Up | K::Char('k') => *scroll = scroll.saturating_sub(1),
            K::Down | K::Char('j') => *scroll = (*scroll + 1).min(last),
            K::PageUp => *scroll = scroll.saturating_sub(10),
            K::PageDown | K::Char(' ') => *scroll = scroll.saturating_add(10).min(last),
            K::Char('g') | K::Home => *scroll = 0,
            K::Char('G') | K::End => *scroll = last,
            _ => {}
        }
    }

    /// `n`: opens the draft modal. Nothing reaches the tracker until it's saved.
    pub(super) fn begin_issue_draft(&mut self) {
        if self.issues.open.is_none() {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        }
        self.issues.draft_title.clear();
        self.issues.draft_body.clear();
        self.issues.draft_recipe.clear();
        self.issues.draft_row = DraftRow::Title;
        self.issues.draft_title_missing = false;
        self.mode = Mode::IssueDraft;
    }

    pub(super) fn handle_key_issue_draft(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let row = self.issues.draft_row;
        match key.code {
            K::Esc => {
                self.mode = Mode::Normal;
                self.notify("draft discarded", Notice::Info);
            }
            K::Enter => self.confirm_issue_draft(),
            // The recipe is picked from the catalog, never typed: `tab` (the key that got
            // here) or typing on its row opens the picker, and backspace clears it.
            K::Tab if row == DraftRow::Recipe => self.pick_recipe_for_draft(None),
            K::Char(c) if row == DraftRow::Recipe && !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.pick_recipe_for_draft(Some(c))
            }
            K::Backspace | K::Delete if row == DraftRow::Recipe => self.issues.draft_recipe.clear(),
            K::Down | K::Tab => {
                self.issues.draft_row = match row {
                    DraftRow::Title => DraftRow::Body,
                    DraftRow::Body | DraftRow::Recipe => DraftRow::Recipe,
                }
            }
            K::Up | K::BackTab => {
                self.issues.draft_row = match row {
                    DraftRow::Recipe => DraftRow::Body,
                    DraftRow::Body | DraftRow::Title => DraftRow::Title,
                }
            }
            _ => {
                let input = match row {
                    DraftRow::Title => &mut self.issues.draft_title,
                    DraftRow::Body => &mut self.issues.draft_body,
                    DraftRow::Recipe => &mut self.issues.draft_recipe,
                };
                if input.handle_key(key) && row == DraftRow::Title {
                    self.issues.draft_title_missing = false;
                }
            }
        }
    }

    /// A bracketed paste into the draft modal's focused field. Only the body keeps breaks.
    pub(super) fn paste_issue_draft(&mut self, text: &str) {
        match self.issues.draft_row {
            DraftRow::Title => {
                self.issues.draft_title.paste(text, false);
                self.issues.draft_title_missing = false;
            }
            DraftRow::Body => self.issues.draft_body.paste(text, true),
            // Picked, not typed: a paste there is the picker's query.
            DraftRow::Recipe => {
                self.pick_recipe_for_draft(None);
                self.paste_recipe_picker(text);
            }
        }
    }

    /// Saves the draft as a `backlog` item, on a background thread.
    fn confirm_issue_draft(&mut self) {
        let title = self.issues.draft_title.trimmed();
        if title.is_empty() {
            self.issues.draft_row = DraftRow::Title;
            self.issues.draft_title_missing = true;
            self.notify("an issue needs a title", Notice::Info);
            return;
        }
        let recipe = Some(self.issues.draft_recipe.trimmed()).filter(|r| !r.is_empty());
        let draft = Draft {
            title,
            body: self.issues.draft_body.as_str().trim_end().to_string(),
            recipe,
            from: None,
            state: ItemState::Backlog,
        };
        if self.begin_issues_write(move |tracker| WriteDone::Created(tracker.and_then(|t| t.create(&draft)))) {
            self.mode = Mode::Normal;
            self.notify("saving the issue…", Notice::Info);
        }
    }

    /// Sets `id`'s recipe to `name` on a background thread (see [`Tracker::set_recipe`]).
    /// `false` when it couldn't start, having said why.
    pub(super) fn begin_set_recipe(&mut self, id: ItemId, name: String) -> bool {
        self.begin_issues_write(move |tracker| {
            let result = tracker.and_then(|t| t.set_recipe(&id, Some(&name)));
            WriteDone::RecipeSet(id, name, result)
        })
    }

    /// `p`: moves the selected proposal to `backlog`, where the queue picks it up. Anything
    /// that isn't a proposal is refused, since promoting a running or failed item would
    /// re-queue work behind the orchestrator's back.
    fn promote_selected_issue(&mut self) {
        let Some(item) = self.issues.selected() else {
            self.notify("pick a proposal to promote", Notice::Info);
            return;
        };
        // A backlog item is already queued, so say that (and why it might still sit there)
        // rather than refusing as if `p` were the wrong move for something running.
        if item.state == ItemState::Backlog {
            let msg = match item.recipe {
                Some(_) => format!("{} is already in the backlog", item_label(&item.id)),
                None => format!("{} is already in the backlog, but has no recipe, so serve won't pick it up", item_label(&item.id)),
            };
            self.notify(msg, Notice::Info);
            return;
        }
        if item.state != ItemState::Proposed {
            let msg = format!("only a proposal can be promoted — {} is {}", item_label(&item.id), item.state);
            self.notify(msg, Notice::Info);
            return;
        }
        let id = item.id.clone();
        if self.begin_issues_write(move |tracker| {
            let result = tracker.and_then(|t| t.set_state(&id, ItemState::Backlog));
            WriteDone::Promoted(id, result)
        }) {
            self.notify("promoting…", Notice::Info);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanstack_tracker::LocalTracker;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    fn board() -> Board {
        Board::from_status(&crate::but::parse_status(include_str!("../../tests/fixtures/status.json")).unwrap())
    }

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn type_text(app: &mut App, text: &str) {
        text.chars().for_each(|c| key(app, KeyCode::Char(c)));
    }

    /// Polls until `done` holds, the way the event loop's ticks would, or fails after a few
    /// seconds rather than hanging the suite.
    fn wait_until(app: &mut App, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting on the tracker");
            std::thread::sleep(Duration::from_millis(5));
            app.poll_issues();
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kanstack-issues-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn local_app(dir: &std::path::Path) -> App {
        let dir = dir.to_path_buf();
        App::from_board(board()).with_tracker(move || Ok(Box::new(LocalTracker::in_dir(dir.clone()))))
    }

    /// A tracker whose `list` answers only when the test says so, or fails outright.
    struct Stub {
        gate: Option<Arc<Mutex<mpsc::Receiver<()>>>>,
        fail: Option<&'static str>,
    }

    impl Tracker for Stub {
        fn id(&self) -> &'static str {
            "stub"
        }
        fn detect(&self, _repo: &std::path::Path) -> bool {
            true
        }
        fn list(&self, _filter: &ItemFilter) -> Result<Vec<Item>> {
            if let Some(gate) = &self.gate {
                gate.lock().unwrap().recv()?;
            }
            if let Some(fail) = self.fail {
                anyhow::bail!("{fail}");
            }
            Ok(vec![Item {
                id: ItemId("7".into()),
                title: "Slow but there".into(),
                state: ItemState::Backlog,
                recipe: None,
                from: None,
                body: String::new(),
            }])
        }
        fn get(&self, _id: &ItemId) -> Result<Item> {
            unimplemented!()
        }
        fn create(&self, _draft: &Draft) -> Result<ItemId> {
            unimplemented!()
        }
        fn set_state(&self, _id: &ItemId, _state: ItemState) -> Result<()> {
            unimplemented!()
        }
        fn set_recipe(&self, _id: &ItemId, _recipe: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        fn comment(&self, _id: &ItemId, _body: &str) -> Result<()> {
            unimplemented!()
        }
        fn link_text(&self, _id: &ItemId) -> Option<String> {
            None
        }
    }

    #[test]
    fn tab_cycles_flat_folders_issues_and_back() {
        let mut app = App::from_board(board());
        assert_eq!(app.unassigned_view, UnassignedView::Flat);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.unassigned_view, UnassignedView::Folders);
        assert!(app.unassigned_grouped_by_folder);
        assert!(app.board.columns[0].cards.iter().all(|c| c.group.is_some()), "folders view groups the files");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.unassigned_view, UnassignedView::Issues);
        assert!(!app.unassigned_grouped_by_folder, "the mirror only says yes for folders");
        assert!(app.board.columns[0].cards.iter().all(|c| c.group.is_none()), "no dividers left behind");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.unassigned_view, UnassignedView::Flat);
        assert!(!app.unassigned_grouped_by_folder);
    }

    #[test]
    fn tab_off_the_unassigned_lane_explains_itself_instead_of_cycling() {
        let mut app = App::from_board(board());
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.unassigned_view, UnassignedView::Flat);
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("unassigned")));
    }

    /// A tracker that takes its time can't hold up a keypress or a tick: entering the view
    /// and polling both return at once, the list shows up whenever the tracker answers, and
    /// the cursor moves meanwhile.
    #[test]
    fn a_slow_tracker_never_blocks_a_key_or_a_tick() {
        let (release, gate) = mpsc::channel();
        let gate = Arc::new(Mutex::new(gate));
        let mut app = App::from_board(board())
            .with_tracker(move || Ok(Box::new(Stub { gate: Some(gate.clone()), fail: None })));

        let start = Instant::now();
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        for _ in 0..10 {
            app.poll_issues();
        }
        key(&mut app, KeyCode::Down);
        assert!(start.elapsed() < Duration::from_millis(500), "took {:?}", start.elapsed());
        assert!(app.issues.fetching() && !app.issues.loaded, "still waiting on the tracker");

        release.send(()).unwrap();
        wait_until(&mut app, |app| app.issues.loaded);
        assert_eq!(app.issues.tracker, Some("stub"));
        assert_eq!(app.issues.items[0].title, "Slow but there");
    }

    #[test]
    fn a_failing_tracker_shows_its_error_and_leaves_the_board_usable() {
        let mut app =
            App::from_board(board()).with_tracker(|| Ok(Box::new(Stub { gate: None, fail: Some("gh: not logged in") })));
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        wait_until(&mut app, |app| !app.issues.fetching());
        assert!(app.issues.error.as_deref().is_some_and(|e| e.contains("not logged in")), "{:?}", app.issues.error);
        assert!(app.issues.items.is_empty());
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.col, 1, "the board still navigates");

        let mut app = App::from_board(board()).with_tracker(|| anyhow::bail!("KANSTACK_TRACKER is \"jira\""));
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        wait_until(&mut app, |app| !app.issues.fetching());
        assert!(app.issues.error.as_deref().is_some_and(|e| e.contains("jira")));
    }

    #[test]
    fn a_draft_is_created_in_backlog_and_selected_once_listed() {
        let dir = scratch("draft");
        let mut app = local_app(&dir);
        // A repository with one recipe, for the recipe field's picker.
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join(".kanstack/recipes")).unwrap();
        std::fs::write(dir.join(".kanstack/recipes/bugfix.md"), "---\ndescription: Fix a bug\n---\n").unwrap();
        app.repo = Some(dir.clone());
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        wait_until(&mut app, |app| app.issues.loaded);
        assert!(app.issues.items.is_empty());

        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.mode, Mode::IssueDraft);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::IssueDraft, "an empty title is refused");
        assert!(app.issues.draft_title_missing);

        type_text(&mut app, "Fix the flaky watch");
        key(&mut app, KeyCode::Down);
        app.on_paste("Retries race the debounce.\nSee #41.");
        key(&mut app, KeyCode::Down);
        // Typing on the recipe row filters the picker rather than the field; ⏎ picks.
        type_text(&mut app, "bugfix");
        assert_eq!(app.mode, Mode::RecipePicker);
        key(&mut app, KeyCode::Enter);
        assert_eq!((app.mode, app.issues.draft_recipe.as_str()), (Mode::IssueDraft, "bugfix"));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        wait_until(&mut app, |app| !app.issues.writing());
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("created #1")), "{:?}", app.message);
        wait_until(&mut app, |app| !app.issues.items.is_empty());

        let item = &app.issues.items[0];
        assert_eq!((item.title.as_str(), item.state, item.recipe.as_deref()), ("Fix the flaky watch", ItemState::Backlog, Some("bugfix")));
        assert_eq!(item.body, "Retries race the debounce.\nSee #41.\n");
        assert_eq!(app.issues.sel, 0, "the new item is selected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn p_promotes_a_proposal_and_refuses_anything_else() {
        let dir = scratch("promote");
        let tracker = LocalTracker::in_dir(&dir);
        tracker.create(&Draft { title: "Already queued".into(), ..Draft::default() }).unwrap();
        let proposal = tracker
            .create(&Draft { title: "Suggested".into(), state: ItemState::Proposed, from: Some(ItemId("1".into())), ..Draft::default() })
            .unwrap();
        let mut app = local_app(&dir);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        wait_until(&mut app, |app| app.issues.items.len() == 2);

        key(&mut app, KeyCode::Char('p'));
        assert!(!app.issues.writing(), "a backlog item isn't promoted");
        assert!(
            app.message.as_ref().is_some_and(|(m, _)| m.contains("#1 is already in the backlog") && m.contains("no recipe")),
            "{:?}",
            app.message
        );

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char('p'));
        wait_until(&mut app, |app| !app.issues.writing());
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("promoted #2")), "{:?}", app.message);
        assert_eq!(tracker.get(&proposal).unwrap().state, ItemState::Backlog);

        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Char('p'));
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("pick a proposal")), "the draft row isn't an item");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_opens_the_detail_and_scrolling_stops_at_its_last_line() {
        let dir = scratch("detail");
        let tracker = LocalTracker::in_dir(&dir);
        let body = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        tracker.create(&Draft { title: "Long".into(), body, ..Draft::default() }).unwrap();
        let mut app = local_app(&dir);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        wait_until(&mut app, |app| app.issues.loaded);

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::IssueDetail);
        assert_eq!(app.issue_detail().map(|i| i.title.as_str()), Some("Long"));
        key(&mut app, KeyCode::Char('G'));
        assert_eq!(app.issues.detail_scroll, 29);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.issues.detail_scroll, 29, "no further than the last line");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_items_lanes_are_the_columns_with_an_item_branch() {
        assert_eq!(item_branch_prefix(&ItemId("44".into())), "item-44-");
        assert_eq!(item_branch_prefix(&ItemId("#44".into())), "item-44-");
        assert_eq!(item_label(&ItemId("44".into())), "#44");
        assert_eq!(item_label(&ItemId("LIN-12".into())), "LIN-12");

        let mut board = board();
        board.columns[2].sections[0].name = "item-4-api".into();
        let app = App::from_board(board);
        assert_eq!(app.item_lanes(&ItemId("4".into())), [2]);
        assert!(app.item_lanes(&ItemId("44".into())).is_empty(), "item-4- isn't item-44-");
    }
}
