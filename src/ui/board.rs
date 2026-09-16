use super::*;

pub(super) fn pane_status_label(status: PaneStatus) -> &'static str {
    match status {
        PaneStatus::Busy => "● busy",
        PaneStatus::Idle => "○ idle",
        PaneStatus::Dead => "✕ pane closed",
        PaneStatus::Unknown => "… pane",
    }
}

pub(super) fn pane_status_tone(status: PaneStatus) -> Tone {
    match status {
        PaneStatus::Busy => Tone::Accent,
        PaneStatus::Idle => Tone::Neutral,
        PaneStatus::Dead => Tone::Bad,
        PaneStatus::Unknown => Tone::Neutral,
    }
}

/// Chooses a column width and the first visible position (an index into `indices`, not into
/// `app.board.columns` itself) so the current column stays on screen.
pub(super) fn visible_columns(app: &App, indices: &[usize], area: Rect) -> (u16, usize, usize) {
    let (width, fit) = columns_that_fit(indices.len(), area.width);
    let pos = indices.iter().position(|&i| i == app.col).unwrap_or(0);
    // Scroll the window just far enough to include the selected column.
    let first = if pos < fit { 0 } else { pos + 1 - fit };
    (width, first, fit)
}

pub(super) fn draw_board(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    if app.board.columns.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(
                // A blocked workspace also has no columns, but it is empty because nothing
                // could be read — not because there is nothing there. Suggesting a command
                // that `but` is currently refusing would be advice that cannot be followed.
                if app.mode == Mode::Blocked {
                    ""
                } else {
                    "  no applied branches — create one with `but branch new <name>`"
                },
                theme::faint(),
            )),
            area,
        );
        return;
    }

    // While the branches drawer is open, the unassigned lane isn't part of the decision
    // being made (which branch to apply, judged against the lanes already there) — it's
    // just uncommitted worktree changes with nothing to add to that call. Leaving it out
    // hands its column of width back to the lanes that are actually relevant.
    let indices: Vec<usize> = if app.mode == Mode::Branches {
        app.board
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.kind != ColumnKind::Unassigned)
            .map(|(i, _)| i)
            .collect()
    } else {
        (0..app.board.columns.len()).collect()
    };
    if indices.is_empty() {
        f.render_widget(
            Paragraph::new(Line::styled(
                "  no applied branches — create one with `but branch new <name>`",
                theme::faint(),
            )),
            area,
        );
        return;
    }

    let (width, first, fit) = visible_columns(app, &indices, area);
    let last = (first + fit).min(indices.len());

    let mut x = area.x + 1;
    for pos in first..last {
        if x + width > area.x + area.width {
            break;
        }
        let col_area = Rect {
            x,
            y: area.y,
            width,
            height: area.height,
        };
        draw_column(f, app, indices[pos], col_area, hits);
        x += width + COL_GAP;
    }

    // Hint that there is more board off-screen, on whichever side(s) actually have
    // something scrolled past — symmetric now: `first > 0` means lanes are hidden to the
    // left too, which used to have no marker of its own at all, only ever the right-hand
    // one. One mark per side, not a wall of them: repeated down every row it reads as a
    // border between panes rather than as a hint.
    if area.height > 0 {
        let row = area.y + area.height / 2;
        if first > 0 {
            let marker = Rect { x: area.x, y: row, width: 1, height: 1 };
            f.render_widget(Paragraph::new(Line::styled("‹", theme::faint())), marker);
        }
        if last < indices.len() {
            let marker = Rect {
                x: area.x + area.width.saturating_sub(1),
                y: row,
                width: 1,
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::styled("›", theme::faint())), marker);
        }
    }
}

