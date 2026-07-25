//! An interactive, hands-on walkthrough of kanstack's keys, run against a real, throwaway
//! GitButler workspace rather than a scripted fake.
//!
//! Every step names an action and only advances once the board actually shows the result —
//! there is no "press any key to continue." That means the steps are checked against real
//! `but` state after every keystroke, the same way the rest of the app refreshes.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::app::App;
use crate::but::But;

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

impl Tutorial {
    pub fn new(seed_commits: usize) -> Self {
        Tutorial {
            steps: steps(seed_commits),
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

fn commits_in(app: &App, branch_name: &str) -> usize {
    app.board
        .columns
        .iter()
        .find(|c| c.branch_name.as_deref() == Some(branch_name))
        .map_or(0, |c| {
            c.cards
                .iter()
                .filter(|card| card.kind == crate::board::CardKind::Commit)
                .count()
        })
}

fn has_lane(app: &App, branch_name: &str) -> bool {
    app.board
        .columns
        .iter()
        .any(|c| c.branch_name.as_deref() == Some(branch_name))
}

/// Builds the scripted lesson. `seed_commits` is how many commits `practice` starts with
/// (so the "commit something" step can watch for one more, whatever the starting count).
pub fn steps(seed_commits: usize) -> Vec<Step> {
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
            prompt: "← goes back to the board. Then press m on the notes.txt card in \"unassigned\", → to reach \"practice\", and ⏎ to drop it there.",
            done: Box::new(|app| {
                app.board
                    .columns
                    .iter()
                    .find(|c| c.branch_name.as_deref() == Some("practice"))
                    .is_some_and(|c| {
                        c.cards.iter().any(|card| {
                            card.title == "notes.txt" && card.kind == crate::board::CardKind::Change
                        })
                    })
            }),
        },
        Step {
            prompt: "Press c, type a message, and ⏎ to commit what's staged there.",
            done: Box::new(move |app| commits_in(app, "practice") > seed_commits),
        },
        Step {
            prompt: "Press M to preview landing this lane onto the target, then ⏎ / y to confirm.",
            done: Box::new(|app| !has_lane(app, "practice")),
        },
        Step {
            prompt: "That really landed onto the practice target. Press z to undo it.",
            done: Box::new(|app| has_lane(app, "practice")),
        },
    ]
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
    let root = std::env::temp_dir().join(format!("kanstack-tutorial-{}", std::process::id()));
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
    run("but", &["setup", "--init", "--format", "json"], &root)?;

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
    but.rub(&warmup_id, "practice")?;
    but.commit("practice", "Warm up the board")?;
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
        app.tutorial = Some(Tutorial::new(1));

        let total_steps = app.tutorial.as_ref().unwrap().steps.len();

        // Step 1: move → to the practice lane.
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.tutorial.as_ref().unwrap().current, 1, "step 1 did not advance");

        // Step 2: ↓ then ⏎ opens the diff.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.tutorial.as_ref().unwrap().current, 2, "step 2 did not advance");

        // Step 3: ← back to the board, ← again to unassigned, m to pick up notes.txt,
        // → to practice, ⏎ to drop it (staging it there).
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Char('m')));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.tutorial.as_ref().unwrap().current,
            3,
            "step 3 did not advance — staging notes.txt to practice failed"
        );

        // Step 4: c, type a message, ⏎.
        app.on_key(key(KeyCode::Char('c')));
        for c in "Remember things".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.tutorial.as_ref().unwrap().current, 4, "step 4 did not advance");

        // Step 5: M, then ⏎ to confirm landing onto the (fake, local) target.
        app.on_key(key(KeyCode::Char('M')));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.tutorial.as_ref().unwrap().current, 5, "step 5 did not advance");

        // Step 6: z to undo the land.
        app.on_key(key(KeyCode::Char('z')));
        let t = app.tutorial.as_ref().unwrap();
        assert!(t.finished, "undo should have completed the final step");
        assert_eq!(t.current, total_steps);

        let _ = std::fs::remove_dir_all(&root);
    }
}
