//! kanstack — an unofficial kanban-style terminal UI for the GitButler CLI.
//!
//! Not affiliated with or endorsed by GitButler Inc. This crate links no GitButler code;
//! it spawns the `but` binary you installed and reads its documented JSON output. That
//! boundary is deliberate — see README for why.

pub mod app;
pub mod board;
pub mod cli;
pub mod but;
pub mod cmux;
pub mod config;
pub mod diff;
pub mod harness;
pub mod harness_launch;
pub mod hit;
pub mod mux;
pub mod model;
pub mod orca;
pub mod pane_status;
pub mod setup;
pub mod snapshot;
pub mod splitter;
pub mod text_input;
pub mod theme;
pub mod tmux;
pub mod tutorial;
pub mod ui;
pub mod watch;
pub mod workstream;

/// Serializes tests, across `cmux.rs`/`tmux.rs`/`orca.rs`/`splitter.rs`, that mutate
/// process-wide env vars each backend's `discover` reads (`PATH`, `KANSTACK_CMUX_BIN`,
/// `KANSTACK_TMUX_BIN`, `KANSTACK_ORCA_BIN`, `TMUX_PANE`, `ORCA_TERMINAL_HANDLE`,
/// `KANSTACK_SPLIT_BACKEND`). `cargo test` runs tests concurrently on separate threads by
/// default, and those vars overlap across all four files' own `with_env` helpers, so a
/// lock scoped to just one file's tests isn't enough —
/// two tests in *different* files can still interleave and clobber each other's value
/// mid-test.
#[cfg(test)]
pub(crate) static SPLIT_BACKEND_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
