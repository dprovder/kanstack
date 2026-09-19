//! The harness launch line itself — building it, and working out how the branch-context
//! note (see [`branch_context_note`]) reaches the harness — shared by every split backend
//! (`crate::cmux`, `crate::tmux`), since typing a command into a fresh terminal is the one
//! part of this that has nothing to do with which multiplexer opened the pane.

use std::path::Path;

/// How the branch-context note (see [`branch_context_note`]) reaches a harness's launch
/// command — resolved once, in [`resolve_note_delivery`], from `KANSTACK_HARNESS_SYSTEM_FLAG`
/// and the configured harness itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteDelivery {
    /// `<flag> <shell-quoted note>`, ahead of the initial message — e.g. `claude
    /// --append-system-prompt "..."`. Additive to the harness's own default system
    /// prompt, not a replacement, and doesn't consume a "turn" the way the initial
    /// message does — so it goes out even when there's no initial message at all.
    Flag(String),
    /// Codex's `-c developer_instructions="<note>"` config override: a generic
    /// instructions-role message, not literally an appended system prompt, but the
    /// closest thing Codex CLI has to one today (no `--append-system-prompt` exists for
    /// it — see `resolve_note_delivery`). Same "goes out regardless of the initial
    /// message" property as `Flag`.
    CodexConfig,
    /// No known out-of-band mechanism for this harness (e.g. OpenCode, which as of
    /// writing has no CLI flag for this at all — a feature request for exactly this was
    /// filed and closed as not planned). Falls back to prepending the note directly onto
    /// the initial message text instead — the one thing every harness's launch command
    /// already has room for.
    FoldIntoMessage,
    /// Explicitly turned off via `KANSTACK_HARNESS_SYSTEM_FLAG=""`.
    Disabled,
}

/// Builds the shell command line a split backend types into the fresh terminal — split out
/// so it can be unit tested without shelling out to a real `cmux`/`tmux`.
pub fn build_launch_command(
    cwd: &Path,
    harness: &str,
    note_delivery: &NoteDelivery,
    name: &str,
    initial_message: Option<&str>,
) -> String {
    let mut launch = format!("cd {} && {}", shell_quote(&cwd.to_string_lossy()), harness);
    let mut message = initial_message.map(str::to_string);

    match note_delivery {
        NoteDelivery::Flag(flag) => {
            launch.push(' ');
            launch.push_str(flag);
            launch.push(' ');
            launch.push_str(&shell_quote(&branch_context_note(name)));
        }
        NoteDelivery::CodexConfig => {
            launch.push_str(" -c ");
            launch.push_str(&shell_quote(&format!(
                "developer_instructions={}",
                toml_quote(&branch_context_note(name))
            )));
        }
        NoteDelivery::FoldIntoMessage => {
            message = Some(match message {
                Some(m) => format!("{}\n\n{m}", branch_context_note(name)),
                None => branch_context_note(name),
            });
        }
        NoteDelivery::Disabled => {}
    }

    if let Some(message) = message {
        launch.push(' ');
        launch.push_str(&shell_quote(&message));
    }
    launch.push('\n');
    launch
}

