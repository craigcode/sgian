use super::*;

/// (T1) Minimum interval between full agent classifications of one pane. The
/// reader thread feeds the screen model per ≤8 KiB output chunk; reclassifying
/// on every chunk would render the grid (a multi-KiB allocation) far more often
/// than any client can usefully consume. Transitions coalesce inside the
/// window — the NEXT classification reports the latest state. The throttle has
/// a TRAILING EDGE (H1): a chunk skipped by the window schedules one deferred
/// classification at window expiry, so the final frame of a burst (typically
/// the permission prompt, after which the agent blocks on stdin and no further
/// chunk ever arrives) is still classified.
pub(crate) const AGENT_CLASSIFY_INTERVAL: Duration = Duration::from_millis(500);

/// (T1) L8: manual agent names are capped and shell-safe (they round-trip
/// through workspace.json and CLI output).
pub(crate) const AGENT_NAME_MAX_LEN: usize = 32;

/// (T1) Strong Claude Code signature markers. Each is one independent marker
/// GROUP; a fourth group is a "❯" prompt combined with box-drawing chrome
/// (checked separately, since it needs both halves). Case-sensitive.
pub(crate) const AGENT_MARKERS: [&str; 3] = ["esc to interrupt", "⏵⏵", "Claude Code"];

/// (T1) Box-drawing chrome the Claude Code TUI draws around its input box.
pub(crate) const AGENT_CHROME_CHARS: [char; 10] =
    ['─', '│', '╭', '╮', '╰', '╯', '┌', '┐', '└', '┘'];

/// (T1) Permission/confirmation prompts: the agent is blocked waiting on the
/// user. `"1. Yes"` requires `"2. No"` alongside (a bare numbered "Yes" line is
/// common prose; a numbered yes/no pair is distinctive).
pub(crate) const AGENT_NEEDS_INPUT: [&str; 3] = [
    "Do you want to proceed?",
    "Waiting for your response",
    "Press enter to continue",
];

/// (T1) The agent is actively working. `"esc to interrupt"` doubles as a
/// signature marker — Claude Code shows it in the footer while a turn runs.
pub(crate) const AGENT_WORKING: [&str; 3] = ["esc to interrupt", "Thinking", "Working"];

/// (T1) Braille spinner frames the Claude Code TUI animates while working.
pub(crate) const AGENT_SPINNER_CHARS: [char; 10] =
    ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// (T1) `needle` occurs in `haystack` (byte-level; no UTF-8 validation and —
/// unlike a `from_utf8_lossy` scan — no allocation on the hot path).
pub(crate) fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// (T1) Cheap raw-output pre-check: could this chunk carry a strong signature
/// marker? A pane with no manual mark and no detected agent is fully classified
/// only when this passes, so plain shell output never pays for the screen-text
/// render. The loose ❯-prompt group is deliberately NOT a candidate: a bare ❯
/// (common zsh prompt) must not trigger renders, and it can never reach the
/// 2-group detection threshold without one of the strong markers anyway.
pub(crate) fn agent_signature_candidate(bytes: &[u8]) -> bool {
    AGENT_MARKERS
        .iter()
        .any(|marker| bytes_contain(bytes, marker.as_bytes()))
}

/// (T1) Count the independent Claude Code signature groups on screen:
/// "esc to interrupt" (working footer), "⏵⏵" (auto-accept/bypass indicator),
/// "Claude Code" (welcome/banner), and a "❯" prompt drawn with box-drawing
/// chrome (the input box). Distinct groups, not occurrences.
pub(crate) fn agent_signature_groups(text: &str) -> usize {
    let mut groups = AGENT_MARKERS
        .iter()
        .filter(|marker| text.contains(**marker))
        .count();
    if text.contains('❯')
        && AGENT_CHROME_CHARS
            .iter()
            .any(|chrome| text.contains(*chrome))
    {
        groups += 1;
    }
    groups
}

/// (T1) Auto-detect the agent from the rendered screen. Fresh detection
/// requires 2+ independent marker groups (conservative: no single marker is
/// proof); a pane already detected keeps its mark while 1+ group remains —
/// hysteresis so transient repaint frames (a full-screen redraw passes through
/// a nearly-empty grid) don't flap the state off and back on.
pub(crate) fn detect_agent(text: &str, already_detected: bool) -> Option<String> {
    let threshold = if already_detected { 1 } else { 2 };
    (agent_signature_groups(text) >= threshold).then(|| "claude".to_string())
}

/// (T1) Classify an agent pane's attention state from its rendered screen.
/// Priority: NeedsInput > Working > Idle — a permission prompt must win over a
/// stale "esc to interrupt" still visible above it. Case-sensitive substrings,
/// deliberately conservative to avoid false positives.
pub(crate) fn classify_agent_attention(text: &str) -> AgentAttention {
    if AGENT_NEEDS_INPUT
        .iter()
        .any(|pattern| text.contains(pattern))
        || (text.contains("1. Yes") && text.contains("2. No"))
    {
        return AgentAttention::NeedsInput;
    }
    if AGENT_WORKING.iter().any(|pattern| text.contains(pattern))
        || AGENT_SPINNER_CHARS.iter().any(|c| text.contains(*c))
    {
        return AgentAttention::Working;
    }
    AgentAttention::Idle
}

/// (T1) The pane's visible screen as newline-joined rows — the same
/// rows-concatenation the wait/snapshot path renders from the vt100 model.
pub(crate) fn model_screen_text(model: &PaneModel) -> String {
    let screen = model.parser.screen();
    let cols = screen.size().1;
    screen.rows(0, cols).collect::<Vec<String>>().join("\n")
}

/// (T1) Per-pane agent state tracked by the daemon: the current agent (manual
/// mark or detected), whether the mark is manual (manual overrides detection
/// and persists), the last classified attention state, and classification
/// throttle bookkeeping.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentPaneState {
    pub(crate) agent: Option<String>,
    pub(crate) manual: bool,
    pub(crate) attention: Option<AgentAttention>,
    pub(crate) last_classified_revision: u64,
    pub(crate) last_classified_at: Option<Instant>,
    /// (T1) H1 trailing edge: one deferred classification is already
    /// scheduled to run at the throttle window's expiry.
    pub(crate) trailing_scheduled: bool,
    /// (T1) M4: consecutive signature-free classifications of a DETECTED
    /// pane; the mark clears on the second (torn-redraw flap guard).
    pub(crate) zero_signature_streak: u8,
    /// (T1) The pane's process has ended (or was killed for a restart): no
    /// further automatic classification until the next spawn — a dead agent
    /// must not be re-classified back to a working/needs-input badge from
    /// its preserved final screen (M2).
    pub(crate) ended: bool,
    /// (M3b) Until when an official `claude agents --json` reading outranks
    /// the screen heuristic for this pane. `None` = never had one.
    pub(crate) official_until: Option<Instant>,
    /// The permission mode read off the screen (see `classify_agent_mode`).
    pub(crate) mode: Option<String>,
    /// The attention state the agent had when its process ended (taken by
    /// `clear_agent_attention`), so `pane.ended` can say whether the agent
    /// was still waiting on a person. Reset on the next spawn.
    pub(crate) last_attention: Option<AgentAttention>,
}

