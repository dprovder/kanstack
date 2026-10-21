//! End-to-end tests against a real `but` binary.
//!
//! Ignored by default because they need the GitButler CLI installed. Run with:
//!
//! ```text
//! cargo test --test live -- --ignored --test-threads=1
//! ```
//!
//! Everything happens inside a temporary directory with `HOME` redirected, so these do
//! not touch the caller's GitButler project registry or settings.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;

use kanstack::board::{Board, CardKind, ColumnKind};
use kanstack::but::{But, Placement, MIN_VERSION};
use kanstack::splitter::Splitter;
use kanstack::workstream::Registry;

struct Sandbox {
    root: PathBuf,
    name: String,
    /// Every real multiplexer [`Self::splitter`] handed out, so `Drop` can close what the
    /// test opened through it before the directory those panes were told to `cd` into goes.
    live: RefCell<Vec<LiveMux>>,
}

impl Sandbox {
    /// A git repo with one commit, before `but setup` has run.
    fn bare(name: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("kanstack-live-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        alive_sandboxes().push(root.clone());
        sweep_husks();
        let sb = Sandbox { root, name: name.to_string(), live: RefCell::new(Vec::new()) };

        // Redirect HOME for this process so `But`'s spawned commands inherit it and write
        // their config into the sandbox rather than the developer's real one.
        std::env::set_var("HOME", sb.home());
        // The workstream registry too: it otherwise follows `XDG_STATE_HOME` when that's set,
        // which would put this sandbox's lanes in the developer's real state directory — and
        // `Drop` reads it back to find the panes to close, so it must be this sandbox's own.
        std::env::set_var("KANSTACK_STATE_PATH", sb.state());
        // Every `but` command forks a detached `but metrics` process (parent pid 1) that goes
        // on writing into `HOME` and the repo's `.git/gitbutler` after the command returns —
        // after `Drop` has removed them, recreating a husk of the sandbox each time. Measured:
        // three such processes per handful of commands with telemetry on, none with it off.
        sb.disable_but_telemetry();

        sb.git(&["init", "-q", "."]);
        sb.git(&["config", "user.email", "test@example.com"]);
        sb.git(&["config", "user.name", "Test"]);
        sb.write("README.md", "base\n");
        sb.git(&["add", "."]);
        sb.git(&["commit", "-qm", "base"]);
        sb
    }

    fn new(name: &str) -> Sandbox {
        let sb = Sandbox::bare(name);
        sb.but(&["setup", "--init", "--json"]);
        sb
    }

    /// A sandbox whose target is a real remote.
    ///
    /// The remote has to exist *before* `but setup`, because setup picks the push target
    /// then: with no remote it invents a local `gb-local` and pushes go there instead of
    /// anywhere useful.
    fn with_remote(name: &str) -> Sandbox {
        let sb = Sandbox::bare(name);
        let remote = sb.root.join("remote.git");
        Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&remote)
            .output()
            .unwrap();
        sb.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
        sb.git(&["push", "-q", "origin", "HEAD:main"]);
        sb.but(&["setup", "--init", "--json"]);
        sb
    }

    /// Lands a commit on the remote's `main` from a separate clone, so the workspace
    /// falls behind its target the way it would when a colleague pushes.
    fn push_upstream_commit(&self, file: &str, message: &str) {
        let clone = self.root.join("upstream-clone");
        let remote = self.root.join("remote.git");
        let git = |args: &[&str], dir: &std::path::Path| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("HOME", self.home())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        Command::new("git")
            .args(["clone", "-q"])
            .arg(&remote)
            .arg(&clone)
            .output()
            .unwrap();
        git(&["config", "user.email", "other@example.com"], &clone);
        git(&["config", "user.name", "Other"], &clone);
        std::fs::write(clone.join(file), "upstream\n").unwrap();
        git(&["add", "."], &clone);
        git(&["commit", "-qm", message], &clone);
        git(&["push", "-q", "origin", "main"], &clone);
        let _ = std::fs::remove_dir_all(&clone);
    }

    /// Ref names present on the bare remote, for asserting what a push actually did.
    fn remote_refs(&self) -> String {
        let out = Command::new("git")
            .args(["--git-dir"])
            .arg(self.root.join("remote.git"))
            .args(["for-each-ref", "--format=%(refname)"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }
    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    /// Turns `but`'s telemetry off for this sandbox's `HOME` — see [`Self::bare`] for why.
    /// Written where macOS and where XDG systems keep GitButler's settings, since which one
    /// `but` reads depends on the platform.
    fn disable_but_telemetry(&self) {
        let settings =
            r#"{"telemetry":{"appMetricsEnabled":false,"appErrorReportingEnabled":false,"appNonAnonMetricsEnabled":false}}"#;
        for dir in [self.home().join("Library/Application Support/gitbutler"), self.home().join(".config/gitbutler")] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.json"), settings).unwrap();
        }
    }

    /// The multiplexer kanstack would really use here — `Splitter::discover`, so a test that
    /// opens a lane runs the real spawn path — but pointed somewhere of this sandbox's own,
    /// so what it opens never lands among the developer's panes, and `Drop` can close all of
    /// it by an id nothing else shares:
    ///
    /// - cmux: a fresh workspace, created unfocused, that every pane is pinned into.
    /// - tmux: a private server on a socket named after this sandbox, which a wrapper
    ///   `KANSTACK_TMUX_BIN` sends every command to.
    /// - anything else (Orca, Ghostty): this suite knows no private space to make, so panes
    ///   open where they would for a person. `Drop` still closes each one, by the exact id
    ///   the registry recorded for it.
    ///
    /// Panes launch a stand-in harness that notes where it started and then sleeps, not
    /// `$KANSTACK_HARNESS`: kanstack's half of a spawn — open, type the launch line, title,
    /// record — is the same either way, and a test has no business starting a real agent
    /// that spends money. `KANSTACK_LIVE_HARNESS` launches something else instead.
    fn splitter(&self) -> Option<Splitter> {
        let backend = Splitter::discover()?.label();
        let harness = std::env::var("KANSTACK_LIVE_HARNESS").unwrap_or_else(|_| self.stand_in_harness());
        let tag = format!("kanstack-live-{}-{}", self.name, std::process::id());
        // Recorded the moment it exists, before anything else here can fail, so `Drop` takes
        // it down even if discovery then does.
        let mut live = self.live.borrow_mut();
        let (isolation, mut env) = match backend {
            "cmux" => (Isolation::Cmux(CmuxWorkspace::create(&tag, &self.root)), vec![]),
            "tmux" => {
                let (server, wrapper, pane) = PrivateTmux::start(&tag, &self.root);
                let env = vec![("KANSTACK_TMUX_BIN", wrapper.to_string_lossy().into_owned()), ("TMUX_PANE", pane)];
                (Isolation::Tmux(server), env)
            }
            _ => (Isolation::None, vec![]),
        };
        live.push(LiveMux { splitter: None, isolation });
        let mux = live.last_mut().unwrap();

        // Pinned to the backend the isolation was made for, so the splitter can't come back
        // as some other one whose panes the isolation above doesn't contain.
        env.push(("KANSTACK_SPLIT_BACKEND", backend.to_string()));
        env.push(("KANSTACK_HARNESS", harness));
        let mut splitter = with_env(&env, Splitter::discover)?;
        if let Isolation::Cmux(workspace) = &mux.isolation {
            splitter.set_workspace(Some(&workspace.uuid));
        }
        // A clone shares the multiplexer (and so its pinned workspace) with the one the test
        // gets, which is all `Drop` needs to close a pane through.
        mux.splitter = Some(splitter.clone());
        Some(splitter)
    }

    /// Whether [`Self::splitter`] found a real multiplexer, so a lane opened through it is
    /// expected to have actually opened a pane.
    fn has_live_mux(&self) -> bool {
        self.live.borrow().iter().any(|l| l.splitter.is_some())
    }

    /// The pane the registry recorded for `branch` — what the board writes the moment a
    /// spawn succeeds, and nothing else does.
    fn spawned_pane(&self, branch: &str) -> Option<String> {
        let registry = Registry::load(&self.repo()).ok()?;
        registry.get(branch)?.pane_id.as_ref().map(|p| p.0.clone())
    }

    /// The directory each stand-in harness found itself in, one per launch that got as far
    /// as running it.
    fn harness_launches(&self) -> Vec<String> {
        let log = std::fs::read_to_string(self.root.join("harness-launches")).unwrap_or_default();
        log.lines().map(str::to_string).collect()
    }

    /// Writes the stand-in harness [`Self::splitter`] launches by default, returning the
    /// command to launch it with.
    fn stand_in_harness(&self) -> String {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("live-harness");
        // Whatever the launch line passes it (a branch note, a first message) is ignored;
        // `exec` so closing the pane ends the sleep rather than leaving it orphaned.
        let script = format!(
            "#!/bin/sh\npwd >> '{}'\nexec sleep 3600\n",
            self.root.join("harness-launches").display()
        );
        std::fs::write(&path, script).unwrap();
        make_executable(&path);
        path.to_string_lossy().into_owned()
    }

    fn write(&self, rel: &str, contents: &str) {
        std::fs::write(self.repo().join(rel), contents).unwrap();
    }

    fn run(&self, bin: &str, args: &[&str]) -> String {
        let out = Command::new(bin)
            .args(args)
            .current_dir(self.repo())
            .env("HOME", self.home())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn {bin}: {e}"));
        assert!(
            out.status.success(),
            "{bin} {args:?} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn git(&self, args: &[&str]) -> String {
        self.run("git", args)
    }
    fn but(&self, args: &[&str]) -> String {
        self.run("but", args)
    }

    /// Creates a branch with one commit touching `file`.
    fn branch_with_commit(&self, branch: &str, file: &str, message: &str) {
        self.write(file, "content\n");
        self.but(&["branch", "new", branch]);
        self.but(&["commit", "-m", message, "-b", branch]);
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Panes first, directory second: a pane's launch line `cd`s into the repo, and one
        // still starting when the repo goes dies at that `cd` and lingers as an empty shell.
        //
        // Each pane is closed by the id it was opened under, whatever it's doing, rather than
        // waited on to be "up" first. Closing a surface or pane works the same whether its
        // shell has run anything yet, while "up" has no signal to wait on that doesn't depend
        // on the harness — a stand-in, a real agent, or one that fails to start at all —
        // and a wait would cost every test that opens a pane its full timeout in that last
        // case. Nothing here may panic either: this may already be running because the test
        // did, and a second panic aborts the run before any later sandbox cleans up.
        let live = std::mem::take(self.live.get_mut());
        if !live.is_empty() {
            // `Registry::load` finds the registry through the environment, which a later
            // sandbox in the same process may since have repointed.
            std::env::set_var("KANSTACK_STATE_PATH", self.state());
            let repo = self.repo();
            for mut mux in live {
                let torn_down = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    mux.close_registered_panes(&repo);
                    drop(mux);
                }));
                if torn_down.is_err() {
                    eprintln!("live teardown: closing {}'s panes panicked; some may be left open", self.name);
                }
            }
        }
        self.end_stragglers();
        alive_sandboxes().retain(|r| r != &self.root);
        // A `but` still finishing (a background refresh the test's `App` started) can write
        // into the tree while it's being removed, so one pass isn't always the last.
        for _ in 0..20 {
            let _ = std::fs::remove_dir_all(&self.root);
            if !self.root.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        eprintln!("live teardown: could not remove {}", self.root.display());
    }
}

