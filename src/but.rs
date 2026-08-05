//! Subprocess client for the `but` CLI.
//!
//! This is deliberately the *only* coupling to GitButler: we spawn a `but` binary the
//! user installed themselves and speak its documented JSON format. We link none of its
//! code, so this project stays independently licensed (see README).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::model::{CliError, DiffOutput, MergeCheck, PullPreview, PushPreview, WorkspaceStatus};

/// Oldest `but` whose JSON shape this was verified against.
///
/// 0.22 overhauled `commit`, `amend`, `squash`, `discard`, and `move`, and removed `rub`,
/// `stage`, and `unstage` outright, replacing `--format json` with a plain `--json` flag.
/// None of that has a spelling that also works on 0.21 — there is no flag or subcommand
/// shape shared by both sides of that release, so this client picks a side rather than
/// branching on version at every call site.
pub const MIN_VERSION: Version = Version {
    major: 0,
    minor: 22,
    patch: 0,
};
/// Newest `but` actually exercised while building this. Newer is allowed but noted.
pub const VERIFIED_THROUGH: Version = Version {
    major: 0,
    minor: 22,
    patch: 0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Parses the `but --version` line, e.g. `but 0.19.3`.
    pub fn parse(s: &str) -> Result<Version> {
        let token = s
            .split_whitespace()
            .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .ok_or_else(|| anyhow!("no version number in {s:?}"))?;
        // Tolerate pre-release/build suffixes like `0.21.2-nightly.3`.
        let core = token.split(['-', '+']).next().unwrap_or(token);
        let mut parts = core.split('.');
        let mut next = |what: &str| -> Result<u32> {
            parts
                .next()
                .ok_or_else(|| anyhow!("missing {what} in version {token:?}"))?
                .parse()
                .with_context(|| format!("bad {what} in version {token:?}"))
        };
        Ok(Version {
            major: next("major")?,
            minor: next("minor")?,
            patch: next("patch").unwrap_or(0),
        })
    }
}

/// Author GitButler stamps on the workspace commit it manages.
const GITBUTLER_AUTHOR: &str = "gitbutler@gitbutler.com";

/// Subject line GitButler gives that commit. Stable across the variants seen, and paired
/// with [`GITBUTLER_AUTHOR`] rather than trusted on its own — see
/// [`But::diagnose_workspace_block`].
const WORKSPACE_SUBJECT: &str = "GitButler Workspace Commit";

/// Recognises the one `but` failure that locks the entire workspace: something committed
/// onto `gitbutler/workspace` with plain Git, so the workspace commit is no longer HEAD.
///
/// This is worth singling out because it is unlike every other error the board can hit.
/// `but` refuses *every* subcommand in this state — including `undo` and `oplog restore`,
/// so the usual escape hatches are gone — which means a board that merely notes the failure
/// and carries on is showing a snapshot of a repository that has since moved, with no way
/// to ever catch up. Stale and confident is worse than stopped and honest.
///
/// Matched on text because `but` reports this as prose on stderr rather than as the
/// structured `CliError` JSON that [`But::run`] prefers. Two spellings are accepted: 0.21
/// emits the `teardown` wording, while `but-workspace`'s `ref_info` carries an older one
/// that suggests `git reset --soft` directly. Both describe the same broken shape.
pub fn is_workspace_block(msg: &str) -> bool {
    msg.contains("GitButler mode exit required")
        || msg.contains("commit(s) on top of the workspace commit")
}

/// A commit sitting on top of the workspace commit that has no business being there.
#[derive(Debug, Clone)]
pub struct StrayCommit {
    /// Abbreviated, since this is only ever shown to a human.
    pub sha: String,
    pub subject: String,
}

/// Everything needed to explain a blocked workspace and offer a way out of it.
#[derive(Debug, Clone)]
pub struct WorkspaceBlock {
    /// What `but` said, kept verbatim — it names the recovery GitButler itself recommends.
    pub message: String,
    /// The workspace commit to reset back onto. `None` when it could not be identified, in
    /// which case the reset route is withheld rather than guessed at.
    pub workspace_sha: Option<String>,
    /// The commits above it, newest first.
    pub stray: Vec<StrayCommit>,
}

