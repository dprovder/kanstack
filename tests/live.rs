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
        sb.but(&["setup", "--init", "--format", "json"]);
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
        sb.but(&["setup", "--init", "--format", "json"]);
        sb
    }

    /// Lands a commit on the remote's `main` from a separate clone, so the workspace
    /// falls behind its target the way it would when a colleague pushes.
    fn push_upstream_commit(&self, file: &str, message: &str) {
        let clone = self.root.join("upstream-clone");
        let remote = self.root.join("remote.git");
        let git = |args: &[&str], dir: &std::path::Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("HOME", self.home())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        Command::new("git")
            .args(["clone", "-q"])
            .arg(&remote)
            .arg(&clone)
            .output()
            .unwrap();
        git(&["config", "user.email", "other@example.com"], &clone);
        git(&["config", "user.name", "Other"], &clone);
        std::fs::write(clone.join(file), "upstream\n").unwrap();
        git(&["add", "."], &clone);
        git(&["commit", "-qm", message], &clone);
        git(&["push", "-q", "origin", "main"], &clone);
        let _ = std::fs::remove_dir_all(&clone);
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

/// The merge check is what makes `M` safe to bind, the same way the push and rebase
/// previews are: it must report what would land and whether it lands cleanly, without
/// touching anything.
#[test]
#[ignore = "requires the GitButler CLI"]
fn land_check_reports_a_clean_land_without_landing() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("landcheck");
    sb.branch_with_commit("feat", "a.txt", "Landable");

    let but = But::discover(&sb.repo()).unwrap();
    let check = but.merge_check("feat").expect("merge check");
    assert_eq!(check.commits_ahead, 1);
    assert_eq!(check.commits[0].subject(), "Landable");
    assert!(check.merge_check.merges_cleanly);
    assert!(check.merge_check.conflicting_files.is_empty());

    // A check must not have landed anything.
    let board = Board::from_status(&but.status().unwrap());
    let lane = board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some("feat"))
        .expect("the lane is still there, unlanded");
    assert_eq!(lane.cards.len(), 1, "the commit is still only on the lane");
}

/// `but land` prints nothing on success, so the caller must refresh separately — pinned
/// here so a regression shows up as a stale board rather than a parse error users only see
/// at the keyboard.
#[test]
#[ignore = "requires the GitButler CLI"]
fn landing_a_lane_lands_it_on_the_target() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("land");
    sb.branch_with_commit("feat", "a.txt", "Landable");

    let but = But::discover(&sb.repo()).unwrap();
    let status = but.land("feat").expect("land");
    let board = Board::from_status(&status);

    // The lane's commit is no longer ahead of the target — it landed, it did not vanish.
    let lane = board.columns.iter().find(|c| c.branch_name.as_deref() == Some("feat"));
    if let Some(lane) = lane {
        assert!(
            lane.cards.iter().all(|c| c.kind != CardKind::Commit),
            "the landed commit should no longer be ahead of the target"
        );
    }
    let log = sb.git(&["log", "--oneline", "--all"]);
    assert!(log.contains("Landable"), "landing must land the content: {log}");
}

/// The whole reason `M` is built on `but land` and not the older `but merge`: `merge`
/// flatly refuses when the workspace target tracks a real remote ("Target remote is
/// origin, not gb-local. This command only works with gb-local targets" — verified against
/// 0.21.2 against this project's own repository). `land` is designed for exactly this case
/// and pushes the result directly, bypassing any pull request.
#[test]
#[ignore = "requires the GitButler CLI"]
fn landing_a_lane_pushes_directly_to_a_real_remote() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("landremote");
    sb.branch_with_commit("feat", "a.txt", "Landable");

    let but = But::discover(&sb.repo()).unwrap();
    but.land("feat").expect("land onto a real remote target");

    let refs = sb.remote_refs();
    assert!(refs.contains("refs/heads/main"), "the bare remote should still have main");
    let remote_log = Command::new("git")
        .args(["--git-dir"])
        .arg(sb.root.join("remote.git"))
        .args(["log", "--oneline", "main"])
        .output()
        .unwrap();
    let remote_log = String::from_utf8_lossy(&remote_log.stdout);
    assert!(
        remote_log.contains("Landable"),
        "the commit must be pushed straight to the remote's main, got {remote_log:?}"
    );
}

