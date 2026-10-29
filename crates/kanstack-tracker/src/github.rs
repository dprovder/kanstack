//! The `github` tracker: issues in the repository's GitHub origin, through the `gh` CLI.
//!
//! Like the multiplexer backends, this shells out rather than talking HTTP, so kanstack takes
//! on no client and no token handling: `gh auth login` is the whole setup. Every command
//! names the origin repository with `--repo`, so a fork's `gh repo set-default` can't point
//! kanstack at the upstream's issues.
//!
//! - **Ids** are issue numbers: `44`, linked as "Closes #44".
//! - **States** are labels named `kanstack:<state>`, created the first time each is needed.
//!   `done` also closes the issue, and leaving `done` reopens it. An issue with no
//!   `kanstack:` label is `backlog` while open, and a closed issue is `done` whatever its
//!   labels say: a person closing an issue on GitHub means it's finished (or dropped), and the
//!   queue mustn't start it again because a stale `kanstack:backlog` label is still on it.
//! - **Recipe and source** ride at the end of the body as `kanstack-recipe: <name>` and
//!   `kanstack-from: #N` lines, stripped again on reading, so [`Item::body`] is only what the
//!   person wrote.
//! - **Reports** are issue comments, so they aren't in [`Item::body`] here.
//!
//! `KANSTACK_GH_BIN` replaces `gh` (tests point it at a stand-in), as `KANSTACK_TMUX_BIN`
//! does for the multiplexer.

use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::{Draft, Item, ItemFilter, ItemId, ItemState, Tracker};

/// What every state label starts with: `kanstack:running`.
const LABEL_PREFIX: &str = "kanstack:";

/// The body lines [`Draft::recipe`] and [`Draft::from`] are kept in.
const RECIPE_LINE: &str = "kanstack-recipe:";
const FROM_LINE: &str = "kanstack-from:";

/// The fields asked of `gh issue list` and `gh issue view`, matching [`Issue`].
const ISSUE_FIELDS: &str = "number,title,body,state,labels";

/// `gh`'s `--limit` for lists. Its default is 30; `gh` pages through the API itself, so this
/// is only a ceiling, set high enough that a queue is never silently cut short.
const LIST_LIMIT: &str = "10000";

/// Issues in one repository's GitHub origin.
pub struct GithubTracker {
    repo: PathBuf,
    gh: PathBuf,
    /// `owner/name` of the origin, read from git on first use rather than on open, since
    /// [`crate::Open`] mustn't touch anything.
    slug: OnceLock<String>,
    /// The `kanstack:` labels known to exist, fetched on the first label that's needed and
    /// added to as this creates them. Held across the `gh label create`, so two threads
    /// can't both create one.
    labels: Mutex<Option<BTreeSet<String>>>,
}

/// One issue, as `gh ... --json` prints it.
#[derive(Debug, Deserialize)]
struct Issue {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    /// `OPEN` or `CLOSED`.
    state: String,
    #[serde(default)]
    labels: Vec<Label>,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

impl Issue {
    fn closed(&self) -> bool {
        self.state.eq_ignore_ascii_case("closed")
    }

    /// The states its `kanstack:` labels name. Normally one; a person can add more by hand.
    fn state_labels(&self) -> impl Iterator<Item = ItemState> + '_ {
        self.labels.iter().filter_map(|label| ItemState::parse(label.name.strip_prefix(LABEL_PREFIX)?))
    }

    /// Its kanstack state (see the module docs). With more than one state label, the one
    /// furthest along the lifecycle wins.
    fn item_state(&self) -> ItemState {
        if self.closed() {
            return ItemState::Done;
        }
        self.state_labels().max_by_key(|state| ItemState::ALL.iter().position(|s| s == state)).unwrap_or(ItemState::Backlog)
    }

    fn into_item(self) -> Item {
        let state = self.item_state();
        let (body, recipe, from) = decode_body(self.body.as_deref().unwrap_or_default());
        Item { id: ItemId(self.number.to_string()), title: self.title, state, recipe, from, body }
    }
}

/// [`crate::Open`] for `github`, with `gh` from `KANSTACK_GH_BIN`.
pub(crate) fn open(repo: &Path) -> Result<Box<dyn Tracker>> {
    let gh = std::env::var_os("KANSTACK_GH_BIN").filter(|bin| !bin.is_empty()).unwrap_or_else(|| "gh".into());
    Ok(Box::new(GithubTracker::with_gh(repo, gh)))
}

impl GithubTracker {
    /// The tracker for the repository at `repo`, running `gh` as `gh`. The test seam, so
    /// tests don't race each other over `KANSTACK_GH_BIN`.
    pub fn with_gh(repo: impl Into<PathBuf>, gh: impl Into<PathBuf>) -> GithubTracker {
        GithubTracker { repo: repo.into(), gh: gh.into(), slug: OnceLock::new(), labels: Mutex::new(None) }
    }

