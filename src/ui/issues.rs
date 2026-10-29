//! The issues view's screens: the item list in the unassigned lane's body, the view switch
//! under that lane's header, the draft modal and the detail popup. State lives in
//! `app::issues`; nothing here talks to a tracker.

use super::*;
use crate::app::{item_label, DraftRow, UnassignedView};
use crate::ui::branch_modal::wrap_ranges;
use kanstack_tracker::{Item, ItemState};

/// An item's state as one glyph and the tone it's drawn in. A filled dot for running work,
/// like a lane's own dot; a hollow one for work waiting its turn; a dotted one for a
/// proposal nobody has accepted yet.
pub(super) fn state_glyph(state: ItemState) -> (&'static str, Tone) {
    match state {
        ItemState::Running => ("●", Tone::Accent),
        ItemState::Backlog => ("○", Tone::Neutral),
        ItemState::Failed => ("✗", Tone::Bad),
        ItemState::Proposed => ("◌", Tone::Neutral),
        ItemState::Review => ("◆", Tone::Warn),
        ItemState::Done => ("✓", Tone::Good),
    }
}

/// The line under the unassigned lane's header naming every view `tab` cycles through, the
/// active one bracketed: `flat · folders · [issues] local`. The tracker's name follows once
/// a list has come back from it, and is the first thing dropped in a narrow lane.
pub(super) fn view_switch_line(app: &App, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, view) in UnassignedView::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", theme::faint()));
        }
        if view == app.unassigned_view {
            spans.push(Span::styled(format!("[{}]", view.label()), theme::tone(Tone::Accent)));
        } else {
            spans.push(Span::styled(view.label(), theme::faint()));
        }
    }
    let width_of = |spans: &[Span]| spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
    if let Some(tracker) = app.issues.tracker.filter(|_| app.showing_issues()) {
        let tracker = Span::styled(format!(" {tracker}"), theme::muted());
        if width_of(&spans) + tracker.content.chars().count() <= width {
            spans.push(tracker);
        }
    }
    if width_of(&spans) > width {
        // Too narrow for all three: just the active one.
        return Line::styled(truncate(&format!("[{}]", app.unassigned_view.label()), width), theme::tone(Tone::Accent));
    }
    Line::from(spans)
}

/// The second line of an item's row: its state, then its recipe (`recipe ?` when it names
/// none) or, for a proposal, the item it came from; then how many lanes it's running on.
pub(super) fn item_meta(app: &App, item: &Item) -> String {
    let mut meta = item.state.to_string();
    match (&item.from, item.state) {
        (Some(from), ItemState::Proposed) => meta.push_str(&format!(" · from {}", item_label(from))),
        _ => meta.push_str(&format!(" · {}", item.recipe.as_deref().unwrap_or("recipe ?"))),
    }
    let lanes = app.item_lanes(&item.id).len();
    if lanes > 0 {
        meta.push_str(&format!(" · {lanes} {}", if lanes == 1 { "lane" } else { "lanes" }));
    }
    meta
}

/// The unassigned lane's body in the issues view: two lines per item, then the
/// "+ draft an issue" row. Returns the lines and where the cursor's row sits in them, for
/// the caller to scroll it into view the same way it does a card.
pub(super) fn issue_rows(app: &App, is_current: bool, width: usize) -> (Vec<Line<'static>>, usize, usize) {
    let issues = &app.issues;
    let mut lines = Vec::new();
    if let Some(error) = &issues.error {
        for (i, l) in wrap(error, width.saturating_sub(4).max(1)).into_iter().enumerate() {
            let lead = if i == 0 { "  ✗ " } else { "    " };
            lines.push(Line::from(vec![
                Span::styled(lead, theme::tone(Tone::Bad)),
                Span::styled(l, theme::tone(Tone::Bad)),
            ]));
        }
        lines.push(Line::raw(""));
    }
    if !issues.loaded && issues.error.is_none() {
        lines.push(Line::styled("  loading issues…", theme::faint()));
        lines.push(Line::raw(""));
    } else if issues.loaded && issues.items.is_empty() {
        lines.push(Line::styled("  no issues yet", theme::faint()));
        lines.push(Line::raw(""));
    }

    let pad = |mut line: Line<'static>, selected: bool| -> Line<'static> {
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        if used < width {
            line.spans.push(Span::raw(" ".repeat(width - used)));
        }
        if selected {
            line = line.style(theme::selected_bg());
        }
        line
    };

    let (mut sel_start, mut sel_len) = (0, 0);
    for (i, item) in issues.items.iter().enumerate() {
        let selected = is_current && i == issues.sel;
        let (glyph, tone) = state_glyph(item.state);
        let label = item_label(&item.id);
        let title_w = width.saturating_sub(3 + label.chars().count());
        if selected {
            sel_start = lines.len();
            sel_len = 2;
        }
        lines.push(pad(
            Line::from(vec![
                Span::styled(format!("{glyph} "), theme::tone(tone)),
                Span::styled(label, theme::id()),
                Span::raw(" "),
                Span::styled(truncate(&item.title, title_w), theme::title(selected)),
            ]),
            selected,
        ));
        lines.push(pad(
            Line::from(vec![
                Span::raw("  "),
                Span::styled(truncate(&item_meta(app, item), width.saturating_sub(2)), theme::faint()),
            ]),
            selected,
        ));
    }

    let draft_selected = is_current && issues.sel >= issues.items.len();
    if draft_selected {
        sel_start = lines.len();
        sel_len = 1;
    }
    lines.push(pad(
        Line::from(vec![
            Span::styled("+ ", theme::tone(Tone::Accent)),
            Span::styled("draft an issue", theme::title(draft_selected)),
        ]),
        draft_selected,
    ));
    (lines, sel_start, sel_len)
}

