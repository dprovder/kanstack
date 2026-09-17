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

`b` spins up a new lane and, when a harness split is available (`cmux`, or `tmux` as a
fallback), hands it straight to a coding agent — told which GitButler branch it's in, not
just dropped into a plain checkout:

![kanstack spinning up a new branch with a coding agent split into a pane beside it](docs/assets/agent-launch.gif)

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

## Setup

The very first time kanstack runs on a machine, it opens a short wizard before the board:
it checks whether `but`, `cmux`, and `tmux` are found (and, for `cmux`, whether the required
`but` version is installed), and lets you pick a default harness — cycling with `←`/`→`
through whichever known harnesses (`claude`, `codex`, `pi`, `opencode`, `kiro`, `gemini`)
were actually found on `PATH`, or just typing a custom command — and a split backend
(`auto`/`cmux`/`tmux`). Saving remembers your choices in
`$XDG_CONFIG_HOME/kanstack/env` (or `$HOME/.config/kanstack/env`), so you don't need to
export the equivalent environment variables every session — an explicit environment
variable always overrides what's saved there. `esc` skips it without picking anything.

Two more rows appear only when there's something to do: if `but` itself isn't found, an
"install GitButler CLI" action (with a confirm first, since it runs GitButler's own
`curl -fsSL https://gitbutler.com/install.sh | sh`); if `but` is found but has no
up-to-date coding-agent skill installed (`but skill check`), an "install/update GitButler
skill" action — the file that teaches whichever harness you spawn in a lane to use `but`
instead of plain `git`. Neither row shows once there's nothing left to fix.

Right after, first run also offers a one-time, equally skippable prompt to walk through
the tutorial below. Neither the setup wizard nor the tutorial offer reappears once you've
been through them once; run either again any time with:

```sh
kanstack --setup
kanstack --tutorial
```

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
| `b` | new branch — stacks on the selected lane, `tab` for a parallel lane with a harness split (`cmux`, or `tmux` as a fallback); asks for an optional initial message before creating anything, so `esc` cancels the whole branch |
| `t` | send a task to this lane's split pane — spawns one first if it isn't open yet |
| `s` | stack this whole lane onto another — rewrites history |
| `p` | push this lane — shows what it will do first |
| `L` | land this lane onto the target, no PR — shows what will happen first |
| `M` | open a PR for this lane's branch — pushes first if needed, then a modal for title/draft before it runs |
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
pushing, rebasing, landing, opening a PR, and undo/redo all work and are covered by tests
against the real CLI. Not yet built:

- **CI and review badges are unverified.** The wire types are bound and rendered, but every
  workspace tested so far had no forge attached, so `ci` and `reviewId` were always null.
  If you use this with real PRs, that is where bugs will be. Opening a PR itself (`M`) is
  untested against a real forge for the same reason.
- Flipping through adjacent cards' diffs without leaving the pane. `→` used to do this,
  but both arrows now close, which is the clearer rule; `n`/`p` or `[`/`]` would give the
  behaviour back without overloading the arrows.
- Reordering commits within a lane. `but move <commit> --above/--below <target>` does it;
  the open question is the gesture, since dropping a card on a card already means squash.

See [docs/BEHAVIOR.md](docs/BEHAVIOR.md) for the reasoning behind specific keys and drop
targets — hunk-level diffs, line-count semantics, rebasing, the branches drawer, deleting a
lane, undo/redo, lane-state dots, grouping, bulk moves, restacking, card identifiers, and
why push, land, and opening a PR each ask first.

## Licence and relationship to GitButler

`kanstack` is MIT licensed. It is an independent program that invokes the `but` CLI as a
subprocess; it contains and links no GitButler source, and is not a derivative work of it.

**This project is not affiliated with, endorsed by, or sponsored by GitButler Inc.**
"GitButler" is their name, used here only to describe what this tool interoperates with.
GitButler itself is distributed under the Functional Source License (FSL-1.1-MIT), whose
terms govern their code — not this repository. If you vendor or fork any GitButler code
into a project, those terms apply to you and this notice does not cover it.

Not legal advice.
