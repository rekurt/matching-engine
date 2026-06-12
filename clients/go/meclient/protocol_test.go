package meclient

import (
	"bytes"
	"encoding/binary"
	"testing"
)

// ─── Encode payload tests ───────────────────────────────────────────────────

func TestEncodePlaceOrderPayload(t *testing.T) {
	req := &PlaceOrderRequest{
		OrderId: 42, SymbolId: 5, Side: SideSell,
		OrderType: OrderTypeLimit, TimeInForce: TimeInForceIOC,
		Price: NewDecimal(50000.50), StopPrice: NewDecimalRaw(0), Qty: NewDecimal(1.5),
	}
	payload := EncodePlaceOrder(req)
	if len(payload) != 37 {
		t.Fatalf("expected 37, got %d", len(payload))
	}
	if binary.LittleEndian.Uint64(payload[0:8]) != 42 {
		t.Fatal("OrderId")
	}
	if binary.LittleEndian.Uint16(payload[8:10]) != 5 {
		t.Fatal("SymbolId")
	}
	if payload[10] != byte(SideSell) {
		t.Fatal("Side")
	}
	if payload[11] != byte(OrderTypeLimit) {
		t.Fatal("OrderType")
	}
	if payload[12] != byte(TimeInForceIOC) {
		t.Fatal("TIF")
	}
}

func TestEncodePlaceOrderAdvancedFieldsRoundtrip(t *testing.T) {
	// GTD only → 46-byte payload.
	gtd := &PlaceOrderRequest{
		OrderId: 1, SymbolId: 1, Side: SideBuy, OrderType: OrderTypeLimit,
		TimeInForce: TimeInForceGTD, Price: NewDecimal(100.0), Qty: NewDecimal(1.0),
		ExpireAt: 1_700_000_000_000_000_000,
	}
	if got := len(EncodePlaceOrder(gtd)); got != placeOrderSizeExpire {
		t.Fatalf("GTD payload size = %d, want %d", got, placeOrderSizeExpire)
	}
	dec, err := DecodePlaceOrder(EncodePlaceOrder(gtd))
	if err != nil {
		t.Fatal(err)
	}
	if dec.TimeInForce != TimeInForceGTD || dec.ExpireAt != gtd.ExpireAt {
		t.Fatalf("GTD roundtrip mismatch: %+v", dec)
	}

	// Iceberg → 62-byte payload, all advanced fields roundtrip.
	ice := &PlaceOrderRequest{
		OrderId: 2, SymbolId: 1, Side: SideSell, OrderType: OrderTypeLimit,
		TimeInForce: TimeInForceGTC, Price: NewDecimal(100.0), Qty: NewDecimal(10.0),
		Flags: 0x01, DisplayQty: NewDecimal(2.0),
	}
	if got := len(EncodePlaceOrder(ice)); got != placeOrderSizeDisplay {
		t.Fatalf("iceberg payload size = %d, want %d", got, placeOrderSizeDisplay)
	}
	dec, err = DecodePlaceOrder(EncodePlaceOrder(ice))
	if err != nil {
		t.Fatal(err)
	}
	if dec.Flags != 0x01 || dec.DisplayQty.RawUint64() != ice.DisplayQty.RawUint64() {
		t.Fatalf("iceberg roundtrip mismatch: %+v", dec)
	}

	// Trailing stop → 54-byte payload.
	trail := &PlaceOrderRequest{
		OrderId: 3, SymbolId: 1, Side: SideSell, OrderType: OrderTypeStopMarket,
		TimeInForce: TimeInForceGTC, StopPrice: NewDecimal(95.0), Qty: NewDecimal(1.0),
		TrailOffset: NewDecimal(5.0),
	}
	if got := len(EncodePlaceOrder(trail)); got != placeOrderSizeTrail {
		t.Fatalf("trailing payload size = %d, want %d", got, placeOrderSizeTrail)
	}
	dec, err = DecodePlaceOrder(EncodePlaceOrder(trail))
	if err != nil {
		t.Fatal(err)
	}
	if dec.TrailOffset.Raw() != trail.TrailOffset.Raw() {
		t.Fatalf("trailing roundtrip mismatch: %+v", dec)
	}

	// Plain order stays 37 bytes (backward compatible).
	plain := &PlaceOrderRequest{
		OrderId: 4, SymbolId: 1, Side: SideBuy, OrderType: OrderTypeLimit,
		TimeInForce: TimeInForceGTC, Price: NewDecimal(100.0), Qty: NewDecimal(1.0),
	}
	if got := len(EncodePlaceOrder(plain)); got != placeOrderPayloadSize {
		t.Fatalf("plain payload size = %d, want %d", got, placeOrderPayloadSize)
	}
}

