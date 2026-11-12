use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// AWS Systems Manager (SSM) Session Manager M5 Transport (#115)
//
// Implements:
// 1. AWS SSM `AgentMessage` binary framing (116..=4096-byte big-endian header,
//    half-swapped [LSB_8 | MSB_8] UUID wire layout, SHA-256 `PayloadDigest`,
//    10 MiB pre-allocation payload bound).
// 2. Reliable sequence / ACK / EWMA-retransmit stream engine (`SsmStreamState`)
//    with deferred-ACK backpressure and `ResumeSession` replay preparation.
// 3. AWS KMS session encryption (`KMSEncryption` / `EncChallengeRequest` /
//    `EncChallengeResponse`) with 64-byte `GenerateDataKey` envelope key split
//    and AES-256-GCM stream payload encryption via `ring::aead`.
// 4. `xtaci/smux` v1 stream multiplexer for `AWS-StartPortForwardingSession`.
// 5. Synchronous RFC 6455 WebSocket client (`ws://` + `wss://` via `rustls`).
// 6. `ClientLeaseTracker` (`Requested` / `Releasing`) for round-trip transports.
// 7. `aws` CLI `session-manager-plugin` capture shim and `--ssm-tunnel` bridge.
// ---------------------------------------------------------------------------

pub(crate) const SSM_PLUGIN_CAPTURE_ARG: &str = "--ssm-plugin-capture";
pub(crate) const SSM_TUNNEL_ARG: &str = "--ssm-tunnel";
pub(crate) const SSM_CAPTURE_FILE_ENV: &str = "SGIAN_SSM_CAPTURE_FILE";
pub(crate) const SSM_KMS_CHALLENGE_CONTEXT_ENV: &str = "SGIAN_SSM_KMS_CHALLENGE_CONTEXT";
pub(crate) const SSM_CLIENT_VERSION: &str = "1.2.463.0";

pub(crate) const SSM_KMS_DATA_KEY_LEN: usize = 64;
pub(crate) const SSM_KMS_HALF_KEY_LEN: usize = 32;
pub(crate) const SSM_AES_GCM_NONCE_LEN: usize = 12;
pub(crate) const SSM_AES_GCM_TAG_LEN: usize = 16;

pub(crate) const SSM_HEADER_LEN: u32 = 116;
pub(crate) const SSM_MAX_HEADER_LEN: u32 = 4096;
pub(crate) const SSM_MAX_PAYLOAD_LEN: u32 = 10 * 1024 * 1024;
pub(crate) const SSM_SCHEMA_VERSION: u32 = 1;

pub(crate) const MSG_INPUT_STREAM_DATA: &str = "input_stream_data";
pub(crate) const MSG_OUTPUT_STREAM_DATA: &str = "output_stream_data";
pub(crate) const MSG_ACKNOWLEDGE: &str = "acknowledge";
pub(crate) const MSG_CHANNEL_CLOSED: &str = "channel_closed";
pub(crate) const MSG_START_PUBLICATION: &str = "start_publication";
pub(crate) const MSG_PAUSE_PUBLICATION: &str = "pause_publication";

pub(crate) const SSM_FLAG_DATA: u64 = 0;
#[cfg(test)]
pub(crate) const SSM_FLAG_SYN: u64 = 1;
#[cfg(test)]
#[allow(dead_code)]
pub(crate) const SSM_FLAG_FIN: u64 = 2;

pub(crate) mod payload_type {
    pub(crate) const OUTPUT: u32 = 1;
    pub(crate) const HANDSHAKE_REQUEST: u32 = 5;
    pub(crate) const HANDSHAKE_RESPONSE: u32 = 6;
    pub(crate) const HANDSHAKE_COMPLETE: u32 = 7;
    pub(crate) const ENC_CHALLENGE_REQUEST: u32 = 8;
    pub(crate) const ENC_CHALLENGE_RESPONSE: u32 = 9;
    pub(crate) const FLAG: u32 = 10;
    pub(crate) const STD_ERR: u32 = 11;
    pub(crate) const EXIT_CODE: u32 = 12;
}

const MAX_INBOUND_OOO_FRAMES: usize = 64;
const MAX_INBOUND_OOO_BYTES: usize = 4 * 1024 * 1024;
const MAX_OUTBOUND_UNACKED_FRAMES: usize = 512;
const MAX_OUTBOUND_UNACKED_BYTES: usize = 8 * 1024 * 1024;
const INITIAL_RTO_MS: u64 = 1_000;
const MIN_RTO_MS: u64 = 200;
const MAX_RTO_MS: u64 = 30_000;

fn constant_time_bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// 1. Half-Swapped SSM UUID (`agentmessage.go` `putUuid` / `getUuid`)
// ---------------------------------------------------------------------------

/// Canonical RFC 4122 16-byte UUID with AWS SSM's half-swapped wire layout:
/// wire bytes `64..72` store canonical `bytes[8..16]` (`leastSignificantBits`)
/// and wire bytes `72..80` store canonical `bytes[0..8]` (`mostSignificantBits`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SsmUuid([u8; 16]);

impl SsmUuid {
    pub(crate) fn new_v4() -> Result<Self, String> {
        let mut b = [0_u8; 16];
        fill_secure_random(&mut b)?;
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        Ok(Self(b))
    }

    #[cfg(test)]
    pub(crate) const fn from_canonical_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[cfg(test)]
    pub(crate) const fn as_canonical_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub(crate) fn to_wire_bytes(self) -> [u8; 16] {
        let mut wire = [0_u8; 16];
        wire[0..8].copy_from_slice(&self.0[8..16]);
        wire[8..16].copy_from_slice(&self.0[0..8]);
        wire
    }

    pub(crate) fn from_wire_bytes(wire: [u8; 16]) -> Self {
        let mut canonical = [0_u8; 16];
        canonical[0..8].copy_from_slice(&wire[8..16]);
        canonical[8..16].copy_from_slice(&wire[0..8]);
        Self(canonical)
    }

    #[cfg(test)]
    pub(crate) fn parse_hyphenated(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let hex_only: String = trimmed.chars().filter(|c| *c != '-').collect();
        if trimmed.len() != 36
            || hex_only.len() != 32
            || trimmed.as_bytes()[8] != b'-'
            || trimmed.as_bytes()[13] != b'-'
            || trimmed.as_bytes()[18] != b'-'
            || trimmed.as_bytes()[23] != b'-'
        {
            return Err(format!("invalid UUID format: '{trimmed}'"));
        }
        let mut out = [0_u8; 16];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex_only[i * 2..i * 2 + 2], 16)
                .map_err(|_| format!("invalid hex in UUID: '{trimmed}'"))?;
        }
        Ok(Self(out))
    }
}

impl fmt::Display for SsmUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3],
            b[4], b[5],
            b[6], b[7],
            b[8], b[9],
            b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

// ---------------------------------------------------------------------------
// 2. AWS SSM `AgentMessage` Binary Codec
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentMessage {
    pub(crate) message_type: String,
    pub(crate) schema_version: u32,
    pub(crate) created_date: u64,
    pub(crate) sequence_number: i64,
    pub(crate) flags: u64,
    pub(crate) message_id: SsmUuid,
    pub(crate) payload_type: u32,
    pub(crate) payload: Vec<u8>,
}

impl AgentMessage {
    pub(crate) fn new(
        message_type: &str,
        sequence_number: i64,
        flags: u64,
        message_id: SsmUuid,
        created_date: u64,
        payload_type: u32,
        payload: Vec<u8>,
    ) -> Result<Self, String> {
        validate_ssm_message_type(message_type)?;
        if payload.len() > SSM_MAX_PAYLOAD_LEN as usize {
            return Err(format!(
                "SSM payload length {} exceeds maximum {}",
                payload.len(),
                SSM_MAX_PAYLOAD_LEN
            ));
        }
        Ok(Self {
            message_type: message_type.to_string(),
            schema_version: SSM_SCHEMA_VERSION,
            created_date,
            sequence_number,
            flags,
            message_id,
            payload_type,
            payload,
        })
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        use sha2::Digest;
        validate_ssm_message_type(&self.message_type)?;
        if self.schema_version != SSM_SCHEMA_VERSION {
            return Err(format!(
                "unsupported SSM schema_version {}",
                self.schema_version
            ));
        }
        if self.payload.len() > SSM_MAX_PAYLOAD_LEN as usize {
            return Err(format!(
                "SSM payload length {} exceeds maximum {}",
                self.payload.len(),
                SSM_MAX_PAYLOAD_LEN
            ));
        }

        let total = (SSM_HEADER_LEN as usize) + 4 + self.payload.len();
        let mut out = Vec::with_capacity(total);

        // 0..4: HeaderLength (116)
        out.extend_from_slice(&SSM_HEADER_LEN.to_be_bytes());

        // 4..36: MessageType (32-byte space-padded ASCII)
        let mut msg_type_buf = [b' '; 32];
        let mt_bytes = self.message_type.as_bytes();
        msg_type_buf[..mt_bytes.len()].copy_from_slice(mt_bytes);
        out.extend_from_slice(&msg_type_buf);

        // 36..40: SchemaVersion
        out.extend_from_slice(&self.schema_version.to_be_bytes());

        // 40..48: CreatedDate (u64 BE epoch ms)
        out.extend_from_slice(&self.created_date.to_be_bytes());

        // 48..56: SequenceNumber (i64 BE)
        out.extend_from_slice(&self.sequence_number.to_be_bytes());

        // 56..64: Flags (u64 BE)
        out.extend_from_slice(&self.flags.to_be_bytes());

        // 64..80: MessageId (16 bytes half-swapped [LSB_8 | MSB_8])
        out.extend_from_slice(&self.message_id.to_wire_bytes());

        // 80..112: PayloadDigest (SHA-256 of payload)
        let digest = sha2::Sha256::digest(&self.payload);
        out.extend_from_slice(&digest);

        // 112..116: PayloadType (u32 BE)
        out.extend_from_slice(&self.payload_type.to_be_bytes());

        // 116..120: PayloadLength (u32 BE)
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());

        // 120..: Payload
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, String> {
        use sha2::Digest;
        if bytes.len() < (SSM_HEADER_LEN as usize) + 4 {
            return Err(format!(
                "SSM frame truncated: {} bytes (minimum 120)",
                bytes.len()
            ));
        }

        let header_len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if !(SSM_HEADER_LEN..=SSM_MAX_HEADER_LEN).contains(&header_len) {
            return Err(format!(
                "invalid SSM HeaderLength {header_len} (expected {SSM_HEADER_LEN}..={SSM_MAX_HEADER_LEN})"
            ));
        }
        let hl = header_len as usize;
        if bytes.len() < hl + 4 {
            return Err(format!(
                "SSM frame shorter ({}) than HeaderLength + 4 ({})",
                bytes.len(),
                hl + 4
            ));
        }

        let raw_type = &bytes[4..36];
        let trimmed_type = raw_type
            .iter()
            .rposition(|&b| b != b' ' && b != 0)
            .map(|end| &raw_type[..=end])
            .unwrap_or(&[]);
        let message_type_str = std::str::from_utf8(trimmed_type)
            .map_err(|_| "SSM MessageType is not valid UTF-8".to_string())?;
        validate_ssm_message_type(message_type_str)?;

        let schema_version = u32::from_be_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        if schema_version != SSM_SCHEMA_VERSION {
            return Err(format!(
                "unsupported SSM SchemaVersion {schema_version} (expected {SSM_SCHEMA_VERSION})"
            ));
        }

        let created_date = u64::from_be_bytes(bytes[40..48].try_into().unwrap());
        let sequence_number = i64::from_be_bytes(bytes[48..56].try_into().unwrap());
        let flags = u64::from_be_bytes(bytes[56..64].try_into().unwrap());

        let mut wire_uuid = [0_u8; 16];
        wire_uuid.copy_from_slice(&bytes[64..80]);
        let message_id = SsmUuid::from_wire_bytes(wire_uuid);

        let expected_digest = &bytes[80..112];
        let payload_type = u32::from_be_bytes(bytes[112..116].try_into().unwrap());

        let payload_len = u32::from_be_bytes(bytes[hl..hl + 4].try_into().unwrap());
        if payload_len > SSM_MAX_PAYLOAD_LEN {
            return Err(format!(
                "SSM PayloadLength {payload_len} exceeds 10 MiB cap ({SSM_MAX_PAYLOAD_LEN})"
            ));
        }
        let pl = payload_len as usize;
        if bytes.len() != hl + 4 + pl {
            return Err(format!(
                "SSM frame length mismatch: frame has {} bytes, header+payload declares {}",
                bytes.len(),
                hl + 4 + pl
            ));
        }

        let payload_slice = &bytes[hl + 4..hl + 4 + pl];
        let actual_digest = sha2::Sha256::digest(payload_slice);
        if !constant_time_bytes_eq(expected_digest, &actual_digest) {
            return Err("SSM PayloadDigest SHA-256 mismatch".to_string());
        }

        Ok(Self {
            message_type: message_type_str.to_string(),
            schema_version,
            created_date,
            sequence_number,
            flags,
            message_id,
            payload_type,
            payload: payload_slice.to_vec(),
        })
    }
}

