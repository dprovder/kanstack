//! Interactive first-run/`--setup` wizard: detects `but`/`cmux`/`tmux`, and lets you pick
//! the handful of environment variables that matter most for a new install
//! (`KANSTACK_HARNESS`, `KANSTACK_SPLIT_BACKEND`, `KANSTACK_BRANCH_UI`), then persists them
//! via `crate::config` so they don't need to be re-exported every session. Every other
//! `KANSTACK_*` var stays an env-var-only power-user override, documented in `--help`
//! rather than surfaced as a field here.
//!
//! Runs standalone — no `But::discover(&cwd)` gate, no repository, no `App` — so it still
//! works usefully on a machine where `but` isn't installed yet; that's the point of the
//! detection panel. It owns its own terminal session rather than plugging into `App`'s
//! `Mode`/`ui::draw` machinery, the same way `--snapshot` renders and exits on its own
//! rather than joining the board's event loop.

use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::but::But;
use crate::cmux::Cmux;
use crate::text_input::TextInput;
use crate::theme;
use crate::tmux::Tmux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitBackendChoice {
    Auto,
    Cmux,
    Tmux,
}

impl SplitBackendChoice {
    fn label(self) -> &'static str {
        match self {
            SplitBackendChoice::Auto => "auto",
            SplitBackendChoice::Cmux => "cmux",
            SplitBackendChoice::Tmux => "tmux",
        }
    }

    fn cycle(self, forward: bool) -> Self {
        use SplitBackendChoice::*;
        match (self, forward) {
            (Auto, true) => Cmux,
            (Cmux, true) => Tmux,
            (Tmux, true) => Auto,
            (Auto, false) => Tmux,
            (Cmux, false) => Auto,
            (Tmux, false) => Cmux,
        }
    }

    fn from_env() -> Self {
        match std::env::var("KANSTACK_SPLIT_BACKEND").as_deref() {
            Ok("cmux") => SplitBackendChoice::Cmux,
            Ok("tmux") => SplitBackendChoice::Tmux,
            _ => SplitBackendChoice::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchUiChoice {
    Modal,
    Footer,
}

impl BranchUiChoice {
    fn label(self) -> &'static str {
        match self {
            BranchUiChoice::Modal => "modal",
            BranchUiChoice::Footer => "footer",
        }
    }

    fn cycle(self) -> Self {
        match self {
            BranchUiChoice::Modal => BranchUiChoice::Footer,
            BranchUiChoice::Footer => BranchUiChoice::Modal,
        }
    }

    fn from_env() -> Self {
        match std::env::var("KANSTACK_BRANCH_UI").as_deref() {
            Ok("footer") => BranchUiChoice::Footer,
            _ => BranchUiChoice::Modal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Harness,
    SplitBackend,
    BranchUi,
    Save,
}

impl Row {
    fn down(self) -> Self {
        match self {
            Row::Harness => Row::SplitBackend,
            Row::SplitBackend => Row::BranchUi,
            Row::BranchUi => Row::Save,
            Row::Save => Row::Harness,
        }
    }

    fn up(self) -> Self {
        match self {
            Row::Harness => Row::Save,
            Row::SplitBackend => Row::Harness,
            Row::BranchUi => Row::SplitBackend,
            Row::Save => Row::BranchUi,
        }
    }
}

/// Every harness `harness_launch::resolve_note_delivery` knows a specific delivery
/// mechanism for, in the same order it lists them in — checked against `PATH` to build
/// both the detection line and the default `KANSTACK_HARNESS` value below. `kiro-cli` is
/// left out of the scan despite being recognized there: it's the same binary as `kiro`
/// under an alternate name, and listing it separately here would just double-count one
/// install as two.
const KNOWN_HARNESSES: &[&str] = &["claude", "codex", "pi", "opencode", "kiro", "gemini"];

/// Whether `name` is on `PATH` — same PATH-search `cmux.rs`/`tmux.rs` each already do for
/// their own binary, just not shared with them since neither exposes it publicly.
fn harness_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(name).is_file())
}

fn detect_harnesses() -> Vec<&'static str> {
    KNOWN_HARNESSES.iter().copied().filter(|name| harness_on_path(name)).collect()
}

/// Read-only preview of what a normal launch would find — reuses the exact same
/// `discover`/`--version` calls `main.rs` itself makes, just without bailing the process
/// when one comes up empty.
struct Detection {
    but: String,
    harness: String,
    cmux: String,
    tmux: String,
}

