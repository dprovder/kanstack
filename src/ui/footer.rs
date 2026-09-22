use super::*;


pub(super) fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
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
            // Enter always creates the branch right now, with no message — `Down` is what
            // opts into the optional initial-message step instead (see
            // `App::advance_to_harness_message`); nothing touches `but` until one or the
            // other fires.
            let hint = if app.will_prompt_for_harness_message() {
                format!(
                    "   ⏎ create · ↓ message · tab switch · shift-tab {} · esc cancel",
                    app.splitter_label()
                )
            } else if app.splitter_available() {
                format!("   ⏎ create · tab switch · shift-tab {} · esc cancel", app.splitter_label())
            } else {
                "   ⏎ create · tab switch · esc cancel".to_string()
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
            // Nothing has been created yet at this point — see
            // `App::advance_to_harness_message` — so `esc` here cancels the branch outright
            // rather than just skipping the message; `↑` goes back to the name field
            // instead, without cancelling.
            let hint = "   ⏎ create · ↑ back · esc cancel branch";
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
        | Mode::ResolveConflict | Mode::Landing | Mode::Blocked | Mode::SetupRequired
        | Mode::UnapplyConfirm | Mode::PrModal | Mode::PrRunning => "",
        Mode::Branches if app.branch_preview.is_some() => {
            "  ↑/↓ scroll · a apply into a new lane · esc/← back"
        }
        Mode::Branches if app.unapplied.truncated => {
            "  ↑/↓ branch · ⏎ preview · a apply · d delete · A load all · esc/← close"
        }
        Mode::Branches => "  ↑/↓ branch · ⏎ preview · a apply · d delete · esc/← close",
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
            "  ←/→ lane · ↑/↓ card · m move · u unstage · c commit · b branch · t task · s stack · ⏎ diff · a unapplied · U unapply · d delete · r rebase · f resolve conflicts · p push · L land · M pr · z/Z undo/redo · ?"
        }
    };
    f.render_widget(Paragraph::new(Line::styled(keys, theme::faint())), area);
}
