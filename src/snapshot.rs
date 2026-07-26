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

    /// The `›` marker only ever said "there's more" on the right; scrolled right past the
    /// first lane, there was nothing on the left saying lanes were hidden there too.
    #[test]
    fn scrolling_right_shows_a_left_marker_too() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.col = 3; // fix-flaky-tests, the last of four lanes
        let mut t = Terminal::new(TestBackend::new(60, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(
            out.contains('‹'),
            "lanes are hidden to the left of the current one, but nothing said so:\n{out}"
        );
    }

    /// GitHub issue #3's other half: "I get disoriented as to where the reference point
    /// is." The `‹`/`›` markers only ever said "there's more", never how much more or
    /// which lane you're on — a persistent position readout in the header fixes that even
    /// before you've scrolled far enough to hit an edge marker at all.
    #[test]
    fn header_shows_lane_position_among_the_total() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.col = 2; // feat-ui, the third of four lanes
        let mut t = Terminal::new(TestBackend::new(160, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("lane 3/4"), "expected a lane 3/4 readout in the header:\n{out}");
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        // Resize handling is the classic TUI crash; make the floor explicit.
        for (w, h) in [(20, 6), (12, 4), (5, 3), (1, 1)] {
            let _ = render(w, h);
        }
    }

    /// Reported as "can't find where the top commit/card is on a long stack": the header
    /// used to be the first lines of the same scrolling text as the cards, so navigating
    /// deep into a long list scrolled the lane name, dot, and status away with it. It's
    /// pinned in its own fixed area now; this forces a long list, scrolls the cursor all
    /// the way to the bottom of it, and checks the header is still on screen right
    /// alongside the deeply-nested selected card.
    #[test]
    fn the_lane_header_stays_pinned_scrolled_deep_into_a_long_stack() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};

        let mut status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let template = status.stacks[0].branches[0].commits[0].clone();
        status.stacks[0].branches[0].commits = (0..30)
            .map(|i| {
                let mut c = template.clone();
                c.cli_id = format!("c{i}");
                c.commit_id = format!("{i:040}");
                c.message = format!("Commit number {i}");
                c
            })
            .collect();

        let mut app = App::from_board(Board::from_status(&status));
        app.col = 1; // feat-auth, now carrying the long run of commits
        let mut t = Terminal::new(TestBackend::new(120, 16)).unwrap();

        // 29 downs from card 0 reaches the last of the 30 cards without wrapping back
        // around.
        for _ in 0..29 {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(
            out.contains("feat-auth"),
            "the lane header must stay visible however deep the cursor is:\n{out}"
        );
        assert!(
            out.contains("Commit number 29"),
            "and the deeply-scrolled selected card must also be on screen:\n{out}"
        );
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
            out.contains("○ fix-flaky-tests"),
            "the stacked branch gets its own dot-and-name header, hollow since it isn't \
             the tip actions target:\n{out}"
        );
        // feat-auth has 2 commits of its own; the stacked branch has 1. The lane header
        // must show 2, not the lane total of 3.
        assert!(
            out.contains("feat-auth +1  2"),
            "lane header counts only the tip's commits:\n{out}"
        );
    }

    /// GitHub issue #3: bold title text and a brighter rule alone were too subtle to tell
    /// which of several lanes is current at a glance. Checked against the actual rendered
    /// buffer, not the plain-text output, since the fix is a background colour, which
    /// `plain()` strips entirely.
    #[test]
    fn the_current_lane_header_reads_differently_from_the_others() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let app = App::from_board(Board::from_status(&status)); // col defaults to 0
        let mut t = Terminal::new(TestBackend::new(160, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let buf = t.backend().buffer();

        // `selected_bg` is reverse video (`Modifier::REVERSED`), not a fixed background
        // colour — deliberately, so it reads correctly under any terminal theme (see
        // `theme::selected_bg`'s doc comment). So the signal to check for is the modifier,
        // not `cell.bg`, which never changes.
        //
        // The header row is the first line of the board, right under the 2-line top
        // header. If the current lane's tint is working, some cell there is reversed and
        // some (further along, in another lane's header) is not.
        let header_y = 2;
        let reversed = |x: u16| buf[(x, header_y)].modifier.contains(Modifier::REVERSED);
        assert!(
            (0..buf.area().width).any(reversed),
            "expected the current lane's header to be reverse-video highlighted"
        );
        assert!(
            (0..buf.area().width).any(|x| !reversed(x)),
            "expected at least one other lane's header to stay unhighlighted"
        );
    }

    /// The land confirm dialog has no live CLI counterpart to snapshot against, so its
    /// content is pinned here: the commit list, and the clean/conflicted framing that
    /// decides whether it reads "land" or "land anyway".
    #[test]
    fn land_confirm_shows_commits_and_a_clean_verdict() {
        use crate::app::Mode;
        use crate::model::{MergeCheck, MergeCheckCommit, MergeCheckResult};

        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.mode = Mode::LandConfirm;
        app.land_check = Some(MergeCheck {
            commits_ahead: 1,
            commits: vec![MergeCheckCommit {
                short_sha: "67d01c0".into(),
                message: "theme passthrough from parent terminal".into(),
            }],
            merge_check: MergeCheckResult {
                merges_cleanly: true,
                conflicting_files: Vec::new(),
            },
        });

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("land onto target"));
        assert!(out.contains("67d01c0"));
        assert!(out.contains("theme passthrough from parent terminal"));
        assert!(out.contains("lands cleanly"));
        assert!(out.contains("⏎ / y  land"));
        assert!(!out.contains("land anyway"), "a clean land must not warn:\n{out}");
    }

    /// A conflicted land must read differently, not just differently-coloured: the
    /// verdict says "conflicts" and the confirm line says "land anyway".
    #[test]
    fn land_confirm_names_conflicting_files() {
        use crate::app::Mode;
        use crate::model::{MergeCheck, MergeCheckCommit, MergeCheckResult};

        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.mode = Mode::LandConfirm;
        app.land_check = Some(MergeCheck {
            commits_ahead: 1,
            commits: vec![MergeCheckCommit {
                short_sha: "abc1234".into(),
                message: "Touch a contested file".into(),
            }],
            merge_check: MergeCheckResult {
                merges_cleanly: false,
                conflicting_files: vec!["src/app.rs".into()],
            },
        });

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("conflicts on land"));
        assert!(out.contains("src/app.rs"));
        assert!(out.contains("land anyway"));
    }

    /// Landing runs on a background thread now (GitHub issue #4) rather than freezing the
    /// UI — this pins the spinner overlay that fills the gap. `PendingLand` is built by
    /// hand rather than through a real `but land`, since the point is to check what the
    /// still-running state looks like, not the CLI call that produces it.
    #[test]
    fn landing_shows_a_spinner_that_advances_and_names_the_target() {
        use crate::app::{Mode, PendingLand};
        use std::sync::mpsc;

        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        let (_tx, rx) = mpsc::channel();
        app.mode = Mode::Landing;
        app.landing = Some(PendingLand {
            title: "feat-auth".into(),
            rx,
            spinner: 0,
        });

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let first = plain(&to_ansi(t.backend().buffer()));
        assert!(first.contains("landing feat-auth onto the target"));

        // Nothing to press while landing — the point is that input is swallowed, not that
        // some other key happens to be a no-op.
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('q'),
        ));
        assert_eq!(app.mode, Mode::Landing, "landing has no cancel key");

        app.landing.as_mut().unwrap().spinner += 1;
        t.draw(|f| crate::ui::draw(f, &app)).unwrap();
        let second = plain(&to_ansi(t.backend().buffer()));
        assert_ne!(first, second, "the spinner glyph should advance between frames");
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
            out.contains("○ fix-flaky-tests  1  +10 -1"),
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
