//! Parsing and validation for the v1 recipe format: a Markdown file whose front matter (YAML,
//! between a leading `---` pair) declares a step DAG, and whose body is shared context text
//! appended to every step's own prompt. See this crate's README for the field semantics; this
//! module only implements them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::graph::resolve_workstream;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    fn parse(s: &str) -> Option<Effort> {
        match s {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            _ => None,
        }
    }
}

/// How a step's agent reaches the code (`spawn:`). See the README's "Spawn modes".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnMode {
    /// Send into the `on:` branch's existing pane.
    Continue,
    /// Stop the `on:` branch's agent, then spawn this step's agent on the same branch.
    Handoff,
    /// Spawn on a new branch, `<target>-<step>`, stacked above the `on:` branch.
    Stack,
    /// Spawn on the step's own branch with `--advisory`: not expected to commit.
    Advisory,
}

impl SpawnMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SpawnMode::Continue => "continue",
            SpawnMode::Handoff => "handoff",
            SpawnMode::Stack => "stack",
            SpawnMode::Advisory => "advisory",
        }
    }

    fn parse(s: &str) -> Option<SpawnMode> {
        match s {
            "continue" => Some(SpawnMode::Continue),
            "handoff" => Some(SpawnMode::Handoff),
            "stack" => Some(SpawnMode::Stack),
            "advisory" => Some(SpawnMode::Advisory),
            _ => None,
        }
    }
}

/// What a step needs, besides a trusted idle, to count as finished (`done_when:`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoneWhen {
    /// The step's branch tip moved since the step's recorded `base`.
    Commits,
    /// The agent reported back with `kanstack note <branch> "..."`.
    Note,
    /// Either of the above.
    Either,
}

