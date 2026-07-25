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
| `m` | pick up a card, then `←/→` and `⏎` to drop it |
| `r` | refresh |
| `?` | help |
| `q` | quit |

Every move runs `but rub SOURCE TARGET`, and the footer spells out what will happen before
you confirm — dropping a commit on the backlog lane is an *uncommit*, not a move, and that
should never be a surprise.

| drag | result |
|---|---|
| commit → lane | move the commit to that branch |
| commit → unassigned | uncommit it into the worktree |
| file → lane | stage it to that branch |
| file → unassigned | unstage it |

## How it talks to GitButler

`kanstack` links no GitButler code. It spawns the `but` binary you installed and reads its
documented JSON output — `but status -f -j` to read, `but rub … -j --status-after` to
write. That last flag returns the refreshed workspace from the same invocation, so a card
move costs one round trip instead of a mutation plus a reload.

Navigation never shells out, so arrow keys are instant. Only mutations and `r` do, at
roughly 90ms, which measurement suggests is dominated by fixed startup rather than
repository size (`tests/live.rs` guards this).

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
- Commit creation, push, and squash. Today those stay in the CLI.

## Licence and relationship to GitButler

`kanstack` is MIT licensed. It is an independent program that invokes the `but` CLI as a
subprocess; it contains and links no GitButler source, and is not a derivative work of it.

**This project is not affiliated with, endorsed by, or sponsored by GitButler Inc.**
"GitButler" is their name, used here only to describe what this tool interoperates with.
GitButler itself is distributed under the Functional Source License (FSL-1.1-MIT), whose
terms govern their code — not this repository. If you vendor or fork any GitButler code
into a project, those terms apply to you and this notice does not cover it.

Not legal advice.
