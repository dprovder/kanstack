//! Durable run state: one `state.json` snapshot per run, so a killed or restarted orchestrator
//! can resume every run where it left off (see this crate's README, "Durable runs").
//!
//! ```text
//! <kanstack state dir>/recipe-<key>/
//!   orchestrator.lock           flock: one orchestrator per repo (see `crate::lock`)
//!   runs/<run-id>/state.json    one run's state, one writer
//! ```
//!
//! **Snapshots, not a journal.** Transitions are minutes apart, so the whole file is rewritten
//! on each one: temp file, fsync, rename, fsync the directory ([`write_atomically`]). A reader
//! therefore only ever sees the old snapshot or the new one, never half of either.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::graph::StepState;
use crate::recipe::{DoneWhen, Recipe};

/// The `version` written into every `state.json`. Bumped only for a change an older reader
/// would misread; new optional fields don't need it.
pub const STATE_VERSION: u32 = 1;

/// Where a step is in its life. `launching`, `running` and `verifying` are all "in flight":
/// the step has been (or is about to be) handed to a lane, and resume has to look at that
/// lane before doing anything else with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    /// Recorded, with `attempt` already incremented, *before* `kanstack spawn`/`send` runs —
    /// so a crash mid-launch still uses up an attempt and can't loop forever.
    Launching,
    /// The spawn/send returned; the step's lane is working on it.
    Running,
    /// The lane went idle and the step's `verify` commands are running.
    Verifying,
    Done,
    Failed,
}

impl StepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            StepStatus::Pending => "pending",
            StepStatus::Launching => "launching",
            StepStatus::Running => "running",
            StepStatus::Verifying => "verifying",
            StepStatus::Done => "done",
            StepStatus::Failed => "failed",
        }
    }

    pub fn in_flight(self) -> bool {
        matches!(self, StepStatus::Launching | StepStatus::Running | StepStatus::Verifying)
    }

    /// The scheduler's coarser four-state view ([`crate::graph`] only needs to know what's
    /// runnable): every in-flight status is `Running` there.
    pub fn graph_state(self) -> StepState {
        match self {
            StepStatus::Pending => StepState::Pending,
            StepStatus::Launching | StepStatus::Running | StepStatus::Verifying => StepState::Running,
            StepStatus::Done => StepState::Complete,
            StepStatus::Failed => StepState::Failed,
        }
    }
}

/// One step's persisted state. `base` and `tip` are the step's branch's commit when it
/// started and when it finished, read from git; either is `null` when the branch didn't exist
/// yet (or git couldn't be asked).
///
/// The fields after `last_error` came with `done_when` and retries, and default to `null` or
/// `0` in a snapshot written before them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub status: StepStatus,
    pub attempt: u32,
    pub branch: Option<String>,
    pub pane: Option<String>,
    pub base: Option<String>,
    pub tip: Option<String>,
    /// Seconds since the Unix epoch, when the current attempt was recorded as `launching`.
    pub started_at: Option<u64>,
    pub last_error: Option<String>,
    /// The events log's length when the current attempt launched: only notes past it are
    /// this attempt's report. `null` for a step whose `done_when` doesn't accept a note.
    #[serde(default)]
    pub notes_from: Option<u64>,
    /// The text of the agent's `kanstack note` report, once one arrived.
    #[serde(default)]
    pub report: Option<String>,
    /// Set while the step's lane is trusted idle but its `done_when` isn't met yet: why it
    /// isn't done.
    #[serde(default)]
    pub waiting_on: Option<DoneWhen>,
    /// How many of `retry_budget` this step has used: failed attempts that were retried.
    /// Transport errors (a `spawn` or `send` that didn't go through) never count.
    #[serde(default)]
    pub retries: u32,
    /// How many retries the step gets, resolved when the run started (see
    /// [`crate::recipe::RetryBudgets`]). `0` for a run started without a budget.
    #[serde(default)]
    pub retry_budget: u32,
    /// Set while a retry is decided but not launched yet: how the next launch goes. Persisted
    /// so a crash in between still retries the same way.
    #[serde(default)]
    pub retry: Option<Retry>,
}

/// How a failed attempt is retried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Retry {
    /// `verify` failed: send `message` (the failing command and its output's tail) into the
    /// step's pane, so the agent fixes forward with its context intact.
    FixForward { message: String },
    /// The pane died or was gone: respawn the step's agent on its branch, with its prompt
    /// followed by `note`.
    Respawn { note: String },
}

impl StepRecord {
    /// `retried 1/2`, once the step has a budget or has used any of it.
    pub fn retry_label(&self) -> Option<String> {
        (self.retries > 0 || self.retry_budget > 0).then(|| format!("retried {}/{}", self.retries, self.retry_budget))
    }
}

impl Default for StepRecord {
    fn default() -> Self {
        StepRecord {
            status: StepStatus::Pending,
            attempt: 0,
            branch: None,
            pane: None,
            base: None,
            tip: None,
            started_at: None,
            last_error: None,
            notes_from: None,
            report: None,
            waiting_on: None,
            retries: 0,
            retry_budget: 0,
            retry: None,
        }
    }
}

