//! End-to-end checks of the control loop an external orchestrator is expected to drive: real
//! subprocesses of the compiled `kanstack` binary (`env!("CARGO_BIN_EXE_kanstack")`), talking
//! only `--json`, against stand-in `but`/`tmux` scripts — as close as this suite gets to "an
//! agent operating kanstack with no knowledge of its internals" without a real GitButler repo
//! or terminal multiplexer. Sibling of `tests/cli_exit.rs` (no `but` needed there) and
//! `tests/live.rs` (`#[ignore]`d, needs a real `but`).
//!
//! Each scenario below is task 8 of the "safe for an external orchestrator" milestone (see
//! `docs/ARCHITECTURE.md` and the README's `--json` section):
//! 1. the basic loop — spawn, status, send, status, stop, status — each step's effect visible
//!    at the next status read;
//! 2. concurrent `spawn`s against one registry, formalizing `Registry::with_lock`'s own
//!    acceptance test (`src/workstream.rs`) at the level an orchestrator actually operates at:
//!    real OS processes racing `run()`/`dispatch()`, not one thread calling functions in turn;
//! 3. a pane closed outside kanstack — `status` must call it `Dead`, and `prune` must remove
//!    only it.

use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

/// A throwaway repository directory, plus stand-in `but`/`tmux` binaries kanstack is pointed
/// at via env vars. Both scripts persist their state in a couple of files under `root`, so
/// separate `kanstack` invocations — each its own process, same as a real orchestrator's
/// separate calls — see the same "open panes" ledger a real multiplexer would answer from.
struct Sandbox {
    root: PathBuf,
    but_bin: PathBuf,
    tmux_bin: PathBuf,
    panes: PathBuf,
    tmux_log: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("kanstack-orchestrator-loop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let panes = root.join("panes");
        let tmux_log = root.join("tmux.log");

        // `but --version`/`status` (the only two calls `spawn`/`status` ever need — no test
        // here exercises a branch `but` doesn't already know about failing to be created, or
        // needs a lane's git state) and `branch new`, which `spawn` calls for any branch this
        // fixture doesn't already list — every branch these tests use.
        let but_body = format!(
            "#!/bin/sh\ncase \"$1\" in \
             --version) echo 'but 0.22.3' ;; \
             status) cat '{fixture}' ;; \
             branch) echo '{{}}' ;; \
             esac\n",
            fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/status_no_feat_ui.json"),
        );
        let but_bin = root.join("but");
        std::fs::write(&but_bin, but_body).unwrap();

        // A stand-in tmux: `split-window` invents a pane id from its own pid (unique across
        // the real, separate processes these tests launch) and records it as open;
        // `kill-pane` forgets it; `list-panes` reports whatever is still on the ledger. Every
        // call is logged, so a test can also check exactly what was typed into a pane.
        let tmux_body = format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\ncase \"$1\" in \
             split-window) id=\"%$$\"; echo \"$id $$\" >> '{panes}'; echo \"$id\" ;; \
             kill-pane) target=\"$3\"; grep -v \"^$target \" '{panes}' > '{panes}.tmp' 2>/dev/null; mv '{panes}.tmp' '{panes}' 2>/dev/null ;; \
             list-panes) cat '{panes}' 2>/dev/null ;; \
             esac\n",
            log = tmux_log.display(),
            panes = panes.display(),
        );
        let tmux_bin = root.join("tmux");
        std::fs::write(&tmux_bin, tmux_body).unwrap();

        for bin in [&but_bin, &tmux_bin] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        Sandbox { root, but_bin, tmux_bin, panes, tmux_log }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kanstack"));
        cmd.args(["-C", self.root.to_str().unwrap()])
            .args(args)
            .env("KANSTACK_STATE_PATH", self.root.join("state"))
            .env("KANSTACK_SPLIT_BACKEND", "tmux")
            .env("KANSTACK_TMUX_BIN", &self.tmux_bin)
            .env("KANSTACK_BUT_BIN", &self.but_bin)
            .env("TMUX_PANE", "%0")
            // Whatever pane is actually running `cargo test` itself must not leak in as a
            // detected backend or "own pane" — see `tests/cli_exit.rs`'s `Sandbox` for the
            // same reasoning.
            .env_remove("CMUX_SURFACE_ID")
            .env_remove("ORCA_TERMINAL_HANDLE")
            .env_remove("TERM_PROGRAM");
        cmd
    }

    fn kanstack(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("kanstack must run")
    }

    /// Launches `kanstack args` without waiting for it, with its output piped rather than
    /// inherited — otherwise a concurrently-running child would write straight to this test
    /// process's own stdout/stderr fds, bypassing `cargo test`'s output capture.
    fn spawn_async(&self, args: &[&str]) -> Child {
        self.command(args).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("kanstack must launch")
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.kanstack(args);
        assert!(out.status.success(), "`kanstack {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("not JSON: {e}: {}", String::from_utf8_lossy(&out.stdout)))
    }

    fn status_json(&self) -> serde_json::Value {
        self.json(&["status", "--json"])
    }

    /// Simulates a pane's terminal being closed by something other than kanstack (the user
    /// closing the tab, the multiplexer server restarting) rather than by `kanstack stop`:
    /// struck straight off the stand-in tmux's "open panes" ledger, the way a real
    /// `tmux list-panes` would simply stop mentioning it, with no `kill-pane` call of
    /// kanstack's own in between.
    fn close_pane_externally(&self, pane: &str) {
        let kept: String = std::fs::read_to_string(&self.panes)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with(&format!("{pane} ")))
            .map(|line| format!("{line}\n"))
            .collect();
        std::fs::write(&self.panes, kept).unwrap();
    }

    fn tmux_log(&self) -> Vec<String> {
        std::fs::read_to_string(&self.tmux_log).unwrap_or_default().lines().map(str::to_string).collect()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The workstream named `branch` in a `status --json` document, if it's listed.
fn workstream<'a>(status: &'a serde_json::Value, branch: &str) -> Option<&'a serde_json::Value> {
    status["workstreams"].as_array().unwrap().iter().find(|w| w["branch"] == branch)
}