impl DoneWhen {
    pub fn as_str(self) -> &'static str {
        match self {
            DoneWhen::Commits => "commits",
            DoneWhen::Note => "note",
            DoneWhen::Either => "either",
        }
    }

    fn parse(s: &str) -> Option<DoneWhen> {
        match s {
            "commits" => Some(DoneWhen::Commits),
            "note" => Some(DoneWhen::Note),
            "either" => Some(DoneWhen::Either),
            _ => None,
        }
    }

    pub fn allows_note(self) -> bool {
        matches!(self, DoneWhen::Note | DoneWhen::Either)
    }

    pub fn allows_commits(self) -> bool {
        matches!(self, DoneWhen::Commits | DoneWhen::Either)
    }

    /// What a step held up by this rule is waiting on, for humans.
    pub fn waiting_label(self) -> &'static str {
        match self {
            DoneWhen::Commits => "commits",
            DoneWhen::Note => "a note",
            DoneWhen::Either => "commits or a note",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Step {
    pub id: String,
    pub agent: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub prompt: String,
    pub needs: Vec<String>,
    pub on: Option<String>,
    pub owns: Vec<String>,
    pub verify: Vec<String>,
    /// `None` when the recipe leaves it out: an `on:` step then picks `continue` or `handoff`
    /// at launch by the agent last run on its branch, and a step with no `on:` spawns its own.
    pub spawn: Option<SpawnMode>,
    /// Already defaulted: `note` for an advisory step or the aggregate step, `commits`
    /// otherwise.
    pub done_when: DoneWhen,
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub steps: BTreeMap<String, Step>,
    pub context: String,
    /// The recipe's `aggregate:` step, if it names one: it runs last, after every other step,
    /// and its prompt carries their outcomes (see [`crate::aggregate`]).
    pub aggregate: Option<String>,
    /// One line saying what the recipe is for, shown by `recipes` and kanstack's recipe
    /// picker. Never part of a prompt.
    pub description: Option<String>,
    /// Free words to find the recipe by, like `description`.
    pub tags: Vec<String>,
}

impl Recipe {
    /// `<step prompt>\n\n## Recipe context\n\n<markdown body>` — the one deterministic
    /// composition, no conditional templating.
    pub fn effective_prompt(&self, step_id: &str) -> String {
        let step = &self.steps[step_id];
        format!("{}\n\n## Recipe context\n\n{}", step.prompt, self.context)
    }

    /// What actually reaches the harness on `branch`: the effective prompt, plus the lines
    /// asking for a `kanstack note` report. Every step is asked, whatever its `done_when`: the
    /// note is the step's report, while `done_when` only decides when it's finished. `kanstack`
    /// is the binary the run drives lanes with (see [`report_instruction`]).
    pub fn launch_prompt(&self, step_id: &str, branch: &str, kanstack: &Path) -> String {
        format!("{}\n\n{}", self.effective_prompt(step_id), report_instruction(kanstack, branch))
    }
}

/// The lines appended to every step's prompt, asking for a report worth reading rather than a
/// one-liner. It names `kanstack` by the path the run was given rather than as a bare
/// `kanstack`, so the note goes through the build that reads it back, not whichever one is
/// first on the agent's `PATH` (which may be too old to have `note` at all).
pub fn report_instruction(kanstack: &Path, branch: &str) -> String {
    format!(
        "When you're finished, report back by running: {} note {branch} \"<report>\"\n\
         Make it a real report, not a one-liner: what changed; what you verified, and how; what you didn't do or \
         couldn't confirm; caveats; and follow-ups worth proposing.",
        shell_word(kanstack)
    )
}

/// `path` as one shell word: as it is when it's made only of characters no shell treats
/// specially, single-quoted otherwise. For commands written into prompts, so the common case
/// stays readable.
pub fn shell_word(path: &Path) -> String {
    let text = path.to_string_lossy();
    let plain = !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c));
    if plain {
        text.into_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

#[derive(Debug)]
pub enum RecipeError {
    Malformed(String),
    UnsupportedVersion(u64),
    NoSteps,
    EmptyStepId,
    MissingAgent(String),
    MissingPrompt(String),
    InvalidEffort { step: String, value: String },
    UnknownNeeds { step: String, target: String },
    UnknownOn { step: String, target: String },
    SelfReference { step: String, field: &'static str },
    NeedsCycle(Vec<String>),
    OnCycle(Vec<String>),
    OnNotInNeeds { step: String, target: String },
    InvalidSpawn { step: String, value: String },
    InvalidDoneWhen { step: String, value: String },
    /// `continue`, `handoff` and `stack` all act on an earlier step's branch, named by `on:`.
    SpawnNeedsOn { step: String, mode: SpawnMode },
    /// `advisory` spawns its own lane, so an `on:` would be silently ignored.
    AdvisoryWithOn { step: String },
    /// `continue` would type this step's prompt into a pane running another agent.
    ContinueAgentMismatch { step: String, agent: String, branch: String, branch_agent: String },
    /// `aggregate:` names a step the recipe doesn't have.
    UnknownAggregate(String),
    /// The aggregate step runs after every other step, so none can wait for or run on it.
    AggregateReferenced { step: String, aggregate: String, field: &'static str },
    /// `description:` is shown on one line wherever recipes are listed.
    MultiLineDescription,
    /// `timeout:` isn't a duration [`parse_duration`] reads. `step` is `None` for the
    /// recipe-level one.
    InvalidTimeout { step: Option<String>, value: String },
}

impl fmt::Display for RecipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "recipe error: ")?;
        match self {
            RecipeError::Malformed(detail) => write!(f, "{detail}"),
            RecipeError::UnsupportedVersion(v) => {
                write!(f, "unsupported recipe version {v} (only 1 is supported)")
            }
            RecipeError::NoSteps => write!(f, "recipe has no steps"),
            RecipeError::EmptyStepId => write!(f, "a step id is empty"),
            RecipeError::MissingAgent(step) => write!(f, "step \"{step}\" is missing `agent`"),
            RecipeError::MissingPrompt(step) => write!(f, "step \"{step}\" is missing `prompt`"),
            RecipeError::InvalidEffort { step, value } => write!(
                f,
                "step \"{step}\" has invalid effort \"{value}\" (expected low, medium or high)"
            ),
            RecipeError::UnknownNeeds { step, target } => write!(
                f,
                "step \"{step}\" depends on unknown step \"{target}\""
            ),
            RecipeError::UnknownOn { step, target } => write!(
                f,
                "step \"{step}\" is placed `on` unknown step \"{target}\""
            ),
            RecipeError::SelfReference { step, field } => {
                write!(f, "step \"{step}\" cannot `{field}` itself")
            }
            RecipeError::NeedsCycle(chain) => {
                write!(f, "cycle in `needs`: {}", chain.join(" -> "))
            }
            RecipeError::OnCycle(chain) => {
                write!(f, "cycle in `on` placement: {}", chain.join(" -> "))
            }
            RecipeError::OnNotInNeeds { step, target } => write!(
                f,
                "step \"{step}\" is placed `on` \"{target}\", which is not in its `needs` \
                 (directly or transitively) — `on` selects placement, not causal order"
            ),
            RecipeError::InvalidSpawn { step, value } => write!(
                f,
                "step \"{step}\" has unknown spawn mode \"{value}\" (expected continue, handoff, stack or advisory)"
            ),
            RecipeError::InvalidDoneWhen { step, value } => write!(
                f,
                "step \"{step}\" has invalid done_when \"{value}\" (expected commits, note or either)"
            ),
            RecipeError::SpawnNeedsOn { step, mode } => write!(
                f,
                "step \"{step}\" has `spawn: {}` but no `on`, which names the earlier step whose branch it works on",
                mode.as_str()
            ),
            RecipeError::AdvisoryWithOn { step } => write!(
                f,
                "step \"{step}\" has `spawn: advisory`, which spawns its own lane, so its `on` would be ignored; drop one of them"
            ),
            RecipeError::ContinueAgentMismatch { step, agent, branch, branch_agent } => write!(
                f,
                "step \"{step}\" has `spawn: continue` with agent {agent}, but the agent on branch \"{branch}\" \
                 by then is {branch_agent}; use `spawn: handoff` to stop {branch_agent} and start {agent} there"
            ),
            RecipeError::UnknownAggregate(step) => write!(f, "`aggregate` names unknown step \"{step}\""),
            RecipeError::AggregateReferenced { step, aggregate, field } => write!(
                f,
                "step \"{step}\" names the aggregate step \"{aggregate}\" in its `{field}`, but the aggregate step runs after every other step"
            ),
            RecipeError::InvalidTimeout { step, value } => {
                match step {
                    Some(step) => write!(f, "step \"{step}\" has invalid timeout \"{value}\"")?,
                    None => write!(f, "invalid timeout \"{value}\"")?,
                }
                write!(f, " (expected a duration like 90s, 45m, 2h or 1h30m)")
            }
            RecipeError::MultiLineDescription => {
                write!(f, "`description` spans several lines; keep it to one, and put the rest in the recipe's body")
            }
        }
    }
}

impl std::error::Error for RecipeError {}

#[derive(Debug, Deserialize)]
struct RawFrontMatter {
    version: u64,
    retries: Option<u32>,
    timeout: Option<serde_yaml::Value>,
    aggregate: Option<String>,
    description: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    steps: BTreeMap<String, RawStep>,
}

