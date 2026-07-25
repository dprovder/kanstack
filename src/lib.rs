//! kanstack — an unofficial kanban-style terminal UI for the GitButler CLI.
//!
//! Not affiliated with or endorsed by GitButler Inc. This crate links no GitButler code;
//! it spawns the `but` binary you installed and reads its documented JSON output. That
//! boundary is deliberate — see README for why.

pub mod app;
pub mod board;
pub mod but;
pub mod cmux;
pub mod diff;
pub mod model;
pub mod snapshot;
pub mod theme;
pub mod tutorial;
pub mod ui;
pub mod watch;
