//! The exit-code and `--json` error contract shared by every headless subcommand.
//!
//! `kanstack spawn/send/status/focus/stop/report/prune` are meant to be driven by a script
//! or another agent, not just typed by hand, so failure needs to be as parseable as success:
//! a stable, small set of process exit codes for "what kind of thing went wrong", and — under
//! `--json` — a symbolic `error.code` precise enough that a caller who needs to branch on the
//! exact condition doesn't have to pattern-match human prose to do it. The exit code stays
//! coarse on purpose (see [`ErrorCode::exit_code`]); `error.code` is what carries the detail.
//!
//! | exit | meaning | `error.code`s |
//! | --- | --- | --- |
//! | `0` | success | — |
//! | `1` | internal: a bug, a corrupt registry, an I/O failure below everything else here | `internal` |
//! | `2` | invalid arguments — caught before anything ran | `invalid_arguments` |
//! | `3` | nothing to act on: the target names no workstream, or it has no pane | `unknown_workstream`, `no_pane` |
//! | `4` | conflict: `spawn` on a branch that already has a live pane, or `--above`/`--below` on a branch that already exists | `workstream_exists`, `branch_already_exists` |
//! | `5` | an external dependency is unavailable or refused: the split backend, `but`, the harness, or delivering a message to a pane | `multiplexer_unavailable`, `harness_unavailable`, `but_failed`, `delivery_failed` |
//!
//! A condition is tagged with [`tag`] at the point in [`super::run`] that already knows which
//! one applies — `run` itself stays untouched by this beyond that one call, since [`Coded`]'s
//! `Display`/`Debug` forward to the wrapped error unchanged, so human-mode output (still
//! `anyhow`'s own formatting, unchanged since before this module existed) never sees it.
//! Anything `run` doesn't explicitly tag — most I/O and registry errors — falls back to
//! `internal`, matching the flat exit code every failure used to get before this existed.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use serde::Serialize;

use crate::events::EventLog;

use super::{Command, run};

/// A symbolic reason a subcommand failed, carried in `--json`'s `error.code` and mapped to a
/// coarser [`exit_code`](ErrorCode::exit_code) for the process itself. See the module doc for
/// the full table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Bad or missing arguments — including `kanstack report` run with no branch to name.
    InvalidArguments,
    /// `<branch|session>` names no registered workstream.
    UnknownWorkstream,
    /// `spawn` on a branch that already has a pane open.
    WorkstreamExists,
    /// `spawn --above`/`--below` on a branch that already exists — stacking an existing
    /// branch onto another is `but move`'s job, not spawn's.
    BranchAlreadyExists,
    /// The workstream has no pane for this command to act on.
    NoPane,
    /// No split backend could be reached — none found, or a call to the one found failed.
    MultiplexerUnavailable,
    /// The configured harness could not be launched. Reserved: nothing raises this today —
    /// kanstack has no harness-availability preflight, it just types a launch line into a
    /// pane — but the code is defined now so a later check has somewhere to report to
    /// without every `--json` consumer needing a new code to handle.
    HarnessUnavailable,
    /// A `but` invocation failed.
    ButFailed,
    /// A message could not be delivered to a pane.
    DeliveryFailed,
    /// Anything else: a bug, a corrupt registry, an I/O failure below all of the above.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidArguments => "invalid_arguments",
            ErrorCode::UnknownWorkstream => "unknown_workstream",
            ErrorCode::WorkstreamExists => "workstream_exists",
            ErrorCode::BranchAlreadyExists => "branch_already_exists",
            ErrorCode::NoPane => "no_pane",
            ErrorCode::MultiplexerUnavailable => "multiplexer_unavailable",
            ErrorCode::HarnessUnavailable => "harness_unavailable",
            ErrorCode::ButFailed => "but_failed",
            ErrorCode::DeliveryFailed => "delivery_failed",
            ErrorCode::Internal => "internal",
        }
    }

    /// The process exit code this condition maps to — several codes share one, since the
    /// exit code is meant to stay small; `--json`'s `error.code` (`as_str`) is what a caller
    /// reads for the precise reason. See the module doc for the table.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::Internal => 1,
            ErrorCode::InvalidArguments => 2,
            ErrorCode::UnknownWorkstream | ErrorCode::NoPane => 3,
            ErrorCode::WorkstreamExists | ErrorCode::BranchAlreadyExists => 4,
            ErrorCode::MultiplexerUnavailable
            | ErrorCode::HarnessUnavailable
            | ErrorCode::ButFailed
            | ErrorCode::DeliveryFailed => 5,
        }
    }
}