/// Scenario 1: spawn → status → send → status → stop → status. Every step's effect must be
/// visible the next time `status` is read — a spawned workstream appears, sending leaves it in
/// place but visibly reaches the pane, and stopping makes it disappear entirely.
#[test]
fn the_basic_loop_spawn_status_send_status_stop_status() {
    let sandbox = Sandbox::new("basic-loop");
    let branch = "feat-loop";

    let spawned = sandbox.json(&["spawn", branch, "--agent", "codex", "--json"]);
    assert_eq!(spawned["ok"], true);
    let pane = spawned["result"]["pane"].as_str().unwrap().to_string();

    let after_spawn = sandbox.status_json();
    let ws = workstream(&after_spawn, branch).expect("the spawned workstream must be visible in status");
    assert_eq!(ws["pane"], pane);
    assert_eq!(ws["agent"], "codex");
    assert_eq!(ws["status"], "idle", "a freshly spawned pane, listed and doing nothing, is idle");

    let sent = sandbox.json(&["send", branch, "run", "the", "tests", "--json"]);
    assert_eq!(sent["result"]["pane"], pane);
    assert!(
        sandbox.tmux_log().iter().any(|l| l.contains(&format!("send-keys -t {pane} -l -- run the tests"))),
        "the message must actually reach the pane: {:?}",
        sandbox.tmux_log()
    );

    let after_send = sandbox.status_json();
    let ws = workstream(&after_send, branch).expect("send must not make the workstream disappear");
    assert_eq!(ws["pane"], pane, "still the same pane");
    assert_eq!(ws["status"], "idle", "the stand-in pane is still there and unbusy");

    let stopped = sandbox.json(&["stop", branch, "--json"]);
    assert_eq!(stopped["result"]["pane"], pane);
    assert!(sandbox.tmux_log().iter().any(|l| l == &format!("kill-pane -t {pane}")), "stop must actually close the pane");

    let after_stop = sandbox.status_json();
    assert!(workstream(&after_stop, branch).is_none(), "a stopped workstream must be gone from status, not just marked dead");
}

/// Scenario 2: `Registry::with_lock`'s own concurrency guarantee (`src/workstream.rs`), but
/// exercised the way it actually matters — several real `kanstack spawn` *processes*, launched
/// at once against one repository, racing `main`'s `dispatch()` end to end (argument parsing,
/// `but`/tmux calls and all), not just the registry mutation in isolation. None may be dropped.
#[test]
fn concurrent_spawns_all_land_in_the_registry_none_dropped() {
    let sandbox = Sandbox::new("concurrent-spawn");
    let branches = ["alpha", "beta", "gamma", "delta", "epsilon"];

    let children: Vec<Child> = branches.iter().map(|b| sandbox.spawn_async(&["spawn", b, "--json"])).collect();
    let outputs: Vec<Output> = children.into_iter().map(|c| c.wait_with_output().unwrap()).collect();
    for (branch, out) in branches.iter().zip(&outputs) {
        assert!(out.status.success(), "spawning {branch} failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    let status = sandbox.status_json();
    let listed: Vec<&str> = status["workstreams"].as_array().unwrap().iter().map(|w| w["branch"].as_str().unwrap()).collect();
    for branch in branches {
        assert!(listed.contains(&branch), "{branch} was dropped by a concurrent spawn: {listed:?}");
    }
    assert_eq!(listed.len(), branches.len(), "and nothing extra either: {listed:?}");

    // Every branch got its own pane — a lost update would show up as two branches sharing
    // one, or as fewer panes on disk than branches spawned.
    let panes: Vec<&str> = status["workstreams"].as_array().unwrap().iter().map(|w| w["pane"].as_str().unwrap()).collect();
    let mut unique = panes.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), branches.len(), "two branches ended up sharing a pane: {panes:?}");
}

/// Scenario 3: a pane closed by something other than kanstack — the user closing the terminal
/// tab, the multiplexer restarting — must be reported `Dead` by `status`, and `prune` must
/// remove exactly that workstream and no other.
#[test]
fn a_pane_closed_outside_kanstack_is_dead_in_status_and_prune_removes_only_it() {
    let sandbox = Sandbox::new("stale-pane");
    let (live, gone) = ("feat-alive", "feat-gone");

    for branch in [live, gone] {
        let spawned = sandbox.json(&["spawn", branch, "--json"]);
        assert_eq!(spawned["ok"], true, "{branch}");
    }

    let before = sandbox.status_json();
    let gone_pane = workstream(&before, gone).unwrap()["pane"].as_str().unwrap().to_string();
    sandbox.close_pane_externally(&gone_pane);

    let after_close = sandbox.status_json();
    assert_eq!(workstream(&after_close, gone).unwrap()["status"], "dead", "a pane the multiplexer no longer lists is dead");
    assert_eq!(workstream(&after_close, live).unwrap()["status"], "idle", "the untouched pane is unaffected");

    let pruned = sandbox.json(&["prune", "--json"]);
    assert_eq!(pruned["pruned"], serde_json::json!([gone]));

    let after_prune = sandbox.status_json();
    assert!(workstream(&after_prune, gone).is_none(), "the dead workstream must be forgotten");
    assert!(workstream(&after_prune, live).is_some(), "and only the dead one — the live workstream must survive prune");
}