impl AgentPaneState {
    /// The wire-facing view of this entry.
    pub(crate) fn info(&self) -> AgentPaneInfo {
        AgentPaneInfo {
            agent: self.agent.clone(),
            attention: self.attention,
            mode: self.mode.clone(),
            unattended: is_unattended_mode(self.mode.as_deref()),
        }
    }
}

/// (T1) Per-pane agent tracking, held by the `OutputRouter` alongside the
/// screen models. Entries are created on first classification and removed when
/// the pane closes; bounded by the pane count.
#[derive(Debug, Default)]
pub(crate) struct AgentTracker {
    pub(crate) panes: HashMap<String, AgentPaneState>,
}

/// A subscriber is fed through a bounded channel drained by its own writer thread, so
/// broadcast never performs socket I/O while holding the subscriber lock (one slow/hung
/// consumer can't stall other panes), while per-subscriber ordering is preserved.
pub(crate) struct Subscriber {
    pub(crate) id: u64,
    pub(crate) sender: SyncSender<Arc<Vec<u8>>>,
    /// Negotiated wire version of this subscriber's connection: events are framed
    /// (v2 envelope) for `>= frame::WIRE_VERSION`, newline-JSON otherwise. This is
    /// what makes the Subscribe event stream framed iff the connection negotiated v2.
    pub(crate) wire_version: u16,
}

/// Encode an already-serialized event `payload_json` for a subscriber on
/// `wire_version`: a framed v2 envelope for `>= frame::WIRE_VERSION`, else
/// newline-JSON (the legacy v1 stream). Returns `None` only when the payload is too
/// large to frame (the event is then skipped for that subscriber rather than
/// corrupting its stream); the newline path is always `Some`.
pub(crate) fn encode_event_for_wire(wire_version: u16, payload_json: &[u8]) -> Option<Vec<u8>> {
    if wire_version >= frame::WIRE_VERSION {
        frame::encode_payload(payload_json).ok()
    } else {
        let mut bytes = Vec::with_capacity(payload_json.len() + 1);
        bytes.extend_from_slice(payload_json);
        bytes.push(b'\n');
        Some(bytes)
    }
}

#[derive(Clone)]
pub(crate) struct OutputRouter {
    pub(crate) subscribers: Arc<Mutex<Vec<Subscriber>>>,
    pub(crate) scrollback_dir: PathBuf,
    /// Panes closed via ClosePane, with the closure time: a still-draining reader
    /// must not recreate their scrollback or deliver further output. Entries are
    /// pruned by `sweep_closed` once the reader window is long past, so the set
    /// stays bounded for the daemon's lifetime (L13).
    pub(crate) closed: Arc<Mutex<HashMap<String, Instant>>>,
    pub(crate) next_subscriber_id: Arc<AtomicU64>,
    /// Tracing dispatch for structured logging from reader/watcher threads. Set
    /// once by `DaemonServer::with_config`; `None` in unit tests (tracing macros
    /// are no-ops without a subscriber).
    pub(crate) log_dispatch: Arc<std::sync::OnceLock<tracing::dispatcher::Dispatch>>,
    /// Workspace key included in structured log fields for events emitted from
    /// reader/watcher threads (pane-end, client-disconnect).
    pub(crate) log_workspace_key: Arc<std::sync::OnceLock<String>>,
    /// Per-pane vt100 screen models, each behind its own `Mutex` so feeding one
    /// pane never blocks another and a reader holds a parser lock only for the
    /// duration of a single `process` call, never across a PTY read (Invariant 9).
    pub(crate) models: Arc<Mutex<HashMap<String, Arc<Mutex<PaneModel>>>>>,
    /// Per-pane scrollback append state (M11): a cached open File handle + the
    /// tracked byte count, so the reader hot path no longer pays
    /// open+write+close+stat per ≤8 KiB chunk. The map lock is held only to
    /// clone the per-pane Arc; all I/O happens under the per-pane state lock,
    /// which also serializes appends with a cap rewrite of the same file.
    /// Entries are invalidated on pane close and re-primed (one stat) after a
    /// cap replaces the file.
    pub(crate) append_handles: Arc<Mutex<HashMap<String, Arc<Mutex<ScrollbackAppendState>>>>>,
    /// (T1) Per-pane agent detection/attention state. Leaf lock: it is only
    /// ever taken briefly, and no other lock is acquired while holding it —
    /// except that the per-pane MODEL lock may already be held by the caller
    /// (lock order: model → agents, never reversed).
    pub(crate) agents: Arc<Mutex<AgentTracker>>,
    /// The workspace ledger (docs/design/keyboard-lease-and-ledger.md), set
    /// once by `DaemonServer::with_config`; `None` in unit tests that build a
    /// bare router. Attention transitions and pane ends are noted here.
    pub(crate) ledger: Arc<std::sync::OnceLock<Arc<Mutex<LedgerSink>>>>,
    /// Per-pane output-guard counters (see `scan_output_tricks`). Leaf lock.
    pub(crate) output_guard: Arc<Mutex<HashMap<String, OutputGuardState>>>,
    /// The daemon's lease table, shared so a `pane.ended` record can name the
    /// keyboard holder at exit. Read only here; set once by
    /// `DaemonServer::with_config`; `None` in unit tests. Leaf lock.
    pub(crate) leases: Arc<std::sync::OnceLock<SharedLeases>>,
}

/// The daemon's lease table as shared with the output router.
pub(crate) type SharedLeases = Arc<Mutex<HashMap<String, HeldLease>>>;

/// Cached append state for one pane's scrollback file (M11): the open handle
/// and the byte count as tracked by appends (a `stat` happens only when the
/// count is unknown — first open, or right after a cap replaced the file).
pub(crate) struct ScrollbackAppendState {
    pub(crate) file: File,
    pub(crate) len: u64,
}

