use super::*;

impl But {
    /// Turns a plain git repo (or no repo at all) into a GitButler project. `--init`
    /// creates a fresh repo + empty commit only when one doesn't already exist — confirmed
    /// idempotent against an existing repo (no extra commit beyond GitButler's own), so
    /// this is always safe to call regardless of which of the two `is_setup_required`
    /// cases triggered it.
    pub fn run_setup(&self) -> Result<()> {
        self.run(&["setup", "--init", "--json"])?;
        Ok(())
    }
}
