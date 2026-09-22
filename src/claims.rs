//! Who is actively editing which file, right now — the preventive half of the cross-lane
//! collision problem `docs/automation.md`'s "Concurrency guarantees" section describes:
//! `but`'s hunk-dependency calculator can get stuck when two lanes' uncommitted edits land
//! within a few lines of each other, and by the time that happens the damage is already on
//! disk. This module lets `kanstack claim` (run from a `PreToolUse` hook, see
//! `crate::cli::claim` and `Harness::status_hooks`) veto an `Edit`/`Write` *before* it lands,
//! when another lane is already mid-edit of the exact same file — instead of reconciling the
//! fused hunks after the fact, which is issue #9's job, not this one's.
//!
//! Structurally the same shape as [`crate::report::Reports`]: one small file per claimed path
//! (not per branch — the key here is the file, since that's what a collision is keyed on),
//! written via a temp file and rename so a reader never sees half of one, in a directory
//! beside `reports-`/`pids-`/`events-` (see [`crate::workstream::claims_dir`]). No daemon, no
//! database, no lock a caller waits on — a claim is just a fact on disk with a freshness
//! window, read and written by whichever `kanstack claim` process happens to run.
//!
//! **Exact file path only.** Two lanes editing different regions of the same large file are
//! still blocked from each other — a false positive this v1 accepts rather than attempting
//! line-range tracking, which is a much harder problem (a file claimed mid-edit has no stable
//! "region" to compare against another lane's). See the module doc on `crate::cli::claim` for
//! the rest of the false-positive/false-negative tradeoffs.
//!
//! **Claude Code only, for now.** `crate::harness::Claude` is the only [`crate::harness::Harness`]
//! that installs the `PreToolUse` hook this reads and writes through; every other supported
//! harness (codex, pi, opencode, kiro, gemini) can block a tool call by some equivalent
//! mechanism, confirmed in the design discussion that led to this module, but wiring them is
//! deliberately left for later — see the `// TODO` on each `Harness` impl's own
//! `status_hooks`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::workstream::{claims_dir, fnv1a};

/// How long a claim is believed to still be live. Much shorter than
/// [`crate::report::FRESH_FOR`] (ten minutes): a report says "a turn is running somewhere in
/// this pane", which can legitimately go quiet between tool calls for a while, but a claim
/// says "this exact file was touched a moment ago" — and `kanstack claim` renews it on every
/// `Edit`/`Write`/`MultiEdit` call, so as long as a lane keeps editing the same file its claim
/// never goes stale. Once it does — because the lane moved on to a different file, or
/// crashed — this is how soon another lane stops being blocked by it. Long enough to span the
/// gap between two tool calls against the same file within one turn (seconds to tens of
/// seconds, even for a slow generation); short enough that it clears years before a stuck
/// `busy` report would (which is also checked — see `crate::cli::claim` — so this mostly
/// matters for a lane that is still busy but has genuinely moved on to another file).
pub const FRESH_FOR: Duration = Duration::from_secs(2 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    /// Only for whoever is looking at the file; the name of the file is a hash of `file`.
    file: String,
    branch: String,
    /// Seconds since the Unix epoch.
    at: u64,
}

/// Where one repository's file-edit claims live. Cheap to clone; a default one (no directory)
/// reads nothing and writes nowhere, same as [`crate::report::Reports`] with nowhere to keep
/// reports — what tests and a machine with no home directory get.
#[derive(Debug, Clone, Default)]
pub struct Claims {
    dir: Option<PathBuf>,
}

impl Claims {
    pub fn for_repo(repo: &Path) -> Self {
        Claims { dir: claims_dir(repo) }
    }

    #[cfg(test)]
    pub(crate) fn in_dir(dir: PathBuf) -> Self {
        Claims { dir: Some(dir) }
    }

    /// One file per claimed path, named by a hash of it so any path — slashes and all — is a
    /// valid file name and none can collide with another.
    fn file(&self, file_path: &str) -> Option<PathBuf> {
        Some(self.dir.as_ref()?.join(format!("{:016x}.json", fnv1a(file_path.as_bytes()))))
    }

