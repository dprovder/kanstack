use super::*;


/// A label/value row with a key hint right-aligned to `content_width`, e.g.
/// `"  action   new parallel lane                                   tab"`.
pub(super) fn hint_row(prefix: &str, value: &str, value_style: Style, hint: &str, content_width: usize) -> Line<'static> {
    let used = prefix.chars().count() + value.chars().count() + hint.chars().count();
    let pad = content_width.saturating_sub(used).max(1);
    Line::from(vec![
        Span::styled(prefix.to_string(), theme::faint()),
        Span::styled(value.to_string(), value_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(hint.to_string(), theme::faint()),
    ])
}

/// Branch creation, as a modal — the default presentation (`KANSTACK_BRANCH_UI=footer`
/// is the one-line alternative; see `BranchUi`). Shows the name, the pending action,
/// and — when one is coming — the initial harness message all at once instead of one
/// field at a time, each with room a footer line never has. Covers `Mode::Branch` and
/// `Mode::HarnessMessage` both, so the name stays visible (now fixed, no cursor) while
/// the message field takes over — nothing about the flow changes from the footer version,
/// only how much of it is on screen together.
pub(super) fn draw_branch_modal(f: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    use crate::app::BranchModalRow;

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

    // The row `Up`/`Down` currently act on gets a `▸` in place of the usual leading
    // spaces — same character count either way, so it doesn't disturb the column layout
    // every row lines up on. Meaningless once past the name step (`editing_message`),
    // which has just the one field.
    let row_marker = |row: BranchModalRow| -> &'static str {
        if !editing_message && app.branch_modal_row == row {
            "▸ "
        } else {
            "  "
        }
    };

    let name_prefix = format!("{}name     ", row_marker(BranchModalRow::Name));
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

    body.push(hint_row(
        &format!("{}action   ", row_marker(BranchModalRow::Action)),
        &app.pending_branch_target(),
        theme::tone(crate::board::Tone::Accent),
        "←/→ tab",
        content_width,
    ));
    // A stacked branch never opens its own split regardless of `open_harness` (see
    // `toggle_open_harness`), so the row that would toggle it is just noise there.
    let split_row = if app.branch_modal_split_row_visible() {
        let (glyph, style) = if app.open_harness {
            ("[x] open harness split", theme::tone(crate::board::Tone::Accent))
        } else {
            ("[ ] open harness split", theme::faint())
        };
        body.push(hint_row(
            &format!("{}{:<9}", row_marker(BranchModalRow::Split), app.splitter_label()),
            glyph,
            style,
            "←/→ shift-tab",
            content_width,
        ));
        Some(body.len() - 1)
    } else {
        None
    };

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
                "  message  (↓ to add an optional initial message for the harness)",
                theme::faint(),
            ));
        }
    }

    body.push(Line::raw(""));
    let hint = if editing_message {
        "  ⏎ create with this message      ↑ back      esc cancel branch"
    } else if will_prompt {
        "  ↑↓ row      ⏎ create now      esc cancel"
    } else {
        "  ↑↓ row      ⏎ create      esc cancel"
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
    // Mirrors `confirm_hitboxes`' own bounds check: skip the hitbox if a too-short popup
    // clipped this row out of view.
    if let Some(idx) = split_row {
        let row = popup.y + 1 + idx as u16;
        if row + 1 < popup.y + popup.height {
            hits.push(
                Rect { x: popup.x + 1, y: row, width: popup.width.saturating_sub(2), height: 1 },
                HitTarget::BranchToggleSplit,
            );
        }
    }
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(Block::bordered().border_style(theme::faint()).style(theme::selected_bg())),
        popup,
    );
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
pub(super) fn wrap_ranges(chars: &[char], width: usize) -> Vec<(usize, usize)> {
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
}
