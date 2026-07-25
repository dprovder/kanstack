//! Board rendering.
//!
//! Columns are laid out side by side and scroll horizontally; each column scrolls
//! vertically on its own so a long lane never pushes its neighbours around.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, Mode, Notice};
use crate::board::Card;
use crate::theme;

const MIN_COL_WIDTH: u16 = 26;
const MAX_COL_WIDTH: u16 = 46;
const COL_GAP: u16 = 2;

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header
            Constraint::Min(1),    // board
            Constraint::Length(1), // footer
        ])
        .split(f.area());

    draw_header(f, app, chunks[0]);
    draw_board(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);

    match app.mode {
        Mode::Help => draw_help(f, f.area()),
        Mode::PushConfirm => draw_push_confirm(f, app, f.area()),
        Mode::DeleteConfirm => draw_delete_confirm(f, app, f.area()),
        Mode::RebaseConfirm => draw_rebase_confirm(f, app, f.area()),
        _ => {}
    }
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
                .style(Style::default().bg(theme::SELECTED_BG)),
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
                .style(Style::default().bg(theme::SELECTED_BG)),
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
                .style(Style::default().bg(theme::SELECTED_BG)),
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
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Chooses a column width and the first visible column so the cursor stays on screen.
fn visible_columns(app: &App, area: Rect) -> (u16, usize, usize) {
    let n = app.column_count().max(1) as u16;
    let usable = area.width.saturating_sub(2);

    // Prefer showing everything; fall back to a comfortable width and scroll. `n` is
    // floored at 1 above, so the division is always safe.
    let width = (usable / n)
        .saturating_sub(COL_GAP)
        .clamp(MIN_COL_WIDTH, MAX_COL_WIDTH);
    let per = width + COL_GAP;
    let fit = (usable / per).max(1) as usize;

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

    // Hint that there is more board off-screen.
    if last < app.board.columns.len() {
        let marker = Rect {
            x: area.x + area.width.saturating_sub(1),
            y: area.y,
            width: 1,
            height: area.height,
        };
        let more: Vec<Line> = (0..area.height)
            .map(|_| Line::styled("›", theme::faint()))
            .collect();
        f.render_widget(Paragraph::new(more), marker);
    }
}

