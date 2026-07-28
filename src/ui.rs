//! Board rendering.
//!
//! Columns are laid out side by side and scroll horizontally; each column scrolls
//! vertically on its own so a long lane never pushes its neighbours around.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{columns_that_fit, App, Mode, Notice, COL_GAP};
use crate::board::{Card, ColumnKind, Tone};
use crate::cmux::PaneStatus;
use crate::theme;

/// Label for a lane's cmux pane status, rendered alongside its ordinary badges. Kept here
/// rather than as a method on `PaneStatus` so `cmux` doesn't need to depend on `board`'s
/// `Tone` just to describe how it's drawn — the same reason `LaneState`'s CI-badge sibling
/// bakes its own label/tone in `board.rs` instead of leaning on `ui.rs`, just mirrored the
/// other way since `PaneStatus` is the newer, ui-only-facing type.
fn pane_status_label(status: PaneStatus) -> &'static str {
    match status {
        PaneStatus::Busy => "● busy",
        PaneStatus::Idle => "○ idle",
        PaneStatus::Dead => "✕ pane closed",
        PaneStatus::Unknown => "… pane",
    }
}

fn pane_status_tone(status: PaneStatus) -> Tone {
    match status {
        PaneStatus::Busy => Tone::Accent,
        PaneStatus::Idle => Tone::Neutral,
        PaneStatus::Dead => Tone::Bad,
        PaneStatus::Unknown => Tone::Neutral,
    }
}

pub fn draw(f: &mut Frame, app: &App) {
    let mut constraints = vec![Constraint::Length(2)]; // header
    if app.tutorial.is_some() {
        constraints.push(Constraint::Length(4)); // tutorial banner
    }
    constraints.push(Constraint::Min(1)); // board
    constraints.push(Constraint::Length(1)); // footer

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    draw_header(f, app, chunks[0]);
    let mut next = 1;
    if let Some(t) = &app.tutorial {
        draw_tutorial_banner(f, t, chunks[next]);
        next += 1;
    }
    let board_area = chunks[next];
    let footer_area = chunks[next + 1];

    if app.mode == Mode::Diff && !app.diff_full {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(board_area);
        draw_board(f, app, split[0]);
        draw_diff(
            f,
            app,
            Rect {
                x: split[1].x + 1,
                width: split[1].width.saturating_sub(1),
                ..split[1]
            },
        );
    } else if app.mode == Mode::Diff {
        draw_diff(f, app, board_area);
    } else {
        draw_board(f, app, board_area);
    }
    draw_footer(f, app, footer_area);

    match app.mode {
        Mode::Help => draw_help(f, f.area()),
        Mode::PushConfirm => draw_push_confirm(f, app, f.area()),
        Mode::LandConfirm => draw_land_confirm(f, app, f.area()),
        Mode::Landing => draw_landing(f, app, f.area()),
        Mode::DeleteConfirm => draw_delete_confirm(f, app, f.area()),
        Mode::RebaseConfirm => draw_rebase_confirm(f, app, f.area()),
        // The board keeps its half unless the diff is expanded, so reading a diff does not
        // cost you your place — the same split gitui uses, and for the same reason.
        Mode::Diff => {}
        _ => {}
    }
}

/// The scripted lesson's current instruction, always visible so it survives whatever mode
/// or popup is on screen — a step like "open a diff" would otherwise hide the very prompt
/// that sent you there.
fn draw_tutorial_banner(f: &mut Frame, t: &crate::tutorial::Tutorial, area: Rect) {
    let w = area.width.saturating_sub(2) as usize;
    let mut lines = vec![Line::styled(
        format!("  tutorial — step {}", t.step_label()),
        theme::muted(),
    )];
    for wrapped in wrap(t.prompt(), w.max(1)) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(wrapped, theme::title(true)),
        ]));
    }
    lines.push(Line::styled("─".repeat(area.width as usize), theme::faint()));
    f.render_widget(Paragraph::new(lines), area);
}

