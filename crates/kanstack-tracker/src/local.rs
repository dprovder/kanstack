//! The `local` tracker: one markdown file per item in kanstack's per-repo state directory.
//!
//! The default, and the fallback when no remote tracker is detected. It works offline and
//! needs no setup. Items live beside kanstack's workstream registry, never in the worktree:
//! any file written there shows up in `but status` as an uncommitted change and gets assigned
//! to a lane.
//!
//! Each item is `<id>.md`, flat `key: value` front matter followed by a markdown body:
//!
//! ```markdown
//! ---
//! id: 44
//! title: Land from the board
//! recipe: land-feature
//! state: backlog
//! ---
//!
//! Add an L key on the board that lands the selected lane after a merge check.
//!
//! ## Reports
//! ```
//!
//! The front matter is parsed by hand rather than as YAML: it's only ever flat lines, and a
//! person editing a file by hand shouldn't need YAML's quoting rules to put a colon in a title.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};

use crate::{Draft, Item, ItemFilter, ItemId, ItemState, Tracker};

/// The heading [`Tracker::comment`] appends reports under. Kept as the body's last section,
/// since reports are appended at the end of the file.
const REPORTS_HEADING: &str = "## Reports";

/// Items as markdown files in one directory. Ids are sequential integers.
///
/// Every write is atomic (a temp file renamed over the item), so a reader never sees half a
/// file. Atomic isn't enough for a read-modify-write, though: two steps of one run
/// commenting on the same item at once would each rewrite the file they read, and the last
/// rename would drop the other's report. So [`Tracker::set_state`], [`Tracker::set_recipe`] and [`Tracker::comment`]
/// hold the item's [`ItemLock`] from read to rename. Ids, the one thing two creators would
/// otherwise race on, are claimed exclusively instead (see [`LocalTracker::claim_id`]).
pub struct LocalTracker {
    dir: PathBuf,
}

