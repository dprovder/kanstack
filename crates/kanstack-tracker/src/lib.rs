//! `kanstack-tracker`: the issue-tracker seam behind kanstack's queue.
//!
//! kanstack is agnostic about which agent runs in a pane and which multiplexer holds it, and
//! issue trackers get the same treatment: everything specific to *which* tracker holds the
//! work lives behind [`Tracker`], one implementor per tracker, one entry in [`KNOWN`]. The
//! orchestrator reads its queue and writes its reports through this, and the TUI's issues
//! view lists through it, so neither changes when a tracker is added.
//!
//! A leaf crate on purpose: it depends on nothing else in this workspace, so kanstack can use
//! it without depending on the orchestrator that sits above kanstack.

pub mod github;
pub mod local;

use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub use github::GithubTracker;
pub use local::LocalTracker;

/// A tracker's own identifier for an item: `44` locally, the issue number `44` on GitHub,
/// `LIN-123` on Linear. A string because remote trackers don't all use numbers; only the
/// backend that handed one out interprets it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub String);

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an item is in kanstack's own lifecycle. Each backend maps these onto whatever its
/// tracker has (labels, workflow states, transitions), so the orchestrator only ever deals in
/// these six. Wire words are lowercase, matched by [`ItemState::parse`] and printed back the
/// same way by `Display`, so a front-matter line, a CLI flag and a JSON field all agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemState {
    /// Suggested by an agent's report, waiting for a person to promote it.
    Proposed,
    /// Ready to be picked up. The default, since a person drafting an item means it.
    #[default]
    Backlog,
    Running,
    Failed,
    /// Its lanes are done and waiting on a person before landing.
    Review,
    Done,
}

impl ItemState {
    /// Every state, in lifecycle order: what an error lists when a word doesn't parse.
    pub const ALL: [ItemState; 6] = [
        ItemState::Proposed,
        ItemState::Backlog,
        ItemState::Running,
        ItemState::Failed,
        ItemState::Review,
        ItemState::Done,
    ];

    pub fn parse(word: &str) -> Option<ItemState> {
        ItemState::ALL.into_iter().find(|state| state.as_str() == word)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ItemState::Proposed => "proposed",
            ItemState::Backlog => "backlog",
            ItemState::Running => "running",
            ItemState::Failed => "failed",
            ItemState::Review => "review",
            ItemState::Done => "done",
        }
    }
}

impl std::fmt::Display for ItemState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One unit of work, as a tracker holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub title: String,
    pub state: ItemState,
    /// Which recipe the orchestrator runs for it. `None` leaves the choice to whoever starts it.
    pub recipe: Option<String>,
    /// Only on proposals: the item whose reports produced this one, so a person promoting it
    /// can see where it came from.
    pub from: Option<ItemId>,
    /// Everything below the title, uninterpreted Markdown. Reports are included locally; on
    /// remote trackers they're comments, and not part of it.
    pub body: String,
}

/// What [`Tracker::create`] needs: an [`Item`] before the tracker has given it an id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub title: String,
    pub body: String,
    pub recipe: Option<String>,
    pub from: Option<ItemId>,
    pub state: ItemState,
}

/// Which items [`Tracker::list`] returns. The default matches everything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFilter {
    /// Only items in this state. The queue asks for `backlog`; the issues view asks for all.
    pub state: Option<ItemState>,
}

impl ItemFilter {
    pub fn state(state: ItemState) -> ItemFilter {
        ItemFilter { state: Some(state) }
    }

    pub fn matches(&self, item: &Item) -> bool {
        self.state.is_none_or(|state| state == item.state)
    }
}

/// One issue tracker, opened for one repository (see [`KNOWN`]). Like the multiplexer
/// backends, a small set of verbs that knows nothing about lanes, recipes or runs.
pub trait Tracker: Sync {
    /// The name `KANSTACK_TRACKER` selects it by: `local`, `github`.
    fn id(&self) -> &'static str;

    /// Other names `KANSTACK_TRACKER` accepts for the same tracker.
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this tracker is usable for `repo`: its CLI is installed and authenticated,
    /// and the repo's remote points at it. Only a suggestion, for a setup wizard to offer:
    /// [`select`] never calls it, so a tracker is used only when `KANSTACK_TRACKER` names it.
    fn detect(&self, repo: &Path) -> bool;

    /// The items matching `filter`, oldest first.
    fn list(&self, filter: &ItemFilter) -> Result<Vec<Item>>;
    fn get(&self, id: &ItemId) -> Result<Item>;
    fn create(&self, draft: &Draft) -> Result<ItemId>;
    fn set_state(&self, id: &ItemId, state: ItemState) -> Result<()>;

    /// Appends a step's report to the item, as a comment on remote trackers.
    fn comment(&self, id: &ItemId, body: &str) -> Result<()>;

