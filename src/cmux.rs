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

/// One line for the setup wizard: whether cmux is usable here and, if not, what to do.
pub fn detection() -> String {
    match Cmux::discover() {
        Some(_) => "✓ cmux found on PATH".to_string(),
        None => "✗ cmux not found — set KANSTACK_CMUX_BIN to point at it".to_string(),
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

    /// Adds `req.launch` as a new surface — cmux's own word for what shows as a tab — in
    /// the pane already holding `req.after` (always `Some` for this call), rather than
    /// `new-split`'s new pane. Measured live against a running cmux: `new-surface --pane
    /// <pane-ref>` answers `OK surface:N pane:M workspace:K` (parsed the same `extract_ref`
    /// way as `new-split`'s own `OK surface:N …`), leaves whichever surface was already
    /// selected in that pane alone rather than stealing focus, and every other operation —
    /// `send`/`send-key` (via `type_and_submit`), `rename-tab`, `focus-panel`,
    /// `close-surface` — already addresses a surface directly and is unaffected by how many
    /// share its pane, so none of them need to change for this.
    ///
    /// `--pane` wants the pane (the split region), not the surface — a bare surface ref
    /// isn't a valid `--pane` target — so the anchor's containing pane is looked up in
    /// `pane.list` first, the same source [`Self::occupant_in_direction`] already reads for
    /// `open_pane`'s own placement.
    fn open_tab(&self, req: &OpenRequest<'_>) -> Result<String> {
        let anchor = req.after.expect("open_tab is only ever called with an anchor");
        let list_out = self.run(&["rpc", "pane.list"])?;
        let pane = pane_containing_surface(&list_out, anchor)
            .with_context(|| format!("no cmux pane found holding {anchor}"))?;

        let cwd_str = req.cwd.to_string_lossy().into_owned();
        let out = self.run_scoped(&["new-surface", "--pane", &pane, "--working-directory", cwd_str.as_str()])?;
        let surface_ref = extract_ref(&out, "surface:")
            .with_context(|| format!("`cmux new-surface` did not report a surface: {out:?}"))?
            .to_string();

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

/// The cmux pane (split region) ref containing `surface_ref`, from a `pane.list`
/// response — what `new-surface --pane` needs, since a surface's own ref isn't a valid
/// `--pane` target. `None` if the response is malformed or no pane lists that surface —
/// the caller reports that as the anchor no longer existing, rather than guessing a pane.
fn pane_containing_surface(list_json: &str, surface_ref: &str) -> Option<String> {
    let list: PaneListResponse = serde_json::from_str(list_json).ok()?;
    list.panes.into_iter().find(|p| p.surface_refs.iter().any(|s| s == surface_ref)).map(|p| p.pane_ref)
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

    #[test]
    fn pane_containing_surface_finds_the_pane_that_lists_it() {
        assert_eq!(pane_containing_surface(PANE_LIST_FIXTURE, "surface:35").as_deref(), Some("pane:7"));
        assert_eq!(pane_containing_surface(PANE_LIST_FIXTURE, "surface:33").as_deref(), Some("pane:13"));
        assert_eq!(pane_containing_surface(PANE_LIST_FIXTURE, "surface:999"), None, "no pane lists a surface that isn't there");
        assert_eq!(pane_containing_surface("not json", "surface:33"), None);
    }

    // What follows drives the real spawn/send/focus/stop/poll paths, through a `Splitter` as
    // kanstack does, against a stand-in `cmux` that logs its arguments. Its replies are what
    // a real cmux printed for the same commands; the poll's are the captured fixtures.

    use std::sync::Arc;

    use crate::harness::HarnessConfig;
    use crate::mux::stand_in;
    use crate::splitter::Splitter;

    const WS: &str = "6AAD1488-99C9-4520-B89A-3E06E25ADB61";

    fn with_fake_cmux(tag: &str, body: &str, extra: &[(&str, Option<&str>)], test: impl FnOnce(Splitter, &Path)) {
        let (bin, log) = stand_in::install(tag, "cmux", body);
        let mut vars = vec![
            ("KANSTACK_CMUX_BIN", Some(bin.to_str().unwrap())),
            // Unset, so kanstack looks like it is not in a cmux pane and geometry is not asked for.
            ("CMUX_SURFACE_ID", None),
            ("CMUX_WORKSPACE_ID", None),
            ("KANSTACK_CMUX_DIRECTION", None),
            ("KANSTACK_CMUX_CHAIN_DIRECTION", None),
        ];
        vars.extend_from_slice(extra);
        stand_in::with_env(&vars, || {
            let cmux = Cmux::discover().unwrap();
            test(Splitter::new(Arc::new(cmux), HarnessConfig::new("claude")), &log)
        });
        stand_in::remove(&bin);
    }

    /// A splitting cmux: each `new-split` reports a fresh surface in `workspace:2`, and the
    /// workspace lists under a UUID.
    fn splitting(list_panels: &str) -> String {
        format!(
            r#"n=$(cat "$0.n" 2>/dev/null || echo 40)
case "$1" in
  new-split) n=$((n+1)); echo $n > "$0.n"; echo "OK surface:$n pane:7 workspace:2" ;;
  --id-format) echo "* workspace:2 {WS}  title  [selected]" ;;
  list-panels) {list_panels} ;;
esac"#
        )
    }

    const LISTS_A_TERMINAL: &str = r#"printf '* surface:41  terminal  [focused]  "x"\n'"#;

    #[test]
    fn the_first_lane_splits_up_pins_the_workspace_by_uuid_and_scopes_every_later_call() {
        with_fake_cmux("first", &splitting(LISTS_A_TERMINAL), &[], |mut splitter, log| {
            let pane = splitter.spawn_harness(Path::new("/repo"), "feat-a", Some("go")).unwrap();
            assert_eq!(pane, "surface:41");
            let lines = stand_in::log_lines(log);
            assert_eq!(lines[0], "new-split up", "no workspace pinned yet, so none is named: {lines:#?}");
            assert_eq!(lines[1], "--id-format both workspace list", "a ref is only an ordinal; the UUID is what is kept");
            assert!(lines[2].starts_with(&format!("send --workspace {WS} --surface surface:41 cd '/repo' && claude ")), "{}", lines[2]);
            assert_eq!(lines[3], format!("send-key --workspace {WS} --surface surface:41 enter"));
            assert_eq!(lines[4], format!("rename-tab --workspace {WS} --surface surface:41 feat-a"));
            assert_eq!(lines.len(), 5, "{lines:#?}");
            assert_eq!(splitter.workspace().as_deref(), Some(WS));
        });
    }

    #[test]
    fn a_later_lane_chains_off_the_previous_surface_inside_the_pinned_workspace() {
        with_fake_cmux("chain", &splitting(LISTS_A_TERMINAL), &[], |mut splitter, log| {
            splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap();
            splitter.spawn_harness(Path::new("/repo"), "feat-b", None).unwrap();
            let lines = stand_in::log_lines(log);
            assert_eq!(lines[5], format!("list-panels --workspace {WS}"), "the pinned workspace is checked to still exist");
            assert_eq!(lines[6], format!("new-split right --surface surface:41 --workspace {WS}"), "{lines:#?}");
        });
    }

    /// A stacked spawn (`KANSTACK_STACK_PANES` unset, so the default `tabbed`) adds a new
    /// surface to the anchor's own pane rather than splitting a new one — measured live
    /// against a real cmux, see `open_tab`'s doc comment.
    #[test]
    fn a_stacked_spawn_adds_a_tab_to_the_anchors_pane_and_is_typed_into_and_titled() {
        let body = r#"case "$1" in
  rpc) printf '{"panes":[{"ref":"pane:7","pixel_frame":{"height":1,"width":1,"x":0,"y":0},"surface_ids":[],"selected_surface_ref":"surface:9","surface_refs":["surface:9"]}]}' ;;
  new-surface) echo "OK surface:99 pane:7 workspace:2" ;;
