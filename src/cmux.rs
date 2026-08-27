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
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::harness_launch::{build_launch_command, resolve_note_delivery, NoteDelivery};
pub use crate::pane_status::PaneStatus;
use crate::pane_status::CPU_BUSY_THRESHOLD_PERCENT;

/// Pixels of slack allowed when treating two pane edges as touching. Frames come back as
/// floats (e.g. `461.5`); this absorbs rounding without false-matching panes that are
/// merely near each other.
const ADJACENCY_EPSILON: f64 = 4.0;

#[derive(Debug, Clone)]
struct PaneHandle {
    surface_ref: String,
    status: PaneStatus,
}

#[derive(Clone)]
pub struct Cmux {
    bin: PathBuf,
    /// Shell command typed into the new terminal, e.g. `"claude"` or `"codex"`.
    harness: String,
    /// How the branch-context note (see `branch_context_note`) reaches this harness —
    /// see [`NoteDelivery`]. From `KANSTACK_HARNESS_SYSTEM_FLAG` if set (including
    /// explicitly to `""`, to opt out); otherwise whatever's known-good for `harness`
    /// itself — see `resolve_note_delivery`.
    note_delivery: NoteDelivery,
    /// Passed as `new-split`'s direction for the *first* lane, which splits off
    /// kanstack's own pane; `left`, `right`, `up`, or `down`.
    direction: String,
    /// Direction for every lane after the first, which splits off the previous lane
    /// instead of kanstack — kanstack's own above/below slot is taken after the first
    /// split, so further lanes fan out to the side rather than restacking that slot.
    chain_direction: String,
    /// Surface ref of the most recently spawned harness, so the next lane splits off it
    /// instead of kanstack's own pane. `None` splits off kanstack itself (the first lane).
    last_anchor: Option<String>,
    /// One entry per lane kanstack has opened a harness for, keyed by the branch name
    /// passed to `spawn_harness` — the lane's *original* parallel branch, which stays the
    /// key even if other branches later stack on top of it (stacking never opens a second
    /// pane).
    panes: HashMap<String, PaneHandle>,
}