/// The diff pane, full screen.
///
/// Hunks are listed one after another with the cursor on one of them, because a hunk is
/// the unit `but` will stage — pressing `m` here picks up exactly the hunk under the
/// cursor, which is how one file ends up split across two lanes.
fn draw_diff(f: &mut Frame, app: &App, area: Rect) {
    use crate::diff::LineKind;
    let Some(view) = &app.diff else { return };

    f.render_widget(Clear, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);

    let (added, removed) = view.totals();
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("  "),
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

/// What rebasing onto the updated target would do, per lane.
///
/// The per-branch outcome is the point: a lane that comes out `conflicted` is worth
/// knowing about before anything moves, and an `integrated` one can be deleted afterwards.
fn draw_rebase_confirm(f: &mut Frame, app: &App, area: Rect) {
    use crate::model::PullStatus;
    let Some(p) = &app.pull_preview else {
        return;
    };

    let mut body = vec![Line::styled("  rebase onto target", theme::muted()), Line::raw("")];
    body.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(
                "{} commit{} from ",
                p.upstream_commits.count,
                if p.upstream_commits.count == 1 { "" } else { "s" }
            ),
            theme::title(true),
        ),
        Span::styled(p.base_branch.name.clone(), theme::tone(crate::board::Tone::Accent)),
    ]));
    for c in p.upstream_commits.commits.iter().take(4) {
        body.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                truncate(c.description.lines().next().unwrap_or(""), 40),
                theme::muted(),
            ),
            Span::styled(
                c.author_name.clone().map(|a| format!("  {a}")).unwrap_or_default(),
                theme::faint(),
            ),
        ]));
    }
    if p.upstream_commits.commits.len() > 4 {
        body.push(Line::styled(
            format!("    … and {} more", p.upstream_commits.commits.len() - 4),
            theme::faint(),
        ));
    }

    let mut any_conflict = false;
    if !p.branch_statuses.is_empty() {
        body.push(Line::raw(""));
        body.push(Line::styled("  your lanes", theme::muted()));
        for b in &p.branch_statuses {
            let (word, tone) = match b.status {
                PullStatus::Updatable => ("rebases cleanly", crate::board::Tone::Good),
                PullStatus::Integrated => ("already integrated", crate::board::Tone::Neutral),
                PullStatus::Conflicted => {
                    any_conflict = true;
                    ("conflicts", crate::board::Tone::Bad)
                }
                PullStatus::Unknown => ("unknown", crate::board::Tone::Neutral),
            };
            body.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{:<22}", truncate(&b.name, 22)), theme::title(false)),
                Span::styled(word, theme::tone(tone)),
            ]));
        }
    }
    if p.has_worktree_conflicts {
        any_conflict = true;
        body.push(Line::raw(""));
        body.push(Line::styled(
            "  the worktree has conflicts already",
            theme::tone(crate::board::Tone::Bad),
        ));
    }

    body.push(Line::raw(""));
    body.push(Line::styled(
        if any_conflict {
            "  ⏎ / y  rebase anyway      esc / n  cancel"
        } else {
            "  ⏎ / y  rebase      esc / n  cancel"
        },
        theme::faint(),
    ));

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
                .border_style(if any_conflict {
                    theme::tone(crate::board::Tone::Bad)
                } else {
                    theme::faint()
                })
                .style(theme::selected_bg()),
        ),
        popup,
    );
}

/// Deleting cannot lose commits — `but` refuses when it would orphan them — but it can
/// dissolve a branch into the one above it, so the consequence is spelled out.
fn draw_delete_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some((name, detail)) = app.pending_delete() else {
        return;
    };
    let body = vec![
        Line::styled("  delete lane", theme::muted()),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(name, theme::title(true)),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(detail, theme::muted()),
        ]),
        Line::raw(""),
        Line::styled("  ⏎ / y  delete      esc / n  cancel", theme::faint()),
    ];

    let w = 56.min(area.width.saturating_sub(4));
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
                .border_style(theme::tone(crate::board::Tone::Warn))
                .style(theme::selected_bg()),
        ),
        popup,
    );
}

