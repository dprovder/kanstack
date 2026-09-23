//! Pure scheduling logic over a [`Recipe`]'s `needs`/`on` graph — no process, no I/O. Four
//! states per step, tracked by the caller (`run.rs`): `pending`, `running`, `complete`,
//! `failed`.

use std::collections::HashMap;

use crate::recipe::Recipe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Pending,
    Running,
    Complete,
    Failed,
}

/// Every `pending` step whose `needs` are all `complete` — runnable right now.
pub fn runnable_steps(recipe: &Recipe, states: &HashMap<String, StepState>) -> Vec<String> {
    recipe
        .steps
        .values()
        .filter(|step| {
            states.get(&step.id).copied().unwrap_or(StepState::Pending) == StepState::Pending
        })
        .filter(|step| {
            step.needs
                .iter()
                .all(|need| states.get(need).copied() == Some(StepState::Complete))
        })
        .map(|step| step.id.clone())
        .collect()
}

/// Follows a step's `on` chain to the step that actually owns the workstream (the one with no
/// `on` of its own) — the branch name a step's work should land on.
pub fn resolve_workstream(recipe: &Recipe, step_id: &str) -> String {
    let mut current = step_id.to_string();
    loop {
        match recipe.steps.get(&current).and_then(|s| s.on.clone()) {
            Some(next) => current = next,
            None => return current,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::parse;

    fn states(pairs: &[(&str, StepState)]) -> HashMap<String, StepState> {
        pairs.iter().map(|(id, s)| (id.to_string(), *s)).collect()
    }

    #[test]
    fn a_alone() {
        let recipe = parse("---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n---\n").unwrap();
        let st = states(&[("a", StepState::Pending)]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["a".to_string()]);

        let st = states(&[("a", StepState::Complete)]);
        assert!(runnable_steps(&recipe, &st).is_empty());
    }

    #[test]
    fn a_then_b() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: B\n---\n",
        )
        .unwrap();

        let st = states(&[("a", StepState::Pending), ("b", StepState::Pending)]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["a".to_string()]);

        let st = states(&[("a", StepState::Complete), ("b", StepState::Pending)]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["b".to_string()]);

        let st = states(&[("a", StepState::Complete), ("b", StepState::Complete)]);
        assert!(runnable_steps(&recipe, &st).is_empty());
    }

    #[test]
    fn a_then_b_and_c() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: B\n  c:\n    agent: codex\n    needs: [a]\n    prompt: C\n---\n",
        )
        .unwrap();

        let st = states(&[
            ("a", StepState::Pending),
            ("b", StepState::Pending),
            ("c", StepState::Pending),
        ]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["a".to_string()]);

        let st = states(&[
            ("a", StepState::Complete),
            ("b", StepState::Pending),
            ("c", StepState::Pending),
        ]);
        let mut runnable = runnable_steps(&recipe, &st);
        runnable.sort();
        assert_eq!(runnable, vec!["b".to_string(), "c".to_string()]);

        let st = states(&[
            ("a", StepState::Complete),
            ("b", StepState::Running),
            ("c", StepState::Complete),
        ]);
        assert!(runnable_steps(&recipe, &st).is_empty());
    }

    #[test]
    fn a_then_b_then_c() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: B\n  c:\n    agent: codex\n    needs: [b]\n    prompt: C\n---\n",
        )
        .unwrap();

        let st = states(&[
            ("a", StepState::Pending),
            ("b", StepState::Pending),
            ("c", StepState::Pending),
        ]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["a".to_string()]);

        let st = states(&[
            ("a", StepState::Complete),
            ("b", StepState::Pending),
            ("c", StepState::Pending),
        ]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["b".to_string()]);

        let st = states(&[
            ("a", StepState::Complete),
            ("b", StepState::Complete),
            ("c", StepState::Pending),
        ]);
        assert_eq!(runnable_steps(&recipe, &st), vec!["c".to_string()]);

        let st = states(&[
            ("a", StepState::Complete),
            ("b", StepState::Complete),
            ("c", StepState::Complete),
        ]);
        assert!(runnable_steps(&recipe, &st).is_empty());
    }

    #[test]
    fn a_failed_never_frees_its_dependent() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  a:\n    agent: codex\n    prompt: A\n  b:\n    agent: codex\n    needs: [a]\n    prompt: B\n---\n",
        )
        .unwrap();
        let st = states(&[("a", StepState::Failed), ("b", StepState::Pending)]);
        assert!(runnable_steps(&recipe, &st).is_empty());
    }

    #[test]
    fn resolve_workstream_follows_on_chain() {
        let recipe = parse(
            "---\nversion: 1\nsteps:\n  implement:\n    agent: codex\n    prompt: Implement\n  review:\n    agent: claude\n    needs: [implement]\n    on: implement\n    prompt: Review\n  docs:\n    agent: codex\n    needs: [review]\n    on: implement\n    prompt: Docs\n---\n",
        )
        .unwrap();
        assert_eq!(resolve_workstream(&recipe, "implement"), "implement");
        assert_eq!(resolve_workstream(&recipe, "review"), "implement");
        assert_eq!(resolve_workstream(&recipe, "docs"), "implement");
    }
}
