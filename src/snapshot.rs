//! Offline rendering of a captured `but status` payload.
//!
//! Draws one frame into an in-memory backend and emits it as ANSI text. This makes the
//! UI reproducible without a repository, which is what bug reports need — attach the
//! JSON, get the exact board back.

use anyhow::{Context, Result};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

use crate::app::App;
use crate::board::Board;

pub fn render_file(path: &str, width: u16, height: u16) -> Result<String> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("could not read {path}"))?;
    let status = crate::but::parse_status(&raw)?;
    let app = App::from_board(Board::from_status(&status));

    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|f| crate::ui::draw(f, &app))?;
    Ok(to_ansi(terminal.backend().buffer()))
}

fn sgr(c: Color, fg: bool) -> Option<String> {
    let base = if fg { 38 } else { 48 };
    match c {
        Color::Reset => None,
        Color::Indexed(n) => Some(format!("\x1b[{base};5;{n}m")),
        Color::Rgb(r, g, b) => Some(format!("\x1b[{base};2;{r};{g};{b}m")),
        // The palette here is entirely indexed; anything else falls back to default.
        _ => None,
    }
}

/// Converts a rendered buffer to ANSI, re-emitting style only when it changes so the
/// output stays readable if someone pipes it through `cat -v`.
fn to_ansi(buf: &Buffer) -> String {
    let area = buf.area();
    let mut out = String::new();
    for y in 0..area.height {
        let mut last: Option<(Color, Color, Modifier)> = None;
        let mut line = String::new();
        for x in 0..area.width {
            let cell = &buf[(x, y)];
            let key = (cell.fg, cell.bg, cell.modifier);
            if last != Some(key) {
                line.push_str("\x1b[0m");
                if let Some(s) = sgr(cell.fg, true) {
                    line.push_str(&s);
                }
                if let Some(s) = sgr(cell.bg, false) {
                    line.push_str(&s);
                }
                if cell.modifier.contains(Modifier::BOLD) {
                    line.push_str("\x1b[1m");
                }
                last = Some(key);
            }
            line.push_str(cell.symbol());
        }
        line.push_str("\x1b[0m");
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        // Strip SGR sequences so assertions read against the visible text.
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn render(w: u16, h: u16) -> String {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let app = App::from_board(Board::from_status(&status));
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        plain(&to_ansi(t.backend().buffer()))
    }

    #[test]
    fn renders_every_lane_when_wide() {
        let out = render(160, 24);
        for lane in ["unassigned", "feat-auth", "feat-ui", "fix-flaky-tests"] {
            assert!(out.contains(lane), "missing lane {lane} in:\n{out}");
        }
        assert!(out.contains("Wire session refresh into gateway"));
        assert!(out.contains("base 3189356"), "header shows the merge base");
    }

    #[test]
    fn narrow_terminal_scrolls_instead_of_crushing_lanes() {
        let out = render(60, 24);
        assert!(out.contains("unassigned"), "cursor lane stays visible");
        assert!(out.contains('›'), "off-screen lanes are advertised");
        for line in out.lines() {
            assert!(line.chars().count() <= 60, "line overflowed: {line:?}");
        }
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        // Resize handling is the classic TUI crash; make the floor explicit.
        for (w, h) in [(20, 6), (12, 4), (5, 3), (1, 1)] {
            let _ = render(w, h);
        }
    }

    /// A stacked lane must look stacked: each branch below the tip repeats the lane
    /// header's treatment, and the counts must agree rather than double-counting.
    #[test]
    fn a_stacked_lane_renders_a_header_per_branch() {
        let mut status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let extra = status.stacks.remove(2).branches.remove(0);
        status.stacks[0].branches.push(extra);

        let app = App::from_board(Board::from_status(&status));
        let mut t = Terminal::new(TestBackend::new(150, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("feat-auth +1"), "lane names the stack depth");
        assert!(
            out.contains("● fix-flaky-tests"),
            "the stacked branch gets its own dot-and-name header:\n{out}"
        );
        // feat-auth has 2 commits of its own; the stacked branch has 1. The lane header
        // must show 2, not the lane total of 3.
        assert!(
            out.contains("feat-auth +1  2"),
            "lane header counts only the tip's commits:\n{out}"
        );
    }

    /// Counts belong on cards, on each branch of a stack, and on the lane — not just one
    /// lump for the whole lane, which said nothing about which branch was big.
    #[test]
    fn line_counts_appear_on_cards_and_on_each_stacked_branch() {
        use std::collections::HashMap;
        let mut status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        // Fold the third stack into the first so the lane holds two branches.
        let extra = status.stacks.remove(2).branches.remove(0);
        status.stacks[0].branches.push(extra);

        let mut commits: HashMap<String, (usize, usize)> = HashMap::new();
        for (i, stack) in status.stacks.iter().enumerate() {
            for branch in &stack.branches {
                for (j, c) in branch.commits.iter().enumerate() {
                    commits.insert(c.commit_id.clone(), (10 * (i + 1) + j, j + 1));
                }
            }
        }
        let diff: crate::model::DiffOutput = serde_json::from_str(
            r#"{"changes":[{"id":"h0","path":"wip1.txt","diff":{"type":"patch","hunks":[
                {"oldStart":1,"oldLines":1,"newStart":1,"newLines":2,
                 "diff":"@@ -1 +1,2 @@\n-a\n+A\n+B\n"}]}}]}"#,
        )
        .unwrap();

        let board = Board::from_status_diff_and_commits(&status, &diff, &commits);
        let app = App::from_board(board);
        let mut t = Terminal::new(TestBackend::new(120, 22)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("+2 -1"), "a working-tree card counts its own hunk:\n{out}");
        assert!(out.contains("+10 -1"), "and a commit card carries its own");
        // Each branch of the stack reports separately rather than sharing one figure.
        // The stacked branch totals only its own commit, not the lane's.
        assert!(
            out.contains("● fix-flaky-tests  1  +10 -1"),
            "the stacked branch shows its own total:\n{out}"
        );
        assert!(
            out.contains("feat-auth +1  2  +31 -4"),
            "and the tip's header counts the tip's commits, not the whole lane:\n{out}"
        );
        // The header says "uncommitted", so it must exclude committed lines.
        assert!(
            out.contains("+2 -1 uncommitted"),
            "the workspace header counts only working-tree changes:\n{out}"
        );
    }

    #[test]
    fn the_diff_pane_shows_hunks_with_line_numbers_and_signs() {
        let out: crate::model::DiffOutput = serde_json::from_str(
            r#"{"changes":[
              {"id":"h0","path":"a.txt","status":"modified","diff":{"type":"patch","hunks":[
                {"oldStart":1,"oldLines":4,"newStart":1,"newLines":4,
                 "diff":"@@ -1,4 +1,4 @@\n-one\n+ONE CHANGED\n two\n three\n"}]}},
              {"id":"i0","path":"a.txt","status":"modified","diff":{"type":"patch","hunks":[
                {"oldStart":9,"oldLines":2,"newStart":9,"newLines":2,
                 "diff":"@@ -9,2 +9,2 @@\n nine\n-ten\n+TEN CHANGED\n"}]}}
            ]}"#,
        )
        .unwrap();

        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.diff = Some(crate::diff::DiffView::from_output("a.txt", &out));
        app.mode = crate::app::Mode::Diff;

        let mut t = Terminal::new(TestBackend::new(120, 20)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("hunk 1 of 2"), "which hunk is selected");
        // The sign is a column of its own, so it survives being read without colour.
        assert!(out.contains("- one"), "removed lines carry a marker:\n{out}");
        assert!(out.contains("+ ONE CHANGED"), "added lines too");
        assert!(out.contains("m stage this hunk"), "staging is offered");
        // The board is still there: reading a diff must not cost you your place.
        assert!(
            out.contains("unassigned"),
            "the board stays visible beside it:\n{out}"
        );
        assert!(
            out.contains('›'),
            "and advertises the lanes its narrower half cannot fit"
        );
        eprintln!("{out}");
    }

    #[test]
    fn a_committed_hunk_does_not_offer_staging() {
        let out: crate::model::DiffOutput = serde_json::from_str(
            r#"{"changes":[{"path":"a.txt","status":"modified","diff":{"type":"patch","hunks":[
                {"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"diff":"@@ -1 +1 @@\n-a\n+b\n"}]}}]}"#,
        )
        .unwrap();
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.diff = Some(crate::diff::DiffView::from_output("Add auth", &out));
        app.mode = crate::app::Mode::Diff;

        let mut t = Terminal::new(TestBackend::new(80, 16)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));
        assert!(out.contains("committed"), "the hunk is marked as history");
        assert!(
            !out.contains("m stage this hunk"),
            "and staging is not offered:\n{out}"
        );
    }

    #[test]
    fn long_commit_subjects_wrap_inside_the_lane() {
        let mut status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        status.stacks[0].branches[0].commits[0].message =
            "Refactor the entire authentication subsystem and its supporting middleware".into();
        let app = App::from_board(Board::from_status(&status));
        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));
        assert!(out.contains("Refactor the entire"));
        for line in out.lines() {
            assert!(line.chars().count() <= 120);
        }
    }
}
