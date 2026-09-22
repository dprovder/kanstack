//! Optional bridge to the `orca` CLI (<https://github.com/stablyai/orca>, CLI reference at
//! <https://www.onorca.dev/docs/cli/reference>), the third harness-split backend beside
//! `crate::mux::cmux` and `crate::mux::tmux`.
//!
//! **What is verified, and against what.** The command names and flags come from the
//! published CLI reference. The JSON shapes, the `path:` selector rules, the split orientation
//! and the `ORCA_TERMINAL_HANDLE` variable are *not* in that reference: they were read from
//! Orca's own source (`stablyai/orca` at 9fbdfc5 — `src/cli/handlers/terminal*.ts`,
//! `src/shared/runtime-terminal-contracts.ts`, `src/main/runtime/orca-runtime-resolve-
//! worktree-selector.ts`), marked "(from source)" below.
//!
//! Those were then run against `orcad`, Orca's headless Node runtime, built from that same
//! commit — *not* the desktop app, which was not available. Confirmed live: `repo add`
//! registers a repository's main checkout as a worktree (`path:<root>` then resolves, and
//! `worktree create` is never needed); `terminal create`/`split`/`rename`/`switch`/`close`/
//! `list`/`read` reply as parsed here; `--command` runs as typed input, in the worktree's
//! directory, even for a 5000-character launch line; `ORCA_TERMINAL_HANDLE` is exported and
//! is the terminal's own handle; a split joins its source's tab; `close` is idempotent; a
//! stale anchor fails the split (as `runtime_unavailable`) and the `create` fallback works.
//! The `tests/fixtures/orca_*.json` files are those replies.
//!
//! **Not** verified, because it needs the desktop app or an agent Orca recognizes: that
//! `tui-idle` ever reads *satisfied* (against a plain shell and a plain process it only timed
//! out, so the idle half of `poll_statuses` is untested live); how a blocked approval prompt
//! is reported; whether Orca ever refuses a send as `no-agent` (a plain terminal *accepted*
//! one); and anything that differs between `orcad` and the renderer-backed desktop app.
//!
//! # One worktree per agent, and why kanstack never creates one
//!
//! Orca is built around one git worktree per agent (`orca worktree create`). Every kanstack
//! lane instead shares GitButler's single workspace checkout — the lane is a virtual branch,
//! not a worktree — so this backend never calls `orca worktree create` (or any other
//! `worktree` subcommand). Terminals attach to a worktree Orca already knows about:
//!
//! - `terminal split --terminal <handle>` takes no worktree at all: the new pane joins the
//!   tab of the terminal it splits, and inherits that terminal's worktree (from source).
//!   The first lane splits kanstack's own terminal, so it lands in whichever Orca worktree
//!   kanstack is running in.
//! - `terminal create --worktree path:<repo root>` is the fallback when that split fails
//!   (see `spawn_pane`). `path:` is an exact-path match against Orca's registered
//!   worktrees, *not* a search of enclosing ones like `active`/`current` — and a registered
//!   repository's main checkout counts as a worktree in its own right (from source), which is
//!   exactly what GitButler's workspace is. So this works only if the repository was added to
//!   Orca (`orca repo add`, or the app's "add project"); otherwise Orca answers
//!   `selector_not_found` and the spawn fails with that message instead of creating anything.
//!
//! Either way `cwd` is still `cd`'d into by the launch line, so the harness runs in the
//! workspace regardless of where the new pane's shell happened to start.
//!
//! # Detecting that kanstack is inside an Orca terminal
//!
//! Orca exports `ORCA_TERMINAL_HANDLE` into every terminal it spawns (from source; the docs
//! mention no environment variables). [`Orca::discover`] requires it, for the same reason
//! `Tmux::discover` requires `$TMUX_PANE`: the first lane splits off kanstack's own terminal,
//! so with none there is nothing to split — and, unlike `orca` on `PATH`, it is a positive
//! sign that an Orca runtime is actually running. Orca's source notes that a long-lived shell
//! can keep a *stale* handle across a window reload; that case is what the `create` fallback
//! is for.
//!
//! The binary is `orca`, or `orca-ide` on Linux, where `/usr/bin/orca` already belongs to the
//! GNOME screen reader (from the CLI overview). `KANSTACK_ORCA_BIN` overrides both. Orca's CLI
//! is only on `PATH` after it is registered under Settings in the app.
//!
//! # Splitting
//!
//! Orca's `terminal split` takes `--direction horizontal|vertical`, and nothing else about
//! placement. In Orca's own UI "Split Right" is `vertical` and "Split Down" is `horizontal`,
//! and the new pane is always the second child — right of, or below, the one it splits (from
//! source). So kanstack's `right` and `down` map exactly, while `left` and `up` cannot be
//! honored: they are accepted (so `KANSTACK_SPAWN_DIRECTION=left` doesn't error) and place
//! the pane where `right`/`down` would. That is why the defaults here are `down` then `right`
//! rather than the `up`/`right` of the other two backends.
//!
//! # Busy/idle
//!
//! Unlike `cmux.rs` and `tmux.rs`, nothing here polls CPU: Orca tracks agent state itself.
//! `terminal wait --for tui-idle` blocks until the agent is idle, so it is used as a probe
//! with a short timeout — satisfied means idle, timing out means busy. There is no
//! non-blocking status verb in the CLI (Orca has an internal one the CLI doesn't expose, from
//! source). Two caveats: `tui-idle` relies on Orca recognizing the agent, so a harness it
//! doesn't know reads busy forever; and a terminal blocked on an approval or trust prompt
//! comes back unsatisfied-with-a-reason, which is reported as busy — "idle" would invite
//! `send_task` to type a task into a permission prompt. Liveness comes from `terminal list`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::mux::{command_exists, Multiplexer, OpenRequest};
use crate::pane_status::PaneStatus;

/// How long each `terminal wait --for tui-idle` probe may block before its timeout is read
/// as "busy". Probes for every tracked terminal run in parallel, so this is roughly what one
/// poll costs in total.
const IDLE_PROBE_TIMEOUT_MS: u32 = 1500;

/// Orca, through its own CLI. Stateless: which terminal belongs to which branch is
/// `crate::splitter::Splitter`'s business, and this only knows how to act on a handle.
pub struct Orca {
    bin: PathBuf,
    /// kanstack's own terminal (`$ORCA_TERMINAL_HANDLE`), read once at `discover` time — the
    /// anchor the *first* lane splits off; later lanes chain off the previous lane instead.
    own_terminal: String,
}

