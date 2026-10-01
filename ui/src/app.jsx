import React, {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { flushSync } from "react-dom";
import { renderMarkdown } from "./markdown.js";
import { resolveSearchKeyAction } from "./search.js";
import { zoomedPaneId } from "./zoom.js";
import {
  groupPanesByProject,
  projectRollup,
  rollupText,
  outputWarningSummary,
  usageText,
  groupLimitText,
} from "./projects.js";
import { buildPaletteCommands, filterPaletteCommands } from "./palette.js";

function paneLabel(pane, index) {
  return pane?.title?.trim() || `pane-${index + 1}`;
}

function formatLastActivity(ms, now = Date.now()) {
  if (!ms) return "—";
  const delta = Math.max(0, now - ms);
  if (delta < 5000) return "just now";
  const seconds = Math.floor(delta / 1000);
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

function agentAttentionLabel(info) {
  if (!info) return "—";
  const attention = info.attention ?? "idle";
  const mode = info.unattended ? ` · ${info.mode} (unattended)` : info.mode ? ` · ${info.mode}` : "";
  if (attention === "needs_input") return `${info.agent} · needs input${mode}`;
  if (attention === "working") return `${info.agent} · working${mode}`;
  return `${info.agent}${mode}`;
}

function AgentBadge({ info }) {
  if (!info) return null;
  const attention = info.attention ?? "idle";
  const base =
    attention === "needs_input"
      ? `${info.agent} needs input`
      : attention === "working"
        ? `${info.agent} is working`
        : info.agent;
  const title = info.unattended
    ? `${base} · ${info.mode}: tools run without your approval`
    : base;
  return (
    <span
      className={`agent-badge agent-badge-${attention}${info.unattended ? " agent-badge-unattended" : ""}`}
      title={title}
      data-mode={info.mode ?? undefined}
    >
      ◆{info.unattended ? " ⚠" : ""}
    </span>
  );
}

function LeaseBadge({ info, holder }) {
  if (!info) return null;
  const mine = Boolean(holder) && info.holder === holder;
  const title = mine ? "You hold this pane's keyboard" : `Keyboard held by ${info.holder}`;
  return (
    <span
      className={`lease-badge${mine ? " lease-badge-mine" : ""}`}
      title={title}
      data-holder={info.holder}
    >
      ⌨ {mine ? "you" : info.holder}
    </span>
  );
}

/**
 * Output-guard badge (docs/design/keyboard-lease-and-ledger.md §7): the pane's
 * output used a trick that hides something from a person (concealed text,
 * clipboard writes, mismatched links, opaque control strings, C1 controls).
 * Counts only ever grow for a pane's life; the title lists them.
 */
/**
 * Usage badge: what the session under this pane last said about itself
 * through the status line (`sgian ctl statusline`): context fill and the
 * tightest rate-limit window. The full line is in the title.
 */
function UsageBadge({ usage }) {
  if (!usage) return null;
  const full = usageText(usage);
  const bits = [];
  if (usage.context !== null) bits.push(`${usage.context}%`);
  const limit = [usage.fiveHour, usage.sevenDay].filter(Boolean).sort((a, b) => b.used - a.used)[0];
  if (limit) bits.push(`${limit.used}% limit`);
  if (bits.length === 0) return null;
  const hot = usage.context >= 80 || (limit && limit.used >= 80);
  return (
    <span
      className={`usage-badge${hot ? " usage-badge-hot" : ""}`}
      title={full}
      data-context={usage.context ?? undefined}
    >
      {bits.join(" · ")}
    </span>
  );
}

function OutputBadge({ warning }) {
  if (!warning) return null;
  return (
    <span
      className="output-badge"
      title={`Output hid something: ${outputWarningSummary(warning)}${warning.sample ? ` · first seen: ${warning.sample}` : ""}`}
      data-total={warning.total}
    >
      ⚠ {warning.total}
    </span>
  );
}

/**
 * Keyboard lease dialog (docs/design/keyboard-lease-and-ledger.md §6 M2):
 * `release` asks for the mandatory hand-back note; `take` asks for the
 * reason when the pane is held by someone else. An empty field cannot submit.
 */
function LeaseDialog({ state, controller }) {
  const dialog = state.leaseDialog;
  const modalRef = useRef(null);
  const fieldRef = useRef(null);
  const [text, setText] = useState("");
  useEffect(() => {
    setText("");
    if (dialog) queueMicrotask(() => fieldRef.current?.focus());
  }, [dialog?.mode, dialog?.paneId]);
  if (!dialog) return null;
  const release = dialog.mode === "release";
  const pane = state.panes.get(dialog.paneId);
  const paneName = pane?.title || dialog.paneId;
  const trimmed = text.trim();
  const submit = () => {
    if (!trimmed) return;
    if (release) void controller.releaseLease(dialog.paneId, trimmed);
    else void controller.takeLease(dialog.paneId, { force: true, why: trimmed });
  };
  const trapFocus = (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      controller.closeLeaseDialog();
      return;
    }
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      submit();
      return;
    }
    if (event.key !== "Tab") return;
    const focusable = Array.from(
      modalRef.current.querySelectorAll(
        'button, input, textarea, select, [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((element) => !element.disabled);
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };
  return (
    <div
      id="lease-overlay"
      className="modal-overlay"
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) controller.closeLeaseDialog();
      }}
    >
      <div
        ref={modalRef}
        id="lease-dialog"
        className="modal lease-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="lease-dialog-title"
        onKeyDown={trapFocus}
      >
        <header className="modal-header">
          <h2 id="lease-dialog-title">
            {release
              ? `Hand back the keyboard for ${paneName}`
              : `Take the keyboard for ${paneName} from ${dialog.heldBy || "its holder"}`}
          </h2>
          <button
            className="modal-close"
            type="button"
            title="Close"
            aria-label="Close lease dialog"
            onClick={controller.closeLeaseDialog}
          >
            ×
          </button>
        </header>
        <form
          className="lease-form"
          onSubmit={(event) => {
            event.preventDefault();
            submit();
          }}
        >
          <label className="lease-label" htmlFor="lease-text">
            {release
              ? "Hand-back note (required): what you did, what the agent should do next"
              : "Why are you taking it over? (required, recorded in the ledger)"}
          </label>
          <textarea
            id="lease-text"
            ref={fieldRef}
            className="lease-text"
            rows={release ? 4 : 2}
            value={text}
            onChange={(event) => setText(event.target.value)}
          />
          {dialog.error ? (
            <p className="lease-error" role="alert">
              {dialog.error}
            </p>
          ) : null}
          <div className="lease-actions">
            <button type="button" className="secondary" onClick={controller.closeLeaseDialog}>
              Cancel
            </button>
            <button type="submit" className="primary" disabled={!trimmed}>
              {release ? "Release keyboard" : "Take keyboard"}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}

