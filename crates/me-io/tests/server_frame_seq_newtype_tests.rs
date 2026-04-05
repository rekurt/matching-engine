/// RFC-0001 Task B1: ServerFrameSeq newtype tests.
///
/// The invariant being encoded: seq=0 is the "unsent" sentinel in the
/// replay pipeline; the first assignable seq is 1. Making seq=0
/// unrepresentable in the type prevents the whole class of bugs where
/// a caller accidentally resets the counter.
use me_io::session_store::ServerFrameSeq;

#[test]
fn first_is_one_not_zero() {
    let seq = ServerFrameSeq::FIRST;
    assert_eq!(
        seq.peek(),
        1,
        "FIRST is 1 — seq=0 is reserved as the 'unsent' sentinel"
    );
}

#[test]
fn next_returns_current_then_advances() {
    let mut seq = ServerFrameSeq::FIRST;
    assert_eq!(seq.next(), 1, "first next() returns 1");
    assert_eq!(seq.next(), 2, "second next() returns 2");
    assert_eq!(seq.next(), 3, "third next() returns 3");
    assert_eq!(seq.peek(), 4, "peek shows next-to-assign");
}

#[test]
fn advance_by_skips_forward() {
    let mut seq = ServerFrameSeq::FIRST;
    seq.advance_by(5);
    assert_eq!(seq.peek(), 6, "FIRST + 5 = 6 next to assign");
}

#[test]
fn advance_by_zero_is_noop() {
    let mut seq = ServerFrameSeq::FIRST;
    seq.advance_by(0);
    assert_eq!(seq.peek(), 1);
}

#[test]
fn reset_to_reseeds_counter() {
    // After ResumeSession, handler calls reset_to() with the value
    // returned by SessionStore::next_outbound_seq(). This is the ONLY
    // way to rewrite the counter from outside.
    let mut seq = ServerFrameSeq::FIRST;
    seq.next(); // advance to 2
    seq.next(); // advance to 3
    seq.reset_to(100);
    assert_eq!(seq.peek(), 100, "reset_to replaces the counter");
    assert_eq!(seq.next(), 100);
    assert_eq!(seq.next(), 101);
}

#[test]
fn default_equals_first() {
    let default = ServerFrameSeq::default();
    assert_eq!(
        default.peek(),
        ServerFrameSeq::FIRST.peek(),
        "default() must behave like FIRST (seq=1)"
    );
}

#[test]
fn debug_format_shows_the_seq() {
    let mut seq = ServerFrameSeq::FIRST;
    seq.next();
    seq.next();
    let dbg = format!("{seq:?}");
    assert!(
        dbg.contains('3'),
        "Debug must show next-to-assign ({}), got: {dbg}",
        3
    );
}
