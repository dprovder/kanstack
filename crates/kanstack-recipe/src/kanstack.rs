//! A thin client for kanstack's public CLI/JSON contract (`kanstack spawn/send/stop/status
//! --json`, and the raw JSONL `kanstack events` prints).
//! Shells out to a `kanstack` binary via [`std::process::Command`] — this crate never imports
//! kanstack's own Rust crate or reaches into its internals, only its documented `--json`
//! envelopes (see the repository's README, "Driving panes from a script or an agent").

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::recipe::{SpawnMode, Step};

/// The `kanstack` binary a command runs, as an absolute path: `given` (`--kanstack-bin`), else
/// whatever `kanstack` resolves to on `PATH`. Resolved once at startup, so the binary the runner
/// calls and the one its prompts tell agents to call are the same file, whatever an agent's
/// own `PATH` holds. A bare name not found on `PATH` is kept as it is, to fail as before.
pub fn resolve_bin(given: Option<PathBuf>) -> PathBuf {
    resolve_bin_on(given, std::env::var_os("PATH").as_deref())
}

/// [`resolve_bin`] against `path`, a `PATH` value.
fn resolve_bin_on(given: Option<PathBuf>, path: Option<&std::ffi::OsStr>) -> PathBuf {
    let bin = given.unwrap_or_else(|| PathBuf::from("kanstack"));
    if bin.components().count() > 1 || bin.is_absolute() {
        return std::path::absolute(&bin).unwrap_or(bin);
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .map(|dir| dir.join(&bin))
        .find(|candidate| is_executable(candidate))
        .map(|found| std::path::absolute(&found).unwrap_or(found))
        .unwrap_or(bin)
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[derive(Debug)]
pub struct KanstackError {
    pub code: Option<String>,
    pub message: String,
}

impl std::fmt::Display for KanstackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(f, "{} ({code})", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for KanstackError {}

impl From<std::io::Error> for KanstackError {
    fn from(e: std::io::Error) -> Self {
        KanstackError {
            code: None,
            message: format!("failed to run kanstack: {e}"),
        }
    }
}

/// The `kanstack spawn` argument vector for a step, split out as a pure function so tests can
/// assert on it without spawning a process. `--model`/`--effort` are included only when the
/// step specifies them, `--above <above>` only when given one (a `stack` step), and
/// `--advisory` only for an advisory step.
pub fn spawn_args(branch: &str, step: &Step, effective_prompt: &str, above: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "spawn".to_string(),
        branch.to_string(),
        "--agent".to_string(),
        step.agent.clone(),
    ];
    if let Some(model) = &step.model {
        args.push("--model".to_string());
        args.push(model.clone());
    }
    if let Some(effort) = step.effort {
        args.push("--effort".to_string());
        args.push(effort.as_str().to_string());
    }
    if let Some(above) = above {
        args.push("--above".to_string());
        args.push(above.to_string());
    }
    if step.spawn == Some(SpawnMode::Advisory) {
        args.push("--advisory".to_string());
    }
    args.push("--prompt".to_string());
    args.push(effective_prompt.to_string());
    args.push("--json".to_string());
    args
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpawnResult {
    #[allow(dead_code)]
    pub created: bool,
    pub pane: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendResult {
    pub pane: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkstreamStatus {
    pub branch: String,
    /// The lane's pane id, so a resumed step can reattach to it.
    #[serde(default)]
    pub pane: Option<String>,
    pub status: String,
}

/// One `kind: "note"` line from the events log: an agent (or anyone) reporting on `branch`
/// with `kanstack note <branch> "<text>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub branch: String,
    pub text: String,
}

#[derive(Debug, Deserialize)]
struct EventLine {
    #[serde(default)]
    branch: Option<String>,
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusEnvelope {
    #[serde(default)]
    workstreams: Vec<WorkstreamStatus>,
}

pub struct KanstackClient {
    bin: PathBuf,
}

impl KanstackClient {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        KanstackClient { bin: bin.into() }
    }

    /// The `kanstack` this client runs, which prompts name too (see [`resolve_bin`]).
    pub fn bin(&self) -> &Path {
        &self.bin
    }

    /// Runs `kanstack <args>`, parses stdout as the shared `--json` envelope, and turns
    /// `ok:false` (or a non-JSON / non-zero-exit failure) into a [`KanstackError`].
    /// `status --json`'s success shape has no `ok` key, so its absence defers to the process
    /// exit code instead of being treated as failure.
    fn invoke(&self, args: &[String]) -> Result<Value, KanstackError> {
        let output = Command::new(&self.bin).args(args).output()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        let value: Value = serde_json::from_str(stdout.trim()).map_err(|_| KanstackError {
            code: None,
            message: format!(
                "kanstack produced no parseable JSON (exit {:?}): {}",
                output.status.code(),
                stderr.trim()
            ),
        })?;

        let ok = value
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| output.status.success());
        if !ok {
            let code = value
                .pointer("/error/code")
                .and_then(Value::as_str)
                .map(String::from);
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("kanstack command failed")
                .to_string();
            return Err(KanstackError { code, message });
        }
        Ok(value)
    }

    /// Runs `kanstack <args>` for a command whose success output isn't a JSON document
    /// (`events` prints raw JSONL), returning stdout as is. A failure is still the `--json`
    /// envelope, so it's turned into a [`KanstackError`] the same way [`Self::invoke`] does.
    fn invoke_raw(&self, args: &[String]) -> Result<Vec<u8>, KanstackError> {
        let output = Command::new(&self.bin).args(args).output()?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        Err(KanstackError {
            code: value.pointer("/error/code").and_then(Value::as_str).map(String::from),
            message: value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| {
                    format!(
                        "kanstack {} failed (exit {:?}): {}",
                        args.first().map(String::as_str).unwrap_or_default(),
                        output.status.code(),
                        String::from_utf8_lossy(&output.stderr).trim()
                    )
                }),
        })
    }

    fn result_of<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, KanstackError> {
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result).map_err(|e| KanstackError {
            code: None,
            message: format!("unexpected result shape from kanstack: {e}"),
        })
    }

    pub fn spawn(
        &self,
        branch: &str,
        step: &Step,
        effective_prompt: &str,
        above: Option<&str>,
    ) -> Result<SpawnResult, KanstackError> {
        let args = spawn_args(branch, step, effective_prompt, above);
        let value = self.invoke(&args)?;
        Self::result_of(value)
    }

    pub fn send(&self, target: &str, message: &str) -> Result<SendResult, KanstackError> {
        let args = vec![
            "send".to_string(),
            target.to_string(),
            message.to_string(),
            "--json".to_string(),
        ];
        let value = self.invoke(&args)?;
        Self::result_of(value)
    }

    /// `kanstack stop <branch>`. A lane that's already gone (`unknown_workstream`) counts as
    /// stopped, as kanstack's own `stop` documents for a caller retrying one.
    pub fn stop(&self, branch: &str) -> Result<(), KanstackError> {
        match self.invoke(&["stop".to_string(), branch.to_string(), "--json".to_string()]) {
            Err(e) if e.code.as_deref() == Some("unknown_workstream") => Ok(()),
            other => other.map(|_| ()),
        }
    }

    /// The events log's current length in bytes: where a step's notes start. `kanstack events`
    /// prints the log byte for byte, so that's the length of everything it prints.
    pub fn events_end(&self) -> Result<u64, KanstackError> {
        let raw = self.invoke_raw(&["events".to_string(), "--json".to_string()])?;
        Ok(raw.len() as u64)
    }

    /// Every note in the events log past byte `since`, oldest first. A line that isn't a
    /// readable event is skipped, as kanstack's own reader does.
    pub fn notes_since(&self, since: u64) -> Result<Vec<Note>, KanstackError> {
        let raw = self.invoke_raw(&["events".to_string(), "--since".to_string(), since.to_string(), "--json".to_string()])?;
        Ok(String::from_utf8_lossy(&raw)
            .lines()
            .filter_map(|line| serde_json::from_str::<EventLine>(line).ok())
            .filter(|e| e.kind == "note")
            .filter_map(|e| Some(Note { branch: e.branch?, text: e.text? }))
            .collect())
    }

    pub fn status(&self) -> Result<Vec<WorkstreamStatus>, KanstackError> {
        let value = self.invoke(&["status".to_string(), "--json".to_string()])?;
        let envelope: StatusEnvelope = serde_json::from_value(value).map_err(|e| KanstackError {
            code: None,
            message: format!("unexpected status shape from kanstack: {e}"),
        })?;
        Ok(envelope.workstreams)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::Effort;

    fn step(agent: &str, model: Option<&str>, effort: Option<Effort>) -> Step {
        Step {
            id: "s".to_string(),
            agent: agent.to_string(),
            model: model.map(|m| m.to_string()),
            effort,
            prompt: "do the thing".to_string(),
            needs: vec![],
            on: None,
            owns: vec![],
            verify: vec![],
            spawn: None,
            done_when: crate::recipe::DoneWhen::Commits,
        }
    }

    #[test]
    fn the_kanstack_binary_is_resolved_to_an_absolute_path_once() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let (empty, bin) = (tmp.path().join("empty"), tmp.path().join("bin"));
        for dir in [&empty, &bin] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(empty.join("kanstack"), "not executable").unwrap();
        let exe = bin.join("kanstack");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([&empty, &bin]).unwrap();

        assert_eq!(resolve_bin_on(None, Some(&path)), exe, "the first executable `kanstack` on PATH");
        assert_eq!(resolve_bin_on(Some(PathBuf::from("/k/kanstack")), Some(&path)), PathBuf::from("/k/kanstack"));
        let relative = resolve_bin_on(Some(PathBuf::from("target/debug/kanstack")), Some(&path));
        assert_eq!(relative, std::env::current_dir().unwrap().join("target/debug/kanstack"));
        assert_eq!(resolve_bin_on(None, Some(std::ffi::OsStr::new(""))), PathBuf::from("kanstack"), "not found: left to fail as before");
    }

    #[test]
    fn omits_model_and_effort_when_absent() {
        let s = step("codex", None, None);
        let args = spawn_args("branch-a", &s, "prompt text", None);
        assert_eq!(
            args,
            vec![
                "spawn", "branch-a", "--agent", "codex", "--prompt", "prompt text", "--json",
            ]
        );
    }

    #[test]
    fn forwards_model_only() {
        let s = step("codex", Some("gpt-5.6"), None);
        let args = spawn_args("branch-a", &s, "prompt text", None);
        assert_eq!(
            args,
            vec![
                "spawn", "branch-a", "--agent", "codex", "--model", "gpt-5.6", "--prompt",
                "prompt text", "--json",
            ]
        );
    }

    #[test]
    fn forwards_effort_only() {
        let s = step("claude", None, Some(Effort::High));
        let args = spawn_args("branch-a", &s, "prompt text", None);
        assert_eq!(
            args,
            vec![
                "spawn", "branch-a", "--agent", "claude", "--effort", "high", "--prompt",
                "prompt text", "--json",
            ]
        );
    }

    #[test]
    fn forwards_both_model_and_effort() {
        let s = step("codex", Some("gpt-5.6"), Some(Effort::Low));
        let args = spawn_args("branch-a", &s, "prompt text", None);
        assert_eq!(
            args,
            vec![
                "spawn", "branch-a", "--agent", "codex", "--model", "gpt-5.6", "--effort", "low",
                "--prompt", "prompt text", "--json",
            ]
        );
    }

    #[test]
    fn stacks_above_a_target_and_marks_advisory() {
        let s = step("claude", None, None);
        assert_eq!(
            spawn_args("impl-review", &s, "p", Some("impl")),
            vec!["spawn", "impl-review", "--agent", "claude", "--above", "impl", "--prompt", "p", "--json"]
        );

        let mut s = step("claude", None, None);
        s.spawn = Some(SpawnMode::Advisory);
        assert_eq!(
            spawn_args("survey", &s, "p", None),
            vec!["spawn", "survey", "--agent", "claude", "--advisory", "--prompt", "p", "--json"]
        );
    }
}
