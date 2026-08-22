# Sgian — Competitive review: Warp Agent CLI launch (2026-08-04)

Warp shipped its standalone Agent CLI today (announced 2026-08-04; previewed
as `warp-tui` since 2026-07-23). This review reads that launch — plus Warp's
2026 arc — against sgian, because Warp's trajectory now runs directly through
sgian's territory: the terminal as an agent-supervision surface. Research
basis: Warp's launch post and docs (docs.warp.dev/cli/*), the day-of HN
thread, the Terminal-Bench 2.0 leaderboard, and Warp's 2026 announcements.
All Warp claims below are first-party unless noted.

## Warp's 2026 arc, compressed

- **Feb 2026** — Oz launches: closed cloud orchestration platform (Docker
  sandboxes, parallel/scheduled agents, audit trails). This is the
  monetization layer.
- **Apr 2026** — the Warp terminal client is open-sourced (AGPL v3; UI
  framework crates MIT), OpenAI as "founding sponsor." Explicit
  repositioning as an "agent development environment." A community fork
  (OpenWarp) that strips the account requirement and cloud dependency drew
  209 HN points — a demand signal worth reading twice.
- **Apr 2026** — Universal Agent Support: Warp terminal hosts third-party
  agent CLIs (Claude Code, Codex, Gemini CLI, OpenCode) with a code-review
  panel, vertical tabs, and multi-agent management UI.
- **Aug 4, 2026** — the Agent CLI: Warp's own agent decoupled from its
  terminal, running in any terminal on a tmux-like pty mux. Sessions
  persist across `cd`/SSH (no remote binary), and the agent can drive
  full-screen apps (vim, gdb, REPLs).

Claimed scale: 700k+ developers, "over half the Fortune 500" (Warp's own
numbers, unverified). Funding: $73M total, last round 2023.

## Feature comparison on sgian's own axes

| Axis | Warp (terminal + Agent CLI) | Sgian |
|---|---|---|
| Session persistence | pty mux "similar to tmux"; sessions survive across dirs/SSH | Daemon-owned PTYs, tmux-style survival by architecture; GUI is a thin client |
| Agent supervision | Universal Agent Support hosts 4 third-party CLIs; management UI; review panel | Agent panes (headless Claude Code / Factory Droid, chat-native, permission cards) + attention badging (working / needs-input / idle) auto-detected even for agent TUIs in plain shell panes |
| Scriptable control plane | **None on the CLI**: no headless/exec mode, no JSON output, no lifecycle hooks (verified against the CLI reference; hooks are an open upstream feature request). Automation routes through the closed Oz platform | `ctl` with `--json`, exact exit codes, `run --timeout` (clean marker-miss failure), batched multi-pane exec, non-PTY `process`, `broadcast`, `wait` |
| Local-first | Account required; refuses to start offline; transcripts cloud-synced; **even self-hosted enterprise routes transcripts/inference through Warp's backend** | Local daemon on a token-authenticated socket/named pipe; no account; no cloud; privacy-safe `ctl diagnostic`; env scrubbing |
| Permission defaults | `--auto-approve` **bypasses the command denylist by default**; a custom denylist **replaces** the built-in rather than extending it | `agent_permission_mode` defaults to `manual` with in-app approval cards; dangerous modes are explicit opt-in config |
| License | Client AGPL v3 (agent backend, Oz, cloud services closed) | MIT, whole product |
| Models | Multi-model with auto-routing + custom routers; BYOK | Claude Code + Droid via each CLI's own auth — vendor-neutral, no middleman on inference |
| Platforms | macOS / Linux / Windows, day one | macOS / Linux shipping; Windows native build exists, acceptance is the §1 release blocker |

## The read

**Threat, honestly stated.** Warp validates and crowds sgian's core concept
at scale — "panes + persistent sessions + supervised agents" is now the
positioning of a funded company with distribution, and its open-sourced
client normalizes the category. Its supervision UI (review panel,
multi-agent management) is ahead of sgian's today. Warp ships Windows day
one; sgian's Windows acceptance blocker (§1 of ENHANCEMENTS) gains
competitive urgency from this.

**The moat Warp cannot cross.** Warp's business is Oz plus inference
credits; the account requirement and cloud routing are structural, not
oversights — even their self-hosted enterprise story routes transcripts
through their backend. Sgian's architecture — local daemon, no account, no
cloud, MIT — is exactly what the OpenWarp fork's traction demonstrates
demand for, and Warp cannot follow without dismantling its revenue model.

**The surprising edge: automation.** The product that launched today as "a
CLI coding agent" has no headless mode, no JSON output, and no hooks.
Sgian's `ctl` already has the machine-readable control plane Warp lacks —
exact exit codes, JSON everywhere, bounded runs, batched exec. That makes
sgian orchestrable by outside tooling (scripts, CI, or a mission
orchestrator such as kranz) in a way Warp's agent simply is not. This is
sgian's most defensible *feature* advantage and it is currently
undersold — the README leads with panes, not with the control plane.

**A cautionary example worth stealing as a checklist.** Warp shipped two
fail-open permission defaults (auto-approve bypassing the denylist;
replace-not-extend denylists). Sgian's defaults are safer today, but the
same audit is worth running deliberately: every config surface where a
custom value could *replace* a safer default (e.g. `scrub_env` vs explicit
`env` precedence), and every place a dangerous agent permission mode
(`auto`, `dontAsk`, `bypassPermissions`) can be active without being
*visible* on the pane that runs under it.

**Benchmark context** (for positioning copy, not engineering): Warp's
"best agent" framing rests on a June 2025 Terminal-Bench result (52%, #1
then). On today's TB 2.0 leaderboard its best entry is 61.2% at rank ~41
(Codex CLI: 82.2%). Sgian doesn't compete on agent quality at all — it
supervises other vendors' agents — which this stale-claim dynamic quietly
vindicates: harness rankings churn; the supervision surface persists.

**AGPL caution.** The open-sourced Warp client is worth *studying* (GPU
renderer, session mux internals, their ConPTY handling is directly relevant
to §1's Windows work) but not lifting from: sgian is MIT and AGPL
contamination would be a one-way door.

## Relation to kranz

The boundary stays clean and is worth restating since Warp blurs the
equivalent line in its own product: sgian supervises live sessions (panes,
attention, human-in-the-loop); kranz governs missions (plans, gates,
evidence). The integration seam is already built on sgian's side: `ctl
--json` + agent-state broadcasts make sgian a natural attended dispatch
surface for an orchestrator, with no new coupling required.

## Candidates fed into ENHANCEMENTS §6

1. Lead with the control plane (docs/positioning — cheap, high leverage).
2. Local-first positioning statement (docs — cheap).
3. Permission-visibility pass: badge panes running under dangerous agent
   permission modes; audit replace-vs-extend config surfaces (small code).
4. Machine-readable agent-state stream (`ctl agent --watch --json` or
   similar) so external tooling can react to needs-input/idle transitions
   (small-medium code; also the kranz integration seam).
5. Additional agent backends behind the existing normalized event stream
   (Codex/Gemini CLI candidates — medium; strengthens the vendor-neutral
   claim Warp's launch makes newly legible).
