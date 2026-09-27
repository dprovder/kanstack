use std::path::{Path, PathBuf};

use crate::harness::launch::{toml_quote, LaunchExtras, NoteDelivery};
use crate::harness::{Effort, Harness};

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

    /// Codex has real `-c key=value` config overrides for both of these — `model`, its own
    /// top-level model selector, and `model_reasoning_effort`, its actual reasoning-effort
    /// config key (confirmed against `codex-rs/config`, not guessed from the docs site) —
    /// so unlike the trait's default (which forwards `model` as a generic `--model` flag and
    /// drops `effort`), Codex maps *both* hints through the same mechanism its note delivery
    /// already uses (see `NoteDelivery::CodexConfig` and [`toml_quote`]): one `-c` argument
    /// per override, each a TOML-quoted string value, `low`/`medium`/`high` passed straight
    /// through unchanged since those are exactly the words Codex's own config expects.
    fn model_effort_args(&self, model: Option<&str>, effort: Option<Effort>) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(model) = model {
            args.push("-c".to_string());
            args.push(format!("model={}", toml_quote(model)));
        }
        if let Some(effort) = effort {
            args.push("-c".to_string());
            args.push(format!("model_reasoning_effort={}", toml_quote(&effort.to_string())));
        }
        args
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

/// One entry from Codex's own locally cached model catalog (see [`cached_models`]) — just
/// the two fields kanstack's model/effort picker (`crate::app::branch_modal`) needs, not the
/// whole schema Codex itself keeps (pricing tier, context window, available plans, and so
/// on, all irrelevant here). `#[serde(default)]` on every field but `slug` so an older or
/// newer cache — Codex's own schema is free to add fields kanstack doesn't know about yet —
/// still parses instead of failing the whole read over one missing key.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CachedModel {
    /// What `-c model=` (see [`Codex::model_effort_args`]) actually takes — confirmed
    /// against Codex's own docs, which say to use the slug, not a fully-qualified name.
    pub slug: String,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<SupportedReasoningLevel>,
}

/// One of a [`CachedModel`]'s valid `model_reasoning_effort` values — `effort` is confirmed
/// to use the same three words (`low`/`medium`/`high`) [`Effort`] does, at least for every
/// model seen so far; `description` is kept only because it costs nothing to, not used yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct SupportedReasoningLevel {
    pub effort: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ModelsCache {
    #[serde(default)]
    models: Vec<CachedModel>,
}

/// Codex's own locally cached model catalog — `$CODEX_HOME/models_cache.json`
/// (`$CODEX_HOME` resolved the same way Codex's own CLI does: itself if set, else
/// `$HOME/.codex`), a plain JSON file `codex-rs/models-manager`'s `ModelsManager` writes
/// after fetching the live list from OpenAI's own API, confirmed against that module's own
/// source rather than guessed at: a `fetched_at`/`etag`/`client_version` for staleness
/// alongside a `models` array, each entry's `slug` exactly what [`Codex::model_effort_args`]
/// hands `-c model=`, and a `supported_reasoning_levels` list this crate has no other way to
/// learn (Codex has no CLI flag to list models — see this module's own top-level doc comment
/// for what was and wasn't found there). Reading the file directly, rather than shelling out
/// to `codex` for it, costs nothing and needs no network access of kanstack's own.
///
/// Best-effort, the same spirit as [`Codex::status_hooks`]'s own `Option` return: a missing
/// file (Codex never run, or never run online, on this machine), an unparseable one, or no
/// home directory to look in at all reads as "nothing cached" — an empty list — rather than
/// an error. This is a convenience suggestion for the model/effort picker, not something
/// launching a pane depends on; every call site here already treats an empty list the same
/// as "no suggestions to offer," same as before this existed.
pub fn cached_models() -> Vec<CachedModel> {
    let Some(path) = models_cache_path() else { return Vec::new() };
    let Ok(raw) = std::fs::read_to_string(path) else { return Vec::new() };
    serde_json::from_str::<ModelsCache>(&raw).map(|c| c.models).unwrap_or_default()
}

/// `$CODEX_HOME/models_cache.json`, `$CODEX_HOME` defaulting to `$HOME/.codex` — the same
/// resolution Codex's own CLI uses (confirmed against `codex-rs`'s own `DefaultHome`), so
/// this reads exactly the file a locally installed Codex would have written. `None` only
/// when neither `$CODEX_HOME` nor `$HOME` can be resolved at all.
fn models_cache_path() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))?;
    Some(home.join("models_cache.json"))
}

#[cfg(test)]
mod tests {
    use crate::harness::launch::NoteDelivery;
    use crate::harness::test_support::{with_system_flag_env, REPORT};
    use crate::harness::resolve_note_delivery;
    use crate::harness::Harness;

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
            let line = config.launch_line(&repo, "feat-x", Some("fix it"), None, None, None).unwrap();
            assert!(line.contains("KANSTACK_BRANCH='feat-x'"), "{line:?}");