#[derive(Debug, Deserialize)]
struct RawStep {
    agent: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    needs: Vec<String>,
    on: Option<String>,
    #[serde(default)]
    owns: Vec<String>,
    #[serde(default)]
    verify: Vec<String>,
    spawn: Option<String>,
    done_when: Option<String>,
    retries: Option<u32>,
    timeout: Option<serde_yaml::Value>,
}

/// A recipe's `retries:`, recipe-wide and per step, read by [`parse_retries`]. Held beside
/// [`Recipe`] rather than in it: a run resolves every step's budget once, when it starts, and
/// keeps it in its own state (`StepRecord::retry_budget`), so the engine never needs it here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryBudgets {
    pub recipe: Option<u32>,
    pub steps: BTreeMap<String, u32>,
}

impl RetryBudgets {
    /// How many retries `step_id` gets: its own `retries:`, else the recipe's, else `default`
    /// (which `serve` takes from `KANSTACK_RETRIES`).
    pub fn budget(&self, step_id: &str, default: u32) -> u32 {
        self.steps.get(step_id).copied().or(self.recipe).unwrap_or(default)
    }
}

/// The `retries:` in `input`'s front matter. Fails exactly when [`parse`] would fail to read
/// the front matter, so a caller that already parsed the recipe can rely on it.
pub fn parse_retries(input: &str) -> Result<RetryBudgets, RecipeError> {
    let (yaml, _) = split_front_matter(input)?;
    let raw: RawFrontMatter = serde_yaml::from_str(yaml)
        .map_err(|e| RecipeError::Malformed(format!("invalid YAML front matter: {e}")))?;
    Ok(RetryBudgets {
        recipe: raw.retries,
        steps: raw.steps.into_iter().filter_map(|(id, step)| Some((id, step.retries?))).collect(),
    })
}

/// A recipe's `timeout:`, recipe-wide and per step, read by [`parse_timeouts`]. Held beside
/// [`Recipe`] for the same reason as [`RetryBudgets`]: a run resolves every step's timeout once,
/// when it starts, and keeps it in its own state (`StepRecord::timeout_secs`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepTimeouts {
    pub recipe: Option<Duration>,
    pub steps: BTreeMap<String, Duration>,
}

impl StepTimeouts {
    /// How long `step_id` may run: its own `timeout:`, else the recipe's, else `default`
    /// (`KANSTACK_STEP_TIMEOUT`, see [`crate::run::step_timeout_from_env`]).
    pub fn timeout(&self, step_id: &str, default: Duration) -> Duration {
        self.steps.get(step_id).copied().or(self.recipe).unwrap_or(default)
    }
}

/// The `timeout:` in `input`'s front matter. Fails exactly when [`parse`] would fail on one, so
/// a caller that already parsed the recipe can rely on it.
pub fn parse_timeouts(input: &str) -> Result<StepTimeouts, RecipeError> {
    let (yaml, _) = split_front_matter(input)?;
    let raw: RawFrontMatter = serde_yaml::from_str(yaml)
        .map_err(|e| RecipeError::Malformed(format!("invalid YAML front matter: {e}")))?;
    let mut steps = BTreeMap::new();
    for (id, step) in raw.steps {
        if let Some(timeout) = timeout(Some(&id), step.timeout.as_ref())? {
            steps.insert(id, timeout);
        }
    }
    Ok(StepTimeouts { recipe: timeout(None, raw.timeout.as_ref())?, steps })
}

/// A `timeout:` value as a duration: a string [`parse_duration`] reads. A bare number is
/// refused rather than guessed at, since it has no unit.
fn timeout(step: Option<&str>, raw: Option<&serde_yaml::Value>) -> Result<Option<Duration>, RecipeError> {
    let Some(raw) = raw else { return Ok(None) };
    let invalid = || RecipeError::InvalidTimeout {
        step: step.map(String::from),
        value: match raw {
            serde_yaml::Value::String(s) => s.clone(),
            other => serde_yaml::to_string(other).unwrap_or_default().trim().to_string(),
        },
    };
    raw.as_str().and_then(parse_duration).map(Some).ok_or_else(invalid)
}

/// A duration written as whole numbers with units, largest first: `90s`, `45m`, `2h`, `1h30m`.
/// `None` for anything else, and for zero.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let mut secs: u64 = 0;
    let mut rest = text.trim();
    let mut last_unit = u64::MAX;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let n: u64 = rest[..digits].parse().ok()?;
        let unit = match rest[digits..].chars().next()? {
            'h' => 3600,
            'm' => 60,
            's' => 1,
            _ => return None,
        };
        if unit >= last_unit {
            return None;
        }
        last_unit = unit;
        secs = secs.checked_add(n.checked_mul(unit)?)?;
        rest = &rest[digits + 1..];
    }
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// `duration` the way [`parse_duration`] reads one: `45m`, `2h`, `1h30m`, `90s`.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut out = String::new();
    for (n, unit) in [(h, 'h'), (m, 'm'), (s, 's')] {
        if n > 0 {
            out.push_str(&format!("{n}{unit}"));
        }
    }
    if out.is_empty() {
        out.push_str("0s");
    }
    out
}

/// What lists a recipe (`recipes`, kanstack's picker): its `description:` and `tags:` and how
/// many steps it declares, read from the front matter without validating the steps, so a
/// recipe that doesn't pass [`parse`] can still be listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Meta {
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub steps: usize,
}

