//! The `items` subcommands: add, list, show, promote and propose work items in the queue,
//! through whichever tracker `kanstack_tracker::select` picks for the repository. The queue
//! itself lives behind the tracker; nothing here keeps state of its own.
//!
//! Exit codes match `main.rs`: 0 success, 2 a usage error (caught before the tracker is
//! touched), 1 anything else. `--json` prints one document on stdout, `{"schema": 1, ...}`
//! like `runs --json`; errors stay plain text on stderr in both modes.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use kanstack_tracker::{Draft, Item, ItemFilter, ItemId, ItemState, Tracker};

/// Versions every `items --json` document, independently of `runs --json`.
const SCHEMA: u32 = 1;

/// `items <args>`: parses `args` (everything after `items`), then runs it against the
/// tracker for the git root of the current directory, printing to stdout.
pub fn command(args: &[String]) -> ExitCode {
    let parsed = match parse(args) {
        Ok(parsed) => parsed,
        Err(e) => return report(e),
    };
    if let Sub::Help = parsed.sub {
        print_usage();
        return ExitCode::SUCCESS;
    }
    let repo = match git_root() {
        Ok(repo) => repo,
        Err(e) => return report(e),
    };
    finish(execute(&parsed, &repo, &mut io::stdout().lock()))
}

/// [`command`] for the repository at `repo`, printing to `out`: the seam tests drive, since
/// the current directory and stdout are both process-wide. Errors still go to stderr.
pub fn run_in(repo: &Path, args: &[String], out: &mut dyn Write) -> ExitCode {
    match parse(args) {
        Ok(Parsed { sub: Sub::Help, .. }) => {
            print_usage();
            ExitCode::SUCCESS
        }
        Ok(parsed) => finish(execute(&parsed, repo, out)),
        Err(e) => report(e),
    }
}

enum Error {
    /// Bad arguments: exit 2, with the usage printed after the message.
    Usage(String),
    /// Everything else: exit 1.
    Failed(String),
}

fn failed(e: impl std::fmt::Display) -> Error {
    // `{:#}` so an anyhow error from the tracker prints its whole context chain.
    Error::Failed(format!("{e:#}"))
}

fn finish(result: Result<(), Error>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => report(e),
    }
}

fn report(e: Error) -> ExitCode {
    match e {
        Error::Usage(msg) => {
            eprintln!("kanstack-recipe: {msg}");
            print_usage();
            ExitCode::from(2)
        }
        Error::Failed(msg) => {
            eprintln!("kanstack-recipe: {msg}");
            ExitCode::FAILURE
        }
    }
}

struct Parsed {
    sub: Sub,
    json: bool,
}

enum Sub {
    Help,
    Add(Draft),
    List(ItemFilter),
    Show(ItemId),
    Promote(ItemId),
    /// The draft already has `from` and `state: proposed` set.
    Propose(Draft),
}

/// Every flag any subcommand takes, gathered in one pass. Each subcommand names the ones it
/// accepts in [`Flags::gather`] and rejects the rest.
#[derive(Default)]
struct Flags {
    title: Option<String>,
    body: Option<String>,
    body_file: Option<String>,
    recipe: Option<String>,
    state: Option<String>,
    from: Option<String>,
    id: Option<String>,
    json: bool,
}

