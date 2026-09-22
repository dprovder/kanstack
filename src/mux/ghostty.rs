//! Optional bridge to the Ghostty terminal app (<https://ghostty.org>), the fourth harness-split
//! backend beside `crate::mux::cmux`, `crate::mux::tmux` and `crate::mux::orca`. macOS only: it drives the
//! app through the AppleScript dictionary Ghostty 1.3 added, by way of `osascript`.
//!
//! **What was measured, against what.** Everything below marked (measured) was run against
//! the official Ghostty 1.3.1 build (`TeamIdentifier=24VZTF6M5V`) on macOS, with an isolated
//! config. Anything marked (assumed) was not.
//!
//! # The dictionary
//!
//! `application > window > tab > terminal`. A terminal has an `id` (a UUID, stable while it
//! lives), a `name` (its current title) and a `working directory` (empty until shell
//! integration reports one). The commands used here: `split <terminal> direction <d> [with
//! configuration <cfg>]`, which returns the new terminal; `new surface configuration`, a
//! record carrying `initial working directory`, `command` and `environment variables` (a list
//! of `KEY=VALUE`); `input text`, `send key`, `focus`, `close` and `perform action` (all
//! measured). `exists terminal id X` throws (-1728) for a missing id, so membership is tested
//! against `id of every terminal` instead; the direction of a split is an enumeration
//! constant, not a string, so the script branches on it.
//!
//! # One osascript call per operation, dynamic values as arguments
//!
//! Each operation is one `osascript` run of a fixed script, whose dynamic values — the launch
//! line, the directory, environment values, the title, ids and text to type — arrive as the
//! script's `argv`, after a `--` (without which a text starting with `-` is read as an
//! option; measured). Nothing is ever interpolated into the script source, so nothing needs
//! quoting and nothing can inject. A run costs 70–200 ms (measured), which is why `probe` is a
//! single call. `KANSTACK_OSASCRIPT_BIN` replaces `osascript`, for tests.
//!
//! # Launching a harness
//!
//! The launch line (`cd '/repo' && KANSTACK_BRANCH=… claude …`) is handed to `split` as the
//! new terminal's `command`, wrapped as `/bin/sh -c '<line>'`. The wrapper is not optional.
//! Measured: on macOS Ghostty starts a command as `login -flp <user> /bin/bash --noprofile
//! --norc -c "exec -l <command>"`, splicing the command into that line *unquoted*. A bare
//! `cd '/repo' && claude` therefore becomes `exec -l cd '/repo'` (and dies: `cd` is a
//! builtin, not a program) with the rest never reached, while `sleep 60 && echo hi` execs
//! `sleep` and drops the `&&`. The pane opens, the command vanishes and the pane stays. With
//! `sh -c '<line>'` the whole line reaches one shell intact, however long or quoted.
//!
//! Nothing is typed into a shell, so the typed-line limit that shapes the other backends does
//! not apply, and there is no shell prompt in the pane at all: the harness *is* the pane's
//! process. The working directory and environment go in the same configuration and apply
//! exactly (measured).
//!
//! That `sh` is a login shell (`exec -l`) but not the user's interactive one, so `~/.zshrc`
//! has not run. A `PATH` that only the rc files build would be missing there, so the caller's
//! `PATH` and every `KANSTACK_*` variable but `KANSTACK_BRANCH` (the state path, harness
//! settings) are passed along.
//! Nothing else of the caller's environment is: a variable that exists only in an interactive
//! shell will not reach a harness launched this way (assumed to matter rarely; not measured).
//!
//! **A pane stays open after its command exits** — exit 0 and exit 3 both stayed listed in
//! `id of every terminal` for 9+ seconds (measured). AppleScript can therefore say a pane was
//! *closed*, never that its harness *finished*. That is what [`Multiplexer::tracks_pids`] is
//! for.
//!
//! # Closing
//!
//! `close <terminal>` removes the pane, and the id leaves `id of every terminal` at once
//! (measured). It does **not** end the process inside it: in Ghostty 1.3.1 a `sleep`, a `cat`
//! and a shell each outlived their closed pane by minutes, and the app still held the pty for
//! them (`lsof` on Ghostty showed the master fds). Closing a *window* did end its process, but
//! that is no use for one split among several. `perform action "close_surface"` on a pane
//! with a running process raises a confirmation dialog in the app instead, which a script can't
//! answer, so it is not used. Ending the harness is left to whatever knows its pid.
//!
//! # Typing
//!
//! `input text` is a real bracketed paste (a TUI that enabled bracketed paste received
//! `ESC[200~hello\nworld ESC[201~`), and `send key "enter"` arrives as a separate `\r`
//! (measured), so a line is a paste followed by its own Enter, in one script run.
//!
//! # Finding kanstack's own terminal
//!
//! Ghostty exports no per-terminal variable to the shell (the `GHOSTTY_SURFACE_ID` seen in
//! some shells is leaked from cmux, which is a Ghostty fork). What a program *can* do is set
//! its own terminal's title, and the new title is readable through AppleScript. So for the
//! first lane, kanstack writes a unique marker title to its controlling terminal
//! (`/dev/tty`, which is right even when stdout is a pipe or the board owns the screen),
//! lists the terminals until one's `name` contains the marker, remembers that id, and writes
//! the terminal's previous title back. Measured: an `OSC 2` title is readable as `name of
//! terminal`; the whole dance works from a headless `kanstack spawn` in a shell and from
//! inside the board's alternate screen (measured both ways: a `kanstack spawn` typed into a
//! shell, and a lane started with `b` in the running board). Later lanes chain off the pane
//! before, so this happens once per process.
//!
//! # Busy and idle
//!
//! Ghostty cannot say whether a pane is busy or idle, only whether it exists, so `probe`
//! reports a pane that is absent from `id of every terminal` as [`PaneStatus::Dead`] and says
//! nothing about one that is present. Status comes from the agent's own reports
//! (`kanstack report`) and from the pid tier, which is why [`Multiplexer::tracks_pids`] is
//! `true` here. If Ghostty itself has been quit, the probe fails rather than declaring every
//! pane dead, and the reports stand.
//!
//! # Discovery
//!
//! [`Ghostty::discover`] requires `TERM_PROGRAM=ghostty` and neither `$CMUX_SURFACE_ID` nor
//! `$TMUX_PANE`, for the same reason `Tmux::discover` requires `$TMUX_PANE`: with no Ghostty
//! terminal to be kanstack's own there is nothing to split. cmux is a Ghostty fork and sets
//! `TERM_PROGRAM=ghostty` too, and a tmux inside Ghostty should still get tmux, hence the two
//! exclusions. On any other OS it is `None` outright.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::harness_launch::shell_quote;
use crate::mux::{command_exists, Multiplexer, OpenRequest};
use crate::mux::pane_status::PaneStatus;

