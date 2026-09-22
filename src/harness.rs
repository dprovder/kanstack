//! What kanstack knows about each coding-agent harness, in one place.
//!
//! A harness is the program typed into a fresh pane (`claude`, `codex`, ...). Everything
//! specific to *which* one it is lives behind [`Harness`]: today that is how the branch
//! context note reaches it (see [`NoteDelivery`]), and it is where anything else that varies
//! per harness belongs — one implementor per harness, one entry in [`KNOWN`].
//!
//! The split backends (`crate::cmux`, `crate::tmux`, `crate::orca`) know nothing about any
//! of this: [`HarnessConfig::launch_line`] turns a harness choice into the finished line,
//! and a backend just types it.

use std::path::Path;

use anyhow::Result;

use crate::harness_launch::{launch_line_with, shell_quote, LaunchExtras, NoteDelivery};

/// One coding-agent harness. Unit structs, looked up by [`for_command`].
pub trait Harness: Sync {
    /// The program's own name, as it would be typed at a shell: `claude`. What
    /// [`for_command`] matches against, and what `--setup` looks for on `PATH`.
    fn id(&self) -> &'static str;

    /// Other names the same program answers to (`kiro-cli` for `kiro`).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// How the branch-context note (see `harness_launch::branch_context_note`) reaches this
    /// harness when nothing overrides it. Defaults to the one delivery every harness
    /// supports: folding the note into the initial message.
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::FoldIntoMessage
    }

    /// What to put on this harness's launch line so that it runs `report` — a shell-ready
    /// command, `'/path/to/kanstack' report` — as its turns go by: `report busy` when a turn
    /// starts and while it works, `report idle` when it ends (see `crate::report`). `branch`
    /// is baked into that command literally, not left to `$KANSTACK_BRANCH`: a harness's own
    /// hook runner may not hand its hook subprocesses the environment it was launched with
    /// (confirmed for Claude, whose hooks saw no `KANSTACK_BRANCH` despite the launched
    /// process itself having it), so the command must be self-contained.
    ///
    /// `None`, the default, means kanstack knows no way to hand this harness hooks when it
    /// launches it. That costs nothing but accuracy: the multiplexer's own reading of the
    /// pane is used instead, and anything can still call `kanstack report` itself.
    fn status_hooks(&self, _report: &str, _branch: &str) -> Option<LaunchExtras> {
        None
    }
}

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
struct Claude;
impl Harness for Claude {
    fn id(&self) -> &'static str {
        "claude"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::Flag("--append-system-prompt".to_string())
    }
    fn status_hooks(&self, report: &str, branch: &str) -> Option<LaunchExtras> {
        let hook = |state: &str| {
            serde_json::json!([{
                "matcher": "",
                "hooks": [{ "type": "command", "command": format!("{report} {state} {}", shell_quote(branch)), "timeout": 5 }],
            }])
        };
        let settings = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": hook("busy"),
                "PreToolUse": hook("busy"),
                "PermissionRequest": hook("waiting"),
                "PostToolUse": hook("busy"),
                "PostToolUseFailure": hook("busy"),
                "Stop": hook("idle"),
                "StopFailure": hook("idle"),
            }
        });
        Some(LaunchExtras { args: vec!["--settings".to_string(), settings.to_string()], env: Vec::new() })
    }
}

/// No `--append-system-prompt` exists (a request for exactly that, openai/codex#11117, is
/// closed unimplemented); `-c developer_instructions=<toml>` is the closest equivalent, a
/// differently-*shaped* mechanism (TOML-quoted, and a "developer" message rather than
/// literally the system prompt), not just a different flag name.
struct Codex;
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::CodexConfig
    }
}

/// Same confirmed `--append-system-prompt <text>` as Claude.
struct Pi;
impl Harness for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::Flag("--append-system-prompt".to_string())
    }
}

/// No such flag (a feature request for exactly this, anomalyco/opencode#16089, was closed
/// as not planned), so the note is folded into the initial message.
struct OpenCode;
impl Harness for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
}

