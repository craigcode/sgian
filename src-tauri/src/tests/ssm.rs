use super::*;
use std::collections::{BTreeMap, VecDeque};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::AtomicI64;

#[test]
fn agent_message_golden_wire_vector_and_half_swapped_uuid() {
    use sha2::Digest;

    let canonical_uuid_bytes: [u8; 16] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, // MSB_8
        0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, // LSB_8
    ];
    let uuid = SsmUuid::from_canonical_bytes(canonical_uuid_bytes);
    assert_eq!(uuid.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
    assert_eq!(
        SsmUuid::parse_hyphenated("00112233-4455-6677-8899-aabbccddeeff").unwrap(),
        uuid
    );
    assert_eq!(uuid.as_canonical_bytes(), &canonical_uuid_bytes);

    let payload = b"hello-ssm-agent".to_vec();
    let msg = AgentMessage::new(
        MSG_INPUT_STREAM_DATA,
        42,
        SSM_FLAG_SYN,
        uuid,
        1_700_000_000_123,
        payload_type::OUTPUT,
        payload.clone(),
    )
    .expect("valid AgentMessage");

    let wire = msg.encode().expect("encode AgentMessage");
    assert_eq!(wire.len(), 120 + payload.len());

    // 0..4: HeaderLength == 116 (0x00000074)
    assert_eq!(&wire[0..4], &[0, 0, 0, 116]);
    // 4..36: space-padded 32-byte ASCII MessageType
    assert_eq!(&wire[4..36], b"input_stream_data               ");
    // 36..40: SchemaVersion == 1
    assert_eq!(&wire[36..40], &[0, 0, 0, 1]);
    // 40..48: CreatedDate BE
    assert_eq!(&wire[40..48], &1_700_000_000_123_u64.to_be_bytes());
    // 48..56: SequenceNumber BE
    assert_eq!(&wire[48..56], &42_i64.to_be_bytes());
    // 56..64: Flags BE
    assert_eq!(&wire[56..64], &SSM_FLAG_SYN.to_be_bytes());
    // 64..80: AWS SSM half-swapped UUID: [LSB_8 (bytes 8..16) | MSB_8 (bytes 0..8)]!
    assert_eq!(
        &wire[64..72],
        &[0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]
    );
    assert_eq!(
        &wire[72..80],
        &[0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77]
    );
    // 80..112: SHA-256 PayloadDigest
    let expected_sha = sha2::Sha256::digest(&payload);
    assert_eq!(&wire[80..112], expected_sha.as_slice());
    // 112..116: PayloadType == 1
    assert_eq!(&wire[112..116], &1_u32.to_be_bytes());
    // 116..120: PayloadLength
    assert_eq!(&wire[116..120], &(payload.len() as u32).to_be_bytes());
    // 120..: Payload
    assert_eq!(&wire[120..], payload.as_slice());

    let decoded = AgentMessage::decode(&wire).expect("decode golden frame");
    assert_eq!(decoded, msg);

    // Acknowledge frame preserves the canonical hyphenated UUID string.
    let ack_frame = build_ack_frame(MSG_OUTPUT_STREAM_DATA, decoded.message_id, 42, 999).unwrap();
    let ack_json: AcknowledgeContent = serde_json::from_slice(&ack_frame.payload).unwrap();
    assert_eq!(
        ack_json.acknowledged_message_id,
        "00112233-4455-6677-8899-aabbccddeeff"
    );
    assert_eq!(ack_json.acknowledged_message_sequence_number, 42);
    assert!(ack_json.is_sequential_message);

    // Tampered payload byte fails SHA-256 verification.
    let mut bad_digest = wire.clone();
    *bad_digest.last_mut().unwrap() ^= 0x01;
    assert!(AgentMessage::decode(&bad_digest)
        .unwrap_err()
        .contains("SHA-256"));

    // Oversized PayloadLength (0xFFFF_FFFF) fails pre-allocation check cleanly.
    let mut huge_len = wire.clone();
    huge_len[116..120].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(AgentMessage::decode(&huge_len)
        .unwrap_err()
        .contains("exceeds 10 MiB"));

    // Extended future header length (e.g. 128 bytes with 12 extension bytes) decodes cleanly.
    let mut ext_wire = Vec::new();
    ext_wire.extend_from_slice(&128_u32.to_be_bytes());
    ext_wire.extend_from_slice(&wire[4..116]);
    ext_wire.extend_from_slice(&[0_u8; 12]); // extension header bytes 116..128
    ext_wire.extend_from_slice(&wire[116..]);
    let ext_decoded = AgentMessage::decode(&ext_wire).expect("extended 128B header");
    assert_eq!(ext_decoded, msg);
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn fill_bytes(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let r = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
    }
}

#[test]
fn seeded_prng_fuzz_10000_frames_never_panics_or_overallocates() {
    let mut rng = SplitMix64(0x5347_4941_4E5F_5353);
    let template_uuid = SsmUuid::from_canonical_bytes([0xAB; 16]);
    let valid_template = AgentMessage::new(
        MSG_OUTPUT_STREAM_DATA,
        7,
        SSM_FLAG_DATA,
        template_uuid,
        123_456,
        payload_type::OUTPUT,
        b"fuzz-seed-payload".to_vec(),
    )
    .unwrap()
    .encode()
    .unwrap();

    let mut smux_dec = SmuxDecoder::new();
    let mut fuzz_kms_cipher = SsmKmsCipher::from_client_data_key(&[0x5A_u8; 64]).unwrap();

    for iter in 0..10_000_u32 {
        let len = (rng.next_u64() as usize) % 320;
        let mut buf = vec![0_u8; len];
        rng.fill_bytes(&mut buf);

        // Mix completely random buffers with mutated copies of a valid AgentMessage
        // (including adversarial HeaderLength and PayloadLength injections).
        if iter % 3 == 0 && len >= 8 {
            let mut mutated = valid_template.clone();
            let flips = 1 + ((rng.next_u64() as usize) % 6);
            for _ in 0..flips {
                let idx = (rng.next_u64() as usize) % mutated.len();
                mutated[idx] ^= (rng.next_u64() & 0xFF) as u8;
            }
            if iter % 9 == 0 {
                // Inject adversarial u32::MAX payload length at offset 116.
                mutated[116..120].copy_from_slice(&u32::MAX.to_be_bytes());
            }
            let _ = AgentMessage::decode(&mutated);
        }

        let _ = AgentMessage::decode(&buf);
        let _ = smux_dec.push(&buf);
        let _ = decode_ws_frame_slice(&buf);
        let _ = fuzz_kms_cipher.decrypt(&buf);
        let _ = fuzz_kms_cipher.decrypt_stream_payload(&buf);
        let _ = fuzz_kms_cipher.handle_challenge_request(&buf);
        if let Ok(as_str) = std::str::from_utf8(&buf) {
            let _ = base64_decode(as_str);
        }
    }
}

