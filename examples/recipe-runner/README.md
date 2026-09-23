# kanstack-recipe-runner

A small, disposable **reference orchestrator** for kanstack. It reads a Markdown+YAML recipe
describing a DAG of steps, and drives them to completion by shelling out to `kanstack`'s public
CLI/JSON contract (`spawn`, `send`, `status --json`) — nothing else. It never imports kanstack's
Rust crate or touches its internals.

kanstack itself has no workflow engine, no scheduler, and no notion of a "task" or "plan" — see
[`docs/automation.md`](../../docs/automation.md) in the main repository. This crate is an
example of the orchestrator kanstack expects *you* to build on top of it, kept boring and small
enough to throw away if the shape is wrong. It is independent of the root `kanstack` package:
building or testing from the repository root never builds or tests this crate, and vice versa.

## Build and run

```sh
cd examples/recipe-runner
cargo build
cargo test

# kanstack not on $PATH? point at it explicitly:
cargo run -- check ../recipes/implement-review-docs.md \
  --kanstack-bin ../../target/debug/kanstack

cargo run -- run path/to/recipe.md
```

## CLI surface

```sh
kanstack-recipe run   path/to/recipe.md [--kanstack-bin <path>]   # execute the recipe
kanstack-recipe check path/to/recipe.md [--kanstack-bin <path>]   # parse + validate only, spawns nothing
```

`--kanstack-bin` overrides where the `kanstack` binary is found (default: looked up on `$PATH`).
There is no TUI and no interactive mode; progress prints to stdout as steps launch and finish,
roughly:

```text
✓ recipe valid
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
| `on` | no | a step id — **placement**. Send this step's prompt to the *same* workstream an earlier step created (`kanstack send`) instead of opening a new one (`kanstack spawn`). Determines *where* a step runs, independent of `needs`. Must reference an already-declared step; chains are followed to the step that actually owns the workstream. |
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

### Validation errors

`check` (and `run`, before doing anything) rejects a recipe with a specific message, e.g.:

```
recipe error: step "review" depends on unknown step "implementt"
```

Rejected: missing/unsupported `version`, missing/empty `steps`, an empty step id, a step missing
`agent` or `prompt`, an invalid `effort` value, a `needs` or `on` entry naming an unknown step, a
step that `needs` or is `on` itself, a cycle in `needs`, a placement cycle through `on`, and
malformed YAML front matter.

## What this crate does not do

No DAG scheduling, recipe parsing, or workflow state lives inside kanstack itself — that stays
entirely in this example crate, which:

- never persists workflow state — if the process exits mid-run, nothing needs to be reconstructed
  on the next run;
- has no retry, timeout, or conditional-branching logic;
- does not build a new file-locking system around `owns` — that stays advisory metadata;
- does not talk to any provider SDK, MCP server, or run as a daemon/network service.

Anything beyond this is out of scope for v1 by design — see the main repository's design notes
for the full non-goals list.
