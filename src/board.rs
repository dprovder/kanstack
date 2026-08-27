//! Projects a `WorkspaceStatus` into the board the UI draws.
//!
//! The mapping is close to 1:1 because GitButler's model is already a board:
//!
//! | board            | GitButler                      |
//! |------------------|--------------------------------|
//! | column           | stack (a lane of applied work) |
//! | backlog column   | `uncommittedChanges`           |
//! | card             | commit, or an assigned file    |
//! | card id badge    | `cliId` — also the rub handle  |
//! | card badges      | branch status, CI, review, conflict |
//!
//! A stack may hold several branches stacked in series. Those stay in one column and
//! become groups within it, since they are one lane of work, not parallel lanes.

use std::collections::HashMap;

use crate::model::{
    Branch, BranchList, BranchStatus, Ci, CiConclusion, CiStatus, DiffOutput, FileChange,
    WorkspaceStatus,
};

/// The special `but` target meaning "unassigned".
pub const UNASSIGNED_TARGET: &str = "zz";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Good,
    Warn,
    Bad,
    Accent,
}

#[derive(Debug, Clone)]
pub struct Badge {
    pub text: String,
    pub tone: Tone,
}

impl Badge {
    fn new(text: impl Into<String>, tone: Tone) -> Self {
        Badge {
            text: text.into(),
            tone,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardKind {
    /// A commit. Rubbing it onto another column moves it there.
    Commit,
    /// A working-tree file. Rubbing it onto a column stages it there.
    Change,
}

#[derive(Debug, Clone)]
pub struct Card {
    /// Short handle shown on the card.
    pub cli_id: String,
    /// Identifier to pass to `but rub`.
    ///
    /// Deliberately not `cli_id`: `rub` re-resolves its arguments with fuzzy matching, so
    /// a two-character commit id can collide with a branch whose *name* contains those
    /// characters — `ea` matches both a commit and the branch `feat`. `but` then refuses
    /// rather than guessing, which is correct but fatal for a non-interactive caller. Full
    /// commit hashes are unambiguous, so commits use those.
    pub rub_id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub badges: Vec<Badge>,
    pub author: Option<String>,
    pub kind: CardKind,
    /// Branch name, set only when the column holds more than one branch.
    pub group: Option<String>,
    /// Lines added and removed.
    ///
    /// Only working-tree cards have this. Commit cards would need a `but diff <sha>` each
    /// — roughly 60ms per commit — where every uncommitted change is covered by the single
    /// `but diff` already being run. Paying N subprocess spawns per refresh to decorate a
    /// card is not worth it; see the README.
    pub stats: Option<(usize, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Unassigned,
    Stack,
}

/// What a branch's dot and status badge both mean.
///
/// Derived once so the two cannot disagree. They previously did: the badge special-cased
/// an empty branch, but the dot read `nothingToPush` straight from the wire and painted it
/// the same green as a pushed one — so a lane with nothing in it looked finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    /// No commits and nothing staged.
    Empty,
    /// Staged work, not yet committed.
    Uncommitted,
    /// Commits that have never reached the remote.
    Unpushed,
    /// Pushing would rewrite remote history.
    NeedsForce,
    /// Everything is on the remote.
    Pushed,
    /// Merged into the target.
    Integrated,
    /// At least one commit is in conflict.
    Conflicted,
    Unknown,
}

impl LaneState {
    pub fn label(self) -> &'static str {
        match self {
            LaneState::Empty => "empty",
            LaneState::Uncommitted => "uncommitted",
            LaneState::Unpushed => "unpushed",
            LaneState::NeedsForce => "needs force",
            LaneState::Pushed => "pushed",
            LaneState::Integrated => "integrated",
            LaneState::Conflicted => "conflicted",
            LaneState::Unknown => "unknown",
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            LaneState::Empty | LaneState::Unknown => Tone::Neutral,
            LaneState::Uncommitted | LaneState::Unpushed => Tone::Accent,
            LaneState::NeedsForce => Tone::Warn,
            LaneState::Pushed | LaneState::Integrated => Tone::Good,
            LaneState::Conflicted => Tone::Bad,
        }
    }
}

fn lane_state(b: &Branch, has_staged: bool) -> LaneState {
    // Conflict outranks everything: it is the one state you must act on.
    if b.commits.iter().any(|c| c.conflicted == Some(true)) {
        return LaneState::Conflicted;
    }
    if b.commits.is_empty() {
        return if has_staged {
            LaneState::Uncommitted
        } else {
            LaneState::Empty
        };
    }
    match b.branch_status {
        BranchStatus::Integrated => LaneState::Integrated,
        BranchStatus::NothingToPush => LaneState::Pushed,
        BranchStatus::UnpushedCommits | BranchStatus::CompletelyUnpushed => LaneState::Unpushed,
        BranchStatus::UnpushedCommitsRequiringForce => LaneState::NeedsForce,
        BranchStatus::Unknown => LaneState::Unknown,
    }
}

/// A branch within a lane, rendered with the same treatment as the lane header so a stack
/// visibly reads as stacked. Every branch gets one, including the tip — the tip's is drawn
/// by the lane header itself.
#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub state: LaneState,
    pub badges: Vec<Badge>,
    pub commits: usize,
    /// Lines added and removed by this branch alone, so a stack shows a figure per branch
    /// rather than one lump for the whole lane.
    pub stats: Option<(usize, usize)>,
    /// Last known status of this branch's harness-split pane (cmux or tmux), if kanstack
    /// has ever tracked one for it. Always `None` straight out of [`Board::build`] — this is not a function of
    /// [`WorkspaceStatus`], it's set afterwards by the app, the same way
    /// `group_unassigned_by_folder` is reapplied post-build rather than threaded through
    /// `build` itself.
    pub pane_status: Option<crate::pane_status::PaneStatus>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub kind: ColumnKind,
    pub title: String,
    /// `None` for the backlog, which is not a branch.
    pub state: Option<LaneState>,
    pub badges: Vec<Badge>,
    pub cards: Vec<Card>,
    /// One entry per branch in the lane, tip first. Lets the renderer give each stacked
    /// branch its own header, and its own push status — which the lane header alone could
    /// not show, since it only ever described the tip.
    pub sections: Vec<Section>,
    /// Identifier to rub a card onto when dropping it here. The branch *name* rather than
    /// its CLI id, for the same fuzzy-matching reason as [`Card::rub_id`].
    pub drop_target: String,
    /// Lines added and removed across the lane's working-tree cards.
    pub stats: Option<(usize, usize)>,
    /// Real name of the lane's tip branch, as `--anchor` wants it. `None` for the backlog.
    /// Distinct from `title`, which may carry a `+N` suffix for stacked branches.
    pub branch_name: Option<String>,
    /// Mirrors the tip section's `pane_status`, so the column header can show it without
    /// the renderer reaching into `sections[0]` itself.
    pub pane_status: Option<crate::pane_status::PaneStatus>,
}

#[derive(Debug, Clone)]
pub struct Board {
    pub columns: Vec<Column>,
    pub base_short_id: String,
    pub behind: usize,
    pub conflicted_files: Vec<String>,
}

impl Board {
    /// A board with nothing on it, for the one state that has no readable workspace behind
    /// it: see `Mode::Blocked`. Deliberately empty rather than absent, so every renderer and
    /// navigation path keeps working on a real `Board` instead of growing an `Option`.
    pub fn empty() -> Board {
        Board {
            columns: Vec::new(),
            base_short_id: String::new(),
            behind: 0,
            conflicted_files: Vec::new(),
        }
    }
}

/// A branch that exists in the repository but is not in the workspace.
///
/// Deliberately not a [`Column`]: an unapplied branch has no cards, no sections and no
/// drop target, because `but` will not enumerate its commits without applying it first.
/// Everything here is what `but branch list` is willing to say about a branch from the
/// outside, and modelling it as a lane would mean inventing the rest.
#[derive(Debug, Clone)]
pub struct UnappliedBranch {
    pub name: String,
    pub commits_ahead: Option<usize>,
    /// Whether applying it would merge cleanly into the upstream base. `Some(false)` is
    /// the single most consequential thing on the row — it is the difference between a
    /// quiet apply and a conflicted worktree — so the renderer gives it a tone rather than
    /// burying it in text. `None` when `but` declined to check.
    pub merges_cleanly: Option<bool>,
    pub author: Option<String>,
    /// Pre-rendered age, e.g. `4w ago`. Formatted here rather than in the renderer because
    /// the list is built once per fetch and drawn many times per second.
    pub age: Option<String>,
    /// False for a branch that only exists on the remote.
    pub has_local: bool,
    /// A heuristic, not a fact `but` reports: old enough (see `STALE_AGE_DAYS`) and no
    /// longer merging cleanly. Both together are what an abandoned branch whose work
    /// landed some other way looks like from the outside — a branch that's merely young,
    /// or merely conflicting because it was rebased five minutes ago, doesn't clear it.
    /// Surfaced as a nudge to go look, never acted on automatically: there is no reliable
    /// way to tell "abandoned" from "deliberately parked" without a person deciding.
    pub stale: bool,
}

/// What the drawer shows, built from one `but branch list` call.
#[derive(Debug, Clone, Default)]
pub struct Unapplied {
    pub branches: Vec<UnappliedBranch>,
    /// `but branch list` truncates to the active branch plus the 20 most recent. Carried
    /// through so the drawer can say it is not showing everything, rather than presenting
    /// a partial list as complete.
    pub truncated: bool,
}

/// How old a non-merging branch has to be before `UnappliedBranch::stale` flags it. Two
/// weeks: long enough that a branch sitting untouched past it is very unlikely to still be
/// active work rather than something whose ideas ended up landing another way.
const STALE_AGE_DAYS: i64 = 14;

/// Whether `then_ms` is at least `STALE_AGE_DAYS` before `now_ms`. `then_ms <= 0` is `but`
/// reporting "no timestamp" (an applied head, per `ListedBranch`'s docs) rather than an
/// actually-ancient commit, so that reads as not old.
fn is_old(then_ms: i64, now_ms: i64) -> bool {
    then_ms > 0 && (now_ms - then_ms) / 1000 / 60 / 60 / 24 >= STALE_AGE_DAYS
}

impl Unapplied {
    /// `now_ms` is passed in rather than read from the clock so ages are testable and the
    /// whole projection stays a pure function of its inputs.
    pub fn from_list(list: &BranchList, now_ms: i64) -> Unapplied {
        let mut branches: Vec<UnappliedBranch> = list
            .branches
            .iter()
            .map(|b| UnappliedBranch {
                name: b.name.clone(),
                commits_ahead: b.commits_ahead,
                merges_cleanly: b.merges_cleanly,
                author: b
                    .last_author
                    .as_ref()
                    .and_then(|a| a.name.clone())
                    .filter(|n| !n.is_empty()),
                age: relative_age(b.last_commit_at, now_ms),
                has_local: b.has_local.unwrap_or(false),
                stale: b.merges_cleanly == Some(false) && is_old(b.last_commit_at, now_ms),
            })
            .collect();
        // Local branches first, remote-only ones after — a stable sort, so `but`'s own
        // recency order survives within each half. Unlike the unassigned lane's
        // folder-grouping this has no flat/grouped toggle: with only ever two buckets,
        // there's nothing worth collapsing back out of.
        branches.sort_by_key(|b| !b.has_local);
        Unapplied {
            branches,
            truncated: list.has_more_branches,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.branches.is_empty()
    }
}

/// Renders an epoch-*millisecond* timestamp as an approximate age, in the same vocabulary
/// `but branch list` uses in its own human output (`4w ago`).
///
/// `None` for the `0` an applied head reports: that is an absence of a timestamp, not a
/// branch last touched in 1970. A timestamp in the future (clock skew between machines,
/// which is ordinary on shared remotes) reads as "just now" rather than going negative.
fn relative_age(then_ms: i64, now_ms: i64) -> Option<String> {
    if then_ms <= 0 {
        return None;
    }
    let secs = (now_ms - then_ms) / 1000;
    if secs < 60 {
        return Some("just now".into());
    }
    let mins = secs / 60;
    if mins < 60 {
        return Some(format!("{mins}m ago"));
    }
    let hours = mins / 60;
    if hours < 24 {
        return Some(format!("{hours}h ago"));
    }
    let days = hours / 24;
    if days < 7 {
        return Some(format!("{days}d ago"));
    }
    if days < 365 {
        return Some(format!("{}w ago", days / 7));
    }
    Some(format!("{}y ago", days / 365))
}

/// Sums added/removed per file path. `but diff` emits one entry per hunk, so a file with
/// several hunks appears several times and its counts accumulate.
pub fn stats_by_path(diff: &DiffOutput) -> HashMap<String, (usize, usize)> {
    let mut map: HashMap<String, (usize, usize)> = HashMap::new();
    for change in &diff.changes {
        let Some(body) = &change.diff else { continue };
        let e = map.entry(change.path.clone()).or_default();
        for hunk in &body.hunks {
            for line in hunk.diff.lines() {
                match line.as_bytes().first() {
                    Some(b'+') => e.0 += 1,
                    Some(b'-') => e.1 += 1,
                    _ => {}
                }
            }
        }
    }
    // The `@@` header starts with neither, so nothing to discount.
    map
}

impl Board {
    pub fn from_status(s: &WorkspaceStatus) -> Board {
        Board::build(s, &HashMap::new(), &HashMap::new())
    }

