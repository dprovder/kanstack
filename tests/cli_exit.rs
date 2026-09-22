//! End-to-end checks of the compiled `kanstack` binary's `--json`/exit-code contract
//! (`cli::dispatch`, `cli::exit`) — as opposed to `tests/live.rs`, these need no `but`
//! installed and are not `#[ignore]`d, since the scenarios here never reach a subprocess of
//! their own.

use std::path::PathBuf;
use std::process::Command;

/// A repository-shaped nothing: kanstack subcommands never need a real git repo to fail an
/// argument check, but `-C` still wants a directory that exists.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("kanstack-cli-exit-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Sandbox { root }
    }

    fn kanstack(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_kanstack"))
            .args(["-C", self.root.to_str().unwrap()])
            .args(args)
            .env("KANSTACK_STATE_PATH", self.root.join("state"))
            // Whatever pane this test binary itself happens to be running in must not leak
            // in as a detected split backend — these scenarios are meant to run the same
            // whether or not the harness driving `cargo test` is itself inside tmux/cmux/Orca.
            .env_remove("TMUX_PANE")
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("ORCA_TERMINAL_HANDLE")
            .env_remove("TERM_PROGRAM")
            .env_remove("KANSTACK_SPLIT_BACKEND")
            .output()
            .expect("kanstack must run")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn an_empty_registry_exits_zero_under_json() {
    let sandbox = Sandbox::new("status-ok");
    let out = sandbox.kanstack(&["status", "--json"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "{\"schema\":1,\"workstreams\":[],\"workspace\":null,\"workspace_blocked\":null}\n"
    );
    assert!(out.stderr.is_empty());
}

/// A bad-arguments failure (never reaching `cli::run`, let alone a `Command`) exits `2` and,
/// under `--json`, prints the generic error envelope with the multi-subcommand `HELP` text
/// trimmed off its `message` — full usage belongs in the human path, not repeated in JSON.
#[test]
fn a_parse_error_exits_two_and_json_mode_trims_the_help_dump_from_the_message() {
    let sandbox = Sandbox::new("parse-error");

    let json = sandbox.kanstack(&["spawn", "--json"]);
    assert_eq!(json.status.code(), Some(2));
    assert!(json.stderr.is_empty(), "json mode writes nothing to stderr");
    assert_eq!(
        String::from_utf8(json.stdout).unwrap(),
        "{\"schema\":1,\"ok\":false,\"command\":\"spawn\",\"error\":{\"code\":\"invalid_arguments\",\"message\":\"`kanstack spawn` needs a <branch>\"}}\n"
    );

    let human = sandbox.kanstack(&["spawn"]);
    assert_eq!(human.status.code(), Some(2));
    assert!(human.stdout.is_empty(), "human mode writes nothing to stdout");
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert!(stderr.starts_with("Error: `kanstack spawn` needs a <branch>\n\nkanstack spawn"), "{stderr}");
}

/// A target naming no registered workstream exits `3`, with `unknown_workstream` as the
/// `--json` error code. `stop` rather than `focus`/`send`: it resolves the target before ever
/// touching a split backend (nothing to close if there's no workstream), so this scenario
/// doesn't also depend on a multiplexer being reachable — `cli::tests` covers that case
/// (against a stand-in one) separately.
#[test]
fn an_unknown_workstream_exits_three() {
    let sandbox = Sandbox::new("unknown-workstream");
    let out = sandbox.kanstack(&["stop", "ghost", "--json"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "{\"schema\":1,\"ok\":false,\"command\":\"stop\",\"error\":{\"code\":\"unknown_workstream\",\"message\":\"no workstream for \\\"ghost\\\" — `kanstack status` lists them\"}}\n"
    );
}