pub fn parse_meta(input: &str) -> Result<Meta, RecipeError> {
    let (yaml, _) = split_front_matter(input)?;
    let raw: RawFrontMatter = serde_yaml::from_str(yaml)
        .map_err(|e| RecipeError::Malformed(format!("invalid YAML front matter: {e}")))?;
    Ok(Meta { description: description(raw.description.as_deref())?, tags: tags(&raw.tags), steps: raw.steps.len() })
}

/// `description:` trimmed, `None` when blank. A folded YAML scalar (`>`) ends in a newline,
/// which the trim forgives; a line break inside it is an error.
fn description(raw: Option<&str>) -> Result<Option<String>, RecipeError> {
    let Some(text) = raw.map(str::trim).filter(|text| !text.is_empty()) else { return Ok(None) };
    if text.contains(['\n', '\r']) {
        return Err(RecipeError::MultiLineDescription);
    }
    Ok(Some(text.to_string()))
}

fn tags(raw: &[String]) -> Vec<String> {
    raw.iter().map(|tag| tag.trim()).filter(|tag| !tag.is_empty()).map(String::from).collect()
}

pub fn parse(input: &str) -> Result<Recipe, RecipeError> {
    let (yaml, body) = split_front_matter(input)?;
    let raw: RawFrontMatter = serde_yaml::from_str(yaml)
        .map_err(|e| RecipeError::Malformed(format!("invalid YAML front matter: {e}")))?;
    validate(raw, body)
}

/// Splits `---\n<yaml>\n---\n<body>` into `(yaml, body)`. The opening delimiter must be the
/// file's first line; the closing delimiter is the first later line that is exactly `---`.
fn split_front_matter(input: &str) -> Result<(&str, &str), RecipeError> {
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let rest = input
        .strip_prefix("---\r\n")
        .or_else(|| input.strip_prefix("---\n"))
        .ok_or_else(|| {
            RecipeError::Malformed("missing opening `---` front-matter delimiter".to_string())
        })?;

    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']) == "---" {
            let yaml = &rest[..offset];
            let body = rest[offset + line.len()..].trim();
            return Ok((yaml, body));
        }
        offset += line.len();
    }

    Err(RecipeError::Malformed(
        "missing closing `---` front-matter delimiter".to_string(),
    ))
}

fn validate(raw: RawFrontMatter, body: &str) -> Result<Recipe, RecipeError> {
    if raw.version != 1 {
        return Err(RecipeError::UnsupportedVersion(raw.version));
    }
    if raw.steps.is_empty() {
        return Err(RecipeError::NoSteps);
    }
    let description = description(raw.description.as_deref())?;
    let tags = tags(&raw.tags);
    if let Some(aggregate) = &raw.aggregate {
        if !raw.steps.contains_key(aggregate) {
            return Err(RecipeError::UnknownAggregate(aggregate.clone()));
        }
        for (id, step) in raw.steps.iter().filter(|(id, _)| *id != aggregate) {
            let field = if step.needs.contains(aggregate) {
                "needs"
            } else if step.on.as_ref() == Some(aggregate) {
                "on"
            } else {
                continue;
            };
            return Err(RecipeError::AggregateReferenced { step: id.clone(), aggregate: aggregate.clone(), field });
        }
    }

    timeout(None, raw.timeout.as_ref())?;
    for (id, step) in &raw.steps {
        timeout(Some(id), step.timeout.as_ref())?;
    }

    let ids: Vec<String> = raw.steps.keys().cloned().collect();
    let mut steps = BTreeMap::new();
    for (id, raw_step) in raw.steps {
        if id.trim().is_empty() {
            return Err(RecipeError::EmptyStepId);
        }
        let agent = raw_step
            .agent
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| RecipeError::MissingAgent(id.clone()))?;
        let prompt = raw_step
            .prompt
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| RecipeError::MissingPrompt(id.clone()))?;
        let effort = match raw_step.effort {
            Some(s) => Some(Effort::parse(&s).ok_or_else(|| RecipeError::InvalidEffort {
                step: id.clone(),
                value: s.clone(),
            })?),
            None => None,
        };
        if raw_step.needs.iter().any(|n| n == &id) {
            return Err(RecipeError::SelfReference {
                step: id.clone(),
                field: "needs",
            });
        }
        if raw_step.on.as_deref() == Some(id.as_str()) {
            return Err(RecipeError::SelfReference {
                step: id.clone(),
                field: "on",
            });
        }
        let is_aggregate = raw.aggregate.as_ref() == Some(&id);
        let spawn = match raw_step.spawn {
            Some(s) => Some(SpawnMode::parse(&s).ok_or_else(|| RecipeError::InvalidSpawn {
                step: id.clone(),
                value: s.clone(),
            })?),
            // The aggregate step reads and proposes; it needs no branch of anyone else's.
            None if is_aggregate && raw_step.on.is_none() => Some(SpawnMode::Advisory),
            None => None,
        };
        match (spawn, &raw_step.on) {
            (Some(SpawnMode::Advisory), Some(_)) => return Err(RecipeError::AdvisoryWithOn { step: id.clone() }),
            (Some(mode @ (SpawnMode::Continue | SpawnMode::Handoff | SpawnMode::Stack)), None) => {
                return Err(RecipeError::SpawnNeedsOn { step: id.clone(), mode })
            }
            _ => {}
        }
        let done_when = match raw_step.done_when {
            Some(s) => DoneWhen::parse(&s).ok_or_else(|| RecipeError::InvalidDoneWhen {
                step: id.clone(),
                value: s.clone(),
            })?,
            None if spawn == Some(SpawnMode::Advisory) || is_aggregate => DoneWhen::Note,
            None => DoneWhen::Commits,
        };
        // The aggregate step implicitly needs every other step, after whatever it lists.
        let mut needs = raw_step.needs;
        if is_aggregate {
            for other in ids.iter().filter(|other| **other != id) {
                if !needs.contains(other) {
                    needs.push(other.clone());
                }
            }
        }

        steps.insert(
            id.clone(),
            Step {
                id: id.clone(),
                agent,
                model: raw_step.model,
                effort,
                prompt,
                needs,
                on: raw_step.on,
                owns: raw_step.owns,
                verify: raw_step.verify,
                spawn,
                done_when,
            },
        );
    }

    for step in steps.values() {
        for need in &step.needs {
            if !steps.contains_key(need) {
                return Err(RecipeError::UnknownNeeds {
                    step: step.id.clone(),
                    target: need.clone(),
                });
            }
        }
        if let Some(on) = &step.on {
            if !steps.contains_key(on) {
                return Err(RecipeError::UnknownOn {
                    step: step.id.clone(),
                    target: on.clone(),
                });
            }
        }
    }

    detect_needs_cycle(&steps)?;
    detect_on_cycle(&steps)?;
    detect_on_without_needs(&steps)?;

    let recipe = Recipe {
        steps,
        context: body.to_string(),
        aggregate: raw.aggregate,
        description,
        tags,
    };
    detect_continue_agent_mismatch(&recipe)?;
    Ok(recipe)
}

