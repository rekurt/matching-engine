/// C5: server_seq monotonicity ACROSS different frame types.
///
/// The server_seq invariant must hold not just for `encode_events` output,
/// but for EVERY frame the server writes during a connection's lifetime:
/// Pong, FlowControlWarning, OpenOrdersBegin/Body/End, SessionResumed, etc.
///
/// These tests push multiple different frame types into the seq-rewrite
/// path and verify each carries a distinct, monotonic seq that matches the
/// replay buffer.
use bytes::{Bytes, BytesMut};
use std::time::Duration;

use me_io::codec;
use me_io::protocol::FRAME_HEADER_SIZE;
use me_io::session_store::{ReplayResult, SessionStore};

/// Extract the seq field (header offset 6..14 LE) from a frame.
fn seq_of(frame: &[u8]) -> u64 {
    u64::from_le_bytes(frame[6..14].try_into().unwrap())
}

/// Extract msg_type (header offset 5) from a frame.
fn msg_type_of(frame: &[u8]) -> u8 {
    frame[5]
}

/// Walk an in-memory byte stream, one frame at a time (respecting payload_len).
fn iter_frames(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut off = 0;
    while off + FRAME_HEADER_SIZE <= buf.len() {
        let payload_len =
            u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let total = FRAME_HEADER_SIZE + payload_len;
        if off + total > buf.len() {
            break;
        }
        out.push(&buf[off..off + total]);
        off += total;
    }
    out
}

#[test]
fn test_mixed_frame_types_get_monotonic_seqs_when_rewritten() {
    // Pack Pong + FlowControlWarning + SessionResumed into one block,
    // rewrite seqs starting at 100 — each should get 100, 101, 102.
    let mut buf = BytesMut::new();
    codec::encode_pong(0, 999, &mut buf); // placeholder seq=0
    codec::encode_flow_control_warning(0, 85, false, &mut buf);
    codec::encode_session_resumed(5, 0, &mut buf);

    // Rewrite via session_store.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();
    let mut block: Vec<u8> = buf.to_vec();
    let count = store.push_frames_with_seq_rewrite(&sid, 100, &mut block);
    assert_eq!(count, 3, "3 frames must be parsed + assigned seqs");

    // Verify each frame now has the expected seq written in its header.
    let frames = iter_frames(&block);
    assert_eq!(frames.len(), 3);
    assert_eq!(seq_of(frames[0]), 100);
    assert_eq!(seq_of(frames[1]), 101);
    assert_eq!(seq_of(frames[2]), 102);

    // Msg types preserved.
    assert_eq!(msg_type_of(frames[0]), 0x18, "Pong"); // MessageType::Pong
    assert_eq!(msg_type_of(frames[1]), 0x23, "FlowControlWarning");
    assert_eq!(msg_type_of(frames[2]), 0x1B, "SessionResumed");

    // Buffer replay returns these frames starting AFTER seq 99.
    match store.resume(&sid, 99) {
        ReplayResult::Ok(replayed) => {
            assert_eq!(replayed.len(), 3);
            assert_eq!(seq_of(&replayed[0]), 100);
            assert_eq!(seq_of(&replayed[2]), 102);
        }
        other => panic!("expected Ok replay, got {other:?}"),
    }
}

