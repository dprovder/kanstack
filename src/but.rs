//! Subprocess client for the `but` CLI.
//!
//! This is deliberately the *only* coupling to GitButler: we spawn a `but` binary the
//! user installed themselves and speak its documented JSON format. We link none of its
//! code, so this project stays independently licensed (see README).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::model::{
    CliError, DiffOutput, MergeCheck, MutationEnvelope, PullPreview, PushPreview, WorkspaceStatus,
};

/// Oldest `but` whose JSON shape this was verified against.
///
/// 0.21 dropped `-j`/`--json` and `--status-after` in favour of `--format json`, which is
/// not a syntax older `but` understands — there is no flag spelling that works on both
/// sides of that release, so this client picks a side rather than branching on version at
/// every call site.
pub const MIN_VERSION: Version = Version {
    major: 0,
    minor: 21,
    patch: 0,
};
/// Newest `but` actually exercised while building this. Newer is allowed but noted.
pub const VERIFIED_THROUGH: Version = Version {
    major: 0,
    minor: 21,
    patch: 2,
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
        let raw = self.run(&["status", "-f", "--format", "json"])?;
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

    /// Runs `but rub SOURCE TARGET`, the CLI's combine primitive, and returns the
    /// refreshed workspace.
    ///
    /// Both a move and a squash are this same call — only the target id differs. Rubbing
    /// onto a branch moves or stages; rubbing onto a commit squashes or amends.
    ///
    /// Mutating commands embed a status in their reply by default now (0.21 dropped the
    /// old opt-in `--status-after` flag entirely), but that embedded status still omits
    /// per-commit file lists — `rub` has no `-f` of its own to ask for them. A second
    /// detailed query follows regardless, which costs roughly another 90ms and is what
    /// keeps cards from losing their file context the moment you move one.
    ///
    /// The embedded status is never actually read, only used (via `status_error`) to tell
    /// a real failure from a plain success — `commit` and `restack_branch` already work
    /// this way. Reproduced live: `but rub`'s very first mutation against a fresh branch
    /// can come back as a bare `{"ok":true}` with no envelope at all outside of kanstack
    /// entirely (plain `but` CLI, no code of ours involved), so treating a missing
    /// `status` as fatal here — the one place among these three that did — occasionally
    /// failed an operation that had, in fact, fully succeeded.
    pub fn rub(&self, source: &str, target: &str) -> Result<WorkspaceStatus> {
        let raw = self.run(&["rub", source, target, "--format", "json"])?;
        let env: MutationEnvelope = serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but rub` output: {raw:.400}"))?;
        if let Some(err) = env.status_error {
            bail!("rub succeeded but the workspace refresh failed: {err}");
        }
        self.status()
    }

    /// `rub`s every source in `sources` onto `target`, one `but rub` call each — the batch
    /// counterpart for a multi-select move. File `cliId`s stay stable across sequential
    /// rubs within the same status snapshot (verified live: staging three unassigned
    /// files one after another using ids captured before any of them, with no re-query in
    /// between, still landed all three), so this can just walk the list once rather than
    /// re-resolving ids after each call the way hunk ids would need.
    pub fn rub_many(&self, sources: &[String], target: &str) -> Result<WorkspaceStatus> {
        for (moved, source) in sources.iter().enumerate() {
            let raw = self.run(&["rub", source, target, "--format", "json"])?;
            let env: MutationEnvelope = serde_json::from_str(raw.trim()).with_context(|| {
                format!("could not parse `but rub` output for `{source}`: {raw:.400}")
            })?;
            if let Some(err) = env.status_error {
                bail!(
                    "moved {moved} of {}, then the workspace refresh failed on `{source}`: {err}",
                    sources.len()
                );
            }
        }
        self.status()
    }
}

impl But {
    /// Commits the changes assigned to `branch`, and only those.
    ///
    /// `--only` is not optional here. Without it `but commit` also sweeps in every
    /// *unassigned* change in the worktree — documented behaviour, and reasonable for a
    /// command line, but wrong for a board: the entire point of dragging cards into a lane
    /// is that the lane's contents are what gets committed. Omitting the flag silently
    /// empties the backlog into whichever lane you happened to be standing on.
    ///
    /// The embedded status still lacks `-f` file lists, so a detailed query follows.
    pub fn commit(&self, branch: &str, message: &str) -> Result<WorkspaceStatus> {
        let raw = self.run(&[
            "commit",
            branch,
            "-m",
            message,
            "--only",
            "--format",
            "json",
        ])?;
        let env: MutationEnvelope = serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but commit` output: {raw:.400}"))?;
        if let Some(err) = env.status_error {
            bail!("commit succeeded but the workspace refresh failed: {err}");
        }
        self.status()
    }

    /// Asks what a push would do, without doing it.
    ///
    /// The branch is always explicit. `but push` with no branch and a non-interactive
    /// stdin does not prompt — it pushes *every* branch with unpushed commits, which is
    /// not something a single keystroke should ever be able to trigger.
    pub fn push_preview(&self, branch: &str) -> Result<PushPreview> {
        let raw = self.run(&["push", branch, "--dry-run", "--format", "json"])?;
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
        let raw = self.run(&["branch", "show", branch, "--check", "--format", "json"])?;
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
        args.push("--format");
        args.push("json");
        self.run(&args)?;
        self.status()
    }

    /// Diff of the uncommitted worktree, one entry per hunk with a rub-able id.
    pub fn diff_uncommitted(&self) -> Result<DiffOutput> {
        let raw = self.run(&["diff", "--format", "json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but diff` output: {raw:.400}"))
    }

    /// Diff of a commit. Entries carry no ids — history is not stageable.
    pub fn diff_target(&self, target: &str) -> Result<DiffOutput> {
        let raw = self.run(&["diff", target, "--format", "json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but diff {target}` output: {raw:.400}"))
    }

    /// Asks what rebasing onto the updated target would do, without doing it.
    ///
    /// `--check` is read-only and reports per-branch outcomes, so a lane that would come
    /// out conflicted is visible before anything moves.
    pub fn pull_check(&self) -> Result<PullPreview> {
        let raw = self.run(&["pull", "--check", "--format", "json"])?;
        serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but pull --check` output: {raw:.400}"))
    }

    /// Fetches and rebases every applied branch onto the updated target.
    ///
    /// This is what GitButler calls a pull: its own help describes it as rebasing all
    /// applied branches on top of the updated target branch. It does not embed a status
    /// in its reply, so the caller refreshes separately.
    pub fn pull(&self) -> Result<()> {
        self.run(&["pull", "--format", "json"])?;
        Ok(())
    }

    /// Deletes a branch from the workspace.
    pub fn branch_delete(&self, name: &str) -> Result<()> {
        self.run(&["branch", "delete", name, "--format", "json"])?;
        Ok(())
    }

    /// Stacks an existing branch on top of another one.
    ///
    /// A single native `but move <branch> <target>` (0.21) — the branch's commits are
    /// rebased onto the target's tip and it becomes the new lane tip. This replaced a
    /// four-call workaround (create a stacked temp branch, `rub` each commit across,
    /// delete the source, rename the temp branch back) that earlier `but` versions needed
    /// because no subcommand exposed the underlying `but_api::branch::move_branch`
    /// directly.
    ///
    /// This rewrites history: moved commits get new SHAs, so a branch that was already
    /// pushed will need a force push afterwards.
    pub fn restack_branch(&self, source: &str, target: &str) -> Result<WorkspaceStatus> {
        if source == target {
            bail!("a branch cannot be stacked on itself");
        }
        let raw = self.run(&["move", source, target, "--format", "json"])?;
        let env: MutationEnvelope = serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but move` output: {raw:.400}"))?;
        if let Some(err) = env.status_error {
            bail!("move succeeded but the workspace refresh failed: {err}");
        }
        self.status()
    }

    /// Performs the push.
    ///
    /// `push` does not embed a status in its reply, so the caller refreshes separately.
    /// Hook flags are deliberately not passed: the spelling changed from `--run-hooks` to
    /// `--no-hooks`, so naming either one would break on one side of that release.
    pub fn push(&self, branch: &str) -> Result<()> {
        self.run(&["push", branch, "--format", "json"])?;
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
        self.run(&["land", branch, "--yes", "--format", "json"])?;
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
            self.run(&["land", branch, "--yes", "--format", "json"])
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
        self.run(&["undo", "--format", "json"])?;
        self.status()
    }

    /// Redoes the last undone operation. Same no-signal-on-no-op behaviour as `undo`.
    pub fn redo(&self) -> Result<WorkspaceStatus> {
        self.run(&["redo", "--format", "json"])?;
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
        assert!(Version::parse("but 0.19.3").unwrap() < MIN_VERSION);
        assert!(Version::parse("but 0.21.0").unwrap() >= MIN_VERSION);
        assert!(Version::parse("but 0.22.0").unwrap() > VERIFIED_THROUGH);
    }

    #[test]
    fn parses_captured_status() {
        let raw = include_str!("../tests/fixtures/status.json");
        let s = parse_status(raw).unwrap();
        assert_eq!(s.stacks.len(), 3);
    }
}
