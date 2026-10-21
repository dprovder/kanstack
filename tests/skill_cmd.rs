//! End-to-end checks of `kanstack skill install` / `kanstack skill check` against the
//! compiled binary: exit codes, `--json` documents, and where files land. Every run gets a
//! temp `HOME`, a temp config file and an empty `PATH`, so nothing here can read or write the
//! real `~/.claude` — or pick up whichever harnesses happen to be installed on this machine.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("kanstack-skill-cmd-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["repo/.git", "home", "bin"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Sandbox { root }
    }

    fn kanstack(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanstack"))
            .args(["-C", self.root.join("repo").to_str().unwrap()])
            .args(args)
            .env("HOME", self.root.join("home"))
            .env("PATH", self.root.join("bin"))
            .env("KANSTACK_CONFIG_PATH", self.root.join("no-config"))
            .env("KANSTACK_HARNESS", "claude")
            .env_remove("XDG_CONFIG_HOME")
            .stdin(Stdio::null())
            .output()
            .expect("kanstack must run")
    }

    fn json(&self, args: &[&str]) -> (i32, serde_json::Value) {
        let out = self.kanstack(args);
        let doc = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {:?} / {:?}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap(), doc)
    }

    fn local(&self) -> PathBuf {
        self.root.join("repo/.claude/skills/kanstack/SKILL.md")
    }

    fn global(&self) -> PathBuf {
        self.root.join("home/.claude/skills/kanstack/SKILL.md")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn install_writes_a_stamped_skill_into_the_repo_and_global_into_home() {
    let sb = Sandbox::new("install");
    let (code, doc) = sb.json(&["skill", "install", "--json"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["skills"][0]["action"], "created");
    let written = std::fs::read_to_string(sb.local()).unwrap();
    assert!(written.contains(&format!("  version: \"{VERSION}\"\n")), "{written}");
    assert!(!sb.global().exists());

    let (code, doc) = sb.json(&["skill", "install", "--global", "--json"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["skills"][0]["scope"], "global");
    assert!(sb.global().is_file());
}

/// No terminal on stdin means no prompt — a repo install from a script just happens.
#[test]
fn install_without_a_terminal_never_prompts() {
    let sb = Sandbox::new("noprompt");
    let out = sb.kanstack(&["skill", "install"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(sb.local().is_file());
}

#[test]
fn an_unsupported_agent_exits_three_with_its_own_code() {
    let sb = Sandbox::new("unsupported");
    let (code, doc) = sb.json(&["skill", "install", "--agent", "codex", "--json"]);
    assert_eq!(code, 3);
    assert_eq!(doc["ok"], false);
    assert_eq!(doc["command"], "skill install");
    assert_eq!(doc["error"]["code"], "unsupported_harness");
    assert!(doc["error"]["message"].as_str().unwrap().contains("no skill format known for codex yet"));
    assert!(!sb.local().exists());
}

#[test]
fn an_edited_copy_exits_four_unless_forced() {
    let sb = Sandbox::new("edited");
    sb.kanstack(&["skill", "install", "--json"]);
    let edited = std::fs::read_to_string(sb.local()).unwrap() + "\nours\n";
    std::fs::write(sb.local(), &edited).unwrap();

    let (code, doc) = sb.json(&["skill", "install", "--json"]);
    assert_eq!(code, 4);
    assert_eq!(doc["error"]["code"], "skill_modified");
    assert_eq!(std::fs::read_to_string(sb.local()).unwrap(), edited);

    let (code, doc) = sb.json(&["skill", "install", "--force", "--json"]);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["skills"][0]["action"], "replaced");
}

#[test]
fn check_finds_a_stale_copy_and_update_refreshes_it() {
    let sb = Sandbox::new("check");
    sb.kanstack(&["skill", "install", "--global", "--json"]);
    // Rewind the stamp the way an older kanstack would have written it: `--force` from a
    // real old binary isn't available here, so re-render through the library instead.
    let old = kanstack::skill::claude::ClaudeSkill;
    std::fs::write(sb.global(), kanstack::skill::SkillFormat::render(&old, "0.0.1")).unwrap();

    let (code, doc) = sb.json(&["skill", "check", "--json"]);
    assert_eq!(code, 0, "an outdated copy is a report, not a failure");
    assert_eq!(doc["outdated_count"], 1);
    assert_eq!(doc["skills"][0]["installed_version"], "0.0.1");

    let (code, doc) = sb.json(&["skill", "check", "--update", "--json"]);
    assert_eq!(code, 0);
    assert_eq!(doc["outdated_count"], 0);
    assert_eq!(doc["skills"][0]["updated"], true);
    assert_eq!(doc["skills"][0]["installed_version"], VERSION);
}

#[test]
fn a_bad_skill_action_is_an_argument_error() {
    let sb = Sandbox::new("badargs");
    let (code, doc) = sb.json(&["skill", "frobnicate", "--json"]);
    assert_eq!(code, 2);
    assert_eq!(doc["error"]["code"], "invalid_arguments");
}
