//! The registry behind the `kanstack spawn/send/status/focus/stop` subcommands.
//!
//! A backend's pane handles (`crate::cmux::Cmux::panes`, `crate::tmux::Tmux::panes`,
//! `crate::orca::Orca::panes`) live only in the memory of whichever process opened them, so
//! a second `kanstack` invocation — an agent in another pane running `kanstack send`, say —
//! can't see a pane the first one opened. A [`Workstream`] is the durable half of that
//! handle: which branch it is, which pane it sits in, which agent is running there. Both the
//! board and the subcommands record into it, and re-seed a fresh backend from it with
//! [`Registry::adopt_into`].
//!
//! One file per repository (see [`state_path`]), so two repositories can each have a
//! `fix-login` branch without one's pane answering for the other's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::splitter::Splitter;

macro_rules! id_newtype {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_newtype!(
    /// A GitButler branch name — what `but` itself uses to address a branch.
    BranchId
);
id_newtype!(
    /// The split backend's own identifier for a pane: tmux's `%3`, cmux's `surface:12`, or
    /// Orca's terminal handle (runtime-scoped, so it stops resolving if Orca restarts).
    /// Doubles as the "session" a subcommand target may name instead of a branch.
    PaneId
);
id_newtype!(
    /// Which harness is running in the pane, as it would be typed at a shell — `claude`,
    /// `codex`, or whatever `KANSTACK_HARNESS` holds.
    AgentId
);
id_newtype!(
    /// An opaque reference to the unit of work this workstream is for (an issue, a ticket).
    /// kanstack only stores it; nothing here interprets it.
    WorkItemRef
);

/// One branch's worth of parallel work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workstream {
    pub branch_id: BranchId,
    pub pane_id: Option<PaneId>,
    pub agent: Option<AgentId>,
    pub item: Option<WorkItemRef>,
}

/// Where the registry for the repository at `repo` lives: under `KANSTACK_STATE_PATH` if
/// set — also the test seam, same as `KANSTACK_CONFIG_PATH` for `crate::config` — else
/// `$XDG_STATE_HOME/kanstack`, else `$HOME/.local/state/kanstack`. `None` only when none of
/// those can be resolved.
pub fn state_path(repo: &Path) -> Option<PathBuf> {
    let dir = std::env::var_os("KANSTACK_STATE_PATH")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
                .map(|base| base.join("kanstack"))
        })?;
    // Canonicalized, and lifted to the repository root, so `-C .`, `-C /abs/path` and a
    // command run from a subdirectory all land on the same file.
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let repo = repo.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&repo);
    Some(dir.join(format!("workstreams-{:016x}.json", fnv1a(repo.to_string_lossy().as_bytes()))))
}

/// FNV-1a rather than `DefaultHasher`, whose output is explicitly unspecified across Rust
/// releases — an upgrade must not orphan every registry on disk.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    path: Option<PathBuf>,
    /// The terminal-multiplexer workspace this repository's panes live in (cmux's
    /// `workspace:1`), so a `spawn` run from a shell whose environment has drifted still
    /// opens its pane beside the others rather than wherever that environment points.
    pub workspace: Option<String>,
    pub workstreams: Vec<Workstream>,
}

/// What's in the file. The first version was a bare array of workstreams, with no room for
/// anything else; still read, and rewritten in this shape the next time it's saved.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum OnDisk {
    Current {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
        workstreams: Vec<Workstream>,
    },
    Legacy(Vec<Workstream>),
}

impl Registry {
    /// Reads the registry for `repo`. A missing file is an empty registry, not an error —
    /// every repository starts that way. An unreadable or malformed one is an error: quietly
    /// starting over would orphan every pane it was tracking.
    pub fn load(repo: &Path) -> Result<Self> {
        let path = state_path(repo);
        let on_disk = match path.as_ref().map(std::fs::read_to_string) {
            Some(Ok(raw)) => Some(
                serde_json::from_str::<OnDisk>(&raw)
                    .with_context(|| format!("{} is not a valid workstream registry", path.as_ref().unwrap().display()))?,
            ),
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => None,
            Some(Err(e)) => {
                return Err(e).with_context(|| format!("reading {}", path.as_ref().unwrap().display()))
            }
            None => None,
        };
        let (workspace, workstreams) = match on_disk {
            Some(OnDisk::Current { workspace, workstreams }) => (workspace, workstreams),
            Some(OnDisk::Legacy(workstreams)) => (None, workstreams),
            None => (None, Vec::new()),
        };
        Ok(Registry { path, workspace, workstreams })
    }