    /// As [`Board::from_status`], with line counts attached to working-tree cards.
    pub fn from_status_and_diff(s: &WorkspaceStatus, diff: &DiffOutput) -> Board {
        Board::build(s, &stats_by_path(diff), &HashMap::new())
    }

    /// The full picture: working-tree counts by path, plus commit counts by SHA.
    ///
    /// Commit counts are passed in rather than fetched because they need one
    /// `but diff <sha>` each. A commit's content is fixed by its hash, so the caller can
    /// cache them forever and only pay for hashes it has not seen.
    pub fn from_status_diff_and_commits(
        s: &WorkspaceStatus,
        diff: &DiffOutput,
        commits: &HashMap<String, (usize, usize)>,
    ) -> Board {
        Board::build(s, &stats_by_path(diff), commits)
    }

    fn build(
        s: &WorkspaceStatus,
        stats: &HashMap<String, (usize, usize)>,
        commit_stats: &HashMap<String, (usize, usize)>,
    ) -> Board {
        let mut columns = Vec::with_capacity(s.stacks.len() + 1);

        columns.push(Column {
            kind: ColumnKind::Unassigned,
            title: "unassigned".into(),
            state: None,
            badges: Vec::new(),
            cards: s
                .uncommitted_changes
                .iter()
                .map(|c| change_card(c, stats))
                .collect(),
            sections: Vec::new(),
            stats: None,
            drop_target: UNASSIGNED_TARGET.into(),
            branch_name: None,
            pane_status: None,
        });

        for stack in &s.stacks {
            // The first branch is the tip of the stack; it names the lane and receives drops.
            let Some(top) = stack.branches.first() else {
                continue;
            };
            let multi = stack.branches.len() > 1;

            let mut cards: Vec<Card> = stack
                .assigned_changes
                .iter()
                .map(|c| change_card(c, stats))
                .collect();
            for branch in &stack.branches {
                let group = multi.then(|| branch.name.clone());
                for commit in &branch.commits {
                    let files = commit.changes.as_deref().unwrap_or(&[]);
                    let subtitle = (!files.is_empty()).then(|| {
                        let names: Vec<&str> = files.iter().map(|f| f.file_path.as_str()).collect();
                        names.join(", ")
                    });

                    let mut badges = Vec::new();
                    if commit.conflicted == Some(true) {
                        badges.push(Badge::new("conflict", Tone::Bad));
                    }
                    if let Some(review) = &commit.review_id {
                        badges.push(Badge::new(format!("#{review}"), Tone::Good));
                    }
                    badges.push(Badge::new(commit.short_id().to_string(), Tone::Neutral));

                    cards.push(Card {
                        cli_id: commit.cli_id.clone(),
                        rub_id: commit.commit_id.clone(),
                        title: commit.subject().to_string(),
                        subtitle,
                        badges,
                        author: Some(commit.author_name.clone()),
                        kind: CardKind::Commit,
                        group: group.clone(),
                        stats: commit_stats.get(&commit.commit_id).copied(),
                    });
                }
            }

            let title = if multi {
                format!("{} +{}", top.name, stack.branches.len() - 1)
            } else {
                top.name.clone()
            };

            // Staged changes belong to the stack rather than to any one branch, so they
            // only count toward the tip's "uncommitted" badge.
            let sections = stack
                .branches
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    let staged = i == 0 && !stack.assigned_changes.is_empty();
                    // Each branch totals its own commits; the tip also owns whatever the
                    // stack has staged, since staged changes belong to no branch below it.
                    let mut totals = b.commits.iter().filter_map(|c| {
                        commit_stats.get(&c.commit_id).copied()
                    });
                    let mut sum = totals.next().unwrap_or((0, 0));
                    for (a, r) in totals {
                        sum = (sum.0 + a, sum.1 + r);
                    }
                    if staged {
                        for c in &stack.assigned_changes {
                            if let Some((a, r)) = stats.get(&c.file_path) {
                                sum = (sum.0 + a, sum.1 + r);
                            }
                        }
                    }
                    Section {
                        name: b.name.clone(),
                        state: lane_state(b, staged),
                        badges: branch_badges(b, staged),
                        commits: b.commits.len(),
                        stats: (sum != (0, 0)).then_some(sum),
                        pane_status: None,
                    }
                })
                .collect();

