use std::path::{Path, PathBuf};

use crate::harness::launch::{LaunchExtras, NoteDelivery};
use crate::harness::Harness;

/// No `--append-system-prompt` exists (a request for exactly that, openai/codex#11117, is
/// closed unimplemented); `-c developer_instructions=<toml>` is the closest equivalent, a
/// differently-*shaped* mechanism (TOML-quoted, and a "developer" message rather than
/// literally the system prompt), not just a different flag name.
///
/// **`status_hooks`: wired for `busy`/`idle`/`waiting` reporting, still not for the
/// claim-check hook.** The reason this was unwired at all — confirmed against the actual
/// `openai/codex` source (`codex-rs/hooks`, `codex-rs/config`), not just the docs site —
/// wasn't a feature flag, it was per-hook **trust review**: `codex-rs/hooks/src/engine/
/// discovery.rs`'s `hook_hash` fingerprints each hook *handler* (event name, matcher, and
/// that one handler's normalized config — command string included) into a `current_hash`,
/// looked up against a `trusted_hash` persisted by key (`hook_key`, keyed by source file +
/// event + position) the first time a human approves it via the TUI's startup review
/// (`tui/src/startup_hooks_review.rs`, `write_hook_trusts`). Since every other harness here
/// bakes the branch name literally into the hook command, the hash — and so the review
/// prompt — was different on every single `kanstack spawn`. That's fixed by making the
/// command itself branch-agnostic (see below): the same command means the same hash forever,
/// so trust granted once (per repository, since the hash also folds in the *source file*)
/// covers every future spawn, not just the branch that happened to trigger the first prompt.
///
/// The command is branch-agnostic because, unlike Claude (see `Claude::status_hooks`'s own
/// doc comment — its hook subprocesses do not see `KANSTACK_BRANCH` even though the launched
/// process does), a Codex hook subprocess *does* inherit it. `codex-rs/hooks/src/engine/
/// command_runner.rs`'s `build_command` explicitly "replay[s] the session snapshot instead of
/// inheriting the live process environment" — `std::env::vars_os()` captured once when the
/// session starts (`registry.rs`'s `Hooks::new`), i.e. exactly the environment `codex` itself
/// was launched with, only scrubbed of a short, unrelated non-inheritable list (auth/identity
/// tokens; see `codex_protocol::shell_environment::NON_INHERITABLE_ENV_VARS`). So `kanstack
/// report busy`/`idle`/`waiting`, with no branch argument at all, reaches `$KANSTACK_BRANCH`
/// the same way it already does for a human typing `kanstack report` by hand (see
/// `cli::report_cmd`'s existing fallback) — `crate::harness::mod::HarnessConfig::launch_line`
/// already adds `KANSTACK_BRANCH` to the launch line whenever `status_hooks` returns `Some`,
/// so nothing else needed to change for that half.
///
/// There is still no CLI flag to hand Codex a hook at launch (confirmed: `-c` takes scalar
/// dotted overrides, and representing a hook array through it is undocumented and untested,
/// same conclusion as before). Hooks are file-only — `<project-root>/.codex/hooks.json` is the
/// project-local layer (`ConfigLayerSource::Project`, resolved the same way Codex resolves its
/// own project root: walking up from `cwd` for a `.git` marker, its own default
/// `project_root_markers`), so [`Codex::status_hooks`] writes there directly, merging into
/// whatever's already in the file rather than overwriting it (same spirit as
/// [`super::gemini::Gemini`]'s settings file, adapted to a file kanstack can't isolate behind
/// its own env-var-pointed tier the way Gemini's `GEMINI_CLI_SYSTEM_SETTINGS_PATH` does — Codex
/// has nothing equivalent, so this really does write into the repository's own `.codex/`
/// directory). Unlike Gemini's per-branch file, this one is shared and identical across every
/// branch (the whole point of a branch-agnostic command), so there is nothing to prune per
/// branch and no per-repository proliferation the way `gemini_hooks_dir` has.
///
/// One prerequisite this doesn't touch or bypass: Codex also gates project-local `.codex/`
/// config behind its own workspace-trust decision (`codex-rs/config/src/loader/mod.rs`'s
/// `ProjectTrustContext`/`decision_for_dir`), separate from and prior to per-hook trust review.
/// That's a one-time-per-directory prompt inherent to using *any* project-local Codex config —
/// not something the stable-command fix here introduces or can route around, and not
/// re-triggered by hook content changes the way per-hook trust was.
///
/// **Still not wired: the claim-check hook (`kanstack claim`, see `crate::cli::claim`).** The
/// trust-review blocker above is solved, but a second, unrelated one from the original
/// research is not: `apply_patch` (Codex's one and only edit tool) hands `tool_input.command`
/// the raw patch text in Codex's own diff format (confirmed again here —
/// `codex-rs/hooks/src/events/pre_tool_use.rs`'s `command_input_json` doc comment: "shell-like
/// tools pass `{ "command": ... }` as `tool_input`", and `apply_patch` is exactly one of
/// those), not a `file_path` field. `cli::claim::file_path_from_hook_payload` would find
/// nothing to check on every single call, so wiring the hook now would install one that can
/// never actually deny an edit — worse than not wiring it, since it would look like the same
/// cross-lane protection Claude/Gemini have while silently providing none. That needs a
/// patch-format parser, real work out of scope here (see this crate's own docs on that
/// division of labor) — not something a stable command can fix.
pub struct Codex;

