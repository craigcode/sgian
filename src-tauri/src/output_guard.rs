use super::*;

// ---------------------------------------------------------------------------
// Output guard (docs/design/keyboard-lease-and-ledger.md §7): count the
// terminal tricks an agent can use to hide output from the person watching.
// ---------------------------------------------------------------------------

/// One rate-limit window as Claude Code reports it on its status line:
/// percent consumed (rounded) and when it resets (unix seconds).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimitWindow {
    #[serde(default)]
    pub used_percentage: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

/// What a Claude Code session says about itself after every turn, read
/// from the status-line payload (`ctl statusline`): the model, how full the
/// context window is, and the account's rate-limit windows (Pro/Max only).
/// Push, not poll: no credentials, no scraping. `updated_at_ms` says how
/// fresh it is; a client should fade a reading older than a few minutes.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Percent of the context window in use (input-only, as Claude Code
    /// computes it), rounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_used_percentage: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<RateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<RateLimitWindow>,
    /// Session cost in US cents (Claude Code reports dollars as a float).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_cents: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub updated_at_ms: u64,
}

impl AgentUsage {
    /// Read the fields we keep out of a status-line payload. Everything is
    /// optional so a payload from a newer CLI still parses; a payload with
    /// nothing we recognise yields `None`.
    pub(crate) fn from_status_payload(payload: &Value) -> Option<AgentUsage> {
        fn percent(value: &Value) -> Option<u8> {
            value
                .as_f64()
                .map(|pct| pct.clamp(0.0, 100.0).round() as u8)
        }
        let window = |value: &Value| -> Option<RateLimitWindow> {
            Some(RateLimitWindow {
                used_percentage: percent(value.get("used_percentage")?)?,
                resets_at: value.get("resets_at").and_then(Value::as_u64),
            })
        };
        let usage = AgentUsage {
            model: payload["model"]["display_name"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(64).collect()),
            model_id: payload["model"]["id"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(128).collect()),
            context_used_percentage: percent(&payload["context_window"]["used_percentage"]),
            context_window_size: payload["context_window"]["context_window_size"].as_u64(),
            five_hour: window(&payload["rate_limits"]["five_hour"]),
            seven_day: window(&payload["rate_limits"]["seven_day"]),
            total_cost_cents: payload["cost"]["total_cost_usd"]
                .as_f64()
                .filter(|usd| usd.is_finite() && *usd >= 0.0)
                .map(|usd| (usd * 100.0).round() as u64),
            session_id: payload["session_id"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(128).collect()),
            updated_at_ms: 0,
        };
        let empty = usage.model.is_none()
            && usage.model_id.is_none()
            && usage.context_used_percentage.is_none()
            && usage.five_hour.is_none()
            && usage.seven_day.is_none()
            && usage.total_cost_cents.is_none();
        (!empty).then_some(usage)
    }

    /// "Opus · 40% context · 5h 23% ↻ 15:00 · 7d 41%": the one-line form the
    /// default status line and `ctl agent` print. `now` is unix seconds.
    pub(crate) fn summary(&self, now: u64) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(model) = &self.model {
            parts.push(model.clone());
        }
        if let Some(pct) = self.context_used_percentage {
            parts.push(format!("{pct}% context"));
        }
        if let Some(window) = &self.five_hour {
            parts.push(format!(
                "5h {}%{}",
                window.used_percentage,
                format_reset(window.resets_at, now)
            ));
        }
        if let Some(window) = &self.seven_day {
            parts.push(format!(
                "7d {}%{}",
                window.used_percentage,
                format_reset(window.resets_at, now)
            ));
        }
        parts.join(" · ")
    }
}