            columns.push(Column {
                kind: ColumnKind::Stack,
                title,
                state: Some(lane_state(top, !stack.assigned_changes.is_empty())),
                badges: branch_badges(top, !stack.assigned_changes.is_empty()),
                cards,
                sections,
                stats: None,
                drop_target: top.name.clone(),
                branch_name: Some(top.name.clone()),
                pane_status: None,
            });
        }

        for col in &mut columns {
            let totals = col.cards.iter().filter_map(|c| c.stats).fold(
                (0usize, 0usize),
                |(a, r), (ca, cr)| (a + ca, r + cr),
            );
            col.stats = (totals != (0, 0)).then_some(totals);
        }

        Board {
            columns,
            base_short_id: s.merge_base.short_id().to_string(),
            behind: s.upstream_state.behind,
            conflicted_files: s.conflicted_files.clone(),
        }
    }
}

fn change_card(c: &FileChange, stats: &HashMap<String, (usize, usize)>) -> Card {
    Card {
        stats: stats.get(&c.file_path).copied(),
        cli_id: c.cli_id.clone(),
        rub_id: c.cli_id.clone(),
        title: c.file_path.clone(),
        subtitle: None,
        badges: vec![Badge::new(c.change_type.label(), Tone::Neutral)],
        author: None,
        kind: CardKind::Change,
        group: None,
    }
}

