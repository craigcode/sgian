// agent.test.js — unit tests for the (T2) chat-state reducer in agent.js.
//
// Covers the full turn lifecycle, tool pairing (both content shapes),
// permission request set/replace/clear, error + process_exit handling, replay
// equivalence, and busy-flag transitions.

import { describe, it, expect } from "vitest";
import {
  createAgentChat,
  applyAgentEvent,
  replayAgentEvents,
  appendUserMessage,
  clearAgentPermission,
  clampAgentChatToPaneEnded,
  normalizeToolContent,
  MAX_CHAT_MESSAGES,
} from "../ui/src/agent.js";

const SESSION = { kind: "session", session_id: "s-1", model: "claude-x" };

/** A complete text turn: start → deltas → complete → turn_complete. */
function textTurnEvents(text) {
  return [
    { kind: "message_start", role: "assistant" },
    { kind: "text_delta", text },
    { kind: "message_complete" },
    { kind: "turn_complete", subtype: "success", cost_usd: 0.01, duration_ms: 1200 },
  ];
}

describe("createAgentChat", () => {
  it("starts empty and idle", () => {
    expect(createAgentChat()).toEqual({
      sessionId: null,
      model: null,
      messages: [],
      busy: false,
      pendingPermission: null,
      lastTurn: null,
      exited: false,
      exitCode: null,
      lastSeq: 0,
    });
  });
});

describe("applyAgentEvent — session", () => {
  it("records session id and model", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, SESSION);
    expect(chat.sessionId).toBe("s-1");
    expect(chat.model).toBe("claude-x");
  });

  it("clears the exited marker on a new session (auto-respawn)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "process_exit", exit_code: 1 });
    expect(chat.exited).toBe(true);
    applyAgentEvent(chat, SESSION);
    expect(chat.exited).toBe(false);
    expect(chat.exitCode).toBeNull();
  });
});

describe("applyAgentEvent — full turn lifecycle", () => {
  it("streams text into one assistant message and ends the turn", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, SESSION);
    expect(chat.busy).toBe(false);

    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    expect(chat.busy).toBe(true);
    expect(chat.messages).toEqual([{ type: "assistant", text: "", open: true }]);

    applyAgentEvent(chat, { kind: "text_delta", text: "Hello" });
    applyAgentEvent(chat, { kind: "text_delta", text: ", world" });
    expect(chat.messages[0].text).toBe("Hello, world");
    expect(chat.messages[0].open).toBe(true);

    applyAgentEvent(chat, { kind: "message_complete" });
    expect(chat.messages[0].open).toBe(false);
    expect(chat.busy).toBe(true); // still busy until turn_complete

    applyAgentEvent(chat, {
      kind: "turn_complete",
      subtype: "success",
      cost_usd: 0.0123,
      duration_ms: 2400,
      num_turns: 1,
    });
    expect(chat.busy).toBe(false);
    expect(chat.lastTurn).toEqual({
      subtype: "success",
      costUsd: 0.0123,
      durationMs: 2400,
      numTurns: 1,
    });
    // The complete text is NOT re-emitted: the message is the folded deltas.
    expect(chat.messages[0].text).toBe("Hello, world");
  });

  it("tolerates text_delta without an open message by opening one", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "text_delta", text: "orphan" });
    expect(chat.messages).toEqual([{ type: "assistant", text: "orphan", open: true }]);
    expect(chat.busy).toBe(true);
  });

  it("a second message_start closes the previous open message", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    applyAgentEvent(chat, { kind: "text_delta", text: "one" });
    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    applyAgentEvent(chat, { kind: "text_delta", text: "two" });
    expect(chat.messages[0]).toEqual({ type: "assistant", text: "one", open: false });
    expect(chat.messages[1]).toEqual({ type: "assistant", text: "two", open: true });
  });

  it("message_complete with nothing open is a no-op", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "message_complete" });
    expect(chat.messages).toEqual([]);
  });

  it("turn_complete without usage/cost records nulls", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "turn_complete", subtype: "error_during_execution" });
    expect(chat.busy).toBe(false);
    expect(chat.lastTurn).toEqual({
      subtype: "error_during_execution",
      costUsd: null,
      durationMs: null,
      numTurns: null,
    });
  });
});

