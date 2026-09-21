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

`b` spins up a new lane and, when a harness split is available (`cmux`, `tmux` as a
fallback, or [Orca](#using-orca)), hands it straight to a coding agent — told which
GitButler branch it's in, not just dropped into a plain checkout:

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

Or, with Nix:

```sh
nix run github:dprovder/kanstack
```

> On Apple Silicon the first run builds `but` from source and takes a few
> minutes; it's cached afterwards.

Want it in your own flake instead? Add `kanstack.url = "github:dprovder/kanstack"` as
an input, then `kanstack.packages.${system}.default` — the pinned `but`, `tmux`, and `git`
(+`cmux` on Apple Silicon) come bundled, so nothing needs installing separately. The dev
shell, packaging options, and per-device `but` provisioning are in
[docs/NIX.md](docs/NIX.md).

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
it checks whether `but`, `cmux`, `tmux`, and `orca` are found (and, for `cmux`, whether the
required `but` version is installed), and lets you pick a default harness — cycling with
`←`/`→` through whichever known harnesses (`claude`, `codex`, `pi`, `opencode`, `kiro`,
`gemini`) were actually found on `PATH`, or just typing a custom command — and a split
backend (`auto`/`cmux`/`tmux`/`orca`). Saving remembers your choices in
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
| `b` | new branch — stacks on the selected lane, `tab` for a parallel lane with a harness split (`cmux`, `tmux` as a fallback, or `orca`); asks for an optional initial message before creating anything, so `esc` cancels the whole branch |
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

## Using Orca

kanstack can hand lanes to [Orca](https://github.com/stablyai/orca)'s terminals as well as
cmux's or tmux's. It is picked automatically when kanstack runs in an Orca terminal (which
Orca marks with `ORCA_TERMINAL_HANDLE`) and isn't in a cmux or tmux pane, or with
`KANSTACK_SPLIT_BACKEND=orca`. Orca's CLI has to be registered under Settings in the app so
`orca` (`orca-ide` on Linux) is on `PATH`, or pointed at with `KANSTACK_ORCA_BIN`.

Orca's own model is one git worktree per agent, but every kanstack lane shares GitButler's
single workspace checkout. So kanstack **never runs `orca worktree create`**: a lane's
terminal is split off kanstack's own, and lives in whichever Orca worktree kanstack is in.
If that split isn't possible (a stale terminal handle, say) the lane opens as a new tab in
the repository's existing worktree instead, which needs the repository to have been added to
Orca.

A few things differ from the other two:

- **Direction.** Orca can only place a new pane to the right of, or below, the one it splits.
  `KANSTACK_ORCA_DIRECTION` and `KANSTACK_ORCA_CHAIN_DIRECTION` default to `below` and
  `right`; `left` and `above` are accepted but behave as `right` and `below`, and so does
  `KANSTACK_SPAWN_DIRECTION`.
- **Busy and idle** come from Orca's own agent detection rather than from CPU usage. A
  harness Orca doesn't recognize will read as busy, and so will one waiting on an approval
  prompt.
- **`t` doesn't wait for the harness.** In testing, Orca accepted a message sent to a
  terminal with no agent running, so pressing `t` before the harness has finished starting
  can type the task into whatever is there — wait for it to be ready, as with the others.

Orca support was written against Orca's CLI reference and source, and exercised against
Orca's headless runtime (`orcad`) — splitting, launching, focusing, closing and the
worktree handling all behave as described. It hasn't been run in the desktop app, and idle
detection hasn't been seen to report *idle* (only busy). If a lane misbehaves,
`KANSTACK_SPLIT_BACKEND` pins one of the others.

## Driving panes from a script or an agent

Everything the board does to a harness pane is also available without the board, so an
agent in one pane can start and talk to others. Run these inside the cmux, tmux or Orca the
panes live in:

```sh
kanstack spawn <branch> [--agent codex] [--prompt "..."]   # creates <branch> if it doesn't exist
kanstack send <branch|session> "..."
kanstack status [--json]
kanstack focus <branch|session>
kanstack stop <branch|session>                             # closes the pane, ends its harness
kanstack report <busy|idle> [<branch>]                     # what an agent says about itself
```

`<session>` is a pane id as `kanstack status` prints it. `spawn` splits off the pane you run it
from, to its right; `KANSTACK_SPAWN_DIRECTION` (`left`, `right`, `above` or `below`, and
saveable in the config file) changes that without touching where the board puts its lanes. `--agent` runs that harness instead
of `$KANSTACK_HARNESS` for this one pane.

`kanstack status --json` prints one JSON document instead of the table, for a script or an
agent to parse:

```json
{"schema":1,"workstreams":[
  {"branch":"fix-login","pane":"%3","agent":"claude","item":"GH-4","status":"busy"},
  {"branch":"planned","pane":null,"agent":null,"item":null,"status":"no-pane"}
]}
```

Every key is always present (`null` when there's no value), one entry per registered
workstream, in registry order. `status` is `busy`, `idle`, `waiting`, `dead`, `unknown` or `no-pane` (the
workstream has no pane). New fields and new `status` values may be added under the same
`schema` number, so ignore keys you don't know and read a `status` you don't recognize as
`unknown`; it changes only if an existing field is renamed, removed or reinterpreted.
Unlike the table, `--json` works outside cmux, tmux and Orca, and when the multiplexer can't
be reached: it still lists every workstream, with `unknown` for those that have a pane. An
empty registry gives `{"schema":1,"workstreams":[]}`.

### Where busy, idle and waiting come from

The multiplexer's own reading (CPU for cmux and tmux, Orca's idle detection) is a guess, and
some multiplexers can't make one. So a harness can say for itself: `kanstack report busy`
when a turn starts, `idle` when it ends, and `waiting` when it stops on a permission prompt,
from any script or hook. `<branch>` defaults to `$KANSTACK_BRANCH`, which kanstack sets in
every pane it launches; it prints nothing, needs no multiplexer, and never reads the
registry, so it is safe to run on every turn. `waiting` is something only an agent can say:
a pane stopped on a prompt looks exactly like one at rest. The board shows it as
`◆ needs you`.

For Claude Code, kanstack does this for you: it launches `claude` with `--settings` carrying
hooks for the start and end of a turn, each tool call, permission prompts, and turns that die
on an API error. They run alongside your own hooks rather than replacing them, and nothing on
disk is edited. Other harnesses get no hooks, because kanstack doesn't yet know a safe way to
hand them any at launch; they keep the multiplexer's reading, and can call `kanstack report`
themselves. `KANSTACK_STATUS_HOOKS=off` launches every harness without them.

How a report and the multiplexer's reading combine:

- A pane the multiplexer says is gone is always `dead`.
- Otherwise a fresh report wins. `busy` and `idle` are believed for ten minutes, `waiting` for
  twelve hours, since a prompt left overnight is exactly what it is for.
- **Interrupting with Escape fires no hook** (checked against real Claude, on a running turn
  and on a permission prompt), so a report can be left behind. After twenty seconds, a `busy`
  report over a pane the multiplexer sees as quiet reads `idle`, and a `waiting` report over
  one it sees as busy reads `busy`. A quiet pane never contradicts `waiting`, since only the
  agent can tell those apart, so a prompt you dismissed with Escape can show `◆ needs you`
  until you next type something.
- A report that has run out reads `unknown` unless the multiplexer knows better, rather than
  leaving the last status standing.

Each command is its own process, so panes are tracked in a per-repository registry under
`$XDG_STATE_HOME/kanstack` (`~/.local/state/kanstack`; `KANSTACK_STATE_PATH` relocates it).
The board records into it too, and picks up panes the commands opened.

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
