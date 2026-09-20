//! What kanstack knows about each coding-agent harness, in one place.
//!
//! A harness is the program typed into a fresh pane (`claude`, `codex`, ...). Everything
//! specific to *which* one it is lives behind [`Harness`]: today that is how the branch
//! context note reaches it (see [`NoteDelivery`]), and it is where anything else that varies
//! per harness belongs — one implementor per harness, one entry in [`KNOWN`].
//!
//! The split backends (`crate::cmux`, `crate::tmux`, `crate::orca`) know nothing about any
//! of this: [`HarnessConfig::launch_line`] turns a harness choice into the finished line,
//! and a backend just types it.

use std::path::Path;

use anyhow::Result;

use crate::harness_launch::{launch_line, NoteDelivery};

/// One coding-agent harness. Unit structs, looked up by [`for_command`].
pub trait Harness: Sync {
    /// The program's own name, as it would be typed at a shell: `claude`. What
    /// [`for_command`] matches against, and what `--setup` looks for on `PATH`.
    fn id(&self) -> &'static str;

    /// Other names the same program answers to (`kiro-cli` for `kiro`).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// How the branch-context note (see `harness_launch::branch_context_note`) reaches this
    /// harness when nothing overrides it. Defaults to the one delivery every harness
    /// supports: folding the note into the initial message.
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::FoldIntoMessage
    }
}

/// Confirmed `--append-system-prompt <text>`, additive to (not replacing) its own default
/// system prompt.
struct Claude;
impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::Flag("--append-system-prompt".to_string())
    }
}

/// No `--append-system-prompt` exists (a request for exactly that, openai/codex#11117, is
/// closed unimplemented); `-c developer_instructions=<toml>` is the closest equivalent, a
/// differently-*shaped* mechanism (TOML-quoted, and a "developer" message rather than
/// literally the system prompt), not just a different flag name.
struct Codex;
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::CodexConfig
    }
}

/// Same confirmed `--append-system-prompt <text>` as Claude.
struct Pi;
impl Harness for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::Flag("--append-system-prompt".to_string())
    }
}

/// No such flag (a feature request for exactly this, anomalyco/opencode#16089, was closed
/// as not planned), so the note is folded into the initial message.
struct OpenCode;
impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
}

/// `kiro-cli chat`'s full flag reference has nothing for system prompt, instructions or
/// context (only pre-configured, not-dynamic-per-invocation agents via `--agent`), so the
/// note is folded into the initial message.
struct Kiro;
impl Harness for Kiro {
    fn id(&self) -> &'static str {
        "kiro"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["kiro-cli"]
    }
}

/// Its only mechanism, `GEMINI_SYSTEM_MD`, is an env var pointing at a file that *fully
/// replaces* the default system prompt rather than appending to it — using it would strip
/// Gemini CLI's own built-in behavior instructions, worse than sending nothing. So the note
/// is folded into the initial message.
struct Gemini;
impl Harness for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
}

/// Anything not in [`KNOWN`]: a wrapper script, a harness kanstack hasn't heard of. Gets
/// the delivery that works everywhere.
struct Generic;
impl Harness for Generic {
    fn id(&self) -> &'static str {
        "generic"
    }
}

/// Every harness kanstack recognizes by name, in the order `--setup` prefers them when
/// several are installed.
pub const KNOWN: &[&dyn Harness] = &[&Claude, &Codex, &Pi, &OpenCode, &Kiro, &Gemini];

static GENERIC: Generic = Generic;

/// The harness `command` runs, matched on the basename of its first word — so `claude
/// --dangerously-skip-permissions` baked into `KANSTACK_HARNESS`, or `/usr/local/bin/claude`,
/// still resolve the same as plain `claude`. [`Generic`] when it isn't recognized.
pub fn for_command(command: &str) -> &'static dyn Harness {
    let program = command.split_whitespace().next().unwrap_or(command);
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    KNOWN
        .iter()
        .copied()
        .find(|h| h.id() == name || h.aliases().contains(&name))
        .unwrap_or(&GENERIC)
}

