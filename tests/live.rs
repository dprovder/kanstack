//! End-to-end tests against a real `but` binary.
//!
//! Ignored by default because they need the GitButler CLI installed. Run with:
//!
//! ```text
//! cargo test --test live -- --ignored --test-threads=1
//! ```
//!
//! Everything happens inside a temporary directory with `HOME` redirected, so these do
//! not touch the caller's GitButler project registry or settings.

use std::path::PathBuf;
use std::process::Command;

use kanstack::board::{Board, CardKind, ColumnKind};
use kanstack::but::{But, MIN_VERSION};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("kanstack-live-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let sb = Sandbox { root };

        // Redirect HOME for this process so `But`'s spawned commands inherit it and write
        // their config into the sandbox rather than the developer's real one.
        std::env::set_var("HOME", sb.home());

        sb.git(&["init", "-q", "."]);
        sb.git(&["config", "user.email", "test@example.com"]);
        sb.git(&["config", "user.name", "Test"]);
        sb.write("README.md", "base\n");
        sb.git(&["add", "."]);
        sb.git(&["commit", "-qm", "base"]);
        sb.but(&["setup", "--init", "-j"]);
        sb
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn write(&self, rel: &str, contents: &str) {
        std::fs::write(self.repo().join(rel), contents).unwrap();
    }

    fn run(&self, bin: &str, args: &[&str]) -> String {
        let out = Command::new(bin)
            .args(args)
            .current_dir(self.repo())
            .env("HOME", self.home())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));
        assert!(
            out.status.success(),
            "{bin} {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.run("git", args)
    }
    fn but(&self, args: &[&str]) -> String {
        self.run("but", args)
    }

    /// Creates a branch with one commit touching `file`.
    fn branch_with_commit(&self, branch: &str, file: &str, message: &str) {
        self.write(file, "content\n");
        self.but(&["branch", "new", branch]);
        self.but(&["commit", branch, "-m", message]);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn skip_if_no_but() -> bool {
    Command::new("but").arg("--version").output().is_err()
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn discovers_and_version_gates_the_cli() {
    if skip_if_no_but() {
        eprintln!("skipping: `but` not on PATH");
        return;
    }
    let sb = Sandbox::new("discover");
    let but = But::discover(&sb.repo()).expect("discover");
    assert!(
        but.version() >= MIN_VERSION,
        "installed but {} is below the supported floor {MIN_VERSION}",
        but.version()
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn reads_a_real_workspace_into_a_board() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("read");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");
    sb.branch_with_commit("feat-ui", "b.txt", "Redesign settings page");
    sb.write("wip.txt", "scratch\n");

    let but = But::discover(&sb.repo()).unwrap();
    let board = Board::from_status(&but.status().expect("status parses"));

    let titles: Vec<&str> = board.columns.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["unassigned", "feat-auth", "feat-ui"]);

    assert_eq!(board.columns[0].kind, ColumnKind::Unassigned);
    assert_eq!(
        board.columns[0]
            .cards
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>(),
        ["wip.txt"],
        "an unstaged file shows up in the backlog lane"
    );

    let auth = &board.columns[1];
    assert_eq!(auth.cards.len(), 1);
    assert_eq!(auth.cards[0].title, "Add auth middleware");
    assert_eq!(auth.cards[0].kind, CardKind::Commit);
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn moving_a_card_between_lanes_rubs_and_returns_fresh_state() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("move");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");
    sb.branch_with_commit("feat-ui", "b.txt", "Redesign settings page");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());

    let commit = before.columns[1].cards[0].clone();
    let target = before.columns[2].drop_target.clone();
    assert_eq!(commit.title, "Add auth middleware");

    // The single call both mutates and returns the refreshed workspace.
    let after = Board::from_status(&but.rub(&commit.cli_id, &target).expect("rub"));

    let auth = after
        .columns
        .iter()
        .find(|c| c.title == "feat-auth")
        .unwrap();
    let ui = after.columns.iter().find(|c| c.title == "feat-ui").unwrap();
    assert!(
        auth.cards.is_empty(),
        "commit left its original lane, found {:?}",
        auth.cards.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    assert!(
        ui.cards.iter().any(|c| c.title == "Add auth middleware"),
        "commit landed in the target lane, found {:?}",
        ui.cards.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn staging_a_backlog_file_moves_it_into_a_lane() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("stage");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");
    sb.write("wip.txt", "scratch\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let file = before.columns[0].cards[0].clone();
    let target = before.columns[1].drop_target.clone();
    assert_eq!(file.kind, CardKind::Change);

    let after = Board::from_status(&but.rub(&file.cli_id, &target).unwrap());
    assert!(
        after.columns[0].cards.is_empty(),
        "file left the backlog once staged"
    );
    assert!(
        after.columns[1].cards.iter().any(|c| c.title == "wip.txt"),
        "staged file appears in the lane it was dropped on"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn reports_a_useful_error_outside_a_gitbutler_project() {
    if skip_if_no_but() {
        return;
    }
    // A plain git repo that was never `but setup`.
    let root = std::env::temp_dir().join(format!("kanstack-live-plain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        vec!["init", "-q", "."],
        vec!["config", "user.email", "t@e.com"],
        vec!["config", "user.name", "T"],
    ] {
        Command::new("git")
            .args(&args)
            .current_dir(&root)
            .output()
            .unwrap();
    }

    let but = But::discover(&root).unwrap();
    let err = but
        .status()
        .expect_err("status must fail outside a project");
    let msg = format!("{err}");
    assert!(
        msg.contains("setup_required") || msg.to_lowercase().contains("no gitbutler project"),
        "error should explain the real problem, got: {msg}"
    );
    assert!(
        msg.contains("but setup"),
        "error should carry the CLI's own hint, got: {msg}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Guards the assumption the whole design rests on: `but status` cost is dominated by
/// fixed overhead, not repository size, so navigation can stay in-memory and refreshes
/// can be synchronous.
#[test]
#[ignore = "requires the GitButler CLI; timing-sensitive"]
fn status_stays_fast_enough_for_a_synchronous_refresh() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("perf");
    for i in 0..6 {
        sb.branch_with_commit(&format!("stack-{i}"), &format!("f{i}.txt"), "work");
    }
    let but = But::discover(&sb.repo()).unwrap();

    let start = std::time::Instant::now();
    let runs = 5;
    for _ in 0..runs {
        but.status().unwrap();
    }
    let per = start.elapsed() / runs;
    assert!(
        per < std::time::Duration::from_millis(600),
        "status took {per:?} per call; a synchronous refresh would feel sluggish"
    );
    eprintln!("status: {per:?} per call");
}