impl OutputRouter {
    pub(crate) fn new(scrollback_dir: PathBuf) -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
            scrollback_dir,
            closed: Arc::new(Mutex::new(HashMap::new())),
            next_subscriber_id: Arc::new(AtomicU64::new(1)),
            log_dispatch: Arc::new(std::sync::OnceLock::new()),
            log_workspace_key: Arc::new(std::sync::OnceLock::new()),
            models: Arc::new(Mutex::new(HashMap::new())),
            append_handles: Arc::new(Mutex::new(HashMap::new())),
            agents: Arc::new(Mutex::new(AgentTracker::default())),
            ledger: Arc::new(std::sync::OnceLock::new()),
            leases: Arc::new(std::sync::OnceLock::new()),
            output_guard: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Count hiding tricks in one output chunk; announce (ledger + event) on
    /// the first hit and then at most every `OUTPUT_WARNING_ANNOUNCE_INTERVAL`.
    pub(crate) fn record_output_tricks(&self, pane_id: &str, data: &str) {
        // Cheap pre-check: nothing to find without an ESC or a non-ASCII byte.
        if !data.bytes().any(|byte| byte == 0x1b || byte >= 0x80) {
            return;
        }
        let (found, sample) = scan_output_tricks_detailed(data);
        self.note_output_tricks(pane_id, found, sample);
    }

    /// Count tricks already found in a pane's output (an agent pane's chat
    /// text is scrubbed before it is shown) and announce as above.
    pub(crate) fn note_output_tricks(
        &self,
        pane_id: &str,
        found: OutputTricks,
        sample: Option<String>,
    ) {
        if found.total() == 0 {
            return;
        }
        let announce = {
            let Ok(mut guard) = self.output_guard.lock() else {
                return;
            };
            let entry = guard.entry(pane_id.to_string()).or_default();
            entry.total.add(&found);
            if entry.sample.is_none() {
                entry.sample = sample;
            }
            let due = entry
                .last_announced
                .is_none_or(|at| at.elapsed() >= OUTPUT_WARNING_ANNOUNCE_INTERVAL);
            if due {
                entry.last_announced = Some(Instant::now());
                let added = entry.total.minus(&entry.announced);
                entry.announced = entry.total;
                Some((added, entry.total, entry.sample.clone()))
            } else {
                None
            }
        };
        if let Some((added, total, sample)) = announce {
            self.ledger_note(
                pane_id,
                "output.suspicious",
                json!({ "added": added, "total": total, "evidence": "scan", "sample": sample }),
            );
            self.broadcast(&DaemonEvent::OutputWarning {
                pane_id: pane_id.to_string(),
                added,
                total,
                sample,
            });
        }
    }

    pub(crate) fn output_tricks(&self, pane_id: &str) -> OutputTricks {
        self.output_guard
            .lock()
            .ok()
            .and_then(|guard| guard.get(pane_id).map(|entry| entry.total))
            .unwrap_or_default()
    }