impl Sandbox {
    /// Ends every process still running with this sandbox as its `HOME`. `but` detaches
    /// background jobs that outlive the command that started them — after a `status` or a
    /// `pull`, a `but refresh-remote-data --fetch --pr --ci --updates`, measured still running
    /// more than five seconds after its test returned, with no setting found that turns it
    /// off — and one of those writing a fresh `settings.json` into a `HOME` that `Drop` just
    /// removed is what leaves a husk of the sandbox behind. Ended rather than waited on: it
    /// is fetching for a repository about to be deleted, and waiting cost the suite minutes.
    ///
    /// Selected on the exact `HOME=` this sandbox set — a path with this test's name and
    /// this process's pid in it — so only `but`s this sandbox launched can match. Best-effort:
    /// `ps -E` is the BSD spelling, and elsewhere this finds nothing and does nothing.
    fn end_stragglers(&self) {
        let needle = format!("HOME={} ", self.home().display());
        // `ps` and `kill` themselves would otherwise inherit this sandbox's `HOME` from this
        // process (see `bare`) and so match it, `ps` finding itself on every look.
        let stragglers = || -> Vec<String> {
            let Ok(out) = Command::new("ps").args(["-ww", "-E", "-axo", "pid=,command="]).env_remove("HOME").output()
            else {
                return vec![];
            };
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.contains(&needle) || l.ends_with(needle.trim_end()))
                .filter_map(|l| l.split_whitespace().next().map(str::to_string))
                .filter(|pid| *pid != std::process::id().to_string())
                .collect()
        };
        let pids = stragglers();
        if pids.is_empty() {
            return;
        }
        if reap_dry_run() {
            eprintln!("live teardown [dry run]: would end {pids:?} (HOME={})", self.home().display());
            return;
        }
        eprintln!("live teardown: ending {pids:?} (HOME={})", self.home().display());
        let _ = Command::new("kill").arg("-TERM").args(&pids).env_remove("HOME").output();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !stragglers().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

/// The roots of the sandboxes this process has made and not yet dropped.
fn alive_sandboxes() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    static ALIVE: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
    ALIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Removes what's left of this process's earlier sandboxes. `Drop` can't see the future: a
/// `but` it never saw running — once a run, in `rebasing_brings_upstream_work_in`, writing
/// a fresh `settings.json` after teardown had already found no straggler — can still put a
/// husk back after its sandbox is gone. Only directories named for this process's pid, and
/// only ones no sandbox here still owns, so neither another run's sandboxes nor a live one
/// in a parallel run of this one are ever touched.
fn sweep_husks() {
    let suffix = format!("-{}", std::process::id());
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    let alive = alive_sandboxes();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("kanstack-live-") && name.ends_with(&suffix) && !alive.contains(&entry.path()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// One real multiplexer a [`Sandbox`] handed out, and what to take down with it.
struct LiveMux {
    /// `None` only if discovery failed after the isolation was already made.
    splitter: Option<Splitter>,
    isolation: Isolation,
}

impl LiveMux {
    /// Closes every pane the sandbox's registry records — written by the board itself as each
    /// spawn succeeds — through kanstack's own `stop`. The ids come from what the multiplexer
    /// answered when each pane opened, never from matching titles or directories, so nothing
    /// this sandbox didn't open can be among them.
    fn close_registered_panes(&mut self, repo: &Path) {
        let Some(splitter) = &mut self.splitter else { return };
        let Ok(registry) = Registry::load(repo) else { return };
        for ws in &registry.workstreams {
            let Some(pane) = &ws.pane_id else { continue };
            let branch = &ws.branch_id.0;
            if reap_dry_run() {
                eprintln!("live teardown [dry run]: would close {} ({branch})", pane.0);
                continue;
            }
            splitter.adopt(branch, &pane.0);
            match splitter.stop(branch) {
                Ok(()) => eprintln!("live teardown: closed {} ({branch})", pane.0),
                Err(e) => eprintln!("live teardown: closing {} ({branch}) failed: {e:#}", pane.0),
            }
        }
    }
}

/// Where a [`Sandbox`]'s panes open, when it isn't among the developer's own. Each variant
/// takes its space down when dropped, after `LiveMux::close_registered_panes` has closed the
/// panes in it — which catches anything the registry missed: a pane whose spawn failed
/// after it opened, or one whose registry write didn't land.
enum Isolation {
    Cmux(CmuxWorkspace),
    Tmux(#[allow(dead_code, reason = "held only for its Drop, which kills the server")] PrivateTmux),
    None,
}

/// `KANSTACK_LIVE_REAP_DRY_RUN=1` logs what teardown would close instead of closing it — for
/// checking what it selects on a machine full of other work before trusting it there. Panes
/// are left open (and their sandbox's directory still removed) in that mode.
fn reap_dry_run() -> bool {
    std::env::var_os("KANSTACK_LIVE_REAP_DRY_RUN").is_some_and(|v| v == "1")
}

/// Runs `f` with `vars` set, then puts back whatever was there before — for settings that
/// discovery reads once, which the next sandbox's discovery shouldn't inherit.
fn with_env<T>(vars: &[(&str, String)], f: impl FnOnce() -> T) -> T {
    let saved: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
    for (k, v) in vars {
        std::env::set_var(k, v);
    }
    let out = f();
    for (k, v) in saved {
        match v {
            Some(v) => std::env::set_var(k, v),
            None => std::env::remove_var(k),
        }
    }
    out
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Runs the cmux CLI kanstack would (`KANSTACK_CMUX_BIN`, else `cmux`), returning its
/// output when it succeeds.
fn cmux(args: &[&str]) -> Option<String> {
    let bin = std::env::var_os("KANSTACK_CMUX_BIN").unwrap_or_else(|| "cmux".into());
    let out = Command::new(bin).args(args).env("CMUX_QUIET", "1").output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The UUIDs of the workspaces in this window. UUIDs, not `workspace:N` refs, are what get
/// kept: a UUID is never handed to a different workspace later.
fn cmux_workspace_uuids() -> Vec<String> {
    let listing = cmux(&["--id-format", "both", "workspace", "list"]).unwrap_or_default();
    listing
        .lines()
        .filter_map(|line| line.trim_start().trim_start_matches('*').split_whitespace().nth(1).map(str::to_string))
        .collect()
}

/// Every surface in every workspace, as both its `surface:N` ref and its UUID.
fn cmux_all_surfaces() -> std::collections::HashSet<String> {
    let tree = cmux(&["--id-format", "both", "tree", "--all"]).unwrap_or_default();
    let mut surfaces = std::collections::HashSet::new();
    for line in tree.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if let Some(i) = tokens.windows(2).position(|w| w[0] == "surface" && w[1].starts_with("surface:")) {
            surfaces.extend(tokens[i + 1..].iter().take(2).map(|t| t.to_string()));
        }
    }
    surfaces
}

/// A cmux workspace made for one test, closed again when dropped. Only ever closed by the
/// UUID cmux gave it at creation, and only after checking that UUID wasn't already in use
/// and isn't the workspace running the tests.
struct CmuxWorkspace {
    uuid: String,
}

impl CmuxWorkspace {
    fn create(name: &str, cwd: &Path) -> CmuxWorkspace {
        let before = cmux_workspace_uuids();
        let cwd = cwd.to_string_lossy();
        let out = cmux(&["new-workspace", "--name", name, "--cwd", &cwd, "--focus", "false"])
            .unwrap_or_else(|| panic!("`cmux new-workspace` failed for {name}"));
        let reference = out
            .split_whitespace()
            .find(|t| t.starts_with("workspace:"))
            .unwrap_or_else(|| panic!("`cmux new-workspace` reported no workspace: {out:?}"));
        let listing = cmux(&["--id-format", "both", "workspace", "list"]).unwrap_or_default();
        let uuid = listing
            .lines()
            .find_map(|line| {
                let mut tokens = line.trim_start().trim_start_matches('*').split_whitespace();
                (tokens.next() == Some(reference)).then(|| tokens.next().map(str::to_string)).flatten()
            })
            .unwrap_or_else(|| panic!("no UUID listed for {reference}"));
        // Not constructed — and so never closed — unless this is unmistakably new.
        assert!(!before.contains(&uuid), "{reference} ({uuid}) already existed; refusing to adopt it");
        assert_ne!(std::env::var("CMUX_WORKSPACE_ID").ok().as_deref(), Some(uuid.as_str()));
        CmuxWorkspace { uuid }
    }

    /// The `surface:N` refs in this workspace.
    fn surfaces(&self) -> Vec<String> {
        let listing = cmux(&["--id-format", "both", "list-panels", "--workspace", &self.uuid]).unwrap_or_default();
        listing
            .lines()
            .filter_map(|line| line.trim_start().trim_start_matches('*').split_whitespace().next())
            .filter(|t| t.starts_with("surface:"))
            .map(str::to_string)
            .collect()
    }
}

impl Drop for CmuxWorkspace {
    fn drop(&mut self) {
        if reap_dry_run() {
            eprintln!("live teardown [dry run]: would close workspace {}", self.uuid);
            return;
        }
        match cmux(&["close-workspace", "--workspace", &self.uuid]) {
            Some(_) => eprintln!("live teardown: closed workspace {}", self.uuid),
            None => eprintln!("live teardown: closing workspace {} failed (already gone?)", self.uuid),
        }
    }
}

/// A tmux server of one test's own, on a socket named after its sandbox, killed when
/// dropped. Nothing outside the test talks to it, so killing it can't touch anyone's panes.
struct PrivateTmux {
    socket: String,
    /// Where tmux put the socket, which `kill-server` leaves behind.
    socket_path: Option<PathBuf>,
}

impl PrivateTmux {
    /// Starts the server and returns it, a wrapper that points `tmux` at it (for
    /// `KANSTACK_TMUX_BIN`), and its first pane's id (for `TMUX_PANE`: kanstack's "own pane").
    fn start(socket: &str, root: &Path) -> (PrivateTmux, PathBuf, String) {
        let out = Command::new("tmux")
            .args(["-L", socket, "-f", "/dev/null", "new-session", "-d", "-s", "live", "-x", "200", "-y", "50"])
            .arg("-c")
            .arg(root)
            .args(["-P", "-F", "#{pane_id}"])
            .output()
            .expect("tmux must run");
        assert!(out.status.success(), "starting a private tmux server failed: {}", String::from_utf8_lossy(&out.stderr));
        let socket_path = Command::new("tmux")
            .args(["-L", socket, "display-message", "-p", "#{socket_path}"])
            .output()
            .ok()
            .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
            .filter(|p| p.file_name().is_some_and(|n| n == socket));
        let server = PrivateTmux { socket: socket.to_string(), socket_path };
        let pane = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let wrapper = bin.join("tmux");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexec tmux -L '{socket}' \"$@\"\n")).unwrap();
        make_executable(&wrapper);
        (server, wrapper, pane)
    }
}

impl Drop for PrivateTmux {
    fn drop(&mut self) {
        if reap_dry_run() {
            eprintln!("live teardown [dry run]: would kill tmux server {}", self.socket);
            return;
        }
        let _ = Command::new("tmux").args(["-L", &self.socket, "kill-server"]).output();
        if let Some(path) = &self.socket_path {
            let _ = std::fs::remove_file(path);
        }
        eprintln!("live teardown: killed tmux server {}", self.socket);
    }
}

fn skip_if_no_but() -> bool {
    Command::new("but").arg("--version").output().is_err()
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn discovers_and_version_gates_the_cli() {
    if skip_if_no_but() {
        eprintln!("skipping: `but` not on PATH");
        return;
    }
    let sb = Sandbox::new("discover");
    let but = But::discover(&sb.repo()).expect("discover");
    assert!(
        but.version() >= MIN_VERSION,
        "installed but {} is below the supported floor {MIN_VERSION}",
        but.version()
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn reads_a_real_workspace_into_a_board() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("read");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");
    sb.branch_with_commit("feat-ui", "b.txt", "Redesign settings page");
    sb.write("wip.txt", "scratch\n");

    let but = But::discover(&sb.repo()).unwrap();
    let board = Board::from_status(&but.status().expect("status parses"));

    let titles: Vec<&str> = board.columns.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["unassigned", "feat-auth", "feat-ui"]);

    assert_eq!(board.columns[0].kind, ColumnKind::Unassigned);
    assert_eq!(
        board.columns[0]
            .cards
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>(),
        ["wip.txt"],
        "an unstaged file shows up in the backlog lane"
    );

    let auth = &board.columns[1];
    assert_eq!(auth.cards.len(), 1);
    assert_eq!(auth.cards[0].title, "Add auth middleware");
    assert_eq!(auth.cards[0].kind, CardKind::Commit);
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn moving_a_card_between_lanes_rubs_and_returns_fresh_state() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("move");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");
    sb.branch_with_commit("feat-ui", "b.txt", "Redesign settings page");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());

    let commit = before.columns[1].cards[0].clone();
    let target = before.columns[2].drop_target.clone();
    assert_eq!(commit.title, "Add auth middleware");

    // The single call both mutates and returns the refreshed workspace.
    let after = Board::from_status(
        &but.move_commits(std::slice::from_ref(&commit.cli_id), &target).expect("move"),
    );

    let auth = after
        .columns
        .iter()
        .find(|c| c.title == "feat-auth")
        .unwrap();
    let ui = after.columns.iter().find(|c| c.title == "feat-ui").unwrap();
    assert!(
        auth.cards.is_empty(),
        "commit left its original lane, found {:?}",
        auth.cards.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    assert!(
        ui.cards.iter().any(|c| c.title == "Add auth middleware"),
        "commit landed in the target lane, found {:?}",
        ui.cards.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
}

/// Dropping an uncommitted file onto a lane with no commits yet has nothing to amend into,
/// so it creates the lane's first commit instead — the case `App::confirm_move` parks for a
/// message before running (`MoveOp::Commit`). Dropping onto a lane that already has a tip
/// commit is the amend case, covered by `dropping_a_file_on_a_commit_amends_it`.
#[test]
#[ignore = "requires the GitButler CLI"]
fn committing_a_backlog_file_onto_an_empty_lane() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commitempty");
    sb.but(&["branch", "new", "feat-empty"]);
    sb.write("wip.txt", "scratch\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let file = before.columns[0].cards[0].clone();
    let target = before.columns[1].drop_target.clone();
    assert_eq!(file.kind, CardKind::Change);
    assert!(
        before.columns[1].cards.is_empty(),
        "the lane must start with no commits for this to exercise the Commit path"
    );

    let after =
        Board::from_status(&but.commit(std::slice::from_ref(&file.cli_id), "Start work", &target).unwrap());
    assert!(
        after.columns[0].cards.is_empty(),
        "file left the backlog once committed"
    );
    let lane = after.columns.iter().find(|c| c.title == "feat-empty").unwrap();
    assert_eq!(lane.cards.len(), 1, "the file became the lane's first commit");
    assert_eq!(lane.cards[0].title, "Start work");
    assert_eq!(lane.cards[0].subtitle.as_deref(), Some("wip.txt"));
}

/// The claim that makes squash free: dropping a commit card onto another commit card is
/// the same `but rub` call as any other move, just with a commit as the target.
#[test]
#[ignore = "requires the GitButler CLI"]
fn dropping_a_commit_on_a_commit_squashes_them() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("squash");
    sb.branch_with_commit("feat", "a.txt", "First commit");
    sb.write("b.txt", "more\n");
    sb.but(&["commit", "-m", "Second commit", "-b", "feat"]);

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let lane = before.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 2, "two commits before squashing");

    // Newest is on top; squash it into the one below.
    let source = lane.cards[0].clone();
    let target = lane.cards[1].clone();
    let after = Board::from_status(
        &but.squash(std::slice::from_ref(&source.cli_id), &target.cli_id).expect("squash"),
    );

    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(
        lane.cards.len(),
        1,
        "the two commits collapsed into one, got {:?}",
        lane.cards.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
}

/// The other half of the same mechanism: a working-tree file dropped on a commit amends.
#[test]
#[ignore = "requires the GitButler CLI"]
fn dropping_a_file_on_a_commit_amends_it() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("amend");
    sb.branch_with_commit("feat", "a.txt", "Only commit");
    sb.write("b.txt", "extra\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let file = before.columns[0].cards[0].clone();
    let commit = before
        .columns
        .iter()
        .find(|c| c.title == "feat")
        .unwrap()
        .cards[0]
        .clone();
    assert_eq!(file.kind, CardKind::Change);
    assert_eq!(commit.kind, CardKind::Commit);

    let after = Board::from_status(
        &but.amend(std::slice::from_ref(&file.cli_id), &commit.cli_id).expect("amend"),
    );
    assert!(
        after.columns[0].cards.is_empty(),
        "the file left the backlog"
    );
    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 1, "still one commit, now amended");
    let files = lane.cards[0].subtitle.clone().unwrap_or_default();
    assert!(
        files.contains("b.txt"),
        "the amended file is in the commit, got {files:?}"
    );
}

/// `but commit` takes the changes to commit as explicit ids now (0.22 removed `--only` along
/// with the whole "stage first, commit whatever's assigned" flow) — naming just one file's
/// id must leave every other uncommitted file alone.
#[test]
#[ignore = "requires the GitButler CLI"]
fn committing_a_lane_leaves_unassigned_changes_alone() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commitonly");
    sb.branch_with_commit("feat", "a.txt", "First");
    sb.write("staged.txt", "for the lane\n");
    sb.write("untouched.txt", "must stay in the backlog\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let staged = before.columns[0]
        .cards
        .iter()
        .find(|c| c.title == "staged.txt")
        .expect("staged.txt in backlog")
        .clone();
    let lane = before.columns.iter().find(|c| c.title == "feat").unwrap();

    let after = Board::from_status(
        &but.commit(std::slice::from_ref(&staged.rub_id), "Only the staged one", &lane.drop_target)
            .unwrap(),
    );

    let backlog: Vec<&str> = after.columns[0]
        .cards
        .iter()
        .map(|c| c.title.as_str())
        .collect();
    assert_eq!(
        backlog, ["untouched.txt"],
        "the unstaged file must survive the commit, got {backlog:?}"
    );

    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    let tip = &lane.cards[0];
    assert_eq!(tip.title, "Only the staged one");
    assert_eq!(
        tip.subtitle.as_deref(),
        Some("staged.txt"),
        "the commit contains only what was staged to the lane"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn committing_a_lane_turns_staged_files_into_a_commit() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commit");
    sb.branch_with_commit("feat", "a.txt", "First");
    sb.write("b.txt", "staged\n");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let file = before.columns[0].cards[0].clone();
    let lane = before.columns.iter().find(|c| c.title == "feat").unwrap();

    let after = Board::from_status(
        &but.commit(std::slice::from_ref(&file.cli_id), "Second commit", &lane.drop_target)
            .expect("commit"),
    );
    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    let titles: Vec<&str> = lane.cards.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Second commit", "First"]);
    assert!(
        lane.cards.iter().all(|c| c.kind == CardKind::Commit),
        "the staged file became part of a commit"
    );
    // The detailed follow-up query is what keeps file context on the new card.
    assert_eq!(lane.cards[0].subtitle.as_deref(), Some("b.txt"));
}

/// Push is the sharpest edge in the CLI, so the preview it is gated behind gets tested
/// directly: it must name the destination and report whether a force is involved.
#[test]
#[ignore = "requires the GitButler CLI"]
fn push_preview_reports_destination_without_pushing() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("pushprev");
    sb.branch_with_commit("feat", "a.txt", "Pushable");

    let but = But::discover(&sb.repo()).unwrap();
    let preview = but.push_preview("feat").expect("dry run");
    let b = preview
        .branches
        .iter()
        .find(|b| b.branch_name == "feat")
        .expect("branch in preview");
    assert_eq!(b.unpushed_commits, 1);
    assert_eq!(b.remote, "origin");
    assert!(!b.requires_force, "a first push is not a rewrite");
    assert_eq!(b.commits.len(), 1);
    assert_eq!(b.commits[0].message.trim(), "Pushable");

    // A dry run must not have moved anything on the remote.
    let refs = sb.remote_refs();
    assert!(
        !refs.contains("feat"),
        "dry run must not push; remote had {refs:?}"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn pushing_a_lane_updates_the_remote() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("push");
    sb.branch_with_commit("feat", "a.txt", "Pushable");

    let but = But::discover(&sb.repo()).unwrap();
    but.push("feat").expect("push");

    let refs = sb.remote_refs();
    assert!(refs.contains("feat"), "remote should have the branch, got {refs:?}");
}

/// The merge check is what makes `M` safe to bind, the same way the push and rebase
/// previews are: it must report what would land and whether it lands cleanly, without
/// touching anything.
#[test]
#[ignore = "requires the GitButler CLI"]
fn land_check_reports_a_clean_land_without_landing() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("landcheck");
    sb.branch_with_commit("feat", "a.txt", "Landable");

    let but = But::discover(&sb.repo()).unwrap();
    let check = but.merge_check("feat").expect("merge check");
    assert_eq!(check.commits_ahead, 1);
    assert_eq!(check.commits[0].subject(), "Landable");
    assert!(check.merge_check.merges_cleanly);
    assert!(check.merge_check.conflicting_files.is_empty());

    // A check must not have landed anything.
    let board = Board::from_status(&but.status().unwrap());
    let lane = board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some("feat"))
        .expect("the lane is still there, unlanded");
    assert_eq!(lane.cards.len(), 1, "the commit is still only on the lane");
}

/// `but land` prints nothing on success, so the caller must refresh separately — pinned
/// here so a regression shows up as a stale board rather than a parse error users only see
/// at the keyboard.
#[test]
#[ignore = "requires the GitButler CLI"]
fn landing_a_lane_lands_it_on_the_target() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("land");
    sb.branch_with_commit("feat", "a.txt", "Landable");

    let but = But::discover(&sb.repo()).unwrap();
    let status = but.land("feat").expect("land");
    let board = Board::from_status(&status);

    // The lane's commit is no longer ahead of the target — it landed, it did not vanish.
    let lane = board.columns.iter().find(|c| c.branch_name.as_deref() == Some("feat"));
    if let Some(lane) = lane {
        assert!(
            lane.cards.iter().all(|c| c.kind != CardKind::Commit),
            "the landed commit should no longer be ahead of the target"
        );
    }
    let log = sb.git(&["log", "--oneline", "--all"]);
    assert!(log.contains("Landable"), "landing must land the content: {log}");
}

/// The whole reason `M` is built on `but land` and not the older `but merge`: `merge`
/// flatly refuses when the workspace target tracks a real remote ("Target remote is
/// origin, not gb-local. This command only works with gb-local targets" — verified against
/// 0.21.2 against this project's own repository). `land` is designed for exactly this case
/// and pushes the result directly, bypassing any pull request.
#[test]
#[ignore = "requires the GitButler CLI"]
fn landing_a_lane_pushes_directly_to_a_real_remote() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("landremote");
    sb.branch_with_commit("feat", "a.txt", "Landable");

