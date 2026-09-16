use super::*;
use ratatui::crossterm::event::KeyCode as K;

/// Wall-clock now, in epoch milliseconds, for rendering branch ages.
///
/// Saturates to 0 rather than panicking if the system clock is set before 1970 — a broken
/// clock should cost an age column, not the whole drawer.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The drawer's on-demand detail view for whichever branch was highlighted when `⏎` was
/// pressed — the same `but branch show --check` the land-preview already runs, aimed at an
/// unapplied branch instead. Its presence is what distinguishes the drawer's two
/// sub-states, list and detail, both still under `Mode::Branches`: there's no separate
/// `Mode` for it, the same way `Mode::Diff` doesn't get a second variant for "full width".
pub struct BranchPreview {
    pub name: String,
    pub check: crate::model::MergeCheck,
    /// Line offset into the rendered preview, for `j`/`k` — mirrors `DiffView`'s own
    /// scroll rather than a cursor, since there's no per-line action to land a cursor on.
    pub scroll: u16,
    /// Carried over from the drawer row's own [`UnappliedBranch::stale`] rather than
    /// recomputed — the list has already done the age/merge-check math, and this is the
    /// same branch, so there's nothing new to derive here.
    pub stale: bool,
}

impl BranchPreview {
    pub(super) fn scroll_by(&mut self, delta: i32) {
        self.scroll = (i32::from(self.scroll) + delta).clamp(0, i32::from(u16::MAX)) as u16;
    }
}

impl App {

    // ---- unapplied branches: the drawer, apply, and unapply ----

    /// Opens the drawer, fetching the branch list, or closes it if it is already open.
    ///
    /// The fetch is synchronous, unlike `land`'s background thread: `but branch list`
    /// without `--review` is local work only — a merge check and a rev walk per branch,
    /// measured at ~40ms — so it lands well inside a frame. `--review` is what would make
    /// this a network call, and it is deliberately not passed (see `But::branch_list`).
    pub(super) fn toggle_branch_drawer(&mut self) {
        if self.mode == Mode::Branches {
            self.mode = Mode::Normal;
            return;
        }
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.branch_list(false) {
            Ok(list) => {
                self.unapplied = Unapplied::from_list(&list, now_ms());
                // Keep the cursor in range across a refetch that shrank the list, the same
                // way `clamp` does for the board.
                self.branch_sel = self
                    .branch_sel
                    .min(self.unapplied.branches.len().saturating_sub(1));
                self.branch_preview = None;
                self.mode = Mode::Branches;
                if self.unapplied.is_empty() {
                    self.notify("every branch is already applied", Notice::Info);
                }
            }
            Err(e) => self.notify(format!("could not list branches: {e}"), Notice::Error),
        }
    }

    /// Re-reads the branch list in place, without touching the mode.
    ///
    /// Called after an apply or unapply, both of which move a branch from one side of the
    /// list to the other. Failing here is a notice rather than a mode change: the mutation
    /// itself already succeeded, so a stale drawer is a cosmetic problem, not a wrong one.
    pub(super) fn refresh_branch_list(&mut self) {
        let Some(but) = &self.but else { return };
        match but.branch_list(false) {
            Ok(list) => {
                self.unapplied = Unapplied::from_list(&list, now_ms());
                self.branch_sel = self
                    .branch_sel
                    .min(self.unapplied.branches.len().saturating_sub(1));
            }
            Err(e) => self.notify(format!("could not list branches: {e}"), Notice::Error),
        }
    }

    /// Re-fetches with `--all`, past the default 20-most-recent-plus-active truncation —
    /// the drawer's escape hatch for when what you're after is further back than that.
    /// Reachable only while the list actually is truncated (see the `Mode::Branches` key
    /// handling), so there's no way to pay this heavier fetch by accident.
    pub(super) fn load_all_branches(&mut self) {
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.branch_list(true) {
            Ok(list) => {
                self.unapplied = Unapplied::from_list(&list, now_ms());
                self.branch_sel = self
                    .branch_sel
                    .min(self.unapplied.branches.len().saturating_sub(1));
                self.notify(
                    format!("loaded all {} branches", self.unapplied.branches.len()),
                    Notice::Success,
                );
            }
            Err(e) => self.notify(format!("could not list branches: {e}"), Notice::Error),
        }
    }

    pub fn selected_unapplied(&self) -> Option<&crate::board::UnappliedBranch> {
        self.unapplied.branches.get(self.branch_sel)
    }

