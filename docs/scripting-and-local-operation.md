# Scripting `ctl`, local operation, and agent backends

This guide records the automation contract for `sgian ctl`, separates local
daemon storage and persistence from child-CLI provider network traffic and
optional SSH remote access, and specifies the adapter seam required to qualify
an agent backend.

## 1. Scripting `sgian ctl`

Every control command accepts `--workspace <path>` (or `SGIAN_WORKSPACE`) and
`--json` before or after the verb. Under `--json`, stdout carries one structured
JSON value (or newline-delimited JSON objects on streaming commands such as
`ctl agent --watch --json`), while errors and human diagnostics go to stderr.

### Exit codes

`sgian ctl` exits `0` on success and non-zero on failure:

| Command | Exit `0` | Exit `1..=255` | Exit `1` |
|---|---|---|---|
| `ctl process [--cwd DIR] [--timeout MS] -- <ARGV...>` | Child exited `0` | Child exited non-zero (`code.clamp(1, 255)`); JSON result still printed on stdout first | Timeout (`timed_out: true`), signal death without exit code, or daemon/argument error |
| `ctl run [--pane P] [--timeout MS] -- <CMD...>` | Command marker reported `0` | Command's own shell status (`code.clamp(1, 255)`); JSON result still printed on stdout first | Marker timeout, dead pane, or batched `--all` / `--panes` with any failing pane |
| `ctl wait <PANE> (--text\|--regex\|--idle\|--exit\|--attention) [--timeout MS]` | Condition matched before timeout | — | Timeout (`matched: false`, JSON still printed) or pane/daemon error |
| `ctl ledger [PANE] --verify` | Hash chain verified intact | — | Tampered or truncated record, or missing ledger |
| All other `ctl` verbs | Request accepted by daemon | — | Refused request (scope, lease conflict, invalid grant, bad arguments, unreachable daemon) |

> **Windows note:** Release `sgian.exe` builds use the Windows GUI subsystem so
> launching the desktop app does not flash a console window. In `cmd.exe` or
> POSIX shells, wait explicitly when scripting exit codes; in PowerShell 7 use
> `(Start-Process -FilePath sgian.exe -ArgumentList 'ctl', ... -Wait -PassThru).ExitCode`.

### Reproducible bounded run example

Use `ctl process` when automation wants verbatim `argv` execution (no shell
expansion, exact exit code, bounded stdout/stderr capture, and `scrub_env`
applied). Use `ctl run` when a command must execute inside a visible shell pane
with a `--timeout` guard against hung TUIs or prompts:

```bash
#!/usr/bin/env bash
set -euo pipefail

export SGIAN_WORKSPACE="${SGIAN_WORKSPACE:-$PWD}"

# 1. Non-PTY verbatim argv execution with a 30s ceiling and exact exit status.
if out="$(sgian ctl --json process --timeout 30000 -- cargo test --lib -- --list)"; then
  printf 'listed tests (%s ms)\n' "$(printf '%s' "$out" | jq -r '.elapsed_ms')"
else
  status=$?
  printf 'cargo exited %d: %s\n' "$status" "$(printf '%s' "$out" | jq -r '.stderr')" >&2
  exit "$status"
fi

# 2. Create or reuse a named shell pane, take a generation-fenced lease, and run
#    a bounded command whose scrollback stays inspectable in every client.
pane_id="$(sgian ctl --json new --name ci-verify | jq -r '.id')"
lease="$(sgian ctl --json lease take "$pane_id" --as ci-runner)"
gen="$(printf '%s' "$lease" | jq -r '.generation')"

set +e
run_json="$(sgian ctl --json run --pane "$pane_id" --timeout 60000 -- cargo test --workspace)"
run_rc=$?
set -e

sgian ctl --json lease release "$pane_id" \
  --as ci-runner \
  --generation "$gen" \
  --note "ci-verify finished (rc=${run_rc})" >/dev/null

sgian ctl --json ledger "$pane_id" --verify >/dev/null
exit "$run_rc"
```

