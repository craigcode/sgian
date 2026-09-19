use super::*;

// ---------------------------------------------------------------------------
// Keyboard lease and session ledger (docs/design/keyboard-lease-and-ledger.md)
//
// Pure state, predicates, and the hash-chained ledger writer/verifier. The
// DaemonServer handlers call these; clients never re-derive the rules.
// ---------------------------------------------------------------------------

/// Per-pane hash-chained ledgers live here beside `agents/` and `scrollback/`.
/// Unlike those two, a ledger survives pane close: it is the audit record.
pub(crate) const LEDGER_DIR: &str = "ledger";
/// Inside the hash input so a record cannot be re-hashed under another
/// version (the same reasoning as Kranz's `kranz.event-log.v2\n`).
pub(crate) const LEDGER_HASH_PREFIX: &str = "sgian.ledger.v1\n";
pub(crate) const HOLDER_MAX_LEN: usize = 64;
pub(crate) const LEASE_NOTE_MAX_BYTES: usize = 4096;
pub(crate) const LEASE_WHY_MAX_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LeasePolicy {
    /// An unheld pane accepts input from anyone; a held pane only from its holder.
    Open,
    /// Every write needs the lease.
    Required,
}

impl LeasePolicy {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "required" => Some(Self::Required),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Required => "required",
        }
    }
}

/// The held half of a pane's lease. Persisted verbatim in workspace.json.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HeldLease {
    pub(crate) holder: String,
    pub(crate) since_ms: u64,
    #[serde(default)]
    pub(crate) writes: u64,
    #[serde(default)]
    pub(crate) bytes_typed: u64,
    #[serde(default)]
    pub(crate) refused_writes: u64,
    #[serde(default)]
    pub(crate) last_input_ms: Option<u64>,
    /// Monotonic per-workspace lease number. A command that names a
    /// generation is refused when the lease has changed hands since, so a
    /// previous holder's late write, answer or release cannot land on the
    /// current holder's session. 0 for leases persisted before generations.
    #[serde(default)]
    pub(crate) generation: u64,
}

impl HeldLease {
    pub(crate) fn new(holder: &str, since_ms: u64, generation: u64) -> Self {
        Self {
            holder: holder.to_string(),
            since_ms,
            writes: 0,
            bytes_typed: 0,
            refused_writes: 0,
            last_input_ms: None,
            generation,
        }
    }
}

/// Wire shape of a pane's lease (snapshot `leases`, lease responses).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseInfo {
    pub pane_id: String,
    pub policy: String,
    pub holder: Option<String>,
    pub since_ms: Option<u64>,
    pub held_ms: Option<u64>,
    #[serde(default)]
    pub writes: u64,
    #[serde(default)]
    pub bytes_typed: u64,
    #[serde(default)]
    pub refused_writes: u64,
    #[serde(default)]
    pub last_input_ms: Option<u64>,
    /// The lease's generation (see `HeldLease::generation`); present while held.
    #[serde(default)]
    pub generation: Option<u64>,
}

impl LeaseInfo {
    pub(crate) fn from_lease(
        pane_id: &str,
        policy: LeasePolicy,
        lease: Option<&HeldLease>,
        now_ms: u64,
    ) -> Self {
        Self {
            pane_id: pane_id.to_string(),
            policy: policy.as_str().to_string(),
            holder: lease.map(|held| held.holder.clone()),
            since_ms: lease.map(|held| held.since_ms),
            held_ms: lease.map(|held| now_ms.saturating_sub(held.since_ms)),
            writes: lease.map(|held| held.writes).unwrap_or(0),
            bytes_typed: lease.map(|held| held.bytes_typed).unwrap_or(0),
            refused_writes: lease.map(|held| held.refused_writes).unwrap_or(0),
            last_input_ms: lease.and_then(|held| held.last_input_ms),
            generation: lease.map(|held| held.generation),
        }
    }
}