impl Flags {
    /// `args` as flags for `sub`, which takes the value flags in `takes` and, if `takes_id`,
    /// one positional id. `--json` is always accepted.
    fn gather(sub: &str, args: &[String], takes: &[&str], takes_id: bool) -> Result<Flags, Error> {
        let mut flags = Flags::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--json" {
                flags.json = true;
                continue;
            }
            if !arg.starts_with("--") {
                if takes_id && flags.id.is_none() {
                    flags.id = Some(arg.clone());
                    continue;
                }
                return Err(Error::Usage(format!("items {sub}: unexpected argument `{arg}`")));
            }
            if !takes.contains(&arg.as_str()) {
                return Err(Error::Usage(format!("items {sub}: unexpected argument `{arg}`")));
            }
            let Some(value) = args.next() else {
                return Err(Error::Usage(format!("items {sub}: {arg} needs a value")));
            };
            let slot = match arg.as_str() {
                "--title" => &mut flags.title,
                "--body" => &mut flags.body,
                "--body-file" => &mut flags.body_file,
                "--recipe" => &mut flags.recipe,
                "--state" => &mut flags.state,
                _ => &mut flags.from,
            };
            if slot.replace(value.clone()).is_some() {
                return Err(Error::Usage(format!("items {sub}: {arg} given twice")));
            }
        }
        if takes_id && flags.id.is_none() {
            return Err(Error::Usage(format!("items {sub}: missing <id>")));
        }
        Ok(flags)
    }

    fn title(&mut self, sub: &str) -> Result<String, Error> {
        self.title.take().ok_or_else(|| Error::Usage(format!("items {sub}: missing --title")))
    }

    /// The body from `--body` or `--body-file`, empty if neither. An unreadable file is a
    /// usage error, like an unreadable recipe in `run`.
    fn body(&mut self, sub: &str) -> Result<String, Error> {
        match (self.body.take(), self.body_file.take()) {
            (Some(_), Some(_)) => Err(Error::Usage(format!("items {sub}: give --body or --body-file, not both"))),
            (Some(body), None) => Ok(body),
            (None, Some(path)) => std::fs::read_to_string(&path)
                .map_err(|e| Error::Usage(format!("items {sub}: could not read {path}: {e}"))),
            (None, None) => Ok(String::new()),
        }
    }

    /// `--state` parsed, if given, and only if it's one of `allowed`.
    fn state(&self, sub: &str, allowed: &[ItemState]) -> Result<Option<ItemState>, Error> {
        let Some(word) = self.state.as_deref() else {
            return Ok(None);
        };
        match ItemState::parse(word) {
            Some(state) if allowed.contains(&state) => Ok(Some(state)),
            _ => {
                let allowed = allowed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
                Err(Error::Usage(format!("items {sub}: --state {word:?} isn't one of {allowed}")))
            }
        }
    }
}

fn parse(args: &[String]) -> Result<Parsed, Error> {
    let Some((sub, rest)) = args.split_first() else {
        return Err(Error::Usage("items: missing subcommand".to_string()));
    };
    let sub = sub.as_str();
    let draft_flags = ["--title", "--body", "--body-file", "--recipe"];
    let (sub, json) = match sub {
        "-h" | "--help" => (Sub::Help, false),
        "add" => {
            let mut flags = Flags::gather(sub, rest, &[&draft_flags[..], &["--state"]].concat(), false)?;
            let state = flags.state(sub, &[ItemState::Backlog, ItemState::Proposed])?.unwrap_or_default();
            let draft = Draft { title: flags.title(sub)?, body: flags.body(sub)?, recipe: flags.recipe.take(), from: None, state };
            (Sub::Add(draft), flags.json)
        }
        "list" => {
            let flags = Flags::gather(sub, rest, &["--state"], false)?;
            let filter = ItemFilter { state: flags.state(sub, &ItemState::ALL)? };
            (Sub::List(filter), flags.json)
        }
        "show" | "promote" => {
            let flags = Flags::gather(sub, rest, &[], true)?;
            let id = ItemId(flags.id.unwrap_or_default());
            (if sub == "show" { Sub::Show(id) } else { Sub::Promote(id) }, flags.json)
        }
        "propose" => {
            let mut flags = Flags::gather(sub, rest, &[&draft_flags[..], &["--from"]].concat(), false)?;
            let from = flags.from.take().ok_or_else(|| Error::Usage("items propose: missing --from".to_string()))?;
            let draft = Draft {
                title: flags.title(sub)?,
                body: flags.body(sub)?,
                recipe: flags.recipe.take(),
                from: Some(ItemId(from)),
                state: ItemState::Proposed,
            };
            (Sub::Propose(draft), flags.json)
        }
        other => return Err(Error::Usage(format!("items: unknown subcommand `{other}`"))),
    };
    Ok(Parsed { sub, json })
}