    let but = But::discover(&sb.repo()).unwrap();
    but.land("feat").expect("land onto a real remote target");

    let refs = sb.remote_refs();
    assert!(refs.contains("refs/heads/main"), "the bare remote should still have main");
    let remote_log = Command::new("git")
        .args(["--git-dir"])
        .arg(sb.root.join("remote.git"))
        .args(["log", "--oneline", "main"])
        .output()
        .unwrap();
    let remote_log = String::from_utf8_lossy(&remote_log.stdout);
    assert!(
        remote_log.contains("Landable"),
        "the commit must be pushed straight to the remote's main, got {remote_log:?}"
    );
}

/// `z` (`but undo`) fully restores the local board after a land — the branch and its
/// commit reappear exactly as before. What it does *not* do is un-push: the remote a real
/// land already reached keeps the landed commit. Nothing is lost (the content sits safely
/// on the remote throughout) but the local board looks more reverted than it is, which is
/// exactly the gap the land confirmation dialog now warns about before you commit to it.
#[test]
#[ignore = "requires the GitButler CLI"]
fn undoing_a_land_reverts_locally_but_not_the_pushed_remote() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("landundo");
    sb.branch_with_commit("feat", "a.txt", "Important real work");

    let but = But::discover(&sb.repo()).unwrap();
    but.land("feat").expect("land onto a real remote target");

    let remote_log = |sb: &Sandbox| {
        let out = Command::new("git")
            .args(["--git-dir"])
            .arg(sb.root.join("remote.git"))
            .args(["log", "--oneline", "main"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    assert!(
        remote_log(&sb).contains("Important real work"),
        "sanity check: the land must have really pushed"
    );

    let status = but.undo().expect("undo the land");
    let board = Board::from_status(&status);
    let lane = board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some("feat"))
        .expect("undo restores the branch locally");
    assert_eq!(lane.cards.len(), 1, "and its commit, exactly as before");

    // The whole point: the remote is untouched by a purely local undo.
    assert!(
        remote_log(&sb).contains("Important real work"),
        "undo must not silently revert what was already pushed to a real remote"
    );
}

/// The two branch gestures are genuinely different operations, so both are pinned:
/// an anchor stacks into the existing lane, no anchor opens a new one.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_branch_with_an_anchor_stacks_into_the_same_lane() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("stackbranch");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth");

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    assert_eq!(before.columns.len(), 2, "backlog + one lane");

    let after = Board::from_status(&but.branch_new("feat-ui", Some(Placement::Above("feat-auth"))).expect("branch"));
    assert_eq!(
        after.columns.len(),
        2,
        "stacking must not open a new lane, got {:?}",
        after.columns.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    let lane = &after.columns[1];
    assert_eq!(
        lane.title, "feat-ui +1",
        "the new branch becomes the tip and the lane says how deep it is"
    );
    assert_eq!(lane.branch_name.as_deref(), Some("feat-ui"));
    assert!(
        lane.cards.iter().any(|c| c.group.as_deref() == Some("feat-auth")),
        "the anchored branch's commits are grouped under its own name"
    );
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn a_branch_without_an_anchor_opens_a_parallel_lane() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("parallelbranch");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth");

    let but = But::discover(&sb.repo()).unwrap();
    let after = Board::from_status(&but.branch_new("feat-solo", None).expect("branch"));
    let titles: Vec<&str> = after.columns.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(after.columns.len(), 3, "a lane of its own, got {titles:?}");
    assert!(titles.contains(&"feat-solo"));
    assert!(titles.contains(&"feat-auth"));
}

/// Restacking is composed from four separate `but` calls because no single command does
/// it, so the end state is worth asserting in detail: one lane, both branches intact,
/// commits in their original order.
#[test]
#[ignore = "requires the GitButler CLI"]
fn restacking_moves_a_whole_lane_onto_another() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("restack");
    sb.branch_with_commit("feat-auth", "a.txt", "Auth work");
    sb.write("b.txt", "ui\n");
    sb.but(&["branch", "new", "feat-ui"]);
    sb.but(&["commit", "-m", "UI first", "-b", "feat-ui"]);
    sb.write("c.txt", "ui2\n");
    sb.but(&["commit", "-m", "UI second", "-b", "feat-ui"]);

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    assert_eq!(before.columns.len(), 3, "backlog + two parallel lanes");

    let after = Board::from_status(&but.restack_branch("feat-ui", "feat-auth").expect("restack"));

    assert_eq!(
        after.columns.len(),
        2,
        "the two lanes became one, got {:?}",
        after.columns.iter().map(|c| &c.title).collect::<Vec<_>>()
    );
    let lane = &after.columns[1];
    assert_eq!(
        lane.branch_name.as_deref(),
        Some("feat-ui"),
        "the original branch name survives the temp-branch dance"
    );
    assert_eq!(lane.title, "feat-ui +1");

    // Order must be preserved: commits are listed newest-first.
    let titles: Vec<&str> = lane.cards.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["UI second", "UI first", "Auth work"], "got {titles:?}");

    // And the anchored branch keeps its own identity within the lane.
    assert!(
        lane.cards
            .iter()
            .any(|c| c.group.as_deref() == Some("feat-auth")),
        "feat-auth is still a distinct branch in the stack"
    );
    assert!(lane.cards.iter().any(|c| c.group.as_deref() == Some("feat-ui")));
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn restacking_a_branch_onto_itself_is_refused() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("restackself");
    sb.branch_with_commit("feat", "a.txt", "Work");
    let but = But::discover(&sb.repo()).unwrap();
    let err = but.restack_branch("feat", "feat").expect_err("must refuse");
    assert!(format!("{err}").contains("cannot be stacked on itself"));
}

