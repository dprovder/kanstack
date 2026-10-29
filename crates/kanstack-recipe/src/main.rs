use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use kanstack_recipe::kanstack::KanstackClient;
use kanstack_recipe::lock::OrchestratorLock;
use kanstack_recipe::recipe;
use kanstack_recipe::resume::{resume, unfinished_runs};
use kanstack_recipe::run::{start_run, RunOptions};
use kanstack_recipe::state::{recipe_hash, RunState, RunStore};

/// Every flag any command takes, parsed once. Each command rejects the ones it doesn't use.
struct Args {
    positional: Option<String>,
    kanstack_bin: Option<PathBuf>,
    force: bool,
    json: bool,
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let command = match args.next() {
        Some(c) => c,
        None => {
            print_usage();
            return ExitCode::from(2);
        }
    };

    if command == "-h" || command == "--help" {
        print_usage();
        return ExitCode::SUCCESS;
    }
    // `items` and `serve` have flags of their own, so each parses the rest itself.
    if command == "items" {
        return kanstack_recipe::items::command(&args.collect::<Vec<_>>());
    }
    if command == "serve" {
        return kanstack_recipe::serve::command(&args.collect::<Vec<_>>());
    }
    if !["run", "check", "resume", "runs"].contains(&command.as_str()) {
        eprintln!("kanstack-recipe: unknown command `{command}`");
        print_usage();
        return ExitCode::from(2);
    }

    let mut parsed = Args {
        positional: None,
        kanstack_bin: None,
        force: false,
        json: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kanstack-bin" if command != "runs" => match args.next() {
                Some(v) => parsed.kanstack_bin = Some(PathBuf::from(v)),
                None => {
                    eprintln!("kanstack-recipe: --kanstack-bin needs a path");
                    return ExitCode::from(2);
                }
            },
            "--force" if command == "resume" => parsed.force = true,
            "--json" if command == "runs" => parsed.json = true,
            other if parsed.positional.is_none() && command != "runs" && !other.starts_with("--") => {
                parsed.positional = Some(other.to_string())
            }
            other => {
                eprintln!("kanstack-recipe: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }

    let client = KanstackClient::new(parsed.kanstack_bin.clone().unwrap_or_else(|| PathBuf::from("kanstack")));
    match command.as_str() {
        "run" | "check" => run_or_check(&command, &parsed, &client),
        "resume" => resume_runs(&parsed, &client),
        _ => list_runs(&parsed),
    }
}

fn run_or_check(command: &str, args: &Args, client: &KanstackClient) -> ExitCode {
    let Some(recipe_path) = args.positional.as_deref().map(PathBuf::from) else {
        eprintln!("kanstack-recipe: missing <recipe.md> path");
        print_usage();
        return ExitCode::from(2);
    };

    let source = match std::fs::read_to_string(&recipe_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "kanstack-recipe: could not read {}: {e}",
                recipe_path.display()
            );
            return ExitCode::from(2);
        }
    };

    let parsed = match recipe::parse(&source) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    println!("✓ recipe valid");

    if command == "check" {
        return ExitCode::SUCCESS;
    }

    let Some((store, _lock)) = store_and_lock() else {
        return ExitCode::FAILURE;
    };
    let workdir = env::current_dir().unwrap_or_default();
    // Absolute, so a resume from another directory still finds it.
    let recipe_path = recipe_path.canonicalize().unwrap_or(recipe_path);
    let id = store.new_run_id(&recipe_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
    let mut run = RunState::new(id, &parsed, &recipe_path, recipe_hash(&source), workdir);
    println!("→ run {}", run.id);

    let outcome = start_run(&parsed, client, &RunOptions::default(), &store, &mut run);
    if outcome.failed {
        eprintln!("✗ recipe failed (run {})", run.id);
        ExitCode::FAILURE
    } else {
        println!("✓ recipe complete");
        ExitCode::SUCCESS
    }
}

/// `resume [<run-id>]`: that run, or every unfinished one, one after another. A lane keeps
/// working in its pane while its run waits its turn, so nothing is lost by the wait — its
/// finish is only noticed later.
fn resume_runs(args: &Args, client: &KanstackClient) -> ExitCode {
    let Some((store, _lock)) = store_and_lock() else {
        return ExitCode::FAILURE;
    };
    let ids = match &args.positional {
        Some(id) => vec![id.clone()],
        None => match unfinished_runs(&store) {
            Ok(ids) => ids,
            Err(e) => {
                eprintln!("kanstack-recipe: could not list runs in {}: {e}", store.root().display());
                return ExitCode::FAILURE;
            }
        },
    };
    if ids.is_empty() {
        println!("✓ no unfinished runs");
        return ExitCode::SUCCESS;
    }

    let mut failed = false;
    for id in &ids {
        println!("↻ run {id}");
        match resume(&store, client, &RunOptions::default(), id, args.force) {
            Ok(outcome) if outcome.failed => {
                eprintln!("✗ run {id} failed");
                failed = true;
            }
            Ok(_) => println!("✓ run {id} complete"),
            Err(e) => {
                eprintln!("✗ run {id}: {e}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `runs [--json]`: every run and its steps' statuses. Read-only, so it takes no lock —
/// snapshots are replaced by rename, so a read never sees a half-written one.
fn list_runs(args: &Args) -> ExitCode {
    let Some(store) = store() else {
        return ExitCode::FAILURE;
    };
    let ids = match store.run_ids() {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("kanstack-recipe: could not list runs in {}: {e}", store.root().display());
            return ExitCode::FAILURE;
        }
    };
    let mut runs = Vec::new();
    for id in &ids {
        match store.load(id) {
            Ok(run) => runs.push(run),
            Err(e) => eprintln!("! skipping run {id}: {e}"),
        }
    }

    if args.json {
        let json = serde_json::json!({ "schema": 1, "runs": runs });
        println!("{json}");
        return ExitCode::SUCCESS;
    }
    if runs.is_empty() {
        println!("no runs");
    }
    for run in &runs {
        println!("{}  {}  {}", run.id, run.status.as_str(), run.recipe);
        for (step_id, rec) in &run.steps {
            println!(
                "  {:<15} {:<10} attempt {}{}{}",
                step_id,
                rec.status.as_str(),
                rec.attempt,
                rec.waiting_on.map(|w| format!("  waiting on {}", w.waiting_label())).unwrap_or_default(),
                rec.last_error.as_deref().map(|e| format!("  {e}")).unwrap_or_default()
            );
        }
    }
    ExitCode::SUCCESS
}

fn store() -> Option<RunStore> {
    let store = RunStore::for_repo(&env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf()));
    if store.is_none() {
        eprintln!("kanstack-recipe: no state directory to keep runs in (set KANSTACK_STATE_PATH or HOME)");
    }
    store
}

fn store_and_lock() -> Option<(RunStore, OrchestratorLock)> {
    let store = store()?;
    match OrchestratorLock::try_acquire(&store) {
        Ok(lock) => Some((store, lock)),
        Err(e) => {
            eprintln!("kanstack-recipe: {e}");
            None
        }
    }
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  kanstack-recipe run <recipe.md> [--kanstack-bin <path>]");
    eprintln!("  kanstack-recipe check <recipe.md> [--kanstack-bin <path>]");
    eprintln!("  kanstack-recipe resume [<run-id>] [--force] [--kanstack-bin <path>]");
    eprintln!("  kanstack-recipe runs [--json]");
    eprintln!("  kanstack-recipe items <add|list|show|promote|propose> ... [--json]  (items --help for details)");
    eprintln!("  kanstack-recipe serve [--max <N>] [--once] [--json] [--kanstack-bin <path>]");
}