#[test]
fn ssm_stream_state_deferred_ack_backpressure_out_of_order_and_resume_replay() {
    let mut state = SsmStreamState::new();
    assert!(state.can_send_outbound());
    assert_eq!(state.rto_ms(), 1_000);

    // 1. Outbound sequence + RTT update + pause/resume publication.
    let out0 = state
        .queue_outbound_data(payload_type::OUTPUT, b"ping".to_vec(), SSM_FLAG_DATA, 1_000)
        .unwrap();
    let out1 = state
        .queue_outbound_data(payload_type::OUTPUT, b"pong".to_vec(), SSM_FLAG_DATA, 1_010)
        .unwrap();
    assert_eq!(out0.sequence_number, 0);
    assert_eq!(out1.sequence_number, 1);
    assert_eq!(state.unacked_count(), 2);

    // Pause publication blocks new outbound sends and retransmits.
    let pause_msg = AgentMessage::new(
        MSG_PAUSE_PUBLICATION,
        0,
        0,
        SsmUuid::from_canonical_bytes([1; 16]),
        1_020,
        0,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        state.ingest_inbound(pause_msg, 1_020).unwrap(),
        InboundOutcome::PublicationPaused
    );
    assert!(!state.publication_allowed());
    assert!(!state.can_send_outbound());
    assert!(state.poll_retransmits(5_000).is_empty());

    let resume_pub = AgentMessage::new(
        MSG_START_PUBLICATION,
        0,
        0,
        SsmUuid::from_canonical_bytes([2; 16]),
        1_030,
        0,
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        state.ingest_inbound(resume_pub, 1_030).unwrap(),
        InboundOutcome::PublicationResumed
    );
    assert!(state.can_send_outbound());

    // ACK out0 at t=1_080 (80ms RTT sample -> RTO clamps to >=200ms).
    let ack0 = build_ack_frame(MSG_INPUT_STREAM_DATA, out0.message_id, 0, 1_080).unwrap();
    assert_eq!(
        state.ingest_inbound(ack0, 1_080).unwrap(),
        InboundOutcome::AckProcessed { seq: 0 }
    );
    assert_eq!(state.unacked_count(), 1);
    assert!(state.rto_ms() >= 200 && state.rto_ms() < 1_000);

    // Retransmit out1 after RTO expires, then prepare_for_resume replays immediately.
    let rexmits = state.poll_retransmits(1_010 + state.rto_ms() + 5);
    assert_eq!(rexmits.len(), 1);
    assert_eq!(rexmits[0].sequence_number, 1);
    assert!(state
        .poll_retransmits(1_010 + state.rto_ms() + 10)
        .is_empty());

    state.prepare_for_resume(3_000);
    let resume_replays = state.poll_retransmits(3_000);
    assert_eq!(resume_replays.len(), 1);
    assert_eq!(resume_replays[0].sequence_number, 1);

    // 2. Out-of-order inbound frames + DEFERRED-ACK backpressure!
    let in2 = AgentMessage::new(
        MSG_OUTPUT_STREAM_DATA,
        2,
        0,
        SsmUuid::from_canonical_bytes([0x22; 16]),
        2_000,
        payload_type::OUTPUT,
        b"third".to_vec(),
    )
    .unwrap();
    let in1 = AgentMessage::new(
        MSG_OUTPUT_STREAM_DATA,
        1,
        0,
        SsmUuid::from_canonical_bytes([0x11; 16]),
        2_000,
        payload_type::OUTPUT,
        b"second".to_vec(),
    )
    .unwrap();
    let in0 = AgentMessage::new(
        MSG_OUTPUT_STREAM_DATA,
        0,
        0,
        SsmUuid::from_canonical_bytes([0x00; 16]),
        2_000,
        payload_type::OUTPUT,
        b"first".to_vec(),
    )
    .unwrap();

    state.ingest_inbound(in2, 2_000).unwrap();
    state.ingest_inbound(in1, 2_001).unwrap();
    // Gap at seq 0: nothing deliverable yet and ZERO ACKs emitted!
    assert!(state.peek_next_deliverable().is_none());
    assert!(state.drain_immediate_acks().is_empty());

    state.ingest_inbound(in0.clone(), 2_002).unwrap();
    // Even after seq 0 arrives, expected_incoming_seq stays 0 (no ACK sent!)
    // until the local consumer commits delivery!
    assert_eq!(state.expected_incoming_seq(), 0);
    assert!(state.drain_immediate_acks().is_empty());

    let mut delivered_payloads = Vec::new();
    let mut acked_seqs = Vec::new();
    while let Some(d) = state.peek_next_deliverable() {
        delivered_payloads.push(d.payload.clone());
        let ack = state.commit_inbound_delivered(&d, 2_010).unwrap();
        let content: AcknowledgeContent = serde_json::from_slice(&ack.payload).unwrap();
        acked_seqs.push(content.acknowledged_message_sequence_number);
    }
    assert_eq!(
        delivered_payloads,
        vec![b"first".to_vec(), b"second".to_vec(), b"third".to_vec()]
    );
    assert_eq!(acked_seqs, vec![0, 1, 2]);
    assert_eq!(state.expected_incoming_seq(), 3);

    // Duplicate seq 0 triggers an immediate re-ACK without re-delivering!
    state.ingest_inbound(in0, 2_050).unwrap();
    assert!(state.peek_next_deliverable().is_none());
    let dup_acks = state.drain_immediate_acks();
    assert_eq!(dup_acks.len(), 1);
    let dup_content: AcknowledgeContent = serde_json::from_slice(&dup_acks[0].payload).unwrap();
    assert_eq!(dup_content.acknowledged_message_sequence_number, 0);

    // 3. Handshake fails closed on KMSEncryption and succeeds on Port.
    let kms_hs = serde_json::to_vec(&json!({
        "AgentVersion": "3.2.0.0",
        "RequestedClientActions": [
            { "ActionType": "KMSEncryption", "ActionParameters": { "KMSKeyId": "alias/test" } }
        ]
    }))
    .unwrap();
    assert!(build_handshake_response(&kms_hs)
        .unwrap_err()
        .contains("KMS"));

    let port_hs = serde_json::to_vec(&json!({
        "AgentVersion": "3.2.0.0",
        "RequestedClientActions": [
            { "ActionType": "SessionType", "ActionParameters": { "SessionType": "Port" } }
        ]
    }))
    .unwrap();
    let port_resp: HandshakeResponsePayload =
        serde_json::from_slice(&build_handshake_response(&port_hs).unwrap()).unwrap();
    assert_eq!(port_resp.processed_client_actions[0].action_status, 1);
}

#[test]
fn client_lease_tracker_models_requested_and_releasing_round_trips() {
    let mut tracker = ClientLeaseTracker::new();
    assert_eq!(*tracker.phase(), ClientLeasePhase::Unheld);
    assert!(tracker
        .can_send_local_input(LeasePolicy::Required, "op@laptop")
        .is_err());
    assert!(tracker
        .can_send_local_input(LeasePolicy::Open, "op@laptop")
        .is_ok());

    tracker.begin_take("op@laptop", 100).unwrap();
    assert!(matches!(
        tracker.phase(),
        ClientLeasePhase::Requested { holder, .. } if holder == "op@laptop"
    ));
    // While `Requested`, premature local keystrokes and release attempts are gated!
    assert!(tracker
        .can_send_local_input(LeasePolicy::Open, "op@laptop")
        .unwrap_err()
        .contains("still in flight"));
    assert!(tracker.begin_release(105).is_err());

    tracker.fail_take();
    assert_eq!(*tracker.phase(), ClientLeasePhase::Unheld);

    tracker.begin_take("op@laptop", 110).unwrap();
    tracker.confirm_taken("op@laptop", 7).unwrap();
    assert_eq!(
        *tracker.phase(),
        ClientLeasePhase::Held {
            holder: "op@laptop".to_string(),
            generation: 7
        }
    );
    assert!(tracker
        .can_send_local_input(LeasePolicy::Required, "op@laptop")
        .is_ok());
    assert!(tracker
        .can_send_local_input(LeasePolicy::Required, "other@box")
        .is_err());

    tracker.begin_release(200).unwrap();
    assert!(matches!(
        tracker.phase(),
        ClientLeasePhase::Releasing {
            generation: Some(7),
            ..
        }
    ));
    assert!(tracker
        .can_send_local_input(LeasePolicy::Open, "op@laptop")
        .unwrap_err()
        .contains("release is still in flight"));

    tracker.fail_release();
    assert!(matches!(tracker.phase(), ClientLeasePhase::Held { .. }));

    tracker.begin_release(210).unwrap();
    tracker.confirm_released();
    assert_eq!(*tracker.phase(), ClientLeasePhase::Unheld);
}

#[test]
fn ssm_plugin_capture_invocation_and_rfc6455_accept_vector() {
    // Official RFC 6455 §1.3 Sec-WebSocket-Accept vector.
    assert_eq!(
        compute_websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );

    assert!(is_ssm_plugin_capture_invocation(&[
        "/tmp/shim/session-manager-plugin".into(),
        "{}".into()
    ]));
    assert!(is_ssm_plugin_capture_invocation(&[
        "sgian".into(),
        SSM_PLUGIN_CAPTURE_ARG.into(),
        "{}".into()
    ]));
    assert!(!is_ssm_plugin_capture_invocation(&[
        "sgian".into(),
        "ctl".into(),
        "panes".into()
    ]));
}