/// `u` on a commit card is `but uncommit`. Confirms it is a true uncommit — the content
/// comes back to the worktree rather than being discarded.
#[test]
#[ignore = "requires the GitButler CLI"]
fn sending_a_commit_to_the_backlog_uncommits_without_losing_content() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("uncommit");
    sb.write("a.txt", "hello\nworld\n");
    sb.but(&["branch", "new", "feat"]);
    sb.but(&["commit", "-m", "Work", "-b", "feat"]);

    let but = But::discover(&sb.repo()).unwrap();
    let before = Board::from_status(&but.status().unwrap());
    let commit = before
        .columns
        .iter()
        .find(|c| c.title == "feat")
        .unwrap()
        .cards[0]
        .clone();

    let after = Board::from_status(&but.uncommit(std::slice::from_ref(&commit.rub_id)).unwrap());

    let lane = after.columns.iter().find(|c| c.title == "feat").unwrap();
    assert!(lane.cards.is_empty(), "the commit is gone from the lane");
    assert!(
        after.columns[0].cards.iter().any(|c| c.title == "a.txt"),
        "and its file is back in the backlog"
    );
    assert_eq!(
        std::fs::read_to_string(sb.repo().join("a.txt")).unwrap(),
        "hello\nworld\n",
        "uncommitting must not touch the worktree content"
    );
}

/// The round trip the drawer exists for: a branch leaves the workspace, shows up in the
/// list of things that can come back, and comes back.
///
/// This is the one flow `but status` cannot verify on its own — an unapplied branch is
/// absent from it entirely, which is exactly why it needs `branch_list` — so each stage
/// asserts against both sources: gone from the board, present in the drawer, and back on
/// the board afterwards.
#[test]
#[ignore = "requires the GitButler CLI"]
fn unapplying_a_branch_moves_it_to_the_drawer_and_applying_brings_it_back() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("applyunapply");
    sb.branch_with_commit("feat-parked", "parked.txt", "work to park");

    let but = But::discover(&sb.repo()).unwrap();
    let on_board = |b: &Board| b.columns.iter().any(|c| c.branch_name.as_deref() == Some("feat-parked"));

    assert!(on_board(&Board::from_status(&but.status().unwrap())), "starts applied");
    assert!(
        !but.branch_list(false).unwrap().branches.iter().any(|b| b.name == "feat-parked"),
        "an applied branch must not also be offered as unapplied"
    );

    but.unapply("feat-parked").unwrap();
    assert!(
        !on_board(&Board::from_status(&but.status().unwrap())),
        "unapply takes the lane off the board"
    );
    let listed = but.branch_list(false).unwrap();
    let parked = listed
        .branches
        .iter()
        .find(|b| b.name == "feat-parked")
        .expect("the unapplied branch is what the drawer lists");
    // The commit went with it rather than being lost — the promise the confirmation makes.
    assert_eq!(parked.commits_ahead, Some(1));

    but.apply("feat-parked").unwrap();
    assert!(
        on_board(&Board::from_status(&but.status().unwrap())),
        "apply brings the lane back"
    );
    assert!(
        !but.branch_list(false).unwrap().branches.iter().any(|b| b.name == "feat-parked"),
        "and it stops being offered once it is applied again"
    );
}