/// An exclusive hold on one item, for the duration of a read-modify-write. Backed by
/// `flock(2)` on `<id>.lock` beside the item rather than the item itself, because the
/// rename at the end of every write replaces the item file, and a lock taken on the old one
/// would no longer exclude anyone who opened the new one. The kernel releases it when this
/// is dropped, so an error partway through still unlocks.
///
/// Lock files are never deleted: removing one while another process waits on it would let
/// a third open a fresh file and lock that, and two writers would hold "the" lock at once.
/// `flock` locks belong to the open file, not the process, so this excludes other threads
/// of the same process too.
struct ItemLock(#[allow(dead_code, reason = "held only for its Drop, which releases the flock")] File);

/// [`crate::Open`] for `local`: the items directory for `repo` in kanstack's state dir.
pub(crate) fn open(repo: &Path) -> Result<Box<dyn Tracker>> {
    Ok(Box::new(LocalTracker::for_repo(repo)?))
}

impl LocalTracker {
    /// The tracker for the repository at `repo`. Nothing is created until the first write.
    pub fn for_repo(repo: &Path) -> Result<LocalTracker> {
        let (dir, key) = repo_state(repo).ok_or_else(|| anyhow!("no home directory to keep items in"))?;
        Ok(LocalTracker::in_dir(dir.join(format!("items-{key:016x}"))))
    }

    /// A tracker keeping its items in `dir`, wherever that is. The test seam, so tests don't
    /// have to point `KANSTACK_STATE_PATH` somewhere and race each other doing it.
    pub fn in_dir(dir: impl Into<PathBuf>) -> LocalTracker {
        LocalTracker { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file for `id`, which must be a local id. Parsed as a number rather than joined as
    /// given, so an id can never name a path outside [`Self::dir`].
    fn path(&self, id: &ItemId) -> Result<PathBuf> {
        let n: u64 = id.0.parse().map_err(|_| anyhow!("{id:?} isn't a local item id (expected a number)"))?;
        Ok(self.dir.join(format!("{n}.md")))
    }

    /// The ids of every item on disk, ascending. Anything not named `<number>.md` (a temp
    /// file, a stray note) is ignored.
    fn ids(&self) -> Result<Vec<u64>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            if let Some(n) = name.to_str().and_then(|n| n.strip_suffix(".md")).and_then(|n| n.parse().ok()) {
                ids.push(n);
            }
        }
        ids.sort_unstable();
        Ok(ids)
    }

    /// Takes the next free id by creating its file with `create_new` (`O_EXCL`), so two
    /// processes creating at once can't both take the same one: the loser sees it exists and
    /// tries the next. The file is left empty for [`Self::write`] to fill.
    fn claim_id(&self) -> Result<u64> {
        let mut n = self.ids()?.last().map_or(1, |last| last + 1);
        loop {
            let path = self.dir.join(format!("{n}.md"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(n),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => n += 1,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
    }

    /// Blocks until no other writer holds `id`'s [`ItemLock`], then holds it. Checks the item
    /// exists first, so a mistyped id is an error rather than a stray lock file.
    fn lock(&self, id: &ItemId) -> Result<ItemLock> {
        let item = self.path(id)?;
        if !item.exists() {
            bail!("no item {id} ({})", item.display());
        }
        let path = item.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            // Only the fd matters, for `flock`; the content is never read or written.
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        // SAFETY: `flock` only reads the fd, which `file` keeps open for this call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error()).with_context(|| format!("locking {}", path.display()));
        }
        Ok(ItemLock(file))
    }

    /// Sets front-matter `key` to `value` under `id`'s lock, or removes it for `None`. The
    /// first `key:` line is replaced in place and any repeats dropped; a missing one goes
    /// after the `after:` line, or last. Every other line, and the body, is left as it was.
    fn set_front_matter(&self, id: &ItemId, key: &str, value: Option<&str>, after: Option<&str>) -> Result<()> {
        let _lock = self.lock(id)?;
        let contents = self.read(id)?;
        let (front, rest) = split(&contents).with_context(|| format!("item {id}"))?;
        let new_line = value.map(|value| format!("{key}: {value}"));
        let mut lines: Vec<String> = Vec::new();
        let mut placed = false;
        for line in front.lines() {
            if key_value(line).is_some_and(|(k, _)| k == key) {
                if !placed {
                    lines.extend(new_line.clone());
                    placed = true;
                }
                continue;
            }
            lines.push(line.to_string());
        }
        if let (false, Some(new_line)) = (placed, new_line) {
            let at = after
                .and_then(|after| lines.iter().position(|line| key_value(line).is_some_and(|(k, _)| k == after)))
                .map_or(lines.len(), |i| i + 1);
            lines.insert(at, new_line);
        }
        self.write(id, &format!("---\n{}\n---\n{rest}", lines.join("\n")))
    }

    fn read(&self, id: &ItemId) -> Result<String> {
        let path = self.path(id)?;
        std::fs::read_to_string(&path).with_context(|| format!("reading item {id} ({})", path.display()))
    }

    /// Replaces `id`'s file with `contents` via a temp file and a rename, so a reader sees
    /// either the old file or the new one, never a partial write. The temp name carries the
    /// pid and a per-process count, so no two writes ever share one, even two threads of one
    /// process writing the same item without its lock.
    fn write(&self, id: &ItemId, contents: &str) -> Result<()> {
        static WRITES: AtomicU64 = AtomicU64::new(0);
        let path = self.path(id)?;
        let n = WRITES.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!(".{id}.md.{}.{n}.tmp", std::process::id()));
        std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))
    }
}