describe("applyAgentEvent — tool use + result pairing", () => {
  it("appends a tool item and attaches the result by id", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, {
      kind: "tool_use",
      id: "toolu_1",
      name: "Bash",
      input: { command: "ls" },
    });
    expect(chat.messages[0]).toEqual({
      type: "tool",
      id: "toolu_1",
      name: "Bash",
      input: { command: "ls" },
      result: null,
      isError: null,
    });

    applyAgentEvent(chat, {
      kind: "tool_result",
      tool_use_id: "toolu_1",
      content: "file.txt",
      is_error: false,
    });
    expect(chat.messages[0].result).toBe("file.txt");
    expect(chat.messages[0].isError).toBe(false);
    expect(chat.messages).toHaveLength(1); // no standalone item
  });

  it("attaches block-array content (the other contract shape)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "tool_use", id: "toolu_2", name: "Read", input: {} });
    applyAgentEvent(chat, {
      kind: "tool_result",
      tool_use_id: "toolu_2",
      content: [{ type: "text", text: "line one" }, { type: "text", text: "line two" }],
      is_error: true,
    });
    expect(chat.messages[0].result).toBe("line one\nline two");
    expect(chat.messages[0].isError).toBe(true);
  });

  it("pairs the newest unmatched tool when several are open", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "tool_use", id: "a", name: "Bash", input: {} });
    applyAgentEvent(chat, { kind: "tool_use", id: "b", name: "Bash", input: {} });
    applyAgentEvent(chat, { kind: "tool_result", tool_use_id: "a", content: "done" });
    expect(chat.messages[0].result).toBe("done");
    expect(chat.messages[1].result).toBeNull();
  });

  it("appends a standalone result item for an unknown tool_use_id", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, {
      kind: "tool_result",
      tool_use_id: "ghost",
      content: "late result",
      is_error: false,
    });
    expect(chat.messages).toEqual([
      {
        type: "tool_result",
        toolUseId: "ghost",
        content: "late result",
        isError: false,
      },
    ]);
  });
});

describe("normalizeToolContent", () => {
  it("passes strings through", () => {
    expect(normalizeToolContent("plain")).toBe("plain");
  });

  it("joins text blocks and stringifies non-text blocks", () => {
    expect(
      normalizeToolContent([{ type: "text", text: "a" }, { type: "image", data: 1 }]),
    ).toBe('a\n{"type":"image","data":1}');
  });

  it("handles null and odd values", () => {
    expect(normalizeToolContent(null)).toBe("");
    expect(normalizeToolContent(42)).toBe("42");
  });
});

describe("applyAgentEvent — permission requests", () => {
  it("sets the pending permission", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, {
      kind: "permission_request",
      request_id: "req-1",
      tool_name: "Bash",
      input: { command: "rm -rf /" },
    });
    expect(chat.pendingPermission).toEqual({
      requestId: "req-1",
      toolName: "Bash",
      input: { command: "rm -rf /" },
      replacedRequestId: null,
    });
  });

  it("a second request replaces the first and is flagged", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-2", tool_name: "Write" });
    expect(chat.pendingPermission.requestId).toBe("req-2");
    expect(chat.pendingPermission.toolName).toBe("Write");
    expect(chat.pendingPermission.replacedRequestId).toBe("req-1");
  });

  it("clearAgentPermission clears it by matching request_id (the approval path)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    clearAgentPermission(chat, "req-1");
    expect(chat.pendingPermission).toBeNull();
  });

  it("clearAgentPermission never blanket-clears a newer request (L4)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-2", tool_name: "Write" });
    // The late approval for the superseded req-1 must leave req-2's card up.
    clearAgentPermission(chat, "req-1");
    expect(chat.pendingPermission.requestId).toBe("req-2");
    clearAgentPermission(chat, "req-2");
    expect(chat.pendingPermission).toBeNull();
  });

  it("clampAgentChatToPaneEnded un-wedges busy + pending permission (H2)", () => {
    const chat = createAgentChat();
    appendUserMessage(chat, "hi");
    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    expect(chat.busy).toBe(true);
    expect(chat.pendingPermission).not.toBeNull();
    expect(chat.messages.at(-1).open).toBe(true);
    expect(clampAgentChatToPaneEnded(chat)).toBe(true);
    expect(chat.busy).toBe(false);
    expect(chat.pendingPermission).toBeNull();
    expect(chat.exited).toBe(true);
    expect(chat.messages.at(-1).open).toBe(false);
    expect(clampAgentChatToPaneEnded(chat)).toBe(false);
  });
});

