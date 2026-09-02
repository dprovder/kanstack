//! An interactive, hands-on walkthrough of kanstack's keys, run against a real, throwaway
//! GitButler workspace rather than a scripted fake.
//!
//! Every step names an action and only advances once the board actually shows the result —
//! there is no "press any key to continue." That means the steps are checked against real
//! `but` state after every keystroke, the same way the rest of the app refreshes.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::but::But;
use crate::theme;

/// One step: what to tell the user, and how to tell whether they did it.
pub struct Step {
    pub prompt: &'static str,
    /// `true` once the board shows the step is done. Takes `&App` rather than `&Board` so
    /// a step can also look at cursor position or mode, not just workspace content.
    done: Box<dyn Fn(&App) -> bool>,
}

pub struct Tutorial {
    steps: Vec<Step>,
    pub current: usize,
    /// Set once the last step completes, so the UI can show a final message instead of
    /// silently running out of steps.
    pub finished: bool,
}

impl Default for Tutorial {
    fn default() -> Self {
        Self::new()
    }
}

impl Tutorial {
    pub fn new() -> Self {
        Tutorial {
            steps: steps(),
            current: 0,
            finished: false,
        }
    }

    pub fn prompt(&self) -> &'static str {
        self.steps
            .get(self.current)
            .map(|s| s.prompt)
            .unwrap_or("You've been through every key kanstack binds. Press q to leave the practice repo, or keep poking around — nothing here is real.")
    }

    pub fn step_label(&self) -> String {
        if self.finished {
            "done".to_string()
        } else {
            format!("{} / {}", self.current + 1, self.steps.len())
        }
    }

    /// Checked after every keystroke. Advances past every step whose condition already
    /// holds (not just the current one), so a user who does two things at once — or
    /// undoes past a step and redoes it — is not stuck waiting on a step already met.
    pub fn advance(&mut self, app: &App) -> bool {
        let before = self.current;
        while !self.finished {
            match self.steps.get(self.current) {
                Some(step) if (step.done)(app) => self.current += 1,
                Some(_) => break,
                None => self.finished = true,
            }
        }
        self.current != before || self.finished && before < self.steps.len()
    }
}

fn has_lane(app: &App, branch_name: &str) -> bool {
    app.board
        .columns
        .iter()
        .any(|c| c.branch_name.as_deref() == Some(branch_name))
}

/// Builds the scripted lesson.
pub fn steps() -> Vec<Step> {
    vec![
        Step {
            prompt: "Welcome to kanstack! This is a real, throwaway practice repo — nothing here touches your projects. Press → to move to the \"practice\" lane.",
            done: Box::new(|app| {
                app.board
                    .columns
                    .get(app.col)
                    .is_some_and(|c| c.branch_name.as_deref() == Some("practice"))
            }),
        },
        Step {
            prompt: "Press ↓ to select the commit in this lane, then ⏎ to open its diff.",
            done: Box::new(|app| app.mode == crate::app::Mode::Diff),
        },
        Step {
            prompt: "← goes back to the board. Then press m on the notes.txt card in \"unassigned\", → to reach \"practice\", and ⏎ to drop it there. \"practice\" already has a commit, so this amends notes.txt straight into it — no separate commit step needed.",
            done: Box::new(|app| {
                app.board
                    .columns
                    .iter()
                    .find(|c| c.branch_name.as_deref() == Some("practice"))
                    .and_then(|c| c.cards.first())
                    .is_some_and(|card| {
                        card.subtitle.as_deref().is_some_and(|s| s.contains("notes.txt"))
                    })
            }),
        },
        Step {
            prompt: "Every lane so far has been applied — in the workspace. Press U to unapply this one, then ⏎ / y to confirm. Watch it leave the board.",
            done: Box::new(|app| !has_lane(app, "practice")),
        },
        Step {
            prompt: "It isn't gone — it's just not in the workspace, so the board has nothing to draw. Most branches in a real repo are like this. Press a to open the drawer on the left, then ⏎ to apply \"practice\" back as a lane.",
            done: Box::new(|app| has_lane(app, "practice")),
        },
        Step {
            prompt: "Press L to preview landing this lane onto the target, then ⏎ / y to confirm.",
            done: Box::new(|app| !has_lane(app, "practice")),
        },
        Step {
            prompt: "That really landed onto the practice target. Press z to undo it.",
            done: Box::new(|app| has_lane(app, "practice")),
        },
    ]
}

