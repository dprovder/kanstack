//! Board rendering.
//!
//! Columns are laid out side by side and scroll horizontally; each column scrolls
//! vertically on its own so a long lane never pushes its neighbours around.
//!
//! Split across files by feature area, mirroring `app`'s own split — a mode's confirm
//! screen or drawer lives next to the other screens like it, not next to every other
//! screen in the app. `draw` itself, and the handful of helpers more than one screen
//! needs (wrapping, truncation, the confirm-dialog hitbox math), stay here.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{columns_that_fit, App, BranchPreview, BranchUi, Mode, Notice, COL_GAP};
use crate::board::{Card, ColumnKind, Tone};
use crate::hit::{HitMap, HitTarget};
use crate::pane_status::PaneStatus;
use crate::theme;

mod board;
mod branch_modal;
mod confirms;
mod diff;
mod drawer;
mod footer;
mod header;
mod help;
mod pr;

use board::draw_board;
use branch_modal::draw_branch_modal;
use confirms::{
    draw_blocked, draw_delete_confirm, draw_land_confirm, draw_landing, draw_not_set_up,
    draw_push_confirm, draw_rebase_confirm,
};
use diff::draw_diff;
use drawer::{draw_branches, draw_unapply_confirm};
use footer::draw_footer;
use header::draw_header;
use help::draw_help;
use pr::{draw_pr_modal, draw_pr_running};

/// How wide the unapplied-branches drawer is, when there is room for it. Branch names run
/// long, so this is wider than the board's own minimum lane; it is halved against the board
/// area before use so a narrow terminal never gives the drawer more room than the lanes.
const DRAWER_WIDTH: u16 = 34;

/// How wide the drawer gets while showing a branch preview instead of the list — commit
/// subjects and file paths need more room than branch names do to read as anything but a
/// wall of ellipses. The same 56 columns the confirm dialogs already use for a commit list
/// (`draw_land_confirm`, `draw_delete_confirm`), not a new number invented for this one
/// panel; still halved against the board area, same as `DRAWER_WIDTH`.
const PREVIEW_WIDTH: u16 = 56;


pub fn draw(f: &mut Frame, app: &App) -> HitMap {
    let mut hits = HitMap::new();
    let mut constraints = vec![Constraint::Length(2)]; // header
    if app.tutorial.is_some() {
        constraints.push(Constraint::Length(4)); // tutorial banner
    }
    constraints.push(Constraint::Min(1)); // board
    constraints.push(Constraint::Length(1)); // footer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    draw_header(f, app, chunks[0]);
    let mut next = 1;
    if let Some(t) = &app.tutorial {
        draw_tutorial_banner(f, t, chunks[next]);
        next += 1;
    }
    let board_area = chunks[next];
    let footer_area = chunks[next + 1];

    if app.mode == Mode::Diff && !app.diff_full {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(board_area);
        draw_board(f, app, split[0], &mut hits);
        draw_diff(
            f,
            app,
            Rect {
                x: split[1].x + 1,
                width: split[1].width.saturating_sub(1),
                ..split[1]
            },
            &mut hits,
        );
    } else if app.mode == Mode::Diff {
        draw_diff(f, app, board_area, &mut hits);
    } else if app.mode == Mode::Branches {
        // Beside the board, not over it, for the same reason the diff splits rather than
        // covers: choosing what to apply is a decision made *against* the lanes already
        // there, so hiding them to show the list would hide half the question.
        //
        // On the left, where the board's own reading order starts: an unapplied branch is
        // upstream of every lane, so it belongs before them rather than after. The lanes
        // shift right while it is open, which is also what makes the drawer's presence
        // obvious without needing a border to announce it.
        let base_w = if app.branch_preview.is_some() {
            PREVIEW_WIDTH
        } else {
            DRAWER_WIDTH
        };
        let w = base_w.min(board_area.width / 2);
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(w), Constraint::Min(0)])
            .split(board_area);
        draw_branches(f, app, split[0], &mut hits);
        draw_board(f, app, split[1], &mut hits);
    } else {
        draw_board(f, app, board_area, &mut hits);
    }
    draw_footer(f, app, footer_area);

    match app.mode {
        Mode::Help => draw_help(f, f.area(), &mut hits),
        Mode::PushConfirm => draw_push_confirm(f, app, f.area(), &mut hits),
        Mode::LandConfirm => draw_land_confirm(f, app, f.area(), &mut hits),
        Mode::Landing => draw_landing(f, app, f.area()),
        Mode::PrModal => draw_pr_modal(f, app, f.area(), &mut hits),
        Mode::PrRunning => draw_pr_running(f, app, f.area()),
        Mode::DeleteConfirm => draw_delete_confirm(f, app, f.area(), &mut hits),
        Mode::UnapplyConfirm => draw_unapply_confirm(f, app, f.area(), &mut hits),
        Mode::RebaseConfirm => draw_rebase_confirm(f, app, f.area(), &mut hits),
        Mode::Blocked => draw_blocked(f, app, f.area()),
        Mode::SetupRequired => draw_not_set_up(f, app, f.area()),
        Mode::Branch | Mode::HarnessMessage if app.branch_ui == BranchUi::Modal => {
            draw_branch_modal(f, app, f.area(), &mut hits)
        }
        // The board keeps its half unless the diff is expanded, so reading a diff does not
        // cost you your place — the same split gitui uses, and for the same reason.
        // `draw_diff` already added its own close control above.
        Mode::Diff => {}
        _ => {}
    }

    hits
}

