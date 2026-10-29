//! `land <item>`: an item in review lands its run's branches bottom of the stack first, stops at
//! the first one that wouldn't land cleanly, and is marked done with its lanes stopped; or, in
//! `pr` mode, gets a PR and stays in review. These drive a stand-in `but` written below, the
//! stand-in `kanstack` in `common` for `stop`, and a local tracker and run store in a scratch
//! directory. Runs are written straight into the store, as `serve` would have left them.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use common::{actions, write_fake_kanstack};

use kanstack_recipe::graph::resolve_workstream;
use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::land::{But, LandMode, LandOptions, Lander};
use kanstack_recipe::recipe;
use kanstack_recipe::state::{recipe_hash, RunState, RunStatus, RunStore, StepStatus};
use kanstack_tracker::{Draft, Item, ItemId, ItemState, LocalTracker, Tracker};

/// `work`, with `audit` stacked on it and `polish` stacked on `audit`. Step ids sort the other
/// way round from the stack, so landing in map order would land a branch before its base.
const STACKED: &str = "---\nversion: 1\nsteps:\n  work:\n    agent: codex\n    prompt: W\n  audit:\n    agent: claude\n    needs: [work]\n    on: work\n    spawn: stack\n    prompt: A\n  polish:\n    agent: claude\n    needs: [audit]\n    on: audit\n    spawn: stack\n    prompt: P\n---\n";

const ONE_STEP: &str = "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\n";

/// Writes an executable stand-in `but` into `dir`. Every call is appended to `but_calls.log`.
///
/// - `branch show <b> --check --json` reports `<b>` as one commit ahead, or none if `empty`
///   lists it; it merges cleanly unless a line of `conflicts` is `<b> <path>`.
/// - `land <b> --yes --json` fails if `land_fail` names `<b>`, and, like the real one, refuses
///   `<b>` while a line of `stacked` says `<b> <base>` and `<base>` isn't in `landed` yet.
///   Otherwise it appends `<b>` to `landed`.
/// - `pr new <b> -t --json` prints a review with a link naming `<b>`.
fn write_fake_but(dir: &Path) -> PathBuf {
    let path = dir.join("but");
    let script = r#"#!/bin/sh
DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
echo "$*" >> "$DIR/but_calls.log"
case "$1" in
  branch)
    b="$3"
    ahead=1
    if grep -qx "$b" "$DIR/empty" 2>/dev/null; then ahead=0; fi
    conflict=$(grep "^$b " "$DIR/conflicts" 2>/dev/null | cut -d' ' -f2)
    if [ -n "$conflict" ]; then
      printf '{"branch":"%s","commitsAhead":%s,"commits":[],"mergeCheck":{"mergesCleanly":false,"conflictingFiles":[{"path":"%s","branch_commits":[],"upstream_commits":[]}]}}\n' "$b" "$ahead" "$conflict"
    else
      printf '{"branch":"%s","commitsAhead":%s,"commits":[],"mergeCheck":{"mergesCleanly":true,"conflictingFiles":[]}}\n' "$b" "$ahead"
    fi
    ;;
  land)
    b="$2"
    if [ "$(cat "$DIR/land_fail" 2>/dev/null)" = "$b" ]; then
      echo "Error: forced land failure for $b" >&2
      exit 1
    fi
    base=$(grep "^$b " "$DIR/stacked" 2>/dev/null | cut -d' ' -f2)
    if [ -n "$base" ] && ! grep -qx "$base" "$DIR/landed" 2>/dev/null; then
      echo "Error: Refusing to land \`$b\`: it is stacked on top of 1 other segment(s) ($base)" >&2
      exit 1
    fi
    echo "$b" >> "$DIR/landed"
    echo '{}'
    ;;
  pr)
    printf '{"reviews":[{"branch":"%s","number":7,"url":"https://forge.test/pr/%s"}]}\n' "$3" "$3"
    ;;
  *)
    echo "fake but: unsupported command: $*" >&2
    exit 2
    ;;
esac
"#;
    fs::write(&path, script).unwrap();
    let mut perms = fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).unwrap();
    path
}

struct Scratch {
    _tmp: tempfile::TempDir,
    bin_dir: PathBuf,
    workdir: PathBuf,
    tracker: LocalTracker,
    store: RunStore,
    kanstack: KanstackClient,
    but: But,
}

