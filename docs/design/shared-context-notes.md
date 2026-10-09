# Shared context notes for a project

**Status: design, not built.** The last roadmap item that is not blocked on
something outside this repository. Written before code because the hard
questions are about ownership and trust, not plumbing.

## What it is for

A project in Sgian is a named group of panes serving one goal, with an
attention roll-up and a merged ledger. What it lacks is a shared memory. The
people and agents in a project learn things that the others need: a test
that is flaky for a known reason, a decision taken in one pane that another
pane is about to contradict, the command that reproduces the bug. Today that
knowledge lives in one agent's context window or one person's head and is
retyped into every new session.

A context note is a short file under the repository, one directory per
project, that every pane in the project can read and that people and agents
can add to with attribution. Git keeps the history; the ledger records who
changed what and when; the daemon tells each pane when something changed.

## Shape

```
<repo>/.sgian/projects/<project-name>/
  README.md         the goal and anything a newcomer must know
  notes/
    2026-10-08-flaky-auth-test.md
    2026-10-08-decided-pg-over-sqlite.md
```

- One file per note, named by date and slug, so two writers never collide
  on a line and git never has to merge inside a note.
- Front matter names the author (`holder`, exactly as the lease and ledger
  name people and runs) and the pane it came from; the body is Markdown.
- The directory is committed to the repository the project names (`repo`
  on the project, the workspace otherwise). It is part of the project's
  history, reviewable in a pull request like anything else.
- A hard cap on size per note and per directory (for instance 16 KiB and
  1 MiB) keeps the agent-facing payload bounded.

## Who writes

Anyone who can type into a pane in the project: a person at a client, a
person through `ctl`, or an agent through its tools. Two rules keep that
honest:

- **Attribution is derived, never declared.** A note's author is the
  credential holder of the connection that wrote it, as with input and
  leases. An agent writing through a tool is attributed to the run or the
  pane that owns it, never to a person.
- **Writes are ledgered.** `note.added`, `note.changed` and `note.removed`
  go into the project's merged ledger with the file name and a content
  hash, next to the lease and attention records, so "who told the agents
  that" has an answer.

The daemon does the writing (`ctl project note add <project> --title …`,
and the equivalent request from a client) so the rules above hold. An agent
that edits the directory directly through its own file tools bypasses them;
that write is still visible in git, and the daemon's file watch treats it as
a change by the pane's holder with evidence `file`, the same distinction the
attention badge makes between `hook` and `screen`.

## Who reads, and how an agent learns a note changed

Reading is open to every pane in the project. The daemon publishes a
`project_notes_changed` event with the project name, the file and the
content hash, and clients show the notes beside the project's panes. For
agents the daemon does not inject anything into a session on its own. Three
ways an agent gets the notes, from least to most involved:

1. **The agent reads the directory itself.** Its working directory is the
   repository; the notes are files. A line in the project's README telling
   agents where to look is enough for Claude Code's `CLAUDE.md` or an
   equivalent instructions file to point at them. This needs nothing from
   Sgian beyond keeping the directory tidy.
2. **`ctl project notes <project>`** prints them in one bounded document,
   newest first, for a Kranz gate, a reviewer or a script, in the shape
   `ctl project dossier` already uses.
3. **An opt-in prompt line per pane.** When a note changes, the daemon can
   send the pane one line, `[sgian] project notes changed: <file>`, as
   input, exactly like a person typing it. Off by default, because unasked
   input into an agent is the one thing this design must not do quietly;
   on only per project, and recorded in the ledger as a write by `sgian`.

## Trust

A note is text that an agent will read and may act on. That makes the
directory an injection surface, and the design treats it as one:

- Notes are rendered to people with the same scrub the agent-pane guard
  applies to agent output: escape sequences and control characters
  removed, bidi overrides and zero-width characters removed and counted.
  A note that tripped the guard is badged where it is shown.
- The output guard's counters apply to the directory as a whole, so a
  project whose notes carry hidden text shows the same amber mark as a pane
  whose output does.
- A note written by an agent is marked as such wherever it is shown, and
  the opt-in prompt line never carries note contents, only the file name.
  An agent decides to read; it is not fed.
- Nothing in a note is ever executed, templated, or interpreted by the
  daemon. It is Markdown to people and bytes to agents.

## What is deliberately not here

- No sync beyond git. Two machines share notes by pushing and pulling, as
  they share the code.
- No per-note permissions. A project's panes read all of it; the project
  is the boundary, as it is for the roll-up and the merged ledger.
- No summarisation or embedding. The notes are short by construction; an
  agent that needs more should read the repository.

## Build order, when it is built

1. Daemon: `project note add|list|rm` requests and `ctl` verbs, the ledger
   records, the file watch with `file` evidence, the size caps, and the
   scrub on read. Tests at the request level.
2. `ctl project notes` and the `project_notes_changed` event, with a line
   in `native-ipc.md`.
3. Clients: a notes list beside the project in the overview, with author
   and badge. The macOS sidebar first, since it already groups by project.
4. The opt-in prompt line, last and off by default.

Each step is useful on its own; the first alone gives agents a shared,
attributed place to write that git keeps.