    /// Records that `branch` touched `file_path` as of `now`, replacing whatever claim was
    /// there before — even one held by a different branch. The newest touch always wins rather
    /// than being refused: `kanstack claim` only calls this once it has already decided
    /// (against a fresh, busy-backed claim) that no other lane is actively in the way, so by
    /// the time this runs, taking the claim is correct, not a race to arbitrate.
    pub fn claim(&self, file_path: &str, branch: &str, now: SystemTime) -> Result<()> {
        let Some(path) = self.file(file_path) else {
            anyhow::bail!("no home directory to keep claims in");
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let at = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let stored = Stored { file: file_path.to_string(), branch: branch.to_string(), at };
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string(&stored)? + "\n")?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Who holds a fresh claim on `file_path` right now, if anyone. A missing, unreadable,
    /// malformed or expired claim reads as "nobody" — same "a claim is only a hint" spirit as
    /// [`crate::report::Reports::said`]: a bad claim file must never get in the way of an edit
    /// that would otherwise be fine.
    pub fn holder(&self, file_path: &str, now: SystemTime) -> Option<String> {
        let raw = std::fs::read_to_string(self.file(file_path)?).ok()?;
        let stored: Stored = serde_json::from_str(&raw).ok()?;
        let at = UNIX_EPOCH + Duration::from_secs(stored.at);
        // A claim from the future (clock skew) is as fresh as it gets, same call
        // `Reports::said` makes.
        let age = now.duration_since(at).unwrap_or_default();
        (age <= FRESH_FOR).then_some(stored.branch)
    }

    /// Drops every claim `branch` holds, wherever it is. Best-effort — a directory that can't
    /// be listed or a file that can't be removed just leaves that claim to expire on its own
    /// (see [`FRESH_FOR`]), the same "never fail the caller" spirit as
    /// [`crate::report::Reports::forget`]. Called when `branch` goes idle or its pane is
    /// stopped (see `Reports::write`/`forget`), so a lane that has moved on doesn't keep
    /// blocking another one on a file it is no longer touching, without needing to wait out
    /// the freshness window.
    ///
    /// A full scan of the claims directory rather than a lookup by branch: claims are keyed by
    /// file, not by branch (unlike reports, one branch can hold several at once), so there is
    /// no direct path from a branch name to its claim files. In practice this directory holds
    /// at most a handful of entries — one per file a lane is mid-edit of — so the scan is
    /// cheap; nothing here is on as hot a path as `kanstack claim` itself.
    pub fn release_all(&self, branch: &str) {
        let Some(dir) = &self.dir else { return };
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(stored) = serde_json::from_str::<Stored>(&raw) else { continue };
            if stored.branch == branch {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> Claims {
        let dir = std::env::temp_dir().join(format!("kanstack-claims-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Claims::in_dir(dir)
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn a_claim_reads_back_as_its_holder() {
        let claims = scratch("roundtrip");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1001)), Some("feat-a".to_string()));
        assert_eq!(claims.holder("/repo/src/other.rs", at(1001)), None, "nobody claimed this one");
    }

    #[test]
    fn a_later_claim_on_the_same_file_replaces_the_holder() {
        let claims = scratch("replace");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        claims.claim("/repo/src/lib.rs", "feat-b", at(1005)).unwrap();
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1006)), Some("feat-b".to_string()));
    }

    #[test]
    fn a_claim_is_believed_for_a_while_and_then_not() {
        let claims = scratch("stale");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        let limit = 1000 + FRESH_FOR.as_secs();
        assert_eq!(claims.holder("/repo/src/lib.rs", at(limit)), Some("feat-a".to_string()), "the limit itself is still fresh");
        assert_eq!(claims.holder("/repo/src/lib.rs", at(limit + 1)), None);
    }

    #[test]
    fn a_claim_from_the_future_counts_as_fresh() {
        let claims = scratch("skew");
        claims.claim("/repo/src/lib.rs", "feat-a", at(5000)).unwrap();
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1000)), Some("feat-a".to_string()));
    }

    #[test]
    fn renewing_a_claim_keeps_it_fresh_past_the_original_window() {
        let claims = scratch("renew");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000 + FRESH_FOR.as_secs() - 1)).unwrap();
        let past_original_window = at(1000 + FRESH_FOR.as_secs() + 1);
        assert_eq!(claims.holder("/repo/src/lib.rs", past_original_window), Some("feat-a".to_string()));
    }

    #[test]
    fn releasing_a_branch_drops_only_its_own_claims() {
        let claims = scratch("release");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        claims.claim("/repo/src/other.rs", "feat-a", at(1000)).unwrap();
        claims.claim("/repo/src/third.rs", "feat-b", at(1000)).unwrap();
        claims.release_all("feat-a");
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1001)), None);
        assert_eq!(claims.holder("/repo/src/other.rs", at(1001)), None);
        assert_eq!(claims.holder("/repo/src/third.rs", at(1001)), Some("feat-b".to_string()));
        claims.release_all("never-claimed-anything"); // nothing to remove is not an error
    }

    #[test]
    fn a_corrupt_claim_reads_as_nobody_holding_it() {
        let claims = scratch("corrupt");
        claims.claim("/repo/src/lib.rs", "feat-a", at(1000)).unwrap();
        std::fs::write(claims.file("/repo/src/lib.rs").unwrap(), "{ not json").unwrap();
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1001)), None);
    }

    #[test]
    fn with_nowhere_to_keep_claims_nothing_is_read_and_writing_says_so() {
        let claims = Claims::default();
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1)), None);
        assert!(claims.claim("/repo/src/lib.rs", "feat-a", at(1)).is_err());
        claims.release_all("feat-a"); // must not panic with nowhere to look
    }

    #[test]
    fn file_paths_with_slashes_are_distinct_files_on_disk() {
        let claims = scratch("paths");
        for (i, path) in ["/repo/src/lib.rs", "/repo/src/mod.rs", "/repo/src/mod/lib.rs"].iter().enumerate() {
            claims.claim(path, if i % 2 == 0 { "feat-a" } else { "feat-b" }, at(1000)).unwrap();
        }
        assert_eq!(claims.holder("/repo/src/lib.rs", at(1001)), Some("feat-a".to_string()));
        assert_eq!(claims.holder("/repo/src/mod.rs", at(1001)), Some("feat-b".to_string()));
        assert_eq!(claims.holder("/repo/src/mod/lib.rs", at(1001)), Some("feat-a".to_string()));
    }
}
