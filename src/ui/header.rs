use super::*;


pub(super) fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let b = &app.board;
    let mut spans = vec![
        Span::styled("  workspace", theme::muted()),
        Span::styled("  ·  ", theme::faint()),
        Span::styled(format!("base {}", b.base_short_id), theme::faint()),
    ];
    // A constant, always-visible "where am I" among the lanes — not just the `‹`/`›`
    // edge markers, which only ever say "there's more", never how much more or which lane
    // out of how many you're currently on. Shown whenever there's more than one lane,
    // whether or not they all currently fit on screen, so it orients you the moment you've
    // moved, not only once you've scrolled far enough to notice something's missing.
    if b.columns.len() > 1 {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("lane {}/{}", app.col + 1, b.columns.len()),
            theme::faint(),
        ));
    }
    // Working-tree cards only. Lane totals now include commits, but the word here is
    // "uncommitted", so counting those would make the header say something untrue.
    let totals = b
        .columns
        .iter()
        .flat_map(|c| c.cards.iter())
        .filter(|c| c.kind == crate::board::CardKind::Change)
        .filter_map(|c| c.stats)
        .fold((0usize, 0usize), |(a, r), (ca, cr)| (a + ca, r + cr));
    if totals != (0, 0) {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("+{}", totals.0),
            theme::tone(crate::board::Tone::Good),
        ));
        spans.push(Span::styled(
            format!(" -{}", totals.1),
            theme::tone(crate::board::Tone::Bad),
        ));
        spans.push(Span::styled(" uncommitted", theme::faint()));
    }
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
    // A selection can span lanes, and its cards can scroll off screen individually — this
    // is the one place it's always visible, the same reasoning as the lane position above.
    if !app.selected.is_empty() {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("{} selected", app.selected.len()),
            theme::tone(crate::board::Tone::Accent),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}
