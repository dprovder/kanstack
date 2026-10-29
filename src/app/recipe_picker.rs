//! The recipe picker: a filterable list of the recipe catalog, opened with `r` on an issue (to
//! set its recipe through `Tracker::set_recipe`) or from the draft modal's recipe field, so
//! nobody has to type a recipe name from memory.
//!
//! **The catalog rules are `kanstack_recipe::catalog`'s, reimplemented.** kanstack can't depend
//! on kanstack-recipe (the orchestrator sits above it), so the directory scan and the
//! front-matter read live here too, and must keep matching that module: a recipe's name is
//! its path below its recipe directory minus `.md`; the directories are
//! `<repo>/.kanstack/recipes/`, then `KANSTACK_RECIPE_PATH`'s colon-separated entries (relative
//! ones from the repository root); the first directory holding a name wins; symlinks are
//! followed, at most [`MAX_DEPTH`] directories down. A name listed here is one `serve` can start.
//!
//! The catalog is read once, when the picker opens. Filtering and drawing only ever look at
//! that copy, so a few hundred recipes cost no file IO per keystroke or frame.

use super::*;
use kanstack_tracker::ItemId;
use ratatui::crossterm::event::{KeyCode as K, KeyModifiers};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// How deep the scan goes below a recipe directory. Mirrors `kanstack_recipe::catalog`.
const MAX_DEPTH: usize = 16;

/// One recipe the picker lists: what `kanstack-recipe recipes` lists, minus what the picker
/// doesn't show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeEntry {
    pub name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
}

/// What a pick is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerTarget {
    /// An existing item: Enter sets its recipe in the tracker.
    Item { id: ItemId, title: String, current: Option<String> },
    /// The draft modal's recipe field: Enter fills it in and goes back to the draft.
    Draft,
}

pub struct RecipePicker {
    pub target: PickerTarget,
    pub entries: Vec<RecipeEntry>,
    /// The directories that were searched, for the empty-catalog message.
    pub dirs: Vec<PathBuf>,
    pub query: TextInput,
    /// Indices into `entries` that match `query`, best first.
    pub matches: Vec<usize>,
    /// Cursor into `matches`.
    pub sel: usize,
}

impl RecipePicker {
    fn new(target: PickerTarget, entries: Vec<RecipeEntry>, dirs: Vec<PathBuf>) -> RecipePicker {
        let mut picker = RecipePicker { target, entries, dirs, query: TextInput::default(), matches: Vec::new(), sel: 0 };
        picker.refilter();
        // Start on the recipe the item already has, so ⏎ straight away changes nothing.
        let current = match &picker.target {
            PickerTarget::Item { current, .. } => current.clone(),
            PickerTarget::Draft => None,
        };
        if let Some(i) = current.and_then(|name| picker.matches.iter().position(|&m| picker.entries[m].name == name)) {
            picker.sel = i;
        }
        picker
    }

    /// The entry under the cursor.
    pub fn selected(&self) -> Option<&RecipeEntry> {
        self.matches.get(self.sel).map(|&i| &self.entries[i])
    }

    fn refilter(&mut self) {
        let query = self.query.as_str();
        let mut scored: Vec<(i64, usize)> =
            self.entries.iter().enumerate().filter_map(|(i, e)| Some((match_score(query, e)?, i))).collect();
        // Best first; ties in name order, which is the catalog's order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.sel = 0;
    }

    fn move_sel(&mut self, delta: isize) {
        if self.matches.is_empty() {
            return;
        }
        let last = self.matches.len() as isize - 1;
        self.sel = (self.sel as isize + delta).clamp(0, last) as usize;
    }
}

/// How well `query` matches `entry`, or `None` if it doesn't. Each whitespace-separated term
/// has to match one of the entry's fields as a case-insensitive subsequence; the entry scores
/// the sum of each term's best field. The name counts most, then its folder and leaf on their
/// own, then tags, then the description. An empty query matches everything equally.
pub fn match_score(query: &str, entry: &RecipeEntry) -> Option<i64> {
    let (folder, leaf) = entry.name.rsplit_once('/').unwrap_or(("", &entry.name));
    let mut fields: Vec<(&str, i64)> = vec![(&entry.name, 4), (leaf, 4), (folder, 3)];
    fields.extend(entry.tags.iter().map(|t| (t.as_str(), 2)));
    if let Some(description) = &entry.description {
        fields.push((description, 1));
    }
    let mut total = 0;
    for term in query.split_whitespace() {
        total += fields.iter().filter_map(|&(text, weight)| Some(subsequence_score(term, text)? * weight)).max()?;
    }
    Some(total)
}