fn validate_ssm_message_type(message_type: &str) -> Result<(), String> {
    if message_type.is_empty() || message_type.len() > 32 {
        return Err(format!(
            "SSM MessageType must be 1..=32 bytes, got len {}",
            message_type.len()
        ));
    }
    if !message_type
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "SSM MessageType contains invalid characters: '{message_type}'"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. Control JSON Payloads & Handshake Handling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OpenDataChannelInput {
    #[serde(rename = "MessageSchemaVersion")]
    pub(crate) message_schema_version: String,
    #[serde(rename = "RequestId")]
    pub(crate) request_id: String,
    #[serde(rename = "TokenValue")]
    pub(crate) token_value: String,
    #[serde(rename = "ClientId")]
    pub(crate) client_id: String,
    #[serde(rename = "ClientVersion")]
    pub(crate) client_version: String,
}

impl OpenDataChannelInput {
    pub(crate) fn new(token_value: &str, client_id: &str) -> Result<Self, String> {
        Ok(Self {
            message_schema_version: "1.0".to_string(),
            request_id: SsmUuid::new_v4()?.to_string(),
            token_value: token_value.to_string(),
            client_id: client_id.to_string(),
            client_version: SSM_CLIENT_VERSION.to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct AcknowledgeContent {
    #[serde(rename = "AcknowledgedMessageType")]
    pub(crate) acknowledged_message_type: String,
    #[serde(rename = "AcknowledgedMessageId")]
    pub(crate) acknowledged_message_id: String,
    #[serde(rename = "AcknowledgedMessageSequenceNumber")]
    pub(crate) acknowledged_message_sequence_number: i64,
    #[serde(rename = "IsSequentialMessage")]
    pub(crate) is_sequential_message: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandshakeRequestPayload {
    #[serde(rename = "AgentVersion", default)]
    pub(crate) agent_version: String,
    #[serde(rename = "RequestedClientActions", default)]
    pub(crate) requested_client_actions: Vec<RequestedClientAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RequestedClientAction {
    #[serde(rename = "ActionType")]
    pub(crate) action_type: String,
    #[serde(rename = "ActionParameters", default)]
    pub(crate) action_parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HandshakeResponsePayload {
    #[serde(rename = "ClientVersion")]
    pub(crate) client_version: String,
    #[serde(rename = "ProcessedClientActions")]
    pub(crate) processed_client_actions: Vec<ProcessedClientAction>,
    #[serde(rename = "Errors", default)]
    pub(crate) errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProcessedClientAction {
    #[serde(rename = "ActionType")]
    pub(crate) action_type: String,
    #[serde(rename = "ActionStatus")]
    pub(crate) action_status: u32,
    #[serde(rename = "ActionResult", skip_serializing_if = "Option::is_none")]
    pub(crate) action_result: Option<Value>,
    #[serde(rename = "Error", skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ChannelClosedPayload {
    #[serde(rename = "MessageId", default)]
    pub(crate) message_id: String,
    #[serde(rename = "CreatedDate", default)]
    pub(crate) created_date: String,
    #[serde(rename = "DestinationId", default)]
    pub(crate) destination_id: String,
    #[serde(rename = "SessionId", default)]
    pub(crate) session_id: String,
    #[serde(rename = "MessageType", default)]
    pub(crate) message_type: String,
    #[serde(rename = "Output", default)]
    pub(crate) output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KmsEncryptionRequest {
    #[serde(rename = "KMSKeyId", alias = "KMSKeyID")]
    pub(crate) kms_key_id: String,
    #[serde(rename = "Challenge", default, skip_serializing_if = "Option::is_none")]
    pub(crate) challenge: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KmsEncryptionResponse {
    #[serde(rename = "KMSCipherTextKey")]
    pub(crate) kms_cipher_text_key: String,
    #[serde(
        rename = "KMSCipherTextHash",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) kms_cipher_text_hash: Option<String>,
    #[serde(
        rename = "ChallengeAcknowledgement",
        default,
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub(crate) challenge_acknowledgement: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EncryptionChallengePayload {
    #[serde(rename = "Challenge")]
    pub(crate) challenge: String,
}

/// Zero-on-drop byte buffer used for transient KMS data-key and challenge
/// plaintexts so raw key material is scrubbed on all exit paths without `unsafe`.
pub(crate) struct ZeroizingBytes(Vec<u8>);

impl ZeroizingBytes {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for ZeroizingBytes {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&mut self.0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

fn zeroize_slice(bytes: &mut [u8]) {
    bytes.fill(0);
    std::hint::black_box(bytes);
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

pub(crate) struct KmsDataKeyMaterial {
    pub(crate) ciphertext_blob: Vec<u8>,
    pub(crate) plaintext_key: ZeroizingBytes,
    pub(crate) challenge_acknowledged: bool,
}

/// Client-side AWS SSM KMS session cipher matching upstream `encrypter.go` and
/// `amazon-ssm-agent` `blockcipher.go`:
/// - `plaintext_key` is 64 bytes from `kms:GenerateDataKey(NumberOfBytes=64)`.
/// - `plaintext_key[0..32]` is the agent->client key (`decryption_key` on client).
/// - `plaintext_key[32..64]` is the client->agent key (`encryption_key` on client).
/// - Wire payload is `[nonce (12B) || AES-256-GCM ciphertext || tag (16B)]` with
///   empty AAD.
pub(crate) struct SsmKmsCipher {
    encryption_key: ring::aead::LessSafeKey,
    decryption_key: ring::aead::LessSafeKey,
    challenge_verified: bool,
}

impl SsmKmsCipher {
    pub(crate) fn from_client_data_key(plaintext_key: &[u8]) -> Result<Self, String> {
        if plaintext_key.len() != SSM_KMS_DATA_KEY_LEN {
            return Err(format!(
                "invalid KMS plaintext data key length {} (expected {SSM_KMS_DATA_KEY_LEN})",
                plaintext_key.len()
            ));
        }
        let mut dec_raw = [0_u8; SSM_KMS_HALF_KEY_LEN];
        let mut enc_raw = [0_u8; SSM_KMS_HALF_KEY_LEN];
        dec_raw.copy_from_slice(&plaintext_key[..SSM_KMS_HALF_KEY_LEN]);
        enc_raw.copy_from_slice(&plaintext_key[SSM_KMS_HALF_KEY_LEN..]);
        let dec_res = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &dec_raw)
            .map(ring::aead::LessSafeKey::new);
        let enc_res = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &enc_raw)
            .map(ring::aead::LessSafeKey::new);
        zeroize_slice(&mut dec_raw);
        zeroize_slice(&mut enc_raw);
        let decryption_key =
            dec_res.map_err(|_| "failed to initialize SSM KMS decryption key".to_string())?;
        let encryption_key =
            enc_res.map_err(|_| "failed to initialize SSM KMS encryption key".to_string())?;
        Ok(Self {
            encryption_key,
            decryption_key,
            challenge_verified: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn new_agent_side_for_test(plaintext_key: &[u8]) -> Result<Self, String> {
        if plaintext_key.len() != SSM_KMS_DATA_KEY_LEN {
            return Err(format!(
                "invalid test agent KMS key length {}",
                plaintext_key.len()
            ));
        }
        let mut enc_raw = [0_u8; SSM_KMS_HALF_KEY_LEN];
        let mut dec_raw = [0_u8; SSM_KMS_HALF_KEY_LEN];
        enc_raw.copy_from_slice(&plaintext_key[..SSM_KMS_HALF_KEY_LEN]);
        dec_raw.copy_from_slice(&plaintext_key[SSM_KMS_HALF_KEY_LEN..]);
        let encryption_key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &enc_raw)
                .map_err(|_| "test agent enc key init failed".to_string())?,
        );
        let decryption_key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &dec_raw)
                .map_err(|_| "test agent dec key init failed".to_string())?,
        );
        zeroize_slice(&mut enc_raw);
        zeroize_slice(&mut dec_raw);
        Ok(Self {
            encryption_key,
            decryption_key,
            challenge_verified: true,
        })
    }

    pub(crate) fn challenge_verified(&self) -> bool {
        self.challenge_verified
    }

    pub(crate) fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let overhead = SSM_AES_GCM_NONCE_LEN + SSM_AES_GCM_TAG_LEN;
        let total = plaintext
            .len()
            .checked_add(overhead)
            .filter(|&len| len <= SSM_MAX_PAYLOAD_LEN as usize)
            .ok_or_else(|| "SSM plaintext exceeds maximum encrypted payload bound".to_string())?;
        let mut nonce_bytes = [0_u8; SSM_AES_GCM_NONCE_LEN];
        fill_secure_random(&mut nonce_bytes)?;
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(plaintext);
        let tag = self
            .encryption_key
            .seal_in_place_separate_tag(
                ring::aead::Nonce::assume_unique_for_key(nonce_bytes),
                ring::aead::Aad::empty(),
                &mut out[SSM_AES_GCM_NONCE_LEN..],
            )
            .map_err(|_| "SSM AES-256-GCM encryption failed".to_string())?;
        out.extend_from_slice(tag.as_ref());
        Ok(out)
    }

    pub(crate) fn decrypt(&self, ciphertext_with_nonce: &[u8]) -> Result<Vec<u8>, String> {
        let min_len = SSM_AES_GCM_NONCE_LEN + SSM_AES_GCM_TAG_LEN;
        if ciphertext_with_nonce.len() < min_len {
            return Err(format!(
                "SSM encrypted payload truncated: {} bytes (minimum {min_len})",
                ciphertext_with_nonce.len()
            ));
        }
        if ciphertext_with_nonce.len() > SSM_MAX_PAYLOAD_LEN as usize {
            return Err(format!(
                "SSM encrypted payload length {} exceeds maximum {}",
                ciphertext_with_nonce.len(),
                SSM_MAX_PAYLOAD_LEN
            ));
        }
        let mut nonce_bytes = [0_u8; SSM_AES_GCM_NONCE_LEN];
        nonce_bytes.copy_from_slice(&ciphertext_with_nonce[..SSM_AES_GCM_NONCE_LEN]);
        let mut buf = ciphertext_with_nonce[SSM_AES_GCM_NONCE_LEN..].to_vec();
        let plain_len = self
            .decryption_key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce_bytes),
                ring::aead::Aad::empty(),
                &mut buf,
            )
            .map_err(|_| "SSM AES-256-GCM authentication tag verification failed".to_string())?
            .len();
        buf.truncate(plain_len);
        Ok(buf)
    }

    pub(crate) fn decrypt_stream_payload(
        &self,
        ciphertext_with_nonce: &[u8],
    ) -> Result<Vec<u8>, String> {
        if !self.challenge_verified {
            return Err(
                "SSM encrypted stream payload arrived before EncChallengeRequest verification"
                    .to_string(),
            );
        }
        self.decrypt(ciphertext_with_nonce)
    }

    pub(crate) fn handle_challenge_request(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let req: EncryptionChallengePayload = serde_json::from_slice(payload)
            .map_err(|error| format!("invalid SSM EncChallengeRequest JSON: {error}"))?;
        let encrypted_challenge = base64_decode(&req.challenge)
            .map_err(|error| format!("invalid SSM EncChallengeRequest base64: {error}"))?;
        let plain_challenge = ZeroizingBytes::new(self.decrypt(&encrypted_challenge)?);
        let reencrypted = self.encrypt(plain_challenge.as_slice())?;
        self.challenge_verified = true;
        let resp = EncryptionChallengePayload {
            challenge: base64_encode(&reencrypted),
        };
        serde_json::to_vec(&resp)
            .map_err(|error| format!("failed to serialize EncChallengeResponse: {error}"))
    }
}

pub(crate) fn validate_kms_key_id(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 {
        return Err("invalid SSM KMSKeyId: must be 1..=2048 characters".to_string());
    }
    if trimmed.starts_with('-') {
        return Err("invalid SSM KMSKeyId: must not start with '-'".to_string());
    }
    if trimmed.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err("invalid SSM KMSKeyId: contains control characters".to_string());
    }
    Ok(trimmed.to_string())
}

#[cfg(test)]
type NoopKmsGenerator = fn(&str, Option<&str>) -> Result<KmsDataKeyMaterial, String>;

#[cfg(test)]
pub(crate) fn build_handshake_response(payload: &[u8]) -> Result<Vec<u8>, String> {
    let none_gen: Option<NoopKmsGenerator> = None;
    negotiate_handshake_response(payload, none_gen).map(|(bytes, _)| bytes)
}

pub(crate) fn negotiate_handshake_response<F>(
    payload: &[u8],
    mut kms_generator: Option<F>,
) -> Result<(Vec<u8>, Option<SsmKmsCipher>), String>
where
    F: FnMut(&str, Option<&str>) -> Result<KmsDataKeyMaterial, String>,
{
    let req: HandshakeRequestPayload = serde_json::from_slice(payload)
        .map_err(|error| format!("invalid SSM HandshakeRequest payload: {error}"))?;
    let mut processed = Vec::with_capacity(req.requested_client_actions.len());
    let mut negotiated_cipher: Option<SsmKmsCipher> = None;

    for action in &req.requested_client_actions {
        match action.action_type.as_str() {
            "KMSEncryption" => {
                if negotiated_cipher.is_some() {
                    return Err(
                        "duplicate KMSEncryption action in SSM HandshakeRequest".to_string()
                    );
                }
                let Some(ref mut generator) = kms_generator else {
                    return Err(
                        "SSM session requires AWS KMS session encryption (KMSEncryption), \
                         but no KMS key provider is configured"
                            .to_string(),
                    );
                };
                let kms_req: KmsEncryptionRequest =
                    serde_json::from_value(action.action_parameters.clone()).map_err(|error| {
                        format!("invalid SSM KMSEncryption ActionParameters: {error}")
                    })?;
                let kms_key_id = validate_kms_key_id(&kms_req.kms_key_id)?;
                let material = generator(&kms_key_id, kms_req.challenge.as_deref())?;
                if material.ciphertext_blob.is_empty() {
                    return Err("KMS GenerateDataKey returned empty CiphertextBlob".to_string());
                }
                let cipher = SsmKmsCipher::from_client_data_key(material.plaintext_key.as_slice())?;
                let action_result = serde_json::to_value(KmsEncryptionResponse {
                    kms_cipher_text_key: base64_encode(&material.ciphertext_blob),
                    kms_cipher_text_hash: None,
                    challenge_acknowledgement: material.challenge_acknowledged,
                })
                .map_err(|error| format!("failed to serialize KMSEncryptionResponse: {error}"))?;
                negotiated_cipher = Some(cipher);
                processed.push(ProcessedClientAction {
                    action_type: "KMSEncryption".to_string(),
                    action_status: 1, // Success
                    action_result: Some(action_result),
                    error: None,
                });
            }
            "SessionType" => {
                let session_type = action
                    .action_parameters
                    .get("SessionType")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match session_type {
                    "Port" | "Standard_Stream" | "InteractiveCommands" | "" => {
                        processed.push(ProcessedClientAction {
                            action_type: "SessionType".to_string(),
                            action_status: 1, // Success
                            action_result: None,
                            error: None,
                        });
                    }
                    other => {
                        return Err(format!("unsupported SSM SessionType '{other}'"));
                    }
                }
            }
            other => {
                processed.push(ProcessedClientAction {
                    action_type: other.to_string(),
                    action_status: 2, // Unsupported
                    action_result: None,
                    error: Some(format!("unsupported client action '{other}'")),
                });
            }
        }
    }
    let resp = HandshakeResponsePayload {
        client_version: SSM_CLIENT_VERSION.to_string(),
        processed_client_actions: processed,
        errors: Vec::new(),
    };
    let bytes = serde_json::to_vec(&resp)
        .map_err(|error| format!("failed to encode HandshakeResponse: {error}"))?;
    Ok((bytes, negotiated_cipher))
}

// ---------------------------------------------------------------------------
// 4. `SsmStreamState` — Reliable Sequence, Deferred ACK, & EWMA Retransmit
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct OutboundEntry {
    msg: AgentMessage,
    first_sent_ms: u64,
    last_sent_ms: u64,
    retransmits: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliverableFrame {
    pub(crate) sequence_number: i64,
    pub(crate) message_id: SsmUuid,
    pub(crate) payload_type: u32,
    pub(crate) flags: u64,
    pub(crate) payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InboundOutcome {
    /// Added to deliverable / out-of-order queue (or duplicate re-ACK queued).
    StreamBuffered,
    /// Processed an `acknowledge` for one of our outbound frames.
    AckProcessed { seq: i64 },
    /// Remote agent paused our outbound publication (`pause_publication`).
    PublicationPaused,
    /// Remote agent resumed our outbound publication (`start_publication`).
    PublicationResumed,
    /// Remote agent closed the SSM data channel (`channel_closed`).
    ChannelClosed { reason: String },
    /// Ignored non-critical control frame.
    Ignored,
}

pub(crate) struct SsmStreamState {
    next_outgoing_seq: i64,
    expected_incoming_seq: i64,
    unacked_outbound: BTreeMap<i64, OutboundEntry>,
    unacked_outbound_bytes: usize,
    pending_inbound: BTreeMap<i64, AgentMessage>,
    pending_inbound_bytes: usize,
    immediate_acks: VecDeque<AgentMessage>,
    publication_allowed: bool,
    handshake_complete: bool,
    srtt_ms: Option<f64>,
    rttvar_ms: f64,
    rto_ms: u64,
}

impl SsmStreamState {
    pub(crate) fn new() -> Self {
        Self {
            next_outgoing_seq: 0,
            expected_incoming_seq: 0,
            unacked_outbound: BTreeMap::new(),
            unacked_outbound_bytes: 0,
            pending_inbound: BTreeMap::new(),
            pending_inbound_bytes: 0,
            immediate_acks: VecDeque::new(),
            publication_allowed: true,
            handshake_complete: false,
            srtt_ms: None,
            rttvar_ms: 0.0,
            rto_ms: INITIAL_RTO_MS,
        }
    }

    #[cfg(test)]
    pub(crate) fn publication_allowed(&self) -> bool {
        self.publication_allowed
    }

    pub(crate) fn handshake_complete(&self) -> bool {
        self.handshake_complete
    }

    #[cfg(test)]
    pub(crate) fn rto_ms(&self) -> u64 {
        self.rto_ms
    }

    #[cfg(test)]
    pub(crate) fn expected_incoming_seq(&self) -> i64 {
        self.expected_incoming_seq
    }

    #[cfg(test)]
    pub(crate) fn unacked_count(&self) -> usize {
        self.unacked_outbound.len()
    }

    pub(crate) fn can_send_outbound(&self) -> bool {
        self.publication_allowed
            && self.unacked_outbound.len() < MAX_OUTBOUND_UNACKED_FRAMES
            && self.unacked_outbound_bytes < MAX_OUTBOUND_UNACKED_BYTES
    }

    /// Build, sequence, and record an outbound `input_stream_data` frame.
    pub(crate) fn queue_outbound_data(
        &mut self,
        payload_type: u32,
        payload: Vec<u8>,
        flags: u64,
        now_ms: u64,
    ) -> Result<AgentMessage, String> {
        let seq = self.next_outgoing_seq;
        let msg = AgentMessage::new(
            MSG_INPUT_STREAM_DATA,
            seq,
            flags,
            SsmUuid::new_v4()?,
            now_ms,
            payload_type,
            payload,
        )?;
        self.next_outgoing_seq = self.next_outgoing_seq.saturating_add(1);
        self.unacked_outbound_bytes = self
            .unacked_outbound_bytes
            .saturating_add(msg.payload.len());
        self.unacked_outbound.insert(
            seq,
            OutboundEntry {
                msg: msg.clone(),
                first_sent_ms: now_ms,
                last_sent_ms: now_ms,
                retransmits: 0,
            },
        );
        Ok(msg)
    }

    /// Process a decoded `AgentMessage` from MGS.
    ///
    /// Note **deferred-ACK backpressure**: in-order `output_stream_data` frames
    /// are staged in `pending_inbound` WITHOUT immediately emitting an
    /// `acknowledge`! The caller drains frames via `take_next_deliverable()` and
    /// calls `commit_inbound_delivered()` ONLY after the local socket / smux
    /// sink accepts the bytes. Duplicate frames (`seq < expected_incoming_seq`)
    /// immediately enqueue an `acknowledge` in `immediate_acks`.
    pub(crate) fn ingest_inbound(
        &mut self,
        msg: AgentMessage,
        now_ms: u64,
    ) -> Result<InboundOutcome, String> {
        match msg.message_type.as_str() {
            MSG_OUTPUT_STREAM_DATA => {
                let seq = msg.sequence_number;
                if seq < self.expected_incoming_seq {
                    // Already delivered to local sink: re-ACK immediately so ssm-agent
                    // stops retransmitting.
                    let ack = build_ack_frame(MSG_OUTPUT_STREAM_DATA, msg.message_id, seq, now_ms)?;
                    self.immediate_acks.push_back(ack);
                    return Ok(InboundOutcome::StreamBuffered);
                }
                if self.pending_inbound.contains_key(&seq) {
                    return Ok(InboundOutcome::StreamBuffered);
                }
                if seq > self.expected_incoming_seq
                    && (self.pending_inbound.len() >= MAX_INBOUND_OOO_FRAMES
                        || self.pending_inbound_bytes.saturating_add(msg.payload.len())
                            > MAX_INBOUND_OOO_BYTES)
                {
                    // Out-of-order buffer full: drop without ACKing so ssm-agent
                    // retransmits once the gap fills (lossless backpressure).
                    return Ok(InboundOutcome::StreamBuffered);
                }
                self.pending_inbound_bytes =
                    self.pending_inbound_bytes.saturating_add(msg.payload.len());
                self.pending_inbound.insert(seq, msg);
                Ok(InboundOutcome::StreamBuffered)
            }
            MSG_ACKNOWLEDGE => {
                let ack: AcknowledgeContent = serde_json::from_slice(&msg.payload)
                    .map_err(|error| format!("invalid SSM AcknowledgeContent: {error}"))?;
                let seq = ack.acknowledged_message_sequence_number;
                if let Some(entry) = self.unacked_outbound.remove(&seq) {
                    self.unacked_outbound_bytes = self
                        .unacked_outbound_bytes
                        .saturating_sub(entry.msg.payload.len());
                    // Karn's algorithm: only sample RTT on non-retransmitted frames.
                    if entry.retransmits == 0 && now_ms >= entry.first_sent_ms {
                        self.update_rtt((now_ms - entry.first_sent_ms) as f64);
                    }
                }
                Ok(InboundOutcome::AckProcessed { seq })
            }
            MSG_PAUSE_PUBLICATION => {
                self.publication_allowed = false;
                Ok(InboundOutcome::PublicationPaused)
            }
            MSG_START_PUBLICATION => {
                self.publication_allowed = true;
                Ok(InboundOutcome::PublicationResumed)
            }
            MSG_CHANNEL_CLOSED => {
                let reason = serde_json::from_slice::<ChannelClosedPayload>(&msg.payload)
                    .ok()
                    .map(|c| {
                        if c.output.trim().is_empty() {
                            "channel_closed".to_string()
                        } else {
                            c.output.trim().to_string()
                        }
                    })
                    .unwrap_or_else(|| "channel_closed".to_string());
                Ok(InboundOutcome::ChannelClosed { reason })
            }
            _ => Ok(InboundOutcome::Ignored),
        }
    }

    /// Peekwhether the next contiguous in-order frame (`expected_incoming_seq`)
    /// is available for delivery to the local sink. Does NOT advance
    /// `expected_incoming_seq` until `commit_inbound_delivered` is called!
    pub(crate) fn peek_next_deliverable(&self) -> Option<DeliverableFrame> {
        let msg = self.pending_inbound.get(&self.expected_incoming_seq)?;
        Some(DeliverableFrame {
            sequence_number: msg.sequence_number,
            message_id: msg.message_id,
            payload_type: msg.payload_type,
            flags: msg.flags,
            payload: msg.payload.clone(),
        })
    }

    /// Commit delivery of the head-of-line frame (`expected_incoming_seq`) AFTER
    /// writing its bytes to the local socket / smux sink, advancing
    /// `expected_incoming_seq` and returning the `acknowledge` `AgentMessage`
    /// to transmit back to MGS.
    pub(crate) fn commit_inbound_delivered(
        &mut self,
        delivered: &DeliverableFrame,
        now_ms: u64,
    ) -> Result<AgentMessage, String> {
        if delivered.sequence_number != self.expected_incoming_seq {
            return Err(format!(
                "out-of-order commit: got seq {}, expected {}",
                delivered.sequence_number, self.expected_incoming_seq
            ));
        }
        if let Some(removed) = self.pending_inbound.remove(&self.expected_incoming_seq) {
            self.pending_inbound_bytes = self
                .pending_inbound_bytes
                .saturating_sub(removed.payload.len());
        }
        if delivered.payload_type == payload_type::HANDSHAKE_COMPLETE {
            self.handshake_complete = true;
        }
        self.expected_incoming_seq = self.expected_incoming_seq.saturating_add(1);
        build_ack_frame(
            MSG_OUTPUT_STREAM_DATA,
            delivered.message_id,
            delivered.sequence_number,
            now_ms,
        )
    }

    pub(crate) fn drain_immediate_acks(&mut self) -> Vec<AgentMessage> {
        self.immediate_acks.drain(..).collect()
    }

    pub(crate) fn poll_retransmits(&mut self, now_ms: u64) -> Vec<AgentMessage> {
        if !self.publication_allowed {
            return Vec::new();
        }
        let base_rto = self.rto_ms;
        let mut due = Vec::new();
        for entry in self.unacked_outbound.values_mut() {
            let shift = entry.retransmits.min(5);
            let effective_rto = base_rto.saturating_mul(1_u64 << shift).min(MAX_RTO_MS);
            if now_ms.saturating_sub(entry.last_sent_ms) >= effective_rto {
                entry.last_sent_ms = now_ms;
                entry.retransmits = entry.retransmits.saturating_add(1);
                due.push(entry.msg.clone());
            }
        }
        due
    }

    /// Prepare all unacknowledged outbound frames to be replayed immediately in
    /// sequence order after `ResumeSession` reconnects the MGS WebSocket.
    pub(crate) fn prepare_for_resume(&mut self, _now_ms: u64) {
        self.publication_allowed = true;
        for entry in self.unacked_outbound.values_mut() {
            entry.last_sent_ms = 0;
        }
    }

    fn update_rtt(&mut self, sample_ms: f64) {
        let sample = sample_ms.max(1.0);
        match self.srtt_ms {
            None => {
                self.srtt_ms = Some(sample);
                self.rttvar_ms = sample / 2.0;
            }
            Some(srtt) => {
                let alpha = 0.125;
                let beta = 0.25;
                self.rttvar_ms = (1.0 - beta) * self.rttvar_ms + beta * (srtt - sample).abs();
                self.srtt_ms = Some((1.0 - alpha) * srtt + alpha * sample);
            }
        }
        let srtt = self.srtt_ms.unwrap_or(sample);
        let rto = (srtt + (4.0 * self.rttvar_ms).max(50.0)).round() as u64;
        self.rto_ms = rto.clamp(MIN_RTO_MS, MAX_RTO_MS);
    }
}

pub(crate) fn build_ack_frame(
    acked_type: &str,
    acked_id: SsmUuid,
    acked_seq: i64,
    now_ms: u64,
) -> Result<AgentMessage, String> {
    let content = AcknowledgeContent {
        acknowledged_message_type: acked_type.to_string(),
        acknowledged_message_id: acked_id.to_string(),
        acknowledged_message_sequence_number: acked_seq,
        is_sequential_message: true,
    };
    let payload = serde_json::to_vec(&content)
        .map_err(|error| format!("failed to serialize AcknowledgeContent: {error}"))?;
    AgentMessage::new(
        MSG_ACKNOWLEDGE,
        0,
        SSM_FLAG_DATA,
        SsmUuid::new_v4()?,
        now_ms,
        0,
        payload,
    )
}

// ---------------------------------------------------------------------------
// 5. `SmuxCodec` (`xtaci/smux` v1 for `AWS-StartPortForwardingSession`)
// ---------------------------------------------------------------------------

pub(crate) const SMUX_VERSION: u8 = 1;
pub(crate) const SMUX_CMD_SYN: u8 = 0;
pub(crate) const SMUX_CMD_FIN: u8 = 1;
pub(crate) const SMUX_CMD_PSH: u8 = 2;
pub(crate) const SMUX_CMD_NOP: u8 = 3;
pub(crate) const SMUX_HEADER_LEN: usize = 8;
pub(crate) const SMUX_MAX_CHUNK: usize = 32 * 1024;
const SMUX_MAX_ACCUM_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SmuxFrame {
    pub(crate) cmd: u8,
    pub(crate) stream_id: u32,
    pub(crate) data: Vec<u8>,
}

impl SmuxFrame {
    pub(crate) fn syn(stream_id: u32) -> Self {
        Self {
            cmd: SMUX_CMD_SYN,
            stream_id,
            data: Vec::new(),
        }
    }

    pub(crate) fn fin(stream_id: u32) -> Self {
        Self {
            cmd: SMUX_CMD_FIN,
            stream_id,
            data: Vec::new(),
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn nop() -> Self {
        Self {
            cmd: SMUX_CMD_NOP,
            stream_id: 0,
            data: Vec::new(),
        }
    }

    pub(crate) fn psh_chunks(stream_id: u32, payload: &[u8]) -> Vec<Self> {
        if payload.is_empty() {
            return Vec::new();
        }
        payload
            .chunks(SMUX_MAX_CHUNK)
            .map(|chunk| Self {
                cmd: SMUX_CMD_PSH,
                stream_id,
                data: chunk.to_vec(),
            })
            .collect()
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, String> {
        if self.data.len() > u16::MAX as usize {
            return Err(format!("smux frame data too large: {}", self.data.len()));
        }
        let mut out = Vec::with_capacity(SMUX_HEADER_LEN + self.data.len());
        out.push(SMUX_VERSION);
        out.push(self.cmd);
        out.extend_from_slice(&(self.data.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.stream_id.to_le_bytes());
        out.extend_from_slice(&self.data);
        Ok(out)
    }
}

pub(crate) struct SmuxDecoder {
    buf: Vec<u8>,
}

impl SmuxDecoder {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub(crate) fn push(&mut self, incoming: &[u8]) -> Result<Vec<SmuxFrame>, String> {
        if self.buf.len().saturating_add(incoming.len()) > SMUX_MAX_ACCUM_BYTES {
            self.buf.clear();
            return Err("smux decoder buffer exceeded 4 MiB safety cap".to_string());
        }
        self.buf.extend_from_slice(incoming);
        let mut frames = Vec::new();
        let mut offset = 0_usize;
        while self.buf.len().saturating_sub(offset) >= SMUX_HEADER_LEN {
            let ver = self.buf[offset];
            let cmd = self.buf[offset + 1];
            if ver != SMUX_VERSION {
                self.buf.clear();
                return Err(format!("invalid smux version {ver} (expected 1)"));
            }
            if cmd > SMUX_CMD_NOP {
                self.buf.clear();
                return Err(format!("invalid smux command {cmd}"));
            }
            let len = u16::from_le_bytes([self.buf[offset + 2], self.buf[offset + 3]]) as usize;
            let sid = u32::from_le_bytes([
                self.buf[offset + 4],
                self.buf[offset + 5],
                self.buf[offset + 6],
                self.buf[offset + 7],
            ]);
            if (cmd == SMUX_CMD_SYN || cmd == SMUX_CMD_FIN || cmd == SMUX_CMD_NOP) && len != 0 {
                self.buf.clear();
                return Err(format!(
                    "smux control frame cmd={cmd} must have zero length, got {len}"
                ));
            }
            if self.buf.len().saturating_sub(offset) < SMUX_HEADER_LEN + len {
                break;
            }
            let data = self.buf[offset + SMUX_HEADER_LEN..offset + SMUX_HEADER_LEN + len].to_vec();
            frames.push(SmuxFrame {
                cmd,
                stream_id: sid,
                data,
            });
            offset += SMUX_HEADER_LEN + len;
        }
        if offset > 0 {
            self.buf.drain(..offset);
        }
        Ok(frames)
    }
}

// ---------------------------------------------------------------------------
// 6. `ClientLeaseTracker` (`Requested` / `Releasing` for Round-Trip Transports)
// ---------------------------------------------------------------------------

/// Client-side lease tracker modeling in-flight round-trip states (`Requested`
/// and `Releasing`, #115) without changing the daemon's wire `LeaseTransition`
/// (`Taken | Released | Revoked`) consumed by native macOS and Windows clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientLeasePhase {
    Unheld,
    Requested {
        holder: String,
        requested_at_ms: u64,
    },
    Held {
        holder: String,
        generation: u64,
    },
    Releasing {
        holder: String,
        generation: Option<u64>,
        requested_at_ms: u64,
    },
}

#[derive(Debug, Clone)]
pub struct ClientLeaseTracker {
    phase: ClientLeasePhase,
}

impl Default for ClientLeaseTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientLeaseTracker {
    pub fn new() -> Self {
        Self {
            phase: ClientLeasePhase::Unheld,
        }
    }

    pub fn phase(&self) -> &ClientLeasePhase {
        &self.phase
    }

    pub fn begin_take(&mut self, holder: &str, now_ms: u64) -> Result<(), String> {
        let valid = validate_holder(holder)?;
        if matches!(self.phase, ClientLeasePhase::Releasing { .. }) {
            return Err("lease release is still in flight".to_string());
        }
        self.phase = ClientLeasePhase::Requested {
            holder: valid,
            requested_at_ms: now_ms,
        };
        Ok(())
    }

    pub fn confirm_taken(&mut self, holder: &str, generation: u64) -> Result<(), String> {
        let valid = validate_holder(holder)?;
        self.phase = ClientLeasePhase::Held {
            holder: valid,
            generation,
        };
        Ok(())
    }

    pub fn fail_take(&mut self) {
        if matches!(self.phase, ClientLeasePhase::Requested { .. }) {
            self.phase = ClientLeasePhase::Unheld;
        }
    }

    pub fn begin_release(&mut self, now_ms: u64) -> Result<(), String> {
        match &self.phase {
            ClientLeasePhase::Held { holder, generation } => {
                self.phase = ClientLeasePhase::Releasing {
                    holder: holder.clone(),
                    generation: Some(*generation),
                    requested_at_ms: now_ms,
                };
                Ok(())
            }
            ClientLeasePhase::Requested { .. } => {
                Err("cannot release while lease request is still in flight".to_string())
            }
            ClientLeasePhase::Releasing { .. } => {
                Err("lease release is already in flight".to_string())
            }
            ClientLeasePhase::Unheld => Err("pane keyboard is not held".to_string()),
        }
    }

    pub fn confirm_released(&mut self) {
        self.phase = ClientLeasePhase::Unheld;
    }

    pub fn fail_release(&mut self) {
        if let ClientLeasePhase::Releasing {
            holder, generation, ..
        } = &self.phase
        {
            self.phase = ClientLeasePhase::Held {
                holder: holder.clone(),
                generation: generation.unwrap_or(0),
            };
        }
    }

    pub fn observe_server_lease(&mut self, info: &LeaseInfo) {
        match &info.holder {
            Some(holder) => {
                self.phase = ClientLeasePhase::Held {
                    holder: holder.clone(),
                    generation: info.generation.unwrap_or(0),
                };
            }
            None => {
                self.phase = ClientLeasePhase::Unheld;
            }
        }
    }

    /// Gate local keystrokes before sending across a high-RTT SSM link so
    /// keystrokes typed while `Requested` or `Releasing` do not race the
    /// transition.
    pub(crate) fn can_send_local_input(
        &self,
        policy: LeasePolicy,
        our_holder: &str,
    ) -> Result<(), String> {
        match &self.phase {
            ClientLeasePhase::Requested { .. } => {
                Err("lease take is still in flight over remote transport".to_string())
            }
            ClientLeasePhase::Releasing { .. } => {
                Err("lease release is still in flight over remote transport".to_string())
            }
            ClientLeasePhase::Held { holder, .. } => {
                if holder == our_holder {
                    Ok(())
                } else {
                    Err(format!("pane keyboard is held by {holder}"))
                }
            }
            ClientLeasePhase::Unheld => match policy {
                LeasePolicy::Open => Ok(()),
                LeasePolicy::Required => {
                    Err("pane keyboard is unheld and lease_policy is required".to_string())
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// 7. RFC 6455 Synchronous WebSocket Client (`ws://` + `wss://` via `rustls`)
// ---------------------------------------------------------------------------

const MAX_WS_FRAME_LEN: usize = 12 * 1024 * 1024;
const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Compact FIPS 180-4 SHA-1 used strictly for RFC 6455 §4.2.2 `Sec-WebSocket-Accept`
/// verification (never for cryptographic signatures or digests).
pub(crate) fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0_u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999_u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1_u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC_u32),
                _ => (b ^ c ^ d, 0xCA62C1D6_u32),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut out = [0_u8; 20];
    out[0..4].copy_from_slice(&h0.to_be_bytes());
    out[4..8].copy_from_slice(&h1.to_be_bytes());
    out[8..12].copy_from_slice(&h2.to_be_bytes());
    out[12..16].copy_from_slice(&h3.to_be_bytes());
    out[16..20].copy_from_slice(&h4.to_be_bytes());
    out
}

pub(crate) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    fn sextet(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let clean: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if clean.is_empty() {
        return Ok(Vec::new());
    }
    if !clean.len().is_multiple_of(4) {
        return Err(format!(
            "invalid base64 length {} (must be a multiple of 4)",
            clean.len()
        ));
    }

    let mut out = Vec::with_capacity((clean.len() / 4) * 3);
    let chunks = clean.as_chunks::<4>().0;
    let total_chunks = chunks.len();
    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        let is_last = chunk_idx + 1 == total_chunks;
        let v0 = sextet(chunk[0]).ok_or_else(|| "invalid base64 character".to_string())?;
        let v1 = sextet(chunk[1]).ok_or_else(|| "invalid base64 character".to_string())?;
        match (chunk[2], chunk[3]) {
            (b'=', b'=') if is_last => {
                if (v1 & 0x0F) != 0 {
                    return Err("non-canonical trailing base64 bits".to_string());
                }
                out.push((v0 << 2) | (v1 >> 4));
            }
            (c2, b'=') if is_last && c2 != b'=' => {
                let v2 = sextet(c2).ok_or_else(|| "invalid base64 character".to_string())?;
                if (v2 & 0x03) != 0 {
                    return Err("non-canonical trailing base64 bits".to_string());
                }
                out.push((v0 << 2) | (v1 >> 4));
                out.push(((v1 & 0x0F) << 4) | (v2 >> 2));
            }
            (c2, c3) if c2 != b'=' && c3 != b'=' => {
                let v2 = sextet(c2).ok_or_else(|| "invalid base64 character".to_string())?;
                let v3 = sextet(c3).ok_or_else(|| "invalid base64 character".to_string())?;
                out.push((v0 << 2) | (v1 >> 4));
                out.push(((v1 & 0x0F) << 4) | (v2 >> 2));
                out.push(((v2 & 0x03) << 6) | v3);
            }
            _ => {
                return Err("invalid base64 padding".to_string());
            }
        }
    }
    Ok(out)
}

pub(crate) fn compute_websocket_accept(sec_key: &str) -> String {
    let mut input = Vec::with_capacity(sec_key.len() + WS_GUID.len());
    input.extend_from_slice(sec_key.as_bytes());
    input.extend_from_slice(WS_GUID);
    base64_encode(&sha1_digest(&input))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawWsFrame {
    pub(crate) fin: bool,
    pub(crate) opcode: u8,
    pub(crate) payload: Vec<u8>,
}

/// Pure slice parser for RFC 6455 frames, returning `Ok(None)` when more bytes
/// are needed, `Ok(Some((frame, consumed_bytes)))` when complete, or `Err` on
/// protocol / cap violations.
pub(crate) fn decode_ws_frame_slice(buf: &[u8]) -> Result<Option<(RawWsFrame, usize)>, String> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if (b0 & 0x70) != 0 {
        return Err(format!("WebSocket RSV bits must be 0, got {b0:#x}"));
    }
    let fin = (b0 & 0x80) != 0;
    let opcode = b0 & 0x0F;
    let masked = (b1 & 0x80) != 0;
    let len7 = (b1 & 0x7F) as usize;

    if opcode >= 0x8 && (!fin || len7 > 125) {
        return Err("invalid fragmented or oversized WebSocket control frame".to_string());
    }

    let (payload_len, mut header_len) = match len7 {
        0..=125 => (len7, 2_usize),
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([buf[2], buf[3]]) as usize, 4_usize)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let raw = u64::from_be_bytes(buf[2..10].try_into().unwrap());
            if raw > MAX_WS_FRAME_LEN as u64 {
                return Err(format!(
                    "WebSocket frame length {raw} exceeds {MAX_WS_FRAME_LEN} cap"
                ));
            }
            (raw as usize, 10_usize)
        }
        _ => unreachable!(),
    };

    if payload_len > MAX_WS_FRAME_LEN {
        return Err(format!(
            "WebSocket frame length {payload_len} exceeds {MAX_WS_FRAME_LEN} cap"
        ));
    }

    let mask_key = if masked {
        if buf.len() < header_len + 4 {
            return Ok(None);
        }
        let key = [
            buf[header_len],
            buf[header_len + 1],
            buf[header_len + 2],
            buf[header_len + 3],
        ];
        header_len += 4;
        Some(key)
    } else {
        None
    };

    if buf.len() < header_len + payload_len {
        return Ok(None);
    }

    let mut payload = buf[header_len..header_len + payload_len].to_vec();
    if let Some(mask) = mask_key {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= mask[i & 3];
        }
    }

    Ok(Some((
        RawWsFrame {
            fin,
            opcode,
            payload,
        },
        header_len + payload_len,
    )))
}

pub(crate) fn encode_ws_client_frame(opcode: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() > MAX_WS_FRAME_LEN {
        return Err(format!(
            "WebSocket outbound frame too large: {}",
            payload.len()
        ));
    }
    let mut mask = [0_u8; 4];
    fill_secure_random(&mut mask)?;
    let mut out = Vec::with_capacity(14 + payload.len());
    out.push(0x80 | (opcode & 0x0F)); // FIN=1
    if payload.len() <= 125 {
        out.push(0x80 | (payload.len() as u8));
    } else if payload.len() <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    for (i, &b) in payload.iter().enumerate() {
        out.push(b ^ mask[i & 3]);
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedWsUrl {
    tls: bool,
    host: String,
    port: u16,
    path_and_query: String,
}

fn parse_ws_url(raw: &str) -> Result<ParsedWsUrl, String> {
    let trimmed = raw.trim();
    let (tls, rest, default_port) = if let Some(r) = trimmed.strip_prefix("wss://") {
        (true, r, 443_u16)
    } else if let Some(r) = trimmed.strip_prefix("ws://") {
        (false, r, 80_u16)
    } else {
        return Err(format!(
            "unsupported StreamUrl scheme (expected wss:// or ws://): '{trimmed}'"
        ));
    };

    let (authority, path_and_query) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err("StreamUrl has empty host".to_string());
    }

    let (host, port) = if let Some(bracket_rest) = authority.strip_prefix('[') {
        let end = bracket_rest
            .find(']')
            .ok_or_else(|| "unclosed IPv6 bracket in StreamUrl".to_string())?;
        let h = bracket_rest[..end].to_string();
        let after = &bracket_rest[end + 1..];
        let p = if let Some(port_str) = after.strip_prefix(':') {
            port_str
                .parse::<u16>()
                .map_err(|_| format!("invalid port in StreamUrl: '{port_str}'"))?
        } else {
            default_port
        };
        (h, p)
    } else if let Some((h, port_str)) = authority.rsplit_once(':') {
        let p = port_str
            .parse::<u16>()
            .map_err(|_| format!("invalid port in StreamUrl: '{port_str}'"))?;
        (h.to_string(), p)
    } else {
        (authority.to_string(), default_port)
    };

    if host.is_empty() || port == 0 {
        return Err("invalid host or port in StreamUrl".to_string());
    }
    Ok(ParsedWsUrl {
        tls,
        host,
        port,
        path_and_query,
    })
}

fn rustls_client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut roots = rustls::RootCertStore::empty();
            roots.add_parsable_certificates(
                webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().cloned(),
            );
            let cfg = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("rustls protocol config error: {e}"))?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(cfg))
        })
        .clone()
}

enum WsIo {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl WsIo {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.set_read_timeout(timeout),
            Self::Tls(tls) => tls.get_ref().set_read_timeout(timeout),
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Plain(tcp) => {
                let _ = tcp.shutdown(Shutdown::Both);
            }
            Self::Tls(tls) => {
                let _ = tls.get_ref().shutdown(Shutdown::Both);
            }
        }
    }
}

impl Read for WsIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.read(buf),
            Self::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for WsIo {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.write(buf),
            Self::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.flush(),
            Self::Tls(tls) => tls.flush(),
        }
    }
}

#[derive(Debug)]
pub(crate) enum WsPoll {
    Binary(Vec<u8>),
    Text,
    WouldBlock,
    Closed,
}

pub(crate) struct WsStream {
    io: WsIo,
    read_buf: Vec<u8>,
    frag_opcode: Option<u8>,
    frag_buf: Vec<u8>,
}

impl WsStream {
    pub(crate) fn connect(stream_url: &str, connect_timeout: Duration) -> Result<Self, String> {
        use std::net::ToSocketAddrs;
        let parsed = parse_ws_url(stream_url)?;
        let addr_spec = format!("{}:{}", parsed.host, parsed.port);
        let addrs: Vec<_> = addr_spec
            .to_socket_addrs()
            .map_err(|error| format!("failed to resolve MGS host '{}': {error}", parsed.host))?
            .collect();
        if addrs.is_empty() {
            return Err(format!("no addresses resolved for '{}'", parsed.host));
        }

        let mut last_err = String::new();
        let mut connected_tcp = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, connect_timeout) {
                Ok(tcp) => {
                    let _ = tcp.set_nodelay(true);
                    let _ = tcp.set_read_timeout(Some(connect_timeout));
                    let _ = tcp.set_write_timeout(Some(connect_timeout));
                    connected_tcp = Some(tcp);
                    break;
                }
                Err(error) => {
                    last_err = error.to_string();
                }
            }
        }
        let tcp = connected_tcp.ok_or_else(|| {
            format!(
                "failed to connect to SSM StreamUrl host '{}:{}': {last_err}",
                parsed.host, parsed.port
            )
        })?;

        let mut io = if parsed.tls {
            let tls_cfg = rustls_client_config()?;
            let server_name = rustls_pki_types::ServerName::try_from(parsed.host.clone())
                .map_err(|e| format!("invalid TLS server name '{}': {e}", parsed.host))?;
            let conn = rustls::ClientConnection::new(tls_cfg, server_name)
                .map_err(|e| format!("TLS connection init failed: {e}"))?;
            WsIo::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
        } else {
            WsIo::Plain(tcp)
        };

        // Perform RFC 6455 HTTP/1.1 Upgrade handshake.
        let mut key_bytes = [0_u8; 16];
        fill_secure_random(&mut key_bytes)?;
        let sec_key = base64_encode(&key_bytes);
        let expected_accept = compute_websocket_accept(&sec_key);

        let host_header =
            if (parsed.tls && parsed.port == 443) || (!parsed.tls && parsed.port == 80) {
                parsed.host.clone()
            } else {
                format!("{}:{}", parsed.host, parsed.port)
            };

        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
            parsed.path_and_query, host_header, sec_key
        );
        io.write_all(request.as_bytes())
            .and_then(|_| io.flush())
            .map_err(|e| format!("WebSocket upgrade write failed: {e}"))?;

        let mut resp_buf = Vec::with_capacity(1024);
        let header_end = loop {
            if let Some(pos) = resp_buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            if resp_buf.len() >= 16 * 1024 {
                return Err("WebSocket upgrade response headers exceeded 16 KiB".to_string());
            }
            let mut chunk = [0_u8; 512];
            let n = io
                .read(&mut chunk)
                .map_err(|e| format!("WebSocket upgrade read failed: {e}"))?;
            if n == 0 {
                return Err("EOF during WebSocket upgrade handshake".to_string());
            }
            resp_buf.extend_from_slice(&chunk[..n]);
        };

        let head_str = std::str::from_utf8(&resp_buf[..header_end])
            .map_err(|_| "WebSocket upgrade headers are not valid UTF-8".to_string())?;
        let mut lines = head_str.split("\r\n");
        let status_line = lines.next().unwrap_or("");
        if !status_line.starts_with("HTTP/1.1 101") {
            return Err(format!("WebSocket upgrade rejected by MGS: {status_line}"));
        }

        let mut accept_ok = false;
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("sec-websocket-accept")
                    && constant_time_bytes_eq(v.trim().as_bytes(), expected_accept.as_bytes())
                {
                    accept_ok = true;
                }
            }
        }
        if !accept_ok {
            return Err("WebSocket upgrade missing or invalid Sec-WebSocket-Accept".to_string());
        }

        let leftover = resp_buf[header_end..].to_vec();
        Ok(Self {
            io,
            read_buf: leftover,
            frag_opcode: None,
            frag_buf: Vec::new(),
        })
    }

    pub(crate) fn set_poll_timeout(&self, timeout: Option<Duration>) {
        let _ = self.io.set_read_timeout(timeout);
    }

    pub(crate) fn send_text(&mut self, text: &str) -> Result<(), String> {
        let frame = encode_ws_client_frame(0x1, text.as_bytes())?;
        self.io
            .write_all(&frame)
            .and_then(|_| self.io.flush())
            .map_err(|e| format!("WebSocket text send failed: {e}"))
    }

    pub(crate) fn send_binary(&mut self, bytes: &[u8]) -> Result<(), String> {
        let frame = encode_ws_client_frame(0x2, bytes)?;
        self.io
            .write_all(&frame)
            .and_then(|_| self.io.flush())
            .map_err(|e| format!("WebSocket binary send failed: {e}"))
    }

    pub(crate) fn send_ping(&mut self, payload: &[u8]) -> Result<(), String> {
        let frame = encode_ws_client_frame(0x9, &payload[..payload.len().min(125)])?;
        self.io
            .write_all(&frame)
            .and_then(|_| self.io.flush())
            .map_err(|e| format!("WebSocket ping send failed: {e}"))
    }

    pub(crate) fn close(&mut self) {
        if let Ok(frame) = encode_ws_client_frame(0x8, &[]) {
            let _ = self.io.write_all(&frame);
            let _ = self.io.flush();
        }
        self.io.shutdown();
    }

    pub(crate) fn poll_message(&mut self) -> Result<WsPoll, String> {
        loop {
            if let Some((frame, consumed)) = decode_ws_frame_slice(&self.read_buf)? {
                self.read_buf.drain(..consumed);
                match frame.opcode {
                    0x8 => {
                        return Ok(WsPoll::Closed);
                    }
                    0x9 => {
                        // Auto-Pong
                        let pong = encode_ws_client_frame(0xA, &frame.payload)?;
                        let _ = self.io.write_all(&pong).and_then(|_| self.io.flush());
                        continue;
                    }
                    0xA => {
                        continue;
                    }
                    0x1 | 0x2 => {
                        if frame.fin {
                            if self.frag_opcode.is_some() {
                                return Err("WebSocket new data frame started mid-fragmentation"
                                    .to_string());
                            }
                            return if frame.opcode == 0x2 {
                                Ok(WsPoll::Binary(frame.payload))
                            } else {
                                Ok(WsPoll::Text)
                            };
                        }
                        self.frag_opcode = Some(frame.opcode);
                        self.frag_buf = frame.payload;
                        continue;
                    }
                    0x0 => {
                        let Some(orig_op) = self.frag_opcode else {
                            return Err("WebSocket continuation frame without start".to_string());
                        };
                        if self.frag_buf.len().saturating_add(frame.payload.len())
                            > MAX_WS_FRAME_LEN
                        {
                            return Err(
                                "WebSocket fragmented message exceeded 12 MiB cap".to_string()
                            );
                        }
                        self.frag_buf.extend_from_slice(&frame.payload);
                        if frame.fin {
                            self.frag_opcode = None;
                            let complete = std::mem::take(&mut self.frag_buf);
                            return if orig_op == 0x2 {
                                Ok(WsPoll::Binary(complete))
                            } else {
                                Ok(WsPoll::Text)
                            };
                        }
                        continue;
                    }
                    other => {
                        return Err(format!("unsupported WebSocket opcode {other:#x}"));
                    }
                }
            }

            let mut tmp = [0_u8; 16 * 1024];
            match self.io.read(&mut tmp) {
                Ok(0) => return Ok(WsPoll::Closed),
                Ok(n) => {
                    if self.read_buf.len().saturating_add(n) > MAX_WS_FRAME_LEN + 64 {
                        return Err("WebSocket read buffer exceeded cap".to_string());
                    }
                    self.read_buf.extend_from_slice(&tmp[..n]);
                }
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    return Ok(WsPoll::WouldBlock);
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(format!("WebSocket read error: {err}")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 8. AWS CLI + `session-manager-plugin` Capture Shim & Discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SsmSessionCredentials {
    #[serde(rename = "SessionId")]
    pub(crate) session_id: String,
    #[serde(rename = "StreamUrl")]
    pub(crate) stream_url: String,
    #[serde(rename = "TokenValue")]
    pub(crate) token_value: String,
}

pub(crate) fn is_ssm_plugin_capture_invocation(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == SSM_PLUGIN_CAPTURE_ARG) {
        return true;
    }
    args.first()
        .and_then(|arg0| Path::new(arg0).file_stem())
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.eq_ignore_ascii_case("session-manager-plugin"))
}

/// Entry point when AWS CLI v2 invokes our shim (`session-manager-plugin '<json>' ...`):
/// captures `{SessionId, StreamUrl, TokenValue}` into `SGIAN_SSM_CAPTURE_FILE` (`0600`)
/// and exits cleanly so Sgian itself drives the MGS WebSocket directly.
pub(crate) fn run_ssm_plugin_capture_from_args(args: &[String]) -> Result<(), String> {
    let capture_path = std::env::var_os(SSM_CAPTURE_FILE_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{SSM_CAPTURE_FILE_ENV} is not set"))?;

    let mut parsed_creds: Option<SsmSessionCredentials> = None;
    for arg in args.iter().skip(1) {
        if arg == SSM_PLUGIN_CAPTURE_ARG {
            continue;
        }
        if let Ok(creds) = serde_json::from_str::<SsmSessionCredentials>(arg) {
            if !creds.session_id.is_empty()
                && !creds.stream_url.is_empty()
                && !creds.token_value.is_empty()
            {
                parsed_creds = Some(creds);
                break;
            }
        }
    }
    let creds = parsed_creds.ok_or_else(|| {
        "session-manager-plugin capture shim did not receive StartSession/ResumeSession JSON"
            .to_string()
    })?;

    let encoded = serde_json::to_vec(&creds)
        .map_err(|e| format!("failed to serialize captured SSM credentials: {e}"))?;
    let temp = capture_path.with_extension("cap.tmp");
    write_file_atomic(&temp, &capture_path, &encoded, false)?;
    Ok(())
}

pub(crate) fn resolve_aws_bin() -> String {
    if let Ok(raw) = std::env::var("SGIAN_AWS_BIN") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "aws".to_string()
}

fn ensure_ssm_capture_shim_dir(runtime_dir: &Path) -> Result<PathBuf, String> {
    let shim_dir = runtime_dir.join("shim");
    ensure_private_dir(&shim_dir)?;
    let exe = std::env::current_exe()
        .map_err(|e| format!("failed to resolve current executable for SSM shim: {e}"))?;
    let is_libtest_runner = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with("sgian_lib-"));

    #[cfg(unix)]
    {
        let shim_path = shim_dir.join("session-manager-plugin");
        if shim_path.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&shim_path);
        }
        if is_libtest_runner {
            use std::os::unix::fs::PermissionsExt;
            let script = format!(
                "#!/bin/sh\n\
                 [ -n \"${SSM_CAPTURE_FILE_ENV}\" ] || exit 1\n\
                 umask 077\n\
                 printf '%s' \"$1\" > \"${{{SSM_CAPTURE_FILE_ENV}}}.tmp\" && \\\n\
                 mv \"${{{SSM_CAPTURE_FILE_ENV}}}.tmp\" \"${{{SSM_CAPTURE_FILE_ENV}}}\"\n"
            );
            fs::write(&shim_path, script).map_err(|e| {
                format!("failed to write test SSM shim {}: {e}", shim_path.display())
            })?;
            fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o700)).map_err(|e| {
                format!("failed to chmod test SSM shim {}: {e}", shim_path.display())
            })?;
        } else {
            std::os::unix::fs::symlink(&exe, &shim_path).map_err(|e| {
                format!(
                    "failed to create SSM shim symlink {}: {e}",
                    shim_path.display()
                )
            })?;
        }
    }

    #[cfg(windows)]
    {
        let _ = is_libtest_runner;
        let shim_path = shim_dir.join("session-manager-plugin.cmd");
        let content = format!(
            "@echo off\r\n\"{}\" {} %*\r\n",
            exe.display(),
            SSM_PLUGIN_CAPTURE_ARG
        );
        write_file_atomic(
            &shim_path.with_extension("cmd.tmp"),
            &shim_path,
            content.as_bytes(),
            false,
        )?;
    }

    Ok(shim_dir)
}

