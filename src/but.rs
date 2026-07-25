//! Subprocess client for the `but` CLI.
//!
//! This is deliberately the *only* coupling to GitButler: we spawn a `but` binary the
//! user installed themselves and speak its documented JSON format. We link none of its
//! code, so this project stays independently licensed (see README).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

use crate::model::{CliError, MutationEnvelope, WorkspaceStatus};

/// Oldest `but` whose JSON shape this was verified against.
pub const MIN_VERSION: Version = Version {
    major: 0,
    minor: 19,
    patch: 0,
};
/// Newest `but` actually exercised while building this. Newer is allowed but noted.
pub const VERIFIED_THROUGH: Version = Version {
    major: 0,
    minor: 19,
    patch: 3,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Version {
    /// Parses the `but --version` line, e.g. `but 0.19.3`.
    pub fn parse(s: &str) -> Result<Version> {
        let token = s
            .split_whitespace()
            .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .ok_or_else(|| anyhow!("no version number in {s:?}"))?;
        // Tolerate pre-release/build suffixes like `0.21.2-nightly.3`.
        let core = token.split(['-', '+']).next().unwrap_or(token);
        let mut parts = core.split('.');
        let mut next = |what: &str| -> Result<u32> {
            parts
                .next()
                .ok_or_else(|| anyhow!("missing {what} in version {token:?}"))?
                .parse()
                .with_context(|| format!("bad {what} in version {token:?}"))
        };
        Ok(Version {
            major: next("major")?,
            minor: next("minor")?,
            patch: next("patch").unwrap_or(0),
        })
    }
}

pub struct But {
    bin: PathBuf,
    cwd: PathBuf,
    version: Version,
}

impl But {
    /// Locates `but`, checks its version, and pins the working directory.
    ///
    /// The version gate exists because the JSON contract is stable *by intent* but is not
    /// versioned in the payload and its types are `pub(crate)` upstream — so there is no
    /// semver promise to lean on. Better to refuse than to mis-render.
    pub fn discover(cwd: &Path) -> Result<Self> {
        let bin = std::env::var_os("KANSTACK_BUT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("but"));

        let out = Command::new(&bin)
            .arg("--version")
            .output()
            .with_context(|| {
                format!(
                    "could not run `{}`. Install the GitButler CLI, or set KANSTACK_BUT_BIN \
                     to its path.",
                    bin.display()
                )
            })?;
        if !out.status.success() {
            bail!("`{} --version` failed", bin.display());
        }
        let version = Version::parse(&String::from_utf8_lossy(&out.stdout))?;
        if version < MIN_VERSION {
            bail!(
                "`but` {version} is too old; this needs at least {MIN_VERSION}. \
                 Run `but update install`."
            );
        }

        Ok(But {
            bin,
            cwd: cwd.to_path_buf(),
            version,
        })
    }

    pub fn version(&self) -> Version {
        self.version
    }

    /// True when running against a `but` newer than anything this was tested with.
    /// Worth surfacing in the UI, but not worth refusing to start over.
    pub fn is_untested_version(&self) -> bool {
        self.version > VERIFIED_THROUGH
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.bin)
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .with_context(|| format!("failed to spawn `but {}`", args.join(" ")))?;

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();

        // In --json mode `but` reports failures as a structured object on stdout. Prefer
        // that over the exit code, since it carries a usable message and hint.
        if let Ok(err) = serde_json::from_str::<CliError>(stdout.trim()) {
            let mut msg = format!("{}: {}", err.error, err.message);
            if let Some(hint) = err.hint {
                msg.push_str(&format!("\nhint: {hint}"));
            }
            bail!(msg);
        }

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim();
            bail!(
                "`but {}` failed{}",
                args.join(" "),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        Ok(stdout)
    }

    /// Reads the whole workspace. `-f` includes per-commit file lists and costs nothing
    /// measurable, so it is always on.
    pub fn status(&self) -> Result<WorkspaceStatus> {
        let raw = self.run(&["status", "-f", "-j"])?;
        parse_status(&raw)
    }

    /// Runs `but rub SOURCE TARGET`, the CLI's combine primitive, and returns the
    /// refreshed workspace from the same invocation.
    ///
    /// `--status-after` is what keeps a card move to a single round trip instead of a
    /// mutation followed by a separate refresh.
    pub fn rub(&self, source: &str, target: &str) -> Result<WorkspaceStatus> {
        let raw = self.run(&["rub", source, target, "-j", "--status-after"])?;
        let env: MutationEnvelope = serde_json::from_str(raw.trim())
            .with_context(|| format!("could not parse `but rub` output: {raw:.400}"))?;
        if let Some(err) = env.status_error {
            bail!("rub succeeded but the workspace refresh failed: {err}");
        }
        env.status
            .ok_or_else(|| anyhow!("`but rub` returned no status payload"))
    }
}

/// Split out so it can be tested against captured output without spawning anything.
pub fn parse_status(raw: &str) -> Result<WorkspaceStatus> {
    serde_json::from_str(raw.trim())
        .with_context(|| format!("could not parse `but status` output: {raw:.400}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_version_strings() {
        assert_eq!(
            Version::parse("but 0.19.3").unwrap(),
            Version {
                major: 0,
                minor: 19,
                patch: 3
            }
        );
        assert_eq!(
            Version::parse("but 0.21.2\n").unwrap(),
            Version {
                major: 0,
                minor: 21,
                patch: 2
            }
        );
        // Pre-release suffixes must not break the gate.
        assert_eq!(
            Version::parse("but 1.0.0-nightly.4").unwrap(),
            Version {
                major: 1,
                minor: 0,
                patch: 0
            }
        );
        assert!(Version::parse("but unknown").is_err());
    }

    #[test]
    fn version_ordering_drives_the_gate() {
        assert!(Version::parse("but 0.18.9").unwrap() < MIN_VERSION);
        assert!(Version::parse("but 0.19.0").unwrap() >= MIN_VERSION);
        assert!(Version::parse("but 0.21.2").unwrap() > VERIFIED_THROUGH);
    }

    #[test]
    fn parses_captured_status() {
        let raw = include_str!("../tests/fixtures/status.json");
        let s = parse_status(raw).unwrap();
        assert_eq!(s.stacks.len(), 3);
    }
}
