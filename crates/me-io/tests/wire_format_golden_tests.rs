/// C2: Wire-format golden bytes stability tests.
///
/// These tests FREEZE the on-wire binary layout of protocol v2 frames. Any
/// accidental format drift — byte reordering, size change, constant shift —
/// will fail these tests loudly. Clients compiled against one version must
/// remain compatible with a server compiled against the same version.
///
/// Header layout (22 bytes, all LE):
///   offset  0..4   u32  payload_len
///   offset  4      u8   version (= 2)
///   offset  5      u8   msg_type
///   offset  6..14  u64  seq
///   offset 14..22  u64  request_id
///
/// Any change here is a breaking wire-protocol change and MUST bump
/// PROTOCOL_VERSION. If you're staring at a failing test below and tempted
/// to update the golden bytes: STOP and decide — is this intentional?
use bytes::BytesMut;
use me_io::codec;

/// Produces the concrete byte sequence for a given encode function. Prints
/// it as a readable hex stream if assertion fails, so debugging is easy.
fn assert_bytes_eq(actual: &[u8], expected: &[u8], label: &str) {
    if actual != expected {
        let fmt = |bs: &[u8]| {
            bs.iter()
                .enumerate()
                .map(|(i, b)| {
                    if i > 0 && i % 4 == 0 {
                        format!(" {b:02x}")
                    } else {
                        format!("{b:02x}")
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        };
        panic!(
            "{label} wire bytes drifted!\n  expected: {}\n  actual:   {}",
            fmt(expected),
            fmt(actual)
        );
    }
}

#[test]
fn golden_pong_frame_bytes() {
    // Pong: header(22) + last_processed_client_seq(8) = 30 bytes total
    // payload_len = 8
    //
    // Inputs: seq=0x0102030405060708, last_processed_client_seq=0x11223344_55667788
    let mut buf = BytesMut::new();
    codec::encode_pong(0x01020304_05060708, 0x11223344_55667788, &mut buf);

    let expected: &[u8] = &[
        // --- header (22 bytes) ---
        0x08, 0x00, 0x00, 0x00, // payload_len = 8
        0x02, // version = 2
        0x18, // msg_type = Pong (0x18)
        0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // seq LE
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // request_id = 0 (server→client)
        // --- payload: last_processed_client_seq LE ---
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11,
    ];

    assert_bytes_eq(&buf[..], expected, "Pong");
    assert_eq!(buf.len(), 30, "Pong frame must be exactly 30 bytes");
}

#[test]
fn golden_session_resumed_frame_bytes() {
    // SessionResumed: header(22) + replayed_count(4) = 26 bytes total
    let mut buf = BytesMut::new();
    codec::encode_session_resumed(7, 42, &mut buf);

    let expected: &[u8] = &[
        // header
        0x04, 0x00, 0x00, 0x00, // payload_len = 4
        0x02, // version
        0x1B, // msg_type = SessionResumed
        0x2A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seq = 42
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // request_id = 0
        // payload: replayed_count = 7
        0x07, 0x00, 0x00, 0x00,
    ];

    assert_bytes_eq(&buf[..], expected, "SessionResumed");
    assert_eq!(buf.len(), 26);
}

#[test]
fn golden_replay_gap_frame_bytes() {
    // ReplayGap: header(22) + missing_from(8) + missing_to(8) = 38 bytes
    let mut buf = BytesMut::new();
    codec::encode_replay_gap(100, 10, 25, &mut buf);

    let expected: &[u8] = &[
        // header
        0x10, 0x00, 0x00, 0x00, // payload_len = 16
        0x02, // version
        0x20, // msg_type = ReplayGap
        0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seq = 100
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // request_id = 0
        // payload: missing_from, missing_to
        0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 10
        0x19, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 25
    ];

    assert_bytes_eq(&buf[..], expected, "ReplayGap");
    assert_eq!(buf.len(), 38);
}

#[test]
fn golden_flow_control_warning_soft_bytes() {
    // FlowControlWarning: header(22) + buffer_used_pct(1) + is_hard_limit(1) = 24 bytes
    let mut buf = BytesMut::new();
    codec::encode_flow_control_warning(99, 85, false, &mut buf);

    let expected: &[u8] = &[
        // header
        0x02, 0x00, 0x00, 0x00, // payload_len = 2
        0x02, // version
        0x23, // msg_type = FlowControlWarning
        0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seq = 99
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // request_id = 0
        // payload: 85% usage, soft limit (is_hard = 0)
        0x55, 0x00,
    ];
    assert_bytes_eq(&buf[..], expected, "FlowControlWarning/soft");
    assert_eq!(buf.len(), 24);
}

#[test]
fn golden_flow_control_warning_hard_bytes() {
    let mut buf = BytesMut::new();
    codec::encode_flow_control_warning(1, 100, true, &mut buf);

    let expected: &[u8] = &[
        0x02, 0x00, 0x00, 0x00, 0x02, 0x23, //
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seq = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // request_id = 0
        0x64, 0x01, // 100%, hard = 1
    ];
    assert_bytes_eq(&buf[..], expected, "FlowControlWarning/hard");
}

#[test]
fn golden_error_frame_bytes() {
    // Error: header(22) + error_code(2) + message_len(2) + message = variable
    use me_io::protocol::ErrorCode;
    let mut buf = BytesMut::new();
    codec::encode_error(5, ErrorCode::HeartbeatTimeout, "oops", &mut buf);

    // payload_len = 2 + 2 + 4 = 8
    // ErrorCode::HeartbeatTimeout value depends on enum numbering; read it dynamically.
    let err_code = ErrorCode::HeartbeatTimeout as u16;
    let mut expected = vec![
        0x08, 0x00, 0x00, 0x00, // payload_len = 8
        0x02, // version
        0x19, // Error
        0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // seq = 5
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // request_id = 0
    ];
    expected.extend_from_slice(&err_code.to_le_bytes());
    expected.extend_from_slice(&(4u16.to_le_bytes())); // message_len
    expected.extend_from_slice(b"oops");

    assert_bytes_eq(&buf[..], &expected, "Error");
    assert_eq!(buf.len(), 30);
}

#[test]
fn golden_header_constants() {
    // FRAME_HEADER_SIZE is load-bearing — it's the offset where payload parsing
    // starts. If this changes accidentally, every decoder in every client
    // breaks. PROTOCOL_VERSION is the compatibility knob.
    use me_io::protocol::{FRAME_HEADER_SIZE, PROTOCOL_VERSION};
    assert_eq!(FRAME_HEADER_SIZE, 22, "frame header must stay at 22 bytes");
    assert_eq!(PROTOCOL_VERSION, 2, "protocol version must stay at 2");
}
