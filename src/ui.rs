//! Board rendering.
//!
//! Columns are laid out side by side and scroll horizontally; each column scrolls
//! vertically on its own so a long lane never pushes its neighbours around.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{columns_that_fit, App, BranchUi, Mode, Notice, COL_GAP};
use crate::board::{Card, ColumnKind, Tone};
use crate::cmux::PaneStatus;
use crate::hit::{HitMap, HitTarget};
use crate::theme;

/// Label for a lane's cmux pane status, rendered alongside its ordinary badges. Kept here
/// rather than as a method on `PaneStatus` so `cmux` doesn't need to depend on `board`'s
/// `Tone` just to describe how it's drawn — the same reason `LaneState`'s CI-badge sibling
/// bakes its own label/tone in `board.rs` instead of leaning on `ui.rs`, just mirrored the
/// other way since `PaneStatus` is the newer, ui-only-facing type.
/// How wide the unapplied-branches drawer is, when there is room for it. Branch names run
/// long, so this is wider than the board's own minimum lane; it is halved against the board
/// area before use so a narrow terminal never gives the drawer more room than the lanes.
const DRAWER_WIDTH: u16 = 34;

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

pub fn draw(f: &mut Frame, app: &App) -> HitMap {
    let mut hits = HitMap::new();
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
        draw_board(f, app, split[0], &mut hits);
        draw_diff(
            f,
            app,
            Rect {
                x: split[1].x + 1,
                width: split[1].width.saturating_sub(1),
                ..split[1]
            },
            &mut hits,
        );
    } else if app.mode == Mode::Diff {
        draw_diff(f, app, board_area, &mut hits);
    } else if app.mode == Mode::Branches {
        // Beside the board, not over it, for the same reason the diff splits rather than
        // covers: choosing what to apply is a decision made *against* the lanes already
        // there, so hiding them to show the list would hide half the question.
        //
        // On the left, where the board's own reading order starts: an unapplied branch is
        // upstream of every lane, so it belongs before them rather than after. The lanes
        // shift right while it is open, which is also what makes the drawer's presence
        // obvious without needing a border to announce it.
        let w = DRAWER_WIDTH.min(board_area.width / 2);
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(w), Constraint::Min(0)])
            .split(board_area);
        draw_branches(f, app, split[0], &mut hits);
        draw_board(f, app, split[1], &mut hits);
    } else {
        draw_board(f, app, board_area, &mut hits);
    }
    draw_footer(f, app, footer_area);

    match app.mode {
        Mode::Help => draw_help(f, f.area(), &mut hits),
        Mode::PushConfirm => draw_push_confirm(f, app, f.area(), &mut hits),
        Mode::LandConfirm => draw_land_confirm(f, app, f.area(), &mut hits),
        Mode::Landing => draw_landing(f, app, f.area()),
        Mode::DeleteConfirm => draw_delete_confirm(f, app, f.area(), &mut hits),
        Mode::UnapplyConfirm => draw_unapply_confirm(f, app, f.area(), &mut hits),
        Mode::RebaseConfirm => draw_rebase_confirm(f, app, f.area(), &mut hits),
        Mode::Blocked => draw_blocked(f, app, f.area()),
        Mode::Branch | Mode::HarnessMessage if app.branch_ui == BranchUi::Modal => {
            draw_branch_modal(f, app, f.area())
        }
        // The board keeps its half unless the diff is expanded, so reading a diff does not
        // cost you your place — the same split gitui uses, and for the same reason.
        // `draw_diff` already added its own close control above.
        Mode::Diff => {}
        _ => {}
    }

    hits
}