function UpdateBanner({ version, controller }) {
  if (!version) return null;
  return (
    <div id="update-banner" className="update-banner">
      <button
        type="button"
        className="update-banner-button"
        onClick={() => void controller.installUpdate()}
      >
        Update to v{version} and restart
      </button>
      <button
        type="button"
        className="update-banner-dismiss"
        title="Dismiss"
        aria-label="Dismiss update"
        onClick={controller.dismissUpdate}
      >
        ×
      </button>
    </div>
  );
}

function Toolbar({ state, controller }) {
  const attentionPaneIds = Array.from(state.agentStates)
    .filter(
      ([paneId, info]) =>
        info?.attention === "needs_input" &&
        paneId !== state.activePaneId &&
        state.panes.has(paneId),
    )
    .map(([paneId]) => paneId);
  return (
    <div className="toolbar" aria-label="Pane actions">
      <UpdateBanner version={state.updateVersion} controller={controller} />
      <button
        id="split-right"
        className="icon-button"
        type="button"
        title="Split right"
        aria-label="Split right"
        onClick={() => void controller.splitActive("row")}
      >
        |
      </button>
      <button
        id="split-down"
        className="icon-button"
        type="button"
        title="Split down"
        aria-label="Split down"
        onClick={() => void controller.splitActive("column")}
      >
        -
      </button>
      <select
        id="profile-select"
        className="profile-select"
        aria-label="Profile for new panes"
        title="Profile for new panes"
        value={state.selectedProfile ?? ""}
        onChange={(event) => controller.setSelectedProfile(event.target.value || null)}
      >
        <option value="">None</option>
        {state.profiles.map((profile) => (
          <option key={profile.name} value={profile.name}>
            {profile.name}
          </option>
        ))}
      </select>
      <button
        id="new-agent"
        className="icon-button text-button"
        type="button"
        title="New agent pane"
        aria-label="New agent pane"
        onClick={() => void controller.newAgentPane()}
      >
        Agent
      </button>
      <select
        id="new-agent-backend"
        className="agent-provider-select"
        aria-label="Agent backend"
        title="Backend for new agent panes"
        value={state.newAgentBackend}
        onChange={(event) => controller.setNewAgentBackend(event.target.value)}
      >
        <option value="claude">Claude</option>
        <option value="droid">Droid</option>
      </select>
      <input
        id="new-agent-model"
        className="agent-model-input"
        type="text"
        aria-label="Agent model"
        title="Optional model id for new agent panes"
        placeholder="default model"
        value={state.newAgentModel}
        onChange={(event) => controller.setNewAgentModel(event.target.value)}
      />
      <button
        id="close-pane"
        className="icon-button danger"
        type="button"
        title="Close pane"
        aria-label="Close pane"
        onClick={() => void controller.closeActive()}
      >
        x
      </button>
      <button
        id="open-overview"
        className="icon-button text-button"
        type="button"
        title="Session overview"
        aria-label="Session overview"
        aria-haspopup="dialog"
        onClick={controller.openOverview}
      >
        Overview
      </button>
      <button
        id="open-settings"
        className="icon-button"
        type="button"
        title="Settings"
        aria-label="Settings"
        aria-haspopup="dialog"
        onClick={controller.openSettingsModal}
      >
        ⚙
      </button>
      {attentionPaneIds.length > 0 && (
        <button
          type="button"
          className="agent-attention-badge"
          title="Focus pane"
          onClick={() => controller.focusPane(attentionPaneIds[0])}
        >
          ◆ {attentionPaneIds.length} agent
          {attentionPaneIds.length === 1 ? "" : "s"} need
          {attentionPaneIds.length === 1 ? "s" : ""} input
        </button>
      )}
    </div>
  );
}

function PaneTabs({ state, controller }) {
  return (
    <div id="pane-tabs" className="pane-tabs">
      {Array.from(state.panes.values()).map((pane, index) => (
        <button
          key={pane.id}
          className="pane-tab"
          type="button"
          data-active={pane.id === state.activePaneId}
          data-runtime={controller.paneRuntimeState(pane.id)}
          title={pane.title}
          onClick={() => controller.focusPane(pane.id)}
        >
          <span className="pane-tab-number">{index + 1}</span>
          <strong>{paneLabel(pane, index)}</strong>
          <AgentBadge info={state.agentStates.get(pane.id)} />
          <OutputBadge warning={state.outputWarnings?.get(pane.id)} />
        </button>
      ))}
    </div>
  );
}