pub(crate) fn run_aws_ssm_session_command(
    aws_bin: &str,
    aws_args: &[String],
    runtime_dir: &Path,
) -> Result<SsmSessionCredentials, String> {
    let shim_dir = ensure_ssm_capture_shim_dir(runtime_dir)?;
    let nonce = SsmUuid::new_v4()?;
    let capture_path = runtime_dir.join(format!("ssm-capture-{nonce}.json"));

    let path_sep = if cfg!(windows) { ";" } else { ":" };
    let current_path = std::env::var("PATH").unwrap_or_default();
    let combined_path = if current_path.is_empty() {
        shim_dir.display().to_string()
    } else {
        format!("{}{path_sep}{current_path}", shim_dir.display())
    };

    let mut cmd = Command::new(aws_bin);
    cmd.args(aws_args)
        .env("PATH", combined_path)
        .env(SSM_CAPTURE_FILE_ENV, &capture_path)
        .env("AWS_PAGER", "")
        .stdin(Stdio::null());
    configure_daemon_process(&mut cmd);

    let output = run_subprocess_bounded(&mut cmd, Duration::from_secs(20)).map_err(|e| {
        let _ = fs::remove_file(&capture_path);
        format!("failed to execute '{aws_bin} {}': {e}", aws_args.join(" "))
    })?;

    // 1. Check capture file populated by `session-manager-plugin` shim.
    if let Ok(bytes) = fs::read(&capture_path) {
        let _ = fs::remove_file(&capture_path);
        if let Ok(creds) = serde_json::from_slice::<SsmSessionCredentials>(&bytes) {
            if !creds.session_id.is_empty()
                && !creds.stream_url.is_empty()
                && !creds.token_value.is_empty()
            {
                return Ok(creds);
            }
        }
    }
    let _ = fs::remove_file(&capture_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        };
        return Err(format!("aws ssm failed: {detail}"));
    }

    // 2. Fallback: parse `{SessionId, StreamUrl, TokenValue}` directly from stdout
    // (supports direct API output / `SGIAN_AWS_BIN` test scripts).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let cleaned = strip_terminal_controls(&stdout);
    let json_slice = extract_first_json_object(&cleaned).ok_or_else(|| {
        "aws ssm did not yield SessionId/StreamUrl/TokenValue via plugin shim or stdout".to_string()
    })?;
    let creds: SsmSessionCredentials = serde_json::from_str(json_slice)
        .map_err(|e| format!("failed to parse SSM session credentials JSON: {e}"))?;
    if creds.session_id.is_empty() || creds.stream_url.is_empty() || creds.token_value.is_empty() {
        return Err(
            "SSM session credentials contained empty SessionId/StreamUrl/TokenValue".to_string(),
        );
    }
    Ok(creds)
}