/// Splits a confirm dialog's last body line — `"  ⏎ / y  <verb>      esc / n  cancel"` —
/// into two clickable halves at the word "esc", the one substring guaranteed not to appear
/// in the verb half of any of these hints.
fn confirm_hitboxes(hits: &mut HitMap, popup: Rect, body_len: usize, hint: &str) {
    if popup.height < 2 {
        return;
    }
    let row = popup.y + 1 + (body_len as u16 - 1);
    if row + 1 >= popup.y + popup.height {
        // The hint line itself was clipped out of a too-short popup; nothing to hit-test.
        return;
    }
    let content_x = popup.x + 1;
    let content_w = popup.width.saturating_sub(2);
    let split = hint.find("esc").unwrap_or(hint.len()) as u16;
    let confirm_w = split.min(content_w);
    hits.push(
        Rect { x: content_x, y: row, width: confirm_w, height: 1 },
        HitTarget::DialogConfirm,
    );
    if confirm_w < content_w {
        hits.push(
            Rect {
                x: content_x + confirm_w,
                y: row,
                width: content_w - confirm_w,
                height: 1,
            },
            HitTarget::DialogCancel,
        );
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
fn draw_diff(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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

/// The workspace is blocked and the board behind this cannot be trusted.
///
/// This one is not a confirm dialog with a cancel: there is nothing to go back to, so the
/// options are the two recoveries and quitting. Both recoveries are spelled out as the
/// commands they actually run, because a modal that rewrites history should be readable as
/// exactly what a person would have typed themselves.
fn draw_blocked(f: &mut Frame, app: &App, area: Rect) {
    let Some(b) = &app.blocked else {
        return;
    };

    let mut body = vec![
        Line::styled("  workspace blocked", theme::tone(Tone::Bad)),
        Line::raw(""),
    ];
    // `but`'s own words, wrapped rather than truncated. It names the recovery GitButler
    // recommends, and paraphrasing it would only put a layer between the user and the
    // thing they can search for.
    for line in b.message.lines().filter(|l| !l.trim().is_empty()) {
        for part in wrap(line.trim(), 68) {
            body.push(Line::styled(format!("  {part}"), theme::muted()));
        }
    }

    if !b.stray.is_empty() {
        body.push(Line::raw(""));
        body.push(Line::styled(
            format!(
                "  {} commit{} on top of the workspace commit",
                b.stray.len(),
                if b.stray.len() == 1 { "" } else { "s" }
            ),
            theme::title(true),
        ));
        for c in b.stray.iter().take(4) {
            body.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{:<9}", c.sha), theme::faint()),
                Span::styled(truncate(&c.subject, 52), theme::muted()),
            ]));
        }
        if b.stray.len() > 4 {
            body.push(Line::styled(
                format!("    … and {} more", b.stray.len() - 4),
                theme::faint(),
            ));
        }
    }

    body.push(Line::raw(""));
    body.push(Line::styled("  the board above is frozen", theme::faint()));
    body.push(Line::styled(
        "  every `but` command refuses until this is fixed, undo included",
        theme::faint(),
    ));
    body.push(Line::raw(""));

    match &b.workspace_sha {
        Some(sha) => {
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("r", theme::tone(Tone::Accent)),
                Span::styled("  put the commits back as uncommitted changes", theme::title(true)),
            ]));
            body.push(Line::styled(
                format!("     git reset --soft {}", &sha[..sha.len().min(12)]),
                theme::faint(),
            ));
            body.push(Line::styled(
                "     nothing is lost, and the board comes back",
                theme::faint(),
            ));
        }
        // Withheld rather than guessed: resetting onto the wrong commit is the one way
        // this modal could do real damage.
        None => body.push(Line::styled(
            "  the workspace commit could not be identified, so reset is not offered",
            theme::tone(Tone::Bad),
        )),
    }
    body.push(Line::raw(""));
    body.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("t", theme::tone(Tone::Accent)),
        Span::styled("  leave GitButler mode and quit", theme::title(true)),
    ]));
    body.push(Line::styled("     but teardown", theme::faint()));
    body.push(Line::styled(
        "     snapshots first, then checks out a real branch",
        theme::faint(),
    ));
    body.push(Line::raw(""));
    body.push(Line::styled("  q  quit and change nothing", theme::faint()));

    let w = 74.min(area.width.saturating_sub(4));
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
                .border_style(theme::tone(Tone::Bad))
                .style(theme::selected_bg()),
        ),
        popup,
    );
}

