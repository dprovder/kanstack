//! `serve`, the pull loop: items start as runs while there's room, item states follow their
//! runs, steps are retried within their budget, and reports land on items as comments. Like
//! the other suites these drive the stand-in `kanstack` in `common`, and a local tracker in a
//! scratch directory. Most tests tick a [`Serve`] in this process; the crash test runs
//! [`serve::command`] in a child process (this test binary, re-run as [`serve_child_process`])
//! and has the stand-in `kill -9` it for real.

mod common;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{acquire_lock, actions, calls_log, fast_opts, init_repo, write_fake_kanstack};

use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::lock::OrchestratorLock;
use kanstack_recipe::serve::{self, Event, EventKind, Serve, ServeOptions};
use kanstack_recipe::state::{RunState, RunStatus, RunStore, StepStatus};
use kanstack_tracker::{Draft, Item, ItemId, ItemState, LocalTracker, Tracker};

const ONE_STEP: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n";
const TWO_STEPS: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: Do B\n---\nctx\n";

/// The environment variable that turns [`serve_child_process`] into a `serve` run: the
/// arguments, one per line, or `where` to print where the tracker and run store live.
const CHILD: &str = "KANSTACK_RECIPE_SERVE_TEST_CHILD";

/// A scratch factory: the stand-in kanstack, a git repository it works in (with recipes in
/// `.kanstack/recipes/`), a local tracker and a run store.
struct Factory {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bin_dir: PathBuf,
    workdir: PathBuf,
    tracker: LocalTracker,
    store: RunStore,
    client: KanstackClient,
}

impl Factory {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (bin_dir, workdir) = (root.join("bin"), root.join("work"));
        fs::create_dir_all(&bin_dir).unwrap();
        write_fake_kanstack(&bin_dir);
        init_repo(&workdir, &bin_dir);
        Factory {
            tracker: LocalTracker::in_dir(root.join("items")),
            store: RunStore::at(root.join("runs")),
            client: KanstackClient::new(bin_dir.join("kanstack")),
            _tmp: tmp,
            root,
            bin_dir,
            workdir,
        }
    }

    fn recipe(&self, name: &str, source: &str) {
        let dir = self.workdir.join(".kanstack/recipes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.md")), source).unwrap();
    }

    fn knob(&self, name: &str, contents: &str) {
        fs::write(self.bin_dir.join(name), contents).unwrap();
    }

    fn item(&self, title: &str, recipe: Option<&str>) -> ItemId {
        add_item(&self.tracker, title, recipe)
    }

    fn get(&self, id: &ItemId) -> Item {
        self.tracker.get(id).unwrap()
    }

    fn serve<'a>(&'a self, lock: &'a OrchestratorLock, max: usize, retries: u32) -> Serve<'a> {
        let opts = ServeOptions { max, retries, recipe_path: None, run: fast_opts() };
        Serve::new(&self.workdir, &self.tracker, &self.store, &self.client, opts, lock).unwrap()
    }

    fn lock(&self) -> OrchestratorLock {
        acquire_lock(&self.store)
    }

    /// The one run for `item`.
    fn run_for(&self, item: &ItemId) -> RunState {
        run_for(&self.store, item)
    }
}

fn add_item(tracker: &dyn Tracker, title: &str, recipe: Option<&str>) -> ItemId {
    let draft = Draft { title: title.to_string(), body: format!("{title}, please."), recipe: recipe.map(String::from), ..Draft::default() };
    tracker.create(&draft).unwrap()
}

fn run_for(store: &RunStore, item: &ItemId) -> RunState {
    let mut runs: Vec<RunState> = store
        .run_ids()
        .unwrap()
        .iter()
        .map(|id| store.load(id).unwrap())
        .filter(|run| run.item.as_deref() == Some(item.0.as_str()))
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs.pop().unwrap()
}