/// What a push is about to do. Shown before it happens because `but push` force-pushes by
/// default, so the destination and the force flag need to be visible, not implied.
fn draw_push_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(preview) = &app.push_preview else {
        return;
    };

    let mut body = vec![Line::styled("  push", theme::muted()), Line::raw("")];
    let mut any_force = false;

    for b in &preview.branches {
        let dest = format!("{}/{}", b.remote, b.branch_name);
        body.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("{} commit{}", b.unpushed_commits, if b.unpushed_commits == 1 { "" } else { "s" }),
                theme::title(true),
            ),
            Span::styled("  →  ", theme::faint()),
            Span::styled(dest, theme::tone(crate::board::Tone::Accent)),
        ]));
        if b.requires_force {
            any_force = true;
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("force — this rewrites remote history", theme::tone(crate::board::Tone::Bad)),
            ]));
        }
        if b.remote_ref.is_none() {
            body.push(Line::styled("  new branch on the remote", theme::muted()));
        }
        for c in b.commits.iter().take(6) {
            body.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{}  ", c.sha_short), theme::faint()),
                Span::styled(
                    truncate(c.message.lines().next().unwrap_or(""), 46),
                    theme::muted(),
                ),
            ]));
        }
        if b.commits.len() > 6 {
            body.push(Line::styled(
                format!("    … and {} more", b.commits.len() - 6),
                theme::faint(),
            ));
        }
        body.push(Line::raw(""));
    }

    body.push(Line::styled(
        if any_force {
            "  ⏎ / y  push anyway      esc / n  cancel"
        } else {
            "  ⏎ / y  push      esc / n  cancel"
        },
        theme::faint(),
    ));

    let w = 64.min(area.width.saturating_sub(4));
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
                .border_style(if any_force {
                    theme::tone(crate::board::Tone::Bad)
                } else {
                    theme::faint()
                })
                .style(theme::selected_bg()),
        ),
        popup,
    );
}

