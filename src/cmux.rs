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

/// Pixels of slack allowed when treating two pane edges as touching. Frames come back as
/// floats (e.g. `461.5`); this absorbs rounding without false-matching panes that are
/// merely near each other.
const ADJACENCY_EPSILON: f64 = 4.0;

/// Last known liveness/activity of a lane's cmux pane. See [`Cmux::poll_statuses`] for how
/// this is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    /// Present in `pane.list`; `cmux top` reports CPU usage above
    /// [`CPU_BUSY_THRESHOLD_PERCENT`] for it.
    Busy,
    /// Present in `pane.list`; CPU usage is at or below the busy threshold. May mean the
    /// harness returned to a resting prompt, or never got a chance to start —
    /// `poll_statuses` cannot tell those apart from this alone.
    Idle,
    /// No longer present in `pane.list` at all — the tab was closed, by the user or by
    /// cmux itself. Whether cmux auto-closes a tab when the shell inside it exits is a
    /// cmux/terminal configuration question this module has not verified either way.
    Dead,
    /// Tracked, but no poll has completed yet (or the last one couldn't classify it).
    Unknown,
}

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

/// CPU usage above this, for a tracked surface, counts as `Busy` rather than `Idle`.
/// Comfortably above the 0–0.9% observed live for a resting shell or an idle `claude`
/// prompt, and well below the 30%+ seen while a harness is actually generating — chosen
/// to absorb background noise (telemetry pings, idle polling) without also absorbing real
/// work.
const CPU_BUSY_THRESHOLD_PERCENT: f64 = 3.0;

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

/// How the branch-context note (see `branch_context_note`) reaches a harness's launch
/// command — resolved once, in `resolve_note_delivery`, from `KANSTACK_HARNESS_SYSTEM_FLAG`
/// and the configured harness itself.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoteDelivery {
    /// `<flag> <shell-quoted note>`, ahead of the initial message — e.g. `claude
    /// --append-system-prompt "..."`. Additive to the harness's own default system
    /// prompt, not a replacement, and doesn't consume a "turn" the way the initial
    /// message does — so it goes out even when there's no initial message at all.
    Flag(String),
    /// Codex's `-c developer_instructions="<note>"` config override: a generic
    /// instructions-role message, not literally an appended system prompt, but the
    /// closest thing Codex CLI has to one today (no `--append-system-prompt` exists for
    /// it — see `resolve_note_delivery`). Same "goes out regardless of the initial
    /// message" property as `Flag`.
    CodexConfig,
    /// No known out-of-band mechanism for this harness (e.g. OpenCode, which as of
    /// writing has no CLI flag for this at all — a feature request for exactly this was
    /// filed and closed as not planned). Falls back to prepending the note directly onto
    /// the initial message text instead — the one thing every harness's launch command
    /// already has room for.
    FoldIntoMessage,
    /// Explicitly turned off via `KANSTACK_HARNESS_SYSTEM_FLAG=""`.
    Disabled,
}

/// Builds the line `cmux send` types into the fresh terminal — split out from
/// `spawn_harness` so it can be unit tested without shelling out to a real `cmux`, the
/// same reason `classify_statuses` is split out from `poll_statuses`.
fn build_launch_command(
    cwd: &Path,
    harness: &str,
    note_delivery: &NoteDelivery,
    name: &str,
    initial_message: Option<&str>,
) -> String {
    let mut launch = format!("cd {} && {}", shell_quote(&cwd.to_string_lossy()), harness);
    let mut message = initial_message.map(str::to_string);

    match note_delivery {
        NoteDelivery::Flag(flag) => {
            launch.push(' ');
            launch.push_str(flag);
            launch.push(' ');
            launch.push_str(&shell_quote(&branch_context_note(name)));
        }
        NoteDelivery::CodexConfig => {
            launch.push_str(" -c ");
            launch.push_str(&shell_quote(&format!(
                "developer_instructions={}",
                toml_quote(&branch_context_note(name))
            )));
        }
        NoteDelivery::FoldIntoMessage => {
            message = Some(match message {
                Some(m) => format!("{}\n\n{m}", branch_context_note(name)),
                None => branch_context_note(name),
            });
        }
        NoteDelivery::Disabled => {}
    }

    if let Some(message) = message {
        launch.push(' ');
        launch.push_str(&shell_quote(&message));
    }
    launch.push('\n');
    launch
}