/// Whether a run still has work to do. Only `running` runs are picked up by `resume`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Done,
    Failed,
}

impl RunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Done => "done",
            RunStatus::Failed => "failed",
        }
    }
}

/// One run's whole `state.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunState {
    pub version: u32,
    pub id: String,
    /// The tracker item this run works on: set by `serve`, `null` for a `run` of a recipe file.
    pub item: Option<String>,
    /// The recipe's name: its file stem.
    pub recipe: String,
    /// Where the recipe was read from, so `resume` can read it again.
    pub recipe_path: PathBuf,
    /// [`recipe_hash`] of the recipe file when the run started. `resume` refuses a run whose
    /// recipe no longer matches, unless forced.
    pub recipe_hash: String,
    /// The directory `run` was invoked in: where `verify` commands run and git is asked for
    /// branch tips, on the first run and on every resume alike.
    pub workdir: PathBuf,
    pub status: RunStatus,
    pub steps: BTreeMap<String, StepRecord>,
    /// Each branch's current agent: the agent of the step last launched on it. An `on:` step
    /// with no `spawn:` hands the branch off when its own agent differs, and continues in the
    /// pane when it's the same.
    #[serde(default)]
    pub branch_agents: BTreeMap<String, String>,
    /// Put in front of every branch the run's steps resolve to, so two items running the same
    /// recipe never share a lane. `serve` sets it per item; empty for a plain `run`.
    #[serde(default)]
    pub branch_prefix: String,
    /// The steps whose outcome has been posted to the run's item, so a restarted `serve`
    /// posts each one once.
    #[serde(default)]
    pub reported: BTreeSet<String>,
    /// The run has ended and its item's state has been set to match.
    #[serde(default)]
    pub settled: bool,
    /// The item's title and body as they were when the run started: every launch prompt
    /// starts with them (see [`crate::aggregate::with_work_item`]), and a resume uses this
    /// copy, so editing the issue later doesn't change what the agents are told. `null` for a
    /// plain `run`, and for a run started before it was recorded.
    #[serde(default)]
    pub work_item: Option<WorkItem>,
}

/// What a run's agents are told about its item: a copy taken when the run started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    pub title: String,
    pub body: String,
}

impl RunState {
    /// A fresh run with every one of `recipe`'s steps `pending`.
    pub fn new(id: impl Into<String>, recipe: &Recipe, recipe_path: &Path, recipe_hash: String, workdir: PathBuf) -> Self {
        RunState {
            version: STATE_VERSION,
            id: id.into(),
            item: None,
            recipe: recipe_name(recipe_path),
            recipe_path: recipe_path.to_path_buf(),
            recipe_hash,
            workdir,
            status: RunStatus::Running,
            steps: recipe.steps.keys().map(|id| (id.clone(), StepRecord::default())).collect(),
            branch_agents: BTreeMap::new(),
            branch_prefix: String::new(),
            reported: BTreeSet::new(),
            settled: false,
            work_item: None,
        }
    }

    /// Every step's [`StepStatus::graph_state`], for [`crate::graph::runnable_steps`].
    pub fn graph_states(&self) -> HashMap<String, StepState> {
        self.steps.iter().map(|(id, rec)| (id.clone(), rec.status.graph_state())).collect()
    }
}

/// FNV-1a of the recipe file's bytes, as 16 hex digits. It only has to notice an edit, not
/// resist a forged one, so it reuses the hash kanstack already keys its state files with.
pub fn recipe_hash(source: &str) -> String {
    format!("{:016x}", fnv1a(source.as_bytes()))
}

fn recipe_name(recipe_path: &Path) -> String {
    recipe_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "recipe".to_string())
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The directory holding one repository's runs and its orchestrator lock.
pub struct RunStore {
    root: PathBuf,
}

