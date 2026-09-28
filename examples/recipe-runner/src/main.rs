use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use kanstack_recipe_runner::kanstack::KanstackClient;
use kanstack_recipe_runner::recipe;
use kanstack_recipe_runner::run::{run_recipe, RunOptions};

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
    if command != "run" && command != "check" {
        eprintln!("kanstack-recipe: unknown command `{command}`");
        print_usage();
        return ExitCode::from(2);
    }

    let mut recipe_path: Option<PathBuf> = None;
    let mut kanstack_bin = PathBuf::from("kanstack");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--kanstack-bin" => match args.next() {
                Some(v) => kanstack_bin = PathBuf::from(v),
                None => {
                    eprintln!("kanstack-recipe: --kanstack-bin needs a path");
                    return ExitCode::from(2);
                }
            },
            other if recipe_path.is_none() => recipe_path = Some(PathBuf::from(other)),
            other => {
                eprintln!("kanstack-recipe: unexpected argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }

    let Some(recipe_path) = recipe_path else {
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

    let client = KanstackClient::new(kanstack_bin);
    let outcome = run_recipe(&parsed, &client, &RunOptions::default());
    if outcome.failed {
        eprintln!("✗ recipe failed");
        ExitCode::FAILURE
    } else {
        println!("✓ recipe complete");
        ExitCode::SUCCESS
    }
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  kanstack-recipe run <recipe.md> [--kanstack-bin <path>]");
    eprintln!("  kanstack-recipe check <recipe.md> [--kanstack-bin <path>]");
}
