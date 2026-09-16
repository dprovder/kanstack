use super::*;

impl But {

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
}
