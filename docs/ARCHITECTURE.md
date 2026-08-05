# Architecture and implementation notes

This is contributor-facing detail that doesn't belong in the README: version-compatibility
internals, wire-format quirks, and the history behind a few design decisions. If you're just
using `kanstack`, you don't need any of this — see [README.md](../README.md) instead.

## How it talks to GitButler

`kanstack` links no GitButler code. It spawns the `but` binary you installed and reads its
documented JSON output — `but status -f --format json` to read, `but rub … --format json`
to write.

Mutating commands like `rub`, `commit`, and `move` embed a status in their reply by
default — 0.21 removed the old opt-in `--status-after` flag in favour of always including
it. That embedded status still is not equivalent to `but status -f`, though: it omits
per-commit file lists, and these commands have no `-f` of their own to ask for them. A move
therefore costs two queries, roughly 180ms.

`but status` itself is around 90ms and measurement suggests it is dominated by fixed
startup rather than repository size (`tests/live.rs` guards this).

The filesystem watcher that keeps the board current respects `.gitignore`, which is what
stops one `cargo build` from drowning it in `target/` events.

## Version compatibility

The JSON is stable by intent — upstream documents stability as a goal — but it carries no
schema version and its types are `pub(crate)`, so there is no semver promise to lean on.
`kanstack` therefore checks `but --version` at startup and refuses to run below 0.21, and
notes in the UI when you are on a release newer than it has been verified against
(`but::VERIFIED_THROUGH`).

0.21 is a hard floor, not a soft one: it dropped `-j`/`--json` and `--status-after` in
favour of `--format json`, a syntax 0.19 does not understand, so there is no flag spelling
that works on both sides of that release. `kanstack` picks a side rather than branching on
version at every call site. If you're hacking on this against an earlier version of `but`,
expect to re-verify every call site — see `but.rs` and `git log` for what changed.

The bindings in `src/model.rs` track the *wire* format, which differs from upstream's Rust
structs in ways worth knowing if you hack on this: `createdAt` is RFC3339 despite a doc
comment claiming otherwise, and `MergeStatus::Conflicted` arrives as an object while its
sibling variants are bare strings. `but status`'s `stacks` array also arrives
newest-created-first on the wire (verified against 0.21.2) — `but::parse_status` reverses
it back to oldest-first so lane position stays stable as you add lanes, which is the one
place in this codebase where the wire order and the order everything else assumes
deliberately differ. Unknown *new* fields are ignored; a field we depend on going missing
is a hard error rather than a half-rendered board.

Hook flags (`--run-hooks`/`--no-hooks`) are deliberately never passed to `but push`: the
spelling changed between releases, so naming either one breaks on one side of that
version boundary.

### `but merge` → `but land`

`M` is built on `but land`, not the older `but merge`, for a concrete reason: `merge` only
ever worked when the workspace target was GitButler's local-only `gb-local` convention, and
refused outright — "Target remote is origin, not gb-local. This command only works with
gb-local targets" (verified against 0.21.2, against this project's own repository) — for a
target tracking a real remote. `land` works either way: local targets are updated locally,
real-remote targets are pushed to directly. `but land` has no `--dry-run` of its own, so
kanstack's preview comes from `but branch show <branch> --check` instead.

### `but move` replaces the old restack composition

`s` (restack) is a single `but move <branch> <target>` call now. This is new in 0.21 —
earlier `but` exposed no subcommand for it. The capability existed in
`but_api::branch::move_branch` and only GitButler's own TUI reached it; `rub` between two
branches reassigns *uncommitted changes* rather than restacking. Before 0.21, this was
composed from four separate calls: a temp branch anchored on the target, `rub` each commit
across oldest-first, delete the source, rename the temp branch back. `but move` replaced
all of that with one native, atomic call.

## Identifiers, and the fuzzy-matching trap

`but rub` re-resolves its arguments with fuzzy matching, and refuses rather than guessing
when more than one thing matches. A two-character commit id like `ea` matches the branch
`feat`, because the branch *name* contains those characters — which is fatal for a
non-interactive caller.

So the ids shown on cards are not the ids sent to `but`. Commits are rubbed by full hash,
lanes by branch name; only file changes use their CLI id, having no alternative.

