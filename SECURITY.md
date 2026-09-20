# Security

Sgian is a local terminal and agent process manager. Commands run with the
permissions of the logged-in OS account. It does not sandbox shells, agent
providers, repositories, or tools. A workspace is an organizational boundary,
not an isolation boundary against another process running as the same user.

The daemon listens on a local Unix socket or an owner-restricted Windows named
pipe; it does not expose a TCP service. Clients must authenticate before any
command is dispatched. On Unix, runtime and data directories are owner-only
and a connection from another uid is dropped before the hello. Beyond the
workspace token, the daemon issues per-client credentials (`ctl identity`)
with `read`, `write` and `admin` scopes; a credential's holder is fixed by the
daemon, so lease and refusal records attribute actions to a credential rather
than a self-declared name. Only credential hashes are stored. With
`identity: required`, the workspace token can read and administer but every
write needs a credential. Remote access is an SSH-forwarded socket presenting
a credential; the daemon never listens on the network. `sgian ctl serve` is
the one loopback HTTP listener in the binary: it hosts the web client on
`127.0.0.1` for a device on an SSH tunnel, acts as its own process's
credential, is read-only unless `--allow-write` (typing and leases, never
configuration), and requires a per-run session key that it prints once as a
URL and keeps as an `HttpOnly` `SameSite=Strict` cookie, because loopback TCP
has no peer identity and another local account could otherwise reach it.
Tokens, configuration, conversation logs, and scrollback are stored in the
user's platform application-data directory. Conversation logs and terminal
scrollback may contain sensitive information; do not include them in public
bug reports. `ctl diagnostic` produces a reduced support bundle, which should
still be reviewed before sharing.

Agent permissions default to manual approval. More permissive modes are an
explicit operator choice. `scrub_env` removes inherited environment variables
from shell panes, agent panes, and `ctl process`; explicit `env` values take
precedence. This is environment hygiene, not protection against a process that
can read the user's files or credentials. Invalid configuration prevents daemon
startup, and a failed live reload preserves the last valid policy.

The web UI renders agent output as text and a restricted Markdown subset.
The native Windows terminal only accepts bridge messages from its bundled
terminal document and blocks other navigation, frames, downloads, and popups.
Native macOS terminal links are limited to HTTP(S); clipboard access via OSC 52
uses SwiftTerm's deny-by-default delegates.

Updates require the embedded minisign public key. Release CI builds without
restoring compiled Rust caches in signing jobs. Only the final publication job
can write releases, after all platform builds and signature checks succeed.
Validation builds receive no signing secrets. The updater feed is a public
GitHub Releases asset; changing a repository's visibility or deleting its
release assets affects existing installations' ability to update.

## Reporting a vulnerability

Use GitHub's **Security → Report a vulnerability** on
[craigcode/sgian](https://github.com/craigcode/sgian/security/advisories/new)
when private vulnerability reporting is enabled. If that option is unavailable,
open a minimal issue asking the maintainer for a private reporting channel;
keep exploit details, tokens, prompts, and private files out of the issue.
The maintainer must enable private vulnerability reporting before public launch.

Only the latest released version is intended to receive security fixes. No
response-time guarantee is currently offered.

## Audit scope and limitations

See [the release review](docs/release-review-2026-09-04.md). Automated dependency
checks do not cover the OS webview, every reachable function, or the security
of user-selected agent CLIs. Keep the operating system, webview runtime, and
provider CLIs updated. The review is a source review with automated and local
runtime checks, not an independent penetration-test certification.
