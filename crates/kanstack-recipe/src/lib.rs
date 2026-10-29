//! `kanstack-recipe`: the orchestrator that runs a
//! Markdown+YAML recipe DAG by driving `kanstack`'s public CLI/JSON contract exclusively.
//! See the crate's README for the recipe format and design choices.

pub mod aggregate;
pub mod catalog;
pub mod graph;
pub mod items;
pub mod kanstack;
pub mod land;
pub mod lock;
pub mod recipe;
pub mod resume;
pub mod run;
pub mod serve;
pub mod state;