fn detect() -> Detection {
    let but = match But::discover(&std::env::current_dir().unwrap_or_default()) {
        Ok(b) => format!("✓ but {} found", b.version()),
        Err(e) => format!("✗ {e}"),
    };
    let harness = match detect_harnesses().as_slice() {
        [] => "✗ no known harness found on PATH — type one below anyway if it's just not on PATH here".to_string(),
        found => format!("✓ found on PATH: {}", found.join(", ")),
    };
    let cmux = match Cmux::discover() {
        Some(_) => "✓ cmux found on PATH".to_string(),
        None => "✗ cmux not found — set KANSTACK_CMUX_BIN to point at it".to_string(),
    };
    let tmux = match Tmux::discover() {
        Some(_) => "✓ tmux found, and this pane is inside one".to_string(),
        None if std::env::var_os("TMUX_PANE").is_none() => {
            "✗ tmux not usable here — not running inside a tmux pane".to_string()
        }
        None => "✗ tmux binary not found on PATH".to_string(),
    };
    Detection { but, harness, cmux, tmux }
}

struct Wizard {
    detection: Detection,
    row: Row,
    harness: TextInput,
    split_backend: SplitBackendChoice,
    branch_ui: BranchUiChoice,
}

impl Wizard {
    fn new() -> Self {
        let detection = detect();
        // `KANSTACK_HARNESS` wins if already set (matches every other field here); failing
        // that, the first installed harness found beats a blind guess of "claude" — see
        // `KNOWN_HARNESSES` for the priority order among several installed at once.
        let default_harness = std::env::var("KANSTACK_HARNESS").ok().unwrap_or_else(|| {
            detect_harnesses().first().map(|s| s.to_string()).unwrap_or_else(|| "claude".to_string())
        });
        let mut harness = TextInput::default();
        harness.set(default_harness);
        Wizard {
            detection,
            row: Row::Harness,
            harness,
            split_backend: SplitBackendChoice::from_env(),
            branch_ui: BranchUiChoice::from_env(),
        }
    }

    /// Only writes a field when it differs from "auto"/default, so a knob left untouched
    /// keeps auto-detecting on future runs rather than getting pinned to today's result.
    fn save(&self) -> Result<()> {
        let mut fields: Vec<(&str, String)> = vec![("KANSTACK_HARNESS", self.harness.trimmed())];
        if self.split_backend != SplitBackendChoice::Auto {
            fields.push(("KANSTACK_SPLIT_BACKEND", self.split_backend.label().to_string()));
        }
        if self.branch_ui != BranchUiChoice::Modal {
            fields.push(("KANSTACK_BRANCH_UI", self.branch_ui.label().to_string()));
        }
        crate::config::save(&fields)
    }
}

/// Runs the wizard in its own terminal session and returns once the user saves or cancels.
pub fn run() -> Result<()> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    use ratatui::crossterm::event::{self, Event, KeyCode as K, KeyEventKind};

    let mut wizard = Wizard::new();
    loop {
        terminal.draw(|f| draw(f, &wizard))?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            K::Esc => return Ok(()),
            K::Down | K::Tab => wizard.row = wizard.row.down(),
            K::Up | K::BackTab => wizard.row = wizard.row.up(),
            K::Enter if wizard.row == Row::Save => {
                wizard.save()?;
                return Ok(());
            }
            K::Right if wizard.row == Row::SplitBackend => {
                wizard.split_backend = wizard.split_backend.cycle(true);
            }
            K::Left if wizard.row == Row::SplitBackend => {
                wizard.split_backend = wizard.split_backend.cycle(false);
            }
            K::Enter if wizard.row == Row::SplitBackend => {
                wizard.split_backend = wizard.split_backend.cycle(true);
            }
            K::Left | K::Right | K::Enter if wizard.row == Row::BranchUi => {
                wizard.branch_ui = wizard.branch_ui.cycle();
            }
            K::Left if wizard.row == Row::Harness => wizard.harness.move_left(),
            K::Right if wizard.row == Row::Harness => wizard.harness.move_right(),
            K::Home if wizard.row == Row::Harness => wizard.harness.move_home(),
            K::End if wizard.row == Row::Harness => wizard.harness.move_end(),
            K::Backspace if wizard.row == Row::Harness => wizard.harness.backspace(),
            K::Delete if wizard.row == Row::Harness => wizard.harness.delete_forward(),
            K::Char(c) if wizard.row == Row::Harness => wizard.harness.insert(c),
            K::Char('q') => return Ok(()),
            _ => {}
        }
    }
}

