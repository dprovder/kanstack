//! Small env-var-seeding config file, written by `crate::setup`'s wizard.
//!
//! kanstack's own tunables are still read fresh from the environment at every call site
//! (see `splitter.rs`, `cmux.rs`, `tmux.rs`, `harness_launch.rs`, `app.rs`) — nothing here
//! changes that. This module only seeds the process environment, once, at startup, for
//! whichever of those vars aren't already set, so a choice made once in `--setup` sticks
//! across sessions without a shell profile edit. An explicit env var always wins:
//! `load_into_env` only fills gaps left by the real environment.

use std::io::Write;
use std::path::PathBuf;

/// Every `KEY=value` line the config file may set. `load_into_env`/`save` only ever touch
/// these — anything else found in the file is ignored, so a typo'd key silently does
/// nothing rather than mysteriously reaching some unrelated part of the app.
const KEYS: &[&str] = &[
    "KANSTACK_HARNESS",
    "KANSTACK_SPLIT_BACKEND",
    "KANSTACK_BRANCH_UI",
    "KANSTACK_SPAWN_DIRECTION",
    "KANSTACK_STACK_PANES",
];

/// Where the config file lives: `KANSTACK_CONFIG_PATH` if set — also the test seam, so
/// tests point this at a temp dir and never touch a real `~/.config` — else
/// `$XDG_CONFIG_HOME/kanstack/env`, else `$HOME/.config/kanstack/env`. `None` only when
/// none of those three can be resolved (no home directory at all).
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KANSTACK_CONFIG_PATH") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("kanstack").join("env"))
}

/// Whether the config file has ever been written — `main.rs`'s first-run trigger for
/// `--setup`.
pub fn exists() -> bool {
    config_path().is_some_and(|p| p.exists())
}

/// Reads the config file, if any, and seeds the process environment with every key in it
/// that isn't already set. Not an error if the file is missing — every run before
/// `--setup` has ever completed has none.
pub fn load_into_env() {
    let Some(path) = config_path() else { return };
    let Ok(raw) = std::fs::read_to_string(&path) else { return };
    for (key, value) in parse(&raw) {
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, value);
        }
    }
}

fn parse(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| KEYS.contains(k))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

/// Writes `fields` to the config file, creating its parent directory if needed, and seeds
/// the current process's environment with each one immediately, so it's picked up without
/// re-reading the file. Always writes the file, even given an empty `fields` (all left on
/// "auto") — so `exists()` never lets the first-run wizard trigger a second time.
pub fn save(fields: &[(&str, String)]) -> anyhow::Result<()> {
    let path = config_path().ok_or_else(|| anyhow::anyhow!("no home directory to save into"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::from(
        "# written by `kanstack --setup` — edit freely, or delete this file to see the wizard again.\n",
    );
    for (key, value) in fields {
        debug_assert!(KEYS.contains(key), "saving an unrecognized key {key:?}");
        out.push_str(key);
        out.push('=');
        out.push_str(value);
        out.push('\n');
        std::env::set_var(key, value);
    }
    std::fs::File::create(&path)?.write_all(out.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same pattern as `splitter.rs`'s own `with_env`: snapshot and restore every var
    /// listed, held for the whole call via the crate-wide lock so this can't interleave
    /// with `splitter.rs`/`cmux.rs`/`tmux.rs`'s own env-mutating tests — `KANSTACK_HARNESS`
    /// and `KANSTACK_SPLIT_BACKEND` overlap with the surface those already guard.
    fn with_env(vars: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        body();
        for (k, v) in old {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    fn temp_config_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "kanstack-config-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn load_into_env_seeds_unset_vars_from_the_file() {
        let path = temp_config_path();
        std::fs::write(&path, "KANSTACK_HARNESS=codex\n# a comment\n\nKANSTACK_BRANCH_UI=footer\n").unwrap();
        with_env(
            &[
                ("KANSTACK_CONFIG_PATH", Some(path.to_str().unwrap())),
                ("KANSTACK_HARNESS", None),
                ("KANSTACK_BRANCH_UI", None),
            ],
            || {
                load_into_env();
                assert_eq!(std::env::var("KANSTACK_HARNESS").unwrap(), "codex");
                assert_eq!(std::env::var("KANSTACK_BRANCH_UI").unwrap(), "footer");
            },
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_already_set_env_var_wins_over_the_file() {
        let path = temp_config_path();
        std::fs::write(&path, "KANSTACK_HARNESS=codex\n").unwrap();
        with_env(
            &[
                ("KANSTACK_CONFIG_PATH", Some(path.to_str().unwrap())),
                ("KANSTACK_HARNESS", Some("claude")),
            ],
            || {
                load_into_env();
                assert_eq!(std::env::var("KANSTACK_HARNESS").unwrap(), "claude");
            },
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_round_trips_through_load_into_env() {
        let path = temp_config_path();
        with_env(
            &[
                ("KANSTACK_CONFIG_PATH", Some(path.to_str().unwrap())),
                ("KANSTACK_HARNESS", None),
                ("KANSTACK_SPLIT_BACKEND", None),
            ],
            || {
                save(&[
                    ("KANSTACK_HARNESS", "pi".to_string()),
                    ("KANSTACK_SPLIT_BACKEND", "tmux".to_string()),
                ])
                .unwrap();
                assert!(exists());
                // `save` already seeds the current process; clear it to prove the file
                // itself, not the earlier set_var, is what `load_into_env` reads back.
                std::env::remove_var("KANSTACK_HARNESS");
                std::env::remove_var("KANSTACK_SPLIT_BACKEND");
                load_into_env();
                assert_eq!(std::env::var("KANSTACK_HARNESS").unwrap(), "pi");
                assert_eq!(std::env::var("KANSTACK_SPLIT_BACKEND").unwrap(), "tmux");
            },
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_writes_a_file_even_with_no_fields_so_first_run_does_not_retrigger() {
        let path = temp_config_path();
        with_env(&[("KANSTACK_CONFIG_PATH", Some(path.to_str().unwrap()))], || {
            assert!(!exists());
            save(&[]).unwrap();
            assert!(exists());
        });
        let _ = std::fs::remove_file(&path);
    }
}
