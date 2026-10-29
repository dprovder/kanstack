//! The recipe catalog: every recipe that can be named, rather than given as a path.
//!
//! A recipe's **name** is its path relative to the recipe directory it's in, minus `.md`, so
//! `.kanstack/recipes/bugfix/small.md` is `bugfix/small`. The **directories** are
//! `<repo>/.kanstack/recipes/`, then each of `KANSTACK_RECIPE_PATH`'s colon-separated entries
//! (relative ones are taken from the repository root), in that order ([`recipe_dirs`]). When
//! two directories hold the same name, the first one's wins.
//!
//! `serve` looks an item's recipe up with [`resolve`], and `recipes` lists [`scan`], both over
//! [`recipe_dirs`], so a name `recipes` lists is exactly one `serve` can start. kanstack's TUI
//! picker reimplements the scan (it can't depend on this crate, which sits above it); its
//! `src/app/recipe_picker.rs` must keep to the same rules.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::recipe;

/// How deep [`scan`] goes below a recipe directory: far deeper than anyone nests recipes, and
/// a stop for a symlink that loops back on its own directory.
const MAX_DEPTH: usize = 16;

/// The directories recipes are named from, in lookup order: `<repo>/.kanstack/recipes/`, then
/// `search_path` (`KANSTACK_RECIPE_PATH`'s value) split on `:`, each joined onto `repo`.
/// Blank entries are skipped. A directory that doesn't exist is still listed; it just holds
/// nothing.
pub fn recipe_dirs(repo: &Path, search_path: Option<&str>) -> Vec<PathBuf> {
    let mut dirs = vec![repo.join(".kanstack").join("recipes")];
    dirs.extend(search_path.unwrap_or_default().split(':').map(str::trim).filter(|d| !d.is_empty()).map(|d| repo.join(d)));
    dirs
}

/// Whether `name` can only mean a file: it ends in `.md`, is absolute, or starts with `./` or
/// has a `..` in it. Anything else is looked up by name first.
fn is_path(name: &str) -> bool {
    name.ends_with(".md") || Path::new(name).is_absolute() || name.starts_with("./") || name.split('/').any(|part| part == "..")
}

/// The file an item's `recipe` names. A path ([`is_path`]) is taken from `repo`. A name is
/// `<dir>/<name>.md` in the first of [`recipe_dirs`] that has it; a name with a `/` that's in
/// none of them is then tried as a path from `repo`, as a recipe file without `.md`.
pub fn resolve(repo: &Path, name: &str, search_path: Option<&str>) -> Result<PathBuf, String> {
    let name = name.trim();
    if is_path(name) {
        let path = repo.join(name);
        return if path.is_file() { Ok(path) } else { Err(format!("recipe file {} doesn't exist", path.display())) };
    }
    let dirs = recipe_dirs(repo, search_path);
    if let Some(path) = dirs.iter().map(|dir| dir.join(format!("{name}.md"))).find(|p| p.is_file()) {
        return Ok(path);
    }
    if name.contains('/') && repo.join(name).is_file() {
        return Ok(repo.join(name));
    }
    let searched = dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
    Err(format!("no recipe named \"{name}\" ({name}.md isn't in {searched})"))
}

/// One recipe in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub name: String,
    pub description: Option<String>,
    pub tags: Vec<String>,
    /// How many steps its front matter declares.
    pub steps: usize,
    pub path: PathBuf,
    /// Why `check` would reject it, if it would. It's listed anyway, so a broken recipe is
    /// visible rather than missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Every recipe in [`recipe_dirs`], sorted by name, each name once (the first directory's).
pub fn scan(repo: &Path, search_path: Option<&str>) -> Vec<Entry> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
    for dir in recipe_dirs(repo, search_path) {
        let mut files = Vec::new();
        walk(&dir, "", 0, &mut files);
        for (name, path) in files {
            found.entry(name).or_insert(path);
        }
    }
    found.into_iter().map(|(name, path)| entry(name, path)).collect()
}

