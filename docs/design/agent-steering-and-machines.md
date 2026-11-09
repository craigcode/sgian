# Agent-to-agent steering and saved remote workspaces (`machines`)

**Status:** Design accepted; `ctl` steering primitives (`ctl lines --last`, `ctl wait --attention`) and `CleanupReceipt` alignment shipped; saved remote workspaces (`machines`, #114) and Windows remote attach (#116) follow directly on this contract.

## 1. Problem

Two recurring workflows push against Sgian's local single-workspace CLI ergonomics:

1. **Agent-to-agent steering inside one workspace.** An orchestrating agent (or a human supervisor script) spawns a worker CLI in a dedicated pane, sends prompts or answers, waits until the worker either finishes its turn (`idle`) or blocks on an approval/question (`needs_input`), and reads recent output without scraping raw ANSI or maintaining fragile line-count state across scrollback trims.
2. **Addressing panes on remote hosts.** Developers run coding agents on Linux devboxes, cloud VMs, and Windows workstations while observing and steering them from a laptop. Sgian's daemon stays strictly local (no cloud relay, no public TCP socket), so remote access rides client-owned SSH port forwards (`ssh -N -L`) and, on AWS hosts without inbound SSH, Systems Manager Session Manager (#115). Today, every client has to hand-manage `ssh -L` sockets, workspace paths, and per-client credential files separately.

This document specifies the first-principles Sgian surface for both workflows while preserving Sgian's security and coordination invariants: credential-derived attribution, generation-fenced keyboard leases, output-trick guards, and hash-chained per-pane ledgers.

## 2. Agent-to-agent steering primitives

### 2.1 Why raw `send` + `snapshot` is insufficient

A supervisor driving a worker pane with `ctl send` and `ctl snapshot` hits three failure modes:

- **Screen truncation:** `ctl snapshot` renders the visible grid (`rows` lines, typically 24–40). A build log or stack trace that scrolled past row 0 is invisible to `snapshot`, while `ctl lines <PANE> <A>:<B>` previously required the caller to know `total_lines` first.
- **Polling vs. attention transitions:** `ctl wait --idle <MS>` measures byte-stream quiet, not semantic readiness. A model thinking for 12 seconds with no token output looks identical to an idle prompt under `--idle`, causing premature wakeups; conversely, `ctl agent --watch --json` streams every workspace event rather than blocking a single command step until one pane reaches `needs_input` or `idle`.
- **Unfenced concurrent writes:** Two scripts (or an orchestrator and a human) writing with bare `ctl send` collide unless the pane's keyboard lease is held and fenced by lease `generation`.

### 2.2 Canonical steering loop in `sgian ctl`

Sgian addresses all three with composable primitives:

1. **Spawn or reuse a named worker pane:**
   ```bash
   pane_id="$(sgian ctl --json exec --new --name worker-auth -- claude | jq -r '.pane_id')"
   ```
2. **Claim a generation-fenced lease (`--as` bound to the caller's credential when `SGIAN_CLIENT_TOKEN` is set):**
   ```bash
   lease="$(sgian ctl --json lease take "$pane_id" --as lead-agent)"
   gen="$(printf '%s' "$lease" | jq -r '.generation')"
   ```
3. **Wait for semantic attention transitions (`--attention`):**
   ```bash
   sgian ctl --json wait "$pane_id" --attention needs_input,idle --timeout 300000
   ```
   `WaitCondition::Attention(Vec<AgentAttention>)` evaluates the daemon's authoritative attention tracker (`working`, `needs_input`, `idle`) across both PTY-hosted agents (fed by `ctl agent-hook`, the process-tree probe, and the screen classifier) and native `PaneKind::Agent` stream-json sessions. If the pane's process exits before matching, `wait` resolves immediately as `{ "matched": false, "reason": "timeout" }` rather than hanging until `--timeout`.
4. **Read recent scrollback without manual range math (`ctl lines -n` / `--last`):**
   ```bash
   sgian ctl lines "$pane_id" --last 80
   ```
   `DaemonRequest::ScrollbackLines` accepts an optional `last: usize` (`1..=2000`), returning `{ pane_id, total_lines, from, to, lines }` with terminal control sequences stripped and 1-based line numbers aligned to the current scrollback file so citations remain interchangeable with `ctl search` and ledger records.
5. **Send fenced input and release with a mandatory hand-back note:**
   ```bash
   sgian ctl send --pane "$pane_id" --as lead-agent --generation "$gen" "Run the auth integration suite"
   sgian ctl lease release "$pane_id" --as lead-agent --generation "$gen" -m "auth suite green"
   ```
   If a human operator force-takes the pane (`ctl lease take --force --why "..."`) while the orchestrator is thinking, `generation` advances; the orchestrator's next `--generation "$gen"` write fails fast as stale instead of injecting keystrokes into the human's session.

### 2.3 Steering (`lease` + `send`) vs. capability execution (`grant_exec`)

Sgian intentionally separates interactive steering from capability-gated command execution:

| Property | Interactive steering (`lease` + `send` / `exec`) | Execution grants (`grant_exec`..`grant_release`) |
|---|---|---|
| Intended caller | Human operator or peer orchestrator agent | Kranz `TerminalProvider` (`terminal/*`) |
| Process model | Interactive shell or agent TUI (`stdin` open, resizeable) | Verbatim `argv` in a PTY (no shell expansion, filtered `env_allow`) |
| Authorization | Workspace/client token + lease policy (`free` / `guarded` / `strict`) | HMAC-SHA256 capability over canonical JSON verified against `.kranz/serve.grant.key` (`0600`) |
| Human keyboard | Permitted when unleased or force-taken | Held by `kranz:<run>` by default; takeover requires `--force --why` (ledgered) |
| Exit & cleanup receipt | `pane.ended` ledger record + `PaneSnapshot::exit_code` | `ExitStatus { exit_code, signal }` (strict XOR) + `CleanupReceipt { terminal_id, exit_status, descendants_reaped, output_drained, released }` |

## 3. Saved remote workspaces (`machines`) and pane addressing

### 3.1 Configuration schema (user-layer global config)

Saved remote targets represent user-level topology (how *this* client machine reaches a remote workspace), not repository policy. They live in the user's global `config.json` (`machines: Vec<MachineEntry>`):

```json
{
  "machines": [
    {
      "name": "devbox",
      "transport": "ssh",
      "host": "craig@devbox.internal",
      "port": 22,
      "workspace": "/home/craig/src/payments"
    },
    {
      "name": "win-build",
      "transport": "ssh",
      "host": "craig@win-build.internal",
      "workspace": "C:\\Users\\craig\\src\\payments"
    }
  ]
}
```

Validation rules:
- `name`: `1..=64` ASCII `[A-Za-z0-9._-]`, must not start with `-` or `.`, and must not equal `"local"` (reserved for the local workspace). Names are unique across `machines`.
- `transport`: `"ssh"` (default) or `"ssm"` (reserved for AWS Session Manager targets in #115; `target` carries the instance/task identifier when `transport == "ssm"`).
- `host` / `workspace` / `target`: non-empty bounded strings that must never start with `-` (preventing SSH/CLI option injection even before `--` argument delimiters).
- Credentials are **never** stored inline in `config.json`. `ctl` stores per-machine `client_token` secrets in owner-only (`0600`) files under `<app_support_dir>/machines/<name>.token` (or explicit `token_file`), while the native macOS and Windows clients store them in the macOS Keychain (`com.sgian.client-token`, account `machine:<name>`) and Windows Credential Vault / DPAPI (`Sgian/machine/<name>`), falling back to the `0600` token file when present so `ctl machine add` works immediately in both CLI and GUI.

### 3.2 Pane addressing in `sgian ctl`

Every `ctl` invocation targets a single workspace daemon via either:

1. **Explicit global flag:** `sgian ctl --machine <name> <verb> ...` (mutually exclusive with `--workspace`; `SGIAN_MACHINE` provides the environment-variable equivalent for scripts).
2. **Qualified pane shorthand:** `<machine>/<pane>` on single-pane commands (`ctl snapshot devbox/worker-auth`, `ctl lines devbox/pane-2 -n 50`, `ctl wait devbox/worker-auth --attention needs_input,idle`, `ctl send --pane devbox/worker-auth ...`, `ctl lease take devbox/worker-auth ...`).
   - Because valid pane ids (`pane-<N>`) and machine names (`[A-Za-z0-9._-]+`) contain no `/`, `<prefix>/<rest>` splits unambiguously when `<prefix>` matches a configured entry in `machines`.
   - If `--machine <M>` is also passed, `<prefix>` must equal `<M>` or `ctl` fails closed rather than silently routing to a different host.
   - Multi-pane batch flags (`--all`, `--panes a,b`) stay scoped to a single workspace (`--machine <M>` or local) so batch exit codes and leases never span failure domains implicitly.

Management verbs:
- `sgian ctl machine add <NAME> --host <HOST> --workspace <PATH> [--port <PORT>] [--transport ssh|ssm] [--target <ID>] [--token-stdin | --token-file <PATH>]`
- `sgian ctl machine list [--json]`
- `sgian ctl machine remove <NAME>`

### 3.3 Client-owned tunnel lifecycle

Sgian's remote daemon remains unchanged on Unix: it listens only on its local `0700` Unix domain socket. On Windows (#116), where OpenSSH cannot forward named pipes (`\\.\pipe\...`), the remote daemon can opt into a loopback-only TCP listener (`127.0.0.1:<remote_loopback_port>`) that **strictly requires** a per-client `client_token` (`ClientIdentity`) on handshake (refusing bare workspace-token auth because loopback TCP carries no kernel peer-UID/SID guarantee).

To avoid duplicating SSH process management across Rust, Swift, and C#, `sgian ctl --machine <name> --json ipc-endpoint` owns tunnel establishment and prints a standard `NativeIpcEndpoint` pointing at a local Unix domain socket (`<runtime_dir>/machines/<name>/daemon.sock`, `0700` directory, supported natively on macOS, Linux, and Windows 10/11 `AF_UNIX`):

1. Acquire an advisory lock (`<runtime_dir>/machines/<name>/tunnel.lock`) so concurrent `ctl` or GUI calls do not race two tunnels.
2. If `daemon.sock` already exists and passes an authenticated `Ping` handshake with the machine's credential, reuse it immediately.
3. Otherwise, run `ssh -o BatchMode=yes [-p PORT] -- <HOST> sgian ctl --workspace <WORKSPACE> --json ipc-endpoint` to discover the remote daemon's Unix `endpoint` (or `loopback_port` on Windows) without copying `daemon.token` over the wire.
4. Remove any stale local `daemon.sock`, spawn a detached `ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -o StreamLocalBindUnlink=yes -L <local_sock>:<remote_target> [-p PORT] -- <HOST>`, record its PID in `tunnel.pid`, verify the local socket with an authenticated handshake, and return the local `NativeIpcEndpoint`.

Both `apps/macos` and `apps/windows` call `sgian ctl --machine <name> --json ipc-endpoint` just as they call `sgian ctl --workspace <path> --json ipc-endpoint` today, keeping background `Subscribe` streams open per configured machine so sidebar badges (`needs_input`, `working`, `lease_state`, `output_warning`) stay live across local and remote workspaces simultaneously.
