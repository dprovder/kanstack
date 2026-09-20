//! Optional bridge to the `cmux` CLI (<https://cmux.com>, `manaflow-ai/cmux`).
//!
//! Unlike [`crate::but::But`], this is not required: kanstack works standalone, and most
//! runs of it are not inside a cmux pane at all. `Cmux::discover` returns `None` rather
//! than an error when the binary is missing, so its absence is silent, not a startup
//! failure.
//!
//! No `--workspace` is ever passed: `new-split` defaults to `$CMUX_WORKSPACE_ID`, which
//! cmux injects into every pane it spawns. Since a cmux workspace here is the whole
//! repository's GitButler workspace, not a lane, that default is exactly right — a new
//! lane becomes a sibling split next to kanstack itself, not a new cmux workspace.
//!
//! A plain terminal split is used rather than cmux's `agent-session` surface type: the
//! latter is a bespoke chat-style panel, not a PTY running the harness binary, which is
//! not what a lane wants here. `cmux send` types the harness command into a normal
//! terminal instead, same as a person would.
//!
//! Before the first lane's split, `cmux rpc pane.list` is checked for a pane already
//! occupying kanstack's configured direction (e.g. something the user had open above it
//! before kanstack ever ran). If one is found, the lane splits off *that* pane sideways
//! instead of restacking a third row onto kanstack — `[[unrelated][claude]][kanstack]`
//! rather than `[unrelated]/[claude]/[kanstack]` stacked three deep. This isn't exposed
//! by any documented CLI verb (`list-panes`/`tree` are text-only, no geometry); `pane.list`
//! is a raw RPC method from `cmux capabilities` that happens to report pixel frames.
//!
//! Note this is distinct from the `cmux-tui` Rust crate that also lives in the cmux
//! monorepo; that one is an unrelated, unbuilt experiment and is not what `cmux` resolves
//! to on `PATH`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub use crate::pane_status::PaneStatus;
use crate::mux::{command_exists, Multiplexer, OpenRequest};
use crate::pane_status::CPU_BUSY_THRESHOLD_PERCENT;

/// Pixels of slack allowed when treating two pane edges as touching. Frames come back as
/// floats (e.g. `461.5`); this absorbs rounding without false-matching panes that are
/// merely near each other.
const ADJACENCY_EPSILON: f64 = 4.0;

/// cmux, through its own CLI. Which pane belongs to which branch is
/// `crate::splitter::Splitter`'s business; the one thing this remembers itself is the cmux
/// workspace new panes open in, because nothing else has such a concept.
pub struct Cmux {
    bin: PathBuf,
    /// The cmux workspace new panes belong in, e.g. `workspace:1`: pinned by the caller
    /// (see [`Multiplexer::set_scope`]) or, failing that, read off the last pane this opened.
    /// `None` until one of those has happened, meaning "wherever `$CMUX_WORKSPACE_ID`
    /// says" — which is right for the board, running inside its own pane, but is only as
    /// good as that variable is for anything else.
    workspace: Mutex<Option<String>>,
}

impl Cmux {
    /// Locates the `cmux` binary. Returns `None` (not an error) when `cmux` is not on
    /// `PATH` and `KANSTACK_CMUX_BIN` is unset — kanstack runs fine outside cmux.
    pub fn discover() -> Option<Self> {
        let bin = match std::env::var_os("KANSTACK_CMUX_BIN") {
            Some(path) => PathBuf::from(path),
            None => {
                let candidate = PathBuf::from("cmux");
                if !command_exists(&candidate) {
                    return None;
                }
                candidate
            }
        };
        Some(Cmux { bin, workspace: Mutex::new(None) })
    }

