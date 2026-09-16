use super::*;


/// The diff pane, full screen.
///
/// Hunks are listed one after another with the cursor on one of them, because a hunk is
/// the unit `but` will stage — pressing `m` here picks up exactly the hunk under the
/// cursor, which is how one file ends up split across two lanes.
pub(super) fn draw_diff(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    use crate::diff::LineKind;
    let Some(view) = &app.diff else { return };

    f.render_widget(Clear, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);

    // A dedicated close control, rather than the whole pane: a click anywhere used to
    // leave the diff, same as `Esc`, but that closed it on anyone just clicking to read —
    // selecting text, or reflexively clicking the way you would in any other reader.
    let back = Rect { x: chunks[0].x, y: chunks[0].y, width: 2, height: 1 };
    hits.push(back, HitTarget::Dismiss);

    let (added, removed) = view.totals();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("‹ ", theme::tone(crate::board::Tone::Accent)),
            Span::styled(view.title.clone(), theme::title(true)),
            Span::styled("   ", theme::faint()),
            Span::styled(format!("+{added}"), theme::tone(crate::board::Tone::Good)),
            Span::raw(" "),
            Span::styled(format!("-{removed}"), theme::tone(crate::board::Tone::Bad)),
            Span::styled(
                format!("   hunk {} of {}", view.cursor + 1, view.entries.len()),
                theme::faint(),
            ),
        ])),
        chunks[0],
    );

    let w = chunks[1].width as usize;
    let mut lines: Vec<Line> = Vec::new();
    let mut sel_start = 0usize;
    let mut sel_len = 0usize;

    for (i, entry) in view.entries.iter().enumerate() {
        let selected = i == view.cursor;
        let start = lines.len();

        let mut head = vec![
            Span::styled(if selected { "▌ " } else { "  " }, theme::tone(crate::board::Tone::Accent)),
            Span::styled(entry.path.clone(), theme::title(selected)),
            Span::styled(format!("  +{} -{}", entry.added, entry.removed), theme::faint()),
        ];
        if entry.rub_id.is_none() {
            head.push(Span::styled("  committed", theme::faint()));
        }
        lines.push(Line::from(head));

        for l in &entry.lines {
            let (sign, style) = match l.kind {
                LineKind::Added => ("+", theme::tone(crate::board::Tone::Good)),
                LineKind::Removed => ("-", theme::tone(crate::board::Tone::Bad)),
                LineKind::Header => ("~", theme::tone(crate::board::Tone::Accent)),
                LineKind::Context => (" ", theme::muted()),
            };
            let no = l
                .new_no
                .map(|n| format!("{n:>4} "))
                .unwrap_or_else(|| "     ".to_string());
            // The sign is its own column rather than glued to the text: colour alone is
            // not a marker you can rely on, and `+foo` reads as content.
            let body = truncate(&l.text, w.saturating_sub(9));
            lines.push(Line::from(vec![
                Span::styled(if selected { "▌" } else { " " }, theme::tone(crate::board::Tone::Accent)),
                Span::styled(no, theme::faint()),
                Span::styled(format!("{sign} "), style),
                Span::styled(body, style),
            ]));
        }
        lines.push(Line::raw(""));

        if selected {
            sel_start = start;
            sel_len = lines.len() - start;
        }
    }

    // Keep the selected hunk on screen, showing its start rather than its end.
    let h = chunks[1].height as usize;
    let scroll = if sel_start + sel_len > h && sel_start + h > sel_len {
        sel_start.min(sel_start + sel_len - h) as u16
    } else {
        0
    };
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), chunks[1]);
}
