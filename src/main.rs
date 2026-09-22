//! kanstack — an unofficial kanban-style terminal UI for the GitButler CLI.
//!
//! Not affiliated with or endorsed by GitButler Inc. This program links no GitButler
//! code; it spawns the `but` binary you installed and reads its documented JSON output.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind,
};
use ratatui::crossterm::execute;

use kanstack::app::{App, Mode, Notice};
use kanstack::but::But;
use kanstack::splitter::Splitter;
use kanstack::tutorial::{self, Tutorial};
use kanstack::watch::Watcher;
use kanstack::workstream::Registry;
use kanstack::{cli, config, setup, snapshot, ui};

const HELP: &str = "\
kanstack — a kanban-style TUI for the GitButler CLI

usage:
  kanstack [options]
  kanstack spawn|send|status|focus|stop ...   drive harness panes without the board:

  kanstack spawn <branch> [--agent <name>] [--prompt \"...\"] [--item <ref>] [--above|--below <base>]
  kanstack send <branch|session> \"...\"
  kanstack status
  kanstack focus <branch|session>
  kanstack stop <branch|session>
  (kanstack spawn --help for details)

options:
  -C <path>          run against the repository at <path> (default: cwd)
  --setup            detect but, your terminal multiplexers and known harnesses, then save a default
                     harness and split-backend, so they don't need exporting every session.
                     Also offers to install the GitButler CLI itself if missing, and to
                     install/update its coding-agent skill (teaches whichever harness you
                     spawn to use `but` instead of plain git) if missing or outdated. Runs
                     automatically before the board the very first time kanstack is
                     run (skippable with esc), followed by a one-time, also-skippable offer
                     to walk through the keys — neither reappears once you've been through it
  --tutorial         walk through the keys in a real, throwaway practice repo
  --snapshot <file>  render captured `but status -f --json` output and exit
  --size <WxH>       terminal size for --snapshot (default: 160x30)
  -V, --version      print version
  -h, --help         print this help

environment:
  Anything set with --setup is remembered in $XDG_CONFIG_HOME/kanstack/env (or
  $HOME/.config/kanstack/env — KANSTACK_CONFIG_PATH to relocate it) and used as a default
  the next time kanstack runs. An explicit environment variable always wins over the saved
  file.

  KANSTACK_BUT_BIN   path to the `but` binary (default: `but` on PATH)
  KANSTACK_CMUX_BIN  path to the `cmux` CLI, to open a terminal split for each new
                     parallel lane (default: `cmux` on PATH if present; the integration is
                     silently skipped otherwise). Skip it for one branch with shift-tab
                     while naming it; stacking a branch never opens one to begin with.
  KANSTACK_TMUX_BIN  path to `tmux`, used instead of cmux when cmux isn't found and
                     kanstack is itself running inside a tmux pane (default: `tmux` on
                     PATH if present and `$TMUX_PANE` is set; skipped otherwise, same as
                     cmux). Splits off kanstack's own pane the same way cmux does.
  KANSTACK_ORCA_BIN  path to the `orca` CLI (`orca-ide` on Linux), used when neither cmux
                     nor tmux is and kanstack is itself running inside an Orca terminal
                     (default: `orca` on PATH if present and `$ORCA_TERMINAL_HANDLE` is
                     set; skipped otherwise). Orca's CLI is only on PATH once registered
                     under Settings in the app. Lanes attach to the Orca worktree
                     kanstack is in — kanstack never runs `orca worktree create`, since
                     every lane shares GitButler's one workspace checkout — so the
                     repository needs to be added to Orca.
  KANSTACK_OSASCRIPT_BIN  path to `osascript`, which is how the `ghostty` backend (macOS
                     only) drives the Ghostty app, used when kanstack is itself running in a
                     Ghostty window (default: `osascript` on PATH if present and
                     `TERM_PROGRAM` is `ghostty` with no cmux or tmux pane; skipped
                     otherwise). macOS asks once to let the app running kanstack control
                     Ghostty (System Settings > Privacy & Security > Automation). Ghostty
                     can't say whether a pane is busy or idle, and a pane stays open after
                     its harness exits, so status there comes from what agents report and
                     from tracking the harness's process
  KANSTACK_SPLIT_BACKEND  force which of the above is used: `cmux`, `tmux`, `orca` or
                     `ghostty`, skipping detection of the others entirely rather than just
                     reordering the fallback (default: unset — cmux if found, else tmux,
                     else orca, else ghostty; except that inside an Orca terminal but not a
                     cmux or tmux pane, orca goes first, and likewise ghostty inside plain
                     Ghostty). Mainly for a machine with several installed where the first
                     isn't actually the one kanstack is running inside right now.
  KANSTACK_TRACK_PIDS  whether kanstack follows each pane's shell process itself, for its
                     busy/idle/dead status: the pane's shell writes its pid before it starts
                     the harness, and `ps` is read for it. Off unless the multiplexer can't
                     say for itself (Ghostty); `1`, `true`, `on` or `yes` turns it on for any
                     of them, `0`, `false`, `off` or `no` turns it off. It sees the shell,
                     not the harness: a finished harness reads idle until the shell exits.
  KANSTACK_HARNESS   command typed into that terminal (default: `claude`)
  KANSTACK_HARNESS_SYSTEM_FLAG  the harness's flag for appending to its own default
                     system prompt, e.g. `--append-system-prompt` (default: whatever's
                     known-good for KANSTACK_HARNESS — `--append-system-prompt` for
                     `claude`/`pi`, Codex CLI's differently-shaped `-c
                     developer_instructions=` config override for `codex`, and for
                     anything else — including harnesses confirmed to have no such
                     mechanism at all, like OpenCode, Kiro CLI, and Gemini CLI — falling
                     back to folding the note directly into the initial message instead).
                     Set explicitly to force a `<flag> <value>`
                     style override for another harness, or to an empty string to opt out
                     entirely. Every new parallel-lane harness is told it's running in a
                     GitButler *virtual* branch this way, since `git status`/`git branch`
                     only ever show the shared workspace checkout every lane sits on, not
                     that branch specifically.
  KANSTACK_CMUX_DIRECTION  split direction for the first lane, off kanstack's own pane:
                           left, right, above, or below (default: `above`)
  KANSTACK_CMUX_CHAIN_DIRECTION  split direction for every lane after the first, off the
                           previous lane instead of kanstack (default: `right`)
  KANSTACK_TMUX_DIRECTION, KANSTACK_TMUX_CHAIN_DIRECTION  the tmux fallback's equivalents
                           of the two above, same defaults
  KANSTACK_ORCA_DIRECTION, KANSTACK_ORCA_CHAIN_DIRECTION  orca's equivalents, defaulting to
                           `below` and `right`. Orca can only put a new pane right of or
                           below the one it splits, so `left` and `above` behave as `right`
                           and `below` there (as does KANSTACK_SPAWN_DIRECTION)
  KANSTACK_GHOSTTY_DIRECTION, KANSTACK_GHOSTTY_CHAIN_DIRECTION  ghostty's equivalents, with
                           the same defaults as tmux's (`above` and `right`); Ghostty splits
                           in all four directions
  KANSTACK_SPAWN_DIRECTION  where `kanstack spawn` splits off the pane it is run from:
                           left, right, above, or below (default: `right`). Separate from
                           the two above, which are the board's own; the chain direction
                           for every lane after the first is shared
  KANSTACK_BRANCH_UI  how `b`'s branch-name/initial-message prompts are presented: `modal`
                      (default), a dedicated box showing the name, the pending action, and
                      the message all together, or `footer`, squeezed into the one-line
                      footer like every other prompt
  KANSTACK_STACK_PANES  how `kanstack spawn` places a pane for a branch stacked on top of
                      one that already has a pane open: grouped with the sibling as a real
                      tab (`tabbed`, the default; tmux and cmux so far — the others fall
                      back to the split below) or split off it (`split`), in the direction
                      orthogonal to the ordinary chain direction so the pair reads as its
                      own cluster rather than continuing the chain

capture a snapshot for a bug report:
  but status -f --json > board.json && kanstack --snapshot board.json

requires the GitButler CLI: https://docs.gitbutler.com/cli-overview
";

fn main() -> Result<()> {
    // Before anything else reads an env var: fills in whatever `--setup` saved last time,
    // for any var not already set in the real environment (which always wins — see the
    // `environment:` note in HELP).
    config::load_into_env();

    let mut cwd: Option<PathBuf> = None;
    let mut snapshot: Option<String> = None;
    let mut size = (160u16, 30u16);
    let mut tutorial_mode = false;
    let mut setup_mode = false;
    let mut subcommand: Option<cli::Command> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            name if cli::SUBCOMMANDS.contains(&name) => {
                let raw: Vec<String> = args.by_ref().collect();
                match cli::parse(name, raw.clone()) {
                    Ok(Some(command)) => subcommand = Some(command),
                    Ok(None) => {
                        print!("{}", cli::HELP);
                        return Ok(());
                    }
                    // No `Command` exists yet to carry `--json` (parsing is what failed), so
                    // this reads the raw argument list for it directly, same as `parse` itself
                    // will once it gets far enough to recognize the flag.
                    Err(e) => {
                        let json = raw.iter().any(|a| a == "--json" || a.starts_with("--json="));
                        // `parse`'s own messages end with the full multi-subcommand `HELP` text
                        // (a `bail!("...\n\n{HELP}")` pattern) — the right thing on stderr, but
                        // not something a JSON error's `message` field should repeat in full;
                        // trim it there and keep the short reason. Human mode is untouched.
                        let e = if json {
                            let short = e.to_string();
                            let short = short.strip_suffix(&format!("\n\n{}", cli::HELP)).unwrap_or(&short).to_string();
                            anyhow::anyhow!(short)
                        } else {
                            e
                        };
                        std::process::exit(cli::report_error(
                            name,
                            cli::invalid_arguments(e),
                            json,
                            &mut std::io::stdout(),
                            &mut std::io::stderr(),
                        ));
                    }
                }
            }
            "--tutorial" => tutorial_mode = true,
            "--setup" => setup_mode = true,
            "--snapshot" => {
                snapshot = Some(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("--snapshot requires a file"))?,
                );
            }
            "--size" => {
                let raw = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--size requires WxH, e.g. 120x30"))?;
                let (w, h) = raw
                    .split_once(['x', 'X'])
                    .ok_or_else(|| anyhow::anyhow!("--size must look like 120x30"))?;
                size = (w.trim().parse()?, h.trim().parse()?);
            }
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("kanstack {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-C" => {
                cwd = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| anyhow::anyhow!("-C requires a path"))?,
                ));
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{HELP}"),
        }
    }

    // Headless: no board, no first-run wizard, no terminal needed — so this comes before
    // every check below that assumes one. Exits directly, rather than returning through
    // `Result<()>`, so a failure can carry a more specific code than the flat `1` every error
    // here used to get — see `cli::dispatch` and the exit-code table in the README.
    if let Some(command) = subcommand {
        let cwd = match cwd {
            Some(p) => p,
            None => std::env::current_dir()?,
        };
        std::process::exit(cli::dispatch(command, &cwd, &mut std::io::stdout(), &mut std::io::stderr()));
    }

    if let Some(path) = snapshot {
        print!("{}", snapshot::render_file(&path, size.0, size.1)?);
        return Ok(());
    }

    if tutorial_mode && cwd.is_some() {
        anyhow::bail!("--tutorial builds its own practice repo; -C does not apply with it");
    }

    // `ratatui::init()` panics rather than erroring when there is no terminal to attach
    // to, which is what happens under a pipe, a CI job, or `kanstack > out.txt`. Check first
    // so that turns into an explanation instead of a backtrace.
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "not running in a terminal, so there is no board to draw.\n\
             To render a captured board non-interactively:\n\
             \n    but status -f --json > board.json && kanstack --snapshot board.json"
        );
    }

    // The very first run on a machine — no config file yet, and neither --tutorial nor an
    // explicit --setup already covers it — gets the wizard, then a dismissible offer to
    // walk through the keys, before the board. Both are skippable; skipping setup still
    // marks first-run done (via the empty `save`) so neither reappears next launch.
    let first_run = !setup_mode && !tutorial_mode && !config::exists();
    if setup_mode || first_run {
        let outcome = setup::run()?;
        if first_run && outcome == setup::Outcome::Cancelled {
            config::save(&[])?;
        }
        if setup_mode {
            return Ok(());
        }
        // Skipped when `-C` named a specific repository: building a throwaway practice
        // repo instead would silently ignore it.
        if cwd.is_none() && tutorial::offer()? {
            tutorial_mode = true;
        }
    }

    let cwd = if tutorial_mode {
        tutorial::build_practice_repo()?
    } else {
        match cwd {
            Some(p) => p,
            None => std::env::current_dir()?,
        }
    };

    // Everything else that can fail with a readable message happens before the alternate
    // screen is entered, so errors are not wiped by the terminal restore.
    let but = But::discover(&cwd)?;
    let mut splitter = Splitter::discover();
    // Panes the subcommands (or an earlier run of the board) opened: track them too, so
    // their status shows and `t` reaches them. A registry that won't load isn't worth
    // refusing to start the board over.
    if let (Some(splitter), Ok(registry)) = (splitter.as_mut(), Registry::load(&cwd)) {
        registry.adopt_into(splitter);
    }
    let mut app = App::new(but, splitter)?;
    if tutorial_mode {
        app.tutorial = Some(Tutorial::new());
    }

    // A failed watch is not fatal: the board still works, it just stops following the
    // repository on its own. Say so rather than dying or silently going stale.
    let mut watcher = match Watcher::new(&cwd) {
        Ok(w) => Some(w),
        Err(e) => {
            app.notify(
                format!("not watching for changes ({e}); the board may go stale"),
                Notice::Error,
            );
            None
        }
    };

    let mut terminal = ratatui::init();
    // Best-effort: a terminal that doesn't understand mouse reporting just never sends
    // `Event::Mouse`, so the board still works with the keyboard alone if this fails.
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    // Also best-effort. Without it a paste arrives as one key press per character, so a
    // newline in it is an Enter that submits the form mid-paste; with it, the paste is one
    // `Event::Paste`. A terminal that ignores this simply keeps the old behavior.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = run(&mut terminal, &mut app, &mut watcher);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    // After the restore, so it lands on the real screen rather than the one being torn
    // down. The teardown recovery reports what it did this way — it exits GitButler mode,
    // so there is no board left to show it on.
    if let Some(note) = &app.exit_note {
        println!("{note}");
    }
    result
}

fn run(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    watcher: &mut Option<Watcher>,
) -> Result<()> {
    loop {
        terminal.draw(|f| {
            // Read before drawing so Shift+←/→ paging always knows how many lanes the
            // frame it's reacting to actually showed.
            app.terminal_width = f.area().width;
            // Stashed so the next mouse event can be resolved against exactly what's on
            // screen right now — `ui::draw` takes `&App`, so it hands the map back rather
            // than recording it directly.
            app.hit_map = ui::draw(f, app);
        })?;

        // Short timeout so the watcher gets looked at promptly; it is the thing that keeps
        // the board current, which is why there is no refresh key.
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                Event::Mouse(mouse) => app.on_mouse(mouse),
                Event::Paste(text) => app.on_paste(&text),
                Event::Resize(_, _) => {}
                _ => {}
            }
            if app.should_quit {
                return Ok(());
            }
        }

        // Non-blocking: applies the result the moment a background `but land` finishes,
        // and advances its spinner in the meantime. Runs every tick regardless of
        // whether a key came in, so the spinner animates even while idle.
        app.poll_land();

        // Same idea for a background `but pr new`: apply the result the moment it
        // finishes, and advance its spinner in the meantime.
        app.poll_pr();

        // Same idea for a watcher-triggered refresh: it starts here but finishes on its
        // own thread, so a save in another window never stalls navigation waiting on it.
        app.poll_background_refresh();

        // Same pattern again for split-pane liveness: apply a finished poll, then kick off
        // the next one if it's due.
        app.poll_split();
        app.maybe_begin_split_poll();

        // Don't yank the board out from under a move in progress.
        if app.mode != Mode::Moving {
            if let Some(w) = watcher.as_mut() {
                if w.poll() {
                    app.begin_background_refresh();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HELP;
    use kanstack::splitter::BACKENDS;

    #[test]
    fn help_documents_process_tracking() {
        assert!(HELP.contains("KANSTACK_TRACK_PIDS"));
    }

    /// `--help` documents each backend by hand, since its variables differ, so a new one
    /// must be added there — this fails until it is.
    #[test]
    fn help_documents_every_backend_and_its_direction_variables() {
        for backend in BACKENDS {
            let upper = backend.name.to_ascii_uppercase();
            assert!(HELP.contains(&format!("`{}`", backend.name)), "--help never names the `{}` backend", backend.name);
            for suffix in ["DIRECTION", "CHAIN_DIRECTION"] {
                let var = format!("KANSTACK_{upper}_{suffix}");
                assert!(HELP.contains(&var), "--help never documents {var}");
            }
        }
    }
}