/// The `NoteDelivery` for `harness`, from `KANSTACK_HARNESS_SYSTEM_FLAG` if set —
/// including explicitly to `""`, to opt out entirely — otherwise whatever's known-good
/// for `harness` itself, matched against its own basename (so `claude
/// --dangerously-skip-permissions` baked into `KANSTACK_HARNESS` still resolves the same
/// as plain `claude` would):
///
/// - `claude`, `pi`: confirmed `--append-system-prompt <text>`, additive to (not
///   replacing) their own default system prompt.
/// - `codex`: no such flag exists (a request for exactly that,
///   openai/codex#11117, is closed unimplemented) — `-c developer_instructions=<toml>` is
///   the closest equivalent, a differently-*shaped* mechanism (TOML-quoted, and a
///   "developer" message rather than literally the system prompt), not just a different
///   flag name.
/// - `opencode`, `kiro`/`kiro-cli`, `gemini`, and anything else unrecognized: no
///   mechanism exists at all, each confirmed separately rather than assumed —
///   OpenCode has no such flag (a feature request for exactly this,
///   anomalyco/opencode#16089, was closed as not planned); Kiro CLI's full `kiro-cli
///   chat` flag reference has nothing for system prompt/instructions/context (only
///   pre-configured, not-dynamic-per-invocation agents via `--agent`); Gemini CLI's only
///   mechanism, `GEMINI_SYSTEM_MD`, is an env var pointing at a file that *fully
///   replaces* the default system prompt rather than appending to it — using it would
///   strip Gemini CLI's own built-in behavior instructions, worse than sending nothing.
///   All fall back to folding the note into the initial message itself, the one delivery
///   every harness supports.
pub fn resolve_note_delivery(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => return NoteDelivery::Disabled,
        Ok(flag) => return NoteDelivery::Flag(flag),
        Err(_) => {}
    }
    delivery_for_harness(harness)
}

/// [`resolve_note_delivery`] for a harness other than the configured one
/// (`kanstack spawn --agent`): `KANSTACK_HARNESS_SYSTEM_FLAG` describes the configured
/// harness, so a flag set there isn't applied to a different one — it would be an unknown
/// option to it. Only the explicit opt-out (`""`) carries over.
pub fn resolve_note_delivery_for_override(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => NoteDelivery::Disabled,
        _ => delivery_for_harness(harness),
    }
}

fn delivery_for_harness(harness: &str) -> NoteDelivery {
    let program = harness.split_whitespace().next().unwrap_or(harness);
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    match name {
        "claude" | "pi" => NoteDelivery::Flag("--append-system-prompt".to_string()),
        "codex" => NoteDelivery::CodexConfig,
        "opencode" | "kiro" | "kiro-cli" | "gemini" => NoteDelivery::FoldIntoMessage,
        _ => NoteDelivery::FoldIntoMessage,
    }
}

/// The context note delivered to a harness, telling it what `git status`/`git branch`
/// won't: it's sitting in `branch`, a GitButler *virtual* branch, not a real one — every
/// lane in the workspace shares the same checked-out ref, so those commands report that
/// shared state rather than anything specific to `branch`.
pub fn branch_context_note(branch: &str) -> String {
    format!(
        "You're running inside a GitButler virtual branch called \"{branch}\". `git status`/\
         `git branch` reflect the shared workspace checkout, not this branch specifically \
         — use `but status` or `but diff` (GitButler's CLI) to see the real state if you \
         need it."
    )
}

