use super::*;
use crate::model::ResolveConflicts;

impl But {
    /// Lists `target`'s conflicts without entering resolution mode.
    ///
    /// `target` is a branch name, not a commit id: applying a resolution changes the
    /// commit id (see `resolve_apply_side`), while `but resolve conflicts <branch>` keeps
    /// finding that branch's oldest conflicted commit for as long as one remains, so
    /// callers can hold onto the branch name across a whole resolution session. `but`
    /// itself refuses a branch with no conflicted commits (plain text on stderr, not
    /// `--json`), which doubles as this method's "nothing to resolve" error.
    pub fn resolve_conflicts(&self, target: &str) -> Result<ResolveConflicts> {
        let raw = self.run(&["resolve", "conflicts", target, "--json"])?;
        parse_resolve_conflicts(&raw)
    }

    /// Resolves every conflict in `path` (within `commit`'s conflicts) by taking one side.
    ///
    /// Refuses a delete/rename conflict — see [`crate::model::ManualConflict`]. `commit` is
    /// best passed as the branch name for the same reason `resolve_conflicts` prefers it:
    /// the commit id this call resolves changes with every apply.
    pub fn resolve_apply_side(&self, commit: &str, path: &str, ours: bool) -> Result<()> {
        let side = if ours { "--ours" } else { "--theirs" };
        self.run(&["resolve", "apply", path, "--commit", commit, side, "--json"])?;
        Ok(())
    }

    /// Resolves every conflict in `commit_id` with the configured AI model, one shot — no
    /// resolution mode entered or left, per `but resolve --help`. Needs an actual commit id
    /// (a branch name is refused, verified live against 0.22.3), so callers fetch this from
    /// a fresh [`ResolveConflicts::commit_id`] immediately beforehand rather than caching it.
    ///
    /// Not exercised end to end here: this environment has no AI provider authorized, so
    /// only the failure path (still a plain `but` error, handled like any other) was seen
    /// live. The one-shot behaviour otherwise rests on `but resolve --help`'s own wording.
    pub fn resolve_ai(&self, commit_id: &str) -> Result<()> {
        self.run(&["resolve", commit_id, "--ai", "--json"])?;
        Ok(())
    }
}

/// Split out, same as `parse_status`, so it can be tested against captured output without
/// spawning anything.
fn parse_resolve_conflicts(raw: &str) -> Result<ResolveConflicts> {
    serde_json::from_str(raw.trim())
        .with_context(|| format!("could not parse `but resolve conflicts` output: {raw:.400}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single text conflict, captured live against 0.22.3: a two-way modification with
    /// one hunk, resolvable with `--ours`/`--theirs`.
    #[test]
    fn parses_a_captured_text_conflict() {
        let raw = include_str!("../../tests/fixtures/resolve_conflicts.json");
        let rc = parse_resolve_conflicts(raw).unwrap();
        assert_eq!(rc.branch, "my-feature");
        assert!(rc.manual.is_empty());
        assert_eq!(rc.files.len(), 1);
        let file = &rc.files[0];
        assert_eq!(file.path, "file.txt");
        assert_eq!(file.hunks.len(), 1);
        assert_eq!(file.hunks[0].ours, "line1-upstream");
        assert_eq!(file.hunks[0].theirs, "line1-feature");
    }

    /// A delete/rename conflict, captured live against 0.22.3: `files` is empty and the
    /// whole thing lands in `manual`, since `resolve apply --ours/--theirs` (and `--ai`)
    /// both refuse this shape outright.
    #[test]
    fn parses_a_captured_manual_only_conflict() {
        let raw = include_str!("../../tests/fixtures/resolve_conflicts_manual.json");
        let rc = parse_resolve_conflicts(raw).unwrap();
        assert!(rc.files.is_empty());
        assert_eq!(rc.manual.len(), 1);
        assert_eq!(rc.manual[0].path, "file.txt");
        assert_eq!(rc.manual[0].reason, "The conflict involves a deletion or rename.");
    }
}
