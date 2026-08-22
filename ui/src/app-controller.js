import {
  leaf,
  split,
  replaceLeaf,
  pruneLeaf,
  firstPaneId,
  layoutLeafIds,
  layoutMatchesPanes,
  defaultLayoutForPanes,
  makeUniqueId,
  reconcileLayout,
  swapLeaves,
  clampRatio,
  isValidLayoutNode,
} from "./layout.js";
import { resolveKeyAction } from "./keyboard.js";
import { resolveDirectionalFocus, resolveFocusIndex } from "./focus.js";
import { mergeAppearance } from "./config.js";
import {
  setPaneRuntimeState,
  paneRuntimeState,
  paneIsEnded,
  normalizeAgentState,
  agentStateEquals,
  handlePtyOutput,
  handlePaneEnded,
  handlePaneCreated,
  handlePaneClosed,
  handlePaneRenamed,
  handleAgentState,
  handleAgentEvent,
} from "./events.js";
import {
  createAgentChat,
  replayAgentEvents,
  applyAgentEvent,
  appendUserMessage,
  removeUserMessage,
  clearAgentPermission,
  clampAgentChatToPaneEnded,
} from "./agent.js";
import {
  populateFormFromConfig,
  validateSettingsForm,
  serializeSettingsForm,
  settingsPassthroughConfig,
} from "./settings.js";
import {
  activeSearchAddon,
  findNext,
  findPrevious,
  createSearchState,
  openSearch,
  closeSearch,
} from "./search.js";
import { activeHasSelection, copySelection, pasteClipboard } from "./clipboard.js";
import {
  createZoomState,
  isZoomed,
  toggleZoom,
  syncZoomWithActive,
  handleZoomedPaneClosed,
  swapPartner,
} from "./zoom.js";
import { getTauriInvoke, getTauriListen, withTimeout } from "./tauri.js";
import { capBufferedOutput, createTerminalController } from "./terminal-controller.js";

const BOOTSTRAP_TIMEOUT_MS = 15_000;
const SEND_AGENT_MESSAGE_TIMEOUT_MS = 60_000;
const RESYNC_INTERVAL_MS = 30_000;
const LAYOUT_PERSIST_FAILURE_NOTICE = 3;

export function formatError(error) {
  if (!error) return "unknown error";
  if (error instanceof Error) return error.message;
  return String(error);
}

function initialState() {
  const pane = {
    id: "pane-1",
    title: "term-1",
    kind: "shell",
    created_at_ms: Date.now(),
  };
  return {
    panes: new Map([[pane.id, pane]]),
    paneStates: new Map([[pane.id, "live"]]),
    agentStates: new Map(),
    agentSpecs: new Map(),
    newAgentBackend: "claude",
    newAgentModel: "",
    profiles: [],
    selectedProfile: null,
    activePaneId: pane.id,
    cwd: "starting workspace",
    layout: leaf(pane.id),
    terminalBuffers: new Map([[pane.id, ""]]),
    terminalSizes: new Map(),
    terminalResizeSyncs: new Map(),
    terminalViews: new Map(),
    terminalEngineAvailable: null,
    chats: new Map(),
    chatViews: new Map(),
    chatDrafts: new Map(),
    backendEventsReady: false,
    booted: false,
    ready: "booting",
    bootStatus: "booting",
    appearance: { fontFamily: null, fontSize: null, theme: null },
    settingsModalOpen: false,
    settingsFocusRestore: null,
    paletteOpen: false,
    paletteFocusRestore: null,
    overviewOpen: false,
    overviewFocusRestore: null,
    lastActivityMs: new Map(),
    settingsDirty: false,
    settingsLoadFailed: false,
    settingsPassthroughConfig: {},
    settingsValues: populateFormFromConfig(null),
    settingsErrors: {},
    settingsError: "",
    search: createSearchState(),
    zoom: createZoomState(),
    updateVersion: null,
    renamingPaneId: null,
    dragging: false,
    drag: null,
  };
}