fn terminate_ssm_session_best_effort(aws_bin: &str, session_id: &str) {
    if session_id.trim().is_empty() {
        return;
    }
    let mut cmd = Command::new(aws_bin);
    cmd.args([
        "ssm",
        "terminate-session",
        "--session-id",
        session_id,
        "--output",
        "json",
    ])
    .env("AWS_PAGER", "")
    .stdin(Stdio::null());
    configure_daemon_process(&mut cmd);
    let _ = run_subprocess_bounded(&mut cmd, Duration::from_secs(5));
}

#[derive(Deserialize)]
struct AwsCliKmsDataKeyOutput {
    #[serde(rename = "Plaintext")]
    plaintext: String,
    #[serde(rename = "CiphertextBlob")]
    ciphertext_blob: String,
}

pub(crate) fn infer_kms_region(kms_key_id: &str, stream_url: &str) -> Option<String> {
    let valid_region = |candidate: &str| -> bool {
        !candidate.is_empty()
            && candidate.len() <= 64
            && !candidate.starts_with('-')
            && candidate
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };

    if kms_key_id.starts_with("arn:") {
        let parts: Vec<&str> = kms_key_id.splitn(6, ':').collect();
        if parts.len() >= 5 && parts[2] == "kms" && valid_region(parts[3]) {
            return Some(parts[3].to_string());
        }
    }

    if let Ok(parsed) = parse_ws_url(stream_url) {
        let host_lower = parsed.host.to_ascii_lowercase();
        if let Some(after) = host_lower.strip_prefix("ssmmessages.") {
            if let Some((region, tail)) = after.split_once('.') {
                if (tail.starts_with("amazonaws.")
                    || tail.ends_with(".ic.gov")
                    || tail.ends_with(".scloud"))
                    && valid_region(region)
                {
                    return Some(region.to_string());
                }
            }
        }
    }
    None
}