/// " ↻ 2h10m" (time until a window resets) or "" when unknown or past.
pub(crate) fn format_reset(resets_at: Option<u64>, now: u64) -> String {
    let Some(at) = resets_at else {
        return String::new();
    };
    if at <= now {
        return String::new();
    }
    let secs = at - now;
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h >= 48 {
        format!(" ↻ {}d", h / 24)
    } else if h > 0 {
        format!(" ↻ {h}h{m:02}m")
    } else {
        format!(" ↻ {m}m")
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputTricks {
    /// SGR 8 (conceal): text present but invisible.
    #[serde(default)]
    pub conceal: u32,
    /// OSC 52: writing the clipboard from output (exfiltration vector).
    #[serde(default)]
    pub clipboard: u32,
    /// OSC 8 hyperlink whose visible text is a URL on a different host.
    #[serde(default)]
    pub hyperlink_mismatch: u32,
    /// DCS / APC / PM / SOS strings: opaque payloads the emulator swallows.
    #[serde(default)]
    pub string_controls: u32,
    /// Raw C1 control characters (U+0080..U+009F) in the text stream.
    #[serde(default)]
    pub c1_controls: u32,
    /// Characters that hide or reorder text without being seen: bidi
    /// overrides and isolates (the "Trojan Source" set), zero-width spaces
    /// and joiners that carry no script, and a byte-order mark inside text.
    /// Counted in agent-pane text, where no emulator stands between the
    /// agent and the person.
    #[serde(default)]
    pub invisible: u32,
}

impl OutputTricks {
    pub(crate) fn total(&self) -> u32 {
        self.conceal
            + self.clipboard
            + self.hyperlink_mismatch
            + self.string_controls
            + self.c1_controls
            + self.invisible
    }

    pub(crate) fn add(&mut self, other: &OutputTricks) {
        self.conceal = self.conceal.saturating_add(other.conceal);
        self.clipboard = self.clipboard.saturating_add(other.clipboard);
        self.hyperlink_mismatch = self
            .hyperlink_mismatch
            .saturating_add(other.hyperlink_mismatch);
        self.string_controls = self.string_controls.saturating_add(other.string_controls);
        self.c1_controls = self.c1_controls.saturating_add(other.c1_controls);
        self.invisible = self.invisible.saturating_add(other.invisible);
    }

    pub(crate) fn minus(&self, other: &OutputTricks) -> OutputTricks {
        OutputTricks {
            conceal: self.conceal.saturating_sub(other.conceal),
            clipboard: self.clipboard.saturating_sub(other.clipboard),
            hyperlink_mismatch: self
                .hyperlink_mismatch
                .saturating_sub(other.hyperlink_mismatch),
            string_controls: self.string_controls.saturating_sub(other.string_controls),
            c1_controls: self.c1_controls.saturating_sub(other.c1_controls),
            invisible: self.invisible.saturating_sub(other.invisible),
        }
    }
}

/// True for a character that can hide or reorder text in a chat view: the
/// bidi overrides and isolates (U+202A..U+202E, U+2066..U+2069), the
/// zero-width space, word joiner and invisible operators (U+200B, U+2060..
/// U+2064) and a byte-order mark (U+FEFF). The zero-width joiner and
/// non-joiner are not included: emoji sequences and several scripts need
/// them, and they cannot reorder text.
pub(crate) fn is_invisible_trick(c: char) -> bool {
    matches!(
        c,
        '\u{200b}' | '\u{2060}'..='\u{2064}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}'
    )
}

/// Scrub text that an agent pane will show as chat rather than through a
/// terminal: drop every escape sequence and control character (keeping line
/// feeds, carriage returns and tabs) and every invisible trick, and count
/// what mattered. Colour codes in a tool's output are dropped without being
/// counted; SGR 8, OSC 52, a mismatched OSC 8 link and opaque string
/// controls count exactly as they do for a shell pane, and each invisible
/// trick counts once. The sample names the first opaque string control.
pub(crate) fn scrub_agent_text(text: &str) -> (String, OutputTricks, Option<String>) {
    let (mut tricks, sample) = scan_output_tricks_detailed(text);
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('_') | Some('^') | Some('X') => {
                    let mut previous_esc = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_esc && next == '\\') {
                            break;
                        }
                        previous_esc = next == '\u{1b}';
                    }
                }
                _ => {}
            },
            '\n' | '\r' | '\t' => clean.push(c),
            '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}' => {}
            c if is_invisible_trick(c) => tricks.invisible += 1,
            c => clean.push(c),
        }
    }
    (clean, tricks, sample)
}