/// Collects every `*.md` file under `dir` as `(name, path)`, where `name` is `prefix` plus
/// its path below `dir` without `.md`. Symlinks are followed, as [`resolve`]'s lookup does.
fn walk(dir: &Path, prefix: &str, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
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

fn entry(name: String, path: PathBuf) -> Entry {
    let mut entry = Entry { name, description: None, tags: Vec::new(), steps: 0, path, error: None };
    let source = match fs::read_to_string(&entry.path) {
        Ok(source) => source,
        Err(e) => {
            entry.error = Some(format!("could not read it: {e}"));
            return entry;
        }
    };
    match recipe::parse_meta(&source) {
        Ok(meta) => {
            (entry.description, entry.tags, entry.steps) = (meta.description, meta.tags, meta.steps);
            entry.error = recipe::parse(&source).err().map(|e| e.to_string());
        }
        Err(e) => entry.error = Some(e.to_string()),
    }
    entry
}

/// `recipes [--json]`: the catalog for the repository at `repo`, printed to `out`.
pub fn command(repo: &Path, search_path: Option<&str>, json: bool, out: &mut dyn std::io::Write) -> std::io::Result<()> {
    let entries = scan(repo, search_path);
    if json {
        return writeln!(out, "{}", serde_json::json!({ "schema": 1, "recipes": entries }));
    }
    if entries.is_empty() {
        let dirs = recipe_dirs(repo, search_path).iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ");
        return writeln!(out, "no recipes (looked in {dirs})");
    }
    let width = entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
    for e in &entries {
        let steps = if e.steps == 1 { "1 step".to_string() } else { format!("{} steps", e.steps) };
        let mut line = format!("{:<width$}  {:<8}  {}", e.name, steps, e.description.as_deref().unwrap_or("-"));
        if !e.tags.is_empty() {
            line.push_str(&format!("  [{}]", e.tags.join(", ")));
        }
        if let Some(error) = &e.error {
            line.push_str(&format!("  ✗ {error}"));
        }
        writeln!(out, "{}", line.trim_end())?;
        writeln!(out, "{:<width$}  {}", "", e.path.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE: &str = "---\nversion: 1\ndescription: Fix a small bug\ntags: [bug, quick]\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nctx\n";

    fn write(path: &Path, source: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    #[test]
    fn resolves_paths_names_and_the_search_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        write(&repo.join(".kanstack/recipes/land.md"), "");
        write(&repo.join(".kanstack/recipes/bugfix/small.md"), "");
        write(&repo.join("more/land.md"), "");
        write(&repo.join("more/only.md"), "");
        write(&repo.join("elsewhere/odd.md"), "");
        write(&repo.join("elsewhere/bare"), "");

        assert_eq!(resolve(repo, "land", None).unwrap(), repo.join(".kanstack/recipes/land.md"));
        assert_eq!(resolve(repo, " land ", Some("")).unwrap(), repo.join(".kanstack/recipes/land.md"));
        assert_eq!(resolve(repo, "land", Some("elsewhere:more")).unwrap(), repo.join(".kanstack/recipes/land.md"), "the repo's dir comes first");
        assert_eq!(resolve(repo, "only", Some("elsewhere:more")).unwrap(), repo.join("more/only.md"));
        assert_eq!(resolve(repo, "bugfix/small", None).unwrap(), repo.join(".kanstack/recipes/bugfix/small.md"), "a nested name");
        assert_eq!(resolve(repo, "elsewhere/odd.md", None).unwrap(), repo.join("elsewhere/odd.md"));
        assert_eq!(resolve(repo, "elsewhere/bare", None).unwrap(), repo.join("elsewhere/bare"), "a path without .md still works");
        assert_eq!(resolve(repo, "odd.md", Some("elsewhere")).unwrap_err(), format!("recipe file {} doesn't exist", repo.join("odd.md").display()));
        let abs = repo.join("more/land.md");
        assert_eq!(resolve(repo, abs.to_str().unwrap(), None).unwrap(), abs);

        let err = resolve(repo, "odd", Some("more")).unwrap_err();
        assert!(err.contains("no recipe named \"odd\"") && err.contains("more"), "{err}");
    }

    #[test]
    fn scan_names_recipes_by_path_and_the_first_directory_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        write(&repo.join(".kanstack/recipes/land.md"), RECIPE);
        write(&repo.join(".kanstack/recipes/bugfix/small.md"), RECIPE);
        write(&repo.join(".kanstack/recipes/notes.txt"), "not a recipe");
        write(&repo.join("shared/land.md"), "---\nversion: 1\ndescription: shadowed\nsteps: {}\n---\n");
        write(&repo.join("shared/broken.md"), "---\nversion: 1\ndescription: Half done\nsteps:\n  a:\n    agent: codex\n---\n");

        let entries = scan(repo, Some("shared:missing"));
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["broken", "bugfix/small", "land"]);

        let land = &entries[2];
        assert_eq!(land.path, repo.join(".kanstack/recipes/land.md"), "not the shared one");
        assert_eq!((land.description.as_deref(), land.tags.clone(), land.steps, land.error.clone()), (Some("Fix a small bug"), vec!["bug".to_string(), "quick".to_string()], 1, None));

        let broken = &entries[0];
        assert_eq!((broken.description.as_deref(), broken.steps), (Some("Half done"), 1), "listed though invalid");
        assert!(broken.error.as_deref().unwrap().contains("missing `prompt`"), "{broken:?}");

        // Every name the catalog lists resolves to the same file.
        for e in &entries {
            assert_eq!(resolve(repo, &e.name, Some("shared:missing")).unwrap(), e.path, "{}", e.name);
        }
    }

    #[test]
    fn the_listing_prints_names_steps_descriptions_and_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let mut out = Vec::new();
        command(repo, None, false, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("no recipes (looked in "));

        write(&repo.join(".kanstack/recipes/bugfix/small.md"), RECIPE);
        let mut out = Vec::new();
        command(repo, None, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("bugfix/small  1 step    Fix a small bug  [bug, quick]\n"), "{text}");

        let mut out = Vec::new();
        command(repo, None, true, &mut out).unwrap();
        let doc: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(doc["schema"], 1);
        let r = &doc["recipes"][0];
        assert_eq!((r["name"].as_str(), r["description"].as_str(), r["steps"].as_u64()), (Some("bugfix/small"), Some("Fix a small bug"), Some(1)));
        assert_eq!(r["tags"], serde_json::json!(["bug", "quick"]));
        assert!(r["path"].as_str().unwrap().ends_with(".kanstack/recipes/bugfix/small.md"));
        assert!(r.get("error").is_none());
    }
}
