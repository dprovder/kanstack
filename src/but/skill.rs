use super::*;

impl But {

    /// Whether a coding-agent skill file teaching `but` usage is installed anywhere (local
    /// repo and/or global home directory) and current for this `but` version — see
    /// `crate::setup`'s "install/update GitButler skill" action.
    pub fn skill_check(&self) -> Result<SkillCheck> {
        let raw = self.run(&["skill", "check", "--json"])?;
        parse_skill_check(&raw)
    }

    /// Installs the skill fresh. In non-interactive mode (no tty on `self.run`'s stdin —
    /// always true here, `but` is only ever spawned with piped/captured output) `but`
    /// itself picks the format for whichever coding agent it detects installed, rather
    /// than prompting.
    pub fn skill_install_global(&self) -> Result<()> {
        self.run(&["skill", "install", "--global", "--json"])?;
        Ok(())
    }

    /// Refreshes every already-installed skill (local and global) that's behind the
    /// current `but` version, in place.
    pub fn skill_update(&self) -> Result<()> {
        self.run(&["skill", "check", "--update", "--json"])?;
        Ok(())
    }
}

/// Split out, same as `parse_status`, so it can be tested against captured output without
/// spawning anything.
fn parse_skill_check(raw: &str) -> Result<SkillCheck> {
    serde_json::from_str(raw.trim())
        .with_context(|| format!("could not parse `but skill check` output: {raw:.400}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_captured_skill_check() {
        let raw = include_str!("../../tests/fixtures/skill_check.json");
        let check = parse_skill_check(raw).unwrap();
        assert_eq!(check.skills.len(), 1);
        assert_eq!(check.skills[0].format_name, "Claude Code");
        assert_eq!(check.skills[0].scope, "global");
        assert!(check.skills[0].up_to_date);
    }
}
