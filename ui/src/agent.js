// agent.js — (T2) pure chat-state reducer for agent panes.
//
// An agent pane runs a headless Claude Code process daemon-side; the GUI keeps
// one chat state per pane and folds normalized `agent-event` payloads (see the
// T2 contract: session / message_start / text_delta / message_complete /
// tool_use / tool_result / permission_request / permission_resolved /
// turn_complete / error / process_exit) into it. User messages are appended
// LOCALLY by the composer (appendUserMessage) — the daemon never echoes them
// back as events.
//
// Every daemon event carries a per-pane `seq` (monotonic from 1); the reducer
// tracks `lastSeq` and drops re-delivered events (seq <= lastSeq) so a replay
// tail overlapping the live stream never double-appends (M1). Events without
// a seq (older daemon, locally injected error lines) always apply.
//
// The module is dependency-free (no DOM, no Tauri) so the reducer is fully
// unit-testable. React rendering lives in app.jsx; event wiring in events.js.

// Cap on retained chat messages per pane (M3), mirroring the backend's
// 500-event replay bound. When the cap is crossed the oldest messages are
// dropped and a lightweight { type: "elided" } marker records how many.
export const MAX_CHAT_MESSAGES = 500;

/**
 * Create an empty chat state for an agent pane.
 *
 * @returns {{
 *   sessionId: string|null, model: string|null,
 *   messages: object[], busy: boolean,
 *   pendingPermission: object|null, lastTurn: object|null,
 *   exited: boolean, exitCode: number|null, lastSeq: number,
 * }}
 */
export function createAgentChat() {
  return {
    sessionId: null,
    model: null,
    messages: [],
    busy: false,
    pendingPermission: null,
    lastTurn: null,
    exited: false,
    exitCode: null,
    lastSeq: 0,
  };
}

/**
 * Normalize a tool_result `content` payload into a display string. The CLI
 * emits both shapes: a plain string, or an array of content blocks
 * (`{type:"text", text}` among others). Anything else is JSON-stringified so
 * the renderer never has to guess.
 */
export function normalizeToolContent(content) {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((block) => {
        if (typeof block === "string") return block;
        if (block && typeof block === "object" && typeof block.text === "string") {
          return block.text;
        }
        return JSON.stringify(block);
      })
      .filter((part) => typeof part === "string" && part.length > 0)
      .join("\n");
  }
  if (content == null) return "";
  return JSON.stringify(content);
}

/** Open a fresh assistant message (streaming target for text_delta). */
function openAssistantMessage(chat) {
  const message = { type: "assistant", text: "", open: true };
  chat.messages.push(message);
  return message;
}

/** Close the currently-open assistant message, if any. */
function closeAssistantMessage(chat) {
  const open = chat.messages.find((m) => m.type === "assistant" && m.open);
  if (!open) return false;
  open.open = false;
  return true;
}

/**
 * Keep the newest ~MAX_CHAT_MESSAGES messages (M3). Once the cap is crossed
 * the oldest messages are dropped and a single leading { type: "elided" }
 * marker records the running total of elided history (the marker itself takes
 * one slot, so the capped list holds MAX - 1 real messages + the marker).
 */
function capChatMessages(chat) {
  if (chat.messages.length <= MAX_CHAT_MESSAGES) return;
  const first = chat.messages[0];
  const prior = first?.type === "elided" ? first.count : 0;
  const dropReal = chat.messages.length - (prior ? 1 : 0) - (MAX_CHAT_MESSAGES - 1);
  if (dropReal <= 0) return;
  chat.messages.splice(0, dropReal + (prior ? 1 : 0));
  chat.messages.unshift({ type: "elided", count: prior + dropReal });
}

/**
 * Fold one normalized agent event into the chat state. Mutates and returns
 * `chat`. Unknown kinds are ignored (forward-compat with new event kinds);
 * malformed events are tolerated rather than throwing — a broken stream must
 * not take the workspace down.
 *
 * Seq dedupe (M1): bootstrap wires the live listener BEFORE the snapshot
 * invoke resolves, so a live event can arrive after the replay tail already
 * folded the same event. Dropping seq <= lastSeq kills the double-append;
 * replay itself sets lastSeq to the max applied. Gaps in seq are fine (the
 * log replay trims). Events without a numeric seq always apply.
 */
