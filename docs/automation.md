# Driving kanstack from an orchestrator

What an external orchestrator — a human, a script, or another agent — needs to know to spawn,
watch, talk to, and stop workstreams through the CLI alone. See
[README.md](../README.md)'s "Driving panes from a script or an agent" section for the full
subcommand list, the `status --json`/`prune --json` schemas, the generic `--json` envelope, and
the exit-code table; this document doesn't restate any of that, only points at it.

## What kanstack is not

kanstack creates, observes, talks to, and stops workstreams. It does not decide what work to
do, break work into steps, or run anything on its own — that's the orchestrating agent's job,
not kanstack's. Concretely, kanstack has no daemon, no workflow engine, no scheduler, no
built-in notion of a "task" or "plan," and no opinion about what order lanes should run in or
when one is "done." A workstream is `busy`, `idle`, `waiting` or `dead`; nothing more.

This is deliberate, not an oversight — kanstack stays a thin, inspectable layer over `but` and
a multiplexer. An orchestrator that wants scheduling, retries, or a dependency graph builds
that itself, on top of the primitives below.

## The primitives

Everything an orchestrator needs is a subcommand, each documented in the README:

- `kanstack spawn <branch> [--agent ...] [--prompt ...] [--item ...] [--above <base>|--below <base>] [--json]`
  — start (or attach to, if already open) a harness pane on a branch, optionally stacked.
- `kanstack send <branch|session> "..." [--json]` — type a message into a pane and submit it.
- `kanstack status [--json]` — list every workstream and its state.
- `kanstack focus <branch|session> [--json]` — bring a pane to the front.
- `kanstack stop <branch|session> [--json]` — close a pane and forget the workstream.
- `kanstack report <busy|idle|waiting> [<branch>] [--json]` — an agent announcing its own state.
- `kanstack prune [--json]` — forget workstreams whose pane is confirmed dead.
- `kanstack events [--since <offset>] [--follow] [--json]` — tail the append-only log instead
  of polling `status` in a loop.

## An example loop

A primary agent spawning a review workstream stacked above an implementation one, nudging it
mid-flight, and tearing it down when done:

```sh
kanstack status --json

kanstack spawn implementation \
  --agent codex \
  --item github:#42 \
  --prompt "Implement the requested feature" \
  --json

kanstack spawn review --above implementation \
  --agent claude \
  --prompt "Review the implementation branch" \
  --json

kanstack send implementation "Review found an issue in parser.rs" --json

kanstack events --follow

kanstack stop review --json
```

`kanstack events --follow` is a blocking tail — run it in its own pane or background process,
not inline in a request/response loop; poll `--since <offset>` instead if the orchestrator
needs a single non-blocking check. There's no "wait until idle" subcommand: an orchestrator
either follows events for a state-change cue and then re-reads `status --json`, or polls
`status --json` directly.

## JSON and exit codes

Every subcommand above takes `--json`. `spawn`/`send`/`focus`/`stop`/`report` share one
generic envelope (`{"schema":1,"ok":true,"command":...,"workstream":...,"result":{...}}` on
success, an `error.code`/`error.message` pair on failure); `status`, `prune`, and `events`
each have their own independently-versioned shape. All of it — the envelope, the `status`
and `prune` schemas, the `events` line format, and the exit-code table (`0`–`5`, with
`error.code` as the precise reason inside each bucket) — is documented in the README's
"Driving panes from a script or an agent" and "Exit codes, and `--json` for every subcommand"
sections. Treat that as the reference; an orchestrator should parse against it directly
rather than against this document.

## Work items are opaque

`--item` (e.g. `--item github:#42`, `--item linear:ENG-7`) is stored verbatim and echoed back
in `status`/`status --json`'s `item` field. kanstack never parses, fetches, or authenticates
against whatever it names — it doesn't know GitHub or Linear exist. An orchestrator that wants
the item's actual content (issue body, ticket status) fetches it itself, through whatever
credentials and API it already has; kanstack is just a label for correlating a workstream back
to it.

