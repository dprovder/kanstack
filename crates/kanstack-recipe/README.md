# kanstack-recipe

The orchestrator for kanstack. It reads a Markdown+YAML recipe describing a DAG of steps, and
drives them to completion by shelling out to `kanstack`'s public CLI/JSON contract (`spawn`,
`send`, `stop`, `status --json`, and the notes in `events`) — nothing else. It never imports kanstack's Rust crate or touches its
internals.

kanstack itself has no workflow engine, no scheduler, and no notion of a "task" or "plan" — see
[`docs/automation.md`](../../docs/automation.md). This crate is where those live, on top of
kanstack rather than inside it. It's a member of the repository's Cargo workspace, but a bare
`cargo build`/`cargo test` at the root still means kanstack alone; pass `-p kanstack-recipe`
(or `--workspace`) to reach this crate.

## Build and run

```sh
cargo build -p kanstack-recipe
cargo test -p kanstack-recipe

# kanstack not on $PATH? point at it explicitly:
cargo run -p kanstack-recipe -- check examples/recipes/implement-review-docs.md \
  --kanstack-bin target/debug/kanstack

cargo run -p kanstack-recipe -- run path/to/recipe.md
cargo run -p kanstack-recipe -- resume        # after a crash or restart
```

## CLI surface

```sh
kanstack-recipe run   path/to/recipe.md [--kanstack-bin <path>]   # execute the recipe
kanstack-recipe check path/to/recipe.md [--kanstack-bin <path>]   # parse + validate only, spawns nothing
kanstack-recipe resume [<run-id>] [--force] [--kanstack-bin <path>]  # pick runs back up (default: every unfinished one)
kanstack-recipe runs [--json]                                     # list runs and their steps' statuses
kanstack-recipe items <add|list|show|promote|propose> ... [--json] # the factory's queue (see "The factory queue")
kanstack-recipe serve [--max <N>] [--once] [--json] [--kanstack-bin <path>]  # pull loop over the queue
kanstack-recipe land <item-id> [--json] [--keep-panes] [--kanstack-bin <path>] [--but-bin <path>]  # land a reviewed item
```

`--kanstack-bin` overrides where the `kanstack` binary is found (default: looked up on `$PATH`).
Every `run` is a durable run with its own id, printed when it starts (see "Durable runs" below).
There is no TUI and no interactive mode; progress prints to stdout as steps launch and finish,
roughly:

```text
✓ recipe valid
→ run 1761609600-implement-review-docs
→ implement        running   codex / gpt-5.6 / high
✓ implement        complete
→ review           running   claude / default / high (handoff on implement)
✓ review           verified  cargo test
→ docs             running   codex / default / low (handoff on implement)
… docs             idle, waiting on commits
✓ docs             complete
✓ recipe complete
```

## The recipe format (v1)

A recipe is a Markdown file with YAML front matter:

```markdown
---
version: 1

steps:
  implement:
    agent: codex
    model: gpt-5.6
    effort: high
    prompt: Implement the requested change.
    owns:
      - src/**

  review:
    agent: claude
    effort: high
    needs: [implement]
    on: implement
    spawn: handoff
    done_when: either
    prompt: Review the implementation and fix any problems.
    verify:
      - cargo test

  docs:
    agent: codex
    effort: low
    needs: [review]
    on: implement
    prompt: Update the documentation.
    owns:
      - docs/**
---

# Context

Complete the requested issue using the existing project architecture.
```

The Markdown body is shared context. It is appended to every step's own prompt with one fixed,
deterministic template — no conditional templating of any kind:

```text
<step prompt>

## Recipe context

<markdown body>
```

That combined text is what reaches the harness, via `kanstack spawn --prompt` or
`kanstack send`. When the step's `done_when` accepts a note, one more line is appended, asking
the agent to report back (see "Completion model"):

```text
When you're finished, report what you did by running: kanstack note <branch> "<report>"
```

