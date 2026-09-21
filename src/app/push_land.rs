use super::*;
use ratatui::crossterm::event::KeyCode as K;

/// A `but land` call running on a background thread, started by `confirm_land`.
///
/// Landing pushes directly to the target — sometimes a real remote — so it can take
/// long enough (and fail in ways worth explaining) that running it on the main thread
/// would freeze the whole UI with no feedback for however long the network takes.
pub struct PendingLand {
    pub title: String,
    /// How many branches this land call covers — 1 for an ordinary land, more on a
    /// stacked lane (see `confirm_land`). Lets the completion notification say "landed 3
    /// branches" instead of just "landed", which would undersell what actually happened.
    pub branch_count: usize,
    // `pub(crate)` rather than private: `ui`'s tests build one directly to exercise the
    // spinner overlay without a real `but` and thread.
    pub(crate) rx: mpsc::Receiver<Result<WorkspaceStatus>>,
    /// Advanced once per poll so the UI can animate a spinner without its own clock.
    pub spinner: usize,
}

impl App {

    /// The branch behind the selected lane, which is what commit and push act on.
    /// `None` for the backlog lane, which has no branch of its own.
    pub(super) fn selected_branch(&self) -> Option<(String, String)> {
        let col = self.board.columns.get(self.col)?;
        if col.kind != ColumnKind::Stack {
            return None;
        }
        Some((col.drop_target.clone(), col.title.clone()))
    }

    /// Asks `but` what a push would do and shows it, rather than pushing straight away.
    ///
    /// This is not ceremony: `but push` force-pushes by default, and with GitHub native
    /// stacking a stack push can transiently retarget open PRs. Seeing the destination and
    /// whether a force is involved before confirming is the thing that makes one keystroke
    /// safe to bind at all.
    pub(super) fn begin_push(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to push — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.push_preview(&branch) {
            Ok(preview) => {
                if preview.branches.iter().all(|b| b.unpushed_commits == 0) {
                    self.notify(format!("{title} has nothing to push"), Notice::Info);
                    return;
                }
                self.push_preview = Some(preview);
                self.mode = Mode::PushConfirm;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    pub(super) fn confirm_push(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;
        self.push_preview = None;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.push(&branch) {
            Ok(()) => {
                // `push` ignores `--status-after`, so the board has to be reloaded here.
                self.refresh_quietly();
                self.notify(format!("pushed {title}"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Asks `but` whether landing the selected lane onto the target would be clean, and
    /// shows that before landing.
    ///
    /// `but land` has no dry-run of its own, so this is `branch show --check` instead —
    /// read-only, so nothing moves until the preview has been seen and confirmed. Landing
    /// rewrites the target branch, which is a shared one, so the same "see it before it
    /// happens" rule as push applies here, more so — landing pushes directly to a real
    /// remote's target, bypassing any pull-request review entirely.
    pub(super) fn begin_land(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.notify(
                "pick a lane to land — the backlog has no branch",
                Notice::Info,
            );
            return;
        };
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.merge_check(&branch) {
            Ok(check) => {
                if check.commits_ahead == 0 {
                    self.notify(format!("{title} has nothing to land"), Notice::Info);
                    return;
                }
                self.land_check = Some(check);
                self.mode = Mode::LandConfirm;
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Starts landing on a background thread rather than blocking the UI for however long
    /// the push takes — `but land` can reach a real remote, and a frozen terminal with no
    /// feedback looks indistinguishable from a hang. `poll_land` picks up the result.
    ///
    /// `but land` refuses a non-base branch outright when the lane is a stack ("it is
    /// stacked on top of ... other segment(s)"), with no flag or stack-id argument that
    /// lands the whole thing for you — so on a stacked lane this lands every branch base
    /// first, one `but land` call each, rather than surfacing that refusal to press `M`
    /// again N times with the CLI open to work out the right order.
    pub(super) fn confirm_land(&mut self) {
        let Some((branch, title)) = self.selected_branch() else {
            self.mode = Mode::Normal;
            return;
        };
        self.land_check = None;

        let Some(but) = self.but.clone() else {
            self.mode = Mode::Normal;
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        // `sections` is tip-first (see `Column::sections`'s own doc comment); landing needs
        // the reverse, base first, or `but land` refuses the first call.
        let branches: Vec<String> = match self.board.columns.get(self.col) {
            Some(col) if col.sections.len() > 1 => {
                col.sections.iter().rev().map(|s| s.name.clone()).collect()
            }
            _ => vec![branch],
        };

        let branch_count = branches.len();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(but.land_stack(&branches));
        });
        self.landing = Some(PendingLand {
            title,
            branch_count,
            rx,
            spinner: 0,
        });
        self.mode = Mode::Landing;
    }

    /// Non-blocking check on a `but land` running on a background thread. Call this every
    /// tick of the event loop — it costs nothing when nothing is landing, and is what
    /// advances the spinner and applies the result the moment it arrives.
    pub fn poll_land(&mut self) {
        let Some(pending) = self.landing.as_mut() else {
            return;
        };
        pending.spinner = pending.spinner.wrapping_add(1);
        match pending.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.landing = None;
                self.mode = Mode::Normal;
                self.notify("landing thread vanished unexpectedly", Notice::Error);
            }
            Ok(Ok(status)) => {
                let title = pending.title.clone();
                let branch_count = pending.branch_count;
                self.landing = None;
                self.mode = Mode::Normal;
                if let Some(but) = self.but.clone() {
                    self.board = Self::board_from(&but, &mut self.commit_stats, &status);
                }
                self.clamp();
                let message = if branch_count > 1 {
                    format!("landed {branch_count} branches onto the target, base first")
                } else {
                    format!("landed {title} onto the target")
                };
                // `but land` is documented to reconcile the remaining lanes onto the result,
                // but 0.22.0 was observed to advance the target and leave the workspace on
                // its old base when a land emptied it. Still behind straight after a land is
                // that state, so go on to the usual preview-then-confirm rebase rather than
                // sending the user to the CLI to `but pull`.
                if self.board.behind > 0 {
                    self.begin_rebase();
                    if self.mode == Mode::RebaseConfirm {
                        self.notify(
                            format!(
                                "{message} — the workspace is still {} behind, review the rebase",
                                self.board.behind
                            ),
                            Notice::Success,
                        );
                        self.check_tutorial_advance();
                        return;
                    }
                }
                self.notify(message, Notice::Success);
            }
            Ok(Err(e)) => {
                self.landing = None;
                self.mode = Mode::Normal;
                self.notify(format!("{e}"), Notice::Error);
            }
        }
        self.check_tutorial_advance();
    }
}

impl App {
    pub(super) fn handle_key_push_confirm(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Enter | K::Char('y') => self.confirm_push(),
            K::Esc | K::Char('n') | K::Char('q') => {
                self.mode = Mode::Normal;
                self.push_preview = None;
                self.notify("push cancelled", Notice::Info);
            }
            _ => {}
        }
    }

    pub(super) fn handle_key_land_confirm(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Enter | K::Char('y') => self.confirm_land(),
            K::Esc | K::Char('n') | K::Char('q') => {
                self.mode = Mode::Normal;
                self.land_check = None;
                self.notify("land cancelled", Notice::Info);
            }
            _ => {}
        }
    }

    /// Nothing to do here but wait: `poll_land` (driven by the event loop, not by a
    /// key) is what moves this out of `Landing`. There is deliberately no cancel —
    /// once `but land` has started it may already be pushing to a real remote, and a
    /// key that looked like it stopped that would be a lie.
    pub(super) fn handle_key_landing(&mut self, _key: ratatui::crossterm::event::KeyEvent) {
    }
}
