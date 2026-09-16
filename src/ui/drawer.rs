use super::*;


/// The drawer's detail view for one branch — what `⏎` opens from the list, and what `Esc`
/// backs out of one level to return to it (see the `Mode::Branches` key handling in
/// `App::handle_key`). Reads the same [`MergeCheck`](crate::model::MergeCheck) payload
/// `draw_land_confirm` does for an applied lane's own land-preview: the branch's own
/// commits, and, when it wouldn't merge cleanly, exactly which files collide and how many
/// upstream commits they collide with — the detail the list's bare ●/✗ dot can't carry.
pub(super) fn draw_branch_preview(f: &mut Frame, preview: &BranchPreview, area: Rect) {
    let w = area.width as usize;
    let fixed = vec![
        Line::from(vec![
            Span::styled("‹ ", theme::tone(crate::board::Tone::Accent)),
            Span::styled(
                truncate(&preview.name, w.saturating_sub(2)),
                theme::title(true),
            ),
        ]),
        Line::styled("─".repeat(w), theme::muted()),
    ];
    let fixed_h = (fixed.len() as u16).min(area.height);
    f.render_widget(
        Paragraph::new(fixed),
        Rect { height: fixed_h, ..area },
    );
    if area.height <= fixed_h {
        return;
    }
    let body = Rect {
        y: area.y + fixed_h,
        height: area.height - fixed_h,
        ..area
    };

    let check = &preview.check;
    let mut lines: Vec<Line<'static>> = vec![
        Line::styled(
            format!(
                "{} commit{}",
                check.commits_ahead,
                if check.commits_ahead == 1 { "" } else { "s" }
            ),
            theme::title(false),
        ),
    ];
    if preview.stale {
        lines.push(Line::styled(
            "looks stale — old, and no longer merges cleanly",
            theme::tone(crate::board::Tone::Warn),
        ));
    }
    lines.push(Line::raw(""));
    // Each commit gets the same shape a board card does: a short id, the subject wrapped
    // across as many lines as it needs rather than clipped to one, and a blank line to
    // separate it from the next — the layout already proven legible, just reused here.
    for c in &check.commits {
        lines.push(Line::styled(c.short_sha.clone(), theme::id()));
        for l in wrap(c.subject(), w) {
            lines.push(Line::styled(l, theme::muted()));
        }
        if let Some(stats) = commit_stat_line(c) {
            lines.push(stats);
        }
        lines.push(Line::raw(""));
    }
    if check.merge_check.merges_cleanly {
        lines.push(Line::styled(
            "merges cleanly",
            theme::tone(crate::board::Tone::Good),
        ));
    } else {
        let n = check.merge_check.conflicting_files.len();
        lines.push(Line::styled(
            format!("{n} conflicting file{}", if n == 1 { "" } else { "s" }),
            theme::tone(crate::board::Tone::Bad),
        ));
        lines.push(Line::raw(""));
        for file in &check.merge_check.conflicting_files {
            for l in wrap(&file.path, w) {
                lines.push(Line::styled(l, theme::title(false)));
            }
            if !file.upstream_commits.is_empty() {
                let uc = file.upstream_commits.len();
                lines.push(Line::styled(
                    format!("vs {uc} upstream commit{}", if uc == 1 { "" } else { "s" }),
                    theme::faint(),
                ));
            }
            lines.push(Line::raw(""));
        }
    }

    f.render_widget(Paragraph::new(lines).scroll((preview.scroll, 0)), body);
}