pub(super) fn draw_column(f: &mut Frame, app: &App, idx: usize, area: Rect, hits: &mut HitMap) {
    let col = &app.board.columns[idx];
    let is_current = idx == app.col;
    let inner_w = area.width as usize;

    // Lowest priority: clicking anywhere in the lane (its padding, an empty area below the
    // last card) still selects the lane. More specific regions (header, individual cards)
    // are pushed after this and so win the hit-test.
    hits.push(area, HitTarget::LaneBody(idx));

    // Header: dot, name, count. While moving, the header is itself a drop position, so it
    // highlights when the drop cursor sits on it. Either way, the current lane's header
    // carries a background tint at all times — bold title text and a brighter rule alone
    // read as too subtle to tell which of several lanes is current at a glance (GitHub
    // issue #3); this is the same "focused thing" language `selected_bg` already gives
    // cards, applied one level up.
    let header_is_target =
        app.mode == Mode::Moving && is_current && app.target_card.is_none();
    let hovered =
        app.mode == Mode::Normal && app.hover == Some(HitTarget::LaneHeader(idx));
    // The unassigned lane has no push status of its own for the dot to carry, which
    // leaves it free to do something else: double as the door into the unapplied-branches
    // drawer, the same `‹` the diff pane and the drawer's own header use for "there's a
    // panel here." Only while the drawer is actually reachable from where the click would
    // land — otherwise it's back to being an ordinary (faint, statusless) dot.
    let is_drawer_door = col.kind == ColumnKind::Unassigned && app.mode == Mode::Normal;
    // The count budget used to be a flat 8 (dot + "  " + up to a 2-digit total), but a
    // `position/total` readout like "10/10" can run longer than that — reserve exactly as
    // much as this lane's label actually needs so the title truncates first instead of the
    // count getting clipped.
    let count_label = lane_count_label(app, idx, col);
    let count_budget = 4 + count_label.chars().count();
    let mut header = Line::from(vec![
        if is_drawer_door {
            Span::styled("‹ ", theme::tone(crate::board::Tone::Accent))
        } else {
            Span::styled("● ", theme::status_dot(col.state))
        },
        Span::styled(
            truncate(&col.title, inner_w.saturating_sub(count_budget)),
            theme::title(is_current || hovered),
        ),
        Span::styled(format!("  {count_label}"), theme::faint()),
    ]);
    if let Some((a, r)) = col.stats {
        header.push_span(Span::styled("  ", theme::faint()));
        header.push_span(Span::styled(format!("+{a}"), theme::tone(crate::board::Tone::Good)));
        header.push_span(Span::styled(format!(" -{r}"), theme::tone(crate::board::Tone::Bad)));
    }
    if let Some(status) = col.pane_status {
        header.push_span(Span::styled("  ", theme::faint()));
        header.push_span(Span::styled(pane_status_label(status), theme::tone(pane_status_tone(status))));
    }
    if header_is_target || is_current {
        header = header.style(theme::selected_bg());
    }

    // The header, rule, and top-level badges are pinned — rendered separately from the
    // cards below them, in their own fixed-height area, rather than as the first lines of
    // the same scrolling text. Before this they scrolled away with everything else: on a
    // long stack, moving the cursor down past the visible cards lost the lane name, dot,
    // and status along with it — the one thing you'd want to still see to know where you
    // are and what the top of the stack was.
    let mut fixed_lines = vec![header];

    // A rule under the header, brighter on the active lane.
    let rule_style = if is_current {
        theme::muted()
    } else {
        theme::faint()
    };
    fixed_lines.push(Line::styled("─".repeat(inner_w), rule_style));

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
        fixed_lines.push(Line::from(spans));
    }

    let fixed_h = (fixed_lines.len() as u16).min(area.height);
    let header_rect = Rect { height: fixed_h, ..area };
    f.render_widget(Paragraph::new(fixed_lines), header_rect);
    hits.push(header_rect, HitTarget::LaneHeader(idx));
    // Narrower and pushed after, so it wins the hit-test over the header it sits inside:
    // clicking the `‹` itself opens the drawer rather than just selecting the lane.
    if is_drawer_door {
        hits.push(
            Rect { x: area.x, y: area.y, width: 2, height: 1 },
            HitTarget::OpenBranches,
        );
    }
    if area.height <= fixed_h {
        return;
    }
    let body_area = Rect {
        y: area.y + fixed_h,
        height: area.height - fixed_h,
        ..area
    };

    // Cards, tracking which lines belong to the cursor so we can scroll to it. Everything
    // from here down is `body_area`'s own scrolling content, independent of the fixed
    // header above it.
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut sel_start = 0usize;
    let mut sel_len = 0usize;
    let mut last_group: Option<&str> = None;
    // Content-line range of each card, for turning a click on the scrolled `Paragraph`
    // below back into a card index — there's no per-card `Rect` otherwise, since cards
    // share one `Paragraph` and vary in height (wrapped titles, badges, group headers).
    let mut card_spans: Vec<(usize, usize, usize)> = Vec::with_capacity(col.cards.len());

    // Folder counts for the unassigned lane's own group headers (GitHub issue #6) — cheap
    // and only built when there is a group to count, since `col.cards.len()` is at most a
    // few hundred even in a large workspace.
    let mut folder_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    if col.kind == ColumnKind::Unassigned {
        for card in &col.cards {
            if let Some(g) = card.group.as_deref() {
                *folder_counts.entry(g).or_insert(0) += 1;
            }
        }
    }

    for (ci, card) in col.cards.iter().enumerate() {
        if let Some(group) = card.group.as_deref() {
            if last_group != Some(group) {
                if col.kind == ColumnKind::Unassigned {
                    lines.push(Line::raw(""));
                    lines.extend(folder_header(
                        group,
                        folder_counts.get(group).copied().unwrap_or(0),
                        inner_w,
                    ));
                } else {
                    // The tip branch is already described by the lane header; the ones
                    // below it get the same treatment here so the lane reads as visibly
                    // stacked.
                    let is_tip = col.sections.first().is_some_and(|s| s.name == group);
                    if !is_tip {
                        if let Some(section) = col.sections.iter().find(|s| s.name == group) {
                            lines.push(Line::raw(""));
                            lines.extend(section_header(section, inner_w));
                        }
                    }
                }
                last_group = Some(group);
            }
        }

        let picked = app.mode == Mode::Moving && idx == app.origin_col && ci == app.origin_card;
        // In move mode the cursor marks the drop position, not the browsing position.
        let selected = if app.mode == Mode::Moving {
            is_current && app.target_card == Some(ci)
        } else {
            is_current && ci == app.card
        };
        let start = lines.len();
        let checked = app.selected.contains(&card.rub_id);
        let hovered = app.mode == Mode::Normal
            && app.hover == Some(HitTarget::Card(idx, ci));
        let card_lines = render_card(card, inner_w, selected, picked, checked, hovered);
        if selected {
            sel_start = start;
            sel_len = card_lines.len();
        }
        card_spans.push((ci, start, card_lines.len()));
        lines.extend(card_lines);
    }

    // A branch lane already carries an "empty" status badge, so a placeholder underneath
    // it just says the same thing twice. Only the backlog lane, which has no badges of its
    // own, needs one.
    if col.cards.is_empty() && col.badges.is_empty() {
        lines.push(Line::styled("  empty", theme::faint()));
    }

    // Keep the selected card in view within the scrollable body — the header above stays
    // put regardless, so this only ever has to account for `body_area`'s own height.
    let body_h = body_area.height as usize;
    let scroll = if sel_len > 0 && sel_start + sel_len > body_h {
        (sel_start + sel_len - body_h) as u16
    } else {
        0
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default())
            .scroll((scroll, 0)),
        body_area,
    );

    // Clip each card's content-line range to what `scroll` actually leaves on screen, then
    // record the resulting strip as this card's clickable region. A card fully scrolled off
    // (or only its blank trailing separator visible) contributes an empty/zero-height rect,
    // which `HitMap::push` drops.
    let visible = scroll as usize..scroll as usize + body_area.height as usize;
    for (ci, start, len) in card_spans {
        let lo = start.max(visible.start);
        let hi = (start + len).min(visible.end);
        if hi <= lo {
            continue;
        }
        hits.push(
            Rect {
                x: body_area.x,
                y: body_area.y + (lo - scroll as usize) as u16,
                width: body_area.width,
                height: (hi - lo) as u16,
            },
            HitTarget::Card(idx, ci),
        );
    }
}

