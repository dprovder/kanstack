use crate::harness::Harness;

/// No such flag (a feature request for exactly this, anomalyco/opencode#16089, was closed
/// as not planned), so the note is folded into the initial message.
pub struct OpenCode;
impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::with_system_flag_env;
    use crate::harness::resolve_note_delivery;

    /// No confirmed mechanism, so the note is folded into the initial message.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_opencode() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("opencode"), NoteDelivery::FoldIntoMessage);
        });
    }
}