/// `z` (`but undo`) fully restores the local board after a land — the branch and its
/// commit reappear exactly as before. What it does *not* do is un-push: the remote a real
/// land already reached keeps the landed commit. Nothing is lost (the content sits safely
/// on the remote throughout) but the local board looks more reverted than it is, which is
/// exactly the gap the land confirmation dialog now warns about before you commit to it.
#[test]
#[ignore = "requires the GitButler CLI"]
fn undoing_a_land_reverts_locally_but_not_the_pushed_remote() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("landundo");
    sb.branch_with_commit("feat", "a.txt", "Important real work");

    let but = But::discover(&sb.repo()).unwrap();
    but.land("feat").expect("land onto a real remote target");

    let remote_log = |sb: &Sandbox| {
        let out = Command::new("git")
            .args(["--git-dir"])
            .arg(sb.root.join("remote.git"))
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(
        remote_log(&sb).contains("Important real work"),
        "sanity check: the land must have really pushed"
    );

    let status = but.undo().expect("undo the land");
    let board = Board::from_status(&status);
    let lane = board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some("feat"))
        .expect("undo restores the branch locally");
    assert_eq!(lane.cards.len(), 1, "and its commit, exactly as before");

    // The whole point: the remote is untouched by a purely local undo.
    assert!(
        remote_log(&sb).contains("Important real work"),
        "undo must not silently revert what was already pushed to a real remote"
    );
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

/// `z`/`Z` are the safety net every other destructive action here now leans on (`d` in
/// particular, since 0.21 stopped refusing an orphaning delete). Both must actually round
/// trip: undo an operation, then redo it back, and land on the exact state each time.
#[test]
#[ignore = "requires the GitButler CLI"]
fn undo_and_redo_round_trip_a_branch_creation() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("undoredo");

    let but = But::discover(&sb.repo()).unwrap();
    let after_create = Board::from_status(&but.branch_new("feat", None).unwrap());
    assert!(after_create.columns.iter().any(|c| c.branch_name.as_deref() == Some("feat")));

    let after_undo = Board::from_status(&but.undo().unwrap());
    assert!(
        after_undo.columns.iter().all(|c| c.branch_name.as_deref() != Some("feat")),
        "undo removes the branch it created"
    );

    let after_redo = Board::from_status(&but.redo().unwrap());
    assert!(
        after_redo.columns.iter().any(|c| c.branch_name.as_deref() == Some("feat")),
        "redo brings it back"
    );
}

/// `d` used to be safe to bind only because `but` refused to delete a branch whose commits
/// would be left orphaned. That guarantee is upstream's, not ours — and 0.21 changed it:
/// deleting a lone branch with unpushed commits now succeeds non-interactively and
/// discards them outright, rather than refusing. `but undo` still recovers them, which is
/// what `pending_delete`'s confirmation text promises instead of a refusal now.
#[test]
#[ignore = "requires the GitButler CLI"]
fn deleting_a_lone_branch_with_commits_is_undoable_not_refused() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("delundo");
    sb.branch_with_commit("feat", "a.txt", "Important work");

    let but = But::discover(&sb.repo()).unwrap();
    but.branch_delete("feat").expect("0.21 no longer refuses this non-interactively");

    let after = Board::from_status(&but.status().unwrap());
    assert!(
        after.columns.iter().all(|c| c.branch_name.as_deref() != Some("feat")),
        "the branch and its commit are gone, not refused"
    );

    // `but undo` is the safety net now, so it had better work.
    sb.but(&["undo", "--format", "json"]);
    let restored = Board::from_status(&but.status().unwrap());
    let lane = restored.columns.iter().find(|c| c.title == "feat").expect("undo restores the lane");
    assert_eq!(lane.cards.len(), 1);
}

/// Inside a stack it succeeds, and the commits fold into the branch above rather than
/// disappearing on their own — recoverable with `but undo`, which is what the confirmation
/// dialog now promises instead of a fold (see `App::pending_delete`).
///
/// `d` in the actual UI always targets the lane's *tip* branch (`col.branch_name`), so this
/// deletes "upper", not the base — matching what a keypress on this lane would really do.
#[test]
#[ignore = "requires the GitButler CLI"]
fn deleting_the_tip_of_a_stack_discards_only_its_own_commit() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("delstack");
    sb.branch_with_commit("lower", "a.txt", "Lower work");
    sb.write("b.txt", "upper\n");
    sb.but(&["branch", "new", "upper", "--anchor", "lower"]);
    sb.but(&["commit", "upper", "-m", "Upper work"]);

    let but = But::discover(&sb.repo()).unwrap();
    but.branch_delete("upper").expect("delete the tip of a stack");

    let after = Board::from_status(&but.status().unwrap());
    assert!(
        after.columns.iter().all(|c| c.branch_name.as_deref() != Some("upper")),
        "the tip branch is gone"
    );
    let lane = after.columns.iter().find(|c| c.title == "lower").unwrap();
    let titles: Vec<&str> = lane.cards.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Lower work"],
        "the branch below is untouched, but the tip's own commit is gone, got {titles:?}"
    );

    sb.but(&["undo", "--format", "json"]);
    let restored = Board::from_status(&but.status().unwrap());
    let titles: Vec<&str> = restored
        .columns
        .iter()
        .find(|c| c.title == "upper +1")
        .expect("undo restores the deleted branch")
        .cards
        .iter()
        .map(|c| c.title.as_str())
        .collect();
    assert_eq!(titles, ["Upper work", "Lower work"], "undo brings the discarded commit back");
}