/// Ticks `serve` until `done` holds, returning every tick's events. Fails the test after 400.
fn tick_until(serve: &mut Serve, mut done: impl FnMut() -> bool) -> Vec<Vec<Event>> {
    let mut ticks = Vec::new();
    for _ in 0..400 {
        ticks.push(serve.tick());
        if done() {
            return ticks;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("never got there; events: {ticks:?}");
}

fn first_tick(ticks: &[Vec<Event>], kind: EventKind, item: &ItemId) -> usize {
    ticks
        .iter()
        .position(|events| events.iter().any(|e| e.kind == kind && e.item == item.0))
        .unwrap_or_else(|| panic!("no {kind:?} event for item {item}: {ticks:?}"))
}

#[test]
fn starts_at_most_max_items_and_the_next_when_one_finishes() {
    let f = Factory::new();
    f.recipe("one", ONE_STEP);
    let items = [f.item("First", Some("one")), f.item("Second", Some("one")), f.item("Third", Some("one"))];
    f.knob("busy", "item-2-a\n");
    let lock = f.lock();
    let mut serve = f.serve(&lock, 2, 2);

    let first = serve.tick();
    let started: Vec<&str> = first.iter().filter(|e| e.kind == EventKind::Started).map(|e| e.item.as_str()).collect();
    assert_eq!(started, vec![items[0].0.as_str(), items[1].0.as_str()], "{first:?}");
    assert_eq!(serve.running().len(), 2);
    assert_eq!(f.get(&items[0]).state, ItemState::Running);
    assert_eq!(f.get(&items[2]).state, ItemState::Backlog, "only two at once");

    let mut ticks = vec![first];
    ticks.extend(tick_until(&mut serve, || {
        assert!(serve_limit_held(&f.store), "never more than two unfinished runs");
        f.get(&items[2]).state != ItemState::Backlog
    }));
    let reviewed = first_tick(&ticks, EventKind::Review, &items[0]);
    let third = first_tick(&ticks, EventKind::Started, &items[2]);
    assert!(third > reviewed, "the third starts once the first is done (review at tick {reviewed}, start at {third})");
    assert_eq!(f.get(&items[1]).state, ItemState::Running, "the second is still busy");

    fs::remove_file(f.bin_dir.join("busy")).unwrap();
    tick_until(&mut serve, || items.iter().all(|id| f.get(id).state == ItemState::Review));
    assert!(serve.running().is_empty());
    for id in &items {
        let run = f.run_for(id);
        assert_eq!((run.status, run.settled, run.branch_prefix.clone()), (RunStatus::Done, true, format!("item-{id}-")));
    }
    let calls = actions(&f.bin_dir);
    assert_eq!(calls.iter().filter(|c| c.starts_with("spawn ")).count(), 3, "{calls:?}");
}

/// No more than two runs in `store` are unfinished.
fn serve_limit_held(store: &RunStore) -> bool {
    store.run_ids().unwrap().iter().filter(|id| store.load(id).unwrap().status == RunStatus::Running).count() <= 2
}

#[test]
fn a_failed_verify_is_retried_in_the_same_pane_and_passes() {
    let f = Factory::new();
    f.recipe(
        "check",
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"[ -f fixed ] || { echo 'assertion failed: widget'; touch fixed; exit 1; }\"\n---\nctx\n",
    );
    let id = f.item("Widget", Some("check"));
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);

    let ticks = tick_until(&mut serve, || f.get(&id).state == ItemState::Review);
    first_tick(&ticks, EventKind::Retried, &id);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("spawn item-1-a "), "{calls:?}");
    assert!(calls[1].starts_with("send item-1-a "), "fixed forward in the same pane: {calls:?}");
    assert!(calls[1].contains("assertion failed: widget") && calls[1].contains("retry 1/2"), "{calls:?}");

    let a = &f.run_for(&id).steps["a"];
    assert_eq!((a.status, a.retries, a.retry_budget, a.retry.clone()), (StepStatus::Done, 1, 2, None), "{a:?}");
    assert_eq!(a.retry_label().as_deref(), Some("retried 1/2"));
    let body = f.get(&id).body;
    assert!(body.contains("### a: retrying (retried 1/2)") && body.contains("### a: done (retried 1/2)"), "{body}");
}

#[test]
fn a_step_out_of_retries_fails_its_item_and_leaves_the_others_alone() {
    let f = Factory::new();
    // `retries:` in the recipe beats the default of 2.
    f.recipe("bad", "---\nversion: 1\nretries: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"false\"\n  b:\n    agent: codex\n    needs: [a]\n    prompt: Do B\n---\nctx\n");
    f.recipe("good", ONE_STEP);
    let bad = f.item("Bad", Some("bad"));
    let good = f.item("Good", Some("good"));
    let lock = f.lock();
    let mut serve = f.serve(&lock, 2, 2);

    tick_until(&mut serve, || f.get(&bad).state == ItemState::Failed && f.get(&good).state == ItemState::Review);

    let run = f.run_for(&bad);
    let a = &run.steps["a"];
    assert_eq!((run.status, a.status, a.retries, a.retry_budget), (RunStatus::Failed, StepStatus::Failed, 1, 1), "{a:?}");
    assert!(a.last_error.as_deref().unwrap_or_default().contains("out of retries"), "{a:?}");
    assert_eq!(run.steps["b"].status, StepStatus::Pending, "a failed step's dependents never run");
    let calls = calls_log(&f.bin_dir);
    assert!(!calls.contains("spawn item-1-b "), "{calls}");
    assert_eq!(calls.matches("send item-1-a ").count(), 1, "one retry, then out: {calls}");
    let body = f.get(&bad).body;
    assert!(body.contains("### a: failed (retried 1/1)") && body.contains("out of retries"), "{body}");
}

