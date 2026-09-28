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
//! # Lanes as tabs of their own
//!
//! Orca nests two levels: tab groups (side-by-side columns in the window) hold tabs, and each
//! tab holds a split tree of terminals. From outside, only the outer level can be rearranged
//! — `session.tabs.move` moves a whole tab between groups (the same code as dragging it),
//! while the one RPC that rewrites a tab's split tree is ignored whenever the desktop window
//! is up — so each lane opens as a tab of its own (`session.tabs.createTerminal`), in a new
//! group split off the previous lane's. Lanes still sit side by side, as columns rather than
//! splits of kanstack's tab, and any lane can later be moved as a unit (`Orca::regroup`).
//! A stacked spawn gets a real tab in its sibling's group (`KANSTACK_STACK_PANES=tabbed`) or
//! a new group split off it (`split`). No CLI verb reaches these RPCs, so kanstack calls them
//! over the runtime's Unix socket the way the CLI itself does (`Orca::rpc`).
//!
//! Written from source (`stablyai/orca` at 080c4ad), then run against `orcad` built from that
//! commit. Confirmed there: `orca-runtime.json` names a Unix socket and token, and `Orca::rpc`
//! gets answers over it; `terminal list`'s `tabId`/`leafId` and `session.tabs.list`'s groups
//! and `<tab>::<leaf>` surface ids line up as `Orca::spot` expects; and `orcad` refuses
//! `session.tabs.createTerminal` with `runtime_unavailable`, because only a window (the desktop
//! app, or its Electron `orca serve`) publishes the tab graph that call needs — so under
//! `orcad` every lane takes the `terminal split` fallback, which also ran there. The tab path
//! itself — `createTerminal` and `session.tabs.move` answered by a desktop window — has not
//! been run live. With no runtime to talk to at all (no `orca-runtime.json`), lanes split
//! without trying it.
//!
//! # Splitting (the fallback)
//!
//! Orca's `terminal split` takes `--direction horizontal|vertical`, and nothing else about
//! placement. In Orca's own UI "Split Right" is `vertical` and "Split Down" is `horizontal`,
//! and the new pane is always the second child — right of, or below, the one it splits (from
//! source). So kanstack's `right` and `down` map exactly, while `left` and `up` cannot be
//! honored: they are accepted (so `KANSTACK_SPAWN_DIRECTION=left` doesn't error) and place
//! the pane where `right`/`down` would. That is why the defaults here are `down` then `right`
//! rather than the `up`/`right` of the other two backends — kept for the tab-group layout too
//! (whose group splits could honor all four), so the fallback and it lay lanes out alike.
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

use crate::mux::{command_exists, Multiplexer, OpenRequest, RegroupRequest, StackPlacement};
use crate::mux::pane_status::PaneStatus;

/// How long each `terminal wait --for tui-idle` probe may block before its timeout is read
/// as "busy". Probes for every tracked terminal run in parallel, so this is roughly what one
/// poll costs in total.
const IDLE_PROBE_TIMEOUT_MS: u32 = 1500;

