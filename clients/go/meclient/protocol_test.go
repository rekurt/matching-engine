package meclient

import (
	"bytes"
	"encoding/binary"
	"testing"
)

// ─── Encode Tests ───────────────────────────────────────────────────────────

func TestEncodePlaceOrder(t *testing.T) {
	msg := &PlaceOrderMsg{
		OrderID:     42,
		SymbolID:    5,
		Side:        SideSell,
		OrderType:   OrderTypeLimit,
		TimeInForce: TimeInForceIOC,
		Price:       NewDecimal(50000.50),
		StopPrice:   DecimalFromRaw(0),
		Qty:         NewDecimal(1.5),
	}

	frame := EncodePlaceOrder(100, msg)
	// Header: 4 (len) + 1 (type) + 8 (seq) = 13, payload = 37
	// But EncodeFrame puts type+seq inside the length prefix
	// len = 1 + 8 + 37 = 46
	innerLen := binary.LittleEndian.Uint32(frame[0:4])
	if innerLen != 46 {
		t.Fatalf("expected inner len 46, got %d", innerLen)
	}
	// Total frame = 4 + 46 = 50
	if len(frame) != 50 {
		t.Fatalf("expected total frame 50, got %d", len(frame))
	}

	msgType := frame[4]
	if msgType != 0x01 {
		t.Fatalf("expected type 0x01, got 0x%02x", msgType)
	}
	seq := binary.LittleEndian.Uint64(frame[5:13])
	if seq != 100 {
		t.Fatalf("expected seq 100, got %d", seq)
	}

	payload := frame[13:]
	orderID := binary.LittleEndian.Uint64(payload[0:8])
	if orderID != 42 {
		t.Fatalf("expected orderID 42, got %d", orderID)
	}
	symbolID := binary.LittleEndian.Uint16(payload[8:10])
	if symbolID != 5 {
		t.Fatalf("expected symbolID 5, got %d", symbolID)
	}
	if payload[10] != byte(SideSell) {
		t.Fatalf("expected side Sell, got %d", payload[10])
	}
	if payload[11] != byte(OrderTypeLimit) {
		t.Fatalf("expected Limit, got %d", payload[11])
	}
	if payload[12] != byte(TimeInForceIOC) {
		t.Fatalf("expected IOC, got %d", payload[12])
	}
}

func TestEncodeCancelOrder(t *testing.T) {
	msg := &CancelOrderMsg{OrderID: 99, SymbolID: 3}
	frame := EncodeCancelOrder(200, msg)

	innerLen := binary.LittleEndian.Uint32(frame[0:4])
	// 1 (type) + 8 (seq) + 10 (payload) = 19
	if innerLen != 19 {
		t.Fatalf("expected inner len 19, got %d", innerLen)
	}
	payload := frame[13:]
	orderID := binary.LittleEndian.Uint64(payload[0:8])
	if orderID != 99 {
		t.Fatalf("expected orderID 99, got %d", orderID)
	}
}

func TestEncodeAmendOrderBothFields(t *testing.T) {
	msg := &AmendOrderMsg{
		OrderID:  7,
		SymbolID: 1,
		HasPrice: true,
		Price:    NewDecimal(100.0),
		HasQty:   true,
		Qty:      NewDecimal(2.0),
	}
	frame := EncodeAmendOrder(300, msg)

	// payload: 8+2+1+8+1+8 = 28
	innerLen := binary.LittleEndian.Uint32(frame[0:4])
	expectedInner := uint32(1 + 8 + 28)
	if innerLen != expectedInner {
		t.Fatalf("expected inner len %d, got %d", expectedInner, innerLen)
	}
}

func TestEncodeAmendOrderPriceOnly(t *testing.T) {
	msg := &AmendOrderMsg{
		OrderID:  7,
		SymbolID: 1,
		HasPrice: true,
		Price:    NewDecimal(100.0),
		HasQty:   false,
	}
	frame := EncodeAmendOrder(300, msg)
	// payload: 8+2+1+8+1 = 20
	innerLen := binary.LittleEndian.Uint32(frame[0:4])
	expectedInner := uint32(1 + 8 + 20)
	if innerLen != expectedInner {
		t.Fatalf("expected inner len %d, got %d", expectedInner, innerLen)
	}
}