    fn pinned_workspace(&self) -> Option<String> {
        self.workspace.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The pinned workspace, with a terminal surface in it to split from — `None` if
    /// nothing is pinned, or the pinned workspace no longer exists (closed since), in which
    /// case the caller falls back to the environment and pins wherever that lands.
    fn pinned_surface(&self) -> Option<String> {
        let workspace = self.pinned_workspace()?;
        let out = self.run(&["list-panels", "--workspace", &workspace]).ok()?;
        terminal_surface(&out)
    }

    /// The UUID for a workspace `workspace_ref` (`workspace:1`), which is what's worth
    /// remembering: a short ref is only an ordinal within the running app and can name a
    /// different workspace after cmux restarts. Falls back to the ref itself if the
    /// listing can't be read.
    fn workspace_id(&self, workspace_ref: &str) -> String {
        self.run(&["--id-format", "both", "workspace", "list"])
            .ok()
            .and_then(|listing| workspace_uuid(&listing, workspace_ref))
            .unwrap_or_else(|| workspace_ref.to_string())
    }

    /// Types `text` into `surface_ref`, then presses Enter as a separate key. A trailing
    /// newline in the same `cmux send` reaches the terminal as a carriage return in the
    /// same burst as the text, which a TUI like Claude Code reads as part of a paste rather
    /// than as Enter — the text lands in its input box and never submits. Verified against
    /// a real `claude` with a ~4KB message: one send with a trailing newline stays in the
    /// input box; the same text followed by its own `send-key enter` is submitted at once.
    /// `tmux.rs` sends its Enter as a separate key for the same reason.
    fn type_and_submit(&self, surface_ref: &str, text: &str) -> Result<()> {
        self.run_scoped(&["send", "--surface", surface_ref, text])?;
        self.run_scoped(&["send-key", "--surface", surface_ref, "enter"])?;
        Ok(())
    }

    /// Looks for a pane already touching kanstack's own pane on `direction`'s side, via
    /// `cmux rpc pane.list`'s pixel geometry. Returns that pane's selected surface, so a
    /// caller can split off it instead of kanstack. `Ok(None)` means the side is clear (or
    /// kanstack's own pane couldn't be identified, e.g. running outside cmux entirely).
    fn occupant_in_direction(&self, direction: &str) -> Result<Option<String>> {
        let Ok(own_surface_id) = std::env::var("CMUX_SURFACE_ID") else {
            return Ok(None);
        };
        let out = self.run(&["rpc", "pane.list"])?;
        let response: PaneListResponse =
            serde_json::from_str(&out).with_context(|| format!("bad pane.list response: {out:?}"))?;

        let Some(own) = response
            .panes
            .iter()
            .find(|p| p.surface_ids.iter().any(|id| id == &own_surface_id))
        else {
            return Ok(None);
        };

        Ok(response
            .panes
            .iter()
            .find(|p| p.pane_ref != own.pane_ref && touches(&own.pixel_frame, &p.pixel_frame, direction))
            .map(|p| p.selected_surface_ref.clone()))
    }

    /// [`Self::run`] with `--workspace <pinned>` added, when a workspace is pinned. cmux
    /// looks a `--surface` up in `$CMUX_WORKSPACE_ID`'s workspace unless told otherwise, so
    /// a shell whose environment names some other workspace gets "surface not found" for a
    /// pane that's right there — or, for `new-split`, opens the pane in the wrong workspace.
    /// The flag goes straight after the subcommand, ahead of any positional argument.
    fn run_scoped(&self, args: &[&str]) -> Result<String> {
        match self.pinned_workspace() {
            Some(ws) if !args.is_empty() => {
                let mut scoped = vec![args[0], "--workspace", ws.as_str()];
                scoped.extend(&args[1..]);
                self.run(&scoped)
            }
            _ => self.run(args),
        }
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out: Output = Command::new(&self.bin)
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn `{} {}`", self.bin.display(), args.join(" ")))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim();
            bail!(
                "`cmux {}` failed{}",
                args.join(" "),
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Multiplexer for Cmux {
    fn name(&self) -> &'static str {
        "cmux"
    }

    fn running_inside(&self) -> bool {
        std::env::var_os("CMUX_SURFACE_ID").is_some()
    }

    /// Splits off the previous lane's pane (or, for the first lane, whatever is right for
    /// where kanstack sits — see below), types `launch` into the fresh terminal, then labels
    /// the tab with the branch name.
    ///
    /// `launch` is the whole harness command line, initial message and branch-context note
    /// included, rather than those being sent as a second `cmux send` afterwards — the
    /// harness needs a moment to start before it can receive typed input, and there's no
    /// "wait until ready" primitive to lean on here. Folding it into the launch line
    /// sidesteps the race instead of racing it.
    ///
    /// The first lane splits a surface in the pinned workspace if one is pinned; failing
    /// that, a pane already sitting in kanstack's own split slot, so a lane joins it rather
    /// than stacking a third row onto kanstack; failing that, kanstack's own pane.
    ///
    /// `req.cwd` is unused: the launch line `cd`s there itself.
    fn open_pane(&self, req: &OpenRequest<'_>) -> Result<String> {
        // A pinned workspace that no longer exists (closed since) is dropped: this spawn
        // falls back to the environment, and pins wherever that lands.
        let pinned = self.pinned_surface();
        if self.pinned_workspace().is_some() && pinned.is_none() {
            self.set_scope(None);
        }
        let (direction, anchor) = match req.after {
            Some(anchor) => (req.chain_direction, Some(anchor.to_string())),
            None => match pinned {
                // A workspace was pinned: split a surface inside it, whatever the
                // environment says about where we're running.
                Some(surface) => (req.first_direction, Some(surface)),
                None => match self.occupant_in_direction(req.first_direction) {
                    // Something already sits in kanstack's own split slot (e.g. a tab the
                    // user had open before kanstack started): join it beside that pane
                    // instead of stacking a third row onto kanstack.
                    Ok(Some(occupant)) => (req.chain_direction, Some(occupant)),
                    Ok(None) => (req.first_direction, None),
                    // No geometry, no $CMUX_SURFACE_ID, or a malformed response: fall back
                    // to the plain behavior rather than failing the whole spawn over it.
                    Err(_) => (req.first_direction, None),
                },
            },
        };

        let workspace = self.pinned_workspace();
        let mut split_args = vec!["new-split", direction];
        if let Some(anchor) = &anchor {
            split_args.extend(["--surface", anchor.as_str()]);
        }
        if let Some(workspace) = &workspace {
            split_args.extend(["--workspace", workspace.as_str()]);
        }
        let split_out = self.run(&split_args)?;
        let surface_ref = extract_ref(&split_out, "surface:")
            .with_context(|| format!("`cmux new-split` did not report a surface: {split_out:?}"))?
            .to_string();
        // Remembered, so every later pane lands in the workspace this one did.
        if let Some(workspace) = extract_ref(&split_out, "workspace:") {
            self.set_scope(Some(&self.workspace_id(workspace)));
        }

        self.type_and_submit(&surface_ref, req.launch)?;
        self.run_scoped(&["rename-tab", "--surface", &surface_ref, req.title])?;
        Ok(surface_ref)
    }

    fn type_line(&self, pane: &str, text: &str) -> Result<()> {
        self.type_and_submit(pane, text)
    }

    /// cmux's "panel" refs are the same `surface:N` refs `new-split` reports.
    fn focus(&self, pane: &str) -> Result<()> {
        self.run_scoped(&["focus-panel", "--panel", pane])?;
        Ok(())
    }

    fn close(&self, pane: &str) -> Result<()> {
        // Whether an already-closed surface is an error here isn't documented; a surface
        // that no longer appears in `pane.list` is the state closing is after either way.
        if self.run_scoped(&["close-surface", "--surface", pane]).is_err() {
            let still_there = self.run(&["rpc", "pane.list"]).map(|out| out.contains(pane)).unwrap_or(true);
            if still_there {
                bail!("`cmux close-surface` failed for {pane}");
            }
        }
        Ok(())
    }

    /// `cmux rpc pane.list` for whether each surface still exists at all, then (only when a
    /// `CMUX_WORKSPACE_ID` is available to scope the query to, and only when at least one
    /// tracked surface is still present) `cmux top --workspace <id> --json` for whether
    /// anything beyond the resting login shell is running in it.
    fn probe(&self, panes: &[&str]) -> Result<HashMap<String, PaneStatus>> {
        if panes.is_empty() {
            return Ok(HashMap::new());
        }
        let workspace = self.pinned_workspace();
        // Without a workspace, `pane.list` covers whichever one is selected right now, which
        // needn't be where these panes are — they'd all read as dead.
        let list_out = match &workspace {
            Some(ws) => self.run(&["rpc", "pane.list", &format!("{{\"workspace_id\":\"{ws}\"}}")])?,
            None => self.run(&["rpc", "pane.list"])?,
        };

        let any_alive = {
            let list: PaneListResponse = serde_json::from_str(&list_out)
                .with_context(|| format!("bad pane.list response: {list_out:?}"))?;
            let present: std::collections::HashSet<String> =
                list.panes.into_iter().flat_map(|p| p.surface_refs).collect();
            panes.iter().any(|p| present.contains(*p))
        };

        let top_out = if any_alive {
            match workspace.ok_or(()).or_else(|()| std::env::var("CMUX_WORKSPACE_ID").map_err(|_| ())) {
                // Else: no workspace to scope `top` to without guessing —
                // existence-only status is still derived below; the Busy/Idle split
                // is skipped this round.
                Ok(workspace_id) => Some(self.run(&["top", "--workspace", &workspace_id, "--json"])?),
                Err(_) => None,
            }
        } else {
            None
        };

        classify_statuses(panes, &list_out, top_out.as_deref())
    }

    fn set_scope(&self, scope: Option<&str>) {
        *self.workspace.lock().unwrap_or_else(|e| e.into_inner()) = scope.map(str::to_string);
    }

    /// The workspace new panes are opened in: what was pinned, or what the last pane this
    /// opened turned out to be in.
    fn scope(&self) -> Option<String> {
        self.pinned_workspace()
    }
}

#[derive(Deserialize)]
struct PaneListResponse {
    panes: Vec<PaneInfo>,
}

#[derive(Deserialize)]
struct PaneInfo {
    #[serde(rename = "ref")]
    pane_ref: String,
    pixel_frame: PixelFrame,
    surface_ids: Vec<String>,
    selected_surface_ref: String,
    /// Surface refs (e.g. `"surface:34"`) contained in this pane. Not documented
    /// alongside the rest of `pane.list`'s raw-RPC shape; degrades to empty rather than a
    /// parse failure if it's ever absent or renamed.
    #[serde(default)]
    surface_refs: Vec<String>,
}

/// Shape of `cmux top --workspace <id> --json`. A first-class, `--help`-documented
/// subcommand (unlike `pane.list`) that reports per-surface resource usage — used here
/// only for `resources.cpu_percent`, as a busy/idle signal. `process_count` was tried
/// first and rejected: a harness like `claude` keeps several child processes (MCP
/// servers, watchers) alive even at a resting prompt, so process count alone never drops
/// back down and every pane reads permanently `Busy`. CPU usage actually falls to near
/// zero at rest (confirmed live: ~35% while generating vs. 0–0.9% idle), so it's the
/// signal that can actually distinguish the two.
#[derive(Deserialize)]
struct TopResponse {
    #[serde(default)]
    windows: Vec<TopWindow>,
}

#[derive(Deserialize)]
struct TopWindow {
    #[serde(default)]
    workspaces: Vec<TopWorkspace>,
}

#[derive(Deserialize)]
struct TopWorkspace {
    #[serde(default)]
    panes: Vec<TopPane>,
}

#[derive(Deserialize)]
struct TopPane {
    #[serde(default)]
    surfaces: Vec<TopSurface>,
}

#[derive(Deserialize)]
struct TopSurface {
    #[serde(rename = "ref")]
    surface_ref: String,
    resources: TopResources,
}

#[derive(Deserialize)]
struct TopResources {
    cpu_percent: f64,
}

#[derive(Deserialize)]
struct PixelFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Pure classification step of [`Cmux::probe`], split out so it can be unit tested against
/// captured JSON fixtures without shelling out to a real `cmux`. `list_json` is a `cmux rpc
/// pane.list` response; `top_json`, if present, is a `cmux top --json` response. A surface
/// present in the listing with no CPU data this round is left out — existence alone can't
/// distinguish `Busy` from `Idle`, so the caller keeps what it knew rather than being
/// handed a guess.
fn classify_statuses(
    panes: &[&str],
    list_json: &str,
    top_json: Option<&str>,
) -> Result<HashMap<String, PaneStatus>> {
    let list: PaneListResponse = serde_json::from_str(list_json)
        .with_context(|| format!("bad pane.list response: {list_json:?}"))?;
    let present: std::collections::HashSet<String> =
        list.panes.into_iter().flat_map(|p| p.surface_refs).collect();

    let mut cpu_percents: HashMap<String, f64> = HashMap::new();
    if let Some(top_json) = top_json {
        let top: TopResponse = serde_json::from_str(top_json)
            .with_context(|| "bad `cmux top` response".to_string())?;
        for surface in top
            .windows
            .into_iter()
            .flat_map(|w| w.workspaces)
            .flat_map(|w| w.panes)
            .flat_map(|p| p.surfaces)
        {
            cpu_percents.insert(surface.surface_ref, surface.resources.cpu_percent);
        }
    }

    Ok(panes
        .iter()
        .filter_map(|&pane| {
            let status = if !present.contains(pane) {
                PaneStatus::Dead
            } else {
                match cpu_percents.get(pane) {
                    Some(cpu) if *cpu > CPU_BUSY_THRESHOLD_PERCENT => PaneStatus::Busy,
                    Some(_) => PaneStatus::Idle,
                    None => return None,
                }
            };
            Some((pane.to_string(), status))
        })
        .collect())
}

/// True when `other` sits flush against `own`'s `direction` edge, with any overlap along
/// the perpendicular axis (not necessarily full-width/full-height — an L-shaped neighbor
/// still counts as occupying that side).
fn touches(own: &PixelFrame, other: &PixelFrame, direction: &str) -> bool {
    let close = |a: f64, b: f64| (a - b).abs() <= ADJACENCY_EPSILON;
    let overlaps_horizontally = other.x < own.x + own.width && other.x + other.width > own.x;
    let overlaps_vertically = other.y < own.y + own.height && other.y + other.height > own.y;
    match direction {
        "up" => close(other.y + other.height, own.y) && overlaps_horizontally,
        "down" => close(own.y + own.height, other.y) && overlaps_horizontally,
        "left" => close(other.x + other.width, own.x) && overlaps_vertically,
        "right" => close(own.x + own.width, other.x) && overlaps_vertically,
        _ => false,
    }
}

/// The UUID `cmux --id-format both workspace list` prints beside `workspace_ref`. Lines
/// look like `* workspace:1 6AAD1488-…  ◑ title  [selected]`.
fn workspace_uuid(listing: &str, workspace_ref: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut tokens = line.trim_start().trim_start_matches('*').split_whitespace();
        (tokens.next() == Some(workspace_ref)).then(|| tokens.next().map(str::to_string)).flatten()
    })
}

/// The terminal surface to split from in a `cmux list-panels` listing: the focused one if
/// it's a terminal, else the first terminal. Lines look like `* surface:158  terminal
/// [focused]  "title"`, the star marking the focused surface.
fn terminal_surface(list_panels: &str) -> Option<String> {
    let terminals = list_panels.lines().filter_map(|line| {
        let focused = line.trim_start().starts_with('*');
        let mut tokens = line.trim_start().trim_start_matches('*').split_whitespace();
        let surface = tokens.next().filter(|t| t.starts_with("surface:"))?;
        (tokens.next() == Some("terminal")).then(|| (focused, surface.to_string()))
    });
    let terminals: Vec<_> = terminals.collect();
    terminals
        .iter()
        .find(|(focused, _)| *focused)
        .or_else(|| terminals.first())
        .map(|(_, surface)| surface.clone())
}

/// Pulls a `<prefix><id>` token (e.g. `surface:31`) out of an `OK ...` response line.
fn extract_ref<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.split_whitespace().find(|tok| tok.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Runs `body` with `PATH` and `KANSTACK_CMUX_BIN` swapped out and restored
    /// afterwards, so this doesn't leak into other tests running in the same process. Both
    /// are process-wide state also read by `tmux.rs`'s and `splitter.rs`'s own env-mutating
    /// tests — held for the whole call via `SPLIT_BACKEND_ENV_LOCK`, not just the swap, so
    /// none of them can interleave with each other either.
    fn with_env(path: Option<&str>, cmux_bin: Option<&str>, body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_path = std::env::var_os("PATH");
        let old_bin = std::env::var_os("KANSTACK_CMUX_BIN");
        match path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        match cmux_bin {
            Some(b) => std::env::set_var("KANSTACK_CMUX_BIN", b),
            None => std::env::remove_var("KANSTACK_CMUX_BIN"),
        }
        body();
        match old_path {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        match old_bin {
            Some(b) => std::env::set_var("KANSTACK_CMUX_BIN", b),
            None => std::env::remove_var("KANSTACK_CMUX_BIN"),
        }
    }

    /// The whole point of `Cmux::discover` returning `Option` rather than `Result`: a
    /// machine with no `cmux` on `PATH` (the common case — kanstack works standalone) must
    /// come back `None`, not an error, so nothing upstream has to handle a spawn failure
    /// for a binary that was never expected to be there.
    #[test]
    fn discover_is_none_without_cmux_on_path_or_an_override() {
        let dir = std::env::temp_dir().join(format!("kanstack-cmux-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(Some(dir.to_str().unwrap()), None, || {
            assert!(
                Cmux::discover().is_none(),
                "an empty PATH with no KANSTACK_CMUX_BIN override must not find a cmux"
            );
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_exists_is_false_for_a_path_without_the_binary() {
        let dir = std::env::temp_dir().join(format!("kanstack-cmux-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(Some(dir.to_str().unwrap()), None, || {
            assert!(!command_exists(Path::new("cmux")));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `KANSTACK_CMUX_BIN` is trusted at face value — set it and `discover` returns
    /// `Some`, even pointing at a path that does not exist. That's intentional (see its
    /// doc comment): the failure surfaces later, from `spawn_pane` actually trying to
    /// run it, as a normal error the caller already handles — not silently, and not by
    /// `discover` re-implementing existence-checking for an explicit override.
    #[test]
    fn an_explicit_override_is_trusted_without_checking_it_exists() {
        with_env(Some(""), Some("/nonexistent/not-cmux"), || {
            let cmux = Cmux::discover();
            assert!(cmux.is_some(), "an explicit KANSTACK_CMUX_BIN is never second-guessed");
        });
    }

    const PANE_LIST_FIXTURE: &str = include_str!("../tests/fixtures/cmux_pane_list.json");
    const TOP_FIXTURE: &str = include_str!("../tests/fixtures/cmux_top.json");

    /// A tracked surface that's present in `pane.list` and shows CPU usage above
    /// [`CPU_BUSY_THRESHOLD_PERCENT`] in `cmux top` (a real, actively-generating `claude`
    /// session, captured live at ~35%) classifies as `Busy`.
    #[test]
    fn classify_statuses_marks_a_high_cpu_surface_busy() {
        let statuses = classify_statuses(&["surface:33"], PANE_LIST_FIXTURE, Some(TOP_FIXTURE)).unwrap();
        assert_eq!(statuses["surface:33"], PaneStatus::Busy);
    }

    /// A tracked surface present in `pane.list` with CPU usage at the busy threshold or
    /// below (a resting `claude` prompt, captured live at 0%) classifies as `Idle` even
    /// though its process count stays high — this is the case `process_count` alone got
    /// wrong.
    #[test]
    fn classify_statuses_marks_a_low_cpu_surface_idle() {
        let statuses = classify_statuses(&["surface:34"], PANE_LIST_FIXTURE, Some(TOP_FIXTURE)).unwrap();
        assert_eq!(statuses["surface:34"], PaneStatus::Idle);
    }

    /// A tracked surface absent from `pane.list` entirely — its tab was closed —
    /// classifies as `Dead`, regardless of what `cmux top` says.
    #[test]
    fn classify_statuses_marks_a_missing_surface_dead() {
        let statuses = classify_statuses(&["surface:999"], PANE_LIST_FIXTURE, Some(TOP_FIXTURE)).unwrap();
        assert_eq!(statuses["surface:999"], PaneStatus::Dead);
    }

    /// Without a `cmux top` response (e.g. no `CMUX_WORKSPACE_ID` to scope it), a surface
    /// still present in `pane.list` is left out: existence alone can't distinguish Busy from
    /// Idle, so the caller keeps its previously known status rather than being handed a
    /// guess.
    #[test]
    fn classify_statuses_leaves_out_a_surface_without_a_top_response() {
        let statuses = classify_statuses(&["surface:33"], PANE_LIST_FIXTURE, None).unwrap();
        assert!(!statuses.contains_key("surface:33"), "{statuses:?}");
    }

    #[test]
    fn terminal_surface_prefers_the_focused_terminal() {
        let listing = "  surface:2  terminal  \"~/repo\"\n\
                       * surface:158  terminal  [focused]  \"feat\"\n\
                         surface:3  terminal  \"cargo run\"\n";
        assert_eq!(terminal_surface(listing).as_deref(), Some("surface:158"));
    }

    #[test]
    fn terminal_surface_skips_browsers_and_falls_back_to_the_first_terminal() {
        let listing = "* surface:74  browser  [focused]  \"docs\"\n  surface:9  terminal  \"sh\"\n  surface:10  terminal  \"sh\"\n";
        assert_eq!(terminal_surface(listing).as_deref(), Some("surface:9"));
        assert_eq!(terminal_surface("  surface:74  browser  \"docs\"\n"), None);
        assert_eq!(terminal_surface(""), None);
    }

    #[test]
    fn workspace_uuid_finds_the_id_beside_a_ref() {
        let listing = "* workspace:1 6AAD1488-99C9-4520-B89A-3E06E25ADB61  \u{25d1} Kanstack CLI  [selected]\n\
                         workspace:10 5C659CD2-D2CC-42BD-85B7-6B0D7166C924  wdl\n";
        assert_eq!(workspace_uuid(listing, "workspace:1").as_deref(), Some("6AAD1488-99C9-4520-B89A-3E06E25ADB61"));
        assert_eq!(workspace_uuid(listing, "workspace:10").as_deref(), Some("5C659CD2-D2CC-42BD-85B7-6B0D7166C924"));
        assert_eq!(workspace_uuid(listing, "workspace:2"), None, "workspace:1 must not match workspace:10 or vice versa");
    }
}
