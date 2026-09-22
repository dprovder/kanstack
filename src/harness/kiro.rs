use crate::harness::Harness;

/// `kiro-cli chat`'s full flag reference has nothing for system prompt, instructions or
/// context (only pre-configured, not-dynamic-per-invocation agents via `--agent`), so the
/// note is folded into the initial message.
///
/// **No `status_hooks` — checked and deliberately left out**, for two independent reasons,
/// confirmed against `kiro.dev/docs/hooks/` and `kiro.dev/docs/cli/hooks/`:
/// - Hooks configure only through standalone `.kiro/hooks/*.json` files (project or user) —
///   no CLI flag exists to inject one at launch, so wiring this needs a file write
///   `status_hooks`'s current `LaunchExtras` (CLI args/env only) can't do without extension —
///   the same class of problem as Gemini's, but without Gemini's env-var-only escape hatch
///   (`GEMINI_CLI_SYSTEM_SETTINGS_PATH`); Kiro has nothing equivalent.
/// - Kiro's `preToolUse` blocks via exit code `2` only — no documented JSON decision output
///   exists as an alternative (unlike Claude's `hookSpecificOutput` or Gemini's flat
///   `{"decision":"deny",...}`, both confirmed, see `cli::claim::DecisionShape`). That
///   conflicts with `cli::claim`'s fail-open-by-construction design, which deliberately never
///   risks exit code `2` for anything but a guaranteed-correct deny (see its module doc's
///   "Fail open, unconditionally") — Kiro would need that design rethought, not just a
///   `status_hooks` override.
///
/// Also worth flagging for whoever revisits this: Kiro's payload uses `tool_input.path`, not
/// `tool_input.file_path` (confirmed via kirodotdev/Kiro issue #7500's own JSON example) — one
/// more reason porting it isn't a drop-in even setting the two points above aside.
pub struct Kiro;
impl Harness for Kiro {
    fn id(&self) -> &'static str {
        "kiro"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["kiro-cli"]
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::with_system_flag_env;
    use crate::harness::resolve_note_delivery;

    /// No confirmed mechanism, so the note is folded into the initial message — for both
    /// the name and its `kiro-cli` alias.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_kiro() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("kiro"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro-cli"), NoteDelivery::FoldIntoMessage);
        });
    }
}