esac"#;
        let vars = [("KANSTACK_STACK_PANES", None)];
        with_fake_cmux("tabs", body, &vars, |mut splitter, log| {
            let pane = splitter.spawn_stacked_harness_with(Path::new("/repo"), "feat-top", Some("go"), None, "surface:9").unwrap();
            assert_eq!(pane, "surface:99");
            let lines = stand_in::log_lines(log);
            assert_eq!(lines[0], "rpc pane.list");
            assert_eq!(lines[1], "new-surface --pane pane:7 --working-directory /repo");
            assert!(lines[2].starts_with("send --surface surface:99 cd '/repo' && claude "), "{}", lines[2]);
            assert_eq!(lines[3], "send-key --surface surface:99 enter");
            assert_eq!(lines[4], "rename-tab --surface surface:99 feat-top");
            assert_eq!(lines.len(), 5, "{lines:#?}");
        });
    }

    #[test]
    fn a_tab_whose_anchor_has_no_pane_is_an_error() {
        let body = r#"case "$1" in rpc) echo '{"panes":[]}' ;; esac"#;
        let vars = [("KANSTACK_STACK_PANES", None)];
        with_fake_cmux("no-pane", body, &vars, |mut splitter, log| {
            let err =
                splitter.spawn_stacked_harness_with(Path::new("/repo"), "feat-top", None, None, "surface:9").unwrap_err().to_string();
            assert!(err.contains("no cmux pane found holding surface:9"), "{err}");
            assert!(!splitter.has_pane("feat-top"));
            assert_eq!(stand_in::log_lines(log).len(), 1, "nothing may be typed once the pane can't be found");
        });
    }

    /// The first lane of a pinned workspace splits a surface inside it, wherever kanstack
    /// itself happens to be running.
    #[test]
    fn a_pinned_workspace_makes_the_first_lane_split_a_surface_inside_it() {
        with_fake_cmux("pinned", &splitting(LISTS_A_TERMINAL), &[], |mut splitter, log| {
            splitter.set_workspace(Some(WS));
            splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap();
            let lines = stand_in::log_lines(log);
            assert_eq!(lines[0], format!("list-panels --workspace {WS}"));
            assert_eq!(lines[1], format!("new-split up --surface surface:41 --workspace {WS}"), "{lines:#?}");
        });
    }

    /// A workspace closed since it was pinned is dropped: the lane falls back to the
    /// environment and pins wherever that lands.
    #[test]
    fn a_pinned_workspace_that_no_longer_exists_is_dropped_and_replaced() {
        let body = splitting(r#"echo "not found" >&2; exit 1"#);
        with_fake_cmux("dropped", &body, &[], |mut splitter, log| {
            splitter.set_workspace(Some("workspace:99"));
            splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap();
            let lines = stand_in::log_lines(log);
            assert_eq!(lines[0], "list-panels --workspace workspace:99");
            assert_eq!(lines[1], "new-split up", "the dead workspace must not be passed on: {lines:#?}");
            assert_eq!(splitter.workspace().as_deref(), Some(WS));
        });
    }

    #[test]
    fn a_failed_split_reports_cmuxs_message_and_tracks_nothing() {
        let body = r#"case "$1" in new-split) echo "no such workspace" >&2; exit 1 ;; esac"#;
        with_fake_cmux("split-fails", body, &[], |mut splitter, log| {
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("no such workspace"), "{err}");
            assert!(!splitter.has_pane("feat-a"));
            assert_eq!(stand_in::log_lines(log), ["new-split up"], "nothing may be typed into a surface that never opened");
        });
        with_fake_cmux("no-surface", r#"case "$1" in new-split) echo "OK" ;; esac"#, &[], |mut splitter, _| {
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("did not report a surface"), "{err}");
        });
    }

    /// The submit is its own key press: a newline in the same burst reads as part of a
    /// paste to a TUI, and the text never sends.
    #[test]
    fn a_message_is_sent_and_then_submitted_with_its_own_enter() {
        with_fake_cmux("send", "", &[], |mut splitter, log| {
            splitter.set_workspace(Some(WS));
            splitter.adopt("feat-a", "surface:5");
            splitter.send_task("feat-a", "run the tests").unwrap();
            assert_eq!(
                stand_in::log_lines(log),
                [format!("send --workspace {WS} --surface surface:5 run the tests"), format!("send-key --workspace {WS} --surface surface:5 enter")]
            );
        });
    }

    #[test]
    fn focus_names_the_panel_inside_the_pinned_workspace() {
        with_fake_cmux("focus", "", &[], |mut splitter, log| {
            splitter.set_workspace(Some(WS));
            splitter.adopt("feat-a", "surface:5");
            splitter.focus("feat-a").unwrap();
            assert_eq!(stand_in::log_lines(log), [format!("focus-panel --workspace {WS} --panel surface:5")]);
        });
    }

    /// Whether closing an already-closed surface is an error isn't documented, so a failed
    /// close only counts as stopped when the surface is verifiably gone from `pane.list`.
    #[test]
    fn stopping_closes_the_surface_and_only_forgives_a_failure_if_it_is_really_gone() {
        with_fake_cmux("stop", "", &[], |mut splitter, log| {
            splitter.adopt("feat-a", "surface:5");
            splitter.stop("feat-a").unwrap();
            assert_eq!(stand_in::log_lines(log), ["close-surface --surface surface:5"]);
        });
        let gone = r#"case "$1" in close-surface) echo "no such surface" >&2; exit 1 ;; rpc) echo '{"panes":[]}' ;; esac"#;
        with_fake_cmux("stop-gone", gone, &[], |mut splitter, _| {
            splitter.adopt("feat-a", "surface:5");
            splitter.stop("feat-a").expect("a surface no longer in pane.list is what stop is after anyway");
        });
        let still_there = r#"case "$1" in close-surface) echo "busy" >&2; exit 1 ;; rpc) echo '{"panes":[{"surface_refs":["surface:5"]}]}' ;; esac"#;
        with_fake_cmux("stop-live", still_there, &[], |mut splitter, _| {
            splitter.adopt("feat-a", "surface:5");
            let err = splitter.stop("feat-a").unwrap_err().to_string();
            assert!(err.contains("close-surface"), "{err}");
        });
    }

    fn fixtures_body() -> String {
        format!(
            r#"case "$1" in
  rpc) cat '{}' ;;
  top) cat '{}' ;;
