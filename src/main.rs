//! kanstack — an unofficial kanban-style terminal UI for the GitButler CLI.
//!
//! Not affiliated with or endorsed by GitButler Inc. This program links no GitButler
//! code; it spawns the `but` binary you installed and reads its documented JSON output.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use kanstack::app::App;
use kanstack::but::But;
use kanstack::{snapshot, ui};

const HELP: &str = "\
kanstack — a kanban-style TUI for the GitButler CLI

usage:
  kanstack [options]

options:
  -C <path>          run against the repository at <path> (default: cwd)
  --snapshot <file>  render captured `but status -f -j` output and exit
  --size <WxH>       terminal size for --snapshot (default: 160x30)
  -V, --version      print version
  -h, --help         print this help

environment:
  KANSTACK_BUT_BIN  path to the `but` binary (default: `but` on PATH)

capture a snapshot for a bug report:
  but status -f -j > board.json && kanstack --snapshot board.json

requires the GitButler CLI: https://docs.gitbutler.com/cli-overview
";

fn main() -> Result<()> {
    let mut cwd: Option<PathBuf> = None;
    let mut snapshot: Option<String> = None;
    let mut size = (160u16, 30u16);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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

    let cwd = match cwd {
        Some(p) => p,
        None => std::env::current_dir()?,
    };

    // `ratatui::init()` panics rather than erroring when there is no terminal to attach
    // to, which is what happens under a pipe, a CI job, or `kanstack > out.txt`. Check first
    // so that turns into an explanation instead of a backtrace.
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "not running in a terminal, so there is no board to draw.\n\
             To render a captured board non-interactively:\n\
             \n    but status -f -j > board.json && kanstack --snapshot board.json"
        );
    }

    // Everything else that can fail with a readable message happens before the alternate
    // screen is entered, so errors are not wiped by the terminal restore.
    let but = But::discover(&cwd)?;
    let mut app = App::new(but)?;

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // A poll timeout keeps the loop responsive to resizes without busy-waiting.
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
            Event::Resize(_, _) => {}
            _ => {}
        }
        if app.should_quit {
            return Ok(());
        }
    }
}