/// Carries an [`ErrorCode`] through an `anyhow::Error` chain without changing what it prints:
/// `Display` forwards to the wrapped error's, and `source()` continues its chain, so wrapping
/// an error with [`tag`] is invisible to the human-readable path — only [`error_code`] (and
/// so `--json`, and the exit code) can tell the difference.
#[derive(Debug)]
struct Coded {
    code: ErrorCode,
    inner: anyhow::Error,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.inner, f)
    }
}

impl std::error::Error for Coded {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // `anyhow::Error` derefs to `dyn Error`, so this reaches its *own* source — i.e. the
        // rest of the chain after `inner`'s own top frame, which `Display` above already
        // printed. Delegating to `inner` itself here would print that top frame twice.
        self.inner.source()
    }
}

/// Marks `e` as `code`, for [`error_code`] and `--json`'s `error.code` to read later. Changes
/// nothing about what `e` prints in the human-readable path — see [`Coded`].
pub(super) fn tag(code: ErrorCode, e: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(Coded { code, inner: e })
}

/// Tags `e` as [`ErrorCode::InvalidArguments`] — for `main`'s own argument parsing, which
/// happens before a [`Command`] exists, so it can't go through [`dispatch`].
pub fn invalid_arguments(e: anyhow::Error) -> anyhow::Error {
    tag(ErrorCode::InvalidArguments, e)
}

/// The [`ErrorCode`] `e` was [`tag`]ged with, or [`ErrorCode::Internal`] if it never was —
/// most I/O and registry errors, which fall back to the same exit code every failure got
/// before this module existed.
pub fn error_code(e: &anyhow::Error) -> ErrorCode {
    e.downcast_ref::<Coded>().map_or(ErrorCode::Internal, |c| c.code)
}

/// The `"schema"` of the generic `--json` envelope [`dispatch`] wraps every result and error
/// in, for `spawn`/`send`/`focus`/`stop`/`report`. `status` and `prune` keep their own,
/// independently versioned shapes (`STATUS_SCHEMA`, `PRUNE_SCHEMA`) — this is a separate
/// number for a separate contract, not a shared one.
pub const RESULT_SCHEMA: u32 = 1;

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    schema: u32,
    ok: bool,
    command: &'a str,
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
}

/// Reports `e`, which happened running `command`, and returns the process exit code for it.
/// `--json` gets one document on `out`, `{"ok":false,...}`, so a script only ever reads one
/// stream whether the command it ran succeeded or failed; otherwise `err_out` gets `anyhow`'s
/// own `Error: {e:?}`, same as when every failure just propagated out of `main` and exited
/// `1`, except the exit code now says more than "something went wrong" — see the module doc.
pub fn report_error(command: &str, e: anyhow::Error, json: bool, out: &mut impl Write, err_out: &mut impl Write) -> i32 {
    let code = error_code(&e);
    if json {
        let envelope = ErrorEnvelope {
            schema: RESULT_SCHEMA,
            ok: false,
            command,
            error: ErrorBody { code: code.as_str(), message: format!("{e:#}") },
        };
        if let Ok(line) = serde_json::to_string(&envelope) {
            let _ = writeln!(out, "{line}");
        }
    } else {
        let _ = writeln!(err_out, "Error: {e:?}");
    }
    code.exit_code()
}

/// Which lifecycle event, if any, `command` should ring the events-log doorbell for
/// (`crate::events`) once it succeeds — `spawn`/`stop`/`prune` only, the branch it named when
/// it named one. `report` already rings it, from `Reports::write`, not from here; `send`,
/// `focus`, `status` and `events` don't change what "lifecycle" means for a workstream, so
/// none of them log anything here.
fn lifecycle_event(command: &Command) -> Option<(&'static str, Option<String>)> {
    match command {
        Command::Spawn { branch, .. } => Some(("spawn", Some(branch.clone()))),
        // Whatever `<branch|session>` was given — not necessarily resolved to a branch name;
        // see the module doc on `crate::events`.
        Command::Stop { target, .. } => Some(("stop", Some(target.clone()))),
        // `prune` can remove several workstreams or none; one event marks that it ran rather
        // than guessing which branches it touched.
        Command::Prune { .. } => Some(("prune", None)),
        Command::Send { .. } | Command::Status { .. } | Command::Focus { .. } | Command::Report { .. } | Command::Events { .. } | Command::Claim { .. } => None,
    }
}

