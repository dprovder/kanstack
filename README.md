# kanstack

A kanban-style terminal UI for [GitButler](https://gitbutler.com)'s `but` CLI.

GitButler's whole premise is *parallel* stacks of work. Its built-in TUI renders them as a
single vertical commit graph, which flattens the one axis that makes the model interesting.
`kanstack` draws them as a board instead: one lane per stack, one card per commit, a backlog
lane for uncommitted work.

![kanstack board overview](docs/assets/overview.gif)

The lane header stays pinned no matter how deep you scroll — even into a 25-commit stack, or
past a lower branch's own commits in a stacked lane:

<table>
<tr>
<td><img src="docs/assets/long-stack.gif" alt="the lane header stays visible 20 commits deep into a long run of commits"></td>
<td><img src="docs/assets/stacked-branches.gif" alt="the tip branch's header stays pinned while a hollow-dot header for a lower branch scrolls into view"></td>
</tr>
<tr>
<td>A long run of commits</td>
<td>A lane of three stacked branches</td>
</tr>
</table>

## Install

Two things are required first:

1. **The [GitButler CLI](https://docs.gitbutler.com/cli-overview)**, `but` 0.22 or newer, on
   your `PATH`.

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

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how it talks to `but`, the wire-format
notes, and the version-compatibility details a contributor would need.

## Tutorial

```sh
kanstack --tutorial
```

Builds a real, throwaway GitButler workspace in a temp directory and walks through moving,
diffing, committing, and landing a change — each step only advances once you've actually
done it, not on any keypress. Nothing it touches is one of your real projects; the practice
repo is thrown away when you're done. `esc` or `q` leaves any time.

## Keys

| key | does |
|---|---|
| `←/→`, `h/l` | move between lanes — wraps around |
| `↑/↓`, `j/k` | move between cards — wraps around |
| `shift ←/→` | page by however many lanes fit on screen at once |
| `shift ↑/↓` | skip to the next stacked branch, or the next folder if grouped |
| `g` / `G` | first / last card |
| `space` | select this card for a bulk move — again to deselect |
| `m` | pick up the selection (or just this card), then `←/→` for a lane, `↑/↓` to drop on a card, `⏎` to confirm |
| `u` | send this card back to the backlog — uncommit a commit |
| `a` | branches that aren't in the workspace — `⏎` applies one as a new lane |
| `U` | unapply this lane — its whole stack leaves, `a` brings it back |
| `d` | delete this lane — asks first |
| `r` | rebase onto the updated target — shows what will happen |
| `tab` | on the unassigned lane: group its cards by folder, or back to a flat list |
| `⏎` | open the diff beside the board — `←` goes back, `m` commits or amends one hunk |
| `b` | new branch — stacks on the selected lane, `tab` for a parallel lane; a parallel lane with a harness split enabled (`cmux` if found, else plain `tmux` as a fallback when kanstack is itself running inside a tmux pane) then asks for an optional initial message to seed the harness with, in a modal showing the name, action, and message together. Nothing is created until that step confirms, so `esc` at any point cancels the whole branch. `KANSTACK_BRANCH_UI=footer` squeezes the same prompts into the one-line footer instead. The harness is also always told it's in a GitButler virtual branch, not one `git status`/`git branch` would recognize — via `--append-system-prompt` for `claude`/`pi`, Codex CLI's own `-c developer_instructions=` config override for `codex`, and folded straight into the initial message for anything without a system-prompt mechanism at all (confirmed for OpenCode, Kiro CLI, and Gemini CLI). `KANSTACK_HARNESS_SYSTEM_FLAG` overrides or disables this |
| `t` | send a task to this lane's split pane — spawns one first if it isn't open yet |
| `s` | stack this whole lane onto another — rewrites history |
| `p` | push this lane — shows what it will do first |
| `L` | land this lane onto the target, no PR — shows what will happen first |
| `z` / `Z` | undo / redo the last operation — fires immediately, no confirm |
| `?` | help |
| `q` | quit |

Nothing is ever "staged" ahead of a commit — GitButler 0.22 dropped that step entirely, so
dropping a card onto a lane commits or amends it there immediately, and the footer spells
out which before you confirm.

| drop | result |
|---|---|
| commit → lane | move the commit to that branch |
| commit → commit | squash them together |
| commit → unassigned | uncommit it into the worktree |
| file → empty lane | commit it there — prompts for a message |
| file → lane with commits | amend it into the tip, no prompt |
| file → commit | amend it into that commit |

Squash and amend are not separate features from a plain move — they're the same drop
gesture, aimed at a card instead of a lane. `u` is the mirror image, aimed at the backlog.

**A drop always names the changes it commits explicitly.** `but commit` with no changes
named sweeps in everything uncommitted — fine from a shell, wrong for a board, where
dropping a card onto a lane is precisely how you say what belongs there. kanstack always
passes the specific file or hunk id(s) being moved.

## No refresh key

A filesystem watcher follows the worktree and `.git`, so the board tracks changes made in
your editor or another terminal on its own — that's the whole reason there's no refresh
key. Navigation never shells out, so arrow keys are instant regardless of what the watcher
is doing; the refresh it triggers runs on its own thread and never blocks input.

## Reporting a bug

Snapshot mode renders a captured payload offline, so a report needs no access to your repo:

```sh
but status -f --json > board.json
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

Usable. Reading, moving, committing by hunk, amending, branching, stacking, deleting,
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
- Reordering commits within a lane. `but move <commit> --above/--below <target>` does it;
  the open question is the gesture, since dropping a card on a card already means squash.

## The diff pane, and hunk-level commits

`⏎` opens the diff **beside** the board rather than over it, so you keep your place. `←`
goes back to the board — the diff sits to the right, so leaving it is a direction rather
than a second meaning for `⏎`. `→` walks on to the next lane's diff without leaving. `tab`
widens it to full width when you want to read properly. This is the split
[gitui](https://github.com/gitui-org/gitui) uses, for the same reason.

Added and removed lines carry a `+` / `-` in a column of their own, next to the new-file
line number. gitui relies on colour alone; a marker survives being read without it.

The useful part is that `but diff --json` emits **one entry per hunk**, each with its own id
that `commit`/`amend` accept. So `m` inside the pane picks up the hunk under the cursor and
hands it to the same lane-targeting flow cards use — which means one file's hunks can go to
different lanes, each becoming its own commit or amending into whatever's already there.
That is the only way to split a file that touches two unrelated things.

Committed hunks carry no id and are marked as such; history is not stageable. Committing a
hunk renumbers the rest, so the pane closes after you commit one and re-opens fresh rather
than trusting a now-stale list.

## Line counts

`+`/`-` appear on every card, on each branch of a stack, and on the lane and workspace
headers. In a stack each branch totals only its own commits, so a lane like
`feat-auth +1  2  +31 -4` sitting above `● fix-flaky-tests  1  +10 -1` tells you which
branch is the big one.

A working-tree file shows its hunks; a commit shows its own diff, read-only. The workspace
header counts working-tree changes only; it says "uncommitted", so including committed
lines there would make it lie.

## Rebasing onto the target

`r` runs `but pull`, which GitButler describes as rebasing every applied branch on top of
the updated target. It is gated behind `but pull --check`, which is read-only and reports
the incoming commits plus each lane's outcome — `rebases cleanly`, `already integrated`, or
`conflicts` — before anything moves. Worktree conflicts are called out too, and the dialog
turns red and says "rebase anyway" when either is true.

Note `r` here is *rebase*, whereas GitButler's own TUI binds `r` to squash — worth knowing
if you use both.

## Branches that aren't on the board

The board draws applied stacks, because that is all `but status` reports. A repository
usually holds more than that — branches someone parked, branches a colleague pushed,
branches from a PR you haven't picked up. None of them appear as lanes, and until you go
looking they are invisible.

`a` opens a drawer on the left of the board listing exactly those. `⏎` applies the highlighted one,
which brings it in as a new parallel lane and drops the cursor on it. `U` on any lane does
the reverse: the lane leaves the workspace and reappears in the drawer.

Each row leads with a dot carrying the one fact that decides whether to apply: green means
the branch merges cleanly into the target, red means applying it will conflict. That check
is `but`'s, and it runs without applying anything — so the warning arrives before the
decision rather than after it. Applying a red branch is still allowed; finding out what
conflicts is a legitimate reason to apply one.

Two things worth knowing:

- **`U` unapplies the whole stack, not one branch.** That is `but unapply`'s own behaviour —
  it acts on the stack containing the branch you name — and it happens to match the board
  exactly, since a lane *is* a stack. On a lane with several branches stacked in it the
  confirmation names every branch that would leave, because that is the part you cannot see
  coming.
- **Nothing is destroyed.** Unlike `d`, unapplying keeps the branch and its commits; only
  the working-directory changes come off disk. That is why `U` confirms but does not warn
  the way `d` does.
- **An empty lane is the one that won't come back through the drawer.** `but branch list`
  omits branches with no commits, so a lane you made with `b` and never committed to will
  not be listed after `U`. The branch still exists — `but apply <name>` brings it back —
  and the confirmation says so rather than promising `a` will.

The drawer is fetched when you open it, not on every refresh. `but branch list` runs a
merge check per branch, which is more work than a status read, and paying for it on every
file save to populate a panel that is usually closed would be a poor trade. The practical
consequence: a branch created in another terminal shows up the next time you open the
drawer rather than instantly.

`but branch list` also truncates to the 20 most recent branches by default. When it does,
the drawer says so rather than presenting a partial list as the whole set.

## Deleting a lane

`d` always deletes the lane's *tip* branch, asks first, and says what will happen.

It can lose commits: deleting a branch (alone, or the tip or base of a stack) discards *its
own* commits outright, non-interactively, with no folding into a neighbouring branch. `but
undo` is the safety net instead, so the confirmation names that rather than implying
nothing can be lost.

## Undo and redo

`z` runs `but undo`, `Z` runs `but redo`. Both fire immediately with no confirmation dialog
— unlike every other mutation here, undo/redo *is* the confirmation: it is what makes `d`
safe to bind at all now that deleting no longer refuses to lose work, and it reverses
anything else, including a `land`.

`but undo`/`but redo` restore the entire prior workspace state, uncommitted changes
included, not just the last commit. Both always succeed and print nothing, whether or not
there was anything to undo or redo, so the notification here just says "undid"/"redid"
without claiming to know whether it was a no-op.

## Lane and branch state

Each branch shows a coloured dot and a word, both derived from one value so they cannot
disagree.

| state | meaning |
|---|---|
| empty | no commits |
| uncommitted | staged work, not yet committed — kept for the wire format, but unreachable in practice since 0.22: nothing is ever assigned to a lane ahead of a commit anymore |
| unpushed | commits that have never reached the remote |
| needs force | pushing would rewrite remote history |
| pushed | everything is on the remote |
| integrated | merged into the target |
| conflicted | at least one commit is in conflict — outranks the rest |

In a stack, every branch gets its own dot and word, not just the tip — but only the tip's
dot is **filled** (`●`); every branch stacked below it gets a **hollow** one (`○`). That
distinction carries real information: `p`/`M`/`z` always act on the tip, never on whichever
branch's cards you happen to be scrolled into, and the hollow dot is there so a lower
branch's header never reads as if it were the one those keys will act on.

The current lane's own header is reverse-video highlighted too, so it's still obvious which
lane is current once there's more than two or three on screen.

## Grouping the unassigned lane by folder

A flat list of loose files stops being navigable by eye once there are a lot of them. `tab`
on the unassigned lane groups its cards by directory instead, with a `▸ path  N` divider
between groups — the same idea as a stacked branch's own hollow-dot header, just without a
push status a bare directory doesn't have. `tab` again goes back to the flat list.

## Moving several cards at once

`space` marks a card for a bulk move (`✓`), and the header counts how many are marked so
the state is never just off-screen checkmarks you have to scroll back to remember. `m` with
a marked selection picks up all of it — not just whatever the cursor happens to be sitting
on — and drops the whole thing wherever you confirm, same target for all of them.

A selection can mix commits and files, or span more than one lane. Sources are grouped by
kind and each group becomes one combined `commit`/`amend`/`squash`/`move`/`uncommit` call
rather than one call per card, so the confirmation just says how many and where rather than
naming one verb that would be wrong for half the selection. Cancelling with `esc` leaves the
selection alone, so a bulk move can be retried at a different target — `esc` with nothing
else pending is what actually clears it.

## Restacking an existing branch

`s` stacks one existing lane onto another. Its commits rebase onto the target's tip and it
becomes the new lane tip.

**This rewrites history.** Moved commits get new SHAs, so a lane that was already pushed
will need a force push afterwards — the push preview will say so.

## Identifiers on cards

The ids shown on cards are not always what you'd type into `but` yourself: commits are
addressed by full hash, lanes by branch name; only file changes use their CLI id, having no
alternative. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) if you're scripting against
`but` directly and wondering why.

## Push, and why it asks first

`p` runs `but push <branch> --dry-run` and shows you the result before doing anything —
destination, commit list, and whether a force is involved. That is not ceremony:

- **`but push` force-pushes by default.**
- **A branch is always named explicitly.** With no branch and a non-interactive stdin,
  `but push` does not prompt — it pushes *every* branch with unpushed commits.
- **With GitHub native stacked PRs**, a stack push temporarily retargets open PRs onto
  trunk, which can transiently merge or close them if you're on an affected GitButler
  version — worth knowing either way.

## Landing onto the target, and why it asks first

`M` runs `but land <branch>`, which lands the lane's commits directly onto the configured
target — no pull request — and then reconciles every other applied lane onto the result,
the same as `but pull` does. That is the actual "get this into trunk" step; `p` only gets a
branch to the remote for review.

It is gated behind a preview, the same way push and rebase are, reporting the commits that
would land and whether they land cleanly before touching anything. The dialog turns red and
says "land anyway" when they would not.

Bound to `M`, not `m`: that key already means "move a card." And not `l`: that is a
navigation key (`←/→`, `h/l` move between lanes).

Once confirmed, `land` itself runs on a background thread with a spinner over the board,
rather than freezing the UI for however long the push takes — real-remote targets mean a
network round trip, not just a local ref update. Input is swallowed while it's in flight (no
cancel key: by the time the spinner is up it may already be pushing) and the board updates
the moment the result comes back.

**On a stacked lane, `M` lands every branch in it, base first.** `but land` refuses a
non-base branch outright — "it is stacked on top of ... other segment(s)" — and there is no
flag or stack-id argument that lands the whole thing for you, so kanstack does it as one
`but land` call per branch instead of surfacing that refusal. The confirmation dialog lists
every branch it's about to land, in order, before you commit to it.

**Read this before binding it to a project that uses pull requests.** `land` deliberately
bypasses PR-based review — that is the whole point of the command — so it is the wrong tool
if your project's process expects one. GitButler's own guidance here: use `but push` and
`but pr new` instead. A branch-protected remote will reject the land regardless.

**`z` does not undo a push to a real remote — only your local workspace.** Landing onto a
real remote and then undoing fully restores the local board (the branch and its commits
reappear, exactly as if nothing happened), but the remote's ref stays at the landed commit.
Nothing is lost — the content is safely on the remote the whole time, and re-landing
correctly reports nothing to do — but the local board *looks* fully reverted when only the
local half is. The land confirmation says so before you commit to it.

## Licence and relationship to GitButler

`kanstack` is MIT licensed. It is an independent program that invokes the `but` CLI as a
subprocess; it contains and links no GitButler source, and is not a derivative work of it.

**This project is not affiliated with, endorsed by, or sponsored by GitButler Inc.**
"GitButler" is their name, used here only to describe what this tool interoperates with.
GitButler itself is distributed under the Functional Source License (FSL-1.1-MIT), whose
terms govern their code — not this repository. If you vendor or fork any GitButler code
into a project, those terms apply to you and this notice does not cover it.

Not legal advice.
