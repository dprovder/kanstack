//! Board rendering.
//!
//! Columns are laid out side by side and scroll horizontally; each column scrolls
//! vertically on its own so a long lane never pushes its neighbours around.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Mode, Notice};
use crate::board::Card;
use crate::theme;

const MIN_COL_WIDTH: u16 = 26;
const MAX_COL_WIDTH: u16 = 46;
const COL_GAP: u16 = 2;

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header
            Constraint::Min(1),    // board
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_board(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    if app.mode == Mode::Help {
        draw_help(f, f.area());
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let b = &app.board;
    let mut spans = vec![
        Span::styled("  workspace", theme::muted()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(format!("base {}", b.base_short_id), theme::faint()),
    ];
    if b.behind > 0 {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("{} behind upstream", b.behind),
            theme::tone(crate::board::Tone::Warn),
        ));
    }
    if !b.conflicted_files.is_empty() {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("{} conflicted", b.conflicted_files.len()),
            theme::tone(crate::board::Tone::Bad),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Chooses a column width and the first visible column so the cursor stays on screen.
fn visible_columns(app: &App, area: Rect) -> (u16, usize, usize) {
    let n = app.column_count().max(1) as u16;
    let usable = area.width.saturating_sub(2);

    // Prefer showing everything; fall back to a comfortable width and scroll. `n` is
    // floored at 1 above, so the division is always safe.
    let width = (usable / n)
        .saturating_sub(COL_GAP)
        .clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
    let per = width + COL_GAP;
    let fit = (usable / per).max(1) as usize;

    // Scroll the window just far enough to include the selected column.
    let first = if app.col < fit { 0 } else { app.col + 1 - fit };
    (width, first, fit)
}

fn draw_board(f: &mut Frame, app: &App, area: Rect) {
    if app.board.columns.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(
                "  no applied branches — create one with `but branch new <name>`",
                theme::faint(),
            )),
            area,
        );
        return;
    }

    let (width, first, fit) = visible_columns(app, area);
    let last = (first + fit).min(app.board.columns.len());

    let mut x = area.x + 1;
    for idx in first..last {
        if x + width > area.x + area.width {
            break;
        }
        let col_area = Rect {
            x,
            y: area.y,
            width,
            height: area.height,
        };
        draw_column(f, app, idx, col_area);
        x += width + COL_GAP;
    }

    // Hint that there is more board off-screen.
    if last < app.board.columns.len() {
        let marker = Rect {
            x: area.x + area.width.saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        };
        let more: Vec<Line> = (0..area.height)
            .map(|_| Line::styled("›", theme::faint()))
            .collect();
        f.render_widget(Paragraph::new(more), marker);
    }
}

fn draw_column(f: &mut Frame, app: &App, idx: usize, area: Rect) {
    let col = &app.board.columns[idx];
    let is_current = idx == app.col;
    let inner_w = area.width as usize;

    // Header: dot, name, count.
    let header = Line::from(vec![
        Span::styled("● ", theme::status_dot(col.status)),
        Span::styled(
            truncate(&col.title, inner_w.saturating_sub(8)),
            theme::title(is_current),
        ),
        Span::styled(format!("  {}", col.cards.len()), theme::faint()),
    ]);

    let mut lines = vec![header];

    // A rule under the header, brighter on the active lane.
    let rule_style = if is_current {
        theme::muted()
    } else {
        theme::faint()
    };
    lines.push(Line::styled("─".repeat(inner_w), rule_style));

    if !col.badges.is_empty() {
        let spans: Vec<Span> = col
            .badges
            .iter()
            .flat_map(|b| {
                [
                    Span::styled(b.text.clone(), theme::tone(b.tone)),
                    Span::raw("  "),
                ]
            })
            .collect();
        lines.push(Line::from(spans));
    }

    // Cards, tracking which lines belong to the cursor so we can scroll to it.
    let mut sel_start = 0usize;
    let mut sel_len = 0usize;
    let mut last_group: Option<&str> = None;

    for (ci, card) in col.cards.iter().enumerate() {
        if let Some(group) = card.group.as_deref() {
            if last_group != Some(group) {
                lines.push(Line::from(vec![
                    Span::styled("┈ ", theme::faint()),
                    Span::styled(truncate(group, inner_w.saturating_sub(4)), theme::muted()),
                ]));
                last_group = Some(group);
            }
        }

        let selected = is_current && ci == app.card;
        let picked = app.mode == Mode::Moving && idx == app.origin_col && ci == app.card;
        let start = lines.len();
        let card_lines = render_card(card, inner_w, selected, picked);
        if selected {
            sel_start = start;
            sel_len = card_lines.len();
        }
        lines.extend(card_lines);
    }

    if col.cards.is_empty() {
        lines.push(Line::styled("  empty", theme::faint()));
    }

    // Keep the selected card in view within this column.
    let body_h = area.height as usize;
    let scroll = if sel_len > 0 && sel_start + sel_len > body_h {
        (sel_start + sel_len - body_h) as u16
    } else {
        0
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default())
            .scroll((scroll, 0)),
        area,
    );
}

