//! The agent-facing skill kanstack ships, and getting it to where a coding agent will load it
//! — `kanstack skill install` / `kanstack skill check` (see `crate::cli::skill`), modelled on
//! `but skill install` / `but skill check` (wrapped in `crate::but::skill`).
//!
//! The skill text is compiled into the binary (see [`claude::SOURCE`]), so a kanstack binary
//! and the skill it installs are one artifact and can't drift apart. What it writes is
//! stamped with this binary's version, which is what lets `check` tell a stale copy from a
//! current one, and with a hash of what was written, which is what lets it tell a copy
//! kanstack wrote from one a person has since edited — `but skill` overwrites either
//! silently; this never overwrites an edited one without being told to.
//!
//! Where a skill goes and what shape it takes is per harness: one [`SkillFormat`] per harness
//! kanstack knows how to write a skill for, one entry in [`FORMATS`]. Claude Code is the only
//! one today; a harness with no entry gets an explicit "no skill format known" result
//! (see [`unsupported_message`]) rather than a guess. Adding one is a new file here and a new
//! line in [`FORMATS`].

pub mod claude;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

/// This binary's own version — what a freshly written skill is stamped with.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// One harness's skill artifact: where it lives and how its version stamp is written and
/// read back.
pub trait SkillFormat: Sync {
    /// The `crate::harness::Harness::id` this format is for: `claude`.
    fn harness(&self) -> &'static str;

    /// The harness's name as a person would say it, for output: `Claude Code`.
    fn format_name(&self) -> &'static str;

    /// Where the skill goes when installed into the repository rooted at `repo`.
    fn local_path(&self, repo: &Path) -> PathBuf;

    /// Where the skill goes when installed for the user whose home directory is `home`.
    fn global_path(&self, home: &Path) -> PathBuf;

    /// The full file to write, stamped with `version`.
    fn render(&self, version: &str) -> String;

    /// The stamp on an already-installed copy.
    fn read_stamp(&self, contents: &str) -> Stamp;
}

/// What an installed copy says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// The kanstack version that wrote it — `None` when it carries no stamp at all, i.e.
    /// kanstack didn't write it (kanstack's own source copy, or one copied by hand).
    pub version: Option<String>,
    /// Whether it's byte-for-byte what that version wrote. `false` for an unstamped copy too:
    /// either way it's not kanstack's to overwrite unasked.
    pub pristine: bool,
}

/// Every harness kanstack can write a skill for.
pub const FORMATS: &[&dyn SkillFormat] = &[&claude::ClaudeSkill];

/// The format for harness `id` (a `crate::harness::Harness::id`), if kanstack knows one.
pub fn format_for(id: &str) -> Option<&'static dyn SkillFormat> {
    FORMATS.iter().copied().find(|f| f.harness() == id)
}

/// What to say about a harness kanstack has no [`SkillFormat`] for.
pub fn unsupported_message(harness: &str) -> String {
    format!("no skill format known for {harness} yet")
}

/// Which copy: the one in a repository, or the one in the user's home directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Local,
    Global,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Scope::Local => "local",
            Scope::Global => "global",
        })
    }
}

/// The repository `cwd` is in — the nearest ancestor with a `.git`, same rule
/// `crate::workstream` keys its registry by. `None` outside one.
pub fn repo_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find(|dir| dir.join(".git").exists()).map(Path::to_path_buf)
}

/// `$HOME`, or `None` when it's unset or empty.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// What's at a skill's path right now, relative to what this binary would write there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Existing {
    Missing,
    /// Written by this very version, untouched since.
    Current,
    /// Written by another version (older or newer), untouched since.
    OtherVersion(String),
    /// Edited since kanstack wrote it, or never written by kanstack at all.
    Modified(Option<String>),
}

/// One copy of the skill: which harness's, which scope, where, and what's there now.
#[derive(Clone)]
pub struct Target {
    pub format: &'static dyn SkillFormat,
    pub scope: Scope,
    pub path: PathBuf,
    pub existing: Existing,
}