    /// `owner/name` of the origin remote, or an error saying why it isn't on GitHub.
    fn slug(&self) -> Result<&str> {
        if let Some(slug) = self.slug.get() {
            return Ok(slug);
        }
        let url = origin_url(&self.repo)?;
        let slug = github_slug(&url).ok_or_else(|| anyhow!("origin ({url}) isn't a github.com repository"))?;
        Ok(self.slug.get_or_init(|| slug))
    }

    /// Runs `gh args...` in the repository and returns its stdout, or an error carrying its
    /// stderr. The error names only the subcommand, never the arguments, which can be a
    /// whole issue body.
    fn gh(&self, args: &[&str]) -> Result<String> {
        let what = || format!("gh {}", args.iter().take(2).copied().collect::<Vec<_>>().join(" "));
        let output = Command::new(&self.gh)
            .args(args)
            .current_dir(&self.repo)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .stdin(Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                bail!("{} isn't installed (or set KANSTACK_GH_BIN to it)", self.gh.display())
            }
            Err(e) => return Err(e).with_context(what),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("{} failed: {}", what(), stderr.trim());
        }
        String::from_utf8(output.stdout).with_context(|| format!("{} printed non-UTF-8", what()))
    }

    fn view(&self, n: u64) -> Result<Issue> {
        let json = self.gh(&["issue", "view", &n.to_string(), "--repo", self.slug()?, "--json", ISSUE_FIELDS])?;
        serde_json::from_str(&json).with_context(|| format!("parsing gh issue view {n}"))
    }

    /// Creates `state`'s label in the repository unless it's known to exist already.
    fn ensure_label(&self, state: ItemState) -> Result<()> {
        let name = label(state);
        let mut known = self.labels.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if known.is_none() {
            *known = Some(self.state_labels()?);
        }
        let known = known.as_mut().expect("just filled");
        if known.contains(&name) {
            return Ok(());
        }
        let description = format!("kanstack item state: {state}");
        let created = self.gh(&[
            "label",
            "create",
            &name,
            "--repo",
            self.slug()?,
            "--color",
            color(state),
            "--description",
            &description,
        ]);
        // Another process (or a person) may have made it since the list: `gh` then fails
        // with "already exists", which a fresh list confirms without matching its wording.
        if let Err(e) = created {
            *known = self.state_labels()?;
            if !known.contains(&name) {
                return Err(e);
            }
        }
        known.insert(name);
        Ok(())
    }

    /// The repository's labels that start with `kanstack:`.
    fn state_labels(&self) -> Result<BTreeSet<String>> {
        let json = self.gh(&["label", "list", "--repo", self.slug()?, "--json", "name", "--limit", LIST_LIMIT])?;
        let labels: Vec<Label> = serde_json::from_str(&json).context("parsing gh label list")?;
        Ok(labels.into_iter().map(|label| label.name).filter(|name| name.starts_with(LABEL_PREFIX)).collect())
    }
}

