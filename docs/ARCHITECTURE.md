# Architecture and implementation notes

This is contributor-facing detail that doesn't belong in the README: version-compatibility
internals, wire-format quirks, and the history behind a few design decisions. If you're just
using `kanstack`, you don't need any of this — see [README.md](../README.md) instead.

## Adding a feature

`src/app/`, `src/ui/`, and `src/but/` are each split one file per feature area (branch
creation, the drawer, push/land, rebase, and so on) instead of one giant `app.rs`/`ui.rs`/
`but.rs`. The point is parallel work: two features being built at the same time should touch
different files, not the same 4000-line one. A new feature should follow the same shape —
its own `app/<name>.rs` (state + key handling), its own `ui/<name>.rs` (rendering), and, if it
needs a new CLI call, its own `but/<name>.rs` — rather than growing an existing file or
falling back into `mod.rs`.

Concretely, adding a mode-driven feature (a new confirm dialog, a new drawer, a new
in-app flow) touches:

- **`src/app/mod.rs`**: one new `Mode` variant, one new field on `App` for whatever state
  the feature needs while active, and one new `match self.mode` arm in `handle_key`
  delegating to a `handle_key_<name>` method — everything else lives in the new file.
- **`src/app/<name>.rs`** (new file): an `impl App` block with that mode's state
  transitions (`begin_*`/`confirm_*` methods) and its `handle_key_<name>` method. Mark
  methods called from `mod.rs` or another feature file `pub(super)` — private methods are
  only visible within their own file plus its own descendants, not to siblings or the
  parent, so anything reached from outside needs at least `pub(super)` (Rust's normal
  privacy rule: a private item is visible in its defining module and that module's
  descendants, nothing else). Anything called only from within the same file can stay
  plain `fn`.
- **`src/ui/mod.rs`**: one new `match app.mode` arm in `draw()` calling the new draw
  function.
- **`src/ui/<name>.rs`** (new file): the `draw_<name>` function and anything private to it.
  Same `pub(super)` rule as above — it's only needed here since `draw()` calls in from the
  parent module.
- **`src/but/<name>.rs`** (new file, if the feature needs a new subcommand): an `impl But`
  block with the wrapper method(s). No visibility changes needed — `pub fn` methods are
  callable as `but.method(...)` from anywhere regardless of which file the `impl But` block
  lives in, since method resolution goes through the type, not the defining module's path.

A private item defined in `mod.rs` (a struct field, a helper method, a shared type) is
already visible to every file in its directory without any extra visibility annotation —
Rust grants descendant modules access to an ancestor's private items automatically. That's
what keeps `use super::*;` at the top of every feature file enough to reach `App`, `Mode`,
`Notice`, and the handful of small shared types (`MoveOp`, `PendingCommitMove`) that stay in
`app/mod.rs` because more than one feature file needs them.

Tests mostly live beside the code they exercise — a `#[cfg(test)] mod tests` per feature
file rather than one shared block. The one exception is `app/mod.rs`'s own test module: most
of `app`'s tests exercise navigation, mouse handling, and cross-mode interactions through
shared helpers (`Nav`, `key()`, `mouse()`, `blocked_app()`), so splitting them per file would
mean duplicating that harness rather than removing duplication. New tests for a genuinely
new, self-contained feature should still go in that feature's own file.

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

## Harness split backends, and Orca's worktree model

### Adding a multiplexer or a harness

Two independent axes, each one file plus one line, with the shared logic written once:

- **A multiplexer** is a `Multiplexer` impl (`src/mux.rs`): open a pane running a command
  line, type a line into one, focus it, close it, and `probe` whether panes are alive and
  busy. It is stateless and knows nothing about branches, lanes or harnesses. Which pane
  belongs to which branch, where the next one splits off, and what a pane's last known status
  was live in `Splitter` (`src/splitter.rs`), once — so a backend is its own CLI calls and
  nothing else. To add one, write `src/<name>.rs` with a `discover()`, a `detection()` (one
  line for the `--setup` wizard) and the impl, then add one entry to `splitter::BACKENDS`.
  That list is the only place backends are named: discovery order, the `KANSTACK_SPLIT_BACKEND`
  override, the wizard's picker and detection panel, and the "no backend found" message all
  read it. What can't be generated is checked instead, so forgetting fails a test: the README
  and this file must mention the backend, `--help` must document it and its
  `KANSTACK_<NAME>_DIRECTION` variables, and the wizard must offer it. Its environment variables come for free:
  `KANSTACK_<NAME>_DIRECTION` and `KANSTACK_<NAME>_CHAIN_DIRECTION`, with defaults from
  `default_directions()`. `probe` returns only the panes it could classify; leaving one out
  means "no news", and the caller keeps what it knew. A backend that can only say whether a
  pane exists returns `true` from `tracks_pids()` and everything else follows (below), as
  `ghostty.rs` does; it need only report a pane that is gone as `Dead`. Test everything above
  the trait against `mux::fake::FakeMux`, and the backend itself against a stand-in binary
  that logs its arguments (see `orca.rs`'s tests, or `ghostty.rs`'s for one that scripts a GUI
  app: its stand-in `osascript` records each call's operation and `argv`).
- **A harness** is a `Harness` impl (`src/harness.rs`): one unit struct, one entry in
  `harness::KNOWN` (which is also what `--setup` scans `PATH` for, in that priority order).
  Its one job today is how the branch-context note reaches it (`note_delivery`); anything
  else that varies per harness belongs on the trait. Anything not listed gets `Generic`.
  Multiplexers never see a harness — `HarnessConfig::launch_line` hands them a finished
  line.

### Where a pane's status comes from

Three sources, combined by `splitter::merge`. A `Multiplexer::probe` is the backend's own
reading of the pane. A *report* is what the agent said about itself, through `kanstack report
busy|idle|waiting`, which a harness's hooks run (`src/report.rs`). For a backend that can't
probe activity there is a third, the process table (below). The first and third are together
the pane's *native* reading, which `Splitter::native_readings` derives in one place, for the
poll and for the confirming looks alike. The rule, in order: a pane that is gone (the
multiplexer says so, or the pid tier finds its shell gone) is `dead`, whatever was reported; a fresh report wins, except
where what the multiplexer sees contradicts it (below); an expired report is `unknown` unless
the multiplexer knows better; with neither, the pane is left out of the result and the last
known status is kept. If the probe itself fails, statuses come from fresh reports alone, and
the error surfaces only when there are none.

- **One file per branch, not a field in the registry.** Hooks fire on every turn from several
  processes at once, and the registry is rewritten whole, so sharing it would lose updates.
  Each report replaces one branch's file atomically. The file name is a hash of the branch,
  so any branch name is a valid one. `spawn` and `stop` delete the file, before the harness
  starts, so a fast agent's first report can't be erased and a new pane can't inherit an old
  one's last word.
- **Reports expire**, `busy` and `idle` after ten minutes (`report::FRESH_FOR`) because a
  crashed agent never says it stopped, `waiting` after twelve hours (`WAITING_FRESH_FOR`)
  because the reason for it is an agent stuck on a prompt overnight. Expiry reads `unknown`,
  not "keep the last applied status": with nothing to contradict it, a stale `busy` would
  otherwise stand forever.
- **Escape fires no hook, and neither does "No".** Checked live against Claude 2026-09:
  interrupting a running turn, pressing Escape on a permission prompt, and answering a prompt
  with "No" each fire nothing, leaving the report at `busy` or `waiting` over a pane at rest.
  `Notification` with `idle_prompt` looked like a recovery signal and was tried: it never fired
  in over 100 seconds, in cmux or in a clean tmux with no launch shim, so it is not installed.
- **Corroboration, and why it is not one reading.** The only thing that clears a stale `busy`
  is the pane staying quiet. `splitter::stale_busy_suspect` picks out a `busy` report older
  than `CORROBORATION_GRACE` (twenty seconds; the multiplexer's reading lags) over a pane
  reading idle, and `confirmed_quiet` looks again `QUIET_CONFIRMATIONS` times, `QUIET_GAP`
  apart. Only if every look is quiet is the report retracted, by writing `idle` over it with
  a compare-and-swap on the second it was written (`Reports::retract_busy`), so a hook that
  fired in between wins. Writing it back is what stops a later CPU blip from resurrecting the
  stale `busy`. Measured on tmux's `ps` CPU: a genuine 63-second turn dipped below the
  threshold three times for under a second each, and a pane quiet after an interrupt blipped
  back to busy twice in 110 seconds; a single-reading rule flickered on both. With
  confirmation, a 44-second turn and a 100-second interrupted pane showed no flicker (one run
  each, so evidence, not proof). It costs the confirmation looks (about 2.4 seconds) only when
  a contradiction exists.
- **`waiting` is never retracted, by design.** The reverse rule — an old `waiting` over a
  busy pane reads `busy` — was tried and removed. It is redundant, because carrying on always
  fires a hook (a new prompt, or the tool finishing), and it misfired: a prompt left for a
  minute read `busy` on CPU blips alone. A prompt dismissed with "No" or Escape therefore
  keeps `waiting` until the next prompt, though the pane is at rest. From outside a prompt
  and rest are identical, so only the agent could say, and it can't. On a multiplexer with no
  reading at all, nothing corroborates either rule, so a stale `busy` stands until it expires.
  That is one reason the pid tier below exists: it gives such a multiplexer a reading, and
  corroboration uses it.
- **The process table, for a multiplexer that can't say (`Multiplexer::tracks_pids`).** Ghostty
  can only say whether a pane exists, and a pane whose command has exited stays listed
  (measured: still listed 9 seconds later), so kanstack follows the process itself
  (`src/procs.rs`). When tracking applies, `spawn_harness_with` puts
  `sh -c 'printf %s "$PPID" > "$0"' <file> && ` in front of the launch line, so the pane's own
  shell records its pid in `pids-<repo>/<fnv1a(branch)>.pid` (`workstream::pids_dir`; one file
  per branch beside the reports, for the same reason: the writer is another process). `$PPID`
  because it is the parent of the `sh` and so the pane's shell whatever that shell is, where
  `$$` is spelled differently in fish; the path is `sh`'s `$0` so it is quoted once, not
  inside another quote. The directory is created from Rust first, since the shell would not,
  and the file is forgotten before a new pane opens and on `stop`, exactly as a report is.
  Tracking applies when the multiplexer asks or `KANSTACK_TRACK_PIDS` is `1`/`true`/`on`/`yes`,
  and not when it is `0`/`false`/`off`/`no` (read when the `Splitter` is made). The setting is
  how the feature is exercised on tmux; it is also why there is a test double for `ps`.
  For a tracked pane with a pid file, the reading is `Dead` if the pid is not in `ps`, else
  `Busy` or `Idle` by `subtree_cpu` against `CPU_BUSY_THRESHOLD_PERCENT`. It is used only when
  the probe gave no `Busy`/`Idle` for the pane: a probe `Dead` wins, and a probe `Busy` or `Idle`
  wins over it. `ps` is read at most once per `native_readings` call (a poll, or one
  confirming look), lazily, and if it fails the panes that needed it have no reading. If the
  probe itself fails, the pid tier is not consulted: statuses come from reports alone, as
  before.
  What it can't do: the CPU is a guess, as it is everywhere; it follows the shell, so where
  the shell outlives the harness (tmux, and any pane that returns to a prompt) a finished
  harness reads `Idle`, and `Dead` only means the shell exited; a recorded pid could in
  principle be reused (unguarded when reading status; `stop` guards it, below), and a zombie
  awaiting its parent's `wait` still has a `ps` row.
  Checked on tmux 3.6 with the pane listing stripped of `pane_pid` (so the probe could only
  say a pane exists): `busy` for a CPU loop, `idle` for `sleep`, `idle` after killing the loop
  (shell left), `dead` after killing the shell in a `remain-on-exit` pane, and a stale `busy`
  report retracted after 2.7 seconds (`QUIET_CONFIRMATIONS` looks at `QUIET_GAP`, each a fresh
  `ps`) while the same report over a CPU-bound shell stood. The pid file matched the pane's
  shell under zsh, bash, dash, ksh and tcsh; fish is untested (not installed).
- **Scenarios verified live (Claude Code 2.1.278):** Write approved, "yes, don't ask again",
  "no", two consecutive prompts, WebFetch, plan mode's `ExitPlanMode` dialog and
  `AskUserQuestion` all fire `PermissionRequest` and report `waiting`; approval returns to
  `busy` via `PostToolUse` and to `idle` via `Stop`. After approving, `waiting` persists while
  the tool runs, since nothing fires at the moment of approval. Test-harness notes worth
  keeping: run the panes in a tmux server started with `env -i` plus only HOME, USER, LOGNAME,
  PATH (with cmux's shim directories removed), TERM and the KANSTACK_* variables — without
  USER, Claude reports "Not logged in" because its keychain lookup is by account name — and the
  first launch in a directory shows a trust screen whose default is "No, exit".
- **`kanstack report` must print nothing.** Claude adds a `UserPromptSubmit` hook's stdout to
  what the model sees, and a silent `PermissionRequest` hook leaves the normal prompt alone
  (exit 0 with no decision) rather than approving or denying. It also returns before the
  registry is read, so a broken registry can't make a per-turn hook noisy.
- **Every hook is synchronous.** `PreToolUse` and `PermissionRequest` fire back to back, and
  an async `busy` could land after the `waiting` and hide the prompt. A few milliseconds per
  event buys strict ordering.
- **Harnesses get hooks through `Harness::status_hooks`**, which returns launch arguments and
  environment, so a harness whose route is a flag, an environment variable or a config
  override can all be expressed the same way. The lane's name travels as `$KANSTACK_BRANCH`,
  so the hook commands are identical for every lane. Only Claude has one, through
  `--settings <json>`, which hooks merge across rather than replace. A route has to work per
  launch and touch no file the user owns, and has to be verified for each harness, not
  assumed from its docs.
- **A hooked launch line is always spilled to files.** The settings JSON pushes it past the
  typed-line limit (`harness_launch::MAX_TYPED_LINE`), so every hooked launch uses the
  read-into-variables path a long prompt takes. It is byte-exact and tested, but it is now the
  common path rather than the rare one.

**Codex has a per-launch route, and it is deliberately not used.** Verified against Codex
0.155.1 (the `codex` installed on the author's machine was a 2025 build with no `-c` override
and no hooks at all): `-c 'hooks.UserPromptSubmit=[{hooks=[{type="command",command='''…'''}]}]'`
and the same for `Stop` merge with the user's hooks and fire in `exec` and in the TUI, with a
JSON payload on stdin. But they only run with `--dangerously-bypass-hook-trust`, which trusts
*every* enabled hook, including any a hostile repository ships in its own config. Handing
that flag to every launched Codex to get a status light is the wrong trade. `notify` set with
`-c` needs no such flag, but fires only at turn end, and a reporter that says `idle` and never
`busy` is actively harmful under this rule: its stale `idle` would outrank the multiplexer's
correct `busy`. Any harness hooked here must report both edges.

### The four backends today

`splitter.rs` wraps whichever of four backends — `cmux.rs`, `tmux.rs`, `orca.rs`, `ghostty.rs` —
discovery found, each opening a terminal split per lane and typing a launch line
(`harness_launch.rs`) into it.
The first two are terminal multiplexers; Orca is an agent-oriented IDE whose CLI drives a
running desktop app, and it differs in ways that shaped `orca.rs`; Ghostty is a terminal
emulator with no CLI at all, scripted through AppleScript, described after Orca's.

**One worktree per agent, against one shared checkout.** Orca's model is a git worktree per
agent (`orca worktree create`). Every kanstack lane shares GitButler's single workspace
checkout, so `orca.rs` never touches `orca worktree …`. A terminal joins an existing worktree
two ways, both without creating one: `terminal split --terminal <handle>` takes no worktree
argument and inherits the split terminal's, and `terminal create --worktree path:<root>`
matches a registered worktree by exact path. A registered repository's main checkout is a
worktree in Orca's own listing, which is what GitButler's workspace is — but only if the
repository was added to Orca, else `selector_not_found`. `active`/`current` are not usable
here: they resolve by finding the registered worktree *enclosing* the cwd, where `path:`
compares for equality, so `orca.rs` lifts `cwd` to the repository root itself.

**What the docs don't say.** Orca's published CLI reference names the commands and flags but
gives no JSON shapes, no environment variables and no split semantics. Those come from its
source (`stablyai/orca` at 9fbdfc5), and were then confirmed against `orcad`, its headless
Node runtime built from that commit (not the desktop app):

- Terminals get `ORCA_TERMINAL_HANDLE` (`src/main/ipc/pty/provider/local-configure.ts`), the
  signal `Orca::discover` requires. A long-lived shell can keep a stale one across a window
  reload, hence the `terminal create` fallback in `spawn_harness_with`.
- `--json` writes `{"id","ok","result"|"error":{"code","message"},"_meta"}` to stdout, and
  the exit status can't be trusted alone: an unaccepted `terminal send` and an unsatisfied
  `terminal wait` exit 1 with an `ok: true` result, and a `terminal wait` that times out is an
  error envelope with code `timeout`. `run_json` therefore reads the envelope, not the status.
- `terminal split --direction vertical` is "Split Right" and `horizontal` is "Split Down" in
  Orca's own menus, and the new pane is always the second child. `left`/`up` can't be honored
  (`split_orientation`), which is why Orca's default directions are `down`/`right`.
- Text values are passed as `--flag=value`: a `--flag value` whose value begins with `--` is
  read as a boolean flag followed by a new one.
- `terminal wait --for tui-idle` is the only status verb the CLI exposes; it blocks, so
  `poll_statuses` runs one short-timeout probe per terminal in parallel and reads a timeout as
  busy. It depends on Orca recognizing the agent, so an unknown harness reads busy forever.

**Confirmed against `orcad`:** every point above except the two idle-related ones below;
that `--command` reaches the shell as typed input, so the `cd … &&` launch line runs in the
worktree directory as under cmux and tmux (also at 5000 characters, through the spill-to-files
path); that `repo add` alone makes `path:<root>` resolve, with no `worktree create`; that a
split joins its source's tab; that `close` is idempotent; and that a stale anchor fails the
split (as `runtime_unavailable`, not a "stale" code) so the `create` fallback takes over. The
replies in `tests/fixtures/orca_*.json` were captured from it. To repeat that: build `orcad`
and the CLI from a checkout of `stablyai/orca` (`pnpm install --ignore-scripts`, `tsc -p
config/tsconfig.cli.json`, `node config/scripts/build-orcad.mjs`, `chmod +x` node-pty's
`spawn-helper`), start it with `ORCA_USER_DATA` on a short path (its daemon socket must fit
in 104 characters), and point `KANSTACK_ORCA_BIN` at a wrapper adding `--pairing-code`.

**Still unverified:** anything that needs the desktop app or a recognized agent. `terminal
wait --for tui-idle` only ever timed out against `orcad` — on a plain process and on an idle
interactive shell — so a satisfied wait, and with it `Idle` in `poll_statuses`, has not been
seen; how a blocked approval prompt is reported is likewise unseen; `orcad` accepted a
`terminal send` to a plain terminal where Orca's types allow a `no-agent` refusal, so
`send_task` cannot rely on that guard; and `orcad` may differ from the renderer-backed app.
Those replies in `orca.rs`'s tests are shaped from Orca's TypeScript types instead.

**Discovery order.** cmux, then tmux, then Orca, then Ghostty, except that inside an Orca
terminal with neither `$CMUX_SURFACE_ID` nor `$TMUX_PANE` set, Orca is tried first, and inside
plain Ghostty (below) Ghostty is. `Cmux::discover` only checks for its binary on `PATH`, so a
machine with cmux installed but not in use would otherwise pick it and fail at spawn time — the
ambiguity `KANSTACK_SPLIT_BACKEND` exists to override, except that here a positive signal
(`ORCA_TERMINAL_HANDLE`, `TERM_PROGRAM=ghostty`) settles it.

### Ghostty (`ghostty.rs`)

macOS only, and the only backend with no CLI: it runs fixed AppleScripts through `osascript`
against the dictionary Ghostty 1.3 added (`Ghostty.app/Contents/Resources/Ghostty.sdef`:
`application > window > tab > terminal`; `split`, `focus`, `close`, `input text`, `send key`,
`perform action`, and a `surface configuration` record for a new terminal's cwd, command and
environment). Which of the claims below were measured and which assumed matters here, since
there is no documentation to fall back on, so each is marked. **Measured** means run against
the official Ghostty 1.3.1 build (Developer ID team `24VZTF6M5V`), with an isolated config
(`XDG_CONFIG_HOME` plus `--config-default-files=false`, since the macOS-specific config
file is read too), launched with a scrubbed environment so it behaved as it does from the Dock.

- **Every dynamic value is an `osascript` argument**, read by the script as `argv`, after a
  `--` (measured: without it a text starting with `-` is parsed as an option; with it, quotes,
  backslashes, newlines and empty strings arrive intact). Nothing is interpolated into
  script source. A call costs 110-160 ms (measured, five terminals open), so `probe` is one.
- **The launch line must be wrapped in `sh -c`** (measured, and not what the surface's
  `command` documentation suggests). On macOS Ghostty starts a command as `login -flp <user>
  /bin/bash --noprofile --norc -c "exec -l <command>"` with the command spliced in
  *unquoted*. `cd '/repo' && claude` becomes `exec -l cd '/repo'` and the pane's process is gone
  before the harness starts, while `sleep 60 && echo hi` runs `sleep 60` alone and drops the
  rest. `/bin/sh -c '<line>'`, quoted with `harness_launch::shell_quote`, reaches one shell
  whole, including the hooked launch line that spills its prompt to files (measured with a
  stand-in `claude` on a real hooked line: cwd, `KANSTACK_BRANCH`, arguments all correct).
- **Working directory and environment apply exactly** (measured). The `sh` is a login shell
  but not the user's interactive one, and a Dock-launched Ghostty's `PATH` is `/usr/bin:/bin:…`
  (measured), so `PATH` and every `KANSTACK_*` variable except `KANSTACK_BRANCH` (which names
  the *caller's* lane, and would make a new lane report as it) are passed in the configuration;
  nothing else is. Whether some other variable that a user's `.zshrc` sets matters to a harness is
  assumed, not measured.
- **`split` returns the new terminal** and the new pane takes focus (measured: the tab's
  focused terminal was the new one). Directions `up`, `down`, `left`, `right` are an
  enumeration constant, not a string, so the script branches on them; all four measured, by
  reading which terminal `goto_split:<direction>` reaches from the old one.
- **The pane's title** is set with `perform action "set_surface_title:<branch>"`, which
  returned true and changed `name of terminal` (measured). A harness that sets its own title
  replaces it (assumed for Claude Code, not measured).
- **A pane stays after its command exits**: exit 0 and exit 3 both stayed in `id of every
  terminal` for 10+ seconds (measured). AppleScript can say a pane was closed, never that its
  command finished; that is why `tracks_pids()` is true for Ghostty and the pid tier supplies
  liveness and CPU. `probe` says `Dead` for an id that is gone and nothing for one that is
  present.
- **`close` does not end the process promptly** (measured, and the most surprising result):
  after `close <terminal>` the id is gone from `id of every terminal`, but a `sleep`, a `cat`
  and a shell ran on, and `lsof` on the app showed it still holding their pty masters. Three
  closed panes' processes were still running 5 to 7 minutes later and were gone at the next
  look, some 18 minutes on (nothing was done to them; why is not established), and one closed
  9 minutes earlier was still running when the app was killed, which ended it. `close window`
  did end its process at once; `perform action "close_surface"` on a pane with a running
  process raises a confirmation dialog no script can answer. `Multiplexer::close` therefore only
  removes the pane, and ending the harness needs its pid.
- **`Splitter::stop` ends the harness through its recorded pid**. Before
  closing, it finds the recorded shell and everything below it in the process table; after
  closing, it sends them `SIGTERM`, *even if the close failed*, since a pane that would not
  close is where a harness is most likely still running. It only acts when process tracking
  applies. **Pid reuse is the danger**: a recorded pid can end up naming an unrelated process,
  and ending that would be ending a stranger. So the process's age (`ps -o etime=`) is compared
  with when the pid file was written: the shell wrote it the moment it started, so a process
  that began after that is not it. Checked against real processes: a real `sleep` is found,
  aged and ended by the real functions, and on a private tmux whose `kill-pane` does nothing
  (as Ghostty's close does) the old build left the harness running and the new one ended it.
  Limits: it is a plain `SIGTERM`, so a harness that ignores it keeps running; an interactive
  shell (a tmux pane's) ignores `SIGTERM` too, so there the harness underneath ends and the
  shell stays until the multiplexer closes the pane; and anything a harness detaches from its
  process tree is out of reach.
- **Panes that never started** (observed, cause not established): during one stretch of about
  six minutes, with the machine idle, *every* pane created failed to run its command at all,
  including a bare `echo hi > file` split by hand with no kanstack involved, and stayed that way
  minutes later. It began when nobody was using the Mac (`pmset -g assertions` said
  `UserIsActive 0`, and the display was probably asleep); a fresh split right after `caffeinate -u`
  worked, and later idle splits (6 of 6) did too. The pane was created and listed either way, so
  nothing but the command's missing effect shows it. Expect a lane spawned while the display is
  asleep to sit empty.
- **Typing**: `input text` is a real bracketed paste and `send key "enter"` is its own `\r`
  (measured, against a TUI that had enabled bracketed paste), so `type_line` is both, in one
  script. `send key` with a plain letter sent nothing to a raw-mode reader (measured), while
  `escape`, `tab` and `enter` did; that is why nothing else here uses it, and why the board
  can't be driven letter by letter through AppleScript in tests.
- **Existence**: `exists terminal id X` throws (-1728) for a missing id, and so does
  `first terminal whose id is X` (-1719, "Invalid index"); membership in `id of every
  terminal` is what the scripts test.
- **Finding kanstack's own terminal.** Ghostty exports no per-terminal variable (a
  `GHOSTTY_SURFACE_ID` seen in some shells was inherited from cmux, a Ghostty fork). A program
  can set its terminal's title with `OSC 2` and read it back as `name of terminal`, so the first
  lane writes a unique marker to `/dev/tty`, lists `id`/`name` pairs until one contains it (the
  title lands asynchronously, so it retries for up to three seconds), and writes the previous
  title back. Measured from a shell (`kanstack spawn`), where the shell's own title (`my-custom-
  title`, set with `printf`) was restored, and from the board's alternate screen with raw mode
  and bracketed paste on (a lane started with `b`, two `list` calls, one `split`, the pane split
  `up`). Not measured: a `/dev/tty` that isn't Ghostty's (it then fails after three seconds
  with an error naming the cause), and two kanstacks finding their terminals in the same
  instant (each marker is unique, so assumed fine).
- **Scripts refuse to launch Ghostty.** `tell application id … ` launches an app that isn't
  running, so each script starts with an `is running` check that raises -600 instead. With the
  scratch Ghostty quit and no installed copy, `osascript` failed earlier than that, at compile
  time: `Can’t get application id "com.mitchellh.ghostty". (-1728)` (measured, and the app was
  not launched). -1728 also means "no such terminal" elsewhere, so `explain` checks the text
  first. The `is running` guard itself, and the -1743 (Automation refused) message, are covered
  only by the stand-in: permission was already granted here, and an installed Ghostty that has
  merely quit was not available.
- **Discovery.** `TERM_PROGRAM=ghostty` and neither `$CMUX_SURFACE_ID` nor `$TMUX_PANE`
  (cmux sets `TERM_PROGRAM=ghostty` too, and tmux in Ghostty should get tmux); anything else,
  and any other OS, is `None`. Measured end to end with `cmux` and `tmux` both on `PATH`
  and no `KANSTACK_SPLIT_BACKEND`: the spawn chose Ghostty.
- **Not measured**: Ghostty older than 1.3 (assumed to fail with a syntax error, which
  `explain` reports as needing 1.3; -2740/-2741/-1708), more than one Ghostty running, a
  window in native fullscreen, and a real Claude in a lane (a stand-in that logged its
  environment and arguments took its place).