impl Target {
    /// Looks at `path` to see what's there.
    pub fn inspect(format: &'static dyn SkillFormat, scope: Scope, path: PathBuf) -> Result<Target> {
        let existing = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                let stamp = format.read_stamp(&contents);
                match stamp.version {
                    Some(v) if stamp.pristine && v == VERSION => Existing::Current,
                    Some(v) if stamp.pristine => Existing::OtherVersion(v),
                    v => Existing::Modified(v),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Existing::Missing,
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        Ok(Target { format, scope, path, existing })
    }

    pub fn installed_version(&self) -> Option<&str> {
        match &self.existing {
            Existing::Missing => None,
            Existing::Current => Some(VERSION),
            Existing::OtherVersion(v) => Some(v),
            Existing::Modified(v) => v.as_deref(),
        }
    }

    /// Whether this copy is older than this binary — what `check --update` refreshes. A
    /// copy from a *newer* kanstack is left alone: refreshing it would be a downgrade.
    pub fn is_behind(&self) -> bool {
        self.installed_version().is_some_and(|v| version_lt(v, VERSION))
    }

    /// Writes this binary's skill to [`Self::path`], creating its directory.
    pub fn write(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
        }
        std::fs::write(&self.path, self.format.render(VERSION))
            .with_context(|| format!("could not write {}", self.path.display()))
    }
}

/// Whether dotted version `a` sorts before `b`, numerically per component. Anything after a
/// `-` or `+` (pre-release, build metadata) is ignored, and a component that isn't a number
/// counts as `0` — this only ever compares versions kanstack itself stamped.
fn version_lt(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        let core = v.split(['-', '+']).next().unwrap_or(v);
        core.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (mut a, mut b) = (parts(a), parts(b));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a < b
}