## Concurrency guarantees

`Registry::with_lock` (an `flock` around the whole read-modify-write of the registry file)
means concurrent `kanstack` invocations — several orchestrators, or one orchestrator racing
itself on retry — never lose an update to the registry: each process's spawn/send/focus/stop
serializes against the others rather than interleaving. Real concurrent `spawn`s land distinct
workstreams with no dropped entries.

What that lock does **not** cover: two agents' *working-tree edits* in the same shared
GitButler workspace. The registry is safe; the files those agents are actually editing are
not automatically coordinated, and two lanes touching the same file can hit a hunk-level
collision when one tries to commit over the other's uncommitted change — this isn't
theoretical, it happened repeatedly while this milestone's own branches were built
concurrently in this repository's workspace (see this repo's git history around the
`events-log`, `spawn-above-flag`, and `mux-module` branches for real examples, and the
GitButler skill's "mixed hunk" guidance for the recovery technique). An orchestrator running
several agents against overlapping files should expect this and have a recovery plan — retry,
reroute to a different file region, or serialize those two lanes — kanstack itself doesn't
prevent or resolve it.

**A preventive check exists for two harnesses.** For Claude Code and Gemini CLI, kanstack
installs a `PreToolUse`/`BeforeTool` hook (`kanstack claim`) that can deny an edit outright,
before it lands, when another live lane already holds a fresh claim on the exact same file and
is currently busy — vetoing the collision above before it happens, rather than reconciling it
after. It matches on the whole file, not a line range, so two lanes editing different parts of
the same file are still blocked from each other; a claim clears itself out on a short timeout
or when the holding lane goes idle, so this never becomes a lock an orchestrator has to clear
by hand. Claude gets this through the same `--settings` flag as its status hooks; Gemini has no
such flag, so kanstack instead points `GEMINI_CLI_SYSTEM_SETTINGS_PATH` at a small settings
file it writes per branch (see `crate::harness::gemini::Gemini::status_hooks`) — additive,
same as Claude's, and cleaned up the same way claims are: `kanstack stop` and a respawn both
delete it (see `crate::report::Reports::forget`). `Codex`, `Pi`, `OpenCode` and `Kiro`
each have a blocking hook of their own, confirmed against their current docs, but none is
wireable the same lightweight way — a required on-disk plugin/extension file with
arbitrary-code-execution risk (Pi, OpenCode), an edit payload with no usable file-path field
(Codex's `apply_patch`, a raw multi-file patch string), or exit-code-2-only blocking with no
JSON alternative (Kiro) — so an orchestrator running codex/pi/opencode/kiro lanes against
overlapping files still needs the recovery plan above.

## Lifecycle semantics

- **Idempotent, safe to retry:** `stop` (closing an already-gone pane succeeds; a second
  `stop` after that gets `unknown_workstream`, a specific, non-`internal` code — not a crash).
  `spawn` (a retry after a mid-failure attempt — branch created but pane open failed — resumes
  cleanly rather than duplicating or corrupting state). `prune` (a second run against the same
  state removes nothing further).
- **Deliberately not idempotent:** `send` — retrying it means "deliver this message again,"
  not "make sure it was delivered once." An orchestrator that can't tell whether a `send`
  landed should not blindly retry it; check `status`/`events` first, or accept the message may
  be duplicated in the pane.
- `send`/`focus` on a registered workstream with no pane return `no_pane` (exit `3`), distinct
  from `unknown_workstream` (also exit `3`) — an orchestrator that needs to tell "never
  existed" from "existed, pane's gone" reads `error.code`, not the exit code.

## Out of scope

No workflow YAML, DAG/recipe engine, role system, scheduler, agent planner, issue
decomposition, provider SDKs, MCP server, ACP integration, daemon, database, or network API.
If an orchestrator needs one of those, it builds it on top of the CLI primitives above —
kanstack's job stops at spawn/observe/talk/stop.
