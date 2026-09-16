use super::*;

impl But {

    /// Creates a new commit on `branch` from `changes` (uncommitted file or hunk ids), with
    /// `message`.
    ///
    /// 0.22 dropped `but stage`/`but rub`: there is no longer a way to mark an uncommitted
    /// change as belonging to a lane without committing it. A file only ever lives in the
    /// single global uncommitted area (`zz`) until it is named directly in a `commit` or
    /// `amend` call, so "move a card onto a lane" now means "commit or amend it there",
    /// immediately — see `App::confirm_move`.
    ///
    /// Mutation commands no longer embed a workspace status in their reply (verified live:
    /// `but commit` now returns a bare `{"commitId":…,"changeId":…}`), so failures are
    /// caught by `run` itself (a structured `CliError` on stdout, or a non-zero exit) and a
    /// full detailed status is always queried separately afterwards.
    ///
    /// A brand-new branch with no commits yet is sometimes reported as "merged upstream"
    /// (reproduced live against 0.22.0: a `branch new` immediately followed by a `commit`
    /// onto it can be refused this way, though the same sequence with a real pause between
    /// the two steps was not observed to fail — the exact trigger wasn't pinned down).
    /// 0.22's real safety net is for *editing already-landed history*, which cannot apply
    /// here — there is no history on the branch yet to disturb — so this retries once with
    /// `--allow-merged` specifically for that error rather than failing a first commit that
    /// is safe by construction.
    pub fn commit(&self, changes: &[String], message: &str, branch: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["commit"];
        args.extend(changes.iter().map(String::as_str));
        args.extend(["-m", message, "-b", branch, "--json"]);
        if let Err(e) = self.run(&args) {
            if e.to_string().contains("merged upstream") {
                args.push("--allow-merged");
                self.run(&args)?;
            } else {
                return Err(e);
            }
        }
        self.status()
    }

    /// Amends `changes` (uncommitted file or hunk ids) into `target` — a branch name (its
    /// tip) or a specific commit id. No message is needed: the target's existing message is
    /// kept.
    pub fn amend(&self, changes: &[String], target: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["amend"];
        args.extend(changes.iter().map(String::as_str));
        args.extend(["-t", target, "--json"]);
        self.run(&args)?;
        self.status()
    }

    /// Moves whole commits onto the top of `branch`'s stack.
    pub fn move_commits(&self, commits: &[String], branch: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["move"];
        args.extend(commits.iter().map(String::as_str));
        args.extend(["-b", branch, "--json"]);
        self.run(&args)?;
        self.status()
    }

    /// Squashes commits into `target`, keeping `target`'s existing message — the same
    /// "combine, don't ask" behaviour `but rub` used to have for a commit-onto-commit move.
    pub fn squash(&self, sources: &[String], target: &str) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["squash"];
        args.extend(sources.iter().map(String::as_str));
        args.extend(["-t", target, "--use-target-message", "--json"]);
        self.run(&args)?;
        self.status()
    }

    /// Uncommits `sources` (commit ids, or `<commit-id>:<file-id>` committed files) back to
    /// the uncommitted area.
    pub fn uncommit(&self, sources: &[String]) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["uncommit"];
        args.extend(sources.iter().map(String::as_str));
        args.push("--json");
        self.run(&args)?;
        self.status()
    }
}