Passing `--generation "$gen"` on `send` and `lease release` guarantees that if a
human force-took the pane (`ctl lease take --force --why ...`) mid-script, late
writes from the script fail cleanly as stale instead of colliding with the human.

### Agent-to-agent steering (`--attention` and `lines --last`)

When an orchestrating agent drives a worker CLI in another pane, prefer
`ctl wait <PANE> --attention needs_input,idle` over byte-quiet `--idle` waits,
and read recent output with `ctl lines <PANE> --last N` (`-n N`) rather than
scraping the visible viewport (`docs/design/agent-steering-and-machines.md`):

```bash
sgian ctl --json wait "$pane_id" --attention needs_input,idle --timeout 300000
sgian ctl lines "$pane_id" --last 80
```

### Streaming transitions without polling

Coordinators and scripts that watch humans and agents should subscribe to
`ctl agent --watch --json` rather than polling `ctl panes` or `ctl snapshot`:

```bash
sgian ctl agent --watch --json | while IFS= read -r event; do
  kind="$(printf '%s' "$event" | jq -r '.event')"
  case "$kind" in
    agent_state)
      # { "event": "agent_state", "pane_id": "pane-2", "agent": "claude",
      #   "attention": "needs_input", "mode": "manual", "unattended": false }
      ;;
    lease_state|pane_ended|output_warning|project_notes_changed)
      ;;
  esac
done
```

### Capability-scoped PTY execution (`ctl grant`)

When Kranz authorizes an ACP worker's `terminal/*` call, Sgian verifies an
HMAC-SHA256-signed `ExecutionGrant` against `<repo>/.kranz/serve.grant.key`
(`0600`) before spawning verbatim `argv` in a PTY pane held under `kranz:<run>`
(`docs/design/execution-grants.md`):

```bash
term="$(sgian ctl --json grant exec --grant @grant.json --sig "$HMAC_HEX")"
tid="$(printf '%s' "$term" | jq -r '.terminal_id')"
sgian ctl --json grant wait "$tid" --timeout 120000
sgian ctl --json grant output "$tid"
sgian ctl --json grant release "$tid"
```

## 2. Local operation, provider traffic, and remote access

Sgian separates four distinct planes so operators know what stays on disk and
what crosses a network:

1. **Daemon persistence and local IPC (strictly local).**
   - One background daemon runs per canonical workspace path and listens only on
     an owner-restricted Unix domain socket (`0700` runtime directory, `0600`
     token and state files, peer-UID check before `hello`) or an owner-SID-DACL
     Windows named pipe. The daemon never opens a public TCP port and emits
     **zero** telemetry or analytics.
   - Workspace state (`workspace.json`, `workspace.cwd`), tokens
     (`daemon.token`, `clients.json` storing SHA-256 credential hashes only),
     raw PTY scrollback (`scrollback/<pane>.log`), normalized scrubbed agent
     conversation logs (`agents/<pane>.jsonl`), hash-chained ledgers
     (`ledger/<pane>.jsonl`), and rotated daemon logs live under the user's OS
     application-data directory.
   - Project context notes (`<repo>/.sgian/projects/<name>/notes/*.md`) live in
     the project repository so git tracks them; Sgian never syncs them over a
     network on its own.
   - Optional background update checks in desktop builds fetch the signed public
     release feed and verify updates against the embedded minisign/Sparkle key;
     errors are silently skipped when offline.

2. **Configuration composition and environment hygiene (`scrub_env` / `env`).**
   - Global `config.json` is overlaid by the per-workspace `config.json`.
     Explicit `Some([])` / `Some(0)` in the workspace layer overrides a non-empty
     global list or non-zero timeout; `write_config` preserves omitted keys and
     replaces present keys. Malformed config layers fail closed at startup and
     preserve the last valid policy on live reload.
   - Child spawns (shell panes, agent panes, and `ctl process`) start from the
     daemon's environment, unconditionally strip `CLAUDECODE`,
     `CLAUDE_CODE_CHILD_SESSION`, and `CLAUDE_CODE_ENTRYPOINT`, remove every
     variable named in `scrub_env`, and finally apply explicit `env` entries
     (and profile `env` overrides). Explicit `env` values therefore win over
     `scrub_env`. Execution grants (`grant_exec`) are stricter: they clear the
     environment completely and copy **only** names listed in the grant's
     `env_allow` from Sgian's computed spawn environment.

