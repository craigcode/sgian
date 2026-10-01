use serde::de::DeserializeOwned;
use serde::Serialize;
use std::io::{ErrorKind, Read, Write};

use super::MAX_FRAME_BYTES;

/// 4-byte magic prefix that identifies a v2 framed envelope.
pub(crate) const MAGIC: [u8; 4] = *b"SGN2";
/// Wire version carried in the header; v1 is the legacy newline-JSON path.
pub(crate) const WIRE_VERSION: u16 = 2;
/// Fixed header size: 4 (magic) + 2 (u16-BE version) + 4 (u32-BE length).
pub(crate) const HEADER_LEN: usize = 10;

/// Encode `value` as a framed envelope wrapping its JSON payload.
///
/// Rejects (before building the frame) a payload larger than `MAX_FRAME_BYTES`
/// so the framed path cannot be driven to unbounded memory on write.
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| format!("failed to encode framed payload: {error}"))?;
    encode_payload(&payload)
}

/// Wrap already-serialized JSON `payload` bytes in a framed envelope.
///
/// Rejects (before building the frame) a payload larger than `MAX_FRAME_BYTES`,
/// the same write-side bound as [`encode`], so the framed path cannot be driven
/// to unbounded memory. Used to frame an event whose JSON was serialized once and
/// fanned out to multiple subscribers.
pub(crate) fn encode_payload(payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() as u64 > MAX_FRAME_BYTES {
        return Err(format!(
            "framed payload of {} bytes exceeds maximum frame size",
            payload.len()
        ));
    }
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&WIRE_VERSION.to_be_bytes());
    // payload.len() <= MAX_FRAME_BYTES (8 MiB) always fits a u32.
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Encode `value` as a framed envelope and write it to `stream`, then flush.
pub(crate) fn write<W: Write, T: Serialize>(stream: &mut W, value: &T) -> Result<(), String> {
    let frame = encode(value)?;
    stream
        .write_all(&frame)
        .map_err(|error| format!("failed to write framed ipc: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("failed to flush framed ipc: {error}"))
}

/// Read one framed envelope and return its raw JSON payload bytes.
///
/// Returns `Ok(None)` on a clean EOF at a frame boundary (the peer closed
/// between frames). Returns `Err` for any malformed frame: invalid magic, an
/// unsupported wire version, a `LENGTH` exceeding `MAX_FRAME_BYTES` (rejected
/// before any payload allocation), a truncated header, or a body shorter than
/// the declared `LENGTH` (peer closed mid-body).
pub(crate) fn read_bytes<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, String> {
    let mut header = [0u8; HEADER_LEN];

    // Read the first header byte on its own so a clean EOF at a frame boundary
    // (nothing left to read) is reported as Ok(None) rather than a spurious
    // truncated-header error.
    loop {
        match reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(ref error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("failed to read framed header: {error}")),
        }
    }
    reader
        .read_exact(&mut header[1..])
        .map_err(|error| format!("framed header truncated: {error}"))?;

    if header[..4] != MAGIC {
        return Err("framed frame has invalid magic".to_string());
    }
    let version = u16::from_be_bytes([header[4], header[5]]);
    if version != WIRE_VERSION {
        return Err(format!(
            "framed frame has unsupported wire version {version}"
        ));
    }
    let length = u32::from_be_bytes([header[6], header[7], header[8], header[9]]) as u64;
    if length > MAX_FRAME_BYTES {
        return Err(format!(
            "framed frame declares length {length} exceeding maximum frame size"
        ));
    }

    let mut payload = vec![0u8; length as usize];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("framed payload truncated: {error}"))?;
    Ok(Some(payload))
}

/// Read one framed envelope and deserialize its JSON payload into `T`.
///
/// `Ok(None)` is a clean EOF at a frame boundary; `Err` covers every codec
/// error from [`read_bytes`] plus a payload that is not valid JSON for `T`
/// (including a zero-length payload).
pub(crate) fn read<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<Option<T>, String> {
    let payload = match read_bytes(reader)? {
        Some(payload) => payload,
        None => return Ok(None),
    };
    let value = serde_json::from_slice(&payload)
        .map_err(|error| format!("failed to decode framed payload: {error}"))?;
    Ok(Some(value))
}
