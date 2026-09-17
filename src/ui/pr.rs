use super::*;
use crate::app::PrModalRow;
use crate::ui::branch_modal::hint_row;

/// Opening a PR, as a modal: a title field (empty falls back to `but pr new --default`,
/// the branch's own commit message) and a draft toggle, both visible at once — the same
/// shape `draw_branch_modal` uses for branch creation, just for the two fields `but pr new`
/// needs instead of three.
pub(super) fn draw_pr_modal(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let cursor = theme::tone(crate::board::Tone::Accent);
    let w = 60.min(area.width.saturating_sub(4));
    let content_width = (w as usize).saturating_sub(2);

    let title_text = match app.pr_target_label() {
        Some(t) => format!("  open a PR — {t}"),
        None => "  open a PR".to_string(),
    };
    let mut body = vec![Line::styled(title_text, theme::muted()), Line::raw("")];

    let row_marker = |row: PrModalRow| -> &'static str {
        if app.pr_modal_row == row { "▸ " } else { "  " }
    };

    let title_prefix = format!("{}title    ", row_marker(PrModalRow::Title));
    let (before, after) = app.pr_title_input.split_at_cursor();
    let budget = footer_input_budget(content_width as u16, title_prefix.chars().count(), 0);
    let (before, after) = scroll_input(before, after, budget);
    body.push(Line::from(vec![
        Span::styled(title_prefix, theme::faint()),
        Span::styled(before, theme::title(true)),
        Span::styled("█", cursor),
        Span::styled(after, theme::title(true)),
    ]));
    if app.pr_title_input.is_empty() {
        body.push(Line::styled(
            "           (empty uses the branch's own commit message)",
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