function RenameInput({ pane, controller }) {
  const [value, setValue] = useState(pane.title);
  const inputRef = useRef(null);
  const settled = useRef(false);
  useLayoutEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);
  const finish = (commit, rawValue = value) => {
    if (settled.current) return;
    settled.current = true;
    if (commit) void controller.commitRename(pane.id, rawValue);
    else controller.cancelRename();
  };
  return (
    <input
      ref={inputRef}
      className="pane-title-input"
      type="text"
      value={value}
      aria-label="Rename pane"
      onChange={(event) => setValue(event.target.value)}
      onKeyDown={(event) => {
        event.stopPropagation();
        if (event.key === "Enter") {
          event.preventDefault();
          finish(true, event.currentTarget.value);
        } else if (event.key === "Escape") {
          event.preventDefault();
          finish(false);
        }
      }}
      onBlur={(event) => finish(true, event.currentTarget.value)}
    />
  );
}

function TerminalSurface({ paneId, title, ended, available, controller }) {
  const hostRef = useRef(null);
  useLayoutEffect(() => {
    if (!available) return undefined;
    const host = hostRef.current;
    controller.terminals.mount(paneId, host);
    return () => controller.terminals.detach(paneId, host);
  }, [available, controller, paneId]);
  return (
    <div
      ref={hostRef}
      className="terminal terminal-xterm"
      data-pane-id={paneId}
      role="group"
      aria-label={`Terminal ${title || paneId}`}
      onPointerDown={() => controller.terminals.focus(paneId)}
    >
      {!available && (
        <div className="terminal-unavailable" role="alert">
          <strong>Terminal engine unavailable</strong>
          <span>Shell rendering is disabled, but agent panes remain available.</span>
        </div>
      )}
      {ended && (
        <div className="pane-restore">
          <span>session ended</span>
          <button
            className="pane-restore-button"
            type="button"
            onClick={(event) => {
              event.stopPropagation();
              controller.focusPane(paneId);
              void controller.restartPane(paneId);
            }}
          >
            restart
          </button>
        </div>
      )}
    </div>
  );
}

function Markdown({ text }) {
  const ref = useRef(null);
  useLayoutEffect(() => {
    ref.current?.replaceChildren(renderMarkdown(text));
  }, [text]);
  return <span ref={ref} />;
}

