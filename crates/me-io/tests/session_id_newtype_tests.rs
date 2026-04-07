/// RFC-0001 Task A1: SessionId newtype tests.
///
/// These tests drive the newtype API — they describe WHAT a caller should
/// be able to do with a SessionId, before any implementation exists.
use me_io::session_store::SessionId;

#[test]
fn none_sentinel_is_all_zero_bytes() {
    let none = SessionId::NONE;
    assert!(none.is_none(), "NONE.is_none() must return true");
    assert_eq!(none.as_bytes(), &[0u8; 16], "NONE maps to 16 zero bytes");
}

#[test]
fn random_sessions_are_not_none() {
    // The probability of random() returning all-zeros is 2^-128 — trust it.
    let sid = SessionId::random();
    assert!(!sid.is_none(), "random() MUST NOT produce NONE sentinel");
}

#[test]
fn random_sessions_are_distinct() {
    // Two independent draws must differ (probabilistic, but 2^-128 collision).
    let a = SessionId::random();
    let b = SessionId::random();
    assert_ne!(a, b, "two independent random() calls must differ");
}

#[test]
fn log_prefix_is_8_char_hex_of_first_4_bytes() {
    // Construct a known SessionId with a distinctive prefix.
    let sid = SessionId::from_bytes([0xAB, 0xCD, 0xEF, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        sid.log_prefix(),
        "abcdef01",
        "log_prefix must be 8 lowercase hex chars of first 4 bytes"
    );
}

#[test]
fn log_prefix_handles_zero_bytes() {
    // NONE sentinel's prefix must be all-zeros hex.
    assert_eq!(SessionId::NONE.log_prefix(), "00000000");
}

#[test]
fn as_bytes_returns_the_inner_array_by_reference() {
    let bytes = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
    let sid = SessionId::from_bytes(bytes);
    assert_eq!(sid.as_bytes(), &bytes);
}

#[test]
fn from_bytes_and_as_bytes_round_trip() {
    let sid = SessionId::random();
    let reconstructed = SessionId::from_bytes(*sid.as_bytes());
    assert_eq!(sid, reconstructed);
}

#[test]
fn session_id_is_copy_and_hashable() {
    // Copy is required for field access ergonomics.
    // Hash is required for use as a HashMap key (the whole sharded store).
    fn requires_copy<T: Copy>() {}
    fn requires_hash<T: std::hash::Hash>() {}
    fn requires_eq<T: Eq>() {}
    requires_copy::<SessionId>();
    requires_hash::<SessionId>();
    requires_eq::<SessionId>();
}

#[test]
fn debug_format_is_readable_hex() {
    let sid = SessionId::from_bytes([0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let dbg = format!("{sid:?}");
    // Debug format should at least contain the log-prefix so it's
    // recognizable in traces; full format can be implementation-defined.
    assert!(
        dbg.contains("deadbeef"),
        "Debug format must include log_prefix ('deadbeef'), got: {dbg}"
    );
}