func TestEncodeCancelOrderPayload(t *testing.T) {
	req := &CancelOrderRequest{OrderId: 99, SymbolId: 3}
	payload := EncodeCancelOrder(req)
	if len(payload) != 10 {
		t.Fatalf("expected 10, got %d", len(payload))
	}
	if binary.LittleEndian.Uint64(payload[0:8]) != 99 {
		t.Fatal("OrderId")
	}
	if binary.LittleEndian.Uint16(payload[8:10]) != 3 {
		t.Fatal("SymbolId")
	}
}

func TestEncodeAmendBothFields(t *testing.T) {
	price := NewDecimal(100.0)
	qty := NewDecimal(2.0)
	req := &AmendOrderRequest{OrderId: 7, SymbolId: 1, Price: &price, Qty: &qty}
	payload := EncodeAmendOrder(req)
	// 8+2+1+8+1+8 = 28
	if len(payload) != 28 {
		t.Fatalf("expected 28, got %d", len(payload))
	}
}

func TestEncodeAmendPriceOnly(t *testing.T) {
	price := NewDecimal(100.0)
	req := &AmendOrderRequest{OrderId: 7, SymbolId: 1, Price: &price}
	payload := EncodeAmendOrder(req)
	// 8+2+1+8+1 = 20
	if len(payload) != 20 {
		t.Fatalf("expected 20, got %d", len(payload))
	}
}

func TestEncodeAmendNoFields(t *testing.T) {
	req := &AmendOrderRequest{OrderId: 7, SymbolId: 1}
	payload := EncodeAmendOrder(req)
	// 8+2+1+1 = 12
	if len(payload) != 12 {
		t.Fatalf("expected 12, got %d", len(payload))
	}
}

// ─── EncodeFrame wrapping ───────────────────────────────────────────────────

func TestEncodeFrame(t *testing.T) {
	payload := EncodePlaceOrder(&PlaceOrderRequest{OrderId: 1, Qty: NewDecimal(1.0)})
	frame := EncodeFrame(0x01, 100, 7, payload)
	// Frame: 4 (len) + 1 (ver) + 1 (type) + 8 (seq) + 8 (reqID) + 37 (payload) = 59
	if len(frame) != 59 {
		t.Fatalf("expected 59, got %d", len(frame))
	}
	// len field covers payload only (37 bytes)
	payloadLen := binary.LittleEndian.Uint32(frame[0:4])
	if payloadLen != 37 {
		t.Fatalf("expected payload len 37, got %d", payloadLen)
	}
	if frame[4] != ProtocolVersion {
		t.Fatalf("version: got %d", frame[4])
	}
	if frame[4] != 2 {
		t.Fatalf("v2 version byte expected 2, got %d", frame[4])
	}
	if frame[5] != 0x01 {
		t.Fatal("type")
	}
	seq := binary.LittleEndian.Uint64(frame[6:14])
	if seq != 100 {
		t.Fatalf("seq: %d", seq)
	}
	reqID := binary.LittleEndian.Uint64(frame[14:22])
	if reqID != 7 {
		t.Fatalf("requestID: got %d, want 7", reqID)
	}
}

// ─── Decode outbound messages ───────────────────────────────────────────────

func mkFrame(msgType uint8, seq uint64, payload []byte) []byte {
	// Test outbound (server→client) frames; request_id 0 is fine for decoding.
	return EncodeFrame(msgType, seq, 0, payload)
}

func decodeFirst(t *testing.T, frame []byte) *Event {
	t.Helper()
	r := bytes.NewReader(frame)
	mt, _, _, pl, err := ReadFrame(r)
	if err != nil {
		t.Fatal(err)
	}
	ev, err := DecodeEvent(mt, pl)
	if err != nil {
		t.Fatal(err)
	}
	return ev
}

