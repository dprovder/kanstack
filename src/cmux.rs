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

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Pixels of slack allowed when treating two pane edges as touching. Frames come back as
/// floats (e.g. `461.5`); this absorbs rounding without false-matching panes that are
/// merely near each other.
const ADJACENCY_EPSILON: f64 = 4.0;

pub struct Cmux {
    bin: PathBuf,
    /// Shell command typed into the new terminal, e.g. `"claude"` or `"codex"`.
    harness: String,
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
        let direction = std::env::var("KANSTACK_CMUX_DIRECTION")
            .map(|raw| normalize_direction(&raw))
            .unwrap_or_else(|_| "up".to_string());
        let chain_direction = std::env::var("KANSTACK_CMUX_CHAIN_DIRECTION")
            .map(|raw| normalize_direction(&raw))
            .unwrap_or_else(|_| "right".to_string());
        Some(Cmux {
            bin,
            harness,
            direction,
            chain_direction,
            last_anchor: None,
        })
    }

    /// Splits off the previous lane's pane (or kanstack's own, for the first lane), types
    /// the configured harness command into the fresh terminal with `cwd` as its working
    /// directory, then labels the tab `name` (e.g. the branch name).
    pub fn spawn_harness(&mut self, cwd: &Path, name: &str) -> Result<()> {
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

        let launch = format!(
            "cd {} && {}\n",
            shell_quote(&cwd.to_string_lossy()),
            self.harness
        );
        self.run(&["send", "--surface", &surface_ref, &launch])?;

        self.run(&["rename-tab", "--surface", &surface_ref, name])?;
        self.last_anchor = Some(surface_ref);
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
}

#[derive(Deserialize)]
struct PixelFrame {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
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

/// Single-quotes a path for the target shell, escaping any embedded `'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn command_exists(bin: &Path) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(bin).is_file())
}