    /// Opens the drawer's detail view for the highlighted branch: its own commits, and —
    /// the thing the list's bare ●/✗ dot can't say — exactly which files would conflict
    /// and against which upstream commits. Reuses `But::merge_check`, the same `but branch
    /// show --check` call the land-preview already makes, just aimed at an unapplied
    /// branch instead of the current lane.
    ///
    /// A synchronous subprocess call, deliberately gated behind `⏎` rather than fired as
    /// the cursor moves — the same reasoning that keeps `--review` out of the list fetch:
    /// paying a round trip per row you glance past would make browsing feel laggy for
    /// detail most presses don't need.
    pub(super) fn open_branch_preview(&mut self) {
        let Some(branch) = self.selected_unapplied() else {
            return;
        };
        let name = branch.name.clone();
        let stale = branch.stale;
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.merge_check(&name) {
            Ok(check) => {
                self.branch_preview = Some(BranchPreview { name, check, scroll: 0, stale });
            }
            Err(e) => self.notify(format!("could not preview {name}: {e}"), Notice::Error),
        }
    }

    /// Applies the highlighted branch, bringing it in as a new lane.
    ///
    /// Applies even when `mergesCleanly` is false rather than refusing: a conflicted apply
    /// is a legitimate thing to want — it is how you find out what conflicts — and `but`
    /// leaves the result recoverable. The row says so beforehand and the notice says so
    /// afterwards, which is the honest version of a guardrail that would otherwise just
    /// block a supported operation.
    pub(super) fn apply_selected_branch(&mut self) {
        let Some(branch) = self.selected_unapplied().cloned() else {
            return;
        };
        // Whether this came from the list or from the detail view open on this same
        // branch, the preview is either about to be gone (applied) or stale (the attempt
        // failed and the list is what's shown again) — either way, drop it.
        self.branch_preview = None;
        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.apply(&branch.name) {
            Ok(()) => {
                let name = branch.name.clone();
                self.refresh_quietly();
                self.refresh_branch_list();
                // Land the cursor on what was just applied, so the lane you asked for is
                // the one selected when the drawer closes — otherwise applying a branch
                // leaves you looking at whichever lane you happened to be on before.
                if let Some(i) = self
                    .board
                    .columns
                    .iter()
                    .position(|c| c.branch_name.as_deref() == Some(name.as_str()))
                {
                    self.col = i;
                    self.card = 0;
                }
                // Not an unconditional assignment: the refresh above can discover a
                // workspace-wide block and switch to `Blocked`, and that must win — it
                // means the board behind this drawer no longer describes the repository.
                if self.mode == Mode::Branches {
                    self.mode = Mode::Normal;
                }
                if branch.merges_cleanly == Some(false) {
                    self.notify(
                        format!("applied {name} — it reported conflicts, check the lane"),
                        Notice::Info,
                    );
                } else {
                    self.notify(format!("applied {name}"), Notice::Success);
                }
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Describes what unapplying the selected lane would do: the branch to name it by, and
    /// what leaves the workspace with it.
    ///
    /// The detail spells out the whole-stack behaviour because that is the one part a user
    /// cannot see coming — `but unapply` takes the stack containing the named branch, so on
    /// a lane of three stacked branches, pressing `U` on the tip removes all three. Naming
    /// them is the difference between a confirmation and a surprise.
    pub fn pending_unapply(&self) -> Option<(String, String)> {
        let col = self.board.columns.get(self.col)?;
        let name = col.branch_name.clone()?;
        // A lane with no commits anywhere in it is the one case where `a` will not bring it
        // back: `but branch list` omits empty branches, so the drawer never offers it. The
        // branch itself survives and `but apply` still works, but promising the drawer
        // would be a lie — and this is reachable, since `b` creates exactly such a lane.
        let commits: usize = col.sections.iter().map(|s| s.commits).sum();
        let detail = if commits == 0 {
            "it has no commits, so the drawer won't list it — `but apply` still brings it back"
                .to_string()
        } else {
            match col.sections.len() {
                0 | 1 => "leaves the workspace; re-apply it any time with `a`".to_string(),
                n => format!(
                    "unapplies the whole stack — all {n} branches: {}",
                    col.sections
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        };
        Some((name, detail))
    }

    pub(super) fn begin_unapply(&mut self) {
        if self.pending_unapply().is_none() {
            self.notify("the backlog is not a branch", Notice::Info);
            return;
        }
        self.mode = Mode::UnapplyConfirm;
    }

    /// Unapplies the selected lane.
    ///
    /// Confirmed rather than immediate, unlike `u`/send-to-backlog: this reaches into the
    /// working directory and takes the branch's changes off disk. Nothing is destroyed —
    /// the branch survives unapplied, which is exactly what the drawer then lists — but a
    /// working tree emptying out without warning is alarming enough to be worth a keypress.
    pub(super) fn confirm_unapply(&mut self) {
        let Some((name, _)) = self.pending_unapply() else {
            self.mode = Mode::Normal;
            return;
        };
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.unapply(&name) {
            Ok(()) => {
                self.refresh_quietly();
                // Only refetch a drawer that has already been populated: an unapply from
                // the board should not pay for a branch list nobody has asked to see.
                if !self.unapplied.is_empty() {
                    self.refresh_branch_list();
                }
                self.notify(format!("unapplied {name} — `a` brings it back"), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }

    /// Sends the selected card back to the backlog.
    ///
    /// One key for what the rub matrix treats as one operation against the `zz` target: a
    /// commit is uncommitted, a staged file is unstaged. Both preserve content — verified
    /// against the real CLI, which reports "Uncommitted <sha>" and leaves the worktree
    /// intact — so this needs no confirmation step.
    ///
    /// It is reachable by dragging a card onto the backlog lane too; this exists because
    /// that can mean three or four keypresses of `←` from a distant lane.
    pub(super) fn send_to_backlog(&mut self) {
        let Some(card) = self.selected_card().cloned() else {
            self.notify("nothing selected", Notice::Info);
            return;
        };
        if self
            .board
            .columns
            .get(self.col)
            .is_some_and(|c| c.kind == ColumnKind::Unassigned)
        {
            self.notify("already unassigned", Notice::Info);
            return;
        }
        // A Change card only ever lives in the unassigned lane itself under the current
        // model (nothing is "assigned" pre-commit anymore), so this is reached only for a
        // Commit card in practice — the guard above already refuses an already-unassigned
        // selection.
        if card.kind == CardKind::Change {
            self.notify("nothing to unassign", Notice::Info);
            return;
        }

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.uncommit(std::slice::from_ref(&card.rub_id)) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(format!("uncommitted {}", card.cli_id), Notice::Success);
            }
            Err(e) => self.notify(format!("{e}"), Notice::Error),
        }
    }
}

impl App {
    pub(super) fn handle_key_unapply_confirm(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        match key.code {
            K::Enter | K::Char('y') => self.confirm_unapply(),
            K::Esc | K::Char('n') | K::Char('q') => {
                self.mode = Mode::Normal;
                self.notify("unapply cancelled", Notice::Info);
            }
            _ => {}
        }
    }

    /// The drawer takes the keys while it is open, so j/k walk the branch list rather
    /// than the cards behind it. The board stays drawn and stays where it was — this is
    /// a focus change, not a screen change.
    pub(super) fn handle_key_branches(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        // The detail view is a second layer inside the same mode (see `BranchPreview`)
        // — its own small key set, mirroring `Mode::Diff`: j/k scroll the preview
        // instead of walking the list, esc/q/← back out one level rather than closing
        // the drawer outright, and `a` applies what's on screen either way. Just ←, not
        // both arrows the way the diff pane takes them — the drawer sits to the left of
        // the board, so → still reads as "into the board" rather than also meaning back.
        if self.branch_preview.is_some() {
            match key.code {
                K::Esc | K::Char('q') | K::Left => self.branch_preview = None,
                K::Down | K::Char('j') => {
                    if let Some(p) = &mut self.branch_preview {
                        p.scroll_by(1);
                    }
                }
                K::Up | K::Char('k') => {
                    if let Some(p) = &mut self.branch_preview {
                        p.scroll_by(-1);
                    }
                }
                K::Char('a') => self.apply_selected_branch(),
                _ => {}
            }
            return;
        }
        let n = self.unapplied.branches.len();
        match key.code {
            K::Esc | K::Char('q') | K::Left => self.mode = Mode::Normal,
            K::Down | K::Char('j') if n > 0 => {
                self.branch_sel = (self.branch_sel + 1).min(n - 1);
            }
            K::Up | K::Char('k') => self.branch_sel = self.branch_sel.saturating_sub(1),
            K::Char('g') if n > 0 => self.branch_sel = 0,
            K::Char('G') if n > 0 => self.branch_sel = n - 1,
            K::Enter if n > 0 => self.open_branch_preview(),
            K::Char('a') if n > 0 => self.apply_selected_branch(),
            K::Char('d') if n > 0 => self.begin_delete_unapplied(),
            K::Char('A') if self.unapplied.truncated => self.load_all_branches(),
            _ => {}
        }
    }
}