### Fields

| field | required | meaning |
| --- | --- | --- |
| `agent` | yes | harness name, forwarded verbatim to `kanstack spawn --agent`. Not validated here — kanstack already handles unknown agents. |
| `model` | no | forwarded verbatim as `--model <value>` when present. No default is invented; omitting it preserves the harness/user default. |
| `effort` | no | one of `low`/`medium`/`high`, validated at parse time, forwarded as `--effort <value>`. |
| `prompt` | yes | this step's own instruction; combined with the recipe's shared context as above. |
| `needs` | no | list of step ids — **causal** dependency. A step is runnable once every step it `needs` has *completed* (see below). Determines *when* a step runs. |
| `on` | no | a step id — **placement**. Run this step on the branch an earlier step worked on, instead of a branch of its own; `spawn` says how (see "Spawn modes"). Determines *where* a step runs. Must reference an already-declared step; chains are followed to the step that actually owns the workstream. Must also be reachable through the step's own `needs` — directly, or transitively through a chain of `needs` (e.g. `docs: { needs: [review] }` may be `on: implement` if `review` itself `needs: [implement]`) — since placing work on a workstream you never causally wait for was never meaningful: `on` selects placement, `needs` still decides *when*, and the two are validated together, not independently. |
| `spawn` | no | how the step's agent reaches its branch: `continue`, `handoff`, `stack` or `advisory` (see "Spawn modes"). Left out, an `on` step continues in the branch's pane when its agent is the branch's current one and hands off when it isn't, and a step with no `on` spawns its own branch. |
| `done_when` | no | what, besides an idle lane, finishes the step: `commits`, `note` or `either` (see "Completion model"). Defaults to `note` for an advisory step and `commits` otherwise. |
| `owns` | no | glob list, advisory only — for prompt context and collision-awareness. Not enforced by this runner or by kanstack; no new locking mechanism is built around it. |
| `verify` | no | list of shell commands, run from the directory the runner itself was invoked in once the step's workstream goes idle. **All** must exit `0` for the step to count as complete. A nonzero exit fails the step (and the whole recipe) unless it has retries left, in which case the failing command and the tail of its output go back into the same pane and the step continues. |
| `aggregate` | no | **recipe-level**, not per step: names the step that closes the run by reviewing every other step's outcome and proposing follow-ups (see "The aggregate step"). |
| `land`, `factory` | no | **recipe-level**: override `KANSTACK_LAND` (`direct` or `pr`) and `KANSTACK_FACTORY` (`light`; `dark` isn't built yet) for this recipe's items. |
| `retries` | no | how many times a failed step is retried before it fails, overriding `KANSTACK_RETRIES`. Also allowed at the top of the front matter, as the default for every step. Resolved when a run starts and kept in run state. |

No other fields exist in v1 (no `timeout`, `priority`, roles, conditions, or anything
else) — see the design notes in the main repository for why.

### Spawn modes

kanstack keeps one pane, running one agent, per branch. `spawn` says how a step's agent gets
to the code:

| mode | needs `on` | what the runner calls | use when |
| --- | --- | --- | --- |
| `continue` | yes | `kanstack send <branch>` into the branch's existing pane | the same agent goes on, keeping its context |
| `handoff` | yes | `kanstack stop <branch>`, then `kanstack spawn <branch>` with this step's agent, model and effort | a different agent takes over the branch, like a fresh-eyes review |
| `stack` | yes | `kanstack spawn <target>-<step> --above <target>`, where `<target>` is the `on` step's branch | the step's changes should stay separate; an empty branch means it changed nothing |
| `advisory` | no (rejected) | `kanstack spawn <step> --advisory` on its own branch | survey, research, triage: work not expected to commit |

With no `spawn`, a step with no `on` spawns its own branch named after itself, and an `on` step
picks between the first two: `handoff` when its agent differs from the agent last run on the
branch, `continue` when it's the same. So a Claude review after a Codex implement gets a Claude
pane rather than having its prompt typed into Codex's. The branch's current agent is recorded in
the run's state (`branch_agents`), so a resumed run decides the same way.

`continue` with an agent other than the branch's is rejected by `check`, with a message
suggesting `handoff`. A sibling on the same branch that `needs` doesn't order against the step
could still change the agent in between, so the runner checks again at launch and fails the
step rather than typing its prompt into another agent's pane. `share` (several agents on one
branch at once) isn't supported.

