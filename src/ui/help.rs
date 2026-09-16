use super::*;


pub(super) fn draw_help(f: &mut Frame, area: Rect, hits: &mut HitMap) {
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
        help_row("a", "branches not in the workspace — a applies one as a new lane"),
        help_row("  then ⏎", "preview its commits and conflicts before deciding"),
        help_row("  then d", "delete the selected one — asks first, stays in the drawer"),
        help_row("  then A", "if truncated to the 20 most recent — loads all of them"),
        help_row("U", "unapply this lane — its whole stack leaves, `a` brings it back"),
        help_row("d", "delete this lane — asks first"),
        help_row("r", "rebase onto the updated target — shows what will happen"),
        help_row("tab", "on unassigned: group its cards by folder, or back to flat"),
        help_row("⏎", "open the diff beside the board — ← goes back"),
        help_row("c", "commit the staged files in this lane"),
        help_row("b", "new branch — stacks on this lane, tab for parallel"),
        help_row("  then ⏎", "create now · ↓ (parallel + split) add an initial message first"),
        help_row("t", "send a task to this lane's split pane — spawns one first if not open"),
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
        help_row("● busy / ○ idle / ✕ pane closed", "a lane's split harness pane (cmux or tmux), if one is open"),
        help_row("drawer ● green/red", "whether applying that branch would merge cleanly"),
        help_row("drawer remote-only", "divider — everything below has no local ref"),
        help_row("drawer stale", "old and no longer merges cleanly — a delete candidate"),
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

pub(super) fn help_row(keys: &str, what: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{keys:<22}"),
            theme::id(),
        ),
        Span::styled(what.to_string(), theme::muted()),
    ])
}
