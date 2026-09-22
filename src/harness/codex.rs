use crate::harness::launch::NoteDelivery;
use crate::harness::Harness;

/// No `--append-system-prompt` exists (a request for exactly that, openai/codex#11117, is
/// closed unimplemented); `-c developer_instructions=<toml>` is the closest equivalent, a
/// differently-*shaped* mechanism (TOML-quoted, and a "developer" message rather than
/// literally the system prompt), not just a different flag name.
///
/// **No `status_hooks` — checked and deliberately left out**, for two independent reasons,
/// confirmed against Codex's own hooks docs (`developers.openai.com/codex/hooks`):
/// - Hooks configure only through `~/.codex/hooks.json`/`config.toml` (or their project-local
///   equivalents) — no CLI flag exists to inject one at launch the way Claude's `--settings`
///   does. `-c` does accept arbitrary dotted-TOML overrides in principle, so `-c
///   hooks.PreToolUse=...` *might* parse, but this exact usage is undocumented with no example
///   anywhere — not something to build a blocking security check on.
/// - Even with a hook wired up, `apply_patch` (Codex's edit tool) hands `tool_input.command`
///   the raw patch text in Codex's own diff format (`*** Update File: path`, etc.), not a
///   `file_path` string — extracting the touched path(s) needs a patch-format parser, real
///   work `cli::claim::file_path_from_hook_payload` doesn't do today, not just another field
///   name to check for.
///
/// **`busy`/`idle` self-reporting checked too, same verdict, more precisely characterized.**
/// `UserPromptSubmit` (busy) and `Stop` (idle) exist and would be the right pair — confirmed
/// against the actual `openai/codex` source (`codex-rs/hooks`,
/// `codex-rs/config/src/hook_config.rs`), not just the docs site, since the docs site's "hooks
/// are experimental/disabled by default" framing turned out to not match the source at all.
/// The real blocker isn't a feature flag: `discovery.rs` only runs a hook once it's
/// `HookTrustStatus::Managed`/`Trusted`, and anything else — including a *new* hook config, or
/// one that changed since it was last trusted — triggers an interactive TUI review prompt
/// (`tui/src/startup_hooks_review.rs`). Since kanstack's hook command bakes the branch name in
/// (same as every other harness), every new branch would produce a hook config Codex has never
/// seen before, so this review would fire on *every single `kanstack spawn`* — worse than any
/// other wired harness's UX. A blanket bypass flag (`bypass_hook_trust`) exists, but it skips
/// trust review for anything else the user has configured too, not just kanstack's own hook —
/// a real security-relevant side effect, not something to flip on by default just to silence a
/// prompt. Combined with the same "no CLI-only injection path" blocker as the claim-check
/// (`CODEX_HOME` exists but redirects Codex's *entire* home directory — auth, sessions,
/// everything — the same problem Gemini's `GEMINI_CLI_HOME` was rejected for), this stays
/// unwired. (`PermissionRequest`, for what it's worth, is a real confirmed `waiting` signal —
/// moot while nothing else here is wireable, but worth knowing if this ever gets revisited.)
pub struct Codex;
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::CodexConfig
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::with_system_flag_env;
    use crate::harness::resolve_note_delivery;

    #[test]
    fn resolve_note_delivery_uses_codex_config_for_codex() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("codex"), NoteDelivery::CodexConfig);
        });
    }
}
