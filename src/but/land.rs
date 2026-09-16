use super::*;

impl But {

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
}