/// The `NoteDelivery` for `harness`, from `KANSTACK_HARNESS_SYSTEM_FLAG` if set —
/// including explicitly to `""`, to opt out entirely — otherwise whatever's known-good
/// for `harness` itself, matched against its own basename (so `claude
/// --dangerously-skip-permissions` baked into `KANSTACK_HARNESS` still resolves the same
/// as plain `claude` would):
///
/// - `claude`, `pi`: confirmed `--append-system-prompt <text>`, additive to (not
///   replacing) their own default system prompt.
/// - `codex`: no such flag exists (a request for exactly that,
///   openai/codex#11117, is closed unimplemented) — `-c developer_instructions=<toml>` is
///   the closest equivalent, a differently-*shaped* mechanism (TOML-quoted, and a
///   "developer" message rather than literally the system prompt), not just a different
///   flag name.
/// - `opencode`, `kiro`/`kiro-cli`, `gemini`, and anything else unrecognized: no
///   mechanism exists at all, each confirmed separately rather than assumed —
///   OpenCode has no such flag (a feature request for exactly this,
///   anomalyco/opencode#16089, was closed as not planned); Kiro CLI's full `kiro-cli
///   chat` flag reference has nothing for system prompt/instructions/context (only
///   pre-configured, not-dynamic-per-invocation agents via `--agent`); Gemini CLI's only
///   mechanism, `GEMINI_SYSTEM_MD`, is an env var pointing at a file that *fully
///   replaces* the default system prompt rather than appending to it — using it would
///   strip Gemini CLI's own built-in behavior instructions, worse than sending nothing.
///   All fall back to folding the note into the initial message itself, the one delivery
///   every harness supports.
fn resolve_note_delivery(harness: &str) -> NoteDelivery {
    match std::env::var("KANSTACK_HARNESS_SYSTEM_FLAG") {
        Ok(flag) if flag.is_empty() => return NoteDelivery::Disabled,
        Ok(flag) => return NoteDelivery::Flag(flag),
        Err(_) => {}
    }
    let program = harness.split_whitespace().next().unwrap_or(harness);
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    match name {
        "claude" | "pi" => NoteDelivery::Flag("--append-system-prompt".to_string()),
        "codex" => NoteDelivery::CodexConfig,
        "opencode" | "kiro" | "kiro-cli" | "gemini" => NoteDelivery::FoldIntoMessage,
        _ => NoteDelivery::FoldIntoMessage,
    }
}

/// The context note delivered to a harness, telling it what `git status`/`git branch`
/// won't: it's sitting in `branch`, a GitButler *virtual* branch, not a real one — every
/// lane in the workspace shares the same checked-out ref, so those commands report that
/// shared state rather than anything specific to `branch`.
fn branch_context_note(branch: &str) -> String {
    format!(
        "You're running inside a GitButler virtual branch called \"{branch}\". `git status`/\
         `git branch` reflect the shared workspace checkout, not this branch specifically \
         — use `but status` or `but diff` (GitButler's CLI) to see the real state if you \
         need it."
    )
}