esac"#,
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cmux_pane_list.json"),
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/cmux_top.json"),
        )
    }

    /// The captured `pane.list` and `top` replies, read through the real process handling.
    #[test]
    fn polling_scopes_both_queries_to_the_workspace_and_reads_cpu_from_top() {
        with_fake_cmux("poll", &fixtures_body(), &[], |mut splitter, log| {
            splitter.set_workspace(Some(WS));
            splitter.adopt("busy", "surface:33");
            splitter.adopt("idle", "surface:34");
            splitter.adopt("closed", "surface:999");
            let statuses = splitter.poll_statuses().unwrap();
            assert_eq!(statuses["busy"], PaneStatus::Busy);
            assert_eq!(statuses["idle"], PaneStatus::Idle);
            assert_eq!(statuses["closed"], PaneStatus::Dead);
            assert_eq!(
                stand_in::log_lines(log),
                [format!("rpc pane.list {{\"workspace_id\":\"{WS}\"}}"), format!("top --workspace {WS} --json")]
            );
        });
    }

    /// With no workspace to scope `top` to, only existence is known: a closed surface is
    /// dead, and a live one is left out, so the last known status stands.
    #[test]
    fn polling_without_a_workspace_knows_only_what_is_gone() {
        with_fake_cmux("poll-unscoped", &fixtures_body(), &[], |mut splitter, log| {
            splitter.adopt("live", "surface:33");
            splitter.adopt("closed", "surface:999");
            let statuses = splitter.poll_statuses().unwrap();
            assert_eq!(statuses.get("closed"), Some(&PaneStatus::Dead));
            assert!(!statuses.contains_key("live"), "{statuses:?}");
            assert_eq!(stand_in::log_lines(log), ["rpc pane.list"], "top must not be asked without a workspace");
        });
    }
}
