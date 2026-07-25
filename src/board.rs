//! Projects a `WorkspaceStatus` into the board the UI draws.
//!
//! The mapping is close to 1:1 because GitButler's model is already a board:
//!
//! | board            | GitButler                      |
//! |------------------|--------------------------------|
//! | column           | stack (a lane of applied work) |
//! | backlog column   | `unassignedChanges`            |
//! | card             | commit, or an assigned file    |
//! | card id badge    | `cliId` — also the rub handle  |
//! | card badges      | branch status, CI, review, conflict |
//!
//! A stack may hold several branches stacked in series. Those stay in one column and
//! become groups within it, since they are one lane of work, not parallel lanes.

use crate::model::{Branch, BranchStatus, Ci, CiConclusion, CiStatus, FileChange, WorkspaceStatus};

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
    pub cli_id: String,
    pub title: String,
    pub subtitle: Option<String>,
    pub badges: Vec<Badge>,
    pub author: Option<String>,
    pub kind: CardKind,
    /// Branch name, set only when the column holds more than one branch.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Unassigned,
    Stack,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub kind: ColumnKind,
    pub title: String,
    pub status: Option<BranchStatus>,
    pub badges: Vec<Badge>,
    pub cards: Vec<Card>,
    /// CLI id to rub a card onto when dropping it here.
    pub drop_target: String,
}

#[derive(Debug, Clone)]
pub struct Board {
    pub columns: Vec<Column>,
    pub base_short_id: String,
    pub behind: usize,
    pub conflicted_files: Vec<String>,
}

impl Board {
    pub fn from_status(s: &WorkspaceStatus) -> Board {
        let mut columns = Vec::with_capacity(s.stacks.len() + 1);

        columns.push(Column {
            kind: ColumnKind::Unassigned,
            title: "unassigned".into(),
            status: None,
            badges: Vec::new(),
            cards: s.unassigned_changes.iter().map(change_card).collect(),
            drop_target: UNASSIGNED_TARGET.into(),
        });

        for stack in &s.stacks {
            // The first branch is the tip of the stack; it names the lane and receives drops.
            let Some(top) = stack.branches.first() else {
                continue;
            };
            let multi = stack.branches.len() > 1;

            let mut cards: Vec<Card> = stack.assigned_changes.iter().map(change_card).collect();
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
                        title: commit.subject().to_string(),
                        subtitle,
                        badges,
                        author: Some(commit.author_name.clone()),
                        kind: CardKind::Commit,
                        group: group.clone(),
                    });
                }
            }

            let title = if multi {
                format!("{} +{}", top.name, stack.branches.len() - 1)
            } else {
                top.name.clone()
            };

            columns.push(Column {
                kind: ColumnKind::Stack,
                title,
                status: Some(top.branch_status),
                badges: branch_badges(top),
                cards,
                drop_target: top.cli_id.clone(),
            });
        }

        Board {
            columns,
            base_short_id: s.merge_base.short_id().to_string(),
            behind: s.upstream_state.behind,
            conflicted_files: s.conflicted_files.clone(),
        }
    }
}

fn change_card(c: &FileChange) -> Card {
    Card {
        cli_id: c.cli_id.clone(),
        title: c.file_path.clone(),
        subtitle: None,
        badges: vec![Badge::new(c.change_type.label(), Tone::Neutral)],
        author: None,
        kind: CardKind::Change,
        group: None,
    }
}

fn branch_badges(b: &Branch) -> Vec<Badge> {
    // The header dot already encodes push state by colour; the label spells it out, since
    // "needs force" and "unpushed" are the same hue family at a glance.
    let mut badges = vec![Badge::new(b.branch_status.label(), Tone::Neutral)];
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

    #[test]
    fn backlog_holds_unassigned_working_tree_files() {
        let b = Board::from_status(&sample());
        let backlog = &b.columns[0];
        let titles: Vec<&str> = backlog.cards.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["wip1.txt", "wip2.txt"]);
        assert!(backlog.cards.iter().all(|c| c.kind == CardKind::Change));
        assert_eq!(backlog.drop_target, UNASSIGNED_TARGET);
    }

    #[test]
    fn drop_target_is_the_branch_cli_id() {
        let s = sample();
        let b = Board::from_status(&s);
        let auth = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        let expected = &s.stacks[0].branches[0].cli_id;
        assert_eq!(
            &auth.drop_target, expected,
            "dropping onto a lane must rub onto its branch, which is what moves a commit"
        );
    }

    #[test]
    fn card_subtitle_lists_touched_files() {
        let b = Board::from_status(&sample());
        let auth = b.columns.iter().find(|c| c.title == "feat-auth").unwrap();
        let tip = &auth.cards[0];
        assert_eq!(tip.subtitle.as_deref(), Some("a2.txt"));
        assert_eq!(tip.author.as_deref(), Some("Dani"));
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
}
