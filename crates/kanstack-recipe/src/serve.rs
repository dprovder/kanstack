//! `serve`: the factory's pull loop. Starts backlog items as recipe runs while fewer than N
//! are running, moves each item's state as its run progresses, and retries failed steps
//! within their budget before the item counts as failed.
//!
//! Each tick, in one synchronous process:
//!
//! 1. **Pull.** While fewer than `--max` runs are unfinished, the oldest `backlog` item that
//!    names a recipe gets a run, and the item goes to `running`. An item whose recipe can't be
//!    found or parsed goes to `failed` instead, with the reason as a comment.
//! 2. **Advance.** Every unfinished run launches whatever it can, then one `status --json`
//!    read is applied to all of them ([`ActiveRun`]), so N runs cost one read per tick, not N.
//! 3. **Report.** Each step that finished is posted to its item as a comment: its report from
//!    `kanstack note`, or why it failed. Each retry is posted too.
//! 4. **Settle.** A run that completed puts its item in `review`, where it waits to be landed;
//!    one that failed puts it in `failed`.
//!
//! **Durable.** Runs are linked to their items in run state ([`RunState::item`]), and which
//! steps have been posted ([`RunState::reported`]) and whether the item's state followed the
//! run's end ([`RunState::settled`]) are saved there too. A killed `serve` started again resumes
//! every unfinished run, never re-runs a done step, posts each step once (at least once: a crash
//! between posting and saving posts it again), and settles items whose run ended just before.
//!
//! Recipes are found by name: an item's `recipe` is a path when it contains `/` or ends in
//! `.md` (relative to the repository), else `<name>.md` in `<repo>/.kanstack/recipes/`, or in
//! the directories `KANSTACK_RECIPE_PATH` lists (colon-separated) when it's set.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

use kanstack_tracker::{Item, ItemFilter, ItemId, ItemState, Tracker};
use serde::Serialize;

use crate::kanstack::KanstackClient;
use crate::lock::OrchestratorLock;
use crate::recipe;
use crate::resume::{load_for_resume, unfinished_runs};
use crate::run::{ActiveRun, RunOptions};
use crate::state::{recipe_hash, RunState, RunStatus, RunStore, StepRecord, StepStatus};

/// The retry budget when neither `KANSTACK_RETRIES` nor the recipe sets one.
pub const DEFAULT_RETRIES: u32 = 2;

pub struct ServeOptions {
    /// How many runs may be unfinished at once (`--max`).
    pub max: usize,
    /// The retry budget for steps whose recipe sets none (`KANSTACK_RETRIES`).
    pub retries: u32,
    /// `KANSTACK_RECIPE_PATH`: where recipes are looked up by name instead of
    /// `<repo>/.kanstack/recipes/`.
    pub recipe_path: Option<String>,
    pub run: RunOptions,
}

/// Something that happened to an item in a tick. `--json` prints each as one line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub kind: EventKind,
    pub item: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// A run started for the item; it's `running` now.
    Started,
    /// One of the item's steps failed and is being retried.
    Retried,
    /// Every step passed; the item waits in `review`.
    Review,
    /// The item is `failed`: a step ran out of retries, or its recipe couldn't be used.
    Failed,
}

impl Event {
    fn new(kind: EventKind, item: &str) -> Event {
        Event { kind, item: item.to_string(), run: None, step: None, reason: None }
    }

    fn human(&self) -> String {
        let run = self.run.as_deref().map(|r| format!(" (run {r})")).unwrap_or_default();
        match self.kind {
            EventKind::Started => format!("▶ item {} started{run}", self.item),
            EventKind::Retried => format!(
                "↻ item {} retrying {}{run}: {}",
                self.item,
                self.step.as_deref().unwrap_or_default(),
                self.reason.as_deref().unwrap_or_default()
            ),
            EventKind::Review => format!("✓ item {} in review{run}", self.item),
            EventKind::Failed => format!("✗ item {} failed{run}: {}", self.item, self.reason.as_deref().unwrap_or_default()),
        }
    }
}