function formatPayload(value) {
  if (typeof value === "string") return value;
  if (value == null) return "";
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function Payload({ label, value, error = false }) {
  return (
    <div className="chat-tool-section">
      <div className="chat-tool-label">{label}</div>
      <pre className={`chat-tool-pre${error ? " chat-tool-pre-error" : ""}`}>
        {formatPayload(value)}
      </pre>
    </div>
  );
}

function ChatMessage({ message, index }) {
  if (message.type === "user") {
    return (
      <div className="chat-msg chat-msg-user">
        <div className="chat-bubble">{message.text}</div>
      </div>
    );
  }
  if (message.type === "assistant") {
    return (
      <div className={`chat-msg chat-msg-assistant${message.open ? " chat-msg-open" : ""}`}>
        <div className="chat-bubble">
          {message.text ? <Markdown text={message.text} /> : message.open ? <span className="chat-stream-hint">…</span> : null}
        </div>
      </div>
    );
  }
  if (message.type === "elided") {
    return <div className="chat-elided">… {message.count} earlier messages elided …</div>;
  }
  if (message.type === "tool") {
    return (
      <details className="chat-tool" data-tool-key={message.id != null ? `id:${message.id}` : `idx:${index}`}>
        <summary className="chat-tool-summary">
          <span className="chat-tool-name">{message.name}</span>
          {message.result === null ? (
            <span className="chat-tool-status">running…</span>
          ) : message.isError ? (
            <span className="chat-tool-error">error</span>
          ) : null}
        </summary>
        <Payload label="input" value={message.input} />
        {message.result !== null && (
          <Payload label="result" value={message.result} error={message.isError} />
        )}
      </details>
    );
  }
  if (message.type === "tool_result") {
    return (
      <div className="chat-toolresult-standalone">
        <div className="chat-tool-label">tool result</div>
        <Payload label="content" value={message.content} error={message.isError} />
      </div>
    );
  }
  return <div className="chat-error">{message.message ?? "agent error"}</div>;
}

function PermissionCard({ paneId, permission, controller }) {
  const [denying, setDenying] = useState(false);
  const [reason, setReason] = useState("");
  const inputRef = useRef(null);
  useEffect(() => {
    setDenying(false);
    setReason("");
  }, [permission.requestId]);
  useLayoutEffect(() => {
    if (denying) inputRef.current?.focus();
  }, [denying]);
  return (
    <div className="chat-permission">
      <div className="chat-permission-title">{permission.toolName} wants permission</div>
      {permission.replacedRequestId && (
        <div className="chat-permission-note">replaced an earlier request</div>
      )}
      <Payload label="input" value={permission.input} />
      {permission.requestId == null ? (
        <div className="chat-permission-note">
          waiting on the agent — this request cannot be answered here
        </div>
      ) : (
        <>
          <div className="chat-permission-actions">
            <button
              type="button"
              className="chat-allow"
              onClick={() => controller.sendAgentApproval(paneId, permission.requestId, true, null)}
            >
              Allow
            </button>
            <button type="button" className="chat-deny" onClick={() => setDenying(true)}>
              Deny
            </button>
          </div>
          {denying && (
            <div className="chat-deny-form">
              <input
                ref={inputRef}
                className="chat-deny-input"
                type="text"
                placeholder="Reason (optional)"
                value={reason}
                onChange={(event) => setReason(event.target.value)}
              />
              <button
                type="button"
                className="chat-deny-send"
                onClick={() => {
                  const message = (inputRef.current?.value ?? reason).trim();
                  setDenying(false);
                  setReason("");
                  controller.sendAgentApproval(
                    paneId,
                    permission.requestId,
                    false,
                    message || null,
                  );
                }}
              >
                Send
              </button>
              <button
                type="button"
                className="chat-deny-cancel"
                onClick={() => {
                  setDenying(false);
                  setReason("");
                }}
              >
                Cancel
              </button>
            </div>
          )}
        </>
      )}
    </div>
  );
}

function turnFooterText(lastTurn) {
  const parts = [];
  if (lastTurn?.subtype) parts.push(lastTurn.subtype);
  if (typeof lastTurn?.costUsd === "number") parts.push(`$${lastTurn.costUsd.toFixed(4)}`);
  if (typeof lastTurn?.durationMs === "number") {
    parts.push(`${(lastTurn.durationMs / 1000).toFixed(1)}s`);
  }
  return parts.join(" · ");
}

function AgentChat({ paneId, state, controller }) {
  const rootRef = useRef(null);
  const listRef = useRef(null);
  const textareaRef = useRef(null);
  const nearBottom = useRef(true);
  const chat = state.chats.get(paneId);
  const draft = state.chatDrafts.get(paneId) ?? "";
  const busy = chat?.busy === true;
  useLayoutEffect(() => {
    controller.registerChatView(paneId, {
      root: rootRef.current,
      textarea: textareaRef.current,
    });
    return () => controller.registerChatView(paneId, null);
  }, [controller, paneId]);
  useLayoutEffect(() => {
    if (nearBottom.current && listRef.current) {
      listRef.current.scrollTop = listRef.current.scrollHeight;
    }
  });
  const send = (rawValue = textareaRef.current?.value) => {
    const text = String(rawValue || "").trim();
    // Read the controller's current chat state rather than the render closure:
    // several normalized agent events can land in one animation frame.
    if (!text || controller.state.chats.get(paneId)?.busy) return;
    nearBottom.current = true;
    controller.sendAgentMessage(paneId, text);
  };
  return (
    <div ref={rootRef} className="chat-root" data-pane-id={paneId}>
      <div
        ref={listRef}
        className="chat-messages"
        onScroll={(event) => {
          const target = event.currentTarget;
          nearBottom.current =
            target.scrollHeight - target.scrollTop - target.clientHeight < 40;
        }}
      >
        {!chat || (chat.messages.length === 0 && !chat.exited) ? (
          <div className="chat-empty">No messages yet — send one to start the agent.</div>
        ) : (
          chat.messages.map((message, index) => (
            <ChatMessage
              key={`${message.type}:${message.id ?? message.toolUseId ?? index}`}
              message={message}
              index={index}
            />
          ))
        )}
        {chat?.pendingPermission && (
          <PermissionCard
            key={chat.pendingPermission.requestId ?? "informational"}
            paneId={paneId}
            permission={chat.pendingPermission}
            controller={controller}
          />
        )}
        {chat?.lastTurn && !busy && turnFooterText(chat.lastTurn) && (
          <div className="chat-turn-footer">{turnFooterText(chat.lastTurn)}</div>
        )}
        {chat?.exited && (
          <div className="chat-exited">agent exited — send a message to restart it</div>
        )}
      </div>
      <div className="chat-composer">
        <textarea
          ref={textareaRef}
          className="chat-input"
          rows={2}
          placeholder="Message the agent…"
          aria-label="Message the agent"
          value={draft}
          disabled={busy}
          onChange={(event) => controller.setChatDraft(paneId, event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              event.stopPropagation();
              send(event.currentTarget.value);
            }
          }}
        />
        <button
          type="button"
          className={`chat-send${busy ? " chat-interrupt" : ""}`}
          onClick={() =>
            controller.state.chats.get(paneId)?.busy
              ? controller.interruptAgent(paneId)
              : send()
          }
        >
          {busy ? "Interrupt" : "Send"}
        </button>
      </div>
    </div>
  );
}

function Pane({ paneId, state, controller }) {
  const pane = state.panes.get(paneId) || { id: paneId, title: paneId, kind: "shell" };
  const index = Array.from(state.panes.keys()).indexOf(paneId);
  const runtime = controller.paneRuntimeState(paneId);
  const zoomed = controller.isZoomed() && zoomedPaneId(state.zoom) === paneId;
  const renaming = state.renamingPaneId === paneId;
  const agentSpec = state.agentSpecs.get(paneId);
  const paneMeta =
    pane.kind === "agent"
      ? [agentSpec?.backend || "claude", agentSpec?.model].filter(Boolean).join(" · ")
      : pane.kind;
  return (
    <article
      className="pane"
      data-active={paneId === state.activePaneId}
      data-pane-id={paneId}
      data-runtime={runtime}
      data-zoomed={zoomed}
      tabIndex={0}
      onFocus={(event) => {
        if (event.target === event.currentTarget) controller.focusPane(paneId);
      }}
      onPointerDown={() => controller.focusPane(paneId)}
    >
      <header className="pane-header">
        {renaming ? (
          <RenameInput pane={pane} controller={controller} />
        ) : (
          <button
            className="pane-title"
            type="button"
            title="Focus pane"
            onClick={() => controller.focusPane(paneId)}
          >
            {paneLabel(pane, index)}
          </button>
        )}
        <button
          className="pane-rename"
          type="button"
          title="Rename pane"
          aria-label="Rename pane"
          onClick={(event) => {
            event.stopPropagation();
            controller.focusPane(paneId);
            controller.beginRename(paneId);
          }}
        >
          ✎
        </button>
        <AgentBadge info={state.agentStates.get(paneId)} />
        <OutputBadge warning={state.outputWarnings?.get(paneId)} />
        <UsageBadge usage={state.agentUsage?.get(paneId)} />
        <LeaseBadge info={state.leases.get(paneId)} holder={state.holder} />
        <span className="pane-meta">
          {runtime === "ended" ? `ended · ${paneMeta}` : paneMeta}
        </span>
      </header>
      {state.leaseToast?.paneId === paneId ? (
        <div className="lease-toast" role="status">
          {state.leaseToast.message}
        </div>
      ) : null}
      {pane.kind === "agent" ? (
        <div className="chat-container" data-pane-id={paneId}>
          <AgentChat paneId={paneId} state={state} controller={controller} />
        </div>
      ) : (
        <TerminalSurface
          paneId={paneId}
          title={paneLabel(pane, index)}
          ended={runtime === "ended"}
          available={state.terminalEngineAvailable !== false}
          controller={controller}
        />
      )}
    </article>
  );
}

