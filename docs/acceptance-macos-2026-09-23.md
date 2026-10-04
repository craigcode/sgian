# macOS native client: hands-on acceptance, 2026-09-23

A pass over the packaged native macOS app with a real Claude Code session,
mirroring the acceptance the roadmap requires on Windows
(`ENHANCEMENTS.md` §1) and the installed-acceptance list in
`docs/native-release.md`. The app was the ad-hoc-signed build from
`scripts/build-macos-native.sh` at `main` `03d8e66`, driven through the
accessibility tree with cua-driver so the operator's own windows never lost
focus; keystrokes were posted to the app's process. Apple Silicon Mac mini,
Claude Code 2.1.278.

## Result

Everything the client is responsible for passed. Four findings, none a
release blocker, are listed at the end with a severity each.

| Step | Result |
| --- | --- |
| Launch, daemon spawn for the workspace, first terminal | Pass |
| Typing into a shell pane | Pass |
| `claude` in a shell pane: trust prompt, one prompt, streamed reply | Pass |
| Attention badge: idle, working while replying, idle after | Pass, both in `ctl agent` and the client |
| Permission mode read off the screen (`auto`), flagged unattended | Pass in the daemon; see finding 3 for the live client |
| Claude's TUI at five sizes: full pane, quarter after three splits, zoomed, unzoomed, after reattach | Pass, no staircased borders, no lost first column |
| Three `Split Down` operations | Pass, panes `term-2`..`term-4` created and listed |
| Pane zoom and unzoom | Pass |
| 32 concurrent `ctl panes` while the client was attached | Pass, all answered in 0.11 s total, daemon responsive after |
| Detach (quit the client) | Pass: daemon, four panes and the Claude session all survived |
| Reattach (relaunch) | Pass: four panes listed, Claude's scrollback restored, badge correct |
| Switch to a second workspace with its own `term-1`, type, switch back | Pass: each workspace showed only its own pane and content, nothing crossed |

Not covered by this pass and still owed by a human on a real install:
resizing the window frame by dragging (the automation could not reach the
window server's resize edge; pane sizes were exercised through splits and
zoom instead), sleep and wake, copy and paste and search, Factory Droid
agent panes, VoiceOver, Gatekeeper and DMG install, and the Sparkle update.

## Findings

1. **Stale saved workspace blocks launch** (minor). The app had a saved
   workspace path from an earlier verify run under `/tmp` that no longer
   existed. Launch showed a modal "Workspace does not exist" alert and,
   after dismissing it, an empty window with a busy indicator; the only
   way forward is the sidebar's Change workspace button. Falling back to
   the workspace picker, or to the most recent existing entry in
   `recentWorkspaces`, would be kinder.
2. **Output guard flags every Claude Code start** (should fix before
   launch). The moment `claude` started, the pane showed the eye-slash
   badge "Output hid something: 1 opaque control strings". Claude Code's
   TUI sends a terminal query at startup that the guard counts as a
   DCS/APC/PM/SOS string. Since every agent session trips it, the badge
   will be tuned out. The guard should recognise the known startup
   queries of supported agent TUIs, or the badge should say what was seen.
3. **Unattended badge is late on a live attach** (minor). With the client
   attached from the start, the pane header showed `auto` and no shield
   while `ctl agent` already reported `unattended: true`. After a detach
   and reattach the header read "auto · unattended" with the shield. The
   live path that applies a mode change appears to skip the unattended
   flag that the snapshot path applies.
4. **The terminal is invisible to accessibility** (verify with VoiceOver).
   The terminal view exposes only an `AXScrollBar`; the rendered text and
   the input area are absent from the accessibility tree, so an AX text
   insertion landed nowhere and a screen reader would have nothing to
   read. The sidebar, toolbar and pane headers are fully exposed. This is
   the item to check first in the assistive-technology pass.

One observation that is not a defect: handing a folder to the app through
`application(_:open:)` did not select it as the workspace. If that is
meant to work, it does not; if not, nothing to do.

## Follow-up