fn draw_column(f: &mut Frame, app: &App, idx: usize, area: Rect) {
    let col = &app.board.columns[idx];
    let is_current = idx == app.col;
    let inner_w = area.width as usize;

    // Header: dot, name, count. While moving, the header is itself a drop position, so it
    // highlights when the drop cursor sits on it.
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
    if header_is_target {
        header = header.style(Style::default().bg(theme::SELECTED_BG));
    }

    let mut lines = vec![header];

    // A rule under the header, brighter on the active lane.
    let rule_style = if is_current {
        theme::muted()
    } else {
        theme::faint()
    };
    lines.push(Line::styled("─".repeat(inner_w), rule_style));

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
        lines.push(Line::from(spans));
    }

    // Cards, tracking which lines belong to the cursor so we can scroll to it.
    let mut sel_start = 0usize;
    let mut sel_len = 0usize;
    let mut last_group: Option<&str> = None;

    for (ci, card) in col.cards.iter().enumerate() {
        if let Some(group) = card.group.as_deref() {
            if last_group != Some(group) {
                // The tip branch is already described by the lane header; the ones below it
                // get the same treatment here so the lane reads as visibly stacked.
                let is_tip = col.sections.first().is_some_and(|s| s.name == group);
                if !is_tip {
                    if let Some(section) = col.sections.iter().find(|s| s.name == group) {
                        lines.push(Line::raw(""));
                        lines.extend(section_header(section, inner_w));
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
        let card_lines = render_card(card, inner_w, selected, picked);
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

    // Keep the selected card in view within this column.
    let body_h = area.height as usize;
    let scroll = if sel_len > 0 && sel_start + sel_len > body_h {
        (sel_start + sel_len - body_h) as u16
    } else {
        0
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default())
            .scroll((scroll, 0)),
        area,
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
fn section_header(section: &crate::board::Section, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![
        Line::from(vec![
            Span::styled("● ", theme::status_dot(Some(section.state))),
            Span::styled(
                truncate(&section.name, width.saturating_sub(8)),
                theme::title(false),
            ),
            Span::styled(format!("  {}", section.commits), theme::faint()),
        ]),
        Line::styled("─".repeat(width), theme::faint()),
    ];
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

fn render_card(card: &Card, width: usize, selected: bool, picked: bool) -> Vec<Line<'static>> {
    let bg = if picked {
        Some(theme::PICKED_BG)
    } else if selected {
        Some(theme::SELECTED_BG)
    } else {
        None
    };
    let pad = |mut line: Line<'static>| -> Line<'static> {
        let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        if used < width {
            line.spans.push(Span::raw(" ".repeat(width - used)));
        }
        if let Some(c) = bg {
            line = line.style(Style::default().bg(c));
        }
        line
    };

    let mut out = Vec::new();

    // Id badge, plus a caret marking the card in hand.
    let mut id_spans = vec![Span::styled(card.cli_id.clone(), theme::id())];
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
            // The typed message with a block cursor, so the footer doubles as the input.
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  message  ", theme::faint()),
                    Span::styled(app.commit_input.clone(), theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled("   ⏎ commit · esc cancel", theme::faint()),
                ])),
                area,
            );
        }
        Mode::Branch => {
            // The pending action is named here for the same reason move mode names its
            // verb: stacking and adding a parallel lane are different things.
            return f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("  branch  ", theme::faint()),
                    Span::styled(app.branch_input.clone(), theme::title(true)),
                    Span::styled("█", theme::tone(crate::board::Tone::Accent)),
                    Span::styled("   ", theme::faint()),
                    Span::styled(
                        app.pending_branch_action(),
                        theme::tone(crate::board::Tone::Accent),
                    ),
                    Span::styled("   ⏎ create · tab switch · esc cancel", theme::faint()),
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
        Mode::PushConfirm | Mode::DeleteConfirm | Mode::RebaseConfirm => "",
        Mode::Help => "  esc close",
        Mode::Normal => {
            "  ←/→ lane · ↑/↓ card · m move · u unstage · c commit · b branch · s stack · d delete · r rebase · p push · ? help"
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
        help_row("g / G", "first / last card"),
        help_row("m", "pick up a card"),
        help_row("  then ←/→", "choose a lane"),
        help_row("  then ↑/↓", "drop on the lane, or onto a card"),
        help_row("  then ⏎", "confirm · esc cancels"),
        help_row("u", "send this card back to the backlog — uncommit or unstage"),
        help_row("d", "delete this lane — asks first"),
        help_row("r", "rebase onto the updated target — shows what will happen"),
        help_row("c", "commit the staged files in this lane"),
        help_row("b", "new branch — stacks on this lane, tab for parallel"),
        help_row("s", "stack this whole lane onto another — rewrites history"),
        help_row("p", "push this lane — shows what it will do first"),
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
                .style(Style::default().bg(theme::SELECTED_BG)),
        ),
        popup,
    );
}

fn help_row(keys: &str, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{keys:<22}"),
            Style::default().fg(theme::ID).add_modifier(Modifier::BOLD),
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
            rub_id: "d5aa11bb".into(),
            title: "Add auth middleware".into(),
            subtitle: Some("a.txt".into()),
            badges: vec![],
            author: Some("Dani".into()),
            kind: crate::board::CardKind::Commit,
            group: None,
        };
        let lines = render_card(&card, 30, true, false);
        for l in &lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            // The trailing spacer line is intentionally empty.
            assert!(w == 30 || w == 0, "line width {w} should fill the card");
        }
    }
}