/// Drafting an issue, as a modal: a one-line title, a body that wraps downward and keeps
/// real line breaks (the PR modal's description treatment), and an optional recipe.
pub(super) fn draw_issue_draft(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let issues = &app.issues;
    let cursor = theme::tone(Tone::Accent);
    let w = 64.min(area.width.saturating_sub(4));
    let content_width = (w as usize).saturating_sub(2);
    let marker = |row: DraftRow| if issues.draft_row == row { "▸ " } else { "  " };

    let heading = match app.issues.tracker {
        Some(tracker) => format!("  draft an issue — {tracker}, in backlog"),
        None => "  draft an issue — in backlog".to_string(),
    };
    let mut body = vec![Line::styled(heading, theme::muted()), Line::raw("")];

    // A single-line field: scrolled around its cursor while focused, right-elided otherwise.
    let single = |prefix: String, input: &crate::text_input::TextInput, focused: bool, empty: (&str, Style)| {
        if input.is_empty() {
            let mut spans = vec![Span::styled(prefix, theme::faint())];
            if focused {
                spans.push(Span::styled("█", cursor));
                spans.push(Span::raw(" "));
            }
            spans.push(Span::styled(empty.0.to_string(), empty.1));
            return Line::from(spans);
        }
        if !focused {
            let value = truncate(input.as_str(), content_width.saturating_sub(prefix.chars().count()));
            return Line::from(vec![Span::styled(prefix, theme::faint()), Span::styled(value, theme::title(true))]);
        }
        let (before, after) = input.split_at_cursor();
        let budget = footer_input_budget(content_width as u16, prefix.chars().count(), 0);
        let (before, after) = scroll_input(before, after, budget);
        Line::from(vec![
            Span::styled(prefix, theme::faint()),
            Span::styled(before, theme::title(true)),
            Span::styled("█", cursor),
            Span::styled(after, theme::title(true)),
        ])
    };

    let title_empty = if issues.draft_title_missing {
        ("required — type a title", theme::tone(Tone::Bad))
    } else {
        ("required", theme::faint())
    };
    body.push(single(
        format!("{}title    ", marker(DraftRow::Title)),
        &issues.draft_title,
        issues.draft_row == DraftRow::Title,
        title_empty,
    ));

    // The body: wrapped downward, the cursor drawn on whichever wrapped line holds it.
    let body_prefix_w = 11;
    let wrap_width = content_width.saturating_sub(body_prefix_w).max(1);
    let on_body = issues.draft_row == DraftRow::Body;
    let text: Vec<char> = issues.draft_body.as_str().chars().collect();
    if text.is_empty() {
        body.push(Line::from(vec![
            Span::styled(format!("{}body     ", marker(DraftRow::Body)), theme::faint()),
            if on_body { Span::styled("█", cursor) } else { Span::styled("(optional)", theme::faint()) },
        ]));
    } else {
        let ranges = wrap_ranges(&text, wrap_width);
        let cursor_idx = issues.draft_body.split_at_cursor().0.chars().count();
        let cursor_line =
            ranges.iter().position(|&(s, e)| cursor_idx >= s && cursor_idx < e).unwrap_or(ranges.len() - 1);
        // A long body would push the recipe row and the hints off the popup: show a window
        // of it around the cursor instead.
        let avail = (area.height as usize).saturating_sub(4 + body.len() + 4).max(1);
        let start = if ranges.len() > avail { cursor_line.saturating_sub(avail / 2).min(ranges.len() - avail) } else { 0 };
        for (li, &(s, e)) in ranges.iter().enumerate().skip(start).take(avail) {
            let label = if li == start {
                Span::styled(format!("{}body     ", marker(DraftRow::Body)), theme::faint())
            } else {
                Span::raw(" ".repeat(body_prefix_w))
            };
            let line: String = text[s..e].iter().filter(|&&c| c != '\n').collect();
            if on_body && li == cursor_line {
                let byte_at = line.char_indices().nth(cursor_idx - s).map(|(b, _)| b).unwrap_or(line.len());
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

    // Picked from the catalog, never typed, so it never shows a text cursor.
    let on_recipe = issues.draft_row == DraftRow::Recipe;
    let recipe_empty = if on_recipe { "tab to pick one" } else { "optional — serve only starts items with one" };
    body.push(single(
        format!("{}recipe   ", marker(DraftRow::Recipe)),
        &issues.draft_recipe,
        false,
        (recipe_empty, if on_recipe { theme::tone(Tone::Accent) } else { theme::faint() }),
    ));

    body.push(Line::raw(""));
    let hint = if on_recipe {
        "  ↑ field    tab pick    ⌫ clear    ⏎ save to backlog    esc discard"
    } else {
        "  ↑↓ field      ⏎ save to backlog      esc discard"
    };
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

/// The recipe picker: a filter line with the match count, then one row per matching recipe —
/// its name, its description, and its tags dimmed — scrolled to keep the cursor in view.
/// Everything comes from the catalog the picker read when it opened; nothing here reads a file.
pub(super) fn draw_recipe_picker(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some(picker) = &app.recipe_picker else { return };
    let w = 84.min(area.width.saturating_sub(4));
    let content_w = (w as usize).saturating_sub(2);
    let cursor = theme::tone(Tone::Accent);

    let (heading, current, confirm) = match &picker.target {
        PickerTarget::Item { id, title, current } => {
            let label = item_label(id);
            let head = format!("  recipe for {label} ");
            let room = content_w.saturating_sub(head.chars().count());
            (
                Line::from(vec![Span::styled(head, theme::muted()), Span::styled(truncate(title, room), theme::title(true))]),
                current.as_deref(),
                "⏎ set",
            )
        }
        PickerTarget::Draft => (Line::styled("  recipe for the new issue", theme::muted()), None, "⏎ use"),
    };
    let mut body = vec![heading, Line::raw("")];

    let count = format!("{}/{}", picker.matches.len(), picker.entries.len());
    let (before, after) = picker.query.split_at_cursor();
    let prefix = "  › ";
    let budget = footer_input_budget(content_w as u16, prefix.chars().count(), count.chars().count() + 2);
    let (before, after) = scroll_input(before, after, budget);
    let used = prefix.chars().count() + before.chars().count() + 1 + after.chars().count();
    let mut filter = vec![
        Span::styled(prefix, theme::tone(Tone::Accent)),
        Span::styled(before, theme::title(true)),
        Span::styled("█", cursor),
        Span::styled(after, theme::title(true)),
    ];
    if picker.query.is_empty() {
        filter.push(Span::styled(" type to filter", theme::faint()));
    }
    let used = used + if picker.query.is_empty() { 15 } else { 0 };
    filter.push(Span::raw(" ".repeat(content_w.saturating_sub(used + count.chars().count() + 2))));
    filter.push(Span::styled(count, theme::faint()));
    body.push(Line::from(filter));
    body.push(Line::styled(format!("  {}", "─".repeat(content_w.saturating_sub(4))), theme::faint()));

    // Rows left once the heading, filter, rule, blank line, hint and border are counted.
    let room = (area.height as usize).saturating_sub(4 + body.len() + 2).clamp(1, 20);
    if picker.entries.is_empty() {
        let dirs = picker.dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
        body.push(Line::styled("  no recipes yet — add one under:", theme::faint()));
        for l in wrap(&dirs, content_w.saturating_sub(4).max(1)) {
            body.push(Line::styled(format!("    {l}"), theme::faint()));
        }
    } else if picker.matches.is_empty() {
        body.push(Line::styled("  nothing matches", theme::faint()));
    } else {
        let n = picker.matches.len();
        let shown = n.min(room);
        let offset = picker.sel.saturating_sub(shown / 2).min(n - shown);
        let name_w = picker.matches[offset..offset + shown]
            .iter()
            .map(|&i| picker.entries[i].name.chars().count())
            .max()
            .unwrap_or(0)
            .min(content_w * 2 / 5);
        for (row, &i) in picker.matches[offset..offset + shown].iter().enumerate() {
            let at = offset + row;
            let entry = &picker.entries[i];
            let selected = at == picker.sel;
            let lead = if selected {
                "▸ "
            } else if row == 0 && offset > 0 {
                "↑ "
            } else if row == shown - 1 && offset + shown < n {
                "↓ "
            } else {
                "  "
            };
            let mut spans = vec![
                Span::styled(lead, if selected { cursor } else { theme::faint() }),
                Span::styled(format!("{:<name_w$}", truncate(&entry.name, name_w)), theme::title(selected)),
            ];
            let mut left = content_w.saturating_sub(2 + name_w);
            let mut push = |text: String, style: Style, spans: &mut Vec<Span<'static>>| {
                if left > 2 && !text.is_empty() {
                    let text = truncate(&format!("  {text}"), left);
                    left -= text.chars().count();
                    spans.push(Span::styled(text, style));
                }
            };
            if current == Some(entry.name.as_str()) {
                push("(current)".to_string(), theme::tone(Tone::Accent), &mut spans);
            }
            push(entry.description.clone().unwrap_or_default(), theme::muted(), &mut spans);
            push(entry.tags.join(" "), theme::faint(), &mut spans);
            let line = Line::from(spans);
            body.push(if selected { line.style(theme::selected_bg()) } else { line });
        }
    }

    body.push(Line::raw(""));
    let hint = format!("  ↑↓ choose    {confirm}    esc cancel");
    body.push(Line::styled(hint.clone(), theme::faint()));

    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    confirm_hitboxes(hits, popup, body.len(), &hint);
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(Block::bordered().border_style(theme::faint()).style(theme::selected_bg())),
        popup,
    );
}

/// The detail popup's width for a terminal `terminal_width` wide, and the width its text
/// wraps to inside the border and indent. Shared with `issue_detail_line_count`, which the
/// app uses to stop scrolling at the last line.
fn detail_widths(terminal_width: u16) -> (u16, usize) {
    let w = 76.min(terminal_width.saturating_sub(4));
    (w, (w as usize).saturating_sub(2 + 4).max(1))
}

/// An item's body (its reports included, which the tracker keeps at the end of it), wrapped
/// to `width`, blank lines kept.
fn detail_body(item: &Item, width: usize) -> Vec<String> {
    let body = item.body.trim();
    if body.is_empty() {
        return vec!["(no body)".to_string()];
    }
    body.lines()
        .flat_map(|line| if line.trim().is_empty() { vec![String::new()] } else { wrap(line, width) })
        .collect()
}

/// How many lines the detail popup's scrolling body has for `item` at this terminal width.
pub(crate) fn issue_detail_line_count(item: &Item, terminal_width: u16) -> usize {
    detail_body(item, detail_widths(terminal_width).1).len()
}

/// One issue, read in full: its title and state pinned at the top, its body and reports
/// scrolling below them.
pub(super) fn draw_issue_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(item) = app.issue_detail() else { return };
    let (w, text_w) = detail_widths(area.width);
    let h = area.height.saturating_sub(2).max(3);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    let (glyph, tone) = state_glyph(item.state);
    let mut head = vec![Line::from(vec![
        Span::styled(format!("  {glyph} "), theme::tone(tone)),
        Span::styled(item_label(&item.id), theme::id()),
        Span::raw(" "),
        Span::styled(truncate(&item.title, text_w.saturating_sub(4)), theme::title(true)),
    ])];
    head.push(Line::styled(format!("    {}", item_meta(app, item)), theme::faint()));
    head.push(Line::styled(format!("  {}", "─".repeat(text_w + 2)), theme::faint()));
    let hint = Line::styled("  ↑↓ scroll      g/G top/bottom      esc close", theme::faint());

    let lines = detail_body(item, text_w);
    let inner = Rect {
        x: popup.x + 1,
        y: popup.y + 1,
        width: popup.width.saturating_sub(2),
        height: popup.height.saturating_sub(2),
    };
    let head_h = (head.len() as u16).min(inner.height);
    let body_h = inner.height.saturating_sub(head_h + 1);
    let max_scroll = lines.len().saturating_sub(body_h as usize) as u16;
    let scroll = app.issues.detail_scroll.min(max_scroll);
    let mut body: Vec<Line<'static>> =
        lines.into_iter().map(|l| Line::from(vec![Span::raw("    "), Span::styled(l, theme::muted())])).collect();
    if scroll < max_scroll && body_h > 0 {
        // Something below: say so on the last visible row's indent.
        let last = (scroll + body_h - 1) as usize;
        body[last].spans[0] = Span::styled("  ↓ ", theme::faint());
    }

    f.render_widget(Clear, popup);
    f.render_widget(Block::bordered().border_style(theme::faint()).style(theme::selected_bg()), popup);
    f.render_widget(Paragraph::new(head), Rect { height: head_h, ..inner });
    f.render_widget(
        Paragraph::new(body).scroll((scroll, 0)),
        Rect { y: inner.y + head_h, height: body_h, ..inner },
    );
    if inner.height > head_h {
        f.render_widget(Paragraph::new(hint), Rect { y: inner.y + inner.height - 1, height: 1, ..inner });
    }
}
