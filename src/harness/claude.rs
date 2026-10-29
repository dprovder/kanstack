use crate::harness::launch::{shell_quote, LaunchExtras, NoteDelivery};
use crate::harness::{Harness, Prompt};

/// Confirmed `--append-system-prompt <text>`, additive to (not replacing) its own default
/// system prompt.
///
/// Hooks come in through `--settings <json>`, which is a settings layer of its own: the
/// hooks in it run alongside the user's and the project's, they don't replace them, so
/// nothing on disk is touched.
///
/// Which event says what: `UserPromptSubmit` starts a turn, and each tool call reasserts
/// `busy` before (`PreToolUse`) and after (`PostToolUse`, `PostToolUseFailure`) it, which
/// both keeps a long turn's report fresh (see `crate::report::FRESH_FOR`) and takes the state
/// back from `waiting` once a permission prompt has been answered. `PermissionRequest` fires
/// after `PreToolUse` and before the prompt shows, and reports `waiting`. `Stop` ends a turn;
/// `StopFailure` ends one that died on an API error, which would otherwise read `busy` until
/// the report expired.
///
/// Interrupting a turn with Escape fires none of these, and neither does answering a
/// permission prompt with "No" (both observed against real Claude), so the report would go on
/// saying `busy` or `waiting` over a pane sitting at its prompt. A `Notification` hook for the
/// `idle_prompt` type looked like the recovery signal, and is deliberately not installed: it
/// never fired in over 100 seconds, in cmux or in a clean tmux with no launch shim. What
/// clears a stale `busy` is `crate::splitter`'s check that the pane has stayed quiet. A stale
/// `waiting` is left standing until the next prompt: after "No" the turn simply ends and the
/// pane is at rest, which from outside is indistinguishable from a prompt still pending, so
/// only the agent could say — and it can't.
///
/// Every hook is synchronous, a few milliseconds each. That is what keeps them in order:
/// `PreToolUse` and `PermissionRequest` fire back to back, and if the first were async its
/// `busy` could land after the second's `waiting` and hide the prompt.
///
/// `report` prints nothing and exits 0, which matters twice over: `UserPromptSubmit`'s
/// stdout is added to what the model sees, and a `PermissionRequest` hook that stays silent
/// leaves the normal prompt alone rather than approving or denying anything.
///
/// A second `PreToolUse` entry, matching only `Edit|Write|MultiEdit`, runs `kanstack claim`
/// (see `crate::claims`, `crate::cli::claim`) — the preflight file-claim check that can deny
/// the tool call outright when another lane is already mid-edit of the same file, via
/// `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny",...}}` on
/// stdout, confirmed against Claude Code's own hooks documentation (only exit code 2 blocks
/// unconditionally there; this JSON form is the one that can't misfire from an unrelated
/// non-zero exit, so `claim` never uses exit code 2). It runs alongside the existing
/// matcher-`""` entry, which still reports `busy` for every tool call regardless of which one
/// this narrower matcher catches.
///
/// No `model_effort_args` override: Claude Code's own `--model <value>` is exactly the
/// trait's default spelling, and it has no reasoning-effort flag of its own to map `effort`
/// onto, so the default's silent drop of it is already the right answer here too.
pub struct Claude;
impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::Flag("--append-system-prompt".to_string())
    }
    fn status_hooks(&self, report: &str, branch: &str, _cwd: &std::path::Path) -> Option<LaunchExtras> {
        let entry = |matcher: &str, command: String| {
            serde_json::json!({ "matcher": matcher, "hooks": [{ "type": "command", "command": command, "timeout": 5 }] })
        };
        let hook = |state: &str| serde_json::json!([entry("", format!("{report} {state} {}", shell_quote(branch)))]);
        // `report` is always `"<quoted-exe> report"` (see `reporter_command`); the claim-check
        // hook needs the same executable, running a different subcommand — the exe is
        // recovered by stripping the trailing subcommand name back off.
        let exe = report.strip_suffix(" report").unwrap_or(report);
        let pre_tool_use = serde_json::json!([
            entry("", format!("{report} busy {}", shell_quote(branch))),
            entry("Edit|Write|MultiEdit", format!("{exe} claim {}", shell_quote(branch))),
        ]);
        let settings = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": hook("busy"),
                "PreToolUse": pre_tool_use,
                "PermissionRequest": hook("waiting"),
                "PostToolUse": hook("busy"),
                "PostToolUseFailure": hook("busy"),
                "Stop": hook("idle"),
                "StopFailure": hook("idle"),
            }
        });
        Some(LaunchExtras { args: vec!["--settings".to_string(), settings.to_string()], ..LaunchExtras::default() })
    }

    /// Claude Code (v2.1, measured through `cmux read-screen`) draws its input box between
    /// two full-width `─` rules near the bottom of the pane. The first row starts with `❯ `,
    /// and wrapped or multi-line input continues on rows indented by two spaces. A long paste
    /// collapses to `[Pasted text #N +M lines]`. Input taller than the box scrolls so its *end*
    /// stays visible, which is why `crate::mux::delivery` matches the end of a message.
    ///
    /// Past user messages in the transcript also start with `❯`, but they don't sit directly
    /// under a rule. That rule is how the live box is told apart from history. A message
    /// queued while Claude is busy leaves the box and appears above the rule (`ctrl+enter to
    /// send now`), and the box then shows `Press up to edit queued messages`.
    ///
    /// A permission prompt, the folder-trust screen, or a non-default input mode (`!` bash
    /// mode) shows no `❯` box, so it reads as [`Prompt::Hidden`].
    fn read_prompt(&self, screen: &str) -> Option<Prompt> {
        let rows: Vec<&str> = screen.lines().collect();
        let is_rule = |row: &str| row.trim().starts_with("────");
        let Some(top) = (1..rows.len()).rev().find(|&i| rows[i].starts_with('❯') && is_rule(rows[i - 1])) else {
            return Some(Prompt::Hidden);
        };
        let Some(len) = rows[top..].iter().position(|row| is_rule(row)) else {
            return Some(Prompt::Hidden);
        };
        let text = rows[top..top + len]
            .iter()
            .enumerate()
            .map(|(i, row)| if i == 0 { row.trim_start_matches('❯').trim() } else { row.trim() })
            .collect::<Vec<_>>()
            .join("\n");
        let collapsed_paste = text.contains("[Pasted text #");
        Some(Prompt::Shown { text, collapsed_paste })
    }
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::{launched, stand_in_harnesses, with_system_flag_env, REPORT};
    use crate::harness::{for_command, resolve_note_delivery, HarnessConfig};

    #[test]
    fn resolve_note_delivery_uses_append_system_prompt_for_claude() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    /// Extra flags baked into `KANSTACK_HARNESS` (e.g. to skip a permissions prompt)
    /// shouldn't stop the harness itself from being recognized.
    #[test]
    fn resolve_note_delivery_matches_the_program_ignoring_its_own_flags() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude --dangerously-skip-permissions"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    // Status hooks.

    fn claude_settings() -> serde_json::Value {
        let extras = for_command("claude")
            .status_hooks(REPORT, "feat-x", std::path::Path::new("/repo"))
            .expect("claude takes hooks at launch");
        assert_eq!(extras.args[0], "--settings");
        serde_json::from_str(&extras.args[1]).expect("--settings takes JSON")
    }

    #[test]
    fn claude_reports_what_each_event_means() {
        let settings = claude_settings();
        let hooks = settings["hooks"].as_object().unwrap();
        let says = |event: &str| {
            let entry = &hooks[event][0];
            assert_eq!(entry["matcher"], "", "{event}");
            let hook = &entry["hooks"][0];
            assert_eq!(hook["type"], "command", "{event}");
            assert_eq!(hook["timeout"], 5, "{event}");
            // Every hook is synchronous: `PreToolUse` and `PermissionRequest` fire back to
            // back, and an async `busy` could land after the `waiting` and hide the prompt.
            assert!(hook.get("async").is_none(), "{event} must not be async");
            // The branch is baked into the command literally: Claude's own hook runner does
            // not hand hook subprocesses `KANSTACK_BRANCH`, even though the launched process
            // itself has it (confirmed against real Claude).
            hook["command"].as_str().unwrap().strip_prefix("'/opt/kanstack' report ").unwrap().strip_suffix(" 'feat-x'").unwrap().to_string()
        };

        assert_eq!(says("UserPromptSubmit"), "busy");
        assert_eq!(says("PreToolUse"), "busy");
        assert_eq!(says("PermissionRequest"), "waiting");
        assert_eq!(says("PostToolUse"), "busy", "takes the state back from waiting once a prompt is answered");
        assert_eq!(says("PostToolUseFailure"), "busy");
        assert_eq!(says("Stop"), "idle");
        assert_eq!(says("StopFailure"), "idle", "a turn that dies on an API error must not read busy");
        assert_eq!(
            hooks.len(),
            7,
            "no other events are hooked — in particular not Notification/idle_prompt, which never fired: {:?}",
            hooks.keys().collect::<Vec<_>>()
        );
    }

    // The input box. Each screen is a real `cmux read-screen` capture of Claude Code v2.1.306,
    // cut down to its last rows with the rules shortened.

    use crate::harness::Prompt;

    const RULE: &str = "──────────────────────────────────────────";

    fn read(rows: &[&str]) -> Prompt {
        let screen = rows.iter().map(|row| row.replace("{rule}", RULE)).collect::<Vec<_>>().join("\n");
        for_command("claude").read_prompt(&screen).expect("claude's input box is readable")
    }

    fn shown(text: &str) -> Prompt {
        Prompt::Shown { text: text.to_string(), collapsed_paste: false }
    }

    const STATUS: [&str; 2] = ["  ░░░░░ 4% | $0.00 | +0 -0 | Haiku 5.5", "  ⏵⏵ auto mode on (shift+tab to cycle)"];

    #[test]
    fn the_box_is_the_prompt_row_between_two_rules_not_a_past_message() {
        let idle = read(&["❯ Reply with just the word PONG-ALPHA-7.", "⏺ PONG-ALPHA-7", "{rule}", "❯ ", "{rule}", STATUS[0], STATUS[1]]);
        assert_eq!(idle, shown(""), "the transcript's `❯` row is history, not the box");
    }

    #[test]
    fn wrapped_and_multi_line_input_is_joined_without_its_indent() {
        let typed = read(&[
            "{rule}",
            "❯ Paragraph 11: this is filler text about the send verification experiment, this is",
            "  filler text about the send verification experiment,",
            "",
            "  Final instruction: reply with only the word PONG-BRAVO-9.",
            "{rule}",
            STATUS[0],
            "  paste again to expand",
        ]);
        assert_eq!(
            typed,
            shown("Paragraph 11: this is filler text about the send verification experiment, this is\nfiller text about the send verification experiment,\n\nFinal instruction: reply with only the word PONG-BRAVO-9.")
        );
    }

    #[test]
    fn a_collapsed_paste_is_flagged() {
        let collapsed = read(&["✻ Brewed for 6s · done 10:51 PM", "{rule}", "❯ [Pasted text #7 +24 lines]", "{rule}", STATUS[0], "  paste again to expand"]);
        assert_eq!(collapsed, Prompt::Shown { text: "[Pasted text #7 +24 lines]".into(), collapsed_paste: true });
    }

    /// Queued while busy: the message has left the box (that is what makes it delivered), and
    /// the box shows a hint instead.
    #[test]
    fn a_queued_message_is_above_the_box_not_in_it() {
        let queued = read(&[
            "  Final instruction: reply with only the word PONG-QUEUED-2.",
            "  ctrl+enter to send now",
            "{rule}",
            "❯ Press up to edit queued messages",
            "{rule}",
            STATUS[0],
        ]);
        assert_eq!(queued, shown("Press up to edit queued messages"));
    }

    #[test]
    fn a_dialog_in_place_of_the_box_is_hidden() {
        let trust = read(&[
            "{rule}",
            " Accessing workspace:",
            " Quick safety check: Is this a project you created or one you trust?",
            " ❯ No, exit",
            "   Yes, I trust this folder",
            " Enter to confirm · Esc to cancel",
        ]);
        assert_eq!(trust, Prompt::Hidden, "a dialog's own `❯` is indented and not under a rule");
        assert_eq!(read(&["{rule}", "❯ half a box, its closing rule not drawn yet"]), Prompt::Hidden);
        assert_eq!(read(&[]), Prompt::Hidden);
    }

    #[test]
    fn a_configured_reporter_hooks_claude_and_names_the_lane_in_the_environment() {
        with_system_flag_env(None, || {
            let dir = stand_in_harnesses("hooked");
            let claude = dir.join("claude").to_string_lossy().into_owned();
            let config = HarnessConfig::new(claude).with_reporter(REPORT);

            let (branch, args) = launched(&config, None, Some("fix it"));
            assert_eq!(branch, "feat-x");
            assert_eq!(args[0], "--append-system-prompt");
            assert_eq!(args[2], "--settings");
            let settings: serde_json::Value = serde_json::from_str(&args[3]).unwrap();
            assert_eq!(settings, claude_settings(), "the settings survive quoting and spilling byte for byte");
            assert_eq!(args.len(), 5, "note flag, note, --settings, json, message: {args:?}");
            assert_eq!(args[4], "fix it", "the first message stays last");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