impl Scratch {
    fn new() -> Scratch {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (bin_dir, workdir) = (root.join("bin"), root.join("work"));
        fs::create_dir_all(&bin_dir).unwrap();
        fs::create_dir_all(&workdir).unwrap();
        Scratch {
            kanstack: KanstackClient::new(write_fake_kanstack(&bin_dir)),
            but: But::new(write_fake_but(&bin_dir)),
            tracker: LocalTracker::in_dir(root.join("items")),
            store: RunStore::at(root.join("runs")),
            _tmp: tmp,
            bin_dir,
            workdir,
        }
    }

    fn knob(&self, name: &str, contents: &str) {
        fs::write(self.bin_dir.join(name), contents).unwrap();
    }

    fn item(&self, state: ItemState) -> ItemId {
        let draft = Draft { title: "Land me".to_string(), body: "The work.".to_string(), recipe: Some("r".to_string()), from: None, state };
        self.tracker.create(&draft).unwrap()
    }

    fn get(&self, id: &ItemId) -> Item {
        self.tracker.get(id).unwrap()
    }

    /// A completed run of `source` for `item`, as `serve` leaves one: every step done on its
    /// `item-<id>-` branch, started in recipe order, with a lane open for each branch.
    fn completed_run(&self, item: &ItemId, source: &str) -> RunState {
        let path = self.workdir.join(format!("recipe-{}.md", item.0));
        fs::write(&path, source).unwrap();
        let parsed = recipe::parse(source).unwrap();
        let mut run = RunState::new(self.store.new_run_id("land"), &parsed, &path, recipe_hash(source), self.workdir.clone());
        run.item = Some(item.0.clone());
        run.branch_prefix = format!("item-{}-", item.0);
        run.status = RunStatus::Done;
        run.settled = true;
        // Launch order follows `needs`; the source lists steps that way.
        let order: Vec<&str> = source.lines().filter_map(|l| l.strip_prefix("  ")?.strip_suffix(':')).filter(|l| !l.starts_with(' ')).collect();
        let mut lanes = String::new();
        for (step_id, rec) in run.steps.iter_mut() {
            let branch = format!("{}{}", run.branch_prefix, resolve_workstream(&parsed, step_id));
            rec.status = StepStatus::Done;
            rec.attempt = 1;
            rec.started_at = Some(1_000 + order.iter().position(|s| s == step_id).unwrap() as u64);
            if !lanes.lines().any(|l| l == branch) {
                lanes.push_str(&format!("{branch}\n"));
            }
            rec.branch = Some(branch);
        }
        self.store.save(&run).unwrap();
        let open = fs::read_to_string(self.bin_dir.join("lanes")).unwrap_or_default();
        fs::write(self.bin_dir.join("lanes"), open + &lanes).unwrap();
        run
    }