/// The unapplied-branches drawer.
///
/// A list, not a column of lanes, and deliberately so: `but` will not enumerate an
/// unapplied branch's commits without applying it first, so there are no cards to draw.
/// Rendering it as a lane would mean inventing the contents. What it shows instead is
/// everything `but branch list` knows from the outside — chiefly whether applying would
/// conflict, the one fact the row itself carries; `⏎` opens the full detail behind it (see
/// `draw_branch_preview`).
pub(super) fn draw_branches(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    // A column of margin on each side. The right-hand one matters now that the board sits
    // on that side: without it the rule under the header would run straight into the first
    // lane, and the drawer would read as part of it rather than as its own panel.
    let area = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(2),
        ..area
    };
    let w = area.width as usize;
    let n = app.unapplied.branches.len();

    // A close control at the start of the header, the same convention (and the same
    // `Dismiss` target) the diff pane's own `‹` uses — one click closes whichever side
    // panel has it, without the rest of the panel's clicks (selecting a row here, reading
    // a diff there) being swallowed by an anywhere-closes rule. Shared by both the list and
    // the detail view below: `Dismiss` just simulates `Esc`, and `Esc` already means
    // "back one level" in whichever of the two is showing.
    let back = Rect { x: area.x, y: area.y, width: 2, height: 1 };
    hits.push(back, HitTarget::Dismiss);

    if let Some(preview) = &app.branch_preview {
        draw_branch_preview(f, preview, area);
        return;
    }

    let mut fixed = vec![
        Line::from(vec![
            Span::styled("‹ ", theme::tone(crate::board::Tone::Accent)),
            Span::styled("unapplied", theme::title(true)),
            Span::styled(format!("  {n}"), theme::faint()),
        ]),
        Line::styled("─".repeat(w), theme::muted()),
    ];
    // Say so when the list is partial rather than presenting 20 branches as if they were
    // all of them — `but branch list` truncates by default and reports that it did. A
    // second line rather than one long one crammed against `truncate`'s ellipsis: the
    // drawer is only ~32 columns wide here, too narrow for both halves on one line.
    if app.unapplied.truncated {
        fixed.push(Line::styled(
            truncate("only the 20 most recent", w),
            theme::tone(crate::board::Tone::Warn),
        ));
        fixed.push(Line::styled(
            truncate("A loads all", w),
            theme::tone(crate::board::Tone::Warn),
        ));
    }
    let fixed_h = (fixed.len() as u16).min(area.height);
    f.render_widget(
        Paragraph::new(fixed),
        Rect {
            height: fixed_h,
            ..area
        },
    );
    if area.height <= fixed_h {
        return;
    }
    let body = Rect {
        y: area.y + fixed_h,
        height: area.height - fixed_h,
        ..area
    };

    if n == 0 {
        f.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::styled(
                    truncate("every branch is already", w),
                    theme::faint(),
                ),
                Line::styled(truncate("in the workspace.", w), theme::faint()),
            ]),
            body,
        );
        return;
    }

    // `but branch list` already orders local branches before remote-only ones (see
    // `Unapplied::from_list`), so the one boundary worth marking is wherever `has_local`
    // first goes false — skipped entirely when the list is all one or the other, since a
    // divider with nothing on one side of it is just noise.
    let remote_count = app.unapplied.branches.iter().filter(|b| !b.has_local).count();
    let mixed = remote_count > 0 && remote_count < n;

    // Built alongside `lines` rather than derived from `ROWS_PER_BRANCH * i`: the divider
    // makes rows non-uniform in height, so each row's start has to be tracked as it's
    // rendered, the same way the board tracks `card_spans` for its own variable-height
    // headers.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(n * ROWS_PER_BRANCH + 2);
    let mut row_starts: Vec<u16> = Vec::with_capacity(n);
    let mut divider_drawn = false;
    for (i, b) in app.unapplied.branches.iter().enumerate() {
        if mixed && !b.has_local && !divider_drawn {
            lines.extend(remote_divider(remote_count, w));
            divider_drawn = true;
        }
        row_starts.push(lines.len() as u16);
        let hovered = app.mode == Mode::Branches && app.hover == Some(HitTarget::BranchRow(i));
        lines.extend(render_unapplied(b, w, i == app.branch_sel, hovered));
    }

    // Scroll just far enough to keep the whole selected row on screen, the same rule the
    // board uses for lanes — the meta line matters as much as the name, so the unit kept
    // visible is the row, not its first line.
    let sel_top = row_starts[app.branch_sel];
    let sel_bottom = sel_top + ROWS_PER_BRANCH as u16;
    let offset = sel_bottom.saturating_sub(body.height);
    f.render_widget(Paragraph::new(lines).scroll((offset, 0)), body);

    for (i, &top) in row_starts.iter().enumerate() {
        let lo = top.max(offset);
        let hi = (top + ROWS_PER_BRANCH as u16).min(offset + body.height);
        if hi <= lo {
            continue;
        }
        hits.push(
            Rect {
                x: body.x,
                y: body.y + (lo - offset),
                width: body.width,
                height: hi - lo,
            },
            HitTarget::BranchRow(i),
        );
    }
}