describe("applyAgentEvent — permission_resolved (H1)", () => {
  it("clears the pending card when the request ids match", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    applyAgentEvent(chat, {
      kind: "permission_resolved",
      request_id: "req-1",
      behavior: "allow",
      reason: "user",
    });
    expect(chat.pendingPermission).toBeNull();
  });

  it("clears on daemon-side resolutions too (timeout/closed/process_exit)", () => {
    for (const reason of ["timeout", "closed", "process_exit"]) {
      const chat = createAgentChat();
      chat.busy = true;
      applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
      applyAgentEvent(chat, { kind: "permission_resolved", request_id: "req-1", behavior: "deny", reason });
      expect(chat.pendingPermission, `reason ${reason}`).toBeNull();
      // closed/process_exit end the session turn; timeout denies the tool
      // but the turn continues, so busy stays set.
      if (reason === "timeout") {
        expect(chat.busy, `reason ${reason}`).toBe(true);
      } else {
        expect(chat.busy, `reason ${reason}`).toBe(false);
      }
    }
  });

  it("a resolution for a superseded request keeps the newer card (out-of-order by-id)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-2", tool_name: "Write" });
    applyAgentEvent(chat, { kind: "permission_resolved", request_id: "req-1", behavior: "deny", reason: "closed" });
    expect(chat.pendingPermission.requestId).toBe("req-2");
    expect(chat.pendingPermission.toolName).toBe("Write");
  });

  it("is a no-op when nothing is pending", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_resolved", request_id: "req-1", behavior: "allow", reason: "user" });
    expect(chat.pendingPermission).toBeNull();
    expect(chat.messages).toEqual([]);
  });

  it("replay through permission_resolved cancels a card answered while away", () => {
    const chat = replayAgentEvents(createAgentChat(), [
      { kind: "permission_request", request_id: "req-1", tool_name: "Bash", seq: 1 },
      { kind: "permission_resolved", request_id: "req-1", behavior: "allow", reason: "user", seq: 2 },
    ]);
    expect(chat.pendingPermission).toBeNull();
  });

  it("turn_complete clears a zombie card (H1)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    applyAgentEvent(chat, { kind: "turn_complete", subtype: "success" });
    expect(chat.pendingPermission).toBeNull();
    expect(chat.busy).toBe(false);
  });

  it("process_exit clears a zombie card (H1)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "permission_request", request_id: "req-1", tool_name: "Bash" });
    applyAgentEvent(chat, { kind: "process_exit", exit_code: 1 });
    expect(chat.pendingPermission).toBeNull();
    expect(chat.exited).toBe(true);
  });
});

describe("applyAgentEvent — seq dedupe + lastSeq (M1)", () => {
  it("tracks lastSeq as events apply in order", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "message_start", seq: 1 });
    applyAgentEvent(chat, { kind: "text_delta", text: "a", seq: 2 });
    applyAgentEvent(chat, { kind: "text_delta", text: "b", seq: 3 });
    expect(chat.lastSeq).toBe(3);
    expect(chat.messages[0].text).toBe("ab");
  });

  it("drops a re-delivered event (seq <= lastSeq) instead of double-appending", () => {
    const chat = createAgentChat();
    // Replay tail folds seq 1..3 (sets lastSeq)…
    replayAgentEvents(chat, [
      { kind: "message_start", seq: 1 },
      { kind: "text_delta", text: "hello", seq: 2 },
      { kind: "message_complete", seq: 3 },
    ]);
    expect(chat.lastSeq).toBe(3);
    // …then the live stream delivers the SAME delta (the replay/live race).
    applyAgentEvent(chat, { kind: "text_delta", text: "hello", seq: 2 });
    expect(chat.messages).toHaveLength(1);
    expect(chat.messages[0].text).toBe("hello");
    // A genuinely new event still applies; gaps in seq are fine.
    applyAgentEvent(chat, { kind: "text_delta", text: " world", seq: 7 });
    expect(chat.messages[1].text).toBe(" world");
    expect(chat.lastSeq).toBe(7);
  });

  it("tolerates missing seq (older daemon) by falling back to always-apply", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "text_delta", text: "x", seq: 4 });
    applyAgentEvent(chat, { kind: "text_delta", text: "y" }); // no seq
    applyAgentEvent(chat, { kind: "text_delta", text: "z", seq: null });
    expect(chat.messages[0].text).toBe("xyz");
    // Unsequenced events must not disturb the seq bookkeeping.
    expect(chat.lastSeq).toBe(4);
  });

  it("does not let an unsequenced error line block later sequenced events", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "text_delta", text: "a", seq: 5 });
    applyAgentEvent(chat, { kind: "error", message: "local line" });
    applyAgentEvent(chat, { kind: "text_delta", text: "b", seq: 6 });
    expect(chat.messages[0].text).toBe("ab");
    expect(chat.lastSeq).toBe(6);
  });
});