/// `kiro-cli chat`'s full flag reference has nothing for system prompt, instructions or
/// context (only pre-configured, not-dynamic-per-invocation agents via `--agent`), so the
/// note is folded into the initial message.
struct Kiro;
impl Harness for Kiro {
    fn id(&self) -> &'static str {
        "kiro"
    }
    fn aliases(&self) -> &'static [&'static str] {
        &["kiro-cli"]
    }
}

/// Its only mechanism, `GEMINI_SYSTEM_MD`, is an env var pointing at a file that *fully
/// replaces* the default system prompt rather than appending to it — using it would strip
/// Gemini CLI's own built-in behavior instructions, worse than sending nothing. So the note
/// is folded into the initial message.
struct Gemini;
impl Harness for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }
}

/// Anything not in [`KNOWN`]: a wrapper script, a harness kanstack hasn't heard of. Gets
/// the delivery that works everywhere.
struct Generic;
impl Harness for Generic {
    fn id(&self) -> &'static str {
        "generic"
    }
}

/// Every harness kanstack recognizes by name, in the order `--setup` prefers them when
/// several are installed.
pub const KNOWN: &[&dyn Harness] = &[&Claude, &Codex, &Pi, &OpenCode, &Kiro, &Gemini];

static GENERIC: Generic = Generic;

/// The harness `command` runs, matched on the basename of its first word — so `claude
/// --dangerously-skip-permissions` baked into `KANSTACK_HARNESS`, or `/usr/local/bin/claude`,
/// still resolve the same as plain `claude`. [`Generic`] when it isn't recognized.
pub fn for_command(command: &str) -> &'static dyn Harness {
    let program = command.split_whitespace().next().unwrap_or(command);
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    KNOWN
        .iter()
        .copied()
        .find(|h| h.id() == name || h.aliases().contains(&name))
        .unwrap_or(&GENERIC)
}

/// The `NoteDelivery` for `harness`: from `KANSTACK_HARNESS_SYSTEM_FLAG` if set — including
/// explicitly to `""`, to opt out entirely — otherwise whatever's known-good for the
/// harness itself (see [`Harness::note_delivery`]).
pub fn resolve_note_delivery(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => return NoteDelivery::Disabled,
        Ok(flag) => return NoteDelivery::Flag(flag),
        Err(_) => {}
    }
    for_command(harness).note_delivery()
}

/// [`resolve_note_delivery`] for a harness other than the configured one
/// (`kanstack spawn --agent`): `KANSTACK_HARNESS_SYSTEM_FLAG` describes the configured
/// harness, so a flag set there isn't applied to a different one — it would be an unknown
/// option to it. Only the explicit opt-out (`""`) carries over.
pub fn resolve_note_delivery_for_override(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => NoteDelivery::Disabled,
        _ => for_command(harness).note_delivery(),
    }
}

/// The harness a [`crate::splitter::Splitter`] launches into new panes: the command as
/// typed (`claude`, `codex --yolo`) and how its note is delivered, resolved once.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    command: String,
    note_delivery: NoteDelivery,
    /// The `kanstack report` command harnesses are handed as their status hooks, when they
    /// have a way to take them (see [`Harness::status_hooks`]).
    report_command: Option<String>,
}

impl HarnessConfig {
    /// `command`, with its note delivery resolved from the environment. No status hooks —
    /// see [`Self::from_env`] and [`Self::with_reporter`].
    pub fn new(command: impl Into<String>) -> Self {
        let command = command.into();
        let note_delivery = resolve_note_delivery(&command);
        HarnessConfig { command, note_delivery, report_command: None }
    }

    /// `$KANSTACK_HARNESS`, or `claude`, with status hooks pointing at this very executable
    /// unless `KANSTACK_STATUS_HOOKS` turns them off (see [`reporter_command`]).
    pub fn from_env() -> Self {
        let config = Self::new(std::env::var("KANSTACK_HARNESS").unwrap_or_else(|_| "claude".to_string()));
        let setting = std::env::var("KANSTACK_STATUS_HOOKS").ok();
        match reporter_command(std::env::current_exe().ok().as_deref(), setting.as_deref()) {
            Some(report) => config.with_reporter(report),
            None => config,
        }
    }

