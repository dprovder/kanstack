---
name: kanstack
description: "Run several coding tasks at once, each on its own GitButler branch with its own coding agent in its own terminal pane, using kanstack — then check on, message, and clean up those agents. Use when the user wants work split up and done in parallel, or asks about agents, lanes or workstreams kanstack already started."
when_to_use: "Examples: \"run these in parallel\", \"spin up agents for these\", \"parallelize this\", \"split this work up\", \"have another agent do X while you do Y\", \"check on the agents\", \"watch the lanes\", \"tell me when they're done\", \"how are the lanes doing?\", \"tell the agent on fix-login to...\", \"stop that agent\". Also use BEFORE hand-rolling parallelism yourself: creating a `git worktree`, splitting a tmux or terminal pane to run another agent, or fanning code-editing work out to Agent/Task subagents. kanstack does that with real branches and visible panes; the skill says how to fall back if kanstack isn't installed."
---

# Operating kanstack

kanstack (https://github.com/dprovder/kanstack) manages parallel workstreams —
one GitButler branch, one pane, one coding agent — over a single shared
workspace checkout. As an orchestrator you drive it entirely through its CLI;
there is no daemon, no API, just commands that print JSON.

## First: is kanstack actually here?

This skill can load in a session where kanstack isn't installed, so check
before relying on it: run `kanstack status --json`. If the command isn't
found, tell the user kanstack isn't installed in this environment (it's at
https://github.com/dprovder/kanstack) and fall back to doing the work
yourself, one task at a time — don't improvise a substitute out of worktrees,
tmux splits or editing subagents. If it runs, its output is your starting
picture (see below).

`spawn`, `send`, `focus` and `stop` drive panes, so they have to run inside
the terminal multiplexer the panes live in (cmux, tmux, Orca or Ghostty). An
`error.code` of `multiplexer_unavailable` means this session isn't running in
one — tell the user rather than retrying. `status --json` and `events` work
from anywhere; outside a multiplexer `status` reports every pane as `unknown`.

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
to the `export` command," not "make the CLI more scriptable." A narrow task
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

## Watching lanes: arm one watch, don't poll

Once lanes are running, don't call `kanstack status` over and over to see
how they're doing. Checking by hand is how you miss a lane that went idle
and has been waiting on you since your last look. Instead, right after
spawning, start one long-running watch on the events log:

    kanstack events --follow --new

`--new` skips the backlog and `--follow` keeps streaming new lines. Run it
as a background command that your harness notifies you from when it prints
(in Claude Code, a background Bash command or a monitor), in the same
repository as the lanes, or with `kanstack -C <repo>`. One watch covers
every lane, because each line carries its own `branch`. Don't start one
watcher per lane.

**Don't consume the raw stream.** A `report` line is appended on every hook
fire, not only when a lane's state changes. A working lane re-reports
`{"kind":"report","state":"busy"}` on every tool call, every few seconds,
for as long as it's working; in a real log, over 95% of lines are these
repeats. A plain tail floods you with them. Collapse the stream to
*transitions* (remember each branch's last state and pass a line only when
the state differs), and drop the `busy` direction, which never needs you:

```sh
kanstack events --follow --new | jq --unbuffered -nc '
  foreach inputs as $e ({last: {}};
    if $e.kind == "report" then
      .emit = (.last[$e.branch] != $e.state and $e.state != "busy")
      | .last[$e.branch] = $e.state
    else .emit = true end;
    select(.emit) | $e)'
```

The filter still records `busy`, so `idle` → `busy` → `idle` prints the
second `idle`. What reaches you is a lane going `idle` or `waiting`, plus
every non-report line (spawn/stop/prune lifecycle, and note/ask/answer).

Each line is a doorbell, not the payload: when one arrives, read
`kanstack status --json` for the real state, then act on it:

- **`waiting`**: the lane is stopped on a permission prompt and needs a
  *human*. Tell the user which lane it is and what it's blocked on. Don't
  `stop` it, and don't `send` to it, because a send types into the prompt
  itself.
- **`idle`**: usually the lane finished a turn and is waiting for your next
  `send`. It does *not* mean the task is done. Look at its branch, then
  either send the next instruction or, if the work really is finished,
  wrap it up (see below).
- **`dead`**: the pane is gone (closed outside kanstack, or its process
  died), and the entry wants `kanstack prune`. This state never appears in
  the events stream: it comes from `status --json` polling the multiplexer.

Coverage gaps: only harnesses kanstack hands status hooks to (Claude Code,
Gemini CLI and Codex, today) ever report. A lane running any other harness
never rings the doorbell, and neither does a pane dying. So if the stream
has been quiet for a long while, one `status --json` check is reasonable.
That's an occasional check, not a loop.

## Before a destructive move: check state

`kanstack stop` and `kanstack prune` are hard to undo — `stop` ends the
harness and closes the pane; `prune` forgets any workstream whose pane is
confirmed gone. Before either, read `kanstack status --json` so you know
what you're about to end. (Waiting for a lane to finish first? That's the
watch above, not a polling loop.) Don't stop
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
in kanstack's own development (see kanstack's GitHub issue #9,
https://github.com/dprovder/kanstack/issues/9). GitButler resolves most concurrent
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
