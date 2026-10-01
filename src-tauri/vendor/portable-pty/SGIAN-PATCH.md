# Sgian portable-pty patch

This is `portable-pty` 0.8.1 with one Windows-only behavioral patch.

Upstream calls `CreatePseudoConsole` with the undocumented flags `0x2 | 0x4`
(`PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE`). In contrast,
Microsoft's `node-pty` implementation—the backend used by VS Code—passes `0`
unless cursor inheritance is explicitly requested. Sgian uses the same default
flags as node-pty so ConPTY is not placed in a different resize/input mode.

The changed line is in `src/win/psuedocon.rs`. All other crate source is the
published 0.8.1 package. When upgrading, re-check whether upstream exposes a
supported flag option or has adopted the default `0` behavior; remove this fork
as soon as it is no longer needed.
