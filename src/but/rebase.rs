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

    /// Where the workspace commit really sits relative to the upstream tip, read from Git.
    ///
    /// `but status` cannot say. Its `mergeBase` is GitButler's *recorded* target, and
    /// nothing else in its JSON carries a base — stacks list commits without parents, and
    /// an empty workspace lists nothing at all. A land that empties the workspace was
    /// observed (0.22.0) to advance the recorded target while leaving the workspace commit
    /// on the old base, so `mergeBase` then equalled the upstream tip while every lane was
    /// twelve commits behind it. `but pull --check` reports the same recorded value as
    /// `baseSha`. Only the commit graph knows, so ask it: `HEAD` is the workspace commit,
    /// and `upstream_tip` is `upstreamState.latestCommit`, the tip `behind` is counted to.
    pub fn workspace_position(&self, upstream_tip: &str) -> Result<WorkspacePosition> {
        let base = self.git(&["merge-base", "HEAD", upstream_tip])?;
        let behind = self.git(&["rev-list", "--count", &format!("HEAD..{upstream_tip}")])?;
        Ok(WorkspacePosition {
            base: base.trim().to_string(),
            behind: behind
                .trim()
                .parse()
                .with_context(|| format!("unexpected `git rev-list --count` output: {behind:?}"))?,
        })
    }
}

/// See [`But::workspace_position`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePosition {
    /// Full id of the newest commit the workspace and the upstream tip share.
    pub base: String,
    /// Commits on the upstream tip that the workspace does not contain.
    pub behind: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The state that lied: upstream has moved three commits past where the
    /// workspace commit sits, with nothing stacked on it. Whatever `but status` calls the
    /// merge base, the graph says the workspace is on `old` and three behind.
    #[test]
    fn an_empty_workspace_left_on_an_old_base_is_reported_as_behind() {
        let dir = std::env::temp_dir().join(format!("kanstack-position-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q", "."]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "T"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "old"]);
        let old = git(&dir, &["rev-parse", "HEAD"]);
        // The workspace commit, parented on the old base.
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "workspace"]);
        // Upstream continues from the old base, not from the workspace commit.
        git(&dir, &["checkout", "-q", "-b", "upstream", &old]);
        for n in 1..=3 {
            git(&dir, &["commit", "-q", "--allow-empty", "-m", &format!("up {n}")]);
        }
        let tip = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &["checkout", "-q", "-"]);

        let but = But { bin: PathBuf::from("but"), cwd: dir.clone(), version: MIN_VERSION };
        let pos = but.workspace_position(&tip).unwrap();
        assert_eq!(pos, WorkspacePosition { base: old, behind: 3 });

        // Once the workspace contains the tip it is level, and the base is the tip itself.
        git(&dir, &["merge", "-q", "--no-edit", &tip]);
        let pos = but.workspace_position(&tip).unwrap();
        assert_eq!(pos, WorkspacePosition { base: tip, behind: 0 });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