/// Refuse a command that names a lease generation which is no longer the
/// pane's current one (or names one while the pane is unheld).
pub(crate) fn check_generation(
    lease: Option<&HeldLease>,
    generation: Option<u64>,
) -> Result<(), String> {
    match (generation, lease) {
        (None, _) => Ok(()),
        (Some(wanted), Some(held)) if held.generation == wanted => Ok(()),
        (Some(wanted), Some(held)) => Err(format!(
            "stale lease: generation {wanted} is no longer current (now {} held by {})",
            held.generation, held.holder
        )),
        (Some(wanted), None) => Err(format!(
            "stale lease: generation {wanted} is no longer current (pane is unheld)"
        )),
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeaseTransition {
    Taken,
    Released,
    Revoked,
}

/// Holder labels are operator text that ends up in ledgers, status lines and
/// error messages: short, printable ASCII, no whitespace.
pub(crate) fn validate_holder(raw: &str) -> Result<String, String> {
    let holder = raw.trim();
    if holder.is_empty() {
        return Err("holder must not be blank".to_string());
    }
    if holder.len() > HOLDER_MAX_LEN {
        return Err(format!("holder is longer than {HOLDER_MAX_LEN} bytes"));
    }
    if !holder.chars().all(|c| c.is_ascii_graphic()) {
        return Err("holder must be printable ASCII with no whitespace".to_string());
    }
    Ok(holder.to_string())
}

/// Notes and reasons: trimmed, bounded, free text (newlines and tabs allowed,
/// other control characters are not).
pub(crate) fn validate_bounded_text(raw: &str, what: &str, max: usize) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err(format!("{what} must not be empty"));
    }
    if text.len() > max {
        return Err(format!("{what} is longer than {max} bytes"));
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(format!("{what} must not contain control characters"));
    }
    Ok(text.to_string())
}

/// What a permitted `take` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TakeOutcome {
    Fresh,
    AlreadyHeld,
    Revoking { previous: String },
}

pub(crate) fn can_take(
    lease: Option<&HeldLease>,
    holder: &str,
    force: bool,
    why: Option<&str>,
) -> Result<TakeOutcome, String> {
    match lease {
        None => Ok(TakeOutcome::Fresh),
        Some(held) if held.holder == holder => Ok(TakeOutcome::AlreadyHeld),
        Some(held) => {
            if !force {
                return Err(format!(
                    "pane keyboard is held by {}; use --force --why REASON to revoke it",
                    held.holder
                ));
            }
            if why.map(str::trim).unwrap_or("").is_empty() {
                return Err("--force requires --why REASON".to_string());
            }
            Ok(TakeOutcome::Revoking {
                previous: held.holder.clone(),
            })
        }
    }
}

pub(crate) fn can_release(lease: Option<&HeldLease>, holder: &str) -> Result<(), String> {
    match lease {
        None => Err("pane keyboard is not held".to_string()),
        Some(held) if held.holder == holder => Ok(()),
        Some(held) => Err(format!(
            "pane keyboard is held by {}, not {holder}",
            held.holder
        )),
    }
}

pub(crate) fn can_write(
    policy: LeasePolicy,
    lease: Option<&HeldLease>,
    holder: Option<&str>,
) -> Result<(), String> {
    match (policy, lease) {
        (_, Some(held)) => {
            if holder == Some(held.holder.as_str()) {
                Ok(())
            } else {
                Err(format!("pane keyboard is held by {}", held.holder))
            }
        }
        (LeasePolicy::Open, None) => Ok(()),
        (LeasePolicy::Required, None) => {
            Err("pane keyboard is unheld and lease_policy is required; take it first".to_string())
        }
    }
}

/// One ledger line. `h` chains over everything else in the record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct LedgerRecord {
    pub(crate) seq: u64,
    pub(crate) ts_ms: u64,
    pub(crate) pane_id: String,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) payload: Value,
    pub(crate) prev: String,
    pub(crate) h: String,
}

pub(crate) fn ledger_path(dir: &Path, pane_id: &str) -> PathBuf {
    dir.join(format!("{pane_id}.jsonl"))
}

/// Sorted-key, whitespace-free JSON: the same bytes regardless of the
/// serializer's map ordering feature or the caller's field order.
pub(crate) fn canonical_json(value: &Value) -> String {
    let mut sorted = value.clone();
    sorted.sort_all_objects();
    sorted.to_string()
}

pub(crate) fn ledger_hash(prev: &str, body: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(LEDGER_HASH_PREFIX.as_bytes());
    hasher.update(prev.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    hex_encode(&hasher.finalize())
}

/// The hashed body: the record without `h`, canonicalized.
pub(crate) fn ledger_body(record: &LedgerRecord) -> String {
    let mut value = serde_json::to_value(record).unwrap_or(Value::Null);
    if let Value::Object(ref mut map) = value {
        map.remove("h");
    }
    canonical_json(&value)
}

/// The chain head `(seq, h)` from a ledger's last non-blank line;
/// `(0, "")` for a missing or empty ledger.
pub(crate) fn ledger_head(path: &Path) -> Result<(u64, String), String> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((0, String::new()))
        }
        Err(error) => return Err(format!("failed to read ledger {}: {error}", path.display())),
    };
    match data.lines().rev().find(|line| !line.trim().is_empty()) {
        None => Ok((0, String::new())),
        Some(line) => {
            let record: LedgerRecord = serde_json::from_str(line).map_err(|error| {
                format!("ledger {} tail is unreadable: {error}", path.display())
            })?;
            Ok((record.seq, record.h))
        }
    }
}