/// What landing the selected lane onto the target would do. `but land` has no
/// `--dry-run`, so this is built from `branch show --check` instead — the commits that
/// would land, and whether they land cleanly.
fn draw_land_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(check) = &app.land_check else {
        return;
    };

    let mut body = vec![
        Line::styled("  land onto target", theme::muted()),
        Line::raw(""),
    ];
    // `but land` refuses a non-base branch outright when the lane is a stack — this lands
    // every branch in it, base first, as one action (see `App::confirm_land`), so anyone
    // about to press `M` on a stack should see that's what's about to happen, not discover
    // it after the fact from a single "landed" notification that undersells it.
    if let Some(col) = app.board.columns.get(app.col) {
        if col.sections.len() > 1 {
            body.push(Line::styled(
                format!("  lands all {} branches, base first:", col.sections.len()),
                theme::tone(crate::board::Tone::Accent),
            ));
            for section in col.sections.iter().rev() {
                body.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(section.name.clone(), theme::muted()),
                ]));
            }
            body.push(Line::raw(""));
        }
    }
    body.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!(
                "{} commit{}",
                check.commits_ahead,
                if check.commits_ahead == 1 { "" } else { "s" }
            ),
            theme::title(true),
        ),
    ]));
    for c in check.commits.iter().take(6) {
        body.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("{}  ", c.short_sha), theme::faint()),
            Span::styled(truncate(c.subject(), 46), theme::muted()),
        ]));
    }
    if check.commits.len() > 6 {
        body.push(Line::styled(
            format!("    … and {} more", check.commits.len() - 6),
            theme::faint(),
        ));
    }

    body.push(Line::raw(""));
    let conflicted = !check.merge_check.merges_cleanly;
    if conflicted {
        body.push(Line::styled(
            "  conflicts on land",
            theme::tone(crate::board::Tone::Bad),
        ));
        for path in check.merge_check.conflicting_files.iter().take(6) {
            body.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(truncate(path, 46), theme::muted()),
            ]));
        }
    } else {
        body.push(Line::styled(
            "  lands cleanly",
            theme::tone(crate::board::Tone::Good),
        ));
    }

    body.push(Line::raw(""));
    body.push(Line::styled(
        "  if the target is a real remote, this pushes to it directly — z undoes the",
        theme::faint(),
    ));
    body.push(Line::styled(
        "  local workspace afterwards, but does not un-push it",
        theme::faint(),
    ));

    body.push(Line::raw(""));
    body.push(Line::styled(
        if conflicted {
            "  ⏎ / y  land anyway      esc / n  cancel"
        } else {
            "  ⏎ / y  land      esc / n  cancel"
        },
        theme::faint(),
    ));

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
                .border_style(if conflicted {
                    theme::tone(crate::board::Tone::Bad)
                } else {
                    theme::faint()
                })
                .style(theme::selected_bg()),
        ),
        popup,
    );
}

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A small popup shown while `but land` runs on a background thread. No key hints —
/// there is nothing to press, `poll_land` is what closes this.
fn draw_landing(f: &mut Frame, app: &App, area: Rect) {
    let Some(pending) = &app.landing else {
        return;
    };
    let frame = SPINNER[pending.spinner % SPINNER.len()];
    let label = if pending.branch_count > 1 {
        format!("landing {} branches onto the target…", pending.branch_count)
    } else {
        format!("landing {} onto the target…", pending.title)
    };

    let body = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(frame.to_string(), theme::tone(crate::board::Tone::Accent)),
            Span::raw("  "),
            Span::styled(label, theme::title(true)),
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

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
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
    // Same reasoning as the lane counter, one level down: a long stack already keeps its
    // header pinned so the lane itself is never in doubt, but the card position within it
    // scrolls out of view just as easily, and nothing else says where you are in it.
    let card_count = b.columns.get(app.col).map_or(0, |c| c.cards.len());
    if card_count > 1 {
        spans.push(Span::styled("  ·  ", theme::faint()));
        spans.push(Span::styled(
            format!("card {}/{card_count}", app.card + 1),
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

/// Chooses a column width and the first visible column so the cursor stays on screen.
fn visible_columns(app: &App, area: Rect) -> (u16, usize, usize) {
    let (width, fit) = columns_that_fit(app.column_count(), area.width);
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
        if last < app.board.columns.len() {
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

fn draw_column(f: &mut Frame, app: &App, idx: usize, area: Rect) {
    let col = &app.board.columns[idx];
    let is_current = idx == app.col;
    let inner_w = area.width as usize;

    // Header: dot, name, count. While moving, the header is itself a drop position, so it
    // highlights when the drop cursor sits on it. Either way, the current lane's header
    // carries a background tint at all times — bold title text and a brighter rule alone
    // read as too subtle to tell which of several lanes is current at a glance (GitHub
    // issue #3); this is the same "focused thing" language `selected_bg` already gives
    // cards, applied one level up.
    let header_is_target =
        app.mode == Mode::Moving && is_current && app.target_card.is_none();
    let mut header = Line::from(vec![
        Span::styled("● ", theme::status_dot(col.state)),
        Span::styled(
            truncate(&col.title, inner_w.saturating_sub(8)),
            theme::title(is_current),
        ),
        Span::styled(format!("  {}", header_count(col)), theme::faint()),
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
    f.render_widget(
        Paragraph::new(fixed_lines),
        Rect { height: fixed_h, ..area },
    );
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
        let card_lines = render_card(card, inner_w, selected, picked, checked);
        if selected {
            sel_start = start;
            sel_len = card_lines.len();
        }
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
}

/// The count shown next to a lane's name.
///
/// For a plain lane that is just its cards. For a stack it must be the *tip branch's*
/// share, not the lane total, because the branches below print their own counts right
/// underneath — a lane reading `board-ops +1  2` above a `live-refresh  1` says board-ops
/// has two commits, which it does not.
fn header_count(col: &crate::board::Column) -> usize {
    if col.sections.len() < 2 {
        return col.cards.len();
    }
    let below: usize = col.sections.iter().skip(1).map(|s| s.commits).sum();
    // Whatever is left is the tip's commits plus any changes staged to the stack.
    col.cards.len().saturating_sub(below)
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
fn folder_header(name: &str, count: usize, width: usize) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("▸ ", theme::faint()),
            Span::styled(truncate(name, width.saturating_sub(6)), theme::title(false)),
            Span::styled(format!("  {count}"), theme::faint()),
        ]),
        Line::styled("─".repeat(width), theme::faint()),
    ]
}

fn section_header(section: &crate::board::Section, width: usize) -> Vec<Line<'static>> {
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

fn render_card(
    card: &Card,
    width: usize,
    selected: bool,
    picked: bool,
    checked: bool,
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
                    Span::styled(
                        "   ←/→ lane · ↑/↓ drop on card · ⏎ confirm · esc cancel",
                        theme::faint(),
                    ),
                ])),
                area,
            );
        }
        Mode::Commit => {
            // The typed message with a block cursor between its two halves — not always
            // after the whole string — so the footer doubles as a real, editable input.
            let (before, after) = app.commit_input.split_at_cursor();
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  message  ", theme::faint()),
                    Span::styled(before.to_string(), theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after.to_string(), theme::title(true)),
                    Span::styled("   ⏎ commit · esc cancel", theme::faint()),
                ])),
                area,
            );
        }
        Mode::Branch => {
            // The pending action is named here for the same reason move mode names its
            // verb: stacking and adding a parallel lane are different things.
            let (before, after) = app.branch_input.split_at_cursor();
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  branch  ", theme::faint()),
                    Span::styled(before.to_string(), theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after.to_string(), theme::title(true)),
                    Span::styled("   ", theme::faint()),
                    Span::styled(
                        app.pending_branch_action(),
                        theme::tone(crate::board::Tone::Accent),
                    ),
                    Span::styled(
                        if app.cmux_available() {
                            "   ⏎ create · tab switch · shift-tab cmux · esc cancel"
                        } else {
                            "   ⏎ create · tab switch · esc cancel"
                        },
                        theme::faint(),
                    ),
                ])),
                area,
            );
        }
        Mode::Task => {
            let (before, after) = app.task_input.split_at_cursor();
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  task  ", theme::faint()),
                    Span::styled(before.to_string(), theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after.to_string(), theme::title(true)),
                    Span::styled("   ⏎ send · esc cancel", theme::faint()),
                ])),
                area,
            );
        }
        Mode::Restacking => {
            let action = app.pending_restack().unwrap_or_default();
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(action, theme::tone(crate::board::Tone::Accent)),
                    Span::styled("  rewrites history", theme::tone(crate::board::Tone::Bad)),
                    Span::styled("   ←/→ lane · ⏎ confirm · esc cancel", theme::faint()),
                ])),
                area,
            );
        }
        Mode::PushConfirm | Mode::LandConfirm | Mode::DeleteConfirm | Mode::RebaseConfirm
        | Mode::Landing => "",
        Mode::Diff => {
            let stageable = app
                .diff
                .as_ref()
                .and_then(|v| v.selected())
                .is_some_and(|e| e.rub_id.is_some());
            return f.render_widget(
                Paragraph::new(Line::styled(
                    if stageable {
                        "  ↑/↓ hunk · m stage this hunk · tab widen · ←/→ back to board"
                    } else {
                        "  ↑/↓ hunk · tab widen · ←/→ back to board"
                    },
                    theme::faint(),
                )),
                area,
            );
        }
        Mode::Help => "  esc close",
        Mode::Normal => {
            "  ←/→ lane · ↑/↓ card · m move · u unstage · c commit · b branch · t task · s stack · ⏎ diff · d delete · r rebase · p push · M land · z/Z undo/redo · ?"
        }
    };
    f.render_widget(Paragraph::new(Line::styled(keys, theme::faint())), area);
}

