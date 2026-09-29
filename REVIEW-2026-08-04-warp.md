# Engineering follow-ups from the Warp CLI review (2026-08-04)

The historical review of [Warp's CLI documentation](https://docs.warp.dev/cli/)
prompted the Sgian requirements below. This public note retains engineering
scope and attribution; it does not assess another product's market position
or certify its current capabilities. See [ENHANCEMENTS.md](ENHANCEMENTS.md)
for current delivery status and prioritization.

## Control-plane documentation

Document how scripts use `ctl --json`, exact exit codes and bounded runs.
Examples should cover pane discovery, command execution and handling a
needs-input transition, with observable success and failure outcomes.

## Local operation and data flow

Describe the local daemon, client attach/detach lifecycle, transcript storage
and diagnostic redaction. Distinguish Sgian's local storage from an agent CLI's
provider connections and from any operator-enabled served view. Documentation
must accurately describe which component can transmit data.

## Permission visibility and configuration composition

Make `auto`, `dontAsk` and `bypassPermissions` visible on every affected pane.
Audit where custom configuration extends or replaces defaults, how explicit
`env` values interact with `scrub_env`, and whether any approval setting can
bypass a deny rule. Cover the documented precedence with regression tests.

## Machine-readable agent state

Expose working, needs-input and idle transitions through a structured
subscription, using the daemon's existing event stream. External scripts and
mission coordinators should be able to react without screen scraping or
polling. Keep delivery, disconnect and reconnect behavior explicit.

## Additional adapters

Evaluate Codex and Gemini CLI adapters behind the existing normalized event
stream. Admission requires version-pinned evidence for authentication,
permissions, streamed output, cancellation and process cleanup. A candidate
name alone is not a supported-backend claim.

## Platform validation and scope

Follow the current platform acceptance and release checks, including Windows
ConPTY behavior. External implementation research does not authorize copying
code; use the repository's contribution and dependency policies.

Sgian owns live sessions, keyboard ownership and their ledger. Kranz owns
mission plans, approvals, gates and evidence. Integrate through documented
control APIs without adding mission orchestration to the terminal daemon.
