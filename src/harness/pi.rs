use crate::harness::launch::NoteDelivery;
use crate::harness::Harness;

/// Same confirmed `--append-system-prompt <text>` as Claude.
///
/// **No `status_hooks` — checked and deliberately left out.** Pi's blocking hook (`tool_call`)
/// is a TypeScript extension module (`pi.on("tool_call", async (event, ctx) => ...)`, returning
/// `{block: true, reason}` to deny), not a subprocess with a JSON stdin/stdout contract — there
/// is no serialization boundary at all, so it can't be pointed at `kanstack claim` the way a
/// `command`-type hook can. Extensions are auto-discovered from `~/.pi/agent/extensions/*.ts`
/// or `.pi/extensions/*.ts`, or loaded ad hoc with `pi -e <path>`, which Pi's own docs call
/// "only for quick tests," not a stable launch-time mechanism to build on. Either way this
/// needs a `.ts` file written to disk before launch — the same class of problem as OpenCode's
/// plugin requirement — plus, per Pi's own docs, extensions "run with your full system
/// permissions and can execute arbitrary code," a materially heavier footprint than every
/// other harness's `status_hooks`.
///
/// **`busy`/`idle` self-reporting checked too — same wall, confirmed independently.**
/// `turn_start`/`turn_end` exist and would be the right pair, but they register through the
/// exact same `pi.on(...)` TypeScript extension API as `tool_call` — there is no separate,
/// lighter subprocess/shell-command hook path for lifecycle events either (an explicit search
/// of Pi's docs found no config-file `hooks:` section or CLI flag anywhere). Extensions can
/// read `process.env` and shell out via `pi.exec(...)`, so a minimal, once-written `.ts` file
/// that just calls `kanstack report` is technically buildable, but that doesn't change the
/// actual objection: it still needs a file on disk before launch, still runs under the same
/// full-system-permissions extension model the docs warn about — a smaller handler body, not a
/// smaller risk. No `waiting`-equivalent event was found either.
pub struct Pi;
impl Harness for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::Flag("--append-system-prompt".to_string())
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::with_system_flag_env;
    use crate::harness::resolve_note_delivery;

    #[test]
    fn resolve_note_delivery_uses_append_system_prompt_for_pi() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("pi"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }
}