impl Tracker for GithubTracker {
    fn id(&self) -> &'static str {
        "github"
    }

    /// The origin remote is on github.com and `gh` is logged in to it. The remote is checked
    /// first, since it's local: `gh auth status` validates its token with GitHub, so a repo
    /// with no GitHub origin never waits on the network.
    fn detect(&self, repo: &Path) -> bool {
        let on_github = origin_url(repo).is_ok_and(|url| github_slug(&url).is_some());
        on_github
            && Command::new(&self.gh)
                .args(["auth", "status", "--hostname", "github.com"])
                .current_dir(repo)
                .env("GH_PROMPT_DISABLED", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    }

    /// Asks `gh` for open issues only when the filter rules closed ones out (everything but
    /// `done`), which is what the queue's `backlog` poll does, so it never pages through a
    /// repository's closed history.
    fn list(&self, filter: &ItemFilter) -> Result<Vec<Item>> {
        let state = match filter.state {
            Some(state) if state != ItemState::Done => "open",
            _ => "all",
        };
        let json = self.gh(&[
            "issue",
            "list",
            "--repo",
            self.slug()?,
            "--state",
            state,
            "--limit",
            LIST_LIMIT,
            "--json",
            ISSUE_FIELDS,
        ])?;
        let mut issues: Vec<Issue> = serde_json::from_str(&json).context("parsing gh issue list")?;
        // `gh` lists newest first.
        issues.sort_by_key(|issue| issue.number);
        Ok(issues.into_iter().map(Issue::into_item).filter(|item| filter.matches(item)).collect())
    }

    fn get(&self, id: &ItemId) -> Result<Item> {
        Ok(self.view(number(id)?)?.into_item())
    }

    fn create(&self, draft: &Draft) -> Result<ItemId> {
        let title = draft.title.split_whitespace().collect::<Vec<_>>().join(" ");
        if title.is_empty() {
            bail!("an item needs a title");
        }
        let from = draft.from.as_ref().map(number).transpose()?;
        let body = encode_body(&draft.body, draft.recipe.as_deref(), from);
        self.ensure_label(draft.state)?;
        let out = self.gh(&[
            "issue",
            "create",
            "--repo",
            self.slug()?,
            "--title",
            &title,
            "--body",
            &body,
            "--label",
            &label(draft.state),
        ])?;
        // `gh issue create` has no --json: it prints the new issue's URL.
        let url = out.lines().map(str::trim).rfind(|line| !line.is_empty()).unwrap_or_default();
        let n: u64 = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| anyhow!("gh issue create printed {url:?}, not an issue URL"))?;
        if draft.state == ItemState::Done {
            self.gh(&["issue", "close", &n.to_string(), "--repo", self.slug()?])?;
        }
        Ok(ItemId(n.to_string()))
    }

    /// Swaps the issue's `kanstack:` labels for `state`'s, then closes it for `done` or
    /// reopens it for anything else. Each is skipped when it's already so, so setting the
    /// state an issue already has changes nothing on GitHub.
    fn set_state(&self, id: &ItemId, state: ItemState) -> Result<()> {
        let n = number(id)?;
        let issue = self.view(n)?;
        let n = n.to_string();
        let (wanted, stale) = (label(state), issue.state_labels().filter(|&s| s != state).map(label).collect::<Vec<_>>().join(","));
        let has_label = issue.state_labels().any(|s| s == state);
        if !has_label || !stale.is_empty() {
            let mut args = vec!["issue", "edit", &n, "--repo", self.slug()?];
            if !has_label {
                self.ensure_label(state)?;
                args.extend(["--add-label", &wanted]);
            }
            if !stale.is_empty() {
                args.extend(["--remove-label", &stale]);
            }
            self.gh(&args)?;
        }
        match (state == ItemState::Done, issue.closed()) {
            (true, false) => self.gh(&["issue", "close", &n, "--repo", self.slug()?]).map(drop),
            (false, true) => self.gh(&["issue", "reopen", &n, "--repo", self.slug()?]).map(drop),
            _ => Ok(()),
        }
    }

    fn comment(&self, id: &ItemId, body: &str) -> Result<()> {
        self.gh(&["issue", "comment", &number(id)?.to_string(), "--repo", self.slug()?, "--body", body.trim()]).map(drop)
    }

    fn link_text(&self, id: &ItemId) -> Option<String> {
        Some(format!("Closes #{}", number(id).ok()?))
    }
}

/// `state`'s label: `kanstack:running`.
fn label(state: ItemState) -> String {
    format!("{LABEL_PREFIX}{state}")
}

/// The colour a label is created with: grey for not started, amber while running, red,
/// purple and green after.
fn color(state: ItemState) -> &'static str {
    match state {
        ItemState::Proposed => "d4c5f9",
        ItemState::Backlog => "ededed",
        ItemState::Running => "fbca04",
        ItemState::Failed => "d93f0b",
        ItemState::Review => "5319e7",
        ItemState::Done => "0e8a16",
    }
}

/// The issue number in `id`, which must be a GitHub id: `44`, or `#44` as typed by a person.
fn number(id: &ItemId) -> Result<u64> {
    id.0.strip_prefix('#')
        .unwrap_or(&id.0)
        .parse()
        .map_err(|_| anyhow!("{id:?} isn't a GitHub issue id (expected an issue number)"))
}

