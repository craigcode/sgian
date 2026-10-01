# A served read-mostly view over the tunnel (M7)

Status: decided and built, 2026-09-19 (decisions: a `sgian ctl serve`
subcommand, read-only by default with `--allow-write`, assets embedded with
`include_dir`). Decision 4 changed in the build: not `tiny_http`, which
buffers a chunked body behind an 8 KiB writer and only exposes the socket
through an upgrade that writes a terminated body first, so server-sent
events would never reach the page in time; the HTTP loop is std-only (the
alternative listed below), three routes, one request per connection, a
flush per event. Shipped: `serve_http.rs`, `ui/src/web-bridge.js` (fetch +
SSE behind the existing controller), loopback-only with a Host check, CSP
headers, the same event and command contract as the Tauri host; the served
page lands on the overview, which is a stack of cards under 760 px. The text
below is the proposal as written.

## Why

M6 gave a second machine a credential and a transport (an SSH-forwarded
socket) but not a client: the Tauri, SwiftUI and WinUI apps are desktop
bundles, and a phone or a borrowed laptop has neither. The thing those
devices want is the board: which panes need a person, who holds the
keyboard, what the agent was waiting for, the last screenful of a pane,
and the account's rate-limit line. That is a read view with one write, the
lease, which M6 already gates.

Enjoy sells exactly this as "work from your phone or browser". Sgian can
offer it without a cloud, without a listener on the network, and with the
same attribution as the desktop.

## Design

**`sgian serve`**, a subcommand of the existing binary, serves the web
client over HTTP on a loopback port and talks to the daemon over the local
socket like any other client. It never binds a non-loopback address; the
remote device reaches it through `ssh -L 8321:127.0.0.1:8321 desk` and
opens `http://localhost:8321`. There is no TLS to manage because the tunnel
is the transport, and no listener to attack from the LAN.

**Identity.** `serve` presents a client credential (`SGIAN_CLIENT_TOKEN`
or `--identity FILE`) to the daemon; its scopes bound what the page can do.
The page does not carry its own credential: the tunnel is the session. A
`read` credential yields a viewer; a `write` credential also allows
taking the lease and typing. Every action is attributed to the credential's
holder, so the ledger says `craig@phone took the keyboard` with the
credential id, the same as the desktop.

**What is served.** The existing Tauri React client, built as static assets
by Vite, with the bridge swapped: instead of `window.__TAURI__` invoke and
listen, a small `fetch` + Server-Sent-Events bridge to `serve`. The app
already routes every daemon call through one controller with an injected
`nativeInvoke` / `nativeListen` pair (the tests use a fake one), so this is
a third bridge, not a fork. `serve` exposes the same request names and
forwards events, filtering by the credential's scope on the daemon side, as
today. The terminal surface stays xterm.js reading `pty_output`; typing
goes through `send_input` and is refused unless the lease and scope allow.

**Read-mostly by default.** `serve` runs `--read-only` unless told
otherwise, which drops the write paths from the bridge entirely even when
the credential could write. The owner turns writes on for a device by
running `serve --allow-write` with a write credential.

**Pane content.** Scrollback and live output are already what the desktop
receives; the same 16 MiB cap and the same output guard apply. The dossier
is one request away for a reviewer on a laptop. Nothing new is stored.

**Lifecycle.** `serve` is a foreground process (or `ctl serve start|stop`
later); it exits when the tunnel does. No daemon changes are needed for
the first cut: every request it makes already exists.

## What this is not

- Not a listener on the network, not a relay through a cloud, not a
  Kranz feature. The tunnel is the only door and SSH is the only auth.
- Not a second UI. The React client is the UI; `serve` is a host for it.
- Not a mobile app. A phone gets a responsive web page; the board and a
  read-only pane are usable at phone width, a full terminal is not the
  point.

## Alternatives considered

- **Serve from the daemon itself.** Cheaper to reach, but it puts HTTP in
  the process that holds every PTY and the ledger, and it means the daemon
  needs a credential story for itself. A separate process with an ordinary
  credential keeps the trust model unchanged.
- **A native remote client (SwiftUI on iOS).** Best experience, most work,
  and it cannot reach a Windows or Linux desk without the same tunnel. The
  served page works everywhere first; a native client can come after.
- **WebSocket instead of SSE.** Needed only if the page pushes a lot; input
  is small and infrequent, so `fetch` for requests and SSE for events is
  enough and simpler to reason about.

## Cost

`serve` (axum or tiny_http, static assets embedded, SSE fan-out): two days.
The web bridge and a build target for it: one day. Responsive tweaks to
the board and the read-only pane at phone width: one day. Docs and a live
check over a real tunnel: half a day.

## Decisions for the owner

1. Ship `serve` as a subcommand of the `sgian` binary (recommended) or as a
   separate crate (`sgian-serve`, which the reserved crate names allow).
2. Read-only by default with `--allow-write` (recommended), or follow the
   credential's scope with no extra switch.
3. Embed the built web assets in the binary (recommended: one file to
   copy, works over the tunnel with nothing else installed) or serve them
   from a directory next to it.
4. Whether to add an HTTP server dependency now (axum is already in the
   Kranz stack; tiny_http is smaller) or hand-roll a minimal server on
   `std` only. Recommended: tiny_http for the first cut.
