use crate::harness::Harness;

/// No such flag (a feature request for exactly this, anomalyco/opencode#16089, was closed
/// as not planned), so the note is folded into the initial message.
///
/// **No `status_hooks` — checked and deliberately left out.** OpenCode's blocking hook
/// (`tool.execute.before`, an `async (input, output) => {...}` function that denies by
/// throwing) is a JS/TS plugin, registered only through an npm package name in
/// `opencode.json`, or a `.ts`/`.js` file OpenCode auto-loads from the project-local
/// `.opencode/plugin/` directory or the global `~/.config/opencode/plugin/` — confirmed
/// against `opencode.ai/docs/plugins/` and `opencode.ai/docs/cli/`'s full flag list, which has
/// nothing resembling Claude's `--settings <json>`. Wiring this would mean kanstack writing a
/// persistent plugin file into the target repository or the user's global config directory,
/// not a self-contained launch-time argument — a heavier, more invasive footprint than
/// `status_hooks`'s `LaunchExtras` (plain CLI args/env) is built for.
///
/// **`busy`/`idle` self-reporting checked too, with fresh eyes since reporting doesn't need to
/// block anything — same wall anyway.** `session.idle` (idle) and `session.created`/
/// `tool.execute.before` (busy) all exist, alongside a full event catalog including
/// `permission.asked`/`permission.replied` (a real `waiting` candidate, confirmed to exist,
/// moot for the same reason as everything else here). Every one of them registers exactly the
/// same way as `tool.execute.before` — a `.ts`/`.js` plugin file, or an npm package name in
/// `opencode.json` — with no distinct "observation-only" registration tier and no config key
/// that takes an inline shell command for any event (the closest thing,
/// `attention.notifications`/`attention.sound`, is a boolean toggle for OpenCode's own built-in
/// desktop notifications, not a hook). Non-blocking doesn't buy a lighter mechanism here.
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
