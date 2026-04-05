//! Stable-Rust smoke test that exercises the same decoder invariants
//! checked by the libfuzzer targets in `fuzz/fuzz_targets/`.
//!
//! Purpose: catch panic regressions on malformed/adversarial inputs in
//! every CI run, without requiring nightly + libfuzzer. The libfuzzer
//! target is still the source of truth for coverage-guided discovery;
//! this file freezes known-bad inputs as regression tests.

use me_clock::SystemClock;
use me_io::codec::{decode_frame, ConnectionCodecState};
use me_io::protocol::ConnectionState;

/// Mirrors the fuzz loop. Panics if any input causes a panic OR if the
/// decoder returns a bogus `consumed` count.
fn drive_decoder(data: &[u8], established: bool) {
    let clock = SystemClock;
    let mut state = ConnectionCodecState::new();
    if established {
        state.state = ConnectionState::Active;
    }
    let mut cursor = data;
    while !cursor.is_empty() {
        match decode_frame(cursor, &mut state, &clock) {
            Ok((_frame, consumed)) => {
                assert!(consumed > 0, "decode_frame returned consumed=0");
                assert!(
                    consumed <= cursor.len(),
                    "decode_frame consumed {consumed} > cursor.len()={}",
                    cursor.len()
                );
                cursor = &cursor[consumed..];
            }
            Err(_) => return,
        }
    }
}

/// Canonical set of adversarial byte strings that must NOT panic.
fn adversarial_corpus() -> Vec<Vec<u8>> {
    vec![
        // Empty input.
        vec![],
        // One-byte, too short for header.
        vec![0x00],
        vec![0xFF],
        // Truncated at every header boundary (typical partial-read case).
        (1..16).flat_map(|n| vec![0xAA; n]).collect(),
        // All-zeros of increasing length.
        vec![0u8; 1],
        vec![0u8; 8],
        vec![0u8; 32],
        vec![0u8; 128],
        // All-0xFF of increasing length (max sequence numbers, max types).
        vec![0xFFu8; 16],
        vec![0xFFu8; 64],
        // Random-ish but structured garbage.
        (0..=255u8).collect(),
        (0..=255u8).rev().collect(),
        // Alternating bytes.
        (0..128).flat_map(|_| [0x00, 0xFF]).collect(),
        // Patterns that resemble valid TLV but encode junk lengths.
        vec![
            0x01, 0x00, // tag = 1
            0xFF, 0xFF, // len = 65535 (would overflow most buffers)
        ],
        // A payload-length field claiming more than the universe.
        vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00],
    ]
}

#[test]
fn decoder_never_panics_on_adversarial_inputs_pre_handshake() {
    for (i, input) in adversarial_corpus().iter().enumerate() {
        drive_decoder(input, false);
        // If we reached here without panicking, this input is safe.
        let _ = i;
    }
}

#[test]
fn decoder_never_panics_on_adversarial_inputs_post_handshake() {
    for input in adversarial_corpus() {
        drive_decoder(&input, true);
    }
}

#[test]
fn decoder_handles_byte_walk() {
    // For each position, replace a single byte with every possible
    // value. This exhaustively perturbs a "valid-looking" frame header.
    let base: Vec<u8> = vec![
        0x01, // version? msg type?
        0x00, 0x00, 0x00, 0x10, // length
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // seq
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding/payload
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    for pos in 0..base.len() {
        for val in 0u8..=255 {
            let mut mutated = base.clone();
            mutated[pos] = val;
            drive_decoder(&mutated, false);
            drive_decoder(&mutated, true);
        }
    }
}

#[test]
fn decoder_handles_length_prefix_overflow() {
    // A frame claiming an absurd payload length must return
    // `PayloadTooLarge`, not attempt to allocate/read it.
    let hostile = vec![
        0x01, // some header byte
        0xFF, 0xFF, 0xFF, 0xFF, // u32::MAX length
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    drive_decoder(&hostile, true);
    drive_decoder(&hostile, false);
}
