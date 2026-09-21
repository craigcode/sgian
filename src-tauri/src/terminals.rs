use super::*;

pub(crate) struct TerminalStore {
    pub(crate) cwd: PathBuf,
    pub(crate) sessions: HashMap<String, TerminalSession>,
    pub(crate) sizes: HashMap<String, PtySize>,
    pub(crate) liveness: Arc<Mutex<HashMap<String, PaneLiveness>>>,
    pub(crate) next_generation: u64,
    pub(crate) router: OutputRouter,
    pub(crate) shell: ShellConfig,
    /// Per-pane shell overrides from named profiles (frozen at create time;
    /// persisted across daemon restarts and preserved across in-process restart).
    pub(crate) pane_shells: HashMap<String, ShellConfig>,
    /// Panes whose spawn (openpty + fork/exec) is currently running WITHOUT the
    /// store lock held (M7). Checked/inserted/removed only under the lock;
    /// paired with DaemonServer::spawn_cvar so a concurrent ensure for the same
    /// pane waits for the in-flight spawn to commit instead of double-spawning.
    pub(crate) spawning: HashSet<String>,
    /// (T2) Live agent sessions by pane id (unix + Windows: agent panes spawn
    /// a headless `claude` CLI with piped stdio; see the T2 section below).
    /// Shares `liveness`, `spawning`, and `next_generation` with PTY sessions
    /// so runtime state, PaneEnded, and the M7 spawn discipline apply
    /// uniformly. On platforms that are neither unix nor Windows the map never
    /// fills (spawn fails with a clean cfg error first).
    #[cfg(any(unix, windows))]
    pub(crate) agent_sessions: HashMap<String, AgentSession>,
    /// (T2) pane id → last known `claude` session id, seeded from persisted
    /// `agents_v2` at daemon start. Consulted for `--resume` when a pane has
    /// no live session to take the id from.
    #[cfg(any(unix, windows))]
    pub(crate) agent_resume: HashMap<String, String>,
    /// Immutable provider/model identity for agent panes, including restored
    /// panes whose process has not been started yet.
    pub(crate) agent_specs: HashMap<String, AgentPaneSpec>,
    /// (T2) Agent spawn config (binary override + permission mode), mirrored
    /// from Config and refreshed by the config file-watch like `shell`.
    /// Read only by the agent-spawn path (unix + Windows).
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    pub(crate) agent_config: AgentSpawnConfig,
    /// (T2) Directory holding per-pane agent conversation logs
    /// (`<data_dir>/agents/<pane-id>.jsonl`); read by the bootstrap replay.
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    pub(crate) agents_dir: PathBuf,
    /// (T2) The daemon's lazy-persist flag: a reader thread sets it when it
    /// records a CLI session id, so agents_v2 reaches workspace.json within a
    /// persist cadence instead of only at shutdown.
    #[cfg_attr(not(any(unix, windows)), allow(dead_code))]
    pub(crate) agent_dirty: Arc<AtomicBool>,
}

pub(crate) fn default_pty_size() -> PtySize {
    pty_size(120, 40)
}

/// Kill a just-spawned child and reap it (spawn partial-failure cleanup): when
/// PTY writer/reader setup fails after `spawn_command` succeeded, the child must
/// not be abandoned — unwatched it would keep running with no liveness entry (a
/// later ensure would spawn a second shell) and un-reaped.
pub(crate) fn kill_and_reap_child(mut child: Box<dyn portable_pty::Child + Send + Sync>) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Everything a spawn needs out of the TerminalStore (M7): read once under the
/// store lock so the expensive PTY setup (`execute_spawn`) can run WITHOUT it.
pub(crate) struct SpawnPlan {
    pub(crate) size: PtySize,
    pub(crate) shell: String,
    pub(crate) args: Vec<String>,
    pub(crate) env: HashMap<String, String>,
    pub(crate) scrub_env: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) command_str: String,
    pub(crate) cwd_str: String,
}

/// A spawned-but-not-yet-committed pane session (M7): the expensive half of a
/// spawn, produced by `execute_spawn` off the store lock and committed under it
/// by `TerminalStore::commit_spawn`.
pub(crate) struct PreparedSpawn {
    pub(crate) master: Box<dyn MasterPty + Send>,
    pub(crate) child: Box<dyn portable_pty::Child + Send + Sync>,
    pub(crate) killer: Box<dyn ChildKiller + Send + Sync>,
    pub(crate) writer: Box<dyn Write + Send>,
    pub(crate) reader: Box<dyn Read + Send>,
    pub(crate) command_str: String,
    pub(crate) cwd_str: String,
}

