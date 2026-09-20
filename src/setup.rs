//! Interactive first-run/`--setup` wizard: detects `but`/`cmux`/`tmux`/`orca`, and lets you pick
//! the two environment variables that matter most for a new install (`KANSTACK_HARNESS`,
//! `KANSTACK_SPLIT_BACKEND`), then persists them via `crate::config` so they don't need to
//! be re-exported every session. Every other `KANSTACK_*` var — including
//! `KANSTACK_BRANCH_UI`, a standing preference rather than a machine-setup concern — stays
//! an env-var-only power-user override, documented in `--help` rather than surfaced here.
//!
//! Two more rows appear only when there's something to do: "install GitButler CLI" when
//! `but` itself is missing, and "install/update GitButler skill" when `but` is present but
//! `but skill check` reports no (or an outdated) coding-agent instruction file — see
//! `crate::but::But::skill_check`. Neither shows once there's nothing left to fix.
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
use crate::orca::Orca;
use crate::text_input::TextInput;
use crate::theme;
use crate::tmux::Tmux;

/// The exact command shown to (and run for) the user — GitButler's own documented
/// installer, already the one linked from this project's own README.
const INSTALL_BUT_CMD: &str = "curl -fsSL https://gitbutler.com/install.sh | sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SplitBackendChoice {
    Auto,
    Cmux,
    Tmux,
    Orca,
}

impl SplitBackendChoice {
    fn label(self) -> &'static str {
        match self {
            SplitBackendChoice::Auto => "auto",
            SplitBackendChoice::Cmux => "cmux",
            SplitBackendChoice::Tmux => "tmux",
            SplitBackendChoice::Orca => "orca",
        }
    }

    fn cycle(self, forward: bool) -> Self {
        use SplitBackendChoice::*;
        match (self, forward) {
            (Auto, true) => Cmux,
            (Cmux, true) => Tmux,
            (Tmux, true) => Orca,
            (Orca, true) => Auto,
            (Auto, false) => Orca,
            (Cmux, false) => Auto,
            (Tmux, false) => Cmux,
            (Orca, false) => Tmux,
        }
    }

    fn from_env() -> Self {
        match std::env::var("KANSTACK_SPLIT_BACKEND").as_deref() {
            Ok("cmux") => SplitBackendChoice::Cmux,
            Ok("tmux") => SplitBackendChoice::Tmux,
            Ok("orca") => SplitBackendChoice::Orca,
            _ => SplitBackendChoice::Auto,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    InstallBut,
    InstallSkill,
    Harness,
    SplitBackend,
    Save,
}

/// Whether a GitButler coding-agent skill file (`but skill check`) needs attention. `None`
/// in `Detection` (rather than a variant here) covers "couldn't even check" — `but` itself
/// missing, or the check call failing outright — since neither is "go install it".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillStatus {
    Missing,
    Outdated,
    UpToDate,
}

/// Whether `name` is on `PATH` — same PATH-search `cmux.rs`/`tmux.rs` each already do for
/// their own binary, just not shared with them since neither exposes it publicly.
fn harness_on_path(name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| dir.join(name).is_file())
}

/// Every harness in `crate::harness::KNOWN` that's on `PATH`, in that list's priority order —
/// what builds both the detection line and the default `KANSTACK_HARNESS` value below. Only
/// each harness's own name is checked, not its aliases: `kiro-cli` is the same binary as
/// `kiro` under an alternate name, and listing it separately would just double-count one
/// install as two.
fn detect_harnesses() -> Vec<&'static str> {
    crate::harness::KNOWN.iter().map(|h| h.id()).filter(|name| harness_on_path(name)).collect()
}

/// Read-only preview of what a normal launch would find — reuses the exact same
/// `discover`/`--version` calls `main.rs` itself makes, just without bailing the process
/// when one comes up empty.
struct Detection {
    but: String,
    but_installed: bool,
    harness: String,
    cmux: String,
    tmux: String,
    orca: String,
    skill: String,
    skill_status: Option<SkillStatus>,
}