    /// `land <item>`: its exit code and stdout.
    fn land(&self, item: &ItemId, opts: LandOptions) -> (ExitCode, String) {
        let lander = Lander { repo: &self.workdir, tracker: &self.tracker, store: &self.store, kanstack: &self.kanstack, but: &self.but };
        let mut out = Vec::new();
        let code = lander.land(item, &opts, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    fn but_calls(&self) -> Vec<String> {
        fs::read_to_string(self.bin_dir.join("but_calls.log")).unwrap_or_default().lines().map(String::from).collect()
    }

    fn lands(&self) -> Vec<String> {
        self.but_calls().into_iter().filter(|c| c.starts_with("land ")).collect()
    }

    fn stops(&self) -> Vec<String> {
        actions(&self.bin_dir).into_iter().filter(|c| c.starts_with("stop ")).collect()
    }
}

fn json(out: &str) -> serde_json::Value {
    assert_eq!(out.lines().count(), 1, "one document on one line: {out}");
    let doc: serde_json::Value = serde_json::from_str(out).unwrap();
    assert_eq!(doc["schema"], 1);
    doc
}

#[test]
fn refuses_an_item_that_is_not_in_review() {
    let s = Scratch::new();
    for state in [ItemState::Running, ItemState::Failed, ItemState::Backlog, ItemState::Done] {
        let item = s.item(state);
        s.completed_run(&item, ONE_STEP);
        let (code, out) = s.land(&item, LandOptions::default());
        assert_eq!(code, ExitCode::FAILURE, "a {state} item");
        assert!(out.is_empty(), "{out}");
        assert_eq!(s.get(&item).state, state, "left as it was");
    }
    assert!(s.but_calls().is_empty(), "nothing was checked or landed: {:?}", s.but_calls());
    assert!(s.stops().is_empty());
}

#[test]
fn refuses_an_item_in_review_without_a_completed_run() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    let mut run = s.completed_run(&item, ONE_STEP);
    run.status = RunStatus::Failed;
    s.store.save(&run).unwrap();

    assert_eq!(s.land(&item, LandOptions::default()).0, ExitCode::FAILURE);
    assert!(s.but_calls().is_empty());
    assert_eq!(s.get(&item).state, ItemState::Review);
}

#[test]
fn lands_a_stack_bottom_first_then_marks_the_item_done() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, STACKED);
    let (work, audit, polish) = ("item-1-work", "item-1-work-audit", "item-1-work-audit-polish");
    s.knob("stacked", &format!("{audit} {work}\n{polish} {audit}\n"));

    let (code, out) = s.land(&item, LandOptions::default());
    assert_eq!(code, ExitCode::SUCCESS, "{out}");
    assert_eq!(
        s.but_calls(),
        vec![
            format!("branch show {work} --check --json"),
            format!("land {work} --yes --json"),
            format!("branch show {audit} --check --json"),
            format!("land {audit} --yes --json"),
            format!("branch show {polish} --check --json"),
            format!("land {polish} --yes --json"),
        ],
        "each branch is checked, then landed, bottom of the stack first"
    );

    let landed = s.get(&item);
    assert_eq!(landed.state, ItemState::Done);
    let comment = landed.body.split("### Landed").nth(1).unwrap_or_else(|| panic!("no Landed comment: {}", landed.body));
    let at = |b: &str| comment.find(&format!("`{b}`: 1 commit")).unwrap_or_else(|| panic!("{b} isn't listed: {comment}"));
    assert!(at(work) < at(audit) && at(audit) < at(polish), "listed bottom first: {comment}");
}

#[test]
fn stops_at_the_first_branch_that_would_not_land_cleanly() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, STACKED);
    s.knob("conflicts", "item-1-work-audit src/lib.rs\n");

    let (code, out) = s.land(&item, LandOptions { json: true, ..LandOptions::default() });
    assert_eq!(code, ExitCode::FAILURE);
    assert_eq!(s.lands(), vec!["land item-1-work --yes --json"], "the base landed, nothing at or after the conflict did");
    assert!(!s.but_calls().iter().any(|c| c.contains("polish")), "the branch above isn't even checked: {:?}", s.but_calls());

    let doc = json(&out);
    assert_eq!(doc["stopped_at"]["branch"], "item-1-work-audit");
    assert!(doc["stopped_at"]["reason"].as_str().unwrap().contains("src/lib.rs"), "{doc}");
    assert_eq!(doc["landed"][0]["branch"], "item-1-work");
    assert_eq!(doc["state"], "review");

    let after = s.get(&item);
    assert_eq!(after.state, ItemState::Review, "the item waits for a fix");
    assert!(after.body.contains("### Landing stopped at `item-1-work-audit`"), "{}", after.body);
    assert!(after.body.contains("src/lib.rs") && after.body.contains("`item-1-work-audit-polish`"), "{}", after.body);
    assert!(s.stops().is_empty(), "no lane is stopped: {:?}", s.stops());
}

#[test]
fn a_failed_land_stops_there_too() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, STACKED);
    s.knob("land_fail", "item-1-work");

    assert_eq!(s.land(&item, LandOptions::default()).0, ExitCode::FAILURE);
    assert_eq!(s.lands(), vec!["land item-1-work --yes --json"]);
    assert_eq!(s.but_calls().len(), 2, "nothing after the failed land: {:?}", s.but_calls());
    let after = s.get(&item);
    assert_eq!(after.state, ItemState::Review);
    assert!(after.body.contains("forced land failure") && after.body.contains("Nothing was landed."), "{}", after.body);
}