/// A one-time, dismissible offer shown right after first-run `--setup`, asking whether to
/// walk through the keys now. `true` means yes. Owns its own terminal session — the same
/// standalone pattern `crate::setup::run` uses — since asking this needs no repository or
/// `App` yet; the practice repo is only built once the answer is yes.
pub fn offer() -> Result<bool> {
    let mut terminal = ratatui::init();
    let result = offer_loop(&mut terminal);
    ratatui::restore();
    result
}

fn offer_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<bool> {
    use ratatui::crossterm::event::{self, Event, KeyCode as K, KeyEventKind};
    loop {
        terminal.draw(draw_offer)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            K::Char('y' | 'Y') | K::Enter => return Ok(true),
            K::Char('n' | 'N') | K::Esc | K::Char('q') => return Ok(false),
            _ => {}
        }
    }
}

fn draw_offer(f: &mut Frame) {
    let area = f.area();
    f.render_widget(Clear, area);
    let body = vec![
        Line::styled("  kanstack", theme::title(true)),
        Line::raw(""),
        Line::raw("  Want a quick interactive walkthrough of the keys, in a real,"),
        Line::raw("  throwaway practice repo? Nothing here touches your real projects."),
        Line::raw(""),
        Line::raw("  y  yes, walk me through it      n / esc  skip, go straight to the board"),
    ];
    let w = 70.min(area.width.saturating_sub(4));
    let h = (body.len() as u16 + 2).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(body).block(Block::bordered().border_style(theme::faint())),
        popup,
    );
}

