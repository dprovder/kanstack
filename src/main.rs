//! kanstack — an unofficial kanban-style terminal UI for the GitButler CLI.
//!
//! Not affiliated with or endorsed by GitButler Inc. This program links no GitButler
//! code; it spawns the `but` binary you installed and reads its documented JSON output.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use kanstack::app::{App, Mode, Notice};
use kanstack::but::But;
use kanstack::cmux::Cmux;
use kanstack::tutorial::{self, Tutorial};
use kanstack::watch::Watcher;
use kanstack::{snapshot, ui};

const HELP: &str = "\
kanstack — a kanban-style TUI for the GitButler CLI

usage:
  kanstack [options]

options:
  -C <path>          run against the repository at <path> (default: cwd)
  --tutorial         walk through the keys in a real, throwaway practice repo
  --snapshot <file>  render captured `but status -f --json` output and exit
  --size <WxH>       terminal size for --snapshot (default: 160x30)
  -V, --version      print version
  -h, --help         print this help

environment:
  KANSTACK_BUT_BIN   path to the `but` binary (default: `but` on PATH)
  KANSTACK_CMUX_BIN  path to the `cmux` CLI, to open a terminal split for each new
                     parallel lane (default: `cmux` on PATH if present; the integration is
                     silently skipped otherwise). Skip it for one branch with shift-tab
                     while naming it; stacking a branch never opens one to begin with.
  KANSTACK_HARNESS   command typed into that terminal (default: `claude`)
  KANSTACK_CMUX_DIRECTION  split direction for the first lane, off kanstack's own pane:
                           left, right, above, or below (default: `above`)
  KANSTACK_CMUX_CHAIN_DIRECTION  split direction for every lane after the first, off the
                           previous lane instead of kanstack (default: `right`)

capture a snapshot for a bug report:
  but status -f --json > board.json && kanstack --snapshot board.json

requires the GitButler CLI: https://docs.gitbutler.com/cli-overview
";

fn main() -> Result<()> {
    let mut cwd: Option<PathBuf> = None;
    let mut snapshot: Option<String> = None;
    let mut size = (160u16, 30u16);
    let mut tutorial_mode = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tutorial" => tutorial_mode = true,
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
    let cmux = Cmux::discover();
    let mut app = App::new(but, cmux)?;
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
    let result = run(&mut terminal, &mut app, &mut watcher);
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
            ui::draw(f, app);
        })?;

        // Short timeout so the watcher gets looked at promptly; it is the thing that keeps
        // the board current, which is why there is no refresh key.
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
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

        // Same idea for a watcher-triggered refresh: it starts here but finishes on its
        // own thread, so a save in another window never stalls navigation waiting on it.
        app.poll_background_refresh();

        // Same pattern again for cmux pane liveness: apply a finished poll, then kick off
        // the next one if it's due.
        app.poll_cmux();
        app.maybe_begin_cmux_poll();

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
