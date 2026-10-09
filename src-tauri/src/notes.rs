//! Shared context notes for a project (docs/design/shared-context-notes.md,
//! step 1): short Markdown files under the project's repository that every
//! pane in the project can read and that people and agents add to with
//! attribution. The daemon writes them so the author is derived from the
//! connection, never declared; git keeps the history; the project ledger
//! records every write with a content hash.
//!
//! Layout: `<root>/.sgian/projects/<project>/notes/<YYYY-MM-DD>-<slug>.md`,
//! where `<root>` is the project's `repo` (the workspace otherwise). One
//! file per note so two writers never collide on a line. Each file opens
//! with a small front matter block the daemon writes and reads itself:
//!
//! ```text
//! ---
//! title: Flaky auth test
//! holder: craig@mac
//! pane: pane-3
//! written_at_ms: 1760000000000
//! ---
//!
//! body
//! ```
//!
//! A file without that block was written outside the daemon (an agent's own
//! file tools, an editor); it is listed with evidence `file` and no holder,
//! the same distinction the attention badge draws between `hook` and `screen`.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::daemon_client::hex_encode;
use crate::output_guard::{scrub_agent_text, OutputTricks};

/// One note's body, after trimming, at most this many bytes.
pub(crate) const NOTE_MAX_BYTES: usize = 16 * 1024;
/// The whole directory at most this many bytes: the agent-facing payload of
/// `project notes` stays bounded however many notes accumulate.
pub(crate) const NOTES_DIR_MAX_BYTES: usize = 1024 * 1024;
/// Notes per project.
pub(crate) const NOTES_MAX_COUNT: usize = 256;
pub(crate) const NOTE_TITLE_MAX_BYTES: usize = 120;
/// The slug part of a file name.
const NOTE_SLUG_MAX_BYTES: usize = 48;
/// `project notes` format tag; bump when a consumer could misread it.
pub(crate) const NOTES_FORMAT: &str = "sgian.notes.v1";

pub(crate) fn notes_dir(root: &Path, project: &str) -> PathBuf {
    root.join(".sgian")
        .join("projects")
        .join(project)
        .join("notes")
}

/// The front matter the daemon writes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteMeta {
    pub(crate) title: String,
    pub(crate) holder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pane: Option<String>,
    pub(crate) written_at_ms: u64,
}

/// One listed note: the file, who wrote it and how we know, and the body as
/// a person should see it (scrubbed; the tricks counted).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteEntry {
    pub(crate) file: String,
    pub(crate) title: String,
    /// `daemon` when the front matter names the writer, `file` otherwise.
    pub(crate) evidence: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) written_at_ms: Option<u64>,
    pub(crate) bytes: usize,
    /// SHA-256 of the file as stored, hex: what the ledger records.
    pub(crate) hash: String,
    pub(crate) body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tricks: Option<OutputTricks>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NotesListing {
    pub(crate) format: String,
    pub(crate) project: String,
    pub(crate) dir: String,
    pub(crate) total: usize,
    pub(crate) bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tricks: Option<OutputTricks>,
    pub(crate) notes: Vec<NoteEntry>,
}

pub(crate) fn note_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(bytes);
    hex_encode(&hasher.finalize())
}

/// `[A-Za-z0-9._-]`, ends in `.md`, no leading dot: a name that is safe in a
/// path and in shell output, and cannot escape the directory.
pub(crate) fn validate_note_file(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("note file must not be blank".to_string());
    }
    if name.len() > 128 {
        return Err("note file name is longer than 128 bytes".to_string());
    }
    if name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("note file may only contain letters, digits, '.', '_' and '-'".to_string());
    }
    if !name.ends_with(".md") || name.len() == 3 {
        return Err("note file must end in .md".to_string());
    }
    Ok(name.to_string())
}

/// A title becomes `[a-z0-9-]`, runs of anything else collapsed to one
/// hyphen, at most NOTE_SLUG_MAX_BYTES; `note` when nothing survives.
pub(crate) fn note_slug(title: &str) -> String {
    let mut slug = String::new();
    let mut pending_hyphen = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_hyphen && !slug.is_empty() {
                slug.push('-');
            }
            pending_hyphen = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_hyphen = true;
        }
        if slug.len() >= NOTE_SLUG_MAX_BYTES {
            break;
        }
    }
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "note".to_string()
    } else {
        slug
    }
}

/// Civil date (UTC) for a Unix millisecond timestamp, `YYYY-MM-DD`. The
/// days-to-civil algorithm from Howard Hinnant's date library; no calendar
/// dependency for one field.
pub(crate) fn civil_date(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `<date>-<slug>.md`, with `-2`, `-3`… when the name is taken.
pub(crate) fn note_file_name(ms: u64, title: &str, taken: &HashSet<String>) -> String {
    let base = format!("{}-{}", civil_date(ms), note_slug(title));
    let first = format!("{base}.md");
    if !taken.contains(&first) {
        return first;
    }
    (2..)
        .map(|n| format!("{base}-{n}.md"))
        .find(|name| !taken.contains(name))
        .unwrap_or(first)
}

pub(crate) fn render_note(meta: &NoteMeta, body: &str) -> String {
    let mut text = String::with_capacity(body.len() + 128);
    text.push_str("---\n");
    text.push_str(&format!("title: {}\n", meta.title));
    text.push_str(&format!("holder: {}\n", meta.holder));
    if let Some(pane) = &meta.pane {
        text.push_str(&format!("pane: {pane}\n"));
    }
    text.push_str(&format!("written_at_ms: {}\n", meta.written_at_ms));
    text.push_str("---\n\n");
    text.push_str(body);
    if !body.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// The front matter the daemon wrote, if the file has one, and the body
/// after it (the whole text otherwise).
pub(crate) fn parse_note(text: &str) -> (Option<NoteMeta>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (None, text);
    };
    let Some(end) = rest.find("\n---\n") else {
        return (None, text);
    };
    let (block, after) = rest.split_at(end);
    let body = after[5..].strip_prefix('\n').unwrap_or(&after[5..]);
    let mut title = None;
    let mut holder = None;
    let mut pane = None;
    let mut written_at_ms = None;
    for line in block.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "title" => title = Some(value.to_string()),
            "holder" => holder = Some(value.to_string()),
            "pane" => pane = Some(value.to_string()),
            "written_at_ms" => written_at_ms = value.parse().ok(),
            _ => {}
        }
    }
    match (title, holder, written_at_ms) {
        (Some(title), Some(holder), Some(written_at_ms)) if !holder.is_empty() => (
            Some(NoteMeta {
                title,
                holder,
                pane,
                written_at_ms,
            }),
            body,
        ),
        _ => (None, text),
    }
}