/// Creates a temporary git repository, runs `but setup` in it, and seeds a small board: one
/// commit on a "practice" lane, and an unstaged file waiting in the backlog. Returns the
/// repository path; the caller owns cleaning it up (or not — it's in the OS temp dir).
///
/// This deliberately duplicates `tests/live.rs`'s `Sandbox` rather than sharing it: that
/// helper lives in the integration test target and is not part of the library, and the two
/// have different jobs — one is a throwaway test fixture, this is a real feature a user
/// runs, so it uses `But`'s own methods to seed content rather than raw `but` subprocess
/// calls wherever that's available.
pub fn build_practice_repo() -> Result<PathBuf> {
    // `but setup --init` registers this path in GitButler's *global* project list and never
    // deregisters it — every `--tutorial` run leaves a permanent entry behind. Naming the
    // directory by pid alone (as this used to) means a later run can reuse a pid the OS
    // already recycled, landing on a path GitButler has stale cached project metadata for
    // from a completely different git history; the real symptom this produced was `but rub`
    // silently dropping its embedded status (`{"ok":true}` with no `status` key) on the very
    // first mutation against the freshly recreated repo. A nanosecond suffix makes the path
    // unique to this run, so there is nothing stale to collide with.
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let root = std::env::temp_dir().join(format!(
        "kanstack-tutorial-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).context("could not create a temp directory")?;

    let run = |bin: &str, args: &[&str], cwd: &Path| -> Result<()> {
        let out = Command::new(bin)
            .args(args)
            .current_dir(cwd)
            .output()
            .with_context(|| format!("failed to spawn {bin} {args:?}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "{bin} {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    };

    run("git", &["init", "-q", "."], &root)?;
    run("git", &["config", "user.email", "you@example.com"], &root)?;
    run("git", &["config", "user.name", "kanstack tutorial"], &root)?;
    std::fs::write(root.join("README.md"), "practice repo\n")?;
    run("git", &["add", "."], &root)?;
    run("git", &["commit", "-qm", "base"], &root)?;
    run("but", &["setup", "--init", "--json"], &root)?;

    let but = But::discover(&root)?;
    but.branch_new("practice", None)?;
    std::fs::write(root.join("warmup.txt"), "warm up the board\n")?;
    let status = but.status()?;
    let warmup_id = status
        .uncommitted_changes
        .iter()
        .find(|c| c.file_path == "warmup.txt")
        .map(|c| c.cli_id.clone())
        .context("seed file did not show up as uncommitted")?;
    but.commit(&[warmup_id], "Warm up the board", "practice")?;
    std::fs::write(root.join("notes.txt"), "things to remember\n")?;

    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};

    fn skip_if_no_but() -> bool {
        Command::new("but").arg("--version").output().is_err()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    /// Drives the whole scripted lesson end to end with the exact keystrokes the prompts
    /// describe, against a real sandboxed repo, and checks every step actually advances —
    /// not just that the app doesn't panic. If a step's `done` predicate is wrong (checks
    /// the wrong lane, the wrong count, whatever), this is what catches it: a predicate
    /// that never becomes true here would hang forever, so it fails loudly on a real repo
    /// rather than shipping a tutorial step nobody can ever complete.
    #[test]
    #[ignore = "requires the GitButler CLI"]
    fn every_step_advances_on_the_prescribed_keys() {
        if skip_if_no_but() {
            return;
        }
        let root = build_practice_repo().expect("seed the practice repo");
        let but = But::discover(&root).expect("discover but in the practice repo");
        let mut app = App::new(but, None).expect("build the app against the seeded repo");
        app.tutorial = Some(Tutorial::new());

        let total_steps = app.tutorial.as_ref().unwrap().steps.len();

        // Step 1: move → to the practice lane.
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.tutorial.as_ref().unwrap().current, 1, "step 1 did not advance");

        // Step 2: ↓ then ⏎ opens the diff.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.tutorial.as_ref().unwrap().current, 2, "step 2 did not advance");

        // Step 3: ← back to the board, ← again to unassigned, m to pick up notes.txt,
        // → to practice, ⏎ to drop it. "practice" already has a commit, so this amends
        // straight into it — no separate commit step needed anymore.
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Char('m')));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.tutorial.as_ref().unwrap().current,
            3,
            "step 3 did not advance — amending notes.txt into practice failed"
        );

        // Step 4: U then ⏎ unapplies the lane. No navigation first — the drop in step 3
        // left the cursor on "practice", which is why the prompt does not ask for one.
        assert_eq!(
            app.board.columns[app.col].branch_name.as_deref(),
            Some("practice"),
            "step 3 should leave the cursor on practice, as step 4's prompt assumes"
        );
        app.on_key(key(KeyCode::Char('U')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.tutorial.as_ref().unwrap().current,
            4,
            "step 4 did not advance — unapplying practice failed"
        );

        // Step 5: a opens the drawer, ⏎ applies the branch under the cursor. The prompt
        // names "practice" specifically, which only holds because it is the sole unapplied
        // branch here — asserted rather than assumed, since a second entry would silently
        // make ⏎ apply something else. This deliberately runs *before* the land: afterwards
        // `practice` has no commits ahead of the target, and `but branch list` hides empty
        // branches, so the drawer would be empty and the step impossible to complete.
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.mode, crate::app::Mode::Branches, "a should open the drawer");
        let listed: Vec<&str> = app
            .unapplied
            .branches
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(listed, ["practice"], "the drawer must offer exactly one branch");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.tutorial.as_ref().unwrap().current,
            5,
            "step 5 did not advance — applying practice back failed"
        );

        // Step 6: L, then ⏎ to confirm landing onto the (fake, local) target. Landing runs
        // on a background thread now, so the step only advances once `poll_land` picks up
        // the result — a step tied to `on_key` alone would never see it complete.
        app.on_key(key(KeyCode::Char('L')));
        app.on_key(key(KeyCode::Enter));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.mode == crate::app::Mode::Landing {
            assert!(std::time::Instant::now() < deadline, "land never completed");
            app.poll_land();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(app.tutorial.as_ref().unwrap().current, 6, "step 6 did not advance");

        // Step 7: z to undo the land.
        app.on_key(key(KeyCode::Char('z')));
        let t = app.tutorial.as_ref().unwrap();
        assert!(t.finished, "undo should have completed the final step");
        assert_eq!(t.current, total_steps);

        let _ = std::fs::remove_dir_all(&root);
    }
}