/// Splits a confirm dialog's last body line — `"  ⏎ / y  <verb>      esc / n  cancel"` —
/// into two clickable halves at the word "esc", the one substring guaranteed not to appear
/// in the verb half of any of these hints.
fn confirm_hitboxes(hits: &mut HitMap, popup: Rect, body_len: usize, hint: &str) {
    if popup.height < 2 {
        return;
    }
    let row = popup.y + 1 + (body_len as u16 - 1);
    if row + 1 >= popup.y + popup.height {
        // The hint line itself was clipped out of a too-short popup; nothing to hit-test.
        return;
    }
    let content_x = popup.x + 1;
    let content_w = popup.width.saturating_sub(2);
    let split = hint.find("esc").unwrap_or(hint.len()) as u16;
    let confirm_w = split.min(content_w);
    hits.push(
        Rect { x: content_x, y: row, width: confirm_w, height: 1 },
        HitTarget::DialogConfirm,
    );
    if confirm_w < content_w {
        hits.push(
            Rect {
                x: content_x + confirm_w,
                y: row,
                width: content_w - confirm_w,
                height: 1,
            },
            HitTarget::DialogCancel,
        );
    }
}

/// The scripted lesson's current instruction, always visible so it survives whatever mode
/// or popup is on screen — a step like "open a diff" would otherwise hide the very prompt
/// that sent you there.
fn draw_tutorial_banner(f: &mut Frame, t: &crate::tutorial::Tutorial, area: Rect) {
    let w = area.width.saturating_sub(2) as usize;
    let mut lines = vec![Line::styled(
        format!("  tutorial — step {}", t.step_label()),
        theme::muted(),
    )];
    for wrapped in wrap(t.prompt(), w.max(1)) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(wrapped, theme::title(true)),
        ]));
    }
    lines.push(Line::styled("─".repeat(area.width as usize), theme::faint()));
    f.render_widget(Paragraph::new(lines), area);
}

/// A commit's `+insertions -deletions`, in the same tones `render_card` gives a lane
/// card's own stats — `None` when `but` didn't compute a diffstat for this commit (true of
/// every commit nested inside a `ConflictingFile`, only ever set on the top-level list).
fn commit_stat_line(c: &crate::model::MergeCheckCommit) -> Option<Line<'static>> {
    let (a, r) = (c.insertions?, c.deletions?);
    Some(Line::from(vec![
        Span::styled(format!("+{a}"), theme::tone(crate::board::Tone::Good)),
        Span::styled(format!(" -{r}"), theme::tone(crate::board::Tone::Bad)),
    ]))
}