/// Where the project-local hook config lives, relative to the repository root Codex itself
/// resolves to (see [`project_root`]).
const HOOKS_RELATIVE_PATH: [&str; 2] = [".codex", "hooks.json"];

impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn note_delivery(&self) -> NoteDelivery {
        NoteDelivery::CodexConfig
    }

    fn status_hooks(&self, report: &str, _branch: &str, cwd: &Path) -> Option<LaunchExtras> {
        let path = hooks_json_path(cwd);
        std::fs::create_dir_all(path.parent()?).ok()?;
        let existing = std::fs::read_to_string(&path).ok();
        let merged = merge_hooks_json(existing.as_deref(), report);
        std::fs::write(&path, merged).ok()?;
        Some(LaunchExtras::default())
    }
}

/// `<project-root>/.codex/hooks.json` — `project_root` found the same way Codex's own
/// `project_root_markers` default does (walking up from `cwd` for the nearest `.git`), so
/// kanstack writes to exactly the file Codex will read, even when `cwd` is a subdirectory of
/// the repository rather than its root.
fn hooks_json_path(cwd: &Path) -> PathBuf {
    let mut path = project_root(cwd);
    for segment in HOOKS_RELATIVE_PATH {
        path.push(segment);
    }
    path
}

fn project_root(cwd: &Path) -> PathBuf {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    canonical.ancestors().find(|dir| dir.join(".git").exists()).map(Path::to_path_buf).unwrap_or(canonical)
}

/// kanstack's own three hook entries: `UserPromptSubmit`/`Stop` bracket a turn the same way
/// they do for Claude, and `PermissionRequest` reports `waiting` — a real, confirmed signal
/// for Codex specifically (unlike Gemini's unverified `Notification`), per the struct doc's
/// prior research. No branch anywhere in any of the three commands — see the struct doc for
/// why that's the entire point. `matcher: null` on all three: none is tool-scoped.
fn kanstack_hook_events(report: &str) -> serde_json::Value {
    let entry = |command: String| {
        serde_json::json!({ "matcher": null, "hooks": [{ "type": "command", "command": command, "timeout": 5 }] })
    };
    serde_json::json!({
        "UserPromptSubmit": [entry(format!("{report} busy"))],
        "Stop": [entry(format!("{report} idle"))],
        "PermissionRequest": [entry(format!("{report} waiting"))],
    })
}