/// The `--prompt` a logged `spawn` call carried (newlines flattened to spaces by the stand-in).
fn spawn_prompt(call: &str) -> &str {
    let start = call.find(" --prompt ").unwrap_or_else(|| panic!("no --prompt in {call}")) + " --prompt ".len();
    call[start..].strip_suffix(" --json").unwrap_or(&call[start..])
}

/// The message a logged `send <branch> <message> --json` call carried.
fn send_message<'c>(call: &'c str, branch: &str) -> &'c str {
    let rest = call.strip_prefix(&format!("send {branch} ")).unwrap_or_else(|| panic!("not a send to {branch}: {call}"));
    rest.strip_suffix(" --json").unwrap_or(rest)
}

#[test]
fn every_launch_starts_with_the_work_item_first_launch_and_fix_forward_retry() {
    let f = Factory::new();
    f.recipe(
        "check",
        "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    verify:\n      - \"[ -f fixed ] || { echo 'assertion failed: widget'; touch fixed; exit 1; }\"\n---\nctx\n",
    );
    let id = f.item("Widget", Some("check"));
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);

    tick_until(&mut serve, || f.get(&id).state == ItemState::Review);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.len(), 2, "{calls:?}");
    let block = "## Work item #1: Widget  Widget, please.  ";
    let first = spawn_prompt(&calls[0]);
    assert!(first.starts_with(&format!("{block}Do A  ## Recipe context")), "first launch: {first}");
    let retry = send_message(&calls[1], "item-1-a");
    assert!(retry.starts_with(block) && retry.contains("assertion failed: widget"), "fix-forward retry: {retry}");

    let work_item = f.run_for(&id).work_item.expect("captured when the run started");
    assert_eq!((work_item.title.as_str(), work_item.body.as_str()), ("Widget", "Widget, please.\n"));
}

#[test]
fn a_respawn_carries_the_work_item_as_it_was_when_the_run_started() {
    let f = Factory::new();
    f.recipe("one", ONE_STEP);
    let id = f.item("Fragile", Some("one"));
    f.knob("die_on_status", "item-1-a\n");
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);

    tick_until(&mut serve, || f.get(&id).state == ItemState::Review);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.len(), 3, "{calls:?}");
    let respawn = spawn_prompt(&calls[2]);
    assert!(respawn.starts_with("## Work item #1: Fragile  Fragile, please.  Do A"), "{respawn}");
    assert!(respawn.contains("ended before it was finished"), "{respawn}");
    // By the respawn the local item's body has the retry comment appended; the run's copy
    // from when it started is what the agent gets.
    assert!(f.get(&id).body.contains("### a: retrying"), "{}", f.get(&id).body);
    assert!(!respawn.contains("retrying"), "the item's later edits stay out: {respawn}");
}

#[test]
fn a_plain_run_has_no_work_item_block() {
    let f = Factory::new();
    let recipe = kanstack_recipe::recipe::parse(ONE_STEP).unwrap();
    let path = f.workdir.join("one.md");
    fs::write(&path, ONE_STEP).unwrap();
    let mut run = RunState::new("r1", &recipe, &path, String::new(), f.workdir.clone());
    run.item = Some("1".to_string());
    assert_eq!(kanstack_recipe::aggregate::launch_prompt(&recipe, &run, "a", "a", Path::new("/k")), recipe.launch_prompt("a", "a", Path::new("/k")), "no captured item, no block");
    run.item = None;
    run.work_item = Some(kanstack_recipe::state::WorkItem { title: "T".to_string(), body: "B".to_string() });
    assert_eq!(kanstack_recipe::aggregate::launch_prompt(&recipe, &run, "a", "a", Path::new("/k")), recipe.launch_prompt("a", "a", Path::new("/k")), "no item, no block");
}