/// The pull loop's state: the runs it's driving, and the ones it's holding back.
pub struct Serve<'a> {
    repo: PathBuf,
    tracker: &'a dyn Tracker,
    store: &'a RunStore,
    client: &'a KanstackClient,
    opts: ServeOptions,
    active: Vec<ActiveRun>,
    /// Runs that ended but whose item couldn't be updated yet; tried again every tick.
    unsettled: Vec<RunState>,
    /// Items whose run this `serve` won't drive (it couldn't be resumed, or its state stopped
    /// saving), so the pull never starts a second run for them.
    held: HashSet<String>,
    /// Each active run's steps' retry counts as last posted, so each retry is posted once.
    retries_seen: HashMap<String, HashMap<String, u32>>,
    /// Held for as long as this drives the store's runs, so no other orchestrator does.
    _lock: &'a OrchestratorLock,
}

impl<'a> Serve<'a> {
    /// Resumes every unfinished run in `store`, and catches up on items whose run ended
    /// without its item following. Holding `lock` makes sure no other orchestrator is driving
    /// the same runs.
    pub fn new(
        repo: &Path,
        tracker: &'a dyn Tracker,
        store: &'a RunStore,
        client: &'a KanstackClient,
        opts: ServeOptions,
        lock: &'a OrchestratorLock,
    ) -> Result<Self, String> {
        let mut serve = Serve {
            repo: repo.to_path_buf(),
            tracker,
            store,
            client,
            opts,
            active: Vec::new(),
            unsettled: Vec::new(),
            held: HashSet::new(),
            retries_seen: HashMap::new(),
            _lock: lock,
        };

        let unfinished = unfinished_runs(store).map_err(|e| format!("could not list runs in {}: {e}", store.root().display()))?;
        for id in store.run_ids().map_err(|e| format!("could not list runs in {}: {e}", store.root().display()))? {
            if unfinished.contains(&id) {
                serve.resume(&id);
            } else if let Ok(run) = store.load(&id) {
                if run.item.is_some() && !run.settled {
                    serve.unsettled.push(run);
                }
            }
        }
        Ok(serve)
    }