fn draw(f: &mut Frame, wizard: &Wizard) {
    let area = f.area();
    f.render_widget(Clear, area);

    let w = 70.min(area.width.saturating_sub(4));
    // Detection lines (especially `But::discover`'s own error text) can run longer than
    // the popup is wide, so they're wrapped ahead of time rather than left to clip —
    // matches `ui.rs`'s own `draw_blocked`/`draw_help`, which wrap for the same reason.
    let text_width = (w as usize).saturating_sub(4).max(1);

    let mut body = vec![Line::styled("  kanstack setup", theme::title(true)), Line::raw("")];
    for line in [&wizard.detection.but, &wizard.detection.harness, &wizard.detection.cmux, &wizard.detection.tmux] {
        for part in wrap(line, text_width) {
            body.push(Line::styled(format!("  {part}"), theme::muted()));
        }
    }
    body.push(Line::raw(""));
    body.push(field_line("harness", &wizard.harness, wizard.row == Row::Harness));
    body.push(choice_line(
        "split backend",
        wizard.split_backend.label(),
        wizard.row == Row::SplitBackend,
    ));
    body.push(choice_line("branch UI", wizard.branch_ui.label(), wizard.row == Row::BranchUi));
    body.push(Line::raw(""));
    body.push(save_line(wizard.row == Row::Save));
    body.push(Line::raw(""));
    for part in wrap(
        "↑/↓ move · ←/→ change · type to edit harness · ⏎ on save · esc cancels",
        text_width,
    ) {
        body.push(Line::styled(format!("  {part}"), theme::faint()));
    }
    // Trailing blank line keeps the border from hugging the hint text.
    body.push(Line::raw(""));

    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(Block::bordered().border_style(theme::faint())),
        popup,
    );
}

fn field_line(label: &str, input: &TextInput, selected: bool) -> Line<'static> {
    let style = if selected { theme::selected_bg() } else { theme::muted() };
    if selected {
        let (before, after) = input.split_at_cursor();
        Line::from(vec![
            Span::styled(format!("  {label:<14}"), theme::id()),
            Span::styled(before.to_string(), style),
            Span::styled("█", style),
            Span::styled(after.to_string(), style),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!("  {label:<14}"), theme::id()),
            Span::styled(input.as_str().to_string(), style),
        ])
    }
}

fn choice_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    let style = if selected { theme::selected_bg() } else { theme::muted() };
    Line::from(vec![
        Span::styled(format!("  {label:<14}"), theme::id()),
        Span::styled(format!("< {value} >"), style),
    ])
}

fn save_line(selected: bool) -> Line<'static> {
    let style = if selected { theme::selected_bg() } else { theme::muted() };
    Line::styled("    [ save ]", style)
}

/// Word-wraps `text` to `width` columns — a local copy of the same small utility
/// `ui.rs`'s `draw_blocked`/`draw_help` use for long, dynamic text (there, `But`'s own
/// error prose; here, the same plus `detect_harnesses`' comma-joined list), which isn't
/// `pub` there to reuse directly.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let extra = if current.is_empty() { 0 } else { 1 };
        if current.chars().count() + extra + word.chars().count() > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_word_boundaries_within_width() {
        let lines = wrap("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 10), "{lines:?}");
        assert_eq!(lines.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn wrap_leaves_short_text_on_one_line() {
        assert_eq!(wrap("short", 20), vec!["short".to_string()]);
    }

    #[test]
    fn known_harnesses_have_no_duplicate_basenames_to_double_count() {
        let mut sorted = KNOWN_HARNESSES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), KNOWN_HARNESSES.len());
    }

    /// Same `PATH`-swap-and-restore pattern as `cmux.rs`'s own `command_exists` tests,
    /// held for the whole call via the crate-wide lock since `PATH` is shared surface with
    /// `cmux.rs`/`tmux.rs`/`splitter.rs`'s own env-mutating tests.
    fn with_path(dir: &std::path::Path, body: impl FnOnce()) {
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("PATH");
        std::env::set_var("PATH", dir);
        body();
        match old {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }

    fn touch(dir: &std::path::Path, name: &str) {
        std::fs::write(dir.join(name), "").unwrap();
    }

    #[test]
    fn detect_harnesses_returns_only_what_is_on_path_in_priority_order() {
        let dir = std::env::temp_dir().join(format!("kanstack-setup-test-{}-a", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Created out of priority order, to prove the result order comes from
        // `KNOWN_HARNESSES`, not from directory listing order.
        touch(&dir, "codex");
        touch(&dir, "claude");
        with_path(&dir, || {
            assert_eq!(detect_harnesses(), vec!["claude", "codex"]);
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detect_harnesses_is_empty_when_none_are_on_path() {
        let dir = std::env::temp_dir().join(format!("kanstack-setup-test-{}-b", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        with_path(&dir, || {
            assert!(detect_harnesses().is_empty());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