impl Orca {
    /// Locates the `orca` binary and kanstack's own terminal handle. Returns `None` (not an
    /// error) whenever either is missing — see the module doc comment for why an explicit
    /// `KANSTACK_ORCA_BIN` doesn't make up for a missing `$ORCA_TERMINAL_HANDLE`.
    pub fn discover() -> Option<Self> {
        let own_terminal = own_terminal_handle()?;
        let bin = match std::env::var_os("KANSTACK_ORCA_BIN") {
            Some(path) => PathBuf::from(path),
            None => {
                let candidate = PathBuf::from(default_bin_name());
                if !command_exists(&candidate) {
                    return None;
                }
                candidate
            }
        };
        Some(Orca { bin, own_terminal })
    }

    /// Whether this process looks to be running inside an Orca terminal — the same
    /// cheap environment check `discover` starts with, for callers that want it without a
    /// binary lookup.
    pub fn running_inside() -> bool {
        own_terminal_handle().is_some()
    }

    fn probe_idle(&self, handle: &str) -> IdleProbe {
        match self.run_raw(&wait_args(handle, IDLE_PROBE_TIMEOUT_MS)) {
            Ok(out) => parse_idle_probe(&String::from_utf8_lossy(&out.stdout)),
            Err(_) => IdleProbe::Failed,
        }
    }

    /// Runs `orca <args>`. The exit status is deliberately not checked here: with `--json`
    /// Orca prints its failure envelope on stdout, and a `terminal wait` that isn't satisfied
    /// or a `terminal send` that isn't accepted exit non-zero with a perfectly good result
    /// beside them (from source) — so callers read the envelope, not the status.
    fn run_raw(&self, args: &[String]) -> Result<Output> {
        Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn `{} {}`", self.bin.display(), args.join(" ")))
    }

    /// [`Self::run_raw`], then the envelope parsed and unwrapped: the `result` on success, an
    /// error naming the `orca` invocation and Orca's own error code and message on failure.
    fn run_json<T: DeserializeOwned>(&self, args: &[String]) -> Result<T> {
        let out = self.run_raw(args)?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let described = || format!("`orca {}` failed", args.join(" "));
        match parse_response::<T>(&stdout) {
            Ok(Response::Ok(result)) => Ok(result),
            Ok(Response::Failed(e)) => bail!("{}: {}: {}", described(), e.code, e.message),
            Err(parse_err) => {
                // Not an envelope at all: the CLI itself failed before it could print one
                // (e.g. Orca isn't running), so whatever it wrote to stderr is the story.
                let stderr = String::from_utf8_lossy(&out.stderr);
                let detail = stderr.trim();
                if detail.is_empty() {
                    Err(parse_err.context(described()))
                } else {
                    bail!("{}: {detail}", described())
                }
            }
        }
    }
}

/// One line for the setup wizard: whether Orca is usable here and, if not, what to do.
pub fn detection() -> String {
    match Orca::discover() {
        Some(_) => "✓ orca found, and this terminal is inside Orca".to_string(),
        None if !Orca::running_inside() => {
            "✗ orca not usable here — not running inside an Orca terminal".to_string()
        }
        None => "✗ orca CLI not found on PATH — register it in Orca's settings, or set KANSTACK_ORCA_BIN".to_string(),
    }
}

impl Multiplexer for Orca {
    fn name(&self) -> &'static str {
        "orca"
    }

    fn running_inside(&self) -> bool {
        Orca::running_inside()
    }

    /// Orca can only place a new terminal right of, or below, the one it splits, so the first
    /// lane defaults to below rather than above. See the module doc for which directions it
    /// can honor.
    fn default_directions(&self) -> (&'static str, &'static str) {
        ("down", "right")
    }

    /// Splits off the previous lane's terminal (or kanstack's own, for the first lane), with
    /// `req.launch` — the whole harness command line, see
    /// `crate::harness::HarnessConfig::launch_line` — as the new terminal's `--command`, then
    /// titles it (e.g. the branch name) — best-effort, since a failed rename shouldn't undo a
    /// harness that did launch.
    ///
    /// If the split fails — most likely a stale or closed anchor handle — the lane opens as a
    /// new tab in the repository's worktree instead (`terminal create --worktree path:…`),
    /// so a stale `$ORCA_TERMINAL_HANDLE` costs the split layout, not the lane. That needs the
    /// repository registered in Orca; see the module doc.
    ///
    /// `--command` reaches the shell as typed input — checked against `orcad`, with the
    /// `cd … && harness …` launch line arriving intact and running in the worktree directory,
    /// at 5000 characters too — which the launch line's shell syntax needs.
    fn open_pane(&self, req: &OpenRequest<'_>) -> Result<String> {
        let (direction, anchor) = match req.after {
            Some(anchor) => (req.chain_direction, anchor),
            None => (req.first_direction, self.own_terminal.as_str()),
        };
        let command = req.launch;

        let handle = match self.run_json::<SplitResult>(&split_args(anchor, direction, command)) {
            Ok(split) => split.split.handle,
            Err(split_err) => self
                .run_json::<CreateResult>(&create_args(req.cwd, command))
                .map(|created| created.terminal.handle)
                .map_err(|create_err| {
                    anyhow!("{split_err:#} — and opening a tab in the worktree instead failed too: {create_err:#}")
                })?,
        };

        let _ = self.run_json::<serde_json::Value>(&rename_args(&handle, req.title));
        Ok(handle)
    }

    /// Sends `text` followed by Enter (`terminal send --enter`). Orca's types allow it to
    /// refuse this as a prompt for an agent it can't see (`no-agent`), and a refusal is
    /// reported by name if it happens — but `orcad` *accepted* text sent to a plain terminal
    /// (with a warning that delivery can't be observed), so this does not reliably guard
    /// against sending before the harness has started.
    fn type_line(&self, pane: &str, text: &str) -> Result<()> {
        let receipt = self.run_json::<SendResult>(&send_args(pane, text))?.send;
        if receipt.accepted {
            return Ok(());
        }
        match receipt.refused_reason.as_deref() {
            Some("no-agent") => bail!("orca sees no agent running in that terminal yet — is the harness still starting?"),
            Some(reason) => bail!("orca refused the message: {reason}"),
            None => bail!("orca did not accept the message"),
        }
    }

    /// Brings the terminal to the front and gives it focus (`terminal switch`).
    fn focus(&self, pane: &str) -> Result<()> {
        self.run_json::<serde_json::Value>(&switch_args(pane))?;
        Ok(())
    }

    fn close(&self, pane: &str) -> Result<()> {
        // How `terminal close` reports an already-closed handle isn't documented, so rather
        // than matching an error code, a handle that no longer appears in `terminal list` is
        // taken as the state closing is after either way — the approach `cmux.rs` takes too.
        let Err(close_err) = self.run_json::<serde_json::Value>(&close_args(pane)) else {
            return Ok(());
        };
        let still_there = self
            .run_json::<ListResult>(&list_args())
            .map(|list| list.terminals.iter().any(|t| t.handle == pane))
            .unwrap_or(true);
        if still_there {
            Err(close_err)
        } else {
            Ok(())
        }
    }

    /// `terminal list` for which terminals still exist (and have not exited), then a
    /// `terminal wait --for tui-idle` probe per live one — see the module doc for how a
    /// probe is read.
    fn probe(&self, panes: &[&str]) -> Result<HashMap<String, PaneStatus>> {
        if panes.is_empty() {
            return Ok(HashMap::new());
        }
        let list: ListResult = self.run_json(&list_args())?;
        let live: Vec<&str> = panes
            .iter()
            .copied()
            .filter(|handle| list.terminals.iter().any(|t| t.handle == *handle && t.exit_cause.is_none()))
            .collect();

        let probes: HashMap<String, IdleProbe> = std::thread::scope(|scope| {
            let jobs: Vec<_> = live
                .iter()
                .map(|handle| scope.spawn(move || (handle.to_string(), self.probe_idle(handle))))
                .collect();
            jobs.into_iter().filter_map(|job| job.join().ok()).collect()
        });

        Ok(classify_statuses(panes, &list, &probes))
    }
}