    /// Every pane with at least one counted trick.
    pub(crate) fn output_warnings(&self) -> HashMap<String, OutputTricks> {
        self.output_guard
            .lock()
            .map(|guard| {
                guard
                    .iter()
                    .filter(|(_, entry)| entry.total.total() > 0)
                    .map(|(pane_id, entry)| (pane_id.clone(), entry.total))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn remove_output_guard(&self, pane_id: &str) {
        if let Ok(mut guard) = self.output_guard.lock() {
            guard.remove(pane_id);
        }
    }

    pub(crate) fn set_ledger(&self, sink: Arc<Mutex<LedgerSink>>) {
        let _ = self.ledger.set(sink);
    }

    /// A session proved an agent is running in the pane (a status-line
    /// payload arrived from under it). Sets the agent name when the pane has
    /// none, leaving attention and manual marks alone; broadcasts only when
    /// something changed.
    pub(crate) fn mark_agent_present(&self, pane_id: &str, agent: &str) {
        let changed = self.agents.lock().ok().and_then(|mut tracker| {
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            if entry.ended || entry.agent.is_some() {
                return None;
            }
            entry.agent = Some(agent.to_string());
            Some((entry.attention, entry.mode.clone()))
        });
        if let Some((attention, mode)) = changed {
            self.broadcast(&DaemonEvent::agent_state(
                pane_id.to_string(),
                Some(agent.to_string()),
                attention,
                mode,
            ));
        }
    }

    pub(crate) fn set_leases(&self, leases: SharedLeases) {
        let _ = self.leases.set(leases);
    }

    /// The keyboard holder of a pane right now, if the lease table is wired.
    pub(crate) fn lease_holder(&self, pane_id: &str) -> Option<String> {
        self.leases
            .get()
            .and_then(|table| table.lock().ok())
            .and_then(|table| table.get(pane_id).map(|held| held.holder.clone()))
    }

    /// Best-effort, non-durable ledger note from the output path. Called with
    /// no other lock held (the ledger is a leaf lock).
    pub(crate) fn ledger_note(&self, pane_id: &str, kind: &str, payload: Value) {
        if let Some(sink) = self.ledger.get() {
            if let Ok(mut sink) = sink.lock() {
                let _ = sink.record(pane_id, kind, payload, false);
            }
        }
    }

    /// Set the tracing dispatch and workspace key so reader/watcher threads can
    /// emit structured log entries. Called once from `DaemonServer::with_config`.
    pub(crate) fn set_log_context(
        &self,
        dispatch: tracing::dispatcher::Dispatch,
        workspace_key: String,
    ) {
        let _ = self.log_dispatch.set(dispatch);
        let _ = self.log_workspace_key.set(workspace_key);
    }

    /// Returns a `set_default` guard that activates the tracing subscriber for the
    /// current thread, or `None` if no subscriber was configured (unit tests).
    pub(crate) fn log_guard(&self) -> Option<tracing::dispatcher::DefaultGuard> {
        self.log_dispatch
            .get()
            .map(tracing::dispatcher::set_default)
    }

    pub(crate) fn log_workspace_key(&self) -> &str {
        self.log_workspace_key
            .get()
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// Live subscriber count, derived from the subscriber list itself. Disconnect
    /// watchers remove entries promptly, so this stays accurate even while idle —
    /// which the idle-shutdown logic depends on.
    pub(crate) fn subscriber_count(&self) -> usize {
        self.subscribers
            .lock()
            .map(|subscribers| subscribers.len())
            .unwrap_or(0)
    }

    pub(crate) fn remove_subscriber(&self, id: u64) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.id != id);
        }
    }

    /// Register a subscriber, spawning its writer + disconnect-watcher threads.
    /// Returns the subscriber id, or — over MAX_SUBSCRIBERS — the reason AND
    /// the stream handed back so the caller can still write a clean error
    /// response on it (M5: each subscriber costs two threads and its connection
    /// slot is released at hand-off, so uncapped subscription would exhaust
    /// threads/fds and permanently suppress idle shutdown). The cap check and
    /// the insertion happen under one lock so concurrent subscribes can't both
    /// pass; on rejection no entry, channel, or thread is created and the
    /// handed-back stream is dropped by the caller — no connection leak.
    pub(crate) fn add_subscriber(
        &self,
        mut stream: TransportStream,
        wire_version: u16,
    ) -> Result<u64, (String, TransportStream)> {
        let id = self.next_subscriber_id.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = sync_channel::<Arc<Vec<u8>>>(SUBSCRIBER_QUEUE_LIMIT);
        {
            let Ok(mut subscribers) = self.subscribers.lock() else {
                return Err(("subscriber lock poisoned".to_string(), stream));
            };
            if subscribers.len() >= MAX_SUBSCRIBERS {
                return Err((
                    format!("subscriber limit reached ({MAX_SUBSCRIBERS}); try again after a client disconnects"),
                    stream,
                ));
            }
            subscribers.push(Subscriber {
                id,
                sender,
                wire_version,
            });
        }

        // A write timeout bounds the per-subscriber writer thread's lifetime if the
        // peer's socket buffer stays full (so the thread can't leak forever).
        let _ = stream.set_write_timeout(Some(SUBSCRIBER_WRITE_TIMEOUT));

        // A cloned handle drives a watcher thread that detects the peer disconnecting
        // (read returns 0/err) and removes the entry; removal drops the sender, which
        // in turn ends the writer thread instead of leaving it parked on recv().
        // If the clone fails there is no watcher and the entry is pruned on the next
        // broadcast whose try_send fails.
        if let Ok(mut watch_stream) = stream.try_clone() {
            let router = self.clone();
            thread::spawn(move || {
                let _log_guard = router.log_guard();
                let _ = watch_stream.set_read_timeout(None);
                let mut buf = [0_u8; 256];
                loop {
                    match watch_stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                tracing::info!(
                    workspace_key = %router.log_workspace_key(),
                    event = "client_disconnect",
                    "client disconnected"
                );
                router.remove_subscriber(id);
            });
        }

        let router = self.clone();
        thread::spawn(move || {
            while let Ok(payload) = receiver.recv() {
                if stream.write_all(&payload).is_err() || stream.flush().is_err() {
                    break;
                }
            }
            // Shut the socket down so the watcher's blocking read returns even when
            // the peer is wedged (connected but neither reading nor closing), then
            // drop the entry so the count reflects reality.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            router.remove_subscriber(id);
        });

        Ok(id)
    }

    /// Send a single event to one subscriber (by id) without broadcasting to all.
    /// Used by subscribe catch-up to push already-ended pane state to the new
    /// subscriber only, so existing subscribers don't receive duplicate events.
    ///
    /// Unlike `broadcast` (which uses non-blocking `try_send` and drops slow
    /// subscribers), this method is **reliable**: it retries with a bounded total
    /// timeout (`CATCHUP_SEND_TIMEOUT`) so a freshly attached subscriber is
    /// guaranteed to receive every catch-up pane state even when ended panes
    /// exceed the queue limit. If the subscriber's queue stays full for the entire
    /// timeout (a stalled consumer), the subscriber is removed so it never
    /// receives a PARTIAL catch-up that omits panes silently — either every
    /// catch-up event is delivered, or the subscriber is disconnected entirely.
    pub(crate) fn send_to_subscriber(&self, id: u64, event: &DaemonEvent) {
        let Ok(payload_json) = serde_json::to_vec(event) else {
            return;
        };
        // Find the sender + negotiated wire version under the lock, clone the sender,
        // then release the lock before the retry loop so broadcast/add_subscriber
        // aren't blocked while we wait for queue space. The wire version selects the
        // per-subscriber encoding (framed v2 vs newline v1).
        let target = {
            let Ok(subscribers) = self.subscribers.lock() else {
                return;
            };
            subscribers
                .iter()
                .find(|s| s.id == id)
                .map(|s| (s.sender.clone(), s.wire_version))
        };
        let Some((sender, wire_version)) = target else {
            return;
        };
        let Some(payload) = encode_event_for_wire(wire_version, &payload_json) else {
            return;
        };
        let payload = Arc::new(payload);
        let deadline = Instant::now() + CATCHUP_SEND_TIMEOUT;
        loop {
            match sender.try_send(Arc::clone(&payload)) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    if Instant::now() >= deadline {
                        // The subscriber's queue stayed full for the entire timeout:
                        // it's stalled. Remove it so it never receives a partial
                        // catch-up that omits panes silently.
                        self.remove_subscriber(id);
                        return;
                    }
                    // If a concurrent broadcast already removed this subscriber
                    // (it was too slow), abort the catch-up rather than delivering
                    // to a dropped subscriber.
                    let still_present = self
                        .subscribers
                        .lock()
                        .map(|subs| subs.iter().any(|s| s.id == id))
                        .unwrap_or(false);
                    if !still_present {
                        return;
                    }
                    thread::sleep(CATCHUP_SEND_RETRY_INTERVAL);
                }
                Err(TrySendError::Disconnected(_)) => {
                    // The writer thread exited (subscriber gone). Remove the entry.
                    self.remove_subscriber(id);
                    return;
                }
            }
        }
    }

    pub(crate) fn mark_closed(&self, pane_id: &str) {
        if let Ok(mut closed) = self.closed.lock() {
            closed.insert(pane_id.to_string(), Instant::now());
        }
    }

    pub(crate) fn is_closed(&self, pane_id: &str) -> bool {
        self.closed
            .lock()
            .map(|closed| closed.contains_key(pane_id))
            .unwrap_or(false)
    }

    /// Prune closed-pane suppression entries older than `max_age`. An entry is
    /// only needed while the closed pane's reader thread might still be draining
    /// (its child is killed at close, so that window is seconds); pane ids are
    /// never reused (monotonic next_id), so pruning is safe and keeps the set
    /// from growing for the daemon's lifetime (L13).
    pub(crate) fn sweep_closed(&self, max_age: Duration) {
        if let Ok(mut closed) = self.closed.lock() {
            closed.retain(|_, marked_at| marked_at.elapsed() < max_age);
        }
    }

    /// Clone the per-pane model handle out of the map under a tiny lock scope, so
    /// callers lock the (per-pane) parser mutex without holding the map lock.
    pub(crate) fn model_handle(&self, pane_id: &str) -> Option<Arc<Mutex<PaneModel>>> {
        self.models.lock().ok()?.get(pane_id).cloned()
    }

    /// Create the pane's screen model (at spawn). If one already exists (an in-place
    /// restart), reset it to a fresh screen while preserving the monotonic revision.
    pub(crate) fn ensure_model(&self, pane_id: &str, cols: u16, rows: u16) {
        if let Ok(mut models) = self.models.lock() {
            match models.get(pane_id) {
                Some(existing) => {
                    if let Ok(mut model) = existing.lock() {
                        model.reset(cols, rows);
                    }
                }
                None => {
                    models.insert(
                        pane_id.to_string(),
                        Arc::new(Mutex::new(PaneModel::new(cols, rows))),
                    );
                }
            }
        }
        // (T1) A (re)spawn revives automatic classification for the pane:
        // its previous process's `ended` latch (M2) must not outlive it.
        if let Ok(mut tracker) = self.agents.lock() {
            if let Some(entry) = tracker.panes.get_mut(pane_id) {
                entry.ended = false;
                entry.last_attention = None;
            }
        }
    }