impl RunStore {
    /// A store rooted at `root` exactly (the `recipe-<key>` directory) — the test seam.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        RunStore { root: root.into() }
    }

    /// The store for the repository at `repo`: `<kanstack state dir>/recipe-<key>`, with the
    /// same state dir and key kanstack uses for that repository's registry. `None` only when
    /// no state dir can be resolved.
    pub fn for_repo(repo: &Path) -> Option<Self> {
        let (dir, key) = repo_state(repo)?;
        Some(RunStore::at(dir.join(format!("recipe-{key:016x}"))))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn lock_path(&self) -> PathBuf {
        self.root.join("orchestrator.lock")
    }

    pub fn state_path(&self, run_id: &str) -> PathBuf {
        self.root.join("runs").join(run_id).join("state.json")
    }

    pub fn save(&self, run: &RunState) -> io::Result<()> {
        let mut json = serde_json::to_vec_pretty(run).map_err(io::Error::other)?;
        json.push(b'\n');
        write_atomically(&self.state_path(&run.id), &json)
    }

    pub fn load(&self, run_id: &str) -> io::Result<RunState> {
        if !is_plain_name(run_id) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a run id: {run_id:?}")));
        }
        let path = self.state_path(run_id);
        let raw = fs::read_to_string(&path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))
    }

    /// Every run id with a `state.json`, sorted. Ids start with the run's start time, so this
    /// is oldest first.
    pub fn run_ids(&self) -> io::Result<Vec<String>> {
        let dir = self.root.join("runs");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(String::from) else { continue };
            if is_plain_name(&id) && entry.path().join("state.json").is_file() {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// A new, unused run id: `<unix seconds>-<recipe name>`, with `-2`, `-3`… on a collision.
    /// Only unique while the caller holds the orchestrator lock, which every writer does.
    pub fn new_run_id(&self, recipe_name: &str) -> String {
        let name: String = recipe_name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
            .collect();
        let base = format!("{}-{name}", now_secs());
        let mut id = base.clone();
        let mut n = 2;
        while self.root.join("runs").join(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}

/// A run id names a directory under `runs/`, so it must not be able to climb out of it.
fn is_plain_name(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\'])
}

/// Replaces `path` with `bytes` so that a crash at any point leaves either the old file or
/// the new one: write a sibling temp file, fsync it, rename it over `path`, fsync the
/// directory (without which the rename itself may not survive a power loss).
pub fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    File::open(dir)?.sync_all()
}

// Mirrors `repo_state` and `fnv1a` in kanstack's src/workstream.rs, which this crate can't
// import (it only talks to kanstack through its CLI). Keep the two in step: a different
// state dir or key here would put runs somewhere kanstack's own state for the repo isn't.

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

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::parse;

    fn sample() -> RunState {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: B\n---\n",
        )
        .unwrap();
        let mut run = RunState::new("100-land", &recipe, Path::new("/r/land.md"), recipe_hash("x"), PathBuf::from("/repo"));
        run.steps.insert(
            "a".to_string(),
            StepRecord {
                status: StepStatus::Done,
                attempt: 1,
                branch: Some("a".to_string()),
                pane: Some("surface:12".to_string()),
                base: Some("a1b2c3d".to_string()),
                tip: Some("9c8d7e6".to_string()),
                started_at: Some(1_761_600_000),
                last_error: None,
                notes_from: Some(512),
                report: Some("added the land endpoint".to_string()),
                waiting_on: None,
                retries: 1,
                retry_budget: 2,
                retry: Some(Retry::FixForward { message: "`cargo test` exited 101".to_string() }),
            },
        );
        run.branch_agents.insert("a".to_string(), "codex".to_string());
        run
    }

    #[test]
    fn snapshot_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::at(tmp.path());
        let run = sample();
        store.save(&run).unwrap();

        assert_eq!(store.load("100-land").unwrap(), run);
        assert_eq!(store.run_ids().unwrap(), vec!["100-land".to_string()]);
        assert_eq!(run.recipe, "land");

        let raw = fs::read_to_string(store.state_path("100-land")).unwrap();
        assert!(raw.contains(r#""status": "done""#), "{raw}");
        assert!(raw.contains(r#""status": "pending""#), "{raw}");
        assert!(raw.contains(r#""kind": "fix_forward""#), "{raw}");
        assert_eq!(run.steps["a"].retry_label().as_deref(), Some("retried 1/2"));
        assert!(!tmp.path().join("runs/100-land/.state.json.tmp").exists(), "the temp file is renamed away");
    }

    /// A snapshot written before `done_when` existed still loads, its new fields empty.
    #[test]
    fn loads_a_snapshot_from_before_done_when() {
        let raw = r#"{"version":1,"id":"1-x","item":null,"recipe":"x","recipe_path":"/r/x.md","recipe_hash":"0","workdir":"/repo","status":"running",
            "steps":{"a":{"status":"running","attempt":1,"branch":"a","pane":"%1","base":null,"tip":null,"started_at":1,"last_error":null}}}"#;
        let run: RunState = serde_json::from_str(raw).unwrap();
        assert_eq!((run.steps["a"].report.clone(), run.steps["a"].waiting_on), (None, None));
        assert_eq!((run.steps["a"].retries, run.steps["a"].retry_label()), (0, None));
        assert!(run.branch_agents.is_empty() && run.branch_prefix.is_empty() && !run.settled);
    }

    #[test]
    fn saving_again_replaces_the_snapshot_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::at(tmp.path());
        let mut run = sample();
        store.save(&run).unwrap();
        run.steps.get_mut("b").unwrap().status = StepStatus::Launching;
        run.steps.get_mut("b").unwrap().attempt = 1;
        store.save(&run).unwrap();

        assert_eq!(store.load("100-land").unwrap().steps["b"].status, StepStatus::Launching);
    }

    #[test]
    fn run_ids_ignore_strays_and_load_refuses_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::at(tmp.path());
        fs::create_dir_all(tmp.path().join("runs/empty-dir")).unwrap();
        assert!(store.run_ids().unwrap().is_empty());
        assert!(store.load("../escape").is_err());
    }

    #[test]
    fn new_run_ids_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RunStore::at(tmp.path());
        let first = store.new_run_id("land feature");
        assert!(first.ends_with("-land-feature"), "{first}");
        fs::create_dir_all(tmp.path().join("runs").join(&first)).unwrap();
        assert_eq!(store.new_run_id("land feature"), format!("{first}-2"));
    }
}
