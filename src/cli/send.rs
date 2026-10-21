//! `kanstack send` — type a message into a pane and submit it.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::exit::{tag, ErrorCode::*};
use super::{seeded_splitter, target_with_pane};
use crate::mux::delivery::Delivery;
use crate::workstream::Registry;

/// `send`'s result: the pane it typed into, and whether delivery was confirmed. `confirmed` is
/// only false when nothing could check the send (see `crate::mux::delivery`), and then
/// `unconfirmed_reason` says why. A send that could be checked and failed is an error
/// (`delivery_failed`), never a result.
#[derive(Debug, Serialize)]
struct SendResult {
    pane: String,
    confirmed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unconfirmed_reason: Option<String>,
}

pub(super) fn run(target: String, text: String, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    let registry = Registry::load(cwd)?;
    let splitter = seeded_splitter(&registry)?;
    let (w, pane) = target_with_pane(&registry, &target)?;
    let branch = w.branch_id.0.clone();
    let pane = pane.0.clone();
    let harness = w.agent.as_ref().map(|a| a.0.as_str());
    let delivery = splitter.deliver(&branch, harness, &text).map_err(|e| tag(DeliveryFailed, e))?;
    let unconfirmed = match delivery {
        Delivery::Confirmed => None,
        Delivery::Unconfirmed(why) => Some(why),
    };
    if json {
        let result = SendResult { pane, confirmed: unconfirmed.is_none(), unconfirmed_reason: unconfirmed };
        writeln!(out, "{}", super::json_result("send", branch, result)?)?;
    } else {
        match unconfirmed {
            None => writeln!(out, "sent to {branch}")?,
            Some(why) => writeln!(out, "sent to {branch}, unconfirmed: {why}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::test_support::*;
    use crate::cli::Command;

    /// `with_tmux_registry` only scripts `list-panes`, inside a `case` arm. This closes that
    /// arm and adds a Claude Code pane's screen: an empty input box until something is
    /// typed, the message in it after that, and an empty box again once Enter is pressed.
    const LIST_PANES_AND_A_CLAUDE_SCREEN: &str = r#"printf '%%3 2000000000\n' ;;
  send-keys) if [ "$4" = "-l" ]; then echo typed > "$0.state"; else echo entered > "$0.state"; fi ;;
  capture-pane)
    case "$(cat "$0.state" 2>/dev/null)" in
      typed) printf '────────\n❯ run tests\n────────\n' ;;
      *) printf '────────\n❯ \n────────\n' ;;
    esac"#;

    fn send_json(agent: &str) -> String {
        let mut out = Vec::new();
        with_tmux_registry(
            &format!("send-json-{agent}"),
            LIST_PANES_AND_A_CLAUDE_SCREEN,
            vec![workstream("fix-login", Some("%3"), Some(agent), None)],
            |repo| {
                crate::cli::run(Command::Send { target: "fix-login".into(), text: "run tests".into(), json: true }, repo, &mut out).unwrap();
            },
        );
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn send_json_reports_the_pane_it_typed_into_and_that_delivery_was_confirmed() {
        assert_eq!(
            send_json("claude"),
            "{\"schema\":1,\"ok\":true,\"command\":\"send\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\",\"confirmed\":true}}\n"
        );
    }

    /// The harness is read from the registry: this lane runs codex, whose input box kanstack
    /// can't read, so the result says the send went out unconfirmed and why.
    #[test]
    fn send_json_says_when_delivery_could_not_be_confirmed() {
        assert_eq!(
            send_json("codex"),
            "{\"schema\":1,\"ok\":true,\"command\":\"send\",\"workstream\":\"fix-login\",\"result\":{\"pane\":\"%3\",\"confirmed\":false,\"unconfirmed_reason\":\"kanstack can't read codex's input box\"}}\n"
        );
    }
}