/// The `NoteDelivery` for `harness`: from `KANSTACK_HARNESS_SYSTEM_FLAG` if set — including
/// explicitly to `""`, to opt out entirely — otherwise whatever's known-good for the
/// harness itself (see [`Harness::note_delivery`]).
pub fn resolve_note_delivery(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => return NoteDelivery::Disabled,
        Ok(flag) => return NoteDelivery::Flag(flag),
        Err(_) => {}
    }
    for_command(harness).note_delivery()
}

/// [`resolve_note_delivery`] for a harness other than the configured one
/// (`kanstack spawn --agent`): `KANSTACK_HARNESS_SYSTEM_FLAG` describes the configured
/// harness, so a flag set there isn't applied to a different one — it would be an unknown
/// option to it. Only the explicit opt-out (`""`) carries over.
pub fn resolve_note_delivery_for_override(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => NoteDelivery::Disabled,
        _ => for_command(harness).note_delivery(),
    }
}

/// The harness a [`crate::splitter::Splitter`] launches into new panes: the command as
/// typed (`claude`, `codex --yolo`) and how its note is delivered, resolved once.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    command: String,
    note_delivery: NoteDelivery,
}

impl HarnessConfig {
    /// `command`, with its note delivery resolved from the environment.
    pub fn new(command: impl Into<String>) -> Self {
        let command = command.into();
        let note_delivery = resolve_note_delivery(&command);
        HarnessConfig { command, note_delivery }
    }

    /// `$KANSTACK_HARNESS`, or `claude`.
    pub fn from_env() -> Self {
        Self::new(std::env::var("KANSTACK_HARNESS").unwrap_or_else(|_| "claude".to_string()))
    }

    /// The command typed into a new pane when nothing overrides it.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The line to type into a fresh terminal so it `cd`s into `cwd` and starts the harness
    /// on lane `name`, with `initial_message` as its first message. `harness` overrides the
    /// configured command for just this pane (`kanstack spawn --agent`), including which
    /// delivery the note uses — that depends on the harness, not on what was configured.
    ///
    /// No trailing newline; the backend submits it. See `harness_launch::launch_line` for
    /// the spill-to-files handling of a prompt too long to type.
    pub fn launch_line(
        &self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
    ) -> Result<String> {
        let (command, delivery) = match harness {
            Some(h) if h != self.command => (h, resolve_note_delivery_for_override(h)),
            Some(_) | None => (self.command.as_str(), self.note_delivery.clone()),
        };
        let line = launch_line(cwd, command, &delivery, name, initial_message)?;
        Ok(line.trim_end_matches('\n').to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every `with_system_flag_env` call against every other one — `cargo
    /// test` runs tests concurrently on separate threads by default, but
    /// `std::env::set_var`/`remove_var` mutate process-wide state, so two of these
    /// running at once could see (or clobber) each other's value mid-test without this.
    static SYSTEM_FLAG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `body` with `KANSTACK_HARNESS_SYSTEM_FLAG` swapped out and restored
    /// afterwards, so this doesn't leak into other tests running in the same process.
    fn with_system_flag_env(value: Option<&str>, body: impl FnOnce()) {
        // Held for the whole call, not just the swap, so `body` (which reads the var via
        // `resolve_note_delivery`) can't be interleaved with another test's swap either.
        let _guard = SYSTEM_FLAG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("KANSTACK_HARNESS_SYSTEM_FLAG");
        match value {
            Some(v) => std::env::set_var("KANSTACK_HARNESS_SYSTEM_FLAG", v),
            None => std::env::remove_var("KANSTACK_HARNESS_SYSTEM_FLAG"),
        }
        body();
        match old {
            Some(v) => std::env::set_var("KANSTACK_HARNESS_SYSTEM_FLAG", v),
            None => std::env::remove_var("KANSTACK_HARNESS_SYSTEM_FLAG"),
        }
    }

    #[test]
    fn known_harnesses_have_distinct_names_and_keep_the_setup_priority_order() {
        let ids: Vec<_> = KNOWN.iter().map(|h| h.id()).collect();
        assert_eq!(ids, ["claude", "codex", "pi", "opencode", "kiro", "gemini"]);
        let mut every_name: Vec<_> = KNOWN.iter().flat_map(|h| [h.id()].into_iter().chain(h.aliases().iter().copied())).collect();
        let total = every_name.len();
        every_name.sort_unstable();
        every_name.dedup();
        assert_eq!(every_name.len(), total, "two harnesses claim the same name");
    }

    #[test]
    fn for_command_matches_the_program_by_basename_ignoring_its_own_flags() {
        assert_eq!(for_command("claude").id(), "claude");
        assert_eq!(for_command("claude --dangerously-skip-permissions").id(), "claude");
        assert_eq!(for_command("/opt/homebrew/bin/codex").id(), "codex");
        assert_eq!(for_command("kiro-cli").id(), "kiro");
        assert_eq!(for_command("./my-custom-harness.sh").id(), "generic");
        assert_eq!(for_command("").id(), "generic");
    }

    #[test]
    fn resolve_note_delivery_covers_claude_and_pi() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
            assert_eq!(
                resolve_note_delivery("pi"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    #[test]
    fn resolve_note_delivery_uses_codex_config_for_codex() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("codex"), NoteDelivery::CodexConfig);
        });
    }