export function createAppController({ nativeInvoke, nativeListen } = {}) {
  const state = initialState();
  const fallbackPanes = new Map();
  const subscribers = new Set();
  const unlisteners = [];
  const chatRenderFrames = new Map();
  let revision = 0;
  let started = false;
  let stopped = false;
  let resyncTimer = null;
  let resyncInFlight = false;
  let reconcileTimer = null;
  let pendingLocalCreates = 0;
  let layoutPersistFailures = 0;
  const activeSync = { inFlight: false, pending: null };

  const bridgeInvoke = nativeInvoke ?? getTauriInvoke();
  const bridgeListen = nativeListen ?? getTauriListen();

  function notify() {
    revision += 1;
    for (const subscriber of subscribers) subscriber();
  }

  function subscribe(subscriber) {
    subscribers.add(subscriber);
    return () => subscribers.delete(subscriber);
  }

  function getSnapshot() {
    return revision;
  }

  function fallbackPane(title) {
    const id = makeUniqueId("local");
    const pane = {
      id,
      title: title || `term-${fallbackPanes.size + 1}`,
      kind: "shell",
      created_at_ms: Date.now(),
    };
    fallbackPanes.set(id, pane);
    return pane;
  }

  async function invoke(command, args = {}) {
    if (bridgeInvoke) return bridgeInvoke(command, args);
    if (command === "bootstrap_workspace") {
      if (fallbackPanes.size === 0) fallbackPane("term-1");
      const panes = Array.from(fallbackPanes.values());
      return {
        panes,
        active_pane_id: panes[0]?.id ?? null,
        cwd: window.location.pathname,
      };
    }
    if (command === "create_pane") return fallbackPane(args.title);
    if (command === "create_agent_pane") {
      const pane = fallbackPane(args.title || "agent");
      pane.kind = "agent";
      pane.agentSpec = {
        backend: args.backend || "claude",
        ...(args.model ? { model: args.model } : {}),
      };
      return pane;
    }
    if (command === "close_pane") fallbackPanes.delete(args.paneId);
    if (command === "rename_pane") {
      const pane = fallbackPanes.get(args.paneId);
      if (pane) pane.title = args.title;
      return pane;
    }
    if (command === "write_to_pane") terminals.append(args.paneId, args.data);
    if (command === "get_config") return null;
    const accepted = new Set([
      "send_agent_message",
      "agent_approval",
      "interrupt_agent",
      "close_pane",
      "write_to_pane",
      "ensure_pane_terminal",
      "restart_pane_terminal",
      "resize_pane_terminal",
      "set_active_pane",
      "update_workspace_layout",
      "write_config",
      "install_update",
    ]);
    if (accepted.has(command)) return { ok: true };
    throw new Error(`Unknown command: ${command}`);
  }

  async function invokeWithTimeout(command, args = {}, timeoutMs) {
    if (!bridgeInvoke) return invoke(command, args);
    return withTimeout(invoke(command, args), command, timeoutMs);
  }

  const terminals = createTerminalController({
    state,
    invoke: invokeWithTimeout,
    write: writeToPane,
    getAppearance: () => state.appearance,
    hasPane: (paneId) => state.panes.has(paneId),
  });

  function setBootStatus(message) {
    if (state.bootStatus === message) return;
    state.bootStatus = message;
    notify();
  }

  function paneIsAgent(paneId) {
    return state.panes.get(paneId)?.kind === "agent";
  }

  function resolveProfile(name) {
    if (!name) return null;
    return state.profiles.find((profile) => profile?.name === name) ?? null;
  }

  function resolveSelectedProfile() {
    return resolveProfile(state.selectedProfile);
  }

  function loadProfilesFromConfig(config) {
    state.profiles = Array.isArray(config?.profiles) ? config.profiles : [];
    if (
      state.selectedProfile &&
      !state.profiles.some((profile) => profile?.name === state.selectedProfile)
    ) {
      state.selectedProfile = null;
    }
  }

  function profileIsAgent(profile) {
    if (!profile || profile.kind === "shell") return false;
    if (profile.kind === "agent") return true;
    return Boolean(profile.agent_backend || profile.agent_model);
  }

  function agentOptionsFromProfile(profile, backend, model) {
    if (!profileIsAgent(profile)) {
      return { backend, model };
    }
    const normalizedModel = String(model || "").trim() || null;
    const profileModel =
      profile.agent_model != null ? String(profile.agent_model).trim() || null : null;
    return {
      backend: profile.agent_backend || backend || "claude",
      model: profileModel ?? normalizedModel,
    };
  }

  function resolveCreateFnForSelectedProfile() {
    const profile = resolveSelectedProfile();
    if (!profile) return createPane;
    if (profileIsAgent(profile)) {
      return () =>
        createAgentPane(null, state.newAgentBackend, state.newAgentModel, profile);
    }
    return () => createPane(null, profile);
  }

  function getOrCreateChat(paneId) {
    let chat = state.chats.get(paneId);
    if (!chat) {
      chat = createAgentChat();
      state.chats.set(paneId, chat);
    }
    return chat;
  }

  function scheduleChatRender(paneId) {
    if (chatRenderFrames.has(paneId)) return;
    const frame = window.requestAnimationFrame(() => {
      chatRenderFrames.delete(paneId);
      if (state.panes.has(paneId)) notify();
    });
    chatRenderFrames.set(paneId, frame);
  }

  function disposeChat(paneId) {
    const frame = chatRenderFrames.get(paneId);
    if (frame) window.cancelAnimationFrame(frame);
    chatRenderFrames.delete(paneId);
    state.chatViews.delete(paneId);
    state.chatDrafts.delete(paneId);
    state.chats.delete(paneId);
  }

  function registerChatView(paneId, view) {
    if (view) state.chatViews.set(paneId, view);
    else state.chatViews.delete(paneId);
  }

  function setChatDraft(paneId, text) {
    state.chatDrafts.set(paneId, text);
    notify();
  }

  function isInvokeTimeout(error) {
    return typeof error?.message === "string" && error.message.endsWith(" timed out");
  }

  function sendAgentMessage(paneId, rawText) {
    const text = String(rawText || "").trim();
    if (!paneId || !text || !paneIsAgent(paneId)) return;
    const chat = getOrCreateChat(paneId);
    if (chat.busy) return;
    state.chatDrafts.set(paneId, "");
    appendUserMessage(chat, text);
    notify();
    invokeWithTimeout(
      "send_agent_message",
      { paneId, text },
      SEND_AGENT_MESSAGE_TIMEOUT_MS,
    ).catch((error) => {
      if (!state.panes.has(paneId) || !paneIsAgent(paneId)) return;
      const live = getOrCreateChat(paneId);
      if (!isInvokeTimeout(error)) {
        removeUserMessage(live, text);
        if (!state.chatDrafts.get(paneId)) state.chatDrafts.set(paneId, text);
        live.busy = false;
      }
      applyAgentEvent(live, {
        kind: "error",
        message: `send failed: ${formatError(error)}`,
      });
      notify();
    });
  }

  function interruptAgent(paneId) {
    if (!paneId) return;
    invokeWithTimeout("interrupt_agent", { paneId }).catch((error) => {
      if (!state.panes.has(paneId) || !paneIsAgent(paneId)) return;
      applyAgentEvent(getOrCreateChat(paneId), {
        kind: "error",
        message: `interrupt failed: ${formatError(error)}`,
      });
      notify();
    });
  }

  function sendAgentApproval(paneId, requestId, allow, message) {
    if (!paneId || !requestId) return;
    invokeWithTimeout("agent_approval", { paneId, requestId, allow, message })
      .then(() => {
        if (!state.panes.has(paneId) || !paneIsAgent(paneId)) return;
        clearAgentPermission(getOrCreateChat(paneId), requestId);
        notify();
      })
      .catch((error) => {
        if (!state.panes.has(paneId) || !paneIsAgent(paneId)) return;
        applyAgentEvent(getOrCreateChat(paneId), {
          kind: "error",
          message: `approval failed: ${formatError(error)}`,
        });
        notify();
      });
  }

  async function persistWorkspaceLayout() {
    if (!bridgeInvoke || !state.layout) return;
    try {
      await invokeWithTimeout("update_workspace_layout", { layout: state.layout });
      if (layoutPersistFailures >= LAYOUT_PERSIST_FAILURE_NOTICE) setBootStatus("native");
      layoutPersistFailures = 0;
    } catch (error) {
      layoutPersistFailures += 1;
      console.warn("failed to persist layout", error);
      if (layoutPersistFailures >= LAYOUT_PERSIST_FAILURE_NOTICE) {
        setBootStatus("layout not saved — daemon unreachable");
      }
    }
  }

  async function loadAppearanceConfig() {
    if (!bridgeInvoke) return null;
    try {
      const config = await invokeWithTimeout("get_config");
      if (config) {
        state.appearance = mergeAppearance(config);
        terminals.applyAppearance(state.appearance);
        loadProfilesFromConfig(config);
      }
      return config;
    } catch (error) {
      console.warn("failed to load config", error);
      return null;
    }
  }

  async function syncActivePane(paneId) {
    if (!paneId || !bridgeInvoke) return;
    if (activeSync.inFlight) {
      activeSync.pending = paneId;
      return;
    }
    activeSync.inFlight = true;
    try {
      await invokeWithTimeout("set_active_pane", { paneId });
    } catch (error) {
      console.warn("failed to sync active pane", error);
    } finally {
      activeSync.inFlight = false;
      const next = activeSync.pending;
      activeSync.pending = null;
      if (next && next !== paneId) void syncActivePane(next);
    }
  }

  function applyWorkspaceSnapshot(snapshot, preRequestPaneIds, { initial }) {
    const snapshotPanes = Array.isArray(snapshot.panes) ? snapshot.panes : [];
    const snapshotIds = new Set(snapshotPanes.map((pane) => pane.id));
    const addedPaneIds = [];
    let changed = false;

    for (const pane of snapshotPanes) {
      const existing = state.panes.get(pane.id);
      if (!existing) {
        addedPaneIds.push(pane.id);
        changed = true;
      } else if (existing.title !== pane.title || existing.kind !== pane.kind) {
        changed = true;
        if (existing.kind !== pane.kind) {
          terminals.dispose(pane.id);
          disposeChat(pane.id);
        }
      }
      state.panes.set(pane.id, pane);
    }

    for (const [paneId, runtimeState] of Object.entries(snapshot.pane_states || {})) {
      if (!snapshotIds.has(paneId)) continue;
      if (state.paneStates.get(paneId) !== runtimeState) changed = true;
      state.paneStates.set(paneId, runtimeState);
    }

    const snapshotAgentStates = snapshot.agent_states || {};
    for (const paneId of snapshotIds) {
      const next = normalizeAgentState(snapshotAgentStates[paneId]);
      const existing = state.agentStates.get(paneId) ?? null;
      if (agentStateEquals(existing, next)) continue;
      if (next) state.agentStates.set(paneId, next);
      else state.agentStates.delete(paneId);
      changed = true;
    }

    const snapshotAgentSpecs = snapshot.agent_specs || {};
    for (const paneId of snapshotIds) {
      const pane = state.panes.get(paneId);
      const next =
        pane?.kind === "agent"
          ? snapshotAgentSpecs[paneId] || { backend: "claude" }
          : null;
      const existing = state.agentSpecs.get(paneId) ?? null;
      if (JSON.stringify(existing) === JSON.stringify(next)) continue;
      if (next) state.agentSpecs.set(paneId, next);
      else state.agentSpecs.delete(paneId);
      changed = true;
    }

    // (T2/H2) Seed missing chats from the snapshot replay tail, then clamp
    // ANY chat (seeded or already-mounted) when pane_states says ended.
    // Existing chats are ahead of the tail for live events, so resync never
    // replaces them — but after daemon death catch-up can report ended
    // without a live process_exit, so mounted mid-turn chats must un-wedge.
    // Healthy mid-turn chats on LIVE panes are left alone.
    const snapshotAgentEvents = snapshot.agent_events || {};
    for (const paneId of snapshotIds) {
      if (state.panes.get(paneId)?.kind !== "agent") continue;
      if (!state.chats.has(paneId)) {
        const events = snapshotAgentEvents[paneId];
        if (!Array.isArray(events) || events.length === 0) continue;
        replayAgentEvents(getOrCreateChat(paneId), events);
      }
      if (snapshot.pane_states?.[paneId] === "ended") {
        const chat = state.chats.get(paneId);
        if (chat && clampAgentChatToPaneEnded(chat)) changed = true;
      }
    }

    for (const paneId of Array.from(state.panes.keys())) {
      if (!snapshotIds.has(paneId) && preRequestPaneIds.has(paneId)) {
        terminals.dispose(paneId);
        disposeChat(paneId);
        state.panes.delete(paneId);
        state.paneStates.delete(paneId);
        state.agentStates.delete(paneId);
        state.agentSpecs.delete(paneId);
        state.lastActivityMs.delete(paneId);
        changed = true;
      }
    }
    for (const paneId of Array.from(state.lastActivityMs.keys())) {
      if (!state.panes.has(paneId)) {
        state.lastActivityMs.delete(paneId);
        changed = true;
      }
    }
    for (const paneId of Array.from(state.terminalViews.keys())) {
      if (!state.panes.has(paneId)) terminals.dispose(paneId);
    }
    for (const paneId of Array.from(state.chats.keys())) {
      if (!state.panes.has(paneId)) disposeChat(paneId);
    }

    const mergedPanes = Array.from(state.panes.values());
    let nextActive = state.activePaneId;
    if (initial || !nextActive || !state.panes.has(nextActive)) {
      nextActive = snapshotIds.has(snapshot.active_pane_id)
        ? snapshot.active_pane_id
        : mergedPanes[0]?.id ?? null;
    }
    if (nextActive !== state.activePaneId) {
      state.activePaneId = nextActive;
      changed = true;
    }
    if (!initial && nextActive && nextActive !== snapshot.active_pane_id) {
      void syncActivePane(nextActive);
    }

    const nextCwd = snapshot.cwd || "workspace";
    if (nextCwd !== state.cwd) {
      state.cwd = nextCwd;
      changed = true;
    }

    if (!layoutMatchesPanes(state.layout, mergedPanes) || (initial && !state.booted)) {
      if (layoutMatchesPanes(snapshot.layout, mergedPanes)) {
        state.layout = snapshot.layout;
      } else if (!initial && isValidLayoutNode(state.layout)) {
        state.layout = reconcileLayout(
          state.layout,
          mergedPanes.map((pane) => pane.id),
          state.activePaneId,
        ).layout;
      } else {
        state.layout = defaultLayoutForPanes(mergedPanes, state.activePaneId);
      }
      changed = true;
    }

    return { changed, addedPaneIds };
  }

  async function ensurePaneTerminal(paneId) {
    if (!paneId || !bridgeInvoke) return;
    try {
      await invokeWithTimeout("ensure_pane_terminal", { paneId });
      if (state.terminalEngineAvailable !== false) setBootStatus("native");
    } catch (error) {
      setBootStatus("terminal error");
      terminals.append(
        paneId,
        `[sgian] failed to start terminal: ${formatError(error)}\r\n`,
      );
    }
  }

  async function bootstrap() {
    setBootStatus(bridgeInvoke ? "native bridge found" : "browser preview");
    // xterm is required for shell rendering, but it is not required for the
    // native bridge, workspace snapshot, or agent chat. Keep booting when the
    // asset is unavailable so agent panes and diagnostics remain usable.
    const terminalAvailable = terminals.available();
    state.terminalEngineAvailable = terminalAvailable;
    if (!terminalAvailable) setBootStatus("terminal engine unavailable");
    try {
      await wireBackendEvents();
    } catch (error) {
      terminals.append(
        state.activePaneId,
        `[sgian] terminal listener unavailable: ${formatError(error)}\n`,
      );
    }

    const preRequestPaneIds = new Set(state.panes.keys());
    const snapshot = await invokeWithTimeout(
      "bootstrap_workspace",
      {},
      BOOTSTRAP_TIMEOUT_MS,
    );
    applyWorkspaceSnapshot(snapshot, preRequestPaneIds, { initial: true });
    for (const pane of snapshot.panes || []) {
      if (pane.kind === "agent") continue;
      const text = bridgeInvoke
        ? snapshot.scrollback?.[pane.id] || ""
        : `$ ${pane.title} attached\r\n$ browser preview only\r\n`;
      terminals.setText(pane.id, text);
    }
    state.ready = "true";
    state.bootStatus = terminalAvailable
      ? bridgeInvoke
        ? "native"
        : "browser"
      : "terminal engine unavailable";
    state.booted = true;
    await loadAppearanceConfig();
    notify();
    void persistWorkspaceLayout();
    for (const pane of snapshot.panes || []) {
      if (pane.kind !== "agent") void ensurePaneTerminal(pane.id);
    }
    startPeriodicResync();
    void maybeRunUiSmoke();
  }

  async function maybeRunUiSmoke() {
    if (!bridgeInvoke) return;
    let enabled = false;
    try {
      enabled = await invokeWithTimeout("ui_smoke_enabled", {});
    } catch {
      return;
    }
    if (enabled !== true) return;
    try {
      const paneCountBeforeSplit = state.panes.size;
      await splitActive("row");
      if (state.panes.size <= paneCountBeforeSplit) {
        throw new Error("UI smoke split did not create a pane");
      }
      openSettingsModal();
      await new Promise((resolve) => window.setTimeout(resolve, 50));
      closeSettingsModal();
      if (state.activePaneId && state.panes.get(state.activePaneId)?.kind !== "agent") {
        await writeToPane(state.activePaneId, "echo sgian-ui-smoke\n");
      }
      // Second bootstrap exercises the reattach path against a live daemon.
      await invokeWithTimeout("bootstrap_workspace", {}, BOOTSTRAP_TIMEOUT_MS);
      await invokeWithTimeout("complete_ui_smoke", { ok: true, error: null });
    } catch (error) {
      try {
        await invokeWithTimeout("complete_ui_smoke", {
          ok: false,
          error: formatError(error),
        });
      } catch {
        // Process may already be exiting.
      }
    }
  }

  function showBootError(error) {
    const paneId = state.activePaneId || "pane-1";
    if (!state.panes.has(paneId)) {
      const pane = {
        id: paneId,
        title: "term-1",
        kind: "shell",
        created_at_ms: Date.now(),
      };
      state.panes = new Map([[pane.id, pane]]);
      state.paneStates = new Map([[pane.id, "ended"]]);
      state.activePaneId = pane.id;
      state.layout = leaf(pane.id);
    }
    state.cwd = `workspace unavailable — ${formatError(error)}`;
    state.ready = "error";
    state.bootStatus = "error";
    terminals.append(
      paneId,
      `\r\n[sgian] failed to load workspace: ${formatError(error)}\r\n`,
    );
    notify();
  }

  function startPeriodicResync() {
    if (resyncTimer || !bridgeInvoke) return;
    resyncTimer = window.setInterval(() => void resyncWorkspace(), RESYNC_INTERVAL_MS);
  }

  function stopPeriodicResync() {
    if (!resyncTimer) return;
    window.clearInterval(resyncTimer);
    resyncTimer = null;
  }

  async function resyncWorkspace() {
    if (resyncInFlight || pendingLocalCreates > 0) return;
    if (
      state.settingsModalOpen ||
      state.paletteOpen ||
      state.overviewOpen ||
      state.dragging ||
      state.renamingPaneId
    ) {
      return;
    }
    resyncInFlight = true;
    try {
      const preRequestPaneIds = new Set(state.panes.keys());
      const snapshot = await invokeWithTimeout(
        "bootstrap_workspace",
        {},
        BOOTSTRAP_TIMEOUT_MS,
      );
      const { changed, addedPaneIds } = applyWorkspaceSnapshot(
        snapshot,
        preRequestPaneIds,
        { initial: false },
      );
      for (const paneId of addedPaneIds) {
        if (paneIsAgent(paneId)) continue;
        terminals.setText(paneId, snapshot.scrollback?.[paneId] || "");
        void ensurePaneTerminal(paneId);
      }
      if (changed) notify();
    } catch (error) {
      console.warn("workspace resync failed", error);
    } finally {
      resyncInFlight = false;
    }
  }

  function schedulePaneReconcile() {
    if (reconcileTimer) return;
    reconcileTimer = window.setTimeout(() => {
      reconcileTimer = null;
      reconcilePaneLayout();
    }, 120);
  }

  function reconcilePaneLayout() {
    if (pendingLocalCreates > 0 || state.renamingPaneId) {
      schedulePaneReconcile();
      return;
    }
    const result = reconcileLayout(state.layout, state.panes.keys(), state.activePaneId);
    if (!result.changed) return;
    state.layout = result.layout;
    if (result.newActiveId) {
      state.activePaneId = result.newActiveId;
      void syncActivePane(state.activePaneId);
    }
    notify();
    void persistWorkspaceLayout();
  }

  async function createPane(title, profile = null) {
    pendingLocalCreates += 1;
    try {
      const profileName =
        typeof profile === "string"
          ? profile
          : profile && typeof profile === "object"
            ? profile.name
            : null;
      const pane = await invokeWithTimeout("create_pane", {
        title,
        profile: profileName || null,
      });
      state.panes.set(pane.id, pane);
      setPaneRuntimeState(state, pane.id, "live");
      void ensurePaneTerminal(pane.id);
      return pane;
    } finally {
      pendingLocalCreates -= 1;
    }
  }

  async function createAgentPane(
    title = null,
    backend = state.newAgentBackend,
    model = state.newAgentModel,
    profile = null,
  ) {
    pendingLocalCreates += 1;
    try {
      const resolvedProfile = profile ?? resolveSelectedProfile();
      const { backend: effectiveBackend, model: effectiveModel } = agentOptionsFromProfile(
        resolvedProfile,
        backend,
        model,
      );
      const normalizedModel = effectiveModel ? String(effectiveModel).trim() || null : null;
      const pane = await invokeWithTimeout("create_agent_pane", {
        title,
        backend: effectiveBackend,
        model: normalizedModel,
      });
      state.panes.set(pane.id, pane);
      state.agentSpecs.set(pane.id, {
        backend: effectiveBackend || "claude",
        ...(normalizedModel ? { model: normalizedModel } : {}),
      });
      setPaneRuntimeState(state, pane.id, "live");
      return pane;
    } finally {
      pendingLocalCreates -= 1;
    }
  }

  async function splitActive(direction, createFn) {
    if (!state.activePaneId) return;
    try {
      const priorActive = state.activePaneId;
      const resolvedCreateFn = createFn ?? resolveCreateFnForSelectedProfile();
      const pane = await resolvedCreateFn();
      if (!layoutLeafIds(state.layout).includes(pane.id)) {
        state.layout = replaceLeaf(
          state.layout,
          priorActive,
          split(direction, leaf(priorActive), leaf(pane.id)),
        );
      }
      state.activePaneId = pane.id;
      if (isZoomed(state.zoom)) syncZoomWithActive(state.zoom, "");
      notify();
      void persistWorkspaceLayout();
    } catch (error) {
      setBootStatus("split failed");
      console.warn("failed to split pane", error);
    }
  }

  function newAgentPane() {
    const profile = resolveSelectedProfile();
    const createFn = profileIsAgent(profile)
      ? () => createAgentPane(null, state.newAgentBackend, state.newAgentModel, profile)
      : profile
        ? () => createPane(null, profile)
        : () => createAgentPane(null, state.newAgentBackend, state.newAgentModel, null);
    return splitActive("row", createFn);
  }

  function newPaneWithProfile(profileName) {
    const profile = resolveProfile(profileName);
    if (!profile) return;
    const createFn = profileIsAgent(profile)
      ? () => createAgentPane(null, state.newAgentBackend, state.newAgentModel, profile)
      : () => createPane(null, profile);
    return splitActive("row", createFn);
  }

  function setSelectedProfile(name) {
    const normalized = String(name || "").trim() || null;
    if (normalized && !resolveProfile(normalized)) return;
    state.selectedProfile = normalized;
    notify();
  }

  function setNewAgentBackend(backend) {
    if (!["claude", "droid"].includes(backend)) return;
    state.newAgentBackend = backend;
    notify();
  }

  function setNewAgentModel(model) {
    state.newAgentModel = String(model ?? "");
    notify();
  }

  async function closePane(paneId) {
    if (!paneId || !state.panes.has(paneId) || state.panes.size <= 1) return;
    try {
      await invokeWithTimeout("close_pane", { paneId });
    } catch (error) {
      setBootStatus("close failed");
      console.warn("failed to close pane", error);
      return;
    }
    handleZoomedPaneClosed(state.zoom, paneId);
    terminals.dispose(paneId);
    disposeChat(paneId);
    state.panes.delete(paneId);
    state.paneStates.delete(paneId);
    state.agentStates.delete(paneId);
    state.agentSpecs.delete(paneId);
    state.lastActivityMs.delete(paneId);
    state.layout = pruneLeaf(state.layout, paneId);
    if (state.activePaneId === paneId) {
      state.activePaneId = firstPaneId(state.layout);
      void syncActivePane(state.activePaneId);
    }
    notify();
    void persistWorkspaceLayout();
  }

  async function closeActive() {
    if (!state.activePaneId || state.panes.size <= 1) return;
    await closePane(state.activePaneId);
  }

  async function restartPane(paneId) {
    if (!paneId || !state.panes.has(paneId)) return;
    try {
      await invokeWithTimeout("restart_pane_terminal", { paneId });
      setPaneRuntimeState(state, paneId, "live");
      terminals.setText(paneId, "");
      state.bootStatus =
        state.terminalEngineAvailable === false ? "terminal engine unavailable" : "native";
      notify();
      queueMicrotask(() => terminals.focus(paneId));
    } catch (error) {
      setBootStatus("terminal error");
      terminals.append(
        paneId,
        `\r\n[sgian] failed to restart terminal: ${formatError(error)}\r\n`,
      );
    }
  }

  function beginRename(paneId) {
    if (!state.panes.has(paneId)) return;
    state.renamingPaneId = paneId;
    notify();
  }

  function cancelRename() {
    if (!state.renamingPaneId) return;
    state.renamingPaneId = null;
    notify();
  }

  async function commitRename(paneId, rawTitle) {
    const pane = state.panes.get(paneId);
    if (!pane) return;
    const title = String(rawTitle || "").trim();
    state.renamingPaneId = null;
    if (title && title !== pane.title) {
      try {
        const updated = await invokeWithTimeout("rename_pane", { paneId, title });
        if (updated) state.panes.set(updated.id, updated);
      } catch (error) {
        state.bootStatus = "rename failed";
        console.warn("failed to rename pane", error);
      }
    }
    notify();
  }

  function touchLastActivity(paneId) {
    if (!paneId || !state.panes.has(paneId)) return;
    state.lastActivityMs.set(paneId, Date.now());
  }

  function attentionPaneIds() {
    return Array.from(state.agentStates)
      .filter(
        ([paneId, info]) =>
          info?.attention === "needs_input" && state.panes.has(paneId),
      )
      .map(([paneId]) => paneId);
  }

  function focusNextAttentionPane() {
    const ids = attentionPaneIds();
    if (ids.length === 0) return;
    const currentIndex = ids.indexOf(state.activePaneId);
    const next = ids[(currentIndex + 1) % ids.length] ?? ids[0];
    focusPane(next);
  }

  function focusPane(id) {
    if (!state.panes.has(id) || state.activePaneId === id) return;
    state.activePaneId = id;
    if (isZoomed(state.zoom)) syncZoomWithActive(state.zoom, id);
    notify();
    void syncActivePane(id);
  }

  function focusPaneByIndex(index) {
    const target = resolveFocusIndex(Array.from(state.panes.keys()), index);
    if (target) focusPane(target);
  }

  function focusDirectional(direction, paneRects) {
    const target = resolveDirectionalFocus(paneRects, state.activePaneId, direction);
    if (target) focusPane(target);
  }

  function toggleZoomActive() {
    if (!state.activePaneId) return;
    toggleZoom(state.zoom, state.activePaneId);
    notify();
  }

  function swapActiveWithPartner() {
    if (!state.layout || !state.activePaneId) return;
    const partner = swapPartner(layoutLeafIds(state.layout), state.activePaneId);
    if (!partner) return;
    state.layout = swapLeaves(state.layout, state.activePaneId, partner);
    notify();
    void persistWorkspaceLayout();
  }

  function setSplitRatio(node, rawRatio, { persist = true } = {}) {
    node.ratio = clampRatio(rawRatio);
    notify();
    if (persist) void persistWorkspaceLayout();
  }

  function setDragging(dragging) {
    state.dragging = dragging;
    state.drag = dragging ? { active: true } : null;
  }

  async function writeToPane(paneId, data) {
    if (!paneId || !data) return;
    if (paneIsEnded(state, paneId)) {
      setBootStatus("session ended");
      return;
    }
    try {
      await invokeWithTimeout("write_to_pane", { paneId, data });
    } catch (error) {
      if (formatError(error).includes("session ended")) {
        setPaneRuntimeState(state, paneId, "ended");
        notify();
      }
      terminals.append(
        paneId,
        `\r\n[sgian] failed to write to terminal: ${formatError(error)}\r\n`,
      );
    }
  }

  function openPalette() {
    state.paletteFocusRestore = document.activeElement;
    state.paletteOpen = true;
    notify();
  }

  function closePalette() {
    const restore = state.paletteFocusRestore;
    state.paletteFocusRestore = null;
    state.paletteOpen = false;
    notify();
    if (restore?.isConnected) restore.focus();
    else queueMicrotask(focusActiveSurface);
  }

  function openOverview() {
    state.overviewFocusRestore = document.activeElement;
    state.overviewOpen = true;
    notify();
  }

  function closeOverview() {
    const restore = state.overviewFocusRestore;
    state.overviewFocusRestore = null;
    state.overviewOpen = false;
    notify();
    if (restore?.isConnected) restore.focus();
    else queueMicrotask(focusActiveSurface);
  }

  function openSettingsModal() {
    // Capture focus before the state change mounts/targets the dialog. Keeping
    // this in the controller avoids effect ordering races on close.
    state.settingsFocusRestore = document.activeElement;
    state.settingsModalOpen = true;
    state.settingsDirty = false;
    state.settingsLoadFailed = false;
    state.settingsErrors = {};
    state.settingsError = "";
    // Do not expose values from the previous open while the fresh full config
    // request is in flight; otherwise a fast submit can pair stale form data
    // with stale passthrough fields.
    state.settingsValues = populateFormFromConfig(null);
    state.settingsPassthroughConfig = {};
    notify();
    void loadAppearanceConfig().then(loadSettingsConfig);
  }

  function closeSettingsModal() {
    const restore = state.settingsFocusRestore;
    state.settingsFocusRestore = null;
    state.settingsModalOpen = false;
    state.settingsDirty = false;
    state.settingsErrors = {};
    state.settingsError = "";
    notify();
    if (restore?.isConnected) restore.focus();
    else queueMicrotask(focusActiveSurface);
  }

  async function loadSettingsConfig() {
    if (!bridgeInvoke) {
      state.settingsValues = populateFormFromConfig(null);
      notify();
      return;
    }
    try {
      const config = await invokeWithTimeout("get_config");
      if (state.settingsModalOpen && !state.settingsDirty) {
        state.settingsLoadFailed = false;
        state.settingsPassthroughConfig = settingsPassthroughConfig(config);
        state.settingsValues = populateFormFromConfig(config);
        state.settingsErrors = {};
        state.settingsError = "";
        notify();
      }
    } catch (error) {
      console.warn("failed to load config for settings modal", error);
      if (state.settingsModalOpen) {
        state.settingsLoadFailed = true;
        state.settingsError =
          `failed to load current settings — saving disabled: ${formatError(error)}`;
        notify();
      }
    }
  }

  function updateSetting(key, value) {
    state.settingsDirty = true;
    state.settingsValues = { ...state.settingsValues, [key]: value };
    if (state.settingsErrors[key]) {
      state.settingsErrors = { ...state.settingsErrors };
      delete state.settingsErrors[key];
    }
    notify();
  }

  async function saveSettings(values = state.settingsValues) {
    if (state.settingsLoadFailed) return false;
    const { valid, errors } = validateSettingsForm(values);
    if (!valid) {
      state.settingsErrors = errors;
      state.settingsError = Object.values(errors).join("; ");
      notify();
      return false;
    }
    const config = {
      ...state.settingsPassthroughConfig,
      ...serializeSettingsForm(values),
    };
    try {
      await invokeWithTimeout("write_config", { config });
      closeSettingsModal();
      return true;
    } catch (error) {
      state.settingsError = `Save failed: ${formatError(error)}`;
      notify();
      return false;
    }
  }

  function openSearchBar() {
    openSearch(state.search);
    notify();
  }

  function closeSearchBar() {
    const addon = activeSearchAddon(state);
    try {
      addon?.clearDecorations();
    } catch (error) {
      console.warn("failed to clear search decorations", error);
    }
    closeSearch(state.search);
    notify();
  }

  function updateSearchQuery(query) {
    state.search.query = query;
    notify();
  }

  function searchNext(query = state.search.query) {
    state.search.query = query;
    return findNext(state, query);
  }

  function searchPrevious(query = state.search.query) {
    state.search.query = query;
    return findPrevious(state, query);
  }

  async function installUpdate() {
    try {
      await invokeWithTimeout("install_update", {}, 300_000);
    } catch (error) {
      console.warn("failed to install update", error);
      setBootStatus("update failed");
    }
  }

  function dismissUpdate() {
    state.updateVersion = null;
    notify();
  }

  function handleConfigChanged(config) {
    if (!config) return;
    state.appearance = mergeAppearance(config);
    terminals.applyAppearance(state.appearance);
    loadProfilesFromConfig(config);
    if (state.settingsModalOpen && state.settingsDirty) return;
    if (state.settingsModalOpen) void loadSettingsConfig();
    notify();
  }

  function eventCallbacks() {
    return {
      appendTerminalOutput: terminals.append,
      render: notify,
      renderTabs: notify,
      renderStatus: notify,
      schedulePaneReconcile,
      disposeTerminalView: terminals.dispose,
      syncActivePane,
      persistWorkspaceLayout,
      scheduleChatRender,
      disposeChatView: disposeChat,
      updatePaneTitle: notify,
    };
  }

  async function wireBackendEvents() {
    if (!bridgeListen || state.backendEventsReady) return;
    const callbacks = eventCallbacks();
    async function listen(name, handler) {
      const unlisten = await bridgeListen(name, handler);
      if (typeof unlisten === "function") unlisteners.push(unlisten);
    }
    await listen("pty-output", (event) => {
      const payload = event.payload || {};
      const paneId = payload.pane_id || payload.paneId;
      touchLastActivity(paneId);
      handlePtyOutput(state, payload, callbacks);
    });
    await listen("pane-ended", (event) => {
      handlePaneEnded(state, event.payload || {}, callbacks);
    });
    await listen("pane-created", (event) => {
      handlePaneCreated(state, event.payload || {}, callbacks);
    });
    await listen("pane-closed", (event) => {
      handleZoomedPaneClosed(state.zoom, event.payload?.pane_id);
      handlePaneClosed(state, event.payload || {}, callbacks);
    });
    await listen("pane-renamed", (event) => {
      handlePaneRenamed(state, event.payload || {}, callbacks);
    });
    await listen("agent-state", (event) => {
      handleAgentState(state, event.payload || {}, callbacks);
    });
    await listen("agent-event", (event) => {
      const payload = event.payload || {};
      const paneId = payload.pane_id || payload.paneId;
      touchLastActivity(paneId);
      handleAgentEvent(state, payload, callbacks);
    });
    await listen("config-changed", (event) => handleConfigChanged(event.payload || {}));
    await listen("update-available", (event) => {
      const version = event.payload?.version;
      if (typeof version === "string" && version) {
        state.updateVersion = version;
        notify();
      }
    });
    state.backendEventsReady = true;
  }

  function collectPaneRects(root = document) {
    const rects = [];
    for (const pane of root.querySelectorAll(".pane[data-pane-id]")) {
      const rect = pane.getBoundingClientRect();
      if (rect.width === 0 || rect.height === 0) continue;
      rects.push({
        id: pane.dataset.paneId,
        rect: { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom },
      });
    }
    return rects;
  }

  function handleGlobalKey(event) {
    if (state.paletteOpen || state.overviewOpen) {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        if (state.paletteOpen) closePalette();
        else closeOverview();
      }
      return;
    }
    if (state.settingsModalOpen) return;
    const active = document.activeElement;
    if (
      active &&
      ["INPUT", "TEXTAREA", "SELECT"].includes(active.tagName) &&
      !active.closest(".terminal")
    ) {
      if (!active.closest(".chat-composer") || !resolveKeyAction(event)) return;
    }
    const action = resolveKeyAction(event);
    if (!action) return;
    // Ctrl/Cmd+K is kill-line in most shells; only mod+Shift+P opens the palette
    // while a terminal (or its helper textarea) has focus.
    if (
      action.type === "command-palette" &&
      event.key.toLowerCase() === "k" &&
      !event.shiftKey &&
      active?.closest?.(".terminal")
    ) {
      return;
    }
    if (action.type === "copy" && !activeHasSelection(state)) return;
    event.preventDefault();
    event.stopPropagation();
    switch (action.type) {
      case "split":
        void splitActive(action.direction);
        break;
      case "close":
        void closeActive();
        break;
      case "rename":
        beginRename(state.activePaneId);
        break;
      case "focus-index":
        focusPaneByIndex(action.index);
        break;
      case "focus-directional":
        focusDirectional(action.direction, collectPaneRects());
        break;
      case "search-open":
        openSearchBar();
        break;
      case "zoom-toggle":
        toggleZoomActive();
        break;
      case "swap":
        swapActiveWithPartner();
        break;
      case "new-agent":
        void newAgentPane();
        break;
      case "command-palette":
        openPalette();
        break;
      case "copy":
        void copySelection(state);
        break;
      case "paste":
        void pasteClipboard(state);
        break;
    }
  }

  function focusActiveSurface() {
    if (
      state.settingsModalOpen ||
      state.paletteOpen ||
      state.overviewOpen ||
      state.renamingPaneId
    ) {
      return;
    }
    const chatView = state.chatViews.get(state.activePaneId);
    if (chatView?.root?.isConnected) {
      if (!chatView.root.contains(document.activeElement) && !chatView.textarea?.disabled) {
        chatView.textarea?.focus({ preventScroll: true });
      }
      return;
    }
    if (terminals.focus(state.activePaneId)) return;
    document
      .querySelector(`.pane[data-pane-id="${CSS.escape(state.activePaneId || "")}"]`)
      ?.focus({ preventScroll: true });
  }

  function handleGlobalError(error) {
    if (state.booted) {
      setBootStatus("error");
      console.error("[sgian] runtime error", error);
    } else {
      showBootError(error);
    }
  }

  function start() {
    if (started) return;
    started = true;
    stopped = false;
    void bootstrap().catch(showBootError);
  }

  function stop() {
    if (stopped) return;
    stopped = true;
    stopPeriodicResync();
    if (reconcileTimer) window.clearTimeout(reconcileTimer);
    reconcileTimer = null;
    for (const unlisten of unlisteners.splice(0)) unlisten();
    for (const frame of chatRenderFrames.values()) window.cancelAnimationFrame(frame);
    chatRenderFrames.clear();
    terminals.disposeAll();
    subscribers.clear();
  }

  return {
    state,
    subscribe,
    getSnapshot,
    start,
    stop,
    notify,
    terminals,
    splitActive,
    newAgentPane,
    newPaneWithProfile,
    setSelectedProfile,
    setNewAgentBackend,
    setNewAgentModel,
    closeActive,
    closePane,
    restartPane,
    beginRename,
    cancelRename,
    commitRename,
    focusPane,
    focusPaneByIndex,
    focusDirectional,
    toggleZoomActive,
    swapActiveWithPartner,
    setSplitRatio,
    setDragging,
    sendAgentMessage,
    interruptAgent,
    sendAgentApproval,
    registerChatView,
    setChatDraft,
    openSettingsModal,
    closeSettingsModal,
    openPalette,
    closePalette,
    openOverview,
    closeOverview,
    focusNextAttentionPane,
    updateSetting,
    saveSettings,
    loadSettingsConfig,
    openSearchBar,
    closeSearchBar,
    updateSearchQuery,
    searchNext,
    searchPrevious,
    installUpdate,
    dismissUpdate,
    handleGlobalKey,
    focusActiveSurface,
    handleGlobalError,
    persistWorkspaceLayout,
    applyWorkspaceSnapshot,
    resyncWorkspace,
    startPeriodicResync,
    stopPeriodicResync,
    syncTerminalSize: terminals.syncSize,
    capBufferedOutput,
    paneRuntimeState: (paneId) => paneRuntimeState(state, paneId),
    isZoomed: () => isZoomed(state.zoom),
  };
}