/// Re-sorts the unassigned column's cards by the directory each file lives in, and sets
/// each `Card::group` to that directory so the renderer prints a header between groups —
/// reusing the same inline-header mechanism a stacked branch's own sections already use.
///
/// GitHub issue #6: with a lot of loose files, a flat list stops being navigable by eye.
/// This is purely a display grouping — it doesn't touch what `but` returns, and reapplying
/// it after every refresh (see `App::clamp`) is what keeps it in effect across the toggle.
pub fn group_unassigned_by_folder(column: &mut Column) {
    if column.kind != ColumnKind::Unassigned {
        return;
    }
    column
        .cards
        .sort_by(|a, b| split_folder(&a.title).cmp(&split_folder(&b.title)));
    for card in &mut column.cards {
        card.group = Some(split_folder(&card.title).0.to_string());
    }
}

/// Splits a path into `(directory, filename)`; a root-level file's directory is `"."`,
/// matching shell convention rather than printing an empty header.
fn split_folder(path: &str) -> (&str, &str) {
    match path.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => (".", path),
    }
}

fn branch_badges(b: &Branch, has_staged: bool) -> Vec<Badge> {
    // A branch with no commits reports `nothingToPush`, which renders as "in sync" and
    // reads as "already pushed" rather than "nothing here yet". And a lane holding staged
    // work is not empty even though it has no commits yet, so check that before saying so.
    let status = lane_state(b, has_staged).label();
    // The header dot already encodes push state by colour; the label spells it out, since
    // "needs force" and "unpushed" are the same hue family at a glance.
    let mut badges = vec![Badge::new(status, Tone::Neutral)];
    if let Some(review) = &b.review_id {
        badges.push(Badge::new(format!("#{review}"), Tone::Good));
    }
    if let Some(ci) = &b.ci {
        badges.push(ci_badge(ci));
    }
    badges
}

