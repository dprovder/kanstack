use super::*;
use crate::app::PrModalRow;
use crate::ui::branch_modal::{hint_row, wrap_ranges};

/// Opening a PR, as a modal: a title, a description that wraps downward as it grows (the
/// same treatment `draw_branch_modal` gives its own harness-message field — one flat
/// string, word-wrapped for display, no real newline in either one), and a draft toggle,
/// all visible and editable at once before anything runs.
pub(super) fn draw_pr_modal(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let cursor = theme::tone(crate::board::Tone::Accent);
    let w = 64.min(area.width.saturating_sub(4));
    let content_width = (w as usize).saturating_sub(2);

    let title_text = match app.pr_target_label() {
        Some(t) => format!("  open a PR — {t}"),
        None => "  open a PR".to_string(),
    };
    let mut body = vec![Line::styled(title_text, theme::muted()), Line::raw("")];

    let row_marker = |row: PrModalRow| -> &'static str {
        if app.pr_modal_row == row { "▸ " } else { "  " }
    };

    // Title: a single line, scrolled horizontally — the same treatment the branch modal
    // gives its own name field.
    let title_prefix = format!("{}title    ", row_marker(PrModalRow::Title));
    // A description with no title is the one combination `confirm_pr` refuses — the title
    // is the first line of `but pr new -m` — so an empty title is flagged in place exactly
    // when that's the state, rather than only via a footer notice after the fact.
    let title_missing = app.pr_title_input.is_empty() && !app.pr_message_input.is_empty();
    let title_focused = app.pr_modal_row == PrModalRow::Title;
    if title_missing {
        let mut spans = vec![Span::styled(title_prefix, theme::faint())];
        if title_focused {
            spans.push(Span::styled("█", cursor));
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            "required — a description needs a title",
            theme::tone(crate::board::Tone::Bad),
        ));
        body.push(Line::from(spans));
    } else if title_focused {
        let (before, after) = app.pr_title_input.split_at_cursor();
        let budget = footer_input_budget(content_width as u16, title_prefix.chars().count(), 0);
        let (before, after) = scroll_input(before, after, budget);
        body.push(Line::from(vec![
            Span::styled(title_prefix, theme::faint()),
            Span::styled(before, theme::title(true)),
            Span::styled("█", cursor),
            Span::styled(after, theme::title(true)),
        ]));
    } else {
        // Another row has focus: no text cursor, just the value, right-elided to fit.
        let title = truncate(app.pr_title_input.as_str(), content_width.saturating_sub(title_prefix.chars().count()));
        body.push(Line::from(vec![
            Span::styled(title_prefix, theme::faint()),
            Span::styled(title, theme::title(true)),
        ]));
    }

    // Description: wraps downward across as many lines as it needs.
    let message_prefix = "  message  ";
    let prefix_width = message_prefix.chars().count();
    let wrap_width = content_width.saturating_sub(prefix_width);
    let on_message_row = app.pr_modal_row == PrModalRow::Message;
    let text: Vec<char> = app.pr_message_input.as_str().chars().collect();
    if text.is_empty() {
        let label = format!("{}message  ", row_marker(PrModalRow::Message));
        body.push(Line::from(vec![
            Span::styled(label, theme::faint()),
            if on_message_row {
                Span::styled("█", cursor)
            } else {
                Span::styled("(optional)", theme::faint())
            },
        ]));
    } else {
        let ranges = wrap_ranges(&text, wrap_width.max(1));
        let cursor_idx = app.pr_message_input.split_at_cursor().0.chars().count();
        let cursor_line = ranges
            .iter()
            .position(|&(s, e)| cursor_idx >= s && cursor_idx < e)
            .unwrap_or(ranges.len() - 1);
        for (li, &(s, e)) in ranges.iter().enumerate() {
            let label = if li == 0 {
                Span::styled(format!("{}message  ", row_marker(PrModalRow::Message)), theme::faint())
            } else {
                Span::raw(" ".repeat(prefix_width))
            };
            let line: String = text[s..e].iter().collect();
            if on_message_row && li == cursor_line {
                let byte_at = line
                    .char_indices()
                    .nth(cursor_idx - s)
                    .map(|(b, _)| b)
                    .unwrap_or(line.len());
                let (before, after) = line.split_at(byte_at);
                body.push(Line::from(vec![
                    label,
                    Span::styled(before.to_string(), theme::title(true)),
                    Span::styled("█", cursor),
                    Span::styled(after.to_string(), theme::title(true)),
                ]));
            } else {
                body.push(Line::from(vec![label, Span::styled(line, theme::title(true))]));
            }
        }
    }

    if app.pr_title_input.is_empty() && app.pr_message_input.is_empty() {
        body.push(Line::styled(
            "             (leave both empty to use the branch's own commit message)",
            theme::faint(),
        ));
    }

    let (glyph, style) = if app.pr_draft {
        ("[x] draft", theme::tone(crate::board::Tone::Accent))
    } else {
        ("[ ] draft", theme::faint())
    };
    body.push(hint_row(
        &format!("{}{:<9}", row_marker(PrModalRow::Draft), ""),
        glyph,
        style,
        "←/→ tab",
        content_width,
    ));

    body.push(Line::raw(""));
    let hint = "  ↑↓ row      ⏎ open PR      esc cancel";
    body.push(Line::styled(hint, theme::faint()));

    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    confirm_hitboxes(hits, popup, body.len(), hint);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(Block::bordered().border_style(theme::faint()).style(theme::selected_bg())),
        popup,
    );
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A small popup shown while `but pr new` runs on a background thread. No key hints — there
/// is nothing to press, `poll_pr` is what closes this. Mirrors `draw_landing`.
pub(super) fn draw_pr_running(f: &mut Frame, app: &App, area: Rect) {
    let Some(pending) = &app.pr_running else {
        return;
    };
    let frame = SPINNER[pending.spinner % SPINNER.len()];

    let body = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(frame.to_string(), theme::tone(crate::board::Tone::Accent)),
            Span::raw("  "),
            Span::styled(format!("opening a PR for {}…", pending.title), theme::title(true)),
        ]),
        Line::raw(""),
    ];

    let w = 50.min(area.width.saturating_sub(4));
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