export function applyAgentEvent(chat, event) {
  if (!chat || !event || typeof event.kind !== "string") return chat;

  if (typeof event.seq === "number" && Number.isFinite(event.seq)) {
    if (event.seq <= (chat.lastSeq ?? 0)) return chat;
    chat.lastSeq = event.seq;
  }

  foldAgentEvent(chat, event);
  capChatMessages(chat);
  return chat;
}

/** The per-kind fold behind applyAgentEvent (seq gate + message cap above). */
function foldAgentEvent(chat, event) {
  switch (event.kind) {
    case "session": {
      if (typeof event.session_id === "string") chat.sessionId = event.session_id;
      if (typeof event.model === "string") chat.model = event.model;
      // A session (re)starts the process: clear any stale exit marker so the
      // "agent exited" hint disappears once the daemon auto-respawns on send.
      chat.exited = false;
      chat.exitCode = null;
      return chat;
    }

    case "message_start": {
      // One open assistant message at a time; a new start closes a straggler.
      closeAssistantMessage(chat);
      openAssistantMessage(chat);
      chat.busy = true;
      return chat;
    }

    case "text_delta": {
      const text = typeof event.text === "string" ? event.text : "";
      if (!text) return chat;
      // Tolerate deltas without an open message by opening one (a replay tail
      // can begin mid-message).
      const open = chat.messages.find((m) => m.type === "assistant" && m.open);
      const target = open ?? openAssistantMessage(chat);
      target.text += text;
      chat.busy = true;
      return chat;
    }

    case "message_complete": {
      closeAssistantMessage(chat);
      return chat;
    }

    case "tool_use": {
      closeAssistantMessage(chat);
      chat.messages.push({
        type: "tool",
        id: event.id ?? null,
        name: typeof event.name === "string" ? event.name : "tool",
        input: event.input ?? null,
        result: null,
        isError: null,
      });
      return chat;
    }

    case "tool_result": {
      const content = normalizeToolContent(event.content);
      const isError = event.is_error === true;
      const toolUseId = event.tool_use_id ?? null;
      // Attach to the matching open tool item (search newest-first; the match
      // is almost always the most recent tool).
      let match = null;
      if (toolUseId != null) {
        for (let i = chat.messages.length - 1; i >= 0; i -= 1) {
          const m = chat.messages[i];
          if (m.type === "tool" && m.id === toolUseId && m.result === null) {
            match = m;
            break;
          }
        }
      }
      if (match) {
        match.result = content;
        match.isError = isError;
      } else {
        // Unknown id: keep the result visible as a standalone item rather
        // than dropping it.
        chat.messages.push({
          type: "tool_result",
          toolUseId,
          content,
          isError,
        });
      }
      return chat;
    }

    case "permission_request": {
      const previous = chat.pendingPermission;
      chat.pendingPermission = {
        requestId: event.request_id ?? null,
        toolName: typeof event.tool_name === "string" ? event.tool_name : "tool",
        input: event.input ?? null,
        // One permission card at a time: a second request replaces the first
        // and is flagged so the UI can note the superseded request.
        replacedRequestId: previous?.requestId ?? null,
      };
      return chat;
    }

    case "permission_resolved": {
      // Emitted whenever a request is answered (user/timeout/closed/
      // process_exit). The backend blocks the reader while a request is
      // pending, so this resolves the CURRENT card — but only when the ids
      // match: a late resolution for a superseded request must not clear the
      // newer card (L4). Replay folds through this naturally, cancelling
      // cards that were answered while the GUI was away (H1).
      const pending = chat.pendingPermission;
      if (!pending) return chat;
      if (event.request_id != null && pending.requestId === event.request_id) {
        chat.pendingPermission = null;
        // Restart/session teardown resolves with reason "closed" and does
        // NOT emit process_exit for the superseded reader generation — clear
        // busy so the composer is not wedged waiting for a turn that will
        // never complete on this session. process_exit also clears busy;
        // user/timeout resolutions leave busy set (the turn continues).
        if (event.reason === "closed" || event.reason === "process_exit") {
          chat.busy = false;
        }
        return chat;
      }
      console.warn(
        `[sgian] ignoring permission_resolved for ${event.request_id ?? "unknown"} ` +
          `(pending is ${pending.requestId ?? "unknown"})`,
      );
      return chat;
    }

    case "turn_complete": {
      chat.busy = false;
      // The reader blocks while a request is pending, so a turn can only
      // complete with no request genuinely live: any card still showing is a
      // zombie (H1).
      chat.pendingPermission = null;
      chat.lastTurn = {
        subtype: typeof event.subtype === "string" ? event.subtype : null,
        costUsd: typeof event.cost_usd === "number" ? event.cost_usd : null,
        durationMs: typeof event.duration_ms === "number" ? event.duration_ms : null,
        numTurns: typeof event.num_turns === "number" ? event.num_turns : null,
      };
      return chat;
    }

    case "error": {
      // Stream-level errors (e.g. an oversized line dropped) are NOT
      // turn-ending, so busy is left untouched.
      chat.messages.push({
        type: "error",
        message: typeof event.message === "string" ? event.message : "agent error",
      });
      return chat;
    }

    case "process_exit": {
      closeAssistantMessage(chat);
      chat.busy = false;
      // Same zombie-card sweep as turn_complete (H1): a dead process has no
      // live permission request.
      chat.pendingPermission = null;
      chat.exited = true;
      chat.exitCode = typeof event.exit_code === "number" ? event.exit_code : null;
      return chat;
    }

    default:
      return chat;
  }
}

