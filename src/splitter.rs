//! The harness-split state kanstack keeps, whichever multiplexer it is in — `cmux` if
//! present, else plain `tmux`, else `orca`, else Ghostty (see `crate::mux::cmux`, `crate::mux::tmux`,
//! `crate::mux::orca`, `crate::mux::ghostty`), paired with the harness it launches (`crate::harness`).
//!
//! A [`Multiplexer`] only opens, types into, focuses, closes and probes panes. Which branch a
//! pane belongs to, where the next one splits off, and what a pane's last known status was
//! are the same for every multiplexer, so they live here once, and `app/`'s call sites
//! needn't care which one they hold.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, bail, Result};

use crate::harness::HarnessConfig;
use crate::mux::cmux::Cmux;
use crate::mux::ghostty::Ghostty;
use crate::mux::orca::Orca;
use crate::mux::tmux::Tmux;
use crate::mux::{configured_directions, normalize_direction, Multiplexer, OpenRequest};
use crate::mux::pane_status::PaneStatus;
use crate::procs::{
    reading_from_pid, real_age, real_killer, real_ps, record_pid_prefix, started_before, subtree, tracking_applies, AgeReader,
    Killer, Pids, PsReader, PsRow,
};
use crate::report::{Reports, Said};

/// How old a `busy` report must be before a quiet pane is allowed to cast doubt on it. A fresh
/// one is taken at its word: the multiplexer's reading lags the agent's.
const CORROBORATION_GRACE: Duration = Duration::from_secs(20);

/// How many times, and how far apart, a quiet pane is looked at again before it is believed
/// over a `busy` report. Real work dips below the CPU threshold for a moment now and then —
/// three times inside one 63-second turn when measured — and a single quiet reading must not
/// be enough. The gap is longer than those dips.
const QUIET_CONFIRMATIONS: usize = 2;
const QUIET_GAP: Duration = Duration::from_millis(1200);

/// A lane's status from what can be seen of its pane from outside (`native`: the multiplexer's
/// reading, or failing that the process table's — see [`Splitter::native_readings`]) and what
/// its agent last said about itself (`said`). `None` means neither has news, and the caller
/// keeps what it knew.
///
/// - A pane that is gone is `Dead`, whatever was reported: a crashed agent can't have said it
///   stopped.
/// - Otherwise a fresh report wins. It comes from the harness itself, where the multiplexer
///   (or the process table) only infers from CPU, or from nothing at all.
/// - A report that has run out is worth nothing, and must not leave the last thing it said
///   standing: where no multiplexer can contradict it, an agent that never said it had
///   stopped would read `busy` forever. It is `Unknown` unless the multiplexer knows better.
///
/// What this does *not* do is second-guess a fresh report from a single CPU reading — see
/// [`stale_busy_suspect`] for the one case where it is done, and only after confirming.
fn merge(native: Option<PaneStatus>, said: Option<Said>) -> Option<PaneStatus> {
    if native == Some(PaneStatus::Dead) {
        return Some(PaneStatus::Dead);
    }
    match said {
        Some(Said::Fresh { status, .. }) => Some(status),
        Some(Said::Stale) => Some(native.unwrap_or(PaneStatus::Unknown)),
        None => native,
    }
}

/// A `busy` report that the pane's quiet may have made stale, identified by the second it was
/// written: it is old enough that the multiplexer has had time to catch up, and the pane
/// reads idle.
///
/// Interrupting a turn with Escape, or answering "No" to a permission prompt, fires no hook —
/// checked against real Claude — so nothing ever takes the `busy` back, and it would stand
/// until it expired. Only a pane that stays quiet can say so. Never `waiting`: a pane stopped
/// on a prompt and one at rest look identical from outside, and the reverse — an old
/// `waiting` over a busy pane — needs no rule at all, because carrying on always fires a hook
/// (a new prompt, or the tool finishing) that replaces it. That rule was tried and dropped: a
/// CPU blip over a prompt left for a minute read it as busy.
fn stale_busy_suspect(native: Option<PaneStatus>, said: Option<Said>) -> Option<u64> {
    match (native, said) {
        (Some(PaneStatus::Idle), Some(Said::Fresh { status: PaneStatus::Busy, age, at })) if age >= CORROBORATION_GRACE => Some(at),
        _ => None,
    }
}

/// The process table for one call of [`Splitter::native_readings`]: read the first time a pane
/// needs it and never again within that call, and read as `None` if `ps` failed — the panes
/// that needed it then have no reading, and it isn't retried for each of them.
struct ProcessTable<'a> {
    read: &'a PsReader,
    rows: Option<Option<Vec<PsRow>>>,
}

impl ProcessTable<'_> {
    fn rows(&mut self) -> Option<&[PsRow]> {
        let read = self.read;
        self.rows.get_or_insert_with(|| read().ok()).as_deref()
    }
}

/// One multiplexer backend kanstack can find. [`BACKENDS`] is the only place they are listed:
/// discovery, the `KANSTACK_SPLIT_BACKEND` override, the setup wizard's picker and its
/// detection panel, and the messages that name them all read it. Adding a backend is a new
/// file, one entry here, and the docs the tests below say are missing.
pub struct BackendEntry {
    /// What `KANSTACK_SPLIT_BACKEND` calls it, and what `Multiplexer::name` returns: `tmux`.
    pub name: &'static str,
    /// How it is written in prose: `Orca`.
    pub label: &'static str,
    discover: fn() -> Option<Arc<dyn Multiplexer>>,
    /// One line for the setup wizard: whether it is usable here and, if not, what to do.
    pub detection: fn() -> String,
}

fn discover_cmux() -> Option<Arc<dyn Multiplexer>> {
    Cmux::discover().map(|m| Arc::new(m) as Arc<dyn Multiplexer>)
}

fn discover_tmux() -> Option<Arc<dyn Multiplexer>> {
    Tmux::discover().map(|m| Arc::new(m) as Arc<dyn Multiplexer>)
}

fn discover_orca() -> Option<Arc<dyn Multiplexer>> {
    Orca::discover().map(|m| Arc::new(m) as Arc<dyn Multiplexer>)
}

fn discover_ghostty() -> Option<Arc<dyn Multiplexer>> {
    Ghostty::discover().map(|m| Arc::new(m) as Arc<dyn Multiplexer>)
}

/// Every backend, in the order automatic discovery tries them.
pub const BACKENDS: &[BackendEntry] = &[
    BackendEntry { name: "cmux", label: "cmux", discover: discover_cmux, detection: crate::mux::cmux::detection },
    BackendEntry { name: "tmux", label: "tmux", discover: discover_tmux, detection: crate::mux::tmux::detection },
    BackendEntry { name: "orca", label: "Orca", discover: discover_orca, detection: crate::mux::orca::detection },
    BackendEntry { name: "ghostty", label: "Ghostty", discover: discover_ghostty, detection: crate::mux::ghostty::detection },
];

