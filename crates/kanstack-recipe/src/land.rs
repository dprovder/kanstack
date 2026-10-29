//! `land <item>`: lands an item's branches once it's in review, bottom of the stack first, with
//! `but branch show --check` before each `but land`, then marks the item done.
//!
//! This is light mode, the default (`KANSTACK_FACTORY` unset or `light`): an item that passed
//! its verify gate waits in `review` until a person runs this. Nothing else lands anything, and
//! `serve` never calls it. Dark mode, where a reviewer agent's approval lands the item, comes
//! later.
//!
//! **Which branches.** The item's newest completed run (runs link to items through
//! [`RunState::item`]) records the branch each step used. A `spawn: stack` step's branch sits
//! above its `on:` step's branch, so the branches form stacks, and they're landed bottom first:
//! `but land` refuses a branch that's stacked on one that hasn't landed yet. A branch with no
//! commits ahead of the target (an advisory or review lane that only reported) has nothing to
//! land and is skipped.
//!
//! **How.** `KANSTACK_LAND`, or the recipe's own `land:`, which wins:
//!
//! - `direct` (the default): each branch in turn gets `but branch show <b> --check --json` and,
//!   if it merges cleanly, `but land <b> --yes --json`, the same calls kanstack's board makes
//!   (`src/but/land.rs`). The first branch that wouldn't land cleanly stops it, with nothing
//!   after it landed; the item stays in `review` with a comment saying where and why. Once all
//!   have landed, a comment lists them, the item is set `done`, and each of its lanes gets
//!   `kanstack stop` unless `--keep-panes` is given.
//! - `pr`: one `but pr new <top> -t --json` for the top branch of each stack (`but` pushes the
//!   branches below it and opens their reviews too), with the link as a comment. The item stays
//!   in `review`: following the PR until it merges isn't built yet, and the comment says so.
//!
//! Exit codes match `items`: 0 landed (or the PR opened), 2 a usage error, 1 anything else,
//! including a landing that stopped partway. `--json` prints one `{"schema": 1, ...}` document
//! on stdout for a landing that was attempted, whether or not it got all the way; an item that
//! can't be landed at all is plain text on stderr in both modes.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use kanstack_tracker::{ItemId, ItemState, Tracker};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::kanstack::KanstackClient;
use crate::recipe::{self, SpawnMode};
use crate::state::{RunState, RunStatus, RunStore};

/// Versions every `land --json` document.
const SCHEMA: u32 = 1;

/// How an item's branches land (`KANSTACK_LAND`, or a recipe's `land:`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LandMode {
    /// `but land` each branch onto the target, bottom first.
    #[default]
    Direct,
    /// Open a PR for the top branch and leave the item in review.
    Pr,
}

impl LandMode {
    pub fn parse(word: &str) -> Option<LandMode> {
        match word.trim() {
            "direct" => Some(LandMode::Direct),
            "pr" => Some(LandMode::Pr),
            _ => None,
        }
    }
}

/// Who lands an item once it's in review (`KANSTACK_FACTORY`, or a recipe's `factory:`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Factory {
    /// A person, by running `land`.
    #[default]
    Light,
    /// A reviewer agent. Not built yet, so `land` refuses it rather than pretend.
    Dark,
}

impl Factory {
    pub fn parse(word: &str) -> Option<Factory> {
        match word.trim() {
            "light" => Some(Factory::Light),
            "dark" => Some(Factory::Dark),
            _ => None,
        }
    }
}

/// What `land` does, before the recipe has had its say.
#[derive(Debug, Clone, Default)]
pub struct LandOptions {
    pub json: bool,
    /// `--keep-panes`: leave the item's lanes running after a direct land.
    pub keep_panes: bool,
    /// From `KANSTACK_LAND`; the recipe's `land:` overrides it.
    pub land: LandMode,
    /// From `KANSTACK_FACTORY`; the recipe's `factory:` overrides it.
    pub factory: Factory,
}

/// Everything `land` talks to: the seam tests drive, since the tracker, run store and both
/// binaries would otherwise all come from the environment.
pub struct Lander<'a> {
    pub repo: &'a Path,
    pub tracker: &'a dyn Tracker,
    pub store: &'a RunStore,
    pub kanstack: &'a KanstackClient,
    pub but: &'a But,
}