/// FNV-1a, 64-bit — a change detector for a file kanstack wrote, not anything
/// security-relevant, so it needn't be more than stable across builds (which std's own
/// `DefaultHasher` doesn't promise).
fn fnv1a64(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Stamping for a Markdown file with a YAML frontmatter block (`---` ... `---`): a
/// `metadata:` map appended to the end of the frontmatter, carrying `version` and a
/// `content-hash` of the rest of the file. `metadata` because it's the one free-form map
/// Claude Code documents as accepted and not acted on — and the Agent Skills spec allows it
/// too, where it rejects an unknown top-level key like the `version:` `but` stamps with.
pub mod frontmatter {
    use super::fnv1a64;

    const OPEN: &str = "---\n";
    const CLOSE: &str = "\n---\n";

    fn stamp_lines(version: &str, hash: &str) -> String {
        format!("metadata:\n  version: \"{version}\"\n  content-hash: \"{hash}\"\n")
    }

    /// Covers the version too, so a copy whose stamp was bumped by hand reads as edited
    /// rather than as current.
    fn hash_of(version: &str, unstamped: &str) -> String {
        format!("fnv1a64:{:016x}", fnv1a64(&format!("{version}\n{unstamped}")))
    }

    /// `source` with its frontmatter stamped. `source` must open with a frontmatter block
    /// that has no `metadata:` of its own — checked by the tests against the embedded skill,
    /// the only thing this is ever handed.
    pub fn stamp(source: &str, version: &str) -> String {
        let end = source.find(CLOSE).expect("skill source has frontmatter") + 1;
        format!("{}{}{}", &source[..end], stamp_lines(version, &hash_of(version, source)), &source[end..])
    }

    /// The stamp [`stamp`] wrote into `contents`, and whether the rest of `contents` still
    /// hashes to what it did then. Any change at all — including to the stamp's own lines —
    /// reads as not pristine.
    pub fn read(contents: &str) -> super::Stamp {
        let not_ours = super::Stamp { version: None, pristine: false };
        let Some(front_end) = contents.strip_prefix(OPEN).and_then(|rest| rest.find(CLOSE)).map(|i| i + OPEN.len() + 1)
        else {
            return not_ours;
        };
        let front = &contents[..front_end];
        let Some(start) = front.find("\nmetadata:\n").map(|i| i + 1) else {
            return not_ours;
        };
        let mut lines = front[start..].lines().skip(1);
        let field = |line: Option<&str>, key: &str| {
            line.and_then(|l| l.strip_prefix(&format!("  {key}: \"")))
                .and_then(|v| v.strip_suffix('"'))
                .map(str::to_string)
        };
        let (Some(version), Some(hash)) = (field(lines.next(), "version"), field(lines.next(), "content-hash")) else {
            return not_ours;
        };
        let written = stamp_lines(&version, &hash);
        let pristine = front[start..].starts_with(&written) && {
            let unstamped = format!("{}{}", &contents[..start], &contents[start + written.len()..]);
            hash_of(&version, &unstamped) == hash
        };
        super::Stamp { version: Some(version), pristine }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(version_lt("0.0.3", "0.0.10"));
        assert!(version_lt("0.9.0", "1.0.0"));
        assert!(version_lt("1.2", "1.2.1"));
        assert!(!version_lt("1.2.0", "1.2"));
        assert!(!version_lt("0.1.0", "0.0.9"));
        assert!(!version_lt("0.2.0-rc.1", "0.2.0"), "pre-release suffix is ignored");
    }

    #[test]
    fn only_claude_has_a_format_and_every_format_names_a_known_harness() {
        assert_eq!(FORMATS.iter().map(|f| f.harness()).collect::<Vec<_>>(), ["claude"]);
        for f in FORMATS {
            assert!(
                crate::harness::KNOWN.iter().any(|h| h.id() == f.harness()),
                "{} is not a crate::harness::KNOWN id",
                f.harness()
            );
        }
        for id in ["codex", "gemini", "kiro", "opencode", "pi"] {
            assert!(format_for(id).is_none(), "{id}");
        }
    }

    #[test]
    fn a_stamp_reads_back_and_any_edit_breaks_it() {
        let source = "---\nname: x\ndescription: \"y\"\n---\n\n# Body\n";
        let stamped = frontmatter::stamp(source, "1.2.3");
        assert!(stamped.starts_with("---\nname: x\ndescription: \"y\"\nmetadata:\n  version: \"1.2.3\"\n"), "{stamped}");
        assert!(stamped.ends_with("---\n\n# Body\n"), "{stamped}");
        assert_eq!(frontmatter::read(&stamped), Stamp { version: Some("1.2.3".into()), pristine: true });

        let body_edit = stamped.replace("# Body", "# Body, edited");
        assert_eq!(frontmatter::read(&body_edit), Stamp { version: Some("1.2.3".into()), pristine: false });
        // Bumping the version by hand to dodge `check` still reads as an edit.
        let version_edit = stamped.replace("1.2.3", "9.9.9");
        assert_eq!(frontmatter::read(&version_edit), Stamp { version: Some("9.9.9".into()), pristine: false });

        assert_eq!(frontmatter::read(source), Stamp { version: None, pristine: false });
        assert_eq!(frontmatter::read("no frontmatter at all"), Stamp { version: None, pristine: false });
    }

    #[test]
    fn inspecting_a_path_classifies_what_is_there() {
        let dir = std::env::temp_dir().join(format!("kanstack-skill-inspect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("SKILL.md");
        let f = format_for("claude").unwrap();
        let inspect = || Target::inspect(f, Scope::Local, path.clone()).unwrap();

        assert_eq!(inspect().existing, Existing::Missing);
        inspect().write().unwrap();
        assert_eq!(inspect().existing, Existing::Current);
        assert!(!inspect().is_behind());

        std::fs::write(&path, f.render("0.0.0")).unwrap();
        assert_eq!(inspect().existing, Existing::OtherVersion("0.0.0".into()));
        assert!(inspect().is_behind());

        std::fs::write(&path, f.render("999.0.0")).unwrap();
        assert!(!inspect().is_behind(), "a newer copy is never a downgrade target");

        std::fs::write(&path, claude::SOURCE).unwrap();
        assert_eq!(inspect().existing, Existing::Modified(None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