/// What rebasing onto the updated target would do, per lane.
///
/// The per-branch outcome is the point: a lane that comes out `conflicted` is worth
/// knowing about before anything moves, and an `integrated` one can be deleted afterwards.
fn draw_rebase_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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

    let hint = if any_conflict {
        "  ⏎ / y  rebase anyway      esc / n  cancel"
    } else {
        "  ⏎ / y  rebase      esc / n  cancel"
    };
    body.push(Line::raw(""));
    body.push(Line::styled(hint, theme::faint()));

    let w = 62.min(area.width.saturating_sub(4));
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
fn draw_delete_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some((name, detail)) = app.pending_delete() else {
        return;
    };
    let hint = "  ⏎ / y  delete      esc / n  cancel";
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
        Line::styled(hint, theme::faint()),
    ];

    let w = 56.min(area.width.saturating_sub(4));
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

/// What a push is about to do. Shown before it happens because `but push` force-pushes by
/// default, so the destination and the force flag need to be visible, not implied.
fn draw_push_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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

    let hint = if any_force {
        "  ⏎ / y  push anyway      esc / n  cancel"
    } else {
        "  ⏎ / y  push      esc / n  cancel"
    };
    body.push(Line::styled(hint, theme::faint()));

    let w = 64.min(area.width.saturating_sub(4));
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