/// The URL `repo`'s origin remote fetches from, with any `insteadOf` rewrite applied.
fn origin_url(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!("{} has no origin remote", repo.display());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `owner/name` when `url` is a github.com repository, in any form git accepts:
/// `https://github.com/o/r.git`, `git@github.com:o/r.git`, `ssh://git@ssh.github.com:443/o/r`.
fn github_slug(url: &str) -> Option<String> {
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => {
            let (authority, path) = rest.split_once('/')?;
            let host = authority.rsplit('@').next()?;
            (host.split(':').next()?, path)
        }
        // scp-like: `[user@]host:path`.
        None => {
            let (authority, path) = url.split_once(':')?;
            (authority.rsplit('@').next()?, path)
        }
    };
    if !["github.com", "www.github.com", "ssh.github.com"].iter().any(|h| host.eq_ignore_ascii_case(h)) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    match path.split('/').collect::<Vec<_>>()[..] {
        [owner, name] if !owner.is_empty() && !name.is_empty() => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

/// The issue body for a draft: its body, then a blank line and the recipe and source lines.
fn encode_body(body: &str, recipe: Option<&str>, from: Option<u64>) -> String {
    let mut text = body.trim_end().to_string();
    let mut trailer = Vec::new();
    if let Some(recipe) = recipe.and_then(|recipe| recipe.split_whitespace().next()) {
        trailer.push(format!("{RECIPE_LINE} {recipe}"));
    }
    if let Some(from) = from {
        trailer.push(format!("{FROM_LINE} #{from}"));
    }
    if !trailer.is_empty() {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&trailer.join("\n"));
    }
    text.push('\n');
    text
}

/// An issue body split into what the person wrote, the recipe and the source. Only the
/// trailing run of `kanstack-` lines (and blank lines among them) counts, so a body that
/// quotes one further up keeps it. Line endings are normalised, since bodies edited on
/// github.com come back with `\r\n`. The body ends with one newline unless it's empty, as on
/// the `local` tracker.
fn decode_body(raw: &str) -> (String, Option<String>, Option<ItemId>) {
    let raw = raw.replace("\r\n", "\n");
    let mut lines: Vec<&str> = raw.lines().collect();
    let (mut recipe, mut from) = (None, None);
    while let Some(line) = lines.last().map(|line| line.trim()) {
        if let Some(value) = line.strip_prefix(RECIPE_LINE) {
            recipe = recipe.or_else(|| value.split_whitespace().next().map(str::to_string));
        } else if let Some(value) = line.strip_prefix(FROM_LINE) {
            let value = value.trim();
            from = from.or_else(|| Some(value.strip_prefix('#').unwrap_or(value)).filter(|v| !v.is_empty()).map(|v| ItemId(v.to_string())));
        } else if !line.is_empty() {
            break;
        }
        lines.pop();
    }
    let mut body = lines.join("\n").trim_end().to_string();
    if !body.is_empty() {
        body.push('\n');
    }
    (body, recipe, from)
}

/// A stand-in `gh` for tests: issues, labels and comments as files in a temp dir, printed
/// back as JSON shaped like `gh`'s. Never calls GitHub. Fails like `gh` does where it
/// matters here: on a `--repo` other than the one it was made for, on adding a label that
/// doesn't exist, on creating one that does, and on flags it doesn't know.
#[cfg(test)]
pub(crate) mod fake {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The origin every fake repository gets.
    pub const SLUG: &str = "acme/widgets";

    const SCRIPT: &str = r#"#!/bin/sh
D='@DIR@'
REPO='@REPO@'
printf '%s\n' "$*" >> "$D/log"

# The file $1, which ends in a sentinel '.', as a JSON string.
js() {
  awk 'BEGIN { ORS = "" }
    { line[NR] = $0 }
    END {
      line[NR] = substr(line[NR], 1, length(line[NR]) - 1)
      printf "\""
      for (i = 1; i <= NR; i++) {
        if (i > 1) printf "\\n"
        s = line[i]
        for (j = 1; j <= length(s); j++) {
          c = substr(s, j, 1)
          if (c == "\\") c = "\\\\"; else if (c == "\"") c = "\\\""; else if (c == "\t") c = "\\t"; else if (c == "\r") c = "\\r"
          printf "%s", c
        }
      }
      printf "\""
    }' "$1"
}

issue_json() {
  i="$D/issues/$1"
  printf '{"number":%s,"title":' "$1"; js "$i/title"
  printf ',"body":'; js "$i/body"
  printf ',"state":"%s","labels":[' "$(cat "$i/state")"
  sep=''
  while IFS= read -r l; do printf '%s{"name":"%s"}' "$sep" "$l"; sep=','; done < "$i/labels"
  printf ']}'
}

# The comma-separated list $1, one per line.
each() { old=$IFS; IFS=,; set -- $1; IFS=$old; for l; do [ -n "$l" ] && printf '%s\n' "$l"; done; return 0; }
has_label() { grep -qxF "$1" "$D/labels" 2>/dev/null; }
need_labels() { for l in $(each "$1"); do has_label "$l" || { echo "could not add label: '$l' not found" >&2; exit 1; }; done; }
need_issue() { [ -d "$D/issues/$num" ] || { echo "GraphQL: Could not resolve to an issue with the number of $num." >&2; exit 1; }; }

cmd="$1 $2"; shift 2
num=''; repo=''; title=''; body=''; state=open; add=''; remove=''
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) repo=$2; shift 2 ;;
    --title) title=$2; shift 2 ;;
    --body) body=$2; shift 2 ;;
    --state) state=$2; shift 2 ;;
    --label|--add-label) add="$add,$2"; shift 2 ;;
    --remove-label) remove="$remove,$2"; shift 2 ;;
    --json|--limit|--color|--description|--hostname) shift 2 ;;
    -*) echo "fake gh: unknown flag $1" >&2; exit 2 ;;
    *) num=$1; shift ;;
  esac
