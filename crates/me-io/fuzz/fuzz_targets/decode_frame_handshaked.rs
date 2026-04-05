//! Fuzz target for decoder paths gated on a completed handshake.
//!
//! Starting state: `ConnectionState::Active`, so commands
//! (PlaceOrder, Cancel, Amend, Batch, QueryOrder, …) are reachable.
//!
//! Run locally: `cargo +nightly fuzz run decode_frame_handshaked`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use me_clock::SystemClock;
use me_io::codec::{decode_frame, ConnectionCodecState};
use me_io::protocol::ConnectionState;

fuzz_target!(|data: &[u8]| {
    let clock = SystemClock;
    let mut state = ConnectionCodecState::new();
    state.state = ConnectionState::Active;
    state.session_id = [0xAB; 16];

    let mut cursor = data;
    while !cursor.is_empty() {
        match decode_frame(cursor, &mut state, &clock) {
            Ok((_frame, consumed)) => {
                if consumed == 0 || consumed > cursor.len() {
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