#[test]
fn a_dead_pane_is_respawned_on_the_same_branch() {
    let f = Factory::new();
    f.recipe("one", ONE_STEP);
    let id = f.item("Fragile", Some("one"));
    f.knob("die_on_status", "item-1-a\n");
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);

    tick_until(&mut serve, || f.get(&id).state == ItemState::Review);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(calls[0].starts_with("spawn item-1-a "), "{calls:?}");
    assert_eq!(calls[1], "stop item-1-a --json");
    assert!(calls[2].starts_with("spawn item-1-a --agent codex "), "{calls:?}");
    assert!(calls[2].contains("Do A") && calls[2].contains("ended before it was finished"), "the prompt plus a note: {calls:?}");

    let a = &f.run_for(&id).steps["a"];
    assert_eq!((a.status, a.retries, a.attempt), (StepStatus::Done, 1, 2), "{a:?}");
    assert!(a.last_error.as_deref().unwrap_or_default().contains("died"), "why it was retried stays visible: {a:?}");
}

#[test]
fn a_lane_stopped_from_outside_is_respawned_on_the_same_branch() {
    let f = Factory::new();
    f.recipe("one", ONE_STEP);
    let id = f.item("Stopped by hand", Some("one"));
    f.knob("vanish_on_status", "item-1-a\n");
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);

    tick_until(&mut serve, || f.get(&id).state == ItemState::Review);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.iter().filter(|c| c.starts_with("spawn item-1-a ")).count(), 2, "respawned once: {calls:?}");
    let a = &f.run_for(&id).steps["a"];
    assert_eq!((a.status, a.retries, a.attempt), (StepStatus::Done, 1, 2), "{a:?}");
    assert!(a.last_error.as_deref().unwrap_or_default().contains("disappeared"), "{a:?}");
}

#[test]
fn transport_errors_retry_without_using_up_the_budget() {
    let f = Factory::new();
    f.recipe("one", ONE_STEP);
    let id = f.item("Flaky transport", Some("one"));
    f.knob("fail_transport_remaining", "2");
    let lock = f.lock();
    // No budget at all: a counted retry would fail the item.
    let mut serve = f.serve(&lock, 1, 0);

    tick_until(&mut serve, || f.get(&id).state == ItemState::Review);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.iter().filter(|c| c.starts_with("spawn item-1-a ")).count(), 3, "{calls:?}");
    let a = &f.run_for(&id).steps["a"];
    assert_eq!((a.status, a.retries, a.attempt), (StepStatus::Done, 0, 3), "{a:?}");
}

#[test]
fn an_item_whose_recipe_cant_be_used_fails_with_the_reason() {
    let f = Factory::new();
    f.recipe("broken", "---\nversion: 1\nsteps: {}\n---\n");
    f.recipe("one", ONE_STEP);
    let missing = f.item("Missing", Some("nope"));
    let broken = f.item("Broken", Some("broken"));
    let no_recipe = f.item("Unassigned", None);
    let fine = f.item("Fine", Some("one"));
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);

    let events = serve.tick();
    assert_eq!(f.get(&missing).state, ItemState::Failed);
    assert_eq!(f.get(&broken).state, ItemState::Failed);
    assert_eq!(f.get(&no_recipe).state, ItemState::Backlog, "an item without a recipe is left for a person");
    assert_eq!(f.get(&fine).state, ItemState::Running, "a failed start doesn't use up the slot");
    assert!(events.iter().any(|e| e.kind == EventKind::Failed && e.item == missing.0), "{events:?}");

    let body = f.get(&missing).body;
    assert!(body.contains("no recipe named \"nope\"") && body.contains(".kanstack/recipes"), "{body}");
    assert!(f.get(&broken).body.contains("no steps"), "{}", f.get(&broken).body);
}

#[test]
fn each_steps_report_lands_on_its_item_once() {
    let f = Factory::new();
    f.recipe("survey", "---\nversion: 1\nsteps:\n  a:\n    agent: claude\n    done_when: note\n    prompt: Look around\n---\nctx\n");
    let id = f.item("Survey", Some("survey"));
    f.knob("notes", "");
    f.knob("no_commits", "");
    {
        let lock = f.lock();
        let mut serve = f.serve(&lock, 1, 2);
        tick_until(&mut serve, || f.get(&id).state == ItemState::Review);
        serve.tick();
    }
    let note = format!("{} note item-1-a", f.bin_dir.join("kanstack").display());
    assert!(calls_log(&f.bin_dir).contains(&note), "the agent is asked to report on its own branch, through the run's kanstack");

    // A later serve doesn't post it again.
    let lock = f.lock();
    let mut serve = f.serve(&lock, 1, 2);
    serve.tick();

    let body = f.get(&id).body;
    assert_eq!(body.matches("### a: done").count(), 1, "{body}");
    assert!(body.contains("## Reports") && body.contains("report from item-1-a"), "{body}");
    assert!(f.run_for(&id).reported.contains("a"));
}