/// A case-insensitive subsequence match of `term` in `text`, scored: each matched character
/// counts, more when it follows the previous match directly or starts a word, and a match
/// that starts late or spreads out counts less. `None` when `term` isn't a subsequence.
pub fn subsequence_score(term: &str, text: &str) -> Option<i64> {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let mut score = 0i64;
    let mut at = 0usize;
    let mut prev: Option<usize> = None;
    for qc in term.chars().flat_map(char::to_lowercase) {
        let found = at + text[at..].iter().position(|&c| c == qc)?;
        score += 1;
        if prev.is_some_and(|p| p + 1 == found) {
            score += 5;
        }
        if found == 0 || !text[found - 1].is_alphanumeric() {
            score += 3;
        }
        score -= match prev {
            Some(p) => (found - p - 1).min(5) as i64,
            None => (found as i64).min(5),
        };
        prev = Some(found);
        at = found + 1;
    }
    Some(score.max(1))
}

/// The directories recipes are named from, in lookup order. Must match
/// `kanstack_recipe::catalog::recipe_dirs`.
pub fn recipe_dirs(repo: &Path, search_path: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![repo.join(".kanstack").join("recipes")];
    dirs.extend(search_path.unwrap_or_default().split(':').map(str::trim).filter(|d| !d.is_empty()).map(|d| repo.join(d)));
    dirs
}

/// Every recipe in [`recipe_dirs`], sorted by name, each name once (the first directory's).
/// Must match `kanstack_recipe::catalog::scan`.
pub fn scan_catalog(repo: &Path, search_path: Option<&str>) -> Vec<RecipeEntry> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in recipe_dirs(repo, search_path) {
        let mut files = Vec::new();
        walk(&dir, "", 0, &mut files);
        for (name, path) in files {
            found.entry(name).or_insert(path);
        }
    }
    found.into_iter().map(|(name, path)| read_entry(name, &path)).collect()
}

fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Some(file_name) = entry.file_name().to_str().map(String::from) else { continue };
        let path = entry.path();
        if path.is_dir() {
            if depth < MAX_DEPTH {
                walk(&path, &format!("{prefix}{file_name}/"), depth + 1, out);
            }
        } else if let Some(stem) = file_name.strip_suffix(".md").filter(|stem| !stem.is_empty()) {
            if path.is_file() {
                out.push((format!("{prefix}{stem}"), path));
            }
        }
    }
}

/// `name`'s entry, with `description:` and `tags:` from the file's front matter. Reads only
/// as far as the closing `---`; a file that can't be read still lists, by name alone.
fn read_entry(name: String, path: &Path) -> RecipeEntry {
    let mut front = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let mut lines = BufReader::new(file).lines().map_while(Result::ok);
        if lines.next().is_some_and(|l| l.trim_start_matches('\u{feff}').trim_end() == "---") {
            front = lines.take_while(|l| l.trim_end() != "---").collect();
        }
    }
    let (description, tags) = parse_front_matter(&front);
    RecipeEntry { name, description, tags }
}

