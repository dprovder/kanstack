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
///
/// **`busy`/`idle` self-reporting checked too, same root cause.** `kiro.dev/docs/hooks/types/`
/// lists `promptSubmit` (a plausible `busy`, though its "can block" status wasn't tested for
/// firing cleanly as a pure side effect) and `agentStop` ("triggers when the agent has
/// completed its turn," a solid `idle`) — `agentSpawn` also exists but reads as a one-time
/// session-activation event, not confirmed to re-fire per turn, so it's the weaker `busy`
/// candidate of the two. None of that matters yet: Kiro's full CLI reference and environment
/// variable docs were checked again, specifically for this purpose, and still have nothing
/// resembling Gemini's `GEMINI_CLI_SYSTEM_SETTINGS_PATH` escape hatch — hooks configure only
/// through static `.kiro/hooks/*.json` files with no CLI-flag or env-var injection path at
/// launch, exit-code semantics aside. No `waiting`-equivalent event was found either.
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