    /// Feed raw PTY bytes to the pane's model. The map lock is held only to clone the
    /// per-pane handle; the parser lock is held only for the `process` call. This
    /// touches no other router lock, so parsing never blocks `emit` (Invariant 9).
    /// (T1) The fresh screen is then reclassified for agent state — throttled,
    /// and skipped cheaply for panes with no agent interest.
    pub(crate) fn feed_model(&self, pane_id: &str, bytes: &[u8]) {
        if let Some(model) = self.model_handle(pane_id) {
            if let Ok(mut model) = model.lock() {
                model.process(bytes);
                self.classify_agent_on_output(pane_id, &model, bytes);
            }
        }
    }

    /// (T1) Reclassify a pane's agent state from its CURRENT screen after
    /// output was processed. The caller holds the pane's model lock; the
    /// tracker lock is taken briefly inside (lock order: model → agents).
    ///
    /// Two cheap gates keep the reader hot path clean: a pane with no manual
    /// mark and no detected agent is classified only when the raw output chunk
    /// carries a strong signature marker (plain shell output never allocates
    /// the screen text), and full classification runs at most once per
    /// AGENT_CLASSIFY_INTERVAL per pane, only when the model revision changed.
    /// A chunk skipped by the throttle schedules a trailing-edge deferred
    /// classification (H1) — see `classify_agent_trailing`.
    pub(crate) fn classify_agent_on_output(&self, pane_id: &str, model: &PaneModel, bytes: &[u8]) {
        // (T1) L6a: a closed pane's still-draining reader must not re-create
        // the tracker entry ClosePane dropped (ghost state).
        if self.is_closed(pane_id) {
            return;
        }
        {
            let Ok(mut tracker) = self.agents.lock() else {
                return;
            };
            let interested = tracker
                .panes
                .get(pane_id)
                .is_some_and(|entry| entry.manual || entry.agent.is_some());
            if !interested && !agent_signature_candidate(bytes) {
                return;
            }
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            if entry.last_classified_revision == model.revision || entry.ended {
                return;
            }
            if entry
                .last_classified_at
                .is_some_and(|at| at.elapsed() < AGENT_CLASSIFY_INTERVAL)
            {
                // (T1) H1 trailing edge: the LAST chunk of a burst is exactly
                // where a permission prompt lands — the agent then blocks on
                // stdin and no further chunk ever triggers classification, so
                // the needs_input transition would be lost. Schedule one
                // deferred classification at window expiry (at most one
                // outstanding per pane); it re-checks the revision itself.
                if entry.trailing_scheduled {
                    return;
                }
                entry.trailing_scheduled = true;
                drop(tracker);
                self.spawn_trailing_classification(pane_id);
                return;
            }
            entry.last_classified_at = Some(Instant::now());
            entry.last_classified_revision = model.revision;
        }
        let text = model_screen_text(model);
        self.apply_agent_classification(pane_id, &text);
    }

    /// (T1) The throttle's trailing edge (H1): spawn the deferred
    /// classification off the reader thread. It runs at window expiry and
    /// takes locks in the same order as the feed path (model → agents), so
    /// it can wait on a busy model lock without ever blocking the reader.
    pub(crate) fn spawn_trailing_classification(&self, pane_id: &str) {
        let router = self.clone();
        let pane_id = pane_id.to_string();
        thread::spawn(move || {
            thread::sleep(AGENT_CLASSIFY_INTERVAL);
            router.classify_agent_trailing(&pane_id);
        });
    }

    /// (T1) The deferred classification itself: re-check the model revision
    /// and classify only if it moved since the last classification (a later
    /// unthrottled classification — or a pane close — makes this a no-op;
    /// an ended/closed pane is never reclassified).
    pub(crate) fn classify_agent_trailing(&self, pane_id: &str) {
        if self.is_closed(pane_id) {
            return;
        }
        let Some(model) = self.model_handle(pane_id) else {
            return;
        };
        let text = {
            let Ok(model) = model.lock() else {
                return;
            };
            let Ok(mut tracker) = self.agents.lock() else {
                return;
            };
            let Some(entry) = tracker.panes.get_mut(pane_id) else {
                return;
            };
            entry.trailing_scheduled = false;
            if entry.last_classified_revision == model.revision || entry.ended {
                return;
            }
            entry.last_classified_at = Some(Instant::now());
            entry.last_classified_revision = model.revision;
            model_screen_text(&model)
        };
        self.apply_agent_classification(pane_id, &text);
    }

    /// (T1) Reclassify a pane NOW, bypassing the signature pre-check and the
    /// throttle: a SetPaneAgent mark/unmark must take effect without waiting
    /// for the next output chunk. A pane with no live model classifies against
    /// an empty screen — a manual mark keeps its agent and defaults to Idle.
    pub(crate) fn classify_agent_now(&self, pane_id: &str) {
        let text = self
            .model_handle(pane_id)
            .and_then(|model| {
                model.lock().ok().map(|model| {
                    if let Ok(mut tracker) = self.agents.lock() {
                        let entry = tracker.panes.entry(pane_id.to_string()).or_default();
                        entry.last_classified_at = Some(Instant::now());
                        entry.last_classified_revision = model.revision;
                    }
                    model_screen_text(&model)
                })
            })
            .unwrap_or_default();
        self.apply_agent_classification(pane_id, &text);
    }