#[test]
fn skips_a_branch_with_nothing_to_land() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, STACKED);
    s.knob("empty", "item-1-work-audit-polish\n");

    let (code, out) = s.land(&item, LandOptions { json: true, ..LandOptions::default() });
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(s.lands(), vec!["land item-1-work --yes --json", "land item-1-work-audit --yes --json"]);
    assert_eq!(json(&out)["skipped"], serde_json::json!(["item-1-work-audit-polish"]));
    assert!(s.get(&item).body.contains("Nothing to land on `item-1-work-audit-polish`."));
}

#[test]
fn stops_every_lane_after_landing_unless_told_to_keep_them() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, STACKED);
    let (code, out) = s.land(&item, LandOptions { json: true, ..LandOptions::default() });
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(s.stops(), vec!["stop item-1-work --json", "stop item-1-work-audit --json", "stop item-1-work-audit-polish --json"]);
    let doc = json(&out);
    assert_eq!(doc["state"], "done");
    assert_eq!(doc["lanes_stopped"].as_array().unwrap().len(), 3);

    let kept = s.item(ItemState::Review);
    s.completed_run(&kept, ONE_STEP);
    let (code, _) = s.land(&kept, LandOptions { keep_panes: true, ..LandOptions::default() });
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(s.get(&kept).state, ItemState::Done);
    assert!(!s.stops().iter().any(|c| c.contains("item-2-")), "--keep-panes stops nothing: {:?}", s.stops());
    assert!(fs::read_to_string(s.bin_dir.join("lanes")).unwrap().lines().any(|l| l == "item-2-a"), "its lane is still open");
}

#[test]
fn lands_from_the_newest_completed_run() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, ONE_STEP);
    // A newer run of another recipe: it's the one whose branches land.
    s.completed_run(&item, "---\nversion: 1\nsteps:\n  b:\n    agent: codex\n    prompt: B\n---\n");

    assert_eq!(s.land(&item, LandOptions::default()).0, ExitCode::SUCCESS);
    assert_eq!(s.lands(), vec!["land item-1-b --yes --json"]);
}

#[test]
fn pr_mode_opens_one_pr_for_the_top_branch_and_leaves_the_item_in_review() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, STACKED);

    let (code, out) = s.land(&item, LandOptions { land: LandMode::Pr, json: true, ..LandOptions::default() });
    assert_eq!(code, ExitCode::SUCCESS, "{out}");
    let prs: Vec<String> = s.but_calls().into_iter().filter(|c| c.starts_with("pr ")).collect();
    assert_eq!(prs, vec!["pr new item-1-work-audit-polish -t --json"]);
    assert!(s.lands().is_empty(), "nothing lands directly: {:?}", s.but_calls());

    let doc = json(&out);
    assert_eq!(doc["mode"], "pr");
    assert_eq!(doc["prs"][0]["url"], "https://forge.test/pr/item-1-work-audit-polish");
    assert_eq!(doc["state"], "review");

    let after = s.get(&item);
    assert_eq!(after.state, ItemState::Review);
    assert!(after.body.contains("https://forge.test/pr/item-1-work-audit-polish"), "the link is on the item: {}", after.body);
    assert!(after.body.contains("out of scope"), "the comment says the merge isn't tracked: {}", after.body);
    assert!(s.stops().is_empty(), "lanes stay up for review feedback");
}

#[test]
fn a_recipe_land_setting_overrides_the_environment() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, "---\nversion: 1\nland: pr\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\n");
    assert_eq!(s.land(&item, LandOptions::default()).0, ExitCode::SUCCESS);
    assert!(s.but_calls().contains(&"pr new item-1-a -t --json".to_string()), "{:?}", s.but_calls());
    assert!(s.lands().is_empty());

    let direct = s.item(ItemState::Review);
    s.completed_run(&direct, "---\nversion: 1\nland: direct\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\n");
    assert_eq!(s.land(&direct, LandOptions { land: LandMode::Pr, ..LandOptions::default() }).0, ExitCode::SUCCESS);
    assert_eq!(s.lands(), vec!["land item-2-a --yes --json"]);
}

#[test]
fn dark_mode_is_refused_until_it_exists() {
    let s = Scratch::new();
    let item = s.item(ItemState::Review);
    s.completed_run(&item, "---\nversion: 1\nfactory: dark\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\n");
    assert_eq!(s.land(&item, LandOptions::default()).0, ExitCode::FAILURE);
    assert!(s.but_calls().is_empty());
    assert_eq!(s.get(&item).state, ItemState::Review);
}