done

if [ "$cmd" = "auth status" ]; then [ -e "$D/authed" ]; exit; fi
[ "$repo" = "$REPO" ] || { echo "fake gh: --repo is '$repo', not '$REPO'" >&2; exit 1; }

case "$cmd" in
  "label list")
    printf '['; sep=''
    if [ -f "$D/labels" ]; then while IFS= read -r l; do printf '%s{"name":"%s"}' "$sep" "$l"; sep=','; done < "$D/labels"; fi
    printf ']\n' ;;
  "label create")
    if has_label "$num"; then echo "label with name \"$num\" already exists; use \`--force\` to update" >&2; exit 1; fi
    printf '%s\n' "$num" >> "$D/labels" ;;
  "issue create")
    need_labels "$add"
    n=$(( $(ls "$D/issues" | wc -l) + 1 )); i="$D/issues/$n"; mkdir "$i"
    printf '%s.' "$title" > "$i/title"; printf '%s.' "$body" > "$i/body"; echo OPEN > "$i/state"; each "$add" > "$i/labels"
    echo "https://github.com/$REPO/issues/$n" ;;
  "issue list")
    printf '['; sep=''
    for n in $(ls "$D/issues" | sort -rn); do
      s=$(cat "$D/issues/$n/state")
      case "$state" in open) [ "$s" = OPEN ] || continue ;; closed) [ "$s" = CLOSED ] || continue ;; esac
      printf '%s' "$sep"; issue_json "$n"; sep=','
    done
    printf ']\n' ;;
  "issue view") need_issue; issue_json "$num"; echo ;;
  "issue edit")
    need_issue; need_labels "$add"; i="$D/issues/$num"
    for l in $(each "$remove"); do grep -vxF "$l" "$i/labels" > "$i/labels.tmp"; mv "$i/labels.tmp" "$i/labels"; done
    for l in $(each "$add"); do grep -qxF "$l" "$i/labels" || printf '%s\n' "$l" >> "$i/labels"; done ;;
  "issue close") need_issue; echo CLOSED > "$D/issues/$num/state" ;;
  "issue reopen") need_issue; echo OPEN > "$D/issues/$num/state" ;;
  "issue comment") need_issue; printf '%s\n--\n' "$body" >> "$D/issues/$num/comments" ;;
  *) echo "fake gh: unknown command $cmd" >&2; exit 2 ;;