fn own_terminal_handle() -> Option<String> {
    std::env::var("ORCA_TERMINAL_HANDLE").ok().filter(|handle| !handle.is_empty())
}

/// `orca-ide` on Linux, where `orca` is GNOME's screen reader; `orca` elsewhere.
fn default_bin_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "orca-ide"
    } else {
        "orca"
    }
}

/// Orca's `--direction` for one of kanstack's four directions: its `vertical` puts panes side
/// by side and its `horizontal` stacks them, and the new pane always comes second (right of,
/// or below, the one it splits) — so `left` and `up` can only ever behave as `right` and
/// `down`. See the module doc.
fn split_orientation(direction: &str) -> &'static str {
    match direction {
        "left" | "right" => "vertical",
        _ => "horizontal",
    }
}

/// The `path:` selector for the worktree containing `cwd`. Orca compares it for *equality*
/// against each registered worktree's path (from source), so it is lifted to the enclosing
/// repository root — the way `workstream::state_path` finds one — and canonicalized, since
/// the paths Orca stores come from `git worktree list`, which resolves symlinks.
fn worktree_selector(cwd: &Path) -> String {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let root = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&cwd);
    format!("path:{}", root.display())
}

// Every value that can carry caller-controlled text goes in as `--flag=value`. Orca's parser
// reads `--flag value` as a boolean flag whenever the value itself starts with `--`, and
// documents `=` as the only unambiguous way to pass one (from source) — a task like
// "--fix the tests" must not turn into an unknown option.

fn split_args(terminal: &str, direction: &str, command: &str) -> Vec<String> {
    let mut args = strings(["terminal", "split"]);
    args.push(format!("--terminal={terminal}"));
    args.push(format!("--direction={}", split_orientation(direction)));
    args.push(format!("--command={command}"));
    args.push("--json".to_string());
    args
}

fn create_args(cwd: &Path, command: &str) -> Vec<String> {
    let mut args = strings(["terminal", "create"]);
    args.push(format!("--worktree={}", worktree_selector(cwd)));
    args.push(format!("--command={command}"));
    args.push("--json".to_string());
    args
}

fn rename_args(terminal: &str, title: &str) -> Vec<String> {
    let mut args = strings(["terminal", "rename"]);
    args.push(format!("--terminal={terminal}"));
    args.push(format!("--title={title}"));
    args.push("--json".to_string());
    args
}

fn send_args(terminal: &str, text: &str) -> Vec<String> {
    let mut args = strings(["terminal", "send"]);
    args.push(format!("--terminal={terminal}"));
    args.push(format!("--text={text}"));
    args.push("--enter".to_string());
    args.push("--json".to_string());
    args
}

fn wait_args(terminal: &str, timeout_ms: u32) -> Vec<String> {
    let mut args = strings(["terminal", "wait"]);
    args.push(format!("--terminal={terminal}"));
    args.push("--for=tui-idle".to_string());
    args.push(format!("--timeout-ms={timeout_ms}"));
    args.push("--json".to_string());
    args
}

fn switch_args(terminal: &str) -> Vec<String> {
    let mut args = strings(["terminal", "switch"]);
    args.push(format!("--terminal={terminal}"));
    args.push("--json".to_string());
    args
}

fn close_args(terminal: &str) -> Vec<String> {
    let mut args = strings(["terminal", "close"]);
    args.push(format!("--terminal={terminal}"));
    args.push("--json".to_string());
    args
}

/// Every terminal in every worktree — no `--worktree` filter, since a lane may be in a
/// different worktree than the one kanstack is in.
fn list_args() -> Vec<String> {
    strings(["terminal", "list", "--json"])
}

fn strings<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.into_iter().map(str::to_string).collect()
}

/// What `--json` prints: `{"id":…,"ok":true,"result":…,"_meta":…}` on success and
/// `{"id":…,"ok":false,"error":{"code":…,"message":…,"data":…},"_meta":…}` on failure, both
/// on stdout (from source).
#[derive(Deserialize)]
struct Envelope<T> {
    ok: bool,
    result: Option<T>,
    error: Option<ErrorBody>,
}

#[derive(Deserialize, Debug, PartialEq, Eq)]
struct ErrorBody {
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug)]
enum Response<T> {
    Ok(T),
    Failed(ErrorBody),
}

/// Parses the first JSON object in `stdout`, skipping anything printed ahead of it — the CLI
/// isn't documented to write a banner or an update notice there, but `but` does, and this is
/// cheap insurance against reading one as a parse failure. `Err` means there was no
/// envelope at all.
fn parse_response<T: DeserializeOwned>(stdout: &str) -> Result<Response<T>> {
    let start = stdout.find('{').ok_or_else(|| anyhow!("no JSON in the output: {stdout:?}"))?;
    let envelope: Envelope<T> = serde_json::Deserializer::from_str(&stdout[start..])
        .into_iter::<Envelope<T>>()
        .next()
        .ok_or_else(|| anyhow!("no JSON in the output: {stdout:?}"))?
        .with_context(|| format!("unexpected output shape: {stdout:?}"))?;
    match (envelope.ok, envelope.result, envelope.error) {
        (true, Some(result), _) => Ok(Response::Ok(result)),
        (false, _, Some(error)) => Ok(Response::Failed(error)),
        _ => bail!("envelope has neither a result nor an error: {stdout:?}"),
    }
}