/// The top level of the work tree the current directory is in.
fn git_root() -> Result<PathBuf, Error> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| failed(format_args!("could not run git: {e}")))?;
    if !output.status.success() {
        return Err(Error::Failed(format!(
            "items needs a git repository: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&output.stdout).trim_end_matches(['\n', '\r'])))
}

fn execute(parsed: &Parsed, repo: &Path, out: &mut dyn Write) -> Result<(), Error> {
    let tracker = kanstack_tracker::select(repo).map_err(failed)?;
    let tracker = tracker.as_ref();
    let json = parsed.json;
    match &parsed.sub {
        Sub::Help => Ok(()),
        Sub::Add(draft) => create(tracker, draft, json, out),
        Sub::List(filter) => list(tracker, filter, json, out),
        Sub::Show(id) => show(tracker, id, json, out),
        Sub::Promote(id) => promote(tracker, id, json, out),
        Sub::Propose(draft) => {
            // Checked here rather than left to the tracker, which would store a dangling `from`.
            let from = draft.from.as_ref().expect("propose always sets from");
            tracker.get(from).map_err(|e| failed(format_args!("--from {from}: {e:#}")))?;
            create(tracker, draft, json, out)
        }
    }
}

/// `add` and `propose`: the new id alone on a line, so `id=$(kanstack-recipe items add ...)`
/// works, or the whole new item with `--json`.
fn create(tracker: &dyn Tracker, draft: &Draft, json: bool, out: &mut dyn Write) -> Result<(), Error> {
    let id = tracker.create(draft).map_err(failed)?;
    if json {
        let item = tracker.get(&id).map_err(failed)?;
        return emit(out, serde_json::json!({ "schema": SCHEMA, "item": item }));
    }
    writeln!(out, "{id}").map_err(failed)
}

fn list(tracker: &dyn Tracker, filter: &ItemFilter, json: bool, out: &mut dyn Write) -> Result<(), Error> {
    let items = tracker.list(filter).map_err(failed)?;
    if json {
        return emit(out, serde_json::json!({ "schema": SCHEMA, "items": items }));
    }
    if items.is_empty() {
        return writeln!(out, "no items").map_err(failed);
    }
    let id_width = items.iter().map(|item| item.id.0.len()).max().unwrap_or(0);
    let recipe_width = items.iter().map(|item| recipe(item).len()).max().unwrap_or(0);
    for item in &items {
        writeln!(
            out,
            "{:<id_width$}  {:<8}  {:<recipe_width$}  {}",
            item.id.0,
            item.state.as_str(),
            recipe(item),
            item.title
        )
        .map_err(failed)?;
    }
    Ok(())
}

fn recipe(item: &Item) -> &str {
    item.recipe.as_deref().unwrap_or("-")
}

fn show(tracker: &dyn Tracker, id: &ItemId, json: bool, out: &mut dyn Write) -> Result<(), Error> {
    let item = tracker.get(id).map_err(failed)?;
    if json {
        return emit(out, serde_json::json!({ "schema": SCHEMA, "item": item }));
    }
    let mut text = format!("id:     {}\ntitle:  {}\nstate:  {}\n", item.id, item.title, item.state);
    if let Some(recipe) = &item.recipe {
        text.push_str(&format!("recipe: {recipe}\n"));
    }
    if let Some(from) = &item.from {
        text.push_str(&format!("from:   {from}\n"));
    }
    let body = item.body.trim_end();
    if !body.is_empty() {
        text.push_str(&format!("\n{body}\n"));
    }
    out.write_all(text.as_bytes()).map_err(failed)
}

/// `proposed` → `backlog`, so the pull loop picks it up. Read then written without a lock
/// across both, so a concurrent state change in between can be overwritten; only a person
/// promotes, so that's not worth a trait method of its own yet.
fn promote(tracker: &dyn Tracker, id: &ItemId, json: bool, out: &mut dyn Write) -> Result<(), Error> {
    let item = tracker.get(id).map_err(failed)?;
    if item.state != ItemState::Proposed {
        return Err(Error::Failed(format!("item {id} is {}, and only a proposed item can be promoted", item.state)));
    }
    tracker.set_state(id, ItemState::Backlog).map_err(failed)?;
    if json {
        let item = tracker.get(id).map_err(failed)?;
        return emit(out, serde_json::json!({ "schema": SCHEMA, "item": item }));
    }
    writeln!(out, "✓ item {id}: proposed → backlog").map_err(failed)
}

fn emit(out: &mut dyn Write, doc: serde_json::Value) -> Result<(), Error> {
    writeln!(out, "{doc}").map_err(failed)
}

fn print_usage() {
    eprintln!("usage:");
    eprintln!("  kanstack-recipe items add --title <t> [--body <b> | --body-file <path>] [--recipe <name>] [--state backlog|proposed] [--json]");
    eprintln!("  kanstack-recipe items list [--state <s>] [--json]");
    eprintln!("  kanstack-recipe items show <id> [--json]");
    eprintln!("  kanstack-recipe items promote <id> [--json]");
    eprintln!("  kanstack-recipe items propose --from <id> --title <t> [--body <b> | --body-file <path>] [--recipe <name>] [--json]");
}