function SplitNode({ node, state, controller }) {
  const splitRef = useRef(null);
  const dragCleanup = useRef(null);
  useEffect(() => () => dragCleanup.current?.(), []);
  if (node.type === "leaf") {
    return <Pane paneId={node.id} state={state} controller={controller} />;
  }

  const resizeByKeyboard = (event) => {
    const step = event.shiftKey ? 0.1 : 0.02;
    let delta = 0;
    if (node.direction === "row") {
      if (event.key === "ArrowLeft") delta = -step;
      else if (event.key === "ArrowRight") delta = step;
    } else if (event.key === "ArrowUp") delta = -step;
    else if (event.key === "ArrowDown") delta = step;
    if (delta === 0) return;
    event.preventDefault();
    controller.setSplitRatio(node, node.ratio + delta);
  };

  const startDrag = (event) => {
    event.preventDefault();
    const element = splitRef.current;
    const bounds = element.getBoundingClientRect();
    element.setPointerCapture?.(event.pointerId);
    controller.setDragging(true);
    const move = (moveEvent) => {
      const raw =
        node.direction === "row"
          ? (moveEvent.clientX - bounds.left) / bounds.width
          : (moveEvent.clientY - bounds.top) / bounds.height;
      controller.setSplitRatio(node, raw, { persist: false });
    };
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      dragCleanup.current = null;
      controller.setDragging(false);
      void controller.persistWorkspaceLayout();
    };
    dragCleanup.current = stop;
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
  };

  return (
    <div
      ref={splitRef}
      className={`split split-${node.direction}`}
      style={{ "--first-size": `${node.ratio * 100}%` }}
      data-split-id={node.id}
    >
      <SplitNode node={node.first} state={state} controller={controller} />
      <div
        className="divider"
        role="separator"
        aria-orientation={node.direction === "row" ? "vertical" : "horizontal"}
        aria-valuemin={18}
        aria-valuemax={82}
        aria-valuenow={Math.round(node.ratio * 100)}
        aria-label="Resize split"
        tabIndex={0}
        onPointerDown={startDrag}
        onKeyDown={resizeByKeyboard}
      />
      <SplitNode node={node.second} state={state} controller={controller} />
    </div>
  );
}

function SearchBar({ state, controller }) {
  const inputRef = useRef(null);
  useLayoutEffect(() => {
    if (state.search.open) {
      inputRef.current?.focus();
      inputRef.current?.select();
    }
  }, [state.search.open]);
  const query = () => inputRef.current?.value ?? state.search.query;
  return (
    <div id="search-bar" className="search-bar" hidden={!state.search.open}>
      <input
        ref={inputRef}
        id="search-input"
        className="search-input"
        type="text"
        placeholder="Search scrollback"
        autoComplete="off"
        aria-label="Search scrollback"
        value={state.search.query}
        onChange={(event) => controller.updateSearchQuery(event.target.value)}
        onKeyDown={(event) => {
          const action = resolveSearchKeyAction(event);
          if (!action) return;
          event.preventDefault();
          if (action.type === "search-next") controller.searchNext(event.currentTarget.value);
          else if (action.type === "search-previous") controller.searchPrevious(event.currentTarget.value);
          else controller.closeSearchBar();
        }}
      />
      <button
        id="search-prev"
        className="search-button"
        type="button"
        title="Previous match (Shift+Enter)"
        aria-label="Previous match"
        onClick={() => {
          inputRef.current?.focus();
          controller.searchPrevious(query());
        }}
      >
        ↑
      </button>
      <button
        id="search-next"
        className="search-button"
        type="button"
        title="Next match (Enter)"
        aria-label="Next match"
        onClick={() => {
          inputRef.current?.focus();
          controller.searchNext(query());
        }}
      >
        ↓
      </button>
      <button
        id="search-close"
        className="search-button close"
        type="button"
        title="Close search (Esc)"
        aria-label="Close search"
        onClick={controller.closeSearchBar}
      >
        ×
      </button>
    </div>
  );
}

const settingsFields = [
  ["shell", "cfg-shell"],
  ["shell_args", "cfg-shell-args"],
  ["env", "cfg-env"],
  ["font_family", "cfg-font-family"],
  ["font_size", "cfg-font-size"],
  ["theme", "cfg-theme"],
  ["profiles", "cfg-profiles"],
  ["idle_shutdown_secs", "cfg-idle-shutdown"],
  ["restore_policy", "cfg-restore-policy"],
];

function formValues(form) {
  return Object.fromEntries(settingsFields.map(([key, id]) => [key, form.elements.namedItem(key)?.value ?? form.querySelector(`#${id}`)?.value ?? ""]));
}

function SettingsField({ name, label, error, children }) {
  return (
    <label
      className="form-field"
      data-invalid={error ? "true" : undefined}
      data-error={error || undefined}
    >
      <span className="form-label">{label}</span>
      {children}
    </label>
  );
}