A step `on` a `stack` step works on that step's branch (`<target>-<step>`), and one `on` an
advisory step on the advisory step's branch.

### Completion model

A step's completion is judged purely from kanstack's own observable state and git — never from
reading chat output or any LLM "judge":

1. launch the step as its spawn mode says (spawn, send, or stop then spawn);
2. poll `kanstack status --json` until that workstream's `status` is `idle`, and not on the
   first read since launch (see below);
3. check the step's `done_when`:
   - `commits`: the branch's tip has moved since the step's `base`, its tip when it started;
   - `note`: the agent has reported with `kanstack note <branch> "<report>"` since the step
     launched;
   - `either`: one of the two.

   If it isn't met, the step isn't failed: it keeps polling, and its state records
   `waiting_on: commits|note|either` so `runs` shows why it isn't done. `idle` alone isn't
   trusted, since it also fires between an agent's turns and after its own housekeeping;
4. if the step has `verify` commands, run them all; every one must exit `0`;
5. mark the step `done`. Its dependents become runnable once *all* of *their* `needs` are
   `done`.

A step that may legitimately change nothing, like a review, should say `done_when: either`.
When `done_when` accepts a note, the step's prompt asks the agent for one, and the note's text
is stored in the step's state as its `report`. Notes are read back through
`kanstack events --since <offset>` (kind `"note"`, matching the step's branch); `<offset>` is
the events log's length when the step launched, so a note from before then doesn't count. If
several arrive, the last one is the report.

### Design choices this runner makes (and why)

**Polling `status --json`, not `events`.** kanstack offers both a poll (`status --json`) and a
push-ish cue to re-poll (`events --json`, an append-only JSONL log). This runner polls
`status --json` directly on a fixed interval, and only reads `events` for the notes that
`done_when` looks for, once a step's lane is trusted idle. It's a short-lived process, not a daemon, so there
is no gain from a separate always-on log tail, and polling keeps the implementation to one
straightforward loop instead of two coordinated ones (a blocking `events --follow` reader feeding
a separate scheduling loop). `events --json` remains a reasonable choice for a longer-lived
orchestrator; this one just doesn't need it.

**At most one in-flight `spawn`/`send` per workstream branch.** Two steps that resolve to the
same branch — an `on` chain, or two independent siblings that both happen to be `on` the same
target and become runnable in the same tick — are never launched onto it in the same tick. A
step whose target branch is already claimed by another currently-running step is deferred to a
later tick instead. This matters because kanstack's `status --json` reports one `idle`/`busy`
reading per workstream, not "did *this specific message's* turn finish" — sending two messages
to one pane back-to-back and then observing the pane go idle once cannot tell you which message
(or whether both) finished. Serializing per branch is what makes "the workstream went idle"
mean "the step I sent is actually done."

**A single transient `status --json` failure is retried, not treated as a step failure.**
Reading status is itself a `kanstack` subprocess call and can hiccup independently of any step's
own work (a `but` timeout, a momentary I/O error). One failed poll is logged and retried on the
next tick; only several in a row (`MAX_CONSECUTIVE_STATUS_FAILURES` in `src/run.rs`, `3` today)
fail every step that's currently running — enough to tell a real, persistent problem (`kanstack`
actually unreachable) apart from a blip, without adding a general retry/timeout policy to the
recipe format itself (still out of scope, per the non-goals above).