/// The rebase preview is what makes `r` safe to bind: it must report the incoming commits
/// and each lane's outcome without touching anything.
#[test]
#[ignore = "requires the GitButler CLI"]
fn rebase_preview_reports_upstream_work_without_applying_it() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("rebase");
    sb.branch_with_commit("feat", "a.txt", "My work");
    sb.push_upstream_commit("upstream.txt", "Upstream change");

    let but = But::discover(&sb.repo()).unwrap();
    let preview = but.pull_check().expect("pull --check");

    assert!(!preview.up_to_date, "the target has moved");
    assert_eq!(preview.upstream_commits.count, 1);
    assert_eq!(
        preview.upstream_commits.commits[0].description.trim(),
        "Upstream change"
    );
    let feat = preview
        .branch_statuses
        .iter()
        .find(|b| b.name == "feat")
        .expect("our lane is assessed");
    assert_eq!(feat.status, kanstack::model::PullStatus::Updatable);

    // A check must not have rebased anything.
    let board = Board::from_status(&but.status().unwrap());
    let lane = board.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 1);
    assert!(!sb.repo().join("upstream.txt").exists(), "check applied nothing");
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn rebasing_brings_upstream_work_in() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("rebasedo");
    sb.branch_with_commit("feat", "a.txt", "My work");
    sb.push_upstream_commit("upstream.txt", "Upstream change");

    let but = But::discover(&sb.repo()).unwrap();
    but.pull().expect("pull");

    assert!(
        sb.repo().join("upstream.txt").exists(),
        "upstream work landed in the worktree"
    );
    let board = Board::from_status(&but.status().unwrap());
    let lane = board.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 1, "our commit survived the rebase");
    assert!(but.pull_check().unwrap().up_to_date, "and we are current now");
}

/// The point of the diff pane: `but diff` emits one entry per hunk with its own id, so a
/// single file's hunks can be sent to different lanes. Whole-file staging could not do
/// this, and it is what makes a mixed file reviewable in pieces.
#[test]
#[ignore = "requires the GitButler CLI"]
fn hunks_of_one_file_can_be_staged_to_different_lanes() {
    if skip_if_no_but() {
        return;
    }
    // The base file has to exist before `but setup`: once GitButler owns the workspace it
    // refuses direct `git commit`s to its branch.
    let sb = Sandbox::bare("hunks");
    sb.write(
        "a.txt",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
    );
    sb.git(&["add", "."]);
    sb.git(&["commit", "-qm", "seed"]);
    sb.but(&["setup", "--init", "--format", "json"]);
    sb.but(&["branch", "new", "top"]);
    sb.but(&["branch", "new", "bottom"]);
    // Two edits far enough apart to be separate hunks.
    sb.write(
        "a.txt",
        "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n",
    );

    let but = But::discover(&sb.repo()).unwrap();
    let out = but.diff_uncommitted().expect("diff");
    let view = kanstack::diff::DiffView::from_output("a.txt", &out);
    assert_eq!(view.entries.len(), 2, "two hunks in one file");
    assert!(
        view.entries.iter().all(|e| e.path == "a.txt"),
        "both hunks belong to the same file"
    );

    // Ids describe the current state, so the second hunk must be resolved *after* the
    // first move — staging one renumbers whatever is left.
    let first = view.entries[0].rub_id.clone().expect("stageable");
    but.rub(&first, "top").expect("stage first hunk");

    let out = but.diff_uncommitted().expect("diff again");
    let view = kanstack::diff::DiffView::from_output("a.txt", &out);
    let remaining = view
        .entries
        .iter()
        .find_map(|e| e.rub_id.clone())
        .expect("the other hunk is still unassigned");
    let board = Board::from_status(&but.rub(&remaining, "bottom").expect("stage second hunk"));

    let staged_in = |name: &str| -> bool {
        board
            .columns
            .iter()
            .find(|c| c.branch_name.as_deref() == Some(name))
            .is_some_and(|c| c.cards.iter().any(|k| k.title == "a.txt"))
    };
    assert!(staged_in("top"), "one hunk landed in top");
    assert!(staged_in("bottom"), "the other landed in bottom");
    assert!(
        board.columns[0].cards.is_empty(),
        "and nothing is left unassigned"
    );
}

