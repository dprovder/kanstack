//! The `items` subcommands, against the `local` tracker. Every test points
//! `KANSTACK_STATE_PATH` at its own temp dir, so they hold [`ENV`] for as long as it's set;
//! the one that drives [`items::command`] changes the current directory under it too.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Mutex, MutexGuard};

use kanstack_recipe::items;
use kanstack_tracker::{ItemFilter, ItemId, ItemState, Tracker};

static ENV: Mutex<()> = Mutex::new(());

/// A scratch repository and the state dir its items live in, with the environment pointed
/// at them for as long as this is alive.
struct Scratch {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    _env: MutexGuard<'static, ()>,
}

impl Scratch {
    fn new() -> Scratch {
        // A test that panicked holding the lock still restored nothing we rely on: every
        // test sets the variables it needs before it reads them.
        let env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (repo, state) = (root.join("repo"), root.join("state"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let init = Command::new("git").args(["init", "-q"]).current_dir(&repo).status().unwrap();
        assert!(init.success());
        std::env::set_var("KANSTACK_STATE_PATH", &state);
        std::env::remove_var("KANSTACK_TRACKER");
        std::env::remove_var("KANSTACK_RECIPE_PATH");
        Scratch { _tmp: tmp, repo, _env: env }
    }

    /// `items <args>` in the repo: its exit code and stdout.
    fn items(&self, args: &[&str]) -> (ExitCode, String) {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let mut out = Vec::new();
        let code = items::run_in(&self.repo, &args, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    /// `items <args>`, which must succeed: its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let (code, out) = self.items(args);
        assert_eq!(code, ExitCode::SUCCESS, "items {args:?} failed; stdout: {out}");
        out
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.ok(args);
        assert_eq!(out.lines().count(), 1, "one document on one line: {out}");
        let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(doc["schema"], 1);
        doc
    }

    fn tracker(&self) -> Box<dyn Tracker> {
        kanstack_tracker::select(&self.repo).unwrap()
    }
}

fn usage() -> ExitCode {
    ExitCode::from(2)
}

#[test]
fn add_prints_the_new_id_and_defaults_to_backlog() {
    let s = Scratch::new();
    let id = s.ok(&["add", "--title", "Land from the board"]);
    assert_eq!(id, "1\n");
    let item = s.tracker().get(&ItemId("1".into())).unwrap();
    assert_eq!(item.title, "Land from the board");
    assert_eq!(item.state, ItemState::Backlog);
    assert_eq!((item.recipe, item.from, item.body.as_str()), (None, None, ""));

    assert_eq!(s.ok(&["add", "--title", "Second", "--state", "proposed", "--recipe", "land-feature", "--body", "Why."]), "2\n");
    let item = s.tracker().get(&ItemId("2".into())).unwrap();
    assert_eq!(item.state, ItemState::Proposed);
    assert_eq!(item.recipe.as_deref(), Some("land-feature"));
    assert_eq!(item.body.trim_end(), "Why.");
}

#[test]
fn add_reads_its_body_from_a_file() {
    let s = Scratch::new();
    let path = s.repo.join("body.md");
    std::fs::write(&path, "Line one.\n\nLine two.\n").unwrap();
    s.ok(&["add", "--title", "From a file", "--body-file", path.to_str().unwrap()]);
    let item = s.tracker().get(&ItemId("1".into())).unwrap();
    assert_eq!(item.body, "Line one.\n\nLine two.\n");
}

#[test]
fn add_json_is_the_whole_new_item() {
    let s = Scratch::new();
    let doc = s.json(&["add", "--title", "T", "--recipe", "r", "--json"]);
    assert_eq!(doc["item"]["id"], "1");
    assert_eq!(doc["item"]["title"], "T");
    assert_eq!(doc["item"]["state"], "backlog");
    assert_eq!(doc["item"]["recipe"], "r");
    assert!(doc["item"]["from"].is_null());
}

#[test]
fn add_rejects_bad_arguments_with_exit_2_and_creates_nothing() {
    let s = Scratch::new();
    let missing = s.repo.join("missing.md");
    for args in [
        &["add"][..],
        &["add", "--body", "no title"],
        &["add", "--title"],
        &["add", "--title", "T", "--state", "running"],
        &["add", "--title", "T", "--state", "nope"],
        &["add", "--title", "T", "--body", "b", "--body-file", "f"],
        &["add", "--title", "T", "--body-file", missing.to_str().unwrap()],
        &["add", "--title", "T", "--title", "U"],
        &["add", "--title", "T", "--from", "1"],
        &["add", "--title", "T", "stray"],
    ] {
        assert_eq!(s.items(args).0, usage(), "items {args:?}");
    }
    assert!(s.tracker().list(&ItemFilter::default()).unwrap().is_empty());
}

#[test]
fn list_prints_one_line_per_item_and_filters_by_state() {
    let s = Scratch::new();
    assert_eq!(s.ok(&["list"]), "no items\n");
    s.ok(&["add", "--title", "First", "--recipe", "land-feature"]);
    s.ok(&["add", "--title", "Second: with a colon", "--state", "proposed"]);

    let out = s.ok(&["list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    let words = |line: &str| line.split_whitespace().take(3).map(str::to_string).collect::<Vec<_>>();
    assert_eq!(words(lines[0]), ["1", "backlog", "land-feature"]);
    assert!(lines[0].ends_with("  First"), "{out}");
    assert_eq!(words(lines[1]), ["2", "proposed", "-"]);
    assert!(lines[1].ends_with("  Second: with a colon"), "{out}");

    let out = s.ok(&["list", "--state", "proposed"]);
    assert_eq!(out.lines().count(), 1);
    assert!(out.starts_with("2 "), "{out}");

    let doc = s.json(&["list", "--state", "backlog", "--json"]);
    let items = doc["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "First");

    assert_eq!(s.items(&["list", "--state", "closed"]).0, usage());
    assert_eq!(s.items(&["list", "extra"]).0, usage());
}

#[test]
fn show_prints_the_whole_item_body_included() {
    let s = Scratch::new();
    s.ok(&["add", "--title", "Parent"]);
    s.ok(&["propose", "--from", "1", "--title", "Child", "--recipe", "fix", "--body", "Para one.\n\nPara two."]);

    let out = s.ok(&["show", "2"]);
    for expected in ["id:     2", "title:  Child", "state:  proposed", "recipe: fix", "from:   1", "Para one.", "Para two."] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }

    let doc = s.json(&["show", "2", "--json"]);
    assert_eq!(doc["item"]["from"], "1");
    assert_eq!(doc["item"]["body"].as_str().unwrap().trim_end(), "Para one.\n\nPara two.");

    assert_eq!(s.items(&["show", "9"]).0, ExitCode::FAILURE);
    assert_eq!(s.items(&["show"]).0, usage());
    assert_eq!(s.items(&["show", "1", "2"]).0, usage());
}

#[test]
fn promote_moves_proposed_to_backlog_and_refuses_anything_else() {
    let s = Scratch::new();
    s.ok(&["add", "--title", "Proposed", "--state", "proposed"]);
    s.ok(&["add", "--title", "Backlog"]);

    s.ok(&["promote", "1"]);
    assert_eq!(s.tracker().get(&ItemId("1".into())).unwrap().state, ItemState::Backlog);

    // Already backlog now, like item 2 always was: both refused, both left alone.
    assert_eq!(s.items(&["promote", "1"]).0, ExitCode::FAILURE);
    assert_eq!(s.items(&["promote", "2"]).0, ExitCode::FAILURE);
    s.tracker().set_state(&ItemId("2".into()), ItemState::Running).unwrap();
    assert_eq!(s.items(&["promote", "2"]).0, ExitCode::FAILURE);
    assert_eq!(s.tracker().get(&ItemId("2".into())).unwrap().state, ItemState::Running);

    s.ok(&["add", "--title", "Another", "--state", "proposed"]);
    let doc = s.json(&["promote", "3", "--json"]);
    assert_eq!(doc["item"]["state"], "backlog");

    assert_eq!(s.items(&["promote", "9"]).0, ExitCode::FAILURE);
    assert_eq!(s.items(&["promote"]).0, usage());
}

#[test]
fn propose_creates_a_proposed_item_from_an_existing_one() {
    let s = Scratch::new();
    s.ok(&["add", "--title", "Parent"]);
    let body = s.repo.join("proposal.md");
    std::fs::write(&body, "Found while landing 1.\n").unwrap();
    assert_eq!(s.ok(&["propose", "--from", "1", "--title", "Follow-up", "--body-file", body.to_str().unwrap()]), "2\n");

    let item = s.tracker().get(&ItemId("2".into())).unwrap();
    assert_eq!(item.state, ItemState::Proposed);
    assert_eq!(item.from, Some(ItemId("1".into())));
    assert_eq!(item.body, "Found while landing 1.\n");

    let doc = s.json(&["propose", "--from", "1", "--title", "Another", "--json"]);
    assert_eq!(doc["item"]["state"], "proposed");
    assert_eq!(doc["item"]["from"], "1");

    // A dangling `from` is refused before anything is created.
    assert_eq!(s.items(&["propose", "--from", "9", "--title", "Orphan"]).0, ExitCode::FAILURE);
    assert_eq!(s.items(&["propose", "--title", "No from"]).0, usage());
    assert_eq!(s.items(&["propose", "--from", "1"]).0, usage());
    assert_eq!(s.items(&["propose", "--from", "1", "--title", "T", "--state", "backlog"]).0, usage());
    assert_eq!(s.tracker().list(&ItemFilter::default()).unwrap().len(), 3);
}

#[test]
fn set_recipe_takes_a_catalog_name_or_force_and_clears() {
    let s = Scratch::new();
    let id = s.ok(&["add", "--title", "Fix it"]).trim().to_string();
    let recipes = s.repo.join(".kanstack/recipes/bugfix");
    std::fs::create_dir_all(&recipes).unwrap();
    std::fs::write(recipes.join("small.md"), "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
    let recipe = || s.tracker().get(&ItemId(id.clone())).unwrap().recipe;

    assert_eq!(s.ok(&["set-recipe", &id, "bugfix/small"]), format!("✓ item {id}: recipe bugfix/small\n"));
    assert_eq!(recipe().as_deref(), Some("bugfix/small"));

    let (code, out) = s.items(&["set-recipe", &id, "bugfix/smal"]);
    assert_eq!((code, out.as_str()), (ExitCode::FAILURE, ""), "not in the catalog");
    assert_eq!(recipe().as_deref(), Some("bugfix/small"), "unchanged");

    let doc = s.json(&["set-recipe", &id, "recipes/odd.md", "--force", "--json"]);
    assert_eq!(doc["item"]["recipe"], "recipes/odd.md");

    assert_eq!(s.ok(&["set-recipe", &id, "--clear"]), format!("✓ item {id}: recipe cleared\n"));
    assert_eq!(recipe(), None);

    for bad in [&["set-recipe"][..], &["set-recipe", &id], &["set-recipe", &id, "x", "--clear"], &["set-recipe", &id, "--clear", "--force"], &["set-recipe", &id, "a", "b"], &["set-recipe", &id, " "], &["set-recipe", &id, "x", "--bogus"]] {
        assert_eq!(s.items(bad).0, usage(), "{bad:?}");
    }
    let (code, _) = s.items(&["set-recipe", "99", "bugfix/small"]);
    assert_eq!(code, ExitCode::FAILURE, "no such item");
}

#[test]
fn bare_unknown_and_help_invocations() {
    let s = Scratch::new();
    assert_eq!(s.items(&[]).0, usage());
    assert_eq!(s.items(&["frobnicate"]).0, usage());
    assert_eq!(s.items(&["--help"]).0, ExitCode::SUCCESS);
    assert_eq!(items::command(&["-h".to_string()]), ExitCode::SUCCESS);
    assert_eq!(items::command(&["add".to_string()]), usage());
}

/// The one test that goes through `command`'s own repository lookup: run from a
/// subdirectory, it still finds the repo's items.
#[test]
fn command_opens_the_tracker_for_the_git_root() {
    let s = Scratch::new();
    let sub = s.repo.join("nested").join("deeper");
    std::fs::create_dir_all(&sub).unwrap();
    let cwd = CurrentDir::change_to(&sub);
    let code = items::command(&["add".to_string(), "--title".to_string(), "From below".to_string()]);
    drop(cwd);
    assert_eq!(code, ExitCode::SUCCESS);
    let items = s.tracker().list(&ItemFilter::default()).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "From below");
}

/// Changes the current directory back when dropped, even if the test panics in between.
struct CurrentDir(PathBuf);

impl CurrentDir {
    fn change_to(dir: &Path) -> CurrentDir {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        CurrentDir(previous)
    }
}

impl Drop for CurrentDir {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}