All four findings were fixed the same day and re-checked live against the
rebuilt app: the terminal now appears as an `AXTextArea` labelled by pane
title with the visible screen as its value; a Claude Code start produces no
warning because the guard recognises terminal capability traffic (the
XTVERSION reply SwiftTerm sends, echoed by the tty before the app goes raw,
was the string it counted); the header reads "auto · unattended" on the
first live attach because the daemon's `agent_state` event now carries
`unattended`; and a saved workspace that no longer exists is forgotten at
launch in favour of the most recent one that does, with a Choose Workspace
button in the empty state. VoiceOver itself has still not been run.

## Hook and status-line lanes (2026-09-24)

The first pass drove the badge from screen scraping and the official probe
only, because this machine's Claude settings carry no `sgian ctl hook` or
`sgian ctl statusline` entries. A second run layered them in with
`claude --settings <file>` (the file held the four hook entries and the
status-line command from the README, with the bundled helper's full path)
in a daemon pane, and the native client attached afterwards.

- After the prompt was submitted the ledger held `attention.changed` idle →
  working and working → idle with `evidence: hook`, ahead of the screen
  classifier.
- `ctl agent --json` carried `usage` with the model, a 1,000,000-token
  context window and the session id from the first status-line call, and
  after the turn 5% context, the five-hour and seven-day windows with their
  reset times, and the cost. The pane's own status line rendered the
  compact default ("Fable 5.1 · 5% context · 5h 3% ↻ 2h14m · 7d 21% ↻
  20h24m") and the client's sidebar row showed the same text.
- Not exercised: a `Notification` hook (`permission_prompt` needs a mode
  that asks, `idle_prompt` needs a minute of idleness), so the needs-input
  badge from a hook and the `hook.received` ledger record still rest on
  the unit tests.

A later session (2026-10-04) exercised the two paths left open above. In a
session started with `--permission-mode default`, asking Claude to run the
tests raised a permission prompt: the pane went to needs input and the
ledger recorded `hook.received` for the `permission_prompt` notification,
"Claude needs your permission". A minute of idleness produced the
`idle_prompt` notification the same way. Every lane has now been seen
working end to end.

One note for anyone scripting a pass with `ctl send`: it does not add Enter
for you. End the text with the `\n` escape, or with a real line feed, and
it is sent as a carriage return, which is what both a shell and Claude
Code's input treat as submit. An earlier version of this note blamed paste
detection for a prompt that sat unsubmitted; the real causes were a literal
line feed, which `ctl send` then passed through unchanged and which Claude
reads as a new line, and separately a login refresh that several Claude
Code processes started at once had collided on.

## Procedure

The same sequence, for the Windows pass or for repeating this one by hand.

1. Build and launch the packaged app. Point it at an empty workspace with
   one marker file (on macOS `SGIAN_WORKSPACE=<dir>` before launch; the
   pass above set the `workspacePath` default instead because the driver
   cannot set environment variables).
2. In `term-1` run `ls`, then `claude`. Accept the trust prompt. Send one
   short prompt. Watch the header badge move idle, working, idle, and
   confirm with `sgian ctl --workspace <dir> --json agent` at each step.
   Note the mode and unattended flag in both places.
3. Split three times. Confirm `ctl panes` lists four live panes and that
   Claude's TUI in the shrunken pane has straight borders and no missing
   first column. Zoom and unzoom the Claude pane.
4. Resize the window frame by dragging, several times, with the TUI
   showing. Look for staircased borders and lost characters.
5. From another terminal, run 32 `ctl panes` calls at once. All must
   succeed and the client must stay responsive.
6. Quit the client. Confirm with `ctl agent` and `ctl panes` that the
   session and panes are still live. Relaunch. Confirm all panes and
   Claude's scrollback are back and the badge is right.
7. Switch to a second workspace with its own `term-1`. Type there. Switch
   back. Neither content nor pending input may cross.
8. Exit Claude, quit, `ctl shutdown` both workspaces, and delete the
   workspaces' state under the app data directory.
