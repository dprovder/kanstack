use super::*;

impl But {
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

    /// Performs the push.
    ///
    /// `push` does not embed a status in its reply, so the caller refreshes separately.
    /// Hook flags are deliberately not passed: the spelling changed from `--run-hooks` to
    /// `--no-hooks`, so naming either one would break on one side of that release.
    pub fn push(&self, branch: &str) -> Result<()> {
        self.run(&["push", branch, "--json"])?;
        Ok(())
    }
}