/// The file contents follow the branch, not just the ref. Unapply is described to the user
/// as "its changes leave the working directory" and apply as bringing them back, so the
/// worktree is the thing worth asserting on — a ref that moved while the files stayed put
/// would satisfy every board-level check above and still be wrong.
#[test]
#[ignore = "requires the GitButler CLI"]
fn unapply_takes_the_files_off_disk_and_apply_restores_them() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("applyfiles");
    sb.branch_with_commit("feat-files", "parked.txt", "work to park");
    let path = sb.repo().join("parked.txt");
    assert!(path.exists(), "the committed file starts on disk");

    let but = But::discover(&sb.repo()).unwrap();
    but.unapply("feat-files").unwrap();
    assert!(!path.exists(), "unapply removes the branch's files from the worktree");

    but.apply("feat-files").unwrap();
    assert!(path.exists(), "apply puts them back");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "content\n");
}

/// `but unapply` takes the whole stack containing the named branch, not just that branch.
/// `pending_unapply`'s confirmation text promises to name every branch that would leave,
/// so this pins the upstream behaviour that promise is describing — if `but` ever narrowed
/// it to a single branch, the confirmation would be overstating what it does.
#[test]
#[ignore = "requires the GitButler CLI"]
fn unapplying_a_stacked_lane_takes_every_branch_in_it() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("applystack");
    sb.branch_with_commit("lower", "lower.txt", "lower work");
    sb.write("upper.txt", "content\n");
    sb.but(&["branch", "new", "upper", "--anchor", "lower"]);
    sb.but(&["commit", "-m", "upper work", "-b", "upper"]);

    let but = But::discover(&sb.repo()).unwrap();
    let board = Board::from_status(&but.status().unwrap());
    let lane = board
        .columns
        .iter()
        .find(|c| c.sections.len() > 1)
        .expect("the two branches share one lane");
    assert_eq!(lane.sections.len(), 2);

    // Named by the tip; the branch below it has to leave too.
    but.unapply("upper").unwrap();
    let after = Board::from_status(&but.status().unwrap());
    for name in ["upper", "lower"] {
        assert!(
            after.columns.iter().all(|c| c.branch_name.as_deref() != Some(name)),
            "{name} should have left with the stack"
        );
    }
}

/// `mergesCleanly` is the drawer's entire basis for warning before an apply, and it is
/// computed without applying anything. A branch that genuinely conflicts must report
/// `Some(false)` — if it silently reported `None` or `true`, the drawer would show a green
/// dot on a branch that is about to conflict, which is worse than showing nothing.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_conflicting_branch_is_flagged_before_it_is_applied() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("applyconflict");

    // Both sides edit the same file, so the parked branch cannot merge into the base.
    sb.branch_with_commit("feat-conflict", "shared.txt", "our side");
    let but = But::discover(&sb.repo()).unwrap();
    but.unapply("feat-conflict").unwrap();
    sb.push_upstream_commit("shared.txt", "their side");
    sb.but(&["pull"]);

    let listed = but.branch_list(false).unwrap();
    let parked = listed
        .branches
        .iter()
        .find(|b| b.name == "feat-conflict")
        .expect("still listed as unapplied");
    assert_eq!(
        parked.merges_cleanly,
        Some(false),
        "a branch that conflicts with the base must say so before it is applied"
    );
}

/// `z`/`Z` are the safety net every other destructive action here now leans on (`d` in
/// particular, since 0.21 stopped refusing an orphaning delete). Both must actually round
/// trip: undo an operation, then redo it back, and land on the exact state each time.
#[test]
#[ignore = "requires the GitButler CLI"]
fn undo_and_redo_round_trip_a_branch_creation() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("undoredo");

    let but = But::discover(&sb.repo()).unwrap();
    let after_create = Board::from_status(&but.branch_new("feat", None).unwrap());
    assert!(after_create.columns.iter().any(|c| c.branch_name.as_deref() == Some("feat")));

    let after_undo = Board::from_status(&but.undo().unwrap());
    assert!(
        after_undo.columns.iter().all(|c| c.branch_name.as_deref() != Some("feat")),
        "undo removes the branch it created"
    );

    let after_redo = Board::from_status(&but.redo().unwrap());
    assert!(
        after_redo.columns.iter().any(|c| c.branch_name.as_deref() == Some("feat")),
        "redo brings it back"
    );
}

/// `d` used to be safe to bind only because `but` refused to delete a branch whose commits
/// would be left orphaned. That guarantee is upstream's, not ours — and 0.21 changed it:
/// deleting a lone branch with unpushed commits now succeeds non-interactively and
/// discards them outright, rather than refusing. `but undo` still recovers them, which is
/// what `pending_delete`'s confirmation text promises instead of a refusal now.
#[test]
#[ignore = "requires the GitButler CLI"]
fn deleting_a_lone_branch_with_commits_is_undoable_not_refused() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("delundo");
    sb.branch_with_commit("feat", "a.txt", "Important work");

    let but = But::discover(&sb.repo()).unwrap();
    but.branch_delete("feat").expect("0.21 no longer refuses this non-interactively");

    let after = Board::from_status(&but.status().unwrap());
    assert!(
        after.columns.iter().all(|c| c.branch_name.as_deref() != Some("feat")),
        "the branch and its commit are gone, not refused"
    );

    // `but undo` is the safety net now, so it had better work.
    sb.but(&["undo", "--json"]);
    let restored = Board::from_status(&but.status().unwrap());
    let lane = restored.columns.iter().find(|c| c.title == "feat").expect("undo restores the lane");
    assert_eq!(lane.cards.len(), 1);
}

/// Inside a stack it succeeds, and the commits fold into the branch above rather than
/// disappearing on their own — recoverable with `but undo`, which is what the confirmation
/// dialog now promises instead of a fold (see `App::pending_delete`).
///
/// `d` in the actual UI always targets the lane's *tip* branch (`col.branch_name`), so this
/// deletes "upper", not the base — matching what a keypress on this lane would really do.
#[test]
#[ignore = "requires the GitButler CLI"]
fn deleting_the_tip_of_a_stack_discards_only_its_own_commit() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("delstack");
    sb.branch_with_commit("lower", "a.txt", "Lower work");
    sb.write("b.txt", "upper\n");
    sb.but(&["branch", "new", "upper", "--anchor", "lower"]);
    sb.but(&["commit", "-m", "Upper work", "-b", "upper"]);

    let but = But::discover(&sb.repo()).unwrap();
    but.branch_delete("upper").expect("delete the tip of a stack");

    let after = Board::from_status(&but.status().unwrap());
    assert!(
        after.columns.iter().all(|c| c.branch_name.as_deref() != Some("upper")),
        "the tip branch is gone"
    );
    let lane = after.columns.iter().find(|c| c.title == "lower").unwrap();
    let titles: Vec<&str> = lane.cards.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        titles,
        ["Lower work"],
        "the branch below is untouched, but the tip's own commit is gone, got {titles:?}"
    );

    sb.but(&["undo", "--json"]);
    let restored = Board::from_status(&but.status().unwrap());
    let titles: Vec<&str> = restored
        .columns
        .iter()
        .find(|c| c.title == "upper +1")
        .expect("undo restores the deleted branch")
        .cards
        .iter()
        .map(|c| c.title.as_str())
        .collect();
    assert_eq!(titles, ["Upper work", "Lower work"], "undo brings the discarded commit back");
}

/// The rebase preview is what makes `r` safe to bind: it must report the incoming commits
/// and each lane's outcome without touching anything.
#[test]
#[ignore = "requires the GitButler CLI"]
fn rebase_preview_reports_upstream_work_without_applying_it() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("rebase");
    sb.branch_with_commit("feat", "a.txt", "My work");
    sb.push_upstream_commit("upstream.txt", "Upstream change");

    let but = But::discover(&sb.repo()).unwrap();
    let preview = but.pull_check().expect("pull --check");

    assert!(!preview.up_to_date, "the target has moved");
    assert_eq!(preview.upstream_commits.count, 1);
    assert_eq!(
        preview.upstream_commits.commits[0].description.trim(),
        "Upstream change"
    );
    let feat = preview
        .branch_statuses
        .iter()
        .find(|b| b.name == "feat")
        .expect("our lane is assessed");
    assert_eq!(feat.status, kanstack::model::PullStatus::Updatable);

    // A check must not have rebased anything.
    let board = Board::from_status(&but.status().unwrap());
    let lane = board.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 1);
    assert!(!sb.repo().join("upstream.txt").exists(), "check applied nothing");
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn rebasing_brings_upstream_work_in() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::with_remote("rebasedo");
    sb.branch_with_commit("feat", "a.txt", "My work");
    sb.push_upstream_commit("upstream.txt", "Upstream change");

    let but = But::discover(&sb.repo()).unwrap();
    but.pull().expect("pull");

    assert!(
        sb.repo().join("upstream.txt").exists(),
        "upstream work landed in the worktree"
    );
    let board = Board::from_status(&but.status().unwrap());
    let lane = board.columns.iter().find(|c| c.title == "feat").unwrap();
    assert_eq!(lane.cards.len(), 1, "our commit survived the rebase");
    assert!(but.pull_check().unwrap().up_to_date, "and we are current now");
}

/// The point of the diff pane: `but diff` emits one entry per hunk with its own id, so a
/// single file's hunks can be committed to different lanes. Whole-file commits could not do
/// this, and it is what makes a mixed file reviewable in pieces.
#[test]
#[ignore = "requires the GitButler CLI"]
fn hunks_of_one_file_can_be_committed_to_different_lanes() {
    if skip_if_no_but() {
        return;
    }
    // The base file has to exist before `but setup`: once GitButler owns the workspace it
    // refuses direct `git commit`s to its branch.
    let sb = Sandbox::bare("hunks");
    sb.write(
        "a.txt",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
    );
    sb.git(&["add", "."]);
    sb.git(&["commit", "-qm", "seed"]);
    sb.but(&["setup", "--init", "--json"]);
    sb.but(&["branch", "new", "top"]);
    sb.but(&["branch", "new", "bottom"]);
    // Two edits far enough apart to be separate hunks.
    sb.write(
        "a.txt",
        "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nTEN\n",
    );

    let but = But::discover(&sb.repo()).unwrap();
    let out = but.diff_uncommitted().expect("diff");
    let view = kanstack::diff::DiffView::from_output("a.txt", &out);
    assert_eq!(view.entries.len(), 2, "two hunks in one file");
    assert!(
        view.entries.iter().all(|e| e.path == "a.txt"),
        "both hunks belong to the same file"
    );

    // Ids describe the current state, so the second hunk must be resolved *after* the
    // first commit — committing one renumbers whatever is left. Both branches start with no
    // commits, so each hunk becomes that branch's first commit.
    let first = view.entries[0].rub_id.clone().expect("committable");
    but.commit(&[first], "Top half", "top").expect("commit first hunk");

    let out = but.diff_uncommitted().expect("diff again");
    let view = kanstack::diff::DiffView::from_output("a.txt", &out);
    let remaining = view
        .entries
        .iter()
        .find_map(|e| e.rub_id.clone())
        .expect("the other hunk is still uncommitted");
    let board = Board::from_status(
        &but.commit(&[remaining], "Bottom half", "bottom")
            .expect("commit second hunk"),
    );

    let committed_in = |name: &str| -> bool {
        board
            .columns
            .iter()
            .find(|c| c.branch_name.as_deref() == Some(name))
            .is_some_and(|c| c.cards.iter().any(|k| k.kind == CardKind::Commit))
    };
    assert!(committed_in("top"), "one hunk landed in top");
    assert!(committed_in("bottom"), "the other landed in bottom");
    assert!(
        board.columns[0].cards.is_empty(),
        "and nothing is left uncommitted"
    );
}

