use crate::harness::Harness;

/// Its only mechanism, `GEMINI_SYSTEM_MD`, is an env var pointing at a file that *fully
/// replaces* the default system prompt rather than appending to it — using it would strip
/// Gemini CLI's own built-in behavior instructions, worse than sending nothing. So the note
/// is folded into the initial message.
pub struct Gemini;
impl Harness for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::with_system_flag_env;
    use crate::harness::resolve_note_delivery;

    /// No confirmed mechanism, so the note is folded into the initial message.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_gemini() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("gemini"), NoteDelivery::FoldIntoMessage);
        });
    }
}