esac
"#;

    /// A fake GitHub in a temp dir: a git repository whose origin is [`SLUG`] on github.com,
    /// and a stand-in `gh` that's logged in and keeps its issues beside it.
    pub struct Fake {
        pub dir: tempfile::TempDir,
    }

    impl Fake {
        pub fn new() -> Fake {
            let fake = Fake { dir: tempfile::tempdir().unwrap() };
            std::fs::create_dir_all(fake.data().join("issues")).unwrap();
            std::fs::write(fake.data().join("authed"), "").unwrap();
            let script = SCRIPT.replace("@DIR@", &fake.data().to_string_lossy()).replace("@REPO@", SLUG);
            std::fs::write(fake.gh(), script).unwrap();
            std::fs::set_permissions(fake.gh(), std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::create_dir(fake.repo()).unwrap();
            fake.git(&["init", "--quiet"]);
            fake.git(&["remote", "add", "origin", &format!("git@github.com:{SLUG}.git")]);
            fake
        }

        pub fn repo(&self) -> PathBuf {
            self.dir.path().join("repo")
        }

        pub fn gh(&self) -> PathBuf {
            self.dir.path().join("gh")
        }

        pub fn data(&self) -> PathBuf {
            self.dir.path().join("github")
        }

        pub fn git(&self, args: &[&str]) {
            let status = Command::new("git").arg("-C").arg(self.repo()).args(args).status().unwrap();
            assert!(status.success(), "git {args:?}");
        }

        pub fn set_authed(&self, authed: bool) {
            let path = self.data().join("authed");
            if authed {
                std::fs::write(path, "").unwrap();
            } else {
                std::fs::remove_file(path).unwrap();
            }
        }

        /// What the stand-in has for issue `n`'s file `name` (`labels`, `state`, `comments`),
        /// with the sentinel `.` on `title` and `body` stripped.
        pub fn issue_file(&self, n: u64, name: &str) -> String {
            let text = std::fs::read_to_string(self.issue_dir(n).join(name)).unwrap_or_default();
            match name {
                "title" | "body" => text.strip_suffix('.').unwrap().to_string(),
                _ => text,
            }
        }

        /// Writes issue `n` straight into the stand-in, as a person on github.com would.
        pub fn put_issue(&self, n: u64, title: &str, body: &str, open: bool, labels: &[&str]) {
            let dir = self.issue_dir(n);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("title"), format!("{title}.")).unwrap();
            std::fs::write(dir.join("body"), format!("{body}.")).unwrap();
            std::fs::write(dir.join("state"), if open { "OPEN\n" } else { "CLOSED\n" }).unwrap();
            std::fs::write(dir.join("labels"), labels.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        }

        pub fn labels(&self) -> Vec<String> {
            let text = std::fs::read_to_string(self.data().join("labels")).unwrap_or_default();
            text.lines().map(str::to_string).collect()
        }

        /// Every command the stand-in has run, one per line (a multi-line body spans more).
        pub fn log(&self) -> String {
            std::fs::read_to_string(self.data().join("log")).unwrap_or_default()
        }

        fn issue_dir(&self, n: u64) -> PathBuf {
            self.data().join("issues").join(n.to_string())
        }
    }

    /// A path that's certainly not a `gh`.
    pub fn missing_gh(dir: &Path) -> PathBuf {
        dir.join("no-such-gh")
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{missing_gh, Fake, SLUG};
    use super::*;

    fn tracker(fake: &Fake) -> GithubTracker {
        GithubTracker::with_gh(fake.repo(), fake.gh())
    }

    fn draft(title: &str, state: ItemState) -> Draft {
        Draft { title: title.to_string(), state, ..Draft::default() }
    }

    fn id(n: u64) -> ItemId {
        ItemId(n.to_string())
    }

    #[test]
    fn detect_wants_a_github_origin_and_gh_logged_in() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        assert!(tracker.detect(&fake.repo()));

        fake.set_authed(false);
        assert!(!tracker.detect(&fake.repo()), "gh auth status fails");
        fake.set_authed(true);

        let no_gh = GithubTracker::with_gh(fake.repo(), missing_gh(fake.dir.path()));
        assert!(!no_gh.detect(&fake.repo()), "gh isn't installed");

        fake.git(&["remote", "set-url", "origin", "https://gitlab.com/acme/widgets.git"]);
        assert!(!tracker.detect(&fake.repo()), "origin isn't on github.com");

        fake.git(&["remote", "remove", "origin"]);
        assert!(!tracker.detect(&fake.repo()), "no origin at all");
        // Without a remote, `gh` isn't even asked.
        assert_eq!(fake.log().matches("auth status").count(), 2);
    }

    #[test]
    fn github_slug_reads_every_url_form() {
        for url in [
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets",
            "https://github.com/acme/widgets/",
            "https://x-access-token:abc@github.com/acme/widgets.git",
            "git@github.com:acme/widgets.git",
            "github.com:acme/widgets",
            "ssh://git@github.com/acme/widgets.git",
            "ssh://git@ssh.github.com:443/acme/widgets.git",
            "git://GitHub.com/acme/widgets",
        ] {
            assert_eq!(github_slug(url).as_deref(), Some("acme/widgets"), "{url}");
        }
        for url in [
            "https://gitlab.com/acme/widgets.git",
            "git@github.example.com:acme/widgets.git",
            "https://github.com/acme",
            "https://github.com/acme/widgets/issues",
            "/srv/git/widgets.git",
            "../widgets",
        ] {
            assert_eq!(github_slug(url), None, "{url}");
        }
    }

    #[test]
    fn create_get_and_list_round_trip() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        let draft = Draft {
            title: "Land from the board: the L key".to_string(),
            body: "Add an L key.\n\nAfter a merge check.\n".to_string(),
            recipe: Some("land-feature".to_string()),
            from: Some(id(38)),
            state: ItemState::Proposed,
        };
        let created = tracker.create(&draft).unwrap();
        assert_eq!(created, id(1));
        let item = tracker.get(&created).unwrap();
        assert_eq!(
            item,
            Item {
                id: created.clone(),
                title: draft.title.clone(),
                state: ItemState::Proposed,
                recipe: draft.recipe.clone(),
                from: draft.from.clone(),
                body: draft.body.clone(),
            }
        );
        assert_eq!(tracker.list(&ItemFilter::default()).unwrap(), vec![item]);
        assert_eq!(
            fake.issue_file(1, "body"),
            "Add an L key.\n\nAfter a merge check.\n\nkanstack-recipe: land-feature\nkanstack-from: #38\n"
        );
        assert_eq!(fake.issue_file(1, "labels"), "kanstack:proposed\n");
        // Scoped to the origin: the stand-in refuses any other --repo, and every call named it.
        assert!(fake.log().lines().filter(|line| line.starts_with("issue ") || line.starts_with("label ")).all(|line| line.contains(SLUG)));
    }

    #[test]
    fn list_is_oldest_first_and_filters_by_state() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        for (title, state) in [("a", ItemState::Backlog), ("b", ItemState::Running), ("c", ItemState::Backlog), ("d", ItemState::Done)] {
            tracker.create(&draft(title, state)).unwrap();
        }
        let titles = |filter: ItemFilter| tracker.list(&filter).unwrap().into_iter().map(|item| item.title).collect::<Vec<_>>();
        assert_eq!(titles(ItemFilter::default()), ["a", "b", "c", "d"]);
        assert_eq!(titles(ItemFilter::state(ItemState::Backlog)), ["a", "c"]);
        assert_eq!(titles(ItemFilter::state(ItemState::Running)), ["b"]);
        assert_eq!(titles(ItemFilter::state(ItemState::Done)), ["d"]);
        // Only `done` pages through closed issues.
        let log = fake.log();
        assert_eq!(log.matches("--state open").count(), 2);
        assert_eq!(log.matches("--state all").count(), 2);
    }

    #[test]
    fn set_state_swaps_the_label_and_closes_and_reopens() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        let item = tracker.create(&draft("T", ItemState::Backlog)).unwrap();
        assert_eq!(fake.issue_file(1, "labels"), "kanstack:backlog\n");

        tracker.set_state(&item, ItemState::Running).unwrap();
        assert_eq!((fake.issue_file(1, "labels").as_str(), fake.issue_file(1, "state").as_str()), ("kanstack:running\n", "OPEN\n"));

        tracker.set_state(&item, ItemState::Done).unwrap();
        assert_eq!((fake.issue_file(1, "labels").as_str(), fake.issue_file(1, "state").as_str()), ("kanstack:done\n", "CLOSED\n"));
        assert_eq!(tracker.get(&item).unwrap().state, ItemState::Done);

        tracker.set_state(&item, ItemState::Review).unwrap();
        assert_eq!((fake.issue_file(1, "labels").as_str(), fake.issue_file(1, "state").as_str()), ("kanstack:review\n", "OPEN\n"));
        assert_eq!(tracker.get(&item).unwrap().state, ItemState::Review);

        // Setting the state it already has touches nothing.
        let before = fake.log().lines().count();
        tracker.set_state(&item, ItemState::Review).unwrap();
        let after: Vec<String> = fake.log().lines().skip(before).map(str::to_string).collect();
        assert!(after.iter().all(|line| line.starts_with("issue view")), "{after:?}");
    }

    #[test]
    fn set_state_leaves_other_labels_and_clears_extra_state_labels() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        fake.put_issue(1, "T", "", true, &["bug", "kanstack:running", "kanstack:failed"]);
        assert_eq!(tracker.get(&id(1)).unwrap().state, ItemState::Failed, "furthest along wins");
        tracker.set_state(&id(1), ItemState::Backlog).unwrap();
        assert_eq!(fake.issue_file(1, "labels"), "bug\nkanstack:backlog\n");
    }

    #[test]
    fn an_issue_closed_on_github_is_done_whatever_its_label() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        fake.put_issue(1, "closed by hand", "", false, &["kanstack:backlog"]);
        assert_eq!(tracker.get(&id(1)).unwrap().state, ItemState::Done);
        assert!(tracker.list(&ItemFilter::state(ItemState::Backlog)).unwrap().is_empty());
        // And moving it out of done reopens it.
        tracker.set_state(&id(1), ItemState::Backlog).unwrap();
        assert_eq!(fake.issue_file(1, "state"), "OPEN\n");
        assert_eq!(tracker.get(&id(1)).unwrap().state, ItemState::Backlog);
    }

    #[test]
    fn unlabeled_issues_are_backlog_when_open_and_done_when_closed() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        fake.put_issue(1, "open, by a person", "Body", true, &["bug"]);
        fake.put_issue(2, "closed, by a person", "", false, &[]);
        let items = tracker.list(&ItemFilter::default()).unwrap();
        let states: Vec<_> = items.iter().map(|item| (item.id.0.as_str(), item.state, item.recipe.clone())).collect();
        assert_eq!(states, [("1", ItemState::Backlog, None), ("2", ItemState::Done, None)]);
        assert_eq!(items[0].body, "Body\n");
        assert!(fake.labels().is_empty(), "reading creates no labels");
    }

    #[test]
    fn creates_each_label_once_the_first_time_it_is_needed() {
        let fake = Fake::new();
        std::fs::write(fake.data().join("labels"), "bug\nkanstack:running\n").unwrap();
        let tracker = tracker(&fake);
        let a = tracker.create(&draft("a", ItemState::Backlog)).unwrap();
        tracker.create(&draft("b", ItemState::Backlog)).unwrap();
        tracker.set_state(&a, ItemState::Running).unwrap();
        tracker.set_state(&a, ItemState::Review).unwrap();
        assert_eq!(fake.labels(), ["bug", "kanstack:running", "kanstack:backlog", "kanstack:review"]);
        let log = fake.log();
        assert_eq!(log.matches("label list").count(), 1, "the labels are fetched once:\n{log}");
        assert_eq!(log.matches("label create").count(), 2, "{log}");
        assert!(log.contains("label create kanstack:backlog --repo acme/widgets --color ededed"), "{log}");
    }

    #[test]
    fn a_label_another_process_created_meanwhile_is_not_an_error() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        tracker.create(&draft("a", ItemState::Backlog)).unwrap();
        // Made by someone else after this tracker listed the labels.
        std::fs::write(fake.data().join("labels"), "kanstack:backlog\nkanstack:running\n").unwrap();
        tracker.set_state(&id(1), ItemState::Running).unwrap();
        assert_eq!(fake.issue_file(1, "labels"), "kanstack:running\n");
    }

    #[test]
    fn recipe_and_from_lines_round_trip_through_the_body() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        for (body, recipe, from) in [
            ("", Some("bugfix"), None),
            ("", None, Some(id(7))),
            ("Text.", None, None),
            ("Line one\n\nkanstack-recipe: quoted, not mine\n\nLast line", Some("r"), Some(id(2))),
        ] {
            let draft = Draft { body: body.to_string(), recipe: recipe.map(str::to_string), from: from.clone(), ..draft("T", ItemState::Backlog) };
            let item = tracker.get(&tracker.create(&draft).unwrap()).unwrap();
            let want = if body.is_empty() { String::new() } else { format!("{body}\n") };
            assert_eq!((item.body, item.recipe.as_deref(), item.from), (want, recipe, from));
        }
        // A `#N` from is stored as given, not as `##N`.
        let item = tracker.create(&Draft { from: Some(ItemId("#3".to_string())), ..draft("T", ItemState::Backlog) }).unwrap();
        assert!(fake.issue_file(item.0.parse().unwrap(), "body").ends_with("kanstack-from: #3\n"));
    }

    #[test]
    fn decode_body_takes_lines_edited_on_github() {
        let (body, recipe, from) = decode_body("Fix it.\r\n\r\nkanstack-from: #12  \r\n\r\nkanstack-recipe: bugfix # by hand\r\n\r\n");
        assert_eq!((body.as_str(), recipe.as_deref(), from), ("Fix it.\n", Some("bugfix"), Some(id(12))));
        let (body, recipe, from) = decode_body("kanstack-recipe: a\nmore text\n");
        assert_eq!((body.as_str(), recipe, from), ("kanstack-recipe: a\nmore text\n", None, None));
    }

    #[test]
    fn comment_adds_an_issue_comment() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        let item = tracker.create(&draft("T", ItemState::Running)).unwrap();
        tracker.comment(&item, "### build: done\n\nAll green.\n").unwrap();
        assert_eq!(fake.issue_file(1, "comments"), "### build: done\n\nAll green.\n--\n");
        assert_eq!(tracker.get(&item).unwrap().body, "", "reports aren't in the body");
        assert!(tracker.comment(&id(9), "x").unwrap_err().to_string().contains("gh issue comment failed"));
    }

    #[test]
    fn link_text_closes_the_issue() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        assert_eq!(tracker.link_text(&id(44)).as_deref(), Some("Closes #44"));
        assert_eq!(tracker.link_text(&ItemId("#44".to_string())).as_deref(), Some("Closes #44"));
        assert_eq!(tracker.link_text(&ItemId("LIN-1".to_string())), None);
    }

    #[test]
    fn rejects_ids_that_are_not_issue_numbers() {
        let fake = Fake::new();
        let tracker = tracker(&fake);
        assert!(tracker.get(&ItemId("../x".to_string())).is_err());
        assert!(tracker.set_state(&ItemId("LIN-1".to_string()), ItemState::Done).is_err());
        assert!(fake.log().is_empty(), "gh isn't asked about an id that can't be an issue");
    }

    #[test]
    fn errors_name_what_is_missing() {
        let fake = Fake::new();
        let no_gh = GithubTracker::with_gh(fake.repo(), missing_gh(fake.dir.path()));
        let err = no_gh.list(&ItemFilter::default()).unwrap_err().to_string();
        assert!(err.contains("isn't installed") && err.contains("KANSTACK_GH_BIN"), "{err}");

        fake.git(&["remote", "set-url", "origin", "https://gitlab.com/acme/widgets.git"]);
        let err = tracker(&fake).list(&ItemFilter::default()).unwrap_err().to_string();
        assert!(err.contains("isn't a github.com repository"), "{err}");
    }
}