    fn say(&self, line: &str) {
        if self.opts.run.progress_to_stderr {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }

    fn resume(&mut self, id: &str) {
        self.say(&format!("↻ run {id}"));
        match load_for_resume(self.store, id, false) {
            Ok((recipe, run)) => {
                // The run was saved before its item was moved to `running`, so a crash in
                // between leaves the item behind; setting it again is harmless.
                if let Some(item) = &run.item {
                    if let Err(e) = self.tracker.set_state(&ItemId(item.clone()), ItemState::Running) {
                        eprintln!("! item {item}: could not set it running: {e:#}");
                    }
                }
                // Remembered before reconciling, so a retry reconcile decides is posted too.
                self.remember_retries(&run);
                self.active.push(ActiveRun::resume(recipe, run, self.client, &self.opts.run, self.store));
            }
            Err(e) => {
                eprintln!("✗ run {id} can't be resumed, leaving it for `kanstack-recipe resume`: {e}");
                if let Some(item) = self.store.load(id).ok().and_then(|run| run.item) {
                    self.held.insert(item);
                }
            }
        }
    }

    /// The runs being driven, as `(item, run id)`.
    pub fn running(&self) -> Vec<(Option<String>, String)> {
        self.active.iter().map(|a| (a.run.item.clone(), a.run.id.clone())).collect()
    }

    /// One tick of the loop (see the module doc). Returns what happened to items.
    pub fn tick(&mut self) -> Vec<Event> {
        let mut events = Vec::new();

        let unsettled = std::mem::take(&mut self.unsettled);
        for run in unsettled {
            self.settle(run, &mut events);
        }

        self.pull(&mut events);

        let mut active = std::mem::take(&mut self.active);
        for run in &mut active {
            run.launch(self.client, &self.opts.run, self.store);
        }
        if active.iter().any(ActiveRun::in_flight) {
            let statuses = self.client.status().map_err(|e| e.to_string());
            for run in active.iter_mut().filter(|run| run.in_flight()) {
                run.poll(self.client, &self.opts.run, self.store, statuses.as_deref().map_err(String::as_str));
            }
        }

        for mut run in active {
            self.post_retries(&run.run, &mut events);
            self.post_steps(&mut run.run);
            if !run.ended() {
                self.active.push(run);
                continue;
            }
            run.finish(self.client, &self.opts.run, self.store);
            self.retries_seen.remove(&run.run.id);
            if run.run.status == RunStatus::Running {
                // Its state stopped saving: it's still unfinished on disk, for a later resume.
                eprintln!("✗ run {} stopped, its state can't be saved; leaving it for a later resume", run.run.id);
                if let Some(item) = &run.run.item {
                    self.held.insert(item.clone());
                }
                continue;
            }
            self.settle(run.run, &mut events);
        }
        events
    }

    /// Starts runs for backlog items while there's room.
    fn pull(&mut self, events: &mut Vec<Event>) {
        if self.active.len() >= self.opts.max {
            return;
        }
        let items = match self.tracker.list(&ItemFilter::state(ItemState::Backlog)) {
            Ok(items) => items,
            Err(e) => {
                eprintln!("! could not read the backlog, trying again next tick: {e:#}");
                return;
            }
        };
        let taken: HashSet<String> = self
            .active
            .iter()
            .map(|a| &a.run)
            .chain(&self.unsettled)
            .filter_map(|run| run.item.clone())
            .chain(self.held.iter().cloned())
            .collect();

        for item in items {
            if self.active.len() >= self.opts.max {
                break;
            }
            let Some(name) = item.recipe.as_deref().map(str::trim).filter(|r| !r.is_empty()) else { continue };
            if taken.contains(&item.id.0) {
                continue;
            }
            let name = name.to_string();
            match self.start(&item, &name, events) {
                Ok(true) => {}
                Ok(false) => break,
                Err(reason) => {
                    let reason = format!("could not start: {reason}");
                    if let Err(e) = self.tracker.comment(&item.id, &format!("### Recipe `{name}`: not started\n\n{reason}")) {
                        eprintln!("! item {}: could not post why it failed: {e:#}", item.id);
                    }
                    match self.tracker.set_state(&item.id, ItemState::Failed) {
                        Ok(()) => events.push(Event { reason: Some(reason), ..Event::new(EventKind::Failed, &item.id.0) }),
                        Err(e) => eprintln!("! item {}: could not set it failed ({reason}): {e:#}", item.id),
                    }
                }
            }
        }
    }

    /// Starts a run of recipe `name` for `item`. `Err` with the reason when the recipe can't
    /// be used, which fails the item; `Ok(false)` when the run's state couldn't be saved,
    /// which is the orchestrator's problem, not the item's, so it stays in the backlog.
    fn start(&mut self, item: &Item, name: &str, events: &mut Vec<Event>) -> Result<bool, String> {
        let path = resolve_recipe(&self.repo, name, self.opts.recipe_path.as_deref())?;
        let source = fs::read_to_string(&path).map_err(|e| format!("could not read recipe {}: {e}", path.display()))?;
        let parsed = recipe::parse(&source).map_err(|e| format!("recipe {}: {e}", path.display()))?;
        let budgets = recipe::parse_retries(&source).map_err(|e| format!("recipe {}: {e}", path.display()))?;
        let path = path.canonicalize().unwrap_or(path);

        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let mut run = RunState::new(self.store.new_run_id(&stem), &parsed, &path, recipe_hash(&source), self.repo.clone());
        run.item = Some(item.id.0.clone());
        run.branch_prefix = branch_prefix(&item.id);
        for (step_id, rec) in &mut run.steps {
            rec.retry_budget = budgets.budget(step_id, self.opts.retries);
        }
        // Saved before the item moves, so an item is never `running` without a run to show for
        // it; a crash in between leaves a run that the next `serve` resumes and the item follows.
        if let Err(e) = self.store.save(&run) {
            eprintln!("✗ could not save a run for item {}, leaving it in the backlog: {e}", item.id);
            return Ok(false);
        }
        if let Err(e) = self.tracker.set_state(&item.id, ItemState::Running) {
            eprintln!("! item {}: could not set it running: {e:#}", item.id);
        }
        self.say(&format!("→ item {} run {} ({})", item.id, run.id, path.display()));
        events.push(Event { run: Some(run.id.clone()), ..Event::new(EventKind::Started, &item.id.0) });
        let active = ActiveRun::start(parsed, run, self.client, &self.opts.run, self.store);
        self.remember_retries(&active.run);
        self.active.push(active);
        Ok(true)
    }

    fn remember_retries(&mut self, run: &RunState) {
        let seen = run.steps.iter().map(|(id, rec)| (id.clone(), rec.retries)).collect();
        self.retries_seen.insert(run.id.clone(), seen);
    }

    /// Posts each retry the run's steps took this tick to its item.
    fn post_retries(&mut self, run: &RunState, events: &mut Vec<Event>) {
        let Some(item) = run.item.clone() else { return };
        let seen = self.retries_seen.entry(run.id.clone()).or_default();
        let mut retried = Vec::new();
        for (step_id, rec) in &run.steps {
            if rec.retries > seen.get(step_id).copied().unwrap_or(0) {
                seen.insert(step_id.clone(), rec.retries);
                retried.push((step_id.clone(), rec.clone()));
            }
        }
        for (step_id, rec) in retried {
            let reason = rec.last_error.clone().unwrap_or_default();
            let label = rec.retry_label().unwrap_or_default();
            if let Err(e) = self.tracker.comment(&ItemId(item.clone()), &format!("### {step_id}: retrying ({label})\n\n{reason}")) {
                eprintln!("! item {item}: could not post {step_id}'s retry: {e:#}");
            }
            events.push(Event {
                run: Some(run.id.clone()),
                step: Some(step_id),
                reason: Some(format!("{reason} ({label})")),
                ..Event::new(EventKind::Retried, &item)
            });
        }
    }

    /// Posts every finished step not posted yet to the run's item, and saves which were.
    fn post_steps(&self, run: &mut RunState) {
        let Some(item) = run.item.clone() else { return };
        let mut posted = false;
        for (step_id, rec) in &run.steps {
            if !matches!(rec.status, StepStatus::Done | StepStatus::Failed) || run.reported.contains(step_id) {
                continue;
            }
            match self.tracker.comment(&ItemId(item.clone()), &step_comment(step_id, rec)) {
                Ok(()) => {
                    run.reported.insert(step_id.clone());
                    posted = true;
                }
                Err(e) => eprintln!("! item {item}: could not post {step_id}'s report, trying again next tick: {e:#}"),
            }
        }
        if posted {
            if let Err(e) = self.store.save(run) {
                eprintln!("! run {}: could not save which reports were posted: {e}", run.id);
            }
        }
    }

    /// An ended run's item follows it: `review` if it completed, `failed` if not. Tried again
    /// next tick if the tracker can't be reached.
    fn settle(&mut self, mut run: RunState, events: &mut Vec<Event>) {
        let Some(item) = run.item.clone() else { return };
        self.post_steps(&mut run);
        let (state, kind) = match run.status {
            RunStatus::Done => (ItemState::Review, EventKind::Review),
            _ => (ItemState::Failed, EventKind::Failed),
        };
        if let Err(e) = self.tracker.set_state(&ItemId(item.clone()), state) {
            eprintln!("! item {item}: could not set it {state}, trying again next tick: {e:#}");
            self.unsettled.push(run);
            return;
        }
        let reason = (kind == EventKind::Failed).then(|| {
            run.steps
                .iter()
                .filter(|(_, rec)| rec.status == StepStatus::Failed)
                .map(|(id, rec)| format!("{id}: {}", rec.last_error.as_deref().unwrap_or("failed")))
                .collect::<Vec<_>>()
                .join("; ")
        });
        events.push(Event { run: Some(run.id.clone()), reason, ..Event::new(kind, &item) });
        run.settled = true;
        if let Err(e) = self.store.save(&run) {
            eprintln!("! run {}: could not save that item {item} was settled: {e}", run.id);
        }
    }
}

/// The comment a finished step posts to its item.
fn step_comment(step_id: &str, rec: &StepRecord) -> String {
    let retried = rec.retry_label().map(|l| format!(" ({l})")).unwrap_or_default();
    let mut comment = format!("### {step_id}: {}{retried}\n\n", rec.status.as_str());
    if let Some(branch) = &rec.branch {
        let short = |sha: &str| sha.chars().take(7).collect::<String>();
        match (&rec.base, &rec.tip) {
            (Some(base), Some(tip)) if base != tip => comment.push_str(&format!("Branch `{branch}`, `{}..{}`.\n\n", short(base), short(tip))),
            _ => comment.push_str(&format!("Branch `{branch}`.\n\n")),
        }
    }
    if rec.status == StepStatus::Failed {
        comment.push_str(rec.last_error.as_deref().unwrap_or("Failed."));
        comment.push('\n');
    }
    if let Some(report) = &rec.report {
        if rec.status == StepStatus::Failed {
            comment.push('\n');
        }
        comment.push_str(report.trim());
        comment.push('\n');
    } else if rec.status == StepStatus::Done {
        comment.push_str("No report.\n");
    }
    comment
}

/// `item-<id>-`, with anything but letters, digits, `-`, `_` and `.` in the id turned into
/// `-`: put in front of every branch an item's run works on.
pub fn branch_prefix(id: &ItemId) -> String {
    let id: String = id.0.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect();
    format!("item-{}-", id.trim_matches('-'))
}

/// Finds the recipe an item names (see the module doc). `search_path` is
/// `KANSTACK_RECIPE_PATH`'s value; blank counts as unset.
pub fn resolve_recipe(repo: &Path, name: &str, search_path: Option<&str>) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.contains('/') || name.ends_with(".md") {
        let path = repo.join(name);
        return if path.is_file() { Ok(path) } else { Err(format!("recipe file {} doesn't exist", path.display())) };
    }
    let dirs: Vec<PathBuf> = match search_path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(paths) => paths.split(':').filter(|d| !d.is_empty()).map(|d| repo.join(d)).collect(),
        None => vec![repo.join(".kanstack").join("recipes")],
    };
    if let Some(path) = dirs.iter().map(|dir| dir.join(format!("{name}.md"))).find(|p| p.is_file()) {
        return Ok(path);
    }
    let searched = dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
    Err(format!("no recipe named \"{name}\" ({name}.md isn't in {searched})"))
}

