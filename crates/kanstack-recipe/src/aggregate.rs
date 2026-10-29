//! The `aggregate` step: a recipe's closing step whose prompt carries the run's reports and asks
//! its agent to draft follow-up work with `kanstack-recipe items propose --from <item>`.
//!
//! A recipe names it with `aggregate: <step-id>` (see [`crate::recipe::Recipe::aggregate`]).
//! Parsing makes it need every other step and defaults it to `spawn: advisory` and
//! `done_when: note`. Two things happen here instead, because they need the run:
//!
//! - **When it runs** ([`runnable_steps`]): once nothing else can, even when a step failed. A
//!   failed step never frees its dependents, so through `needs` alone the aggregate would only
//!   run after a clean run, and never hear about the failures it's most useful for. The run
//!   still ends `failed`.
//! - **What it's told** ([`launch_prompt`]): every other step's outcome from run state, then
//!   how to propose follow-ups for the run's item, checking the queue first so it doesn't
//!   propose what's already there. The factory dedupes nothing itself. A plain `run` has no
//!   item to propose for, so it's asked for a summary note instead.

use std::collections::HashMap;

use crate::graph::{self, StepState};
use crate::recipe::Recipe;
use crate::state::{RunState, StepRecord, StepStatus};

/// [`graph::runnable_steps`], plus the recipe's aggregate step once no other step is in flight
/// or runnable: the ones still `pending` then are waiting on a failed step, forever.
pub fn runnable_steps(recipe: &Recipe, states: &HashMap<String, StepState>) -> Vec<String> {
    let mut runnable = graph::runnable_steps(recipe, states);
    let Some(aggregate) = &recipe.aggregate else { return runnable };
    let state = |id: &str| states.get(id).copied().unwrap_or(StepState::Pending);
    let settled = runnable.is_empty() && recipe.steps.keys().all(|id| id == aggregate || state(id) != StepState::Running);
    if settled && state(aggregate) == StepState::Pending {
        runnable.push(aggregate.clone());
    }
    runnable
}

/// What reaches the harness for `step_id` on `branch`: [`Recipe::launch_prompt`], except for
/// the aggregate step, whose effective prompt is followed by [`outcomes_block`].
pub fn launch_prompt(recipe: &Recipe, run: &RunState, step_id: &str, branch: &str) -> String {
    if recipe.aggregate.as_deref() != Some(step_id) {
        return recipe.launch_prompt(step_id, branch);
    }
    format!("{}\n\n{}", recipe.effective_prompt(step_id), outcomes_block(recipe, run, branch))
}

/// Every step's outcome but the aggregate's, then what to do with them: propose follow-ups
/// for the run's item and report with `kanstack note <branch>`, or, with no item, just report
/// a summary.
pub fn outcomes_block(recipe: &Recipe, run: &RunState, branch: &str) -> String {
    let mut block = String::from("## How the run went\n\nEvery other step of this run has finished. Here is what each one left.\n");
    for (id, step) in &recipe.steps {
        if recipe.aggregate.as_deref() == Some(id.as_str()) {
            continue;
        }
        let default = StepRecord::default();
        let rec = run.steps.get(id).unwrap_or(&default);
        let retried = rec.retry_label().map(|l| format!(" ({l})")).unwrap_or_default();
        block.push_str(&format!("\n### {id}: {}{retried}\n\n", outcome_label(rec.status)));
        if rec.status == StepStatus::Pending {
            let unfinished: Vec<String> = step
                .needs
                .iter()
                .filter(|need| run.steps.get(*need).map_or(true, |r| r.status != StepStatus::Done))
                .map(|need| format!("`{need}`"))
                .collect();
            block.push_str(&format!("It never ran: it needs {}, which didn't finish.\n", unfinished.join(", ")));
            continue;
        }
        if let Some(branch) = &rec.branch {
            block.push_str(&format!("{}\n", commit_range(branch, rec)));
        }
        if rec.status == StepStatus::Failed {
            block.push_str(&format!("\nIt failed: {}\n", rec.last_error.as_deref().unwrap_or("no reason was recorded")));
        }
        match &rec.report {
            Some(report) => block.push_str(&format!("\nIts report:\n\n{}\n", report.trim())),
            None if rec.status == StepStatus::Done => block.push_str("\nIt left no report.\n"),
            None => {}
        }
    }

    match &run.item {
        Some(item) => block.push_str(&format!(
            "\n## Follow-ups\n\n\
             This run works on item {item}. Turn anything it left undone, or turned up, into proposed follow-ups for it:\n\n\
             1. First run `kanstack-recipe items list --json` to see what's already queued, and don't propose anything \
             that's already there.\n\
             2. Then, for each follow-up worth doing, run `kanstack-recipe items propose --from {item} --title \"<title>\" \
             --body \"<what to do and why>\"` once. If nothing is worth doing, propose nothing.\n\
             3. When you're finished, report what you proposed by running: kanstack note {branch} \"<report>\"\n"
        )),
        None => block.push_str(&format!(
            "\n## Summary\n\n\
             This run isn't working on a queue item, so don't propose follow-ups. When you're finished, report a summary \
             of the run, and anything worth following up on, by running: kanstack note {branch} \"<summary>\"\n"
        )),
    }
    block
}

fn outcome_label(status: StepStatus) -> &'static str {
    match status {
        StepStatus::Done => "done",
        StepStatus::Failed => "failed",
        StepStatus::Pending => "never ran",
        // Not reachable through `runnable_steps`, but a hand-built run can say anything.
        StepStatus::Launching | StepStatus::Running | StepStatus::Verifying => "still running",
    }
}

/// Where a step left its branch: the commits it added, as `base..tip`, or none.
fn commit_range(branch: &str, rec: &StepRecord) -> String {
    let short = |sha: &str| sha.chars().take(7).collect::<String>();
    match (&rec.base, &rec.tip) {
        (Some(base), Some(tip)) if base != tip => format!("Commits `{}..{}` on `{branch}`.", short(base), short(tip)),
        (None, Some(tip)) => format!("Commits up to `{}` on `{branch}`.", short(tip)),
        _ => format!("No commits on `{branch}`."),
    }
}