**Independent siblings on failure: let them finish, don't stop them.** When a step fails (spawn
error or a `verify` command exiting nonzero), any *other*, independent step that's already
running is left alone rather than torn down with `kanstack stop`. Stopping them would discard
in-progress agent work for a failure that has nothing to do with them; letting them finish is the
simpler, less destructive default. This isn't configurable in v1 — the recipe's own dependents of
the failed step never launch regardless (their `needs` can never all become `complete`), which is
the actual causal-safety mechanism; the sibling policy is only about already-running, unrelated
work.

### Durable runs

Each `run` keeps its state on disk, so a killed or restarted orchestrator picks up where it
left off with `resume`: finished steps are never re-run, running steps are reattached to their
lanes, and attempt counts survive.

```text
<kanstack state dir>/recipe-<key>/
  orchestrator.lock           flock: one orchestrator per repository
  runs/<run-id>/state.json    one run's state, one writer
```

The state dir and key are kanstack's own for the repository (`KANSTACK_STATE_PATH`, else
`$XDG_STATE_HOME/kanstack`, else `~/.local/state/kanstack`; the key hashes the repository's
root), mirrored in `src/state.rs`. `state.json` records the recipe's path and hash, the
directory `run` was invoked in, each branch's current agent (`branch_agents`), and for each step
its status (`pending`, `launching`, `running`, `verifying`, `done` or `failed`), `attempt`,
`branch`, `pane`, `base` and `tip` (its branch's commit when it started and when it finished,
from git), `started_at`, `last_error`, `notes_from` (the events log's length when it launched),
`report` (its agent's note) and `waiting_on` (set while its lane is idle but its `done_when`
isn't met).

**Snapshots, not a journal.** Transitions are minutes apart, so the whole `state.json` is
rewritten on each one: temp file, fsync, rename, fsync the directory. A reader such as `runs`
only ever sees a whole snapshot, so it takes no lock.

**Record before acting.** `launching`, with `attempt` incremented, is saved before
`kanstack spawn`/`send` runs, and `running` with the pane id after. A crash mid-launch still
uses up its attempt, and a launch that couldn't be recorded never happens: if a snapshot
can't be saved, the run stops where it is, still unfinished, for a later `resume`.

**Resume.** `done` steps are never touched. Every `launching`, `running` or `verifying` step
is first matched against its lane in one `status --json` read:

| The lane is | Resume does |
| --- | --- |
| busy (or waiting, or unknown) | Reattach and keep polling. |
| idle | Judge the step by its `done_when`, then run its `verify` commands, which must be safe to re-run. A step that was already `verifying` had its `done_when` met, and only re-runs `verify`. |
| gone (absent, dead, or no pane) | Count a failed attempt. There are no retries yet, so the step fails. |

Resume never sends a prompt to a lane that still exists — not even for a step that was only
`launching`, whose send may never have landed — because a duplicate send garbles the agent's
input. Instead, `done_when` catches a send that never landed: with no new commits and no note,
the step waits rather than passing. The first-read rule above still holds: unless the step was
already `verifying`, an `idle` on resume's own read is only trusted on the next one.

One gap remains: a step whose `base` couldn't be recorded counts any branch tip as new
commits. That only happens when the orchestrator was killed between `kanstack spawn` creating
a new branch and reading its tip. Several unfinished runs are
resumed one after another; a lane keeps working in its pane while its run waits its turn.

**One orchestrator per repository.** `run` and `resume` take an exclusive, non-blocking
`flock` on `orchestrator.lock` and refuse to start if another orchestrator holds it. The
kernel drops the lock when the process exits, however it exits.

**Recipe edits.** `resume` refuses a run whose recipe file no longer matches the hash recorded
when it started, unless given `--force`, which resumes it against the edited recipe and records
the new hash.

### Validation errors

`check` (and `run`, before doing anything) rejects a recipe with a specific message, e.g.:

```
recipe error: step "review" depends on unknown step "implementt"
```

Rejected: missing/unsupported `version`, missing/empty `steps`, an empty step id, a step missing
`agent` or `prompt`, an invalid `effort` value, a `needs` or `on` entry naming an unknown step, a
step that `needs` or is `on` itself, a cycle in `needs`, a placement cycle through `on`, an `on`
target not reachable (directly or transitively) through the step's own `needs`, an unknown
`spawn` mode (including `share`) or `done_when`, `spawn: continue`, `handoff` or `stack` without
an `on`, `spawn: advisory` with one, `spawn: continue` with an agent other than the one on the
branch by then, and malformed YAML front matter.

## The factory queue

`items` and `serve` turn the runner into the minimal software factory: work goes into a queue,
`serve` pulls it into recipe runs, and each item's state follows its run.

**Items** live behind kanstack-tracker's `Tracker` trait. `KANSTACK_TRACKER` picks one,
and nothing is auto-detected: unset means `local`, one markdown file per item in kanstack's
per-repo state directory, never in the worktree. `KANSTACK_TRACKER=github` uses the origin
repository's issues through the `gh` CLI (`KANSTACK_GH_BIN` substitutes the binary): states are
`kanstack:<state>` labels, a closed issue reads as `done`, and an open issue with no `kanstack:`
label reads as `proposed`, so `serve` never pulls an issue until someone promotes it, which adds
`kanstack:backlog`. An item has a title, a body, an optional recipe, a state — `proposed`,
`backlog`, `running`, `failed`, `review` or `done` — and, for a proposal, the item it came from.

```sh
kanstack-recipe items add --title "Land from the board" --recipe land-feature   # prints the new id
kanstack-recipe items list [--state backlog]
kanstack-recipe items show 44
kanstack-recipe items propose --from 44 --title "Dedupe the timeout constant"   # a proposed follow-up
kanstack-recipe items promote 45                                                # proposed → backlog
```

`--body <text>` or `--body-file <path>` sets the body, and every subcommand takes `--json` for
one `{"schema": 1, ...}` document. Only a proposed item can be promoted.

**`serve`** takes the orchestrator lock, resumes every unfinished run, then loops. Each tick it:

- starts a run for the oldest backlog item that names a recipe while fewer than `--max` runs
  (default 1) are unfinished, and sets the item `running`. The run is saved first, so an item is
  never running without one;
- advances every unfinished run by one tick, with one `status --json` read shared by all;
- posts each finished step and each retry to its item as a comment (the step's report, its
  branch range, or why it failed);
- sets an item to `review` when its run completes and `failed` when it fails.

Without `--once`, `serve` runs until it's killed; `--once` runs a single tick and exits, for tests
and cron. `--json` prints one line per item event on stdout and keeps progress on stderr:
`{"schema":1,"kind":"started|retried|review|failed","item":…,"run":…,"step":…,"reason":…}`.
A backlog item with no `recipe` is never pulled, and items wait in `review` until `land`. On restart `serve` resumes its unfinished runs, posting each step's comment once,
except a run whose recipe file has changed since it started, which waits for `resume --force`.
Run state links each run to its `item` and records its `branch_prefix`, which steps were
`reported` and whether the item has been `settled`; each step also records `retries`,
`retry_budget` and any pending `retry`, which `runs` shows as `retried 1/2`.

An item's `recipe` is a path when it contains `/` or ends in `.md`; otherwise it's looked up as
`<name>.md` in `<repo>/.kanstack/recipes/`, or in the colon-separated directories of
`KANSTACK_RECIPE_PATH`. An item whose recipe can't be found or parsed goes to `failed`, with the
reason as a comment. Each item's branches are prefixed `item-<id>-`, so two items running one
recipe never share a lane.

**Retries.** `serve` retries a failed step up to `KANSTACK_RETRIES` times (default 2; a plain
`run` defaults to 0), overridable with `retries:`. A failed verify goes back into the same pane
with the failing command and the tail of its output, so the agent fixes forward; a dead, pane-less
or gone lane is stopped and respawned with the same agent on the same branch, with its prompt plus
a note about what happened. The pending retry is persisted, so a crash in between retries the same
way. Transport errors from `spawn`/`send` (`multiplexer_unavailable`, `but_failed`,
`delivery_failed` and the like) back off and retry without using the budget, up to 5 in a row.
Out of budget, the step fails and so does its item.

**Landing.** `land <item-id>` lands an item that's in `review` and has a completed run; anything
else exits 1 before touching `but`. Light mode (`KANSTACK_FACTORY` unset or `light`) is the
default and the only mode built: nothing lands until someone runs `land`, and `serve` never does.
The item's branches land bottom of the stack first (a `spawn: stack` step's branch sits above its
target), and a branch with no commits — a review that only reported, say — is skipped.

- `KANSTACK_LAND=direct` (the default): for each branch, `but branch show <b> --check --json` then
  `but land <b> --yes --json`. The first branch that wouldn't land cleanly stops it, with nothing
  above it landed, and a comment on the item says where and why; the item stays in `review`.
  Once everything has landed it comments what landed, sets the item `done`, and stops the item's
  lanes with `kanstack stop` unless `--keep-panes`.
- `KANSTACK_LAND=pr`: `but pr new <top> -t --json` for each stack's top branch, a comment with the
  link, and the item stays in `review` — following the PR to its merge isn't built.

Exit codes are 0, 1 (including a land that stopped partway) and 2 for usage. `--json` prints
`{"schema":1,"item","run","mode","branches","landed":[{"branch","commits"}],"skipped",
"stopped_at"?,"prs":[{"branch","url"}],"state","lanes_stopped","lanes_not_stopped"?}`, even when
a land stops partway.

### The aggregate step

A recipe can close its runs with an aggregate step: `aggregate: <step-id>` at the top of the
front matter names one of its steps. The factory has no aggregator of its own; the recipe picks
the agent and prompt.

```yaml
aggregate: follow-ups
steps:
  implement:  { agent: codex,  prompt: Implement the change. }
  follow-ups: { agent: claude, prompt: Decide what's worth doing next. }
```

The named step needs every other step and defaults to `done_when: note`, and to `spawn: advisory`
when it has no `on:`. It runs last, once no other step is running or can still start — even after
a failure, so it can report what went wrong; the run and its item still fail. It's an error for
`aggregate:` to name a step that doesn't exist, or for another step to name it in `needs` or `on`.

Its prompt gains a "How the run went" section: each other step's status, retries, commit range,
stored report, and why it failed or never ran. Under `serve`, it's then asked to check
`kanstack-recipe items list --json`, propose each follow-up worth doing with `kanstack-recipe
items propose --from <item>` (it does its own deduplication against what's listed), and finish
with `kanstack note`. In a plain `run` with no item, it's asked for a summary note instead.
Proposals arrive as `proposed` and wait for `items promote`.

## What this crate does not do

No DAG scheduling, recipe parsing, or workflow state lives inside kanstack itself — that stays
entirely in this crate, which (as of v1):

- persists only its own run state (see "Durable runs"), never kanstack's or the tracker's, and
  never as the source of truth for a lane — a resumed step always asks kanstack about its lane
  first;
- retries a failed step only within its budget, and has no timeout or conditional-branching logic;
- does not build a new file-locking system around `owns` — that stays advisory metadata;
- does not follow a PR to its merge (`KANSTACK_LAND=pr` leaves the item in `review`), and has no
  dark mode yet;
- does not talk to any provider SDK or MCP server. `serve` is a foreground loop, not a
  daemon or network service.

Anything beyond this is out of scope for v1 by design — see the main repository's design notes
for the full non-goals list.