/// A commit's diff is readable but not stageable — `but` gives committed changes no ids.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_commits_diff_is_readable_but_not_stageable() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("commitdiff");
    sb.branch_with_commit("feat", "a.txt", "Work");

    let but = But::discover(&sb.repo()).unwrap();
    let board = Board::from_status(&but.status().unwrap());
    let commit = board.columns.iter().find(|c| c.title == "feat").unwrap().cards[0].clone();

    let out = but.diff_target(&commit.rub_id).expect("diff of a commit");
    let view = kanstack::diff::DiffView::from_output(commit.title.clone(), &out);
    assert!(!view.entries.is_empty(), "there is something to read");
    assert!(
        view.entries.iter().all(|e| e.rub_id.is_none()),
        "history offers no hunk to stage"
    );
}

/// Line counts on cards come from a real `but diff`, so check them against a known edit
/// rather than only against a hand-written fixture.
#[test]
#[ignore = "requires the GitButler CLI"]
fn cards_carry_real_line_counts() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::bare("stats");
    sb.write("a.txt", "one\ntwo\nthree\n");
    sb.git(&["add", "."]);
    sb.git(&["commit", "-qm", "seed"]);
    sb.but(&["setup", "--init", "--json"]);
    // One line changed, two added: +3 -1.
    sb.write("a.txt", "ONE\ntwo\nthree\nfour\nfive\n");

    let but = But::discover(&sb.repo()).unwrap();
    let status = but.status().unwrap();
    let diff = but.diff_uncommitted().unwrap();
    let board = Board::from_status_and_diff(&status, &diff);

    let card = board.columns[0]
        .cards
        .iter()
        .find(|c| c.title == "a.txt")
        .expect("the edited file is on the board");
    assert_eq!(card.stats, Some((3, 1)), "3 added, 1 removed");
    assert_eq!(board.columns[0].stats, Some((3, 1)), "and the lane totals it");
}

#[test]
#[ignore = "requires the GitButler CLI"]
fn reports_a_useful_error_outside_a_gitbutler_project() {
    if skip_if_no_but() {
        return;
    }
    // A plain git repo that was never `but setup`.
    let root = std::env::temp_dir().join(format!("kanstack-live-plain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        vec!["init", "-q", "."],
        vec!["config", "user.email", "t@e.com"],
        vec!["config", "user.name", "T"],
    ] {
        Command::new("git")
            .args(&args)
            .current_dir(&root)
            .output()
            .unwrap();
    }

    let but = But::discover(&root).unwrap();
    let err = but
        .status()
        .expect_err("status must fail outside a project");
    let msg = format!("{err}");
    assert!(
        msg.contains("setup_required") || msg.to_lowercase().contains("no gitbutler project"),
        "error should explain the real problem, got: {msg}"
    );
    assert!(
        msg.contains("but setup"),
        "error should carry the CLI's own hint, got: {msg}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Guards the assumption the whole design rests on: `but status` cost is dominated by
/// fixed overhead, not repository size, so navigation can stay in-memory and refreshes
/// can be synchronous.
#[test]
#[ignore = "requires the GitButler CLI; timing-sensitive"]
fn status_stays_fast_enough_for_a_synchronous_refresh() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("perf");
    for i in 0..6 {
        sb.branch_with_commit(&format!("stack-{i}"), &format!("f{i}.txt"), "work");
    }
    let but = But::discover(&sb.repo()).unwrap();

    let start = std::time::Instant::now();
    let runs = 5;
    for _ in 0..runs {
        but.status().unwrap();
    }
    let per = start.elapsed() / runs;
    assert!(
        per < std::time::Duration::from_millis(600),
        "status took {per:?} per call; a synchronous refresh would feel sluggish"
    );
    eprintln!("status: {per:?} per call");
}

/// Reproduction attempt for GitHub issue #2, "crash on adding a branch on the unassigned
/// stack": a brand-new workspace with zero applied branches (so the board is just the
/// backlog column), pressing `b` from there, typing a name, and confirming.
#[test]
#[ignore = "requires the GitButler CLI"]
fn creating_a_branch_from_the_empty_unassigned_column_does_not_panic() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;

    let sb = Sandbox::new("newbranchcrash");
    let but = But::discover(&sb.repo()).unwrap();
    // Real detection, not `None`: if `cmux` (or, failing that, `tmux`) happens to be
    // usable (as `cmux` is on the machine this was first reproduced on), the
    // spawn-a-harness path actually runs.
    let mut app = App::new(but, sb.splitter()).expect("build the app against a fresh workspace");
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();

    assert_eq!(app.column_count(), 1, "only the backlog column exists yet");
    assert_eq!(app.col, 0);

    let mut press = |app: &mut App, code: KeyCode| {
        app.on_key(KeyEvent::from(code));
        term.draw(|f| {
            kanstack::ui::draw(f, app);
        }).unwrap();
    };
    press(&mut app, KeyCode::Char('b'));
    for c in "first-branch".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(app.column_count(), 2, "the backlog plus the new lane");
    // `create_branch` reports a failed spawn only as a footer notice, which nothing above
    // looks at: without this, the test passes the same whether the pane opened or not, and
    // the coverage the comment at the top promises is never actually checked.
    if sb.has_live_mux() {
        assert!(sb.spawned_pane("first-branch").is_some(), "a multiplexer was found, so the lane should have opened a pane");
    }
}

/// Same issue, a second guess at repro: unassigned holds real file cards, the card cursor
/// sits on one that isn't index 0, and `b` is pressed with Tab toggled (stack-onto) before
/// typing a name — probing whether the crash needs cards/cursor state present, not just an
/// empty backlog. Renders through the real `ui::draw` after every keystroke too: `on_key`
/// alone can't catch a panic that only happens while drawing (an unsaturated subtraction in
/// a width calculation, an out-of-bounds slice), which is exactly the kind of bug a "crash"
/// report with no repro steps could be.
#[test]
#[ignore = "requires the GitButler CLI"]
fn creating_a_branch_from_unassigned_with_cards_and_a_moved_cursor_does_not_panic() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use ratatui::Terminal;

    let sb = Sandbox::new("newbranchcrash2");
    sb.write("a.txt", "a\n");
    sb.write("b.txt", "b\n");
    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app against the workspace");
    let mut term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    term.draw(|f| {
        kanstack::ui::draw(f, &app);
    }).unwrap();

    assert_eq!(app.column_count(), 1);
    let mut press = |app: &mut App, code: KeyCode| {
        app.on_key(KeyEvent::from(code));
        term.draw(|f| {
            kanstack::ui::draw(f, app);
        }).unwrap();
    };

    // Move the card cursor onto the second file before creating a branch.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Char('b'));
    press(&mut app, KeyCode::Tab); // toggle stack-onto with nothing to stack onto
    press(&mut app, KeyCode::Tab); // toggle back
    for c in "second-branch".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(app.column_count(), 2);
    if sb.has_live_mux() {
        assert!(sb.spawned_pane("second-branch").is_some(), "a multiplexer was found, so the lane should have opened a pane");
    }

    // Pressed again immediately, from the lane `begin_branch` just landed the cursor on:
    // that lane now has a `branch_name`, so this stacks rather than opening a new column
    // (documented, intentional — `b`'s default is to stack when a lane is selected).
    press(&mut app, KeyCode::Char('b'));
    assert_eq!(app.pending_branch_action(), "stack on second-branch");
    for c in "third-branch".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert_eq!(app.column_count(), 2, "stacked onto the existing lane, not a new column");
    assert_eq!(sb.spawned_pane("third-branch"), None, "a stacked branch opens no pane of its own by default");
}

/// The guarantee every test that opens a real pane leans on: a sandbox's teardown closes the
/// panes its test opened and nothing else — not a pane titled the same as its lane, not one
/// whose shell sits in the sandbox's own repo, not the pane running these tests. cmux only:
/// a tmux sandbox gets a server of its own, which can't reach anyone else's panes at all.
///
/// Other sessions open and close panes of their own while this runs, so "nothing else
/// closed" is checked as "everything that vanished was the sandbox's" over the short window
/// of the teardown itself: one of theirs closing in exactly that window fails this, falsely,
/// on the side of caution.
#[test]
#[ignore = "requires the GitButler CLI and cmux"]
fn sandbox_teardown_closes_only_the_panes_it_opened() {
    if skip_if_no_but() {
        return;
    }
    let sb = Sandbox::new("reapselective");
    let Some(mut splitter) = sb.splitter() else {
        eprintln!("skipping: no multiplexer");
        return;
    };
    if splitter.label() != "cmux" {
        eprintln!("skipping: {} isn't cmux", splitter.label());
        return;
    }
    let lane = "reaped-lane";

    // A decoy built to fool a loose reaper: outside the sandbox, but titled like its lane and
    // with its shell sitting in its repo. Closed by its own `Drop` at the end, pass or fail.
    let decoy = CmuxWorkspace::create(&format!("kanstack-live-decoy-{}", std::process::id()), &sb.repo());
    let decoy_surfaces = decoy.surfaces();
    assert!(!decoy_surfaces.is_empty());
    cmux(&["rename-tab", "--workspace", &decoy.uuid, "--surface", &decoy_surfaces[0], lane]).expect("title the decoy");
    let own = std::env::var("CMUX_SURFACE_ID").ok();

    let pane = splitter.spawn_harness(&sb.repo(), lane, None).expect("open the lane's pane");
    kanstack::workstream::record_spawn(&sb.repo(), lane, &pane, None, splitter.workspace().as_deref());
    let workspace = CmuxWorkspace { uuid: splitter.workspace().expect("the pane's workspace is pinned") };
    let sandbox_surfaces = workspace.surfaces();
    std::mem::forget(workspace); // not this test's to close: the sandbox owns it
    assert!(sandbox_surfaces.contains(&pane), "the lane opened in the sandbox's workspace: {sandbox_surfaces:?}");
    assert!(!decoy_surfaces.contains(&pane));

    // Closed live, not mid-start: wait for the harness to be running in it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while sb.harness_launches().is_empty() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let launches = sb.harness_launches();
    assert!(
        launches.len() == 1 && launches[0].ends_with("/repo"),
        "the stand-in harness should have started once, in the repo: {launches:?}"
    );

    let before = cmux_all_surfaces();
    assert!(before.contains(&pane));
    drop(sb);
    let after = cmux_all_surfaces();

    assert!(!after.contains(&pane), "the lane's pane outlived its sandbox");
    for surface in &decoy_surfaces {
        assert!(after.contains(surface), "the decoy {surface} was closed");
    }
    if let Some(own) = own {
        assert!(after.contains(&own), "the pane running these tests was closed");
    }
    let gone: Vec<&String> = before.difference(&after).filter(|s| s.starts_with("surface:")).collect();
    for surface in &gone {
        assert!(sandbox_surfaces.contains(surface), "teardown closed {surface}, which the sandbox never opened");
    }
}