#[test]
fn test_seq_continues_monotonically_across_multiple_pushes() {
    // Simulate two write-buf flushes: first pushes 3 frames at seqs 1-3,
    // then another flush pushes 2 frames at seqs 4-5. Replay should return
    // all 5 in order.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    let mut buf1 = BytesMut::new();
    codec::encode_pong(0, 10, &mut buf1);
    codec::encode_pong(0, 11, &mut buf1);
    codec::encode_pong(0, 12, &mut buf1);
    let mut block1: Vec<u8> = buf1.to_vec();
    let n1 = store.push_frames_with_seq_rewrite(&sid, 1, &mut block1);
    assert_eq!(n1, 3);

    let mut buf2 = BytesMut::new();
    codec::encode_session_resumed(100, 0, &mut buf2);
    codec::encode_replay_gap(0, 1, 10, &mut buf2);
    let mut block2: Vec<u8> = buf2.to_vec();
    let n2 = store.push_frames_with_seq_rewrite(&sid, 4, &mut block2);
    assert_eq!(n2, 2);

    // Replay from 0 gets all 5, seqs 1..=5.
    match store.resume(&sid, 0) {
        ReplayResult::Ok(frames) => {
            assert_eq!(frames.len(), 5);
            for (i, f) in frames.iter().enumerate() {
                assert_eq!(seq_of(f), (i + 1) as u64);
            }
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn test_seq_in_wire_bytes_matches_buffer_stored_seq() {
    // CRITICAL: the header.seq bytes in the MUTATED wire block MUST match
    // the seq stored as the map key in the replay buffer. Otherwise a client
    // ACKing a seq it saw on the wire wouldn't match what the server stored.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    let mut buf = BytesMut::new();
    codec::encode_flow_control_warning(0, 50, false, &mut buf);
    codec::encode_pong(0, 777, &mut buf);
    let mut block: Vec<u8> = buf.to_vec();
    store.push_frames_with_seq_rewrite(&sid, 42, &mut block);

    // Read seqs from the rewritten wire block.
    let wire_frames = iter_frames(&block);
    let wire_seqs: Vec<u64> = wire_frames.iter().map(|f| seq_of(f)).collect();

    // Read seqs from the replay buffer.
    let replayed = match store.resume(&sid, 0) {
        ReplayResult::Ok(f) => f,
        other => panic!("expected Ok, got {other:?}"),
    };
    let replay_seqs: Vec<u64> = replayed.iter().map(|f| seq_of(f)).collect();

    assert_eq!(
        wire_seqs, replay_seqs,
        "wire-side seqs MUST match replay-buffer seqs"
    );

    // And both must equal the expected sequence.
    assert_eq!(wire_seqs, vec![42, 43]);
}

#[test]
fn test_truncated_final_frame_is_not_assigned_a_seq() {
    // If the caller pushes a block ending with a half-written frame (shouldn't
    // happen but we're defensive), the parser must stop at the truncation
    // boundary and NOT assign a seq to it. Otherwise a subsequent correctly-
    // completed frame would collide.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();

    let mut buf = BytesMut::new();
    codec::encode_pong(0, 1, &mut buf);
    codec::encode_pong(0, 2, &mut buf);
    // Truncate last 3 bytes of the 2nd frame.
    let mut block: Vec<u8> = buf.to_vec();
    block.truncate(block.len() - 3);

    let count = store.push_frames_with_seq_rewrite(&sid, 50, &mut block);
    assert_eq!(count, 1, "only the complete 1st frame gets a seq");

    match store.resume(&sid, 0) {
        ReplayResult::Ok(frames) => {
            assert_eq!(frames.len(), 1);
            assert_eq!(seq_of(&frames[0]), 50);
        }
        other => panic!("expected Ok, got {other:?}"),
    }
}

#[test]
fn test_empty_block_returns_zero_without_errors() {
    // Defensive: pushing an empty block is a no-op.
    let store = SessionStore::new(1024, Duration::from_secs(60));
    let sid = store.create();
    let mut empty: Vec<u8> = Vec::new();
    let count = store.push_frames_with_seq_rewrite(&sid, 999, &mut empty);
    assert_eq!(count, 0);
    assert_eq!(store.buffer_len(&sid), 0);
}

#[test]
fn test_push_to_expired_session_logs_and_drops() {
    // If the session has been GC'd between encode and push, frames are
    // silently dropped (logged via tracing::warn!). The wire frames still
    // went out, but they're not replayable. Verify count=0.
    let store = SessionStore::new(1024, Duration::from_millis(10));
    let sid = store.create();

    // Expire it.
    std::thread::sleep(Duration::from_millis(30));
    store.gc();

    let mut buf = BytesMut::new();
    codec::encode_pong(0, 1, &mut buf);
    let mut block: Vec<u8> = buf.to_vec();

    let count = store.push_frames_with_seq_rewrite(&sid, 100, &mut block);
    assert_eq!(count, 0, "expired session: no frames stored");
}

// Helper: build a well-formed wire frame to stuff into the rewriter.
#[allow(dead_code)]
fn make_simple_frame() -> Bytes {
    let mut buf = BytesMut::new();
    codec::encode_pong(0, 0, &mut buf);
    buf.freeze()
}
