use super::*;

impl But {
    /// Turns a plain git repo (or no repo at all) into a GitButler project. `--init`
    /// creates a fresh repo + empty commit only when one doesn't already exist — confirmed
    /// idempotent against an existing repo (no extra commit beyond GitButler's own), so
    /// this is always safe to call regardless of which of the two `is_setup_required`
    /// cases triggered it.
    ///
    /// This is the one call whose spelling is known to change after
    /// [`VERIFIED_THROUGH`] (see `docs/ARCHITECTURE.md`, "Setup after 0.22.3"), so a
    /// failure on a newer `but` is reworded by [`explain_setup_failure`] rather than
    /// passed through raw.
    pub fn run_setup(&self) -> Result<()> {
        self.run(&["setup", "--init", "--json"])
            .map_err(|e| explain_setup_failure(self.version, e))?;
        Ok(())
    }
}

/// Says plainly that kanstack doesn't know how this `but` sets a repo up, instead of
/// surfacing a raw error that reads like the user did something wrong.
///
/// Only reworded past [`VERIFIED_THROUGH`]: within the verified range a setup failure is
/// a real failure of a known command (no git identity, say) and `but`'s own text is the
/// right thing to show. `but`'s text is kept either way, so callers matching on it — like
/// [`is_missing_git_identity`] — still see it.
fn explain_setup_failure(version: Version, err: anyhow::Error) -> anyhow::Error {
    if version <= VERIFIED_THROUGH {
        return err;
    }
    anyhow!(
        "kanstack doesn't know how `but` {version} sets up a GitButler workspace — it is \
         verified through {VERIFIED_THROUGH}, where that is `but setup --init`, and that \
         failed here. Set the workspace up with `but` directly, then reopen kanstack.\n\
         `but` said: {err}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const V0_22_3: Version = Version { major: 0, minor: 22, patch: 3 };
    const V0_23_0: Version = Version { major: 0, minor: 23, patch: 0 };

    /// What a master build of `but` (self-reporting 0.23.0) printed for
    /// `but setup --init --json`, verbatim from stderr — exit 1, nothing on stdout.
    const MASTER_SETUP_ERROR: &str = "`but setup --init --json` failed: Error: You no longer \
        need to run `but setup`, you can just start using `but` with any Git repository.\n\n\
        Hint: Run `but init` instead to initialize a repository.";

    #[test]
    fn verified_versions_pass_setup_failures_through_untouched() {
        for version in [MIN_VERSION, V0_22_3] {
            let raw = "`but setup --init --json` failed: Error: Author identity is not configured";
            let msg = explain_setup_failure(version, anyhow!(raw)).to_string();
            assert_eq!(msg, raw, "{version} is verified, so its error is already the right one");
        }
    }

    #[test]
    fn a_newer_but_says_its_setup_path_is_unknown_and_keeps_buts_own_words() {
        let msg = explain_setup_failure(V0_23_0, anyhow!(MASTER_SETUP_ERROR)).to_string();
        assert!(msg.contains("doesn't know how `but` 0.23.0 sets up"), "{msg}");
        assert!(msg.contains("verified through 0.22.3"), "{msg}");
        assert!(msg.contains(MASTER_SETUP_ERROR), "`but`'s text must survive: {msg}");
    }

    /// The recovery screen picks its hint by matching `but`'s text, so rewording must not
    /// hide it — a newer `but` that still fails on identity should still get that hint.
    #[test]
    fn rewording_keeps_the_identity_hint_reachable() {
        let raw = "`but setup --init --json` failed: Error: Author identity is not configured";
        let msg = explain_setup_failure(V0_23_0, anyhow!(raw)).to_string();
        assert!(is_missing_git_identity(&msg), "{msg}");
    }
}