/// Runs `command` against `cwd` and returns the process exit code — `0` for success, else
/// [`ErrorCode::exit_code`]. Whatever `command` itself has to say on success — human text, or
/// one `--json` document — goes to `out` from inside [`super::run`], unchanged by this
/// wrapper; a failure is [`report_error`]. A spawn/stop/prune that succeeds also rings the
/// events-log doorbell (best-effort, see [`lifecycle_event`] and `crate::events`).
pub fn dispatch(command: Command, cwd: &Path, out: &mut impl Write, err_out: &mut impl Write) -> i32 {
    let json = command.wants_json();
    let name = command.name();
    let lifecycle = lifecycle_event(&command);
    match run(command, cwd, out) {
        Ok(()) => {
            if let Some((event, branch)) = lifecycle {
                EventLog::for_repo(cwd).record_lifecycle(branch.as_deref(), event, SystemTime::now());
            }
            0
        }
        Err(e) => report_error(name, e, json, out, err_out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [ErrorCode; 10] = [
        ErrorCode::InvalidArguments,
        ErrorCode::UnknownWorkstream,
        ErrorCode::WorkstreamExists,
        ErrorCode::BranchAlreadyExists,
        ErrorCode::NoPane,
        ErrorCode::MultiplexerUnavailable,
        ErrorCode::HarnessUnavailable,
        ErrorCode::ButFailed,
        ErrorCode::DeliveryFailed,
        ErrorCode::Internal,
    ];

    /// Every code has its own `error.code` string — a caller branching on it never confuses
    /// two conditions that share an exit code.
    #[test]
    fn every_code_has_a_distinct_symbolic_name() {
        let mut names: Vec<_> = ALL.iter().map(|c| c.as_str()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before);
    }

    /// The exit code stays small (see the module doc's table) — several `ErrorCode`s share
    /// one — but every code still lands in `1..=5`, never `0` (reserved for success).
    #[test]
    fn every_exit_code_is_nonzero_and_in_the_documented_range() {
        for code in ALL {
            assert!((1..=5).contains(&code.exit_code()), "{code:?} -> {}", code.exit_code());
        }
    }

    #[test]
    fn the_exit_code_table_matches_the_module_doc() {
        assert_eq!(ErrorCode::Internal.exit_code(), 1);
        assert_eq!(ErrorCode::InvalidArguments.exit_code(), 2);
        assert_eq!(ErrorCode::UnknownWorkstream.exit_code(), 3);
        assert_eq!(ErrorCode::NoPane.exit_code(), 3);
        assert_eq!(ErrorCode::WorkstreamExists.exit_code(), 4);
        assert_eq!(ErrorCode::BranchAlreadyExists.exit_code(), 4);
        assert_eq!(ErrorCode::MultiplexerUnavailable.exit_code(), 5);
        assert_eq!(ErrorCode::HarnessUnavailable.exit_code(), 5);
        assert_eq!(ErrorCode::ButFailed.exit_code(), 5);
        assert_eq!(ErrorCode::DeliveryFailed.exit_code(), 5);
    }

    /// An untagged error — everything `run` doesn't explicitly classify — reads as `internal`,
    /// matching the flat exit code every failure got before this module existed.
    #[test]
    fn an_untagged_error_falls_back_to_internal() {
        assert_eq!(error_code(&anyhow::anyhow!("boom")), ErrorCode::Internal);
    }

    /// [`tag`] changes nothing a human sees: the same `Display` text, and the same `{:?}`
    /// chain `anyhow`'s own Debug formatting would have printed anyway.
    #[test]
    fn tagging_an_error_does_not_change_what_it_prints() {
        let original = anyhow::anyhow!("bad thing").context("while doing stuff");
        let before_display = original.to_string();
        let before_debug = format!("{original:?}");
        let tagged = tag(ErrorCode::ButFailed, original);
        assert_eq!(error_code(&tagged), ErrorCode::ButFailed);
        assert_eq!(tagged.to_string(), before_display);
        assert_eq!(format!("{tagged:?}"), before_debug);
    }

    #[test]
    fn invalid_arguments_tags_with_that_code() {
        assert_eq!(error_code(&invalid_arguments(anyhow::anyhow!("nope"))), ErrorCode::InvalidArguments);
    }

    #[test]
    fn report_error_json_is_the_generic_error_envelope() {
        let e = tag(ErrorCode::WorkstreamExists, anyhow::anyhow!("fix-login already has a pane open"));
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = report_error("spawn", e, true, &mut out, &mut err_out);
        assert_eq!(code, 4);
        assert!(err_out.is_empty(), "json mode writes nothing to the error stream");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"schema\":1,\"ok\":false,\"command\":\"spawn\",\"error\":{\"code\":\"workstream_exists\",\"message\":\"fix-login already has a pane open\"}}\n"
        );
    }

    #[test]
    fn report_error_human_mode_writes_anyhows_own_debug_to_err_out_and_nothing_to_out() {
        let e = tag(ErrorCode::UnknownWorkstream, anyhow::anyhow!("no workstream for \"x\""));
        let mut out = Vec::new();
        let mut err_out = Vec::new();
        let code = report_error("focus", e, false, &mut out, &mut err_out);
        assert_eq!(code, 3);
        assert!(out.is_empty(), "human mode writes nothing to stdout");
        assert_eq!(String::from_utf8(err_out).unwrap(), "Error: no workstream for \"x\"\n");
    }

    #[test]
    fn lifecycle_event_covers_spawn_stop_and_prune_only() {
        assert_eq!(
            lifecycle_event(&Command::Spawn {
                branch: "fix-login".into(),
                agent: None,
                prompt: None,
                item: None,
                above: None,
                below: None,
                json: false,
            }),
            Some(("spawn", Some("fix-login".to_string())))
        );
        assert_eq!(
            lifecycle_event(&Command::Stop { target: "fix-login".into(), json: false }),
            Some(("stop", Some("fix-login".to_string())))
        );
        assert_eq!(lifecycle_event(&Command::Prune { json: false }), Some(("prune", None)), "prune names no single branch");
        assert_eq!(lifecycle_event(&Command::Status { json: false }), None);
        assert_eq!(lifecycle_event(&Command::Focus { target: "fix-login".into(), json: false }), None);
        assert_eq!(lifecycle_event(&Command::Send { target: "fix-login".into(), text: "hi".into(), json: false }), None);
        assert_eq!(
            lifecycle_event(&Command::Report { state: crate::report::Reported::Busy, branch: None, json: false }),
            None,
            "report rings the doorbell itself, from Reports::write"
        );
        assert_eq!(lifecycle_event(&Command::Events { since: 0, follow: false, json: false }), None);
        assert_eq!(lifecycle_event(&Command::Claim { branch: None, json: false }), None);
    }

    /// `dispatch` is what `main` actually calls, and it's the one place that knows both a
    /// command's outcome and (for spawn/stop/prune) which branch it was about — this pins down
    /// that a successful one of those three actually rings the events-log doorbell.
    #[test]
    fn dispatch_rings_the_events_log_doorbell_for_a_successful_prune_and_stop() {
        use crate::mux::stand_in;
        let tag_ = "exit-events-doorbell";
        let (bin, _log) = stand_in::install(tag_, "tmux", r#"case "$1" in list-panes) printf '%%3 2000000000\n' ;; esac"#);
        let state = std::env::temp_dir().join(format!("kanstack-cli-{tag_}-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        stand_in::with_env(
            &[
                ("KANSTACK_STATE_PATH", Some(state.to_str().unwrap())),
                ("KANSTACK_SPLIT_BACKEND", Some("tmux")),
                ("KANSTACK_TMUX_BIN", Some(bin.to_str().unwrap())),
                ("TMUX_PANE", Some("%0")),
            ],
            || {
                use crate::workstream::{AgentId, BranchId, PaneId, Registry, Workstream};
                let repo = Path::new("/repo/exit-events-doorbell");
                let mut registry = Registry::load(repo).unwrap();
                registry.upsert(Workstream {
                    branch_id: BranchId("fix-login".into()),
                    pane_id: Some(PaneId("%3".into())),
                    agent: Some(AgentId("claude".into())),
                    item: None,
                });
                registry.save().unwrap();

                // A read-only command first: `status` must ring nothing.
                let mut out = Vec::new();
                let mut err_out = Vec::new();
                assert_eq!(dispatch(Command::Status { json: true }, repo, &mut out, &mut err_out), 0);

                assert_eq!(dispatch(Command::Stop { target: "fix-login".into(), json: true }, repo, &mut Vec::new(), &mut Vec::new()), 0);
                assert_eq!(dispatch(Command::Prune { json: true }, repo, &mut Vec::new(), &mut Vec::new()), 0);

                let raw = std::fs::read_to_string(crate::workstream::events_path(repo).unwrap()).unwrap();
                let lines: Vec<&str> = raw.lines().collect();
                assert_eq!(lines.len(), 2, "status must not have logged anything: {raw}");
                assert!(lines[0].contains(r#""command":"stop""#) && lines[0].contains(r#""branch":"fix-login""#), "{}", lines[0]);
                assert!(lines[1].contains(r#""command":"prune""#) && !lines[1].contains("\"branch\""), "{}", lines[1]);
            },
        );
        stand_in::remove(&bin);
        let _ = std::fs::remove_dir_all(&state);
    }
}