func TestEncodeAmendOrderNoFields(t *testing.T) {
	msg := &AmendOrderMsg{
		OrderID:  7,
		SymbolID: 1,
	}
	frame := EncodeAmendOrder(300, msg)
	// payload: 8+2+1+1 = 12
	innerLen := binary.LittleEndian.Uint32(frame[0:4])
	expectedInner := uint32(1 + 8 + 12)
	if innerLen != expectedInner {
		t.Fatalf("expected inner len %d, got %d", expectedInner, innerLen)
	}
}

// ─── Decode Tests (ReadFrame + DecodeEvent) ─────────────────────────────────

func makeOutboundFrame(msgType uint8, seq uint64, payload []byte) []byte {
	innerLen := 1 + 8 + len(payload)
	buf := make([]byte, 4+innerLen)
	binary.LittleEndian.PutUint32(buf[0:4], uint32(innerLen))
	buf[4] = msgType
	binary.LittleEndian.PutUint64(buf[5:13], seq)
	copy(buf[13:], payload)
	return buf
}

func decodeOne(t *testing.T, frame []byte) *Event {
	t.Helper()
	r := bytes.NewReader(frame)
	msgType, seq, payload, err := ReadFrame(r)
	if err != nil {
		t.Fatalf("ReadFrame: %v", err)
	}
	ev, err := DecodeEvent(msgType, seq, payload)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}
	return ev
}

func TestDecodeOrderAccepted(t *testing.T) {
	var p [16]byte
	binary.LittleEndian.PutUint64(p[0:8], 42)
	binary.LittleEndian.PutUint64(p[8:16], 1000)
	frame := makeOutboundFrame(0x10, 1, p[:])

	ev := decodeOne(t, frame)
	if ev.OrderAccepted == nil {
		t.Fatal("expected OrderAccepted")
	}
	if ev.OrderAccepted.OrderID != 42 {
		t.Fatalf("expected orderID 42, got %d", ev.OrderAccepted.OrderID)
	}
	if ev.OrderAccepted.Timestamp != 1000 {
		t.Fatalf("expected ts 1000, got %d", ev.OrderAccepted.Timestamp)
	}
}

func TestDecodeOrderRejected(t *testing.T) {
	var p [17]byte
	binary.LittleEndian.PutUint64(p[0:8], 10)
	p[8] = byte(RejectInvalidPrice)
	binary.LittleEndian.PutUint64(p[9:17], 2000)
	frame := makeOutboundFrame(0x11, 2, p[:])

	ev := decodeOne(t, frame)
	if ev.OrderRejected == nil {
		t.Fatal("expected OrderRejected")
	}
	if ev.OrderRejected.Reason != RejectInvalidPrice {
		t.Fatalf("expected InvalidPrice, got %d", ev.OrderRejected.Reason)
	}
}

func TestDecodeTrade(t *testing.T) {
	var p [51]byte
	binary.LittleEndian.PutUint64(p[0:8], 1)
	binary.LittleEndian.PutUint16(p[8:10], 0)
	binary.LittleEndian.PutUint64(p[10:18], uint64(NewDecimal(100.0).Raw()))
	binary.LittleEndian.PutUint64(p[18:26], NewDecimal(1.0).RawUnsigned())
	binary.LittleEndian.PutUint64(p[26:34], 10)
	binary.LittleEndian.PutUint64(p[34:42], 20)
	p[42] = byte(SideSell)
	binary.LittleEndian.PutUint64(p[43:51], 3000)
	frame := makeOutboundFrame(0x12, 3, p[:])

	ev := decodeOne(t, frame)
	if ev.Trade == nil {
		t.Fatal("expected Trade")
	}
	if ev.Trade.TradeID != 1 {
		t.Fatalf("expected tradeID 1, got %d", ev.Trade.TradeID)
	}
	if ev.Trade.MakerOrderID != 10 {
		t.Fatalf("expected maker 10, got %d", ev.Trade.MakerOrderID)
	}
}