/// The expensive half of a spawn — openpty + fork/exec + reader/writer setup —
/// run WITHOUT the TerminalStore lock (M7: a slow spawn used to stall input,
/// resize, and liveness for every pane). On a partial failure after
/// `spawn_command` succeeded, the child is killed + reaped (review-low).
/// Whether an `openpty` failure is worth a brief retry: the kernel's pty pool
/// momentarily exhausted (macOS ENXIO "Device not configured", EAGAIN on
/// either platform) rather than a configuration error.
pub(crate) fn is_transient_pty_error(message: &str) -> bool {
    message.contains("Device not configured")
        || message.contains("Resource temporarily unavailable")
        || message.contains("os error 6)")
        || message.contains("os error 11)")
        || message.contains("os error 35)")
}

/// `openpty` with a short bounded retry on transient pool exhaustion (seen
/// under parallel test load on CI runners); anything else fails immediately.
pub(crate) fn open_pty_with_retry(
    pty_system: &dyn portable_pty::PtySystem,
    size: PtySize,
) -> Result<portable_pty::PtyPair, String> {
    let mut attempt: u32 = 0;
    loop {
        match pty_system.openpty(size) {
            Ok(pair) => return Ok(pair),
            Err(error) => {
                let message = error.to_string();
                attempt += 1;
                if !is_transient_pty_error(&message) || attempt >= 8 {
                    return Err(format!("failed to open pty: {message}"));
                }
                thread::sleep(Duration::from_millis(25 * u64::from(attempt)));
            }
        }
    }
}

pub(crate) fn execute_spawn(plan: &SpawnPlan) -> Result<PreparedSpawn, String> {
    let pty_system = native_pty_system();
    let pair = open_pty_with_retry(pty_system.as_ref(), plan.size)?;

    let mut command = CommandBuilder::new(&plan.shell);
    for arg in &plan.args {
        command.arg(arg);
    }
    command.cwd(&plan.cwd);
    // Opt-in environment scrubbing: remove sensitive inherited vars before
    // the pane's shell sees them. Default (empty scrub list) preserves the
    // full inherited environment. Explicit `env` entries (including the
    // TERM/COLORTERM defaults below) are applied AFTER scrubbing, so an
    // operator-set value takes precedence over the scrub list for the same
    // variable name. VAL-SEC-003/004/007.
    for key in INHERITED_SESSION_MARKERS {
        command.env_remove(key);
    }
    for key in &plan.scrub_env {
        command.env_remove(key);
    }
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for (key, value) in &plan.env {
        command.env(key, value);
    }

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("failed to spawn shell: {error}"))?;
    // Split a killer off the child: the session keeps the killer to terminate
    // the process on close/restart, while the reader thread takes ownership of
    // `child` so it can reap it via `child.wait()` (the authoritative exit code).
    let killer = child.clone_killer();
    // (review-low) A failure setting up the writer/reader AFTER the child
    // spawned must not abandon it: kill + reap, or the shell leaks unwatched
    // (no liveness entry exists yet, so a later ensure would spawn a second).
    let writer = match pair.master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            kill_and_reap_child(child);
            return Err(format!("failed to open pty writer: {error}"));
        }
    };
    let reader = match pair.master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            kill_and_reap_child(child);
            return Err(format!("failed to open pty reader: {error}"));
        }
    };

    Ok(PreparedSpawn {
        master: pair.master,
        child,
        killer,
        writer,
        reader,
        command_str: plan.command_str.clone(),
        cwd_str: plan.cwd_str.clone(),
    })
}

/// Upper bounds on a pane's grid. Every size — resize requests (u16s straight
/// from a client), persisted sizes, and the vt100 model resize — funnels through
/// `pty_size`, so this is the single clamp that keeps a hostile/hand-edited
/// 65535×65535 from allocating a ~4.3-billion-cell screen model and OOM-killing
/// the daemon (and every shell it owns). 2000×1000 (2M cells) comfortably exceeds
/// any real display — an 8K monitor at a tiny 6 px cell is ~1280 columns — while
/// bounding the worst-case model allocation to tens of megabytes.
pub(crate) const MAX_PTY_COLS: u16 = 2000;
pub(crate) const MAX_PTY_ROWS: u16 = 1000;