/// Single-quotes a path for the target shell, escaping any embedded `'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double-quotes `s` as a TOML basic string value, escaping `\` and `"` — what Codex's
/// `-c key=value` needs for a string value, on top of (inside) the shell-level quoting
/// `shell_quote` already applies to the whole `-c` argument.
fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
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

    /// `Disabled` (`KANSTACK_HARNESS_SYSTEM_FLAG=""`) is the one delivery that sends no
    /// note at all — the launch line is exactly what it would've been before this
    /// feature existed.
    #[test]
    fn build_launch_command_sends_nothing_when_disabled() {
        let launch = build_launch_command(Path::new("/repo"), "claude", &NoteDelivery::Disabled, "feat-x", Some("fix it"));
        assert_eq!(launch, "cd '/repo' && claude 'fix it'\n");
    }

    /// `Flag` puts the note through the configured flag, ahead of the initial message,
    /// which stays a separate, ordinary positional argument.
    #[test]
    fn build_launch_command_passes_the_note_through_a_flag() {
        let delivery = NoteDelivery::Flag("--append-system-prompt".to_string());
        let launch = build_launch_command(Path::new("/repo"), "claude", &delivery, "feat-x", Some("fix it"));
        assert_eq!(
            launch,
            format!(
                "cd '/repo' && claude --append-system-prompt {} 'fix it'\n",
                shell_quote(&branch_context_note("feat-x"))
            )
        );
    }

    /// A `Flag` delivery's note goes out even with no initial message at all — appending
    /// to the system prompt doesn't put words in anyone's mouth the way a positional
    /// prompt argument would, so an otherwise-blank REPL still gets to know where it's
    /// running.
    #[test]
    fn build_launch_command_sends_a_flag_note_even_without_an_initial_message() {
        let delivery = NoteDelivery::Flag("--append-system-prompt".to_string());
        let launch = build_launch_command(Path::new("/repo"), "claude", &delivery, "feat-x", None);
        assert!(
            launch.contains("--append-system-prompt"),
            "the note must go out regardless of whether there's a message: {launch:?}"
        );
        assert!(
            !launch.trim_end().ends_with("--append-system-prompt"),
            "the note's own text must follow the flag: {launch:?}"
        );
    }

    /// `CodexConfig` wraps the note in TOML double-quotes *inside* the shell-quoted
    /// `-c` argument — two independent layers of quoting, not one.
    #[test]
    fn build_launch_command_toml_quotes_the_note_for_codex_config() {
        let launch = build_launch_command(Path::new("/repo"), "codex", &NoteDelivery::CodexConfig, "feat-x", Some("fix it"));
        let expected_arg = shell_quote(&format!(
            "developer_instructions={}",
            toml_quote(&branch_context_note("feat-x"))
        ));
        assert_eq!(launch, format!("cd '/repo' && codex -c {expected_arg} 'fix it'\n"));
    }

    /// `FoldIntoMessage` has no separate channel — the note becomes the start of the
    /// initial message itself, with the original message kept intact after it.
    #[test]
    fn build_launch_command_folds_the_note_onto_an_existing_message() {
        let launch = build_launch_command(Path::new("/repo"), "opencode", &NoteDelivery::FoldIntoMessage, "feat-x", Some("fix it"));
        let expected_message = format!("{}\n\nfix it", branch_context_note("feat-x"));
        assert_eq!(launch, format!("cd '/repo' && opencode {}\n", shell_quote(&expected_message)));
    }

    /// With no initial message to fold onto, the note becomes the whole message — unlike
    /// `Flag`/`CodexConfig`, this is the only delivery that has to spend the "turn" a
    /// positional prompt argument costs, since folding is all it has.
    #[test]
    fn build_launch_command_sends_the_note_alone_when_folding_with_no_message() {
        let launch = build_launch_command(Path::new("/repo"), "opencode", &NoteDelivery::FoldIntoMessage, "feat-x", None);
        assert_eq!(
            launch,
            format!("cd '/repo' && opencode {}\n", shell_quote(&branch_context_note("feat-x")))
        );
    }

    #[test]
    fn branch_context_note_names_the_branch_and_points_at_but() {
        let note = branch_context_note("feat-x");
        assert!(note.contains("\"feat-x\""), "missing the branch name: {note:?}");
        assert!(note.contains("but status"), "missing the but-CLI pointer: {note:?}");
    }

    #[test]
    fn toml_quote_escapes_backslashes_and_quotes() {
        assert_eq!(toml_quote(r#"say "hi" \ bye"#), r#""say \"hi\" \\ bye""#);
    }

    /// Serializes every `with_system_flag_env` call against every other one — `cargo
    /// test` runs tests concurrently on separate threads by default, but
    /// `std::env::set_var`/`remove_var` mutate process-wide state, so two of these
    /// running at once could see (or clobber) each other's value mid-test without this.
    static SYSTEM_FLAG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs `body` with `KANSTACK_HARNESS_SYSTEM_FLAG` swapped out and restored
    /// afterwards, so this doesn't leak into other tests running in the same process.
    fn with_system_flag_env(value: Option<&str>, body: impl FnOnce()) {
        // Held for the whole call, not just the swap, so `body` (which reads the var via
        // `resolve_note_delivery`) can't be interleaved with another test's swap either.
        let _guard = SYSTEM_FLAG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("KANSTACK_HARNESS_SYSTEM_FLAG");
        match value {
            Some(v) => std::env::set_var("KANSTACK_HARNESS_SYSTEM_FLAG", v),
            None => std::env::remove_var("KANSTACK_HARNESS_SYSTEM_FLAG"),
        }
        body();
        match old {
            Some(v) => std::env::set_var("KANSTACK_HARNESS_SYSTEM_FLAG", v),
            None => std::env::remove_var("KANSTACK_HARNESS_SYSTEM_FLAG"),
        }
    }

    #[test]
    fn resolve_note_delivery_covers_claude_and_pi() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
            assert_eq!(
                resolve_note_delivery("pi"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    #[test]
    fn resolve_note_delivery_uses_codex_config_for_codex() {
        with_system_flag_env(None, || {
            assert_eq!(resolve_note_delivery("codex"), NoteDelivery::CodexConfig);
        });
    }

    /// Extra flags baked into `KANSTACK_HARNESS` (e.g. to skip a permissions prompt)
    /// shouldn't stop the harness itself from being recognized.
    #[test]
    fn resolve_note_delivery_matches_the_program_ignoring_its_own_flags() {
        with_system_flag_env(None, || {
            assert_eq!(
                resolve_note_delivery("claude --dangerously-skip-permissions"),
                NoteDelivery::Flag("--append-system-prompt".to_string())
            );
        });
    }

    /// Harnesses with no confirmed mechanism (including OpenCode, and anything unknown)
    /// fall back to folding the note into the message rather than guessing at a flag
    /// that might not exist for them.
    #[test]
    fn resolve_note_delivery_falls_back_to_folding_for_unconfirmed_harnesses() {
        with_system_flag_env(None, || {
            // opencode: confirmed no such flag exists at all (anomalyco/opencode#16089,
            // a request for exactly this, closed not planned).
            assert_eq!(resolve_note_delivery("opencode"), NoteDelivery::FoldIntoMessage);
            // kiro / kiro-cli: the full `kiro-cli chat` flag reference has nothing for
            // system prompt/instructions/context.
            assert_eq!(resolve_note_delivery("kiro"), NoteDelivery::FoldIntoMessage);
            assert_eq!(resolve_note_delivery("kiro-cli"), NoteDelivery::FoldIntoMessage);
            // gemini: the only mechanism (GEMINI_SYSTEM_MD) fully replaces the default
            // system prompt rather than appending, so using it would do more harm than
            // sending nothing.
            assert_eq!(resolve_note_delivery("gemini"), NoteDelivery::FoldIntoMessage);
            // Genuinely unrecognized commands get the same safe fallback.
            assert_eq!(resolve_note_delivery("./my-custom-harness.sh"), NoteDelivery::FoldIntoMessage);
        });
    }

    #[test]
    fn resolve_note_delivery_honors_an_explicit_flag_override() {
        with_system_flag_env(Some("--my-custom-flag"), || {
            assert_eq!(
                resolve_note_delivery("codex"),
                NoteDelivery::Flag("--my-custom-flag".to_string())
            );
        });
    }

    #[test]
    fn resolve_note_delivery_is_disabled_by_an_empty_override() {
        with_system_flag_env(Some(""), || {
            assert_eq!(resolve_note_delivery("claude"), NoteDelivery::Disabled);
        });
    }

    /// Runs `body` with `PATH` and `KANSTACK_CMUX_BIN` swapped out and restored
    /// afterwards, so this doesn't leak into other tests running in the same process —
    /// both are process-wide state, and other tests never touch either, but this one
    /// mutates both, so it must clean up regardless of how `body` returns.
    fn with_env(path: Option<&str>, cmux_bin: Option<&str>, body: impl FnOnce()) {
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
