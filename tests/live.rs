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
    /// A git repo with one commit, before `but setup` has run.
    fn bare(name: &str) -> Sandbox {
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
        sb
    }

    fn new(name: &str) -> Sandbox {
        let sb = Sandbox::bare(name);
        sb.but(&["setup", "--init", "-j"]);
        sb
    }

    /// A sandbox whose target is a real remote.
    ///
    /// The remote has to exist *before* `but setup`, because setup picks the push target
    /// then: with no remote it invents a local `gb-local` and pushes go there instead of
    /// anywhere useful.
    fn with_remote(name: &str) -> Sandbox {
        let sb = Sandbox::bare(name);
        let remote = sb.root.join("remote.git");
        Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .output()
            .unwrap();
        sb.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        sb.git(&["push", "-q", "origin", "HEAD:main"]);
        sb.but(&["setup", "--init", "-j"]);
        sb
    }

    /// Ref names present on the bare remote, for asserting what a push actually did.
    fn remote_refs(&self) -> String {
        let out = Command::new("git")
            .args(["--git-dir"])
            .arg(self.root.join("remote.git"))
            .args(["for-each-ref", "--format=%(refname)"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
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

/// The claim that makes squash free: dropping a commit card onto another commit card is
/// the same `but rub` call as any other move, just with a commit as the target.
#[test]
#[ignore = "requires the GitButler CLI"]
fn dropping_a_commit_on_a_commit_squashes_them() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("squash");
    sb.branch_with_commit("feat", "a.txt", "First commit");
    sb.write("b.txt", "more\n");
    sb.but(&["commit", "feat", "-m", "Second commit"]);

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let lane = before.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 2, "two commits before squashing");

    // Newest is on top; rub it onto the one below.
    let source = lane.cards[0].clone();
    let target = lane.cards[1].clone();
    let after = Board::from_status(&but.rub(&source.cli_id, &target.cli_id).expect("rub"));

    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(
        lane.cards.len(),
        1,
        "the two commits collapsed into one, got {:?}",
        lane.cards.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
}

/// The other half of the same mechanism: a working-tree file dropped on a commit amends.
#[test]
#[ignore = "requires the GitButler CLI"]
fn dropping_a_file_on_a_commit_amends_it() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("amend");
    sb.branch_with_commit("feat", "a.txt", "Only commit");
    sb.write("b.txt", "extra\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let file = before.columns[0].cards[0].clone();
    let commit = before
        .columns
        .iter()
        .find(|c| c.title == "feat")
        .unwrap()
        .cards[0]
        .clone();
    assert_eq!(file.kind, CardKind::Change);
    assert_eq!(commit.kind, CardKind::Commit);

    let after = Board::from_status(&but.rub(&file.cli_id, &commit.cli_id).expect("rub"));
    assert!(
        after.columns[0].cards.is_empty(),
        "the file left the backlog"
    );
    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 1, "still one commit, now amended");
    let files = lane.cards[0].subtitle.clone().unwrap_or_default();
    assert!(
        files.contains("b.txt"),
        "the amended file is in the commit, got {files:?}"
    );
}

/// Regression: `but commit` includes *all* unassigned changes unless `--only` is passed,
/// so committing a lane used to empty the whole backlog into it.
///
/// The original test could not have caught this: it staged its only file, leaving nothing
/// unassigned, so "commits the staged file" and "commits everything" looked identical.
/// The leftover unassigned file below is the entire point of this test.
#[test]
#[ignore = "requires the GitButler CLI"]
fn committing_a_lane_leaves_unassigned_changes_alone() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commitonly");
    sb.branch_with_commit("feat", "a.txt", "First");
    sb.write("staged.txt", "for the lane\n");
    sb.write("untouched.txt", "must stay in the backlog\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let staged = before.columns[0]
        .cards
        .iter()
        .find(|c| c.title == "staged.txt")
        .expect("staged.txt in backlog")
        .clone();
    let lane = before.columns.iter().find(|c| c.title == "feat").unwrap();
    but.rub(&staged.rub_id, &lane.drop_target).unwrap();

    let after = Board::from_status(&but.commit("feat", "Only the staged one").unwrap());

    let backlog: Vec<&str> = after.columns[0]
        .cards
        .iter()
        .map(|c| c.title.as_str())
        .collect();
    assert_eq!(
        backlog, ["untouched.txt"],
        "the unstaged file must survive the commit, got {backlog:?}"
    );

    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    let tip = &lane.cards[0];
    assert_eq!(tip.title, "Only the staged one");
    assert_eq!(
        tip.subtitle.as_deref(),
        Some("staged.txt"),
        "the commit contains only what was staged to the lane"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn committing_a_lane_turns_staged_files_into_a_commit() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commit");
    sb.branch_with_commit("feat", "a.txt", "First");
    sb.write("b.txt", "staged\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let file = before.columns[0].cards[0].clone();
    let lane = before.columns.iter().find(|c| c.title == "feat").unwrap();
    but.rub(&file.cli_id, &lane.drop_target).unwrap();

    let after = Board::from_status(&but.commit("feat", "Second commit").expect("commit"));
    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    let titles: Vec<&str> = lane.cards.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Second commit", "First"]);
    assert!(
        lane.cards.iter().all(|c| c.kind == CardKind::Commit),
        "the staged file became part of a commit"
    );
    // The detailed follow-up query is what keeps file context on the new card.
    assert_eq!(lane.cards[0].subtitle.as_deref(), Some("b.txt"));
}

/// Push is the sharpest edge in the CLI, so the preview it is gated behind gets tested
/// directly: it must name the destination and report whether a force is involved.
#[test]
#[ignore = "requires the GitButler CLI"]
fn push_preview_reports_destination_without_pushing() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("pushprev");
    sb.branch_with_commit("feat", "a.txt", "Pushable");

    let but = But::discover(&sb.repo()).unwrap();
    let preview = but.push_preview("feat").expect("dry run");
    let b = preview
        .branches
        .iter()
        .find(|b| b.branch_name == "feat")
        .expect("branch in preview");
    assert_eq!(b.unpushed_commits, 1);
    assert_eq!(b.remote, "origin");
    assert!(!b.requires_force, "a first push is not a rewrite");
    assert_eq!(b.commits.len(), 1);
    assert_eq!(b.commits[0].message.trim(), "Pushable");

    // A dry run must not have moved anything on the remote.
    let refs = sb.remote_refs();
    assert!(
        !refs.contains("feat"),
        "dry run must not push; remote had {refs:?}"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn pushing_a_lane_updates_the_remote() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("push");
    sb.branch_with_commit("feat", "a.txt", "Pushable");

    let but = But::discover(&sb.repo()).unwrap();
    but.push("feat").expect("push");

    let refs = sb.remote_refs();
    assert!(refs.contains("feat"), "remote should have the branch, got {refs:?}");
}

/// The two branch gestures are genuinely different operations, so both are pinned:
/// an anchor stacks into the existing lane, no anchor opens a new one.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_branch_with_an_anchor_stacks_into_the_same_lane() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("stackbranch");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    assert_eq!(before.columns.len(), 2, "backlog + one lane");

    let after = Board::from_status(&but.branch_new("feat-ui", Some("feat-auth")).expect("branch"));
    assert_eq!(
        after.columns.len(),
        2,
        "stacking must not open a new lane, got {:?}",
        after.columns.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    let lane = &after.columns[1];
    assert_eq!(
        lane.title, "feat-ui +1",
        "the new branch becomes the tip and the lane says how deep it is"
    );
    assert_eq!(lane.branch_name.as_deref(), Some("feat-ui"));
    assert!(
        lane.cards.iter().any(|c| c.group.as_deref() == Some("feat-auth")),
        "the anchored branch's commits are grouped under its own name"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn a_branch_without_an_anchor_opens_a_parallel_lane() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("parallelbranch");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth");

    let but = But::discover(&sb.repo()).unwrap();
    let after = Board::from_status(&but.branch_new("feat-solo", None).expect("branch"));
    let titles: Vec<&str> = after.columns.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(after.columns.len(), 3, "a lane of its own, got {titles:?}");
    assert!(titles.contains(&"feat-solo"));
    assert!(titles.contains(&"feat-auth"));
}

/// Restacking is composed from four separate `but` calls because no single command does
/// it, so the end state is worth asserting in detail: one lane, both branches intact,
/// commits in their original order.
#[test]
#[ignore = "requires the GitButler CLI"]
fn restacking_moves_a_whole_lane_onto_another() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("restack");
    sb.branch_with_commit("feat-auth", "a.txt", "Auth work");
    sb.write("b.txt", "ui\n");
    sb.but(&["branch", "new", "feat-ui"]);
    sb.but(&["commit", "feat-ui", "-m", "UI first"]);
    sb.write("c.txt", "ui2\n");
    sb.but(&["commit", "feat-ui", "-m", "UI second"]);

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    assert_eq!(before.columns.len(), 3, "backlog + two parallel lanes");

    let after = Board::from_status(&but.restack_branch("feat-ui", "feat-auth").expect("restack"));

    assert_eq!(
        after.columns.len(),
        2,
        "the two lanes became one, got {:?}",
        after.columns.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    let lane = &after.columns[1];
    assert_eq!(
        lane.branch_name.as_deref(),
        Some("feat-ui"),
        "the original branch name survives the temp-branch dance"
    );
    assert_eq!(lane.title, "feat-ui +1");

    // Order must be preserved: commits are listed newest-first.
    let titles: Vec<&str> = lane.cards.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["UI second", "UI first", "Auth work"], "got {titles:?}");

    // And the anchored branch keeps its own identity within the lane.
    assert!(
        lane.cards
            .iter()
            .any(|c| c.group.as_deref() == Some("feat-auth")),
        "feat-auth is still a distinct branch in the stack"
    );
    assert!(lane.cards.iter().any(|c| c.group.as_deref() == Some("feat-ui")));
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn restacking_a_branch_onto_itself_is_refused() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("restackself");
    sb.branch_with_commit("feat", "a.txt", "Work");
    let but = But::discover(&sb.repo()).unwrap();
    let err = but.restack_branch("feat", "feat").expect_err("must refuse");
    assert!(format!("{err}").contains("cannot be stacked on itself"));
}

/// `u` is `rub <card> zz`. Confirms it is a true uncommit — the content comes back to the
/// worktree rather than being discarded — since the rub matrix only calls it "Undo".
#[test]
#[ignore = "requires the GitButler CLI"]
fn sending_a_commit_to_the_backlog_uncommits_without_losing_content() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("uncommit");
    sb.write("a.txt", "hello\nworld\n");
    sb.but(&["branch", "new", "feat"]);
    sb.but(&["commit", "feat", "-m", "Work"]);

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let commit = before
        .columns
        .iter()
        .find(|c| c.title == "feat")
        .unwrap()
        .cards[0]
        .clone();

    let after =
        Board::from_status(&but.rub(&commit.rub_id, kanstack::board::UNASSIGNED_TARGET).unwrap());

    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    assert!(lane.cards.is_empty(), "the commit is gone from the lane");
    assert!(
        after.columns[0].cards.iter().any(|c| c.title == "a.txt"),
        "and its file is back in the backlog"
    );
    assert_eq!(
        std::fs::read_to_string(sb.repo().join("a.txt")).unwrap(),
        "hello\nworld\n",
        "uncommitting must not touch the worktree content"
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