func TestDecodeOrderFilled(t *testing.T) {
	var p [32]byte
	binary.LittleEndian.PutUint64(p[0:8], 5)
	binary.LittleEndian.PutUint64(p[8:16], 100_000_000)
	binary.LittleEndian.PutUint64(p[16:24], uint64(NewDecimal(50.0).Raw()))
	binary.LittleEndian.PutUint64(p[24:32], 4000)
	frame := makeOutboundFrame(0x13, 4, p[:])

	ev := decodeOne(t, frame)
	if ev.OrderFilled == nil {
		t.Fatal("expected OrderFilled")
	}
	if ev.OrderFilled.OrderID != 5 {
		t.Fatalf("expected orderID 5, got %d", ev.OrderFilled.OrderID)
	}
}

func TestDecodeOrderPartiallyFilled(t *testing.T) {
	var p [40]byte
	binary.LittleEndian.PutUint64(p[0:8], 6)
	binary.LittleEndian.PutUint64(p[8:16], 50_000_000)
	binary.LittleEndian.PutUint64(p[16:24], 50_000_000)
	binary.LittleEndian.PutUint64(p[24:32], uint64(NewDecimal(100.0).Raw()))
	binary.LittleEndian.PutUint64(p[32:40], 5000)
	frame := makeOutboundFrame(0x14, 5, p[:])

	ev := decodeOne(t, frame)
	if ev.OrderPartiallyFilled == nil {
		t.Fatal("expected OrderPartiallyFilled")
	}
}

func TestDecodeOrderCancelled(t *testing.T) {
	var p [24]byte
	binary.LittleEndian.PutUint64(p[0:8], 7)
	binary.LittleEndian.PutUint64(p[8:16], 100_000_000)
	binary.LittleEndian.PutUint64(p[16:24], 6000)
	frame := makeOutboundFrame(0x15, 6, p[:])

	ev := decodeOne(t, frame)
	if ev.OrderCancelled == nil {
		t.Fatal("expected OrderCancelled")
	}
	if ev.OrderCancelled.OrderID != 7 {
		t.Fatalf("expected orderID 7, got %d", ev.OrderCancelled.OrderID)
	}
}

func TestDecodeOrderAmended(t *testing.T) {
	var p [32]byte
	binary.LittleEndian.PutUint64(p[0:8], 8)
	binary.LittleEndian.PutUint64(p[8:16], uint64(NewDecimal(200.0).Raw()))
	binary.LittleEndian.PutUint64(p[16:24], 300_000_000)
	binary.LittleEndian.PutUint64(p[24:32], 7000)
	frame := makeOutboundFrame(0x16, 7, p[:])

	ev := decodeOne(t, frame)
	if ev.OrderAmended == nil {
		t.Fatal("expected OrderAmended")
	}
}

func TestDecodeStopTriggered(t *testing.T) {
	var p [24]byte
	binary.LittleEndian.PutUint64(p[0:8], 9)
	binary.LittleEndian.PutUint64(p[8:16], uint64(NewDecimal(100.0).Raw()))
	binary.LittleEndian.PutUint64(p[16:24], 8000)
	frame := makeOutboundFrame(0x17, 8, p[:])

	ev := decodeOne(t, frame)
	if ev.StopTriggered == nil {
		t.Fatal("expected StopTriggered")
	}
}

// ─── Edge Cases ─────────────────────────────────────────────────────────────

func TestReadFrameIncomplete(t *testing.T) {
	data := []byte{0x05, 0x00, 0x00, 0x00, 0x01}
	r := bytes.NewReader(data)
	_, _, _, err := ReadFrame(r)
	if err == nil {
		t.Fatal("expected error for incomplete frame")
	}
}

func TestDecodeInvalidMessageType(t *testing.T) {
	_, err := DecodeEvent(0xFF, 1, make([]byte, 32))
	if err == nil {
		t.Fatal("expected error for invalid message type")
	}
}

func TestDecodeOrderAcceptedTooShort(t *testing.T) {
	_, err := DecodeEvent(0x10, 1, make([]byte, 8)) // too short
	if err == nil {
		t.Fatal("expected error for short payload")
	}
}