    /// Extra flags baked into `KANSTACK_HARNESS` (e.g. to skip a permissions prompt)
    /// shouldn't stop the harness itself from being recognized.
    #[test]
    fn resolve_note_delivery_matches_the_program_ignoring_its_own_flags() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude --dangerously-skip-permissions"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    /// Harnesses with no confirmed mechanism (including OpenCode, and anything unknown)
    /// fall back to folding the note into the message rather than guessing at a flag
    /// that might not exist for them.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_unconfirmed_harnesses() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("opencode"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro-cli"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("gemini"), NoteDelivery::FoldIntoMessage);
            // Genuinely unrecognized commands get the same safe fallback.
            assert_eq!(resolve_note_delivery("./my-custom-harness.sh"), NoteDelivery::FoldIntoMessage);
        });
    }

    #[test]
    fn resolve_note_delivery_honors_an_explicit_flag_override() {
        with_system_flag_env(Some("--my-custom-flag"), || {
            assert_eq!(
                resolve_note_delivery("codex"),
                NoteDelivery::Flag("--my-custom-flag".to_string())
            );
        });
    }

    #[test]
    fn resolve_note_delivery_is_disabled_by_an_empty_override() {
        with_system_flag_env(Some(""), || {
            assert_eq!(resolve_note_delivery("claude"), NoteDelivery::Disabled);
        });
    }

    /// A different harness for one pane brings its own delivery, and the configured
    /// harness's `KANSTACK_HARNESS_SYSTEM_FLAG` — meaningless to it — is left behind.
    #[test]
    fn an_overriding_harness_gets_its_own_delivery_not_the_configured_flag() {
        with_system_flag_env(Some("--my-custom-flag"), || {
            assert_eq!(resolve_note_delivery_for_override("codex"), NoteDelivery::CodexConfig);
        });
        with_system_flag_env(Some(""), || {
            assert_eq!(resolve_note_delivery_for_override("codex"), NoteDelivery::Disabled);
        });
    }

    #[test]
    fn launch_line_uses_the_configured_harness_unless_overridden() {
        with_system_flag_env(None, || {
            let config = HarnessConfig::new("claude");
            let cwd = Path::new("/repo");

            let configured = config.launch_line(cwd, "feat-x", Some("fix it"), None).unwrap();
            assert!(configured.starts_with("cd '/repo' && claude --append-system-prompt "), "{configured:?}");
            assert!(!configured.ends_with('\n'), "the backend submits the line, so it carries no newline");

            // Naming the configured harness is not an override.
            assert_eq!(config.launch_line(cwd, "feat-x", Some("fix it"), Some("claude")).unwrap(), configured);

            let other = config.launch_line(cwd, "feat-x", Some("fix it"), Some("codex")).unwrap();
            assert!(other.starts_with("cd '/repo' && codex -c "), "{other:?}");
            assert!(!other.contains("--append-system-prompt"), "{other:?}");
        });
    }
}
