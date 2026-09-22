use super::*;

/// The conflict picker: a list of a conflicted commit's files down the left, the selected
/// file's ours/theirs content below it. Not a plain confirm dialog — there's no single
/// "yes" action, so unlike `draw_rebase_confirm` and its neighbours in `confirms`, this
/// lives in a file of its own.
pub(super) fn draw_resolve_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(view) = &app.resolve_view else {
        return;
    };

    let mut body = vec![
        Line::from(vec![
            Span::raw("  resolve conflicts — "),
            Span::styled(view.title.clone(), theme::title(true)),
        ]),
        Line::raw(""),
    ];

    for (i, file) in view.data.files.iter().enumerate() {
        let marker = if i == view.selected { "▸ " } else { "  " };
        let n = file.hunks.len();
        body.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                format!("{:<28}", truncate(&file.path, 28)),
                theme::title(i == view.selected),
            ),
            Span::styled(
                format!("{n} conflict{}", if n == 1 { "" } else { "s" }),
                theme::faint(),
            ),
        ]));
    }
    for (i, manual) in view.data.manual.iter().enumerate() {
        let row = view.data.files.len() + i;
        let marker = if row == view.selected { "▸ " } else { "  " };
        body.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(
                format!("{:<28}", truncate(&manual.path, 28)),
                theme::title(row == view.selected),
            ),
            Span::styled("needs a manual edit", theme::tone(crate::board::Tone::Warn)),
        ]));
    }

    if let Some(file) = view.data.files.get(view.selected) {
        body.push(Line::raw(""));
        for hunk in file.hunks.iter().take(3) {
            body.push(Line::from(vec![
                Span::styled("    ours    ", theme::faint()),
                Span::styled(truncate(hunk.ours.trim(), 44), theme::title(false)),
            ]));
            body.push(Line::from(vec![
                Span::styled("    theirs  ", theme::faint()),
                Span::styled(truncate(hunk.theirs.trim(), 44), theme::title(false)),
            ]));
        }
        if file.hunks.len() > 3 {
            body.push(Line::styled(
                format!("    … and {} more", file.hunks.len() - 3),
                theme::faint(),
            ));
        }
    } else if let Some(manual) = view
        .data
        .manual
        .get(view.selected.saturating_sub(view.data.files.len()))
    {
        body.push(Line::raw(""));
        body.push(Line::styled(format!("    {}", manual.reason), theme::faint()));
    }

    let hint = "  ↑/↓ file · o ours · t theirs · A ask the agent · esc cancel";
    body.push(Line::raw(""));
    body.push(Line::styled(hint, theme::faint()));

    let w = 68.min(area.width.saturating_sub(4));
    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    // No single confirm action to hit-test, unlike a plain yes/no dialog — clicking a row
    // to select it is left for later, so only the popup itself (as a dismiss target) and
    // the keyboard are wired up for now.
    hits.push(popup, HitTarget::Dismiss);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(
            Block::bordered()
                .border_style(theme::tone(crate::board::Tone::Bad))
                .style(theme::selected_bg()),
        ),
        popup,
    );
}