/// Merges [`kanstack_hook_events`] into `existing` (this repository's current
/// `.codex/hooks.json`, if any) rather than overwriting it, so a user's own project hooks — or
/// ones from a previous kanstack run — survive. Adds only entries that aren't already present
/// (compared by full equality, so a hook this exact shape is never duplicated), which makes
/// this idempotent: re-running it with the same `report` on an already-merged file reproduces
/// the same bytes, so writing it on every `kanstack spawn` doesn't perturb the hash Codex's
/// trust review keys on. Malformed or unexpectedly-shaped existing content is not preserved
/// verbatim past what can be salvaged — the `hooks` object (or the whole document) is replaced
/// with an empty one rather than this failing outright, the same "never let a status-hook
/// side effect break the spawn it's attached to" spirit `status_hooks`'s `Option` return
/// already carries.
fn merge_hooks_json(existing: Option<&str>, report: &str) -> String {
    let mut doc = existing
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));

    if !doc.get("hooks").is_some_and(serde_json::Value::is_object) {
        doc["hooks"] = serde_json::json!({});
    }
    let hooks = doc["hooks"].as_object_mut().expect("just ensured this is an object");

    let wanted = kanstack_hook_events(report);
    for (event, groups) in wanted.as_object().expect("kanstack_hook_events always returns an object") {
        if !hooks.get(event).is_some_and(serde_json::Value::is_array) {
            hooks.insert(event.clone(), serde_json::json!([]));
        }
        let existing_groups = hooks[event].as_array_mut().expect("just ensured this is an array");
        for group in groups.as_array().expect("kanstack_hook_events groups are always arrays") {
            if !existing_groups.contains(group) {
                existing_groups.push(group.clone());
            }
        }
    }

    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::{with_system_flag_env, REPORT};
    use crate::harness::resolve_note_delivery;

    #[test]
    fn resolve_note_delivery_uses_codex_config_for_codex() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("codex"), NoteDelivery::CodexConfig);
        });
    }

    // `merge_hooks_json`.

    #[test]
    fn merge_hooks_json_creates_the_three_events_from_nothing() {
        let written = super::merge_hooks_json(None, REPORT);
        let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
        let hooks = doc["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 3, "no claim-check hook — see the module doc's why not: {:?}", hooks.keys().collect::<Vec<_>>());

        let says = |event: &str| {
            let entry = &hooks[event][0];
            assert!(entry["matcher"].is_null(), "{event}");
            let hook = &entry["hooks"][0];
            assert_eq!(hook["type"], "command", "{event}");
            assert_eq!(hook["timeout"], 5, "{event}");
            hook["command"].as_str().unwrap().strip_prefix("'/opt/kanstack' report ").unwrap().to_string()
        };
        assert_eq!(says("UserPromptSubmit"), "busy", "no branch anywhere in the command");
        assert_eq!(says("Stop"), "idle");
        assert_eq!(says("PermissionRequest"), "waiting");
    }

    #[test]
    fn merge_hooks_json_is_byte_identical_on_a_second_run() {
        let first = super::merge_hooks_json(None, REPORT);
        let second = super::merge_hooks_json(Some(&first), REPORT);
        assert_eq!(first, second, "re-running on an already-merged file must not perturb the trust hash");
    }

    #[test]
    fn merge_hooks_json_keeps_a_users_own_hooks_and_description() {
        let existing = r#"{
            "description": "my own hooks",
            "hooks": { "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo mine"}]}] }
        }"#;
        let written = super::merge_hooks_json(Some(existing), REPORT);
        let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(doc["description"], "my own hooks");
        assert_eq!(doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "echo mine");
        assert_eq!(doc["hooks"]["Stop"][0]["hooks"][0]["command"], "'/opt/kanstack' report idle");
    }

    #[test]
    fn merge_hooks_json_does_not_duplicate_its_own_entries_across_report_binaries_that_agree() {
        let first = super::merge_hooks_json(None, REPORT);
        let written = super::merge_hooks_json(Some(&first), REPORT);
        let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn merge_hooks_json_recovers_from_a_malformed_existing_file() {
        let written = super::merge_hooks_json(Some("{ not json"), REPORT);
        let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(doc["hooks"]["Stop"][0]["hooks"][0]["command"], "'/opt/kanstack' report idle");
    }

    // `project_root`/`hooks_json_path`.

    #[test]
    fn hooks_json_path_walks_up_to_the_nearest_git_ancestor() {
        let dir = std::env::temp_dir().join(format!("kanstack-codex-hooks-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();

        let from_root = super::hooks_json_path(&dir);
        let from_subdir = super::hooks_json_path(&dir.join("sub/deeper"));
        assert_eq!(from_root, from_subdir);
        // `canonicalize`, not the raw `dir`: on macOS `std::env::temp_dir()` sits under a
        // `/var` that is itself a symlink to `/private/var`, which `hooks_json_path` resolves
        // through (it canonicalizes before walking for `.git`).
        assert_eq!(from_root, dir.canonicalize().unwrap().join(".codex").join("hooks.json"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    // Status hooks end to end.

    #[test]
    fn status_hooks_writes_the_project_local_hooks_file_and_needs_no_extras() {
        let dir = std::env::temp_dir().join(format!("kanstack-codex-hooks-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        let extras = crate::harness::for_command("codex")
            .status_hooks(REPORT, "feat-x", &dir)
            .expect("codex takes hooks at launch, via a written project-local file");
        assert!(extras.args.is_empty(), "everything goes through the file, not extra flags");
        assert!(extras.env.is_empty(), "KANSTACK_BRANCH is added by HarnessConfig::launch_line itself");

        let written = std::fs::read_to_string(dir.join(".codex").join("hooks.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"], "'/opt/kanstack' report busy");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two branches launching in the same repository share the exact same file — unlike
    /// Gemini's per-branch settings, there is nothing branch-specific to keep apart.
    #[test]
    fn status_hooks_gives_every_branch_in_the_same_repo_the_same_file() {
        let dir = std::env::temp_dir().join(format!("kanstack-codex-hooks-shared-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        crate::harness::for_command("codex").status_hooks(REPORT, "feat-a", &dir).unwrap();
        let after_a = std::fs::read_to_string(dir.join(".codex").join("hooks.json")).unwrap();
        crate::harness::for_command("codex").status_hooks(REPORT, "feat-b", &dir).unwrap();
        let after_b = std::fs::read_to_string(dir.join(".codex").join("hooks.json")).unwrap();
        assert_eq!(after_a, after_b, "the command carries no branch, so the file never needs to differ");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The full launch line: `KANSTACK_BRANCH` reaches Codex's own process environment (and so,
    /// per the struct doc, its hook subprocesses too) via the same `LaunchExtras.env` wiring
    /// every other hooked harness uses — nothing Codex-specific needed for that half.
    #[test]
    fn a_configured_reporter_hooks_codex_and_names_the_lane_in_the_environment() {
        use crate::harness::HarnessConfig;

        with_system_flag_env(None, || {
            let repo = std::env::temp_dir().join(format!("kanstack-codex-hooks-launch-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&repo);
            std::fs::create_dir_all(repo.join(".git")).unwrap();

            let config = HarnessConfig::new("codex").with_reporter(REPORT);
            let line = config.launch_line(&repo, "feat-x", Some("fix it"), None).unwrap();
            assert!(line.contains("KANSTACK_BRANCH='feat-x'"), "{line:?}");

            let _ = std::fs::remove_dir_all(&repo);
        });
    }
}
