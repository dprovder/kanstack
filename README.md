# kanstack

A kanban-style terminal UI for [GitButler](https://gitbutler.com)'s `but` CLI.

GitButler's whole premise is *parallel* stacks of work. Its built-in TUI renders them as a
single vertical commit graph, which flattens the one axis that makes the model interesting.
`kanstack` draws them as a board instead: one lane per stack, one card per commit, a backlog
lane for unstaged work.

```
  workspace  ·  base 3189356

 ● unassigned  2                      ● feat-auth  2                       ● feat-ui  2
 ──────────────────────────────       ──────────────────────────────       ──────────────────────────────
 mv                                   local only                           local only
 wip1.txt                             d5                                   ca
 added                                Wire session refresh into gateway    Fix settings tab focus ring
                                      a2.txt                               b2.txt
 nr                                   d593db0  Dani                        ca3b30b  Dani
 wip2.txt
 added                                e1                                   47
                                      Add auth middleware                  Redesign settings page
                                      a.txt                                b.txt
                                      e17b60c  Dani                        47a6412  Dani

  ←/→ lane · ↑/↓ card · m move · r refresh · ? help · q quit
```

## Install

Requires the [GitButler CLI](https://docs.gitbutler.com/cli-overview) on your `PATH`
(`but` 0.19 or newer) and a repository you have run `but setup` in.

```sh
cargo install --path .
kanstack
```

## Keys

| key | does |
|---|---|
| `←/→`, `h/l` | move between lanes |
| `↑/↓`, `j/k` | move between cards |
| `g` / `G` | first / last card |
| `m` | pick up a card, then `←/→` for a lane, `↑/↓` to drop on a card, `⏎` to confirm |
| `u` | send this card back to the backlog — uncommit a commit, unstage a file |
| `c` | commit the files staged to this lane (only what is staged) |
| `b` | new branch — stacks on the selected lane, `tab` for a parallel lane |
| `s` | stack this whole lane onto another — rewrites history |
| `p` | push this lane — shows what it will do first |
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
documented JSON output — `but status -f -j` to read, `but rub … -j --status-after` to
write.

`--status-after` reports whether a mutation landed, but the status it embeds is not
equivalent to `but status -f`: it omits per-commit file lists, and `rub` rejects `-f`, so
they cannot be had in one invocation. A move therefore costs two queries, roughly 180ms.

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
`kanstack` therefore checks `but --version` at startup and refuses to run below 0.19, and
notes in the UI when you are on a release newer than it has been verified against.

The bindings in `src/model.rs` track the *wire* format, which differs from upstream's Rust
structs in ways worth knowing if you hack on this: the field upstream calls
`uncommitted_changes` is emitted as `unassignedChanges`, `createdAt` is RFC3339 despite a
doc comment claiming otherwise, and `MergeStatus::Conflicted` arrives as an object while
its sibling variants are bare strings. Unknown *new* fields are ignored; a field we depend
on going missing is a hard error rather than a half-rendered board.

## Reporting a bug

Snapshot mode renders a captured payload offline, so a report needs no access to your repo:

```sh
but status -f -j > board.json
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

Early. The board, navigation, and `rub`-backed moves work and are tested against the real
CLI. Not yet built:

- **A diff pane.** `but diff -j` already returns hunk-level patches with ids, so the data
  is there; the work is rendering and syntax highlighting.
- **Live refresh.** Right now you press `r`. The CLI has no watch mode, so this needs a
  filesystem watcher on `.git` and the worktree, debounced.
- **CI and review badges are unverified.** The wire types are bound and rendered, but every
  workspace tested so far had no forge attached, so `ci` and `reviewId` were always null.
  If you use this with real PRs, that is where bugs will be.
- `land`, which skips the pull request entirely. Deliberately not bound yet: it rewrites
  trunk, and deserves more than a keystroke.
- Reordering commits within a lane.

## Restacking an existing branch

`s` stacks one existing lane onto another. There is no `but` command for this: the
capability exists in `but_api::branch::move_branch` and their own TUI uses it, but no
subcommand exposes it, and `rub` between two branches reassigns *uncommitted changes*
rather than restacking. So `s` composes the effect from four calls that are exposed:

```sh
but branch new <tmp> --anchor <target>   # a stacked branch on the target
but rub <commit> <tmp>                   # move each commit, oldest first
but branch delete <source>               # drop the emptied branch
but reword <tmp> -m <source>             # rename back, so the name survives
```

Commits are re-resolved from a fresh `but status` before each move, because CLI ids
describe the current state and every move rebases what is left.

**This rewrites history.** Moved commits get new SHAs, so a lane that was already pushed
will need a force push afterwards — the push preview will say so. And it is not atomic: if
a step fails, the error names which one and what state that leaves, rather than implying a
rollback that did not happen.

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

## Which commands honour `--status-after`

Not all of them, despite the flag being accepted everywhere. Verified against 0.19.3:

| command | `-j --status-after` |
|---|---|
| `commit` | returns `{result, status}` |
| `rub` | returns `{result, status}` |
| `push` | flag accepted, silently ignored |
| `branch new` | flag accepted, silently ignored |

The two that ignore it get a separate `but status` call afterwards. And even where the
envelope does arrive, its status omits per-commit file lists, so a detailed query follows
regardless.

## Licence and relationship to GitButler

`kanstack` is MIT licensed. It is an independent program that invokes the `but` CLI as a
subprocess; it contains and links no GitButler source, and is not a derivative work of it.

**This project is not affiliated with, endorsed by, or sponsored by GitButler Inc.**
"GitButler" is their name, used here only to describe what this tool interoperates with.
GitButler itself is distributed under the Functional Source License (FSL-1.1-MIT), whose
terms govern their code — not this repository. If you vendor or fork any GitButler code
into a project, those terms apply to you and this notice does not cover it.

Not legal advice.