impl Tracker for LocalTracker {
    fn id(&self) -> &'static str {
        "local"
    }

    /// Always usable: it needs nothing but a state directory.
    fn detect(&self, _repo: &Path) -> bool {
        true
    }

    fn list(&self, filter: &ItemFilter) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        for n in self.ids()? {
            let id = ItemId(n.to_string());
            let contents = match self.read(&id) {
                Ok(contents) => contents,
                // Gone since `ids` looked: another process removed it. Not an error to list.
                Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == ErrorKind::NotFound) => {
                    continue
                }
                Err(e) => return Err(e),
            };
            // Claimed by `create` but not written yet.
            if contents.is_empty() {
                continue;
            }
            let item = parse(id, &contents)?;
            if filter.matches(&item) {
                items.push(item);
            }
        }
        Ok(items)
    }

    fn get(&self, id: &ItemId) -> Result<Item> {
        parse(id.clone(), &self.read(id)?)
    }

    fn create(&self, draft: &Draft) -> Result<ItemId> {
        let title = one_line(&draft.title);
        if title.is_empty() {
            bail!("an item needs a title");
        }
        std::fs::create_dir_all(&self.dir).with_context(|| format!("creating {}", self.dir.display()))?;
        let id = ItemId(self.claim_id()?.to_string());
        let mut contents = format!("---\nid: {id}\ntitle: {title}\n");
        if let Some(recipe) = draft.recipe.as_deref().map(one_line).filter(|r| !r.is_empty()) {
            contents.push_str(&format!("recipe: {recipe}\n"));
        }
        contents.push_str(&format!("state: {}\n", draft.state));
        if let Some(from) = &draft.from {
            contents.push_str(&format!("from: {}\n", one_line(&from.0)));
        }
        contents.push_str("---\n");
        let body = draft.body.trim_end();
        if !body.is_empty() {
            contents.push_str(&format!("\n{body}\n"));
        }
        self.write(&id, &contents)?;
        Ok(id)
    }

    /// Rewrites the `state:` line and nothing else: the body, and any front-matter line
    /// kanstack doesn't know (one a person added by hand), come back byte for byte.
    fn set_state(&self, id: &ItemId, state: ItemState) -> Result<()> {
        self.set_front_matter(id, "state", Some(state.as_str()), None)
    }

    /// Rewrites the `recipe:` line, adding it under `title:` when there's none, or drops it
    /// for `None`. Like [`Self::set_state`], nothing else in the file changes.
    fn set_recipe(&self, id: &ItemId, recipe: Option<&str>) -> Result<()> {
        let recipe = recipe.map(one_line).filter(|r| !r.is_empty());
        self.set_front_matter(id, "recipe", recipe.as_deref(), Some("title"))
    }

    /// Appends `body` at the end of the file, under a `## Reports` heading that's added the
    /// first time. Reports are separated by a blank line only, so a caller that wants them
    /// told apart (by step, by time) puts that in `body` itself.
    fn comment(&self, id: &ItemId, body: &str) -> Result<()> {
        let _lock = self.lock(id)?;
        let mut contents = self.read(id)?;
        let (_, rest) = split(&contents).with_context(|| format!("item {id}"))?;
        let has_heading = rest.lines().any(|line| line.trim_end() == REPORTS_HEADING);
        let trimmed = contents.trim_end().len();
        contents.truncate(trimmed);
        contents.push_str("\n\n");
        if !has_heading {
            contents.push_str(REPORTS_HEADING);
            contents.push_str("\n\n");
        }
        contents.push_str(body.trim());
        contents.push('\n');
        self.write(id, &contents)
    }

    /// `None`: kanstack closes local items itself, and nothing outside it reads the link.
    fn link_text(&self, _id: &ItemId) -> Option<String> {
        None
    }
}

