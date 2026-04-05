/// S8: TLV iterator — Result-based error surfacing.
///
/// These tests verify that `tlv_iter` distinguishes:
/// - clean end of stream (None)
/// - partial header mid-stream (Some(Err(TruncatedHeader)))
/// - value len exceeding remaining data (Some(Err(TruncatedValue)))
use bytes::BytesMut;
use me_io::codec::{tlv_iter, write_tlv, TlvError};

#[test]
fn tlv_iter_reads_multiple_records_cleanly() {
    let mut buf = BytesMut::new();
    write_tlv(&mut buf, 0x0001, &[0xAA, 0xBB]);
    write_tlv(&mut buf, 0x0002, &[0x11, 0x22, 0x33]);
    write_tlv(&mut buf, 0x0003, b""); // zero-length value is legal

    let items: Vec<_> = tlv_iter(&buf).collect();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].as_ref().unwrap(), &(0x0001u16, &[0xAA, 0xBB][..]));
    assert_eq!(
        items[1].as_ref().unwrap(),
        &(0x0002u16, &[0x11, 0x22, 0x33][..])
    );
    assert_eq!(items[2].as_ref().unwrap(), &(0x0003u16, &[][..]));
}

#[test]
fn tlv_iter_empty_buffer_is_clean_end() {
    let empty: &[u8] = &[];
    let items: Vec<_> = tlv_iter(empty).collect();
    assert!(items.is_empty(), "empty data → no items, no errors");
}

#[test]
fn tlv_iter_reports_truncated_header() {
    // 2-byte buffer: mid-header corruption.
    let partial: &[u8] = &[0x01, 0x00];
    let items: Vec<_> = tlv_iter(partial).collect();
    assert_eq!(items.len(), 1);
    match &items[0] {
        Err(TlvError::TruncatedHeader { remaining: 2 }) => {}
        other => panic!("expected TruncatedHeader{{remaining:2}}, got {other:?}"),
    }
}

#[test]
fn tlv_iter_reports_truncated_value() {
    // Build: tag=0xABCD, len=10, but only 3 bytes follow.
    let mut buf = Vec::new();
    buf.extend_from_slice(&0xABCDu16.to_le_bytes());
    buf.extend_from_slice(&10u16.to_le_bytes());
    buf.extend_from_slice(&[0xDE, 0xAD, 0xBE]); // truncated: only 3 of 10

    let items: Vec<_> = tlv_iter(&buf).collect();
    assert_eq!(items.len(), 1);
    match &items[0] {
        Err(TlvError::TruncatedValue {
            tag: 0xABCD,
            expected: 10,
            actual: 3,
        }) => {}
        other => panic!("expected TruncatedValue, got {other:?}"),
    }
}

#[test]
fn tlv_iter_stops_after_error() {
    // A valid TLV followed by garbage that triggers an error — the iterator
    // must yield the first record successfully, then Err, then None.
    let mut buf = BytesMut::new();
    write_tlv(&mut buf, 0x0001, &[0xAA]);
    // Append a broken trailing byte.
    buf.extend_from_slice(&[0x42]);

    let items: Vec<_> = tlv_iter(&buf).collect();
    assert_eq!(items.len(), 2);
    assert!(items[0].is_ok(), "first TLV decodes cleanly");
    assert!(items[1].is_err(), "trailing garbage surfaces as error");
}

#[test]
fn tlv_iter_filter_ok_ignores_corruption() {
    // Consumers who only want well-formed records can `filter_map(Result::ok)`.
    let mut buf = BytesMut::new();
    write_tlv(&mut buf, 0x0001, &[0xAA]);
    // Append broken 3-byte header.
    buf.extend_from_slice(&[0x42, 0x43, 0x44]);

    let good: Vec<_> = tlv_iter(&buf).filter_map(Result::ok).collect();
    assert_eq!(good.len(), 1);
    assert_eq!(good[0], (0x0001u16, &[0xAA][..]));
}