fn detect() -> Detection {
    let but_result = But::discover(&std::env::current_dir().unwrap_or_default());
    let but_installed = but_result.is_ok();
    let but = match &but_result {
        Ok(b) => format!("✓ but {} found", b.version()),
        Err(e) => format!("✗ {e}"),
    };
    let (skill, skill_status) = match &but_result {
        Ok(b) => describe_skill(b),
        Err(_) => ("— install GitButler above first to check for a coding-agent skill".to_string(), None),
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
    let orca = match Orca::discover() {
        Some(_) => "✓ orca found, and this terminal is inside Orca".to_string(),
        None if !Orca::running_inside() => {
            "✗ orca not usable here — not running inside an Orca terminal".to_string()
        }
        None => "✗ orca CLI not found on PATH — register it in Orca's settings, or set KANSTACK_ORCA_BIN".to_string(),
    };
    Detection { but, but_installed, harness, cmux, tmux, orca, skill, skill_status }
}

/// Describes the result of `but skill check` for the detection panel, and classifies it
/// into a [`SkillStatus`] for `Wizard::visible_rows` to act on.
fn describe_skill(but: &But) -> (String, Option<SkillStatus>) {
    match but.skill_check() {
        Ok(check) if check.skills.is_empty() => (
            "✗ no GitButler skill installed for your coding agent yet".to_string(),
            Some(SkillStatus::Missing),
        ),
        Ok(check) if check.skills.iter().any(|s| !s.up_to_date) => {
            let names: Vec<&str> = check.skills.iter().map(|s| s.format_name.as_str()).collect();
            (format!("⚠ GitButler skill outdated: {}", names.join(", ")), Some(SkillStatus::Outdated))
        }
        Ok(check) => {
            let names: Vec<String> =
                check.skills.iter().map(|s| format!("{} ({})", s.format_name, s.scope)).collect();
            (format!("✓ GitButler skill up to date: {}", names.join(", ")), Some(SkillStatus::UpToDate))
        }
        Err(e) => (format!("✗ could not check for a GitButler skill: {e}"), None),
    }
}

struct Wizard {
    detection: Detection,
    row: Row,
    harness: TextInput,
    /// Every harness actually found on `PATH` (see `crate::harness::KNOWN`) — what `←`/`→` cycle
    /// through on the harness row. Typing overrides the current pick with anything custom;
    /// cycling again then jumps back in from whichever end matches the direction pressed,
    /// rather than trying to find the closest match to what was typed.
    harness_choices: Vec<&'static str>,
    split_backend: SplitBackendChoice,
    /// Whether the "install GitButler CLI now?" y/n sub-dialog is showing, replacing the
    /// normal body — set by `⏎` on `Row::InstallBut`, cleared by the answer either way.
    confirm_install_but: bool,
    /// True only for the duration of the blocking install call, so a "please wait" line
    /// can be drawn before it (the CLI install is a real network download; the skill
    /// install/update is a fast local file operation and doesn't bother with this).
    installing: bool,
    /// One-line result of the last install/update action, shown until the next one runs.
    action_message: Option<String>,
}

impl Wizard {
    fn new() -> Self {
        let detection = detect();
        let harness_choices = detect_harnesses();
        // `KANSTACK_HARNESS` wins if already set (matches every other field here); failing
        // that, the first installed harness found beats a blind guess of "claude" — see
        // `crate::harness::KNOWN` for the priority order among several installed at once.
        let default_harness = std::env::var("KANSTACK_HARNESS")
            .ok()
            .or_else(|| harness_choices.first().map(|s| s.to_string()))
            .unwrap_or_else(|| "claude".to_string());
        let mut harness = TextInput::default();
        harness.set(default_harness);
        let mut wizard = Wizard {
            detection,
            row: Row::Harness,
            harness,
            harness_choices,
            split_backend: SplitBackendChoice::from_env(),
            confirm_install_but: false,
            installing: false,
            action_message: None,
        };
        // Start on whichever row actually needs attention first, rather than always on
        // "default harness" — a missing `but` or skill is the more pressing thing to fix.
        wizard.row = wizard.visible_rows()[0];
        wizard
    }

    /// Every row worth showing right now, top to bottom — `Row::InstallBut`/`InstallSkill`
    /// only when there's actually something to install or update. Navigation (`move_row`)
    /// walks this list rather than a fixed cycle, so a row that just disappeared (an
    /// install that just succeeded) is never landed on.
    fn visible_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        if !self.detection.but_installed {
            rows.push(Row::InstallBut);
        }
        if matches!(self.detection.skill_status, Some(SkillStatus::Missing) | Some(SkillStatus::Outdated)) {
            rows.push(Row::InstallSkill);
        }
        rows.push(Row::Harness);
        rows.push(Row::SplitBackend);
        rows.push(Row::Save);
        rows
    }

    fn move_row(&mut self, forward: bool) {
        let rows = self.visible_rows();
        let len = rows.len();
        let idx = rows.iter().position(|r| *r == self.row).unwrap_or(0);
        self.row = rows[if forward { (idx + 1) % len } else { (idx + len - 1) % len }];
    }

    /// Steps the harness field to the next (or previous) detected harness. A no-op with
    /// nothing detected. When the current value isn't one of the detected choices at all
    /// (typed in by hand, or nothing found), `→` lands on the first choice and `←` on the
    /// last, rather than guessing which one it was "closest" to.
    fn cycle_harness(&mut self, forward: bool) {
        let len = self.harness_choices.len();
        if len == 0 {
            return;
        }
        let current = self.harness_choices.iter().position(|h| *h == self.harness.as_str());
        let next = match (current, forward) {
            (Some(i), true) => (i + 1) % len,
            (Some(i), false) => (i + len - 1) % len,
            (None, true) => 0,
            (None, false) => len - 1,
        };
        self.harness.set(self.harness_choices[next]);
    }

    /// Runs the confirmed CLI installer and re-detects everything afterward, so the panel,
    /// row visibility (`Row::InstallBut` disappearing), and the harness default all
    /// reflect a freshly-installed `but` immediately.
    fn install_but(&mut self) {
        self.action_message = None;
        let result = std::process::Command::new("sh")
            .arg("-c")
            .arg(INSTALL_BUT_CMD)
            .stdin(std::process::Stdio::null())
            .output();
        self.action_message = Some(match result {
            Ok(out) if out.status.success() => "✓ GitButler CLI installed".to_string(),
            Ok(out) => {
                let detail = String::from_utf8_lossy(&out.stderr);
                let last_line = detail.trim().lines().next_back().unwrap_or("installer exited with an error");
                format!("✗ install failed: {last_line}")
            }
            Err(e) => format!("✗ could not run the installer: {e}"),
        });
        self.refresh_detection();
    }

    /// Installs the skill fresh if none was found, or refreshes it in place if outdated —
    /// see `SkillStatus`. Re-detects afterward so the panel and row visibility update.
    fn install_or_update_skill(&mut self) {
        self.action_message = None;
        let Ok(but) = But::discover(&std::env::current_dir().unwrap_or_default()) else {
            self.action_message = Some("✗ but is no longer available".to_string());
            return;
        };
        let result = match self.detection.skill_status {
            Some(SkillStatus::Missing) => but.skill_install_global(),
            _ => but.skill_update(),
        };
        self.action_message = Some(match result {
            Ok(()) => "✓ GitButler skill installed/updated".to_string(),
            Err(e) => format!("✗ skill action failed: {e}"),
        });
        self.refresh_detection();
    }

    fn refresh_detection(&mut self) {
        self.detection = detect();
        let rows = self.visible_rows();
        if !rows.contains(&self.row) {
            self.row = rows[0];
        }
    }

    /// Only writes a field when it differs from "auto"/default, so a knob left untouched
    /// keeps auto-detecting on future runs rather than getting pinned to today's result.
    fn save(&self) -> Result<()> {
        let mut fields: Vec<(&str, String)> = vec![("KANSTACK_HARNESS", self.harness.trimmed())];
        if self.split_backend != SplitBackendChoice::Auto {
            fields.push(("KANSTACK_SPLIT_BACKEND", self.split_backend.label().to_string()));
        }
        crate::config::save(&fields)
    }
}