/// The count shown next to a lane's name.
///
/// For a plain lane that is just its cards. For a stack it must be the *tip branch's*
/// share, not the lane total, because the branches below print their own counts right
/// underneath — a lane reading `board-ops +1  2` above a `live-refresh  1` says board-ops
/// has two commits, which it does not.
pub(super) fn header_count(col: &crate::board::Column) -> usize {
    if col.sections.len() < 2 {
        return col.cards.len();
    }
    let below: usize = col.sections.iter().skip(1).map(|s| s.commits).sum();
    // Whatever is left is the tip's commits plus any changes staged to the stack.
    col.cards.len().saturating_sub(below)
}

/// The lane header's count: a plain total normally, but for the current lane a
/// `position/total` — the card counter used to live in the app header, but a long stack
/// already keeps its own header pinned while the cursor scrolls out of view within it, so
/// this is the one place that still says where you are.
pub(super) fn lane_count_label(app: &App, idx: usize, col: &crate::board::Column) -> String {
    // `app.card` indexes `col.cards` in full (every section, not just the tip), so the
    // position it reports has to share that denominator rather than `header_count`'s
    // tip-only total.
    if idx == app.col && col.cards.len() > 1 {
        format!("{}/{}", app.card + 1, col.cards.len())
    } else {
        header_count(col).to_string()
    }
}