    /// Hands harnesses that can take it `report` as their status hooks.
    pub fn with_reporter(mut self, report: impl Into<String>) -> Self {
        self.report_command = Some(report.into());
        self
    }

    /// The command typed into a new pane when nothing overrides it.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The line to type into a fresh terminal so it `cd`s into `cwd` and starts the harness
    /// on lane `name`, with `initial_message` as its first message. `harness` overrides the
    /// configured command for just this pane (`kanstack spawn --agent`), including which
    /// delivery the note uses — that depends on the harness, not on what was configured.
    ///
    /// No trailing newline; the backend submits it. See `harness_launch::launch_line` for
    /// the spill-to-files handling of a prompt too long to type.
    pub fn launch_line(
        &self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
    ) -> Result<String> {
        let (command, delivery) = match harness {
            Some(h) if h != self.command => (h, resolve_note_delivery_for_override(h)),
            Some(_) | None => (self.command.as_str(), self.note_delivery.clone()),
        };
        // The hook commands themselves name `name` directly (see `Harness::status_hooks`);
        // `KANSTACK_BRANCH` is set here too, but only for a human running `kanstack report`
        // by hand from this same pane, since the harness's own hooks can't be trusted to see it.
        let extras = self
            .report_command
            .as_deref()
            .and_then(|report| for_command(command).status_hooks(report, name))
            .map(|mut extras| {
                extras.env.push(("KANSTACK_BRANCH".to_string(), name.to_string()));
                extras
            })
            .unwrap_or_default();
        let line = launch_line_with(cwd, command, &delivery, name, initial_message, &extras)?;
        Ok(line.trim_end_matches('\n').to_string())
    }
}

