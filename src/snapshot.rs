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
    terminal.draw(|f| {
        crate::ui::draw(f, &app);
    })?;
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        plain(&to_ansi(t.backend().buffer()))
    }

    /// Builds a board with the drawer open over the captured workspace.
    fn drawer_app() -> App {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let list: crate::model::BranchList =
            serde_json::from_str(include_str!("../tests/fixtures/branch_list.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.unapplied = crate::board::Unapplied::from_list(&list, 1_785_000_000_000);
        app.mode = crate::app::Mode::Branches;
        app
    }

    fn render_app(app: &App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, app);
        }).unwrap();
        plain(&to_ansi(t.backend().buffer()))
    }

    /// The modal is a fixed 64-column box (see `draw_branch_modal`), so a message longer
    /// than that must wrap onto as many lines as it needs — the whole thing stays visible
    /// at once, unlike the footer's single scrolling line, which has to hide whatever
    /// doesn't fit behind an ellipsis.
    #[test]
    fn branch_modal_wraps_a_message_longer_than_the_box_instead_of_hiding_it() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.branch_ui = crate::app::BranchUi::Modal;
        app.mode = crate::app::Mode::HarnessMessage;
        let message = "please refactor the auth middleware to use the new session token format and update the tests accordingly";
        for c in message.chars() {
            app.harness_message_input.insert(c);
        }
        let out = render_app(&app, 160, 30);
        assert!(
            out.contains("please refactor the auth middleware"),
            "the start of the message must still be visible, not scrolled away:\n{out}"
        );
        assert!(
            out.contains("accordingly"),
            "the end of the message, next to the cursor, must be visible too:\n{out}"
        );
        assert!(!out.contains('…'), "wrapping should mean nothing needs to be elided:\n{out}");
    }

    /// `KANSTACK_BRANCH_UI=modal` swaps `b`'s footer prompt for a dedicated dialog — this
    /// pins that the dialog actually appears (and the footer stays blank under it) once
    /// `branch_ui` is set, independent of whatever wiring gets `branch_ui` there in a real
    /// run (see `BranchUi::from_env`, read once at startup from that env var).
    #[test]
    fn branch_ui_modal_shows_a_dialog_instead_of_the_footer_prompt() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.branch_ui = crate::app::BranchUi::Modal;
        app.mode = crate::app::Mode::Branch;
        let out = render_app(&app, 160, 30);
        assert!(out.contains("new branch"), "missing the modal's title:\n{out}");
        assert!(out.contains("⏎ create"), "missing the modal's hint line:\n{out}");
        assert!(
            !out.contains("tab switch · shift-tab cmux · esc cancel"),
            "the old footer-style hint text leaked through instead of the modal's own:\n{out}"
        );
    }

    /// The drawer's job is to answer "what can I apply, and will it hurt" — so both branch
    /// names and both merge verdicts have to survive the render, not just the names.
    #[test]
    fn the_drawer_lists_unapplied_branches_with_their_merge_verdict() {
        let out = render_app(&drawer_app(), 160, 24);
        assert!(out.contains("unapplied"), "missing the drawer header:\n{out}");
        assert!(out.contains("feat-theme"), "missing the clean branch:\n{out}");
        assert!(
            out.contains("cmux-tab-on-(b)ranch"),
            "missing the conflicting branch:\n{out}"
        );
        assert!(out.contains("clean"), "missing the clean verdict:\n{out}");
        assert!(
            out.contains("conflicts"),
            "a branch that will not merge must say so before you apply it:\n{out}"
        );
    }

    /// A branch flagged `stale` gets a visible nudge in the list row — distinct from the
    /// plain `conflicts` verdict a fresh, merely-not-yet-merged branch also carries.
    #[test]
    fn a_stale_branch_is_marked_in_the_list_row() {
        let mut app = drawer_app();
        app.unapplied.branches[1].stale = true; // cmux-tab-on-(b)ranch, already conflicting
        let out = render_app(&app, 160, 24);
        assert!(out.contains("stale"), "the stale branch should be flagged:\n{out}");
        // feat-theme (row 0) is left alone — only one branch was marked stale.
        let stale_line = out.lines().find(|l| l.contains("stale")).unwrap();
        assert!(
            !stale_line.contains("feat-theme"),
            "stale must mark the flagged row, not bleed into the other one:\n{out}"
        );
    }

    /// The detail view repeats the same nudge, with the reasoning spelled out — the list's
    /// one-word tag doesn't have room to say *why*.
    #[test]
    fn the_branch_preview_explains_a_stale_flag() {
        use crate::model::{MergeCheck, MergeCheckCommit, MergeCheckResult};

        let mut app = drawer_app();
        app.branch_preview = Some(crate::app::BranchPreview {
            name: "cmux-tab-on-(b)ranch".into(),
            check: MergeCheck {
                commits_ahead: 1,
                commits: vec![MergeCheckCommit {
                    short_sha: "56b9641".into(),
                    message: "Optional cmux-tui bridge".into(),
                    insertions: None,
                    deletions: None,
                }],
                merge_check: MergeCheckResult {
                    merges_cleanly: false,
                    conflicting_files: Vec::new(),
                },
            },
            scroll: 0,
            stale: true,
        });
        let out = render_app(&app, 160, 24);
        assert!(
            out.contains("looks stale"),
            "the preview should explain the stale flag, not just repeat the tag:\n{out}"
        );
    }

    /// The drawer's detail view replaces the list in the same pane — the branch's own
    /// commits, then exactly which files would conflict, since that's the whole reason
    /// `⏎` exists: the list's dot says *whether*, the preview says *what*.
    #[test]
    fn the_branch_preview_shows_commits_and_conflicting_files() {
        use crate::model::{ConflictingFile, MergeCheck, MergeCheckCommit, MergeCheckResult};

        let mut app = drawer_app();
        app.branch_preview = Some(crate::app::BranchPreview {
            name: "cmux-tab-on-(b)ranch".into(),
            check: MergeCheck {
                commits_ahead: 1,
                commits: vec![MergeCheckCommit {
                    short_sha: "56b9641".into(),
                    message: "Optional cmux-tui bridge".into(),
                    insertions: Some(242),
                    deletions: Some(3),
                }],
                merge_check: MergeCheckResult {
                    merges_cleanly: false,
                    conflicting_files: vec![ConflictingFile {
                        path: "src/cmux.rs".into(),
                        branch_commits: Vec::new(),
                        upstream_commits: vec![MergeCheckCommit {
                            short_sha: "def5678".into(),
                            message: "unrelated change".into(),
                            insertions: None,
                            deletions: None,
                        }],
                    }],
                },
            },
            scroll: 0,
            stale: false,
        });

        let out = render_app(&app, 160, 24);
        assert!(
            out.contains("cmux-tab-on-(b)ranch"),
            "missing the previewed branch's name:\n{out}"
        );
        assert!(out.contains("56b9641"), "missing its commit:\n{out}");
        assert!(
            out.contains("Optional cmux-tui bridge"),
            "the preview is wide enough (PREVIEW_WIDTH) that this shouldn't need truncating:\n{out}"
        );
        assert!(out.contains("+242"), "missing the commit's insertion count:\n{out}");
        assert!(out.contains("-3"), "missing the commit's deletion count:\n{out}");
        assert!(
            out.contains("src/cmux.rs"),
            "missing the conflicting file:\n{out}"
        );
        assert!(
            out.contains("vs 1 upstream commit"),
            "missing the upstream-collision count:\n{out}"
        );
        // The list itself must be gone — this is a replacement, not an addition.
        assert!(
            !out.contains("feat-theme"),
            "the list's other branch should not still be showing:\n{out}"
        );
    }

    /// A drawer mixing local and remote-only branches draws one divider between them,
    /// after every local row and before every remote-only one — not scattered per-branch
    /// "remote" tags with nothing grouping them.
    #[test]
    fn the_drawer_divides_local_branches_from_remote_only_ones() {
        let branch = |name: &str, has_local: bool| crate::board::UnappliedBranch {
            name: name.into(),
            commits_ahead: None,
            merges_cleanly: None,
            author: None,
            age: None,
            has_local,
            stale: false,
        };
        let mut app = drawer_app();
        app.unapplied = crate::board::Unapplied {
            branches: vec![
                branch("local-one", true),
                branch("remote-one", false),
                branch("remote-two", false),
            ],
            truncated: false,
        };
        let out = render_app(&app, 160, 24);
        assert!(out.contains("remote-only"), "missing the divider:\n{out}");

        let local_row = out.lines().position(|l| l.contains("local-one")).unwrap();
        let divider_row = out.lines().position(|l| l.contains("remote-only")).unwrap();
        let remote_row = out.lines().position(|l| l.contains("remote-one")).unwrap();
        assert!(
            local_row < divider_row && divider_row < remote_row,
            "the divider must sit between the local and remote-only rows:\n{out}"
        );
    }

    /// An all-local or all-remote list has nothing to divide, so no divider should appear
    /// — one side of it would always be empty.
    #[test]
    fn an_undivided_drawer_draws_no_divider() {
        let out = render_app(&drawer_app(), 160, 24);
        assert!(
            !out.contains("remote-only"),
            "both fixture branches are remote-only; there's nothing to divide:\n{out}"
        );
    }

    /// The drawer splits the board rather than covering it. Applying a branch is a choice
    /// made against the lanes already open, so losing sight of them would hide half the
    /// question — the same reason the diff pane splits.
    #[test]
    fn the_drawer_leaves_the_board_visible_beside_it() {
        let out = render_app(&drawer_app(), 160, 24);
        for lane in ["feat-auth", "feat-ui", "fix-flaky-tests"] {
            assert!(out.contains(lane), "the drawer hid lane {lane}:\n{out}");
        }
    }

    /// The drawer opens on the left, before the lanes. Pinned by column position rather
    /// than trusted to the layout constraints: swapping the two `split` halves is a
    /// one-character edit that nothing else here would catch.
    #[test]
    fn the_drawer_opens_on_the_left_of_the_lanes() {
        let out = render_app(&drawer_app(), 160, 24);
        // The unassigned lane is hidden while the drawer is open (see
        // `board_browsing_hides_the_unassigned_lane` below), so the first lane beside the
        // drawer is the first real one — `fix-flaky-tests` in this fixture.
        let row = out
            .lines()
            .find(|l| l.contains("unapplied") && l.contains("fix-flaky-tests"))
            .expect("the drawer header and the first lane share a row");
        assert!(
            row.find("unapplied") < row.find("fix-flaky-tests"),
            "the drawer must sit left of the board:\n{row}"
        );

        // And it must not crowd them: a gap between the drawer's rule and the first lane
        // is what keeps it reading as its own panel rather than as another column.
        //
        // Measured in character columns, not byte offsets — the rule is box-drawing and the
        // lane headers start with `●`, both multi-byte, so `str::find` would report neither
        // a column nor anything comparable between the two rows.
        let col_of = |line: &str, needle: &str| -> usize {
            let byte = line.find(needle).expect("needle is on this row");
            line[..byte].chars().count()
        };
        let rule = out
            .lines()
            .find(|l| l.trim_start().starts_with('─'))
            .expect("the drawer draws a rule under its header");
        // The first unbroken run of `─` is the drawer's; the lanes draw their own further
        // right, so this stops at the first gap rather than spanning the whole row.
        let start = col_of(rule, "─");
        let drawer_end = start + rule.chars().skip(start).take_while(|c| *c == '─').count();
        let lane_start = col_of(row, "fix-flaky-tests");
        assert!(
            lane_start > drawer_end + 1,
            "drawer rule ends at column {drawer_end}, lane starts at {lane_start} — \
             too tight to read as a separate panel:\n{out}"
        );
    }

    /// Browsing unapplied branches is a decision made against the lanes already applied —
    /// the unassigned lane (uncommitted worktree changes) has nothing to add to that call,
    /// so it drops out of view while the drawer is open rather than eating a column of the
    /// now-scarcer width.
    #[test]
    fn board_browsing_hides_the_unassigned_lane() {
        let out = render_app(&drawer_app(), 160, 24);
        assert!(
            !out.contains("unassigned"),
            "the unassigned lane should be hidden while the branches drawer is open:\n{out}"
        );
    }

    /// A terminal too narrow to split must still render something coherent. The drawer is
    /// capped at half the board area precisely so it can never squeeze the lanes out.
    #[test]
    fn the_drawer_does_not_panic_in_a_narrow_terminal() {
        for w in [20, 30, 40, 60] {
            let out = render_app(&drawer_app(), w, 12);
            assert!(!out.is_empty());
        }
    }

    /// The detail view does its own width-dependent truncation (subject, conflicting
    /// paths) independent of the list's — worth the same narrow-terminal sweep.
    #[test]
    fn the_branch_preview_does_not_panic_in_a_narrow_terminal() {
        let mut app = drawer_app();
        app.branch_preview = Some(crate::app::BranchPreview {
            name: "cmux-tab-on-(b)ranch".into(),
            check: crate::model::MergeCheck {
                commits_ahead: 1,
                commits: vec![crate::model::MergeCheckCommit {
                    short_sha: "56b9641".into(),
                    message: "Optional cmux-tui bridge".into(),
                    insertions: Some(242),
                    deletions: Some(3),
                }],
                merge_check: crate::model::MergeCheckResult {
                    merges_cleanly: false,
                    conflicting_files: vec![crate::model::ConflictingFile {
                        path: "src/cmux.rs".into(),
                        branch_commits: Vec::new(),
                        upstream_commits: vec![crate::model::MergeCheckCommit {
                            short_sha: "def5678".into(),
                            message: "unrelated change".into(),
                            insertions: None,
                            deletions: None,
                        }],
                    }],
                },
            },
            scroll: 0,
            stale: false,
        });
        for w in [1, 5, 20, 30, 40, 60] {
            let out = render_app(&app, w, 12);
            assert!(!out.is_empty());
        }
    }

    /// An empty drawer says why it is empty. "No rows" and "could not read the branches"
    /// look identical otherwise, and only one of them means there is nothing to do.
    #[test]
    fn an_empty_drawer_explains_itself() {
        let mut app = drawer_app();
        app.unapplied = crate::board::Unapplied::default();
        let out = render_app(&app, 160, 24);
        assert!(
            out.contains("every branch is already"),
            "an empty drawer must explain itself:\n{out}"
        );
    }

    /// `but branch list` truncates to the 20 most recent by default. A drawer that quietly
    /// showed a partial list as if it were complete would be lying by omission.
    #[test]
    fn a_truncated_branch_list_says_so() {
        let mut app = drawer_app();
        app.unapplied.truncated = true;
        let out = render_app(&app, 160, 24);
        assert!(
            out.contains("only the 20 most recent"),
            "a truncated list must admit it:\n{out}"
        );
        assert!(
            out.contains("A loads all"),
            "a truncated list must also say how to see the rest:\n{out}"
        );
    }

    /// The confirmation for `U` has to name the whole-stack behaviour, because that is the
    /// part the user cannot see coming: `but unapply` takes the entire stack containing the
    /// named branch, so on a stacked lane one keypress removes several branches at once.
    #[test]
    fn the_unapply_confirmation_names_every_branch_that_would_leave() {
        // Same construction `a_stacked_lane_renders_a_header_per_branch` uses: the captured
        // workspace has three parallel lanes and no stacked one, so one branch is moved
        // into another lane to make one.
        let mut status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let extra = status.stacks.remove(2).branches.remove(0);
        status.stacks[0].branches.push(extra);

        let mut app = App::from_board(Board::from_status(&status));
        // Land on the lane that now has more than one branch stacked in it.
        let stacked = app
            .board
            .columns
            .iter()
            .position(|c| c.sections.len() > 1)
            .expect("fixture has a stacked lane");
        app.col = stacked;
        let names: Vec<String> = app.board.columns[stacked]
            .sections
            .iter()
            .map(|s| s.name.clone())
            .collect();
        app.mode = crate::app::Mode::UnapplyConfirm;

        let out = render_app(&app, 160, 24);
        assert!(out.contains("unapply lane"), "missing the title:\n{out}");
        for n in &names {
            assert!(
                out.contains(n.as_str()),
                "the confirmation must name {n}, which would also leave:\n{out}"
            );
        }
        assert!(
            out.contains("nothing is lost"),
            "unapply is reversible and must say so, unlike delete:\n{out}"
        );
    }

    /// The one lane `a` cannot bring back. `but branch list` omits branches with no
    /// commits, so an empty lane — exactly what `b` creates — vanishes from the UI on `U`
    /// unless the confirmation says otherwise. Pins the wording against the promise the
    /// non-empty case makes, since the two must not be confused.
    #[test]
    fn unapplying_an_empty_lane_does_not_promise_the_drawer_will_offer_it_back() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        let lane = app
            .board
            .columns
            .iter()
            .position(|c| c.branch_name.is_some())
            .expect("fixture has a lane");
        // Strip the lane back to an empty branch, the state `b` leaves behind.
        app.board.columns[lane].cards.clear();
        for s in &mut app.board.columns[lane].sections {
            s.commits = 0;
        }
        app.col = lane;
        app.mode = crate::app::Mode::UnapplyConfirm;

        let out = render_app(&app, 160, 24);
        assert!(
            out.contains("no commits") && out.contains("but apply"),
            "an empty lane must be told it comes back via `but apply`, not the drawer:\n{out}"
        );
        assert!(
            !out.contains("re-apply it any time with"),
            "and must not repeat the promise the non-empty case makes:\n{out}"
        );
    }

    /// GitHub issue #6: with a lot of loose files, a flat unassigned list stops being
    /// navigable by eye. Grouping is applied the same way `App::clamp` does it — as a
    /// post-process on the built board's backlog column — rather than reaching into
    /// `Board::build`, so this pins the exact same code path a real toggle takes.
    #[test]
    fn folder_grouping_shows_a_divider_per_directory_and_flat_shows_none() {
        let mut status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        status.uncommitted_changes[0].file_path = "src/nested/wip1.txt".into();
        status.uncommitted_changes[1].file_path = "docs/wip2.txt".into();

        let flat = App::from_board(Board::from_status(&status));
        let mut t = Terminal::new(TestBackend::new(160, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &flat);
        }).unwrap();
        let flat_out = plain(&to_ansi(t.backend().buffer()));
        assert!(
            !flat_out.contains("▸ "),
            "flat view should show no folder dividers:\n{flat_out}"
        );

        let mut board = Board::from_status(&status);
        crate::board::group_unassigned_by_folder(&mut board.columns[0]);
        let grouped = App::from_board(board);
        let mut t = Terminal::new(TestBackend::new(160, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &grouped);
        }).unwrap();
        let grouped_out = plain(&to_ansi(t.backend().buffer()));
        assert!(grouped_out.contains("▸ docs"), "missing the docs divider:\n{grouped_out}");
        assert!(
            grouped_out.contains("▸ src/nested"),
            "missing the src/nested divider:\n{grouped_out}"
        );
        // docs sorts before src/nested, so wip2.txt (in docs) should read first.
        assert!(grouped_out.find("wip2.txt") < grouped_out.find("wip1.txt"));
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("lane 3/4"), "expected a lane 3/4 readout in the header:\n{out}");
    }

    #[test]
    fn header_shows_card_position_within_the_lane_too() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.col = 1; // feat-auth, two commits
        app.card = 1;
        let mut t = Terminal::new(TestBackend::new(160, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(
            out.contains("feat-auth  2/2"),
            "expected a 2/2 readout in the lane's own header:\n{out}"
        );
    }

    #[test]
    fn header_omits_card_position_with_only_one_card() {
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.col = 3; // fix-flaky-tests, a single-commit lane
        let mut t = Terminal::new(TestBackend::new(160, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(!out.contains("1/1"), "a single card is not a position worth reporting:\n{out}");
    }

    #[test]
    fn a_double_digit_position_readout_is_not_clipped() {
        // The title truncation used to reserve a flat budget sized for a plain one- or
        // two-digit count; a "position/total" readout that runs to double digits both
        // sides (e.g. "10/10") needs more room, or its tail gets clipped off the lane
        // header instead of the title losing a character to it. A long branch name at the
        // minimum column width is what pushes the title right up against that budget.
        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.col = 1;
        app.board.columns[1].title = "a-rather-long-feature-branch-name".to_string();
        app.board.columns[1].cards.truncate(1);
        for i in 0..9 {
            let mut card = app.board.columns[1].cards[0].clone();
            card.cli_id = format!("c{i}");
            app.board.columns[1].cards.push(card);
        }
        app.card = 9; // the 10th of 10 cards
        // Narrow enough that the lane sits at the minimum column width, where a long
        // title's truncation point actually meets the count budget.
        let mut t = Terminal::new(TestBackend::new(110, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(
            out.contains("10/10"),
            "expected the full 10/10 readout, not a clipped tail:\n{out}"
        );
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
                insertions: Some(43),
                deletions: Some(32),
            }],
            merge_check: MergeCheckResult {
                merges_cleanly: true,
                conflicting_files: Vec::new(),
            },
        });

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));

        assert!(out.contains("land onto target"));
        assert!(out.contains("67d01c0"));
        assert!(out.contains("theme passthrough from parent terminal"));
        assert!(out.contains("+43"), "missing the commit's insertion count:\n{out}");
        assert!(out.contains("-32"), "missing the commit's deletion count:\n{out}");
        assert!(out.contains("lands cleanly"));
        assert!(out.contains("⏎ / y  land"));
        assert!(!out.contains("land anyway"), "a clean land must not warn:\n{out}");
    }

    /// A conflicted land must read differently, not just differently-coloured: the
    /// verdict says "conflicts" and the confirm line says "land anyway".
    #[test]
    fn land_confirm_names_conflicting_files() {
        use crate::app::Mode;
        use crate::model::{ConflictingFile, MergeCheck, MergeCheckCommit, MergeCheckResult};

        let status =
            crate::but::parse_status(include_str!("../tests/fixtures/status.json")).unwrap();
        let mut app = App::from_board(Board::from_status(&status));
        app.mode = Mode::LandConfirm;
        app.land_check = Some(MergeCheck {
            commits_ahead: 1,
            commits: vec![MergeCheckCommit {
                short_sha: "abc1234".into(),
                message: "Touch a contested file".into(),
                insertions: None,
                deletions: None,
            }],
            merge_check: MergeCheckResult {
                merges_cleanly: false,
                conflicting_files: vec![ConflictingFile {
                    path: "src/app.rs".into(),
                    branch_commits: Vec::new(),
                    upstream_commits: Vec::new(),
                }],
            },
        });

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
            branch_count: 1,
            rx,
            spinner: 0,
        });

        let mut t = Terminal::new(TestBackend::new(120, 24)).unwrap();
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let first = plain(&to_ansi(t.backend().buffer()));
        assert!(first.contains("landing feat-auth onto the target"));

        // Nothing to press while landing — the point is that input is swallowed, not that
        // some other key happens to be a no-op.
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('q'),
        ));
        assert_eq!(app.mode, Mode::Landing, "landing has no cancel key");

        app.landing.as_mut().unwrap().spinner += 1;
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
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
        t.draw(|f| {
            crate::ui::draw(f, &app);
        }).unwrap();
        let out = plain(&to_ansi(t.backend().buffer()));
        assert!(out.contains("Refactor the entire"));
        for line in out.lines() {
            assert!(line.chars().count() <= 120);
        }
    }
}