pub(crate) fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.clamp(1, MAX_PTY_ROWS),
        cols: cols.clamp(2, MAX_PTY_COLS),
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl TerminalStore {
    /// Apply a reloaded config: the default shell and agent spawn config for
    /// future spawns, and the scrub list for every stored per-pane shell too.
    /// A profiled pane keeps its own shell/args/env, but the scrub list is
    /// workspace policy and must follow the reload so a newly added secret
    /// name applies on that pane's next restart (S8 of the 2026-09-20 review).
    pub(crate) fn apply_reloaded_config(&mut self, shell: ShellConfig, agent: AgentSpawnConfig) {
        for stored in self.pane_shells.values_mut() {
            stored.scrub_env = shell.scrub_env.clone();
        }
        self.shell = shell;
        self.agent_config = agent;
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        cwd: PathBuf,
        router: OutputRouter,
        sizes: HashMap<String, PtySize>,
        shell: ShellConfig,
        agent_config: AgentSpawnConfig,
        agents_dir: PathBuf,
        agent_dirty: Arc<AtomicBool>,
        agent_specs: HashMap<String, AgentPaneSpec>,
    ) -> Self {
        Self {
            cwd,
            sessions: HashMap::new(),
            sizes,
            liveness: Arc::new(Mutex::new(HashMap::new())),
            next_generation: 0,
            router,
            shell,
            pane_shells: HashMap::new(),
            spawning: HashSet::new(),
            #[cfg(any(unix, windows))]
            agent_sessions: HashMap::new(),
            #[cfg(any(unix, windows))]
            agent_resume: HashMap::new(),
            agent_specs,
            agent_config,
            agents_dir,
            agent_dirty,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_tests(cwd: PathBuf) -> Self {
        Self::new(
            cwd,
            OutputRouter::new(std::env::temp_dir()),
            HashMap::new(),
            ShellConfig::default(),
            AgentSpawnConfig::default(),
            std::env::temp_dir().join("sgian-test-agents"),
            Arc::new(AtomicBool::new(false)),
            HashMap::new(),
        )
    }

    /// Seed the liveness map with `ended: true` entries for panes whose Ended state
    /// was persisted across a daemon restart. This ensures `runtime_states` reports
    /// the correct state for restored panes that have not been (re)spawned yet.
    /// A subsequent `spawn_pane` replaces the entry with `ended: false`.
    pub(crate) fn seed_ended_panes(&mut self, pane_ids: &[String]) {
        if pane_ids.is_empty() {
            return;
        }
        if let Ok(mut liveness) = self.liveness.lock() {
            for pane_id in pane_ids {
                // Only seed if there is no existing entry (don't overwrite a live pane).
                liveness.entry(pane_id.clone()).or_insert(PaneLiveness {
                    generation: 0,
                    ended: true,
                    command: None,
                    cwd: None,
                    exit_code: None,
                    pid: None,
                });
            }
        }
    }

    /// A pane is live when its current-generation reader is still running. A session
    /// whose shell exited (reader hit EOF) is reported not-live so it can be respawned.
    pub(crate) fn is_live(&self, pane_id: &str) -> bool {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .get(pane_id)
                    .map(|entry| !entry.ended)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }

    /// A pane with a spawn IN FLIGHT (M7) has no liveness entry yet, so `is_live`
    /// reports false during the fork/exec window — `wait --exit` must not resolve a
    /// spurious "exit" there, so callers asking "has this pane ended?" use this.
    pub(crate) fn is_live_or_spawning(&self, pane_id: &str) -> bool {
        self.spawning.contains(pane_id) || self.is_live(pane_id)
    }

    /// Read everything a spawn needs out of the store (M7): the cheap half of a
    /// spawn, taken under the lock so the expensive PTY setup (`execute_spawn`)
    /// can run WITHOUT the store lock.
    pub(crate) fn plan_spawn(&self, pane_id: &str) -> SpawnPlan {
        let cfg = self.pane_shells.get(pane_id).unwrap_or(&self.shell);
        let shell = if cfg.shell.is_empty() {
            default_shell()
        } else {
            cfg.shell.clone()
        };
        // Capture spawn metadata (launched command + resolved working dir) so it is
        // queryable via snapshot/find for the life of the pane — including after it
        // has ended, until it is closed or restarted.
        let command_str = if cfg.args.is_empty() {
            shell.clone()
        } else {
            format!("{} {}", shell, cfg.args.join(" "))
        };
        SpawnPlan {
            size: self
                .sizes
                .get(pane_id)
                .copied()
                .unwrap_or_else(default_pty_size),
            shell,
            args: cfg.args.clone(),
            env: cfg.env.clone(),
            scrub_env: cfg.scrub_env.clone(),
            cwd: self.cwd.clone(),
            command_str,
            cwd_str: self.cwd.to_string_lossy().to_string(),
        }
    }

    /// Single-caller path (unit tests): plan → execute → commit, all under
    /// the caller's store lock. Production spawns go through DaemonServer's
    /// ensure/restart, which run `execute_spawn` WITHOUT the lock (M7) — so
    /// this wrapper is `#[cfg(test)]`, like the other test-only helpers.
    #[cfg(test)]
    pub(crate) fn spawn_pane(&mut self, pane_id: &str) -> Result<(), String> {
        let plan = self.plan_spawn(pane_id);
        let prepared = execute_spawn(&plan)?;
        self.commit_spawn(pane_id, plan.size, prepared);
        Ok(())
    }

    /// Commit a spawned session under the store lock (M7): bump the generation,
    /// install liveness/model/session/sizes, and start the reader thread. This
    /// is the only part of a spawn that mutates the store — the expensive
    /// fork/exec in `execute_spawn` runs without the lock, so a slow spawn no
    /// longer stalls input/resize/liveness for every other pane.
    pub(crate) fn commit_spawn(&mut self, pane_id: &str, size: PtySize, prepared: PreparedSpawn) {
        let PreparedSpawn {
            master,
            child,
            killer,
            writer,
            mut reader,
            command_str,
            cwd_str,
        } = prepared;

        self.next_generation += 1;
        let generation = self.next_generation;
        let child_pid = child.process_id();
        if let Ok(mut liveness) = self.liveness.lock() {
            liveness.insert(
                pane_id.to_string(),
                PaneLiveness {
                    generation,
                    ended: false,
                    command: Some(command_str),
                    cwd: Some(cwd_str),
                    exit_code: None,
                    pid: child_pid,
                },
            );
        }

        // Create (or reset, on an in-place restart) the pane's vt100 screen model
        // before the reader starts feeding it. `ensure_model` preserves the
        // monotonic revision across a restart.
        self.router.ensure_model(pane_id, size.cols, size.rows);

        let output_pane_id = pane_id.to_string();
        let output_router = self.router.clone();
        let liveness = Arc::clone(&self.liveness);
        thread::spawn(move || {
            // Take ownership of the child so this reader can reap it (`child.wait()`)
            // at EOF to capture the authoritative exit status.
            let mut child = child;
            // Activate structured logging for this reader thread (best-effort:
            // no-op if no subscriber was configured, e.g. in unit tests).
            let _log_guard = output_router.log_guard();
            // A reader only emits while it owns the pane's current generation; a
            // restart or close supersedes it, and a superseded reader must not keep
            // leaking its dying session's output into the replacement session's
            // stream. The check is per-chunk (not atomic with the emit), so at most
            // one already-read chunk can slip through on a restart.
            let owns_session = || {
                liveness
                    .lock()
                    .ok()
                    .map(|map| {
                        map.get(&output_pane_id)
                            .is_some_and(|entry| entry.generation == generation)
                    })
                    .unwrap_or(false)
            };

            let mut buffer = [0_u8; 8192];
            let mut pending_utf8 = Vec::new();
            loop {
                let byte_count = match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(byte_count) => byte_count,
                    Err(_) => break,
                };
                if !owns_session() {
                    // Superseded (restart/close killed our child): still reap it.
                    // Returning without wait() left a zombie for the daemon's
                    // lifetime (M5); the child is already killed, so this is prompt.
                    let _ = child.wait();
                    return;
                }

                // Feed the SAME raw PTY bytes into the screen model. vt100 keeps its
                // own UTF-8 state across calls, so it gets the full byte stream
                // (unlike `emit`, which holds back split multibyte tails). The parser
                // lock is held only for this call, never across the PTY read above.
                output_router.feed_model(&output_pane_id, &buffer[..byte_count]);

                pending_utf8.extend_from_slice(&buffer[..byte_count]);
                for data in drain_complete_utf8(&mut pending_utf8) {
                    output_router.emit(&output_pane_id, data);
                }
            }

            if !pending_utf8.is_empty() && owns_session() {
                let data = String::from_utf8_lossy(&pending_utf8).to_string();
                output_router.emit(&output_pane_id, data);
            }

            // Reap the child to obtain the authoritative exit status. EOF means the
            // slave closed (the process has exited or is exiting), so this returns
            // promptly. Running the reaper on THIS reader thread, at the natural reap
            // point, means the captured code and the single PaneEnded emission can
            // never race a second emitter (Invariant 1); `wait()` is authoritative,
            // and an errored wait falls back to a `None` code without double-emitting.
            let exit_code = match child.wait() {
                Ok(status) => reaped_exit_code(&status),
                Err(_) => None,
            };

            // Only report the pane ended if this reader still owns the live session;
            // a newer session (restart) or a ClosePane will have superseded us, in
            // which case its (replaced/removed) entry's generation no longer matches
            // and we claim nothing — so a stale reader never ends a live pane and
            // PaneEnded fires exactly once per generation.
            let claimed = liveness
                .lock()
                .ok()
                .and_then(|mut liveness| {
                    liveness.get_mut(&output_pane_id).map(|entry| {
                        if entry.generation == generation && !entry.ended {
                            entry.ended = true;
                            entry.exit_code = exit_code;
                            true
                        } else {
                            false
                        }
                    })
                })
                .unwrap_or(false);
            if claimed {
                // (T1) M2: a dead agent must not keep a working/needs-input
                // badge — clear the attention half of its tracked state (the
                // mark itself stays; the signature is on the final screen).
                output_router.clear_agent_attention(&output_pane_id);
                output_router.emit_pane_ended(&output_pane_id, exit_code);
            }
        });

        self.sessions.insert(
            pane_id.to_string(),
            TerminalSession {
                _master: master,
                pid: child_pid,
                #[cfg(windows)]
                _job: child_pid.and_then(KillOnCloseJob::attach),
                killer,
                input: spawn_input_writer(writer),
            },
        );
        self.sizes.insert(pane_id.to_string(), size);
    }

    pub(crate) fn close_pane(&mut self, pane_id: &str) {
        self.sessions.remove(pane_id);
        self.pane_shells.remove(pane_id);
        // (T2) Dropping the AgentSession denies its pending approvals (waking
        // a blocked reader) and kills the CLI (SIGTERM→SIGKILL on unix,
        // TerminateProcess on Windows).
        #[cfg(any(unix, windows))]
        {
            self.agent_sessions.remove(pane_id);
            self.agent_resume.remove(pane_id);
        }
        self.agent_specs.remove(pane_id);
        self.sizes.remove(pane_id);
        if let Ok(mut liveness) = self.liveness.lock() {
            liveness.remove(pane_id);
        }
        // (T1) M2: same attention clear as the reader-EOF path — a restart
        // kills the process without a claimed EOF, and a dead agent must not
        // keep its badge. On ClosePane the tracker entry is already gone
        // (remove_agent runs first), so this is a no-op there.
        self.router.clear_agent_attention(pane_id);
    }

    /// Kill every live session's child directly. Daemon shutdown must not rely
    /// on `TerminalSession::drop` alone: lingering connection threads hold
    /// `Arc<DaemonServer>` clones, which can defer the store's drop indefinitely
    /// and leak SIGHUP-ignoring children past daemon exit (L17).
    pub(crate) fn kill_all_sessions(&mut self) {
        for session in self.sessions.values_mut() {
            #[cfg(unix)]
            if let Some(pid) = session.pid {
                terminate_process_tree(pid);
            }
            let _ = session.killer.kill();
        }
        // (T2) Same for agent CLIs; each AgentSession's Drop also fires, but a
        // deferred store drop would defer it past daemon exit (L17 above).
        #[cfg(any(unix, windows))]
        for session in self.agent_sessions.values() {
            session.kill();
        }
    }

    /// Test-only store-level restart (production restarts go through
    /// DaemonServer::restart_terminal, which runs the fork/exec off the lock —
    /// M7). close_pane drops the sizes entry, which would silently reset the
    /// pane to the default 120x40 on respawn (review-low): preserve the size so
    /// the restarted pane keeps its dimensions (persisted map AND new PTY).
    #[cfg(test)]
    pub(crate) fn restart_pane(&mut self, pane_id: &str) -> Result<(), String> {
        let size = self.sizes.get(pane_id).copied();
        let shell = self.pane_shells.get(pane_id).cloned();
        self.close_pane(pane_id);
        if let Some(size) = size {
            self.sizes.insert(pane_id.to_string(), size);
        }
        if let Some(shell) = shell {
            self.pane_shells.insert(pane_id.to_string(), shell);
        }
        self.spawn_pane(pane_id)
    }

    /// Read a pane's spawn/exit metadata (launched command, working dir, captured
    /// exit code) out of its generation-tagged liveness entry. Returns the default
    /// (all `None`) for an unknown pane. Stays populated for an ended pane until it
    /// is closed/restarted (VAL-TERM-021).
    pub(crate) fn pane_meta(&self, pane_id: &str) -> PaneMeta {
        self.liveness
            .lock()
            .ok()
            .and_then(|liveness| {
                liveness.get(pane_id).map(|entry| PaneMeta {
                    command: entry.command.clone(),
                    cwd: entry.cwd.clone(),
                    exit_code: entry.exit_code,
                })
            })
            .unwrap_or_default()
    }

    pub(crate) fn runtime_states(&self, pane_ids: &[String]) -> HashMap<String, PaneRuntimeState> {
        let liveness = self.liveness.lock().ok();
        pane_ids
            .iter()
            .map(|pane_id| {
                let live = liveness
                    .as_ref()
                    .and_then(|liveness| liveness.get(pane_id))
                    .map(|entry| !entry.ended)
                    .unwrap_or(false);
                let state = if live {
                    PaneRuntimeState::Live
                } else {
                    PaneRuntimeState::Ended
                };
                (pane_id.clone(), state)
            })
            .collect()
    }

    /// Queue input to a pane. The blocking PTY write happens on the pane's own
    /// writer thread — never here, where the caller holds the store mutex (H2).
    pub(crate) fn write_to_pane(&self, pane_id: &str, data: &str) -> Result<(), String> {
        if !self.is_live(pane_id) {
            return Err(format!("terminal session ended: {pane_id}"));
        }

        let session = self
            .sessions
            .get(pane_id)
            .ok_or_else(|| format!("terminal session not found: {pane_id}"))?;
        queue_pane_input(&session.input, pane_id, data)
    }

    /// (M4) Live panes and their spawn cwd (the default Kranz repo).
    pub(crate) fn live_pane_cwds(&self) -> HashMap<String, String> {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .iter()
                    .filter(|(_, entry)| !entry.ended)
                    .filter_map(|(pane_id, entry)| {
                        entry.cwd.clone().map(|cwd| (pane_id.clone(), cwd))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn pane_cwd(&self, pane_id: &str) -> Option<String> {
        self.liveness
            .lock()
            .ok()
            .and_then(|liveness| liveness.get(pane_id).and_then(|entry| entry.cwd.clone()))
    }

    /// (M3b) Live shell panes with a recorded child pid, for the official
    /// agent probe's process-tree mapping.
    pub(crate) fn live_pane_pids(&self) -> Vec<(String, u32)> {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .iter()
                    .filter(|(_, entry)| !entry.ended)
                    .filter_map(|(pane_id, entry)| entry.pid.map(|pid| (pane_id.clone(), pid)))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn live_pane_ids(&self) -> Vec<String> {
        self.liveness
            .lock()
            .map(|liveness| {
                liveness
                    .iter()
                    .filter(|(_, entry)| !entry.ended)
                    .map(|(pane_id, _)| pane_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Write `data` to every live pane except those in `skip` (best-effort);
    /// returns the panes written to. `skip` carries panes whose keyboard is
    /// held by someone other than the writer.
    pub(crate) fn write_to_live_except(&self, data: &str, skip: &HashSet<String>) -> Vec<String> {
        let mut written = Vec::new();
        for pane_id in self.live_pane_ids() {
            if skip.contains(&pane_id) {
                continue;
            }
            if self.write_to_pane(&pane_id, data).is_ok() {
                written.push(pane_id);
            }
        }
        written
    }

    pub(crate) fn resize_pane(
        &mut self,
        pane_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), String> {
        let size = pty_size(cols, rows);

        if let Some(session) = self.sessions.get_mut(pane_id) {
            session
                ._master
                .resize(size)
                .map_err(|error| format!("failed to resize terminal: {error}"))?;
        }

        self.sizes.insert(pane_id.to_string(), size);
        // Resize the screen model to match the PTY so post-resize output lays out on
        // the new grid. This does not bump the model's revision (resize is not output).
        self.router.resize_model(pane_id, size.cols, size.rows);
        Ok(())
    }
}
