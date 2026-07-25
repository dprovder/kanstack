//! Filesystem watching, so the board follows the repository.
//!
//! Mutations already return fresh state via `--status-after`, so the only thing that can
//! make the board stale is a change made outside this process: an editor save, a `but`
//! invocation in another terminal, the desktop app. Watching for those is what removes
//! the need for a manual refresh key.
//!
//! Two details matter for this to be usable rather than a CPU fire:
//!
//! 1. **Ignored paths are skipped.** A naive worktree watch turns one `cargo build` into
//!    thousands of events from `target/`. Paths excluded by `.gitignore` are dropped.
//! 2. **Events are coalesced.** A save often lands as several events, and a `but` command
//!    rewrites many refs at once; the watcher reports "something changed" at most once per
//!    debounce window rather than once per inotify event.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// How long to wait for a burst of events to settle before reporting a change.
const DEBOUNCE: Duration = Duration::from_millis(250);

pub struct Watcher {
    rx: Receiver<PathBuf>,
    /// Kept alive because dropping it stops the watch.
    _inner: RecommendedWatcher,
    ignore: ignore::gitignore::Gitignore,
    root: PathBuf,
    pending_since: Option<Instant>,
}

impl Watcher {
    /// Watches `root` (the worktree) and its `.git` directory.
    pub fn new(root: &Path) -> Result<Watcher> {
        let (tx, rx) = channel();
        let mut inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // Access events fire constantly and never change content.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            for path in event.paths {
                let _ = tx.send(path);
            }
        })
        .context("could not create a filesystem watcher")?;

        inner
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("could not watch {}", root.display()))?;

        // `.git` is usually inside the worktree and therefore already covered, but it is a
        // file (not a directory) for linked worktrees, and may live elsewhere entirely.
        let dot_git = root.join(".git");
        if dot_git.is_dir() && !dot_git.starts_with(root) {
            let _ = inner.watch(&dot_git, RecursiveMode::Recursive);
        }

        let (ignore, _) = ignore::gitignore::Gitignore::new(root.join(".gitignore"));

        Ok(Watcher {
            rx,
            _inner: inner,
            ignore,
            root: root.to_path_buf(),
            pending_since: None,
        })
    }

    /// True when a path we care about changed, at most once per debounce window.
    ///
    /// Call this each tick of the event loop; it never blocks.
    pub fn poll(&mut self) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(path) => {
                    if self.is_interesting(&path) {
                        self.pending_since.get_or_insert_with(Instant::now);
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        match self.pending_since {
            Some(since) if since.elapsed() >= DEBOUNCE => {
                self.pending_since = None;
                true
            }
            _ => false,
        }
    }

    fn is_interesting(&self, path: &Path) -> bool {
        is_interesting_path(&self.root, &self.ignore, path)
    }
}

/// Split out from `Watcher` so the filtering rules can be tested without a real watch.
fn is_interesting_path(
    root: &Path,
    ignore: &ignore::gitignore::Gitignore,
    path: &Path,
) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path);

    // Git's own churn: lock files and the object database change constantly during any
    // operation and never tell us anything the refs won't.
    let s = rel.to_string_lossy();
    if s.ends_with(".lock") || s.contains("/.lock") {
        return false;
    }
    if rel.starts_with(".git") {
        // Within .git, only refs, HEAD, and GitButler's own state are worth reacting to.
        return s.contains("refs")
            || s.contains("HEAD")
            || s.contains("gitbutler")
            || s.contains("index");
    }

    // Anything the repository ignores is not workspace state — this is what keeps
    // `target/` and `node_modules/` from drowning the watcher.
    let is_dir = path.is_dir();
    if ignore.matched_path_or_any_parents(rel, is_dir).is_ignore() {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignore_for(root: &Path, patterns: &[&str]) -> ignore::gitignore::Gitignore {
        let mut b = ignore::gitignore::GitignoreBuilder::new(root);
        for p in patterns {
            b.add_line(None, p).unwrap();
        }
        b.build().unwrap()
    }

    #[test]
    fn ignored_build_output_is_not_interesting() {
        let root = Path::new("/repo");
        let ig = ignore_for(root, &["/target", "node_modules"]);
        // The exact case that would otherwise make `cargo build` melt the watcher.
        assert!(!is_interesting_path(root, &ig, Path::new("/repo/target/debug/foo")));
        assert!(!is_interesting_path(
            root,
            &ig,
            Path::new("/repo/node_modules/x/index.js")
        ));
    }

    #[test]
    fn tracked_source_is_interesting() {
        let root = Path::new("/repo");
        let ig = ignore_for(root, &["/target"]);
        assert!(is_interesting_path(root, &ig, Path::new("/repo/src/main.rs")));
        assert!(is_interesting_path(root, &ig, Path::new("/repo/README.md")));
    }

    #[test]
    fn git_lock_churn_is_filtered_but_refs_are_not() {
        let root = Path::new("/repo");
        let ig = ignore_for(root, &[]);
        assert!(!is_interesting_path(
            root,
            &ig,
            Path::new("/repo/.git/index.lock")
        ));
        assert!(is_interesting_path(
            root,
            &ig,
            Path::new("/repo/.git/refs/heads/feat-x")
        ));
        assert!(is_interesting_path(root, &ig, Path::new("/repo/.git/HEAD")));
        // Object writes are pure noise; refs tell us what actually moved.
        assert!(!is_interesting_path(
            root,
            &ig,
            Path::new("/repo/.git/objects/ab/cdef")
        ));
    }
}
