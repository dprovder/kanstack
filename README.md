# kanstack

A kanban-style terminal UI for [GitButler](https://gitbutler.com)'s `but` CLI.

GitButler's whole premise is *parallel* stacks of work. Its built-in TUI renders them as a
single vertical commit graph, which flattens the one axis that makes the model interesting.
`kanstack` draws them as a board instead: one lane per stack, one card per commit, a backlog
lane for unstaged work.

```
  workspace  ·  base 3189356

 ● unassigned  2             ● feat-auth  2              ● feat-ui  2
 ──────────────────────────  ──────────────────────────  ──────────────────────────
 mv                          unpushed                    unpushed
 wip1.txt                    d5                          ca
 added                       Wire session refresh into   Fix settings tab focus
                             gateway                     ring
 nr                          a2.txt                      b2.txt                                        ›
 wip2.txt                    d593db0  Dani               ca3b30b  Dani
 added
                             e1                          47
                             Add auth middleware         Redesign settings page
                             a.txt                       b.txt
                             e17b60c  Dani               47a6412  Dani

  ←/→ lane · ↑/↓ card · m move · u unstage · c commit · b branch
  s stack · ⏎ diff · d delete · r rebase · p push · M land · z/Z undo/redo · ? help · q quit
```

## Install

Two things are required first:

1. **The [GitButler CLI](https://docs.gitbutler.com/cli-overview)**, `but` 0.21 or newer, on
   your `PATH` — 0.21 changed enough of the CLI's flags and JSON that older releases are no
   longer supported (see [Version compatibility](#version-compatibility)).

   Just the CLI, no GUI app (macOS or Linux):
   ```sh
   curl -fsSL https://gitbutler.com/install.sh | sh
   ```
   If you also want the GitButler desktop app, on macOS:
   ```sh
   brew install --cask gitbutler
   ```
   (this also puts `but` on your `PATH`). Otherwise, follow GitButler's own install docs
   linked above.
2. **A repository you have run `but setup` in.** If you don't have one yet:
   ```sh
   cd your-project
   but setup
   ```

Then install `kanstack` itself — no Rust toolchain required:

```sh
brew install dprovder/tap/kanstack
```

Or grab a prebuilt binary for macOS (Apple Silicon or Intel) or Linux (x86_64 or aarch64)
from the [Releases page](https://github.com/dprovder/kanstack/releases) and put it on your
`PATH`.

Then, from inside the repository you ran `but setup` in:

```sh
kanstack
```

### Building from source

If you'd rather build it yourself (or want to hack on it), this does need Rust — install it
via [rustup](https://rustup.rs) if you don't have it already:

```sh
git clone https://github.com/dprovder/kanstack
cd kanstack
cargo install --path .
```

## Tutorial

```sh
kanstack --tutorial
```

Builds a real, throwaway GitButler workspace in a temp directory and walks through moving,
diffing, staging, committing, and landing a change — each step only advances once you've
actually done it, not on any keypress. Nothing it touches is one of your real projects; the
practice repo is thrown away when you're done. `esc` or `q` leaves any time.

## Keys

| key | does |
|---|---|
| `←/→`, `h/l` | move between lanes — wraps around |
| `↑/↓`, `j/k` | move between cards — wraps around |
| `g` / `G` | first / last card |
| `m` | pick up a card, then `←/→` for a lane, `↑/↓` to drop on a card, `⏎` to confirm |
| `u` | send this card back to the backlog — uncommit a commit, unstage a file |
| `d` | delete this lane — asks first |
| `r` | rebase onto the updated target — shows what will happen |
| `⏎` | open the diff beside the board — `←` goes back, `m` stages one hunk |
| `c` | commit the files staged to this lane (only what is staged) |
| `b` | new branch — stacks on the selected lane, `tab` for a parallel lane |
| `s` | stack this whole lane onto another — rewrites history |
| `p` | push this lane — shows what it will do first |
| `M` | land this lane onto the target, no PR — shows what will happen first |
| `z` / `Z` | undo / redo the last operation — fires immediately, no confirm |
| `?` | help |
| `q` | quit |

Every move runs `but rub SOURCE TARGET`, and the footer spells out what will happen before
you confirm — dropping a commit on the backlog lane is an *uncommit*, not a move, and that
should never be a surprise.

| drop | result |
|---|---|
| commit → lane | move the commit to that branch |
| commit → commit | squash them together |
| commit → unassigned | uncommit it into the worktree |
| file → lane | stage it to that branch |
| file → commit | amend it into that commit |
| file → unassigned | unstage it |

Squash and amend are not separate features — they are the same `rub`, aimed at a card
instead of a lane. `u` is the same again, aimed at the backlog.

**Committing takes only what is staged to the lane.** `but commit` otherwise sweeps in
every unassigned change as well — documented, sensible for a command line, and wrong for a
board, where putting cards in a lane is precisely how you say what belongs in the commit.
kanstack always passes `--only`.

## How it talks to GitButler

`kanstack` links no GitButler code. It spawns the `but` binary you installed and reads its
documented JSON output — `but status -f --format json` to read, `but rub … --format json`
to write.

Mutating commands like `rub`, `commit`, and `move` embed a status in their reply by
default — 0.21 removed the old opt-in `--status-after` flag in favour of always including
it. That embedded status still is not equivalent to `but status -f`, though: it omits
per-commit file lists, and these commands have no `-f` of their own to ask for them. A move
therefore costs two queries, roughly 180ms.

Navigation never shells out, so arrow keys are instant. `but status` is around 90ms and
measurement suggests it is dominated by fixed startup rather than repository size
(`tests/live.rs` guards this).

**There is no refresh key.** A filesystem watcher follows the worktree and `.git`, so the
board tracks changes made in your editor or another terminal on its own. The watch
respects `.gitignore`, which is what stops one `cargo build` from drowning it in `target/`
events.

### Version compatibility

The JSON is stable by intent — upstream documents stability as a goal — but it carries no
schema version and its types are `pub(crate)`, so there is no semver promise to lean on.
`kanstack` therefore checks `but --version` at startup and refuses to run below 0.21, and
notes in the UI when you are on a release newer than it has been verified against.

0.21 is a hard floor, not a soft one: it dropped `-j`/`--json` and `--status-after` in
favour of `--format json`, a syntax 0.19 does not understand, so there is no flag spelling
that works on both sides of that release. `kanstack` picks a side rather than branching on
version at every call site. If you hacked on an earlier version of this project against
0.19, expect to re-verify every call site — see `but.rs` and `git log` for what changed.

The bindings in `src/model.rs` track the *wire* format, which differs from upstream's Rust
structs in ways worth knowing if you hack on this: `createdAt` is RFC3339 despite a doc
comment claiming otherwise, and `MergeStatus::Conflicted` arrives as an object while its
sibling variants are bare strings. `but status`'s `stacks` array also arrives
newest-created-first on the wire (verified against 0.21.2) — `but::parse_status` reverses
it back to oldest-first so lane position stays stable as you add lanes, which is the one
place in this codebase where the wire order and the order everything else assumes
deliberately differ. Unknown *new* fields are ignored; a field we depend on going missing
is a hard error rather than a half-rendered board.

## Reporting a bug

Snapshot mode renders a captured payload offline, so a report needs no access to your repo:

```sh
but status -f --format json > board.json
kanstack --snapshot board.json --size 160x40
```

Attach `board.json`. Redact it first if your branch names are sensitive.

## Tests

```sh
cargo test                                            # unit + rendering
cargo test --test live -- --ignored --test-threads=1  # against a real `but`
```

The live tests build a throwaway repository with `HOME` redirected into a temp directory,
so they never touch your GitButler project registry or settings.

## Status

Usable. Reading, moving, staging by hunk, committing, branching, stacking, deleting,
pushing, rebasing, landing, and undo/redo all work and are covered by tests against the
real CLI. Not yet built:

- **CI and review badges are unverified.** The wire types are bound and rendered, but every
  workspace tested so far had no forge attached, so `ci` and `reviewId` were always null.
  If you use this with real PRs, that is where bugs will be.
- Opening a PR (`but pr new`) as an alternative to `M`, for projects that land through
  review rather than landing directly.
- Flipping through adjacent cards' diffs without leaving the pane. `→` used to do this,
  but both arrows now close, which is the clearer rule; `n`/`p` or `[`/`]` would give the
  behaviour back without overloading the arrows.
- Reordering commits within a lane. `but move <commit> <target> -a` does it; the open
  question is the gesture, since dropping a card on a card already means squash.

## The diff pane, and hunk staging

`⏎` opens the diff **beside** the board rather than over it, so you keep your place. `←`
goes back to the board — the diff sits to the right, so leaving it is a direction rather
than a second meaning for `⏎`. `→` walks on to the next lane's diff without leaving. `tab` widens it to full width when
you want to read properly. This is the split [gitui](https://github.com/gitui-org/gitui)
uses, for the same reason.

Added and removed lines carry a `+` / `-` in a column of their own, next to the new-file
line number. gitui relies on colour alone; a marker survives being read without it.

## Line counts

`+`/`-` appear on every card, on each branch of a stack, and on the lane and workspace
headers. In a stack each branch totals only its own commits, so a lane like
`feat-auth +1  2  +31 -4` sitting above `● fix-flaky-tests  1  +10 -1` tells you which
branch is the big one.

Uncommitted changes all come from the single `but diff` run alongside `but status`. Commits
need `but diff <sha>` *each*, which would be ruinous per refresh — except a commit's content
is fixed by its hash, so those results are cached by SHA and never expire. Only hashes never
seen before cost anything, which after the first draw is usually none of them.

The workspace header counts working-tree changes only; it says "uncommitted", so including
committed lines there would make it lie.

A working-tree file shows its hunks; a commit shows its own diff, read-only.

The useful part is that `but diff --format json` emits **one entry per hunk**, each with its own id
that `rub` accepts. So `m` inside the pane picks up the hunk under the cursor and hands it
to the same lane-targeting flow cards use — which means one file's hunks can go to
different lanes. That is the only way to split a file that touches two unrelated things.

Committed hunks carry no id and are marked as such; history is not stageable.

Hunk ids describe the *current* state, and staging one renumbers the rest, so the pane
closes when you stage. Reopening re-queries rather than trusting a stale list.

## Rebasing onto the target

`r` runs `but pull`, which GitButler describes as rebasing every applied branch on top of
the updated target. It is gated behind `but pull --check`, which is read-only and reports
the incoming commits plus each lane's outcome — `rebases cleanly`, `already integrated`, or
`conflicts` — before anything moves. Worktree conflicts are called out too, and the dialog
turns red and says "rebase anyway" when either is true.

The header has always shown how far behind the target you are; without this that number
was a dead end.

Note `r` here is *rebase*, whereas GitButler's own TUI binds `r` to squash — worth knowing
if you use both.

## Deleting a lane

`d` always deletes the lane's *tip* branch, asks first, and says what will happen.

It can lose commits — verified against 0.21.2, this changed from earlier `but`: deleting a
branch (alone, or the tip or base of a stack) now discards *its own* commits outright,
non-interactively, with no refusal and no folding into a neighbouring branch. `but undo` is
the safety net instead, so the confirmation names that rather than implying nothing can be
lost.

## Undo and redo

`z` runs `but undo`, `Z` runs `but redo`. Both fire immediately with no confirmation dialog
— unlike every other mutation here, undo/redo *is* the confirmation: it is what makes `d`
safe to bind at all now that deleting no longer refuses to lose work, and it reverses
anything else, including a `land`.

`but undo`/`but redo` restore the entire prior workspace state, uncommitted changes
included, not just the last commit. Both always succeed and print nothing, whether or not
there was anything to undo or redo — verified against 0.21.2, there is no way to tell a real
undo from a silent no-op except by comparing the board before and after, so the notification
here just says "undid"/"redid" without claiming to know which happened.

## Lane and branch state

Each branch shows a coloured dot and a word, both derived from one value so they cannot
disagree — they did once, and an empty lane came out the same green as a fully pushed one.

| state | meaning |
|---|---|
| empty | no commits, nothing staged |
| uncommitted | staged work, not yet committed |
| unpushed | commits that have never reached the remote |
| needs force | pushing would rewrite remote history |
| pushed | everything is on the remote |
| integrated | merged into the target |
| conflicted | at least one commit is in conflict — outranks the rest |

In a stack, every branch gets its own dot and word, not just the tip — but only the tip's
dot is **filled** (`●`); every branch stacked below it gets a **hollow** one (`○`). That
distinction carries real information: `c`/`p`/`M`/`z` always act on the tip, never on
whichever branch's cards you happen to be scrolled into, and the hollow dot is there so a
lower branch's header never reads as if it were the one those keys will act on.

The current lane's own header is reverse-video highlighted too — not just a bolder title
and a brighter rule, which read as too subtle to tell which of several lanes is current at
a glance once there's more than two or three on screen.

## Restacking an existing branch

`s` stacks one existing lane onto another: a single `but move <branch> <target>`. Its
commits rebase onto the target's tip and it becomes the new lane tip.

This is new in 0.21. Earlier `but` exposed no subcommand for it — the capability existed in
`but_api::branch::move_branch` and only their own TUI reached it, and `rub` between two
branches reassigns *uncommitted changes* rather than restacking — so this used to be
composed from four separate calls (a temp branch anchored on the target, `rub` each commit
across oldest-first, delete the source, rename the temp branch back). `but move` replaced
all of that with one native, atomic call.

**This rewrites history.** Moved commits get new SHAs, so a lane that was already pushed
will need a force push afterwards — the push preview will say so.

## Identifiers, and the fuzzy-matching trap

`but rub` re-resolves its arguments with fuzzy matching, and refuses rather than guessing
when more than one thing matches. A two-character commit id like `ea` matches the branch
`feat`, because the branch *name* contains those characters — which is fatal for a
non-interactive caller.

So the ids shown on cards are not the ids sent to `but`. Commits are rubbed by full hash,
lanes by branch name; only file changes use their CLI id, having no alternative.

## Push, and why it asks first

`p` runs `but push <branch> --dry-run` and shows you the result before doing anything —
destination, commit list, and whether a force is involved. That is not ceremony:

- **`but push` force-pushes by default.** `--with-force` is `default_value_t = true`.
- **A branch is always named explicitly.** With no branch and a non-interactive stdin,
  `but push` does not prompt — it pushes *every* branch with unpushed commits.
- **With GitHub native stacked PRs**, a stack push temporarily retargets open PRs onto
  trunk, which has been able to transiently merge or close them. Opt-in, and fixed
  upstream in July 2026, but worth knowing.

Hook flags are deliberately never passed: the spelling changed from `--run-hooks` to
`--no-hooks`, so naming either one breaks on one side of that release.

## Landing onto the target, and why it asks first

`M` runs `but land <branch>`, which lands the lane's commits directly onto the configured
target — no pull request — and then reconciles every other applied lane onto the result,
the same as `but pull` does. That is the actual "get this into trunk" step; `p` only gets a
branch to the remote for review.

`land` is what replaced `but merge` in 0.21 (see [Version compatibility](#version-compatibility)),
and it is strictly more capable: `merge` only ever worked when the workspace target was
GitButler's local-only `gb-local` convention, and refused outright — "Target remote is
origin, not gb-local. This command only works with gb-local targets" — for a target
tracking a real remote (verified against 0.21.2, against this project's own repository).
`land` works either way: local targets are updated locally, real-remote targets are pushed
to directly.

It is gated behind a preview, the same way push and rebase are. `but land` has no
`--dry-run` of its own, so the preview instead comes from `but branch show <branch>
--check`, which reports the commits that would land and whether they land cleanly without
touching anything. The dialog turns red and says "land anyway" when they would not.

Bound to `M`, not `m`: that key already means "move a card." And not `l`: that is a
navigation key (`←/→`, `h/l` move between lanes).

**Read this before binding it to a project that uses pull requests.** `land` deliberately
bypasses PR-based review — that is the whole point of the command — so it is the wrong tool
if your project's process expects one. GitButler's own guidance here: use `but push` and
`but pr new` instead. A branch-protected remote will reject the land regardless.

**`z` does not undo a push to a real remote — only your local workspace.** Verified by
landing onto a real bare-repo remote and then undoing: `but undo` fully restores the local
board (the branch and its commits reappear, exactly as if nothing happened), but the
remote's ref stays at the landed commit — `git log` on the remote confirms it, and
`but status` correctly reports the workspace as now behind upstream because of it. Nothing
is lost — the content is safely on the remote the whole time, and re-landing correctly
reports nothing to do — but the local board *looks* fully reverted when only the local
half is. The land confirmation says so before you commit to it, since that's the moment it's
actually useful to know.

## Licence and relationship to GitButler

`kanstack` is MIT licensed. It is an independent program that invokes the `but` CLI as a
subprocess; it contains and links no GitButler source, and is not a derivative work of it.

**This project is not affiliated with, endorsed by, or sponsored by GitButler Inc.**
"GitButler" is their name, used here only to describe what this tool interoperates with.
GitButler itself is distributed under the Functional Source License (FSL-1.1-MIT), whose
terms govern their code — not this repository. If you vendor or fork any GitButler code
into a project, those terms apply to you and this notice does not cover it.

Not legal advice.