impl Cmux {
    /// Locates the `cmux` binary and the configured harness/split direction. Returns
    /// `None` (not an error) when `cmux` is not on `PATH` and `KANSTACK_CMUX_BIN` is
    /// unset — kanstack runs fine outside cmux.
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
        let harness =
            std::env::var("KANSTACK_HARNESS").unwrap_or_else(|_| "claude".to_string());
        let note_delivery = resolve_note_delivery(&harness);
        let direction = std::env::var("KANSTACK_CMUX_DIRECTION")
            .map(|raw| normalize_direction(&raw))
            .unwrap_or_else(|_| "up".to_string());
        let chain_direction = std::env::var("KANSTACK_CMUX_CHAIN_DIRECTION")
            .map(|raw| normalize_direction(&raw))
            .unwrap_or_else(|_| "right".to_string());
        Some(Cmux {
            bin,
            harness,
            note_delivery,
            direction,
            chain_direction,
            last_anchor: None,
            panes: HashMap::new(),
        })
    }

    /// Whether kanstack has ever opened a pane for `branch` (regardless of its current
    /// status) — used to decide whether task dispatch needs to spawn one first.
    pub fn has_pane(&self, branch: &str) -> bool {
        self.panes.contains_key(branch)
    }

    /// Last known status of `branch`'s pane. `None` if kanstack has never tracked one for
    /// it at all (as opposed to `Some(PaneStatus::Dead)`, which means one existed and its
    /// surface has since disappeared from `pane.list`).
    pub fn pane_status(&self, branch: &str) -> Option<PaneStatus> {
        self.panes.get(branch).map(|p| p.status)
    }

    /// No panes tracked at all — nothing worth polling.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    /// Splits off the previous lane's pane (or kanstack's own, for the first lane), types
    /// the configured harness command into the fresh terminal with `cwd` as its working
    /// directory, then labels the tab `name` (e.g. the branch name).
    ///
    /// `initial_message`, if given, is appended to that same command line as a quoted
    /// argument (e.g. `claude "fix the flaky login test"`) rather than sent as a second
    /// `cmux send` afterwards — the harness needs a moment to start before it can receive
    /// typed input, same problem `confirm_task_dispatch` works around by asking for a
    /// second `t` press, and there's no "wait until ready" primitive to lean on here
    /// either. Folding it into the launch line sidesteps the race instead of racing it.
    ///
    /// A note about `name` being a GitButler virtual branch — not a real one
    /// `git status`/`git branch` would recognize — also goes out on this same launch
    /// line, via whichever delivery `note_delivery` resolved to for this harness; see
    /// [`NoteDelivery`] and `build_launch_command`.
    pub fn spawn_harness(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<()> {
        let (direction, anchor) = match &self.last_anchor {
            Some(anchor) => (self.chain_direction.clone(), Some(anchor.clone())),
            None => match self.occupant_in_direction(&self.direction) {
                // Something already sits in kanstack's own split slot (e.g. a tab the
                // user had open before kanstack started): join it beside that pane
                // instead of stacking a third row onto kanstack.
                Ok(Some(occupant)) => (self.chain_direction.clone(), Some(occupant)),
                Ok(None) => (self.direction.clone(), None),
                // No geometry, no $CMUX_SURFACE_ID, or a malformed response: fall back
                // to the plain behavior rather than failing the whole spawn over it.
                Err(_) => (self.direction.clone(), None),
            },
        };

        let mut split_args = vec!["new-split", direction.as_str()];
        if let Some(anchor) = &anchor {
            split_args.extend(["--surface", anchor.as_str()]);
        }
        let split_out = self.run(&split_args)?;
        let surface_ref = extract_ref(&split_out, "surface:")
            .with_context(|| format!("`cmux new-split` did not report a surface: {split_out:?}"))?
            .to_string();

        let launch = build_launch_command(cwd, &self.harness, &self.note_delivery, name, initial_message);
        self.run(&["send", "--surface", &surface_ref, &launch])?;

        self.run(&["rename-tab", "--surface", &surface_ref, name])?;
        self.panes.insert(
            name.to_string(),
            PaneHandle {
                surface_ref: surface_ref.clone(),
                status: PaneStatus::Unknown,
            },
        );
        self.last_anchor = Some(surface_ref);
        Ok(())
    }

    /// Sends `text` followed by Enter into `branch`'s tracked pane via `cmux send
    /// --surface`, the same mechanism `spawn_harness` already uses to type the launch
    /// command — just generalized to target a pane recorded earlier rather than the one
    /// just created.
    pub fn send_task(&self, branch: &str, text: &str) -> Result<()> {
        let Some(pane) = self.panes.get(branch) else {
            bail!("no cmux pane open for {branch} yet");
        };
        self.run(&["send", "--surface", &pane.surface_ref, &format!("{text}\n")])?;
        Ok(())
    }

    /// Re-derives every tracked pane's status: `cmux rpc pane.list` for whether the
    /// surface still exists at all, then (only when a `CMUX_WORKSPACE_ID` is available to
    /// scope the query to, and only when at least one tracked surface is still present)
    /// `cmux top --workspace <id> --json` for whether anything beyond the resting login
    /// shell is running in it.
    ///
    /// Pure and read-only on `self` — safe to call from a background thread against a
    /// cloned snapshot.
    pub fn poll_statuses(&self) -> Result<HashMap<String, PaneStatus>> {
        if self.panes.is_empty() {
            return Ok(HashMap::new());
        }
        let list_out = self.run(&["rpc", "pane.list"])?;

        let any_alive = {
            let list: PaneListResponse = serde_json::from_str(&list_out)
                .with_context(|| format!("bad pane.list response: {list_out:?}"))?;
            let present: std::collections::HashSet<String> = list
                .panes
                .into_iter()
                .flat_map(|p| p.surface_refs)
                .collect();
            self.panes.values().any(|p| present.contains(&p.surface_ref))
        };

        let top_out = if any_alive {
            match std::env::var("CMUX_WORKSPACE_ID") {
                // Else: no workspace to scope `top` to without guessing —
                // existence-only status is still derived below; the Busy/Idle split
                // is skipped this round.
                Ok(workspace_id) => Some(self.run(&["top", "--workspace", &workspace_id, "--json"])?),
                Err(_) => None,
            }
        } else {
            None
        };

        classify_statuses(&self.panes, &list_out, top_out.as_deref())
    }

    /// Merges a `poll_statuses` result back in, keyed by branch. Entries for lanes deleted
    /// since the poll started are simply absent from `self.panes` and are ignored.
    pub fn apply_statuses(&mut self, statuses: HashMap<String, PaneStatus>) {
        for (branch, status) in statuses {
            if let Some(pane) = self.panes.get_mut(&branch) {
                pane.status = status;
            }
        }
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

/// Pure classification step of [`Cmux::poll_statuses`], split out so it can be unit
/// tested against captured JSON fixtures without shelling out to a real `cmux`.
/// `list_json` is a `cmux rpc pane.list` response; `top_json`, if present, is a `cmux top
/// --json` response.
fn classify_statuses(
    panes: &HashMap<String, PaneHandle>,
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
        .map(|(branch, pane)| {
            let status = if !present.contains(pane.surface_ref.as_str()) {
                PaneStatus::Dead
            } else {
                match cpu_percents.get(&pane.surface_ref) {
                    Some(cpu) if *cpu > CPU_BUSY_THRESHOLD_PERCENT => PaneStatus::Busy,
                    Some(_) => PaneStatus::Idle,
                    // No CPU data this round: keep whatever was already known rather
                    // than guessing.
                    None => pane.status,
                }
            };
            (branch.clone(), status)
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

/// Accepts the more readable `above`/`below` alongside `cmux new-split`'s native
/// `up`/`down`/`left`/`right`, so `KANSTACK_CMUX_DIRECTION=above` doesn't just get passed
/// straight through to a flag that rejects it.
fn normalize_direction(raw: &str) -> String {
    match raw {
        "above" => "up",
        "below" => "down",
        other => other,
    }
    .to_string()
}

/// Pulls a `<prefix><id>` token (e.g. `surface:31`) out of an `OK ...` response line.
fn extract_ref<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.split_whitespace().find(|tok| tok.starts_with(prefix))
}

fn command_exists(bin: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin).is_file())
}

#[cfg(test)]
mod tests {
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
    /// doc comment): the failure surfaces later, from `spawn_harness` actually trying to
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

    fn pane(surface_ref: &str, status: PaneStatus) -> PaneHandle {
        PaneHandle {
            surface_ref: surface_ref.to_string(),
            status,
        }
    }

    /// A tracked surface that's present in `pane.list` and shows CPU usage above
    /// [`CPU_BUSY_THRESHOLD_PERCENT`] in `cmux top` (a real, actively-generating `claude`
    /// session, captured live at ~35%) classifies as `Busy`.
    #[test]
    fn classify_statuses_marks_a_high_cpu_surface_busy() {
        let panes = HashMap::from([("feature-a".to_string(), pane("surface:33", PaneStatus::Unknown))]);
        let statuses =
            classify_statuses(&panes, PANE_LIST_FIXTURE, Some(TOP_FIXTURE)).unwrap();
        assert_eq!(statuses["feature-a"], PaneStatus::Busy);
    }

    /// A tracked surface present in `pane.list` with CPU usage at the busy threshold or
    /// below (a resting `claude` prompt, captured live at 0%) classifies as `Idle` even
    /// though its process count stays high — this is the case `process_count` alone got
    /// wrong.
    #[test]
    fn classify_statuses_marks_a_low_cpu_surface_idle() {
        let panes = HashMap::from([("feature-b".to_string(), pane("surface:34", PaneStatus::Unknown))]);
        let statuses =
            classify_statuses(&panes, PANE_LIST_FIXTURE, Some(TOP_FIXTURE)).unwrap();
        assert_eq!(statuses["feature-b"], PaneStatus::Idle);
    }

    /// A tracked surface absent from `pane.list` entirely — its tab was closed —
    /// classifies as `Dead`, regardless of what `cmux top` says.
    #[test]
    fn classify_statuses_marks_a_missing_surface_dead() {
        let panes = HashMap::from([("feature-c".to_string(), pane("surface:999", PaneStatus::Busy))]);
        let statuses =
            classify_statuses(&panes, PANE_LIST_FIXTURE, Some(TOP_FIXTURE)).unwrap();
        assert_eq!(statuses["feature-c"], PaneStatus::Dead);
    }

    /// Without a `cmux top` response (e.g. no `CMUX_WORKSPACE_ID` to scope it), a surface
    /// still present in `pane.list` keeps its previously known status rather than being
    /// guessed at — existence alone can't distinguish Busy from Idle.
    #[test]
    fn classify_statuses_keeps_prior_status_without_a_top_response() {
        let panes = HashMap::from([("feature-a".to_string(), pane("surface:33", PaneStatus::Busy))]);
        let statuses = classify_statuses(&panes, PANE_LIST_FIXTURE, None).unwrap();
        assert_eq!(statuses["feature-a"], PaneStatus::Busy);
    }
}