/// Whether the wizard's `run()` ended by saving or by being dismissed — `main.rs` only
/// needs this to decide whether the *implicit* first-run trigger should still mark itself
/// done on a skip (an explicit `--setup` leaves an existing file alone either way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Saved,
    Cancelled,
}

/// Runs the wizard in its own terminal session and returns once the user saves or cancels.
pub fn run() -> Result<Outcome> {
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<Outcome> {
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
        if wizard.confirm_install_but {
            match key.code {
                K::Char('y' | 'Y') | K::Enter => {
                    wizard.confirm_install_but = false;
                    wizard.installing = true;
                    terminal.draw(|f| draw(f, &wizard))?;
                    wizard.install_but();
                    wizard.installing = false;
                }
                K::Char('n' | 'N') | K::Esc => wizard.confirm_install_but = false,
                _ => {}
            }
            continue;
        }
        match key.code {
            K::Esc => return Ok(Outcome::Cancelled),
            K::Down | K::Tab => wizard.move_row(true),
            K::Up | K::BackTab => wizard.move_row(false),
            K::Enter if wizard.row == Row::Save => {
                wizard.save()?;
                return Ok(Outcome::Saved);
            }
            K::Enter if wizard.row == Row::InstallBut => wizard.confirm_install_but = true,
            K::Enter if wizard.row == Row::InstallSkill => wizard.install_or_update_skill(),
            K::Right if wizard.row == Row::SplitBackend => {
                wizard.split_backend = wizard.split_backend.cycle(true);
            }
            K::Left if wizard.row == Row::SplitBackend => {
                wizard.split_backend = wizard.split_backend.cycle(false);
            }
            K::Enter if wizard.row == Row::SplitBackend => {
                wizard.split_backend = wizard.split_backend.cycle(true);
            }
            K::Right if wizard.row == Row::Harness => wizard.cycle_harness(true),
            K::Left if wizard.row == Row::Harness => wizard.cycle_harness(false),
            _ if wizard.row == Row::Harness && wizard.harness.handle_key(key) => {}
            K::Char('q') => return Ok(Outcome::Cancelled),
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

    let body = if wizard.confirm_install_but {
        confirm_install_but_body(text_width)
    } else {
        main_body(wizard, text_width)
    };

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

fn confirm_install_but_body(text_width: usize) -> Vec<Line<'static>> {
    let mut body = vec![Line::styled("  kanstack setup", theme::title(true)), Line::raw("")];
    for part in wrap("Install the GitButler CLI now? This runs:", text_width) {
        body.push(Line::styled(format!("  {part}"), theme::muted()));
    }
    body.push(Line::raw(""));
    body.push(Line::styled(format!("    {INSTALL_BUT_CMD}"), theme::faint()));
    body.push(Line::raw(""));
    body.push(Line::styled("  y  yes, install it      n / esc  cancel", theme::muted()));
    body.push(Line::raw(""));
    body
}

fn main_body(wizard: &Wizard, text_width: usize) -> Vec<Line<'static>> {
    let rows = wizard.visible_rows();
    let mut body = vec![Line::styled("  kanstack setup", theme::title(true)), Line::raw("")];
    for line in [
        &wizard.detection.but,
        &wizard.detection.harness,
        &wizard.detection.cmux,
        &wizard.detection.tmux,
        &wizard.detection.orca,
        &wizard.detection.skill,
    ] {
        for part in wrap(line, text_width) {
            body.push(Line::styled(format!("  {part}"), theme::muted()));
        }
    }
    if wizard.installing {
        body.push(Line::raw(""));
        for part in wrap("⏳ installing — this may take a moment…", text_width) {
            body.push(Line::styled(format!("  {part}"), theme::muted()));
        }
    } else if let Some(msg) = &wizard.action_message {
        body.push(Line::raw(""));
        for part in wrap(msg, text_width) {
            body.push(Line::styled(format!("  {part}"), theme::muted()));
        }
    }
    body.push(Line::raw(""));
    if rows.contains(&Row::InstallBut) {
        body.push(action_row("install GitButler CLI", wizard.row == Row::InstallBut));
    }
    if rows.contains(&Row::InstallSkill) {
        body.push(action_row("install/update GitButler skill", wizard.row == Row::InstallSkill));
    }
    if rows.contains(&Row::InstallBut) || rows.contains(&Row::InstallSkill) {
        body.push(Line::raw(""));
    }
    body.push(field_line("default harness", &wizard.harness, wizard.row == Row::Harness));
    body.push(choice_line(
        "split backend",
        wizard.split_backend.label(),
        wizard.row == Row::SplitBackend,
    ));
    body.push(Line::raw(""));
    body.push(action_row("save", wizard.row == Row::Save));
    body.push(Line::raw(""));
    for part in wrap(
        "↑/↓ move · ←/→ cycle choices or detected harnesses · type for a custom harness · \
         ⏎ acts on the selected row · esc cancels",
        text_width,
    ) {
        body.push(Line::styled(format!("  {part}"), theme::faint()));
    }
    // Trailing blank line keeps the border from hugging the hint text.
    body.push(Line::raw(""));
    body
}

fn field_line(label: &str, input: &TextInput, selected: bool) -> Line<'static> {
    let style = if selected { theme::selected_bg() } else { theme::muted() };
    if selected {
        let (before, after) = input.split_at_cursor();
        Line::from(vec![
            Span::styled(format!("  {label:<16}"), theme::id()),
            Span::styled(before.to_string(), style),
            Span::styled("█", style),
            Span::styled(after.to_string(), style),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!("  {label:<16}"), theme::id()),
            Span::styled(input.as_str().to_string(), style),
        ])
    }
}