/// `M` then `⏎` used to call `but land` synchronously, freezing the whole UI for however
/// long the push took with zero feedback. It now moves to `Mode::Landing` immediately and
/// finishes on a background thread; `poll_land` (driven by the event loop, not a key) is
/// what applies the result. This drives that exact path through `App`, not `But` directly,
/// so it also catches a regression where the confirm key blocks instead of backgrounding.
#[test]
#[ignore = "requires the GitButler CLI"]
fn landing_through_the_app_runs_on_a_background_thread_and_updates_the_board() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::{App, Mode};
    use kanstack::board::CardKind;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    let sb = Sandbox::new("threadedland");
    sb.branch_with_commit("feat", "a.txt", "Threaded landable");

    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app against the workspace");

    let lane = app
        .board
        .columns
        .iter()
        .position(|c| c.branch_name.as_deref() == Some("feat"))
        .expect("the feat lane exists");
    app.col = lane;

    app.on_key(KeyEvent::from(KeyCode::Char('L')));
    assert_eq!(app.mode, Mode::LandConfirm, "shows the preview before landing");
    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(
        app.mode,
        Mode::Landing,
        "confirming hands off to the background thread rather than blocking here"
    );
    assert!(app.landing.is_some());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.mode == Mode::Landing {
        assert!(std::time::Instant::now() < deadline, "land never completed");
        app.poll_land();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert_eq!(app.mode, Mode::Normal);
    assert!(app.landing.is_none());
    let lane = app
        .board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some("feat"));
    if let Some(lane) = lane {
        assert!(
            lane.cards.iter().all(|c| c.kind != CardKind::Commit),
            "the landed commit should no longer be ahead of the target"
        );
    }
    let log = sb.git(&["log", "--oneline", "--all"]);
    assert!(log.contains("Threaded landable"), "landing must land the content: {log}");
}

/// A watcher-triggered refresh used to run `but status` + `but diff` synchronously on the
/// same thread that reads keys, so a save in another window could stall navigation for the
/// whole round trip. `begin_background_refresh`/`poll_background_refresh` move that off
/// the main thread the same way landing already was; this drives the pair the way `main`'s
/// event loop does, against a change made outside the App entirely (a second `but branch
/// new`, standing in for "someone edited a file in another terminal").
#[test]
#[ignore = "requires the GitButler CLI"]
fn background_refresh_picks_up_an_external_change_without_blocking() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;

    let sb = Sandbox::new("bgrefresh");
    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");
    assert_eq!(app.column_count(), 1, "only the backlog column exists yet");

    // Stand in for a change made outside kanstack entirely -- another terminal, an editor,
    // the desktop app -- the exact thing the filesystem watcher exists to notice.
    sb.branch_with_commit("external-work", "a.txt", "Made outside kanstack");

    app.begin_background_refresh();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while app.column_count() == 1 {
        assert!(std::time::Instant::now() < deadline, "background refresh never landed");
        app.poll_background_refresh();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert_eq!(app.column_count(), 2, "the backlog plus the externally created lane");
    let lane = app
        .board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some("external-work"));
    assert!(lane.is_some(), "the externally created branch should be on the board");
}

/// The one way async refresh could regress on the synchronous version it replaced: a
/// mutation completes *after* a background refresh started but *before* that refresh's
/// (now-stale) result arrives, and the stale result clobbers the mutation's fresher state
/// when it lands. `board_generation` exists specifically to catch this -- this test starts
/// a refresh, only then makes a real mutation through the App, and checks the mutation's
/// result survives once the slower, now-stale refresh finally reports in.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_stale_background_refresh_does_not_clobber_a_newer_mutation() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;

    let sb = Sandbox::new("bgrefreshstale");
    sb.branch_with_commit("feat", "a.txt", "first");
    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");
    assert_eq!(app.column_count(), 2, "backlog plus the one lane so far");

    // Start a refresh as the watcher would, but don't poll it yet -- its result is still
    // in flight, unaware of what happens next.
    app.begin_background_refresh();

    // A real mutation lands through the App while that refresh is still in the air,
    // bumping board_generation past what the in-flight refresh captured.
    sb.branch_with_commit("second", "b.txt", "second");
    app.refresh(); // synchronous, the same as any ordinary in-app refresh
    assert_eq!(app.column_count(), 3, "the mutation's own refresh sees all three lanes");

    // Now let the slower, stale background refresh actually report in.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        app.poll_background_refresh();
        if app.background_refresh_is_pending() {
            assert!(std::time::Instant::now() < deadline, "background refresh never resolved");
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        break;
    }

    assert_eq!(
        app.column_count(),
        3,
        "the stale refresh must not have reverted the newer mutation's state"
    );
}

/// GitHub issue #6: drives the real `tab` toggle through the App against a workspace with
/// loose files nested a couple of directories deep, and checks both directions -- grouped
/// shows a divider per directory, and toggling back off returns to the flat list rather
/// than leaving the board stuck in whichever state the last press left it in.
#[test]
#[ignore = "requires the GitButler CLI"]
fn tab_groups_the_unassigned_lane_by_folder_and_back() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    let sb = Sandbox::new("foldergroup");
    std::fs::create_dir_all(sb.repo().join("src/nested")).unwrap();
    std::fs::create_dir_all(sb.repo().join("docs")).unwrap();
    sb.write("src/nested/wip1.txt", "a\n");
    sb.write("docs/wip2.txt", "b\n");
    sb.write("wip3.txt", "c\n");

    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");
    assert_eq!(app.col, 0, "starts on the backlog lane");
    assert!(!app.unassigned_grouped_by_folder);

    app.on_key(KeyEvent::from(KeyCode::Tab));
    assert!(app.unassigned_grouped_by_folder, "tab turned grouping on");
    let backlog = &app.board.columns[0];
    let groups: Vec<Option<&str>> = backlog.cards.iter().map(|c| c.group.as_deref()).collect();
    assert_eq!(
        groups,
        [Some("."), Some("docs"), Some("src/nested")],
        "root file first under '.', then directories alphabetically"
    );

    app.on_key(KeyEvent::from(KeyCode::Tab));
    assert!(!app.unassigned_grouped_by_folder, "tab turned it back off");
    assert!(
        app.board.columns[0].cards.iter().all(|c| c.group.is_none()),
        "flat view has no groups left over from the toggle"
    );
}

/// `tab` is scoped to the unassigned lane on purpose (see the App-level doc comment on
/// `toggle_unassigned_grouping`) -- pressed from a stack lane it should explain that rather
/// than silently doing nothing or, worse, toggling a view option for a lane the cursor
/// isn't even on.
#[test]
#[ignore = "requires the GitButler CLI"]
fn tab_from_a_stack_lane_explains_itself_instead_of_toggling() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    let sb = Sandbox::new("foldergroupwronglane");
    sb.branch_with_commit("feat", "a.txt", "work");
    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");

    app.on_key(KeyEvent::from(KeyCode::Right)); // off the backlog, onto "feat"
    assert_eq!(app.col, 1);
    app.on_key(KeyEvent::from(KeyCode::Tab));

    assert!(!app.unassigned_grouped_by_folder, "grouping must not have toggled");
    let (msg, _) = app.message.as_ref().expect("tab should explain itself here");
    assert!(
        msg.contains("unassigned"),
        "message should point at the unassigned lane, got {msg:?}"
    );
}

/// Drives the real `Shift+←/→`/`Shift+↑/↓` key events (not the private methods directly)
/// through a real App, so the KeyModifiers::SHIFT wiring in handle_key is exercised end to
/// end and not just the navigation math underneath it.
#[test]
#[ignore = "requires the GitButler CLI"]
fn shift_arrows_page_lanes_and_skip_groups_through_the_app() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::App;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let sb = Sandbox::new("shiftarrows");
    sb.branch_with_commit("base-work", "a.txt", "base work");

    // A second branch, anchored on the first, so the lane is a real two-branch stack --
    // exactly the shape Shift+↑/↓ is meant to skip across.
    let but = But::discover(&sb.repo()).unwrap();
    but.branch_new("mid-work", Some(Placement::Above("base-work"))).unwrap();
    sb.write("b.txt", "mid\n");
    let status = but.status().unwrap();
    let id = status
        .uncommitted_changes
        .iter()
        .find(|c| c.file_path == "b.txt")
        .unwrap()
        .cli_id
        .clone();
    but.commit(&[id], "mid work", "mid-work").unwrap();

    // A separate, unrelated lane, so paging has more than one column to jump across.
    sb.branch_with_commit("other-work", "c.txt", "other work");

    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");
    app.terminal_width = 60;

    // The stacked lane ("mid-work +1") sits after the backlog; land the cursor there and
    // confirm Shift+↓ actually crosses into the other branch's own commits.
    app.on_key(KeyEvent::from(KeyCode::Right));
    let stacked_col = app.col;
    let starting_group = app.board.columns[stacked_col].cards[app.card].group.clone();
    app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT));
    let landed_group = app.board.columns[stacked_col].cards[app.card].group.clone();
    assert_ne!(
        landed_group, starting_group,
        "Shift+Down through the app should cross into the other branch's group"
    );

    // Shift+Right from the backlog should page by more than one lane at this width.
    app.col = 0;
    app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
    assert!(app.col > 1, "a plain Right would only ever move to column 1");
}