/// Scrub every string in a normalized agent event in place (text deltas,
/// tool results, tool inputs, permission requests: anything a person reads
/// or approves) and return what was counted.
pub(crate) fn scrub_agent_event(event: &mut serde_json::Value) -> (OutputTricks, Option<String>) {
    let mut total = OutputTricks::default();
    let mut first_sample = None;
    fn walk(value: &mut serde_json::Value, total: &mut OutputTricks, sample: &mut Option<String>) {
        match value {
            serde_json::Value::String(text) => {
                if text.bytes().any(|byte| !(0x20..0x80).contains(&byte)) {
                    let (clean, found, found_sample) = scrub_agent_text(text);
                    total.add(&found);
                    if sample.is_none() {
                        *sample = found_sample;
                    }
                    if clean != *text {
                        *text = clean;
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    walk(item, total, sample);
                }
            }
            serde_json::Value::Object(entries) => {
                for item in entries.values_mut() {
                    walk(item, total, sample);
                }
            }
            _ => {}
        }
    }
    walk(event, &mut total, &mut first_sample);
    (total, first_sample)
}

/// The host of a URL-ish string (`scheme://host[:port]/…` or `www.host…`),
/// lowercased; None when it does not look like a URL.
pub(crate) fn url_host(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let rest = if let Some((_, rest)) = trimmed.split_once("://") {
        rest
    } else if trimmed.starts_with("www.") {
        trimmed
    } else {
        return None;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Terminal capability traffic that legitimately travels as a DCS or APC
/// string and must not count as hidden output:
///
/// - replies a terminal sends to an application's query, which the tty echoes
///   into the output stream when they arrive before the application has
///   switched off echo (Claude Code asks `CSI > 0 q` at startup and SwiftTerm
///   answers `DCS > | … ST`, so every agent session used to trip the guard);
/// - the queries themselves, which an application prints to learn what the
///   terminal supports.
///
/// `kind` is the introducer (`P` for DCS, `_` for APC) and `body` the text
/// between it and the terminator.
pub(crate) fn is_terminal_capability_traffic(kind: char, body: &str) -> bool {
    match kind {
        'P' => {
            // XTVERSION reply; DECRQSS reply / query; XTGETTCAP reply / query.
            body.starts_with(">|")
                || body.starts_with("1$r")
                || body.starts_with("0$r")
                || body.starts_with("$q")
                || body.starts_with("1+r")
                || body.starts_with("0+r")
                || body.starts_with("+q")
        }
        // Kitty graphics protocol support query (`a=q`), not an image.
        '_' => body.starts_with('G') && body.split(';').next().is_some_and(|k| k.contains("a=q")),
        _ => false,
    }
}

/// Bound on the sample kept for an opaque string control, in characters.
const SAMPLE_CHARS: usize = 48;

/// Render an opaque string control as `DCS "…"` with the body escaped and
/// bounded, so a badge or ledger record can say what was seen without
/// carrying the payload.
fn describe_string_control(kind: char, body: &str) -> String {
    let name = match kind {
        'P' => "DCS",
        '_' => "APC",
        '^' => "PM",
        _ => "SOS",
    };
    let shown: String = body.chars().take(SAMPLE_CHARS).collect();
    let suffix = if body.chars().count() > SAMPLE_CHARS {
        "…"
    } else {
        ""
    };
    format!("{name} {shown:?}{suffix}")
}

/// [`scan_output_tricks_detailed`] without the sample.
#[cfg(test)]
pub(crate) fn scan_output_tricks(text: &str) -> OutputTricks {
    scan_output_tricks_detailed(text).0
}

/// Scan one output chunk. Sequences split across chunks are missed, which is
/// acceptable for a counter meant to raise a flag, not to censor. The second
/// value describes the first opaque string control counted, if any.
pub(crate) fn scan_output_tricks_detailed(text: &str) -> (OutputTricks, Option<String>) {
    let mut tricks = OutputTricks::default();
    let mut sample: Option<String> = None;
    let mut chars = text.chars().peekable();
    // The open OSC 8 target host while inside a hyperlink, and the visible
    // text collected under it.
    let mut link_host: Option<String> = None;
    let mut link_text = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    let mut final_byte = None;
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            final_byte = Some(next);
                            break;
                        }
                        params.push(next);
                    }
                    if final_byte == Some('m') {
                        // SGR: a standalone `8` conceals; `38;5;8` (a colour
                        // index) does not.
                        let mut parts = params.split(';');
                        while let Some(part) = parts.next() {
                            match part {
                                "8" => tricks.conceal += 1,
                                "38" | "48" | "58" => match parts.next() {
                                    Some("5") => {
                                        parts.next();
                                    }
                                    Some("2") => {
                                        for _ in 0..3 {
                                            parts.next();
                                        }
                                    }
                                    _ => {}
                                },
                                _ => {}
                            }
                        }
                    }
                }
                Some(']') => {
                    let mut body = String::new();
                    let mut previous_esc = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_esc && next == '\\') {
                            break;
                        }
                        previous_esc = next == '\u{1b}';
                        if !previous_esc {
                            body.push(next);
                        }
                    }
                    if body.starts_with("52;") {
                        tricks.clipboard += 1;
                    } else if let Some(rest) = body.strip_prefix("8;") {
                        let target = rest.split_once(';').map(|(_, t)| t).unwrap_or("");
                        if target.is_empty() {
                            // Closing the link: compare what was shown with where it went.
                            if let (Some(host), Some(shown)) =
                                (link_host.take(), url_host(&link_text))
                            {
                                if shown != host {
                                    tricks.hyperlink_mismatch += 1;
                                }
                            }
                            link_text.clear();
                        } else {
                            link_host = url_host(target);
                            link_text.clear();
                        }
                    }
                }
                Some(kind @ ('P' | '_' | '^' | 'X')) => {
                    let mut body = String::new();
                    let mut previous_esc = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (previous_esc && next == '\\') {
                            break;
                        }
                        previous_esc = next == '\u{1b}';
                        if !previous_esc {
                            body.push(next);
                        }
                    }
                    if is_terminal_capability_traffic(kind, &body) {
                        continue;
                    }
                    tricks.string_controls += 1;
                    if sample.is_none() {
                        sample = Some(describe_string_control(kind, &body));
                    }
                }
                _ => {}
            },
            '\u{80}'..='\u{9f}' => tricks.c1_controls += 1,
            c => {
                if link_host.is_some() {
                    link_text.push(c);
                }
            }
        }
    }
    (tricks, sample)
}

/// Announce at most this often per pane (ledger + event); counts always accumulate.
pub(crate) const OUTPUT_WARNING_ANNOUNCE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub(crate) struct OutputGuardState {
    pub(crate) total: OutputTricks,
    pub(crate) announced: OutputTricks,
    pub(crate) last_announced: Option<Instant>,
    /// The first opaque string control seen on this pane, described.
    pub(crate) sample: Option<String>,
}
