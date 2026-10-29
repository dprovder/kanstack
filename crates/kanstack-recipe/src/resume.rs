//! Resuming persisted runs by id: reload the run's snapshot and its recipe, refuse a recipe
//! that changed since the run started, then hand both to [`resume_run`].

use std::fs;
use std::io;
use std::path::PathBuf;

use crate::kanstack::KanstackClient;
use crate::recipe::{self, Recipe, RecipeError};
use crate::run::{resume_run, RunOptions, RunOutcome};
use crate::state::{recipe_hash, RunState, RunStatus, RunStore};

#[derive(Debug)]
pub enum ResumeError {
    /// The run's `state.json` couldn't be read, parsed or saved.
    State(io::Error),
    /// The run already ended; there's nothing to resume.
    Finished(RunStatus),
    ReadRecipe(PathBuf, io::Error),
    Recipe(RecipeError),
    /// The recipe file's hash no longer matches the one recorded when the run started.
    RecipeChanged(PathBuf),
}

impl std::fmt::Display for ResumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResumeError::State(e) => write!(f, "could not load run state: {e}"),
            ResumeError::Finished(status) => write!(f, "run already {}", status.as_str()),
            ResumeError::ReadRecipe(path, e) => write!(f, "could not read recipe {}: {e}", path.display()),
            ResumeError::Recipe(e) => write!(f, "{e}"),
            ResumeError::RecipeChanged(path) => write!(
                f,
                "recipe {} changed since the run started; pass --force to resume it anyway",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ResumeError {}

/// Resumes the run `run_id` in `store`. With `force`, a changed recipe is resumed anyway and
/// its new hash recorded, so the next resume doesn't need forcing again.
pub fn resume(store: &RunStore, client: &KanstackClient, opts: &RunOptions, run_id: &str, force: bool) -> Result<RunOutcome, ResumeError> {
    let (recipe, mut run) = load_for_resume(store, run_id, force)?;
    Ok(resume_run(&recipe, client, opts, store, &mut run))
}

/// Everything [`resume`] checks before it drives the run: the run `run_id` in `store`, still
/// unfinished, and its recipe, unchanged since it started (or, with `force`, changed and its
/// new hash saved). `serve` drives what this returns a tick at a time instead.
pub fn load_for_resume(store: &RunStore, run_id: &str, force: bool) -> Result<(Recipe, RunState), ResumeError> {
    let mut run = store.load(run_id).map_err(ResumeError::State)?;
    if run.status != RunStatus::Running {
        return Err(ResumeError::Finished(run.status));
    }

    let source = fs::read_to_string(&run.recipe_path).map_err(|e| ResumeError::ReadRecipe(run.recipe_path.clone(), e))?;
    let hash = recipe_hash(&source);
    if hash != run.recipe_hash {
        if !force {
            return Err(ResumeError::RecipeChanged(run.recipe_path.clone()));
        }
        eprintln!("! {}: recipe changed since the run started, resuming anyway (--force)", run.id);
        run.recipe_hash = hash;
    }
    let recipe = recipe::parse(&source).map_err(ResumeError::Recipe)?;
    store.save(&run).map_err(ResumeError::State)?;
    Ok((recipe, run))
}

/// The ids of every run not known to be finished, oldest first. A run whose state can't be
/// read is included, so resuming it reports why instead of skipping it silently.
pub fn unfinished_runs(store: &RunStore) -> io::Result<Vec<String>> {
    Ok(store
        .run_ids()?
        .into_iter()
        .filter(|id| store.load(id).map_or(true, |run| run.status == RunStatus::Running))
        .collect())
}