/// The command a harness's hooks run to report: `exe` (quoted, since it can have spaces in
/// it) followed by `report`. `None` when there is no executable to point at, or `setting` —
/// `KANSTACK_STATUS_HOOKS` — is `0`, `off`, `false` or `no`, for anyone who would rather
/// harnesses were left entirely alone.
fn reporter_command(exe: Option<&Path>, setting: Option<&str>) -> Option<String> {
    let off = setting.is_some_and(|s| matches!(s.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"));
    if off {
        return None;
    }
    Some(format!("{} report", shell_quote(&exe?.to_string_lossy())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes every `with_system_flag_env` call against every other one — `cargo
    /// test` runs tests concurrently on separate threads by default, but
    /// `std::env::set_var`/`remove_var` mutate process-wide state, so two of these
    /// running at once could see (or clobber) each other's value mid-test without this.
    static SYSTEM_FLAG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `body` with `KANSTACK_HARNESS_SYSTEM_FLAG` swapped out and restored
    /// afterwards, so this doesn't leak into other tests running in the same process.
    fn with_system_flag_env(value: Option<&str>, body: impl FnOnce()) {
        // Held for the whole call, not just the swap, so `body` (which reads the var via
        // `resolve_note_delivery`) can't be interleaved with another test's swap either.
        let _guard = SYSTEM_FLAG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("KANSTACK_HARNESS_SYSTEM_FLAG");
        match value {
            Some(v) => std::env::set_var("KANSTACK_HARNESS_SYSTEM_FLAG", v),
            None => std::env::remove_var("KANSTACK_HARNESS_SYSTEM_FLAG"),
        }
        body();
        match old {
            Some(v) => std::env::set_var("KANSTACK_HARNESS_SYSTEM_FLAG", v),
            None => std::env::remove_var("KANSTACK_HARNESS_SYSTEM_FLAG"),
        }
    }

    #[test]
    fn known_harnesses_have_distinct_names_and_keep_the_setup_priority_order() {
        let ids: Vec<_> = KNOWN.iter().map(|h| h.id()).collect();
        assert_eq!(ids, ["claude", "codex", "pi", "opencode", "kiro", "gemini"]);
        let mut every_name: Vec<_> = KNOWN.iter().flat_map(|h| [h.id()].into_iter().chain(h.aliases().iter().copied())).collect();
        let total = every_name.len();
        every_name.sort_unstable();
        every_name.dedup();
        assert_eq!(every_name.len(), total, "two harnesses claim the same name");
    }

    #[test]
    fn for_command_matches_the_program_by_basename_ignoring_its_own_flags() {
        assert_eq!(for_command("claude").id(), "claude");
        assert_eq!(for_command("claude --dangerously-skip-permissions").id(), "claude");
        assert_eq!(for_command("/opt/homebrew/bin/codex").id(), "codex");
        assert_eq!(for_command("kiro-cli").id(), "kiro");
        assert_eq!(for_command("./my-custom-harness.sh").id(), "generic");
        assert_eq!(for_command("").id(), "generic");
    }

    #[test]
    fn resolve_note_delivery_covers_claude_and_pi() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
            assert_eq!(
                resolve_note_delivery("pi"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    #[test]
    fn resolve_note_delivery_uses_codex_config_for_codex() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("codex"), NoteDelivery::CodexConfig);
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

    /// Harnesses with no confirmed mechanism (including OpenCode, and anything unknown)
    /// fall back to folding the note into the message rather than guessing at a flag
    /// that might not exist for them.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_unconfirmed_harnesses() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("opencode"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro-cli"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("gemini"), NoteDelivery::FoldIntoMessage);
            // Genuinely unrecognized commands get the same safe fallback.
            assert_eq!(resolve_note_delivery("./my-custom-harness.sh"), NoteDelivery::FoldIntoMessage);
        });
    }

    #[test]
    fn resolve_note_delivery_honors_an_explicit_flag_override() {
        with_system_flag_env(Some("--my-custom-flag"), || {
            assert_eq!(
                resolve_note_delivery("codex"),
                NoteDelivery::Flag("--my-custom-flag".to_string())
            );
        });
    }

    #[test]
    fn resolve_note_delivery_is_disabled_by_an_empty_override() {
        with_system_flag_env(Some(""), || {
            assert_eq!(resolve_note_delivery("claude"), NoteDelivery::Disabled);
        });
    }

    /// A different harness for one pane brings its own delivery, and the configured
    /// harness's `KANSTACK_HARNESS_SYSTEM_FLAG` — meaningless to it — is left behind.
    #[test]
    fn an_overriding_harness_gets_its_own_delivery_not_the_configured_flag() {
        with_system_flag_env(Some("--my-custom-flag"), || {
            assert_eq!(resolve_note_delivery_for_override("codex"), NoteDelivery::CodexConfig);
        });
        with_system_flag_env(Some(""), || {
            assert_eq!(resolve_note_delivery_for_override("codex"), NoteDelivery::Disabled);
        });
    }

    #[test]
    fn launch_line_uses_the_configured_harness_unless_overridden() {
        with_system_flag_env(None, || {
            let config = HarnessConfig::new("claude");
            let cwd = Path::new("/repo");

            let configured = config.launch_line(cwd, "feat-x", Some("fix it"), None).unwrap();
            assert!(configured.starts_with("cd '/repo' && claude --append-system-prompt "), "{configured:?}");
            assert!(!configured.ends_with('\n'), "the backend submits the line, so it carries no newline");

            // Naming the configured harness is not an override.
            assert_eq!(config.launch_line(cwd, "feat-x", Some("fix it"), Some("claude")).unwrap(), configured);

            let other = config.launch_line(cwd, "feat-x", Some("fix it"), Some("codex")).unwrap();
            assert!(other.starts_with("cd '/repo' && codex -c "), "{other:?}");
            assert!(!other.contains("--append-system-prompt"), "{other:?}");
        });
    }

    // Status hooks.

    const REPORT: &str = "'/opt/kanstack' report";

    fn claude_settings() -> serde_json::Value {
        let extras = for_command("claude").status_hooks(REPORT, "feat-x").expect("claude takes hooks at launch");
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

    /// Only harnesses with a launch-time route are handed hooks; kanstack does not guess at
    /// the others'.
    #[test]
    fn harnesses_without_a_known_launch_time_route_get_no_hooks() {
        for name in ["codex", "pi", "opencode", "kiro", "gemini", "./my-wrapper.sh"] {
            assert_eq!(for_command(name).status_hooks(REPORT, "feat-x"), None, "{name}");
        }
    }

    /// A directory holding stand-ins named `claude` and `codex` that print the environment
    /// variable hooks rely on, then each argument they were started with.
    fn stand_in_harnesses(tag: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kanstack-harness-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["claude", "codex"] {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$KANSTACK_BRANCH\"\nfor a in \"$@\"; do printf '%s\\0' \"$a\"; done\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir
    }

    /// Runs the launch line for `config` under `sh`, as a pane's shell would, and returns
    /// `$KANSTACK_BRANCH` as the harness saw it and the arguments it was started with. The
    /// line is too long to type once it carries hooks, so this also runs the spill-to-files
    /// path for real.
    fn launched(config: &HarnessConfig, harness: Option<&str>, message: Option<&str>) -> (String, Vec<String>) {
        // `sh` and `cat` are found through `PATH`, which the backends' discovery tests swap.
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let line = config.launch_line(Path::new("/tmp"), "feat-x", message, harness).unwrap();
        let out = std::process::Command::new("sh").arg("-c").arg(&line).output().unwrap();
        assert!(out.status.success(), "{line:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8(out.stdout).unwrap();
        let (branch, args) = stdout.split_once('\n').unwrap();
        let mut args: Vec<String> = args.split('\0').map(str::to_string).collect();
        assert_eq!(args.pop().as_deref(), Some(""), "every argument ends in a NUL");
        (branch.to_string(), args)
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

    #[test]
    fn without_a_reporter_the_harness_gets_no_hooks_and_no_environment() {
        with_system_flag_env(None, || {
            let dir = stand_in_harnesses("plain");
            let config = HarnessConfig::new(dir.join("claude").to_string_lossy().into_owned());
            let (branch, args) = launched(&config, None, Some("fix it"));
            assert_eq!(branch, "");
            assert!(!args.iter().any(|a| a == "--settings"), "{args:?}");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// `--agent codex` from a claude-configured board must not be handed claude's flags, and
    /// the other way round.
    #[test]
    fn hooks_follow_the_harness_actually_launched_not_the_configured_one() {
        with_system_flag_env(None, || {
            let dir = stand_in_harnesses("override");
            let claude = dir.join("claude").to_string_lossy().into_owned();
            let codex = dir.join("codex").to_string_lossy().into_owned();

            let config = HarnessConfig::new(claude.clone()).with_reporter(REPORT);
            let (branch, args) = launched(&config, Some(&codex), Some("fix it"));
            assert_eq!(branch, "", "codex takes no hooks, so no lane variable either");
            assert!(!args.iter().any(|a| a == "--settings"), "{args:?}");

            let config = HarnessConfig::new(codex).with_reporter(REPORT);
            let (branch, args) = launched(&config, Some(&claude), Some("fix it"));
            assert_eq!(branch, "feat-x");
            assert!(args.iter().any(|a| a == "--settings"), "{args:?}");
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn the_reporter_points_at_the_running_executable_unless_turned_off() {
        let exe = Path::new("/Applications/My Tools/kanstack");
        assert_eq!(reporter_command(Some(exe), None).as_deref(), Some("'/Applications/My Tools/kanstack' report"));
        assert_eq!(reporter_command(Some(exe), Some("1")).as_deref(), Some("'/Applications/My Tools/kanstack' report"));
        for off in ["0", "off", "OFF", "false", "no", " off "] {
            assert_eq!(reporter_command(Some(exe), Some(off)), None, "{off:?}");
        }
        assert_eq!(reporter_command(None, None), None, "nothing to point the hooks at");
    }
}