fn detect_needs_cycle(steps: &BTreeMap<String, Step>) -> Result<(), RecipeError> {
    #[derive(PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit(
        id: &str,
        steps: &BTreeMap<String, Step>,
        marks: &mut HashMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> Result<(), RecipeError> {
        match marks.get(id) {
            Some(Mark::Done) => return Ok(()),
            Some(Mark::Visiting) => {
                let start = stack.iter().position(|s| s == id).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..].to_vec();
                cycle.push(id.to_string());
                return Err(RecipeError::NeedsCycle(cycle));
            }
            None => {}
        }
        marks.insert(id.to_string(), Mark::Visiting);
        stack.push(id.to_string());
        for need in &steps[id].needs {
            visit(need, steps, marks, stack)?;
        }
        stack.pop();
        marks.insert(id.to_string(), Mark::Done);
        Ok(())
    }

    let mut marks = HashMap::new();
    let mut stack = Vec::new();
    for id in steps.keys() {
        visit(id, steps, &mut marks, &mut stack)?;
    }
    Ok(())
}

/// `on` is workspace placement, `needs` is causal order — but placing a step on a workstream it
/// never causally waits for was never valid: it could be sent to a branch that isn't even
/// spawned yet, or isn't done with whatever it was doing. Requires the `on` target to be
/// present in the step's `needs`, directly or transitively (e.g. `docs` doesn't need
/// `implement` directly, but does via `needs: [review]` where `review` needs `implement` —
/// that transitive reach is what makes `docs: { on: implement }` valid).
fn detect_on_without_needs(steps: &BTreeMap<String, Step>) -> Result<(), RecipeError> {
    let mut memo = HashMap::new();
    for step in steps.values() {
        if let Some(on) = &step.on {
            let closure = needs_closure(&step.id, steps, &mut memo);
            if !closure.contains(on) {
                return Err(RecipeError::OnNotInNeeds {
                    step: step.id.clone(),
                    target: on.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Every step `id` waits for, directly or transitively through `needs`.
fn needs_closure(
    id: &str,
    steps: &BTreeMap<String, Step>,
    memo: &mut HashMap<String, HashSet<String>>,
) -> HashSet<String> {
    if let Some(cached) = memo.get(id) {
        return cached.clone();
    }
    let mut closure = HashSet::new();
    for need in &steps[id].needs {
        closure.insert(need.clone());
        for transitive in needs_closure(need, steps, memo) {
            closure.insert(transitive);
        }
    }
    memo.insert(id.to_string(), closure.clone());
    closure
}

/// `spawn: continue` keeps whatever agent is in the branch's pane, so it must be the step's
/// own. The agent there by then is the one of the latest steps it waits for that ran on the
/// same branch: those in its `needs` closure that resolve to its branch and that no other such
/// step waits for. If any of them is a different agent, `continue` would hand this step's
/// prompt to it.
///
/// This only sees what `needs` orders. A sibling on the same branch that isn't ordered against
/// the step can still run between them, which the runner catches at launch instead.
fn detect_continue_agent_mismatch(recipe: &Recipe) -> Result<(), RecipeError> {
    let mut memo = HashMap::new();
    for step in recipe.steps.values().filter(|s| s.spawn == Some(SpawnMode::Continue)) {
        let branch = resolve_workstream(recipe, &step.id);
        let mut before: Vec<&str> = needs_closure(&step.id, &recipe.steps, &mut memo)
            .into_iter()
            .filter(|id| resolve_workstream(recipe, id) == branch)
            .map(|id| recipe.steps[&id].id.as_str())
            .collect();
        before.sort();
        for &latest in &before {
            let superseded = before
                .iter()
                .any(|&other| other != latest && needs_closure(other, &recipe.steps, &mut memo).contains(latest));
            let agent = &recipe.steps[latest].agent;
            if !superseded && *agent != step.agent {
                return Err(RecipeError::ContinueAgentMismatch {
                    step: step.id.clone(),
                    agent: step.agent.clone(),
                    branch: branch.clone(),
                    branch_agent: agent.clone(),
                });
            }
        }
    }
    Ok(())
}

fn detect_on_cycle(steps: &BTreeMap<String, Step>) -> Result<(), RecipeError> {
    for start in steps.keys() {
        let mut seen: HashSet<&str> = HashSet::new();
        seen.insert(start.as_str());
        let mut chain = vec![start.clone()];
        let mut current = start.as_str();
        while let Some(next) = steps[current].on.as_deref() {
            chain.push(next.to_string());
            if !seen.insert(next) {
                return Err(RecipeError::OnCycle(chain));
            }
            current = next;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_valid_minimal_recipe() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\nBody text\n",
        )
        .unwrap();
        assert_eq!(recipe.steps.len(), 1);
        let a = &recipe.steps["a"];
        assert_eq!(a.agent, "codex");
        assert_eq!(a.prompt, "Do A");
        assert_eq!(recipe.context, "Body text");
    }

    #[test]
    fn extracts_markdown_body_and_builds_effective_prompt() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n---\n\n# Context\n\nShared notes.\n",
        )
        .unwrap();
        assert_eq!(recipe.context, "# Context\n\nShared notes.");
        assert_eq!(
            recipe.effective_prompt("a"),
            "Do A\n\n## Recipe context\n\n# Context\n\nShared notes."
        );
    }

    #[test]
    fn populates_every_optional_field() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    model: gpt-5.6\n    effort: high\n    needs: [implement]\n    on: implement\n    prompt: Review\n    owns:\n      - src/**\n    verify:\n      - cargo test\n---\nctx\n",
        )
        .unwrap();
        let review = &recipe.steps["review"];
        assert_eq!(review.model.as_deref(), Some("gpt-5.6"));
        assert_eq!(review.effort, Some(Effort::High));
        assert_eq!(review.needs, vec!["implement".to_string()]);
        assert_eq!(review.on.as_deref(), Some("implement"));
        assert_eq!(review.owns, vec!["src/**".to_string()]);
        assert_eq!(review.verify, vec!["cargo test".to_string()]);
    }

    #[test]
    fn retries_fall_back_from_step_to_recipe_to_the_default() {
        let source = "---\nversion: 1\nretries: 1\nsteps:\n  a:\n    agent: codex\n    retries: 4\n    prompt: A\n  b:\n    agent: codex\n    prompt: B\n---\n";
        parse(source).unwrap();
        let budgets = parse_retries(source).unwrap();
        assert_eq!((budgets.budget("a", 2), budgets.budget("b", 2)), (4, 1));

        let budgets = parse_retries("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
        assert_eq!(budgets.budget("a", 2), 2);
        assert!(parse("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    retries: -1\n    prompt: A\n---\n").is_err());
    }

    #[test]
    fn durations_read_whole_units_largest_first() {
        let secs = |s: &str| parse_duration(s).map(|d| d.as_secs());
        assert_eq!((secs("90s"), secs("45m"), secs("2h"), secs("1h30m"), secs(" 2h5s ")), (Some(90), Some(2700), Some(7200), Some(5400), Some(7205)));
        for bad in ["", "45", "m", "1.5h", "30m1h", "1h1h", "0m", "45 m", "-1m", "1d"] {
            assert_eq!(secs(bad), None, "{bad:?}");
        }
        for text in ["30s", "1m30s", "45m", "2h", "1h30m", "2h5s"] {
            assert_eq!(format_duration(parse_duration(text).unwrap()), text);
        }
        assert_eq!(format_duration(parse_duration("90s").unwrap()), "1m30s");
    }

    #[test]
    fn timeouts_fall_back_from_step_to_recipe_to_the_default() {
        let source = "---\nversion: 1\ntimeout: 2h\nsteps:\n  a:\n    agent: codex\n    timeout: 45m\n    prompt: A\n  b:\n    agent: codex\n    prompt: B\n---\n";
        parse(source).unwrap();
        let timeouts = parse_timeouts(source).unwrap();
        let default = Duration::from_secs(3600);
        assert_eq!((timeouts.timeout("a", default).as_secs(), timeouts.timeout("b", default).as_secs()), (2700, 7200));
        let none = parse_timeouts("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
        assert_eq!(none.timeout("a", default), default);

        let err = parse("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    timeout: 45\n    prompt: A\n---\n").unwrap_err();
        assert!(matches!(&err, RecipeError::InvalidTimeout { step: Some(s), value } if s == "a" && value == "45"), "{err:?}");
        assert!(err.to_string().contains("like 90s, 45m, 2h"), "{err}");
        let err = parse("---\nversion: 1\ntimeout: soon\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap_err();
        assert!(matches!(&err, RecipeError::InvalidTimeout { step: None, .. }), "{err:?}");
    }

    #[test]
    fn rejects_malformed_front_matter_missing_delimiters() {
        let err = parse("version: 1\nsteps: {}\n").unwrap_err();
        assert!(matches!(err, RecipeError::Malformed(_)));

        let err = parse("---\nversion: 1\nsteps: {}\n").unwrap_err();
        assert!(matches!(err, RecipeError::Malformed(_)));
    }

    #[test]
    fn rejects_unparsable_yaml() {
        let err = parse("---\n: not: valid: yaml:\n---\nbody\n").unwrap_err();
        assert!(matches!(err, RecipeError::Malformed(_)));
    }

    #[test]
    fn rejects_invalid_effort() {
        let err = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: Do A\n    effort: extreme\n---\nbody\n",
        )
        .unwrap_err();
        assert!(matches!(err, RecipeError::InvalidEffort { .. }));
    }

    #[test]
    fn rejects_unknown_dependency() {
        let err = parse(
            "---\nversion: 1\nsteps:\n  review:\n    agent: claude\n    prompt: Review\n    needs: [implementt]\n---\nbody\n",
        )
        .unwrap_err();
        match err {
            RecipeError::UnknownNeeds { step, target } => {
                assert_eq!(step, "review");
                assert_eq!(target, "implementt");
            }
            other => panic!("expected UnknownNeeds, got {other:?}"),
        }
    }

    #[test]
    fn rejects_needs_cycle() {
        let err = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n    needs: [b]\n  b:\n    agent: codex\n    prompt: B\n    needs: [a]\n---\nbody\n",
        )
        .unwrap_err();
        assert!(matches!(err, RecipeError::NeedsCycle(_)));
    }

    #[test]
    fn rejects_invalid_on_reference() {
        let err = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n    on: nope\n---\nbody\n",
        )
        .unwrap_err();
        assert!(matches!(err, RecipeError::UnknownOn { .. }));
    }

    #[test]
    fn rejects_on_target_not_in_needs() {
        // review is placed `on: implement` but never `needs` it, directly or transitively --
        // `on` is placement, not a substitute for causal ordering.
        let err = parse(
            "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    on: implement\n    prompt: Review\n---\nbody\n",
        )
        .unwrap_err();
        match err {
            RecipeError::OnNotInNeeds { step, target } => {
                assert_eq!(step, "review");
                assert_eq!(target, "implement");
            }
            other => panic!("expected OnNotInNeeds, got {other:?}"),
        }
    }

    #[test]
    fn accepts_on_target_reached_transitively_through_needs() {
        // docs needs review, review needs implement -- docs is `on: implement`, which is not a
        // *direct* need of docs but is reachable transitively, matching the acceptance recipe's
        // shape (implement -> review -> docs, all placed on implement).
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: Review\n  docs:\n    agent: codex\n    needs: [review]\n    on: implement\n    prompt: Docs\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(recipe.steps["docs"].on.as_deref(), Some("implement"));
    }

    #[test]
    fn rejects_on_placement_cycle() {
        let err = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n    on: b\n  b:\n    agent: codex\n    prompt: B\n    on: a\n---\nbody\n",
        )
        .unwrap_err();
        assert!(matches!(err, RecipeError::OnCycle(_)));
    }

    #[test]
    fn rejects_missing_agent_and_prompt() {
        let err =
            parse("---\nversion: 1\nsteps:\n  a:\n    prompt: A\n---\nbody\n").unwrap_err();
        assert!(matches!(err, RecipeError::MissingAgent(_)));

        let err =
            parse("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n---\nbody\n").unwrap_err();
        assert!(matches!(err, RecipeError::MissingPrompt(_)));
    }

    #[test]
    fn rejects_unsupported_version_and_empty_steps() {
        let err = parse("---\nversion: 2\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\nbody\n")
            .unwrap_err();
        assert!(matches!(err, RecipeError::UnsupportedVersion(2)));

        let err = parse("---\nversion: 1\nsteps: {}\n---\nbody\n").unwrap_err();
        assert!(matches!(err, RecipeError::NoSteps));
    }

    #[test]
    fn validates_the_acceptance_recipe() {
        let source = include_str!("../../../examples/recipes/implement-review-docs.md");
        let recipe = parse(source).expect("acceptance recipe should parse cleanly");
        assert_eq!(recipe.steps.len(), 3);
        assert!(recipe.steps.contains_key("implement"));
        assert!(recipe.steps.contains_key("review"));
        assert!(recipe.steps.contains_key("docs"));
        assert_eq!(recipe.steps["review"].on.as_deref(), Some("implement"));
        assert_eq!(recipe.steps["docs"].on.as_deref(), Some("implement"));
        assert_eq!(recipe.steps["review"].needs, vec!["implement".to_string()]);
        assert_eq!(recipe.steps["docs"].needs, vec!["review".to_string()]);
        assert_eq!(recipe.steps["review"].spawn, Some(SpawnMode::Handoff));
        assert_eq!(recipe.steps["review"].done_when, DoneWhen::Either);
    }

    /// `implement` (codex), then `review` (claude) on its branch with `spawn`/`done_when`
    /// lines spliced in from `review_extra`.
    fn with_review(review_extra: &str) -> Result<Recipe, RecipeError> {
        parse(&format!(
            "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: I\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: R\n{review_extra}---\nctx\n"
        ))
    }

    #[test]
    fn parses_spawn_and_done_when_with_their_defaults() {
        let recipe = with_review("    spawn: handoff\n    done_when: either\n").unwrap();
        assert_eq!(recipe.steps["review"].spawn, Some(SpawnMode::Handoff));
        assert_eq!(recipe.steps["review"].done_when, DoneWhen::Either);
        assert_eq!((recipe.steps["implement"].spawn, recipe.steps["implement"].done_when), (None, DoneWhen::Commits));

        let recipe = parse("---\nversion: 1\nsteps:\n  survey:\n    agent: claude\n    spawn: advisory\n    prompt: S\n---\n").unwrap();
        assert_eq!(recipe.steps["survey"].done_when, DoneWhen::Note, "advisory defaults to a note");
        let recipe = with_review("    spawn: stack\n").unwrap();
        assert_eq!(recipe.steps["review"].done_when, DoneWhen::Commits);
    }

    #[test]
    fn rejects_an_unknown_spawn_mode_and_done_when() {
        for mode in ["share", "fork"] {
            let err = with_review(&format!("    spawn: {mode}\n")).unwrap_err();
            assert!(matches!(&err, RecipeError::InvalidSpawn { step, value } if step == "review" && value == mode), "{err:?}");
            assert!(err.to_string().contains("expected continue, handoff, stack or advisory"), "{err}");
        }
        let err = with_review("    done_when: idle\n").unwrap_err();
        assert!(matches!(&err, RecipeError::InvalidDoneWhen { value, .. } if value == "idle"), "{err:?}");
    }

    #[test]
    fn continue_handoff_and_stack_need_on() {
        for mode in ["continue", "handoff", "stack"] {
            let err = parse(&format!("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    spawn: {mode}\n    prompt: A\n---\n")).unwrap_err();
            assert!(matches!(&err, RecipeError::SpawnNeedsOn { step, mode: m } if step == "a" && m.as_str() == mode), "{err:?}");
            assert!(err.to_string().contains(&format!("`spawn: {mode}` but no `on`")), "{err}");
        }
    }

    #[test]
    fn rejects_advisory_with_on() {
        let err = with_review("    spawn: advisory\n").unwrap_err();
        assert!(matches!(err, RecipeError::AdvisoryWithOn { .. }), "{err:?}");
    }

    #[test]
    fn rejects_continue_with_a_different_agent_and_suggests_handoff() {
        let err = with_review("    spawn: continue\n").unwrap_err();
        match &err {
            RecipeError::ContinueAgentMismatch { step, agent, branch, branch_agent } => {
                assert_eq!((step.as_str(), agent.as_str(), branch.as_str(), branch_agent.as_str()), ("review", "claude", "implement", "codex"));
            }
            other => panic!("expected ContinueAgentMismatch, got {other:?}"),
        }
        assert!(err.to_string().contains("use `spawn: handoff`"), "{err}");
    }

    /// The agent `continue` meets is the latest one on the branch, not the `on` target's:
    /// after review hands `implement` to claude, codex can't continue there, but claude can.
    #[test]
    fn continue_is_checked_against_the_branchs_latest_agent() {
        let recipe = |docs_agent: &str| {
            parse(&format!(
                "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: I\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    spawn: handoff\n    prompt: R\n  docs:\n    agent: {docs_agent}\n    needs: [review]\n    on: implement\n    spawn: continue\n    prompt: D\n---\n"
            ))
        };
        let err = recipe("codex").unwrap_err();
        assert!(matches!(&err, RecipeError::ContinueAgentMismatch { branch_agent, .. } if branch_agent == "claude"), "{err:?}");
        recipe("claude").expect("claude continues claude's pane");

        // A stack step's branch is its own, so continuing on it meets the stack step's agent.
        parse("---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: I\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    spawn: stack\n    prompt: R\n  fix:\n    agent: claude\n    needs: [review]\n    on: review\n    spawn: continue\n    prompt: F\n---\n")
            .expect("fix continues review's claude pane on implement-review");
    }

    #[test]
    fn reads_description_and_tags_and_rejects_a_multi_line_description() {
        let recipe = parse("---\nversion: 1\ndescription: >\n  Fix a small bug\ntags: [bug, \" quick \"]\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
        assert_eq!((recipe.description.as_deref(), recipe.tags), (Some("Fix a small bug"), vec!["bug".to_string(), "quick".to_string()]));

        let bare = parse("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
        assert_eq!((bare.description, bare.tags), (None, Vec::new()));

        let err = parse("---\nversion: 1\ndescription: |\n  One\n  Two\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap_err();
        assert!(matches!(err, RecipeError::MultiLineDescription), "{err}");
        assert!(err.to_string().contains("keep it to one"), "{err}");
        assert!(parse_meta("---\nversion: 1\ndescription: \"a\\nb\"\n---\n").is_err());

        let meta = parse_meta("---\nversion: 1\ndescription: Listed anyway\nsteps:\n  a: {}\n  b: {}\n---\n").unwrap();
        assert_eq!(meta, Meta { description: Some("Listed anyway".to_string()), tags: Vec::new(), steps: 2 });
    }

    #[test]
    fn launch_prompt_asks_every_step_for_a_report_whatever_its_done_when() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    done_when: note\n    prompt: B\n  c:\n    agent: codex\n    done_when: either\n    prompt: C\n---\nctx\n",
        )
        .unwrap();
        let kanstack = Path::new("/opt/dev/kanstack");
        assert_eq!(
            recipe.launch_prompt("a", "a", kanstack),
            "A\n\n## Recipe context\n\nctx\n\nWhen you're finished, report back by running: /opt/dev/kanstack note a \"<report>\"\n\
             Make it a real report, not a one-liner: what changed; what you verified, and how; what you didn't do or \
             couldn't confirm; caveats; and follow-ups worth proposing."
        );
        assert!(recipe.launch_prompt("b", "b", kanstack).contains("/opt/dev/kanstack note b \"<report>\"\n"));
        assert!(recipe.launch_prompt("c", "lane-c", kanstack).contains("/opt/dev/kanstack note lane-c \"<report>\"\n"));
        assert!(recipe
            .launch_prompt("c", "lane-c", Path::new("/My Tools/kanstack"))
            .contains("'/My Tools/kanstack' note lane-c \"<report>\"\n"));
    }

    #[test]
    fn shell_word_quotes_only_what_needs_it() {
        assert_eq!(shell_word(Path::new("/usr/local/bin/kanstack")), "/usr/local/bin/kanstack");
        assert_eq!(shell_word(Path::new("/a b/it's")), "'/a b/it'\\''s'");
    }
}
