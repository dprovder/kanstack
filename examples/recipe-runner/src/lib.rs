//! `kanstack-recipe-runner`: a small, disposable reference orchestrator that runs a
//! Markdown+YAML recipe DAG by driving `kanstack`'s public CLI/JSON contract exclusively.
//! See the crate's README for the recipe format and design choices.

pub mod graph;
pub mod kanstack;
pub mod recipe;
pub mod run;