/// How long the first lane waits for its marker title to show up in Ghostty's terminal list.
/// The title is applied by Ghostty asynchronously, so the first look can be too early.
const MARKER_WAIT: Duration = Duration::from_secs(3);

/// Ghostty, through AppleScript. Which pane belongs to which branch is
/// `crate::splitter::Splitter`'s business; the one thing this remembers is which terminal is
/// kanstack's own, once it has found out.
pub struct Ghostty {
    /// `osascript`, or `$KANSTACK_OSASCRIPT_BIN`.
    bin: PathBuf,
    /// Where the marker title is written: kanstack's controlling terminal.
    tty: PathBuf,
    /// How long to look for the marker before giving up.
    marker_wait: Duration,
    /// kanstack's own terminal, found by the first lane and kept for the life of the process.
    own_terminal: Mutex<Option<String>>,
}

impl Ghostty {
    fn new(bin: PathBuf) -> Self {
        Ghostty { bin, tty: PathBuf::from("/dev/tty"), marker_wait: MARKER_WAIT, own_terminal: Mutex::new(None) }
    }

    /// Locates `osascript` and confirms kanstack is inside Ghostty. `None` (not an error) on
    /// any other OS, outside Ghostty, or without `osascript`; see the module doc for why an
    /// explicit `KANSTACK_OSASCRIPT_BIN` doesn't make up for not being in Ghostty.
    pub fn discover() -> Option<Self> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        Self::locate()
    }

    /// [`Self::discover`] without the OS check, so what it does past that check is testable
    /// anywhere.
    fn locate() -> Option<Self> {
        if !Self::running_inside() {
            return None;
        }
        let bin = match std::env::var_os("KANSTACK_OSASCRIPT_BIN") {
            Some(path) => PathBuf::from(path),
            None => {
                let candidate = PathBuf::from("osascript");
                if !command_exists(&candidate) {
                    return None;
                }
                candidate
            }
        };
        Some(Ghostty::new(bin))
    }

    /// Whether this process looks to be running in a Ghostty terminal: `TERM_PROGRAM` says
    /// so, and it is not really cmux (a Ghostty fork that says so too) or a tmux pane inside
    /// Ghostty. The same cheap environment check `discover` starts with.
    pub fn running_inside() -> bool {
        std::env::var("TERM_PROGRAM").is_ok_and(|program| program == "ghostty")
            && std::env::var_os("CMUX_SURFACE_ID").is_none()
            && std::env::var_os("TMUX_PANE").is_none()
    }

    /// Runs one fixed script with `args` as its `argv`. `op` only names the operation in an
    /// error message.
    fn run(&self, op: &str, body: &str, args: &[&str]) -> Result<String> {
        let mut command = Command::new(&self.bin);
        for line in script(op, body).lines().filter(|line| !line.trim().is_empty()) {
            command.arg("-e").arg(line);
        }
        let out = command
            .arg("--")
            .args(args)
            .output()
            .with_context(|| format!("failed to spawn `{}`", self.bin.display()))?;
        if !out.status.success() {
            bail!("{}", explain(op, &String::from_utf8_lossy(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Every terminal's id and title, in Ghostty's order.
    fn terminals(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .run("list", LIST, &[])?
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(id, title)| (id.to_string(), title.to_string()))
            .collect())
    }

    /// kanstack's own terminal: remembered from an earlier call, else found by giving it a
    /// marker title (see the module doc) and looking for that.
    fn own_terminal(&self) -> Result<String> {
        if let Some(id) = self.own_terminal.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(id);
        }
        let before = self.terminals()?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        let marker = format!("kanstack-{}-{nanos:x}", std::process::id());
        self.set_title(&marker).context(
            "can't tell which Ghostty terminal is kanstack's own: writing to this process's controlling terminal failed",
        )?;

        let started = Instant::now();
        let found = loop {
            if let Some((id, _)) = self.terminals()?.into_iter().find(|(_, title)| title.contains(&marker)) {
                break Some(id);
            }
            if started.elapsed() >= self.marker_wait {
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let Some(id) = found else {
            // Nothing to restore it to, and no way to say which terminal to restore.
            bail!(
                "can't tell which Ghostty terminal is kanstack's own: the title it set never appeared in Ghostty \
                 (is this really running in a Ghostty window?)"
            );
        };
        // The marker is not what this terminal was called. Best-effort: the shell integration
        // retitles it at the next prompt anyway.
        if let Some((_, original)) = before.iter().find(|(before_id, _)| *before_id == id) {
            let _ = self.set_title(original);
        }
        *self.own_terminal.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        Ok(id)
    }

    /// Sets the title of the terminal this process runs in, with `OSC 2`.
    fn set_title(&self, title: &str) -> Result<()> {
        let safe: String = title.chars().filter(|c| !c.is_control()).collect();
        let mut tty = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.tty)
            .with_context(|| format!("opening {}", self.tty.display()))?;
        tty.write_all(format!("\x1b]2;{safe}\x07").as_bytes())?;
        Ok(())
    }
}

/// One line for the setup wizard: whether Ghostty is usable here and, if not, what to do.
pub fn detection() -> String {
    if !cfg!(target_os = "macos") {
        return "✗ ghostty not usable here — the Ghostty backend is macOS only".to_string();
    }
    match Ghostty::discover() {
        Some(_) => "✓ ghostty found, and this terminal is inside it (macOS will ask to allow Automation)".to_string(),
        None if !Ghostty::running_inside() => {
            "✗ ghostty not usable here — not running inside a Ghostty window (TERM_PROGRAM is not ghostty, or this is a cmux or tmux pane)"
                .to_string()
        }
        None => "✗ ghostty found, but not `osascript` on PATH — set KANSTACK_OSASCRIPT_BIN".to_string(),
    }
}

impl Multiplexer for Ghostty {
    fn name(&self) -> &'static str {
        "ghostty"
    }

    fn running_inside(&self) -> bool {
        Ghostty::running_inside()
    }

    /// Ghostty splits natively in all four directions, so the defaults are tmux's and cmux's.
    fn default_directions(&self) -> (&'static str, &'static str) {
        ("up", "right")
    }

    /// Splits the previous lane's terminal (or, for the first lane, kanstack's own, found as
    /// the module doc describes) running `req.launch`, wrapped in `/bin/sh -c`, as the new
    /// terminal's command in `req.cwd`, with the caller's `PATH` and `KANSTACK_*` variables, and returns its id.
    /// The new terminal is titled with the branch name where that works; if it doesn't, the
    /// pane is still opened and only the label is missing.
    fn open_pane(&self, req: &OpenRequest<'_>) -> Result<String> {
        let (direction, anchor) = match req.after {
            Some(anchor) => (req.chain_direction, anchor.to_string()),
            None => (req.first_direction, self.own_terminal()?),
        };
        if !["up", "down", "left", "right"].contains(&direction) {
            bail!("Ghostty can't split `{direction}`: use up, down, left or right (or above or below)");
        }
        let cwd = req.cwd.to_string_lossy();
        let env = forwarded_environment();
        let command = format!("/bin/sh -c {}", shell_quote(req.launch));
        let mut args: Vec<&str> = vec![&cwd, &command, &anchor, direction, req.title];
        args.extend(env.iter().map(String::as_str));
        let out = self.run("split", SPLIT, &args)?;
        let id = out.trim();
        if id.is_empty() {
            bail!("Ghostty's `split` did not report a terminal id");
        }
        Ok(id.to_string())
    }

    /// A paste, then Enter as its own key: a paste alone is not submitted.
    fn type_line(&self, pane: &str, text: &str) -> Result<()> {
        self.run("type", TYPE, &[pane, text])?;
        Ok(())
    }

    fn focus(&self, pane: &str) -> Result<()> {
        self.run("focus", FOCUS, &[pane])?;
        Ok(())
    }

    /// Closes the terminal. One that is already gone is what closing is after, so it is not
    /// an error.
    fn close(&self, pane: &str) -> Result<()> {
        match self.run("close", CLOSE, &[pane]) {
            Err(e) if !e.to_string().contains(NO_SUCH_TERMINAL) => Err(e),
            _ => Ok(()),
        }
    }

    /// One call, `id of every terminal`: a pane missing from it is [`PaneStatus::Dead`], and
    /// one present is left out (no news), because Ghostty cannot say busy or idle.
    fn probe(&self, panes: &[&str]) -> Result<HashMap<String, PaneStatus>> {
        if panes.is_empty() {
            return Ok(HashMap::new());
        }
        let out = self.run("probe", PROBE, &[])?;
        let present: std::collections::HashSet<&str> = out.lines().map(str::trim).collect();
        Ok(panes.iter().filter(|p| !present.contains(*p)).map(|p| (p.to_string(), PaneStatus::Dead)).collect())
    }

    /// A pane outlives its command, and Ghostty only knows whether the pane is there.
    fn tracks_pids(&self) -> bool {
        true
    }
}

/// What the split's shell needs from the caller's environment: `PATH`, because the pane's
/// `sh -c` has not run the user's rc files, and every `KANSTACK_*` variable, so a `kanstack`
/// inside the pane sees the same state path and settings as this one. Not `KANSTACK_BRANCH`:
/// it names the lane the *caller* runs on (a `spawn` from inside a lane's pane has it set), and
/// a harness with no status hooks gets no `KANSTACK_BRANCH` of its own on its launch line, so
/// forwarding it would make the new lane report as the old one.
fn forwarded_environment() -> Vec<String> {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .filter(|(key, _)| key == "PATH" || (key.starts_with("KANSTACK_") && key != "KANSTACK_BRANCH"))
        .map(|(key, value)| format!("{key}={value}"))
        .collect()
}

/// The phrase that marks an error as "there is no such terminal", for `close`.
const NO_SUCH_TERMINAL: &str = "no such Ghostty terminal";

/// An `osascript` failure, in words for whoever ran kanstack. The codes are AppleScript's.
fn explain(op: &str, stderr: &str) -> String {
    let detail = stderr.trim();
    if detail.contains("(-1743)") {
        "macOS blocked kanstack from controlling Ghostty. Allow it in System Settings > Privacy & Security > \
         Automation (turn on Ghostty for the app that runs kanstack), then run it again"
            .to_string()
    } else if detail.contains("(-600)")
        || detail.contains("isn't running")
        || detail.contains("is not running")
        || detail.contains("Can’t get application")
        || detail.contains("Can't get application")
    {
        // The last two are what AppleScript says when nothing by that bundle id is running and
        // Launch Services knows of no installed copy either (measured, with a Ghostty that had
        // just quit and no other installed): -1728, at compile time, which must not be read as
        // "no such terminal".
        "Ghostty isn't running (or isn't installed), and kanstack drives it through AppleScript".to_string()
    } else if detail.contains("(-1719)") || detail.contains("(-1728)") {
        format!("{NO_SUCH_TERMINAL} (it was closed): {detail}")
    } else if detail.contains("(-2740)") || detail.contains("(-2741)") || detail.contains("(-1708)") {
        format!("this Ghostty has no AppleScript support, which needs Ghostty 1.3 or newer: {detail}")
    } else if detail.is_empty() {
        format!("Ghostty {op} failed")
    } else {
        format!("Ghostty {op} failed: {detail}")
    }
}

/// A complete script for `op`: a comment naming it (so a log of the calls reads), then an
/// `on run argv` handler that refuses to run — rather than launch the app — when Ghostty
/// isn't open, then `body`.
fn script(op: &str, body: &str) -> String {
    format!(
        "-- kanstack: {op}\n\
         on run argv\n\
         if not (application id \"com.mitchellh.ghostty\" is running) then error \"Ghostty is not running\" number -600\n\
         {body}\n\
         end run"
    )
}

/// Every terminal's id, one per line.
const PROBE: &str = r#"tell application id "com.mitchellh.ghostty" to set ids to id of every terminal
set AppleScript's text item delimiters to linefeed
return ids as text"#;

/// Every terminal's id and title, tab-separated, one per line.
const LIST: &str = r#"tell application id "com.mitchellh.ghostty"
set ids to id of every terminal
set titles to name of every terminal
end tell
set rows to {}
repeat with i from 1 to count of ids
set end of rows to (item i of ids) & tab & (item i of titles)
end repeat
set AppleScript's text item delimiters to linefeed
return rows as text"#;

/// argv: cwd, command line, terminal to split, direction, title, then `KEY=VALUE`s. Returns
/// the new terminal's id. The title is best-effort.
const SPLIT: &str = r#"set cwd to item 1 of argv
set cmdline to item 2 of argv
set anchor to item 3 of argv
set dir to item 4 of argv
set title_ to item 5 of argv
set envs to {}
if (count of argv) > 5 then set envs to items 6 thru -1 of argv
tell application id "com.mitchellh.ghostty"
set cfg to new surface configuration
set initial working directory of cfg to cwd
set command of cfg to cmdline
set environment variables of cfg to envs
set parentTerm to first terminal whose id is anchor
if dir is "right" then
set made to split parentTerm direction right with configuration cfg
else if dir is "left" then
set made to split parentTerm direction left with configuration cfg
else if dir is "down" then
set made to split parentTerm direction down with configuration cfg
else
set made to split parentTerm direction up with configuration cfg
end if
try
perform action ("set_surface_title:" & title_) on made
end try
return id of made
end tell"#;

/// argv: terminal id, text. A paste, then Enter as its own key.
const TYPE: &str = r#"set terminalId to item 1 of argv
set payload to item 2 of argv
tell application id "com.mitchellh.ghostty"
set dest to first terminal whose id is terminalId
input text payload to dest
send key "enter" to dest
end tell"#;

/// argv: terminal id.
const FOCUS: &str = r#"set terminalId to item 1 of argv
tell application id "com.mitchellh.ghostty"
focus (first terminal whose id is terminalId)
end tell"#;

/// argv: terminal id. Does nothing when there is no such terminal.
const CLOSE: &str = r#"set terminalId to item 1 of argv
tell application id "com.mitchellh.ghostty"
if terminalId is in (id of every terminal) then close (first terminal whose id is terminalId)
end tell"#;

#[cfg(test)]
mod tests {
    // What follows drives the real spawn/send/focus/stop/poll paths, through a `Splitter` as
    // kanstack does, against a stand-in `osascript` that logs its arguments and prints scripted
    // replies. The replies are what real Ghostty 1.3.1 printed for the same scripts: ids are
    // UUIDs, one per line here because the scripts join them with linefeeds. The stand-in can
    // say nothing about whether a script is *valid* AppleScript — only the live run can.

    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::*;
    use crate::harness::HarnessConfig;
    use crate::mux::stand_in;
    use crate::splitter::Splitter;

    /// Where the tests' own terminal is, in the stand-in's listing.
    const OWN: &str = "OWN";

    struct Fake {
        dir: PathBuf,
        /// The stand-in's own log: one line per call, every argument including the script.
        log: PathBuf,
    }

    /// One `osascript` call: the operation its script names, and the `argv` it got.
    #[derive(Debug, PartialEq)]
    struct Call {
        op: String,
        args: Vec<String>,
    }

    impl Fake {
        fn calls(&self) -> Vec<Call> {
            let text = std::fs::read_to_string(self.dir.join("argv")).unwrap_or_default();
            let mut calls: Vec<Call> = Vec::new();
            for line in text.lines() {
                match line.strip_prefix("@@ ") {
                    Some(op) => calls.push(Call { op: op.to_string(), args: Vec::new() }),
                    None => calls.last_mut().unwrap().args.push(line.to_string()),
                }
            }
            calls
        }

        fn ops(&self) -> Vec<String> {
            self.calls().into_iter().map(|c| c.op).collect()
        }

        fn tty(&self) -> String {
            std::fs::read_to_string(self.dir.join("tty")).unwrap()
        }

        /// A second backend over the same stand-in, for tests that call it directly.
        fn ghostty(&self) -> Ghostty {
            let mut ghostty = Ghostty::locate().unwrap();
            ghostty.tty = self.dir.join("tty");
            ghostty.marker_wait = Duration::from_millis(300);
            ghostty
        }
    }

    /// Runs `test` with `osascript` replaced by a stand-in that records each call's operation
    /// and `argv` and then runs `arms`: the `case "$op"` arms for whatever it should answer.
    /// The lists it prints reflect whatever the tests wrote to the fake controlling terminal,
    /// so a marker title written there comes back as the terminal's name.
    fn with_fake(tag: &str, arms: &str, extra: &[(&str, Option<&str>)], test: impl FnOnce(Splitter, &Fake)) {
        let dir = std::env::temp_dir().join(format!("kanstack-ghostty-fake-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tty"), "").unwrap();
        let body = format!(
            r#"DIR='{dir}'
op=$(printf '%s\n' "$@" | sed -n 's/^-- kanstack: //p' | head -1)
while [ "$1" != "--" ]; do shift; done; shift
{{ echo "@@ $op"; printf '%s\n' "$@"; }} >> "$DIR/argv"
case "$op" in
{arms}
esac"#,
            dir = dir.display()
        );
        let (bin, log) = stand_in::install(tag, "osascript", &body);
        let mut vars = vec![
            ("KANSTACK_OSASCRIPT_BIN", Some(bin.to_str().unwrap())),
            ("TERM_PROGRAM", Some("ghostty")),
            ("CMUX_SURFACE_ID", None),
            ("TMUX_PANE", None),
            ("KANSTACK_GHOSTTY_DIRECTION", None),
            ("KANSTACK_GHOSTTY_CHAIN_DIRECTION", None),
            ("KANSTACK_STATE_PATH", Some("/state")),
            ("KANSTACK_BRANCH", Some("the-callers-own-lane")),
        ];
        vars.extend_from_slice(extra);
        stand_in::with_env(&vars, || {
            let fake = Fake { dir: dir.clone(), log: log.clone() };
            let splitter = Splitter::new(Arc::new(fake.ghostty()), HarnessConfig::new("claude"));
            test(splitter, &fake)
        });
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two terminals, the second of which is titled by whatever is in the fake tty; a split
    /// answers with an id derived from the terminal it was asked to split (`$3`, after the
    /// working directory and the command line), so a chain of splits can be told apart.
    const ARMS: &str = r#"list) name=zsh; [ -s "$DIR/tty" ] && name=$(cat "$DIR/tty"); printf 'OTHER\tvim\n'; printf 'OWN\t%s\n' "$name" ;;
split) echo "NEW-after-$3" ;;
probe) printf 'OWN\nOTHER\nNEW-after-OWN\n' ;;"#;

    fn spawn(splitter: &mut Splitter, branch: &str) -> String {
        splitter.spawn_harness(Path::new("/repo"), branch, Some("go")).unwrap()
    }

    #[test]
    fn the_first_lane_finds_its_own_terminal_by_a_marker_title_and_splits_it() {
        with_fake("first", ARMS, &[], |mut splitter, fake| {
            assert_eq!(spawn(&mut splitter, "feat-a"), "NEW-after-OWN");
            assert_eq!(fake.ops(), ["list", "list", "split"], "one look before the marker, one after, then the split");

            let tty = fake.tty();
            assert!(tty.starts_with("\x1b]2;kanstack-"), "the marker is an OSC 2 title: {tty:?}");
            assert!(tty.ends_with("\x1b]2;zsh\x07"), "and the terminal's own title goes back afterwards: {tty:?}");

            let split = &fake.calls()[2];
            assert_eq!(split.args[2], OWN, "kanstack's own terminal is what gets split");
            assert_eq!(split.args[3], "up", "the first lane goes above by default");
        });
    }

    #[test]
    fn a_later_lane_chains_off_the_previous_one_without_looking_for_its_own_terminal_again() {
        with_fake("chain", ARMS, &[], |mut splitter, fake| {
            spawn(&mut splitter, "feat-a");
            spawn(&mut splitter, "feat-b");
            let calls = fake.calls();
            assert_eq!(fake.ops(), ["list", "list", "split", "split"]);
            assert_eq!(calls[3].args[2], "NEW-after-OWN", "chained off the previous lane's terminal");
            assert_eq!(calls[3].args[3], "right");
        });
    }

    #[test]
    fn its_own_terminal_is_remembered_after_the_lane_chained_off_is_stopped() {
        with_fake("remembered", ARMS, &[], |mut splitter, fake| {
            spawn(&mut splitter, "feat-a");
            splitter.stop("feat-a").unwrap();
            spawn(&mut splitter, "feat-b");
            assert_eq!(fake.ops(), ["list", "list", "split", "close", "split"], "no second search");
            assert_eq!(fake.calls()[4].args[2], OWN);
        });
    }

    #[test]
    fn directions_come_from_the_ghostty_variables_and_are_normalized() {
        let vars = [("KANSTACK_GHOSTTY_DIRECTION", Some("below")), ("KANSTACK_GHOSTTY_CHAIN_DIRECTION", Some("left"))];
        with_fake("directions", ARMS, &vars, |mut splitter, fake| {
            spawn(&mut splitter, "feat-a");
            spawn(&mut splitter, "feat-b");
            let calls = fake.calls();
            assert_eq!(calls[2].args[3], "down");
            assert_eq!(calls[3].args[3], "left");
        });
    }

    #[test]
    fn a_direction_ghostty_cannot_split_is_refused_before_anything_is_split() {
        let vars = [("KANSTACK_GHOSTTY_DIRECTION", Some("sideways"))];
        with_fake("bad-direction", ARMS, &vars, |mut splitter, fake| {
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("sideways"), "{err}");
            assert!(!fake.ops().contains(&"split".to_string()), "{:?}", fake.ops());
            assert!(!splitter.has_pane("feat-a"));
        });
    }

    #[test]
    fn the_launch_line_is_the_terminals_command_with_its_directory_title_and_environment() {
        with_fake("launch", ARMS, &[], |mut splitter, fake| {
            splitter.spawn_harness(Path::new("/my repo"), "feat-a", Some("do the thing")).unwrap();
            let split = &fake.calls()[2];
            let [cwd, command, _anchor, _direction, title, env @ ..] = split.args.as_slice() else {
                panic!("{split:?}")
            };
            assert_eq!(cwd, "/my repo");
            assert!(command.starts_with("/bin/sh -c 'cd '\\''/my repo'\\'' && "), "the launch line goes whole through sh -c: {command}");
            assert!(command.contains("claude") && command.contains("do the thing"), "{command}");
            assert_eq!(title, "feat-a");
            assert!(env.contains(&format!("PATH={}", std::env::var("PATH").unwrap())), "{env:?}");
            assert!(env.contains(&"KANSTACK_STATE_PATH=/state".to_string()), "{env:?}");
            assert!(
                env.iter().all(|kv| kv.starts_with("PATH=") || kv.starts_with("KANSTACK_")),
                "nothing else of the caller's environment goes along: {env:?}"
            );
            assert!(
                !env.iter().any(|kv| kv.starts_with("KANSTACK_BRANCH=")),
                "the caller's lane must not become the new lane's: {env:?}"
            );
        });
    }

    /// Ghostty on macOS runs a command as `bash -c "exec -l <command>"`, unquoted, so a launch
    /// line with `&&`, `cd` and quotes in it only survives if wrapped. This runs the very
    /// string handed to Ghostty through that same bash line (measured on Ghostty 1.3.1).
    #[test]
    fn the_command_survives_the_bash_line_ghostty_splices_it_into() {
        with_fake("bash-line", ARMS, &[], |mut splitter, fake| {
            let out = fake.dir.join("ran.txt");
            let launch = format!(
                "cd '/tmp' && printf '%s\\n' \"it's\" 'a \"b\" $c' \"$PWD\" > '{}'; echo done >> '{}'",
                out.display(),
                out.display()
            );
            splitter.set_anchor("OWN");
            let real = crate::harness_launch::shell_quote(&launch);
            assert_ne!(real, launch);
            let ghostty = fake.ghostty();
            ghostty
                .open_pane(&OpenRequest {
                    cwd: Path::new("/"),
                    title: "t",
                    launch: &launch,
                    after: Some("OWN"),
                    first_direction: "up",
                    chain_direction: "right",
                })
                .unwrap();
            let command = fake.calls().pop().unwrap().args[1].clone();
            assert!(command.starts_with("/bin/sh -c "), "{command}");
            let status = Command::new("/bin/bash")
                .args(["--noprofile", "--norc", "-c", &format!("exec -l {command}")])
                .status()
                .unwrap();
            assert!(status.success());
            // `cd '/tmp'` really ran (macOS reports /tmp as /private/tmp), and the quotes held.
            let ran = std::fs::read_to_string(&out).unwrap();
            let lines: Vec<&str> = ran.lines().collect();
            assert_eq!(&lines[..2], ["it's", "a \"b\" $c"], "{ran:?}");
            assert!(lines[2].ends_with("/tmp"), "{ran:?}");
            assert_eq!(lines[3], "done");
        });
    }

    /// The point of `argv`: what the caller controls never becomes script source.
    #[test]
    fn dynamic_values_reach_osascript_as_arguments_never_in_the_script() {
        with_fake("argv", ARMS, &[], |mut splitter, fake| {
            splitter.spawn_harness(Path::new("/it's a \"repo\""), "b\"r'anch\\ $(x)", Some("--flag 'q' \"d\"")).unwrap();
            splitter.send_task("b\"r'anch\\ $(x)", "-x \"quoted\" 'single' $HOME `tick` \\ done").unwrap();
            let log = stand_in::log_lines(&fake.log);
            assert_eq!(log.len(), 4, "list, list, split, type: {log:#?}");
            for line in &log {
                let script = line.split("end run").next().unwrap();
                for value in ["it's a", "repo", "branch", "$(x)", "--flag", "quoted", "$HOME", "tick"] {
                    assert!(!script.contains(value), "{value:?} leaked into the script source: {script}");
                }
                assert!(line.contains("end run --"), "arguments follow a `--` so a leading dash is not an option: {line}");
            }
            let type_call = fake.calls().into_iter().find(|c| c.op == "type").unwrap();
            assert_eq!(type_call.args, ["NEW-after-OWN", "-x \"quoted\" 'single' $HOME `tick` \\ done"]);
        });
    }

    #[test]
    fn every_script_refuses_to_launch_ghostty_when_it_isnt_running() {
        for (op, body) in [("probe", PROBE), ("list", LIST), ("split", SPLIT), ("type", TYPE), ("focus", FOCUS), ("close", CLOSE)] {
            let source = script(op, body);
            assert!(source.starts_with(&format!("-- kanstack: {op}\n")), "{source}");
            assert!(
                source.contains("if not (application id \"com.mitchellh.ghostty\" is running) then error"),
                "{op} would launch Ghostty:\n{source}"
            );
        }
    }

    #[test]
    fn typing_is_a_paste_and_then_its_own_enter_in_one_call() {
        with_fake("type", ARMS, &[], |mut splitter, fake| {
            spawn(&mut splitter, "feat-a");
            splitter.send_task("feat-a", "hello\nworld").unwrap();
            let call = fake.calls().pop().unwrap();
            assert_eq!(call, Call { op: "type".to_string(), args: vec!["NEW-after-OWN".to_string(), "hello".to_string(), "world".to_string()] },
                "the multi-line text is one argument (printed by the stand-in one line per argument)");
            let source = script("type", TYPE);
            let paste = source.find("input text payload to dest").unwrap();
            let enter = source.find("send key \"enter\" to dest").unwrap();
            assert!(paste < enter, "Enter follows the paste, and is its own key:\n{source}");
        });
    }

    #[test]
    fn focus_names_the_terminal() {
        with_fake("focus", ARMS, &[], |mut splitter, fake| {
            spawn(&mut splitter, "feat-a");
            splitter.focus("feat-a").unwrap();
            assert_eq!(fake.calls().pop().unwrap(), Call { op: "focus".to_string(), args: vec!["NEW-after-OWN".to_string()] });
        });
    }

    #[test]
    fn stopping_closes_the_terminal_and_a_terminal_already_gone_is_fine() {
        with_fake("stop", ARMS, &[], |mut splitter, fake| {
            spawn(&mut splitter, "feat-a");
            splitter.stop("feat-a").unwrap();
            assert_eq!(fake.calls().pop().unwrap(), Call { op: "close".to_string(), args: vec!["NEW-after-OWN".to_string()] });
        });
        let gone = format!(
            "{ARMS}\nclose) echo 'execution error: Ghostty got an error: Can’t get terminal 1 whose id = \"X\". Invalid index. (-1719)' >&2; exit 1 ;;"
        );
        with_fake("stop-gone", &gone, &[], |mut splitter, _| {
            spawn(&mut splitter, "feat-a");
            splitter.stop("feat-a").expect("a terminal that is already gone counts as closed");
        });
        let broken = format!("{ARMS}\nclose) echo 'execution error: something else broke (-10000)' >&2; exit 1 ;;");
        with_fake("stop-broken", &broken, &[], |mut splitter, _| {
            spawn(&mut splitter, "feat-a");
            let err = splitter.stop("feat-a").unwrap_err().to_string();
            assert!(err.contains("something else broke"), "{err}");
        });
    }

    #[test]
    fn probe_reports_a_missing_terminal_dead_and_says_nothing_about_one_that_is_there() {
        with_fake("probe", ARMS, &[], |_, fake| {
            let statuses = fake.ghostty().probe(&["OWN", "GONE", "NEW-after-OWN"]).unwrap();
            assert_eq!(statuses, HashMap::from([("GONE".to_string(), PaneStatus::Dead)]));
            assert_eq!(fake.ops(), ["probe"], "one call for all of them");
        });
    }

    #[test]
    fn probe_with_no_panes_never_calls_osascript() {
        with_fake("probe-none", ARMS, &[], |_, fake| {
            assert!(fake.ghostty().probe(&[]).unwrap().is_empty());
            assert!(fake.ops().is_empty());
        });
    }

    #[test]
    fn a_failed_probe_is_an_error_not_a_dead_pane() {
        let failing = "probe) echo 'execution error: boom (-10000)' >&2; exit 1 ;;";
        with_fake("probe-fails", failing, &[], |_, fake| {
            let err = fake.ghostty().probe(&["OWN"]).unwrap_err().to_string();
            assert!(err.contains("boom"), "{err}");
        });
    }

    #[test]
    fn a_probe_when_ghostty_has_no_terminals_left_reads_everything_dead() {
        with_fake("probe-empty", "probe) ;;", &[], |_, fake| {
            let statuses = fake.ghostty().probe(&["A", "B"]).unwrap();
            assert_eq!(statuses.len(), 2);
            assert!(statuses.values().all(|s| *s == PaneStatus::Dead));
        });
    }

    #[test]
    fn refused_automation_permission_says_where_to_grant_it() {
        let denied = "probe|list|split) echo 'execution error: Not authorized to send Apple events to Ghostty. (-1743)' >&2; exit 1 ;;";
        with_fake("denied", denied, &[], |mut splitter, fake| {
            let err = fake.ghostty().probe(&["OWN"]).unwrap_err().to_string();
            assert!(err.contains("System Settings > Privacy & Security > Automation"), "{err}");
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("Automation"), "{err}");
            assert!(!splitter.has_pane("feat-a"));
        });
    }

    #[test]
    fn a_ghostty_that_is_not_running_says_so() {
        let quit = "probe) echo 'execution error: Ghostty is not running (-600)' >&2; exit 1 ;;";
        with_fake("not-running", quit, &[], |_, fake| {
            let err = fake.ghostty().probe(&["OWN"]).unwrap_err().to_string();
            assert!(err.contains("isn't running"), "{err}");
        });
    }

    #[test]
    fn a_marker_that_never_appears_fails_the_first_lane_and_tracks_nothing() {
        let never = "list) printf 'OWN\\tzsh\\n' ;;\nsplit) echo NEW ;;";
        with_fake("no-marker", never, &[], |mut splitter, fake| {
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("which Ghostty terminal is kanstack's own"), "{err}");
            assert!(!fake.ops().contains(&"split".to_string()));
            assert!(!splitter.has_pane("feat-a"));
        });
    }

    #[test]
    fn an_unwritable_controlling_terminal_fails_the_first_lane_with_that_reason() {
        with_fake("no-tty", ARMS, &[], |_, fake| {
            let mut ghostty = fake.ghostty();
            ghostty.tty = fake.dir.join("no-such-dir/tty");
            let err = format!("{:#}", ghostty.own_terminal().unwrap_err());
            assert!(err.contains("controlling terminal"), "{err}");
        });
    }

    #[test]
    fn a_split_that_answers_nothing_is_an_error() {
        with_fake("empty-split", "list) printf 'OWN\\tzsh\\n' ;;\nsplit) ;;", &[], |mut splitter, _| {
            // This stand-in never echoes the marker, so chain off a known terminal instead.
            splitter.set_anchor("OWN");
            let err = splitter.spawn_harness(Path::new("/repo"), "feat-a", None).unwrap_err().to_string();
            assert!(err.contains("did not report a terminal id"), "{err}");
            assert!(!splitter.has_pane("feat-a"));
        });
    }

    #[test]
    fn ghostty_tracks_the_harness_process_itself_and_places_panes_like_tmux() {
        with_fake("traits", ARMS, &[], |splitter, fake| {
            let ghostty = fake.ghostty();
            assert!(ghostty.tracks_pids(), "a pane outlives its command, so Ghostty cannot say it finished");
            assert_eq!(ghostty.name(), "ghostty");
            assert_eq!(ghostty.default_directions(), ("up", "right"));
            assert_eq!(splitter.label(), "ghostty");
            assert!(splitter.running_inside_host());
        });
    }

    #[test]
    fn explain_puts_known_failures_in_words() {
        assert!(explain("split", "execution error: Not authorized to send Apple events to Ghostty. (-1743)")
            .contains("System Settings > Privacy & Security > Automation"));
        assert!(explain("split", "execution error: Ghostty is not running (-600)").contains("isn't running"));
        assert!(explain("split", "Application isn't running. (-600)").contains("isn't running"));
        // Verbatim from `osascript` with the app quit and no installed copy (-1728, too).
        let gone = "176:214: syntax error: Can’t get application id \"com.mitchellh.ghostty\". (-1728)";
        assert!(explain("focus", gone).contains("isn't running"), "not read as a missing terminal");
        assert!(explain("close", "Can’t get terminal 1 whose id = \"X\". Invalid index. (-1719)").contains(NO_SUCH_TERMINAL));
        assert!(explain("split", "syntax error: A identifier can't go after this identifier. (-2740)").contains("1.3"));
        assert_eq!(explain("focus", ""), "Ghostty focus failed");
        assert_eq!(explain("focus", "odd\n"), "Ghostty focus failed: odd");
    }

    fn inside(vars: &[(&str, Option<&str>)]) -> bool {
        let mut all = vec![("TERM_PROGRAM", None), ("CMUX_SURFACE_ID", None), ("TMUX_PANE", None)];
        all.retain(|(k, _)| !vars.iter().any(|(v, _)| v == k));
        all.extend_from_slice(vars);
        let mut answer = false;
        stand_in::with_env(&all, || answer = Ghostty::running_inside());
        answer
    }

    /// cmux, a Ghostty fork, sets `TERM_PROGRAM=ghostty` too, and tmux inside Ghostty should
    /// still get tmux; neither is Ghostty as far as this backend goes.
    #[test]
    fn running_inside_needs_ghostty_and_neither_cmux_nor_tmux() {
        assert!(inside(&[("TERM_PROGRAM", Some("ghostty"))]));
        assert!(!inside(&[]));
        assert!(!inside(&[("TERM_PROGRAM", Some("Apple_Terminal"))]));
        assert!(!inside(&[("TERM_PROGRAM", Some("iTerm.app"))]));
        assert!(!inside(&[("TERM_PROGRAM", Some("ghostty")), ("CMUX_SURFACE_ID", Some("ABC"))]));
        assert!(!inside(&[("TERM_PROGRAM", Some("ghostty")), ("TMUX_PANE", Some("%1"))]));
    }

    #[test]
    fn locate_needs_osascript_or_an_override_and_being_inside_ghostty() {
        let dir = std::env::temp_dir().join(format!("kanstack-ghostty-locate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let empty_path = dir.to_str().unwrap().to_string();
        fn base<'a>(path: &'a str, extra: &[(&'a str, Option<&'a str>)]) -> Vec<(&'a str, Option<&'a str>)> {
            let mut vars = vec![
                ("TERM_PROGRAM", Some("ghostty")),
                ("CMUX_SURFACE_ID", None),
                ("TMUX_PANE", None),
                ("PATH", Some(path)),
                ("KANSTACK_OSASCRIPT_BIN", None),
            ];
            vars.retain(|(k, _)| !extra.iter().any(|(e, _)| e == k));
            vars.extend_from_slice(extra);
            vars
        }
        let path = empty_path.as_str();
        stand_in::with_env(&base(path, &[]), || assert!(Ghostty::locate().is_none(), "no osascript on PATH"));
        stand_in::with_env(&base(path, &[("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/osascript"))]), || {
            assert!(Ghostty::locate().is_some(), "an override is trusted without checking it exists");
        });
        stand_in::with_env(
            &base(path, &[("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/osascript")), ("TERM_PROGRAM", Some("iTerm.app"))]),
            || assert!(Ghostty::locate().is_none(), "an override doesn't make up for not being in Ghostty"),
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Off macOS there is no Ghostty to script, whatever the environment says.
    #[test]
    fn discover_is_macos_only() {
        stand_in::with_env(
            &[
                ("TERM_PROGRAM", Some("ghostty")),
                ("CMUX_SURFACE_ID", None),
                ("TMUX_PANE", None),
                ("KANSTACK_OSASCRIPT_BIN", Some("/nonexistent/osascript")),
            ],
            || assert_eq!(Ghostty::discover().is_some(), cfg!(target_os = "macos")),
        );
    }

    #[test]
    fn the_setup_wizards_line_says_what_to_do() {
        stand_in::with_env(&[("TERM_PROGRAM", Some("Apple_Terminal")), ("CMUX_SURFACE_ID", None), ("TMUX_PANE", None)], || {
            let line = detection();
            assert!(line.starts_with('✗') && line.to_lowercase().contains("ghostty"), "{line}");
            assert!(line.contains("macOS only") || line.contains("TERM_PROGRAM"), "{line}");
        });
    }
}