fn validate_kms_context_value(label: &str, raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 512 {
        return Err(format!(
            "invalid SSM KMS encryption context {label}: must be 1..=512 characters"
        ));
    }
    if trimmed.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err(format!(
            "invalid SSM KMS encryption context {label}: contains control characters"
        ));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn run_aws_kms_generate_data_key(
    aws_bin: &str,
    kms_key_id: &str,
    session_id: &str,
    target_id: &str,
    stream_url: &str,
    random_challenge: Option<&str>,
    include_random_challenge: bool,
) -> Result<KmsDataKeyMaterial, String> {
    let valid_key_id = validate_kms_key_id(kms_key_id)?;
    let valid_session = validate_kms_context_value("SessionId", session_id)?;
    let valid_target = validate_kms_context_value("TargetId", target_id)?;

    let mut enc_ctx = serde_json::Map::new();
    enc_ctx.insert(
        "aws:ssm:SessionId".to_string(),
        Value::String(valid_session),
    );
    enc_ctx.insert("aws:ssm:TargetId".to_string(), Value::String(valid_target));
    let mut challenge_acknowledged = false;
    if include_random_challenge {
        if let Some(ch) = random_challenge.map(str::trim).filter(|s| !s.is_empty()) {
            let valid_challenge = validate_kms_context_value("RandomChallenge", ch)?;
            enc_ctx.insert(
                "aws:ssm:RandomChallenge".to_string(),
                Value::String(valid_challenge),
            );
            challenge_acknowledged = true;
        }
    }
    let enc_ctx_json = Value::Object(enc_ctx).to_string();

    let mut aws_args = vec![
        "kms".to_string(),
        "generate-data-key".to_string(),
        "--key-id".to_string(),
        valid_key_id.clone(),
        "--number-of-bytes".to_string(),
        SSM_KMS_DATA_KEY_LEN.to_string(),
        "--encryption-context".to_string(),
        enc_ctx_json,
        "--output".to_string(),
        "json".to_string(),
    ];
    if let Some(region) = infer_kms_region(&valid_key_id, stream_url) {
        aws_args.push("--region".to_string());
        aws_args.push(region);
    }

    let mut cmd = Command::new(aws_bin);
    cmd.args(&aws_args)
        .env("AWS_PAGER", "")
        .stdin(Stdio::null());
    configure_daemon_process(&mut cmd);

    let output = run_subprocess_bounded(&mut cmd, Duration::from_secs(15))
        .map_err(|e| format!("failed to execute '{aws_bin} kms generate-data-key': {e}"))?;
    let stdout_guard = ZeroizingBytes::new(output.stdout);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stderr
        };
        return Err(format!("aws kms generate-data-key failed: {detail}"));
    }

    let mut parsed: AwsCliKmsDataKeyOutput = serde_json::from_slice(stdout_guard.as_slice())
        .map_err(|_| "failed to parse aws kms generate-data-key JSON output".to_string())?;
    let plain_res = base64_decode(&parsed.plaintext);
    let cipher_res = base64_decode(&parsed.ciphertext_blob);
    // Scrub base64 plaintext string before propagating any decode error.
    let mut plain_str_bytes = std::mem::take(&mut parsed.plaintext).into_bytes();
    zeroize_slice(&mut plain_str_bytes);

    let plaintext_vec = ZeroizingBytes::new(
        plain_res
            .map_err(|_| "invalid base64 Plaintext in KMS GenerateDataKey output".to_string())?,
    );
    if plaintext_vec.as_slice().len() != SSM_KMS_DATA_KEY_LEN {
        return Err(format!(
            "KMS GenerateDataKey returned {}-byte Plaintext (expected {SSM_KMS_DATA_KEY_LEN})",
            plaintext_vec.as_slice().len()
        ));
    }
    let ciphertext_blob = cipher_res
        .map_err(|_| "invalid base64 CiphertextBlob in KMS GenerateDataKey output".to_string())?;
    if ciphertext_blob.is_empty() || ciphertext_blob.len() > 64 * 1024 {
        return Err("KMS GenerateDataKey returned invalid CiphertextBlob length".to_string());
    }

    Ok(KmsDataKeyMaterial {
        ciphertext_blob,
        plaintext_key: plaintext_vec,
        challenge_acknowledged,
    })
}