    /// The text to put in a commit or PR body to link it, e.g. "Closes #44", "Fixes LIN-123".
    /// Only a link: kanstack closes items itself with [`Tracker::set_state`] after landing,
    /// because closing keywords don't fire when a branch lands on a target that's never pushed.
    fn link_text(&self, id: &ItemId) -> Option<String>;
}

/// Opens one tracker for the repository at a path. Must not touch the tracker itself (no
/// network, no files written): [`select`] opens each candidate just to ask its name, and a
/// setup wizard opens each to ask [`Tracker::detect`].
pub type Open = fn(&Path) -> Result<Box<dyn Tracker>>;

/// Every tracker kanstack knows: what `KANSTACK_TRACKER` can name, and what a setup wizard
/// offers. Remote backends go ahead of `local`, so a wizard asking each to
/// [`Tracker::detect`] in turn suggests a remote tracker the repo is on before the fallback.
pub const KNOWN: &[Open] = &[github::open, local::open];

/// The tracker for `repo`: the one `KANSTACK_TRACKER` names, or `local` when it's unset or
/// blank. Never detected: a repo on GitHub keeps its queue in `local` until someone opts in,
/// so kanstack doesn't start reading and writing a team's real issues on its own. Nor is an
/// explicit choice checked with `detect`: a tracker that isn't set up should fail on first
/// use with its own CLI's error, not be silently swapped for another one.
pub fn select(repo: &Path) -> Result<Box<dyn Tracker>> {
    select_named(repo, std::env::var("KANSTACK_TRACKER").ok().as_deref())
}

/// [`select`], with the `KANSTACK_TRACKER` value passed in. Blank counts as unset.
fn select_named(repo: &Path, name: Option<&str>) -> Result<Box<dyn Tracker>> {
    let name = name.map(str::trim).filter(|name| !name.is_empty()).unwrap_or("local");
    let mut ids = Vec::new();
    for open in KNOWN {
        let tracker = open(repo)?;
        if tracker.id() == name || tracker.aliases().contains(&name) {
            return Ok(tracker);
        }
        ids.push(tracker.id());
    }
    bail!("KANSTACK_TRACKER is {name:?}, which isn't a known tracker (known: {})", ids.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_state_round_trips_through_its_wire_word() {
        for state in ItemState::ALL {
            assert_eq!(ItemState::parse(&state.to_string()), Some(state));
        }
        assert_eq!(ItemState::parse("Backlog"), None);
        assert_eq!(ItemState::parse("closed"), None);
    }

    #[test]
    fn select_is_local_when_unset_or_blank() {
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(select_named(repo.path(), None).unwrap().id(), "local");
        assert_eq!(select_named(repo.path(), Some("  ")).unwrap().id(), "local");
    }

    #[test]
    fn select_honors_a_named_tracker_and_rejects_an_unknown_one() {
        let repo = tempfile::tempdir().unwrap();
        assert_eq!(select_named(repo.path(), Some("local")).unwrap().id(), "local");
        let err = select_named(repo.path(), Some("jira")).err().unwrap().to_string();
        assert!(err.contains("\"jira\"") && err.contains("known: github, local"), "{err}");
    }

    /// A repo that `github` detects still gets `local` unless `github` is named, and naming it
    /// doesn't run `detect` either. The only test that sets `KANSTACK_GH_BIN`: it points any
    /// `gh` that select wrongly runs at the stand-in, never the real one, and its log shows
    /// none ran.
    #[test]
    fn select_uses_github_only_when_named() {
        let fake = github::fake::Fake::new();
        std::env::set_var("KANSTACK_GH_BIN", fake.gh());
        let detects = github::open(&fake.repo()).unwrap().detect(&fake.repo());
        let before = fake.log().len();
        let unset = select_named(&fake.repo(), None).map(|tracker| tracker.id());
        let blank = select_named(&fake.repo(), Some(" ")).map(|tracker| tracker.id());
        let named = select_named(&fake.repo(), Some("github")).map(|tracker| tracker.id());
        std::env::remove_var("KANSTACK_GH_BIN");
        assert!(detects, "the fake repo is one github detects");
        assert_eq!(unset.unwrap(), "local");
        assert_eq!(blank.unwrap(), "local");
        assert_eq!(named.unwrap(), "github");
        assert_eq!(fake.log().len(), before, "select ran gh:\n{}", fake.log());
    }

    /// The only test here that sets `KANSTACK_TRACKER`, so it can't race another.
    #[test]
    fn select_reads_kanstack_tracker() {
        let repo = tempfile::tempdir().unwrap();
        std::env::set_var("KANSTACK_TRACKER", "nope");
        let unknown = select(repo.path()).err();
        std::env::set_var("KANSTACK_TRACKER", "local");
        let local = select(repo.path()).map(|tracker| tracker.id());
        std::env::remove_var("KANSTACK_TRACKER");
        assert!(unknown.is_some());
        assert_eq!(local.unwrap(), "local");
    }
}