function CommandPalette({ state, controller }) {
  const modalRef = useRef(null);
  const inputRef = useRef(null);
  const [query, setQuery] = useState("");
  const [selectedIndex, setSelectedIndex] = useState(0);
  const commands = buildPaletteCommands(controller);
  const filtered = filterPaletteCommands(commands, query);
  const clampedIndex = filtered.length === 0 ? 0 : Math.min(selectedIndex, filtered.length - 1);
  const activeOptionId =
    filtered.length > 0 ? `palette-option-${filtered[clampedIndex].id}` : undefined;

  useLayoutEffect(() => {
    if (!state.paletteOpen) return;
    setQuery("");
    setSelectedIndex(0);
    queueMicrotask(() => {
      if (controller.state.paletteOpen) inputRef.current?.focus();
    });
  }, [state.paletteOpen, controller]);

  useEffect(() => {
    setSelectedIndex(0);
  }, [query]);

  const runSelected = () => {
    const command = filtered[clampedIndex];
    if (!command) return;
    controller.closePalette();
    command.run();
  };

  const trapFocus = (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      controller.closePalette();
      return;
    }
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (filtered.length === 0) return;
      setSelectedIndex((index) => (index + 1) % filtered.length);
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      if (filtered.length === 0) return;
      setSelectedIndex((index) => (index - 1 + filtered.length) % filtered.length);
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      runSelected();
      return;
    }
    if (event.key !== "Tab") return;
    const focusable = Array.from(
      modalRef.current.querySelectorAll(
        'button, input, textarea, select, [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((element) => !element.disabled);
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  return (
    <div
      id="palette-overlay"
      className="modal-overlay palette-overlay"
      hidden={!state.paletteOpen}
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) controller.closePalette();
      }}
    >
      <div
        ref={modalRef}
        id="command-palette"
        className="modal palette-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="palette-title"
        onKeyDown={trapFocus}
      >
        <header className="modal-header">
          <h2 id="palette-title">Command palette</h2>
          <button
            id="palette-close"
            className="modal-close"
            type="button"
            title="Close"
            aria-label="Close command palette"
            onClick={controller.closePalette}
          >
            ×
          </button>
        </header>
        <div className="palette-body">
          <input
            ref={inputRef}
            id="palette-input"
            className="palette-input"
            type="text"
            role="combobox"
            placeholder="Type a command…"
            autoComplete="off"
            aria-label="Filter commands"
            aria-autocomplete="list"
            aria-controls="palette-listbox"
            aria-expanded={state.paletteOpen}
            aria-activedescendant={activeOptionId}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown" || event.key === "ArrowUp" || event.key === "Enter") {
                trapFocus(event);
              }
            }}
          />
          <ul id="palette-listbox" className="palette-list" role="listbox" aria-label="Commands">
            {filtered.length === 0 ? (
              <li className="palette-empty">No matching commands</li>
            ) : (
              filtered.map((command, index) => (
                <li key={command.id} role="presentation">
                  <button
                    type="button"
                    id={`palette-option-${command.id}`}
                    className="palette-item"
                    role="option"
                    tabIndex={-1}
                    aria-selected={index === clampedIndex}
                    data-selected={index === clampedIndex ? "true" : undefined}
                    onMouseEnter={() => setSelectedIndex(index)}
                    onClick={() => {
                      setSelectedIndex(index);
                      runSelected();
                    }}
                  >
                    <span className="palette-item-label">{command.label}</span>
                  </button>
                </li>
              ))
            )}
          </ul>
        </div>
      </div>
    </div>
  );
}