/// The backends' names as prose: `cmux, tmux, Orca or Ghostty`.
pub fn describe_backends() -> String {
    let labels: Vec<&str> = BACKENDS.iter().map(|b| b.label).collect();
    match labels.as_slice() {
        [] => String::new(),
        [only] => only.to_string(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}

/// One lane's pane, as this process knows it.
#[derive(Debug, Clone)]
struct Pane {
    /// The multiplexer's own id for it.
    id: String,
    status: PaneStatus,
}

#[derive(Clone)]
pub struct Splitter {
    mux: Arc<dyn Multiplexer>,
    harness: HarnessConfig,
    /// Where the first lane goes relative to what it splits.
    first_direction: String,
    /// Where every later lane goes relative to the one before it.
    chain_direction: String,
    /// The pane the most recent spawn opened, so the next lane splits off it instead of
    /// kanstack's own pane. `None` for the first lane.
    last_anchor: Option<String>,
    /// One entry per lane a harness was opened for, keyed by the branch name passed to
    /// `spawn_harness` — the lane's *original* parallel branch, which stays the key even if
    /// other branches later stack on top of it (stacking never opens a second pane).
    panes: HashMap<String, Pane>,
    /// What each lane's agent has said about itself, where it can — see `crate::report`.
    reports: Reports,
    /// Where each pane's shell pid is recorded, when its process is tracked — see
    /// `crate::procs`.
    pids: Pids,
    /// `KANSTACK_TRACK_PIDS` as it was when this splitter was made, so what a splitter does
    /// doesn't change under a test that sets it for another.
    track_setting: Option<String>,
    /// Where the process table comes from, for a pane whose process is tracked: `ps`, except
    /// in a test.
    ps: PsReader,
    /// How long a process has been running, to tell a pane's recorded shell from a stranger
    /// that has since been given its pid: `ps`, except in a test.
    age: AgeReader,
    /// How a pane's processes are ended when it is stopped: `SIGTERM`, except in a test.
    kill: Killer,
    /// How long to wait between the looks that confirm a pane is really quiet.
    quiet_gap: Duration,
}

impl Splitter {
    /// A splitter over `mux` launching `harness`, with the directions `mux` is configured
    /// for (see `crate::mux::configured_directions`) and no panes yet.
    pub fn new(mux: Arc<dyn Multiplexer>, harness: HarnessConfig) -> Self {
        let (first_direction, chain_direction) = configured_directions(mux.as_ref());
        Splitter {
            mux,
            harness,
            first_direction,
            chain_direction,
            last_anchor: None,
            panes: HashMap::new(),
            reports: Reports::default(),
            pids: Pids::default(),
            track_setting: std::env::var("KANSTACK_TRACK_PIDS").ok(),
            ps: real_ps(),
            age: real_age(),
            kill: real_killer(),
            quiet_gap: QUIET_GAP,
        }
    }

    /// Tries `Cmux::discover` first, then `Tmux::discover`, then `Orca::discover`, then
    /// `Ghostty::discover` — see each for what makes a backend usable at all. `None` means none
    /// is available, same as any alone.
    ///
    /// `KANSTACK_SPLIT_BACKEND=cmux`/`tmux`/`orca`/`ghostty` skips the other backends' detection
    /// entirely, rather than just reordering the fallback: `Cmux::discover` only checks
    /// whether the `cmux` binary is on `PATH`, not whether kanstack is actually running
    /// inside a cmux pane (unlike `Tmux::discover`, which requires `$TMUX_PANE`,
    /// `Orca::discover`, which requires `$ORCA_TERMINAL_HANDLE`, and `Ghostty::discover`, which
    /// requires `TERM_PROGRAM=ghostty`), so a machine with both
    /// binaries installed — cmux for unrelated reasons, tmux the one actually in use right
    /// now — would otherwise have cmux win by default and fail at spawn time instead of
    /// falling through. The override exists for exactly that ambiguity; an unrecognized
    /// value falls back to plain auto-detection, same as `KANSTACK_BRANCH_UI` does.
    ///
    /// One case the override needn't be reached for: running inside an Orca terminal, and
    /// not inside a cmux pane (`$CMUX_SURFACE_ID`) or tmux pane (`$TMUX_PANE`). That is the
    /// same cmux-installed-but-not-in-use ambiguity, and here there *is* a positive signal
    /// that Orca is the one in use, so it is tried ahead of cmux's bare `PATH` check. Inside
    /// a tmux or cmux pane in an Orca terminal, the default order is left as it was.
    ///
    /// The same goes for Ghostty, and for the same reason: `TERM_PROGRAM=ghostty` with no
    /// `$CMUX_SURFACE_ID` and no `$TMUX_PANE` is a positive sign that plain Ghostty is what
    /// kanstack is in, so it goes ahead of cmux's bare `PATH` check too. (cmux, a Ghostty
    /// fork, sets `TERM_PROGRAM=ghostty` as well, which is why `$CMUX_SURFACE_ID` is part of
    /// that sign.) Ghostty is otherwise last.
    ///
    /// The harness launched into new panes is `$KANSTACK_HARNESS` (default `claude`), read
    /// here once rather than by each backend.
    pub fn discover() -> Option<Self> {
        Some(Splitter::new(Self::discover_mux()?, HarnessConfig::from_env()))
    }

    fn discover_mux() -> Option<Arc<dyn Multiplexer>> {
        let forced = std::env::var("KANSTACK_SPLIT_BACKEND").ok();
        if let Some(backend) = BACKENDS.iter().find(|b| Some(b.name) == forced.as_deref()) {
            return (backend.discover)();
        }
        let in_cmux_or_tmux =
            std::env::var_os("CMUX_SURFACE_ID").is_some() || std::env::var_os("TMUX_PANE").is_some();
        if Orca::running_inside() && !in_cmux_or_tmux {
            if let Some(orca) = discover_orca() {
                return Some(orca);
            }
        }
        // `running_inside` already rules out cmux and tmux panes.
        if Ghostty::running_inside() {
            if let Some(ghostty) = discover_ghostty() {
                return Some(ghostty);
            }
        }
        BACKENDS.iter().find_map(|b| (b.discover)())
    }

    /// Which backend this is, for the one place UI copy needs to name it: the branch
    /// modal's checkbox row label.
    pub fn label(&self) -> &'static str {
        self.mux.name()
    }

    /// Whether kanstack itself looks like it's still running inside this backend's own
    /// pane right now — checked before starting a background poll, since polling only
    /// makes sense while that's true.
    pub fn running_inside_host(&self) -> bool {
        self.mux.running_inside()
    }

    /// Whether kanstack has ever opened a pane for `branch` (regardless of its current
    /// status) — used to decide whether task dispatch needs to spawn one first.
    pub fn has_pane(&self, branch: &str) -> bool {
        self.panes.contains_key(branch)
    }

    /// Last known status of `branch`'s pane. `None` if kanstack has never tracked one for
    /// it at all (as opposed to `Some(PaneStatus::Dead)`, which means one existed and has
    /// since disappeared).
    pub fn pane_status(&self, branch: &str) -> Option<PaneStatus> {
        self.panes.get(branch).map(|p| p.status)
    }

    /// No panes tracked at all — nothing worth polling.
    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    pub fn spawn_harness(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>) -> Result<String> {
        self.spawn_harness_with(cwd, name, initial_message, None)
    }

    /// Opens a pane running the harness on lane `name`, with `cwd` as its working directory,
    /// and starts tracking it. `harness` runs in place of the configured one, for just this
    /// pane. Returns the new pane's backend-specific id, for a caller that needs to find it
    /// again from another process.
    ///
    /// `initial_message` and the note that `name` is a GitButler virtual branch go out on
    /// the launch line itself rather than as a second message afterwards: the harness needs
    /// a moment to start before it can receive typed input, and there is no "wait until
    /// ready" primitive to lean on, so folding them in sidesteps the race instead of racing
    /// it. See `crate::harness::HarnessConfig::launch_line`.
    pub fn spawn_harness_with(
        &mut self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
    ) -> Result<String> {
        let launch = self.prepare_launch(cwd, name, initial_message, harness)?;
        let id = self.mux.open_pane(&OpenRequest {
            cwd,
            title: name,
            launch: &launch,
            after: self.last_anchor.as_deref(),
            first_direction: &self.first_direction,
            chain_direction: &self.chain_direction,
        })?;
        self.panes.insert(name.to_string(), Pane { id: id.clone(), status: PaneStatus::Unknown });
        self.last_anchor = Some(id.clone());
        Ok(id)
    }

    /// Opens a pane for lane `name` grouped with `group_anchor` — another pane already open
    /// for a sibling branch in the same GitButler stack — instead of chaining off whatever
    /// this process last spawned. Used only by `kanstack spawn` (see `crate::cli`), the one
    /// place a stacked branch can get a pane of its own alongside a sibling's: the interactive
    /// `b` flow never does (see `crate::app::branch_modal`).
    ///
    /// A real tab alongside `group_anchor`, or a split off it in the direction orthogonal to
    /// the ordinary lane chain, depending on `KANSTACK_STACK_PANES` (see
    /// [`crate::mux::StackPlacement`]) — either way, deliberately grouped with the sibling
    /// rather than the last thing *this process* spawned, and deliberately not becoming what
    /// the next *unrelated* lane chains off: `self.last_anchor` is left untouched.
    pub fn spawn_stacked_harness_with(
        &mut self,
        cwd: &Path,
        name: &str,
        initial_message: Option<&str>,
        harness: Option<&str>,
        group_anchor: &str,
    ) -> Result<String> {
        let launch = self.prepare_launch(cwd, name, initial_message, harness)?;
        let stack_direction = crate::mux::orthogonal_direction(&self.chain_direction);
        let req = OpenRequest {
            cwd,
            title: name,
            launch: &launch,
            after: Some(group_anchor),
            first_direction: &self.first_direction,
            chain_direction: stack_direction,
        };
        let id = match crate::mux::StackPlacement::from_env() {
            crate::mux::StackPlacement::Tabbed => self.mux.open_tab(&req)?,
            crate::mux::StackPlacement::Split => self.mux.open_pane(&req)?,
        };
        self.panes.insert(name.to_string(), Pane { id: id.clone(), status: PaneStatus::Unknown });
        Ok(id)
    }

    /// The shared first half of both spawn methods: the launch line, with the harness's
    /// stale state for `name` forgotten first — a fast harness could report before we got
    /// back, and the last word of a previous pane on this branch must not outlive it — and,
    /// where process tracking applies, the pid-recording prefix folded in.
    fn prepare_launch(&mut self, cwd: &Path, name: &str, initial_message: Option<&str>, harness: Option<&str>) -> Result<String> {
        let mut launch = self.harness.launch_line(cwd, name, initial_message, harness)?;
        self.reports.forget(name);
        self.pids.forget(name);
        if self.tracks_pids() {
            // Best-effort, like the registry: a state directory that can't be made must not
            // stop the pane opening, only leave it untracked.
            if let Ok(Some(file)) = self.pids.prepare(name) {
                launch = format!("{}{launch}", record_pid_prefix(&file));
            }
        }
        Ok(launch)
    }

    /// Stands in for `ps`, so what a test reads from the process table doesn't depend on what is
    /// running.
    #[cfg(test)]
    fn set_process_table(&mut self, read: PsReader) {
        self.ps = read;
    }

    #[cfg(test)]
    fn set_process_hooks(&mut self, age: AgeReader, kill: Killer) {
        self.age = age;
        self.kill = kill;
    }

    /// Where to read what agents report about themselves. Without it, statuses come from the
    /// multiplexer alone.
    pub fn set_reports(&mut self, reports: Reports) {
        self.reports = reports;
    }

    /// Where panes' shell pids are recorded. Without it nothing is tracked by process, whatever
    /// the multiplexer asks for.
    pub fn set_pids(&mut self, pids: Pids) {
        self.pids = pids;
    }

    /// Whether panes' processes are tracked by kanstack itself: what the multiplexer asks for
    /// (see [`Multiplexer::tracks_pids`]), unless `KANSTACK_TRACK_PIDS` says otherwise.
    fn tracks_pids(&self) -> bool {
        tracking_applies(self.mux.tracks_pids(), self.track_setting.as_deref())
    }

    /// Starts tracking a pane another process opened — see `crate::workstream::Registry`.
    pub fn adopt(&mut self, branch: &str, pane_id: &str) {
        self.panes.insert(branch.to_string(), Pane { id: pane_id.to_string(), status: PaneStatus::Unknown });
    }

    /// Pins the multiplexer-level grouping new panes open in (cmux's workspace); a no-op
    /// for the others.
    pub fn set_workspace(&mut self, workspace: Option<&str>) {
        self.mux.set_scope(workspace);
    }

    /// The workspace new panes go in, if this backend has such a thing.
    pub fn workspace(&self) -> Option<String> {
        self.mux.scope()
    }

    /// Overrides the first-lane split direction. For a caller with its own setting — the
    /// `kanstack spawn` subcommand has `KANSTACK_SPAWN_DIRECTION`, since the board's `above`
    /// puts lanes over kanstack's own pane and an agent's pane is usually somewhere else.
    pub fn set_first_direction(&mut self, direction: &str) {
        self.first_direction = normalize_direction(direction);
    }

    /// Makes the next spawn split off `pane_id` instead of the caller's own pane, the way
    /// consecutive spawns within one process already chain.
    pub fn set_anchor(&mut self, pane_id: &str) {
        self.last_anchor = Some(pane_id.to_string());
    }

    pub fn pane_id(&self, branch: &str) -> Option<String> {
        self.panes.get(branch).map(|p| p.id.clone())
    }

    /// `branch`'s tracked pane, or the error every action on an untracked lane reports.
    fn pane(&self, branch: &str) -> Result<&Pane> {
        self.panes.get(branch).ok_or_else(|| anyhow!("no {} pane open for {branch} yet", self.mux.name()))
    }

    pub fn focus(&self, branch: &str) -> Result<()> {
        self.mux.focus(&self.pane(branch)?.id)
    }

    /// Closes `branch`'s pane, ending whatever harness is running in it, and stops tracking
    /// it. Already gone counts as stopped.
    pub fn stop(&mut self, branch: &str) -> Result<()> {
        let Some(pane) = self.panes.remove(branch) else {
            bail!("no {} pane open for {branch} yet", self.mux.name());
        };
        if self.last_anchor.as_deref() == Some(pane.id.as_str()) {
            self.last_anchor = None;
        }
        // What to end is worked out first: once the pane is closed and its pid forgotten there
        // is nothing left to find it by.
        let to_end = self.processes_to_end(branch);
        self.reports.forget(branch);
        self.pids.forget(branch);
        let closed = self.mux.close(&pane.id);
        // Closing a pane doesn't always end what runs in it — Ghostty drops the pane from its
        // list but leaves the process holding its terminal, measured running for eighteen
        // minutes — and a harness left running is an agent still spending. So it is ended
        // too, and even if closing the pane reported a problem.
        (self.kill)(&to_end);
        closed
    }

    /// The processes under `branch`'s pane that stopping it must end: the shell that recorded its
    /// pid and everything below it. Only when process tracking applies, and only if that pid is
    /// still the shell that wrote it, not an unrelated process that has since been given it.
    fn processes_to_end(&self, branch: &str) -> Vec<u32> {
        if !self.tracks_pids() {
            return Vec::new();
        }
        let (Some(pid), Some(written)) = (self.pids.read(branch), self.pids.written_at(branch)) else { return Vec::new() };
        let Some(age) = (self.age)(pid) else { return Vec::new() };
        if !started_before(age, SystemTime::now(), written) {
            return Vec::new();
        }
        match (self.ps)() {
            Ok(table) => subtree(pid, &table),
            Err(_) => Vec::new(),
        }
    }

    /// Sends `text` followed by Enter into `branch`'s tracked pane — the same
    /// literal-then-Enter sequence spawning uses to type the launch command, just aimed at a
    /// pane recorded earlier.
    pub fn send_task(&self, branch: &str, text: &str) -> Result<()> {
        self.mux.type_line(&self.pane(branch)?.id, text).map_err(|e| anyhow!("{branch}: {e:#}"))
    }

    /// Re-derives the status of every tracked pane, keyed by branch, from what the multiplexer
    /// (and, for a pane whose process kanstack tracks, the process table — see
    /// [`Self::native_readings`]) says about the pane, and what the agent in it last said about
    /// itself (see `crate::report`).
    ///
    /// A pane that is gone is `Dead`, whatever the agent last said — a crashed one can't have
    /// said it stopped. Otherwise a fresh report wins: it comes from the harness itself, where
    /// the other two only infer from CPU, or from nothing at all. Otherwise the reading stands.
    /// A pane with neither is left out, so [`Self::apply_statuses`] keeps what was known.
    ///
    /// Read-only — safe to call from a background thread against a cloned snapshot.
    pub fn poll_statuses(&self) -> Result<HashMap<String, PaneStatus>> {
        self.poll_statuses_at(SystemTime::now())
    }

    fn poll_statuses_at(&self, now: SystemTime) -> Result<HashMap<String, PaneStatus>> {
        if self.panes.is_empty() {
            return Ok(HashMap::new());
        }
        let panes: Vec<(&str, &str)> = self.panes.iter().map(|(branch, pane)| (branch.as_str(), pane.id.as_str())).collect();
        let native = match self.native_readings(&panes) {
            Ok(native) => native,
            // The multiplexer couldn't be read — no server, a permission refused — but what
            // agents said about themselves is still good. Only whether a pane is gone is
            // unknowable, so that is all that is lost. The error surfaces only when there is
            // nothing at all to report instead.
            Err(err) => {
                let reported = self.reported_statuses(now);
                return if reported.is_empty() { Err(err) } else { Ok(reported) };
            }
        };
        let mut statuses: HashMap<String, PaneStatus> = HashMap::new();
        let mut suspects: Vec<(&str, &str, u64)> = Vec::new(); // (branch, pane id, report second)
        for (branch, pane) in &self.panes {
            let native = native.get(branch).copied();
            let said = self.reports.said(branch, now);
            if let Some(at) = stale_busy_suspect(native, said) {
                suspects.push((branch, &pane.id, at));
            }
            if let Some(status) = merge(native, said) {
                statuses.insert(branch.clone(), status);
            }
        }
        if !suspects.is_empty() {
            for (branch, at) in self.confirmed_quiet(suspects) {
                if self.reports.retract_busy(branch, at, now) {
                    statuses.insert(branch.to_string(), PaneStatus::Idle);
                }
            }
        }
        Ok(statuses)
    }

    /// What can be seen of each of `panes` (branch, pane id) from outside the agent, keyed by
    /// branch — the `native` reading [`merge`] takes. The one place it is derived, so the poll
    /// and the looks that confirm a quiet pane (see [`Self::confirmed_quiet`]) can't disagree
    /// about what counts.
    ///
    /// Two tiers. The multiplexer's own probe comes first: a pane it says is `Dead`, `Busy` or
    /// `Idle` is that. Where it has no reading — Ghostty can only say a pane exists — and the
    /// pane's process is tracked (see [`Self::tracks_pids`]) and its shell has recorded its pid,
    /// the process table answers instead: [`reading_from_pid`]. A tracked pane without a pid
    /// yet has no reading at all, and is left out. The table is read at most once per call, and
    /// not at all when no pane needs it. It can fail without failing the call: those panes
    /// simply have no reading, which the callers already treat as "no news".
    ///
    /// Fails only if the multiplexer can't be probed.
    fn native_readings(&self, panes: &[(&str, &str)]) -> Result<HashMap<String, PaneStatus>> {
        let ids: Vec<&str> = panes.iter().map(|(_, id)| *id).collect();
        let probed = self.mux.probe(&ids)?;
        let mut table = ProcessTable { read: &self.ps, rows: None };
        let mut readings = HashMap::new();
        for (branch, id) in panes {
            let reading = match probed.get(*id).copied() {
                Some(status @ (PaneStatus::Dead | PaneStatus::Busy | PaneStatus::Idle)) => Some(status),
                other => self.reading_from_recorded_pid(branch, &mut table).or(other),
            };
            if let Some(status) = reading {
                readings.insert(branch.to_string(), status);
            }
        }
        Ok(readings)
    }

    /// The process table's reading of `branch`'s pane: `None` unless its process is tracked and
    /// its shell has recorded a pid. `table` is only read once there is a pid to look up.
    fn reading_from_recorded_pid(&self, branch: &str, table: &mut ProcessTable<'_>) -> Option<PaneStatus> {
        if !self.tracks_pids() {
            return None;
        }
        let pid = self.pids.read(branch)?;
        Some(reading_from_pid(pid, table.rows()?))
    }

    /// Of `suspects` — panes whose `busy` report a quiet reading casts doubt on — the ones
    /// that are still quiet on every one of [`QUIET_CONFIRMATIONS`] further looks, spaced
    /// [`QUIET_GAP`] apart. A pane that shows any activity, or can't be read, keeps its report.
    /// Each look is a full [`Self::native_readings`], so a pane whose only reading is from the
    /// process table is confirmed by it, with a fresh read of the table each time.
    fn confirmed_quiet<'a>(&self, mut suspects: Vec<(&'a str, &str, u64)>) -> Vec<(&'a str, u64)> {
        for _ in 0..QUIET_CONFIRMATIONS {
            std::thread::sleep(self.quiet_gap);
            let panes: Vec<(&str, &str)> = suspects.iter().map(|(branch, id, _)| (*branch, *id)).collect();
            let Ok(seen) = self.native_readings(&panes) else { return Vec::new() };
            suspects.retain(|(branch, _, _)| seen.get(*branch) == Some(&PaneStatus::Idle));
            if suspects.is_empty() {
                break;
            }
        }
        suspects.into_iter().map(|(branch, _, at)| (branch, at)).collect()
    }

    /// Every lane whose agent has said something recent about itself, keyed by branch.
    fn reported_statuses(&self, now: SystemTime) -> HashMap<String, PaneStatus> {
        self.panes.keys().filter_map(|branch| Some((branch.clone(), self.reports.status(branch, now)?))).collect()
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `body` with each of `vars` swapped out and restored afterwards — same pattern
    /// as `cmux.rs`'s and `tmux.rs`'s own `with_env` helpers, just handling several vars at
    /// once since `discover` here spans both backends' env surface. Held for the whole
    /// call via `SPLIT_BACKEND_ENV_LOCK`, not just the swap, so this can't interleave with
    /// either of those other two files' own env-mutating tests.
    ///
    /// `TERM_PROGRAM` is cleared unless a test sets it, so these don't depend on whether the
    /// developer is running `cargo test` inside Ghostty.
    fn with_env(vars: &[(&str, Option<&str>)], body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut vars = vars.to_vec();
        if !vars.iter().any(|(k, _)| *k == "TERM_PROGRAM") {
            vars.push(("TERM_PROGRAM", None));
        }
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

    /// Both backends "found" (via trusted explicit-bin overrides, so no real `cmux`/`tmux`
    /// binary is needed) and no `KANSTACK_SPLIT_BACKEND` set: cmux wins, matching
    /// `discover`'s documented default order.
    #[test]
    fn discover_prefers_cmux_by_default_when_both_qualify() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", Some("%3")),
                ("ORCA_TERMINAL_HANDLE", None),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux"));
            },
        );
    }

    /// `KANSTACK_SPLIT_BACKEND=tmux` skips cmux detection entirely, even though cmux would
    /// otherwise qualify — the whole point of the override.
    #[test]
    fn discover_honors_an_explicit_tmux_override_over_a_qualifying_cmux() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", Some("%3")),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("tmux"));
            },
        );
    }

    /// `KANSTACK_SPLIT_BACKEND=cmux` does not fall through to tmux when cmux itself isn't
    /// actually usable — the override pins the backend, it doesn't just reorder the
    /// fallback.
    #[test]
    fn discover_honors_an_explicit_cmux_override_without_falling_back_to_tmux() {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("cmux")),
                ("PATH", Some(dir.to_str().unwrap())),
                ("KANSTACK_CMUX_BIN", None),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", Some("%3")),
            ],
            || {
                assert!(
                    Splitter::discover().is_none(),
                    "cmux was pinned but isn't usable, so this must not fall back to tmux"
                );
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unrecognized `KANSTACK_SPLIT_BACKEND` value falls back to plain auto-detection
    /// rather than treating it as "neither backend," same as `KANSTACK_BRANCH_UI` ignores
    /// a value it doesn't recognize.
    #[test]
    fn discover_falls_back_to_auto_detect_on_an_unrecognized_override_value() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("bogus")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("KANSTACK_TMUX_BIN", None),
                ("TMUX_PANE", None),
                ("ORCA_TERMINAL_HANDLE", None),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux"));
            },
        );
    }

    /// `KANSTACK_SPLIT_BACKEND=orca` pins Orca, and — like the other two pins — doesn't fall
    /// through to a backend that would otherwise qualify when Orca itself isn't usable.
    #[test]
    fn discover_honors_an_explicit_orca_override_without_falling_back() {
        let usable = [
            ("KANSTACK_SPLIT_BACKEND", Some("orca")),
            ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
            ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
            ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
            ("TMUX_PANE", Some("%3")),
        ];
        let mut with_handle = usable.to_vec();
        with_handle.push(("ORCA_TERMINAL_HANDLE", Some("term_1")));
        with_env(&with_handle, || {
            assert_eq!(Splitter::discover().map(|s| s.label()), Some("orca"));
        });

        let mut without_handle = usable.to_vec();
        without_handle.push(("ORCA_TERMINAL_HANDLE", None));
        with_env(&without_handle, || {
            assert!(
                Splitter::discover().is_none(),
                "orca was pinned but this isn't an Orca terminal, so this must not fall back to cmux or tmux"
            );
        });
    }

    /// Inside an Orca terminal with nothing saying kanstack is also inside a cmux or tmux
    /// pane, Orca goes ahead of cmux's bare "is the binary on PATH" check — the whole
    /// cmux-installed-but-not-in-use ambiguity, with a positive signal to settle it.
    #[test]
    fn discover_prefers_orca_over_a_merely_installed_cmux_inside_an_orca_terminal() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
                ("ORCA_TERMINAL_HANDLE", Some("term_1")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("CMUX_SURFACE_ID", None),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", None),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("orca"));
            },
        );
    }

    /// The same Orca terminal, but kanstack is in a cmux pane or a tmux pane within it: the
    /// positive signal points elsewhere, so the default order stands.
    #[test]
    fn discover_leaves_the_default_order_alone_inside_a_cmux_or_tmux_pane_in_orca() {
        let base = [
            ("KANSTACK_SPLIT_BACKEND", None),
            ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
            ("ORCA_TERMINAL_HANDLE", Some("term_1")),
            ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
            ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
        ];
        let mut in_cmux = base.to_vec();
        in_cmux.extend([("CMUX_SURFACE_ID", Some("ABC")), ("TMUX_PANE", None)]);
        with_env(&in_cmux, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux")));

        let mut in_tmux = base.to_vec();
        in_tmux.extend([("CMUX_SURFACE_ID", None), ("TMUX_PANE", Some("%3"))]);
        with_env(&in_tmux, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux")));
    }

    /// Orca is last in the chain: a tmux pane whose `tmux` binary can't be found (no cmux
    /// either) falls through to Orca rather than to nothing.
    #[test]
    fn discover_falls_through_to_orca_when_cmux_and_tmux_are_both_unusable() {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-orca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("PATH", Some(dir.to_str().unwrap())),
                ("KANSTACK_CMUX_BIN", None),
                ("KANSTACK_TMUX_BIN", None),
                ("TMUX_PANE", Some("%3")),
                ("CMUX_SURFACE_ID", None),
                ("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")),
                ("ORCA_TERMINAL_HANDLE", Some("term_1")),
            ],
            || {
                assert_eq!(Splitter::discover().map(|s| s.label()), Some("orca"));
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `KANSTACK_SPLIT_BACKEND=ghostty` pins Ghostty, and like the other pins doesn't fall
    /// through to a backend that would otherwise qualify when Ghostty itself isn't usable.
    /// (Off macOS there is no Ghostty backend at all, so the positive half is macOS only.)
    #[test]
    fn discover_honors_an_explicit_ghostty_override_without_falling_back() {
        let usable = [
            ("KANSTACK_SPLIT_BACKEND", Some("ghostty")),
            ("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/not-osascript")),
            ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
            ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
            ("TMUX_PANE", None),
            ("CMUX_SURFACE_ID", None),
        ];
        if cfg!(target_os = "macos") {
            let mut inside = usable.to_vec();
            inside.push(("TERM_PROGRAM", Some("ghostty")));
            with_env(&inside, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("ghostty")));
        }
        let mut elsewhere = usable.to_vec();
        elsewhere.push(("TERM_PROGRAM", Some("Apple_Terminal")));
        with_env(&elsewhere, || {
            assert!(
                Splitter::discover().is_none(),
                "ghostty was pinned but this isn't a Ghostty window, so this must not fall back to cmux"
            );
        });
    }

    /// Inside plain Ghostty (`TERM_PROGRAM=ghostty`, not a cmux or tmux pane) a merely
    /// installed cmux must not win: Ghostty is tried ahead of cmux's bare `PATH` check.
    #[cfg(target_os = "macos")]
    #[test]
    fn discover_prefers_ghostty_over_a_merely_installed_cmux_inside_ghostty() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", None),
                ("TERM_PROGRAM", Some("ghostty")),
                ("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/not-osascript")),
                ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
                ("CMUX_SURFACE_ID", None),
                ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
                ("TMUX_PANE", None),
                ("ORCA_TERMINAL_HANDLE", None),
            ],
            || assert_eq!(Splitter::discover().map(|s| s.label()), Some("ghostty")),
        );
    }

    /// cmux is a Ghostty fork and sets `TERM_PROGRAM=ghostty` too, and tmux in Ghostty has a
    /// pane of its own: with `$CMUX_SURFACE_ID` or `$TMUX_PANE` set the default order stands.
    #[test]
    fn discover_leaves_the_default_order_alone_inside_a_cmux_or_tmux_pane_in_ghostty() {
        let base = [
            ("KANSTACK_SPLIT_BACKEND", None),
            ("TERM_PROGRAM", Some("ghostty")),
            ("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/not-osascript")),
            ("KANSTACK_CMUX_BIN", Some("/nonexistent/not-cmux")),
            ("KANSTACK_TMUX_BIN", Some("/nonexistent/not-tmux")),
            ("ORCA_TERMINAL_HANDLE", None),
        ];
        let mut in_cmux = base.to_vec();
        in_cmux.extend([("CMUX_SURFACE_ID", Some("ABC")), ("TMUX_PANE", None)]);
        with_env(&in_cmux, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux")));

        let mut in_tmux = base.to_vec();
        in_tmux.extend([("CMUX_SURFACE_ID", None), ("TMUX_PANE", Some("%3"))]);
        with_env(&in_tmux, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("cmux")));
    }

    /// Ghostty is last in the chain: in Ghostty with nothing else usable (no cmux on `PATH`,
    /// no tmux pane) it is what is found, and outside Ghostty nothing is.
    #[cfg(target_os = "macos")]
    #[test]
    fn discover_finds_ghostty_last_and_only_inside_ghostty() {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-ghostty-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = [
            ("KANSTACK_SPLIT_BACKEND", None),
            ("PATH", Some(dir.to_str().unwrap())),
            ("KANSTACK_CMUX_BIN", None),
            ("KANSTACK_TMUX_BIN", None),
            ("TMUX_PANE", None),
            ("CMUX_SURFACE_ID", None),
            ("ORCA_TERMINAL_HANDLE", None),
            ("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/not-osascript")),
        ];
        let mut inside = base.to_vec();
        inside.push(("TERM_PROGRAM", Some("ghostty")));
        with_env(&inside, || assert_eq!(Splitter::discover().map(|s| s.label()), Some("ghostty")));

        let mut outside = base.to_vec();
        outside.push(("TERM_PROGRAM", Some("Apple_Terminal")));
        with_env(&outside, || assert!(Splitter::discover().is_none()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Ghostty backend is macOS only: `discover` is `None` anywhere else, whatever the
    /// environment says.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn discover_never_finds_ghostty_off_macos() {
        with_env(
            &[
                ("KANSTACK_SPLIT_BACKEND", Some("ghostty")),
                ("TERM_PROGRAM", Some("ghostty")),
                ("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/not-osascript")),
            ],
            || assert!(Splitter::discover().is_none()),
        );
    }

    /// `running_inside_host` gates the board's background poll, and for Orca it is read off
    /// `$ORCA_TERMINAL_HANDLE` at the moment of asking, not remembered from `discover`.
    #[test]
    fn an_orca_splitter_names_itself_and_asks_the_environment_whether_to_poll() {
        with_env(
            &[("KANSTACK_ORCA_BIN", Some("/nonexistent/not-orca")), ("ORCA_TERMINAL_HANDLE", Some("term_1"))],
            || {
                let splitter = Splitter::new(Arc::new(Orca::discover().unwrap()), HarnessConfig::new("claude"));
                assert_eq!(splitter.label(), "orca");
                assert!(splitter.running_inside_host());
                assert_eq!(splitter.workspace(), None, "there is no workspace to pin");

                std::env::remove_var("ORCA_TERMINAL_HANDLE");
                assert!(!splitter.running_inside_host());
            },
        );
    }

    // Everything below runs against `FakeMux`, so what it pins is the layer every backend
    // shares — which pane belongs to which lane, where the next one goes, what a pane's last
    // known status was — once, rather than once per backend.

    use crate::mux::fake::FakeMux;

    fn fake_splitter() -> (Splitter, Arc<FakeMux>) {
        let mux = FakeMux::new();
        (Splitter::new(mux.clone(), HarnessConfig::new("claude")), mux)
    }

    fn cwd() -> &'static Path {
        Path::new("/repo")
    }

    #[test]
    fn the_first_lane_splits_kanstacks_own_pane_and_each_later_one_chains_off_the_last() {
        let (mut splitter, mux) = fake_splitter();
        assert_eq!(splitter.spawn_harness(cwd(), "feat-a", None).unwrap(), "p1");
        assert_eq!(splitter.spawn_harness(cwd(), "feat-b", None).unwrap(), "p2");
        assert_eq!(splitter.spawn_harness(cwd(), "feat-c", None).unwrap(), "p3");
        let lines = mux.lines();
        assert!(lines[0].starts_with("open p1 up of own in /repo as feat-a: cd '/repo' && claude"), "{lines:#?}");
        assert!(lines[1].starts_with("open p2 right of p1 in /repo as feat-b:"), "{lines:#?}");
        assert!(lines[2].starts_with("open p3 right of p2 in /repo as feat-c:"), "{lines:#?}");
    }

    #[test]
    fn a_spawned_lane_is_tracked_with_an_unknown_status_until_polled() {
        let (mut splitter, _) = fake_splitter();
        assert!(splitter.is_empty());
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(!splitter.is_empty());
        assert!(splitter.has_pane("feat-a"));
        assert_eq!(splitter.pane_id("feat-a").as_deref(), Some("p1"));
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Unknown));
        assert_eq!(splitter.pane_status("never-spawned"), None);
    }

    /// A launch that failed must not leave a phantom lane, or move the anchor the next
    /// lane splits off.
    #[test]
    fn a_failed_open_tracks_nothing_and_leaves_the_anchor_alone() {
        let (mut splitter, mux) = fake_splitter();
        *mux.fail_open.lock().unwrap() = true;
        assert!(splitter.spawn_harness(cwd(), "feat-a", None).is_err());
        assert!(!splitter.has_pane("feat-a"));
        *mux.fail_open.lock().unwrap() = false;
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(mux.lines()[0].starts_with("open p1 up of own"), "{:#?}", mux.lines());
    }

    #[test]
    fn stopping_the_newest_lane_sends_the_next_one_back_to_kanstacks_own_pane() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.stop("feat-a").unwrap();
        assert!(!splitter.has_pane("feat-a"));
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        let lines = mux.lines();
        assert_eq!(lines[1], "close p1", "{lines:#?}");
        assert!(lines[2].starts_with("open p2 up of own"), "the next lane must not split a closed pane: {lines:#?}");
    }

    #[test]
    fn stopping_an_older_lane_keeps_chaining_off_the_newest() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        splitter.stop("feat-a").unwrap();
        splitter.spawn_harness(cwd(), "feat-c", None).unwrap();
        assert!(mux.lines().last().unwrap().starts_with("open p3 right of p2"), "{:#?}", mux.lines());
    }

    #[test]
    fn acting_on_a_lane_with_no_pane_says_which_backend_and_which_lane() {
        let (mut splitter, mux) = fake_splitter();
        for err in [
            splitter.focus("nope").unwrap_err(),
            splitter.send_task("nope", "x").unwrap_err(),
            splitter.stop("nope").unwrap_err(),
        ] {
            assert_eq!(err.to_string(), "no fake pane open for nope yet");
        }
        assert!(mux.lines().is_empty(), "nothing should reach the multiplexer: {:#?}", mux.lines());
    }

    /// The subcommands run in a process that never opened the pane, so they adopt one from
    /// the registry and then act on it exactly as if they had.
    #[test]
    fn an_adopted_pane_can_be_focused_typed_into_and_stopped() {
        let (mut splitter, mux) = fake_splitter();
        splitter.adopt("feat-a", "p9");
        splitter.focus("feat-a").unwrap();
        splitter.send_task("feat-a", "run the tests").unwrap();
        splitter.stop("feat-a").unwrap();
        assert_eq!(mux.lines(), ["focus p9", "type p9: run the tests", "close p9"]);
    }

    #[test]
    fn set_anchor_makes_the_next_lane_split_that_pane_in_the_chain_direction() {
        let (mut splitter, mux) = fake_splitter();
        splitter.set_anchor("p42");
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(mux.lines()[0].starts_with("open p1 right of p42"), "{:#?}", mux.lines());
    }

    #[test]
    fn a_first_direction_override_is_normalized() {
        let (mut splitter, mux) = fake_splitter();
        splitter.set_first_direction("below");
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(mux.lines()[0].starts_with("open p1 down of own"), "{:#?}", mux.lines());
    }

    #[test]
    fn a_harness_override_changes_only_that_lanes_launch_line() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness_with(cwd(), "feat-a", Some("go"), Some("codex")).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", Some("go")).unwrap();
        let lines = mux.lines();
        assert!(lines[0].contains("&& codex "), "{lines:#?}");
        assert!(lines[1].contains("&& claude "), "{lines:#?}");
    }

    /// The default (`KANSTACK_STACK_PANES` unset): a real tab alongside the sibling, in the
    /// direction orthogonal to the ordinary chain — and, unlike `spawn_harness`, it must not
    /// move `last_anchor`: the next *unrelated* lane must still chain off what this process
    /// last spawned for itself, not off a sibling's pane borrowed for grouping.
    #[test]
    fn a_stacked_spawn_defaults_to_a_tab_orthogonal_to_the_chain_and_leaves_the_anchor_alone() {
        with_env(&[("KANSTACK_STACK_PANES", None)], || {
            let (mut splitter, mux) = fake_splitter();
            splitter.spawn_harness(cwd(), "feat-base", None).unwrap();
            let tab = splitter.spawn_stacked_harness_with(cwd(), "feat-top", None, None, "p1").unwrap();
            assert_eq!(tab, "p2");
            assert!(mux.lines()[1].starts_with("tab p2 alongside p1 in /repo as feat-top:"), "{:#?}", mux.lines());

            // The default chain direction is `right` (horizontal), so the orthogonal stack
            // split direction would be `down` — but this asserts the tab path was taken at
            // all, which `set_anchor` below is what actually exercises the direction with.
            splitter.spawn_harness(cwd(), "feat-c", None).unwrap();
            assert!(mux.lines()[2].starts_with("open p3 right of p1"), "unrelated lanes must still chain off the last real spawn, not the borrowed tab anchor: {:#?}", mux.lines());
        });
    }

    /// `KANSTACK_STACK_PANES=split` splits off the sibling instead of tabbing, in the
    /// direction orthogonal to the ordinary chain direction.
    #[test]
    fn a_stacked_spawn_can_be_configured_to_split_orthogonally_instead_of_tabbing() {
        with_env(&[("KANSTACK_STACK_PANES", Some("split"))], || {
            let (mut splitter, mux) = fake_splitter();
            splitter.spawn_harness(cwd(), "feat-base", None).unwrap();
            splitter.spawn_stacked_harness_with(cwd(), "feat-top", None, None, "p1").unwrap();
            // Default chain direction is `right` (horizontal) — orthogonal is `down`.
            assert!(mux.lines()[1].starts_with("open p2 down of p1"), "{:#?}", mux.lines());
        });
    }

    /// A vertical chain direction (`up`, kanstack's own default first-lane direction) flips
    /// the orthogonal stack split to horizontal (`right`).
    #[test]
    fn the_orthogonal_split_direction_follows_the_chain_directions_axis() {
        with_env(&[("KANSTACK_STACK_PANES", Some("split")), ("KANSTACK_FAKE_CHAIN_DIRECTION", Some("up"))], || {
            let (mut splitter, mux) = fake_splitter();
            splitter.spawn_harness(cwd(), "feat-base", None).unwrap();
            splitter.spawn_stacked_harness_with(cwd(), "feat-top", None, None, "p1").unwrap();
            assert!(mux.lines()[1].starts_with("open p2 right of p1"), "{:#?}", mux.lines());
        });
    }

    /// Statuses come back keyed by branch, and a pane the backend had no news about is left
    /// out — so applying them keeps what was already known instead of overwriting it.
    #[test]
    fn polling_reports_by_branch_and_no_news_keeps_the_last_known_status() {
        let (mut splitter, mux) = fake_splitter();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();

        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Busy);
        let polled = splitter.poll_statuses().unwrap();
        assert_eq!(polled, HashMap::from([("feat-a".to_string(), PaneStatus::Busy)]));
        splitter.apply_statuses(polled);
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Busy));
        assert_eq!(splitter.pane_status("feat-b"), Some(PaneStatus::Unknown));

        mux.statuses.lock().unwrap().clear();
        let polled = splitter.poll_statuses().unwrap();
        assert!(polled.is_empty());
        splitter.apply_statuses(polled);
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Busy), "no news must not erase it");

        mux.statuses.lock().unwrap().insert("p2".to_string(), PaneStatus::Dead);
        splitter.apply_statuses(splitter.poll_statuses().unwrap());
        assert_eq!(splitter.pane_status("feat-b"), Some(PaneStatus::Dead));
    }

    /// A lane deleted while a poll was in flight is simply not there to update.
    #[test]
    fn applying_statuses_for_a_lane_that_has_since_gone_is_a_no_op() {
        let (mut splitter, _) = fake_splitter();
        splitter.apply_statuses(HashMap::from([("gone".to_string(), PaneStatus::Busy)]));
        assert!(!splitter.has_pane("gone"));
    }

    #[test]
    fn polling_with_no_panes_never_asks_the_multiplexer() {
        let (splitter, mux) = fake_splitter();
        assert!(splitter.poll_statuses().unwrap().is_empty());
        assert!(mux.lines().is_empty());
    }

    #[test]
    fn the_workspace_is_the_multiplexers_scope() {
        let (mut splitter, _) = fake_splitter();
        assert_eq!(splitter.workspace(), None);
        splitter.set_workspace(Some("workspace:2"));
        assert_eq!(splitter.workspace().as_deref(), Some("workspace:2"));
        assert_eq!(splitter.label(), "fake");
        assert!(splitter.running_inside_host());
    }

    // What an agent says about itself, against what the multiplexer infers.

    use crate::report::{Reported, Reports, FRESH_FOR};
    use std::time::{Duration, SystemTime};

    /// A splitter with one lane, `feat-a`, in pane `p1`, reading reports from a fresh
    /// directory.
    fn splitter_with_reports(tag: &str) -> (Splitter, Arc<FakeMux>, Reports) {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-reports-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reports = Reports::in_dir(dir);
        let (mut splitter, mux) = fake_splitter();
        splitter.set_reports(reports.clone());
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        (splitter, mux, reports)
    }

    fn polled(splitter: &Splitter, now: SystemTime) -> Option<PaneStatus> {
        splitter.poll_statuses_at(now).unwrap().get("feat-a").copied()
    }

    /// The multiplexer only infers from CPU; the harness knows. So where both speak, the
    /// harness is believed.
    #[test]
    fn a_fresh_report_wins_over_the_multiplexers_reading() {
        let (splitter, mux, reports) = splitter_with_reports("wins");
        let now = SystemTime::now();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        reports.write("feat-a", Reported::Busy, now).unwrap();
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Busy));
    }

    /// A multiplexer with no idea whether its panes are busy — Ghostty's scripting can't
    /// say — is where reports matter most.
    #[test]
    fn a_report_is_the_whole_answer_when_the_multiplexer_has_no_reading() {
        let (splitter, _mux, reports) = splitter_with_reports("alone");
        let now = SystemTime::now();
        assert_eq!(polled(&splitter, now), None, "nobody has said anything yet");
        reports.write("feat-a", Reported::Idle, now).unwrap();
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Idle));
    }

    /// A crashed harness never reports that it stopped.
    #[test]
    fn a_closed_pane_is_dead_whatever_the_agent_last_said() {
        let (splitter, mux, reports) = splitter_with_reports("dead");
        let now = SystemTime::now();
        reports.write("feat-a", Reported::Busy, now).unwrap();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Dead);
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Dead));
    }

    #[test]
    fn a_stale_report_gives_way_to_the_multiplexers_reading() {
        let (splitter, mux, reports) = splitter_with_reports("stale");
        let then = SystemTime::now();
        reports.write("feat-a", Reported::Busy, then).unwrap();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        let later = then + FRESH_FOR + Duration::from_secs(1);
        assert_eq!(polled(&splitter, later), Some(PaneStatus::Idle));
        mux.statuses.lock().unwrap().clear();
        assert_eq!(
            polled(&splitter, later),
            Some(PaneStatus::Unknown),
            "stale with nothing else to go on is unknown, not left as whatever was last applied"
        );
    }

    #[test]
    fn a_new_pane_on_a_branch_does_not_inherit_the_old_ones_last_word() {
        let (mut splitter, _mux, reports) = splitter_with_reports("respawn");
        let now = SystemTime::now();
        reports.write("feat-a", Reported::Busy, now).unwrap();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert_eq!(polled(&splitter, now), None);
    }

    #[test]
    fn stopping_a_lane_forgets_what_its_agent_said() {
        let (mut splitter, _mux, reports) = splitter_with_reports("stop");
        let now = SystemTime::now();
        reports.write("feat-a", Reported::Busy, now).unwrap();
        splitter.stop("feat-a").unwrap();
        assert_eq!(reports.status("feat-a", now), None);
    }

    /// If the multiplexer can't be read, an agent's own word is still the best there is —
    /// and a permission the user refused once shouldn't blank every status.
    #[test]
    fn a_failed_probe_still_yields_what_agents_reported() {
        let (splitter, mux, reports) = splitter_with_reports("probe-fails");
        let now = SystemTime::now();
        *mux.fail_probe.lock().unwrap() = true;
        reports.write("feat-a", Reported::Busy, now).unwrap();
        assert_eq!(polled(&splitter, now), Some(PaneStatus::Busy));
    }

    #[test]
    fn a_failed_probe_with_nothing_reported_is_still_an_error() {
        let (splitter, mux, _reports) = splitter_with_reports("probe-fails-silent");
        *mux.fail_probe.lock().unwrap() = true;
        let err = splitter.poll_statuses_at(SystemTime::now()).unwrap_err().to_string();
        assert!(err.contains("fake probe failed"), "{err}");
    }

    /// A pane blocked on a prompt burns no CPU, so the multiplexer reads it as idle — or, with
    /// a stale reading, busy. Only the agent knows it is waiting on the user.
    #[test]
    fn a_fresh_waiting_report_wins_over_either_multiplexer_reading() {
        for native in [PaneStatus::Busy, PaneStatus::Idle] {
            let (splitter, mux, reports) = splitter_with_reports("waiting");
            let now = SystemTime::now();
            mux.statuses.lock().unwrap().insert("p1".to_string(), native);
            reports.write("feat-a", Reported::Waiting, now).unwrap();
            assert_eq!(polled(&splitter, now), Some(PaneStatus::Waiting), "over {native:?}");
        }
    }

    /// Nothing else can retract a `busy` an agent never took back — a turn interrupted with
    /// Escape fires no hook — so once it runs out the pane must stop claiming it.
    #[test]
    fn an_expired_report_reads_unknown_rather_than_leaving_the_last_status_standing() {
        let (mut splitter, mux, reports) = splitter_with_reports("expired");
        let then = SystemTime::now();
        reports.write("feat-a", Reported::Busy, then).unwrap();
        let fresh = splitter.poll_statuses_at(then).unwrap();
        splitter.apply_statuses(fresh);
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Busy));

        let later = then + FRESH_FOR + Duration::from_secs(1);
        splitter.apply_statuses(splitter.poll_statuses_at(later).unwrap());
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Unknown));

        // ...unless the multiplexer can say.
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        splitter.apply_statuses(splitter.poll_statuses_at(later).unwrap());
        assert_eq!(splitter.pane_status("feat-a"), Some(PaneStatus::Idle));
    }

    // The merge rule as a table: what the multiplexer sees, what the agent said, and the status
    // that results. `at` is irrelevant to it.

    fn fresh(status: PaneStatus, secs: u64) -> Option<Said> {
        Some(Said::Fresh { status, age: Duration::from_secs(secs), at: 1000 })
    }

    #[test]
    fn merge_believes_a_fresh_report_over_the_multiplexer_whatever_its_age() {
        use PaneStatus::*;
        assert_eq!(merge(Some(Idle), fresh(Busy, 1)), Some(Busy));
        assert_eq!(merge(Some(Busy), fresh(Idle, 1)), Some(Idle));
        assert_eq!(merge(Some(Idle), fresh(Waiting, 1)), Some(Waiting));
        assert_eq!(merge(None, fresh(Busy, 1)), Some(Busy), "a multiplexer with no reading at all");
        assert_eq!(merge(None, fresh(Busy, 3000)), Some(Busy), "with nothing to contradict it, it stands");
        // One CPU reading never overrides a report: a pane at a prompt for a minute blips busy,
        // and a real turn blips idle.
        assert_eq!(merge(Some(Idle), fresh(Busy, 500)), Some(Busy));
        assert_eq!(merge(Some(Busy), fresh(Waiting, 500)), Some(Waiting));
        assert_eq!(merge(Some(Idle), fresh(Waiting, 100_000)), Some(Waiting));
    }

    #[test]
    fn merge_reports_a_closed_pane_dead_whatever_was_said() {
        use PaneStatus::*;
        for said in [fresh(Busy, 1), fresh(Waiting, 1), fresh(Idle, 1), Some(Said::Stale)] {
            assert_eq!(merge(Some(Dead), said), Some(Dead));
        }
    }

    #[test]
    fn merge_reads_a_stale_report_as_unknown_unless_the_multiplexer_knows() {
        use PaneStatus::*;
        assert_eq!(merge(None, Some(Said::Stale)), Some(Unknown));
        assert_eq!(merge(Some(Idle), Some(Said::Stale)), Some(Idle));
    }

    #[test]
    fn merge_with_nothing_said_is_the_multiplexers_reading_or_no_news() {
        assert_eq!(merge(Some(PaneStatus::Busy), None), Some(PaneStatus::Busy));
        assert_eq!(merge(None, None), None);
    }

    /// The one contradiction that is acted on: an old busy report over a quiet pane.
    #[test]
    fn only_an_old_busy_report_over_an_idle_pane_is_a_suspect() {
        use PaneStatus::*;
        assert_eq!(stale_busy_suspect(Some(Idle), fresh(Busy, 20)), Some(1000));
        assert_eq!(stale_busy_suspect(Some(Idle), fresh(Busy, 19)), None, "the multiplexer's reading lags");
        assert_eq!(stale_busy_suspect(Some(Busy), fresh(Busy, 500)), None, "they agree");
        assert_eq!(stale_busy_suspect(None, fresh(Busy, 500)), None, "nothing to contradict it with");
        assert_eq!(stale_busy_suspect(Some(Idle), fresh(Waiting, 500)), None, "a prompt and rest look alike");
        assert_eq!(stale_busy_suspect(Some(Busy), fresh(Waiting, 500)), None, "and busy never retracts waiting");
        assert_eq!(stale_busy_suspect(Some(Idle), fresh(Idle, 500)), None);
        assert_eq!(stale_busy_suspect(Some(Idle), Some(Said::Stale)), None);
        assert_eq!(stale_busy_suspect(Some(Dead), fresh(Busy, 500)), None);
    }

    // Confirming that a quiet pane really is quiet, against scripted probes.

    fn idle_at(pane: &str) -> HashMap<String, PaneStatus> {
        HashMap::from([(pane.to_string(), PaneStatus::Idle)])
    }

    fn busy_at(pane: &str) -> HashMap<String, PaneStatus> {
        HashMap::from([(pane.to_string(), PaneStatus::Busy)])
    }

    /// A splitter with one lane whose agent said `busy` a minute ago and whose pane reads idle.
    fn quiet_pane_with_an_old_busy_report(tag: &str) -> (Splitter, Arc<FakeMux>, Reports, SystemTime) {
        let (mut splitter, mux, reports) = splitter_with_reports(tag);
        splitter.quiet_gap = Duration::ZERO;
        let said = SystemTime::now();
        reports.write("feat-a", Reported::Busy, said).unwrap();
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        (splitter, mux, reports, said + Duration::from_secs(60))
    }

    /// An interrupted turn: the report says busy, nothing ever takes it back, and the pane stays
    /// quiet. It is corrected, and the correction is written down so it sticks.
    #[test]
    fn a_pane_that_stays_quiet_retracts_an_old_busy_report_and_it_stays_retracted() {
        let (splitter, mux, reports, later) = quiet_pane_with_an_old_busy_report("retract");
        assert_eq!(polled(&splitter, later), Some(PaneStatus::Idle));
        assert_eq!(*mux.probes.lock().unwrap(), 1 + QUIET_CONFIRMATIONS as u32, "one look, then each confirmation");
        assert_eq!(reports.status("feat-a", later), Some(PaneStatus::Idle), "written back as an idle report");

        // A later blip of CPU can't bring the stale busy back: it is no longer there to believe.
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Busy);
        assert_eq!(polled(&splitter, later + Duration::from_secs(1)), Some(PaneStatus::Idle));
    }

    /// Real work dips below the threshold for a moment. One quiet look is not enough.
    #[test]
    fn a_pane_that_shows_any_activity_while_being_confirmed_keeps_its_busy_report() {
        for blip_at in 0..QUIET_CONFIRMATIONS {
            let (splitter, mux, reports, later) = quiet_pane_with_an_old_busy_report("blip");
            let mut script: Vec<_> = (0..QUIET_CONFIRMATIONS).map(|_| idle_at("p1")).collect();
            script.insert(0, idle_at("p1")); // the poll itself
            script[1 + blip_at] = busy_at("p1");
            *mux.probe_queue.lock().unwrap() = script.into();
            assert_eq!(polled(&splitter, later), Some(PaneStatus::Busy), "activity on confirmation {}", blip_at + 1);
            assert!(matches!(reports.said("feat-a", later), Some(Said::Fresh { status: PaneStatus::Busy, .. })), "the report is untouched");
        }
    }

    #[test]
    fn a_pane_that_cannot_be_read_while_confirming_keeps_its_busy_report() {
        let (splitter, mux, reports, later) = quiet_pane_with_an_old_busy_report("unreadable");
        // The poll's own look succeeds and says quiet; the first confirmation fails.
        *mux.fail_probe_after.lock().unwrap() = Some(1);
        assert_eq!(polled(&splitter, later), Some(PaneStatus::Busy), "an unconfirmed doubt does not overturn the agent");
        assert!(matches!(reports.said("feat-a", later), Some(Said::Fresh { status: PaneStatus::Busy, .. })), "the report is untouched");
        assert_eq!(*mux.probes.lock().unwrap(), 2, "the poll, then the one failed confirmation");
    }

    /// The multiplexer's reading lags, so a recent report is left alone and no extra looks are
    /// taken — the common case costs nothing.
    #[test]
    fn a_recent_busy_report_is_never_second_guessed_and_costs_no_extra_probes() {
        let (splitter, mux, _reports) = {
            let (splitter, mux, reports) = splitter_with_reports("recent");
            let now = SystemTime::now();
            reports.write("feat-a", Reported::Busy, now).unwrap();
            mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
            (splitter, mux, reports)
        };
        assert_eq!(polled(&splitter, SystemTime::now() + Duration::from_secs(5)), Some(PaneStatus::Busy));
        assert_eq!(*mux.probes.lock().unwrap(), 1);
    }

    /// The point of `waiting` is a prompt left overnight. A minute of a prompt sitting there
    /// read as busy under the rule this replaces, on CPU blips alone.
    #[test]
    fn a_waiting_report_stands_however_old_and_whatever_the_pane_reads() {
        for native in [PaneStatus::Idle, PaneStatus::Busy] {
            let (splitter, mux, reports) = splitter_with_reports("waiting-stands");
            let said = SystemTime::now();
            reports.write("feat-a", Reported::Waiting, said).unwrap();
            mux.statuses.lock().unwrap().insert("p1".to_string(), native);
            assert_eq!(polled(&splitter, said + Duration::from_secs(3600)), Some(PaneStatus::Waiting), "over {native:?}");
            assert_eq!(*mux.probes.lock().unwrap(), 1, "no confirmation looks for a prompt");
        }
    }

    // Process tracking: what goes on the launch line, and the pid file it fills.

    use crate::procs::Pids;

    /// A splitter over a fake multiplexer that does (`mux_asks`) or doesn't ask for its panes'
    /// processes to be tracked, with `KANSTACK_TRACK_PIDS` set to `setting` while it is made, and
    /// pids kept in a fresh directory.
    fn splitter_tracking(tag: &str, mux_asks: bool, setting: Option<&str>) -> (Splitter, Arc<FakeMux>, Pids) {
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-pids-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pids = Pids::in_dir(dir);
        let mut made = None;
        with_env(&[("KANSTACK_TRACK_PIDS", setting)], || {
            let (mut splitter, mux) = fake_splitter();
            *mux.tracks_pids.lock().unwrap() = mux_asks;
            splitter.set_pids(pids.clone());
            made = Some((splitter, mux));
        });
        let (splitter, mux) = made.unwrap();
        (splitter, mux, pids)
    }

    /// The `feat-a` pane's launch line as the multiplexer was handed it.
    fn launched(mux: &FakeMux) -> String {
        let line = mux.lines().into_iter().find(|l| l.starts_with("open ")).expect("a pane was opened");
        line.split_once(" as feat-a: ").expect("opened as feat-a").1.to_string()
    }

    /// The whole point of the prefix: the pane's own shell writes its pid before the harness
    /// starts, and the harness still launches after it.
    #[test]
    fn a_tracked_launch_records_the_shells_pid_before_the_harness_starts() {
        let (mut splitter, mux, pids) = splitter_tracking("prefix", true, None);
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        let file = pids.prepare("feat-a").unwrap().unwrap();
        let untracked = {
            let (mut plain, plain_mux) = fake_splitter();
            plain.spawn_harness(cwd(), "feat-a", None).unwrap();
            launched(&plain_mux)
        };
        assert!(untracked.starts_with("cd '/repo' && claude "), "{untracked}");
        assert_eq!(
            launched(&mux),
            format!(r#"sh -c 'printf %s "$PPID" > "$0"' '{}' && {untracked}"#, file.display()),
        );
        assert!(file.parent().unwrap().is_dir(), "the directory exists before the pane's shell writes into it");
    }

    /// A path with spaces and a quote in it stays one word, quoted once.
    #[test]
    fn the_pid_file_path_is_quoted_for_the_pane_shell() {
        let dir = std::env::temp_dir().join(format!("kanstack it's spaced-{}", std::process::id()));
        let (mut splitter, mux, _) = splitter_tracking("quoting", true, None);
        splitter.set_pids(Pids::in_dir(dir.clone()));
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        let quoted = crate::harness_launch::shell_quote(&dir.join("x").to_string_lossy());
        let quoted_dir = quoted.trim_end_matches("x'");
        let line = launched(&mux);
        assert!(line.contains("'\\''s"), "the apostrophe is closed out and escaped: {line}");
        assert!(line.starts_with(&format!(r#"sh -c 'printf %s "$PPID" > "$0"' {quoted_dir}"#)), "{line}");
        assert!(line.contains(".pid' && cd '/repo' && claude "), "{line}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_multiplexer_that_reports_for_itself_gets_the_launch_line_untouched_and_no_pid_directory() {
        let (mut splitter, mux, pids) = splitter_tracking("off", false, None);
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(launched(&mux).starts_with("cd '/repo' && claude "), "{}", launched(&mux));
        assert!(!launched(&mux).contains("PPID"));
        let dir = pids.prepare("feat-a").unwrap().unwrap().parent().unwrap().to_path_buf();
        let _ = std::fs::remove_dir_all(&dir);
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        assert!(!dir.exists(), "nothing was prepared for an untracked pane");
    }

    #[test]
    fn the_setting_turns_tracking_on_for_a_multiplexer_that_does_not_ask_and_off_for_one_that_does() {
        for (mux_asks, setting, tracked) in [
            (false, Some("1"), true),
            (false, Some("true"), true),
            (false, Some("on"), true),
            (false, Some("yes"), true),
            (false, Some("0"), false),
            (false, None, false),
            (true, None, true),
            (true, Some("1"), true),
            (true, Some("0"), false),
            (true, Some("off"), false),
            (true, Some("false"), false),
            (true, Some("no"), false),
            (true, Some("whatever"), true),
        ] {
            let (mut splitter, mux, _) = splitter_tracking("setting", mux_asks, setting);
            splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
            assert_eq!(launched(&mux).contains("$PPID"), tracked, "mux asks {mux_asks}, KANSTACK_TRACK_PIDS={setting:?}");
        }
    }

    /// The setting is read when the splitter is made: a test (or a process) that changes it later
    /// doesn't change what an existing splitter does.
    #[test]
    fn the_setting_is_read_once_when_the_splitter_is_made() {
        let (mut splitter, mux, _) = splitter_tracking("once", true, Some("off"));
        with_env(&[("KANSTACK_TRACK_PIDS", Some("1"))], || {
            splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        });
        assert!(!launched(&mux).contains("PPID"));
    }

    /// Nowhere to keep a pid means nothing to record: no prefix, rather than one pointing at a
    /// file nobody will read.
    #[test]
    fn without_a_pid_directory_there_is_no_prefix_however_tracking_was_asked_for() {
        let mut made = None;
        with_env(&[("KANSTACK_TRACK_PIDS", Some("1"))], || {
            let (splitter, mux) = fake_splitter();
            *mux.tracks_pids.lock().unwrap() = true;
            made = Some((splitter, mux));
        });
        let (mut splitter, mux) = made.unwrap();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert!(launched(&mux).starts_with("cd '/repo' && claude "), "{}", launched(&mux));
    }

    /// The last pane's shell is gone with it, and a pane that hasn't written its own yet must not
    /// be read through it. Before the pane opens, not after: the new shell may be quick.
    #[test]
    fn a_new_pane_on_a_branch_does_not_inherit_the_old_ones_pid() {
        let (mut splitter, _mux, pids) = splitter_tracking("respawn", true, None);
        let file = pids.prepare("feat-a").unwrap().unwrap();
        std::fs::write(&file, "4242").unwrap();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert_eq!(pids.read("feat-a"), None);
    }

    /// ...even when tracking has since been turned off, so nothing stale is left to be read later.
    #[test]
    fn a_new_pane_forgets_the_old_pid_even_when_it_is_not_itself_tracked() {
        let (mut splitter, _mux, pids) = splitter_tracking("respawn-off", false, None);
        std::fs::write(pids.prepare("feat-a").unwrap().unwrap(), "4242").unwrap();
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        assert_eq!(pids.read("feat-a"), None);
    }

    #[test]
    fn stopping_a_lane_forgets_its_pid() {
        let (mut splitter, _mux, pids) = splitter_tracking("stop", true, None);
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        std::fs::write(pids.prepare("feat-a").unwrap().unwrap(), "4242").unwrap();
        std::fs::write(pids.prepare("feat-b").unwrap().unwrap(), "4243").unwrap();
        splitter.stop("feat-a").unwrap();
        assert_eq!(pids.read("feat-a"), None);
        assert_eq!(pids.read("feat-b"), Some(4243), "only that lane's");
    }

    /// An open that failed opened nothing to track, but the old pane's pid is stale either way.
    #[test]
    fn a_failed_open_leaves_no_pid_behind() {
        let (mut splitter, mux, pids) = splitter_tracking("failed-open", true, None);
        std::fs::write(pids.prepare("feat-a").unwrap().unwrap(), "4242").unwrap();
        *mux.fail_open.lock().unwrap() = true;
        assert!(splitter.spawn_harness(cwd(), "feat-a", None).is_err());
        assert_eq!(pids.read("feat-a"), None);
    }

    // The process table as a third source: what a multiplexer that can't say whether a pane is busy
    // reads like, from the pid its shell recorded, against a table the test scripts.

    use crate::procs::PsRow;
    use crate::mux::pane_status::CPU_BUSY_THRESHOLD_PERCENT;
    use std::sync::Mutex;

    /// A process table the test controls, that counts how often it was read.
    #[derive(Default)]
    struct FakeProcs {
        table: Mutex<Vec<PsRow>>,
        /// Tables for the next reads, one per call, before falling back to `table`.
        queue: Mutex<std::collections::VecDeque<Vec<PsRow>>>,
        reads: Mutex<u32>,
        /// Makes every read after this many fail.
        fail_after: Mutex<Option<u32>>,
    }

    impl FakeProcs {
        fn set(&self, table: Vec<PsRow>) {
            *self.table.lock().unwrap() = table;
        }

        fn reads(&self) -> u32 {
            *self.reads.lock().unwrap()
        }

        fn reader(self: &Arc<Self>) -> crate::procs::PsReader {
            let this = self.clone();
            Arc::new(move || {
                let n = {
                    let mut reads = this.reads.lock().unwrap();
                    *reads += 1;
                    *reads
                };
                if this.fail_after.lock().unwrap().is_some_and(|after| n > after) {
                    anyhow::bail!("fake ps failed");
                }
                Ok(this.queue.lock().unwrap().pop_front().unwrap_or_else(|| this.table.lock().unwrap().clone()))
            })
        }
    }

    /// The shell of `feat-a`'s pane, as `ps` lists it.
    const SHELL: u32 = 100;

    fn shell_using(cpu: f64) -> Vec<PsRow> {
        vec![(1, 0, 0.0), (SHELL, 1, cpu)]
    }

    struct Tracked {
        splitter: Splitter,
        mux: Arc<FakeMux>,
        procs: Arc<FakeProcs>,
        pids: Pids,
        reports: Reports,
    }

    /// A splitter over a multiplexer that has no reading of its own and asks for tracking, with one
    /// lane, `feat-a`, whose shell (pid [`SHELL`]) has recorded itself and is using no CPU.
    fn tracked_lane(tag: &str) -> Tracked {
        let (mut splitter, mux, pids) = splitter_tracking(&format!("procs-{tag}"), true, None);
        let dir = std::env::temp_dir().join(format!("kanstack-splitter-procs-reports-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let reports = Reports::in_dir(dir);
        splitter.set_reports(reports.clone());
        let procs = Arc::new(FakeProcs::default());
        procs.set(shell_using(0.0));
        splitter.set_process_table(procs.reader());
        splitter.quiet_gap = Duration::ZERO;
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        std::fs::write(pids.prepare("feat-a").unwrap().unwrap(), SHELL.to_string()).unwrap();
        Tracked { splitter, mux, procs, pids, reports }
    }

    /// A shell that has exited is not in the table. On a multiplexer that keeps a finished pane
    /// listed, this is the only way the pane is ever noticed to be over.
    #[test]
    fn a_recorded_shell_that_is_gone_from_the_process_table_reads_dead() {
        let t = tracked_lane("gone");
        t.procs.set(vec![(1, 0, 0.0), (999, 1, 50.0)]);
        assert_eq!(polled(&t.splitter, SystemTime::now()), Some(PaneStatus::Dead));
    }

    #[test]
    fn a_recorded_shell_that_is_present_reads_idle_or_busy_by_its_subtrees_cpu() {
        let t = tracked_lane("cpu");
        let now = SystemTime::now();
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Idle));
        t.procs.set(shell_using(60.0));
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Busy));
        // The shell itself is quiet; the harness under it is not, and so is what it runs.
        t.procs.set(vec![(1, 0, 0.0), (SHELL, 1, 0.0), (200, SHELL, 0.2), (300, 200, 45.0)]);
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Busy));
        // Busy elsewhere is not busy here.
        t.procs.set(vec![(1, 0, 90.0), (SHELL, 1, 0.5), (999, 1, 90.0)]);
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Idle));
    }

    /// "Above" the threshold, as the multiplexers that measure CPU themselves have it: exactly at
    /// it is still idle.
    #[test]
    fn the_cpu_threshold_is_exclusive() {
        let t = tracked_lane("boundary");
        let now = SystemTime::now();
        t.procs.set(shell_using(CPU_BUSY_THRESHOLD_PERCENT));
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Idle), "at the threshold");
        t.procs.set(shell_using(CPU_BUSY_THRESHOLD_PERCENT + 0.1));
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Busy), "just above it");
        t.procs.set(shell_using(CPU_BUSY_THRESHOLD_PERCENT - 0.1));
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Idle), "just below it");
    }

    /// The shell writes its pid a moment after the pane opens. Until it has, nothing is known, and
    /// the table isn't read for a pane it can't look up.
    #[test]
    fn a_tracked_pane_with_no_pid_recorded_has_no_reading_and_costs_no_ps() {
        let t = tracked_lane("no-pid");
        t.pids.forget("feat-a");
        assert_eq!(polled(&t.splitter, SystemTime::now()), None);
        assert_eq!(t.procs.reads(), 0);
    }

    /// The multiplexer measures CPU itself where it can, and a reading of its own is not
    /// second-guessed by a guess from `ps`.
    #[test]
    fn the_multiplexers_own_busy_or_idle_reading_wins_over_the_pid_reading() {
        let t = tracked_lane("probe-wins");
        let now = SystemTime::now();
        t.procs.set(shell_using(60.0));
        t.mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Idle));
        t.procs.set(shell_using(0.0));
        t.mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Busy);
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Busy));
        assert_eq!(t.procs.reads(), 0, "nothing needed the table");
    }

    #[test]
    fn a_pane_the_multiplexer_says_is_gone_is_dead_whatever_the_process_table_says() {
        let t = tracked_lane("probe-dead");
        let now = SystemTime::now();
        t.procs.set(shell_using(60.0));
        t.reports.write("feat-a", Reported::Busy, now).unwrap();
        t.mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Dead);
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Dead));
        assert_eq!(t.procs.reads(), 0);
    }

    /// A shell that is gone can't have said it stopped either.
    #[test]
    fn a_recorded_shell_that_is_gone_is_dead_whatever_the_agent_last_said() {
        let t = tracked_lane("gone-said");
        let now = SystemTime::now();
        t.reports.write("feat-a", Reported::Busy, now).unwrap();
        t.procs.set(vec![(1, 0, 0.0)]);
        assert_eq!(polled(&t.splitter, now), Some(PaneStatus::Dead));
    }

    /// The pid reading is a guess from CPU, the same as a multiplexer's, so it ranks where that
    /// does: under what the agent says.
    #[test]
    fn a_fresh_report_wins_over_the_pid_reading_and_a_stale_one_gives_way_to_it() {
        let t = tracked_lane("said");
        let then = SystemTime::now();
        t.reports.write("feat-a", Reported::Waiting, then).unwrap();
        t.procs.set(shell_using(60.0));
        assert_eq!(polled(&t.splitter, then), Some(PaneStatus::Waiting));
        t.reports.write("feat-a", Reported::Idle, then).unwrap();
        assert_eq!(polled(&t.splitter, then), Some(PaneStatus::Idle), "not busy, despite the CPU");
        let later = then + FRESH_FOR + Duration::from_secs(1);
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Busy), "expired: the table is all there is");
        t.procs.set(shell_using(0.0));
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Idle));
    }

    /// One `ps` for the whole poll, however many panes need it.
    #[test]
    fn the_process_table_is_read_once_per_poll_however_many_panes_need_it() {
        let mut t = tracked_lane("once");
        t.splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        t.splitter.spawn_harness(cwd(), "feat-c", None).unwrap();
        for (branch, pid) in [("feat-b", 101), ("feat-c", 102)] {
            std::fs::write(t.pids.prepare(branch).unwrap().unwrap(), pid.to_string()).unwrap();
        }
        t.procs.set(vec![(SHELL, 1, 0.0), (101, 1, 80.0)]);
        let polled = t.splitter.poll_statuses_at(SystemTime::now()).unwrap();
        assert_eq!(
            polled,
            HashMap::from([
                ("feat-a".to_string(), PaneStatus::Idle),
                ("feat-b".to_string(), PaneStatus::Busy),
                ("feat-c".to_string(), PaneStatus::Dead),
            ])
        );
        assert_eq!(t.procs.reads(), 1);
        assert_eq!(*t.mux.probes.lock().unwrap(), 1);
    }

    /// Only the panes that the multiplexer had nothing for need the table, and if there are none
    /// it is not read.
    #[test]
    fn a_pane_with_its_own_reading_beside_one_without_still_costs_one_read() {
        let mut t = tracked_lane("mixed");
        t.splitter.spawn_harness(cwd(), "feat-b", None).unwrap();
        std::fs::write(t.pids.prepare("feat-b").unwrap().unwrap(), "101").unwrap();
        t.mux.statuses.lock().unwrap().insert("p2".to_string(), PaneStatus::Idle);
        t.procs.set(vec![(SHELL, 1, 80.0), (101, 1, 80.0)]);
        let polled = t.splitter.poll_statuses_at(SystemTime::now()).unwrap();
        assert_eq!(polled["feat-a"], PaneStatus::Busy, "from the table");
        assert_eq!(polled["feat-b"], PaneStatus::Idle, "from the multiplexer, which is believed");
        assert_eq!(t.procs.reads(), 1);
    }

    /// For a multiplexer that reports for itself nothing changes: a pid file left over from a
    /// time tracking was on is not looked at, and `ps` is not run.
    #[test]
    fn with_tracking_off_the_pid_file_and_the_process_table_are_ignored() {
        let (mut splitter, mux, pids) = splitter_tracking("procs-off", false, None);
        let procs = Arc::new(FakeProcs::default());
        procs.set(shell_using(60.0));
        splitter.set_process_table(procs.reader());
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        std::fs::write(pids.prepare("feat-a").unwrap().unwrap(), SHELL.to_string()).unwrap();
        assert_eq!(polled(&splitter, SystemTime::now()), None, "no news is still no news");
        mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        assert_eq!(polled(&splitter, SystemTime::now()), Some(PaneStatus::Idle));
        assert_eq!(procs.reads(), 0);
    }

    /// The setting can turn the pid tier on for a multiplexer that reports for itself, and off for
    /// one that doesn't.
    #[test]
    fn the_setting_governs_the_pid_reading_as_it_governs_the_launch_line() {
        for (mux_asks, setting, reads) in [(false, Some("1"), true), (true, Some("0"), false), (true, None, true)] {
            let (mut splitter, _mux, pids) = splitter_tracking("procs-setting", mux_asks, setting);
            let procs = Arc::new(FakeProcs::default());
            procs.set(shell_using(0.0));
            splitter.set_process_table(procs.reader());
            splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
            std::fs::write(pids.prepare("feat-a").unwrap().unwrap(), SHELL.to_string()).unwrap();
            let expected = reads.then_some(PaneStatus::Idle);
            assert_eq!(polled(&splitter, SystemTime::now()), expected, "mux asks {mux_asks}, setting {setting:?}");
        }
    }

    /// `ps` failing is not evidence of anything: the pane has no reading, and is not called dead.
    #[test]
    fn a_failed_ps_leaves_a_tracked_pane_with_no_reading_rather_than_dead() {
        let t = tracked_lane("ps-fails");
        *t.procs.fail_after.lock().unwrap() = Some(0);
        assert_eq!(polled(&t.splitter, SystemTime::now()), None);
        t.reports.write("feat-a", Reported::Busy, SystemTime::now()).unwrap();
        assert_eq!(polled(&t.splitter, SystemTime::now()), Some(PaneStatus::Busy), "reports are unaffected");
    }

    // Corroboration, from the process table alone: an interrupted turn on a multiplexer that
    // can't say the pane is quiet.

    /// A tracked lane whose agent said `busy` a minute ago, whose shell is using no CPU and whose
    /// multiplexer has nothing to say.
    fn quiet_shell_with_an_old_busy_report(tag: &str) -> (Tracked, SystemTime) {
        let t = tracked_lane(tag);
        let said = SystemTime::now();
        t.reports.write("feat-a", Reported::Busy, said).unwrap();
        (t, said + Duration::from_secs(60))
    }

    /// The same as `a_pane_that_stays_quiet_retracts_an_old_busy_report_and_it_stays_retracted`,
    /// with the quiet coming from the process table at every look.
    #[test]
    fn a_shell_that_stays_quiet_retracts_an_old_busy_report_and_it_stays_retracted() {
        let (t, later) = quiet_shell_with_an_old_busy_report("retract");
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Idle));
        assert_eq!(t.procs.reads(), 1 + QUIET_CONFIRMATIONS as u32, "one look, then each confirmation, each its own read");
        assert_eq!(*t.mux.probes.lock().unwrap(), 1 + QUIET_CONFIRMATIONS as u32);
        assert_eq!(t.reports.status("feat-a", later), Some(PaneStatus::Idle), "written back as an idle report");

        // A later blip of CPU can't bring the stale busy back.
        t.procs.set(shell_using(60.0));
        assert_eq!(polled(&t.splitter, later + Duration::from_secs(1)), Some(PaneStatus::Idle));
    }

    #[test]
    fn a_shell_that_shows_cpu_at_any_confirming_look_keeps_its_busy_report() {
        for blip_at in 0..QUIET_CONFIRMATIONS {
            let (t, later) = quiet_shell_with_an_old_busy_report("blip");
            let mut script: Vec<_> = (0..1 + QUIET_CONFIRMATIONS).map(|_| shell_using(0.0)).collect();
            script[1 + blip_at] = shell_using(60.0); // the poll's own look is first
            *t.procs.queue.lock().unwrap() = script.into();
            assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Busy), "CPU at confirmation {}", blip_at + 1);
            assert!(matches!(t.reports.said("feat-a", later), Some(Said::Fresh { status: PaneStatus::Busy, .. })), "untouched");
        }
    }

    /// A look that can't be made confirms nothing.
    #[test]
    fn a_shell_that_cannot_be_read_while_confirming_keeps_its_busy_report() {
        let (t, later) = quiet_shell_with_an_old_busy_report("ps-fails-confirming");
        *t.procs.fail_after.lock().unwrap() = Some(1);
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Busy));
        assert!(matches!(t.reports.said("feat-a", later), Some(Said::Fresh { status: PaneStatus::Busy, .. })));
        assert_eq!(t.procs.reads(), 2, "the poll, then the one failed confirmation");
    }

    /// A shell that has gone during confirmation is not quiet, it is dead: the report stands (the
    /// next poll says dead), rather than being overwritten with `idle`.
    #[test]
    fn a_shell_that_disappears_while_confirming_is_not_retracted_to_idle() {
        let (t, later) = quiet_shell_with_an_old_busy_report("vanishes");
        *t.procs.queue.lock().unwrap() = vec![shell_using(0.0), vec![(1, 0, 0.0)]].into();
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Busy));
        assert!(matches!(t.reports.said("feat-a", later), Some(Said::Fresh { status: PaneStatus::Busy, .. })));
        t.procs.set(vec![(1, 0, 0.0)]);
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Dead));
    }

    /// The multiplexer's reading lags, and the process table is no better: a recent report is left
    /// alone and costs one read.
    #[test]
    fn a_recent_busy_report_over_a_quiet_shell_is_left_alone() {
        let (t, _) = quiet_shell_with_an_old_busy_report("recent");
        let soon = SystemTime::now() + Duration::from_secs(5);
        assert_eq!(polled(&t.splitter, soon), Some(PaneStatus::Busy));
        assert_eq!(t.procs.reads(), 1);
    }

    /// A busy shell is what the report said: nothing to doubt, no extra looks.
    #[test]
    fn a_busy_report_over_a_busy_shell_is_not_a_suspect() {
        let (t, later) = quiet_shell_with_an_old_busy_report("agree");
        t.procs.set(shell_using(60.0));
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Busy));
        assert_eq!(t.procs.reads(), 1);
        assert_eq!(*t.mux.probes.lock().unwrap(), 1);
    }

    /// The other tier is unchanged: a probe reading of its own is what confirms.
    #[test]
    fn corroboration_still_uses_the_multiplexers_reading_where_it_has_one() {
        let (t, later) = quiet_shell_with_an_old_busy_report("probe-confirms");
        t.procs.set(shell_using(60.0)); // would say busy, if it were asked
        t.mux.statuses.lock().unwrap().insert("p1".to_string(), PaneStatus::Idle);
        assert_eq!(polled(&t.splitter, later), Some(PaneStatus::Idle));
        assert_eq!(t.procs.reads(), 0);
    }

    // The backend list is the one place backends are named. What can't be generated from it —
    // prose in the docs, `--help` — is checked here instead, so adding a backend without
    // documenting it fails a test rather than shipping silently.

    #[test]
    fn backends_are_described_as_prose_from_the_list() {
        assert_eq!(describe_backends(), "cmux, tmux, Orca or Ghostty");
    }

    #[test]
    fn backend_names_are_unique_lowercase_words() {
        let mut names: Vec<&str> = BACKENDS.iter().map(|b| b.name).collect();
        for name in &names {
            assert!(name.chars().all(|c| c.is_ascii_lowercase()), "{name:?}: names are the middle of KANSTACK_<NAME>_DIRECTION");
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), BACKENDS.len(), "two backends share a name");
    }

    /// Each backend must be named in the README and in the architecture doc.
    #[test]
    fn every_backend_is_documented() {
        let docs = [
            ("README.md", include_str!("../README.md")),
            ("docs/ARCHITECTURE.md", include_str!("../docs/ARCHITECTURE.md")),
        ];
        for backend in BACKENDS {
            for (file, text) in docs {
                assert!(
                    text.to_lowercase().contains(&backend.label.to_lowercase()),
                    "{file} never mentions the {} backend",
                    backend.label
                );
            }
        }
    }

    // Stopping a pane must end what runs in it, not only close it.

    type Killed = Arc<std::sync::Mutex<Vec<Vec<u32>>>>;

    //   100 (the pane's shell) ── 101 (the harness) ── 102 (a tool it runs)      200 (unrelated)
    fn process_tree() -> Vec<PsRow> {
        vec![(1, 0, 0.0), (100, 1, 0.0), (101, 100, 4.0), (102, 101, 9.0), (200, 1, 30.0)]
    }

    /// A tracked splitter with a `feat-a` pane whose shell (pid 100) recorded itself `age` ago and
    /// has been running for `age`, over `process_tree`, with every attempt to end a process
    /// recorded.
    fn stoppable(tag: &str, mux_asks: bool, age: Duration) -> (Splitter, Arc<FakeMux>, Pids, Killed) {
        let (mut splitter, mux, pids) = splitter_tracking(tag, mux_asks, None);
        splitter.spawn_harness(cwd(), "feat-a", None).unwrap();
        let file = pids.prepare("feat-a").unwrap().unwrap();
        std::fs::write(&file, "100").unwrap();
        let written = SystemTime::now() - age;
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(written).unwrap();
        let killed: Killed = Arc::default();
        let recorder = killed.clone();
        splitter.set_process_hooks(
            Arc::new(move |pid| (pid == 100).then_some(age)),
            Arc::new(move |pids: &[u32]| recorder.lock().unwrap().push(pids.to_vec())),
        );
        splitter.set_process_table(Arc::new(|| Ok(process_tree())));
        (splitter, mux, pids, killed)
    }

    fn ended(killed: &Killed) -> Vec<u32> {
        let mut all: Vec<u32> = killed.lock().unwrap().concat();
        all.sort_unstable();
        all
    }

    /// Ghostty drops a closed pane from its list and leaves the process running.
    #[test]
    fn stopping_a_tracked_pane_ends_its_shell_and_everything_under_it_and_closes_it() {
        let (mut splitter, mux, pids, killed) = stoppable("end", true, Duration::from_secs(100));
        splitter.stop("feat-a").unwrap();
        assert_eq!(ended(&killed), [100, 101, 102], "the shell, the harness and its tool — not the unrelated process");
        assert!(mux.lines().contains(&"close p1".to_string()), "{:#?}", mux.lines());
        assert_eq!(pids.read("feat-a"), None, "and the pid is forgotten");
    }

    #[test]
    fn a_pane_opened_by_another_process_is_ended_the_same_way() {
        let (mut splitter, _mux, _pids, killed) = stoppable("adopted", true, Duration::from_secs(100));
        splitter.stop("feat-a").unwrap();
        assert!(!ended(&killed).is_empty());
        // ...and one this process adopted from the registry, as `kanstack stop` does.
        let (mut splitter, _mux, pids, killed) = {
            let (mut splitter, mux, pids) = splitter_tracking("adopted2", true, None);
            let file = pids.prepare("feat-b").unwrap().unwrap();
            std::fs::write(&file, "100").unwrap();
            std::fs::File::options().write(true).open(&file).unwrap().set_modified(SystemTime::now() - Duration::from_secs(50)).unwrap();
            let killed: Killed = Arc::default();
            let recorder = killed.clone();
            splitter.set_process_hooks(
                Arc::new(|pid| (pid == 100).then_some(Duration::from_secs(50))),
                Arc::new(move |pids: &[u32]| recorder.lock().unwrap().push(pids.to_vec())),
            );
            splitter.set_process_table(Arc::new(|| Ok(process_tree())));
            splitter.adopt("feat-b", "p7");
            (splitter, mux, pids, killed)
        };
        splitter.stop("feat-b").unwrap();
        assert_eq!(ended(&killed), [100, 101, 102]);
        assert_eq!(pids.read("feat-b"), None);
    }

    /// Pids are reused. A recorded one that now names a process which began after it was written
    /// belongs to someone else, and ending it would be ending a stranger.
    #[test]
    fn a_recorded_pid_that_now_belongs_to_a_stranger_is_left_alone() {
        let (mut splitter, _mux, _pids, killed) = stoppable("stranger", true, Duration::from_secs(3600));
        // The file was written an hour ago; the process holding pid 100 has run for five seconds.
        splitter.set_process_hooks(
            Arc::new(|pid| (pid == 100).then_some(Duration::from_secs(5))),
            {
                let recorder = killed.clone();
                Arc::new(move |pids: &[u32]| recorder.lock().unwrap().push(pids.to_vec()))
            },
        );
        splitter.stop("feat-a").unwrap();
        assert!(ended(&killed).is_empty(), "{:?}", ended(&killed));
    }

    #[test]
    fn nothing_is_ended_where_process_tracking_does_not_apply() {
        let (mut splitter, mux, _pids, killed) = stoppable("untracked", false, Duration::from_secs(100));
        splitter.stop("feat-a").unwrap();
        assert!(ended(&killed).is_empty(), "a multiplexer that can close a pane properly needs no help");
        assert!(mux.lines().contains(&"close p1".to_string()));
    }

    #[test]
    fn nothing_is_ended_when_there_is_no_recorded_pid_or_no_such_process_or_no_process_table() {
        // No pid file: the shell never wrote it.
        let (mut splitter, _mux, pids, killed) = stoppable("no-file", true, Duration::from_secs(100));
        pids.forget("feat-a");
        splitter.stop("feat-a").unwrap();
        assert!(ended(&killed).is_empty());

        // The recorded shell is already gone.
        let (mut splitter, _mux, _pids, killed) = stoppable("gone", true, Duration::from_secs(100));
        splitter.set_process_hooks(Arc::new(|_| None), {
            let recorder = killed.clone();
            Arc::new(move |pids: &[u32]| recorder.lock().unwrap().push(pids.to_vec()))
        });
        splitter.stop("feat-a").unwrap();
        assert!(ended(&killed).is_empty());

        // `ps` can't be read: better to end nothing than to guess.
        let (mut splitter, _mux, _pids, killed) = stoppable("no-ps", true, Duration::from_secs(100));
        splitter.set_process_table(Arc::new(|| anyhow::bail!("ps failed")));
        splitter.stop("feat-a").unwrap();
        assert!(ended(&killed).is_empty());
    }

    /// A pane that failed to close is the case where the harness is most likely still running.
    #[test]
    fn a_failed_close_still_ends_the_processes_and_still_reports_the_failure() {
        let (mut splitter, mux, _pids, killed) = stoppable("close-fails", true, Duration::from_secs(100));
        *mux.fail_close.lock().unwrap() = true;
        let err = splitter.stop("feat-a").unwrap_err().to_string();
        assert!(err.contains("fake close failed"), "{err}");
        assert_eq!(ended(&killed), [100, 101, 102]);
        assert!(!splitter.has_pane("feat-a"), "the lane is forgotten regardless");
    }
}
