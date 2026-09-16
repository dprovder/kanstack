use super::*;

impl But {

    /// Undoes the last operation, restoring the workspace's prior state — including
    /// uncommitted changes. Verified against 0.21.2: this always succeeds and prints
    /// nothing, silently doing nothing when there is no prior operation to undo, so the
    /// caller cannot tell a real undo from a no-op except by comparing the board.
    pub fn undo(&self) -> Result<WorkspaceStatus> {
        self.run(&["undo", "--json"])?;
        self.status()
    }

    /// Redoes the last undone operation. Same no-signal-on-no-op behaviour as `undo`.
    pub fn redo(&self) -> Result<WorkspaceStatus> {
        self.run(&["redo", "--json"])?;
        self.status()
    }
}