## Line-count caching

Uncommitted changes all come from the single `but diff` run alongside `but status`. Commits
need `but diff <sha>` *each*, which would be ruinous per refresh — except a commit's content
is fixed by its hash, so those results are cached by SHA and never expire. Only hashes never
seen before cost anything, which after the first draw is usually none of them.

## A commit on the workspace head locks everything

If something commits onto `gitbutler/workspace` with plain Git, the workspace commit stops
being HEAD and `but` refuses *every* subcommand until it is fixed — `undo` and
`oplog restore` included, so the usual escape hatches are gone with the rest. Verified
against 0.21.2, where the refusal reads:

```text
Error: GitButler mode exit required: please run `but teardown` to preserve your work.
```

It arrives as prose on stderr with an empty stdout, not as the structured `CliError` JSON
that `But::run` prefers, so it is recognised by text (`but::is_workspace_block`). A second
spelling is matched too: `but-workspace`'s `ref_info` carries an older wording that
suggests `git reset --soft` directly. Both mean the same shape.

This is why `Mode::Blocked` exists rather than a notification. Every refresh from here on
fails identically, so a board that noted the error and carried on would sit frozen at its
last good snapshot — still navigable, still accepting mutation keys — while describing a
repository that had moved. Stale and confident is worse than stopped and honest.

`but setup` installs a `pre-commit` hook that blocks this outright, so reaching it takes
`--no-verify`, a `core.hooksPath` override (husky and friends), or a project set up before
the hook existed.

Both recoveries the modal offers work, and neither loses anything. `git reset --soft` is the
default because it keeps GitButler mode on, so the board comes straight back; the stray
commits' content returns as uncommitted changes and the commits stay in the reflog.
`but teardown` is GitButler's own recommendation and does more (snapshot, uncommit, check
out a real branch) but exits GitButler mode, so kanstack quits after it rather than
pretending there is still a workspace to draw. `but setup` is deliberately left to the user.

### The workspace commit has two shapes

Identifying the commit to reset back onto cannot use `but`, since `but` is refusing
everything, so it is a first-parent walk over `git log`. The trap: **there is more than one
kind of workspace commit.** The one `but setup` writes carries a `gitbutler-headers-version`
header; the one left by `but teardown` followed by `but setup` is a merge commit with no
header at all. Matching on the header alone silently failed on the second kind — the modal
came up saying the workspace commit could not be identified and withheld the reset, leaving
teardown as the only way out of a state a reset would have fixed outright.

Identification therefore needs the GitButler author *plus* either the subject or the header.
Author alone is too loose, and either confirming signal can be absent.

## Undo/redo has no success signal

`but undo`/`but redo` restore the entire prior workspace state, uncommitted changes
included, not just the last commit — verified against 0.21.2. Both always succeed and print
nothing, whether or not there was anything to undo or redo: there is no way to tell a real
undo from a silent no-op except by comparing the board before and after, which is why the
notification just says "undid"/"redid" without claiming to know which happened.

## Deleting no longer refuses

Verified against 0.21.2, this changed from earlier `but`: deleting a branch (alone, or the
tip or base of a stack) now discards *its own* commits outright, non-interactively, with no
refusal and no folding into a neighbouring branch. `but undo` is the safety net instead.

## The tutorial's practice repo path must be unique per run

`but setup --init` registers the repo's path in GitButler's *global* project list and never
deregisters it — every `--tutorial` run leaves a permanent entry behind (check
`~/Library/Application Support/com.gitbutler.app/projects.json` on macOS; dozens can pile up
over a project's lifetime). The practice directory used to be named `kanstack-tutorial-{pid}`
only. OS process ids get recycled, so a later run can land on a path GitButler still has
stale cached project metadata for from a completely different git history it once pointed
at. Reproduced live: `but rub`'s very first mutation against a freshly recreated practice
repo returned a bare `{"ok":true}` with no embedded `status` at all, on a path that had
previously been a different (now-deleted) practice repo. `build_practice_repo` now suffixes
the directory with a nanosecond timestamp so the path is never reused across runs — nothing
stale to collide with, since nothing ever points at that exact path twice.