/// The one-time boundary between local and remote-only branches in the drawer — the same
/// "label, then a full-width rule" shape `folder_header` uses for the unassigned lane,
/// toned down to a plain label since it's never itself a selectable row.
pub(super) fn remote_divider(count: usize, width: usize) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("remote-only", theme::faint()),
            Span::styled(format!("  {count}"), theme::faint()),
        ]),
        Line::styled("─".repeat(width), theme::faint()),
    ]
}

/// Lines per drawer row: name, metadata, and the blank that separates it from the next.
/// Named because the scroll arithmetic above depends on it matching `render_unapplied`.
const ROWS_PER_BRANCH: usize = 3;

pub(super) fn render_unapplied(
    b: &crate::board::UnappliedBranch,
    width: usize,
    selected: bool,
    hovered: bool,
) -> Vec<Line<'static>> {
    let pad = |mut line: Line<'static>| -> Line<'static> {
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        if used < width {
            line.spans.push(Span::raw(" ".repeat(width - used)));
        }
        if selected {
            line = line.style(theme::selected_bg());
        }
        line
    };

    // The dot carries the merge check, in the same colour language the lane headers use
    // for push status: green is safe to apply, red will conflict, faint means unchecked.
    let (dot_tone, verdict) = match b.merges_cleanly {
        Some(true) => (crate::board::Tone::Good, "clean"),
        Some(false) => (crate::board::Tone::Bad, "conflicts"),
        None => (crate::board::Tone::Neutral, ""),
    };

    let mut out = Vec::with_capacity(ROWS_PER_BRANCH);
    out.push(pad(Line::from(vec![
        Span::styled("● ", theme::tone(dot_tone)),
        Span::styled(
            truncate(&b.name, width.saturating_sub(2)),
            theme::title(selected || hovered),
        ),
    ])));

    // The verdict keeps its tone; everything after it is one faint string truncated as a
    // whole, so a long author name is dropped cleanly at a boundary instead of clipping
    // mid-word at the drawer's edge.
    let mut meta = vec![Span::raw("  ")];
    let mut used = 2;
    if !verdict.is_empty() {
        meta.push(Span::styled(verdict, theme::tone(dot_tone)));
        used += verdict.chars().count();
    }
    // A nudge, not a verdict of its own — `conflicts` already said the mechanical fact;
    // this says the row is old enough on top of that to be worth a second look, rather
    // than just a branch that got rebased five minutes ago.
    if b.stale {
        let label = " · stale";
        meta.push(Span::styled(label, theme::tone(crate::board::Tone::Warn)));
        used += label.chars().count();
    }
    let mut tail = String::new();
    if let Some(ahead) = b.commits_ahead {
        tail.push_str(&format!("  ↑{ahead}"));
    }
    if !b.has_local {
        tail.push_str("  remote");
    }
    if let Some(age) = &b.age {
        tail.push_str(&format!("  {age}"));
    }
    if let Some(author) = &b.author {
        tail.push_str(&format!("  {author}"));
    }
    if !tail.is_empty() {
        meta.push(Span::styled(
            truncate(&tail, width.saturating_sub(used)),
            theme::faint(),
        ));
    }
    out.push(pad(Line::from(meta)));

    // Whitespace separates rows, as it does cards. Not padded: a highlighted blank line
    // would stretch the selection a row further than the thing it marks.
    out.push(Line::raw(""));
    out
}

/// What unapplying is about to take out of the working directory.
///
/// Shown before it happens for the reason `d` is: the lane's changes come off disk. Unlike
/// `d` nothing is discarded — hence the reassurance in the body, which is the whole point
/// of confirming rather than refusing.
pub(super) fn draw_unapply_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some((name, detail)) = app.pending_unapply() else {
        return;
    };
    let w = 56.min(area.width.saturating_sub(4));
    let inner = w.saturating_sub(4) as usize;
    let mut body = vec![
        Line::styled("  unapply lane", theme::muted()),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(name, theme::title(true)),
        ]),
    ];
    for l in wrap(&detail, inner) {
        body.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(l, theme::muted()),
        ]));
    }
    body.push(Line::raw(""));
    body.push(Line::styled(
        "  its changes leave the working directory.",
        theme::faint(),
    ));
    body.push(Line::styled(
        "  nothing is lost — the branch keeps its commits.",
        theme::faint(),
    ));
    let hint = "  ⏎ / y  unapply      esc / n  cancel";
    body.push(Line::raw(""));
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
        Paragraph::new(body).block(
            Block::bordered()
                .border_style(theme::tone(crate::board::Tone::Warn))
                .style(theme::selected_bg()),
        ),
        popup,
    );
}