#[derive(Deserialize)]
struct Handle {
    handle: String,
}

/// `terminal split` → `{"split": {"handle": …, …}}`.
#[derive(Deserialize)]
struct SplitResult {
    split: Handle,
}

/// `terminal create` → `{"terminal": {"handle": …, "worktreeId": …, …}}`.
#[derive(Deserialize)]
struct CreateResult {
    terminal: Handle,
}

/// `terminal send` → `{"send": {"handle": …, "accepted": …, "refusedReason"?: "no-agent" |
/// "permission", …}}`. An unaccepted send still exits non-zero, with this as its result.
#[derive(Deserialize)]
struct SendResult {
    send: SendReceipt,
}

#[derive(Deserialize)]
struct SendReceipt {
    accepted: bool,
    #[serde(default, rename = "refusedReason")]
    refused_reason: Option<String>,
}

/// `terminal list` → `{"terminals": [...], "totalCount": …, "truncated": …, "hostScope"?: …}`,
/// the result itself rather than nested under a key like the others.
#[derive(Deserialize)]
struct ListResult {
    #[serde(default)]
    terminals: Vec<TerminalSummary>,
    /// More terminals exist than were listed, so one missing from `terminals` proves nothing.
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TerminalSummary {
    handle: String,
    /// Absent while the process is running; present once it has ended, however it ended.
    #[serde(default, rename = "exitCause")]
    exit_cause: Option<serde_json::Value>,
}

/// `terminal wait` → `{"wait": {"satisfied": …, "blockedReason"?: …, …}}`. Not satisfied
/// exits non-zero; a timeout is instead an error envelope with code `timeout`.
#[derive(Deserialize)]
struct WaitResult {
    wait: WaitReceipt,
}

#[derive(Deserialize)]
struct WaitReceipt {
    satisfied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleProbe {
    Idle,
    Busy,
    /// The probe couldn't say either way; the pane keeps whatever status it had.
    Failed,
}

/// Reads one `terminal wait --for tui-idle` answer. Satisfied is idle; a timeout is busy;
/// unsatisfied *with* an answer means Orca found the terminal blocked on a prompt (approval,
/// trust, update — see the module doc for why that is busy, not idle). Anything else,
/// including output that isn't an envelope, is `Failed`.
fn parse_idle_probe(stdout: &str) -> IdleProbe {
    match parse_response::<WaitResult>(stdout) {
        Ok(Response::Ok(wait)) if wait.wait.satisfied => IdleProbe::Idle,
        Ok(Response::Ok(_)) => IdleProbe::Busy,
        Ok(Response::Failed(error)) if error.code == "timeout" => IdleProbe::Busy,
        Ok(Response::Failed(_)) | Err(_) => IdleProbe::Failed,
    }
}

/// Reads each of `panes` off the listing and its probe. A handle missing from a complete
/// listing, or listed with an exit cause, is `Dead`; a live one takes its probe's answer. A
/// live one whose probe couldn't give one is left out — no news — and so is one missing from
/// a *truncated* listing, which proves nothing about it: either way the caller keeps its
/// previous status.
fn classify_statuses(
    panes: &[&str],
    list: &ListResult,
    probes: &HashMap<String, IdleProbe>,
) -> HashMap<String, PaneStatus> {
    panes
        .iter()
        .filter_map(|&handle| {
            let status = match list.terminals.iter().find(|t| t.handle == handle) {
                None if list.truncated => return None,
                None => PaneStatus::Dead,
                Some(t) if t.exit_cause.is_some() => PaneStatus::Dead,
                Some(_) => match probes.get(handle) {
                    Some(IdleProbe::Idle) => PaneStatus::Idle,
                    Some(IdleProbe::Busy) => PaneStatus::Busy,
                    Some(IdleProbe::Failed) | None => return None,
                },
            };
            Some((handle.to_string(), status))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::splitter::Splitter;

    /// Runs `body` with each of `vars` swapped out and restored afterwards. These are
    /// process-wide state also read by the other backends' and `splitter.rs`'s own
    /// env-mutating tests — held for the whole call via `SPLIT_BACKEND_ENV_LOCK`, not just
    /// the swap, so none of them can interleave with each other either.
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

    #[test]
    fn discover_is_none_without_the_binary_on_path_or_an_override() {
        let dir = std::env::temp_dir().join(format!("kanstack-orca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(
            &[("PATH", Some(dir.to_str().unwrap())), ("KANSTACK_ORCA_BIN", None), ("ORCA_TERMINAL_HANDLE", Some("term_1"))],
            || assert!(Orca::discover().is_none()),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The handle is what makes "inside Orca" positive: an explicit binary override can't make
    /// up for having no terminal of our own to split — same stance as `Tmux::discover`.
    #[test]
    fn discover_is_none_outside_an_orca_terminal_even_with_an_explicit_override() {
        with_env(&[("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")), ("ORCA_TERMINAL_HANDLE", None)], || {
            assert!(Orca::discover().is_none());
            assert!(!Orca::running_inside());
        });
        with_env(&[("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")), ("ORCA_TERMINAL_HANDLE", Some(""))], || {
            assert!(Orca::discover().is_none(), "an empty handle is no handle");
        });
    }

    #[test]
    fn discover_trusts_an_explicit_override_inside_an_orca_terminal() {
        with_env(&[("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")), ("ORCA_TERMINAL_HANDLE", Some("term_1"))], || {
            assert!(Orca::running_inside());
            let orca = Orca::discover().expect("an explicit override is never second-guessed");
            assert_eq!(orca.own_terminal, "term_1");
        });
    }

    #[test]
    fn discover_defaults_to_splitting_down_then_chaining_right() {
        with_env(
            &[
                ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
                ("ORCA_TERMINAL_HANDLE", Some("term_1")),
                ("KANSTACK_ORCA_DIRECTION", None),
                ("KANSTACK_ORCA_CHAIN_DIRECTION", None),
            ],
            || {
                let orca = Orca::discover().unwrap();
                assert_eq!(crate::mux::configured_directions(&orca), ("down".to_string(), "right".to_string()));
            },
        );
        with_env(
            &[
                ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
                ("ORCA_TERMINAL_HANDLE", Some("term_1")),
                ("KANSTACK_ORCA_DIRECTION", Some("below")),
                ("KANSTACK_ORCA_CHAIN_DIRECTION", Some("above")),
            ],
            || {
                let orca = Orca::discover().unwrap();
                assert_eq!(crate::mux::configured_directions(&orca), ("down".to_string(), "up".to_string()));
            },
        );
    }

    #[test]
    fn the_binary_is_orca_ide_only_on_linux() {
        assert_eq!(default_bin_name(), if cfg!(target_os = "linux") { "orca-ide" } else { "orca" });
    }

    #[test]
    fn split_orientation_maps_right_and_down_exactly_and_the_others_onto_them() {
        assert_eq!(split_orientation("right"), "vertical", "Orca's `vertical` is side by side");
        assert_eq!(split_orientation("down"), "horizontal", "Orca's `horizontal` is stacked");
        assert_eq!(split_orientation("left"), "vertical", "can only be honored as `right`");
        assert_eq!(split_orientation("up"), "horizontal", "can only be honored as `down`");
    }

    #[test]
    fn split_args_pin_the_anchor_and_always_state_the_orientation() {
        assert_eq!(
            split_args("term_9", "up", "cd '/repo' && claude"),
            ["terminal", "split", "--terminal=term_9", "--direction=horizontal", "--command=cd '/repo' && claude", "--json"]
        );
    }

    /// The whole reason for the `--flag=value` spelling: with a space, a value that starts
    /// with `--` is read as the next flag and the real one becomes a bare boolean.
    #[test]
    fn a_value_that_looks_like_a_flag_stays_a_value() {
        let args = send_args("term_9", "--fix the tests");
        assert!(args.contains(&"--text=--fix the tests".to_string()), "{args:?}");
        assert!(args.contains(&"--enter".to_string()) && args.contains(&"--json".to_string()));
    }

    #[test]
    fn the_terminal_verbs_name_the_handle_and_ask_for_json() {
        assert_eq!(switch_args("t"), ["terminal", "switch", "--terminal=t", "--json"]);
        assert_eq!(close_args("t"), ["terminal", "close", "--terminal=t", "--json"]);
        assert_eq!(rename_args("t", "fix-login"), ["terminal", "rename", "--terminal=t", "--title=fix-login", "--json"]);
        assert_eq!(list_args(), ["terminal", "list", "--json"]);
        assert_eq!(
            wait_args("t", 1500),
            ["terminal", "wait", "--terminal=t", "--for=tui-idle", "--timeout-ms=1500", "--json"]
        );
    }

    /// kanstack must never create a worktree of its own: no argument list it builds may
    /// touch the `worktree` subcommand.
    #[test]
    fn no_command_kanstack_builds_touches_the_worktree_subcommand() {
        let all = [
            split_args("t", "down", "c"),
            create_args(Path::new("/repo"), "c"),
            rename_args("t", "n"),
            send_args("t", "x"),
            wait_args("t", 1),
            switch_args("t"),
            close_args("t"),
            list_args(),
        ];
        for args in all {
            assert_eq!(args[0], "terminal", "{args:?}");
        }
    }

    #[test]
    fn create_attaches_to_the_existing_worktree_by_path_and_never_makes_one() {
        let args = create_args(Path::new("/nonexistent/repo"), "cd '/nonexistent/repo' && claude");
        assert_eq!(
            args,
            ["terminal", "create", "--worktree=path:/nonexistent/repo", "--command=cd '/nonexistent/repo' && claude", "--json"]
        );
    }

    /// `path:` is an exact match against a registered worktree's root, so a working directory
    /// below the repository root must be lifted to it.
    #[test]
    fn worktree_selector_lifts_a_subdirectory_to_the_repository_root() {
        let dir = std::env::temp_dir().join(format!("kanstack-orca-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        // Canonicalized on purpose: on macOS the temp dir is itself a symlink.
        let root = dir.canonicalize().unwrap();
        assert_eq!(worktree_selector(&dir.join("src/deep")), format!("path:{}", root.display()));
        assert_eq!(worktree_selector(&dir), format!("path:{}", root.display()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Real replies, captured from Orca's headless runtime (`orcad`, built from stablyai/orca
    // at 9fbdfc5) with paths, username and hostname scrubbed. The desktop app was not run.
    const CREATE_LIVE: &str = include_str!("../../tests/fixtures/orca_create.json");
    const SPLIT_LIVE: &str = include_str!("../../tests/fixtures/orca_split.json");
    const LIST_LIVE: &str = include_str!("../../tests/fixtures/orca_list.json");
    const WAIT_TIMEOUT_LIVE: &str = include_str!("../../tests/fixtures/orca_wait_timeout.json");
    const SEND_PLAIN_LIVE: &str = include_str!("../../tests/fixtures/orca_send_plain.json");
    const CLOSE_LIVE: &str = include_str!("../../tests/fixtures/orca_close.json");
    const SPLIT_STALE_LIVE: &str = include_str!("../../tests/fixtures/orca_split_stale.json");
    const BAD_SELECTOR_LIVE: &str = include_str!("../../tests/fixtures/orca_create_bad_selector.json");

    #[test]
    fn live_create_and_split_replies_yield_the_new_handle() {
        let Response::Ok(created) = parse_response::<CreateResult>(CREATE_LIVE).unwrap() else { panic!("not ok") };
        assert!(created.terminal.handle.starts_with("term_"), "{}", created.terminal.handle);
        let Response::Ok(split) = parse_response::<SplitResult>(SPLIT_LIVE).unwrap() else { panic!("not ok") };
        assert!(split.split.handle.starts_with("term_"), "{}", split.split.handle);
    }

    /// The live listing carries many fields this module ignores, and `"exitCause": null`
    /// (not an absent key) for a running terminal — both must read as "alive".
    #[test]
    fn a_live_listing_reads_running_terminals_as_alive_and_a_missing_one_as_dead() {
        let Response::Ok(list) = parse_response::<ListResult>(LIST_LIVE).unwrap() else { panic!("not ok") };
        assert_eq!(list.terminals.len(), 2);
        assert!(!list.truncated);
        assert!(list.terminals.iter().all(|t| t.exit_cause.is_none()));

        let live = list.terminals[0].handle.clone();
        let gone = "term_00000000-0000-0000-0000-000000000000";
        let probes = HashMap::from([(live.clone(), IdleProbe::Busy)]);
        let statuses = classify_statuses(&[live.as_str(), gone], &list, &probes);
        assert_eq!(statuses[live.as_str()], PaneStatus::Busy);
        assert_eq!(statuses[gone], PaneStatus::Dead);
    }

    /// An idle *shell* is not an idle *agent*: Orca's `tui-idle` timed out against one, so
    /// only a harness Orca recognizes can ever read idle.
    #[test]
    fn a_live_tui_idle_timeout_reads_as_busy() {
        assert_eq!(parse_idle_probe(WAIT_TIMEOUT_LIVE), IdleProbe::Busy);
    }

    /// Orca *accepted* text sent to a plain, non-agent terminal — with a warning that it
    /// can't see delivery, rather than the `no-agent` refusal its types allow for.
    #[test]
    fn a_live_send_to_a_plain_terminal_is_accepted() {
        let Response::Ok(sent) = parse_response::<SendResult>(SEND_PLAIN_LIVE).unwrap() else { panic!("not ok") };
        assert!(sent.send.accepted);
        assert_eq!(sent.send.refused_reason, None);
    }

    #[test]
    fn a_live_close_reply_parses_as_a_success() {
        assert!(matches!(parse_response::<serde_json::Value>(CLOSE_LIVE), Ok(Response::Ok(_))));
    }

    /// A stale anchor handle fails as `runtime_unavailable` — not a "stale" code — which is
    /// why `spawn_pane` falls back on any split failure instead of matching a code.
    #[test]
    fn live_failures_carry_orcas_own_error_codes() {
        let Response::Failed(stale) = parse_response::<SplitResult>(SPLIT_STALE_LIVE).unwrap() else { panic!("not a failure") };
        assert_eq!(stale.code, "runtime_unavailable");
        let Response::Failed(bad) = parse_response::<CreateResult>(BAD_SELECTOR_LIVE).unwrap() else { panic!("not a failure") };
        assert_eq!(bad.code, "selector_not_found");
    }

    // The rest could not be provoked against `orcad` — they need an agent Orca recognizes
    // or a blocked prompt — so they are shaped from Orca's TypeScript result types (see the
    // module doc) and not observed.

    const SPLIT_OK: &str = r#"{"id":"req_1","ok":true,"result":{"split":{"handle":"term_new","tabId":"tab_1","paneRuntimeId":4}},"_meta":{"runtimeId":"rt"}}"#;
    const SEND_REFUSED: &str = r#"{"id":"r","ok":true,"result":{"send":{"handle":"t","accepted":false,"bytesWritten":0,"refusedReason":"no-agent"}},"_meta":{"runtimeId":"rt"}}"#;
    const WAIT_IDLE: &str = r#"{"id":"r","ok":true,"result":{"wait":{"handle":"t","condition":"tui-idle","satisfied":true,"status":"running","exitCode":null}},"_meta":{"runtimeId":"rt"}}"#;
    const WAIT_BLOCKED: &str = r#"{"id":"r","ok":true,"result":{"wait":{"handle":"t","condition":"tui-idle","satisfied":false,"status":"running","exitCode":null,"blockedReason":"agent-approval-prompt"}},"_meta":{"runtimeId":"rt"}}"#;
    const WAIT_TIMEOUT: &str = r#"{"id":"r","ok":false,"error":{"code":"timeout","message":"timeout"},"_meta":{"runtimeId":"rt"}}"#;
    const LIST: &str = r#"{"id":"r","ok":true,"result":{"terminals":[
        {"handle":"term_a","worktreeId":"repo::/repo","worktreePath":"/repo","title":"feat-a","connected":true,"writable":true},
        {"handle":"term_b","worktreeId":"repo::/repo","worktreePath":"/repo","title":"feat-b","connected":true,"writable":true},
        {"handle":"term_c","worktreeId":"repo::/repo","worktreePath":"/repo","title":"feat-c","connected":false,"writable":false,"exitCause":{"kind":"exited","exitCode":0}}
    ],"totalCount":3,"truncated":false},"_meta":{"runtimeId":"rt"}}"#;

    #[test]
    fn parse_response_unwraps_a_result_and_names_an_error() {
        let Response::Ok(split) = parse_response::<SplitResult>(SPLIT_OK).unwrap() else { panic!("not ok") };
        assert_eq!(split.split.handle, "term_new");

        let failed = r#"{"id":"r","ok":false,"error":{"code":"selector_not_found","message":"No worktree","data":{}}}"#;
        let Response::Failed(error) = parse_response::<SplitResult>(failed).unwrap() else { panic!("not a failure") };
        assert_eq!(error, ErrorBody { code: "selector_not_found".into(), message: "No worktree".into() });
    }

    #[test]
    fn parse_response_skips_anything_printed_ahead_of_the_envelope_and_after_it() {
        let noisy = format!("Update available\n{SPLIT_OK}\ntrailing\n");
        assert!(matches!(parse_response::<SplitResult>(&noisy), Ok(Response::Ok(_))));
    }

    #[test]
    fn parse_response_rejects_output_that_is_not_an_envelope() {
        assert!(parse_response::<SplitResult>("").is_err());
        assert!(parse_response::<SplitResult>("Orca is not running").is_err());
        assert!(parse_response::<SplitResult>("{\"unrelated\":1}").is_err());
        // A split reply asked for as a wait: right envelope, wrong result.
        assert!(parse_response::<WaitResult>(SPLIT_OK).is_err());
    }

    #[test]
    fn a_refused_send_parses_as_a_result_with_its_reason() {
        let Response::Ok(sent) = parse_response::<SendResult>(SEND_REFUSED).unwrap() else { panic!("not ok") };
        assert!(!sent.send.accepted);
        assert_eq!(sent.send.refused_reason.as_deref(), Some("no-agent"));
    }

    #[test]
    fn idle_probe_reads_satisfied_as_idle_and_a_timeout_or_a_blocked_prompt_as_busy() {
        assert_eq!(parse_idle_probe(WAIT_IDLE), IdleProbe::Idle);
        assert_eq!(parse_idle_probe(WAIT_TIMEOUT), IdleProbe::Busy);
        assert_eq!(parse_idle_probe(WAIT_BLOCKED), IdleProbe::Busy);
    }

    #[test]
    fn idle_probe_gives_up_on_anything_else() {
        let stale = r#"{"id":"r","ok":false,"error":{"code":"terminal_handle_stale","message":"stale"}}"#;
        assert_eq!(parse_idle_probe(stale), IdleProbe::Failed);
        assert_eq!(parse_idle_probe(""), IdleProbe::Failed);
        assert_eq!(parse_idle_probe("Orca is not running"), IdleProbe::Failed);
    }

    fn list() -> ListResult {
        let Response::Ok(list) = parse_response::<ListResult>(LIST).unwrap() else { panic!("not ok") };
        list
    }

    #[test]
    fn classify_statuses_takes_a_live_terminals_probe() {
        let probes = HashMap::from([("term_a".to_string(), IdleProbe::Busy), ("term_b".to_string(), IdleProbe::Idle)]);
        let statuses = classify_statuses(&["term_a", "term_b"], &list(), &probes);
        assert_eq!(statuses["term_a"], PaneStatus::Busy);
        assert_eq!(statuses["term_b"], PaneStatus::Idle);
    }

    /// Gone from a complete listing, or still listed but ended: either way its harness is
    /// not running, whatever any probe says.
    #[test]
    fn classify_statuses_marks_a_missing_or_exited_terminal_dead() {
        let probes = HashMap::from([("term_c".to_string(), IdleProbe::Idle), ("term_zzz".to_string(), IdleProbe::Idle)]);
        let statuses = classify_statuses(&["term_zzz", "term_c"], &list(), &probes);
        assert_eq!(statuses["term_zzz"], PaneStatus::Dead);
        assert_eq!(statuses["term_c"], PaneStatus::Dead);
    }

    /// A live terminal whose probe failed, or never ran, is left out: no news, so the caller
    /// keeps whatever it already knew.
    #[test]
    fn classify_statuses_leaves_out_a_terminal_when_a_probe_fails_or_is_missing() {
        let probes = HashMap::from([("term_a".to_string(), IdleProbe::Failed)]);
        let statuses = classify_statuses(&["term_a", "term_b"], &list(), &probes);
        assert!(statuses.is_empty(), "{statuses:?}");
    }

    /// A truncated listing can't prove a terminal is gone — it may just not have made the
    /// page — so it must not flip a live pane to `Dead`.
    #[test]
    fn classify_statuses_does_not_read_absence_from_a_truncated_listing_as_death() {
        let truncated = r#"{"id":"r","ok":true,"result":{"terminals":[],"totalCount":500,"truncated":true}}"#;
        let Response::Ok(list) = parse_response::<ListResult>(truncated).unwrap() else { panic!("not ok") };
        assert!(classify_statuses(&["term_a"], &list, &HashMap::new()).is_empty());
    }

    // What follows drives the real `spawn`/`send`/`poll`/`stop` paths against a stand-in
    // `orca`: a shell script that logs its arguments and prints the envelopes above. It
    // checks kanstack's process handling — arguments arriving intact, exit statuses read
    // from the envelope rather than trusted, the split-to-create fallback — but only as
    // faithfully as those envelopes match a real Orca's, which is unverified.

    /// Writes a fake `orca` running `body` after logging its arguments, and returns it with
    /// its log path. Both live in a directory unique to `tag`.
    fn fake_orca(tag: &str, body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kanstack-orca-fake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, log) = (dir.join("orca"), dir.join("log"));
        std::fs::write(&bin, format!("#!/bin/sh\necho \"$@\" >> '{}'\n{body}\n", log.display())).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        (bin, log)
    }

    /// Runs `test` with a `Splitter` over an `Orca` discovered against the fake, inside an
    /// "Orca terminal" whose own handle is `term_own`, and the fake's log path. Going through
    /// the `Splitter` is what kanstack itself does, so these cover the shared layer's use of
    /// the backend too. Cleans up after itself.
    fn with_fake_orca(tag: &str, body: &str, test: impl FnOnce(Splitter, &Path)) {
        let (bin, log) = fake_orca(tag, body);
        with_env(
            &[
                ("KANSTACK_ORCA_BIN", Some(bin.to_str().unwrap())),
                ("ORCA_TERMINAL_HANDLE", Some("term_own")),
                ("KANSTACK_ORCA_DIRECTION", None),
                ("KANSTACK_ORCA_CHAIN_DIRECTION", None),
                ("KANSTACK_HARNESS", Some("claude")),
            ],
            || {
                let orca = Orca::discover().unwrap();
                test(Splitter::new(std::sync::Arc::new(orca), crate::harness::HarnessConfig::new("claude")), &log)
            },
        );
        let _ = std::fs::remove_dir_all(bin.parent().unwrap());
    }

    fn spawn(orca: &mut Splitter, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<String> {
        orca.spawn_harness(cwd, name, initial_message)
    }

    fn log_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Answers a split with a handle derived from the terminal it was asked to split, so a
    /// chain of splits can be told apart, and anything else (a rename) with an empty result.
    const SPLITS_AND_ACKS: &str = r#"
case "$2" in
  split) printf '{"id":"r","ok":true,"result":{"split":{"handle":"after_%s","tabId":"t"}}}\n' "${3#--terminal=}" ;;
  *) echo '{"id":"r","ok":true,"result":{}}' ;;
esac"#;

    #[test]
    fn the_first_lane_splits_kanstacks_own_terminal_down_and_the_next_chains_off_it_to_the_right() {
        with_fake_orca("chain", SPLITS_AND_ACKS, |mut orca, log| {
            let first = spawn(&mut orca, Path::new("/nonexistent/repo"), "feat-a", Some("--fix it")).unwrap();
            let second = spawn(&mut orca, Path::new("/nonexistent/repo"), "feat-b", None).unwrap();
            assert_eq!(first, "after_term_own");
            assert_eq!(second, "after_after_term_own");
            assert_eq!(orca.pane_id("feat-b").as_deref(), Some("after_after_term_own"));

            let lines = log_lines(log);
            let splits: Vec<_> = lines.iter().filter(|l| l.starts_with("terminal split")).collect();
            assert_eq!(splits.len(), 2, "{lines:#?}");
            assert!(splits[0].starts_with("terminal split --terminal=term_own --direction=horizontal --command=cd '/nonexistent/repo' && claude"), "{}", splits[0]);
            assert!(splits[0].contains("--fix it"), "the first message rides the launch line: {}", splits[0]);
            assert!(splits[1].starts_with("terminal split --terminal=after_term_own --direction=vertical --command="), "{}", splits[1]);
            assert!(lines.contains(&"terminal rename --terminal=after_term_own --title=feat-a --json".to_string()), "{lines:#?}");
        });
    }

    /// A stale handle (or any failed split) costs the layout, not the lane — and along the
    /// way kanstack still never touches `orca worktree`.
    #[test]
    fn a_failed_split_falls_back_to_a_tab_in_the_existing_worktree_and_never_creates_one() {
        let body = r#"
case "$2" in
  split) echo '{"id":"r","ok":false,"error":{"code":"terminal_handle_stale","message":"stale"}}'; exit 1 ;;
  create) echo '{"id":"r","ok":true,"result":{"terminal":{"handle":"term_tab","worktreeId":"w","title":null}}}' ;;
  *) echo '{"id":"r","ok":true,"result":{}}' ;;
esac"#;
        with_fake_orca("fallback", body, |mut orca, log| {
            let handle = spawn(&mut orca, Path::new("/nonexistent/repo"), "feat-a", None).unwrap();
            assert_eq!(handle, "term_tab");
            let lines = log_lines(log);
            assert!(lines.iter().any(|l| l.starts_with("terminal create --worktree=path:/nonexistent/repo --command=cd ")), "{lines:#?}");
            assert!(lines.iter().all(|l| l.starts_with("terminal ")), "only `terminal` verbs, ever: {lines:#?}");
        });
    }