function SessionOverview({ state, controller }) {
  const modalRef = useRef(null);
  const now = Date.now();

  useLayoutEffect(() => {
    if (!state.overviewOpen) return;
    queueMicrotask(() => {
      if (controller.state.overviewOpen) {
        modalRef.current?.querySelector("button")?.focus();
      }
    });
  }, [state.overviewOpen, controller]);

  const trapFocus = (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      controller.closeOverview();
      return;
    }
    if (event.key !== "Tab") return;
    const focusable = Array.from(
      modalRef.current.querySelectorAll(
        'button, input, textarea, select, [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((element) => !element.disabled);
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  const panes = Array.from(state.panes.values());
  const groups = groupPanesByProject(state.panes, state.projects ?? new Map());
  const paneIndex = new Map(panes.map((pane, index) => [pane.id, index]));
  const holderText = (lease) => {
    if (!lease) return "—";
    return state.holder && lease.holder === state.holder ? "you" : lease.holder;
  };
  const renderPaneRow = (pane) => {
    const index = paneIndex.get(pane.id) ?? 0;
    const runtime = controller.paneRuntimeState(pane.id);
    const agentInfo = state.agentStates.get(pane.id);
    const lastActivity = state.lastActivityMs.get(pane.id);
    const warning = state.outputWarnings?.get(pane.id);
    const usage = state.agentUsage?.get(pane.id);
    const canClose = state.panes.size > 1;
    return (
      <tr
        key={pane.id}
        data-pane-id={pane.id}
        data-active={pane.id === state.activePaneId ? "true" : undefined}
      >
        <td className="overview-pane" data-label="Pane">{paneLabel(pane, index)}</td>
        <td data-label="Kind">{pane.kind}</td>
        <td data-label="Runtime">{runtime}</td>
        <td data-label="Agent">{agentAttentionLabel(agentInfo)}</td>
        <td className="overview-keyboard" data-label="Keyboard">{holderText(state.leases.get(pane.id))}</td>
        <td
          className={warning ? "overview-output overview-output-warning" : "overview-output"}
          data-label="Output"
        >
          {warning ? `⚠ ${outputWarningSummary(warning)}` : "—"}
        </td>
        <td className="overview-usage" data-label="Usage">{usage ? usageText(usage) : "—"}</td>
        <td data-label="Activity">{formatLastActivity(lastActivity, now)}</td>
        <td className="overview-actions" data-label="Actions">
          <button
            type="button"
            className="overview-action"
            onClick={() => {
              const narrow = window.matchMedia?.("(max-width: 760px)")?.matches;
              if (narrow) controller.focusPaneZoomed(pane.id);
              else controller.focusPane(pane.id);
              controller.closeOverview();
            }}
          >
            Focus
          </button>
          {pane.kind !== "agent" && (
            <button
              type="button"
              className="overview-action"
              onClick={() => void controller.restartPane(pane.id)}
            >
              Restart
            </button>
          )}
          <button
            type="button"
            className="overview-action danger"
            disabled={!canClose}
            onClick={() => {
              void controller.closePane(pane.id).then(() => {
                queueMicrotask(() => {
                  if (!controller.state.overviewOpen) return;
                  modalRef.current
                    ?.querySelector(
                      'button:not([disabled]), [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
                    )
                    ?.focus();
                });
              });
            }}
          >
            Close
          </button>
        </td>
      </tr>
    );
  };

  return (
    <div
      id="overview-overlay"
      className="modal-overlay overview-overlay"
      hidden={!state.overviewOpen}
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) controller.closeOverview();
      }}
    >
      <div
        ref={modalRef}
        id="session-overview"
        className="modal overview-modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="overview-title"
        onKeyDown={trapFocus}
      >
        <header className="modal-header">
          <h2 id="overview-title">Session overview</h2>
          <button
            id="overview-close"
            className="modal-close"
            type="button"
            title="Close"
            aria-label="Close session overview"
            onClick={controller.closeOverview}
          >
            ×
          </button>
        </header>
        <div className="overview-body">
          {panes.length === 0 ? (
            <p className="overview-empty">No panes in this workspace.</p>
          ) : (
            <table className="overview-table">
              <thead>
                <tr>
                  <th scope="col">Pane</th>
                  <th scope="col">Kind</th>
                  <th scope="col">Runtime</th>
                  <th scope="col">Agent</th>
                  <th scope="col">Keyboard</th>
                  <th scope="col">Output</th>
                  <th scope="col">Usage</th>
                  <th scope="col">Activity (this attach)</th>
                  <th scope="col">Actions</th>
                </tr>
              </thead>
              {groups.map((group) => {
                const rollup = projectRollup(group.panes, state);
                const key = group.name ?? "\u0000unassigned";
                const showHeading = group.name !== null || groups.length > 1;
                return (
                  <tbody
                    key={key}
                    className="overview-group"
                    data-project={group.name ?? undefined}
                  >
                    {showHeading && (
                      <tr className="overview-group-row">
                        <th scope="rowgroup" colSpan={9}>
                          <span className="overview-group-name">
                            {group.name ?? "No project"}
                          </span>
                          {group.goal && (
                            <span className="overview-group-goal" title={group.goal}>
                              {group.goal}
                            </span>
                          )}
                          <span className="overview-group-rollup">{rollupText(rollup)}</span>
                          {groupLimitText(group.panes, state) && (
                            <span className="overview-group-limit">
                              {groupLimitText(group.panes, state)}
                            </span>
                          )}
                        </th>
                      </tr>
                    )}
                    {group.panes.map(renderPaneRow)}
                  </tbody>
                );
              })}
            </table>
          )}
        </div>
      </div>
    </div>
  );
}

function SettingsDialog({ state, controller }) {
  const modalRef = useRef(null);
  useLayoutEffect(() => {
    if (!state.settingsModalOpen) return;
    queueMicrotask(() => {
      if (controller.state.settingsModalOpen) {
        modalRef.current?.querySelector("button, input, textarea, select")?.focus();
      }
    });
  }, [state.settingsModalOpen, controller]);
  const values = state.settingsValues;
  const update = (key) => (event) => controller.updateSetting(key, event.target.value);
  const trapFocus = (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      controller.closeSettingsModal();
      return;
    }
    if (event.key !== "Tab") return;
    const focusable = Array.from(
      modalRef.current.querySelectorAll(
        'button, input, textarea, select, [tabindex]:not([tabindex="-1"])',
      ),
    ).filter((element) => !element.disabled);
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable.at(-1);
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };
  return (
    <div
      id="settings-overlay"
      className="modal-overlay"
      hidden={!state.settingsModalOpen}
      onPointerDown={(event) => {
        if (event.target === event.currentTarget) controller.closeSettingsModal();
      }}
    >
      <div
        ref={modalRef}
        id="settings-modal"
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-title"
        onKeyDown={trapFocus}
      >
        <header className="modal-header">
          <h2 id="settings-title">Settings</h2>
          <button
            id="settings-close"
            className="modal-close"
            type="button"
            title="Close"
            aria-label="Close settings"
            onClick={controller.closeSettingsModal}
          >
            ×
          </button>
        </header>
        <form
          id="settings-form"
          className="modal-form"
          onSubmit={(event) => {
            event.preventDefault();
            void controller.saveSettings(formValues(event.currentTarget));
          }}
        >
          <SettingsField name="shell" label="Shell" error={state.settingsErrors.shell}>
            <input id="cfg-shell" name="shell" type="text" placeholder="/bin/bash" autoComplete="off" value={values.shell} onChange={update("shell")} />
          </SettingsField>
          <SettingsField name="shell_args" label="Shell args (one per line)" error={state.settingsErrors.shell_args}>
            <textarea id="cfg-shell-args" name="shell_args" rows={2} placeholder="-l" value={values.shell_args} onChange={update("shell_args")} />
          </SettingsField>
          <SettingsField name="env" label="Environment (KEY=VALUE per line)" error={state.settingsErrors.env}>
            <textarea id="cfg-env" name="env" rows={3} placeholder="FOO=bar" value={values.env} onChange={update("env")} />
          </SettingsField>
          <SettingsField name="font_family" label="Font family" error={state.settingsErrors.font_family}>
            <input id="cfg-font-family" name="font_family" type="text" placeholder="Menlo" autoComplete="off" value={values.font_family} onChange={update("font_family")} />
          </SettingsField>
          <SettingsField name="font_size" label="Font size" error={state.settingsErrors.font_size}>
            <input id="cfg-font-size" name="font_size" type="number" min="1" placeholder="12" autoComplete="off" value={values.font_size} onChange={update("font_size")} />
          </SettingsField>
          <SettingsField name="theme" label="Theme (JSON object)" error={state.settingsErrors.theme}>
            <textarea id="cfg-theme" name="theme" rows={4} placeholder='{"background":"#000"}' value={values.theme} onChange={update("theme")} />
          </SettingsField>
          <SettingsField name="profiles" label="Profiles (JSON array)" error={state.settingsErrors.profiles}>
            <textarea
              id="cfg-profiles"
              name="profiles"
              rows={4}
              placeholder='[{"name":"dev","kind":"shell","shell":"/bin/bash"}]'
              value={values.profiles}
              onChange={update("profiles")}
            />
          </SettingsField>
          <SettingsField name="idle_shutdown_secs" label="Idle shutdown (seconds, 0 = disabled)" error={state.settingsErrors.idle_shutdown_secs}>
            <input id="cfg-idle-shutdown" name="idle_shutdown_secs" type="number" min="0" placeholder="0" autoComplete="off" value={values.idle_shutdown_secs} onChange={update("idle_shutdown_secs")} />
          </SettingsField>
          <SettingsField name="restore_policy" label="Restore policy" error={state.settingsErrors.restore_policy}>
            <select id="cfg-restore-policy" name="restore_policy" value={values.restore_policy} onChange={update("restore_policy")}>
              <option value="">(default)</option>
              <option value="auto_respawn">auto_respawn</option>
              <option value="restore_on_demand">restore_on_demand</option>
            </select>
          </SettingsField>
          <div id="settings-error" className="modal-error" role="alert" hidden={!state.settingsError}>
            {state.settingsError}
          </div>
          <footer className="modal-footer">
            <button id="settings-cancel" className="modal-button" type="button" onClick={controller.closeSettingsModal}>Cancel</button>
            <button id="settings-save" className="modal-button primary" type="submit" disabled={state.settingsLoadFailed || !state.settingsLoaded}>Save</button>
          </footer>
        </form>
      </div>
    </div>
  );
}