/// `serve` is killed for real, mid-run, by the stand-in's `spawn` of the second step; a second
/// `serve` reattaches to that step's lane and finishes the item without re-running the first.
#[test]
fn a_killed_serve_resumes_without_rerunning_done_steps() {
    let f = Factory::new();
    f.recipe("two", TWO_STEPS);
    let state = f.root.join("state");
    let (items_dir, store_root) = child_where(&f.workdir, &state);
    let tracker = LocalTracker::in_dir(items_dir);
    let store = RunStore::at(store_root);
    let id = add_item(&tracker, "Two steps", Some("two"));
    f.knob("crash_after_spawn", "item-1-b");
    let bin = f.bin_dir.join("kanstack").to_string_lossy().into_owned();

    let output = child(&f.workdir, &state, &["--kanstack-bin", &bin]);
    assert!(!output.status.success(), "serve was killed mid-spawn: {}", String::from_utf8_lossy(&output.stderr));
    let run = run_for(&store, &id);
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!((run.steps["a"].status, run.steps["b"].status), (StepStatus::Done, StepStatus::Launching));
    assert_eq!(tracker.get(&id).unwrap().state, ItemState::Running);

    let output = child(&f.workdir, &state, &["--once", "--json", "--kanstack-bin", &bin]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success(), "{stdout}{}", String::from_utf8_lossy(&output.stderr));
    // Each event is one line of JSON; the test harness's own output may start the first.
    let events: Vec<serde_json::Value> = stdout.lines().filter_map(|l| l.find('{').map(|at| serde_json::from_str(&l[at..]).unwrap())).collect();
    assert!(events.iter().any(|e| e["schema"] == 1 && e["kind"] == "review" && e["item"] == id.0.as_str()), "{stdout}");

    let item = tracker.get(&id).unwrap();
    assert_eq!(item.state, ItemState::Review);
    assert_eq!((item.body.matches("### a: done").count(), item.body.matches("### b: done").count()), (1, 1), "{}", item.body);
    let calls = calls_log(&f.bin_dir);
    assert_eq!(calls.matches("spawn item-1-a ").count(), 1, "a done step is never re-run:\n{calls}");
    assert_eq!(calls.matches("spawn item-1-b ").count(), 1, "{calls}");
    assert!(!calls.contains("send "), "a live lane is never sent its prompt again:\n{calls}");
}

#[test]
fn serve_refuses_bad_flags() {
    let f = Factory::new();
    let output = child(&f.workdir, &f.root.join("state"), &["--max", "none"]);
    assert_eq!(output.status.code(), Some(2), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--max"));
}

/// Where a `serve` run in `workdir` with state under `state` keeps its items and runs.
fn child_where(workdir: &Path, state: &Path) -> (PathBuf, PathBuf) {
    let output = child_command(workdir, state, "where").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The test harness's own `test serve_child_process ... ` may share the first line.
    let mut lines = stdout.lines().filter_map(|l| l.split_once("where: ").map(|(_, path)| path));
    let (items, runs) = (lines.next().expect(&stdout), lines.next().expect(&stdout));
    (PathBuf::from(items), PathBuf::from(runs))
}

/// `serve <args>` in a child process, waited for with a deadline so a hang fails the test.
fn child(workdir: &Path, state: &Path, args: &[&str]) -> Output {
    let mut child = child_command(workdir, state, &args.join("\n")).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("serve {args:?} didn't exit: {:?}", child.wait_with_output().unwrap());
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn child_command(workdir: &Path, state: &Path, mode: &str) -> Command {
    let mut command = Command::new(env::current_exe().unwrap());
    command
        .args(["serve_child_process", "--exact", "--nocapture", "--test-threads", "1"])
        .current_dir(workdir)
        .env(CHILD, mode)
        .env("KANSTACK_STATE_PATH", state)
        .env_remove("KANSTACK_TRACKER")
        .env_remove("KANSTACK_RETRIES")
        .env_remove("KANSTACK_RECIPE_PATH");
    command
}

/// Not a test of its own: the child process the tests above start (see [`CHILD`]). Without
/// the variable set it does nothing.
#[test]
fn serve_child_process() {
    let Ok(mode) = env::var(CHILD) else { return };
    let cwd = env::current_dir().unwrap();
    if mode == "where" {
        println!("where: {}", LocalTracker::for_repo(&cwd).unwrap().dir().display());
        println!("where: {}", RunStore::for_repo(&cwd).unwrap().root().display());
        std::process::exit(0);
    }
    let args: Vec<String> = mode.lines().map(String::from).collect();
    let code = serve::command(&args);
    std::process::exit(if code == ExitCode::SUCCESS {
        0
    } else if code == ExitCode::from(2) {
        2
    } else {
        1
    });
}