/// Branch creation, as a modal — the `KANSTACK_BRANCH_UI=modal` alternative to the
/// default one-line footer prompt (see `BranchUi`). Shows the name, the pending action,
/// and — when one is coming — the initial harness message all at once instead of one
/// field at a time, each with room a footer line never has. Covers `Mode::Branch` and
/// `Mode::HarnessMessage` both, so the name stays visible (now fixed, no cursor) while
/// the message field takes over — nothing about the flow changes from the footer version,
/// only how much of it is on screen together.
fn draw_branch_modal(f: &mut Frame, app: &App, area: Rect) {
    let editing_message = app.mode == Mode::HarnessMessage;
    let will_prompt = editing_message || app.will_prompt_for_harness_message();
    let cursor = theme::tone(crate::board::Tone::Accent);

    // Fixed ahead of the body so the name/message fields can be scrolled to fit it — the
    // popup is sized to the body's line count, but its *width* never depends on their
    // content, so there's no chicken-and-egg problem computing this first.
    let w = 64.min(area.width.saturating_sub(4));
    // Inside the block's border on each side.
    let content_width = (w as usize).saturating_sub(2);

    let mut body = vec![Line::styled("  new branch", theme::muted()), Line::raw("")];

    let name_prefix = "  name     ";
    if editing_message {
        // Already handed off to `pending_branch` — fixed, no cursor of its own anymore,
        // so no scrolling needed either; `truncate` (right-elided) is the right shape for
        // a value that's just being displayed rather than actively edited.
        let name = truncate(app.branch_modal_name(), content_width.saturating_sub(name_prefix.chars().count()));
        body.push(Line::from(vec![
            Span::styled(name_prefix, theme::faint()),
            Span::styled(name, theme::title(true)),
        ]));
    } else {
        let (before, after) = app.branch_input.split_at_cursor();
        let budget = footer_input_budget(content_width as u16, name_prefix.chars().count(), 0);
        let (before, after) = scroll_input(before, after, budget);
        body.push(Line::from(vec![
            Span::styled(name_prefix, theme::faint()),
            Span::styled(before, theme::title(true)),
            Span::styled("█", cursor),
            Span::styled(after, theme::title(true)),
        ]));
    }

    body.push(Line::from(vec![
        Span::styled("  action   ", theme::faint()),
        Span::styled(app.pending_branch_action(), theme::tone(crate::board::Tone::Accent)),
    ]));

    if will_prompt {
        body.push(Line::raw(""));
        if editing_message {
            // Unlike the name field (a single-line git ref, scrolled horizontally) or the
            // footer version of this same prompt (a fixed one-line strip), the modal has
            // room to grow downward — so the message wraps across as many lines as it
            // needs instead of scrolling one line sideways with older text hidden behind
            // an ellipsis.
            let message_prefix = "  message  ";
            let prefix_width = message_prefix.chars().count();
            let wrap_width = content_width.saturating_sub(prefix_width);
            let text: Vec<char> = app.harness_message_input.as_str().chars().collect();
            let ranges = wrap_ranges(&text, wrap_width);
            let cursor_idx = app.harness_message_input.split_at_cursor().0.chars().count();
            let cursor_line = ranges
                .iter()
                .position(|&(s, e)| cursor_idx >= s && cursor_idx < e)
                .unwrap_or(ranges.len() - 1);
            for (li, &(s, e)) in ranges.iter().enumerate() {
                let label = if li == 0 {
                    Span::styled(message_prefix, theme::faint())
                } else {
                    Span::raw(" ".repeat(prefix_width))
                };
                let line: String = text[s..e].iter().collect();
                if li == cursor_line {
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
        } else {
            body.push(Line::styled(
                "  message  (next: an optional initial message for the harness)",
                theme::faint(),
            ));
        }
    }

    body.push(Line::raw(""));
    let hint = if editing_message {
        "  ⏎ open harness      esc cancel branch"
    } else if will_prompt {
        "  ⏎ next: message · tab switch · shift-tab cmux      esc cancel"
    } else if app.cmux_available() {
        "  ⏎ create · tab switch · shift-tab cmux      esc cancel"
    } else {
        "  ⏎ create · tab switch      esc cancel"
    };
    body.push(Line::styled(hint, theme::faint()));

    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(Block::bordered().border_style(theme::faint()).style(theme::selected_bg())),
        popup,
    );
}

/// What landing the selected lane onto the target would do. `but land` has no
/// `--dry-run`, so this is built from `branch show --check` instead — the commits that
/// would land, and whether they land cleanly.
fn draw_land_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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

    let hint = if conflicted {
        "  ⏎ / y  land anyway      esc / n  cancel"
    } else {
        "  ⏎ / y  land      esc / n  cancel"
    };
    body.push(Line::raw(""));
    body.push(Line::styled(hint, theme::faint()));

    let w = 62.min(area.width.saturating_sub(4));
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

fn draw_board(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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
        draw_column(f, app, idx, col_area, hits);
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

fn draw_column(f: &mut Frame, app: &App, idx: usize, area: Rect, hits: &mut HitMap) {
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
    let mut header = Line::from(vec![
        if is_drawer_door {
            Span::styled("‹ ", theme::tone(crate::board::Tone::Accent))
        } else {
            Span::styled("● ", theme::status_dot(col.state))
        },
        Span::styled(
            truncate(&col.title, inner_w.saturating_sub(8)),
            theme::title(is_current || hovered),
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

/// The unapplied-branches drawer.
///
/// A list, not a column of lanes, and deliberately so: `but` will not enumerate an
/// unapplied branch's commits without applying it first, so there are no cards to draw.
/// Rendering it as a lane would mean inventing the contents. What it shows instead is
/// everything `but branch list` knows from the outside — chiefly whether applying would
/// conflict, which is the one fact that decides whether to press ⏎.
fn draw_branches(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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
    // a diff there) being swallowed by an anywhere-closes rule.
    let back = Rect { x: area.x, y: area.y, width: 2, height: 1 };
    hits.push(back, HitTarget::Dismiss);

    let mut fixed = vec![
        Line::from(vec![
            Span::styled("‹ ", theme::tone(crate::board::Tone::Accent)),
            Span::styled("unapplied", theme::title(true)),
            Span::styled(format!("  {n}"), theme::faint()),
        ]),
        Line::styled("─".repeat(w), theme::muted()),
    ];
    // Say so when the list is partial rather than presenting 20 branches as if they were
    // all of them — `but branch list` truncates by default and reports that it did.
    if app.unapplied.truncated {
        fixed.push(Line::styled(
            truncate("only the 20 most recent", w),
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

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(n * ROWS_PER_BRANCH);
    for (i, b) in app.unapplied.branches.iter().enumerate() {
        let hovered = app.mode == Mode::Branches && app.hover == Some(HitTarget::BranchRow(i));
        lines.extend(render_unapplied(b, w, i == app.branch_sel, hovered));
    }

    // Scroll just far enough to keep the whole selected row on screen, the same rule the
    // board uses for lanes — the meta line matters as much as the name, so the unit kept
    // visible is the row, not its first line.
    let sel_top = (app.branch_sel * ROWS_PER_BRANCH) as u16;
    let sel_bottom = sel_top + ROWS_PER_BRANCH as u16;
    let offset = sel_bottom.saturating_sub(body.height);
    f.render_widget(Paragraph::new(lines).scroll((offset, 0)), body);

    for i in 0..n {
        let top = (i * ROWS_PER_BRANCH) as u16;
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

/// Lines per drawer row: name, metadata, and the blank that separates it from the next.
/// Named because the scroll arithmetic above depends on it matching `render_unapplied`.
const ROWS_PER_BRANCH: usize = 3;

fn render_unapplied(
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
fn draw_unapply_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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
        // The modal (`draw_branch_modal`) carries its own hints and is drawn over the
        // board separately — see `draw`'s dispatch — so the footer stays blank rather
        // than duplicating them under it.
        Mode::Branch | Mode::HarnessMessage if app.branch_ui == BranchUi::Modal => "",
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
            let prefix = "  message  ";
            let hint = "   ⏎ commit · esc cancel";
            let budget = footer_input_budget(area.width, prefix.chars().count(), hint.chars().count());
            let (before, after) = scroll_input(before, after, budget);
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, theme::faint()),
                    Span::styled(before, theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after, theme::title(true)),
                    Span::styled(hint, theme::faint()),
                ])),
                area,
            );
        }
        Mode::Branch => {
            // The pending action is named here for the same reason move mode names its
            // verb: stacking and adding a parallel lane are different things.
            let (before, after) = app.branch_input.split_at_cursor();
            let prefix = "  branch  ";
            let pending = app.pending_branch_action();
            // A parallel lane opening a harness doesn't create anything on this Enter —
            // it goes on to prompt for an initial message first, and `confirm_branch`
            // doesn't touch `but` until that prompt confirms.
            let verb = if app.will_prompt_for_harness_message() { "next" } else { "create" };
            let hint = if app.cmux_available() {
                format!("   ⏎ {verb} · tab switch · shift-tab cmux · esc cancel")
            } else {
                format!("   ⏎ {verb} · tab switch · esc cancel")
            };
            let suffix_len = 3 + pending.chars().count() + hint.chars().count();
            let budget = footer_input_budget(area.width, prefix.chars().count(), suffix_len);
            let (before, after) = scroll_input(before, after, budget);
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, theme::faint()),
                    Span::styled(before, theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after, theme::title(true)),
                    Span::styled("   ", theme::faint()),
                    Span::styled(pending, theme::tone(crate::board::Tone::Accent)),
                    Span::styled(hint, theme::faint()),
                ])),
                area,
            );
        }
        Mode::Task => {
            let (before, after) = app.task_input.split_at_cursor();
            let prefix = "  task  ";
            let hint = "   ⏎ send · esc cancel";
            let budget = footer_input_budget(area.width, prefix.chars().count(), hint.chars().count());
            let (before, after) = scroll_input(before, after, budget);
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, theme::faint()),
                    Span::styled(before, theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after, theme::title(true)),
                    Span::styled(hint, theme::faint()),
                ])),
                area,
            );
        }
        Mode::HarnessMessage => {
            let (before, after) = app.harness_message_input.split_at_cursor();
            let prefix = "  initial message  ";
            // Nothing has been created yet at this point — see `App::confirm_branch` — so
            // `esc` here cancels the branch outright rather than just skipping the
            // message.
            let hint = "   ⏎ open harness · esc cancel branch";
            let budget = footer_input_budget(area.width, prefix.chars().count(), hint.chars().count());
            let (before, after) = scroll_input(before, after, budget);
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(prefix, theme::faint()),
                    Span::styled(before, theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled(after, theme::title(true)),
                    Span::styled(hint, theme::faint()),
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
        | Mode::Landing | Mode::Blocked | Mode::UnapplyConfirm => "",
        Mode::Branches => "  ↑/↓ branch · ⏎ apply into a new lane · a/esc close",
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
            "  ←/→ lane · ↑/↓ card · m move · u unstage · c commit · b branch · t task · s stack · ⏎ diff · a unapplied · U unapply · d delete · r rebase · p push · L land · z/Z undo/redo · ?"
        }
    };
    f.render_widget(Paragraph::new(Line::styled(keys, theme::faint())), area);
}

