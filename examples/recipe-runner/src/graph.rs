//! Pure scheduling logic over a [`Recipe`]'s `needs`/`on` graph — no process, no I/O. Four
//! states per step, tracked by the caller (`run.rs`): `pending`, `running`, `complete`,
//! `failed`.

use std::collections::{HashMap, HashSet};

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
///
/// [`crate::recipe::parse`] already rejects an `on` cycle at validation time, so a *parsed*
/// recipe can never reach one here. But `Recipe`/`Step` are public, plain data — a caller using
/// this crate as a library can build one by hand and skip `parse()` entirely — so this still
/// guards against looping forever on a hand-built cyclic `on` chain: it stops and returns the
/// first repeated step id once it revisits one, rather than hanging.
pub fn resolve_workstream(recipe: &Recipe, step_id: &str) -> String {
    let mut seen = HashSet::new();
    let mut current = step_id.to_string();
    seen.insert(current.clone());
    loop {
        match recipe.steps.get(&current).and_then(|s| s.on.clone()) {
            Some(next) => {
                if !seen.insert(next.clone()) {
                    return current;
                }
                current = next;
            }
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
    fn resolve_workstream_does_not_hang_on_a_hand_built_cyclic_on_chain() {
        // parse() rejects this shape, so build it by hand — a library caller who skips
        // parse() is exactly who the guard in resolve_workstream protects.
        use crate::recipe::Step;
        use std::collections::BTreeMap;

        let mut steps = BTreeMap::new();
        steps.insert(
            "a".to_string(),
            Step {
                id: "a".to_string(),
                agent: "codex".to_string(),
                model: None,
                effort: None,
                prompt: "A".to_string(),
                needs: vec![],
                on: Some("b".to_string()),
                owns: vec![],
                verify: vec![],
            },
        );
        steps.insert(
            "b".to_string(),
            Step {
                id: "b".to_string(),
                agent: "codex".to_string(),
                model: None,
                effort: None,
                prompt: "B".to_string(),
                needs: vec![],
                on: Some("a".to_string()),
                owns: vec![],
                verify: vec![],
            },
        );
        let recipe = Recipe {
            steps,
            context: String::new(),
        };

        // Must return promptly rather than looping forever.
        let resolved = resolve_workstream(&recipe, "a");
        assert!(resolved == "a" || resolved == "b");
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