func TestAllRejectReasons(t *testing.T) {
	reasons := []RejectReason{
		RejectNoLiquidity, RejectInsufficientLiquidity, RejectInvalidPrice,
		RejectInvalidQuantity, RejectInvalidLotSize, RejectInvalidTickSize,
		RejectOrderNotFound, RejectDuplicateOrderId, RejectSymbolNotFound,
	}
	for _, reason := range reasons {
		var p [17]byte
		binary.LittleEndian.PutUint64(p[0:8], 1)
		p[8] = byte(reason)
		binary.LittleEndian.PutUint64(p[9:17], 1000)
		ev, err := DecodeEvent(0x11, 1, p[:])
		if err != nil {
			t.Fatalf("failed for reason %d: %v", reason, err)
		}
		if ev.OrderRejected.Reason != reason {
			t.Fatalf("expected %d, got %d", reason, ev.OrderRejected.Reason)
		}
	}
}

func TestAllSides(t *testing.T) {
	for _, s := range []Side{SideBuy, SideSell} {
		msg := &PlaceOrderMsg{OrderID: 1, Side: s, Qty: NewDecimal(1.0)}
		frame := EncodePlaceOrder(1, msg)
		payload := frame[13:]
		if payload[10] != byte(s) {
			t.Fatalf("expected side %d, got %d", s, payload[10])
		}
	}
}

func TestAllOrderTypes(t *testing.T) {
	for _, ot := range []OrderType{OrderTypeLimit, OrderTypeMarket, OrderTypeStopLimit, OrderTypeStopMarket} {
		msg := &PlaceOrderMsg{OrderID: 1, OrderType: ot, Qty: NewDecimal(1.0)}
		frame := EncodePlaceOrder(1, msg)
		payload := frame[13:]
		if payload[11] != byte(ot) {
			t.Fatalf("expected orderType %d, got %d", ot, payload[11])
		}
	}
}

func TestAllTIF(t *testing.T) {
	for _, tif := range []TimeInForce{TimeInForceGTC, TimeInForceIOC, TimeInForceFOK} {
		msg := &PlaceOrderMsg{OrderID: 1, TimeInForce: tif, Qty: NewDecimal(1.0)}
		frame := EncodePlaceOrder(1, msg)
		payload := frame[13:]
		if payload[12] != byte(tif) {
			t.Fatalf("expected tif %d, got %d", tif, payload[12])
		}
	}
}

func TestZeroValues(t *testing.T) {
	msg := &PlaceOrderMsg{} // all zeros
	frame := EncodePlaceOrder(0, msg)
	if len(frame) != 4+1+8+37 {
		t.Fatalf("unexpected frame size: %d", len(frame))
	}
}

func TestNegativePrice(t *testing.T) {
	msg := &PlaceOrderMsg{
		OrderID: 1,
		Price:   DecimalFromRaw(-100_000_000),
		Qty:     NewDecimal(1.0),
	}
	frame := EncodePlaceOrder(1, msg)
	payload := frame[13:]
	rawPrice := int64(binary.LittleEndian.Uint64(payload[13:21]))
	if rawPrice != -100_000_000 {
		t.Fatalf("expected -100000000, got %d", rawPrice)
	}
}

func TestMultipleFramesRead(t *testing.T) {
	var p1 [16]byte
	binary.LittleEndian.PutUint64(p1[0:8], 1)
	binary.LittleEndian.PutUint64(p1[8:16], 1000)
	frame1 := makeOutboundFrame(0x10, 1, p1[:])

	var p2 [24]byte
	binary.LittleEndian.PutUint64(p2[0:8], 2)
	binary.LittleEndian.PutUint64(p2[8:16], 100_000_000)
	binary.LittleEndian.PutUint64(p2[16:24], 2000)
	frame2 := makeOutboundFrame(0x15, 2, p2[:])

	combined := append(frame1, frame2...)
	r := bytes.NewReader(combined)

	// Read first
	mt1, seq1, pl1, err := ReadFrame(r)
	if err != nil {
		t.Fatal(err)
	}
	ev1, err := DecodeEvent(mt1, seq1, pl1)
	if err != nil {
		t.Fatal(err)
	}
	if ev1.OrderAccepted == nil {
		t.Fatal("expected first event to be OrderAccepted")
	}

	// Read second
	mt2, seq2, pl2, err := ReadFrame(r)
	if err != nil {
		t.Fatal(err)
	}
	ev2, err := DecodeEvent(mt2, seq2, pl2)
	if err != nil {
		t.Fatal(err)
	}
	if ev2.OrderCancelled == nil {
		t.Fatal("expected second event to be OrderCancelled")
	}
}
