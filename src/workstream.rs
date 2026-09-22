//! The registry behind the `kanstack spawn/send/status/focus/stop` subcommands.
//!
//! A [`Splitter`]'s pane handles — which pane belongs to which branch — live only in the
//! memory of whichever process opened them, so
//! a second `kanstack` invocation — an agent in another pane running `kanstack send`, say —
//! can't see a pane the first one opened. A [`Workstream`] is the durable half of that
//! handle: which branch it is, which pane it sits in, which agent is running there. Both the
//! board and the subcommands record into it, and re-seed a fresh [`Splitter`] from it with
//! [`Registry::adopt_into`].
//!
//! One file per repository (see [`state_path`]), so two repositories can each have a
//! `fix-login` branch without one's pane answering for the other's.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::procs::Pids;
use crate::report::Reports;
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
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("workstreams-{key:016x}.json")))
}

/// The advisory-lock file guarding `repo`'s registry — a sibling of [`state_path`], not the
/// registry file itself, so a lock attempt never has to worry about the file it's locking
/// being renamed out from under it mid-hold (which [`Registry::save`]'s temp-file-and-rename
/// would otherwise do to any lock taken on the registry file directly).
fn lock_path(repo: &Path) -> Option<PathBuf> {
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("workstreams-{key:016x}.lock")))
}

/// Where the reports agents make about themselves for the repository at `repo` live — one
/// small file per branch, see `crate::report`. A directory beside the registry rather than
/// a field in it: hooks write these often and from several processes at once, and the
/// registry is rewritten whole, so sharing it would lose updates.
pub fn reports_dir(repo: &Path) -> Option<PathBuf> {
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("reports-{key:016x}")))
}

/// Where the shell pids of `repo`'s panes are recorded, when they are tracked — one small
/// file per branch, see `crate::procs::Pids`. A directory beside the reports, for the same
/// reason: the pane's own shell writes it, from the launch line, in a process that never
/// loads the registry.
pub fn pids_dir(repo: &Path) -> Option<PathBuf> {
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("pids-{key:016x}")))
}

/// Where the append-only events log for `repo` lives — one JSONL file, see `crate::events`. A
/// sibling of `reports_dir`/`pids_dir`, not inside either: it's one file, not one per branch,
/// so it doesn't need its own directory.
pub fn events_path(repo: &Path) -> Option<PathBuf> {
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("events-{key:016x}.jsonl")))
}

/// Where `repo`'s file-edit claims live — one small file per claimed path, see
/// `crate::claims`. A sibling of `reports_dir`/`pids_dir`, same reasoning: a hook writes one
/// of these on every file-editing tool call, from whichever lane's pane is running it, so
/// sharing the registry would lose updates the same way reports would.
pub fn claims_dir(repo: &Path) -> Option<PathBuf> {
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("claims-{key:016x}")))
}

/// Where `repo`'s Gemini CLI `PreToolUse`-equivalent hook settings live — one small
/// `settings.json` per branch, written at launch time and pointed at via
/// `GEMINI_CLI_SYSTEM_SETTINGS_PATH` (see `crate::harness::gemini::Gemini::status_hooks`).
/// A sibling of `claims_dir`/`reports_dir`, same repository key, its own directory since it's
/// one file per branch, not per repository. Unlike `claims_dir`, this is *not* released on
/// `Reports::write`'s `Idle` — Gemini reads the file once at its own process startup, not on
/// every turn, so removing it while merely idle-between-turns would be pointless (already
/// loaded for that process's lifetime) and risks breaking a later `kanstack send` if Gemini
/// somehow re-reads it later. It is released on `Reports::forget` instead, the same
/// stop/respawn choke point `Claims::release_all` also hangs off — see [`Reports::forget`].
pub fn gemini_hooks_dir(repo: &Path) -> Option<PathBuf> {
    let (dir, key) = repo_state(repo)?;
    Some(dir.join(format!("gemini-hooks-{key:016x}")))
}

/// The state directory, and the key identifying `repo` within it.
fn repo_state(repo: &Path) -> Option<(PathBuf, u64)> {
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
    Some((dir, fnv1a(repo.to_string_lossy().as_bytes())))
}

/// FNV-1a rather than `DefaultHasher`, whose output is explicitly unspecified across Rust
/// releases — an upgrade must not orphan every registry on disk.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