    /// Writes via a temp file and rename, so a reader in another pane never sees a
    /// half-written registry.
    pub fn save(&self) -> Result<()> {
        let path = self.path.as_ref().ok_or_else(|| anyhow::anyhow!("no home directory to keep workstreams in"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let on_disk = OnDisk::Current { workspace: self.workspace.clone(), workstreams: self.workstreams.clone() };
        std::fs::write(&tmp, serde_json::to_string_pretty(&on_disk)? + "\n")?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn get(&self, branch: &str) -> Option<&Workstream> {
        self.workstreams.iter().find(|w| w.branch_id.0 == branch)
    }

    /// Finds the workstream a subcommand's `<branch|session>` argument names: a branch
    /// first, then a pane id.
    pub fn resolve(&self, target: &str) -> Option<&Workstream> {
        self.get(target)
            .or_else(|| self.workstreams.iter().find(|w| w.pane_id.as_ref().is_some_and(|p| p.0 == target)))
    }

    /// Adds `workstream`, replacing any existing one for the same branch.
    pub fn upsert(&mut self, workstream: Workstream) {
        match self.workstreams.iter_mut().find(|w| w.branch_id == workstream.branch_id) {
            Some(slot) => *slot = workstream,
            None => self.workstreams.push(workstream),
        }
    }

    pub fn remove(&mut self, branch: &str) -> Option<Workstream> {
        let i = self.workstreams.iter().position(|w| w.branch_id.0 == branch)?;
        Some(self.workstreams.remove(i))
    }

    /// Seeds a freshly discovered `splitter` with every recorded pane, so its own
    /// `send_task`/`poll_statuses`/`focus`/`stop` work on panes another process opened.
    pub fn adopt_into(&self, splitter: &mut Splitter) {
        for w in &self.workstreams {
            if let Some(pane) = &w.pane_id {
                splitter.adopt(&w.branch_id.0, &pane.0);
            }
        }
    }
}

/// Records a pane the board just opened, so the subcommands can find it. Best-effort: a
/// registry that can't be written must not undo a pane that did open.
pub fn record_spawn(repo: &Path, branch: &str, pane_id: &str, agent: Option<&str>, workspace: Option<&str>) {
    let Ok(mut registry) = Registry::load(repo) else { return };
    if let Some(workspace) = workspace {
        registry.workspace = Some(workspace.to_string());
    }
    let existing = registry.get(branch).cloned();
    registry.upsert(Workstream {
        branch_id: BranchId(branch.to_string()),
        pane_id: Some(PaneId(pane_id.to_string())),
        agent: agent.map(|a| AgentId(a.to_string())),
        item: existing.and_then(|w| w.item),
    });
    let _ = registry.save();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(branch: &str, pane: Option<&str>) -> Workstream {
        Workstream {
            branch_id: BranchId(branch.into()),
            pane_id: pane.map(|p| PaneId(p.into())),
            agent: Some(AgentId("claude".into())),
            item: None,
        }
    }

    #[test]
    fn upsert_replaces_by_branch_and_remove_returns_it() {
        let mut r = Registry::default();
        r.upsert(ws("a", Some("%1")));
        r.upsert(ws("b", Some("%2")));
        r.upsert(ws("a", Some("%9")));
        assert_eq!(r.workstreams.len(), 2);
        assert_eq!(r.get("a").unwrap().pane_id, Some(PaneId("%9".into())));
        assert!(r.remove("a").is_some());
        assert!(r.remove("a").is_none());
    }

    #[test]
    fn resolve_accepts_a_branch_or_a_pane_id() {
        let mut r = Registry::default();
        r.upsert(ws("fix-login", Some("surface:12")));
        assert_eq!(r.resolve("fix-login").unwrap().branch_id.0, "fix-login");
        assert_eq!(r.resolve("surface:12").unwrap().branch_id.0, "fix-login");
        assert!(r.resolve("nope").is_none());
    }

    #[test]
    fn a_branch_name_wins_over_a_pane_id_it_shadows() {
        let mut r = Registry::default();
        r.upsert(ws("%1", Some("%2")));
        r.upsert(ws("other", Some("%1")));
        assert_eq!(r.resolve("%1").unwrap().branch_id.0, "%1");
    }

    #[test]
    fn the_registry_round_trips_through_disk_and_is_per_repo() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);

        let mut a = Registry::load(Path::new("/repo/a")).unwrap();
        assert!(a.workstreams.is_empty(), "a missing file is an empty registry");
        a.upsert(Workstream { item: Some(WorkItemRef("GH-4".into())), ..ws("fix-login", Some("%3")) });
        a.save().unwrap();

        let again = Registry::load(Path::new("/repo/a")).unwrap();
        assert_eq!(again.workstreams, a.workstreams);
        assert!(Registry::load(Path::new("/repo/b")).unwrap().workstreams.is_empty());

        record_spawn(Path::new("/repo/a"), "fix-login", "%7", Some("codex"), Some("workspace:1"));
        let after = Registry::load(Path::new("/repo/a")).unwrap();
        let w = after.get("fix-login").unwrap();
        assert_eq!(w.pane_id, Some(PaneId("%7".into())));
        assert_eq!(w.item, Some(WorkItemRef("GH-4".into())), "re-recording keeps the work item");
        assert_eq!(after.workspace.as_deref(), Some("workspace:1"), "and pins the workspace");

        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_registry_is_an_error_not_a_silent_reset() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let path = state_path(Path::new("/repo/c")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "{ not json").unwrap();
        assert!(Registry::load(Path::new("/repo/c")).is_err());
        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first registry format was a bare array. Files written then must still load, with
    /// no workspace pinned, and come back out in the current shape.
    #[test]
    fn a_registry_from_the_first_format_still_loads() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let path = state_path(Path::new("/repo/d")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, r#"[{"branch_id":"a","pane_id":"surface:1","agent":"claude","item":null}]"#).unwrap();

        let mut r = Registry::load(Path::new("/repo/d")).unwrap();
        assert_eq!(r.workstreams.len(), 1);
        assert_eq!(r.workspace, None);
        r.workspace = Some("workspace:2".into());
        r.save().unwrap();
        let again = Registry::load(Path::new("/repo/d")).unwrap();
        assert_eq!((again.workspace.as_deref(), again.workstreams.len()), (Some("workspace:2"), 1));

        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