    /// (T1) Recompute a pane's (agent, attention) from its rendered screen text
    /// and broadcast an `AgentState` event on TRANSITIONS only. Manual marks
    /// override detection; an unmarked pane re-derives detection from the
    /// screen, clearing to (None, None) when the signature is gone.
    pub(crate) fn apply_agent_classification(&self, pane_id: &str, text: &str) {
        let Ok(mut tracker) = self.agents.lock() else {
            return;
        };
        let entry = tracker.panes.entry(pane_id.to_string()).or_default();
        // (M3b) A fresh official reading outranks the screen heuristic for
        // attention; the permission mode is only ever on the screen, so it
        // still tracks it.
        if entry
            .official_until
            .is_some_and(|until| until > Instant::now())
        {
            let new_mode = entry
                .agent
                .as_ref()
                .and_then(|_| classify_agent_mode(text).map(str::to_string));
            if entry.mode == new_mode {
                return;
            }
            let previous_mode = std::mem::replace(&mut entry.mode, new_mode.clone());
            let agent = entry.agent.clone();
            let attention = entry.attention;
            drop(tracker);
            self.note_mode_change(pane_id, agent.as_deref(), previous_mode, new_mode.clone());
            self.broadcast(&DaemonEvent::agent_state(
                pane_id.to_string(),
                agent,
                attention,
                new_mode,
            ));
            return;
        }
        let new_agent = if entry.manual {
            entry.agent.clone()
        } else {
            let groups = agent_signature_groups(text);
            if groups == 0 && entry.agent.is_some() {
                // (T1) M4: a torn full-screen redraw passes through a
                // nearly-empty grid; clearing a detected mark on the FIRST
                // signature-free classification would flap the mark off and
                // back on. Require TWO consecutive zero-group results (one
                // throttle window apart); the first leaves the state
                // untouched.
                entry.zero_signature_streak += 1;
                if entry.zero_signature_streak < 2 {
                    return;
                }
                None
            } else {
                entry.zero_signature_streak = 0;
                detect_agent(text, entry.agent.is_some())
            }
        };
        // (T1) The ended latch (set by clear_agent_attention) blocks automatic
        // classify paths; classify_agent_now / SetPaneAgent must honor it too
        // so a preserved final screen cannot resurrect Working/NeedsInput.
        let new_attention = if entry.ended {
            None
        } else {
            new_agent.as_ref().map(|_| classify_agent_attention(text))
        };
        let new_mode = new_agent
            .as_ref()
            .and_then(|_| classify_agent_mode(text).map(str::to_string));
        if entry.agent == new_agent && entry.attention == new_attention && entry.mode == new_mode {
            return;
        }
        let previous_attention = entry.attention;
        let previous_agent = entry.agent.clone();
        let previous_mode = std::mem::replace(&mut entry.mode, new_mode.clone());
        entry.agent = new_agent.clone();
        entry.attention = new_attention;
        drop(tracker);
        // Every transition is ledgered with its evidence so a wrong guess is
        // auditable (docs/design/keyboard-lease-and-ledger.md §6 M3).
        if previous_agent != new_agent || previous_attention != new_attention {
            self.ledger_note(
                pane_id,
                "attention.changed",
                json!({
                    "agent": new_agent,
                    "from": previous_attention,
                    "to": new_attention,
                    "evidence": "screen",
                }),
            );
        }
        if previous_mode != new_mode {
            self.note_mode_change(
                pane_id,
                new_agent.as_deref(),
                previous_mode,
                new_mode.clone(),
            );
        }
        self.broadcast(&DaemonEvent::agent_state(
            pane_id.to_string(),
            new_agent,
            new_attention,
            new_mode,
        ));
    }

    /// A permission-mode change is its own ledger record: "the agent went
    /// unattended at 14:02" is exactly the line an audit wants to find.
    pub(crate) fn note_mode_change(
        &self,
        pane_id: &str,
        agent: Option<&str>,
        from: Option<String>,
        to: Option<String>,
    ) {
        let unattended = is_unattended_mode(to.as_deref());
        self.ledger_note(
            pane_id,
            "mode.changed",
            json!({
                "agent": agent,
                "from": from,
                "to": to,
                "unattended": unattended,
                "evidence": "screen",
            }),
        );
    }

    /// (M3b) An official reading from `claude agents --json` for a pane whose
    /// process tree contains that session. It outranks screen classification
    /// until `ttl` elapses without a refresh, then the heuristic resumes.
    /// Transitions are ledgered with evidence `claude-agents`.
    /// Returns true when this is the pane's first official reading (the
    /// caller logs the acquisition once).
    pub(crate) fn apply_official_attention(
        &self,
        pane_id: &str,
        agent: &str,
        attention: AgentAttention,
        ttl: Duration,
    ) -> bool {
        self.apply_official_attention_with(pane_id, agent, attention, ttl, "claude-agents")
    }

    pub(crate) fn apply_official_attention_with(
        &self,
        pane_id: &str,
        agent: &str,
        attention: AgentAttention,
        ttl: Duration,
        evidence: &'static str,
    ) -> bool {
        let Ok(mut tracker) = self.agents.lock() else {
            return false;
        };
        let entry = tracker.panes.entry(pane_id.to_string()).or_default();
        if entry.ended {
            return false;
        }
        // First reading, or the first after a lapse: worth one log line.
        let newly_official = entry
            .official_until
            .is_none_or(|until| until <= Instant::now());
        entry.official_until = Some(Instant::now() + ttl);
        let new_agent = Some(agent.to_string());
        let new_attention = Some(attention);
        if entry.agent == new_agent && entry.attention == new_attention {
            return newly_official;
        }
        let previous_attention = entry.attention;
        let mode = entry.mode.clone();
        entry.agent = new_agent.clone();
        entry.attention = new_attention;
        drop(tracker);
        self.ledger_note(
            pane_id,
            "attention.changed",
            json!({
                "agent": new_agent,
                "from": previous_attention,
                "to": new_attention,
                "evidence": evidence,
            }),
        );
        self.broadcast(&DaemonEvent::agent_state(
            pane_id.to_string(),
            new_agent,
            new_attention,
            mode,
        ));
        newly_official
    }

    /// (M3b) The official session a pane was mapped to is gone from the
    /// listing (two probes in a row): drop the official reading and, unless
    /// the pane is manually marked, clear its agent badge — the screen
    /// heuristic would otherwise keep a stale "claude · idle" over the
    /// shell prompt that replaced the agent.
    pub(crate) fn clear_official_attention(&self, pane_id: &str) {
        let Ok(mut tracker) = self.agents.lock() else {
            return;
        };
        let Some(entry) = tracker.panes.get_mut(pane_id) else {
            return;
        };
        if entry.official_until.is_none() {
            return;
        }
        entry.official_until = None;
        if entry.manual || (entry.agent.is_none() && entry.attention.is_none()) {
            return;
        }
        let previous_agent = entry.agent.take();
        let previous_attention = entry.attention.take();
        entry.mode = None;
        drop(tracker);
        self.ledger_note(
            pane_id,
            "attention.changed",
            json!({
                "agent": previous_agent,
                "from": previous_attention,
                "to": Value::Null,
                "evidence": "claude-agents: session gone",
            }),
        );
        self.broadcast(&DaemonEvent::agent_state(
            pane_id.to_string(),
            None,
            None,
            None,
        ));
    }

    /// (T1) Set or clear a pane's manual agent mark. `Some(name)` marks the
    /// pane (overriding auto-detection); `None` returns it to auto-detection.
    /// Only the flag/mark is updated here — the caller then runs
    /// `classify_agent_now` to recompute state and broadcast any transition.
    pub(crate) fn set_manual_agent(&self, pane_id: &str, agent: Option<String>) {
        if let Ok(mut tracker) = self.agents.lock() {
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            match agent {
                Some(name) => {
                    entry.manual = true;
                    entry.agent = Some(name);
                }
                None => {
                    entry.manual = false;
                    // (T1) M3: a manual unmark clears the mark itself, so the
                    // next classification re-detects with the FRESH 2-group
                    // threshold — the 1-group hysteresis floor only applies
                    // to genuinely auto-detected panes.
                    entry.agent = None;
                    entry.zero_signature_streak = 0;
                }
            }
        }
    }