/// Greedy word wrap. Long unbroken tokens (paths, hashes) are hard-split rather than
/// allowed to overflow the column.
fn wrap(s: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        let wl = word.chars().count();
        let cur = line.chars().count();
        if cur == 0 {
            if wl <= width {
                line.push_str(word);
            } else {
                let mut rest = word;
                while rest.chars().count() > width {
                    let cut: String = rest.chars().take(width).collect();
                    out.push(cut.clone());
                    rest = &rest[cut.len()..];
                }
                line.push_str(rest);
            }
        } else if cur + 1 + wl <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line.push_str(word);
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// How many character cells a footer text input can use for its typed content: the
/// area's width minus whatever fixed text flanks it (a label before, hints/status after)
/// and one more for the cursor glyph itself.
fn footer_input_budget(area_width: u16, prefix_chars: usize, suffix_chars: usize) -> usize {
    (area_width as usize)
        .saturating_sub(prefix_chars)
        .saturating_sub(suffix_chars)
        .saturating_sub(1)
}

/// Slides the cursor-split halves of a footer text input so the cursor stays visible
/// within `budget` character cells.
///
/// The footer is a single fixed-height line (`Constraint::Length(1)`) that `Paragraph`
/// never wraps, so once a `Commit`/`Branch`/`Task`/`HarnessMessage` input grew past the
/// terminal's width, everything from that point on — including the cursor itself — used
/// to run off the right edge and simply not be drawn, with no indication anything was
/// being typed at all. This keeps a `budget`-wide window centred on the cursor instead,
/// eliding whichever side(s) don't fit with `…`, the same signal `truncate` uses for card
/// text — with one side handing its unused share to the other (typically `after`, which
/// is empty while typing forward, so `before` gets the whole window rather than half of
/// it going to waste).
fn scroll_input(before: &str, after: &str, budget: usize) -> (String, String) {
    if budget == 0 {
        return (String::new(), String::new());
    }
    let before_len = before.chars().count();
    let after_len = after.chars().count();
    if before_len + after_len <= budget {
        return (before.to_string(), after.to_string());
    }

    let after_cells = after_len.min(budget / 2);
    let before_cells = before_len.min(budget - after_cells);
    let after_cells = after_len.min(budget - before_cells);

    let before_elided = before_cells < before_len;
    let after_elided = after_cells < after_len;
    // The ellipsis itself takes one of that side's own cells, so the total still fits.
    let before_take = before_cells.saturating_sub(before_elided as usize);
    let after_take = after_cells.saturating_sub(after_elided as usize);

    let mut before_show = String::new();
    if before_elided {
        before_show.push('…');
    }
    before_show.extend(before.chars().skip(before_len - before_take));

    let mut after_show: String = after.chars().take(after_take).collect();
    if after_elided {
        after_show.push('…');
    }

    (before_show, after_show)
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    let mut t: String = s.chars().take(keep).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_input_shows_everything_when_it_already_fits() {
        assert_eq!(scroll_input("hello ", "world", 20), ("hello ".into(), "world".into()));
    }

    /// Typing forward with nothing after the cursor should use the whole window on
    /// `before`, not split it evenly with an empty `after`.
    #[test]
    fn scroll_input_gives_an_empty_side_its_share_back() {
        let (before, after) = scroll_input("a very long line of typed text", "", 10);
        assert_eq!(before.chars().count() + after.chars().count(), 10);
        assert!(after.is_empty());
        assert!(before.starts_with('…'), "the clipped side must say so: {before:?}");
        assert!(before.ends_with("text"), "the cursor-adjacent tail must stay visible: {before:?}");
    }

    /// Content on both sides of the cursor gets clipped independently, each marked with
    /// its own ellipsis, and the cursor's own position (the boundary) is always shown.
    #[test]
    fn scroll_input_elides_both_sides_when_both_overflow() {
        let (before, after) = scroll_input("nine chars", "ten chars!", 8);
        assert_eq!(before.chars().count() + after.chars().count(), 8);
        assert!(before.starts_with('…'));
        assert!(before.ends_with("ars"), "got {before:?}");
        assert!(after.ends_with('…'));
        assert!(after.starts_with("ten"), "got {after:?}");
    }

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(
            wrap("Wire session refresh into gateway", 14),
            ["Wire session", "refresh into", "gateway"]
        );
    }

    #[test]
    fn hard_splits_tokens_that_cannot_fit() {
        // A long path must not overflow the lane.
        let out = wrap("src/very/long/path/to/a/file.rs", 10);
        assert!(out.iter().all(|l| l.chars().count() <= 10), "got {out:?}");
        assert_eq!(out.concat(), "src/very/long/path/to/a/file.rs");
    }

    #[test]
    fn truncate_marks_elision() {
        assert_eq!(truncate("feat-auth", 20), "feat-auth");
        assert_eq!(truncate("a-very-long-branch-name", 10), "a-very-lo…");
    }
}
