//! The `aggregate:` step: how it parses and validates, when it becomes runnable, the outcomes
//! block its prompt carries, and an end-to-end `serve` run against the stand-in `kanstack` in
//! `common` where it's spawned last, as advisory.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use common::{actions, fast_opts, init_repo, write_fake_kanstack};

use kanstack_recipe::aggregate::{launch_prompt, runnable_steps};
use kanstack_recipe::graph::StepState;
use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::lock::OrchestratorLock;
use kanstack_recipe::recipe::{parse, DoneWhen, Recipe, RecipeError, SpawnMode};
use kanstack_recipe::serve::{Serve, ServeOptions};
use kanstack_recipe::state::{RunState, RunStore, StepRecord, StepStatus};
use kanstack_tracker::{Draft, ItemId, ItemState, LocalTracker, Tracker};

/// `implement`, then `review` and `docs` on its branch, closed by `followups`.
const RECIPE: &str = "---\nversion: 1\naggregate: followups\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: Review\n  docs:\n    agent: codex\n    needs: [review]\n    prompt: Docs\n  followups:\n    agent: claude\n    prompt: Read the run and propose follow-ups.\n---\nctx\n";

fn recipe() -> Recipe {
    parse(RECIPE).unwrap()
}

#[test]
fn aggregate_defaults_to_advisory_and_a_note_and_needs_every_other_step() {
    let recipe = recipe();
    assert_eq!(recipe.aggregate.as_deref(), Some("followups"));
    let step = &recipe.steps["followups"];
    assert_eq!((step.spawn, step.done_when), (Some(SpawnMode::Advisory), DoneWhen::Note));
    assert_eq!(step.needs, vec!["docs", "implement", "review"]);
    // The other steps keep their own defaults.
    assert_eq!((recipe.steps["docs"].spawn, recipe.steps["docs"].done_when), (None, DoneWhen::Commits));

    // What the recipe sets wins, and needs it lists come first, without repeats.
    let recipe = parse(
        "---\nversion: 1\naggregate: wrap\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    prompt: B\n  wrap:\n    agent: claude\n    needs: [b]\n    on: a\n    done_when: either\n    prompt: W\n---\n",
    )
    .unwrap();
    let wrap = &recipe.steps["wrap"];
    assert_eq!((wrap.spawn, wrap.done_when, wrap.on.as_deref()), (None, DoneWhen::Either, Some("a")), "an `on` aggregate isn't made advisory");
    assert_eq!(wrap.needs, vec!["b", "a"]);

    // A recipe without one has none.
    let recipe = parse("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
    assert_eq!(recipe.aggregate, None);
}

#[test]
fn aggregate_must_name_a_step_that_nothing_else_needs_or_runs_on() {
    let err = parse("---\nversion: 1\naggregate: nope\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap_err();
    assert!(matches!(&err, RecipeError::UnknownAggregate(s) if s == "nope"), "{err:?}");
    assert!(err.to_string().contains("`aggregate` names unknown step \"nope\""), "{err}");

    let err = parse("---\nversion: 1\naggregate: wrap\nsteps:\n  a:\n    agent: codex\n    needs: [wrap]\n    prompt: A\n  wrap:\n    agent: claude\n    prompt: W\n---\n").unwrap_err();
    assert!(
        matches!(&err, RecipeError::AggregateReferenced { step, aggregate, field } if step == "a" && aggregate == "wrap" && *field == "needs"),
        "{err:?}"
    );
    assert!(err.to_string().contains("runs after every other step"), "{err}");

    let err = parse("---\nversion: 1\naggregate: wrap\nsteps:\n  a:\n    agent: codex\n    on: wrap\n    prompt: A\n  wrap:\n    agent: claude\n    prompt: W\n---\n").unwrap_err();
    assert!(matches!(&err, RecipeError::AggregateReferenced { field, .. } if *field == "on"), "{err:?}");
}

fn states(pairs: &[(&str, StepState)]) -> HashMap<String, StepState> {
    pairs.iter().map(|(id, s)| (id.to_string(), *s)).collect()
}

#[test]
fn aggregate_runs_once_nothing_else_can_even_after_a_failure() {
    use StepState::*;
    let recipe = recipe();
    let runnable = |pairs: &[(&str, StepState)]| runnable_steps(&recipe, &states(pairs));

    assert_eq!(runnable(&[]), vec!["implement"]);
    assert_eq!(runnable(&[("implement", Complete), ("review", Complete), ("docs", Running)]), Vec::<String>::new());
    assert_eq!(runnable(&[("implement", Complete), ("review", Complete), ("docs", Pending)]), vec!["docs"]);
    assert_eq!(runnable(&[("implement", Complete), ("review", Complete), ("docs", Complete)]), vec!["followups"]);

    // review failed, so docs can never run: the aggregate goes anyway, to report it.
    assert_eq!(runnable(&[("implement", Complete), ("review", Failed), ("docs", Pending)]), vec!["followups"]);
    assert_eq!(runnable(&[("implement", Failed)]), vec!["followups"]);
    // It runs once.
    assert_eq!(runnable(&[("implement", Failed), ("followups", Running)]), Vec::<String>::new());
    assert_eq!(runnable(&[("implement", Failed), ("followups", Complete)]), Vec::<String>::new());
}

#[test]
fn aggregate_waits_for_a_sibling_still_running_after_a_failure() {
    use StepState::*;
    let recipe = parse(
        "---\nversion: 1\naggregate: wrap\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    prompt: B\n  wrap:\n    agent: claude\n    prompt: W\n---\n",
    )
    .unwrap();
    assert!(runnable_steps(&recipe, &states(&[("a", Failed), ("b", Running)])).is_empty());
    assert_eq!(runnable_steps(&recipe, &states(&[("a", Failed), ("b", Pending)])), vec!["b"], "b can still run");
    assert_eq!(runnable_steps(&recipe, &states(&[("a", Failed), ("b", Complete)])), vec!["wrap"]);
}

/// A run of [`RECIPE`] where implement committed and reported, review failed after a retry,
/// and docs never ran.
fn finished_run(recipe: &Recipe, item: Option<&str>) -> RunState {
    let mut run = RunState::new("r1", recipe, Path::new("recipe.md"), String::new(), PathBuf::from("."));
    run.item = item.map(String::from);
    run.steps.insert(
        "implement".to_string(),
        StepRecord {
            status: StepStatus::Done,
            branch: Some("item-44-implement".to_string()),
            base: Some("1111111aaaaaaa".to_string()),
            tip: Some("2222222bbbbbbb".to_string()),
            report: Some("Added the widget.\n".to_string()),
            ..StepRecord::default()
        },
    );
    run.steps.insert(
        "review".to_string(),
        StepRecord {
            status: StepStatus::Failed,
            branch: Some("item-44-implement".to_string()),
            base: Some("2222222bbbbbbb".to_string()),
            tip: Some("2222222bbbbbbb".to_string()),
            last_error: Some("verify failed: `cargo test` exited 101 (out of retries, 1/1)".to_string()),
            retries: 1,
            retry_budget: 1,
            ..StepRecord::default()
        },
    );
    run
}

#[test]
fn the_block_lists_each_steps_report_commit_range_or_failure_and_asks_for_proposals() {
    let recipe = recipe();
    let run = finished_run(&recipe, Some("44"));
    let prompt = launch_prompt(&recipe, &run, "followups", "item-44-followups");

    assert!(prompt.starts_with(&recipe.effective_prompt("followups")), "{prompt}");
    let block = &prompt[recipe.effective_prompt("followups").len()..];
    let expected_outcomes = "\n\n## How the run went\n\nEvery other step of this run has finished. Here is what each one left.\n\
        \n### docs: never ran\n\nIt never ran: it needs `review`, which didn't finish.\n\
        \n### implement: done\n\nCommits `1111111..2222222` on `item-44-implement`.\n\nIts report:\n\nAdded the widget.\n\
        \n### review: failed (retried 1/1)\n\nNo commits on `item-44-implement`.\n\nIt failed: verify failed: `cargo test` exited 101 (out of retries, 1/1)\n";
    assert!(block.starts_with(expected_outcomes), "{block}");
    assert!(!block.contains("### followups"), "the aggregate doesn't list itself: {block}");

    let follow_ups = &block[expected_outcomes.len()..];
    assert!(follow_ups.starts_with("\n## Follow-ups\n\nThis run works on item 44."), "{follow_ups}");
    let list = follow_ups.find("kanstack-recipe items list --json").expect(follow_ups);
    let propose = follow_ups
        .find("kanstack-recipe items propose --from 44 --title \"<title>\" --body \"<what to do and why>\"")
        .expect(follow_ups);
    let note = follow_ups.find("kanstack note item-44-followups \"<report>\"").expect(follow_ups);
    assert!(list < propose && propose < note, "list, then propose, then note: {follow_ups}");
    assert!(follow_ups.contains("don't propose anything that's already there"), "{follow_ups}");
    assert_eq!(prompt.matches("kanstack note").count(), 1, "one report instruction, not two: {prompt}");
}

#[test]
fn without_an_item_the_block_keeps_the_outcomes_and_asks_for_a_summary() {
    let recipe = recipe();
    let with_item = launch_prompt(&recipe, &finished_run(&recipe, Some("44")), "followups", "followups");
    let prompt = launch_prompt(&recipe, &finished_run(&recipe, None), "followups", "followups");

    let outcomes_end = with_item.find("\n## Follow-ups").unwrap();
    assert_eq!(&prompt[..outcomes_end], &with_item[..outcomes_end], "the same outcomes");
    let rest = &prompt[outcomes_end..];
    assert!(rest.starts_with("\n## Summary\n\nThis run isn't working on a queue item, so don't propose follow-ups."), "{rest}");
    assert!(rest.ends_with("by running: kanstack note followups \"<summary>\"\n"), "{rest}");
    assert!(!prompt.contains("items propose") && !prompt.contains("items list"), "{prompt}");
}

#[test]
fn other_steps_prompts_are_left_alone() {
    let recipe = recipe();
    let run = finished_run(&recipe, Some("44"));
    for id in ["implement", "review", "docs"] {
        assert_eq!(launch_prompt(&recipe, &run, id, id), recipe.launch_prompt(id, id));
    }
}

/// A scratch factory, like `tests/serve.rs`'s: the stand-in kanstack, a git repository with
/// recipes in `.kanstack/recipes/`, a local tracker and a run store.
struct Factory {
    _tmp: tempfile::TempDir,
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
            bin_dir,
            workdir,
        }
    }

    fn recipe(&self, name: &str, source: &str) {
        let dir = self.workdir.join(".kanstack/recipes");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.md")), source).unwrap();
    }

    fn item(&self, title: &str, recipe: &str) -> ItemId {
        let draft = Draft { title: title.to_string(), body: format!("{title}, please."), recipe: Some(recipe.to_string()), ..Draft::default() };
        self.tracker.create(&draft).unwrap()
    }

    fn serve<'a>(&'a self, lock: &'a OrchestratorLock, retries: u32) -> Serve<'a> {
        let opts = ServeOptions { max: 1, retries, recipe_path: None, run: fast_opts() };
        Serve::new(&self.workdir, &self.tracker, &self.store, &self.client, opts, lock).unwrap()
    }

    /// Ticks `serve` until `item` leaves `running`, then returns its state.
    fn serve_until_settled(&self, item: &ItemId, retries: u32) -> ItemState {
        let lock = OrchestratorLock::try_acquire(&self.store).unwrap();
        let mut serve = self.serve(&lock, retries);
        for _ in 0..400 {
            serve.tick();
            let state = self.tracker.get(item).unwrap().state;
            if !matches!(state, ItemState::Backlog | ItemState::Running) {
                return state;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("item {item} never settled: {:?}", actions(&self.bin_dir));
    }
}

/// `implement` reports and commits; `followups` closes the run.
const SERVE_RECIPE: &str = "---\nversion: 1\naggregate: followups\nsteps:\n  implement:\n    agent: codex\n    done_when: either\n    prompt: Implement\n  followups:\n    agent: claude\n    prompt: Propose follow-ups.\n---\nctx\n";

#[test]
fn serve_spawns_the_aggregate_last_as_advisory_with_the_runs_outcomes() {
    let f = Factory::new();
    f.recipe("agg", SERVE_RECIPE);
    fs::write(f.bin_dir.join("notes"), "").unwrap();
    let id = f.item("Widget", "agg");

    assert_eq!(f.serve_until_settled(&id, 0), ItemState::Review);

    let calls = actions(&f.bin_dir);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("spawn item-1-implement --agent codex --prompt Implement"), "{calls:?}");
    let last = &calls[1];
    assert!(last.starts_with("spawn item-1-followups --agent claude --advisory --prompt Propose follow-ups."), "{last}");
    assert!(last.contains("### implement: done"), "{last}");
    assert!(last.contains("Its report:  report from item-1-implement"), "{last}");
    assert!(last.contains("Commits `") && last.contains("` on `item-1-implement`."), "{last}");
    assert!(last.contains("kanstack-recipe items propose --from 1 --title"), "{last}");
    assert!(last.contains("kanstack note item-1-followups \"<report>\""), "{last}");

    let body = f.tracker.get(&id).unwrap().body;
    assert!(body.contains("### followups: done") && body.contains("report from item-1-followups"), "{body}");
}

#[test]
fn serve_still_runs_the_aggregate_after_a_step_fails_and_the_item_fails() {
    let f = Factory::new();
    f.recipe(
        "agg",
        "---\nversion: 1\naggregate: followups\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n    verify:\n      - \"echo 'assertion failed: widget'; exit 1\"\n  docs:\n    agent: codex\n    needs: [implement]\n    prompt: Docs\n  followups:\n    agent: claude\n    prompt: Propose follow-ups.\n---\nctx\n",
    );
    fs::write(f.bin_dir.join("notes"), "").unwrap();
    let id = f.item("Widget", "agg");

    assert_eq!(f.serve_until_settled(&id, 0), ItemState::Failed);

    let calls = actions(&f.bin_dir);
    let spawned: Vec<&str> = calls.iter().filter_map(|c| c.strip_prefix("spawn ")).map(|c| c.split(' ').next().unwrap()).collect();
    assert_eq!(spawned, vec!["item-1-implement", "item-1-followups"], "docs never runs: {calls:?}");
    let last = calls.last().unwrap();
    assert!(last.contains("--advisory"), "{last}");
    assert!(last.contains("### implement: failed") && last.contains("It failed: verify failed"), "{last}");
    assert!(last.contains("### docs: never ran") && last.contains("it needs `implement`, which didn't finish."), "{last}");
}