fn draw_help(f: &mut Frame, area: Rect) {
    let body = vec![
        Line::styled("  keys", theme::muted()),
        Line::raw(""),
        help_row("←/→  h/l", "move between lanes"),
        help_row("↑/↓  j/k", "move between cards"),
        help_row("shift ←/→", "page by however many lanes fit on screen"),
        help_row("shift ↑/↓", "skip to the next branch (stacked) or folder (grouped)"),
        help_row("g / G", "first / last card"),
        help_row("space", "select this card, for a bulk move — again to deselect"),
        help_row("m", "pick up the selection, or just this card if nothing's selected"),
        help_row("  then ←/→", "choose a lane"),
        help_row("  then ↑/↓", "drop on the lane, or onto a card"),
        help_row("  then ⏎", "confirm · esc cancels, keeping the selection"),
        help_row("esc", "with a selection and nothing else to cancel: clears it"),
        help_row("u", "send this card back to the backlog — uncommit or unstage"),
        help_row("d", "delete this lane — asks first"),
        help_row("r", "rebase onto the updated target — shows what will happen"),
        help_row("tab", "on unassigned: group its cards by folder, or back to flat"),
        help_row("⏎", "open the diff beside the board — ← goes back"),
        help_row("c", "commit the staged files in this lane"),
        help_row("b", "new branch — stacks on this lane, tab for parallel"),
        help_row("t", "send a task to this lane's cmux pane — spawns one first if not open"),
        help_row("s", "stack this whole lane onto another — rewrites history"),
        help_row("p", "push this lane — shows what it will do first"),
        help_row("M", "land this lane onto the target — no PR, shows what will happen first"),
        help_row("z / Z", "undo / redo the last operation — fires immediately, no confirm"),
        help_row("? / esc", "toggle this help"),
        help_row("q", "quit"),
        Line::raw(""),
        Line::styled("  what a drop does", theme::muted()),
        Line::raw(""),
        help_row("commit → lane", "moves the commit to that branch"),
        help_row("commit → commit", "squashes them together"),
        help_row("commit → unassigned", "uncommits it into the worktree"),
        help_row("file → lane", "stages it to that branch"),
        help_row("file → commit", "amends it into that commit"),
        help_row("file → unassigned", "unstages it"),
        Line::raw(""),
        Line::styled("  reading the board", theme::muted()),
        Line::raw(""),
        help_row("current lane", "tinted header — the one ←/→ and m/c/p/M/d act on"),
        help_row("● filled dot", "a lane's tip branch — c/p/M/z always act here"),
        help_row("○ hollow dot", "a branch stacked below the tip, along for the ride"),
        help_row("▸ folder", "a directory divider in unassigned, when grouped by folder"),
        help_row("● busy / ○ idle / ✕ pane closed", "a lane's cmux harness pane, if one is open"),
        Line::raw(""),
        Line::styled(
            "  every drop is one `but rub SOURCE TARGET`.",
            theme::faint(),
        ),
        Line::styled(
            "  the board follows the repo; no refresh key needed.",
            theme::faint(),
        ),
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
            stats: Some((3, 1)),
            rub_id: "d5aa11bb".into(),
            title: "Add auth middleware".into(),
            subtitle: Some("a.txt".into()),
            badges: vec![],
            author: Some("Dani".into()),
            kind: crate::board::CardKind::Commit,
            group: None,
        };
        let lines = render_card(&card, 30, true, false, false);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            // The trailing spacer line is intentionally empty.
            assert!(w == 30 || w == 0, "line width {w} should fill the card");
        }
    }
}
