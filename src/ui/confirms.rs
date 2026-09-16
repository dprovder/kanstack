use super::*;


/// The workspace is blocked and the board behind this cannot be trusted.
///
/// This one is not a confirm dialog with a cancel: there is nothing to go back to, so the
/// options are the two recoveries and quitting. Both recoveries are spelled out as the
/// commands they actually run, because a modal that rewrites history should be readable as
/// exactly what a person would have typed themselves.
pub(super) fn draw_blocked(f: &mut Frame, app: &App, area: Rect) {
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
pub(super) fn draw_rebase_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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
pub(super) fn draw_delete_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let Some((name, detail)) = app.pending_delete() else {
        return;
    };
    let hint = "  ⏎ / y  delete      esc / n  cancel";
    let title = if app.deleting_unapplied() {
        "  delete branch"
    } else {
        "  delete lane"
    };
    let body = vec![
        Line::styled(title, theme::muted()),
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
pub(super) fn draw_push_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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

/// What landing the selected lane onto the target would do. `but land` has no
/// `--dry-run`, so this is built from `branch show --check` instead — the commits that
/// would land, and whether they land cleanly.
pub(super) fn draw_land_confirm(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
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
        if let Some(Line { spans, .. }) = commit_stat_line(c) {
            let mut line = vec![Span::raw("      ")];
            line.extend(spans);
            body.push(Line::from(line));
        }
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
        for file in check.merge_check.conflicting_files.iter().take(6) {
            body.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(truncate(&file.path, 46), theme::muted()),
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
pub(super) fn draw_landing(f: &mut Frame, app: &App, area: Rect) {
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
