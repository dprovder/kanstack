//! Claude Code's skill format: a `SKILL.md` with YAML frontmatter, in a directory named for
//! the skill under `.claude/skills/` — the repository's for a project skill, which loads only
//! in sessions in that repository, or `~/.claude/skills/` for a personal one, which loads in
//! every session on the machine.

use std::path::{Path, PathBuf};

use super::{frontmatter, SkillFormat, Stamp};

/// The skill as it sits in kanstack's own repository — which is itself a Claude Code project
/// skill there, loaded by sessions working on kanstack. That copy carries no stamp, so
/// `kanstack skill check` run inside kanstack's own repository reads it as one kanstack
/// didn't write, and never offers to "update" its own source.
pub const SOURCE: &str = include_str!("../../.claude/skills/kanstack/SKILL.md");

pub struct ClaudeSkill;

impl SkillFormat for ClaudeSkill {
    fn harness(&self) -> &'static str {
        "claude"
    }

    fn format_name(&self) -> &'static str {
        "Claude Code"
    }

    fn local_path(&self, repo: &Path) -> PathBuf {
        repo.join(".claude/skills/kanstack/SKILL.md")
    }

    fn global_path(&self, home: &Path) -> PathBuf {
        home.join(".claude/skills/kanstack/SKILL.md")
    }

    fn render(&self, version: &str) -> String {
        frontmatter::stamp(SOURCE, version)
    }

    fn read_stamp(&self, contents: &str) -> Stamp {
        frontmatter::read(contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frontmatter_of(text: &str) -> &str {
        let rest = text.strip_prefix("---\n").expect("opens with frontmatter");
        &rest[..rest.find("\n---\n").expect("frontmatter closes")]
    }

    fn field<'a>(front: &'a str, key: &str) -> Option<&'a str> {
        front.lines().find_map(|l| l.strip_prefix(&format!("{key}: ")))
    }

    /// The source must be stampable: frontmatter first, and no `metadata:` of its own for
    /// `frontmatter::stamp` to collide with.
    #[test]
    fn the_embedded_source_is_stampable_and_unstamped() {
        let front = frontmatter_of(SOURCE);
        assert_eq!(field(front, "name"), Some("kanstack"));
        assert!(!front.lines().any(|l| l.starts_with("metadata:")), "{front}");
        assert_eq!(ClaudeSkill.read_stamp(SOURCE).version, None);
    }

    /// Claude Code truncates `description` and `when_to_use` together at 1,536 characters in
    /// its skill listing; anything past that never reaches the model.
    #[test]
    fn description_and_when_to_use_fit_the_listing_cap() {
        let front = frontmatter_of(SOURCE);
        let description = field(front, "description").expect("description");
        let when_to_use = field(front, "when_to_use").expect("when_to_use");
        assert!(description.len() + when_to_use.len() < 1536, "{}", description.len() + when_to_use.len());
        // Matched before any tool runs, so it can't hinge on what's installed — that's the
        // body's job (its "is kanstack actually here?" check).
        assert!(!description.contains("on PATH") && !when_to_use.contains("on PATH"));
    }

    /// Installed into someone else's repository, "this project" or a bare "issue #9" points at
    /// the wrong project.
    #[test]
    fn the_body_names_kanstack_rather_than_this_project() {
        assert!(!SOURCE.contains("this very project"));
        assert!(SOURCE.contains("https://github.com/dprovder/kanstack/issues/9"));
    }

    #[test]
    fn a_render_is_the_source_plus_a_version_stamp() {
        let rendered = ClaudeSkill.render("4.5.6");
        let front = frontmatter_of(&rendered);
        assert!(front.contains("\nmetadata:\n  version: \"4.5.6\"\n  content-hash: \"fnv1a64:"), "{front}");
        assert_eq!(ClaudeSkill.read_stamp(&rendered), Stamp { version: Some("4.5.6".into()), pristine: true });
        assert!(rendered.ends_with(SOURCE.split_once("\n---\n").unwrap().1));
    }

    #[test]
    fn local_and_global_paths() {
        assert_eq!(ClaudeSkill.local_path(Path::new("/r")), Path::new("/r/.claude/skills/kanstack/SKILL.md"));
        assert_eq!(ClaudeSkill.global_path(Path::new("/h")), Path::new("/h/.claude/skills/kanstack/SKILL.md"));
    }
}
