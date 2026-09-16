use super::*;

impl But {

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
}