fn render_card(card: &Card, width: usize, selected: bool, picked: bool) -> Vec<Line<'static>> {
    let bg = if picked {
        Some(theme::picked_bg())
    } else if selected {
        Some(theme::selected_bg())
    } else {
        None
    };
    let pad = |mut line: Line<'static>| -> Line<'static> {
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        if used < width {
            line.spans.push(Span::raw(" ".repeat(width - used)));
        }
        if let Some(style) = bg {
            line = line.style(style);
        }
        line
    };

    let mut out = Vec::new();

    // Id badge, plus a caret marking the card in hand.
    let mut id_spans = vec![Span::styled(card.cli_id.clone(), theme::id())];
    if picked {
        id_spans.push(Span::styled(
            "  ⤴ moving",
            theme::tone(crate::board::Tone::Accent),
        ));
    }
    out.push(pad(Line::from(id_spans)));

    for l in wrap(&card.title, width) {
        out.push(pad(Line::from(Span::styled(l, theme::title(selected)))));
    }

    if let Some(sub) = &card.subtitle {
        // One line of file context is enough; more turns the card into a diff.
        let l = wrap(sub, width).into_iter().next().unwrap_or_default();
        let l = if sub.chars().count() > l.chars().count() {
            format!("{}…", l.trim_end())
        } else {
            l
        };
        out.push(pad(Line::from(Span::styled(l, theme::muted()))));
    }

    let mut meta: Vec<Span> = card
        .badges
        .iter()
        .flat_map(|b| {
            [
                Span::styled(b.text.clone(), theme::tone(b.tone)),
                Span::raw("  "),
            ]
        })
        .collect();
    if let Some(author) = &card.author {
        meta.push(Span::styled(author.clone(), theme::faint()));
    }
    if !meta.is_empty() {
        out.push(pad(Line::from(meta)));
    }

    // Whitespace, not borders, is what separates cards.
    out.push(Line::raw(""));
    out
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some((msg, kind)) = &app.message {
        let style = match kind {
            Notice::Info => theme::muted(),
            Notice::Success => theme::tone(crate::board::Tone::Good),
            Notice::Error => theme::tone(crate::board::Tone::Bad),
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(msg.clone(), style),
            ])),
            area,
        );
        return;
    }

    let keys = match app.mode {
        Mode::Moving => {
            let action = app.pending_action().unwrap_or_default();
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(action, theme::tone(crate::board::Tone::Accent)),
                    Span::styled("   ←/→ pick lane · ⏎ confirm · esc cancel", theme::faint()),
                ])),
                area,
            );
        }
        Mode::Help => "  esc close",
        Mode::Normal => "  ←/→ lane · ↑/↓ card · m move · r refresh · ? help · q quit",
    };
    f.render_widget(Paragraph::new(Line::styled(keys, theme::faint())), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let body = vec![
        Line::styled("  keys", theme::muted()),
        Line::raw(""),
        help_row("←/→  h/l", "move between lanes"),
        help_row("↑/↓  j/k", "move between cards"),
        help_row("g / G", "first / last card"),
        help_row("m", "pick up a card, then ←/→ and ⏎ to drop"),
        help_row("r", "refresh from `but status`"),
        help_row("? / esc", "toggle this help"),
        help_row("q", "quit"),
        Line::raw(""),
        Line::styled("  what a move does", theme::muted()),
        Line::raw(""),
        help_row("commit → lane", "moves the commit to that branch"),
        help_row("commit → unassigned", "uncommits it into the worktree"),
        help_row("file → lane", "stages it to that branch"),
        help_row("file → unassigned", "unstages it"),
        Line::raw(""),
        Line::styled("  all moves run `but rub SOURCE TARGET`.", theme::faint()),
    ];

    let w = 62.min(area.width.saturating_sub(4));
    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };

    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(
            Block::bordered()
                .border_style(theme::faint())
                .style(theme::selected_bg()),
        ),
        popup,
    );
}

fn help_row(keys: &str, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{keys:<22}"),
            theme::id(),
        ),
        Span::styled(what.to_string(), theme::muted()),
    ])
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

    #[test]
    fn card_lines_are_padded_to_column_width() {
        let card = Card {
            cli_id: "d5".into(),
            title: "Add auth middleware".into(),
            subtitle: Some("a.txt".into()),
            badges: vec![],
            author: Some("Dani".into()),
            kind: crate::board::CardKind::Commit,
            group: None,
        };
        let lines = render_card(&card, 30, true, false);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            // The trailing spacer line is intentionally empty.
            assert!(w == 30 || w == 0, "line width {w} should fill the card");
        }
    }
}
