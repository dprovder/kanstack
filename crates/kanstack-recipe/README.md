# kanstack-recipe

The orchestrator for kanstack. It reads a Markdown+YAML recipe describing a DAG of steps, and
drives them to completion by shelling out to `kanstack`'s public CLI/JSON contract (`spawn`,
`send`, `status --json`) — nothing else. It never imports kanstack's Rust crate or touches its
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
→ review           running   claude / default / high (on implement)
✓ review           verified  cargo test
→ docs             running   codex / default / low (on implement)
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
`kanstack send`.

### Fields

| field | required | meaning |
| --- | --- | --- |
| `agent` | yes | harness name, forwarded verbatim to `kanstack spawn --agent`. Not validated here — kanstack already handles unknown agents. |
| `model` | no | forwarded verbatim as `--model <value>` when present. No default is invented; omitting it preserves the harness/user default. |
| `effort` | no | one of `low`/`medium`/`high`, validated at parse time, forwarded as `--effort <value>`. |
| `prompt` | yes | this step's own instruction; combined with the recipe's shared context as above. |
| `needs` | no | list of step ids — **causal** dependency. A step is runnable once every step it `needs` has *completed* (see below). Determines *when* a step runs. |
| `on` | no | a step id — **placement**. Send this step's prompt to the *same* workstream an earlier step created (`kanstack send`) instead of opening a new one (`kanstack spawn`). Determines *where* a step runs. Must reference an already-declared step; chains are followed to the step that actually owns the workstream. Must also be reachable through the step's own `needs` — directly, or transitively through a chain of `needs` (e.g. `docs: { needs: [review] }` may be `on: implement` if `review` itself `needs: [implement]`) — since placing work on a workstream you never causally wait for was never meaningful: `on` selects placement, `needs` still decides *when*, and the two are validated together, not independently. |
| `owns` | no | glob list, advisory only — for prompt context and collision-awareness. Not enforced by this runner or by kanstack; no new locking mechanism is built around it. |
| `verify` | no | list of shell commands, run from the directory the runner itself was invoked in once the step's workstream goes idle. **All** must exit `0` for the step to count as complete; any nonzero exit fails the step (and the whole recipe). Run once each, no retries. |

No other fields exist in v1 (no `retry`, `timeout`, `priority`, roles, conditions, or anything
else) — see the design notes in the main repository for why.

### Completion model

A step's completion is judged purely from kanstack's own observable state — never from reading
chat output or any LLM "judge":

1. spawn (new workstream) or send (existing workstream, via `on`) the step's effective prompt;
2. poll `kanstack status --json` until that workstream's `status` is `idle`;
3. if the step has `verify` commands, run them all; every one must exit `0`;
4. mark the step `complete`. Its dependents become runnable once *all* of *their* `needs` are
   `complete`.
5. a step with no `verify` commands is `complete` as soon as it goes `idle`.

### Design choices this runner makes (and why)

**Polling `status --json`, not `events`.** kanstack offers both a poll (`status --json`) and a
push-ish cue to re-poll (`events --json`, an append-only JSONL log). This runner polls
`status --json` directly on a fixed interval. It's a short-lived process, not a daemon, so there
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
directory `run` was invoked in, and for each step its status (`pending`, `launching`,
`running`, `verifying`, `done` or `failed`), `attempt`, `branch`, `pane`, `base` and `tip`
(its branch's commit when it started and when it finished, from git), `started_at` and
`last_error`.

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
| idle | Run the step's `verify` commands, which must be safe to re-run. |
| gone (absent, dead, or no pane) | Count a failed attempt. There are no retries yet, so the step fails. |

Resume never sends a prompt to a lane that still exists — not even for a step that was only
`launching`, whose send may never have landed — because a duplicate send garbles the agent's
input. The first-read rule above still holds: unless the step was already `verifying`, an
`idle` on resume's own read is only trusted on the next one. Several unfinished runs are
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
target not reachable (directly or transitively) through the step's own `needs`, and malformed
YAML front matter.

## What this crate does not do

No DAG scheduling, recipe parsing, or workflow state lives inside kanstack itself — that stays
entirely in this crate, which (as of v1):

- persists only its own run state (see "Durable runs"), never kanstack's or the tracker's, and
  never as the source of truth for a lane — a resumed step always asks kanstack about its lane
  first;
- counts attempts but never retries one, and has no timeout or conditional-branching logic;
- does not build a new file-locking system around `owns` — that stays advisory metadata;
- does not talk to any provider SDK, MCP server, or run as a daemon/network service.

Anything beyond this is out of scope for v1 by design — see the main repository's design notes
for the full non-goals list.
