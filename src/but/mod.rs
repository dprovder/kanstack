//! Subprocess client for the `but` CLI.
//!
//! This is deliberately the *only* coupling to GitButler: we spawn a `but` binary the
//! user installed themselves and speak its documented JSON format. We link none of its
//! code, so this project stays independently licensed (see README).
//!
//! Split across files by command family, mirroring `app`'s own split — a new subcommand
//! wrapper gets its own file rather than another method appended here. `But` itself, and
//! the exec plumbing every command uses (`run`/`git`), stay in this file; each `pub fn`
//! command method is still callable as `but.method(...)` from anywhere regardless of which
//! file its `impl But` block lives in — only the type needs to be in scope, not the module
//! the method happens to be defined in.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::model::{
    BranchList, CliError, DiffOutput, MergeCheck, PullPreview, PushPreview, WorkspaceStatus,
};

mod branch;
mod diff;
mod land;
mod move_commits;
mod pr;
mod push;
mod rebase;
mod setup;
mod skill;
mod undo;

pub use pr::PrOptions;

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
    patch: 3,
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

/// Recognises the other special `but status` failure worth a dedicated recovery screen:
/// the directory isn't a GitButler project. Two spellings, same fix: `setup_required` is
/// `but`'s structured error code for an existing git repo that's never had `but setup` run
/// (see `model::CliError`); "No git repository found" is its plain-text equivalent for a
/// directory that isn't even a git repo yet. Both are fixed by the same command — see
/// `But::run_setup` — so there's no need to tell them apart past this point.
pub fn is_setup_required(msg: &str) -> bool {
    msg.contains("setup_required") || msg.contains("No git repository found")
}

/// Recognises `but setup --init`'s one distinct, actionable failure: no git identity
/// configured anywhere (confirmed live — no fallback identity, and no flag to supply one
/// inline), so the empty commit it needs to create has no author. Worth a specific hint
/// rather than a bare "setup failed", since the fix is a one-liner and unrelated to
/// anything about the repository itself.
pub fn is_missing_git_identity(msg: &str) -> bool {
    msg.contains("Author identity is not configured")
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

/// `but skill check --json`'s reply: every GitButler coding-agent skill file found,
/// installed locally and/or globally, and whether each is current for this `but` version.
/// Field names are already snake_case on the wire (unlike most of `but`'s other JSON,
/// which is camelCase — see `crate::model`), so this needs no `rename_all`.
#[derive(Debug, Clone, Deserialize)]
pub struct SkillCheck {
    pub skills: Vec<SkillEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillEntry {
    /// e.g. "Claude Code", "Codex", "OpenCode" — whatever coding agent this install is
    /// shaped for.
    pub format_name: String,
    pub scope: String,
    pub up_to_date: bool,
}

pub struct But {
    bin: PathBuf,
    cwd: PathBuf,
    version: Version,
}

impl But {
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

    // 0.22.1 added `--status-after`, which folds a mutation and its resulting workspace
    // status into one call (`{"result":…,"status":…}`) instead of two. Tempting, but
    // deliberately *not* used here: verified live against 0.22.3, the status it appends is
    // never `-f`-detailed — commits in it always carry `"changes": null` — and every commit
    // card's file-name subtitle (board.rs, reading `commit.changes`) depends on that detail
    // being present on every refresh, not just the next background one. There is no flag
    // that asks the appended status for file lists, so adopting this would trade a real
    // round trip for a real (if brief) rendering regression. Left as a plain note rather
    // than removed from `but`'s vocabulary entirely, in case a future `but` adds an
    // `-f`-equivalent to `--status-after` and this becomes worth revisiting.

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
}

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

    /// Both spellings matter: the structured `setup_required` error code for an existing,
    /// not-yet-`but setup` repo, and the plain-text "no git repository" wording for a
    /// directory that isn't a repo at all. Both get the same recovery screen.
    #[test]
    fn recognises_both_spellings_of_setup_required() {
        assert!(is_setup_required(
            "setup_required: No GitButler project found at .\nhint: run `but setup` to configure the project"
        ));
        assert!(is_setup_required(
            "`but status -f --json` failed: Error: No git repository found at .\nPlease run 'but setup' to initialize the project."
        ));
    }

    /// Ordinary failures, including the *other* special case, must not be mistaken for this
    /// one — escalating either into the wrong recovery screen would be its own bug.
    #[test]
    fn setup_required_leaves_other_failures_alone() {
        for msg in [
            "`but land` failed: Configured target branch has no push remote",
            "could not run `but`. Install the GitButler CLI",
            "Error: GitButler mode exit required: please run `but teardown` to preserve your work.",
        ] {
            assert!(!is_setup_required(msg), "{msg:?} is not a setup-required failure");
        }
    }

    /// Confirmed live: this is the exact (and only observed) wording `but setup --init`
    /// produces with no git identity configured anywhere.
    #[test]
    fn recognises_a_missing_git_identity() {
        assert!(is_missing_git_identity("Error: Author identity is not configured"));
        assert!(!is_missing_git_identity("setup_required: No GitButler project found at ."));
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
        let raw = include_str!("../../tests/fixtures/status.json");
        let s = parse_status(raw).unwrap();
        assert_eq!(s.stacks.len(), 3);
    }
}