func TestDecodeOrderAccepted(t *testing.T) {
	var p [16]byte
	binary.LittleEndian.PutUint64(p[0:8], 42)
	binary.LittleEndian.PutUint64(p[8:16], 1000)
	ev := decodeFirst(t, mkFrame(0x10, 1, p[:]))
	if ev.OrderAccepted == nil || ev.OrderAccepted.OrderId != 42 {
		t.Fatal("wrong")
	}
}

func TestDecodeOrderRejected(t *testing.T) {
	var p [17]byte
	binary.LittleEndian.PutUint64(p[0:8], 10)
	p[8] = byte(RejectInvalidPrice)
	binary.LittleEndian.PutUint64(p[9:17], 2000)
	ev := decodeFirst(t, mkFrame(0x11, 2, p[:]))
	if ev.OrderRejected == nil {
		t.Fatal("nil")
	}
	if ev.OrderRejected.Reason != RejectInvalidPrice {
		t.Fatal("reason")
	}
}

func TestDecodeTrade(t *testing.T) {
	var p [59]byte
	binary.LittleEndian.PutUint64(p[0:8], 1)  // trade_id
	binary.LittleEndian.PutUint16(p[8:10], 0) // symbol
	binary.LittleEndian.PutUint64(p[10:18], uint64(NewDecimal(100.0).Raw()))
	binary.LittleEndian.PutUint64(p[18:26], uint64(NewDecimal(1.0).Raw()))
	binary.LittleEndian.PutUint64(p[26:34], 10) // maker
	binary.LittleEndian.PutUint64(p[34:42], 20) // taker
	p[42] = byte(SideSell)
	binary.LittleEndian.PutUint64(p[43:51], 3000) // timestamp
	binary.LittleEndian.PutUint64(p[51:59], 77)   // trade sequence
	ev := decodeFirst(t, mkFrame(0x12, 3, p[:]))
	if ev.Trade == nil {
		t.Fatal("nil")
	}
	if ev.Trade.MakerOrderId != 10 {
		t.Fatal("maker")
	}
	if ev.Trade.Sequence != 77 {
		t.Fatalf("sequence: got %d, want 77", ev.Trade.Sequence)
	}
}

func TestDecodeOrderFilled(t *testing.T) {
	var p [32]byte
	binary.LittleEndian.PutUint64(p[0:8], 5)
	binary.LittleEndian.PutUint64(p[8:16], 100_000_000)
	binary.LittleEndian.PutUint64(p[16:24], uint64(NewDecimal(50.0).Raw()))
	binary.LittleEndian.PutUint64(p[24:32], 4000)
	ev := decodeFirst(t, mkFrame(0x13, 4, p[:]))
	if ev.OrderFilled == nil {
		t.Fatal("nil")
	}
}

func TestDecodePartiallyFilled(t *testing.T) {
	var p [40]byte
	binary.LittleEndian.PutUint64(p[0:8], 6)
	binary.LittleEndian.PutUint64(p[8:16], 50_000_000)
	binary.LittleEndian.PutUint64(p[16:24], 50_000_000)
	binary.LittleEndian.PutUint64(p[24:32], uint64(NewDecimal(100.0).Raw()))
	binary.LittleEndian.PutUint64(p[32:40], 5000)
	ev := decodeFirst(t, mkFrame(0x14, 5, p[:]))
	if ev.OrderPartiallyFilled == nil {
		t.Fatal("nil")
	}
}

func TestDecodeOrderCancelled(t *testing.T) {
	var p [24]byte
	binary.LittleEndian.PutUint64(p[0:8], 7)
	binary.LittleEndian.PutUint64(p[8:16], 100_000_000)
	binary.LittleEndian.PutUint64(p[16:24], 6000)
	ev := decodeFirst(t, mkFrame(0x15, 6, p[:]))
	if ev.OrderCancelled == nil {
		t.Fatal("nil")
	}
}

func TestDecodeAmended(t *testing.T) {
	var p [32]byte
	binary.LittleEndian.PutUint64(p[0:8], 8)
	binary.LittleEndian.PutUint64(p[8:16], uint64(NewDecimal(200.0).Raw()))
	binary.LittleEndian.PutUint64(p[16:24], 300_000_000)
	binary.LittleEndian.PutUint64(p[24:32], 7000)
	ev := decodeFirst(t, mkFrame(0x16, 7, p[:]))
	if ev.OrderAmended == nil {
		t.Fatal("nil")
	}
}