#[test]
fn ssm_kms_cipher_key_split_directionality_challenge_and_tamper_rejection() {
    // Distinct halves: [0..32] = agent->client (client decrypts), [32..64] = client->agent (client encrypts).
    let mut key64 = [0_u8; 64];
    for (i, slot) in key64[..32].iter_mut().enumerate() {
        *slot = (0x10 + i) as u8;
    }
    for (i, slot) in key64[32..].iter_mut().enumerate() {
        *slot = (0x90 + i) as u8;
    }

    assert!(SsmKmsCipher::from_client_data_key(&key64[..32]).is_err());
    assert!(SsmKmsCipher::from_client_data_key(&[0_u8; 63]).is_err());

    let mut client_cipher = SsmKmsCipher::from_client_data_key(&key64).expect("client KMS cipher");
    let agent_cipher = SsmKmsCipher::new_agent_side_for_test(&key64).expect("agent KMS cipher");
    assert!(!client_cipher.challenge_verified());

    // 1. Directional key-half isolation matching `encrypter.go` & `blockcipher.go`.
    let from_agent = agent_cipher.encrypt(b"hello-from-ssm-agent").unwrap();
    assert_eq!(
        from_agent.len(),
        SSM_AES_GCM_NONCE_LEN + b"hello-from-ssm-agent".len() + SSM_AES_GCM_TAG_LEN
    );
    assert_eq!(
        client_cipher.decrypt(&from_agent).unwrap(),
        b"hello-from-ssm-agent"
    );
    // Agent cannot decrypt its own outbound frame with its decryption key (different half!).
    assert!(agent_cipher.decrypt(&from_agent).is_err());

    let from_client = client_cipher.encrypt(b"hello-from-sgian-client").unwrap();
    assert_eq!(
        agent_cipher.decrypt(&from_client).unwrap(),
        b"hello-from-sgian-client"
    );
    assert!(client_cipher.decrypt(&from_client).is_err());

    // Empty plaintext round-trips as 28 bytes (12B nonce + 16B tag); <28 bytes fails closed.
    let empty_ct = agent_cipher.encrypt(b"").unwrap();
    assert_eq!(empty_ct.len(), 28);
    assert!(client_cipher.decrypt(&empty_ct).unwrap().is_empty());
    assert!(client_cipher.decrypt(&[]).is_err());
    assert!(client_cipher.decrypt(&[0_u8; 12]).is_err());
    assert!(client_cipher.decrypt(&[0_u8; 27]).is_err());

    // Stream payload delivery is gated until `EncChallengeRequest` succeeds!
    assert!(client_cipher
        .decrypt_stream_payload(&from_agent)
        .unwrap_err()
        .contains("before EncChallengeRequest"));

    // 2. Tamper detection across nonce, ciphertext body, and GCM authentication tag.
    for tamper_idx in [0_usize, 15, from_agent.len() - 1] {
        let mut tampered = from_agent.clone();
        tampered[tamper_idx] ^= 0x01;
        assert!(client_cipher.decrypt(&tampered).is_err());
    }

    // 3. Full `EncChallengeRequest` -> `EncChallengeResponse` round-trip.
    let raw_challenge = b"kms-challenge-16";
    let agent_enc_challenge = agent_cipher.encrypt(raw_challenge).unwrap();
    let challenge_req_bytes = serde_json::to_vec(&EncryptionChallengePayload {
        challenge: base64_encode(&agent_enc_challenge),
    })
    .unwrap();
    let challenge_resp_bytes = client_cipher
        .handle_challenge_request(&challenge_req_bytes)
        .expect("client handles EncChallengeRequest");
    assert!(client_cipher.challenge_verified());

    let parsed_resp: EncryptionChallengePayload =
        serde_json::from_slice(&challenge_resp_bytes).unwrap();
    let reencrypted_bytes = base64_decode(&parsed_resp.challenge).unwrap();
    assert_ne!(reencrypted_bytes, agent_enc_challenge);
    assert_eq!(
        agent_cipher.decrypt(&reencrypted_bytes).unwrap(),
        raw_challenge
    );

    // Now stream payloads are unlocked!
    assert_eq!(
        client_cipher.decrypt_stream_payload(&from_agent).unwrap(),
        b"hello-from-ssm-agent"
    );

    // 4. `validate_kms_key_id`, `infer_kms_region`, and `negotiate_handshake_response`.
    assert!(validate_kms_key_id("").is_err());
    assert!(validate_kms_key_id("--cli-input-json").is_err());
    assert!(validate_kms_key_id("alias/bad\nkey").is_err());
    assert_eq!(
        validate_kms_key_id(" alias/sgian-ssm ").unwrap(),
        "alias/sgian-ssm"
    );
    assert_eq!(
        infer_kms_region(
            "arn:aws:kms:eu-west-1:111122223333:key/1234abcd",
            "ws://127.0.0.1:9000/stream"
        ),
        Some("eu-west-1".to_string())
    );
    assert_eq!(
        infer_kms_region(
            "arn:aws-us-gov:kms:us-gov-west-1:111122223333:key/1234abcd",
            "ws://127.0.0.1:9000/stream"
        ),
        Some("us-gov-west-1".to_string())
    );
    assert_eq!(
        infer_kms_region(
            "alias/my-ssm-key",
            "wss://ssmmessages.ap-northeast-1.amazonaws.com/v1/data-channel/sess-1"
        ),
        Some("ap-northeast-1".to_string())
    );
    assert_eq!(
        infer_kms_region("alias/my-ssm-key", "ws://127.0.0.1:9000/stream"),
        None
    );

    let hs_payload = serde_json::to_vec(&json!({
        "AgentVersion": "3.3.0.0",
        "RequestedClientActions": [
            { "ActionType": "SessionType", "ActionParameters": { "SessionType": "Port" } },
            {
                "ActionType": "KMSEncryption",
                "ActionParameters": {
                    "KMSKeyId": "arn:aws:kms:us-west-2:111122223333:key/example",
                    "Challenge": "c2VydmVyLXJhbmRvbQ=="
                }
            }
        ]
    }))
    .unwrap();
    let (hs_resp_bytes, negotiated) = negotiate_handshake_response(
        &hs_payload,
        Some(|key_id: &str, ch: Option<&str>| {
            assert_eq!(key_id, "arn:aws:kms:us-west-2:111122223333:key/example");
            assert_eq!(ch, Some("c2VydmVyLXJhbmRvbQ=="));
            Ok(KmsDataKeyMaterial {
                ciphertext_blob: b"fake-kms-ciphertext-blob".to_vec(),
                plaintext_key: ZeroizingBytes::new(key64.to_vec()),
                challenge_acknowledged: true,
            })
        }),
    )
    .expect("negotiate KMSEncryption handshake");
    assert!(negotiated.is_some());
    let hs_resp: HandshakeResponsePayload = serde_json::from_slice(&hs_resp_bytes).unwrap();
    assert_eq!(hs_resp.processed_client_actions.len(), 2);
    let kms_action = hs_resp
        .processed_client_actions
        .iter()
        .find(|a| a.action_type == "KMSEncryption")
        .unwrap();
    assert_eq!(kms_action.action_status, 1);
    let kms_res = kms_action.action_result.as_ref().unwrap();
    assert_eq!(
        kms_res["KMSCipherTextKey"],
        json!(base64_encode(b"fake-kms-ciphertext-blob"))
    );
    assert_eq!(kms_res["ChallengeAcknowledgement"], json!(true));
}

// ---------------------------------------------------------------------------
// In-Process Mock AWS MGS WebSocket Server + Fake `aws` CLI Integration Suite
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[derive(Clone)]
struct MockMgsKmsConfig {
    kms_key_id: String,
    data_key_64: [u8; 64],
    ciphertext_blob: Vec<u8>,
    corrupt_challenge: bool,
}

#[cfg(unix)]
type SharedBackendMap = Arc<Mutex<BTreeMap<u32, TcpStream>>>;
#[cfg(unix)]
type MockSessionEntry = (SharedBackendMap, Arc<AtomicI64>, Option<Arc<SsmKmsCipher>>);
#[cfg(unix)]
type MockSessionTable = Arc<Mutex<HashMap<String, MockSessionEntry>>>;

