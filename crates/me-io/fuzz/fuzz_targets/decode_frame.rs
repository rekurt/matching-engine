//! Fuzz target for the wire protocol decoder — handshake-pending state.
//!
//! Invariant under test: `decode_frame` MUST NEVER panic on arbitrary
//! input. It may return `Err(DecodeError::*)` or succeed, but
//! malformed/adversarial bytes must be handled as data, not as a bug.
//!
//! Run locally: `cargo +nightly fuzz run decode_frame` (from this dir).

#![no_main]

use libfuzzer_sys::fuzz_target;
use me_clock::SystemClock;
use me_io::codec::{decode_frame, ConnectionCodecState};

fuzz_target!(|data: &[u8]| {
    let clock = SystemClock;
    let mut state = ConnectionCodecState::new();
    // Call the decoder until the buffer is exhausted or an error
    // surfaces. This mirrors the loop in the real TCP/UDS servers.
    let mut cursor = data;
    while !cursor.is_empty() {
        match decode_frame(cursor, &mut state, &clock) {
            Ok((_frame, consumed)) => {
                if consumed == 0 || consumed > cursor.len() {
                    // Protocol violation: a successful decode must
                    // always consume strictly positive, non-overflowing
                    // bytes. A real bug would surface here.
                    panic!(
                        "decode_frame returned bogus consumed={} for cursor.len()={}",
                        consumed,
                        cursor.len()
                    );
                }
                cursor = &cursor[consumed..];
            }
            Err(_) => break,
        }
    }
});