fn kms_challenge_context_enabled() -> bool {
    std::env::var(SSM_KMS_CHALLENGE_CONTEXT_ENV)
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE"))
}

fn remote_ssm_discovery_command(workspace: &str) -> String {
    let escaped = workspace.replace('"', "\\\"");
    format!("sgian ctl --workspace \"{escaped}\" --json ipc-endpoint")
}

/// Discover the remote workspace's `loopback_port` via a short-lived
/// `AWS-StartInteractiveCommand` session (`sgian ctl --json ipc-endpoint`).
pub(crate) fn discover_ssm_remote_loopback_port(
    entry: &MachineEntry,
    runtime_dir: &Path,
    aws_bin: &str,
) -> Result<u16, String> {
    let target = entry
        .target
        .as_deref()
        .ok_or_else(|| format!("SSM machine '{}' has no target", entry.name))?;
    let remote_cmd = remote_ssm_discovery_command(&entry.workspace);
    let params = json!({ "command": [remote_cmd] }).to_string();
    let aws_args = vec![
        "ssm".to_string(),
        "start-session".to_string(),
        "--target".to_string(),
        target.to_string(),
        "--document-name".to_string(),
        "AWS-StartInteractiveCommand".to_string(),
        "--parameters".to_string(),
        params,
    ];
    let creds = run_aws_ssm_session_command(aws_bin, &aws_args, runtime_dir)?;
    let client_id = SsmUuid::new_v4()?.to_string();
    let result = collect_ssm_discovery_output(&creds, &client_id, aws_bin, target);
    terminate_ssm_session_best_effort(aws_bin, &creds.session_id);
    let raw_stdout = result?;
    let cleaned = strip_terminal_controls(&raw_stdout);
    let json_text = extract_first_json_object(&cleaned).ok_or_else(|| {
        format!(
            "remote `sgian ctl ipc-endpoint` on SSM target '{target}' did not return JSON \
             (is `sgian` installed and on PATH on the remote instance?)"
        )
    })?;
    let endpoint: RemoteEndpointDiscovery = serde_json::from_str(json_text)
        .map_err(|e| format!("failed to parse remote ipc-endpoint JSON from '{target}': {e}"))?;
    if endpoint.protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch on SSM machine '{}': remote={}, local={PROTOCOL_VERSION}",
            entry.name, endpoint.protocol_version
        ));
    }
    endpoint.loopback_port.filter(|&p| p > 0).ok_or_else(|| {
        format!(
            "remote workspace '{}' on SSM target '{target}' does not advertise a `loopback_port`; \
             set `\"remote_loopback_port\": 0` in the remote workspace's config.json (or start the \
             remote daemon with `--listen-loopback 0`) so SSM port forwarding can carry binary SGN2 frames",
            entry.workspace
        )
    })
}

