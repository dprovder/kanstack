use std::path::Path;

use crate::harness::launch::{shell_quote, LaunchExtras};
use crate::harness::Harness;

/// Its only mechanism, `GEMINI_SYSTEM_MD`, is an env var pointing at a file that *fully
/// replaces* the default system prompt rather than appending to it — using it would strip
/// Gemini CLI's own built-in behavior instructions, worse than sending nothing. So the note
/// is folded into the initial message.
///
/// `status_hooks`, confirmed against Gemini CLI's own hooks reference
/// (github.com/google-gemini/gemini-cli, `docs/hooks/reference.md`) and its file-system
/// tools doc (`docs/tools/file-system.md`), not assumed to match Claude's:
///
/// - The blocking event is `BeforeTool`, not `PreToolUse` — fired before `write_file`/
///   `replace` (Gemini's edit tools) the same way `PreToolUse` fires before Claude's
///   `Edit`/`Write`/`MultiEdit`. The stdin payload otherwise lines up with Claude's:
///   `tool_name`, and — confirmed against both edit tools' own parameter docs, each
///   independently — `tool_input.file_path`, the exact field `cli::claim::file_path_from_hook_payload`
///   already reads. No payload-parsing change was needed for Gemini.
/// - The deny decision is a **flat** JSON object, not Claude's `hookSpecificOutput` wrapper:
///   `{"decision":"deny","reason":"..."}`. `cli::claim::DecisionShape` picks this over
///   Claude's shape by reading the payload's own `hook_event_name` (`"BeforeTool"` vs
///   `"PreToolUse"`), so this hook command is `kanstack claim <branch>` — byte for byte the
///   same invocation as Claude's, no extra flag needed to tell them apart.
/// - Gemini's own docs are explicit that exit code `0` with `stdout` JSON is "preferred for
///   all logic," and exit `2` is a *different*, stderr-only path — so unlike Kiro (exit-code-2
///   only, no JSON alternative found), Gemini has the same safe escape hatch Claude does, and
///   `claim` uses it the same way: always exit `0`, deny via JSON, never exit `2`.
/// - Unlike Claude, there is no `--settings <json>` (or any) CLI flag — confirmed against
///   Gemini CLI's full configuration reference. Hooks are configured only through
///   `.gemini/settings.json` (project) or `~/.gemini/settings.json` (user), and this task
///   deliberately does not write into either — clobbering or merging into a user's own,
///   persistent settings file from a `spawn` call is a heavier and riskier side effect than
///   anything another harness's `status_hooks` does. Instead, `GEMINI_CLI_SYSTEM_SETTINGS_PATH`
///   (documented in `docs/cli/enterprise.md`) points Gemini at an entirely separate file for
///   the "system overrides" tier, which the same doc says merges object-valued settings
///   (`hooks` included) with the lower tiers rather than replacing them — additive, like every
///   other status hook here. That file still has to exist on disk, though, which
///   `LaunchExtras`'s plain `args`/`env` can't do on its own — see
///   `crate::workstream::gemini_hooks_dir`'s doc comment for what this writes and the
///   cleanup gap it leaves.
pub struct Gemini;
impl Harness for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn status_hooks(&self, report: &str, branch: &str, cwd: &Path) -> Option<LaunchExtras> {
        let exe = report.strip_suffix(" report").unwrap_or(report);
        let dir = crate::workstream::gemini_hooks_dir(cwd)?;
        std::fs::create_dir_all(&dir).ok()?;
        let settings = settings_json(exe, branch);
        let path = dir.join(format!("{:016x}.json", crate::workstream::fnv1a(branch.as_bytes())));
        std::fs::write(&path, serde_json::to_vec(&settings).ok()?).ok()?;
        Some(LaunchExtras {
            args: Vec::new(),
            env: vec![("GEMINI_CLI_SYSTEM_SETTINGS_PATH".to_string(), path.to_string_lossy().into_owned())],
        })
    }
}