describe("message cap (M3)", () => {
  /** Push n error lines (one message per event) into a fresh chat. */
  function chatWithMessages(n) {
    const chat = createAgentChat();
    for (let i = 1; i <= n; i += 1) {
      applyAgentEvent(chat, { kind: "error", message: `m${i}` });
    }
    return chat;
  }

  it("keeps every message at exactly the cap", () => {
    const chat = chatWithMessages(MAX_CHAT_MESSAGES);
    expect(chat.messages).toHaveLength(MAX_CHAT_MESSAGES);
    expect(chat.messages[0]).toEqual({ type: "error", message: "m1" });
  });

  it("drops the oldest and marks elided history once the cap is crossed", () => {
    const chat = chatWithMessages(MAX_CHAT_MESSAGES + 1);
    expect(chat.messages).toHaveLength(MAX_CHAT_MESSAGES);
    expect(chat.messages[0]).toEqual({ type: "elided", count: 2 });
    // The newest messages are preserved at the tail.
    expect(chat.messages.at(-1)).toEqual({ type: "error", message: `m${MAX_CHAT_MESSAGES + 1}` });
  });

  it("accumulates the elided count instead of stacking markers", () => {
    const chat = chatWithMessages(MAX_CHAT_MESSAGES + 10);
    expect(chat.messages).toHaveLength(MAX_CHAT_MESSAGES);
    const markers = chat.messages.filter((m) => m.type === "elided");
    expect(markers).toHaveLength(1);
    // 501st push elides 2 (one slot for the marker), each further push 1 more.
    expect(markers[0].count).toBe(11);
    expect(chat.messages.at(-1).message).toBe(`m${MAX_CHAT_MESSAGES + 10}`);
  });

  it("caps locally-appended user messages too", () => {
    const chat = chatWithMessages(MAX_CHAT_MESSAGES);
    appendUserMessage(chat, "over the cap");
    expect(chat.messages).toHaveLength(MAX_CHAT_MESSAGES);
    expect(chat.messages[0].type).toBe("elided");
    expect(chat.messages.at(-1)).toEqual({ type: "user", text: "over the cap" });
  });
});

describe("applyAgentEvent — error and process_exit", () => {
  it("appends an error item without ending the turn", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    applyAgentEvent(chat, { kind: "error", message: "oversized agent output line dropped" });
    expect(chat.messages[1]).toEqual({
      type: "error",
      message: "oversized agent output line dropped",
    });
    expect(chat.busy).toBe(true); // stream-level errors are not turn-ending
  });

  it("process_exit marks exited, closes the stream, and ends the turn", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "message_start", role: "assistant" });
    applyAgentEvent(chat, { kind: "text_delta", text: "partial" });
    applyAgentEvent(chat, { kind: "process_exit", exit_code: 2 });
    expect(chat.exited).toBe(true);
    expect(chat.exitCode).toBe(2);
    expect(chat.busy).toBe(false);
    expect(chat.messages[0].open).toBe(false);
  });
});

describe("appendUserMessage", () => {
  it("appends a user message locally and marks busy", () => {
    const chat = createAgentChat();
    appendUserMessage(chat, "fix the bug");
    expect(chat.messages).toEqual([{ type: "user", text: "fix the bug" }]);
    expect(chat.busy).toBe(true);
  });
});

describe("replayAgentEvents", () => {
  it("folding a recorded stream equals applying it live", () => {
    const events = [
      SESSION,
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "Hello" },
      { kind: "text_delta", text: "!" },
      { kind: "message_complete" },
      { kind: "tool_use", id: "toolu_1", name: "Bash", input: { command: "ls" } },
      { kind: "tool_result", tool_use_id: "toolu_1", content: "ok", is_error: false },
      { kind: "turn_complete", subtype: "success", cost_usd: 0.02, duration_ms: 900 },
    ];

    const live = createAgentChat();
    for (const event of events) applyAgentEvent(live, event);

    const replayed = replayAgentEvents(createAgentChat(), events);
    expect(replayed).toEqual(live);
  });

  it("restores an in-flight turn from a mid-stream tail", () => {
    const chat = replayAgentEvents(createAgentChat(), [
      SESSION,
      { kind: "message_start", role: "assistant" },
      { kind: "text_delta", text: "still streaming" },
    ]);
    expect(chat.busy).toBe(true);
    expect(chat.messages[0]).toEqual({
      type: "assistant",
      text: "still streaming",
      open: true,
    });
  });

  it("tolerates a missing or malformed tail", () => {
    const chat = createAgentChat();
    expect(replayAgentEvents(chat, null)).toBe(chat);
    expect(replayAgentEvents(chat, "junk")).toBe(chat);
    expect(replayAgentEvents(chat, [null, { noKind: true }, { kind: 42 }])).toBe(chat);
    expect(chat.messages).toEqual([]);
  });

  it("ignores unknown event kinds (forward-compat)", () => {
    const chat = createAgentChat();
    applyAgentEvent(chat, { kind: "thinking", text: "hmm" });
    expect(chat.messages).toEqual([]);
    expect(chat.busy).toBe(false);
  });
});
