use super::*;

/// What the PR modal gathered, for `But::pr_new`.
pub struct PrOptions {
    /// Review title (and, past the first line, description) — see `but pr new --help`'s
    /// `-m`. `None` when the modal's title field was left empty, in which case
    /// `pr_new` passes `--default` instead so the review still gets opened, using the
    /// branch's own commit message.
    pub title: Option<String>,
    pub draft: bool,
}

impl But {

    /// Opens a review (a GitHub PR, a GitLab MR, whatever the configured forge calls it)
    /// for `branch`, pushing it first if it needs pushing — the same as `but push` does
    /// for a first-time push, so there is no separate "push, then PR" step to sequence.
    ///
    /// `-m`/`--default` are the two ways to skip `but pr new`'s own interactive prompt,
    /// which this client's subprocess has no stdin to answer: `-m <title>` when the modal's
    /// title field has something in it, `--default` (falls back to the branch's own commit
    /// message) when it doesn't. Doesn't fold a status into its reply (mirrors `push`), so
    /// the caller refreshes separately — that refresh is also what picks up the branch's new
    /// `review_id`.
    pub fn pr_new(&self, branch: &str, opts: &PrOptions) -> Result<WorkspaceStatus> {
        let mut args: Vec<&str> = vec!["pr", "new", branch, "--json"];
        match &opts.title {
            Some(title) => {
                args.push("-m");
                args.push(title);
            }
            None => args.push("--default"),
        }
        if opts.draft {
            args.push("-d");
        }
        self.run(&args)?;
        self.status()
    }
}
