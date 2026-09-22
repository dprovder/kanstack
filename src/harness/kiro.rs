use crate::harness::Harness;

/// `kiro-cli chat`'s full flag reference has nothing for system prompt, instructions or
/// context (only pre-configured, not-dynamic-per-invocation agents via `--agent`), so the
/// note is folded into the initial message.
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