#[cfg(unix)]
struct MockMgsServer {
    addr: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    active_port_sockets: Arc<Mutex<Vec<TcpStream>>>,
    outbox_control: Arc<Mutex<VecDeque<AgentMessage>>>,
    resume_tokens_issued: Arc<AtomicUsize>,
    start_port_sessions: Arc<AtomicUsize>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl MockMgsServer {
    fn spawn(daemon_loopback_port: u16, discovery_json: String) -> Self {
        Self::spawn_with_kms(daemon_loopback_port, discovery_json, None)
    }

    fn spawn_with_kms(
        daemon_loopback_port: u16,
        discovery_json: String,
        kms_cfg: Option<MockMgsKmsConfig>,
    ) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind mock MGS");
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let active_port_sockets: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let outbox_control: Arc<Mutex<VecDeque<AgentMessage>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let resume_tokens_issued = Arc::new(AtomicUsize::new(0));
        let start_port_sessions = Arc::new(AtomicUsize::new(0));

        // Shared session table across WebSocket reconnects (`ResumeSession`!) so an
        // in-flight port-forwarding session keeps its smux backend connections and
        // KMS cipher alive across simulated WebSocket drops!
        let session_backends: MockSessionTable = Arc::new(Mutex::new(HashMap::new()));

        let stop_loop = Arc::clone(&stop);
        let sockets_loop = Arc::clone(&active_port_sockets);
        let outbox_loop = Arc::clone(&outbox_control);
        let resume_cnt = Arc::clone(&resume_tokens_issued);
        let start_cnt = Arc::clone(&start_port_sessions);

        let join = thread::spawn(move || {
            while !stop_loop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let stop_conn = Arc::clone(&stop_loop);
                        let sockets_conn = Arc::clone(&sockets_loop);
                        let outbox_conn = Arc::clone(&outbox_loop);
                        let backends_map = Arc::clone(&session_backends);
                        let disc = discovery_json.clone();
                        let kms_clone = kms_cfg.clone();
                        let resume_c = Arc::clone(&resume_cnt);
                        let start_c = Arc::clone(&start_cnt);
                        thread::spawn(move || {
                            handle_mock_mgs_ws(
                                stream,
                                daemon_loopback_port,
                                disc,
                                kms_clone,
                                stop_conn,
                                sockets_conn,
                                outbox_conn,
                                backends_map,
                                resume_c,
                                start_c,
                            );
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            active_port_sockets,
            outbox_control,
            resume_tokens_issued,
            start_port_sessions,
            join: Some(join),
        }
    }

    fn drop_active_port_websockets(&self) {
        if let Ok(mut list) = self.active_port_sockets.lock() {
            for sock in list.drain(..) {
                let _ = sock.shutdown(Shutdown::Both);
            }
        }
    }

    fn send_channel_closed(&self, reason: &str) {
        let payload = serde_json::to_vec(&json!({
            "MessageType": MSG_CHANNEL_CLOSED,
            "Output": reason,
        }))
        .unwrap();
        let msg = AgentMessage::new(
            MSG_CHANNEL_CLOSED,
            0,
            0,
            SsmUuid::new_v4().unwrap(),
            now_millis(),
            0,
            payload,
        )
        .unwrap();
        self.outbox_control.lock().unwrap().push_back(msg);
    }

    fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.drop_active_port_websockets();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(unix)]
fn encode_ws_server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + payload.len());
    out.push(0x80 | (opcode & 0x0F));
    if payload.len() <= 125 {
        out.push(payload.len() as u8);
    } else if payload.len() <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn handle_mock_mgs_ws(
    mut stream: TcpStream,
    daemon_loopback_port: u16,
    discovery_json: String,
    kms_cfg: Option<MockMgsKmsConfig>,
    stop: Arc<AtomicBool>,
    active_port_sockets: Arc<Mutex<Vec<TcpStream>>>,
    outbox_control: Arc<Mutex<VecDeque<AgentMessage>>>,
    session_backends: MockSessionTable,
    resume_cnt: Arc<AtomicUsize>,
    start_cnt: Arc<AtomicUsize>,
) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    // 1. Read HTTP/1.1 Upgrade headers.
    let mut req_buf = Vec::new();
    let header_end = loop {
        if let Some(pos) = req_buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let mut chunk = [0_u8; 512];
        let Ok(n) = stream.read(&mut chunk) else {
            return;
        };
        if n == 0 {
            return;
        }
        req_buf.extend_from_slice(&chunk[..n]);
    };
    let head_str = String::from_utf8_lossy(&req_buf[..header_end]).to_string();
    let first_line = head_str.lines().next().unwrap_or("");
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let mut sec_key = String::new();
    for line in head_str.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("sec-websocket-key") {
                sec_key = v.trim().to_string();
            }
        }
    }
    let accept = compute_websocket_accept(&sec_key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(resp.as_bytes()).is_err() {
        return;
    }

    let mut ws_buf = req_buf[header_end..].to_vec();

    // 2. Read OpenDataChannelInput text frame.
    let open_payload = loop {
        if let Ok(Some((frame, used))) = decode_ws_frame_slice(&ws_buf) {
            ws_buf.drain(..used);
            if frame.opcode == 0x1 {
                break frame.payload;
            }
        }
        let mut tmp = [0_u8; 1024];
        let Ok(n) = stream.read(&mut tmp) else {
            return;
        };
        if n == 0 {
            return;
        }
        ws_buf.extend_from_slice(&tmp[..n]);
    };
    let Ok(open_input) = serde_json::from_slice::<OpenDataChannelInput>(&open_payload) else {
        return;
    };
    if !open_input.token_value.starts_with("tok-") {
        return;
    }

    let is_discovery = path.contains("disc-");
    let is_resume = open_input.token_value.starts_with("tok-resume-");
    let session_id = path
        .rsplit('/')
        .next()
        .unwrap_or("sess-default")
        .to_string();

    let build_hs_request = |session_type: &str, kms: Option<&MockMgsKmsConfig>| -> Vec<u8> {
        let mut actions = vec![json!({
            "ActionType": "SessionType",
            "ActionParameters": { "SessionType": session_type }
        })];
        if let Some(k) = kms {
            actions.push(json!({
                "ActionType": "KMSEncryption",
                "ActionParameters": {
                    "KMSKeyId": k.kms_key_id,
                    "Challenge": "c2VydmVyLWNoYWxsZW5nZQ=="
                }
            }));
        }
        serde_json::to_vec(&json!({
            "AgentVersion": "3.2.0.0",
            "RequestedClientActions": actions,
        }))
        .unwrap()
    };

    if is_discovery {
        // Discovery session: send Handshake (+ optional KMS EncChallenge) + ipc-endpoint JSON + channel_closed.
        let hs_req = build_hs_request("InteractiveCommands", kms_cfg.as_ref());
        let mut disc_seq: i64 = 0;
        let m0 = AgentMessage::new(
            MSG_OUTPUT_STREAM_DATA,
            disc_seq,
            0,
            SsmUuid::new_v4().unwrap(),
            now_millis(),
            payload_type::HANDSHAKE_REQUEST,
            hs_req,
        )
        .unwrap();
        disc_seq += 1;
        let _ = stream.write_all(&encode_ws_server_frame(0x2, &m0.encode().unwrap()));

        let read_next_input =
            |stream: &mut TcpStream, ws_buf: &mut Vec<u8>| -> Option<AgentMessage> {
                loop {
                    if let Ok(Some((frame, used))) = decode_ws_frame_slice(ws_buf) {
                        ws_buf.drain(..used);
                        if frame.opcode == 0x2 {
                            if let Ok(in_msg) = AgentMessage::decode(&frame.payload) {
                                if in_msg.message_type == MSG_INPUT_STREAM_DATA {
                                    let ack = build_ack_frame(
                                        MSG_INPUT_STREAM_DATA,
                                        in_msg.message_id,
                                        in_msg.sequence_number,
                                        now_millis(),
                                    )
                                    .unwrap();
                                    let _ = stream.write_all(&encode_ws_server_frame(
                                        0x2,
                                        &ack.encode().unwrap(),
                                    ));
                                    return Some(in_msg);
                                }
                            }
                        }
                    }
                    let mut tmp = [0_u8; 2048];
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => return None,
                        Ok(n) => ws_buf.extend_from_slice(&tmp[..n]),
                    }
                }
            };

        let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
        let Some(hs_in) = read_next_input(&mut stream, &mut ws_buf) else {
            return;
        };
        if hs_in.payload_type != payload_type::HANDSHAKE_RESPONSE {
            return;
        }

        let disc_cipher = if let Some(ref kcfg) = kms_cfg {
            let Ok(hs_resp) = serde_json::from_slice::<HandshakeResponsePayload>(&hs_in.payload)
            else {
                return;
            };
            let Some(kms_act) = hs_resp
                .processed_client_actions
                .iter()
                .find(|a| a.action_type == "KMSEncryption" && a.action_status == 1)
            else {
                return;
            };
            let expected_b64 = base64_encode(&kcfg.ciphertext_blob);
            if kms_act
                .action_result
                .as_ref()
                .and_then(|v| v.get("KMSCipherTextKey"))
                .and_then(Value::as_str)
                != Some(expected_b64.as_str())
            {
                return;
            }
            let ac = SsmKmsCipher::new_agent_side_for_test(&kcfg.data_key_64).unwrap();
            let raw_ch = b"disc-challenge16";
            let mut enc_ch = ac.encrypt(raw_ch).unwrap();
            if kcfg.corrupt_challenge {
                *enc_ch.last_mut().unwrap() ^= 0xFF;
            }
            let ch_payload = serde_json::to_vec(&EncryptionChallengePayload {
                challenge: base64_encode(&enc_ch),
            })
            .unwrap();
            let ch_msg = AgentMessage::new(
                MSG_OUTPUT_STREAM_DATA,
                disc_seq,
                0,
                SsmUuid::new_v4().unwrap(),
                now_millis(),
                payload_type::ENC_CHALLENGE_REQUEST,
                ch_payload,
            )
            .unwrap();
            disc_seq += 1;
            let _ = stream.write_all(&encode_ws_server_frame(0x2, &ch_msg.encode().unwrap()));

            let Some(ch_in) = read_next_input(&mut stream, &mut ws_buf) else {
                return;
            };
            if ch_in.payload_type != payload_type::ENC_CHALLENGE_RESPONSE {
                return;
            }
            let Ok(ch_resp) = serde_json::from_slice::<EncryptionChallengePayload>(&ch_in.payload)
            else {
                return;
            };
            let Ok(decoded_ch) = base64_decode(&ch_resp.challenge) else {
                return;
            };
            let Ok(plain_ch) = ac.decrypt(&decoded_ch) else {
                return;
            };
            if plain_ch != raw_ch {
                return;
            }
            Some(ac)
        } else {
            None
        };

        let hs_done = AgentMessage::new(
            MSG_OUTPUT_STREAM_DATA,
            disc_seq,
            0,
            SsmUuid::new_v4().unwrap(),
            now_millis(),
            payload_type::HANDSHAKE_COMPLETE,
            b"{}".to_vec(),
        )
        .unwrap();
        disc_seq += 1;
        let _ = stream.write_all(&encode_ws_server_frame(0x2, &hs_done.encode().unwrap()));

        let noisy_raw =
            format!("\x1b[32m$ sgian ctl ipc-endpoint\x1b[0m\r\n{discovery_json}\r\n").into_bytes();
        let noisy_wire = match disc_cipher {
            Some(ref ac) => ac.encrypt(&noisy_raw).unwrap(),
            None => noisy_raw,
        };
        let out_msg = AgentMessage::new(
            MSG_OUTPUT_STREAM_DATA,
            disc_seq,
            0,
            SsmUuid::new_v4().unwrap(),
            now_millis(),
            payload_type::OUTPUT,
            noisy_wire,
        )
        .unwrap();
        let _ = stream.write_all(&encode_ws_server_frame(0x2, &out_msg.encode().unwrap()));

        let closed = AgentMessage::new(
            MSG_CHANNEL_CLOSED,
            0,
            0,
            SsmUuid::new_v4().unwrap(),
            now_millis(),
            0,
            b"{}".to_vec(),
        )
        .unwrap();
        let _ = stream.write_all(&encode_ws_server_frame(0x2, &closed.encode().unwrap()));
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(50));
        return;
    }

    // Port-forwarding session (`StartSession` or `ResumeSession`)!
    if let Ok(cloned) = stream.try_clone() {
        active_port_sockets.lock().unwrap().push(cloned);
    }

    let (backends, out_seq, session_cipher) = {
        let mut map = session_backends.lock().unwrap();
        map.entry(session_id)
            .or_insert_with(|| {
                let cipher_arc = kms_cfg.as_ref().map(|k| {
                    Arc::new(SsmKmsCipher::new_agent_side_for_test(&k.data_key_64).unwrap())
                });
                (
                    Arc::new(Mutex::new(BTreeMap::new())),
                    Arc::new(AtomicI64::new(0)),
                    cipher_arc,
                )
            })
            .clone()
    };

    let expected_port_challenge = b"port-challenge16";

    if is_resume {
        resume_cnt.fetch_add(1, Ordering::SeqCst);
    } else {
        start_cnt.fetch_add(1, Ordering::SeqCst);
        let hs_req = build_hs_request("Port", kms_cfg.as_ref());
        let seq0 = out_seq.fetch_add(1, Ordering::SeqCst);
        let m0 = AgentMessage::new(
            MSG_OUTPUT_STREAM_DATA,
            seq0,
            0,
            SsmUuid::new_v4().unwrap(),
            now_millis(),
            payload_type::HANDSHAKE_REQUEST,
            hs_req,
        )
        .unwrap();
        if stream
            .write_all(&encode_ws_server_frame(0x2, &m0.encode().unwrap()))
            .is_err()
        {
            return;
        }
    }

    let _ = stream.set_read_timeout(Some(Duration::from_millis(5)));
    let mut smux_dec = SmuxDecoder::new();

    while !stop.load(Ordering::SeqCst) {
        // Inject queued control messages (e.g. `channel_closed`).
        if let Some(ctrl) = outbox_control.lock().unwrap().pop_front() {
            let _ = stream.write_all(&encode_ws_server_frame(0x2, &ctrl.encode().unwrap()));
            let _ = stream.flush();
            if ctrl.message_type == MSG_CHANNEL_CLOSED {
                thread::sleep(Duration::from_millis(30));
                return;
            }
        }

        // Read client WS frames.
        let mut tmp = [0_u8; 16 * 1024];
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => ws_buf.extend_from_slice(&tmp[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return,
        }

        while let Ok(Some((frame, used))) = decode_ws_frame_slice(&ws_buf) {
            ws_buf.drain(..used);
            if frame.opcode == 0x8 {
                return;
            }
            if frame.opcode == 0x9 {
                let _ = stream.write_all(&encode_ws_server_frame(0xA, &frame.payload));
                continue;
            }
            if frame.opcode != 0x2 {
                continue;
            }
            let Ok(in_msg) = AgentMessage::decode(&frame.payload) else {
                continue;
            };
            if in_msg.message_type == MSG_INPUT_STREAM_DATA {
                let ack = build_ack_frame(
                    MSG_INPUT_STREAM_DATA,
                    in_msg.message_id,
                    in_msg.sequence_number,
                    now_millis(),
                )
                .unwrap();
                if stream
                    .write_all(&encode_ws_server_frame(0x2, &ack.encode().unwrap()))
                    .is_err()
                {
                    return;
                }

                if in_msg.payload_type == payload_type::HANDSHAKE_RESPONSE {
                    if let (Some(ref kcfg), Some(ref ac)) = (&kms_cfg, &session_cipher) {
                        let Ok(hs_resp) =
                            serde_json::from_slice::<HandshakeResponsePayload>(&in_msg.payload)
                        else {
                            return;
                        };
                        let Some(kms_act) = hs_resp
                            .processed_client_actions
                            .iter()
                            .find(|a| a.action_type == "KMSEncryption" && a.action_status == 1)
                        else {
                            return;
                        };
                        let expected_b64 = base64_encode(&kcfg.ciphertext_blob);
                        if kms_act
                            .action_result
                            .as_ref()
                            .and_then(|v| v.get("KMSCipherTextKey"))
                            .and_then(Value::as_str)
                            != Some(expected_b64.as_str())
                        {
                            return;
                        }
                        let mut enc_ch = ac.encrypt(expected_port_challenge).unwrap();
                        if kcfg.corrupt_challenge {
                            *enc_ch.last_mut().unwrap() ^= 0xFF;
                        }
                        let ch_payload = serde_json::to_vec(&EncryptionChallengePayload {
                            challenge: base64_encode(&enc_ch),
                        })
                        .unwrap();
                        let seq_ch = out_seq.fetch_add(1, Ordering::SeqCst);
                        let ch_msg = AgentMessage::new(
                            MSG_OUTPUT_STREAM_DATA,
                            seq_ch,
                            0,
                            SsmUuid::new_v4().unwrap(),
                            now_millis(),
                            payload_type::ENC_CHALLENGE_REQUEST,
                            ch_payload,
                        )
                        .unwrap();
                        if stream
                            .write_all(&encode_ws_server_frame(0x2, &ch_msg.encode().unwrap()))
                            .is_err()
                        {
                            return;
                        }
                    } else {
                        let seq_hc = out_seq.fetch_add(1, Ordering::SeqCst);
                        let hc = AgentMessage::new(
                            MSG_OUTPUT_STREAM_DATA,
                            seq_hc,
                            0,
                            SsmUuid::new_v4().unwrap(),
                            now_millis(),
                            payload_type::HANDSHAKE_COMPLETE,
                            b"{}".to_vec(),
                        )
                        .unwrap();
                        if stream
                            .write_all(&encode_ws_server_frame(0x2, &hc.encode().unwrap()))
                            .is_err()
                        {
                            return;
                        }
                    }
                } else if in_msg.payload_type == payload_type::ENC_CHALLENGE_RESPONSE {
                    let Some(ref ac) = session_cipher else {
                        return;
                    };
                    let Ok(ch_resp) =
                        serde_json::from_slice::<EncryptionChallengePayload>(&in_msg.payload)
                    else {
                        return;
                    };
                    let Ok(decoded_ch) = base64_decode(&ch_resp.challenge) else {
                        return;
                    };
                    let Ok(plain_ch) = ac.decrypt(&decoded_ch) else {
                        return;
                    };
                    if plain_ch != expected_port_challenge {
                        return;
                    }
                    let seq_hc = out_seq.fetch_add(1, Ordering::SeqCst);
                    let hc = AgentMessage::new(
                        MSG_OUTPUT_STREAM_DATA,
                        seq_hc,
                        0,
                        SsmUuid::new_v4().unwrap(),
                        now_millis(),
                        payload_type::HANDSHAKE_COMPLETE,
                        b"{}".to_vec(),
                    )
                    .unwrap();
                    if stream
                        .write_all(&encode_ws_server_frame(0x2, &hc.encode().unwrap()))
                        .is_err()
                    {
                        return;
                    }
                } else if in_msg.payload_type == payload_type::OUTPUT {
                    let smux_input = match session_cipher.as_ref() {
                        Some(ac) => match ac.decrypt(&in_msg.payload) {
                            Ok(plain) => plain,
                            Err(_) => return,
                        },
                        None => in_msg.payload.clone(),
                    };
                    if let Ok(sframes) = smux_dec.push(&smux_input) {
                        let mut bmap = backends.lock().unwrap();
                        for sf in sframes {
                            match sf.cmd {
                                SMUX_CMD_SYN => {
                                    if let Ok(tcp) =
                                        TcpStream::connect(("127.0.0.1", daemon_loopback_port))
                                    {
                                        let _ = tcp.set_nodelay(true);
                                        let _ = tcp.set_nonblocking(true);
                                        bmap.insert(sf.stream_id, tcp);
                                    }
                                }
                                SMUX_CMD_PSH => {
                                    if let Some(tcp) = bmap.get_mut(&sf.stream_id) {
                                        let _ = tcp.set_nonblocking(false);
                                        let _ = tcp.write_all(&sf.data);
                                        let _ = tcp.set_nonblocking(true);
                                    }
                                }
                                SMUX_CMD_FIN => {
                                    if let Some(tcp) = bmap.remove(&sf.stream_id) {
                                        let _ = tcp.shutdown(Shutdown::Both);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        // Poll backend loopback TCP sockets and emit smux PSH / FIN inside `output_stream_data`.
        let mut outbound_smux: Vec<SmuxFrame> = Vec::new();
        {
            let mut bmap = backends.lock().unwrap();
            let sids: Vec<u32> = bmap.keys().copied().collect();
            let mut finished = Vec::new();
            for sid in sids {
                let mut rbuf = [0_u8; 16 * 1024];
                match bmap.get_mut(&sid).unwrap().read(&mut rbuf) {
                    Ok(0) => {
                        finished.push(sid);
                        outbound_smux.push(SmuxFrame::fin(sid));
                    }
                    Ok(n) => {
                        outbound_smux.extend(SmuxFrame::psh_chunks(sid, &rbuf[..n]));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        finished.push(sid);
                        outbound_smux.push(SmuxFrame::fin(sid));
                    }
                }
            }
            for sid in finished {
                bmap.remove(&sid);
            }
        }

        for sf in outbound_smux {
            let raw_sf = sf.encode().unwrap();
            let wire_sf = match session_cipher.as_ref() {
                Some(ac) => ac.encrypt(&raw_sf).unwrap(),
                None => raw_sf,
            };
            let seq = out_seq.fetch_add(1, Ordering::SeqCst);
            let out_msg = AgentMessage::new(
                MSG_OUTPUT_STREAM_DATA,
                seq,
                0,
                SsmUuid::new_v4().unwrap(),
                now_millis(),
                payload_type::OUTPUT,
                wire_sf,
            )
            .unwrap();
            if stream
                .write_all(&encode_ws_server_frame(0x2, &out_msg.encode().unwrap()))
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(unix)]
fn write_fake_aws_cli(script_path: &Path, aws_log: &Path, mgs_addr: std::net::SocketAddr) {
    write_fake_aws_cli_with_kms(script_path, aws_log, mgs_addr, None);
}

#[cfg(unix)]
fn write_fake_aws_cli_with_kms(
    script_path: &Path,
    aws_log: &Path,
    mgs_addr: std::net::SocketAddr,
    kms_cfg: Option<&MockMgsKmsConfig>,
) {
    use std::os::unix::fs::PermissionsExt;
    let counter_file = script_path.with_extension("seq");
    fs::write(&counter_file, "0").unwrap();
    let (kms_pt_b64, kms_ct_b64) = match kms_cfg {
        Some(k) => (
            base64_encode(&k.data_key_64),
            base64_encode(&k.ciphertext_blob),
        ),
        None => (String::new(), String::new()),
    };
    let script = format!(
        "#!/bin/sh\n\
         echo \"$@\" >> \"{}\"\n\
         n=$(cat \"{}\")\n\
         n=$((n + 1))\n\
         printf '%s' \"$n\" > \"{}\"\n\
         case \"$*\" in\n\
           *generate-data-key*)\n\
             case \"$*\" in\n\
               *\"--number-of-bytes 64\"*aws:ssm:SessionId*aws:ssm:TargetId*)\n\
                 printf '{{\"Plaintext\":\"{}\",\"CiphertextBlob\":\"{}\",\"KeyId\":\"mock\"}}\\n'\n\
                 exit 0\n\
                 ;;\n\
               *)\n\
                 echo \"missing expected KMS GenerateDataKey args\" >&2\n\
                 exit 2\n\
                 ;;\n\
             esac\n\
             ;;\n\
           *terminate-session*)\n\
             printf '{{\"SessionId\":\"terminated\"}}\\n'\n\
             exit 0\n\
             ;;\n\
           *resume-session*)\n\
             prev=\"\"\n\
             sid=\"sess-resumed\"\n\
             for a in \"$@\"; do\n\
               if [ \"$prev\" = \"--session-id\" ]; then sid=\"$a\"; break; fi\n\
               prev=\"$a\"\n\
             done\n\
             json=$(printf '{{\"SessionId\":\"%s\",\"StreamUrl\":\"ws://{}/stream/%s\",\"TokenValue\":\"tok-resume-%s\"}}' \"$sid\" \"$sid\" \"$n\")\n\
             session-manager-plugin \"$json\" us-east-1 ResumeSession default '{{}}' https://ssm.us-east-1.amazonaws.com\n\
             exit $?\n\
             ;;\n\
           *AWS-StartInteractiveCommand*)\n\
             sid=\"disc-$n\"\n\
             json=$(printf '{{\"SessionId\":\"%s\",\"StreamUrl\":\"ws://{}/stream/%s\",\"TokenValue\":\"tok-disc-%s\"}}' \"$sid\" \"$sid\" \"$n\")\n\
             session-manager-plugin \"$json\" us-east-1 StartSession default '{{}}' https://ssm.us-east-1.amazonaws.com\n\
             exit $?\n\
             ;;\n\
           *AWS-StartPortForwardingSession*)\n\
             sid=\"port-$n\"\n\
             json=$(printf '{{\"SessionId\":\"%s\",\"StreamUrl\":\"ws://{}/stream/%s\",\"TokenValue\":\"tok-port-%s\"}}' \"$sid\" \"$sid\" \"$n\")\n\
             session-manager-plugin \"$json\" us-east-1 StartSession default '{{}}' https://ssm.us-east-1.amazonaws.com\n\
             exit $?\n\
             ;;\n\
         esac\n\
         exit 1\n",
        aws_log.display(),
        counter_file.display(),
        counter_file.display(),
        kms_pt_b64,
        kms_ct_b64,
        mgs_addr,
        mgs_addr,
        mgs_addr,
    );
    fs::write(script_path, script).unwrap();
    fs::set_permissions(script_path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[cfg(unix)]
#[test]
fn ssm_mock_mgs_end_to_end_multiplexing_auth_and_tunnel_reuse() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        remote_loopback_port: Some(0),
        ..Default::default()
    });
    let local_admin = daemon.client();
    let loopback_port = native_ipc_endpoint(&local_admin)
        .expect("endpoint")
        .loopback_port
        .expect("loopback_port");

    let issued: Value = local_admin
        .request(DaemonRequest::IdentityIssue {
            holder: "ssm-operator".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
        })
        .expect("issue credential");
    let cred_value = issued["token"].as_str().unwrap().to_string();

    let discovery_json = serde_json::to_string(&json!({
        "transport": "unix_socket",
        "endpoint": "/run/user/1000/sgian/ec2/daemon.sock",
        "protocol_version": PROTOCOL_VERSION,
        "loopback_port": loopback_port,
    }))
    .unwrap();

    let mgs = MockMgsServer::spawn(loopback_port, discovery_json);

    // Use `/tmp` so forwarded Unix domain socket stays well below macOS 104-byte SUN_LEN.
    let root = tempfile::tempdir_in("/tmp").expect("short temp dir");
    let app_dir = root.path().join("app");
    fs::create_dir_all(&app_dir).unwrap();
    let aws_log = root.path().join("aws.log");
    let fake_aws = root.path().join("fake-aws.sh");
    write_fake_aws_cli(&fake_aws, &aws_log, mgs.addr);

    let add_cmd = parse_machine_args(&[
        "add".into(),
        "ec2box".into(),
        "--transport".into(),
        "ssm".into(),
        "--target".into(),
        "i-0123456789abcdef0".into(),
        "--workspace".into(),
        "/srv/ec2repo".into(),
    ])
    .unwrap();
    control_machine_in(&app_dir, add_cmd, std::io::empty(), true).unwrap();

    // 1. Missing client_token fails closed on remote loopback target!
    let unauth_err = ensure_machine_client_with_bins_in(
        &app_dir,
        "ec2box",
        None,
        Some(fake_aws.to_str().unwrap()),
    )
    .err()
    .expect("missing client_token over SSM must fail closed");
    assert!(
        unauth_err.contains("authentication") || unauth_err.contains("identity issue"),
        "expected actionable auth error, got: {unauth_err}"
    );

    // 2. Configure credential and attach via SSM + smux port forwarding!
    write_machine_token_in(&app_dir, "ec2box", &cred_value).unwrap();
    let (remote_client, endpoint) = ensure_machine_client_with_bins_in(
        &app_dir,
        "ec2box",
        None,
        Some(fake_aws.to_str().unwrap()),
    )
    .expect("SSM tunnel should come up and authenticate");
    assert_eq!(endpoint.workspace_key, "machine:ec2box");

    let whoami: Value = remote_client
        .request(DaemonRequest::Whoami)
        .expect("Whoami over SSM smux");
    assert_eq!(whoami["holder"], json!("ssm-operator"));

    // 3. Exercise concurrent multiplexed smux streams (`Subscribe` + `CreatePane` +
    // `TakeLease` + `SendInputAs` + `ReleaseLease`) over the single MGS WebSocket!
    let events_seen = Arc::new(AtomicUsize::new(0));
    let events_sink = Arc::clone(&events_seen);
    let sub_handle = remote_client.start_subscription_with_sink(move |_ev| {
        events_sink.fetch_add(1, Ordering::SeqCst);
    });

    let created: Value = remote_client
        .request(DaemonRequest::CreatePane {
            title: Some("ssm-shell".to_string()),
            profile: None,
        })
        .expect("CreatePane over SSM");
    let new_pane_id = created["id"].as_str().unwrap().to_string();

    let ev_deadline = Instant::now() + Duration::from_secs(4);
    while events_seen.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < ev_deadline,
            "SSM multiplexed subscription did not receive PaneCreated"
        );
        thread::sleep(Duration::from_millis(20));
    }

    let lease: LeaseInfo = remote_client
        .request(DaemonRequest::TakeLease {
            pane_id: new_pane_id.clone(),
            holder: "ssm-operator".to_string(),
            force: false,
            why: None,
        })
        .expect("TakeLease over SSM");
    assert_eq!(lease.holder.as_deref(), Some("ssm-operator"));

    let _: CommandOk = remote_client
        .request(DaemonRequest::SendInputAs {
            pane_id: new_pane_id.clone(),
            input: "hello-over-ssm\n".to_string(),
            holder: "ssm-operator".to_string(),
            generation: lease.generation,
        })
        .expect("SendInputAs over SSM");

    let _: LeaseInfo = remote_client
        .request(DaemonRequest::ReleaseLease {
            pane_id: new_pane_id.clone(),
            holder: "ssm-operator".to_string(),
            note: "verified over ssm".to_string(),
            generation: lease.generation,
        })
        .expect("ReleaseLease over SSM");

    // 4. Second attach reuses the live SSM tunnel with zero extra `aws ssm` spawns!
    let log_before_reuse = fs::read_to_string(&aws_log).unwrap();
    let count_before_reuse = log_before_reuse.lines().count();
    let (reused_client, _) = ensure_machine_client_with_bins_in(
        &app_dir,
        "ec2box",
        None,
        Some(fake_aws.to_str().unwrap()),
    )
    .expect("reuse live SSM tunnel");
    let ok: CommandOk = reused_client.request(DaemonRequest::Ping).unwrap();
    assert!(ok.ok);
    assert_eq!(
        fs::read_to_string(&aws_log).unwrap().lines().count(),
        count_before_reuse,
        "live SSM tunnel should be reused without spawning aws again"
    );

    sub_handle.cancel();
    let _ = control_machine_in(
        &app_dir,
        MachineCommand::Remove {
            name: "ec2box".to_string(),
        },
        std::io::empty(),
        true,
    );
    mgs.shutdown();
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn ssm_mock_mgs_websocket_blip_resumes_session_and_verifies_remote_ledger() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        remote_loopback_port: Some(0),
        ..Default::default()
    });
    let local_admin = daemon.client();
    let panes: PaneList = local_admin.request(DaemonRequest::ListPanes).unwrap();
    let initial_pane_id = panes.panes[0].pane.id.clone();

    let loopback_port = native_ipc_endpoint(&local_admin)
        .expect("endpoint")
        .loopback_port
        .expect("loopback_port");

    let issued: Value = local_admin
        .request(DaemonRequest::IdentityIssue {
            holder: "ssm-resumer".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
        })
        .expect("issue credential");
    let cred_value = issued["token"].as_str().unwrap().to_string();

    let discovery_json = serde_json::to_string(&json!({
        "transport": "named_pipe",
        "endpoint": "\\\\.\\pipe\\sgian2-ws-001",
        "protocol_version": PROTOCOL_VERSION,
        "loopback_port": loopback_port,
    }))
    .unwrap();

    let mgs = MockMgsServer::spawn(loopback_port, discovery_json);

    let root = tempfile::tempdir_in("/tmp").expect("short temp dir");
    let app_dir = root.path().join("app");
    fs::create_dir_all(&app_dir).unwrap();
    let aws_log = root.path().join("aws.log");
    let fake_aws = root.path().join("fake-aws.sh");
    write_fake_aws_cli(&fake_aws, &aws_log, mgs.addr);

    let add_cmd = parse_machine_args(&[
        "add".into(),
        "wsbox".into(),
        "--transport".into(),
        "ssm".into(),
        "--target".into(),
        "mi-0987654321fedcba0".into(),
        "--workspace".into(),
        "C:\\Users\\op\\repo".into(),
    ])
    .unwrap();
    control_machine_in(&app_dir, add_cmd, std::io::empty(), true).unwrap();
    write_machine_token_in(&app_dir, "wsbox", &cred_value).unwrap();

    let (remote_client, _) = ensure_machine_client_with_bins_in(
        &app_dir,
        "wsbox",
        None,
        Some(fake_aws.to_str().unwrap()),
    )
    .expect("SSM tunnel should connect to simulated Windows WorkSpace");

    // Wait briefly for the initial `session.started` audit record to land.
    let started_deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let ledger_resp: Value = remote_client
            .request(DaemonRequest::PaneLedger {
                pane_id: initial_pane_id.clone(),
                limit: 0,
                verify: false,
            })
            .unwrap();
        let recs = ledger_resp["records"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if recs.iter().any(|r| r["type"] == json!("session.started")) {
            break;
        }
        assert!(
            Instant::now() < started_deadline,
            "timed out waiting for session.started ledger record: {recs:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // 1. Simulate a mid-session WebSocket blip (NO `channel_closed`): tunnel must
    // call `aws ssm resume-session`, recover seamlessly, and record `session.resumed`!
    mgs.drop_active_port_websockets();

    let resume_deadline = Instant::now() + Duration::from_secs(5);
    while mgs.resume_tokens_issued.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < resume_deadline,
            "SSM tunnel did not invoke ResumeSession after WebSocket drop"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let ok: CommandOk = remote_client
        .request(DaemonRequest::Ping)
        .expect("Ping after ResumeSession recovery");
    assert!(ok.ok);

    // Wait for `session.resumed` to land in the remote pane's hash-chained ledger,
    // then verify the remote ledger chain cryptographically via `PaneLedger { verify: true }`
    // AND client-side `ledger_verify_records`!
    let ledger_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let verified: Value = remote_client
            .request(DaemonRequest::PaneLedger {
                pane_id: initial_pane_id.clone(),
                limit: 0,
                verify: true,
            })
            .expect("remote PaneLedger --verify");
        assert_eq!(verified["ok"], json!(true));
        let items = verified["items"].as_array().cloned().unwrap_or_default();
        let local_summary =
            ledger_verify_records(&items).expect("client-side cryptographic chain verification");
        assert_eq!(json!(local_summary.records), verified["records"]);
        assert_eq!(json!(local_summary.head), verified["head"]);
        if items.iter().any(|r| r["type"] == json!("session.resumed")) {
            break;
        }
        assert!(
            Instant::now() < ledger_deadline,
            "timed out waiting for session.resumed in verified ledger: {items:?}"
        );
        thread::sleep(Duration::from_millis(30));
    }

    // 2. Simulate `channel_closed` from MGS: tunnel must start a fresh `StartSession`
    // and record `session.terminated` + `session.started`!
    let starts_before = mgs.start_port_sessions.load(Ordering::SeqCst);
    mgs.send_channel_closed("max_session_duration");

    let restart_deadline = Instant::now() + Duration::from_secs(5);
    while mgs.start_port_sessions.load(Ordering::SeqCst) <= starts_before {
        assert!(
            Instant::now() < restart_deadline,
            "SSM tunnel did not start a fresh port session after channel_closed"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let term_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(verified) = remote_client.request::<Value>(DaemonRequest::PaneLedger {
            pane_id: initial_pane_id.clone(),
            limit: 0,
            verify: true,
        }) {
            let items = verified["items"].as_array().cloned().unwrap_or_default();
            ledger_verify_records(&items).expect("chain remains valid after channel_closed");
            if items
                .iter()
                .any(|r| r["type"] == json!("session.terminated"))
            {
                break;
            }
        }
        assert!(
            Instant::now() < term_deadline,
            "timed out waiting for session.terminated ledger record"
        );
        thread::sleep(Duration::from_millis(30));
    }

    let aws_invocations = fs::read_to_string(&aws_log).unwrap();
    assert!(aws_invocations.contains("resume-session"));
    assert!(aws_invocations.contains("AWS-StartPortForwardingSession"));

    let _ = control_machine_in(
        &app_dir,
        MachineCommand::Remove {
            name: "wsbox".to_string(),
        },
        std::io::empty(),
        true,
    );
    mgs.shutdown();
    daemon.shutdown();
}

#[cfg(unix)]
#[test]
fn ssm_mock_mgs_kms_encrypted_discovery_multiplexing_resume_and_tamper_refusal() {
    let daemon = TestDaemon::spawn(Config {
        shell: Some("/bin/cat".to_string()),
        remote_loopback_port: Some(0),
        ..Default::default()
    });
    let local_admin = daemon.client();
    let loopback_port = native_ipc_endpoint(&local_admin)
        .expect("endpoint")
        .loopback_port
        .expect("loopback_port");

    let issued: Value = local_admin
        .request(DaemonRequest::IdentityIssue {
            holder: "ssm-kms-operator".to_string(),
            scopes: vec!["read".to_string(), "write".to_string()],
        })
        .expect("issue credential");
    let cred_value = issued["token"].as_str().unwrap().to_string();

    let discovery_json = serde_json::to_string(&json!({
        "transport": "unix_socket",
        "endpoint": "/run/user/1000/sgian/kms/daemon.sock",
        "protocol_version": PROTOCOL_VERSION,
        "loopback_port": loopback_port,
    }))
    .unwrap();

    let mut data_key_64 = [0_u8; 64];
    for (i, slot) in data_key_64[..32].iter_mut().enumerate() {
        *slot = (0x21 + i) as u8;
    }
    for (i, slot) in data_key_64[32..].iter_mut().enumerate() {
        *slot = (0xA1 + i) as u8;
    }

    // 1. Tampered `EncChallengeRequest` fails closed during discovery!
    let bad_kms_cfg = MockMgsKmsConfig {
        kms_key_id: "arn:aws:kms:us-west-2:111122223333:key/bad-challenge-key".to_string(),
        data_key_64,
        ciphertext_blob: b"kms-ciphertext-bad".to_vec(),
        corrupt_challenge: true,
    };
    let bad_mgs = MockMgsServer::spawn_with_kms(
        loopback_port,
        discovery_json.clone(),
        Some(bad_kms_cfg.clone()),
    );

    let root = tempfile::tempdir_in("/tmp").expect("short temp dir");
    let bad_app_dir = root.path().join("bad-app");
    fs::create_dir_all(&bad_app_dir).unwrap();
    let bad_aws_log = root.path().join("bad-aws.log");
    let bad_fake_aws = root.path().join("bad-fake-aws.sh");
    write_fake_aws_cli_with_kms(
        &bad_fake_aws,
        &bad_aws_log,
        bad_mgs.addr,
        Some(&bad_kms_cfg),
    );

    let bad_add = parse_machine_args(&[
        "add".into(),
        "kmsbad".into(),
        "--transport".into(),
        "ssm".into(),
        "--target".into(),
        "i-0badc0ffee0000001".into(),
        "--workspace".into(),
        "/srv/kmsbad".into(),
    ])
    .unwrap();
    control_machine_in(&bad_app_dir, bad_add, std::io::empty(), true).unwrap();
    write_machine_token_in(&bad_app_dir, "kmsbad", &cred_value).unwrap();

    let tamper_err = ensure_machine_client_with_bins_in(
        &bad_app_dir,
        "kmsbad",
        None,
        Some(bad_fake_aws.to_str().unwrap()),
    )
    .err()
    .expect("corrupted EncChallengeRequest must fail closed");
    assert!(
        tamper_err.contains("authentication tag") || tamper_err.contains("EncChallenge"),
        "expected AES-GCM tag failure on tampered challenge, got: {tamper_err}"
    );
    bad_mgs.shutdown();

    // 2. Valid KMS envelope encryption across BOTH `AWS-StartInteractiveCommand`
    // discovery AND `AWS-StartPortForwardingSession` smux multiplexing + `ResumeSession`!
    let kms_cfg = MockMgsKmsConfig {
        kms_key_id: "arn:aws:kms:us-west-2:111122223333:key/good-ssm-key".to_string(),
        data_key_64,
        ciphertext_blob: b"kms-ciphertext-good-blob".to_vec(),
        corrupt_challenge: false,
    };
    let mgs = MockMgsServer::spawn_with_kms(loopback_port, discovery_json, Some(kms_cfg.clone()));

    let app_dir = root.path().join("good-app");
    fs::create_dir_all(&app_dir).unwrap();
    let aws_log = root.path().join("good-aws.log");
    let fake_aws = root.path().join("good-fake-aws.sh");
    write_fake_aws_cli_with_kms(&fake_aws, &aws_log, mgs.addr, Some(&kms_cfg));

    let add_cmd = parse_machine_args(&[
        "add".into(),
        "kmsbox".into(),
        "--transport".into(),
        "ssm".into(),
        "--target".into(),
        "i-0fedcba9876543210".into(),
        "--workspace".into(),
        "/srv/kmsrepo".into(),
    ])
    .unwrap();
    control_machine_in(&app_dir, add_cmd, std::io::empty(), true).unwrap();
    write_machine_token_in(&app_dir, "kmsbox", &cred_value).unwrap();

    let (remote_client, endpoint) = ensure_machine_client_with_bins_in(
        &app_dir,
        "kmsbox",
        None,
        Some(fake_aws.to_str().unwrap()),
    )
    .expect("KMS-encrypted SSM discovery and port-forwarding tunnel should come up");
    assert_eq!(endpoint.workspace_key, "machine:kmsbox");

    let whoami: Value = remote_client
        .request(DaemonRequest::Whoami)
        .expect("Whoami over KMS-encrypted SSM smux");
    assert_eq!(whoami["holder"], json!("ssm-kms-operator"));

    let created: Value = remote_client
        .request(DaemonRequest::CreatePane {
            title: Some("kms-shell".to_string()),
            profile: None,
        })
        .expect("CreatePane over KMS-encrypted SSM");
    let pane_id = created["id"].as_str().unwrap().to_string();

    let lease: LeaseInfo = remote_client
        .request(DaemonRequest::TakeLease {
            pane_id: pane_id.clone(),
            holder: "ssm-kms-operator".to_string(),
            force: false,
            why: None,
        })
        .expect("TakeLease over KMS-encrypted SSM");
    let _: CommandOk = remote_client
        .request(DaemonRequest::SendInputAs {
            pane_id: pane_id.clone(),
            input: "kms-encrypted-payload\n".to_string(),
            holder: "ssm-kms-operator".to_string(),
            generation: lease.generation,
        })
        .expect("SendInputAs over KMS-encrypted SSM");
    let _: LeaseInfo = remote_client
        .request(DaemonRequest::ReleaseLease {
            pane_id: pane_id.clone(),
            holder: "ssm-kms-operator".to_string(),
            note: "kms session ok".to_string(),
            generation: lease.generation,
        })
        .expect("ReleaseLease over KMS-encrypted SSM");

    // Verify `kms generate-data-key` was called twice (once for discovery, once for port session)
    // with `--region us-west-2` inferred from the KMS key ARN!
    let log_before_blip = fs::read_to_string(&aws_log).unwrap();
    let kms_calls_before_blip = log_before_blip
        .lines()
        .filter(|l| l.contains("generate-data-key") && l.contains("--region us-west-2"))
        .count();
    assert_eq!(
        kms_calls_before_blip, 2,
        "expected KMS GenerateDataKey on discovery + initial port session, log:\n{log_before_blip}"
    );

    // 3. Simulate WebSocket drop (`ResumeSession`): existing KMS cipher MUST stay active
    // without calling `kms generate-data-key` again!
    mgs.drop_active_port_websockets();
    let resume_deadline = Instant::now() + Duration::from_secs(5);
    while mgs.resume_tokens_issued.load(Ordering::SeqCst) == 0 {
        assert!(
            Instant::now() < resume_deadline,
            "KMS SSM tunnel did not invoke ResumeSession after WebSocket drop"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let ok: CommandOk = remote_client
        .request(DaemonRequest::Ping)
        .expect("Ping after KMS ResumeSession");
    assert!(ok.ok);

    let log_after_blip = fs::read_to_string(&aws_log).unwrap();
    let kms_calls_after_blip = log_after_blip
        .lines()
        .filter(|l| l.contains("generate-data-key"))
        .count();
    assert_eq!(
        kms_calls_after_blip, 2,
        "ResumeSession must preserve active SsmKmsCipher without generating a new data key"
    );

    // 4. Simulate `channel_closed`: fresh `StartSession` resets cipher and generates a new key!
    let starts_before = mgs.start_port_sessions.load(Ordering::SeqCst);
    mgs.send_channel_closed("kms_session_rotated");
    let restart_deadline = Instant::now() + Duration::from_secs(5);
    while mgs.start_port_sessions.load(Ordering::SeqCst) <= starts_before {
        assert!(
            Instant::now() < restart_deadline,
            "KMS SSM tunnel did not start a fresh session after channel_closed"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let key_rot_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let cur_log = fs::read_to_string(&aws_log).unwrap();
        let kms_cnt = cur_log
            .lines()
            .filter(|l| l.contains("generate-data-key"))
            .count();
        if kms_cnt >= 3 {
            break;
        }
        assert!(
            Instant::now() < key_rot_deadline,
            "expected fresh KMS GenerateDataKey after channel_closed, got {kms_cnt}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let ok_post_rotate: CommandOk = remote_client
        .request(DaemonRequest::Ping)
        .expect("Ping after KMS channel_closed rotation");
    assert!(ok_post_rotate.ok);

    let _ = control_machine_in(
        &app_dir,
        MachineCommand::Remove {
            name: "kmsbox".to_string(),
        },
        std::io::empty(),
        true,
    );
    mgs.shutdown();
    daemon.shutdown();
}

#[test]
fn remote_ssm_discovery_command_escapes_trailing_backslash_and_quotes() {
    assert_eq!(
        crate::ssm::remote_ssm_discovery_command(r"foo\"),
        r#"sgian ctl --workspace "foo\\" --json ipc-endpoint"#
    );
    assert_eq!(
        crate::ssm::remote_ssm_discovery_command(r#"foo\"; id #" "#),
        r#"sgian ctl --workspace "foo\\\"; id #\" " --json ipc-endpoint"#
    );
}

#[test]
fn parse_ws_url_rejects_header_injection_and_malformed_brackets() {
    assert!(crate::ssm::parse_ws_url("wss://host.example.com\r\nInjected: 1/stream").is_err());
    assert!(crate::ssm::parse_ws_url("wss://host.example.com/stream\r\nX-Test: 1").is_err());
    assert!(crate::ssm::parse_ws_url("wss://host.example.com/stream with spaces").is_err());
    assert!(crate::ssm::parse_ws_url("wss://[::1]junk:443/stream").is_err());
    let parsed =
        crate::ssm::parse_ws_url("wss://[::1]:8443/v1/stream?token=1").expect("valid IPv6 ws url");
    assert_eq!(parsed.host, "::1");
    assert_eq!(parsed.port, 8443);
    assert_eq!(parsed.path_and_query, "/v1/stream?token=1");
}