func TestDecodeStopTriggered(t *testing.T) {
	var p [24]byte
	binary.LittleEndian.PutUint64(p[0:8], 9)
	binary.LittleEndian.PutUint64(p[8:16], uint64(NewDecimal(100.0).Raw()))
	binary.LittleEndian.PutUint64(p[16:24], 8000)
	ev := decodeFirst(t, mkFrame(0x17, 8, p[:]))
	if ev.StopTriggered == nil {
		t.Fatal("nil")
	}
}

// ─── Edge Cases ─────────────────────────────────────────────────────────────

func TestReadFrameIncomplete(t *testing.T) {
	// Only 5 bytes, but the v2 header needs 22 -- should fail with unexpected EOF.
	r := bytes.NewReader([]byte{5, 0, 0, 0, 1})
	_, _, _, _, err := ReadFrame(r)
	if err == nil {
		t.Fatal("expected error")
	}
}

func TestReadFrameIncompletePayload(t *testing.T) {
	// Valid 22-byte v2 header claiming 10-byte payload, but only 2 payload bytes.
	var hdr [24]byte
	binary.LittleEndian.PutUint32(hdr[0:4], 10) // payload len = 10
	hdr[4] = ProtocolVersion
	hdr[5] = MsgOrderAccepted
	binary.LittleEndian.PutUint64(hdr[6:14], 1)  // seq
	binary.LittleEndian.PutUint64(hdr[14:22], 0) // request id
	hdr[22] = 0xAA                               // partial payload
	hdr[23] = 0xBB
	r := bytes.NewReader(hdr[:])
	_, _, _, _, err := ReadFrame(r)
	if err == nil {
		t.Fatal("expected error for incomplete payload")
	}
}

func TestDecodeInvalidType(t *testing.T) {
	_, err := DecodeEvent(0xFF, make([]byte, 32))
	if err == nil {
		t.Fatal("expected error")
	}
}

func TestDecodeShortPayload(t *testing.T) {
	_, err := DecodeEvent(0x10, make([]byte, 8))
	if err == nil {
		t.Fatal("expected error")
	}
}

// ─── Connection management message tests ──────────────────────────────────

func TestEncodeHandshake(t *testing.T) {
	payload := EncodeHandshake()
	if len(payload) != 1 {
		t.Fatalf("expected 1, got %d", len(payload))
	}
	if payload[0] != ProtocolVersion {
		t.Fatalf("expected version %d, got %d", ProtocolVersion, payload[0])
	}
}

func TestDecodeHandshakeAck(t *testing.T) {
	ev := decodeFirst(t, mkFrame(MsgHandshakeAck, 1, []byte{ProtocolVersion}))
	if ev.HandshakeAck == nil {
		t.Fatal("nil")
	}
	if ev.HandshakeAck.ServerVersion != ProtocolVersion {
		t.Fatalf("server version: got %d, want %d", ev.HandshakeAck.ServerVersion, ProtocolVersion)
	}
}

func TestDecodePong(t *testing.T) {
	ev := decodeFirst(t, mkFrame(MsgPong, 42, nil))
	if ev.Pong == nil {
		t.Fatal("nil")
	}
}

func TestDecodeErrorFrame(t *testing.T) {
	// Build error payload: code(2) + msg_len(2) + message
	msg := "handshake required"
	payload := make([]byte, 4+len(msg))
	binary.LittleEndian.PutUint16(payload[0:2], ErrorCodeHandshakeRequired)
	binary.LittleEndian.PutUint16(payload[2:4], uint16(len(msg)))
	copy(payload[4:], msg)

	ev := decodeFirst(t, mkFrame(MsgError, 1, payload))
	if ev.Error == nil {
		t.Fatal("nil")
	}
	if ev.Error.Code != ErrorCodeHandshakeRequired {
		t.Fatalf("code: got %d, want %d", ev.Error.Code, ErrorCodeHandshakeRequired)
	}
	if ev.Error.Message != msg {
		t.Fatalf("message: got %q, want %q", ev.Error.Message, msg)
	}
}

func TestDecodeErrorFrameEmpty(t *testing.T) {
	// Error with zero-length message.
	payload := make([]byte, 4)
	binary.LittleEndian.PutUint16(payload[0:2], ErrorCodeBackpressure)
	binary.LittleEndian.PutUint16(payload[2:4], 0)

	ev := decodeFirst(t, mkFrame(MsgError, 1, payload))
	if ev.Error == nil {
		t.Fatal("nil")
	}
	if ev.Error.Code != ErrorCodeBackpressure {
		t.Fatal("code")
	}
	if ev.Error.Message != "" {
		t.Fatalf("message should be empty, got %q", ev.Error.Message)
	}
}