3. **Agent CLI provider traffic (outbound from child CLIs, not local).**
   - Sgian does not proxy model inference. When you run `claude`, `droid`, or
     another coding agent in a shell pane or an agent pane, **that child CLI
     connects directly over the network to its model provider** (Anthropic,
     Factory, or endpoints configured in that CLI's own settings/environment)
     using the child CLI's own credentials, prompts, tool inputs, and repository
     context.
   - Sgian's local IPC boundary does not keep an agent CLI's prompts or tool
     outputs from leaving the machine to its configured model provider.

4. **Optional remote access (`ssh -L` and saved `machines`).**
   - Remote `ctl` or desktop access forwards the remote daemon's IPC endpoint
     over SSH (`ssh -N -L`) and authenticates with a scoped per-client
     credential (`SGIAN_CLIENT_TOKEN`, Keychain/PasswordVault, or a `0600`
     per-machine token file issued via `ctl identity issue --holder NAME --scope
     read|write`).
   - **Saved remote workspaces (`machines`, #114):** Register remote hosts once
     in user-global `config.json` with `ctl machine add|list|remove` and target
     them from any directory via `--machine <name>` (`-m <name>` before the
     subcommand), `SGIAN_MACHINE`, or `<machine>/<pane>` shorthand:

     ```bash
     # 1. On the remote host, issue a scoped client credential:
     sgian ctl --workspace /srv/repo identity issue --holder laptop-op --scope read --scope write

     # 2. On the local workstation, save the machine and store the token (0600):
     printf '%s\n' "$REMOTE_TOKEN" | sgian ctl machine add devbox \
       --host ubuntu@10.0.1.5 \
       --workspace /srv/repo \
       --token-stdin

     # 3. Target panes directly — Sgian discovers the remote endpoint and owns
     #    the local ssh -N -L tunnel under runtime/machines/<name>/:
     sgian ctl --machine devbox panes
     sgian ctl wait devbox/pane-2 --attention needs_input,idle
     sgian ctl lines devbox/pane-2 -n 60
     ```

     Native macOS and Windows desktop clients read the same global `machines`
     list, display saved machines in the sidebar and command palette, and attach
     through `sgian ctl --machine <name> --json ipc-endpoint`.
   - **Windows daemon as a remote target (#116):** Windows named pipes cannot be
     forwarded directly by OpenSSH `-L`. Setting `"remote_loopback_port": 0` (for
     an OS-assigned ephemeral port recorded in `daemon.loopback_port`) or an
     explicit port `1..=65535` in the Windows workspace or global `config.json`
     (or starting the daemon with `--listen-loopback <PORT>`) binds a second IPC
     listener strictly on `127.0.0.1:<port>` alongside the local named pipe.
     `ctl --json ipc-endpoint` surfaces `"loopback_port"`, which `machines`
     forwards over `ssh -N -L` automatically. Because loopback TCP lacks kernel
     peer-UID/SID identity, every connection on `127.0.0.1:<port>` **strictly
     requires** a valid per-client `client_token` (`daemon.token` alone is
     rejected).
   - **Headless Windows daemon operation (Task Scheduler / S4U):**
     - Standard SSH sessions or Task Scheduler tasks configured with **"Run
       whether user is logged on or not"** (S4U / non-interactive logon) run
       outside the interactive desktop (`Session 0`). Windows `ConPTY`
       (`CreatePseudoConsole`) works headlessly in `Session 0` on Windows 10
       1809+ / Windows 11 / Server 2019+, so shell panes, `ctl run`, and CLI
       agents operate without an interactive GUI logon.
     - Register a startup task for persistent headless operation:
       `schtasks /Create /TN "SgianDaemon-Repo" /SC ONSTART /RU "<DOMAIN\User>" /RP * /RL LIMITED /TR "\"C:\Program Files\Sgian\sgian.exe\" --daemon --cwd \"C:\Users\User\repo\" --listen-loopback 0"`
       (or use S4U `"Do not store password"` when network share / DPAPI user-key
       decryption is not needed).
     - **Session 0 caveats:** Interactive GUI credential prompts (Git Credential
       Manager modal windows, browser-based OAuth login flows) cannot render in
       `Session 0`. Pre-authenticate agent CLIs (`ANTHROPIC_API_KEY` or stored
       non-GUI credentials), configure non-interactive Git credentials (SSH keys
       via the Windows `ssh-agent` service or PATs in a file/credential store
       compatible with your logon type), and avoid S4U passwordless tasks if a
       tool relies on user-DPAPI secrets that require a full password logon.
   - **Loopback web view (`sgian ctl serve`):** Binds **only** to `127.0.0.1`
     (default port `8321`), requires a per-run random session key exchanged into
     an `HttpOnly; SameSite=Strict` cookie (so other local OS accounts on a
     shared host cannot read the loopback port), validates `Host`/`Origin`, caps
     open connections, and is read-only unless `--allow-write` is passed on the
     host. Reach it from a phone or laptop exclusively over
     `ssh -N -L 8321:127.0.0.1:8321`.

## 3. Agent backend adapter contract

Chat-native agent panes isolate provider wire differences inside
`AgentBackendAdapter` (`src-tauri/src/agent_stream.rs`), currently implemented
by `CLAUDE_BACKEND_ADAPTER` (Claude Code `stream-json`) and
`DROID_BACKEND_ADAPTER` (Factory Droid `stream-jsonrpc`).

Every adapter defines:

- **Binary resolution:** `default_bin`, `env_override` (`SGIAN_<PROVIDER>_BIN`),
  and `configured_bin(&AgentSpawnConfig)` — resolved through `plan_agent_bin`
  (including Windows `.exe`/`.cmd`/`.bat` `PATH` probing and `cmd.exe /c` batch
  wrapping).
- **Spawn argv and handshake:** `spawn_args(permission_mode, model,
  resume_session_id)` (filtering model/session tokens through
  `is_safe_cli_token` to block `-`/`--` flag injection) and
  `initial_stdin(pane_id, cwd, model, resume_session_id)`.
- **Stdin request framing:** `encode_user_message(seq, text)`,
  `encode_approval_response(request_id, tool_input, allow, message)`, and
  `encode_interrupt_request(seq)`.
- **Stdout normalization and non-blocking replies:** `immediate_reply(raw)` for
  unsupported interactive RPCs that would otherwise wedge the child,
  `normalize_event(raw)` translating provider JSON lines into Sgian's stable
  event kinds (`session`, `message_start`, `text_delta`, `message_complete`,
  `tool_use`, `tool_result`, `permission_request`, `turn_complete`, `error`),
  and `suppress_idle_turn_complete`.

### Qualification checklist for new backends (Codex, Gemini CLI, ACP)

Additional backends remain sequenced behind platform acceptance
(`ENHANCEMENTS.md` §§1, 6). Before merging a new `AgentBackendKind` variant:

1. **Empirical wire probe:** Record exact startup, multi-turn, `--resume` /
   session-load, tool-approval block/allow/deny, and interrupt payloads against
   a pinned CLI version.
2. **Manual-approval default & unattended visibility:** Default spawns must
   route tool approvals through `permission_request` → `AgentApproval`; any mode
   that executes tools without human confirmation must map to
   `unattended: true` in `AgentTracker`, badges, `ctl agent`, and `mode.changed`
   ledger records.
3. **Output-guard parity:** All normalized strings flow through
   `AgentEventLog::emit` (`scrub_agent_event`), stripping escapes/bidi/zero-width
   characters and crediting `OutputTricks` before persistence or broadcast.
4. **Lifecycle & Windows job containment:** Prove `SIGTERM` → `SIGKILL` (Unix)
   and suspended-spawn `KillOnCloseJob` termination (Windows), bounded line
   reading (`AGENT_OUTPUT_LINE_MAX`), approval timeout/child-exit unwedging, and
   fake-driver end-to-end integration tests in `src-tauri/src/tests/agents.rs`.