export function App({ controller }) {
  // Tauri events originate outside React's event system. Flush their external
  // store notification synchronously so the DOM and controller snapshot never
  // expose different pane/agent states to the next native event.
  const subscribe = useCallback(
    (callback) => controller.subscribe(() => flushSync(callback)),
    [controller],
  );
  useSyncExternalStore(subscribe, controller.getSnapshot, controller.getSnapshot);
  const state = controller.state;

  useEffect(() => {
    // Starting can resolve an injected/browser bridge synchronously. Defer it
    // past React's effect commit so an external-store notification never tries
    // to flush while React is still running lifecycle work.
    let mounted = true;
    queueMicrotask(() => {
      if (mounted) controller.start();
    });
    const onKeyDown = (event) => controller.handleGlobalKey(event);
    const onResize = () => controller.terminals.resizeVisible();
    const onError = (event) => controller.handleGlobalError(event.error || event.message);
    const onRejection = (event) => controller.handleGlobalError(event.reason);
    window.addEventListener("keydown", onKeyDown, true);
    window.addEventListener("resize", onResize);
    window.addEventListener("error", onError);
    window.addEventListener("unhandledrejection", onRejection);
    return () => {
      mounted = false;
      window.removeEventListener("keydown", onKeyDown, true);
      window.removeEventListener("resize", onResize);
      window.removeEventListener("error", onError);
      window.removeEventListener("unhandledrejection", onRejection);
      controller.stop();
    };
  }, [controller]);

  useLayoutEffect(() => {
    queueMicrotask(controller.focusActiveSurface);
  }, [
    controller,
    state.activePaneId,
    state.ready,
    state.renamingPaneId,
  ]);

  const active = state.panes.get(state.activePaneId);
  const activeState = active ? controller.paneRuntimeState(active.id) : null;
  const zoom = controller.isZoomed() ? zoomedPaneId(state.zoom) : undefined;
  return (
    <>
      <main id="app" className="app-shell" data-ready={state.ready}>
        <header className="topbar">
          <div className="brand">
            <div className="brand-mark" aria-hidden="true">S2</div>
            <div>
              <h1>Sgian</h1>
              <p id="workspace-cwd">{state.cwd}</p>
            </div>
          </div>
          <Toolbar state={state} controller={controller} />
        </header>
        <aside className="rail" aria-label="Panes">
          <PaneTabs state={state} controller={controller} />
        </aside>
        <section className="workbench" aria-label="Pane workbench">
          <div id="layout-root" className="layout-root" data-zoom={zoom}>
            {state.layout && (
              <SplitNode node={state.layout} state={state} controller={controller} />
            )}
          </div>
        </section>
        <footer className="statusbar">
          <span id="pane-count">{state.panes.size} pane{state.panes.size === 1 ? "" : "s"}</span>
          <span id="active-pane">
            {active ? `${paneLabel(active, 0)}${activeState === "ended" ? " ended" : ""}` : "no pane"}
          </span>
          <span id="boot-status">{state.bootStatus}</span>
        </footer>
      </main>
      <SearchBar state={state} controller={controller} />
      <CommandPalette state={state} controller={controller} />
      <SessionOverview state={state} controller={controller} />
      <SettingsDialog state={state} controller={controller} />
      <LeaseDialog state={state} controller={controller} />
    </>
  );
}