/// The `but` binary, run in the repository. Only the three calls landing needs.
pub struct But {
    bin: PathBuf,
}

/// `but branch show <b> --check --json`. Mixed casing on the wire, as kanstack's own `MergeCheck`
/// (`src/model.rs`) notes: camelCase at the top and in `mergeCheck`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeCheck {
    commits_ahead: usize,
    merge_check: MergeResult,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeResult {
    merges_cleanly: bool,
    #[serde(default)]
    conflicting_files: Vec<ConflictingFile>,
}

#[derive(Debug, Deserialize)]
struct ConflictingFile {
    path: String,
}

impl But {
    pub fn new(bin: impl Into<PathBuf>) -> Self {
        But { bin: bin.into() }
    }

    /// `but <args>` in `repo`: its stdout, or why it failed.
    fn run(&self, repo: &Path, args: &[&str]) -> Result<String, String> {
        let output = Command::new(&self.bin)
            .args(args)
            .current_dir(repo)
            .output()
            .map_err(|e| format!("could not run {}: {e}", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if output.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let why = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        Err(format!("`but {}` failed (exit {:?}): {why}", args.join(" "), output.status.code()))
    }

    fn merge_check(&self, repo: &Path, branch: &str) -> Result<MergeCheck, String> {
        let raw = self.run(repo, &["branch", "show", branch, "--check", "--json"])?;
        serde_json::from_str(raw.trim()).map_err(|e| format!("could not read `but branch show --check` output ({e}): {raw:.400}"))
    }

    fn land(&self, repo: &Path, branch: &str) -> Result<(), String> {
        self.run(repo, &["land", branch, "--yes", "--json"]).map(|_| ())
    }

    /// Opens a PR for `branch`, and the link to it if `but` printed one.
    fn pr_new(&self, repo: &Path, branch: &str) -> Result<Option<String>, String> {
        let raw = self.run(repo, &["pr", "new", branch, "-t", "--json"])?;
        Ok(review_url(&raw))
    }
}

/// The first web link in `but pr new --json`'s output: a `url`-ish field if it has one,
/// else any string that is one. Not tied to one shape, which this crate hasn't pinned down.
fn review_url(raw: &str) -> Option<String> {
    fn find(value: &Value, keyed: bool) -> Option<String> {
        match value {
            Value::String(s) if s.starts_with("http://") || s.starts_with("https://") => Some(s.clone()),
            Value::Object(map) if keyed => map
                .iter()
                .filter(|(k, _)| k.to_ascii_lowercase().ends_with("url"))
                .find_map(|(_, v)| find(v, false))
                .or_else(|| map.values().find_map(|v| find(v, true))),
            Value::Object(map) => map.values().find_map(|v| find(v, false)),
            Value::Array(items) => items.iter().find_map(|v| find(v, keyed)),
            _ => None,
        }
    }
    match serde_json::from_str::<Value>(raw.trim()) {
        Ok(value) => find(&value, true).or_else(|| find(&value, false)),
        Err(_) => raw.split_whitespace().find(|w| w.starts_with("https://") || w.starts_with("http://")).map(String::from),
    }
}

/// One branch of the item, in landing order.
#[derive(Debug, Clone)]
struct Branch {
    name: String,
    /// The branch it's stacked on, if any.
    below: Option<String>,
}

/// The `land:` and `factory:` in a recipe's front matter. `recipe::parse` ignores both.
#[derive(Debug, Default, Deserialize)]
struct LandFrontMatter {
    land: Option<String>,
    factory: Option<String>,
}

fn land_front_matter(source: &str) -> Result<LandFrontMatter, String> {
    let source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Some(rest) = source.strip_prefix("---\n").or_else(|| source.strip_prefix("---\r\n")) else {
        return Ok(LandFrontMatter::default());
    };
    let mut yaml = String::new();
    for line in rest.lines() {
        if line.trim_end() == "---" {
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    serde_yaml::from_str(&yaml).map_err(|e| format!("invalid YAML front matter: {e}"))
}

/// The item's newest run that completed.
fn completed_run(store: &RunStore, item: &ItemId) -> Result<RunState, String> {
    let ids = store.run_ids().map_err(|e| format!("could not list runs in {}: {e}", store.root().display()))?;
    for id in ids.iter().rev() {
        let Ok(run) = store.load(id) else { continue };
        if run.item.as_deref() == Some(item.0.as_str()) && run.status == RunStatus::Done {
            return Ok(run);
        }
    }
    Err(format!("item {item} has no completed run in {}", store.root().display()))
}

/// The branches `run`'s steps used, bottom of each stack first. Stacks are taken in the order
/// their bottom branch was first launched; a branch's own stacks follow it the same way.
fn stack_order(run: &RunState, parsed: &recipe::Recipe) -> Vec<Branch> {
    // Each branch, the branch it sits on, and when it was first launched.
    let mut branches: BTreeMap<String, (Option<String>, u64)> = BTreeMap::new();
    for (step_id, rec) in &run.steps {
        let Some(name) = rec.branch.clone() else { continue };
        let below = parsed
            .steps
            .get(step_id)
            .filter(|step| step.spawn == Some(SpawnMode::Stack))
            .and_then(|step| step.on.as_ref())
            .and_then(|on| run.steps.get(on))
            .and_then(|target| target.branch.clone())
            .filter(|below| *below != name);
        let started = rec.started_at.unwrap_or(u64::MAX);
        let entry = branches.entry(name).or_insert((None, started));
        entry.0 = entry.0.take().or(below);
        entry.1 = entry.1.min(started);
    }

    let mut children: BTreeMap<Option<String>, Vec<(u64, String)>> = BTreeMap::new();
    for (name, (below, started)) in &branches {
        // A branch sitting on one the run doesn't know is a bottom of its own.
        let below = below.clone().filter(|b| branches.contains_key(b));
        children.entry(below).or_default().push((*started, name.clone()));
    }
    for list in children.values_mut() {
        list.sort();
    }

    let mut order = Vec::new();
    let mut seen = BTreeSet::new();
    let mut todo: Vec<(Option<String>, String)> =
        children.get(&None).into_iter().flatten().rev().map(|(_, name)| (None, name.clone())).collect();
    while let Some((below, name)) = todo.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let above = children.get(&Some(name.clone())).into_iter().flatten().rev();
        todo.extend(above.map(|(_, child)| (Some(name.clone()), child.clone())));
        order.push(Branch { name, below });
    }
    // Anything a cycle kept out of reach still gets landed, last.
    for name in branches.keys().filter(|name| !seen.contains(*name)) {
        order.push(Branch { name: name.clone(), below: None });
    }
    order
}

/// A landing that was attempted: what `--json` prints.
#[derive(Debug, Default, Serialize)]
struct Outcome {
    schema: u32,
    item: String,
    run: String,
    mode: LandMode,
    /// The item's branches, bottom first.
    branches: Vec<String>,
    landed: Vec<LandedBranch>,
    /// Branches with no commits ahead of the target, so nothing to land.
    skipped: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stopped_at: Option<Stop>,
    prs: Vec<Pr>,
    /// The item's state afterwards.
    state: String,
    lanes_stopped: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    lanes_not_stopped: Vec<Stop>,
}

#[derive(Debug, Serialize)]
struct LandedBranch {
    branch: String,
    commits: usize,
}

#[derive(Debug, Serialize)]
struct Stop {
    branch: String,
    reason: String,
}

#[derive(Debug, Serialize)]
struct Pr {
    branch: String,
    url: Option<String>,
}

impl Lander<'_> {
    /// Lands `item` (see the module doc), printing progress, or the `--json` document, to `out`.
    /// Errors go to stderr.
    pub fn land(&self, item: &ItemId, opts: &LandOptions, out: &mut dyn Write) -> ExitCode {
        let say = |out: &mut dyn Write, line: String| {
            if !opts.json {
                let _ = writeln!(out, "{line}");
            }
        };
        let fail = |why: String| {
            eprintln!("kanstack-recipe land: {why}");
            ExitCode::FAILURE
        };

        let current = match self.tracker.get(item) {
            Ok(current) => current,
            Err(e) => return fail(format!("{e:#}")),
        };
        if current.state != ItemState::Review {
            return fail(format!(
                "item {item} is {}; only an item in review can be landed, once its run has passed every verify",
                current.state
            ));
        }
        let run = match completed_run(self.store, item) {
            Ok(run) => run,
            Err(e) => return fail(e),
        };
        let source = match std::fs::read_to_string(&run.recipe_path) {
            Ok(source) => source,
            Err(e) => return fail(format!("run {}: could not read its recipe {}: {e}", run.id, run.recipe_path.display())),
        };
        let (parsed, front) = match recipe::parse(&source).map_err(|e| e.to_string()).and_then(|r| Ok((r, land_front_matter(&source)?))) {
            Ok(both) => both,
            Err(e) => return fail(format!("run {}: recipe {}: {e}", run.id, run.recipe_path.display())),
        };
        let factory = match front.factory.as_deref().map(|w| (w, Factory::parse(w))) {
            None => opts.factory,
            Some((_, Some(factory))) => factory,
            Some((word, None)) => return fail(format!("recipe {}: `factory: {word}` isn't light or dark", run.recipe_path.display())),
        };
        if factory == Factory::Dark {
            return fail(format!(
                "item {item}'s factory mode is dark, where a reviewer agent lands it, and dark mode isn't built yet; \
                 use light (unset KANSTACK_FACTORY, or `factory: light` in the recipe) to land it by hand"
            ));
        }
        let mode = match front.land.as_deref().map(|w| (w, LandMode::parse(w))) {
            None => opts.land,
            Some((_, Some(mode))) => mode,
            Some((word, None)) => return fail(format!("recipe {}: `land: {word}` isn't direct or pr", run.recipe_path.display())),
        };

        let branches = stack_order(&run, &parsed);
        if branches.is_empty() {
            return fail(format!("run {} for item {item} recorded no branches to land", run.id));
        }
        let mut outcome = Outcome {
            schema: SCHEMA,
            item: item.0.clone(),
            run: run.id.clone(),
            mode,
            branches: branches.iter().map(|b| b.name.clone()).collect(),
            state: current.state.as_str().to_string(),
            ..Outcome::default()
        };
        say(out, format!("→ item {item}: landing run {} ({}), {}", run.id, mode_word(mode), outcome.branches.join(" → ")));

        let code = match mode {
            LandMode::Direct => self.direct(item, &branches, opts, &mut outcome, out, &say),
            LandMode::Pr => self.pr(item, &branches, &mut outcome, out, &say),
        };
        if opts.json {
            match serde_json::to_string(&outcome) {
                Ok(doc) => {
                    let _ = writeln!(out, "{doc}");
                }
                Err(e) => eprintln!("kanstack-recipe land: could not print the outcome as JSON: {e}"),
            }
        }
        code
    }

    fn direct(
        &self,
        item: &ItemId,
        branches: &[Branch],
        opts: &LandOptions,
        outcome: &mut Outcome,
        out: &mut dyn Write,
        say: &dyn Fn(&mut dyn Write, String),
    ) -> ExitCode {
        for branch in branches {
            let name = &branch.name;
            let stop = match self.but.merge_check(self.repo, name) {
                Err(e) => Some(format!("could not check whether it lands cleanly: {e}")),
                Ok(check) if !check.merge_check.merges_cleanly => {
                    let files: Vec<String> = check.merge_check.conflicting_files.iter().map(|f| format!("`{}`", f.path)).collect();
                    Some(if files.is_empty() {
                        "it doesn't merge cleanly onto the target".to_string()
                    } else {
                        format!("it doesn't merge cleanly onto the target: {} conflict", files.join(", "))
                    })
                }
                Ok(check) if check.commits_ahead == 0 => {
                    say(out, format!("· {name}: no commits ahead of the target, nothing to land"));
                    outcome.skipped.push(name.clone());
                    None
                }
                Ok(check) => match self.but.land(self.repo, name) {
                    Ok(()) => {
                        say(out, format!("✓ landed {name} ({})", commits(check.commits_ahead)));
                        outcome.landed.push(LandedBranch { branch: name.clone(), commits: check.commits_ahead });
                        None
                    }
                    Err(e) => Some(format!("`but land` failed: {e}")),
                },
            };
            if let Some(reason) = stop {
                return self.stopped(item, name, reason, outcome);
            }
        }

        if let Err(e) = self.tracker.comment(item, &landed_comment(outcome)) {
            eprintln!("! item {item}: could not post what landed: {e:#}");
        }
        if let Err(e) = self.tracker.set_state(item, ItemState::Done) {
            eprintln!("kanstack-recipe land: item {item}'s branches landed, but it could not be set done: {e:#}");
            return ExitCode::FAILURE;
        }
        outcome.state = ItemState::Done.as_str().to_string();
        say(out, format!("✓ item {item} done"));

        if opts.keep_panes {
            return ExitCode::SUCCESS;
        }
        for branch in branches {
            match self.kanstack.stop(&branch.name) {
                Ok(()) => {
                    say(out, format!("■ stopped {}", branch.name));
                    outcome.lanes_stopped.push(branch.name.clone());
                }
                Err(e) => {
                    eprintln!("! could not stop lane {}: {e}", branch.name);
                    outcome.lanes_not_stopped.push(Stop { branch: branch.name.clone(), reason: e.to_string() });
                }
            }
        }
        ExitCode::SUCCESS
    }

    /// A direct land that stopped at `branch`: says so on the item, which stays in review.
    fn stopped(&self, item: &ItemId, branch: &str, reason: String, outcome: &mut Outcome) -> ExitCode {
        let after: Vec<&str> = outcome.branches.iter().skip_while(|b| *b != branch).skip(1).map(String::as_str).collect();
        let mut comment = format!("### Landing stopped at `{branch}`\n\n`{branch}` wasn't landed: {reason}.\n\n");
        if outcome.landed.is_empty() {
            comment.push_str("Nothing was landed.\n");
        } else {
            let landed: Vec<String> = outcome.landed.iter().map(|b| format!("`{}`", b.branch)).collect();
            comment.push_str(&format!("Landed before it: {}.\n", landed.join(", ")));
        }
        if !after.is_empty() {
            let after: Vec<String> = after.iter().map(|b| format!("`{b}`")).collect();
            comment.push_str(&format!("Not landed after it: {}.\n", after.join(", ")));
        }
        comment.push_str("\nThe item stays in review. Fix the branch and run `land` again.\n");
        if let Err(e) = self.tracker.comment(item, &comment) {
            eprintln!("! item {item}: could not post where landing stopped: {e:#}");
        }
        eprintln!("kanstack-recipe land: item {item}: stopped at {branch}: {reason}; nothing after it was landed");
        outcome.stopped_at = Some(Stop { branch: branch.to_string(), reason });
        ExitCode::FAILURE
    }

    fn pr(
        &self,
        item: &ItemId,
        branches: &[Branch],
        outcome: &mut Outcome,
        out: &mut dyn Write,
        say: &dyn Fn(&mut dyn Write, String),
    ) -> ExitCode {
        // A branch with nothing ahead of the target can't carry a PR, so the top of each stack
        // is its highest branch with commits.
        let mut empty = BTreeSet::new();
        for branch in branches {
            match self.but.merge_check(self.repo, &branch.name) {
                Ok(check) if check.commits_ahead == 0 => {
                    empty.insert(branch.name.clone());
                    outcome.skipped.push(branch.name.clone());
                }
                Ok(_) => {}
                Err(e) => {
                    let reason = format!("could not check it: {e}");
                    eprintln!("kanstack-recipe land: item {item}: {}: {reason}", branch.name);
                    outcome.stopped_at = Some(Stop { branch: branch.name.clone(), reason });
                    return ExitCode::FAILURE;
                }
            }
        }
        let tops = pr_tops(branches, &empty);
        if tops.is_empty() {
            eprintln!("kanstack-recipe land: item {item}: none of its branches has commits ahead of the target, so there's nothing to open a PR for");
            return ExitCode::FAILURE;
        }

        for top in &tops {
            match self.but.pr_new(self.repo, top) {
                Ok(url) => {
                    say(out, format!("✓ opened a PR for {top}{}", url.as_deref().map(|u| format!(": {u}")).unwrap_or_default()));
                    outcome.prs.push(Pr { branch: top.clone(), url });
                }
                Err(e) => {
                    eprintln!("kanstack-recipe land: item {item}: could not open a PR for {top}: {e}");
                    outcome.stopped_at = Some(Stop { branch: top.clone(), reason: e });
                    break;
                }
            }
        }
        if !outcome.prs.is_empty() {
            if let Err(e) = self.tracker.comment(item, &pr_comment(outcome)) {
                eprintln!("! item {item}: could not post the PR link: {e:#}");
            }
        }
        say(out, format!("· item {item} stays in review until its PR merges"));
        if outcome.stopped_at.is_some() {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        }
    }
}

/// The top of each stack in `branches`: a branch with commits that nothing with commits sits
/// above. `but pr new` on it pushes, and opens reviews for, the branches below it as well.
fn pr_tops(branches: &[Branch], empty: &BTreeSet<String>) -> Vec<String> {
    let below: BTreeMap<&str, Option<&str>> = branches.iter().map(|b| (b.name.as_str(), b.below.as_deref())).collect();
    let mut covered = BTreeSet::new();
    for branch in branches.iter().filter(|b| !empty.contains(&b.name)) {
        let mut next = branch.below.as_deref();
        while let Some(name) = next {
            if !covered.insert(name) {
                break;
            }
            next = below.get(name).copied().flatten();
        }
    }
    branches
        .iter()
        .filter(|b| !empty.contains(&b.name) && !covered.contains(b.name.as_str()))
        .map(|b| b.name.clone())
        .collect()
}

fn mode_word(mode: LandMode) -> &'static str {
    match mode {
        LandMode::Direct => "direct",
        LandMode::Pr => "pr",
    }
}

fn commits(n: usize) -> String {
    format!("{n} commit{}", if n == 1 { "" } else { "s" })
}

/// The comment a finished direct land posts to its item.
fn landed_comment(outcome: &Outcome) -> String {
    let mut comment = String::from("### Landed\n\n");
    if outcome.landed.is_empty() {
        comment.push_str("Nothing had commits ahead of the target, so nothing landed.\n");
    } else {
        comment.push_str("Landed onto the target, bottom of the stack first:\n\n");
        for b in &outcome.landed {
            comment.push_str(&format!("- `{}`: {}\n", b.branch, commits(b.commits)));
        }
    }
    if !outcome.skipped.is_empty() {
        let skipped: Vec<String> = outcome.skipped.iter().map(|b| format!("`{b}`")).collect();
        comment.push_str(&format!("\nNothing to land on {}.\n", skipped.join(", ")));
    }
    comment
}

/// The comment a `pr` land posts to its item.
fn pr_comment(outcome: &Outcome) -> String {
    let mut comment = String::from("### Opened a PR\n\n");
    for pr in &outcome.prs {
        match &pr.url {
            Some(url) => comment.push_str(&format!("- `{}`: {url}\n", pr.branch)),
            None => comment.push_str(&format!("- `{}` (`but` printed no link)\n", pr.branch)),
        }
    }
    comment.push_str(
        "\nThe item stays in review. Following the PR until it merges is out of scope for now, \
         so nothing here will notice the merge: set the item done once it has.\n",
    );
    comment
}

struct Flags {
    item: Option<String>,
    json: bool,
    keep_panes: bool,
    kanstack_bin: PathBuf,
    but_bin: PathBuf,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags { item: None, json: false, keep_panes: false, kanstack_bin: PathBuf::from("kanstack"), but_bin: PathBuf::from("but") };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => flags.json = true,
            "--keep-panes" => flags.keep_panes = true,
            "--kanstack-bin" => flags.kanstack_bin = PathBuf::from(args.next().ok_or("--kanstack-bin needs a path")?),
            "--but-bin" => flags.but_bin = PathBuf::from(args.next().ok_or("--but-bin needs a path")?),
            other if !other.starts_with("--") && flags.item.is_none() => flags.item = Some(other.to_string()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    if flags.item.is_none() {
        return Err("missing <item-id>".to_string());
    }
    Ok(flags)
}

/// `KANSTACK_LAND` and `KANSTACK_FACTORY`, blank counting as unset.
fn env_modes() -> Result<(LandMode, Factory), String> {
    let var = |name: &str| env::var(name).ok().filter(|v| !v.trim().is_empty());
    let land = match var("KANSTACK_LAND") {
        None => LandMode::default(),
        Some(v) => LandMode::parse(&v).ok_or_else(|| format!("KANSTACK_LAND must be direct or pr, not `{v}`"))?,
    };
    let factory = match var("KANSTACK_FACTORY") {
        None => Factory::default(),
        Some(v) => Factory::parse(&v).ok_or_else(|| format!("KANSTACK_FACTORY must be light or dark, not `{v}`"))?,
    };
    Ok((land, factory))
}

const USAGE: &str = "usage: kanstack-recipe land <item-id> [--json] [--kanstack-bin <path>] [--but-bin <path>] [--keep-panes]";

/// `kanstack-recipe land <item-id> [--json] [--kanstack-bin <path>] [--but-bin <path>]
/// [--keep-panes]`, with `args` everything after `land`, for the repository the current
/// directory is in.
pub fn command(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let usage = |e: String| {
        eprintln!("kanstack-recipe land: {e}");
        eprintln!("{USAGE}");
        ExitCode::from(2)
    };
    let flags = match parse_flags(args) {
        Ok(flags) => flags,
        Err(e) => return usage(e),
    };
    let (land, factory) = match env_modes() {
        Ok(modes) => modes,
        Err(e) => return usage(e),
    };

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&cwd).to_path_buf();
    let Some(store) = RunStore::for_repo(&repo) else {
        eprintln!("kanstack-recipe: no state directory to read runs from (set KANSTACK_STATE_PATH or HOME)");
        return ExitCode::FAILURE;
    };
    let tracker = match kanstack_tracker::select(&repo) {
        Ok(tracker) => tracker,
        Err(e) => {
            eprintln!("kanstack-recipe: {e:#}");
            return ExitCode::FAILURE;
        }
    };
    let kanstack = KanstackClient::new(crate::kanstack::resolve_bin(Some(flags.kanstack_bin)));
    let but = But::new(flags.but_bin);
    let lander = Lander { repo: &repo, tracker: tracker.as_ref(), store: &store, kanstack: &kanstack, but: &but };
    let opts = LandOptions { json: flags.json, keep_panes: flags.keep_panes, land, factory };
    lander.land(&ItemId(flags.item.unwrap_or_default()), &opts, &mut io::stdout().lock())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_review_link_in_any_shape() {
        assert_eq!(review_url(r#"{"reviews":[{"number":7,"url":"https://f.test/pr/7"}]}"#).as_deref(), Some("https://f.test/pr/7"));
        assert_eq!(review_url(r#"{"title":"see http://x.test","htmlUrl":"https://f.test/pr/8"}"#).as_deref(), Some("https://f.test/pr/8"));
        assert_eq!(review_url(r#"{"a":{"b":"https://f.test/pr/9"}}"#).as_deref(), Some("https://f.test/pr/9"));
        assert_eq!(review_url("Created review https://f.test/pr/10 for x").as_deref(), Some("https://f.test/pr/10"));
        assert_eq!(review_url(r#"{"ok":true}"#), None);
    }

    #[test]
    fn reads_land_and_factory_from_the_front_matter() {
        let front = land_front_matter("---\nversion: 1\nland: pr\nfactory: light\nsteps: {}\n---\nbody\n").unwrap();
        assert_eq!((front.land.as_deref(), front.factory.as_deref()), (Some("pr"), Some("light")));
        let front = land_front_matter("---\nversion: 1\n---\n").unwrap();
        assert_eq!((front.land, front.factory), (None, None));
    }

    #[test]
    fn flags_need_an_item() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(parse_flags(&[]).is_err());
        let flags = parse_flags(&args(&["44", "--json", "--keep-panes", "--but-bin", "/b", "--kanstack-bin", "/k"])).unwrap();
        assert_eq!((flags.item.as_deref(), flags.json, flags.keep_panes), (Some("44"), true, true));
        assert_eq!((flags.but_bin, flags.kanstack_bin), (PathBuf::from("/b"), PathBuf::from("/k")));
        assert!(parse_flags(&args(&["44", "45"])).is_err());
        assert!(parse_flags(&args(&["44", "--but-bin"])).is_err());
    }

    #[test]
    fn pr_tops_skip_empty_branches_and_cover_what_is_below() {
        let b = |name: &str, below: Option<&str>| Branch { name: name.to_string(), below: below.map(String::from) };
        let branches = [b("a", None), b("a-r", Some("a")), b("a-r-p", Some("a-r")), b("solo", None)];
        assert_eq!(pr_tops(&branches, &BTreeSet::new()), vec!["a-r-p", "solo"]);
        let empty: BTreeSet<String> = ["a-r-p".to_string()].into();
        assert_eq!(pr_tops(&branches, &empty), vec!["a-r", "solo"]);
    }
}