    /// (T1) The pane's (manual, agent) mark pair — captured before a
    /// SetPaneAgent so a failed persist can revert (L8).
    pub(crate) fn agent_mark(&self, pane_id: &str) -> (bool, Option<String>) {
        self.agents
            .lock()
            .ok()
            .and_then(|tracker| {
                tracker
                    .panes
                    .get(pane_id)
                    .map(|entry| (entry.manual, entry.agent.clone()))
            })
            .unwrap_or((false, None))
    }

    /// (T1) Restore a (manual, agent) mark pair captured by `agent_mark`:
    /// the revert half of a SetPaneAgent whose persist failed (L8), keeping
    /// disk, memory, and clients from diverging.
    pub(crate) fn restore_agent_mark(&self, pane_id: &str, mark: (bool, Option<String>)) {
        if let Ok(mut tracker) = self.agents.lock() {
            let entry = tracker.panes.entry(pane_id.to_string()).or_default();
            entry.manual = mark.0;
            entry.agent = mark.1;
            entry.zero_signature_streak = 0;
        }
    }

    /// (T1) A pane's process ended (or was killed for a restart): a dead
    /// agent is neither working nor waiting, so its attention state clears
    /// and no automatic classification runs until the next spawn (the
    /// `ended` flag — a trailing-edge classification must not resurrect a
    /// badge from the preserved final screen). The agent MARK is kept: the
    /// signature is still on screen, and a manual mark outlives its process.
    /// Broadcasts the final AgentState transition if the pane had attention.
    pub(crate) fn clear_agent_attention(&self, pane_id: &str) {
        let cleared = self.agents.lock().ok().and_then(|mut tracker| {
            let entry = tracker.panes.get_mut(pane_id)?;
            entry.ended = true;
            let previous = entry.attention.take()?;
            entry.last_attention = Some(previous);
            Some((entry.agent.clone(), previous, entry.mode.clone()))
        });
        if let Some((Some(agent), previous, mode)) = cleared {
            self.ledger_note(
                pane_id,
                "attention.changed",
                json!({
                    "agent": agent,
                    "from": previous,
                    "to": Value::Null,
                    "evidence": "process ended",
                }),
            );
            self.broadcast(&DaemonEvent::agent_state(
                pane_id.to_string(),
                Some(agent),
                None,
                mode,
            ));
        }
    }