/// An exclusive hold on `repo`'s registry, for the duration of a load-mutate-save that must
/// not interleave with another process's. Backed by `flock(2)` on a sibling lock file (not
/// the registry itself — see [`lock_path`]), which the kernel releases the moment this is
/// dropped, closed handle and all, so a command that errors partway through still unlocks;
/// there is deliberately no manual unlock to forget to call.
pub struct RegistryLock(#[allow(dead_code, reason = "held only for its Drop, which releases the flock")] File);

impl RegistryLock {
    fn acquire(repo: &Path) -> Result<Self> {
        let path = lock_path(repo).ok_or_else(|| anyhow::anyhow!("no home directory to keep workstreams in"))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            // Its content is never read or written — only the fd matters, for `flock` — so
            // there's nothing to truncate.
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // Blocks until whichever other `kanstack` process holds this releases it, rather than
        // racing it for the registry file. Same-process double-acquire (e.g. nested
        // `with_lock` calls) would deadlock here, which is intentional: it means the code
        // grew a hold longer than intended.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("locking {}", path.display()));
        }
        Ok(RegistryLock(file))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    path: Option<PathBuf>,
    /// What agents in this repository have reported about themselves, handed to a
    /// [`Splitter`] by [`Self::adopt_into`].
    reports: Reports,
    /// Where panes' shell pids are recorded, handed to a [`Splitter`] by
    /// [`Self::adopt_into`] beside `reports`.
    pids: Pids,
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
        Ok(Registry { path, reports: Reports::for_repo(repo), pids: Pids::for_repo(repo), workspace, workstreams })
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

    /// Runs `f` against `repo`'s registry, freshly loaded, saving whatever it left behind —
    /// all under one [`RegistryLock`] hold, so two `kanstack` processes doing this at once
    /// serialize instead of one's save clobbering the other's (the load-modify-save race a
    /// bare [`Self::load`]/[`Self::save`] pair can't protect against on its own). `f` failing
    /// skips the save, same as a bare `load`-then-`?`-riddled-mutation-then-`save` would.
    ///
    /// A read that never saves (`status`, the board's own poll) can keep calling
    /// [`Self::load`] directly — nothing to serialize when there's no write to lose.
    pub fn with_lock<T>(repo: &Path, f: impl FnOnce(&mut Registry) -> Result<T>) -> Result<T> {
        let _lock = RegistryLock::acquire(repo)?;
        let mut registry = Self::load(repo)?;
        let result = f(&mut registry)?;
        registry.save()?;
        Ok(result)
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
        splitter.set_reports(self.reports.clone());
        splitter.set_pids(self.pids.clone());
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
    let _ = Registry::with_lock(repo, |registry| {
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
        Ok(())
    });
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

    /// Reports and pids are both one-file-per-branch directories in the state directory, keyed by
    /// the repository, so each repository's are its own.
    #[test]
    fn reports_and_pids_live_in_sibling_directories_keyed_by_the_repository() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("KANSTACK_STATE_PATH", "/state");
        let (reports, pids) = (reports_dir(Path::new("/repo/a")).unwrap(), pids_dir(Path::new("/repo/a")).unwrap());
        assert_eq!(reports.parent(), Some(Path::new("/state")));
        assert_eq!(pids.parent(), Some(Path::new("/state")));
        assert_eq!(
            reports.file_name().unwrap().to_string_lossy().strip_prefix("reports-"),
            pids.file_name().unwrap().to_string_lossy().strip_prefix("pids-"),
            "the same key: {reports:?} {pids:?}"
        );
        assert_ne!(pids, pids_dir(Path::new("/repo/b")).unwrap());
        std::env::remove_var("KANSTACK_STATE_PATH");
    }

    /// The events log is also keyed by the repository, a sibling of `reports`/`pids` — same
    /// key, same directory, but one file rather than a directory of its own.
    #[test]
    fn the_events_log_is_a_sibling_file_keyed_by_the_repository() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("KANSTACK_STATE_PATH", "/state");
        let (reports, events) = (reports_dir(Path::new("/repo/a")).unwrap(), events_path(Path::new("/repo/a")).unwrap());
        assert_eq!(events.parent(), Some(Path::new("/state")));
        assert_eq!(
            reports.file_name().unwrap().to_string_lossy().strip_prefix("reports-"),
            events.file_name().unwrap().to_string_lossy().strip_suffix(".jsonl").and_then(|s| s.strip_prefix("events-")),
        );
        assert_ne!(events, events_path(Path::new("/repo/b")).unwrap());
        std::env::remove_var("KANSTACK_STATE_PATH");
    }

    /// Claims are also keyed by the repository, a sibling of `reports`/`pids`/`events` — same
    /// key, its own directory (one file per claimed path, not per branch).
    #[test]
    fn the_claims_directory_is_a_sibling_keyed_by_the_repository() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("KANSTACK_STATE_PATH", "/state");
        let (reports, claims) = (reports_dir(Path::new("/repo/a")).unwrap(), claims_dir(Path::new("/repo/a")).unwrap());
        assert_eq!(claims.parent(), Some(Path::new("/state")));
        assert_eq!(
            reports.file_name().unwrap().to_string_lossy().strip_prefix("reports-"),
            claims.file_name().unwrap().to_string_lossy().strip_prefix("claims-"),
        );
        assert_ne!(claims, claims_dir(Path::new("/repo/b")).unwrap());
        std::env::remove_var("KANSTACK_STATE_PATH");
    }

    /// Same shape, for Gemini's hook settings files.
    #[test]
    fn the_gemini_hooks_directory_is_a_sibling_keyed_by_the_repository() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("KANSTACK_STATE_PATH", "/state");
        let (reports, gemini) = (reports_dir(Path::new("/repo/a")).unwrap(), gemini_hooks_dir(Path::new("/repo/a")).unwrap());
        assert_eq!(gemini.parent(), Some(Path::new("/state")));
        assert_eq!(
            reports.file_name().unwrap().to_string_lossy().strip_prefix("reports-"),
            gemini.file_name().unwrap().to_string_lossy().strip_prefix("gemini-hooks-"),
        );
        assert_ne!(gemini, gemini_hooks_dir(Path::new("/repo/b")).unwrap());
        std::env::remove_var("KANSTACK_STATE_PATH");
    }

    /// The subcommands seed a fresh splitter from the registry, and the pid directory has to come
    /// with it or a pane they spawn would never record its shell.
    #[test]
    fn adopting_into_a_splitter_hands_it_the_repositorys_pid_directory() {
        use crate::harness::HarnessConfig;
        use crate::mux::fake::FakeMux;
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-pids-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        std::env::set_var("KANSTACK_TRACK_PIDS", "1");

        let mux = FakeMux::new();
        let mut splitter = Splitter::new(mux.clone(), HarnessConfig::new("claude"));
        Registry::load(Path::new("/repo/a")).unwrap().adopt_into(&mut splitter);
        splitter.spawn_harness(Path::new("/repo/a"), "feat-a", None).unwrap();
        let line = mux.lines().remove(0);
        let expected = pids_dir(Path::new("/repo/a")).unwrap();
        assert!(line.contains(&format!("'{}/", expected.display())), "{line}");
        assert!(expected.is_dir(), "made before the pane's shell writes into it");

        std::env::remove_var("KANSTACK_TRACK_PIDS");
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

    // Concurrency safety: `Registry::with_lock` exists because a bare `load`, mutate, `save`
    // is a classic lost-update race between two `kanstack` processes. The next test
    // reproduces that race directly, to show what's being fixed; the one after shows
    // `with_lock` closing it even under the same forced overlap.

    /// Without any lock, two processes that each load before either saves clobber one
    /// another: the second `save` has no idea the first one happened, so it writes over it.
    /// Modeled sequentially rather than with real threads — two processes never share a pid,
    /// but `save`'s temp file name is derived from one, so racing this same test in threads
    /// would trip over that instead of the lost-update bug it's meant to demonstrate.
    #[test]
    fn without_a_lock_two_concurrent_load_mutate_saves_can_lose_an_update() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let repo = Path::new("/repo/race");
        Registry::load(repo).unwrap().save().unwrap();

        // Both "processes" load the same pre-mutation snapshot before either has saved.
        let mut one = Registry::load(repo).unwrap();
        let mut two = Registry::load(repo).unwrap();
        one.upsert(ws("one", Some("one")));
        two.upsert(ws("two", Some("two")));
        one.save().unwrap();
        two.save().unwrap();

        let after = Registry::load(repo).unwrap();
        assert_eq!(after.workstreams.len(), 1, "two's save clobbered one's update: {after:?}");

        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same forced overlap as above — a sleep between load and save, wider than any real
    /// scheduling accident would need — but now through `with_lock`. Three mutations, echoing
    /// `kanstack spawn one`/`two`/`three` launched at once: every one of them must still be on
    /// disk afterwards.
    #[test]
    fn with_lock_serializes_concurrent_mutations_so_none_are_lost() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let repo = Path::new("/repo/locked");
        Registry::load(repo).unwrap().save().unwrap();

        let branches = ["one", "two", "three"];
        std::thread::scope(|scope| {
            for branch in branches {
                scope.spawn(move || {
                    Registry::with_lock(repo, |registry| {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        registry.upsert(ws(branch, Some(branch)));
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });

        let after = Registry::load(repo).unwrap();
        for branch in branches {
            assert!(after.get(branch).is_some(), "{branch} was lost to a concurrent save: {after:?}");
        }
        assert_eq!(after.workstreams.len(), branches.len());

        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A closure that errors partway through must neither save its half-done mutation nor
    /// leave the lock held — the latter checked by having a second `with_lock` prove it can
    /// still get in, bounded so a regression hangs this test instead of the whole suite.
    #[test]
    fn a_failed_mutation_is_not_saved_and_its_lock_is_released() {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-workstream-lockerr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        let repo = Path::new("/repo/lockerr");

        let err = Registry::with_lock(repo, |registry| -> Result<()> {
            registry.upsert(ws("doomed", Some("%1")));
            anyhow::bail!("simulated failure partway through")
        });
        assert!(err.is_err());
        assert!(Registry::load(repo).unwrap().get("doomed").is_none(), "a failed mutation must not be saved");

        let (tx, rx) = std::sync::mpsc::channel();
        let repo_owned = repo.to_path_buf();
        std::thread::spawn(move || {
            let _ = Registry::with_lock(&repo_owned, |registry| {
                registry.upsert(ws("ok", Some("%2")));
                Ok(())
            });
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("a lock left over from the failed call would hang this instead of releasing");
        assert!(Registry::load(repo).unwrap().get("ok").is_some());

        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