/// The top-level `description:` and `tags:` of a recipe's YAML front matter, read by hand
/// (kanstack has no YAML parser): a plain or quoted one-line description, or a folded `>`
/// block; tags as a flow list (`[a, b]`) or a block list of `- a` lines. Anything fancier
/// reads as absent, which only costs the picker a line of text.
pub fn parse_front_matter(lines: &[String]) -> (Option<String>, Vec<String>) {
    let (mut description, mut tags) = (None, Vec::new());
    let top_level = |line: &str| !line.starts_with([' ', '\t']) && !line.trim().is_empty();
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        i += 1;
        if !top_level(line) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = strip_comment(value).trim();
        let block: Vec<&str> = lines[i..].iter().take_while(|l| !top_level(l)).map(|l| l.trim()).collect();
        match key.trim() {
            "description" if value.starts_with(['>', '|']) => {
                let text = block.iter().filter(|l| !l.is_empty()).copied().collect::<Vec<_>>().join(" ");
                description = Some(text).filter(|t| !t.is_empty());
            }
            "description" => description = Some(unquote(value).to_string()).filter(|t| !t.is_empty()),
            "tags" if value.starts_with('[') => {
                let inner = value.trim_start_matches('[').trim_end_matches(']');
                tags = inner.split(',').map(|t| unquote(t.trim()).to_string()).filter(|t| !t.is_empty()).collect();
            }
            "tags" => {
                tags = block
                    .iter()
                    .filter_map(|l| l.strip_prefix('-'))
                    .map(|t| unquote(strip_comment(t).trim()).to_string())
                    .filter(|t| !t.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    (description, tags)
}

fn unquote(value: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = value.strip_prefix(q).and_then(|v| v.strip_suffix(q)) {
            return inner;
        }
    }
    value
}

/// `value` up to a ` #` comment, unless it's quoted.
fn strip_comment(value: &str) -> &str {
    if value.trim_start().starts_with(['"', '\'']) {
        return value;
    }
    value.find(" #").map_or(value, |at| &value[..at])
}

/// The repository root the catalog is read for: the nearest directory at or above `dir`
/// holding `.git`, as `kanstack-recipe` uses `git rev-parse --show-toplevel`.
fn repo_root(dir: &Path) -> PathBuf {
    dir.ancestors().find(|d| d.join(".git").exists()).unwrap_or(dir).to_path_buf()
}

impl App {
    /// Opens the picker for `target`, reading the catalog once.
    fn open_recipe_picker(&mut self, target: PickerTarget) {
        let Some(repo) = self.repo.as_deref().map(repo_root) else {
            self.notify("snapshot is read-only", Notice::Info);
            return;
        };
        let search_path = std::env::var("KANSTACK_RECIPE_PATH").ok();
        let entries = scan_catalog(&repo, search_path.as_deref());
        let dirs = recipe_dirs(&repo, search_path.as_deref());
        self.recipe_picker = Some(RecipePicker::new(target, entries, dirs));
        self.mode = Mode::RecipePicker;
    }

    /// `r` in the issues view: the picker for the selected item.
    pub(super) fn pick_recipe_for_selected_issue(&mut self) {
        let Some(item) = self.issues.selected() else {
            self.notify("pick an issue to set its recipe", Notice::Info);
            return;
        };
        let target = PickerTarget::Item { id: item.id.clone(), title: item.title.clone(), current: item.recipe.clone() };
        self.open_recipe_picker(target);
    }

    /// From the draft modal's recipe field: the picker, seeded with whatever was typed.
    pub(super) fn pick_recipe_for_draft(&mut self, seed: Option<char>) {
        self.open_recipe_picker(PickerTarget::Draft);
        if let (Some(picker), Some(c)) = (self.recipe_picker.as_mut(), seed) {
            picker.query.set(c.to_string());
            picker.refilter();
        }
    }

    fn close_recipe_picker(&mut self) {
        let target = self.recipe_picker.take().map(|p| p.target);
        self.mode = if target == Some(PickerTarget::Draft) { Mode::IssueDraft } else { Mode::Normal };
    }

    pub(super) fn handle_key_recipe_picker(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        let Some(picker) = self.recipe_picker.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            K::Esc => self.close_recipe_picker(),
            K::Enter => self.confirm_recipe_pick(),
            K::Up => picker.move_sel(-1),
            K::Down => picker.move_sel(1),
            K::Char('p') if ctrl => picker.move_sel(-1),
            K::Char('n') if ctrl => picker.move_sel(1),
            K::PageUp => picker.move_sel(-10),
            K::PageDown => picker.move_sel(10),
            _ => {
                let before = picker.query.as_str().to_string();
                if picker.query.handle_key(key) && picker.query.as_str() != before {
                    picker.refilter();
                }
            }
        }
    }

    pub(super) fn paste_recipe_picker(&mut self, text: &str) {
        if let Some(picker) = self.recipe_picker.as_mut() {
            picker.query.paste(text, false);
            picker.refilter();
        }
    }

    fn confirm_recipe_pick(&mut self) {
        let Some(picker) = self.recipe_picker.as_ref() else { return };
        let Some(name) = picker.selected().map(|e| e.name.clone()) else {
            self.notify("nothing matches — esc to cancel", Notice::Info);
            return;
        };
        match picker.target.clone() {
            PickerTarget::Draft => {
                self.issues.draft_recipe.set(name);
                self.close_recipe_picker();
            }
            PickerTarget::Item { id, current, .. } => {
                if current.as_deref() == Some(name.as_str()) {
                    self.close_recipe_picker();
                    self.notify(format!("{} already runs {name}", item_label(&id)), Notice::Info);
                    return;
                }
                let msg = format!("setting {}'s recipe to {name}…", item_label(&id));
                if self.begin_set_recipe(id, name) {
                    self.close_recipe_picker();
                    self.notify(msg, Notice::Info);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanstack_tracker::{Draft, ItemState, LocalTracker, Tracker};
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use std::time::{Duration, Instant};

    fn entry(name: &str, description: Option<&str>, tags: &[&str]) -> RecipeEntry {
        RecipeEntry { name: name.into(), description: description.map(String::from), tags: tags.iter().map(|t| t.to_string()).collect() }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kanstack-picker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    #[test]
    fn subsequence_scores_prefer_tight_word_start_matches() {
        assert_eq!(subsequence_score("xyz", "bugfix"), None);
        assert!(subsequence_score("BUG", "bugfix/small").is_some(), "case-insensitive");
        let tight = subsequence_score("small", "bugfix/small").unwrap();
        let spread = subsequence_score("small", "s-m-a-l-l-ish").unwrap();
        assert!(tight > spread, "{tight} vs {spread}");
        let start = subsequence_score("rev", "review").unwrap();
        let middle = subsequence_score("rev", "prereview").unwrap();
        assert!(start > middle, "{start} vs {middle}");
    }

    #[test]
    fn matching_covers_name_folder_description_and_tags_best_first() {
        let entries = [
            entry("bugfix/small", Some("Fix a small bug"), &["quick"]),
            entry("feature/large", Some("A bigger change, reviewed twice"), &["review"]),
            entry("docs", None, &[]),
        ];
        let rank = |q: &str| {
            let mut scored: Vec<(i64, &str)> =
                entries.iter().filter_map(|e| Some((match_score(q, e)?, e.name.as_str()))).collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
            scored.into_iter().map(|(_, n)| n).collect::<Vec<_>>()
        };
        assert_eq!(rank(""), ["bugfix/small", "docs", "feature/large"], "everything, by name");
        assert_eq!(rank("bugfix"), ["bugfix/small"], "the folder");
        assert_eq!(rank("quick"), ["bugfix/small"], "a tag");
        assert_eq!(rank("twice"), ["feature/large"], "the description");
        assert_eq!(rank("feat rev"), ["feature/large"], "every term has to match somewhere");
        assert_eq!(rank("doc")[0], "docs", "a name match beats a scattered one: {:?}", rank("doc"));
    }

    #[test]
    fn front_matter_reads_flow_and_block_lists_and_folded_descriptions() {
        let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
        assert_eq!(
            parse_front_matter(&lines("version: 1\ndescription: \"Fix: a bug\"\ntags: [bug, 'quick']\nsteps:\n  a:\n    description: not this\n")),
            (Some("Fix: a bug".to_string()), vec!["bug".to_string(), "quick".to_string()])
        );
        assert_eq!(
            parse_front_matter(&lines("description: >\n  Two words\ntags:\n  - one  # note\n  - two\nsteps: {}\n")),
            (Some("Two words".to_string()), vec!["one".to_string(), "two".to_string()])
        );
        assert_eq!(parse_front_matter(&lines("version: 1\n")), (None, Vec::new()));
    }

    /// The same rules as `kanstack_recipe::catalog::scan`: nested names, the repo's directory
    /// before `KANSTACK_RECIPE_PATH`'s, first one wins, only `.md` files.
    #[test]
    fn scan_matches_kanstack_recipes_catalog_rules() {
        let repo = scratch("scan");
        write(&repo.join(".kanstack/recipes/land.md"), "---\nversion: 1\ndescription: Land it\n---\n");
        write(&repo.join(".kanstack/recipes/bugfix/small.md"), "---\ntags: [bug]\n---\n");
        write(&repo.join(".kanstack/recipes/README.txt"), "no");
        write(&repo.join("shared/land.md"), "---\ndescription: shadowed\n---\n");
        write(&repo.join("shared/extra.md"), "no front matter at all");

        let entries = scan_catalog(&repo, Some("shared:missing"));
        assert_eq!(
            entries,
            [
                entry("bugfix/small", None, &["bug"]),
                entry("extra", None, &[]),
                entry("land", Some("Land it"), &[]),
            ]
        );
        assert_eq!(recipe_dirs(&repo, None), [repo.join(".kanstack/recipes")]);
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn key(app: &mut App, code: KeyCode) {
        app.handle_key(KeyEvent::from(code));
    }

    fn typed(app: &mut App, text: &str) {
        text.chars().for_each(|c| key(app, KeyCode::Char(c)));
    }

    fn board() -> Board {
        Board::from_status(&crate::but::parse_status(include_str!("../../tests/fixtures/status.json")).unwrap())
    }

    fn wait_until(app: &mut App, done: impl Fn(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out waiting on the tracker");
            std::thread::sleep(Duration::from_millis(5));
            app.poll_issues();
        }
    }

    /// An app on the issues view, over a local tracker and a repo with `recipes` recipes.
    fn app_with(tag: &str, recipes: usize) -> (App, PathBuf) {
        let repo = scratch(tag);
        for n in 0..recipes {
            write(&repo.join(format!(".kanstack/recipes/group{}/recipe-{n:03}.md", n % 7)), &format!("---\ndescription: Recipe number {n}\ntags: [t{n}]\n---\n"));
        }
        let items = repo.join("items");
        let tracker = LocalTracker::in_dir(&items);
        tracker.create(&Draft { title: "Fix it".into(), ..Draft::default() }).unwrap();
        let mut app = App::from_board(board()).with_tracker(move || Ok(Box::new(LocalTracker::in_dir(items.clone()))));
        app.repo = Some(repo.clone());
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        wait_until(&mut app, |app| app.issues.loaded);
        (app, repo)
    }

    #[test]
    fn r_picks_a_recipe_for_the_selected_issue_from_hundreds() {
        let (mut app, repo) = app_with("issue", 300);
        key(&mut app, KeyCode::Char('r'));
        assert_eq!(app.mode, Mode::RecipePicker);
        let picker = app.recipe_picker.as_ref().unwrap();
        assert_eq!((picker.entries.len(), picker.matches.len()), (300, 300));

        // The catalog was read on open: files removed now don't change what's listed.
        std::fs::remove_dir_all(repo.join(".kanstack")).unwrap();
        typed(&mut app, "recipe-123");
        let picker = app.recipe_picker.as_ref().unwrap();
        assert_eq!(picker.selected().map(|e| e.name.as_str()), Some("group4/recipe-123"), "the exact name first");
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.recipe_picker.is_none());

        wait_until(&mut app, |app| !app.issues.writing());
        wait_until(&mut app, |app| app.issues.items.first().is_some_and(|i| i.recipe.is_some()));
        let item = &app.issues.items[0];
        assert_eq!(item.recipe.as_deref(), Some("group4/recipe-123"));
        assert!(app.message.as_ref().is_some_and(|(m, _)| m.contains("group4/recipe-123")), "{:?}", app.message);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn esc_cancels_and_no_match_sets_nothing() {
        let (mut app, repo) = app_with("cancel", 3);
        key(&mut app, KeyCode::Char('r'));
        typed(&mut app, "zzz");
        assert!(app.recipe_picker.as_ref().unwrap().matches.is_empty());
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::RecipePicker, "nothing to pick");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
        assert!(!app.issues.writing());
        assert_eq!(app.issues.items[0].recipe, None);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn the_draft_modals_recipe_field_opens_the_picker() {
        let (mut app, repo) = app_with("draft", 5);
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(app.mode, Mode::IssueDraft);
        typed(&mut app, "New one");
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.issues.draft_row, DraftRow::Recipe);
        // Typing on the recipe row opens the picker with the letter as its query.
        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.mode, Mode::RecipePicker);
        assert_eq!(app.recipe_picker.as_ref().unwrap().query.as_str(), "3");
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.mode, Mode::IssueDraft, "back to the draft");
        assert_eq!(app.issues.draft_recipe.as_str(), "group3/recipe-003");

        // Esc from the picker leaves the field alone; backspace clears it.
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.mode, Mode::RecipePicker);
        key(&mut app, KeyCode::Esc);
        assert_eq!((app.mode, app.issues.draft_recipe.as_str()), (Mode::IssueDraft, "group3/recipe-003"));
        key(&mut app, KeyCode::Backspace);
        assert!(app.issues.draft_recipe.is_empty());

        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        wait_until(&mut app, |app| app.issues.items.len() == 2);
        assert_eq!(app.issues.items[1].recipe.as_deref(), Some("group0/recipe-000"));
        assert_eq!(app.issues.items[1].state, ItemState::Backlog);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