fn collect_ssm_discovery_output(
    creds: &SsmSessionCredentials,
    client_id: &str,
    aws_bin: &str,
    target_id: &str,
) -> Result<String, String> {
    let mut ws = WsStream::connect(&creds.stream_url, Duration::from_secs(10))?;
    let open = OpenDataChannelInput::new(&creds.token_value, client_id)?;
    let open_json =
        serde_json::to_string(&open).map_err(|e| format!("OpenDataChannelInput encode: {e}"))?;
    ws.send_text(&open_json)?;
    ws.set_poll_timeout(Some(Duration::from_millis(50)));

    let mut state = SsmStreamState::new();
    let mut kms_cipher: Option<SsmKmsCipher> = None;
    let mut collected = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);

    while Instant::now() < deadline {
        match ws.poll_message()? {
            WsPoll::Binary(frame_bytes) => {
                let now = now_millis();
                let msg = AgentMessage::decode(&frame_bytes)?;
                let outcome = state.ingest_inbound(msg, now)?;
                for dup_ack in state.drain_immediate_acks() {
                    ws.send_binary(&dup_ack.encode()?)?;
                }
                while let Some(deliv) = state.peek_next_deliverable() {
                    match deliv.payload_type {
                        payload_type::HANDSHAKE_REQUEST => {
                            let ack = state.commit_inbound_delivered(&deliv, now)?;
                            ws.send_binary(&ack.encode()?)?;
                            let use_ch = kms_challenge_context_enabled();
                            let (resp_bytes, negotiated) = negotiate_handshake_response(
                                &deliv.payload,
                                Some(|key_id: &str, ch: Option<&str>| {
                                    run_aws_kms_generate_data_key(
                                        aws_bin,
                                        key_id,
                                        &creds.session_id,
                                        target_id,
                                        &creds.stream_url,
                                        ch,
                                        use_ch,
                                    )
                                }),
                            )
                            .inspect_err(|_| ws.close())?;
                            kms_cipher = negotiated;
                            let out_msg = state.queue_outbound_data(
                                payload_type::HANDSHAKE_RESPONSE,
                                resp_bytes,
                                SSM_FLAG_DATA,
                                now,
                            )?;
                            ws.send_binary(&out_msg.encode()?)?;
                        }
                        payload_type::ENC_CHALLENGE_REQUEST => {
                            let ack = state.commit_inbound_delivered(&deliv, now)?;
                            ws.send_binary(&ack.encode()?)?;
                            let cipher = kms_cipher.as_mut().ok_or_else(|| {
                                ws.close();
                                "SSM EncChallengeRequest arrived without active KMSEncryption handshake"
                                    .to_string()
                            })?;
                            let resp_bytes = cipher
                                .handle_challenge_request(&deliv.payload)
                                .inspect_err(|_| ws.close())?;
                            let out_msg = state.queue_outbound_data(
                                payload_type::ENC_CHALLENGE_RESPONSE,
                                resp_bytes,
                                SSM_FLAG_DATA,
                                now,
                            )?;
                            ws.send_binary(&out_msg.encode()?)?;
                        }
                        payload_type::HANDSHAKE_COMPLETE => {
                            if let Some(ref c) = kms_cipher {
                                if !c.challenge_verified() {
                                    ws.close();
                                    return Err(
                                        "SSM HandshakeComplete arrived before EncChallengeRequest verification"
                                            .to_string(),
                                    );
                                }
                            }
                            let ack = state.commit_inbound_delivered(&deliv, now)?;
                            ws.send_binary(&ack.encode()?)?;
                        }
                        payload_type::OUTPUT => {
                            let plain = match kms_cipher.as_ref() {
                                Some(c) => c
                                    .decrypt_stream_payload(&deliv.payload)
                                    .inspect_err(|_| ws.close())?,
                                None => deliv.payload.clone(),
                            };
                            let ack = state.commit_inbound_delivered(&deliv, now)?;
                            ws.send_binary(&ack.encode()?)?;
                            collected.extend_from_slice(&plain);
                            if collected.len() > 256 * 1024 {
                                ws.close();
                                return Err("SSM ipc-endpoint discovery output exceeded 256 KiB"
                                    .to_string());
                            }
                            let text = String::from_utf8_lossy(&collected);
                            let cleaned = strip_terminal_controls(&text);
                            if extract_first_json_object(&cleaned).is_some() {
                                ws.close();
                                return Ok(text.into_owned());
                            }
                        }
                        payload_type::STD_ERR | payload_type::EXIT_CODE => {
                            if let Some(ref c) = kms_cipher {
                                c.decrypt_stream_payload(&deliv.payload)
                                    .inspect_err(|_| ws.close())?;
                            }
                            let ack = state.commit_inbound_delivered(&deliv, now)?;
                            ws.send_binary(&ack.encode()?)?;
                        }
                        _ => {
                            let ack = state.commit_inbound_delivered(&deliv, now)?;
                            ws.send_binary(&ack.encode()?)?;
                        }
                    }
                }
                if matches!(outcome, InboundOutcome::ChannelClosed { .. }) {
                    break;
                }
            }
            WsPoll::Text => {}
            WsPoll::WouldBlock => {
                let now = now_millis();
                for rexmit in state.poll_retransmits(now) {
                    ws.send_binary(&rexmit.encode()?)?;
                }
            }
            WsPoll::Closed => break,
        }
    }
    ws.close();
    Ok(String::from_utf8_lossy(&collected).into_owned())
}

// ---------------------------------------------------------------------------
// 9. `--ssm-tunnel` Background Port-Forwarding Process
// ---------------------------------------------------------------------------

enum LocalListener {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener),
    Tcp(TcpListener),
}

enum LocalConn {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    Tcp(TcpStream),
}

impl LocalConn {
    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.set_nonblocking(nonblocking),
            Self::Tcp(s) => s.set_nonblocking(nonblocking),
        }
    }

    fn shutdown_both(&self) {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
            Self::Tcp(s) => {
                let _ = s.shutdown(Shutdown::Both);
            }
        }
    }
}

impl Read for LocalConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.read(buf),
            Self::Tcp(s) => s.read(buf),
        }
    }
}

impl Write for LocalConn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.write(buf),
            Self::Tcp(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
            Self::Tcp(s) => s.flush(),
        }
    }
}

