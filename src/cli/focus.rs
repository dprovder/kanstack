//! `kanstack focus` — bring a pane to the front.

use std::io::Write;
use std::path::Path;

use anyhow::Result;

use super::exit::{tag, ErrorCode::*};
use super::{seeded_splitter, target_with_pane, PaneResult};
use crate::workstream::Registry;

pub(super) fn run(target: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    let registry = Registry::load(cwd)?;
    let splitter = seeded_splitter(&registry)?;
    let (w, pane) = target_with_pane(&registry, &target)?;
    let branch = w.branch_id.0.clone();
    let pane = pane.0.clone();
    splitter.focus(&branch).map_err(|e| tag(MultiplexerUnavailable, e))?;
    if json {
        writeln!(out, "{}", super::json_result("focus", branch, PaneResult { pane: Some(pane) })?)?;
    } else {
        writeln!(out, "focused {branch}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::test_support::*;
    use crate::cli::Command;

    #[test]
    fn focus_json_reports_the_pane_it_focused() {
        with_tmux_registry(
            "focus-json",
            r#"printf '%%3 2000000000\n'"#,
            vec![workstream("fix-login", Some("%3"), Some("claude"), None)],
            |repo| {
                let mut out = Vec::new();
                crate::cli::run(Command::Focus { target: "fix-login".into(), json: true }, repo, &mut out).unwrap();
                assert_eq!(
                    String::from_utf8(out).unwrap(),
                    "{\"schema\":1,\"ok\":true,\"command\":\"focus\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\"}}\n"
                );
            },
        );
    }
}
