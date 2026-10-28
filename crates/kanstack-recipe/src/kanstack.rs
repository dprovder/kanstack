//! A thin client for kanstack's public CLI/JSON contract (`kanstack spawn/send/status --json`).
//! Shells out to a `kanstack` binary via [`std::process::Command`] — this crate never imports
//! kanstack's own Rust crate or reaches into its internals, only its documented `--json`
//! envelopes (see the repository's README, "Driving panes from a script or an agent").

use std::path::PathBuf;
use std::process::Command;

use serde::Deserialize;
use serde_json::Value;

use crate::recipe::Step;

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
/// step specifies them.
pub fn spawn_args(branch: &str, step: &Step, effective_prompt: &str) -> Vec<String> {
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
pub struct WorkstreamStatus {
    pub branch: String,
    pub status: String,
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
    ) -> Result<SpawnResult, KanstackError> {
        let args = spawn_args(branch, step, effective_prompt);
        let value = self.invoke(&args)?;
        Self::result_of(value)
    }

    pub fn send(&self, target: &str, message: &str) -> Result<(), KanstackError> {
        let args = vec![
            "send".to_string(),
            target.to_string(),
            message.to_string(),
            "--json".to_string(),
        ];
        self.invoke(&args)?;
        Ok(())
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
        }
    }

    #[test]
    fn omits_model_and_effort_when_absent() {
        let s = step("codex", None, None);
        let args = spawn_args("branch-a", &s, "prompt text");
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
        let args = spawn_args("branch-a", &s, "prompt text");
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
        let args = spawn_args("branch-a", &s, "prompt text");
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
        let args = spawn_args("branch-a", &s, "prompt text");
        assert_eq!(
            args,
            vec![
                "spawn", "branch-a", "--agent", "codex", "--model", "gpt-5.6", "--effort", "low",
                "--prompt", "prompt text", "--json",
            ]
        );
    }
}