/**
 * Fold a replay tail (bootstrap `agent_events`) into the chat state.
 * Tolerates a missing/non-array tail.
 */
export function replayAgentEvents(chat, events) {
  if (!chat || !Array.isArray(events)) return chat;
  for (const event of events) {
    applyAgentEvent(chat, event);
  }
  return chat;
}

/**
 * Append a locally-composed user message and mark the chat busy (a turn is
 * now expected). Called by the composer BEFORE the send_agent_message invoke
 * resolves so the UI disables immediately.
 */
export function appendUserMessage(chat, text) {
  if (!chat) return chat;
  chat.messages.push({ type: "user", text: String(text) });
  capChatMessages(chat);
  chat.busy = true;
  return chat;
}

/**
 * Remove the most recent locally-appended user bubble with this exact text.
 * Used when send_agent_message fails: the phantom bubble is rolled back and
 * the draft is restored to the composer, so no text is lost (M2). Returns
 * true when a bubble was removed.
 */
export function removeUserMessage(chat, text) {
  if (!chat) return false;
  for (let i = chat.messages.length - 1; i >= 0; i -= 1) {
    const m = chat.messages[i];
    if (m.type === "user" && m.text === String(text)) {
      chat.messages.splice(i, 1);
      return true;
    }
  }
  return false;
}

/**
 * Clear the pending permission request after the GUI has answered it via
 * agent_approval. Matches by request_id (L4): never blanket-clear — a newer
 * request may have replaced the one this approval answered. The daemon's own
 * permission_resolved event folds through the reducer and converges on the
 * same state; this local clear just avoids waiting for it.
 */
export function clearAgentPermission(chat, requestId) {
  if (!chat) return chat;
  const pending = chat.pendingPermission;
  if (!pending) return chat;
  if (pending.requestId !== requestId) return chat;
  chat.pendingPermission = null;
  return chat;
}

/**
 * Clamp chat UI state when the daemon reports the pane process has ended
 * (H2). Used on bootstrap/resync for both freshly-seeded and already-mounted
 * chats: after daemon death, subscribe catch-up may emit PaneEnded without a
 * live process_exit, so a mid-turn chat would otherwise stay wedged busy.
 * Leave live panes alone — callers must only invoke this when pane_states
 * says "ended". Returns true when any render-visible state changed so a
 * snapshot caller can decide whether React needs a store notification.
 */
export function clampAgentChatToPaneEnded(chat) {
  if (!chat) return false;
  const assistantClosed = closeAssistantMessage(chat);
  const changed =
    assistantClosed ||
    chat.busy !== false ||
    chat.pendingPermission !== null ||
    chat.exited !== true;
  chat.busy = false;
  chat.pendingPermission = null;
  chat.exited = true;
  return changed;
}