/// A commit's diff is readable but not stageable — `but` gives committed changes no ids.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_commits_diff_is_readable_but_not_stageable() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commitdiff");
    sb.branch_with_commit("feat", "a.txt", "Work");

    let but = But::discover(&sb.repo()).unwrap();
    let board = Board::from_status(&but.status().unwrap());
    let commit = board.columns.iter().find(|c| c.title == "feat").unwrap().cards[0].clone();

    let out = but.diff_target(&commit.rub_id).expect("diff of a commit");
    let view = kanstack::diff::DiffView::from_output(commit.title.clone(), &out);
    assert!(!view.entries.is_empty(), "there is something to read");
    assert!(
        view.entries.iter().all(|e| e.rub_id.is_none()),
        "history offers no hunk to stage"
    );
}

/// Line counts on cards come from a real `but diff`, so check them against a known edit
/// rather than only against a hand-written fixture.
#[test]
#[ignore = "requires the GitButler CLI"]
fn cards_carry_real_line_counts() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::bare("stats");
    sb.write("a.txt", "one\ntwo\nthree\n");
    sb.git(&["add", "."]);
    sb.git(&["commit", "-qm", "seed"]);
    sb.but(&["setup", "--init", "--format", "json"]);
    // One line changed, two added: +3 -1.
    sb.write("a.txt", "ONE\ntwo\nthree\nfour\nfive\n");

    let but = But::discover(&sb.repo()).unwrap();
    let status = but.status().unwrap();
    let diff = but.diff_uncommitted().unwrap();
    let board = Board::from_status_and_diff(&status, &diff);

    let card = board.columns[0]
        .cards
        .iter()
        .find(|c| c.title == "a.txt")
        .expect("the edited file is on the board");
    assert_eq!(card.stats, Some((3, 1)), "3 added, 1 removed");
    assert_eq!(board.columns[0].stats, Some((3, 1)), "and the lane totals it");
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

/// Reproduction attempt for GitHub issue #2, "crash on adding a branch on the unassigned
/// stack": a brand-new workspace with zero applied branches (so the board is just the
/// backlog column), pressing `b` from there, typing a name, and confirming.
#[test]
#[ignore = "requires the GitButler CLI"]
fn creating_a_branch_from_the_empty_unassigned_column_does_not_panic() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use kanstack::cmux::Cmux;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;

    let sb = Sandbox::new("newbranchcrash");
    let but = But::discover(&sb.repo()).unwrap();
    // Real detection, not `None`: if `cmux` happens to be on PATH (as it is on the machine
    // this was first reproduced on), the spawn-a-harness path actually runs.
    let mut app = App::new(but, Cmux::discover()).expect("build the app against a fresh workspace");
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();

    assert_eq!(app.column_count(), 1, "only the backlog column exists yet");
    assert_eq!(app.col, 0);

    let mut press = |app: &mut App, code: KeyCode| {
        app.on_key(KeyEvent::from(code));
        term.draw(|f| kanstack::ui::draw(f, app)).unwrap();
    };
    press(&mut app, KeyCode::Char('b'));
    for c in "first-branch".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(app.column_count(), 2, "the backlog plus the new lane");
}

/// Same issue, a second guess at repro: unassigned holds real file cards, the card cursor
/// sits on one that isn't index 0, and `b` is pressed with Tab toggled (stack-onto) before
/// typing a name — probing whether the crash needs cards/cursor state present, not just an
/// empty backlog. Renders through the real `ui::draw` after every keystroke too: `on_key`
/// alone can't catch a panic that only happens while drawing (an unsaturated subtraction in
/// a width calculation, an out-of-bounds slice), which is exactly the kind of bug a "crash"
/// report with no repro steps could be.
#[test]
#[ignore = "requires the GitButler CLI"]
fn creating_a_branch_from_unassigned_with_cards_and_a_moved_cursor_does_not_panic() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use kanstack::cmux::Cmux;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;

    let sb = Sandbox::new("newbranchcrash2");
    sb.write("a.txt", "a\n");
    sb.write("b.txt", "b\n");
    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, Cmux::discover()).expect("build the app against the workspace");
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    term.draw(|f| kanstack::ui::draw(f, &app)).unwrap();

    assert_eq!(app.column_count(), 1);
    let mut press = |app: &mut App, code: KeyCode| {
        app.on_key(KeyEvent::from(code));
        term.draw(|f| kanstack::ui::draw(f, app)).unwrap();
    };

    // Move the card cursor onto the second file before creating a branch.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('b'));
    press(&mut app, KeyCode::Tab); // toggle stack-onto with nothing to stack onto
    press(&mut app, KeyCode::Tab); // toggle back
    for c in "second-branch".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(app.column_count(), 2);

    // Pressed again immediately, from the lane `begin_branch` just landed the cursor on:
    // that lane now has a `branch_name`, so this stacks rather than opening a new column
    // (documented, intentional — `b`'s default is to stack when a lane is selected).
    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.pending_branch_action(), "stack on second-branch");
    for c in "third-branch".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(app.column_count(), 2, "stacked onto the existing lane, not a new column");
}