    #[test]
    fn a_spawn_where_both_the_split_and_the_fallback_fail_reports_both() {
        let body = r#"echo '{"id":"r","ok":false,"error":{"code":"selector_not_found","message":"No such worktree"}}'; exit 1"#;
        with_fake_orca("both-fail", body, |mut orca, _| {
            let err = spawn(&mut orca, Path::new("/nonexistent/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("terminal split") && err.contains("terminal create"), "{err}");
            assert!(err.contains("selector_not_found"), "{err}");
            assert!(!orca.has_pane("feat-a"), "a lane that never opened must not be tracked");
        });
    }

    /// Orca exits non-zero for an unaccepted send but still prints a result: kanstack reads
    /// that, and names the reason.
    #[test]
    fn a_refused_send_is_an_error_that_says_why_and_an_accepted_one_is_not() {
        let refused = r#"echo '{"id":"r","ok":true,"result":{"send":{"handle":"term_a","accepted":false,"bytesWritten":0,"refusedReason":"no-agent"}}}'; exit 1"#;
        with_fake_orca("refused", refused, |mut orca, log| {
            orca.adopt("feat-a", "term_a");
            let err = orca.send_task("feat-a", "--fix the tests").unwrap_err().to_string();
            assert!(err.contains("no agent"), "{err}");
            assert_eq!(log_lines(log), ["terminal send --terminal=term_a --text=--fix the tests --enter --json"]);
            assert!(orca.send_task("nope", "x").is_err(), "an untracked lane has no terminal to send to");
        });
        let accepted = r#"echo '{"id":"r","ok":true,"result":{"send":{"handle":"term_a","accepted":true,"bytesWritten":4}}}'"#;
        with_fake_orca("accepted", accepted, |mut orca, _| {
            orca.adopt("feat-a", "term_a");
            orca.send_task("feat-a", "go").unwrap();
        });
    }

    /// `Orca is not running` and the like arrive on stderr with no envelope at all.
    #[test]
    fn a_cli_that_prints_no_envelope_reports_its_stderr() {
        with_fake_orca("stderr", "echo 'Orca is not running. Run orca open first.' >&2; exit 1", |mut orca, _| {
            orca.adopt("feat-a", "term_a");
            let err = orca.focus("feat-a").unwrap_err().to_string();
            assert!(err.contains("terminal switch") && err.contains("Orca is not running"), "{err}");
        });
    }

    #[test]
    fn poll_statuses_lists_once_probes_each_live_terminal_and_reads_gone_ones_as_dead() {
        let body = r#"
case "$2" in
  list) echo '{"id":"r","ok":true,"result":{"terminals":[{"handle":"term_a"},{"handle":"term_b"}],"totalCount":2,"truncated":false}}' ;;
  wait) case "$3" in
    --terminal=term_a) echo '{"id":"r","ok":true,"result":{"wait":{"satisfied":true}}}' ;;
    *) echo '{"id":"r","ok":false,"error":{"code":"timeout","message":"timeout"}}'; exit 1 ;;
  esac ;;
esac"#;
        with_fake_orca("poll", body, |mut orca, log| {
            orca.adopt("feat-a", "term_a");
            orca.adopt("feat-b", "term_b");
            orca.adopt("feat-gone", "term_zzz");
            let statuses = orca.poll_statuses().unwrap();
            assert_eq!(statuses["feat-a"], PaneStatus::Idle);
            assert_eq!(statuses["feat-b"], PaneStatus::Busy);
            assert_eq!(statuses["feat-gone"], PaneStatus::Dead);

            let lines = log_lines(log);
            assert_eq!(lines.iter().filter(|l| l.starts_with("terminal list")).count(), 1, "{lines:#?}");
            assert_eq!(lines.iter().filter(|l| l.starts_with("terminal wait")).count(), 2, "a dead terminal isn't probed: {lines:#?}");

            orca.apply_statuses(statuses);
            assert_eq!(orca.pane_status("feat-b"), Some(PaneStatus::Busy));
        });
    }

    #[test]
    fn polling_with_nothing_tracked_never_runs_orca() {
        with_fake_orca("idle-poll", "exit 1", |orca, log| {
            assert!(orca.poll_statuses().unwrap().is_empty());
            assert!(log_lines(log).is_empty());
        });
    }

    /// A close that errors is fine if the terminal is gone afterwards — the state `stop`
    /// wants — and is an error if it is still listed.
    #[test]
    fn stop_treats_an_already_gone_terminal_as_stopped_but_not_a_live_one() {
        let gone = r#"
case "$2" in
  close) echo '{"id":"r","ok":false,"error":{"code":"terminal_handle_stale","message":"stale"}}'; exit 1 ;;
  list) echo '{"id":"r","ok":true,"result":{"terminals":[],"totalCount":0,"truncated":false}}' ;;
esac"#;
        with_fake_orca("stop-gone", gone, |mut orca, _| {
            orca.adopt("feat-a", "term_a");
            orca.set_anchor("term_a");
            orca.stop("feat-a").unwrap();
            assert!(!orca.has_pane("feat-a"));
        });
        let live = r#"
case "$2" in
  close) echo '{"id":"r","ok":false,"error":{"code":"terminal_stop_live","message":"The PTY is live."}}'; exit 1 ;;
  list) echo '{"id":"r","ok":true,"result":{"terminals":[{"handle":"term_a"}],"totalCount":1,"truncated":false}}' ;;
esac"#;
        with_fake_orca("stop-live", live, |mut orca, _| {
            orca.adopt("feat-a", "term_a");
            let err = orca.stop("feat-a").unwrap_err().to_string();
            assert!(err.contains("terminal_stop_live"), "{err}");
        });
    }
}