    /// (T1) The manual agent marks to persist (pane_id → agent name). Detected
    /// (non-manual) state is deliberately excluded: it re-derives from the
    /// screen after a restart.
    pub(crate) fn manual_agent_marks(&self) -> HashMap<String, String> {
        self.agents
            .lock()
            .map(|tracker| {
                tracker
                    .panes
                    .iter()
                    .filter(|(_, entry)| entry.manual)
                    .filter_map(|(pane_id, entry)| {
                        entry.agent.clone().map(|agent| (pane_id.clone(), agent))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (T1) Restore persisted manual marks at daemon startup. Entries start
    /// unclassified (attention None); the pane's next output (or an explicit
    /// SetPaneAgent) classifies from the live screen.
    pub(crate) fn seed_manual_agents(&self, marks: HashMap<String, String>) {
        if let Ok(mut tracker) = self.agents.lock() {
            for (pane_id, agent) in marks {
                tracker.panes.insert(
                    pane_id,
                    AgentPaneState {
                        agent: Some(agent),
                        manual: true,
                        ..Default::default()
                    },
                );
            }
        }
    }

    /// (T1) Current agent info for one pane (default/empty when untracked).
    pub(crate) fn agent_state(&self, pane_id: &str) -> AgentPaneInfo {
        self.agents
            .lock()
            .ok()
            .and_then(|tracker| tracker.panes.get(pane_id).map(AgentPaneState::info))
            .unwrap_or_default()
    }

    /// (T1) Agent info for every tracked pane — find reads it once instead of
    /// locking per pane.
    pub(crate) fn agent_info_map(&self) -> HashMap<String, AgentPaneInfo> {
        self.agents
            .lock()
            .map(|tracker| {
                tracker
                    .panes
                    .iter()
                    .map(|(pane_id, entry)| (pane_id.clone(), entry.info()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (T1) Agent info for panes with a KNOWN agent (manual or detected) — the
    /// bootstrap payload's per-pane agent map.
    pub(crate) fn agent_states(&self) -> HashMap<String, AgentPaneInfo> {
        self.agents
            .lock()
            .map(|tracker| {
                tracker
                    .panes
                    .iter()
                    .filter(|(_, entry)| entry.agent.is_some())
                    .map(|(pane_id, entry)| (pane_id.clone(), entry.info()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// (T1) Drop a pane's agent tracking when the pane is closed: its mark dies
    /// with it (pane ids are never reused) and persist stops carrying it.
    pub(crate) fn remove_agent(&self, pane_id: &str) {
        if let Ok(mut tracker) = self.agents.lock() {
            tracker.panes.remove(pane_id);
        }
    }

    /// Track a pane resize in its model so post-resize content lays out on the new grid.
    pub(crate) fn resize_model(&self, pane_id: &str, cols: u16, rows: u16) {
        if let Some(model) = self.model_handle(pane_id) {
            if let Ok(mut model) = model.lock() {
                model.set_size(cols, rows);
            }
        }
    }

    /// Drop a pane's screen model when the pane is closed.
    pub(crate) fn remove_model(&self, pane_id: &str) {
        if let Ok(mut models) = self.models.lock() {
            models.remove(pane_id);
        }
    }

    pub(crate) fn emit(&self, pane_id: &str, data: String) {
        // A pane removed via ClosePane must never recreate its scrollback file or
        // deliver further output to subscribers, even if its reader is still draining.
        if self.is_closed(pane_id) {
            return;
        }
        let _ = self.append_scrollback(pane_id, &data);
        self.record_output_tricks(pane_id, &data);

        let event = DaemonEvent::PtyOutput {
            pane_id: pane_id.to_string(),
            data,
        };
        self.broadcast(&event);
    }

    pub(crate) fn emit_pane_ended(&self, pane_id: &str, exit_code: Option<i32>) {
        if self.is_closed(pane_id) {
            return;
        }
        tracing::info!(
            workspace_key = %self.log_workspace_key(),
            pane_id = %pane_id,
            exit_code = exit_code.unwrap_or(-1),
            event = "pane_end",
            "pane ended"
        );
        self.ledger_note(
            pane_id,
            "pane.ended",
            self.pane_exit_record(pane_id, exit_code),
        );
        let event = DaemonEvent::PaneEnded {
            pane_id: pane_id.to_string(),
            exit_code,
        };
        self.broadcast(&event);
    }

    /// The `pane.ended` payload: the exit code plus what a reviewer (or a
    /// Kranz gate) classifies the run on. `attention` is the state the agent
    /// was in when its process ended (`needs_input` = it was still waiting on
    /// a person), `holder` the keyboard holder at exit, `output_tricks` the
    /// guard's totals when any fired. Every key beyond `exit_code` is
    /// additive; older readers ignore them.
    pub(crate) fn pane_exit_record(&self, pane_id: &str, exit_code: Option<i32>) -> Value {
        let (agent, attention, mode, unattended) = self
            .agents
            .lock()
            .ok()
            .and_then(|tracker| {
                tracker.panes.get(pane_id).map(|entry| {
                    (
                        entry.agent.clone(),
                        entry.last_attention.or(entry.attention),
                        entry.mode.clone(),
                        is_unattended_mode(entry.mode.as_deref()),
                    )
                })
            })
            .unwrap_or((None, None, None, false));
        let mut payload = json!({
            "exit_code": exit_code,
            "agent": agent,
            "attention": attention,
            "mode": mode,
            "unattended": unattended,
            "holder": self.lease_holder(pane_id),
        });
        let tricks = self.output_tricks(pane_id);
        if tricks.total() > 0 {
            payload["output_tricks"] = json!(tricks);
        }
        payload
    }

    pub(crate) fn broadcast(&self, event: &DaemonEvent) {
        // (M11) Skip the serialization entirely when nobody is listening —
        // PtyOutput is the hottest event and the daemon routinely runs with
        // zero subscribers (e.g. between GUI sessions).
        if self
            .subscribers
            .lock()
            .map(|subscribers| subscribers.is_empty())
            .unwrap_or(true)
        {
            return;
        }
        let Ok(payload_json) = serde_json::to_vec(&event) else {
            return;
        };

        // The JSON payload is serialized once; the two wire encodings (newline v1 and
        // framed v2) are built lazily — at most once each per broadcast — so the
        // shared-Arc fan-out is preserved even with mixed-version subscribers.
        let mut newline: Option<Arc<Vec<u8>>> = None;
        let mut framed: Option<Arc<Vec<u8>>> = None;

        // Only a non-blocking try_send happens under the lock; the actual socket write
        // is done by each subscriber's writer thread. A subscriber whose queue is full
        // (a consumer too slow to keep up) or whose writer thread has exited is dropped.
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| {
                let cached = if subscriber.wire_version >= frame::WIRE_VERSION {
                    &mut framed
                } else {
                    &mut newline
                };
                let payload = match cached {
                    Some(payload) => Arc::clone(payload),
                    None => match encode_event_for_wire(subscriber.wire_version, &payload_json) {
                        Some(bytes) => {
                            let arc = Arc::new(bytes);
                            *cached = Some(Arc::clone(&arc));
                            arc
                        }
                        // An event too large to frame is skipped for this subscriber
                        // (rather than corrupting its stream); keep it subscribed.
                        None => return true,
                    },
                };
                !matches!(
                    subscriber.sender.try_send(payload),
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_))
                )
            });
        }
    }

    pub(crate) fn append_scrollback(&self, pane_id: &str, data: &str) -> Result<(), String> {
        // Clone the per-pane append state out of the map under a tiny lock
        // (creating it on first append), then do ALL I/O under the per-pane
        // state lock (M11): no open/close/stat per chunk on the hot path, and
        // the state lock serializes appends with the cap rewrite below so no
        // chunk can land in the cap's read→rename window.
        let state = {
            let mut handles = self
                .append_handles
                .lock()
                .map_err(|_| "scrollback append-handles lock poisoned".to_string())?;
            match handles.get(pane_id) {
                Some(state) => Arc::clone(state),
                None => {
                    let path = scrollback_path(&self.scrollback_dir, pane_id);
                    let file = match open_scrollback_append(&path) {
                        Ok(file) => file,
                        // The scrollback dir vanished at runtime (e.g. manual
                        // cleanup): recreate it once and retry, instead of
                        // paying a mkdir+chmod on every chunk.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            ensure_private_dir(&self.scrollback_dir)?;
                            open_scrollback_append(&path)
                                .map_err(|error| format!("failed to open scrollback: {error}"))?
                        }
                        Err(error) => {
                            return Err(format!("failed to open scrollback: {error}"));
                        }
                    };
                    // The byte count is unknown on first open: stat ONCE here to
                    // seed the in-memory count (M11: stat is the fallback when
                    // the count is unknown, never per-chunk).
                    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                    let state = Arc::new(Mutex::new(ScrollbackAppendState { file, len }));
                    handles.insert(pane_id.to_string(), Arc::clone(&state));
                    state
                }
            }
        };

        let mut state = state
            .lock()
            .map_err(|_| "scrollback append lock poisoned".to_string())?;
        state
            .file
            .write_all(data.as_bytes())
            .map_err(|error| format!("failed to write scrollback: {error}"))?;
        state.len += data.len() as u64;

        if state.len > SCROLLBACK_MAX_BYTES {
            // The cap rewrites the file (temp + rename), replacing the inode the
            // cached handle points at. Still holding the state lock — so no
            // append can interleave — cap, then re-open the new file and
            // re-prime the count (the only other stat on this path).
            cap_scrollback_file(&self.scrollback_dir, pane_id)?;
            let path = scrollback_path(&self.scrollback_dir, pane_id);
            let file = open_scrollback_append(&path)
                .map_err(|error| format!("failed to open scrollback: {error}"))?;
            state.len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
            state.file = file;
        }
        Ok(())
    }

    /// Drop a pane's cached append handle when its scrollback file is deleted
    /// (ClosePane / create rollback), so a later append can't keep writing to
    /// the unlinked inode (M11). Restart needs no invalidation: the file is
    /// untouched, so the handle and byte count stay valid.
    pub(crate) fn invalidate_append_handle(&self, pane_id: &str) {
        if let Ok(mut handles) = self.append_handles.lock() {
            handles.remove(pane_id);
        }
    }

    /// Drop cached append handles for panes no longer in the registry: a reader
    /// racing ClosePane can re-create a handle (and file) for a dead pane;
    /// swept on the closed-pane cadence so the map can't grow unboundedly (M11).
    pub(crate) fn prune_orphan_append_handles(&self, live_pane_ids: &HashSet<String>) {
        if let Ok(mut handles) = self.append_handles.lock() {
            handles.retain(|pane_id, _| live_pane_ids.contains(pane_id));
        }
    }
}
