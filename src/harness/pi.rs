use crate::harness::launch::NoteDelivery;
use crate::harness::Harness;

/// Same confirmed `--append-system-prompt <text>` as Claude.
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
