---
name: kanstack
description: "Operate kanstack as an external orchestrator: spawn/send/inspect/stop workstreams (parallel GitButler lanes, each its own coding-agent pane) instead of managing worktrees or panes by hand. Use whenever you are about to start, message, check on, or tear down a coding-agent workstream in a repo that has `kanstack` on PATH."
---

# Operating kanstack

kanstack manages parallel workstreams — one GitButler branch, one pane, one
coding agent — over a single shared workspace checkout. As an orchestrator you
drive it entirely through its CLI; there is no daemon, no API, just commands
that print JSON.

## Before anything: look first

Run `kanstack status --json` before spawning, sending, stopping, or pruning
anything. It's cheap (no multiplexer required) and tells you, per workstream,
whether a pane exists and whether it's `busy`, `idle`, `waiting`, `dead`, or
`unknown`, plus the branch's git state (commits, conflicts, whether it's
landed, push status). Never assume a workstream's state from what you last
did — another agent, or the human, may have changed it since.

If `workspace_blocked` is non-null, `but` is refusing every command until a
stray commit on `gitbutler/workspace` is cleared — that's a specific, fixable
condition, not "kanstack is broken." Surface it rather than retrying blindly.

## Spawning a workstream: keep the job narrow

`kanstack spawn <branch> --prompt "..." --json` creates a branch, opens a
pane, and hands the agent in it a task. Scope that task the way you'd scope a
commit: one bounded piece of work with a clear "done" — "add `--json` output
to the `focus` subcommand," not "make the CLI more scriptable." A narrow task
is one you can tell is finished by reading its diff, and one another
workstream can safely run alongside without touching the same code.

If the new work depends on another workstream's branch (its base doesn't
exist without that branch's commits, or you want it reviewed as a stack),
use `kanstack spawn <branch> --above <base>` (or `--below`) to stack it
there directly, rather than improvising the branch relationship yourself.

## Following up: send, don't respawn

Once a workstream has a pane, `kanstack send <branch|session> "..." --json`
delivers a follow-up message to the same agent, same context. Only `spawn` a
new workstream when you actually want a new, independent branch — respawning
a lane that's mid-task to change direction just orphans its pane and its
context. Check `status --json` first: if the branch already has a pane, send;
if it doesn't, spawn.

## Before a destructive move: check state

`kanstack stop` and `kanstack prune` are hard to undo — `stop` ends the
harness and closes the pane; `prune` forgets any workstream whose pane is
confirmed gone. Before either, read `kanstack status --json` (or
`kanstack events --follow` if you're waiting on a specific workstream to
finish rather than polling) so you know what you're about to end. Don't stop
a workstream just because it's `idle` — idle often means it's waiting for
your next `send`, not that it's done. `waiting` means it's stopped on a
permission prompt and needs a human, not a `stop`.

## Clean up finished or abandoned work

Don't let panes accumulate. Once a workstream's task is done and landed (or
you've decided to abandon it), `kanstack stop` it. Run `kanstack prune`
periodically (or before a batch of new `spawn`s) to clear workstreams whose
panes died outside kanstack's knowledge — it only ever removes confirmed-dead
entries, so it's safe to run speculatively.

## Don't run two agents on the same conceptual task

If two workstreams are likely to touch the same files or functions, don't
run them as blind parallel siblings — sequence them. Either stack the second
one on the first (`kanstack spawn --above <first-branch>`) so it starts from
the first one's committed result, or hold it until the first lands.

This is not a hypothetical caution — it's a real, repeatedly-hit failure mode
in this very project's own development (see GitHub issue #9, and the git
history of concurrent branches colliding). GitButler resolves most concurrent
edits at the commit level by hunk, but **it cannot split a diff hunk that two
concurrent edits fuse into one contiguous region** — if your agent and
another agent's edits land on adjacent or overlapping lines before either
side has committed, one edit can silently clobber the other in the shared
working tree, or the two changes end up entangled in a single hunk that
can't be committed to either branch alone without manually re-splitting the
file by hand. The fix is to not create the collision in the first place:
give concurrent workstreams non-overlapping code, or make them serial
(stacked or sequenced) instead of parallel when their targets overlap.
Running two agents in parallel on the *same* task deliberately (e.g. for
review — one implements, one critiques without editing) is fine; two agents
both editing is the case to avoid.

## Let kanstack and GitButler own the mechanics

Don't shell out to raw `git` (worktrees, branches, checkouts) or invent your
own scheme for isolating a workstream's changes. GitButler's virtual-branch
model and kanstack's registry already handle that — a `kanstack spawn` gives
you a real branch and a real pane without a worktree per agent. Drive
version control through `but` (the GitButler CLI) if you need to inspect or
touch git state directly, and drive workstream/pane lifecycle through
`kanstack`'s own subcommands (`spawn`, `send`, `focus`, `stop`, `status`,
`prune`, `events`). Every one of those takes `--json`; use it.
