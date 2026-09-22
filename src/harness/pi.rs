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