/// `text` with line breaks folded to spaces, so it fits on one front-matter line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `contents` split into its front matter (between the `---` lines, without them) and
/// everything after the closing `---` line.
fn split(contents: &str) -> Result<(&str, &str)> {
    let after_open = contents.strip_prefix("---\n").ok_or_else(|| anyhow!("doesn't start with a --- line"))?;
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches('\n') == "---" {
            let front = after_open[..offset].strip_suffix('\n').unwrap_or(&after_open[..offset]);
            return Ok((front, &after_open[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("its front matter has no closing --- line")
}

/// A `key: value` line, split at its first colon, so a title can hold colons of its own.
fn key_value(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    Some((key.trim(), value.trim()))
}

/// The item in `contents`, the text of `id`'s file. The id comes from the file name, not the
/// front matter, so a file copied by hand can't claim another item's id.
fn parse(id: ItemId, contents: &str) -> Result<Item> {
    let (front, rest) = split(contents).with_context(|| format!("item {id}"))?;
    let (mut title, mut state, mut recipe, mut from) = (None, None, None, None);
    for line in front.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = key_value(line).ok_or_else(|| anyhow!("item {id}: front-matter line {line:?} isn't key: value"))?;
        // A word-valued key takes its first word, so a trailing `# comment` written by hand
        // (as in the design doc's example) doesn't break it. A title is free text and doesn't.
        let word = value.split_whitespace().next().unwrap_or_default();
        match key {
            "title" => title = Some(value.to_string()),
            "state" => {
                state = Some(ItemState::parse(word).ok_or_else(|| {
                    let known = ItemState::ALL.map(ItemState::as_str).join(", ");
                    anyhow!("item {id}: unknown state {word:?} (expected one of {known})")
                })?)
            }
            "recipe" if !word.is_empty() => recipe = Some(word.to_string()),
            "from" if !word.is_empty() => from = Some(ItemId(word.to_string())),
            _ => {}
        }
    }
    Ok(Item {
        title: title.ok_or_else(|| anyhow!("item {id} has no title"))?,
        state: state.ok_or_else(|| anyhow!("item {id} has no state"))?,
        recipe,
        from,
        // The blank line that separates the front matter from the body isn't part of it.
        body: rest.strip_prefix('\n').unwrap_or(rest).to_string(),
        id,
    })
}

/// The state directory, and the key identifying `repo` within it.
///
/// A copy of `repo_state` in kanstack's `src/workstream.rs`, which this crate can't depend
/// on. It must stay in step with that function: if the two ever disagree on the directory
/// or the key, kanstack and the orchestrator read different item stores for the same repo.
fn repo_state(repo: &Path) -> Option<(PathBuf, u64)> {
    let dir = std::env::var_os("KANSTACK_STATE_PATH").map(PathBuf::from).or_else(|| {
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state")))
            .map(|base| base.join("kanstack"))
    })?;
    // Canonicalized, and lifted to the repository root, so `-C .`, `-C /abs/path` and a
    // command run from a subdirectory all land on the same items.
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let repo = repo.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&repo);
    Some((dir, fnv1a(repo.to_string_lossy().as_bytes())))
}

/// FNV-1a rather than `DefaultHasher`, whose output is explicitly unspecified across Rust
/// releases: an upgrade must not orphan every item on disk. Same as `src/workstream.rs`.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x100000001b3))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> (tempfile::TempDir, LocalTracker) {
        let dir = tempfile::tempdir().unwrap();
        let tracker = LocalTracker::in_dir(dir.path().join("items"));
        (dir, tracker)
    }

    fn draft(title: &str, state: ItemState) -> Draft {
        Draft { title: title.to_string(), state, ..Draft::default() }
    }

    #[test]
    fn create_get_and_list_round_trip() {
        let (_dir, tracker) = tracker();
        let draft = Draft {
            title: "Land from the board: the L key".to_string(),
            body: "Add an L key.\n\nAfter a merge check.\n".to_string(),
            recipe: Some("land-feature".to_string()),
            from: Some(ItemId("38".to_string())),
            state: ItemState::Proposed,
        };
        let id = tracker.create(&draft).unwrap();
        let item = tracker.get(&id).unwrap();
        assert_eq!(
            item,
            Item {
                id: id.clone(),
                title: draft.title.clone(),
                state: ItemState::Proposed,
                recipe: draft.recipe.clone(),
                from: draft.from.clone(),
                body: draft.body.clone(),
            }
        );
        assert_eq!(tracker.list(&ItemFilter::default()).unwrap(), vec![item]);
    }

    #[test]
    fn writes_the_documented_file_format() {
        let (_dir, tracker) = tracker();
        let id = tracker
            .create(&Draft { body: "Body.".to_string(), recipe: Some("r".to_string()), ..draft("T", ItemState::Backlog) })
            .unwrap();
        let on_disk = std::fs::read_to_string(tracker.dir().join("1.md")).unwrap();
        assert_eq!(id.0, "1");
        assert_eq!(on_disk, "---\nid: 1\ntitle: T\nrecipe: r\nstate: backlog\n---\n\nBody.\n");
    }

    #[test]
    fn ids_are_sequential_and_skip_ones_already_taken() {
        let (_dir, tracker) = tracker();
        let ids: Vec<_> = (0..3).map(|i| tracker.create(&draft(&format!("item {i}"), ItemState::Backlog)).unwrap().0).collect();
        assert_eq!(ids, ["1", "2", "3"]);
        // An id claimed by another process mid-create (the empty file it leaves) isn't reused,
        // and doesn't show up in a list half-written.
        std::fs::write(tracker.dir().join("4.md"), "").unwrap();
        assert_eq!(tracker.create(&draft("next", ItemState::Backlog)).unwrap().0, "5");
        assert_eq!(tracker.list(&ItemFilter::default()).unwrap().len(), 4);
    }

    #[test]
    fn list_filters_by_state() {
        let (_dir, tracker) = tracker();
        tracker.create(&draft("a", ItemState::Backlog)).unwrap();
        tracker.create(&draft("b", ItemState::Running)).unwrap();
        tracker.create(&draft("c", ItemState::Backlog)).unwrap();
        let titles = |state| {
            tracker.list(&ItemFilter::state(state)).unwrap().into_iter().map(|item| item.title).collect::<Vec<_>>()
        };
        assert_eq!(titles(ItemState::Backlog), ["a", "c"]);
        assert_eq!(titles(ItemState::Running), ["b"]);
        assert!(titles(ItemState::Done).is_empty());
    }

    #[test]
    fn set_state_rewrites_only_the_front_matter() {
        let (_dir, tracker) = tracker();
        let id = tracker.create(&Draft { body: "Keep me.\n---\nAnd me.".to_string(), ..draft("T", ItemState::Backlog) }).unwrap();
        // A front-matter line kanstack doesn't know, added by hand, survives too.
        let path = tracker.dir().join("1.md");
        let contents = std::fs::read_to_string(&path).unwrap().replacen("title: T\n", "title: T\nowner: me\n", 1);
        std::fs::write(&path, contents).unwrap();

        tracker.set_state(&id, ItemState::Running).unwrap();
        let item = tracker.get(&id).unwrap();
        assert_eq!(item.state, ItemState::Running);
        assert_eq!(item.body, "Keep me.\n---\nAnd me.\n");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "---\nid: 1\ntitle: T\nowner: me\nstate: running\n---\n\nKeep me.\n---\nAnd me.\n"
        );
    }

    #[test]
    fn set_recipe_rewrites_adds_and_clears_only_the_recipe_line() {
        let (_dir, tracker) = tracker();
        let id = tracker.create(&Draft { body: "Keep me.\nrecipe: not front matter".to_string(), ..draft("T", ItemState::Backlog) }).unwrap();
        let path = tracker.dir().join("1.md");
        let contents = std::fs::read_to_string(&path).unwrap().replacen("title: T\n", "title: T\nowner: me\n", 1);
        std::fs::write(&path, contents).unwrap();
        let file = |recipe: &str| format!("---\nid: 1\ntitle: T\n{recipe}owner: me\nstate: backlog\n---\n\nKeep me.\nrecipe: not front matter\n");

        tracker.set_recipe(&id, Some("bugfix/small")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file("recipe: bugfix/small\n"), "added under the title");
        assert_eq!(tracker.get(&id).unwrap().recipe.as_deref(), Some("bugfix/small"));

        tracker.set_recipe(&id, Some("land-feature")).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file("recipe: land-feature\n"), "replaced in place");

        tracker.set_recipe(&id, None).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), file(""));
        let item = tracker.get(&id).unwrap();
        assert_eq!((item.recipe, item.body.as_str()), (None, "Keep me.\nrecipe: not front matter\n"));

        assert!(tracker.set_recipe(&ItemId("9".to_string()), Some("x")).is_err(), "no such item");
    }

    /// The recipe is set under the item lock, so it doesn't lose a report written meanwhile.
    #[test]
    fn set_recipe_and_comments_at_once_both_land() {
        let (_dir, tracker) = tracker();
        let id = tracker.create(&draft("T", ItemState::Backlog)).unwrap();
        std::thread::scope(|scope| {
            let (t, i) = (&tracker, &id);
            scope.spawn(move || (0..20).for_each(|n| t.comment(i, &format!("report {n}")).unwrap()));
            scope.spawn(move || (0..20).for_each(|n| t.set_recipe(i, Some(&format!("r{n}"))).unwrap()));
        });
        let item = tracker.get(&id).unwrap();
        assert_eq!(item.recipe.as_deref(), Some("r19"));
        assert!((0..20).all(|n| item.body.contains(&format!("report {n}\n"))), "{}", item.body);
    }

    #[test]
    fn comment_appends_under_one_reports_heading() {
        let (_dir, tracker) = tracker();
        let id = tracker.create(&Draft { body: "The work.".to_string(), ..draft("T", ItemState::Running) }).unwrap();
        tracker.comment(&id, "First report.\n").unwrap();
        tracker.comment(&id, "Second report.").unwrap();
        tracker.set_state(&id, ItemState::Review).unwrap();
        assert_eq!(
            tracker.get(&id).unwrap().body,
            "The work.\n\n## Reports\n\nFirst report.\n\nSecond report.\n"
        );
    }

    /// Two writers hammering one item at once: without the item lock, each would rewrite the
    /// file it read and the last rename would drop the other's reports.
    #[test]
    fn concurrent_writers_to_one_item_both_land() {
        let (_dir, tracker) = tracker();
        let id = tracker.create(&draft("T", ItemState::Running)).unwrap();
        const EACH: usize = 25;
        std::thread::scope(|scope| {
            for writer in ["a", "b"] {
                let (tracker, id) = (&tracker, &id);
                scope.spawn(move || {
                    for i in 0..EACH {
                        tracker.comment(id, &format!("report {writer}{i}")).unwrap();
                        if writer == "b" {
                            tracker.set_state(id, ItemState::ALL[i % ItemState::ALL.len()]).unwrap();
                        }
                    }
                });
            }
        });
        tracker.set_state(&id, ItemState::Review).unwrap();
        let item = tracker.get(&id).unwrap();
        for writer in ["a", "b"] {
            for i in 0..EACH {
                let report = format!("report {writer}{i}\n");
                assert!(item.body.contains(&report), "lost {report:?} from:\n{}", item.body);
            }
        }
        assert_eq!(item.body.matches(REPORTS_HEADING).count(), 1);
        assert_eq!(item.state, ItemState::Review);
    }

    #[test]
    fn comment_on_an_item_with_no_body() {
        let (_dir, tracker) = tracker();
        let id = tracker.create(&draft("T", ItemState::Running)).unwrap();
        tracker.comment(&id, "Report.").unwrap();
        assert_eq!(tracker.get(&id).unwrap().body, "## Reports\n\nReport.\n");
    }

    #[test]
    fn reads_a_hand_written_file_with_comments_and_colons() {
        let (_dir, tracker) = tracker();
        std::fs::create_dir_all(tracker.dir()).unwrap();
        std::fs::write(
            tracker.dir().join("44.md"),
            "---\nid: 44\ntitle: Fix: the thing\nstate: backlog        # proposed | backlog\nfrom: 38   # source\n---\n\nBody\n",
        )
        .unwrap();
        let item = tracker.get(&ItemId("44".to_string())).unwrap();
        assert_eq!((item.title.as_str(), item.state, item.from), ("Fix: the thing", ItemState::Backlog, Some(ItemId("38".to_string()))));
    }

    #[test]
    fn rejects_ids_that_are_not_local() {
        let (_dir, tracker) = tracker();
        assert!(tracker.get(&ItemId("../workstreams".to_string())).is_err());
        assert!(tracker.set_state(&ItemId("LIN-1".to_string()), ItemState::Done).is_err());
        // A well-formed id with no item behind it is an error too, and leaves no lock file.
        tracker.create(&draft("T", ItemState::Backlog)).unwrap();
        assert!(tracker.comment(&ItemId("9".to_string()), "x").is_err());
        assert!(!tracker.dir().join("9.lock").exists());
    }

    #[test]
    fn listing_a_tracker_with_no_directory_yet_is_empty() {
        let (_dir, tracker) = tracker();
        assert!(tracker.list(&ItemFilter::default()).unwrap().is_empty());
        assert!(!tracker.dir().exists());
    }

    #[test]
    fn link_text_is_none() {
        let (_dir, tracker) = tracker();
        assert_eq!(tracker.link_text(&ItemId("1".to_string())), None);
    }
}