/// One `--json` line: an [`Event`], versioned.
#[derive(Serialize)]
struct JsonLine<'e> {
    schema: u32,
    #[serde(flatten)]
    event: &'e Event,
}

struct Flags {
    max: usize,
    kanstack_bin: PathBuf,
    once: bool,
    json: bool,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags { max: 1, kanstack_bin: PathBuf::from("kanstack"), once: false, json: false };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--max" => {
                let value = args.next().ok_or("--max needs a number")?;
                flags.max = value.parse().ok().filter(|n| *n > 0).ok_or_else(|| format!("--max needs a number above 0, not `{value}`"))?;
            }
            "--kanstack-bin" => flags.kanstack_bin = PathBuf::from(args.next().ok_or("--kanstack-bin needs a path")?),
            "--once" => flags.once = true,
            "--json" => flags.json = true,
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok(flags)
}

/// `kanstack-recipe serve [--max <N>] [--kanstack-bin <path>] [--once] [--json]`, with `args`
/// everything after `serve`. Runs until killed, or for one tick with `--once`. `--json` prints
/// each [`Event`] as a line of JSON on stdout, and progress on stderr.
pub fn command(args: &[String]) -> ExitCode {
    let flags = match parse_flags(args) {
        Ok(flags) => flags,
        Err(e) => {
            eprintln!("kanstack-recipe serve: {e}");
            eprintln!("usage: kanstack-recipe serve [--max <N>] [--kanstack-bin <path>] [--once] [--json]");
            return ExitCode::from(2);
        }
    };
    let retries = match env::var("KANSTACK_RETRIES").ok().filter(|v| !v.trim().is_empty()) {
        None => DEFAULT_RETRIES,
        Some(v) => match v.trim().parse() {
            Ok(n) => n,
            Err(_) => {
                eprintln!("kanstack-recipe serve: KANSTACK_RETRIES must be a whole number, not `{v}`");
                return ExitCode::from(2);
            }
        },
    };

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&cwd).to_path_buf();
    let Some(store) = RunStore::for_repo(&repo) else {
        eprintln!("kanstack-recipe: no state directory to keep runs in (set KANSTACK_STATE_PATH or HOME)");
        return ExitCode::FAILURE;
    };
    let lock = match OrchestratorLock::try_acquire(&store) {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("kanstack-recipe: {e}");
            return ExitCode::FAILURE;
        }
    };
    let tracker = match kanstack_tracker::select(&repo) {
        Ok(tracker) => tracker,
        Err(e) => {
            eprintln!("kanstack-recipe: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let client = KanstackClient::new(flags.kanstack_bin);
    let opts = ServeOptions {
        max: flags.max,
        retries,
        recipe_path: env::var("KANSTACK_RECIPE_PATH").ok(),
        run: RunOptions { progress_to_stderr: flags.json, ..RunOptions::default() },
    };
    let poll_interval = opts.run.poll_interval;

    let mut serve = match Serve::new(&repo, tracker.as_ref(), &store, &client, opts, &lock) {
        Ok(serve) => serve,
        Err(e) => {
            eprintln!("kanstack-recipe: {e}");
            return ExitCode::FAILURE;
        }
    };
    loop {
        for event in serve.tick() {
            if flags.json {
                match serde_json::to_string(&JsonLine { schema: 1, event: &event }) {
                    Ok(line) => println!("{line}"),
                    Err(e) => eprintln!("! could not print an event as JSON: {e}"),
                }
            } else {
                println!("{}", event.human());
            }
        }
        if flags.once {
            return ExitCode::SUCCESS;
        }
        thread::sleep(poll_interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_paths_names_and_the_search_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        fs::create_dir_all(repo.join(".kanstack/recipes")).unwrap();
        fs::create_dir_all(repo.join("elsewhere")).unwrap();
        fs::create_dir_all(repo.join("more")).unwrap();
        fs::write(repo.join(".kanstack/recipes/land.md"), "").unwrap();
        fs::write(repo.join("more/land.md"), "").unwrap();
        fs::write(repo.join("elsewhere/odd.md"), "").unwrap();

        assert_eq!(resolve_recipe(repo, "land", None).unwrap(), repo.join(".kanstack/recipes/land.md"));
        assert_eq!(resolve_recipe(repo, " land ", Some("")).unwrap(), repo.join(".kanstack/recipes/land.md"));
        assert_eq!(resolve_recipe(repo, "land", Some("elsewhere:more")).unwrap(), repo.join("more/land.md"));
        assert_eq!(resolve_recipe(repo, "elsewhere/odd.md", None).unwrap(), repo.join("elsewhere/odd.md"));
        assert_eq!(resolve_recipe(repo, "odd.md", Some("elsewhere")).unwrap_err(), format!("recipe file {} doesn't exist", repo.join("odd.md").display()));
        let abs = repo.join("more/land.md");
        assert_eq!(resolve_recipe(repo, abs.to_str().unwrap(), None).unwrap(), abs);

        let err = resolve_recipe(repo, "odd", Some("more")).unwrap_err();
        assert!(err.contains("no recipe named \"odd\"") && err.contains("more"), "{err}");
    }

    #[test]
    fn branch_prefixes_are_safe_branch_names() {
        assert_eq!(branch_prefix(&ItemId("44".to_string())), "item-44-");
        assert_eq!(branch_prefix(&ItemId("#44".to_string())), "item-44-");
        assert_eq!(branch_prefix(&ItemId("LIN-12/x".to_string())), "item-LIN-12-x-");
    }

    #[test]
    fn flags_parse_with_their_defaults() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let flags = parse_flags(&[]).unwrap();
        assert_eq!((flags.max, flags.once, flags.json), (1, false, false));
        let flags = parse_flags(&args(&["--max", "3", "--once", "--json", "--kanstack-bin", "/k"])).unwrap();
        assert_eq!((flags.max, flags.once, flags.json, flags.kanstack_bin), (3, true, true, PathBuf::from("/k")));
        assert!(parse_flags(&args(&["--max", "0"])).is_err());
        assert!(parse_flags(&args(&["--max"])).is_err());
        assert!(parse_flags(&args(&["--bogus"])).is_err());
    }
}
