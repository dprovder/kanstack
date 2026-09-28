use super::*;

/// How many wrapped lines of one question the inbox shows before eliding the rest — enough
/// to read a real question, not so many that one long one pushes every other entry off
/// screen. `Enter` on it goes to the lane, where the orchestrator's own pane has the rest.
const QUESTION_LINES: usize = 3;

/// The ask inbox (`Mode::Asks`, see `app::asks`): every pending ask on the board, as its
/// branch name above its question, with the `▸` row marker the branch modal uses for the
/// selected one. While answering, a one-line input appears under the selected question
/// (scrolled around its cursor the same way the branch modal's name field is), and the hint
/// line changes to match. Centered and sized to its content like `draw_help`, and scrolled
/// to keep the selected entry in view when there are more asks than fit.
pub(super) fn draw_asks(f: &mut Frame, app: &App, area: Rect) {
    let asks = app.pending_asks();
    let w = 64.min(area.width.saturating_sub(4));
    let content_width = (w as usize).saturating_sub(2);
    let text_width = content_width.saturating_sub(4).max(1);
    let cursor = theme::tone(Tone::Accent);

    let mut body = vec![
        Line::from(vec![
            Span::styled("  ask inbox", theme::muted()),
            Span::styled(format!("  ·  {} pending", asks.len()), theme::faint()),
        ]),
        Line::raw(""),
    ];
    let mut sel_start = 0usize;
    let mut sel_len = 0usize;
    for (i, (_, branch, ask)) in asks.iter().enumerate() {
        let selected = i == app.ask_sel;
        let start = body.len();
        body.push(Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, theme::tone(Tone::Warn)),
            Span::styled(truncate(branch, content_width.saturating_sub(2)), theme::title(selected)),
        ]));
        let mut lines = wrap(&ask.question, text_width);
        if lines.len() > QUESTION_LINES {
            lines.truncate(QUESTION_LINES);
            let last = lines.last_mut().unwrap();
            *last = truncate(&format!("{last} …"), text_width);
        }
        for l in lines {
            body.push(Line::from(vec![Span::raw("    "), Span::styled(l, theme::muted())]));
        }
        if selected && app.ask_answering.is_some() {
            let prefix = "    answer  ";
            let (before, after) = app.ask_answer_input.split_at_cursor();
            let budget = footer_input_budget(content_width as u16, prefix.chars().count(), 0);
            let (before, after) = scroll_input(before, after, budget);
            body.push(Line::from(vec![
                Span::styled(prefix, theme::faint()),
                Span::styled(before, theme::title(true)),
                Span::styled("█", cursor),
                Span::styled(after, theme::title(true)),
            ]));
        }
        body.push(Line::raw(""));
        if selected {
            sel_start = start;
            sel_len = body.len() - start;
        }
    }
    body.push(Line::styled(
        if app.ask_answering.is_some() {
            "  ⏎ send answer      esc back to the list"
        } else {
            "  ↑↓ select      ⏎ go to lane      a answer      esc close"
        },
        theme::faint(),
    ));

    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    // Inside the border. Scrolls only as far as the selected entry needs — with a short
    // list, nothing moves and the title stays on top.
    let inner_h = h.saturating_sub(2) as usize;
    let scroll = (sel_start + sel_len).saturating_sub(inner_h) as u16;
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body)
            .scroll((scroll, 0))
            .block(Block::bordered().border_style(theme::faint()).style(theme::selected_bg())),
        popup,
    );
}
