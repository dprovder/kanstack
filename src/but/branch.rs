use super::*;

/// Where a new branch is placed relative to an existing applied branch, for
/// [`But::branch_new`] — `but branch new --above`/`--below` (0.22). Spelled out as an enum,
/// rather than the crate's older single `anchor: Option<&str>` (which only ever meant
/// "above", via the now-deprecated `--anchor` alias), once a caller (`kanstack spawn
/// --below`) needed to say which direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement<'a> {
    Above(&'a str),
    Below(&'a str),
}

impl But {

    /// Creates a branch, optionally stacked above or below `placement`'s branch.
    ///
    /// With no placement the branch becomes its own lane, applied in parallel. With one,
    /// it shares that branch's stack — which is what makes the two gestures on a board
    /// different operations rather than the same one.
    ///
    /// `branch new` does not embed a status the way `rub`/`commit`/`move` do (verified
    /// against 0.21.2: the reply is just `{"branch":…,"anchor":…}`), so the board is
    /// queried separately.
    pub fn branch_new(&self, name: &str, placement: Option<Placement<'_>>) -> Result<WorkspaceStatus> {
        let mut args = vec!["branch", "new", name];
        match placement {
            Some(Placement::Above(a)) => args.extend(["--above", a]),
            Some(Placement::Below(b)) => args.extend(["--below", b]),
            None => {}
        }
        args.push("--json");
        self.run(&args)?;
        self.status()
    }

    /// Deletes a branch from the workspace.
    pub fn branch_delete(&self, name: &str) -> Result<()> {
        self.run(&["branch", "delete", name, "--json"])?;
        Ok(())
    }

    /// Lists every branch `but` knows about, applied and not.
    ///
    /// The only window onto branches outside the workspace — `status` reports applied
    /// stacks and nothing else, so without this the board cannot even know a parked branch
    /// exists. Deliberately *not* on the refresh path: the default listing runs a merge
    /// check and an ahead-count per branch, which is far more work than a status read, so
    /// this is called when the drawer opens rather than on every file save.
    ///
    /// `--review` is left off, which is what keeps that cost bounded to local work: it
    /// would add a forge round-trip per branch. The default truncation (active + 20 most
    /// recent) is likewise left in place unless `all` is set — `has_more_branches` on the
    /// default listing is what tells the drawer there's more to ask for, so the common case
    /// stays cheap and the uncommon one is at least honest about what it is not showing.
    pub fn branch_list(&self, all: bool) -> Result<BranchList> {
        let mut args = vec!["branch", "list"];
        if all {
            args.push("--all");
        }
        args.push("--json");
        let raw = self.run(&args)?;
        serde_json::from_str(&raw)
            .with_context(|| format!("could not parse `but branch list` output: {raw:.400}"))
    }

    /// Applies an unapplied branch, bringing it into the workspace as a parallel lane.
    ///
    /// This writes to the working directory — the branch's changes materialize on disk —
    /// so it can fail on a dirty tree or produce conflicts. `apply` embeds no status in its
    /// reply, so the caller refreshes separately, the same as `push` and `branch_delete`.
    pub fn apply(&self, name: &str) -> Result<()> {
        self.run(&["apply", name, "--json"])?;
        Ok(())
    }

    /// Unapplies a branch, and with it the *entire stack* the branch belongs to.
    ///
    /// That whole-stack behaviour is `but unapply`'s own, documented and not negotiable
    /// from here: "if a branch name is provided, the entire stack containing that branch
    /// will be unapplied". It happens to match the board exactly — a lane *is* a stack —
    /// so the unit the CLI acts on and the unit the user selected are the same thing. On a
    /// lane of several stacked branches they are not, which is why the confirmation names
    /// every branch that is about to leave rather than just the tip.
    ///
    /// Nothing is lost: the work becomes an unapplied branch and can be applied again.
    pub fn unapply(&self, identifier: &str) -> Result<()> {
        self.run(&["unapply", identifier, "--json"])?;
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
}