            let _ = std::fs::remove_dir_all(&repo);
        });
    }

    // `model_effort_args`.

    #[test]
    fn model_effort_args_maps_model_and_effort_to_their_real_codex_config_keys() {
        let args = super::Codex.model_effort_args(Some("o3"), Some(crate::harness::Effort::High));
        assert_eq!(args, ["-c", r#"model="o3""#, "-c", r#"model_reasoning_effort="high""#]);
    }

    #[test]
    fn model_effort_args_omits_whichever_of_the_two_was_not_given() {
        assert_eq!(super::Codex.model_effort_args(Some("o3"), None), ["-c", r#"model="o3""#]);
        assert_eq!(
            super::Codex.model_effort_args(None, Some(crate::harness::Effort::Low)),
            ["-c", r#"model_reasoning_effort="low""#]
        );
        assert!(super::Codex.model_effort_args(None, None).is_empty());
    }

    /// The full launch line: each `-c` override reaches Codex as its own shell word, TOML
    /// quoting intact, alongside the note's own `-c developer_instructions=...`.
    #[test]
    fn a_launch_line_carries_model_and_effort_as_their_own_c_arguments() {
        use crate::harness::HarnessConfig;

        with_system_flag_env(None, || {
            let repo = std::env::temp_dir().join(format!("kanstack-codex-model-effort-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&repo);
            std::fs::create_dir_all(repo.join(".git")).unwrap();

            let config = HarnessConfig::new("codex");
            let line = config
                .launch_line(&repo, "feat-x", Some("fix it"), None, Some("o3"), Some(crate::harness::Effort::Medium))
                .unwrap();
            assert!(line.contains(r#"'-c' 'model="o3"'"#), "{line:?}");
            assert!(line.contains(r#"'-c' 'model_reasoning_effort="medium"'"#), "{line:?}");

            let _ = std::fs::remove_dir_all(&repo);
        });
    }

    // `cached_models`/`models_cache_path`.

    /// Runs `body` with `CODEX_HOME` pointed at a fresh temp directory (and `HOME` cleared,
    /// so a stray real `~/.codex` on the machine running this test can never leak in),
    /// restoring both afterward. Holds the same env-mutation lock every other test that
    /// touches process environment here shares, so two of these can't interleave.
    fn with_codex_home(tag: &str, body: impl FnOnce(&std::path::Path)) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("kanstack-codex-home-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let old_home = std::env::var_os("CODEX_HOME");
        std::env::set_var("CODEX_HOME", &dir);
        body(&dir);
        match old_home {
            Some(v) => std::env::set_var("CODEX_HOME", v),
            None => std::env::remove_var("CODEX_HOME"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shape `codex-rs`'s own `ModelsManager` writes: a `models` array alongside cache
    /// bookkeeping (`fetched_at`, `etag`, `client_version`) this crate has no use for and
    /// must tolerate rather than choke on.
    fn sample_cache() -> &'static str {
        r#"{
            "fetched_at": "2026-01-01T00:00:00Z",
            "etag": "abc123",
            "client_version": "0.153.0",
            "models": [
                {
                    "slug": "gpt-6-sol",
                    "display_name": "GPT-6-Sol",
                    "supported_reasoning_levels": [
                        {"effort": "low", "description": "fast"},
                        {"effort": "medium", "description": "balanced"}
                    ]
                },
                {
                    "slug": "gpt-6-astra",
                    "supported_reasoning_levels": [
                        {"effort": "high", "description": "frontier"}
                    ]
                }
            ]
        }"#
    }

    #[test]
    fn cached_models_reads_every_slug_and_its_supported_reasoning_levels() {
        with_codex_home("read", |dir| {
            std::fs::write(dir.join("models_cache.json"), sample_cache()).unwrap();
            let models = super::cached_models();
            assert_eq!(models.len(), 2);
            assert_eq!(models[0].slug, "gpt-6-sol");
            assert_eq!(models[0].supported_reasoning_levels[0].effort, "low");
            assert_eq!(models[1].slug, "gpt-6-astra");
            assert_eq!(models[1].supported_reasoning_levels[0].effort, "high");
        });
    }

    /// Codex is free to add fields to its own schema (pricing, plan gating, and so on) that
    /// this crate has never heard of — `#[serde(default)]`/plain field skipping must not
    /// choke on them.
    #[test]
    fn cached_models_ignores_fields_it_does_not_know_about() {
        with_codex_home("extra-fields", |dir| {
            let raw = r#"{"models":[{"slug":"gpt-6-sol","priority":1,"context_window":272000,"available_in_plans":["pro"]}]}"#;
            std::fs::write(dir.join("models_cache.json"), raw).unwrap();
            let models = super::cached_models();
            assert_eq!(models, vec![super::CachedModel { slug: "gpt-6-sol".to_string(), supported_reasoning_levels: vec![] }]);
        });
    }

    /// No file at all — Codex has never run, or never run online, on this machine — reads as
    /// an empty list, not an error: this is a convenience suggestion, nothing depends on it.
    #[test]
    fn cached_models_with_no_cache_file_is_an_empty_list_not_an_error() {
        with_codex_home("missing", |_dir| {
            assert!(super::cached_models().is_empty());
        });
    }

    #[test]
    fn cached_models_with_a_malformed_cache_file_is_an_empty_list_not_a_panic() {
        with_codex_home("malformed", |dir| {
            std::fs::write(dir.join("models_cache.json"), "{ not json").unwrap();
            assert!(super::cached_models().is_empty());
        });
    }
}