fn choice_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    let style = if selected { theme::selected_bg() } else { theme::muted() };
    Line::from(vec![
        Span::styled(format!("  {label:<16}"), theme::id()),
        Span::styled(format!("< {value} >"), style),
    ])
}

fn action_row(label: &str, selected: bool) -> Line<'static> {
    let style = if selected { theme::selected_bg() } else { theme::muted() };
    Line::styled(format!("    [ {label} ]"), style)
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
        // `crate::harness::KNOWN`, not from directory listing order.
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

    fn test_detection(but_installed: bool, skill_status: Option<SkillStatus>) -> Detection {
        Detection {
            but: String::new(),
            but_installed,
            harness: String::new(),
            cmux: String::new(),
            tmux: String::new(),
            orca: String::new(),
            skill: String::new(),
            skill_status,
        }
    }

    fn test_wizard(current: &str, choices: Vec<&'static str>) -> Wizard {
        let mut harness = TextInput::default();
        harness.set(current);
        Wizard {
            detection: test_detection(true, Some(SkillStatus::UpToDate)),
            row: Row::Harness,
            harness,
            harness_choices: choices,
            split_backend: SplitBackendChoice::Auto,
            confirm_install_but: false,
            installing: false,
            action_message: None,
        }
    }

    /// Every backend is reachable from the wizard, in both directions, and the cycle closes.
    #[test]
    fn the_split_backend_choice_cycles_through_all_four_and_wraps_both_ways() {
        use SplitBackendChoice::*;
        let forward: Vec<_> = std::iter::successors(Some(Auto), |c| Some(c.cycle(true))).take(5).collect();
        assert_eq!(forward, [Auto, Cmux, Tmux, Orca, Auto]);
        let backward: Vec<_> = std::iter::successors(Some(Auto), |c| Some(c.cycle(false))).take(5).collect();
        assert_eq!(backward, [Auto, Orca, Tmux, Cmux, Auto]);
        assert_eq!(Orca.label(), "orca");
    }

    #[test]
    fn cycle_harness_steps_through_detected_choices_and_wraps_both_ways() {
        let mut w = test_wizard("codex", vec!["claude", "codex", "pi"]);
        w.cycle_harness(true);
        assert_eq!(w.harness.as_str(), "pi");
        w.cycle_harness(true);
        assert_eq!(w.harness.as_str(), "claude", "wraps forward past the last choice");
        w.cycle_harness(false);
        assert_eq!(w.harness.as_str(), "pi", "wraps backward past the first choice");
    }

    #[test]
    fn cycle_harness_from_a_custom_value_lands_on_the_matching_end() {
        let mut forward = test_wizard("my-custom-thing", vec!["claude", "codex"]);
        forward.cycle_harness(true);
        assert_eq!(forward.harness.as_str(), "claude");

        let mut backward = test_wizard("my-custom-thing", vec!["claude", "codex"]);
        backward.cycle_harness(false);
        assert_eq!(backward.harness.as_str(), "codex");
    }

    #[test]
    fn cycle_harness_is_a_no_op_with_nothing_detected() {
        let mut w = test_wizard("claude", vec![]);
        w.cycle_harness(true);
        assert_eq!(w.harness.as_str(), "claude");
    }

    fn wizard_with(but_installed: bool, skill_status: Option<SkillStatus>) -> Wizard {
        let mut w = test_wizard("claude", vec![]);
        w.detection = test_detection(but_installed, skill_status);
        w
    }

    #[test]
    fn but_missing_shows_only_the_install_but_row() {
        let rows = wizard_with(false, None).visible_rows();
        assert_eq!(
            rows,
            vec![Row::InstallBut, Row::Harness, Row::SplitBackend, Row::Save],
            "no skill row while but itself isn't installed yet"
        );
    }

    #[test]
    fn missing_skill_shows_the_install_skill_row() {
        let rows = wizard_with(true, Some(SkillStatus::Missing)).visible_rows();
        assert_eq!(rows, vec![Row::InstallSkill, Row::Harness, Row::SplitBackend, Row::Save]);
    }

    #[test]
    fn outdated_skill_shows_the_install_skill_row_too() {
        let rows = wizard_with(true, Some(SkillStatus::Outdated)).visible_rows();
        assert_eq!(rows, vec![Row::InstallSkill, Row::Harness, Row::SplitBackend, Row::Save]);
    }

    #[test]
    fn up_to_date_skill_shows_neither_extra_row() {
        let rows = wizard_with(true, Some(SkillStatus::UpToDate)).visible_rows();
        assert_eq!(rows, vec![Row::Harness, Row::SplitBackend, Row::Save]);
    }

    #[test]
    fn move_row_wraps_within_the_visible_set_only() {
        let mut w = wizard_with(false, None);
        w.row = Row::Save;
        w.move_row(true);
        assert_eq!(w.row, Row::InstallBut, "wraps forward past Save back to the top");
        w.move_row(false);
        assert_eq!(w.row, Row::Save, "wraps backward past the top back to Save");
    }

    #[test]
    fn move_row_lands_on_the_first_visible_row_if_the_current_one_disappeared() {
        let mut w = wizard_with(false, None);
        w.row = Row::InstallBut;
        // Simulates a successful install: the row this cursor was on is now gone.
        w.detection = test_detection(true, Some(SkillStatus::UpToDate));
        w.move_row(true);
        assert_eq!(w.row, Row::SplitBackend, "falls back to index 0 (Harness), then steps forward once");
    }
}