/// A branch header inside a lane, drawn like the lane header itself: dot, name, count,
/// rule, status. Repeating the treatment is the point — it is what makes a stack look
/// stacked rather than like one list with faint dividers in it.
///
/// Only ever called for a *non-tip* branch (the tip's header is the lane header itself),
/// which is exactly the distinction its hollow dot exists to carry: scrolling down into a
/// lower branch's cards used to look just as "current" as the tip, with nothing on screen
/// to say that `c`/`p`/`M`/`z` still act on the tip above, not whatever's on screen (GitHub
/// issue #3). A hollow dot here versus the lane header's filled one is the same "state
/// carried by the dot" convention the badge/dot pairing already uses elsewhere — see
/// `theme::status_dot`.
/// A directory divider inside the unassigned lane (GitHub issue #6), when grouped by
/// folder (`tab`). Lighter than `section_header` on purpose — a folder has no push status,
/// no branch dot; it is just a place a name of one already-loose file happens to sit.
pub(super) fn folder_header(name: &str, count: usize, width: usize) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("▸ ", theme::faint()),
            Span::styled(truncate(name, width.saturating_sub(6)), theme::title(false)),
            Span::styled(format!("  {count}"), theme::faint()),
        ]),
        Line::styled("─".repeat(width), theme::faint()),
    ]
}

pub(super) fn section_header(section: &crate::board::Section, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(vec![
            Span::styled("○ ", theme::status_dot(Some(section.state))),
            Span::styled(
                truncate(&section.name, width.saturating_sub(8)),
                theme::title(false),
            ),
            Span::styled(format!("  {}", section.commits), theme::faint()),
        ]),
        Line::styled("─".repeat(width), theme::faint()),
    ];
    if let Some((a, r)) = section.stats {
        out[0].push_span(Span::styled("  ", theme::faint()));
        out[0].push_span(Span::styled(
            format!("+{a}"),
            theme::tone(crate::board::Tone::Good),
        ));
        out[0].push_span(Span::styled(
            format!(" -{r}"),
            theme::tone(crate::board::Tone::Bad),
        ));
    }
    if let Some(status) = section.pane_status {
        out[0].push_span(Span::styled("  ", theme::faint()));
        out[0].push_span(Span::styled(pane_status_label(status), theme::tone(pane_status_tone(status))));
    }
    if !section.badges.is_empty() {
        out.push(Line::from(
            section
                .badges
                .iter()
                .flat_map(|b| {
                    [
                        Span::styled(b.text.clone(), theme::tone(b.tone)),
                        Span::raw("  "),
                    ]
                })
                .collect::<Vec<_>>(),
        ));
    }
    out
}

pub(super) fn render_card(
    card: &Card,
    width: usize,
    selected: bool,
    picked: bool,
    checked: bool,
    hovered: bool,
) -> Vec<Line<'static>> {
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

    // A check marking the card as part of a pending bulk move (`space`), the id badge,
    // and a caret marking the card in hand.
    let mut id_spans = Vec::new();
    if checked {
        id_spans.push(Span::styled("✓ ", theme::tone(crate::board::Tone::Accent)));
    }
    id_spans.push(Span::styled(card.cli_id.clone(), theme::id()));
    if picked {
        id_spans.push(Span::styled(
            "  ⤴ moving",
            theme::tone(crate::board::Tone::Accent),
        ));
    }
    out.push(pad(Line::from(id_spans)));

    for l in wrap(&card.title, width) {
        out.push(pad(Line::from(Span::styled(l, theme::title(selected || hovered)))));
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
    if let Some((a, r)) = card.stats {
        meta.push(Span::styled(
            format!("+{a}"),
            theme::tone(crate::board::Tone::Good),
        ));
        meta.push(Span::styled(
            format!(" -{r}  "),
            theme::tone(crate::board::Tone::Bad),
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_lines_are_padded_to_column_width() {
        let card = Card {
            cli_id: "d5".into(),
            stats: Some((3, 1)),
            rub_id: "d5aa11bb".into(),
            title: "Add auth middleware".into(),
            subtitle: Some("a.txt".into()),
            badges: vec![],
            author: Some("Dani".into()),
            kind: crate::board::CardKind::Commit,
            group: None,
        };
        let lines = render_card(&card, 30, true, false, false, false);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            // The trailing spacer line is intentionally empty.
            assert!(w == 30 || w == 0, "line width {w} should fill the card");
        }
    }
}