/// One workspace's ledger writer: the directory plus cached chain heads,
/// shared by the daemon handlers (lease events, durable) and the output
/// router (attention transitions and pane ends, best-effort). A LEAF lock:
/// `record` does file I/O under it and no caller holds another lock then.
pub(crate) struct LedgerSink {
    pub(crate) dir: PathBuf,
    pub(crate) heads: HashMap<String, (u64, String)>,
}

impl LedgerSink {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            heads: HashMap::new(),
        }
    }

    pub(crate) fn record(
        &mut self,
        pane_id: &str,
        kind: &str,
        payload: Value,
        durable: bool,
    ) -> Result<LedgerRecord, String> {
        ledger_append(&self.dir, &mut self.heads, pane_id, kind, payload, durable)
    }
}

/// Append one record, chaining from the cached head (seeded from disk on
/// first use). One `write_all` of line+'\n'; `durable` adds an fsync (lease
/// events are rare and are the product; attention flaps are frequent and
/// are not).
pub(crate) fn ledger_append(
    dir: &Path,
    heads: &mut HashMap<String, (u64, String)>,
    pane_id: &str,
    kind: &str,
    payload: Value,
    durable: bool,
) -> Result<LedgerRecord, String> {
    let path = ledger_path(dir, pane_id);
    let (seq, prev) = match heads.get(pane_id) {
        Some(head) => head.clone(),
        None => ledger_head(&path)?,
    };
    let mut record = LedgerRecord {
        seq: seq.saturating_add(1),
        ts_ms: now_millis(),
        pane_id: pane_id.to_string(),
        kind: kind.to_string(),
        payload,
        prev,
        h: String::new(),
    };
    record.h = ledger_hash(&record.prev, &ledger_body(&record));
    let line = serde_json::to_string(&record)
        .map_err(|error| format!("failed to encode ledger record: {error}"))?;
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .private_mode()
        .open(&path)
        .map_err(|error| format!("failed to open ledger {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|_| if durable { file.sync_all() } else { Ok(()) })
        .map_err(|error| format!("failed to append ledger {}: {error}", path.display()))?;
    heads.insert(pane_id.to_string(), (record.seq, record.h.clone()));
    Ok(record)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LedgerSummary {
    pub(crate) records: u64,
    pub(crate) head: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct LedgerBreak {
    pub(crate) line: usize,
    pub(crate) seq: Option<u64>,
    pub(crate) reason: String,
}

/// Walk a ledger and report the first break: an unparseable line, a sequence
/// gap, a `prev` that does not match, or a record whose bytes no longer hash
/// to `h`. Truncation from the tail is NOT detectable here; pin `head` from a
/// prior run to catch it.
pub(crate) fn ledger_verify(path: &Path) -> Result<LedgerSummary, LedgerBreak> {
    let data = fs::read_to_string(path).map_err(|error| LedgerBreak {
        line: 0,
        seq: None,
        reason: format!("cannot read ledger: {error}"),
    })?;
    let mut prev = String::new();
    let mut expected_seq: u64 = 1;
    let mut records: u64 = 0;
    for (index, line) in data.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let record: LedgerRecord = serde_json::from_str(line).map_err(|error| LedgerBreak {
            line: line_no,
            seq: None,
            reason: format!("unparseable record: {error}"),
        })?;
        if record.seq != expected_seq {
            return Err(LedgerBreak {
                line: line_no,
                seq: Some(record.seq),
                reason: format!("sequence {} where {expected_seq} was expected", record.seq),
            });
        }
        if record.prev != prev {
            return Err(LedgerBreak {
                line: line_no,
                seq: Some(record.seq),
                reason: "prev hash does not match the previous record".to_string(),
            });
        }
        let expected_hash = ledger_hash(&record.prev, &ledger_body(&record));
        if !constant_time_eq(&expected_hash, &record.h) {
            return Err(LedgerBreak {
                line: line_no,
                seq: Some(record.seq),
                reason: "record hash mismatch (content altered)".to_string(),
            });
        }
        prev = record.h;
        expected_seq = expected_seq.saturating_add(1);
        records += 1;
    }
    Ok(LedgerSummary {
        records,
        head: prev,
    })
}

/// The last `limit` records (0 = all) as raw JSON values; unparseable lines
/// are skipped so a torn tail still lists what came before it.
pub(crate) fn read_ledger_tail(path: &Path, limit: usize) -> Vec<Value> {
    let data = fs::read_to_string(path).unwrap_or_default();
    let parsed: Vec<Value> = data
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    if limit == 0 || parsed.len() <= limit {
        parsed
    } else {
        parsed[parsed.len() - limit..].to_vec()
    }
}