fn note_files(dir: &Path) -> Result<Vec<(String, u64)>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("failed to read {}: {error}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| format!("failed to read {}: {error}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if validate_note_file(&name).is_err() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("failed to stat {name}: {error}"))?;
        if !metadata.is_file() {
            continue;
        }
        files.push((name, metadata.len()));
    }
    Ok(files)
}

/// Every note in the directory, newest first (by the daemon's timestamp
/// where there is one, then by name descending so the date prefix orders
/// files written outside the daemon). Bodies are scrubbed as agent output
/// is: escapes and controls removed, the hidden-text tricks counted.
pub(crate) fn list_notes(dir: &Path, project: &str) -> Result<NotesListing, String> {
    let mut notes = Vec::new();
    let mut bytes = 0usize;
    let mut tricks = OutputTricks::default();
    for (file, _) in note_files(dir)? {
        let path = dir.join(&file);
        let raw = fs::read(&path).map_err(|error| format!("failed to read {file}: {error}"))?;
        bytes += raw.len();
        let hash = note_hash(&raw);
        let text = String::from_utf8_lossy(&raw);
        let (meta, body) = parse_note(&text);
        let (clean_title, title_tricks, _) =
            scrub_agent_text(meta.as_ref().map_or("", |m| m.title.as_str()));
        let (clean_body, body_tricks, _) = scrub_agent_text(body);
        let mut note_tricks = title_tricks;
        note_tricks.add(&body_tricks);
        tricks.add(&note_tricks);
        let title = if meta.is_some() {
            clean_title
        } else {
            file.trim_end_matches(".md").to_string()
        };
        notes.push(NoteEntry {
            file,
            title,
            evidence: if meta.is_some() { "daemon" } else { "file" }.to_string(),
            holder: meta.as_ref().map(|m| m.holder.clone()),
            pane: meta.as_ref().and_then(|m| m.pane.clone()),
            written_at_ms: meta.as_ref().map(|m| m.written_at_ms),
            bytes: raw.len(),
            hash,
            body: clean_body.trim_end().to_string(),
            tricks: (note_tricks.total() > 0).then_some(note_tricks),
        });
    }
    notes.sort_by(|a, b| {
        b.written_at_ms
            .unwrap_or(0)
            .cmp(&a.written_at_ms.unwrap_or(0))
            .then_with(|| b.file.cmp(&a.file))
    });
    Ok(NotesListing {
        format: NOTES_FORMAT.to_string(),
        project: project.to_string(),
        dir: dir.display().to_string(),
        total: notes.len(),
        bytes,
        tricks: (tricks.total() > 0).then_some(tricks),
        notes,
    })
}

/// What `add_note` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrittenNote {
    pub(crate) file: String,
    pub(crate) path: PathBuf,
    pub(crate) hash: String,
    pub(crate) bytes: usize,
}

/// Write one note. The caller has validated the title and body; this
/// enforces the count and directory caps and never overwrites.
pub(crate) fn add_note(dir: &Path, meta: &NoteMeta, body: &str) -> Result<WrittenNote, String> {
    let existing = note_files(dir)?;
    if existing.len() >= NOTES_MAX_COUNT {
        return Err(format!("project has {NOTES_MAX_COUNT} notes already"));
    }
    let text = render_note(meta, body);
    let used: u64 = existing.iter().map(|(_, len)| len).sum();
    if used as usize + text.len() > NOTES_DIR_MAX_BYTES {
        return Err(format!(
            "project notes would exceed {NOTES_DIR_MAX_BYTES} bytes; remove some first"
        ));
    }
    let taken: HashSet<String> = existing.into_iter().map(|(name, _)| name).collect();
    let file = note_file_name(meta.written_at_ms, &meta.title, &taken);
    fs::create_dir_all(dir)
        .map_err(|error| format!("failed to create {}: {error}", dir.display()))?;
    let path = dir.join(&file);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut handle = options
        .open(&path)
        .map_err(|error| format!("failed to create {}: {error}", path.display()))?;
    std::io::Write::write_all(&mut handle, text.as_bytes())
        .map_err(|error| format!("failed to write {}: {error}", path.display()))?;
    Ok(WrittenNote {
        file,
        path,
        hash: note_hash(text.as_bytes()),
        bytes: text.len(),
    })
}

/// Remove one note by file name; returns the hash of what was removed so
/// the ledger can say exactly which content went.
pub(crate) fn remove_note(dir: &Path, file: &str) -> Result<(String, usize), String> {
    let file = validate_note_file(file)?;
    let path = dir.join(&file);
    let raw = fs::read(&path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("no note '{file}'"),
        _ => format!("failed to read {file}: {error}"),
    })?;
    fs::remove_file(&path).map_err(|error| format!("failed to remove {file}: {error}"))?;
    Ok((note_hash(&raw), raw.len()))
}