impl LocalListener {
    fn try_accept(&self) -> std::io::Result<Option<LocalConn>> {
        match self {
            #[cfg(unix)]
            Self::Unix(l) => match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nonblocking(true);
                    Ok(Some(LocalConn::Unix(s)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            },
            Self::Tcp(l) => match l.accept() {
                Ok((s, _)) => {
                    let _ = s.set_nodelay(true);
                    let _ = s.set_nonblocking(true);
                    Ok(Some(LocalConn::Tcp(s)))
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            },
        }
    }
}

struct ActiveSmuxStream {
    conn: LocalConn,
    pending_to_local: VecDeque<u8>,
    remote_fin: bool,
}

#[derive(Debug, Clone)]
struct SsmTunnelConfig {
    target: String,
    remote_port: u16,
    runtime_dir: PathBuf,
    local_sock: Option<PathBuf>,
    local_port: Option<u16>,
    aws_bin: String,
    token_file: Option<PathBuf>,
}

fn parse_ssm_tunnel_args(args: &[String]) -> Result<SsmTunnelConfig, String> {
    let mut target: Option<String> = None;
    let mut remote_port: Option<u16> = None;
    let mut runtime_dir: Option<PathBuf> = None;
    let mut local_sock: Option<PathBuf> = None;
    let mut local_port: Option<u16> = None;
    let mut aws_bin = resolve_aws_bin();
    let mut token_file: Option<PathBuf> = None;

    let mut idx = 1;
    while idx < args.len() {
        let flag = args[idx].as_str();
        let next = |i: usize, f: &str| -> Result<String, String> {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{f} requires a value"))
        };
        match flag {
            SSM_TUNNEL_ARG => idx += 1,
            "--target" => {
                target = Some(next(idx, "--target")?);
                idx += 2;
            }
            "--remote-port" => {
                let raw = next(idx, "--remote-port")?;
                remote_port = Some(
                    raw.parse::<u16>()
                        .ok()
                        .filter(|&p| p > 0)
                        .ok_or_else(|| format!("invalid --remote-port '{raw}'"))?,
                );
                idx += 2;
            }
            "--runtime-dir" => {
                runtime_dir = Some(PathBuf::from(next(idx, "--runtime-dir")?));
                idx += 2;
            }
            "--local-sock" => {
                local_sock = Some(PathBuf::from(next(idx, "--local-sock")?));
                idx += 2;
            }
            "--local-port" => {
                let raw = next(idx, "--local-port")?;
                local_port = Some(
                    raw.parse::<u16>()
                        .ok()
                        .filter(|&p| p > 0)
                        .ok_or_else(|| format!("invalid --local-port '{raw}'"))?,
                );
                idx += 2;
            }
            "--aws-bin" => {
                aws_bin = next(idx, "--aws-bin")?;
                idx += 2;
            }
            "--token-file" => {
                token_file = Some(PathBuf::from(next(idx, "--token-file")?));
                idx += 2;
            }
            other => {
                return Err(format!("unexpected --ssm-tunnel argument: {other}"));
            }
        }
    }

    let target = target.ok_or_else(|| "--ssm-tunnel requires --target".to_string())?;
    let remote_port =
        remote_port.ok_or_else(|| "--ssm-tunnel requires --remote-port".to_string())?;
    let runtime_dir =
        runtime_dir.ok_or_else(|| "--ssm-tunnel requires --runtime-dir".to_string())?;
    if local_sock.is_none() && local_port.is_none() {
        return Err("--ssm-tunnel requires --local-sock or --local-port".to_string());
    }

    Ok(SsmTunnelConfig {
        target,
        remote_port,
        runtime_dir,
        local_sock,
        local_port,
        aws_bin,
        token_file,
    })
}

fn start_port_forwarding_session(cfg: &SsmTunnelConfig) -> Result<SsmSessionCredentials, String> {
    let params = json!({
        "portNumber": [cfg.remote_port.to_string()],
        "localPortNumber": ["0"],
    })
    .to_string();
    let aws_args = vec![
        "ssm".to_string(),
        "start-session".to_string(),
        "--target".to_string(),
        cfg.target.clone(),
        "--document-name".to_string(),
        "AWS-StartPortForwardingSession".to_string(),
        "--parameters".to_string(),
        params,
    ];
    run_aws_ssm_session_command(&cfg.aws_bin, &aws_args, &cfg.runtime_dir)
}

fn resume_port_forwarding_session(
    cfg: &SsmTunnelConfig,
    session_id: &str,
) -> Result<SsmSessionCredentials, String> {
    let aws_args = vec![
        "ssm".to_string(),
        "resume-session".to_string(),
        "--session-id".to_string(),
        session_id.to_string(),
    ];
    run_aws_ssm_session_command(&cfg.aws_bin, &aws_args, &cfg.runtime_dir)
}

fn open_mgs_data_channel(
    creds: &SsmSessionCredentials,
    client_id: &str,
) -> Result<WsStream, String> {
    let mut ws = WsStream::connect(&creds.stream_url, Duration::from_secs(10))?;
    let open = OpenDataChannelInput::new(&creds.token_value, client_id)?;
    let open_json =
        serde_json::to_string(&open).map_err(|e| format!("OpenDataChannelInput encode: {e}"))?;
    ws.send_text(&open_json)?;
    ws.set_poll_timeout(Some(Duration::from_millis(10)));
    Ok(ws)
}

fn enqueue_smux_frame(
    state: &mut SsmStreamState,
    kms_cipher: Option<&SsmKmsCipher>,
    ws: &mut WsStream,
    frame: &SmuxFrame,
    now_ms: u64,
) -> Result<(), String> {
    let raw_bytes = frame.encode()?;
    let payload = match kms_cipher {
        Some(cipher) => cipher.encrypt(&raw_bytes)?,
        None => raw_bytes,
    };
    let msg = state.queue_outbound_data(payload_type::OUTPUT, payload, SSM_FLAG_DATA, now_ms)?;
    ws.send_binary(&msg.encode()?)
}

/// Dispatch a durable `DaemonRequest::SessionEvent` (`session.started`,
/// `session.resumed`, `session.terminated`) on a short-lived helper thread
/// through our own local forwarded socket/port so the main `--ssm-tunnel`
/// event loop multiplexes it cleanly over `smux` v1 without blocking.
fn spawn_audit_session_event(
    cfg: &SsmTunnelConfig,
    event_name: &str,
    session_id: &str,
    reason: Option<&str>,
) {
    let token = client_token_from_env().or_else(|| {
        cfg.token_file
            .as_deref()
            .and_then(|p| read_token(p).ok().flatten())
    });
    let Some(tok) = token else {
        return;
    };
    let mut details = json!({
        "transport": "ssm",
        "target": cfg.target,
        "session_id": session_id,
    });
    if let Some(r) = reason {
        details["reason"] = Value::String(r.to_string());
    }
    let sock_path = cfg
        .local_sock
        .clone()
        .unwrap_or_else(|| cfg.runtime_dir.join(SOCKET_FILE));
    let tcp_addr = cfg
        .local_port
        .map(|port| std::net::SocketAddr::from(([127, 0, 0, 1], port)));
    let ev = event_name.to_string();
    thread::spawn(move || {
        if let Ok(mut conn) = DaemonConnection::connect_with_options(
            &sock_path,
            tcp_addr,
            "",
            Some(&tok),
            Some(Duration::from_secs(5)),
        ) {
            let req = DaemonRequest::SessionEvent {
                pane_id: None,
                event: ev,
                details,
            };
            if conn.write_request(&req).is_ok() {
                let _ = conn.read_response();
            }
        }
    });
}

pub(crate) fn run_ssm_tunnel_from_args(args: &[String]) -> Result<(), String> {
    let cfg = parse_ssm_tunnel_args(args)?;
    ensure_private_dir(&cfg.runtime_dir)?;

    let client_id = SsmUuid::new_v4()?.to_string();
    let mut creds = start_port_forwarding_session(&cfg)?;
    let mut ws = open_mgs_data_channel(&creds, &client_id)?;
    let mut state = SsmStreamState::new();
    let mut kms_cipher: Option<SsmKmsCipher> = None;
    let mut smux_decoder = SmuxDecoder::new();

    // Bind the local listener ONLY after the initial SSM session and MGS data
    // channel are open so `ensure_machine_client` observes socket readiness at
    // the exact moment port forwarding is live.
    let listener = if let Some(ref sock_path) = cfg.local_sock {
        #[cfg(unix)]
        {
            remove_stale_socket(sock_path)?;
            let l = std::os::unix::net::UnixListener::bind(sock_path).map_err(|e| {
                format!(
                    "failed to bind SSM local socket {}: {e}",
                    sock_path.display()
                )
            })?;
            let _ = set_private_file_permissions(sock_path);
            l.set_nonblocking(true).map_err(|e| {
                format!("failed to set nonblocking on {}: {e}", sock_path.display())
            })?;
            LocalListener::Unix(l)
        }
        #[cfg(not(unix))]
        {
            let _ = sock_path;
            return Err("--local-sock is only supported on Unix".to_string());
        }
    } else {
        let port = cfg.local_port.unwrap();
        let l = TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("failed to bind SSM local TCP port {port}: {e}"))?;
        l.set_nonblocking(true)
            .map_err(|e| format!("failed to set nonblocking on 127.0.0.1:{port}: {e}"))?;
        LocalListener::Tcp(l)
    };

    let mut streams: BTreeMap<u32, ActiveSmuxStream> = BTreeMap::new();
    let mut next_stream_id: u32 = 3;
    let mut pending_audit_events: VecDeque<(String, String, Option<String>)> = VecDeque::new();
    pending_audit_events.push_back((
        "session.started".to_string(),
        creds.session_id.clone(),
        None,
    ));
    let mut last_ws_ping = Instant::now();
    let stop_sentinel = cfg.runtime_dir.join("tunnel.stop");
    let _ = fs::remove_file(&stop_sentinel);
    let mut tick_counter: u32 = 0;

    loop {
        tick_counter = tick_counter.wrapping_add(1);
        if (tick_counter & 31) == 0 && stop_sentinel.exists() {
            let _ = fs::remove_file(&stop_sentinel);
            break;
        }
        let mut did_work = false;
        let now = now_millis();

        // 1. Flush queued transport ledger audit events once handshake is ready.
        if state.handshake_complete() && state.can_send_outbound() {
            while let Some((ev, sid_str, reason)) = pending_audit_events.pop_front() {
                spawn_audit_session_event(&cfg, &ev, &sid_str, reason.as_deref());
                did_work = true;
            }
        }

        // 2. Accept new local connections once the SSM Port handshake is complete.
        if state.handshake_complete() && state.can_send_outbound() {
            match listener.try_accept() {
                Ok(Some(conn)) => {
                    let _ = conn.set_nonblocking(true);
                    let sid = next_stream_id;
                    next_stream_id = next_stream_id.wrapping_add(2);
                    if enqueue_smux_frame(
                        &mut state,
                        kms_cipher.as_ref(),
                        &mut ws,
                        &SmuxFrame::syn(sid),
                        now,
                    )
                    .is_ok()
                    {
                        streams.insert(
                            sid,
                            ActiveSmuxStream {
                                conn,
                                pending_to_local: VecDeque::new(),
                                remote_fin: false,
                            },
                        );
                        did_work = true;
                    }
                }
                Ok(None) => {}
                Err(_) => break,
            }
        }

        // 3. Flush pending inbound bytes to local connections.
        let mut closed_sids = Vec::new();
        for (&sid, entry) in streams.iter_mut() {
            while !entry.pending_to_local.is_empty() {
                let (slice, _) = entry.pending_to_local.as_slices();
                match entry.conn.write(slice) {
                    Ok(0) => {
                        closed_sids.push((sid, false));
                        break;
                    }
                    Ok(n) => {
                        entry.pending_to_local.drain(..n);
                        did_work = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        closed_sids.push((sid, false));
                        break;
                    }
                }
            }
            if entry.pending_to_local.is_empty() && entry.remote_fin {
                closed_sids.push((sid, false));
            }
        }

        // 4. Read outbound bytes from local connections into smux PSH frames when
        // `SsmStreamState` has outbound window space (`can_send_outbound()`).
        let sids: Vec<u32> = streams.keys().copied().collect();
        for sid in sids {
            if !state.handshake_complete() || !state.can_send_outbound() {
                break;
            }
            if closed_sids.iter().any(|(c, _)| *c == sid) {
                continue;
            }
            let mut buf = [0_u8; 16 * 1024];
            let read_res = streams.get_mut(&sid).unwrap().conn.read(&mut buf);
            match read_res {
                Ok(0) => {
                    closed_sids.push((sid, true));
                    did_work = true;
                }
                Ok(n) => {
                    for chunk in SmuxFrame::psh_chunks(sid, &buf[..n]) {
                        let _ = enqueue_smux_frame(
                            &mut state,
                            kms_cipher.as_ref(),
                            &mut ws,
                            &chunk,
                            now,
                        );
                    }
                    did_work = true;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    closed_sids.push((sid, true));
                }
            }
        }

        for (sid, send_fin) in closed_sids {
            if let Some(removed) = streams.remove(&sid) {
                removed.conn.shutdown_both();
                if send_fin && state.handshake_complete() {
                    let _ = enqueue_smux_frame(
                        &mut state,
                        kms_cipher.as_ref(),
                        &mut ws,
                        &SmuxFrame::fin(sid),
                        now,
                    );
                }
            }
        }

        // 5. Deliver staged SSM inbound frames ONLY while local stream buffers are
        // within a healthy bound (deferred-ACK backpressure!).
        let local_backlogged = streams
            .values()
            .any(|s| s.pending_to_local.len() >= 512 * 1024);
        if !local_backlogged {
            while let Some(deliv) = state.peek_next_deliverable() {
                let mut fatal_err: Option<String> = None;
                match deliv.payload_type {
                    payload_type::HANDSHAKE_REQUEST => {
                        let use_ch = kms_challenge_context_enabled();
                        let session_id = creds.session_id.clone();
                        let stream_url = creds.stream_url.clone();
                        match negotiate_handshake_response(
                            &deliv.payload,
                            Some(|key_id: &str, ch: Option<&str>| {
                                run_aws_kms_generate_data_key(
                                    &cfg.aws_bin,
                                    key_id,
                                    &session_id,
                                    &cfg.target,
                                    &stream_url,
                                    ch,
                                    use_ch,
                                )
                            }),
                        ) {
                            Ok((resp_bytes, negotiated)) => {
                                kms_cipher = negotiated;
                                if let Ok(out_msg) = state.queue_outbound_data(
                                    payload_type::HANDSHAKE_RESPONSE,
                                    resp_bytes,
                                    SSM_FLAG_DATA,
                                    now,
                                ) {
                                    let _ = ws.send_binary(&out_msg.encode()?);
                                }
                            }
                            Err(err) => fatal_err = Some(err),
                        }
                    }
                    payload_type::ENC_CHALLENGE_REQUEST => match kms_cipher.as_mut() {
                        Some(cipher) => match cipher.handle_challenge_request(&deliv.payload) {
                            Ok(resp_bytes) => {
                                if let Ok(out_msg) = state.queue_outbound_data(
                                    payload_type::ENC_CHALLENGE_RESPONSE,
                                    resp_bytes,
                                    SSM_FLAG_DATA,
                                    now,
                                ) {
                                    let _ = ws.send_binary(&out_msg.encode()?);
                                }
                            }
                            Err(err) => fatal_err = Some(err),
                        },
                        None => {
                            fatal_err = Some(
                                "SSM EncChallengeRequest arrived without active KMSEncryption handshake"
                                    .to_string(),
                            );
                        }
                    },
                    payload_type::HANDSHAKE_COMPLETE => {
                        if let Some(ref cipher) = kms_cipher {
                            if !cipher.challenge_verified() {
                                fatal_err = Some(
                                    "SSM HandshakeComplete arrived before EncChallengeRequest verification"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    payload_type::OUTPUT => {
                        let decrypted = match kms_cipher.as_ref() {
                            Some(cipher) => cipher.decrypt_stream_payload(&deliv.payload),
                            None => {
                                // Only unencrypted sessions may implicitly mark ready on first Output.
                                state.handshake_complete = true;
                                Ok(deliv.payload.clone())
                            }
                        };
                        match decrypted.and_then(|plain| smux_decoder.push(&plain)) {
                            Ok(smux_frames) => {
                                for sf in smux_frames {
                                    match sf.cmd {
                                        SMUX_CMD_PSH => {
                                            if let Some(st) = streams.get_mut(&sf.stream_id) {
                                                st.pending_to_local.extend(&sf.data);
                                            }
                                        }
                                        SMUX_CMD_FIN => {
                                            if let Some(st) = streams.get_mut(&sf.stream_id) {
                                                st.remote_fin = true;
                                            }
                                        }
                                        SMUX_CMD_NOP | SMUX_CMD_SYN => {}
                                        _ => {}
                                    }
                                }
                            }
                            Err(err) => fatal_err = Some(err),
                        }
                    }
                    payload_type::STD_ERR | payload_type::EXIT_CODE => {
                        if let Some(ref cipher) = kms_cipher {
                            if let Err(err) = cipher.decrypt_stream_payload(&deliv.payload) {
                                fatal_err = Some(err);
                            }
                        }
                    }
                    // Port forwarding Flag payload (e.g. ConnectToPortError = 2).
                    payload_type::FLAG if deliv.payload.len() == 4 => {
                        let flag_val = u32::from_be_bytes(deliv.payload[0..4].try_into().unwrap());
                        if flag_val == 2 {
                            // ConnectToPortError: close active streams cleanly.
                            for (_, st) in streams.split_off(&0) {
                                st.conn.shutdown_both();
                            }
                        }
                    }
                    _ => {}
                }

                if let Some(err) = fatal_err {
                    ws.close();
                    terminate_ssm_session_best_effort(&cfg.aws_bin, &creds.session_id);
                    return Err(err);
                }
                let ack = state.commit_inbound_delivered(&deliv, now)?;
                let _ = ws.send_binary(&ack.encode()?);
                did_work = true;
            }
        }

        // 6. Poll MGS WebSocket for inbound frames, handling unexpected drops via
        // `ResumeSession` and `channel_closed` via fresh `StartSession`!
        let mut ws_dropped = false;
        let mut channel_closed_reason: Option<String> = None;

        match ws.poll_message() {
            Ok(WsPoll::Binary(bytes)) => {
                did_work = true;
                if let Ok(msg) = AgentMessage::decode(&bytes) {
                    match state.ingest_inbound(msg, now) {
                        Ok(InboundOutcome::ChannelClosed { reason }) => {
                            channel_closed_reason = Some(reason);
                        }
                        Ok(_) => {}
                        Err(err) => {
                            ws.close();
                            terminate_ssm_session_best_effort(&cfg.aws_bin, &creds.session_id);
                            return Err(err);
                        }
                    }
                    for dup_ack in state.drain_immediate_acks() {
                        let _ = ws.send_binary(&dup_ack.encode()?);
                    }
                }
            }
            Ok(WsPoll::Text) => {
                did_work = true;
            }
            Ok(WsPoll::WouldBlock) => {}
            Ok(WsPoll::Closed) | Err(_) => {
                ws_dropped = true;
            }
        }

        if let Some(reason) = channel_closed_reason {
            ws.close();
            for (_, st) in std::mem::take(&mut streams) {
                st.conn.shutdown_both();
            }
            let terminated_sid = creds.session_id.clone();
            let fresh_creds = start_port_forwarding_session(&cfg)?;
            let fresh_ws = open_mgs_data_channel(&fresh_creds, &client_id)?;
            creds = fresh_creds;
            ws = fresh_ws;
            state = SsmStreamState::new();
            kms_cipher = None;
            smux_decoder = SmuxDecoder::new();
            pending_audit_events.push_back((
                "session.terminated".to_string(),
                terminated_sid,
                Some(reason),
            ));
            pending_audit_events.push_back((
                "session.started".to_string(),
                creds.session_id.clone(),
                None,
            ));
            continue;
        }

        if ws_dropped {
            ws.close();
            // First attempt `aws ssm resume-session` so open smux streams and
            // unacknowledged frames survive transient WebSocket drops seamlessly!
            let resumed =
                resume_port_forwarding_session(&cfg, &creds.session_id).and_then(|resumed_creds| {
                    let resumed_ws = open_mgs_data_channel(&resumed_creds, &client_id)?;
                    Ok((resumed_creds, resumed_ws))
                });
            match resumed {
                Ok((resumed_creds, mut resumed_ws)) => {
                    creds = resumed_creds;
                    let resume_now = now_millis();
                    state.prepare_for_resume(resume_now);
                    for replay in state.poll_retransmits(resume_now) {
                        let _ = resumed_ws.send_binary(&replay.encode()?);
                    }
                    ws = resumed_ws;
                    pending_audit_events.push_back((
                        "session.resumed".to_string(),
                        creds.session_id.clone(),
                        None,
                    ));
                    last_ws_ping = Instant::now();
                    continue;
                }
                Err(_) => {
                    // Resume failed (e.g., session timed out on MGS): reset streams
                    // and fall back to a fresh `StartSession`.
                    for (_, st) in std::mem::take(&mut streams) {
                        st.conn.shutdown_both();
                    }
                    let old_sid = creds.session_id.clone();
                    let fresh_creds = start_port_forwarding_session(&cfg)?;
                    let fresh_ws = open_mgs_data_channel(&fresh_creds, &client_id)?;
                    creds = fresh_creds;
                    ws = fresh_ws;
                    state = SsmStreamState::new();
                    kms_cipher = None;
                    smux_decoder = SmuxDecoder::new();
                    pending_audit_events.push_back((
                        "session.terminated".to_string(),
                        old_sid,
                        Some("websocket_disconnected_resume_failed".to_string()),
                    ));
                    pending_audit_events.push_back((
                        "session.started".to_string(),
                        creds.session_id.clone(),
                        None,
                    ));
                    last_ws_ping = Instant::now();
                    continue;
                }
            }
        }

        // 7. EWMA retransmit timer & WebSocket keepalive Ping.
        for rexmit in state.poll_retransmits(now) {
            let _ = ws.send_binary(&rexmit.encode()?);
            did_work = true;
        }
        if last_ws_ping.elapsed() >= Duration::from_secs(30) {
            let _ = ws.send_ping(b"sgian-keepalive");
            last_ws_ping = Instant::now();
        }

        if !did_work {
            thread::sleep(Duration::from_millis(2));
        }
    }

    ws.close();
    terminate_ssm_session_best_effort(&cfg.aws_bin, &creds.session_id);
    Ok(())
}