/// Single-quotes a path for the target shell, escaping any embedded `'`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double-quotes `s` as a TOML basic string value, escaping `\` and `"` — what Codex's
/// `-c key=value` needs for a string value, on top of (inside) the shell-level quoting
/// `shell_quote` already applies to the whole `-c` argument.
pub fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Disabled` (`KANSTACK_HARNESS_SYSTEM_FLAG=""`) is the one delivery that sends no
    /// note at all — the launch line is exactly what it would've been before this
    /// feature existed.
    #[test]
    fn build_launch_command_sends_nothing_when_disabled() {
        let launch = build_launch_command(Path::new("/repo"), "claude", &NoteDelivery::Disabled, "feat-x", Some("fix it"));
        assert_eq!(launch, "cd '/repo' && claude 'fix it'\n");
    }

    /// `Flag` puts the note through the configured flag, ahead of the initial message,
    /// which stays a separate, ordinary positional argument.
    #[test]
    fn build_launch_command_passes_the_note_through_a_flag() {
        let delivery = NoteDelivery::Flag("--append-system-prompt".to_string());
        let launch = build_launch_command(Path::new("/repo"), "claude", &delivery, "feat-x", Some("fix it"));
        assert_eq!(
            launch,
            format!(
                "cd '/repo' && claude --append-system-prompt {} 'fix it'\n",
                shell_quote(&branch_context_note("feat-x"))
            )
        );
    }

    /// A `Flag` delivery's note goes out even with no initial message at all — appending
    /// to the system prompt doesn't put words in anyone's mouth the way a positional
    /// prompt argument would, so an otherwise-blank REPL still gets to know where it's
    /// running.
    #[test]
    fn build_launch_command_sends_a_flag_note_even_without_an_initial_message() {
        let delivery = NoteDelivery::Flag("--append-system-prompt".to_string());
        let launch = build_launch_command(Path::new("/repo"), "claude", &delivery, "feat-x", None);
        assert!(
            launch.contains("--append-system-prompt"),
            "the note must go out regardless of whether there's a message: {launch:?}"
        );
        assert!(
            !launch.trim_end().ends_with("--append-system-prompt"),
            "the note's own text must follow the flag: {launch:?}"
        );
    }

    /// `CodexConfig` wraps the note in TOML double-quotes *inside* the shell-quoted
    /// `-c` argument — two independent layers of quoting, not one.
    #[test]
    fn build_launch_command_toml_quotes_the_note_for_codex_config() {
        let launch = build_launch_command(Path::new("/repo"), "codex", &NoteDelivery::CodexConfig, "feat-x", Some("fix it"));
        let expected_arg = shell_quote(&format!(
            "developer_instructions={}",
            toml_quote(&branch_context_note("feat-x"))
        ));
        assert_eq!(launch, format!("cd '/repo' && codex -c {expected_arg} 'fix it'\n"));
    }

    /// `FoldIntoMessage` has no separate channel — the note becomes the start of the
    /// initial message itself, with the original message kept intact after it.
    #[test]
    fn build_launch_command_folds_the_note_onto_an_existing_message() {
        let launch = build_launch_command(Path::new("/repo"), "opencode", &NoteDelivery::FoldIntoMessage, "feat-x", Some("fix it"));
        let expected_message = format!("{}\n\nfix it", branch_context_note("feat-x"));
        assert_eq!(launch, format!("cd '/repo' && opencode {}\n", shell_quote(&expected_message)));
    }

    /// With no initial message to fold onto, the note becomes the whole message — unlike
    /// `Flag`/`CodexConfig`, this is the only delivery that has to spend the "turn" a
    /// positional prompt argument costs, since folding is all it has.
    #[test]
    fn build_launch_command_sends_the_note_alone_when_folding_with_no_message() {
        let launch = build_launch_command(Path::new("/repo"), "opencode", &NoteDelivery::FoldIntoMessage, "feat-x", None);
        assert_eq!(
            launch,
            format!("cd '/repo' && opencode {}\n", shell_quote(&branch_context_note("feat-x")))
        );
    }

    #[test]
    fn branch_context_note_names_the_branch_and_points_at_but() {
        let note = branch_context_note("feat-x");
        assert!(note.contains("\"feat-x\""), "missing the branch name: {note:?}");
        assert!(note.contains("but status"), "missing the but-CLI pointer: {note:?}");
    }

    #[test]
    fn toml_quote_escapes_backslashes_and_quotes() {
        assert_eq!(toml_quote(r#"say "hi" \ bye"#), r#""say \"hi\" \\ bye""#);
    }

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
            // opencode: confirmed no such flag exists at all (anomalyco/opencode#16089,
            // a request for exactly this, closed not planned).
            assert_eq!(resolve_note_delivery("opencode"), NoteDelivery::FoldIntoMessage);
            // kiro / kiro-cli: the full `kiro-cli chat` flag reference has nothing for
            // system prompt/instructions/context.
            assert_eq!(resolve_note_delivery("kiro"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro-cli"), NoteDelivery::FoldIntoMessage);
            // gemini: the only mechanism (GEMINI_SYSTEM_MD) fully replaces the default
            // system prompt rather than appending, so using it would do more harm than
            // sending nothing.
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
}