/// `but land` refuses a non-base branch outright on a stacked lane -- verified directly
/// against the CLI: "Refusing to land `tip`: it is stacked on top of 1 other segment(s)
/// (base) ... Land the bottom segment `base` (or the whole stack) instead," and there is no
/// flag or stack-id argument that does that landing in one call (`but land <stack-id>`
/// fails with "Expected a branch ID, got a stack"). So `M` on a stacked lane has to land
/// every branch itself, base first. This drives that through a real three-branch stack via
/// the App -- not `But::land_stack` directly -- and checks the whole stack actually lands,
/// in the right order, as one `M` press.
#[test]
#[ignore = "requires the GitButler CLI"]
fn landing_a_stacked_lane_lands_every_branch_base_first() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::{App, Mode};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    let sb = Sandbox::new("cascadeland");
    sb.branch_with_commit("base-work", "a.txt", "base work");
    let but = But::discover(&sb.repo()).unwrap();
    but.branch_new("mid-work", Some(Placement::Above("base-work"))).unwrap();
    sb.write("b.txt", "mid\n");
    let status = but.status().unwrap();
    let id = status
        .uncommitted_changes
        .iter()
        .find(|c| c.file_path == "b.txt")
        .unwrap()
        .cli_id
        .clone();
    but.commit(&[id], "mid work", "mid-work").unwrap();
    but.branch_new("tip-work", Some(Placement::Above("mid-work"))).unwrap();
    sb.write("c.txt", "tip\n");
    let status = but.status().unwrap();
    let id = status
        .uncommitted_changes
        .iter()
        .find(|c| c.file_path == "c.txt")
        .unwrap()
        .cli_id
        .clone();
    but.commit(&[id], "tip work", "tip-work").unwrap();

    // Confirm the CLI really does refuse the tip directly, so this test is exercising the
    // refusal kanstack is meant to route around -- not a scenario that was never blocked.
    let refused = But::discover(&sb.repo())
        .unwrap()
        .land("tip-work")
        .unwrap_err();
    assert!(
        refused.to_string().contains("stacked"),
        "expected the CLI's own stacked-branch refusal, got: {refused}"
    );

    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");
    let lane = app
        .board
        .columns
        .iter()
        .position(|c| c.branch_name.as_deref() == Some("tip-work"))
        .expect("the stacked lane exists, named for its tip");
    app.col = lane;

    app.on_key(KeyEvent::from(KeyCode::Char('L')));
    assert_eq!(app.mode, Mode::LandConfirm);
    app.on_key(KeyEvent::from(KeyCode::Enter));
    assert_eq!(app.mode, Mode::Landing);
    let pending_branches = app.landing.as_ref().unwrap().branch_count;
    assert_eq!(pending_branches, 3, "all three branches, not just the tip");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while app.mode == Mode::Landing {
        assert!(std::time::Instant::now() < deadline, "stacked land never completed");
        app.poll_land();
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    assert_eq!(app.mode, Mode::Normal);
    assert!(
        app.board.columns.iter().all(|c| c.branch_name.as_deref() != Some("tip-work")),
        "the whole stack should be gone from the workspace, landed onto the target"
    );
    let log = sb.git(&["log", "--oneline", "--all"]);
    for expected in ["base work", "mid work", "tip work"] {
        assert!(log.contains(expected), "{expected} missing from landed history:\n{log}");
    }
}

/// Multi-select: `space` two of three unassigned files, `m`, drop them on a lane, confirm
/// — both land as staged changes on that branch in one action, and the file that was never
/// selected stays behind in the backlog untouched. Drives it through real `space`/`m`/
/// arrow/`⏎` KeyEvents against the App, not the private methods directly, and checks the
/// result via a real `but status` afterward, not just in-memory state.
#[test]
#[ignore = "requires the GitButler CLI"]
fn space_selecting_several_unassigned_files_then_m_moves_them_all_at_once() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::{App, Mode};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    let sb = Sandbox::new("multiselect");
    sb.write("keep.txt", "not selected\n");
    sb.write("a.txt", "selected one\n");
    sb.write("b.txt", "selected two\n");
    let but = But::discover(&sb.repo()).unwrap();
    but.branch_new("feat", None).unwrap();

    let but = But::discover(&sb.repo()).unwrap();
    let mut app = App::new(but, sb.splitter()).expect("build the app");
    assert_eq!(app.col, 0, "starts on the backlog");
    let backlog_names: Vec<&str> = app.board.columns[0]
        .cards
        .iter()
        .map(|c| c.title.as_str())
        .collect();
    let a_idx = backlog_names.iter().position(|&n| n == "a.txt").unwrap();
    let b_idx = backlog_names.iter().position(|&n| n == "b.txt").unwrap();

    app.card = a_idx;
    app.on_key(KeyEvent::from(KeyCode::Char(' ')));
    app.card = b_idx;
    app.on_key(KeyEvent::from(KeyCode::Char(' ')));
    assert_eq!(app.selected.len(), 2);

    app.on_key(KeyEvent::from(KeyCode::Char('m')));
    assert_eq!(app.mode, Mode::Moving);
    let feat_col = app
        .board
        .columns
        .iter()
        .position(|c| c.branch_name.as_deref() == Some("feat"))
        .expect("the feat lane exists");
    while app.col != feat_col {
        app.on_key(KeyEvent::from(KeyCode::Right));
    }
    app.on_key(KeyEvent::from(KeyCode::Enter));

    // "feat" has no commits yet, so the batch has nothing to amend into — it parks for a
    // message (`MoveOp::Commit`) instead of landing immediately.
    assert_eq!(app.mode, Mode::Commit, "an empty lane needs a message before it can commit");
    for c in "Two files together".chars() {
        app.on_key(KeyEvent::from(KeyCode::Char(c)));
    }
    app.on_key(KeyEvent::from(KeyCode::Enter));

    assert_eq!(app.mode, Mode::Normal);
    assert!(app.selected.is_empty(), "selection clears once the move lands");

    let but = But::discover(&sb.repo()).unwrap();
    let status = but.status().unwrap();
    let feat = status.stacks.iter().find(|s| s.branches[0].name == "feat").unwrap();
    assert_eq!(feat.branches[0].commits.len(), 1, "both files landed in one combined commit");
    let committed: Vec<&str> = feat.branches[0].commits[0]
        .changes
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert!(committed.contains(&"a.txt"), "a.txt should be in the commit: {committed:?}");
    assert!(committed.contains(&"b.txt"), "b.txt should be in the commit: {committed:?}");
    let still_uncommitted: Vec<&str> = status
        .uncommitted_changes
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert_eq!(
        still_uncommitted,
        ["keep.txt"],
        "only the file that was never selected should remain uncommitted"
    );
}

/// The failure mode that has to stop the board rather than be noted and shrugged off: a
/// plain `git commit` landing on top of the workspace commit.
///
/// `but` then refuses *every* command — `undo` and `oplog restore` included — so a board
/// that merely logged the failure and carried on would be showing a repository that had
/// since moved, with no way to ever catch up, while still accepting keys that act on it.
#[test]
#[ignore = "requires the GitButler CLI"]
fn a_commit_on_the_workspace_head_blocks_the_board_until_recovered() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::app::{App, Mode};
    use kanstack::but::is_workspace_block;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    let sb = Sandbox::new("blocked");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");

    // `--no-verify` because `but setup` installs a pre-commit hook that refuses exactly
    // this. The hook is the first line of defence; what is being tested here is what
    // happens when something gets past it — `--no-verify`, a `core.hooksPath` override,
    // or a project set up before the hook existed.
    sb.write("rogue.txt", "written without asking\n");
    sb.git(&["add", "."]);
    sb.git(&["commit", "-q", "--no-verify", "-m", "agent commit on the virtual head"]);

    let but = But::discover(&sb.repo()).unwrap();
    let err = but.status().expect_err("`but` refuses to read a workspace in this state");
    assert!(is_workspace_block(&err.to_string()), "should be recognised, got: {err}");

    // Startup opens into the modal rather than aborting. Bailing here printed one line of
    // prose and vanished, which reads as a crash.
    let mut app = App::new(But::discover(&sb.repo()).unwrap(), sb.splitter())
        .expect("a blocked workspace still starts the app");
    assert_eq!(app.mode, Mode::Blocked);

    let block = app.blocked.clone().expect("the block is diagnosed, not just reported");
    assert_eq!(block.stray.len(), 1, "one commit is on top: {:?}", block.stray);
    assert!(
        block.stray[0].subject.contains("agent commit"),
        "the stray commit is named: {:?}",
        block.stray
    );
    assert!(
        block.workspace_sha.is_some(),
        "the workspace commit must be identifiable, or `r` cannot be offered"
    );

    // Keys that would act on the frozen board bounce off it.
    for code in [KeyCode::Char('m'), KeyCode::Char('c'), KeyCode::Down, KeyCode::Enter] {
        app.on_key(KeyEvent::from(code));
        assert_eq!(app.mode, Mode::Blocked, "{code:?} must not escape the block");
    }

    // `r` resets back onto the workspace commit and the board comes back live.
    app.on_key(KeyEvent::from(KeyCode::Char('r')));
    assert_eq!(app.mode, Mode::Normal, "recovery returns to a working board");
    assert!(app.blocked.is_none());

    // Nothing was discarded: the stray commit's content is uncommitted work again.
    let status = But::discover(&sb.repo()).unwrap().status().expect("readable again");
    let unassigned: Vec<&str> = status
        .uncommitted_changes
        .iter()
        .map(|c| c.file_path.as_str())
        .collect();
    assert!(
        unassigned.contains(&"rogue.txt"),
        "the commit's content comes back as uncommitted changes: {unassigned:?}"
    );
    assert_eq!(
        status.stacks.len(),
        1,
        "and the lane that was there before survives untouched"
    );
}

/// The same block, but against the *other* shape of workspace commit.
///
/// `but setup` writes a workspace commit carrying a `gitbutler-headers-version` header;
/// the one left behind by `but teardown` followed by `but setup` is a merge commit with no
/// header at all. Identifying the commit by that header alone silently failed on this
/// second kind — the modal came up saying the workspace commit could not be found and
/// withheld the reset, leaving teardown as the only way out of a state a reset would have
/// fixed outright.
#[test]
#[ignore = "requires the GitButler CLI"]
fn the_headerless_workspace_commit_is_still_identified() {
    if skip_if_no_but() {
        return;
    }
    use kanstack::but::is_workspace_block;

    let sb = Sandbox::new("blocked-headerless");
    sb.branch_with_commit("feat-auth", "a.txt", "Add auth middleware");

    // Round-trip out of and back into GitButler mode, which is what produces the
    // header-less variant.
    sb.but(&["teardown"]);
    sb.but(&["setup", "--init", "--json"]);
    let header_block = sb.git(&["cat-file", "-p", "HEAD"]);
    assert!(
        !header_block.contains("gitbutler-headers-version"),
        "this variant is the point of the test; if it now carries a header the fixture is \
         no longer exercising anything:\n{header_block}"
    );

    sb.write("rogue.txt", "written without asking\n");
    sb.git(&["add", "."]);
    sb.git(&["commit", "-q", "--no-verify", "-m", "agent commit on the virtual head"]);

    let but = But::discover(&sb.repo()).unwrap();
    let err = but.status().expect_err("`but` refuses this state");
    assert!(is_workspace_block(&err.to_string()), "should be recognised, got: {err}");

    let block = but.diagnose_workspace_block(err.to_string());
    assert!(
        block.workspace_sha.is_some(),
        "a header-less workspace commit must still be identified, or the reset that would \
         fix this is never offered"
    );
    assert_eq!(block.stray.len(), 1, "one commit is on top: {:?}", block.stray);
}