fn draw_help(f: &mut Frame, area: Rect, hits: &mut HitMap) {
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
        help_row("a", "branches not in the workspace — ⏎ applies one as a new lane"),
        help_row("U", "unapply this lane — its whole stack leaves, `a` brings it back"),
        help_row("d", "delete this lane — asks first"),
        help_row("r", "rebase onto the updated target — shows what will happen"),
        help_row("tab", "on unassigned: group its cards by folder, or back to flat"),
        help_row("⏎", "open the diff beside the board — ← goes back"),
        help_row("c", "commit the staged files in this lane"),
        help_row("b", "new branch — stacks on this lane, tab for parallel"),
        help_row("  then ⏎", "a parallel lane with cmux enabled asks for an initial message"),
        help_row("t", "send a task to this lane's cmux pane — spawns one first if not open"),
        help_row("s", "stack this whole lane onto another — rewrites history"),
        help_row("p", "push this lane — shows what it will do first"),
        help_row("L", "land this lane onto the target — no PR, shows what will happen first"),
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
        help_row("drawer ● green/red", "whether applying that branch would merge cleanly"),
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

    hits.push(popup, HitTarget::Dismiss);
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

/// How many character cells a footer text input can use for its typed content: the
/// area's width minus whatever fixed text flanks it (a label before, hints/status after)
/// and one more for the cursor glyph itself.
fn footer_input_budget(area_width: u16, prefix_chars: usize, suffix_chars: usize) -> usize {
    (area_width as usize)
        .saturating_sub(prefix_chars)
        .saturating_sub(suffix_chars)
        .saturating_sub(1)
}

/// Slides the cursor-split halves of a footer text input so the cursor stays visible
/// within `budget` character cells.
///
/// The footer is a single fixed-height line (`Constraint::Length(1)`) that `Paragraph`
/// never wraps, so once a `Commit`/`Branch`/`Task`/`HarnessMessage` input grew past the
/// terminal's width, everything from that point on — including the cursor itself — used
/// to run off the right edge and simply not be drawn, with no indication anything was
/// being typed at all. This keeps a `budget`-wide window centred on the cursor instead,
/// eliding whichever side(s) don't fit with `…`, the same signal `truncate` uses for card
/// text — with one side handing its unused share to the other (typically `after`, which
/// is empty while typing forward, so `before` gets the whole window rather than half of
/// it going to waste).
fn scroll_input(before: &str, after: &str, budget: usize) -> (String, String) {
    if budget == 0 {
        return (String::new(), String::new());
    }
    let before_len = before.chars().count();
    let after_len = after.chars().count();
    if before_len + after_len <= budget {
        return (before.to_string(), after.to_string());
    }

    let after_cells = after_len.min(budget / 2);
    let before_cells = before_len.min(budget - after_cells);
    let after_cells = after_len.min(budget - before_cells);

    let before_elided = before_cells < before_len;
    let after_elided = after_cells < after_len;
    // The ellipsis itself takes one of that side's own cells, so the total still fits.
    let before_take = before_cells.saturating_sub(before_elided as usize);
    let after_take = after_cells.saturating_sub(after_elided as usize);

    let mut before_show = String::new();
    if before_elided {
        before_show.push('…');
    }
    before_show.extend(before.chars().skip(before_len - before_take));

    let mut after_show: String = after.chars().take(after_take).collect();
    if after_elided {
        after_show.push('…');
    }

    (before_show, after_show)
}

/// Word-wraps `chars` into line ranges — each an exclusive `[start, end)` back into
/// `chars` — for a box that can grow downward, like the branch-modal message field.
///
/// Unlike `wrap`, which rebuilds the string and collapses whitespace runs to a single
/// space, this preserves the exact original text and reports *where* each line falls in
/// it, which a caller needs to locate a cursor position afterwards (`wrap`'s rebuilt
/// string has no reliable mapping back to the source). Breaks at the last space that
/// still fits a line, keeping that space at the line's end; a run with no space to break
/// at (a single word longer than `width`) hard-splits at `width`, the same fallback
/// `wrap` uses for a long path or URL.
fn wrap_ranges(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    if width == 0 || chars.is_empty() {
        return vec![(0, chars.len())];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut end = (start + width).min(chars.len());
        if end < chars.len() {
            if let Some(back) = chars[start..end].iter().rposition(|&c| c == ' ') {
                let space_at = start + back;
                // A space right at `start` is a leading space on this line, not a break
                // point before it — falling through to the hard split at `width` avoids
                // producing a zero-length line.
                if space_at > start {
                    end = space_at + 1;
                }
            }
        }
        lines.push((start, end));
        start = end;
    }
    lines
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

    fn ranges_text(text: &str, ranges: &[(usize, usize)]) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        ranges.iter().map(|&(s, e)| chars[s..e].iter().collect()).collect()
    }

    #[test]
    fn wrap_ranges_breaks_at_the_last_space_that_fits() {
        let chars: Vec<char> = "the quick brown fox".chars().collect();
        let ranges = wrap_ranges(&chars, 10);
        assert_eq!(ranges_text("the quick brown fox", &ranges), ["the quick ", "brown fox"]);
    }

    /// A run with no space to break at (a single long token) hard-splits at `width`
    /// rather than overflowing it, the same fallback `wrap` uses for a long path.
    #[test]
    fn wrap_ranges_hard_splits_a_token_with_no_space() {
        let chars: Vec<char> = "aaaaaaaaaaaaaaaa".chars().collect(); // 16 chars, no spaces
        let ranges = wrap_ranges(&chars, 6);
        assert_eq!(ranges_text("aaaaaaaaaaaaaaaa", &ranges), ["aaaaaa", "aaaaaa", "aaaa"]);
    }

    #[test]
    fn wrap_ranges_leaves_short_text_on_one_line() {
        let chars: Vec<char> = "hi there".chars().collect();
        assert_eq!(wrap_ranges(&chars, 20), [(0, 8)]);
    }

    /// The ranges are what a caller uses to place the cursor on the right wrapped line —
    /// this pins that every character position in the original text is covered by
    /// exactly one range, with no gaps or overlaps a cursor could fall into.
    #[test]
    fn wrap_ranges_cover_every_char_position_with_no_gaps() {
        let text = "the quick brown fox jumps over the lazy dog and then some more";
        let chars: Vec<char> = text.chars().collect();
        let ranges = wrap_ranges(&chars, 12);
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, chars.len());
        for w in ranges.windows(2) {
            assert_eq!(w[0].1, w[1].0, "ranges must be contiguous: {ranges:?}");
        }
    }

    #[test]
    fn scroll_input_shows_everything_when_it_already_fits() {
        assert_eq!(scroll_input("hello ", "world", 20), ("hello ".into(), "world".into()));
    }

    /// Typing forward with nothing after the cursor should use the whole window on
    /// `before`, not split it evenly with an empty `after`.
    #[test]
    fn scroll_input_gives_an_empty_side_its_share_back() {
        let (before, after) = scroll_input("a very long line of typed text", "", 10);
        assert_eq!(before.chars().count() + after.chars().count(), 10);
        assert!(after.is_empty());
        assert!(before.starts_with('…'), "the clipped side must say so: {before:?}");
        assert!(before.ends_with("text"), "the cursor-adjacent tail must stay visible: {before:?}");
    }

    /// Content on both sides of the cursor gets clipped independently, each marked with
    /// its own ellipsis, and the cursor's own position (the boundary) is always shown.
    #[test]
    fn scroll_input_elides_both_sides_when_both_overflow() {
        let (before, after) = scroll_input("nine chars", "ten chars!", 8);
        assert_eq!(before.chars().count() + after.chars().count(), 8);
        assert!(before.starts_with('…'));
        assert!(before.ends_with("ars"), "got {before:?}");
        assert!(after.ends_with('…'));
        assert!(after.starts_with("ten"), "got {after:?}");
    }

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
        let lines = render_card(&card, 30, true, false, false, false);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            // The trailing spacer line is intentionally empty.
            assert!(w == 30 || w == 0, "line width {w} should fill the card");
        }
    }
}
