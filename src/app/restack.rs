use super::*;

impl App {

    /// Picks up the whole lane, to stack it onto another one.
    pub(super) fn begin_restack(&mut self) {
        if self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.as_ref())
            .is_none()
        {
            self.notify("the backlog is not a branch to stack", Notice::Info);
            return;
        }
        if self.board.columns.iter().filter(|c| c.branch_name.is_some()).count() < 2 {
            self.notify("nothing to stack onto — there is only one lane", Notice::Info);
            return;
        }
        self.origin_col = self.col;
        self.mode = Mode::Restacking;
        // Land on a lane that is actually a valid target. Stepping one column blindly can
        // wrap onto the backlog, which has no branch and so can never accept a stack.
        if let Some(i) = self.next_stack_column(self.col) {
            self.col = i;
        }
    }

    /// Next column after `from` that is a branch lane other than the origin.
    pub(super) fn next_stack_column(&self, from: usize) -> Option<usize> {
        let n = self.board.columns.len();
        (1..=n)
            .map(|step| (from + step) % n)
            .find(|&i| i != self.origin_col && self.board.columns[i].branch_name.is_some())
    }

    /// As [`Self::next_stack_column`], walking the other way.
    pub(super) fn prev_stack_column(&self, from: usize) -> Option<usize> {
        let n = self.board.columns.len();
        (1..=n)
            .map(|step| (from + n - (step % n)) % n)
            .find(|&i| i != self.origin_col && self.board.columns[i].branch_name.is_some())
    }

    /// What `s` will do, named before it happens — this rewrites history.
    pub fn pending_restack(&self) -> Option<String> {
        if self.mode != Mode::Restacking {
            return None;
        }
        let source = self.board.columns.get(self.origin_col)?.branch_name.as_ref()?;
        let target = self.board.columns.get(self.col)?;
        match &target.branch_name {
            Some(t) if t != source => Some(format!("stack {source} onto {t}")),
            Some(_) => Some(format!("{source} — pick a different lane")),
            None => Some(format!("{source} — the backlog cannot hold a stack")),
        }
    }

    pub(super) fn confirm_restack(&mut self) {
        let source = self
            .board
            .columns
            .get(self.origin_col)
            .and_then(|c| c.branch_name.clone());
        let target = self
            .board
            .columns
            .get(self.col)
            .and_then(|c| c.branch_name.clone());
        let (Some(source), Some(target)) = (source, target) else {
            self.notify("pick a lane to stack onto", Notice::Info);
            return;
        };
        if source == target {
            self.notify("pick a different lane", Notice::Info);
            return;
        }
        self.mode = Mode::Normal;

        let Some(but) = &self.but else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        match but.restack_branch(&source, &target) {
            Ok(status) => {
                self.board = Self::board_from(but, &mut self.commit_stats, &status);
                self.clamp();
                self.notify(
                    format!("stacked {source} onto {target} — commits were rewritten"),
                    Notice::Success,
                );
                if let Some(i) = self
                    .board
                    .columns
                    .iter()
                    .position(|c| c.branch_name.as_deref() == Some(source.as_str()))
                {
                    self.col = i;
                    self.card = 0;
                }
            }
            Err(e) => self.notify(format!("{e:#}"), Notice::Error),
        }
    }
}