/// How long [`Orca::rpc`] waits for the runtime to answer before giving up on it. Generous,
/// because a `session.tabs.createTerminal` answered by the desktop window may itself wait up
/// to 10s for the window and 11s more for the terminal to come up (from source).
const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How many times, and how far apart, [`Orca::handle_in_tab`] looks for a new tab's terminal.
const HANDLE_POLLS: u32 = 10;
const HANDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

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

    /// Calls one of the runtime's RPC methods directly, for the few (`session.tabs.*`) no CLI
    /// verb exposes, the way the `orca` CLI itself reaches the runtime (from source,
    /// `src/cli/runtime/transport.ts`): read `orca-runtime.json` from Orca's user-data
    /// directory (see [`runtime_metadata_path`]) for the Unix socket and auth token, write one
    /// JSON line `{id, authToken, method, params}`, and read newline-delimited JSON back,
    /// skipping `{"_keepalive":true}` frames and anything not answering this `id`. The answer
    /// is the same envelope `--json` prints, so [`parse_response`] reads it.
    fn rpc<T: DeserializeOwned>(&self, method: &str, params: serde_json::Value) -> Result<T> {
        use std::io::{BufRead, BufReader, Write};

        let (endpoint, token) = runtime_endpoint()?;
        let described = || format!("orca runtime `{method}`");
        let mut stream = std::os::unix::net::UnixStream::connect(&endpoint)
            .with_context(|| format!("{} failed: can't connect to {endpoint}", described()))?;
        stream.set_read_timeout(Some(RPC_TIMEOUT))?;
        let id = format!("kanstack-{}-{method}", std::process::id());
        let request = serde_json::json!({ "id": id, "authToken": token, "method": method, "params": params });
        writeln!(stream, "{request}").with_context(|| format!("{} failed to send", described()))?;

        for line in BufReader::new(stream).lines() {
            let line = line.with_context(|| format!("{} failed while waiting for its answer", described()))?;
            let answers_this = serde_json::from_str::<serde_json::Value>(&line).is_ok_and(|frame| frame["id"] == id.as_str());
            if !answers_this {
                continue;
            }
            return match parse_response::<T>(&line)? {
                Response::Ok(result) => Ok(result),
                Response::Failed(e) => bail!("{} failed: {}: {}", described(), e.code, e.message),
            };
        }
        bail!("{} failed: the runtime closed the connection without answering", described())
    }

    /// Where `handle` sits in Orca's tab layout, read off a `terminal list` (`list`) and then
    /// `session.tabs.list`: everything `session.tabs.createTerminal` and `session.tabs.move`
    /// need to place a tab beside it.
    fn spot(&self, list: &ListResult, handle: &str) -> Result<TabSpot> {
        let terminal =
            list.terminals.iter().find(|t| t.handle == handle).with_context(|| format!("orca lists no terminal {handle}"))?;
        let (Some(tab), Some(worktree)) = (&terminal.tab_id, &terminal.worktree_id) else {
            bail!("`orca terminal list` didn't say which tab {handle} is in");
        };
        let worktree = format!("id:{worktree}");
        let tabs: SessionTabs = self.rpc("session.tabs.list", serde_json::json!({ "worktree": worktree }))?;
        let (group, index) = tabs
            .tab_groups
            .iter()
            .find_map(|g| g.tab_order.iter().position(|id| is_tab(id, tab)).map(|i| (g.id.clone(), i)))
            .with_context(|| format!("no Orca tab group holds {handle}'s tab"))?;
        let surface = tabs
            .tabs
            .iter()
            .find(|t| t.parent_tab_id.as_ref() == Some(tab) && t.leaf_id.is_some() && t.leaf_id == terminal.leaf_id)
            .map(|t| t.id.clone());
        Ok(TabSpot { worktree, surface, group, index })
    }

    /// Opens `req.launch` as a terminal in a tab of its own, in the tab group holding `anchor`
    /// and right after `anchor`'s tab — then, given a `split` direction, moves that tab out
    /// into a new group split off the anchor's in that direction.
    ///
    /// [`TabOpen::NotOpened`] means nothing was created, so the caller can fall back to a
    /// plain `terminal split`. Once the tab exists there is no falling back — a split would
    /// start a second harness on the same lane — so a failure after that is an `Err`, while a
    /// failed move just leaves the tab beside the anchor's rather than failing a harness that
    /// is already running.
    ///
    /// `session.tabs.createTerminal` is what Orca's own "new tab" goes through from a remote
    /// client: `command` is delivered to the new shell as its startup command, like `terminal
    /// create --command`, and `activate: false` keeps focus where it was (the board, usually)
    /// — Orca's runtime starts the terminal's process itself if a hidden tab hasn't (from
    /// source). No `cwd`: the launch line `cd`s there itself.
    fn open_as_tab(&self, req: &OpenRequest<'_>, anchor: &str, split: Option<&str>) -> Result<TabOpen> {
        let created = (|| {
            // No runtime to talk to: fall back before running anything else.
            runtime_endpoint()?;
            let list: ListResult = self.run_json(&list_args())?;
            let at = self.spot(&list, anchor)?;
            let mut params = serde_json::json!({
                "worktree": at.worktree, "targetGroupId": at.group, "command": req.launch, "activate": false,
            });
            if let Some(surface) = &at.surface {
                params["afterTabId"] = surface.as_str().into();
            }
            let created: CreatedTerminal = self.rpc("session.tabs.createTerminal", params)?;
            anyhow::Ok((at, created))
        })();
        let (at, created) = match created {
            Ok(created) => created,
            Err(_) => return Ok(TabOpen::NotOpened),
        };
        let handle = match created.tab.terminal {
            Some(handle) => handle,
            None => self.handle_in_tab(&created.tab.parent_tab_id)?,
        };
        if let Some(direction) = split {
            let _ = self.rpc::<serde_json::Value>(
                "session.tabs.move",
                serde_json::json!({
                    "worktree": at.worktree, "tabId": created.tab.parent_tab_id, "targetGroupId": at.group,
                    "kind": "split", "splitDirection": direction,
                }),
            );
        }
        let _ = self.run_json::<serde_json::Value>(&rename_args(&handle, req.title));
        Ok(TabOpen::Opened(handle))
    }

    /// The handle of the terminal in tab `tab`, for a `createTerminal` that answered before
    /// its terminal had one (`status: "pending-handle"`): looked for in `terminal list` a few
    /// times over a couple of seconds.
    fn handle_in_tab(&self, tab: &str) -> Result<String> {
        for attempt in 0..HANDLE_POLLS {
            if attempt > 0 {
                std::thread::sleep(HANDLE_POLL_INTERVAL);
            }
            let list: ListResult = self.run_json(&list_args())?;
            if let Some(t) = list.terminals.iter().find(|t| t.tab_id.as_deref() == Some(tab)) {
                return Ok(t.handle.clone());
            }
        }
        bail!("orca created tab {tab} but never listed a terminal in it")
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

    /// Opens the lane as a tab of its own, in a new tab group split off the previous lane's
    /// (or kanstack's own, for the first lane) in `req`'s direction — see [`Self::open_as_tab`]
    /// — so that, being alone in its tab, it can later be moved as a whole (see
    /// [`Self::regroup`]), while lanes still sit side by side as columns. Orca's group splits
    /// take all four directions, unlike `terminal split`.
    ///
    /// Without a reachable runtime (or if that fails before creating anything), it falls back
    /// to what kanstack did before: split off the previous lane's terminal with `req.launch` —
    /// the whole harness command line, see `crate::harness::HarnessConfig::launch_line` — as
    /// the new terminal's `--command`, which puts it in that terminal's tab. Either way the
    /// terminal is then titled (e.g. the branch name) — best-effort, since a failed rename
    /// shouldn't undo a harness that did launch.
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
        if let TabOpen::Opened(handle) = self.open_as_tab(req, anchor, Some(direction))? {
            return Ok(handle);
        }
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

    /// A real tab for a stacked spawn (`KANSTACK_STACK_PANES=tabbed`): the new terminal opens in
    /// a tab of its own right after `req.after`'s, in the same tab group — see
    /// [`Self::open_as_tab`]. Without a reachable runtime, falls back to [`Self::open_pane`]'s
    /// split, as every backend without tabs does.
    fn open_tab(&self, req: &OpenRequest<'_>) -> Result<String> {
        let anchor = req.after.expect("open_tab is only ever called with an anchor");
        match self.open_as_tab(req, anchor, None)? {
            TabOpen::Opened(handle) => Ok(handle),
            TabOpen::NotOpened => self.open_pane(req),
        }
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

    /// Moves `req.pane`'s whole Orca *tab* into the tab group holding `req.anchor` — right
    /// after the anchor's tab (`tabbed`), or into a new group split off that one in
    /// `req.split_direction` (`split`) — through the runtime's `session.tabs.move` RPC (see
    /// [`Self::rpc`]; no CLI verb exposes it). The desktop app hands that RPC to the very
    /// `dropUnifiedTab` its tab drag-and-drop uses, so this is the same move a person dragging
    /// the tab would make: the terminal, its process and its handle all carry on (from
    /// source, `stablyai/orca` at 080c4ad: `session-tab-mutation-methods.ts`,
    /// `session-tab-ipc-bridge.ts`, `tabs-drop-actions.ts`).
    ///
    /// Only whole tabs can be moved from outside (see the module doc). Lanes opened as tabs of
    /// their own ([`Self::open_pane`]'s usual path) always can be; the exceptions are lanes
    /// from the `terminal split` fallback, or opened before kanstack did that:
    ///
    /// - `req.pane` already in the anchor's tab: already grouped as closely as Orca allows,
    ///   and nothing is moved.
    /// - `req.pane` sharing its tab with any other terminal: not moved, because moving the tab
    ///   would drag those terminals — kanstack's own, possibly — along with it. Likewise when
    ///   `terminal list` is truncated and can't prove the tab holds only this terminal.
    /// - Different worktrees: not moved; Orca's drop refuses moves across worktrees anyway.
    ///
    /// Each of those is `Ok(())`: an out-of-place pane is cosmetic. Anything that goes wrong
    /// talking to Orca is an error, reported by the caller.
    ///
    /// **Partly run live**: the lookups ran against `orcad`, but `session.tabs.move` answered
    /// by a desktop window has not been (see the module doc).
    fn regroup(&self, req: &RegroupRequest<'_>) -> Result<()> {
        let list: ListResult = self.run_json(&list_args())?;
        let find = |handle: &str| {
            list.terminals.iter().find(|t| t.handle == handle).with_context(|| format!("orca lists no terminal {handle}"))
        };
        let (pane, anchor) = (find(req.pane)?, find(req.anchor)?);
        let (Some(tab), Some(anchor_tab), Some(worktree)) = (&pane.tab_id, &anchor.tab_id, &anchor.worktree_id) else {
            bail!("`orca terminal list` didn't say which tab {} and {} are in", req.pane, req.anchor);
        };
        let shares_its_tab = list.terminals.iter().any(|t| t.handle != pane.handle && t.tab_id.as_ref() == Some(tab));
        if tab == anchor_tab || shares_its_tab || list.truncated || pane.worktree_id.as_ref() != Some(worktree) {
            return Ok(());
        }

        let at = self.spot(&list, req.anchor)?;
        let mut params = serde_json::json!({ "worktree": at.worktree, "tabId": tab, "targetGroupId": at.group });
        match req.placement {
            StackPlacement::Tabbed => {
                params["kind"] = "move-to-group".into();
                params["index"] = (at.index + 1).into();
            }
            StackPlacement::Split => {
                params["kind"] = "split".into();
                params["splitDirection"] = req.split_direction.into();
            }
        }
        self.rpc::<serde_json::Value>("session.tabs.move", params)?;
        Ok(())
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
    /// The Orca tab this terminal is a split of — the unit `session.tabs.move` moves (see
    /// `regroup`). Several terminals split off one another share it.
    #[serde(default, rename = "tabId")]
    tab_id: Option<String>,
    #[serde(default, rename = "leafId")]
    leaf_id: Option<String>,
    #[serde(default, rename = "worktreeId")]
    worktree_id: Option<String>,
}

/// `session.tabs.list` → the worktree's tabs and the groups (side-by-side columns) holding
/// them; only the groups matter here.
#[derive(Deserialize)]
struct SessionTabs {
    #[serde(default, rename = "tabGroups")]
    tab_groups: Vec<TabGroup>,
    #[serde(default)]
    tabs: Vec<SessionTab>,
}

/// One entry of `session.tabs.list`'s `tabs`. A terminal's `id` is its own surface
/// (`<tab>::<leaf>`), which is what `createTerminal`'s `afterTabId` wants (from source).
#[derive(Deserialize)]
struct SessionTab {
    id: String,
    #[serde(default, rename = "parentTabId")]
    parent_tab_id: Option<String>,
    #[serde(default, rename = "leafId")]
    leaf_id: Option<String>,
}

/// `session.tabs.createTerminal` → `{"tab": {...}, …}`: the new terminal's surface, whose
/// `terminal` handle is `null` while it is still `pending-handle`.
#[derive(Deserialize)]
struct CreatedTerminal {
    tab: CreatedTab,
}

#[derive(Deserialize)]
struct CreatedTab {
    #[serde(rename = "parentTabId")]
    parent_tab_id: String,
    #[serde(default)]
    terminal: Option<String>,
}

/// What [`Orca::open_as_tab`] managed.
enum TabOpen {
    /// A tab of its own, with this terminal handle.
    Opened(String),
    /// Nothing was created, so the caller falls back to a split.
    NotOpened,
}

/// Where a terminal sits in Orca's tab layout — see [`Orca::spot`].
struct TabSpot {
    /// Its worktree, as the `id:` selector the `session.tabs.*` methods take.
    worktree: String,
    /// Its own surface id, if `session.tabs.list` listed it.
    surface: Option<String>,
    /// The tab group holding its tab, and the tab's position in it.
    group: String,
    index: usize,
}

#[derive(Deserialize)]
struct TabGroup {
    id: String,
    /// Top-level tab ids, left to right — a terminal tab's is the `tabId` `terminal list`
    /// reports (from source).
    #[serde(default, rename = "tabOrder")]
    tab_order: Vec<String>,
}

/// Whether `id`, from a group's `tabOrder`, is `tab`: the tab's own id, or — defensively, in
/// case a runtime ever lists surfaces there — one of its terminals' `<tab>::<leaf>` ids.
fn is_tab(id: &str, tab: &str) -> bool {
    id == tab || id.strip_prefix(tab).is_some_and(|rest| rest.starts_with("::"))
}

/// `orca-runtime.json`, which the running Orca writes into its user-data directory: where its
/// RPC socket is and the token every request must carry (from source,
/// `src/shared/runtime-bootstrap.ts`). Older runtimes wrote one `transport` instead of the
/// `transports` list.
#[derive(Deserialize)]
struct RuntimeMetadata {
    #[serde(default)]
    transports: Vec<RuntimeTransport>,
    #[serde(default)]
    transport: Option<RuntimeTransport>,
    #[serde(default, rename = "authToken")]
    auth_token: Option<String>,
}

#[derive(Deserialize)]
struct RuntimeTransport {
    kind: String,
    endpoint: String,
}

/// The running Orca runtime's Unix socket and auth token, from `orca-runtime.json` (see
/// [`runtime_metadata_path`]). `Err` — saying where it looked — when there is no running Orca
/// to talk to.
fn runtime_endpoint() -> Result<(String, String)> {
    let path = runtime_metadata_path().context("can't tell where Orca keeps its runtime metadata")?;
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("can't read Orca's runtime metadata at {} — is Orca running?", path.display()))?;
    let metadata: RuntimeMetadata =
        serde_json::from_str(&raw).with_context(|| format!("unexpected Orca runtime metadata at {}", path.display()))?;
    let socket = metadata
        .transports
        .into_iter()
        .chain(metadata.transport)
        .find(|t| t.kind == "unix")
        .with_context(|| format!("Orca's runtime metadata at {} names no Unix socket", path.display()))?;
    let token = metadata.auth_token.with_context(|| format!("Orca's runtime metadata at {} has no auth token", path.display()))?;
    Ok((socket.endpoint, token))
}

/// Where the Orca CLI looks for `orca-runtime.json`, and so where kanstack does too (from
/// source, `src/cli/runtime/metadata.ts`): `$ORCA_USER_DATA_PATH` if set — Orca's own way of
/// pointing its CLI at a particular instance — else Electron's default user-data directory:
/// `~/Library/Application Support/orca` on macOS, `$XDG_CONFIG_HOME/orca` (or
/// `~/.config/orca`) elsewhere.
fn runtime_metadata_path() -> Option<PathBuf> {
    let dir = match std::env::var_os("ORCA_USER_DATA_PATH") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            if cfg!(target_os = "macos") {
                home.join("Library/Application Support/orca")
            } else {
                std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("orca")
            }
        }
    };
    Some(dir.join("orca-runtime.json"))
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
        with_fake_orca_and(tag, body, &[], test)
    }

    /// [`with_fake_orca`] with `extra` variables swapped in as well.
    fn with_fake_orca_and(tag: &str, body: &str, extra: &[(&str, Option<&str>)], test: impl FnOnce(Splitter, &Path)) {
        let (bin, log) = fake_orca(tag, body);
        let mut vars = vec![
            ("KANSTACK_ORCA_BIN", Some(bin.to_str().unwrap())),
            ("ORCA_TERMINAL_HANDLE", Some("term_own")),
            ("KANSTACK_ORCA_DIRECTION", None),
            ("KANSTACK_ORCA_CHAIN_DIRECTION", None),
            ("KANSTACK_HARNESS", Some("claude")),
            // Nowhere, unless a test brings its own `FakeRuntime`: a developer's real Orca must
            // never be reached, and without a runtime every spawn takes the `terminal split` path.
            ("ORCA_USER_DATA_PATH", Some("/nonexistent/orca-user-data")),
        ];
        vars.extend_from_slice(extra);
        with_env(
            &vars,
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

    /// A stand-in for the Orca runtime's RPC socket: writes `orca-runtime.json` into a fresh
    /// user-data directory (for `ORCA_USER_DATA_PATH`) and answers each connection's one request
    /// with `answer(method)` (`Err` answers with that error code), preceded by a keepalive and a frame for some other request — both
    /// of which `Orca::rpc` must skip. Every request it receives is kept, in order.
    struct FakeRuntime {
        dir: PathBuf,
        requests: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    }

    impl FakeRuntime {
        /// Short paths: a Unix socket's must fit in about 100 bytes.
        fn start(tag: &str, answer: fn(&str) -> Result<serde_json::Value, &'static str>) -> FakeRuntime {
            use std::io::{BufRead, BufReader, Write};
            let dir = std::env::temp_dir().join(format!("ks-orca-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let socket = dir.join("s");
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let metadata = serde_json::json!({
                "runtimeId": "r", "pid": 1, "startedAt": 0, "authToken": "tok",
                "transports": [{"kind": "unix", "endpoint": socket}],
            });
            std::fs::write(dir.join("orca-runtime.json"), metadata.to_string()).unwrap();
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut line = String::new();
                    BufReader::new(&stream).read_line(&mut line).unwrap();
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let reply = match answer(request["method"].as_str().unwrap()) {
                        Ok(result) => serde_json::json!({"id": request["id"], "ok": true, "result": result}),
                        Err(code) => serde_json::json!({"id": request["id"], "ok": false, "error": {"code": code, "message": code}}),
                    };
                    seen.lock().unwrap().push(request);
                    let _ = writeln!(stream, "{{\"_keepalive\":true}}");
                    let _ = writeln!(stream, "{{\"id\":\"someone-else\",\"ok\":false,\"error\":{{\"code\":\"nope\"}}}}");
                    let _ = writeln!(stream, "{reply}");
                }
            });
            FakeRuntime { dir, requests }
        }

        fn user_data(&self) -> &str {
            self.dir.to_str().unwrap()
        }

        fn requests(&self) -> Vec<serde_json::Value> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for FakeRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Kanstack's own terminal and one sibling lane split off it share tab `tab-own`; `feat-base`
    /// is alone in `tab-base`, and `feat-top` alone in `tab-top`. The groups: `tab-own` and
    /// `tab-base` in `g1`, `tab-top` in `g2`.
    const TABS_LISTING: &str = r#"
case "$2" in
  list) cat <<'JSON'
{"id":"r","ok":true,"result":{"terminals":[
 {"handle":"term_own","tabId":"tab-own","leafId":"l0","worktreeId":"W::/repo"},
 {"handle":"term_lane","tabId":"tab-own","leafId":"l1","worktreeId":"W::/repo"},
 {"handle":"term_base","tabId":"tab-base","leafId":"l2","worktreeId":"W::/repo"},
 {"handle":"term_top","tabId":"tab-top","leafId":"l3","worktreeId":"W::/repo"},
 {"handle":"term_new","tabId":"tab-new","leafId":"l9","worktreeId":"W::/repo"}],"totalCount":5,"truncated":false}}
JSON
  ;;
  *) echo '{"id":"r","ok":true,"result":{}}' ;;