pub struct But {
    bin: PathBuf,
    cwd: PathBuf,
    version: Version,
}

impl But {
    /// Locates `but`, checks its version, and pins the working directory.
    ///
    /// The version gate exists because the JSON contract is stable *by intent* but is not
    /// versioned in the payload and its types are `pub(crate)` upstream — so there is no
    /// semver promise to lean on. Better to refuse than to mis-render.
    pub fn discover(cwd: &Path) -> Result<Self> {
        let bin = std::env::var_os("KANSTACK_BUT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("but"));

        let out = Command::new(&bin)
            .arg("--version")
            .output()
            .with_context(|| {
                format!(
                    "could not run `{}`. Install the GitButler CLI, or set KANSTACK_BUT_BIN \
                     to its path.",
                    bin.display()
                )
            })?;
        if !out.status.success() {
            bail!("`{} --version` failed", bin.display());
        }
        let version = Version::parse(&String::from_utf8_lossy(&out.stdout))?;
        if version < MIN_VERSION {
            bail!(
                "`but` {version} is too old; this needs at least {MIN_VERSION}. \
                 Run `but update install`."
            );
        }

        Ok(But {
            bin,
            cwd: cwd.to_path_buf(),
            version,
        })
    }

    pub fn version(&self) -> Version {
        self.version
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// True when running against a `but` newer than anything this was tested with.
    /// Worth surfacing in the UI, but not worth refusing to start over.
    pub fn is_untested_version(&self) -> bool {
        self.version > VERIFIED_THROUGH
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .with_context(|| format!("failed to spawn `but {}`", args.join(" ")))?;

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

        // In JSON format `but` sometimes reports failures as a structured object on
        // stdout; prefer that over the exit code when present, since it carries a usable
        // message and hint. Other failures (verified against 0.21.2, e.g. `land` onto a
        // branch that doesn't exist) arrive as plain text on stderr instead, which the
        // exit-code branch below handles.
        if let Ok(err) = serde_json::from_str::<CliError>(stdout.trim()) {
            let mut msg = format!("{}: {}", err.error, err.message);
            if let Some(hint) = err.hint {
                msg.push_str(&format!("\nhint: {hint}"));
            }
            bail!(msg);
        }

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim();
            bail!(
                "`but {}` failed{}",
                args.join(" "),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        Ok(stdout)
    }

    /// Reads the whole workspace. `-f` includes per-commit file lists and costs nothing
    /// measurable, so it is always on.
    pub fn status(&self) -> Result<WorkspaceStatus> {
        let raw = self.run(&["status", "-f", "--json"])?;
        parse_status(&raw)
    }

    /// Read-only `git`, for the one situation `but` cannot answer questions about: a
    /// workspace so broken that every `but` subcommand refuses (see [`is_workspace_block`]).
    /// Identifying the commit to reset back onto has to come from somewhere, and with `but`
    /// refusing, plain Git is the only thing left that can still read the repository.
    fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .with_context(|| format!("failed to spawn `git {}`", args.join(" ")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("`git {}` failed: {}", args.join(" "), stderr.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Works out what is sitting on top of the workspace commit, so the block can be
    /// explained in terms of the actual commits rather than just repeating `but`'s prose.
    ///
    /// Walks first-parent from HEAD looking for GitButler's own workspace commit.
    ///
    /// Identifying it needs two agreeing signals, because neither is sound alone. The
    /// author is necessary but not sufficient — it is the one thing every workspace commit
    /// observed has in common, but nothing stops an ordinary commit being authored under
    /// the same identity. Confirmation then comes from *either* the subject or the
    /// `gitbutler-headers-version` header, because GitButler does not always write both:
    /// a workspace commit created by `but setup` carries the header, while the one left by
    /// `but teardown` + `but setup` is a merge commit with no header at all. Requiring the
    /// header (the first thing tried here) silently declined to identify the second kind.
    ///
    /// A miss is not fatal — [`WorkspaceBlock::workspace_sha`] is then `None` and only the
    /// teardown route gets offered, which needs no sha.
    pub fn diagnose_workspace_block(&self, message: String) -> WorkspaceBlock {
        let mut block = WorkspaceBlock {
            message,
            workspace_sha: None,
            stray: Vec::new(),
        };
        // One subprocess for the whole walk. The depth bound is a backstop against a
        // repository where no workspace commit exists at all; in practice the answer is a
        // handful of commits down at most.
        let Ok(log) = self.git(&[
            "log",
            "--first-parent",
            "-n",
            "50",
            "--format=%H%x1f%ae%x1f%s",
            "HEAD",
        ]) else {
            return block;
        };
        let mut stray = Vec::new();
        for line in log.lines() {
            let mut fields = line.split('\x1f');
            let (Some(sha), Some(email), Some(subject)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            if email == GITBUTLER_AUTHOR
                && (subject.starts_with(WORKSPACE_SUBJECT) || self.has_managed_header(sha))
            {
                block.workspace_sha = Some(sha.to_string());
                block.stray = stray;
                return block;
            }
            stray.push(StrayCommit {
                sha: sha.chars().take(7).collect(),
                subject: subject.to_string(),
            });
        }
        // Falling out of the loop means no workspace commit was found within the bound, so
        // `stray` is just the last N commits of an ordinary history rather than a list of
        // anything wrong. Deliberately not reported: naming innocent commits as strays,
        // next to an offer to reset past them, would be worse than saying nothing.
        block
    }

    /// Whether a commit carries GitButler's own header block. Present on workspace commits
    /// written by `but setup`, absent on the merge commit `but teardown` leaves behind —
    /// hence a confirming signal rather than a required one. Headers sit above the blank
    /// line separating them from the message, so only that leading run is examined.
    fn has_managed_header(&self, sha: &str) -> bool {
        self.git(&["cat-file", "-p", sha]).is_ok_and(|body| {
            body.lines()
                .take_while(|l| !l.is_empty())
                .any(|l| l.starts_with("gitbutler-headers-version"))
        })
    }

    /// Moves the branch ref back to the workspace commit, leaving the index and worktree
    /// untouched — the stray commits' content comes back as uncommitted changes, ready to
    /// be committed properly with `but commit`.
    ///
    /// Deliberately `--soft`: nothing is discarded, and the stray commits stay in the
    /// reflog, so this is recoverable even if it turns out to be the wrong call.
    pub fn reset_soft(&self, sha: &str) -> Result<()> {
        self.git(&["reset", "--soft", sha])?;
        Ok(())
    }

    /// Runs `but teardown`: GitButler's own recommended escape, which snapshots first,
    /// uncommits the stray commits keeping their changes, and checks out a real branch.
    ///
    /// Output is captured rather than inherited, so this cannot scribble over the board
    /// while the alternate screen is still up.
    pub fn teardown(&self) -> Result<String> {
        self.run(&["teardown"])
    }

    /// Creates a new commit on `branch` from `changes` (uncommitted file or hunk ids), with
    /// `message`.
    ///
    /// 0.22 dropped `but stage`/`but rub`: there is no longer a way to mark an uncommitted
    /// change as belonging to a lane without committing it. A file only ever lives in the
    /// single global uncommitted area (`zz`) until it is named directly in a `commit` or
    /// `amend` call, so "move a card onto a lane" now means "commit or amend it there",
    /// immediately — see `App::confirm_move`.
    ///
    /// Mutation commands no longer embed a workspace status in their reply (verified live:
    /// `but commit` now returns a bare `{"commitId":…,"changeId":…}`), so failures are
    /// caught by `run` itself (a structured `CliError` on stdout, or a non-zero exit) and a
    /// full detailed status is always queried separately afterwards.
    ///
    /// A brand-new branch with no commits yet is sometimes reported as "merged upstream"
    /// (reproduced live against 0.22.0: a `branch new` immediately followed by a `commit`
    /// onto it can be refused this way, though the same sequence with a real pause between
    /// the two steps was not observed to fail — the exact trigger wasn't pinned down).
    /// 0.22's real safety net is for *editing already-landed history*, which cannot apply
    /// here — there is no history on the branch yet to disturb — so this retries once with
    /// `--allow-merged` specifically for that error rather than failing a first commit that
    /// is safe by construction.
    pub fn commit(&self, changes: &[String], message: &str, branch: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["commit"];
        args.extend(changes.iter().map(String::as_str));
        args.extend(["-m", message, "-b", branch, "--json"]);
        if let Err(e) = self.run(&args) {
            if e.to_string().contains("merged upstream") {
                args.push("--allow-merged");
                self.run(&args)?;
            } else {
                return Err(e);
            }
        }
        self.status()
    }

    /// Amends `changes` (uncommitted file or hunk ids) into `target` — a branch name (its
    /// tip) or a specific commit id. No message is needed: the target's existing message is
    /// kept.
    pub fn amend(&self, changes: &[String], target: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["amend"];
        args.extend(changes.iter().map(String::as_str));
        args.extend(["-t", target, "--json"]);
        self.run(&args)?;
        self.status()
    }

    /// Moves whole commits onto the top of `branch`'s stack.
    pub fn move_commits(&self, commits: &[String], branch: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["move"];
        args.extend(commits.iter().map(String::as_str));
        args.extend(["-b", branch, "--json"]);
        self.run(&args)?;
        self.status()
    }

    /// Squashes commits into `target`, keeping `target`'s existing message — the same
    /// "combine, don't ask" behaviour `but rub` used to have for a commit-onto-commit move.
    pub fn squash(&self, sources: &[String], target: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["squash"];
        args.extend(sources.iter().map(String::as_str));
        args.extend(["-t", target, "--use-target-message", "--json"]);
        self.run(&args)?;
        self.status()
    }

    /// Uncommits `sources` (commit ids, or `<commit-id>:<file-id>` committed files) back to
    /// the uncommitted area.
    pub fn uncommit(&self, sources: &[String]) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["uncommit"];
        args.extend(sources.iter().map(String::as_str));
        args.push("--json");
        self.run(&args)?;
        self.status()
    }
}

impl But {

    /// Asks what a push would do, without doing it.
    ///
    /// The branch is always explicit. `but push` with no branch and a non-interactive
    /// stdin does not prompt — it pushes *every* branch with unpushed commits, which is
    /// not something a single keystroke should ever be able to trigger.
    pub fn push_preview(&self, branch: &str) -> Result<PushPreview> {
        let raw = self.run(&["push", branch, "--dry-run", "--json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but push --dry-run` output: {raw:.400}"))
    }

    /// Checks whether landing `branch` onto the target would be clean, without doing it.
    ///
    /// `but land` has no `--dry-run` of its own — `branch show --check` is the closest
    /// thing to one: it reports the commits that would land and whether they merge
    /// cleanly. (0.21 also fixed the branch-id cache bug this used to need a `but branch`
    /// priming call to work around; `branch show` resolves names on its own now.)
    pub fn merge_check(&self, branch: &str) -> Result<MergeCheck> {
        let raw = self.run(&["branch", "show", branch, "--check", "--json"])?;
        serde_json::from_str(raw.trim()).with_context(|| {
            format!("could not parse `but branch show --check` output: {raw:.400}")
        })
    }

    /// Creates a branch, optionally stacked on top of `anchor`.
    ///
    /// With no anchor the branch becomes its own lane, applied in parallel. With one, it
    /// is stacked on top of that branch and shares its lane — which is what makes the two
    /// gestures on a board different operations rather than the same one.
    ///
    /// `branch new` does not embed a status the way `rub`/`commit`/`move` do (verified
    /// against 0.21.2: the reply is just `{"branch":…,"anchor":…}`), so the board is
    /// queried separately.
    pub fn branch_new(&self, name: &str, anchor: Option<&str>) -> Result<WorkspaceStatus> {
        let mut args = vec!["branch", "new", name];
        if let Some(a) = anchor {
            args.push("--anchor");
            args.push(a);
        }
        args.push("--json");
        self.run(&args)?;
        self.status()
    }

    /// Diff of the uncommitted worktree, one entry per hunk with a rub-able id.
    pub fn diff_uncommitted(&self) -> Result<DiffOutput> {
        let raw = self.run(&["diff", "--json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but diff` output: {raw:.400}"))
    }

    /// Diff of a commit. Entries carry no ids — history is not stageable.
    pub fn diff_target(&self, target: &str) -> Result<DiffOutput> {
        let raw = self.run(&["diff", target, "--json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but diff {target}` output: {raw:.400}"))
    }

    /// Asks what rebasing onto the updated target would do, without doing it.
    ///
    /// `--check` is read-only and reports per-branch outcomes, so a lane that would come
    /// out conflicted is visible before anything moves.
    pub fn pull_check(&self) -> Result<PullPreview> {
        let raw = self.run(&["pull", "--check", "--json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but pull --check` output: {raw:.400}"))
    }

    /// Fetches and rebases every applied branch onto the updated target.
    ///
    /// This is what GitButler calls a pull: its own help describes it as rebasing all
    /// applied branches on top of the updated target branch. It does not embed a status
    /// in its reply, so the caller refreshes separately.
    pub fn pull(&self) -> Result<()> {
        self.run(&["pull", "--json"])?;
        Ok(())
    }

    /// Deletes a branch from the workspace.
    pub fn branch_delete(&self, name: &str) -> Result<()> {
        self.run(&["branch", "delete", name, "--json"])?;
        Ok(())
    }

    /// Stacks an existing branch on top of another one.
    ///
    /// `but move <branch> --above <target>` (0.22) — the branch's commits are rebased onto
    /// the target's tip and it becomes the new lane tip.
    ///
    /// This rewrites history: moved commits get new SHAs, so a branch that was already
    /// pushed will need a force push afterwards.
    pub fn restack_branch(&self, source: &str, target: &str) -> Result<WorkspaceStatus> {
        if source == target {
            bail!("a branch cannot be stacked on itself");
        }
        self.run(&["move", source, "--above", target, "--json"])?;
        self.status()
    }

    /// Performs the push.
    ///
    /// `push` does not embed a status in its reply, so the caller refreshes separately.
    /// Hook flags are deliberately not passed: the spelling changed from `--run-hooks` to
    /// `--no-hooks`, so naming either one would break on one side of that release.
    pub fn push(&self, branch: &str) -> Result<()> {
        self.run(&["push", branch, "--json"])?;
        Ok(())
    }

    /// Lands `branch` directly onto the configured target — no pull request — and, per
    /// `but land`'s own documented behaviour, reconciles every other applied branch onto
    /// the result afterwards, the same as `but pull` does.
    ///
    /// Unlike the `but merge` this replaces (removed in 0.21), `land` works whether the
    /// target is local (`gb-local`, refs updated locally only) or a real remote (pushed
    /// directly, bypassing any PR/review process — verified against 0.21.2 with a real
    /// bare-repo remote). `--yes` skips the CLI's own confirmation prompt, since this
    /// client shows its own confirmation first. `land` prints nothing on success, so the
    /// board is queried separately.
    pub fn land(&self, branch: &str) -> Result<WorkspaceStatus> {
        self.run(&["land", branch, "--yes", "--json"])?;
        self.status()
    }

    /// Lands every branch in a stack, one `but land` call each.
    ///
    /// `but land` refuses a non-base branch outright rather than cascading on its own —
    /// verified live: "Refusing to land `tip`: it is stacked on top of 1 other segment(s)
    /// (base) whose commits would also be published to ... . Land the bottom segment
    /// `base` (or the whole stack) instead." There is no flag or stack-id argument that
    /// does that landing for you (`but land <stack-id>` fails with "Expected a branch ID,
    /// got a stack") — landing bottom-to-top by name, one call at a time, is the only way.
    ///
    /// `branches` must already be ordered base first; this only ever calls `land` in the
    /// order given; it does not sort.
    pub fn land_stack(&self, branches: &[String]) -> Result<WorkspaceStatus> {
        for (landed, branch) in branches.iter().enumerate() {
            self.run(&["land", branch, "--yes", "--json"])
                .with_context(|| {
                    format!(
                        "landed {landed} of {} branches in the stack, then failed on `{branch}`",
                        branches.len()
                    )
                })?;
        }
        self.status()
    }

    /// Undoes the last operation, restoring the workspace's prior state — including
    /// uncommitted changes. Verified against 0.21.2: this always succeeds and prints
    /// nothing, silently doing nothing when there is no prior operation to undo, so the
    /// caller cannot tell a real undo from a no-op except by comparing the board.
    pub fn undo(&self) -> Result<WorkspaceStatus> {
        self.run(&["undo", "--json"])?;
        self.status()
    }

    /// Redoes the last undone operation. Same no-signal-on-no-op behaviour as `undo`.
    pub fn redo(&self) -> Result<WorkspaceStatus> {
        self.run(&["redo", "--json"])?;
        self.status()
    }
}

/// Split out so it can be tested against captured output without spawning anything.
///
/// Reverses `stacks` back to oldest-first: verified against 0.21.2, the wire order is
/// newest-created-first, reversed from 0.19's oldest-first. Normalizing here, at the one
/// place every caller's status passes through, keeps lane position stable as work
/// continues — a new parallel lane appends on the right rather than shoving every existing
/// lane sideways — without every downstream consumer needing to know about the flip.
pub fn parse_status(raw: &str) -> Result<WorkspaceStatus> {
    let mut status: WorkspaceStatus = serde_json::from_str(raw.trim())
        .with_context(|| format!("could not parse `but status` output: {raw:.400}"))?;
    status.stacks.reverse();
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both spellings matter: 0.21 emits the `teardown` wording, while `but-workspace`'s
    /// `ref_info` carries an older one that suggests `git reset --soft` directly. Missing
    /// either would leave the board silently frozen instead of stopping.
    #[test]
    fn recognises_both_spellings_of_a_blocked_workspace() {
        assert!(is_workspace_block(
            "`but status -f --format json` failed: Error: GitButler mode exit required: \
             please run `but teardown` to preserve your work."
        ));
        assert!(is_workspace_block(
            "Found 2 commit(s) on top of the workspace commit.\n\n    git reset --soft abc123"
        ));
    }

    /// Ordinary failures must stay ordinary failures. Escalating one of these to a modal
    /// that offers to rewrite history would be its own bug.
    #[test]
    fn leaves_ordinary_failures_alone() {
        for msg in [
            "`but land` failed: Configured target branch has no push remote",
            "could not run `but`. Install the GitButler CLI",
            "rub succeeded but the workspace refresh failed: no such commit",
            "`but commit` failed: changes depend on another branch",
        ] {
            assert!(!is_workspace_block(msg), "{msg:?} is not a workspace block");
        }
    }

    #[test]
    fn parses_version_strings() {
        assert_eq!(
            Version::parse("but 0.19.3").unwrap(),
            Version {
                major: 0,
                minor: 19,
                patch: 3
            }
        );
        assert_eq!(
            Version::parse("but 0.21.2\n").unwrap(),
            Version {
                major: 0,
                minor: 21,
                patch: 2
            }
        );
        // Pre-release suffixes must not break the gate.
        assert_eq!(
            Version::parse("but 1.0.0-nightly.4").unwrap(),
            Version {
                major: 1,
                minor: 0,
                patch: 0
            }
        );
        assert!(Version::parse("but unknown").is_err());
    }

    #[test]
    fn version_ordering_drives_the_gate() {
        assert!(Version::parse("but 0.21.2").unwrap() < MIN_VERSION);
        assert!(Version::parse("but 0.22.0").unwrap() >= MIN_VERSION);
        assert!(Version::parse("but 0.23.0").unwrap() > VERIFIED_THROUGH);
    }

    #[test]
    fn parses_captured_status() {
        let raw = include_str!("../tests/fixtures/status.json");
        let s = parse_status(raw).unwrap();
        assert_eq!(s.stacks.len(), 3);
    }
}