fn ci_badge(ci: &Ci) -> Badge {
    // Report in-progress ahead of conclusion: a green tick on a still-running suite reads
    // as "done and passing", which it is not.
    if ci.status == CiStatus::InProgress {
        let n = ci.pending_check_titles.len();
        return Badge::new(format!("ci {n} running"), Tone::Warn);
    }
    match ci.conclusion {
        CiConclusion::Success => {
            let n = ci.passing_check_titles.len();
            Badge::new(format!("ci {n} pass"), Tone::Good)
        }
        CiConclusion::Failure => {
            let n = ci.failing_check_titles.len();
            Badge::new(format!("ci {n} failed"), Tone::Bad)
        }
        CiConclusion::Unknown => Badge::new("ci ?", Tone::Neutral),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkspaceStatus {
        crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap()
    }

    #[test]
    fn builds_backlog_plus_one_column_per_stack() {
        let b = Board::from_status(&sample());
        let titles: Vec<&str> = b.columns.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            ["unassigned", "feat-auth", "feat-ui", "fix-flaky-tests"]
        );
        assert_eq!(b.columns[0].kind, ColumnKind::Unassigned);
    }

    #[test]
    fn commits_become_cards_newest_first() {
        let b = Board::from_status(&sample());
        let ui = b.columns.iter().find(|c| c.title == "feat-ui").unwrap();
        let titles: Vec<&str> = ui.cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Fix settings tab focus ring", "Redesign settings page"],
            "but status lists the tip commit first, which is the order a board wants"
        );
    }

    fn bare_change_card(title: &str) -> Card {
        Card {
            cli_id: "xx".into(),
            rub_id: "xx".into(),
            title: title.into(),
            subtitle: None,
            badges: Vec::new(),
            author: None,
            kind: CardKind::Change,
            group: None,
            stats: None,
        }
    }

    #[test]
    fn folder_grouping_sorts_by_directory_then_name() {
        let mut col = Column {
            kind: ColumnKind::Unassigned,
            title: "unassigned".into(),
            state: None,
            badges: Vec::new(),
            cards: vec![
                bare_change_card("src/ui.rs"),
                bare_change_card("README.md"),
                bare_change_card("src/board.rs"),
                bare_change_card("docs/assets/overview.gif"),
            ],
            sections: Vec::new(),
            stats: None,
            drop_target: UNASSIGNED_TARGET.into(),
            branch_name: None,
            pane_status: None,
        };
        group_unassigned_by_folder(&mut col);
        let got: Vec<(Option<&str>, &str)> = col
            .cards
            .iter()
            .map(|c| (c.group.as_deref(), c.title.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                (Some("."), "README.md"),
                (Some("docs/assets"), "docs/assets/overview.gif"),
                (Some("src"), "src/board.rs"),
                (Some("src"), "src/ui.rs"),
            ],
            "root-level files sort first under '.', then directories alphabetically, \
             files within a directory alphabetically"
        );
    }

    #[test]
    fn folder_grouping_is_a_no_op_on_a_stack_column() {
        let mut col = Column {
            kind: ColumnKind::Stack,
            title: "feat-auth".into(),
            state: None,
            badges: Vec::new(),
            cards: vec![bare_change_card("b.txt"), bare_change_card("a.txt")],
            sections: Vec::new(),
            stats: None,
            drop_target: "feat-auth".into(),
            branch_name: Some("feat-auth".into()),
            pane_status: None,
        };
        group_unassigned_by_folder(&mut col);
        assert_eq!(
            col.cards.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            ["b.txt", "a.txt"],
            "grouping only ever applies to the unassigned lane"
        );
        assert!(col.cards.iter().all(|c| c.group.is_none()));
    }

    #[test]
    fn backlog_holds_unassigned_working_tree_files() {
        let b = Board::from_status(&sample());
        let backlog = &b.columns[0];
        let titles: Vec<&str> = backlog.cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["wip1.txt", "wip2.txt"]);
        assert!(backlog.cards.iter().all(|c| c.kind == CardKind::Change));
        assert_eq!(backlog.drop_target, UNASSIGNED_TARGET);
    }

    /// Identifiers handed to `but rub` must be unambiguous, because `rub` re-resolves them
    /// with fuzzy matching and refuses rather than guessing when two things match. A short
    /// CLI id like `ea` collides with the branch `feat`, so names and full hashes are used.
    #[test]
    fn rub_identifiers_avoid_the_fuzzy_matching_trap() {
        let s = sample();
        let b = Board::from_status(&s);

        let auth = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        assert_eq!(
            auth.drop_target, "feat-auth",
            "a lane rubs onto its branch name, not its two-character id"
        );

        let commit = &auth.cards[0];
        assert_eq!(
            commit.rub_id,
            s.stacks[0].branches[0].commits[0].commit_id,
            "a commit rubs by full hash"
        );
        assert_ne!(
            commit.rub_id, commit.cli_id,
            "the displayed id and the rub id are deliberately different"
        );
    }

    #[test]
    fn a_file_card_rubs_by_its_cli_id() {
        // Files have no hash to fall back on, so the CLI id is all there is.
        let b = Board::from_status(&sample());
        let file = &b.columns[0].cards[0];
        assert_eq!(file.rub_id, file.cli_id);
    }

    #[test]
    fn card_subtitle_lists_touched_files() {
        let b = Board::from_status(&sample());
        let auth = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        let tip = &auth.cards[0];
        assert_eq!(tip.subtitle.as_deref(), Some("a2.txt"));
        assert_eq!(tip.author.as_deref(), Some("Dani"));
    }

    /// Found by dogfooding: a freshly created branch reports `nothingToPush`, and calling
    /// that "in sync" reads as "already pushed" when nothing has been written yet.
    #[test]
    fn a_branch_with_no_commits_reads_as_empty_not_in_sync() {
        let mut s = sample();
        s.stacks[0].branches[0].commits.clear();
        s.stacks[0].branches[0].branch_status = crate::model::BranchStatus::NothingToPush;

        let b = Board::from_status(&s);
        let lane = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        let labels: Vec<&str> = lane.badges.iter().map(|b| b.text.as_str()).collect();
        assert!(labels.contains(&"empty"), "got {labels:?}");
        assert!(!labels.contains(&"in sync"), "got {labels:?}");
    }

    /// Also found by dogfooding: staging a file into a commit-less lane leaves it with no
    /// commits but real work in it, so "empty" is wrong the moment something is staged.
    #[test]
    fn a_lane_holding_staged_work_is_not_empty() {
        let mut s = sample();
        let staged = s.uncommitted_changes.remove(0);
        s.stacks[0].branches[0].commits.clear();
        s.stacks[0].assigned_changes.push(staged);

        let b = Board::from_status(&s);
        let lane = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        let labels: Vec<&str> = lane.badges.iter().map(|b| b.text.as_str()).collect();
        assert!(labels.contains(&"uncommitted"), "got {labels:?}");
        assert!(!labels.contains(&"empty"), "got {labels:?}");
        assert_eq!(lane.cards.len(), 1, "the staged file shows as a card");
    }

    /// Each branch in a stack carries its own push status. The lane header only ever
    /// described the tip, so a pushed branch underneath had nothing showing it at all.
    #[test]
    fn every_branch_in_a_stack_gets_its_own_status() {
        let mut s = sample();
        let extra = s.stacks.remove(2).branches.remove(0);
        let extra_name = extra.name.clone();
        s.stacks[0].branches.push(extra);

        let b = Board::from_status(&s);
        let lane = &b.columns[1];
        assert_eq!(lane.sections.len(), 2, "one section per branch, tip first");
        assert_eq!(lane.sections[0].name, "feat-auth");
        assert_eq!(lane.sections[1].name, extra_name);
        assert!(
            lane.sections.iter().all(|s| !s.badges.is_empty()),
            "each branch reports its own state, not just the tip"
        );
    }

    /// The dot colour and the status word come from one value, so they cannot drift.
    /// They did: an empty branch reports `nothingToPush`, which the dot painted the same
    /// green as a fully pushed branch while the badge correctly said "empty".
    #[test]
    fn an_empty_lane_is_not_coloured_like_a_pushed_one() {
        let mut s = sample();
        s.stacks[0].branches[0].commits.clear();
        s.stacks[0].branches[0].branch_status = BranchStatus::NothingToPush;

        let b = Board::from_status(&s);
        let lane = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        assert_eq!(lane.state, Some(LaneState::Empty));
        assert_eq!(lane.state.unwrap().tone(), Tone::Neutral, "not the good/green tone");
        assert_ne!(LaneState::Empty.tone(), LaneState::Pushed.tone());
    }

    #[test]
    fn a_conflicted_commit_outranks_the_push_status() {
        let mut s = sample();
        s.stacks[0].branches[0].commits[0].conflicted = Some(true);
        s.stacks[0].branches[0].branch_status = BranchStatus::NothingToPush;
        let b = Board::from_status(&s);
        let lane = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        assert_eq!(lane.state, Some(LaneState::Conflicted));
        assert_eq!(lane.state.unwrap().tone(), Tone::Bad);
    }

    #[test]
    fn the_dot_and_the_badge_always_agree() {
        // Whatever the state, the word next to the lane is that state's own label.
        let b = Board::from_status(&sample());
        for col in b.columns.iter().filter(|c| c.state.is_some()) {
            let state = col.state.unwrap();
            let labels: Vec<&str> = col.badges.iter().map(|b| b.text.as_str()).collect();
            assert!(
                labels.contains(&state.label()),
                "lane {:?} is {state:?} but its badges say {labels:?}",
                col.title
            );
        }
    }

    /// `but diff` emits one entry per hunk, so a file with several hunks must have its
    /// counts summed rather than overwritten.
    #[test]
    fn line_counts_accumulate_across_a_files_hunks() {
        let diff: crate::model::DiffOutput = serde_json::from_str(
            r#"{"changes":[
              {"id":"h0","path":"wip1.txt","diff":{"type":"patch","hunks":[
                {"oldStart":1,"oldLines":1,"newStart":1,"newLines":2,
                 "diff":"@@ -1 +1,2 @@\n-a\n+A\n+B\n"}]}},
              {"id":"i0","path":"wip1.txt","diff":{"type":"patch","hunks":[
                {"oldStart":9,"oldLines":1,"newStart":10,"newLines":1,
                 "diff":"@@ -9 +10 @@\n-z\n+Z\n"}]}}
            ]}"#,
        )
        .unwrap();
        let by_path = stats_by_path(&diff);
        assert_eq!(by_path.get("wip1.txt"), Some(&(3, 2)), "3 added, 2 removed");

        let b = Board::from_status_and_diff(&sample(), &diff);
        let card = b.columns[0]
            .cards
            .iter()
            .find(|c| c.title == "wip1.txt")
            .unwrap();
        assert_eq!(card.stats, Some((3, 2)));
        assert_eq!(b.columns[0].stats, Some((3, 2)), "the lane totals its cards");
    }

    #[test]
    fn commit_cards_carry_no_line_counts() {
        // Deliberate: those would cost one `but diff <sha>` per commit, per refresh.
        let b = Board::from_status(&sample());
        let lane = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        assert!(lane.cards.iter().all(|c| c.stats.is_none()));
        assert!(lane.stats.is_none(), "so the lane has nothing to total");
    }

    #[test]
    fn a_pushed_branch_says_pushed() {
        // `nothingToPush` used to render as "in sync", which nobody reads as "pushed".
        let mut s = sample();
        s.stacks[0].branches[0].branch_status = BranchStatus::NothingToPush;
        let b = Board::from_status(&s);
        let lane = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        let labels: Vec<&str> = lane.badges.iter().map(|b| b.text.as_str()).collect();
        assert!(labels.contains(&"pushed"), "got {labels:?}");
    }

    #[test]
    fn running_ci_is_not_reported_as_passing() {
        let ci = Ci {
            pending_check_titles: vec!["build".into()],
            passing_check_titles: vec!["lint".into()],
            failing_check_titles: vec![],
            status: CiStatus::InProgress,
            conclusion: CiConclusion::Success,
        };
        let badge = ci_badge(&ci);
        assert_eq!(badge.tone, Tone::Warn);
        assert_eq!(badge.text, "ci 1 running");
    }

    #[test]
    fn stacked_branches_share_one_lane_and_are_grouped() {
        let mut s = sample();
        // Fold the third stack's branch into the first stack, as stacked branches.
        let extra = s.stacks.remove(2).branches.remove(0);
        let extra_name = extra.name.clone();
        s.stacks[0].branches.push(extra);

        let b = Board::from_status(&s);
        assert_eq!(b.columns.len(), 3, "backlog + two stacks");
        let lane = &b.columns[1];
        assert_eq!(lane.title, "feat-auth +1");
        assert!(
            lane.cards
                .iter()
                .any(|c| c.group.as_deref() == Some(extra_name.as_str())),
            "cards from a stacked branch are labelled with their branch"
        );
    }

    /// Epoch *milliseconds*, and a fixed `now`, so this pins the unit as much as the
    /// wording — reading the field as seconds would put every branch decades in the past
    /// and the bug would look like a formatting quirk rather than a unit error.
    #[test]
    fn ages_render_in_the_same_vocabulary_but_uses() {
        const NOW: i64 = 1_785_000_000_000;
        let ago = |ms: i64| relative_age(NOW - ms, NOW);
        assert_eq!(ago(5_000).as_deref(), Some("just now"));
        assert_eq!(ago(90 * 1_000).as_deref(), Some("1m ago"));
        assert_eq!(ago(3 * 3_600 * 1_000).as_deref(), Some("3h ago"));
        assert_eq!(ago(2 * 86_400 * 1_000).as_deref(), Some("2d ago"));
        assert_eq!(ago(28 * 86_400 * 1_000).as_deref(), Some("4w ago"));
        assert_eq!(ago(800 * 86_400 * 1_000).as_deref(), Some("2y ago"));
    }

    /// An applied head reports `lastCommitAt: 0`. That is "no timestamp", not "1970", and
    /// rendering it as a 56-year-old branch would be worse than showing nothing at all.
    #[test]
    fn a_missing_timestamp_has_no_age_rather_than_a_wrong_one() {
        assert_eq!(relative_age(0, 1_785_000_000_000), None);
    }

    /// Clocks on shared remotes disagree, so a commit timestamped slightly ahead of local
    /// now is ordinary. It must not underflow into a nonsense age.
    #[test]
    fn a_future_timestamp_reads_as_just_now() {
        const NOW: i64 = 1_785_000_000_000;
        assert_eq!(relative_age(NOW + 60_000, NOW).as_deref(), Some("just now"));
    }

    #[test]
    fn unapplied_projects_only_the_branches_outside_the_workspace() {
        let list: crate::model::BranchList =
            serde_json::from_str(include_str!("../tests/fixtures/branch_list.json")).unwrap();
        let u = Unapplied::from_list(&list, 1_785_000_000_000);

        // `appliedStacks` is not folded in: those lanes are already on the board, drawn
        // from `but status`, and listing them again in the drawer would offer to apply
        // branches that are applied.
        assert_eq!(u.branches.len(), 2);
        assert!(
            !u.branches.iter().any(|b| b.name == "new-landing-page"),
            "the applied head must not appear in the drawer"
        );
        assert!(!u.truncated);

        let clean = &u.branches[0];
        assert_eq!(clean.name, "feat-theme");
        assert_eq!(clean.merges_cleanly, Some(true));
        assert_eq!(clean.commits_ahead, Some(1));
        assert_eq!(clean.author.as_deref(), Some("dprovder"));
        assert!(clean.age.is_some());
        assert!(!clean.has_local, "this one is remote-only");

        assert_eq!(u.branches[1].merges_cleanly, Some(false));
    }

    /// Local branches sort ahead of remote-only ones, so the drawer can draw one divider
    /// between the two rather than interleaving them — but the sort is stable, so `but`'s
    /// own recency order still holds within each half.
    #[test]
    fn unapplied_sorts_local_branches_before_remote_only_ones() {
        fn listed(name: &str, has_local: bool) -> crate::model::ListedBranch {
            crate::model::ListedBranch {
                name: name.into(),
                reviews: Vec::new(),
                has_local: Some(has_local),
                last_commit_at: 0,
                commits_ahead: None,
                last_author: None,
                merges_cleanly: None,
            }
        }
        let list = crate::model::BranchList {
            applied_stacks: Vec::new(),
            branches: vec![
                listed("remote-old", false),
                listed("local-a", true),
                listed("remote-new", false),
                listed("local-b", true),
            ],
            has_more_branches: false,
        };
        let u = Unapplied::from_list(&list, 1_785_000_000_000);
        let names: Vec<&str> = u.branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["local-a", "local-b", "remote-old", "remote-new"]);
    }

    /// `stale` needs both signals, not either alone — a branch that's merely old (say,
    /// deliberately parked) or merely conflicting (rebased minutes ago) isn't the "probably
    /// abandoned" shape this is meant to flag; only old *and* non-clean together are.
    #[test]
    fn unapplied_flags_old_non_merging_branches_as_stale() {
        const NOW: i64 = 1_785_000_000_000;
        const DAY_MS: i64 = 86_400_000;

        fn listed(name: &str, age_days: i64, merges_cleanly: Option<bool>) -> crate::model::ListedBranch {
            crate::model::ListedBranch {
                name: name.into(),
                reviews: Vec::new(),
                has_local: Some(true),
                last_commit_at: NOW - age_days * DAY_MS,
                commits_ahead: Some(1),
                last_author: None,
                merges_cleanly,
            }
        }
        let list = crate::model::BranchList {
            applied_stacks: Vec::new(),
            branches: vec![
                listed("old-and-conflicting", 20, Some(false)),
                listed("old-but-clean", 20, Some(true)),
                listed("recent-and-conflicting", 1, Some(false)),
                listed("old-but-unchecked", 20, None),
            ],
            has_more_branches: false,
        };
        let u = Unapplied::from_list(&list, NOW);
        let stale = |name: &str| u.branches.iter().find(|b| b.name == name).unwrap().stale;
        assert!(stale("old-and-conflicting"), "old + non-clean should flag as stale");
        assert!(!stale("old-but-clean"), "clean branches aren't stale regardless of age");
        assert!(!stale("recent-and-conflicting"), "conflicting alone isn't stale — could just be a fresh rebase");
        assert!(!stale("old-but-unchecked"), "no merge check means no basis to call it stale");
    }
}