esac"#;

    fn tab_groups(method: &str) -> Result<serde_json::Value, &'static str> {
        Ok(match method {
            "session.tabs.list" => serde_json::json!({
                "worktree": "id:W::/repo", "publicationEpoch": "e", "snapshotVersion": 3,
                "tabs": [
                    {"type": "terminal", "id": "tab-own::l0", "parentTabId": "tab-own", "leafId": "l0", "title": "", "isActive": true},
                    {"type": "terminal", "id": "tab-own::l1", "parentTabId": "tab-own", "leafId": "l1", "title": "", "isActive": false},
                    {"type": "terminal", "id": "tab-base::l2", "parentTabId": "tab-base", "leafId": "l2", "title": "", "isActive": false},
                    {"type": "terminal", "id": "tab-top::l3", "parentTabId": "tab-top", "leafId": "l3", "title": "", "isActive": false},
                ],
                "tabGroups": [
                    {"id": "g1", "activeTabId": "tab-own", "tabOrder": ["tab-own", "tab-base"]},
                    {"id": "g2", "activeTabId": "tab-top", "tabOrder": ["tab-top"]},
                ],
            }),
            "session.tabs.createTerminal" => serde_json::json!({
                "publicationEpoch": "e", "snapshotVersion": 4,
                "tab": {"type": "terminal", "id": "tab-new::l9", "parentTabId": "tab-new", "leafId": "l9", "title": "Terminal",
                        "isActive": false, "status": "ready", "terminal": "term_new"},
            }),
            _ => serde_json::json!({"moved": true}),
        })
    }

    /// Joins `feat-top` (in `term_top`) onto `feat-base` (in `term_base`), after a first look
    /// that only records.
    fn restack_top_onto_base(orca: &mut Splitter) -> Result<()> {
        orca.adopt("feat-top", "term_top");
        orca.adopt("feat-base", "term_base");
        orca.restack_moved_branches(&crate::splitter::status_with_stacks(&[&["feat-base"], &["feat-top"]])).unwrap();
        orca.restack_moved_branches(&crate::splitter::status_with_stacks(&[&["feat-top", "feat-base"]]))
    }

    /// `tabbed` (the default): `feat-top`'s tab moves into the group holding `feat-base`'s,
    /// right after it — the drop a person dragging the tab there would make — over the
    /// runtime socket, with the auth token, naming the worktree by id.
    #[test]
    fn a_regroup_moves_the_terminals_tab_into_the_anchors_group_right_after_it() {
        let runtime = FakeRuntime::start("tab", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data())), ("KANSTACK_STACK_PANES", None)];
        with_fake_orca_and("regroup-tab", TABS_LISTING, &vars, |mut orca, log| {
            restack_top_onto_base(&mut orca).unwrap();
            assert_eq!(log_lines(log), ["terminal list --json"]);
            let requests = runtime.requests();
            assert_eq!(requests.len(), 2, "{requests:#?}");
            assert_eq!(requests[0]["method"], "session.tabs.list");
            assert_eq!(requests[0]["authToken"], "tok");
            assert_eq!(requests[0]["params"], serde_json::json!({"worktree": "id:W::/repo"}));
            assert_eq!(requests[1]["method"], "session.tabs.move");
            assert_eq!(
                requests[1]["params"],
                serde_json::json!({"worktree": "id:W::/repo", "tabId": "tab-top", "targetGroupId": "g1", "kind": "move-to-group", "index": 2})
            );
            assert_eq!(orca.pane_id("feat-top").as_deref(), Some("term_top"), "a moved terminal keeps its handle");
        });
    }

    /// `split`: a new group split off the anchor's, in the orthogonal direction (`down`, off
    /// Orca's default `right` chain).
    #[test]
    fn a_split_regroup_splits_the_terminals_tab_off_the_anchors_group() {
        let runtime = FakeRuntime::start("split", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data())), ("KANSTACK_STACK_PANES", Some("split"))];
        with_fake_orca_and("regroup-split", TABS_LISTING, &vars, |mut orca, _| {
            restack_top_onto_base(&mut orca).unwrap();
            assert_eq!(
                runtime.requests()[1]["params"],
                serde_json::json!({"worktree": "id:W::/repo", "tabId": "tab-top", "targetGroupId": "g1", "kind": "split", "splitDirection": "down"})
            );
        });
    }

    /// Kanstack's own lanes split its terminal, so they share its tab: already as close to a
    /// sibling as Orca can put them, and nothing is asked of the runtime.
    #[test]
    fn a_terminal_already_in_the_anchors_tab_is_left_where_it_is() {
        let runtime = FakeRuntime::start("same", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data())), ("KANSTACK_STACK_PANES", None)];
        with_fake_orca_and("regroup-same", TABS_LISTING, &vars, |mut orca, _| {
            orca.adopt("feat-top", "term_lane");
            orca.adopt("feat-base", "term_own");
            orca.restack_moved_branches(&crate::splitter::status_with_stacks(&[&["feat-base"], &["feat-top"]])).unwrap();
            orca.restack_moved_branches(&crate::splitter::status_with_stacks(&[&["feat-top", "feat-base"]])).unwrap();
            assert!(runtime.requests().is_empty(), "{:#?}", runtime.requests());
        });
    }

    /// Moving a tab moves every terminal in it, so one that shares its tab — here with
    /// kanstack's own terminal — is never moved, rather than dragging kanstack along.
    #[test]
    fn a_terminal_sharing_its_tab_with_another_is_never_moved() {
        let runtime = FakeRuntime::start("shared", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data())), ("KANSTACK_STACK_PANES", None)];
        with_fake_orca_and("regroup-shared", TABS_LISTING, &vars, |mut orca, _| {
            orca.adopt("feat-top", "term_lane");
            orca.adopt("feat-base", "term_base");
            orca.restack_moved_branches(&crate::splitter::status_with_stacks(&[&["feat-base"], &["feat-top"]])).unwrap();
            orca.restack_moved_branches(&crate::splitter::status_with_stacks(&[&["feat-top", "feat-base"]])).unwrap();
            assert!(runtime.requests().is_empty(), "{:#?}", runtime.requests());
        });
    }

    /// No runtime metadata where Orca keeps it: the move fails with where it looked, rather
    /// than silently doing nothing.
    #[test]
    fn a_regroup_with_no_runtime_to_talk_to_says_where_it_looked() {
        let missing = std::env::temp_dir().join(format!("ks-orca-none-{}", std::process::id()));
        let vars = [("ORCA_USER_DATA_PATH", Some(missing.to_str().unwrap())), ("KANSTACK_STACK_PANES", None)];
        with_fake_orca_and("regroup-none", TABS_LISTING, &vars, |mut orca, _| {
            let err = restack_top_onto_base(&mut orca).unwrap_err().to_string();
            assert!(err.contains("feat-top") && err.contains("orca-runtime.json") && err.contains("is Orca running"), "{err}");
        });
    }

    /// With a runtime to talk to, a lane opens as a tab of its own — created in the group of
    /// the terminal it would have split (kanstack's own, for the first lane), right after it,
    /// without taking focus — and then moves out into a new group split off that one, in the
    /// lane direction (`down` first, Orca's default). No `terminal split` at all.
    #[test]
    fn a_lane_opens_as_a_tab_of_its_own_in_a_new_group_split_off_the_last() {
        let runtime = FakeRuntime::start("lane", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data()))];
        with_fake_orca_and("lane-tab", TABS_LISTING, &vars, |mut orca, log| {
            let handle = spawn(&mut orca, Path::new("/repo"), "feat-a", Some("go")).unwrap();
            assert_eq!(handle, "term_new");
            assert_eq!(orca.pane_id("feat-a").as_deref(), Some("term_new"));

            let requests = runtime.requests();
            let methods: Vec<_> = requests.iter().map(|r| r["method"].as_str().unwrap()).collect();
            assert_eq!(methods, ["session.tabs.list", "session.tabs.createTerminal", "session.tabs.move"]);
            let create = &requests[1]["params"];
            assert_eq!(create["worktree"], "id:W::/repo");
            assert_eq!(create["targetGroupId"], "g1");
            assert_eq!(create["afterTabId"], "tab-own::l0", "right after kanstack's own tab");
            assert_eq!(create["activate"], false, "a spawn never takes focus");
            let command = create["command"].as_str().unwrap();
            assert!(command.starts_with("cd '/repo' && ") && command.contains("claude") && command.contains("go"), "{command}");
            assert_eq!(
                requests[2]["params"],
                serde_json::json!({"worktree": "id:W::/repo", "tabId": "tab-new", "targetGroupId": "g1", "kind": "split", "splitDirection": "down"})
            );

            let lines = log_lines(log);
            assert_eq!(lines, ["terminal list --json", "terminal rename --terminal=term_new --title=feat-a --json"]);
        });
    }

    /// `KANSTACK_STACK_PANES=tabbed` (the default): a real tab right after the sibling's, in
    /// the sibling's group, and left there.
    #[test]
    fn a_stacked_spawn_opens_a_real_tab_right_after_the_siblings() {
        let runtime = FakeRuntime::start("stack-tab", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data())), ("KANSTACK_STACK_PANES", None)];
        with_fake_orca_and("stack-tab", TABS_LISTING, &vars, |mut orca, _| {
            let handle =
                orca.spawn_stacked_harness_with(Path::new("/repo"), "feat-top", None, None, None, None, "term_base").unwrap();
            assert_eq!(handle, "term_new");
            let requests = runtime.requests();
            let methods: Vec<_> = requests.iter().map(|r| r["method"].as_str().unwrap()).collect();
            assert_eq!(methods, ["session.tabs.list", "session.tabs.createTerminal"], "no move: it stays a tab");
            assert_eq!(requests[1]["params"]["targetGroupId"], "g1");
            assert_eq!(requests[1]["params"]["afterTabId"], "tab-base::l2");
        });
    }

    /// `KANSTACK_STACK_PANES=split`: a new group split off the sibling's, in the direction at
    /// right angles to the lane chain (`down`, off Orca's default `right`).
    #[test]
    fn a_stacked_spawn_can_split_a_new_group_off_the_siblings_instead() {
        let runtime = FakeRuntime::start("stack-split", tab_groups);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data())), ("KANSTACK_STACK_PANES", Some("split"))];
        with_fake_orca_and("stack-split", TABS_LISTING, &vars, |mut orca, _| {
            orca.spawn_stacked_harness_with(Path::new("/repo"), "feat-top", None, None, None, None, "term_base").unwrap();
            let requests = runtime.requests();
            assert_eq!(requests.len(), 3, "{requests:#?}");
            assert_eq!(requests[2]["params"]["kind"], "split");
            assert_eq!(requests[2]["params"]["splitDirection"], "down");
            assert_eq!(requests[2]["params"]["targetGroupId"], "g1");
        });
    }

    fn pending_handle(method: &str) -> Result<serde_json::Value, &'static str> {
        match method {
            "session.tabs.createTerminal" => Ok(serde_json::json!({
                "publicationEpoch": "e", "snapshotVersion": 4,
                "tab": {"type": "terminal", "id": "tab-new::l9", "parentTabId": "tab-new", "leafId": "l9", "title": "",
                        "isActive": false, "status": "pending-handle", "terminal": null},
            })),
            other => tab_groups(other),
        }
    }

    /// A tab that answered before its terminal had a handle is found in `terminal list` by
    /// its tab id.
    #[test]
    fn a_tab_created_before_its_handle_is_found_by_its_tab_in_the_listing() {
        let runtime = FakeRuntime::start("pending", pending_handle);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data()))];
        with_fake_orca_and("pending", TABS_LISTING, &vars, |mut orca, log| {
            assert_eq!(spawn(&mut orca, Path::new("/repo"), "feat-a", None).unwrap(), "term_new");
            assert_eq!(log_lines(log).iter().filter(|l| *l == "terminal list --json").count(), 2);
        });
    }

    fn creates_an_unlisted_tab(method: &str) -> Result<serde_json::Value, &'static str> {
        match method {
            "session.tabs.createTerminal" => Ok(serde_json::json!({
                "publicationEpoch": "e", "snapshotVersion": 4,
                "tab": {"type": "terminal", "id": "tab-ghost::l9", "parentTabId": "tab-ghost", "leafId": "l9", "title": "",
                        "isActive": false, "status": "pending-handle", "terminal": null},
            })),
            other => tab_groups(other),
        }
    }

    /// Once a tab exists its harness is running, so a later failure is an error — never a
    /// fall back to `terminal split`, which would start a second harness on the same lane.
    #[test]
    fn a_tab_that_was_created_but_never_found_is_an_error_not_a_second_launch() {
        let runtime = FakeRuntime::start("ghost", creates_an_unlisted_tab);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data()))];
        with_fake_orca_and("ghost", TABS_LISTING, &vars, |mut orca, log| {
            let err = spawn(&mut orca, Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("tab-ghost"), "{err}");
            assert!(!orca.has_pane("feat-a"));
            assert!(!log_lines(log).iter().any(|l| l.starts_with("terminal split") || l.starts_with("terminal create")), "{:#?}", log_lines(log));
        });
    }

    fn refuses_to_create(method: &str) -> Result<serde_json::Value, &'static str> {
        match method {
            "session.tabs.createTerminal" => Err("runtime_unavailable"),
            other => tab_groups(other),
        }
    }

    /// A runtime that refuses to create the tab costs the tab layout, not the lane: it falls
    /// back to splitting, exactly as before tabs.
    #[test]
    fn a_refused_tab_falls_back_to_splitting_the_terminal() {
        let runtime = FakeRuntime::start("refuse", refuses_to_create);
        let vars = [("ORCA_USER_DATA_PATH", Some(runtime.user_data()))];
        let body = TABS_LISTING.replace(
            "  *) echo",
            "  split) echo '{\"id\":\"r\",\"ok\":true,\"result\":{\"split\":{\"handle\":\"term_split\",\"tabId\":\"t\"}}}' ;;\n  *) echo",
        );
        with_fake_orca_and("refuse", &body, &vars, |mut orca, log| {
            assert_eq!(spawn(&mut orca, Path::new("/repo"), "feat-a", None).unwrap(), "term_split");
            assert!(log_lines(log).iter().any(|l| l.starts_with("terminal split --terminal=term_own")), "{:#?}", log_lines(log));
        });
    }

    /// With no runtime at all, nothing but the old `terminal split` path runs — not even the
    /// listing the tab path starts with.
    #[test]
    fn with_no_runtime_a_lane_splits_as_before_without_asking_anything_else() {
        with_fake_orca("no-runtime", SPLITS_AND_ACKS, |mut orca, log| {
            spawn(&mut orca, Path::new("/repo"), "feat-a", None).unwrap();
            assert!(log_lines(log)[0].starts_with("terminal split --terminal=term_own"), "{:#?}", log_lines(log));
        });
    }
}