func TestEncodeFrameZeroPayload(t *testing.T) {
	// Ping has no payload and (as a connection-management frame) request_id 0.
	frame := EncodeFrame(MsgPing, 5, 0, nil)
	// 22 header + 0 payload = 22
	if len(frame) != 22 {
		t.Fatalf("expected 22, got %d", len(frame))
	}
	payloadLen := binary.LittleEndian.Uint32(frame[0:4])
	if payloadLen != 0 {
		t.Fatalf("expected payload len 0, got %d", payloadLen)
	}
	if frame[5] != MsgPing {
		t.Fatalf("type: got 0x%02x", frame[5])
	}
}

func TestAllRejectReasons(t *testing.T) {
	for r := RejectReason(0); r <= 10; r++ {
		var p [17]byte
		binary.LittleEndian.PutUint64(p[0:8], 1)
		p[8] = byte(r)
		binary.LittleEndian.PutUint64(p[9:17], 1000)
		ev, err := DecodeEvent(0x11, p[:])
		if err != nil {
			t.Fatal(err)
		}
		if ev.OrderRejected.Reason != r {
			t.Fatal("mismatch")
		}
	}
}

func TestAllSidesEncode(t *testing.T) {
	for _, s := range []Side{SideBuy, SideSell} {
		payload := EncodePlaceOrder(&PlaceOrderRequest{OrderId: 1, Side: s, Qty: NewDecimal(1.0)})
		if payload[10] != byte(s) {
			t.Fatal("side")
		}
	}
}

func TestAllOrderTypesEncode(t *testing.T) {
	for _, ot := range []OrderType{0, 1, 2, 3} {
		payload := EncodePlaceOrder(&PlaceOrderRequest{OrderId: 1, OrderType: ot, Qty: NewDecimal(1.0)})
		if payload[11] != byte(ot) {
			t.Fatal("type")
		}
	}
}

func TestPlaceOrderRoundtrip(t *testing.T) {
	req := &PlaceOrderRequest{
		OrderId: 123, SymbolId: 7, Side: SideBuy,
		OrderType: OrderTypeStopLimit, TimeInForce: TimeInForceFOK,
		Price: NewDecimal(100.0), StopPrice: NewDecimal(95.0), Qty: NewDecimal(2.5),
	}
	payload := EncodePlaceOrder(req)
	decoded, err := DecodePlaceOrder(payload)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.OrderId != 123 {
		t.Fatal("OrderId")
	}
	if decoded.Side != SideBuy {
		t.Fatal("Side")
	}
	if decoded.OrderType != OrderTypeStopLimit {
		t.Fatal("OrderType")
	}
}

func TestCancelOrderRoundtrip(t *testing.T) {
	req := &CancelOrderRequest{OrderId: 456, SymbolId: 2}
	payload := EncodeCancelOrder(req)
	decoded, err := DecodeCancelOrder(payload)
	if err != nil {
		t.Fatal(err)
	}
	if decoded.OrderId != 456 || decoded.SymbolId != 2 {
		t.Fatal("mismatch")
	}
}

func TestMultiFrameRead(t *testing.T) {
	var p1 [16]byte
	binary.LittleEndian.PutUint64(p1[0:8], 1)
	binary.LittleEndian.PutUint64(p1[8:16], 1000)
	var p2 [24]byte
	binary.LittleEndian.PutUint64(p2[0:8], 2)
	binary.LittleEndian.PutUint64(p2[8:16], 100_000_000)
	binary.LittleEndian.PutUint64(p2[16:24], 2000)

	combined := append(mkFrame(0x10, 1, p1[:]), mkFrame(0x15, 2, p2[:])...)
	r := bytes.NewReader(combined)

	mt1, _, _, pl1, _ := ReadFrame(r)
	ev1, _ := DecodeEvent(mt1, pl1)
	if ev1.OrderAccepted == nil {
		t.Fatal("first")
	}

	mt2, _, _, pl2, _ := ReadFrame(r)
	ev2, _ := DecodeEvent(mt2, pl2)
	if ev2.OrderCancelled == nil {
		t.Fatal("second")
	}
}