/// The `settings.json` content written to `GEMINI_CLI_SYSTEM_SETTINGS_PATH` — a single
/// `BeforeTool` hook, matching Gemini's own edit tools by name (`write_file`, `replace`; see
/// `docs/tools/file-system.md`), running the same `kanstack claim <branch>` invocation Claude's
/// `PreToolUse` claim-check hook runs. `timeout` is milliseconds here, confirmed against
/// Gemini's own hooks reference example — unlike Claude's, which is seconds (see
/// `Claude::status_hooks`) — so the two aren't interchangeable numbers even though both hooks
/// exist to do the same job.
fn settings_json(exe: &str, branch: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": {
            "BeforeTool": [{
                "matcher": "write_file|replace",
                "hooks": [{
                    "type": "command",
                    "command": format!("{exe} claim {}", shell_quote(branch)),
                    "timeout": 5000,
                }]
            }]
        }
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::{with_system_flag_env, REPORT};
    use crate::harness::resolve_note_delivery;

    /// No confirmed mechanism, so the note is folded into the initial message.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_gemini() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("gemini"), NoteDelivery::FoldIntoMessage);
        });
    }

    // Status hooks.

    /// Scopes `KANSTACK_STATE_PATH` to a scratch directory for the duration of `body`, the
    /// same seam `crate::workstream`'s own tests use — `gemini_hooks_dir` reads it, so
    /// `status_hooks`'s file write needs somewhere real (but temporary) to land.
    fn with_scratch_state(tag: &str, body: impl FnOnce(&Path)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-gemini-hooks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("KANSTACK_STATE_PATH", &dir);
        body(&dir);
        std::env::remove_var("KANSTACK_STATE_PATH");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_hooks_points_gemini_at_a_written_settings_file() {
        with_scratch_state("written", |_state| {
            let extras = crate::harness::for_command("gemini")
                .status_hooks(REPORT, "feat-x", Path::new("/repo/a"))
                .expect("gemini takes hooks at launch, via a written settings file");
            assert!(extras.args.is_empty(), "everything goes through the env var, not extra flags");
            assert_eq!(extras.env.len(), 1);
            let (name, path) = &extras.env[0];
            assert_eq!(name, "GEMINI_CLI_SYSTEM_SETTINGS_PATH");

            let written = std::fs::read_to_string(path).unwrap();
            let settings: serde_json::Value = serde_json::from_str(&written).unwrap();
            let hook = &settings["hooks"]["BeforeTool"][0];
            assert_eq!(hook["matcher"], "write_file|replace", "Gemini's own edit tool names, not Claude's");
            assert_eq!(hook["hooks"][0]["type"], "command");
            assert_eq!(hook["hooks"][0]["command"], "'/opt/kanstack' claim 'feat-x'", "same invocation as Claude's own claim-check hook");
            assert_eq!(hook["hooks"][0]["timeout"], 5000, "milliseconds, not Claude's seconds");
        });
    }

    /// Two branches launching concurrently must not collide on the same settings file.
    #[test]
    fn status_hooks_gives_each_branch_its_own_settings_file() {
        with_scratch_state("branches", |_state| {
            let a = crate::harness::for_command("gemini").status_hooks(REPORT, "feat-a", Path::new("/repo")).unwrap();
            let b = crate::harness::for_command("gemini").status_hooks(REPORT, "feat-b", Path::new("/repo")).unwrap();
            assert_ne!(a.env[0].1, b.env[0].1);
        });
    }

    #[test]
    fn status_hooks_gives_each_repo_its_own_settings_directory() {
        with_scratch_state("repos", |_state| {
            let a = crate::harness::for_command("gemini").status_hooks(REPORT, "feat-x", Path::new("/repo/a")).unwrap();
            let b = crate::harness::for_command("gemini").status_hooks(REPORT, "feat-x", Path::new("/repo/b")).unwrap();
            assert_ne!(a.env[0].1, b.env[0].1);
        });
    }
}
