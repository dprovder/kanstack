use crate::harness::launch::NoteDelivery;
use crate::harness::Harness;

/// No `--append-system-prompt` exists (a request for exactly that, openai/codex#11117, is
/// closed unimplemented); `-c developer_instructions=<toml>` is the closest equivalent, a
/// differently-*shaped* mechanism (TOML-quoted, and a "developer" message rather than
/// literally the system prompt), not just a different flag name.
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
