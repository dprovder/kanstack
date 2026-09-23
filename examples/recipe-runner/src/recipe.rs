//! Parsing and validation for the v1 recipe format: a Markdown file whose front matter (YAML,
//! between a leading `---` pair) declares a step DAG, and whose body is shared context text
//! appended to every step's own prompt. See this crate's README for the field semantics; this
//! module only implements them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use serde::Deserialize;

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
}

#[derive(Debug, Clone)]
pub struct Recipe {
    pub steps: BTreeMap<String, Step>,
    pub context: String,
}

impl Recipe {
    /// `<step prompt>\n\n## Recipe context\n\n<markdown body>` — the one deterministic
    /// composition, no conditional templating.
    pub fn effective_prompt(&self, step_id: &str) -> String {
        let step = &self.steps[step_id];
        format!("{}\n\n## Recipe context\n\n{}", step.prompt, self.context)
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
        }
    }
}

impl std::error::Error for RecipeError {}

#[derive(Debug, Deserialize)]
struct RawFrontMatter {
    version: u64,
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

        steps.insert(
            id.clone(),
            Step {
                id: id.clone(),
                agent,
                model: raw_step.model,
                effort,
                prompt,
                needs: raw_step.needs,
                on: raw_step.on,
                owns: raw_step.owns,
                verify: raw_step.verify,
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

    Ok(Recipe {
        steps,
        context: body.to_string(),
    })
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
        let source = include_str!("../../recipes/implement-review-docs.md");
        let recipe = parse(source).expect("acceptance recipe should parse cleanly");
        assert_eq!(recipe.steps.len(), 3);
        assert!(recipe.steps.contains_key("implement"));
        assert!(recipe.steps.contains_key("review"));
        assert!(recipe.steps.contains_key("docs"));
        assert_eq!(recipe.steps["review"].on.as_deref(), Some("implement"));
        assert_eq!(recipe.steps["docs"].on.as_deref(), Some("implement"));
        assert_eq!(recipe.steps["review"].needs, vec!["implement".to_string()]);
        assert_eq!(recipe.steps["docs"].needs, vec!["review".to_string()]);
    }
}
