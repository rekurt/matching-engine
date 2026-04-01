package meclient

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// Frame header sizes.
const (
	frameLenSize  = 4 // u32 length prefix
	frameTypeSize = 1 // u8 message type
	frameSeqSize  = 8 // u64 sequence number
	frameHdrSize  = frameTypeSize + frameSeqSize // type + seq inside the frame
)

// Inbound message type codes.
const (
	typePlaceOrder  uint8 = 0x01
	typeCancelOrder uint8 = 0x02
	typeAmendOrder  uint8 = 0x03
)

// Payload sizes for fixed-length inbound messages.
const (
	placeOrderPayloadSize  = 37
	cancelOrderPayloadSize = 10
)

// Payload sizes for fixed-length outbound messages.
const (
	orderAcceptedPayloadSize        = 16
	orderRejectedPayloadSize        = 17
	tradePayloadSize                = 51
	orderFilledPayloadSize          = 32
	orderPartiallyFilledPayloadSize = 40
	orderCancelledPayloadSize       = 24
	orderAmendedPayloadSize         = 32
	stopTriggeredPayloadSize        = 24
)

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

var (
	// ErrIncompleteFrame is returned when not enough bytes are available.
	ErrIncompleteFrame = errors.New("meclient: incomplete frame")
	// ErrInvalidMessageType is returned for unknown message type codes.
	ErrInvalidMessageType = errors.New("meclient: invalid message type")
	// ErrInvalidPayloadSize is returned when payload length does not match expected.
	ErrInvalidPayloadSize = errors.New("meclient: invalid payload size")
)

// ---------------------------------------------------------------------------
// Encoding helpers (client -> engine)
// ---------------------------------------------------------------------------

// EncodeFrame wraps a typed payload into a length-prefixed frame.
// The length field covers type (1) + seq (8) + payload.
func EncodeFrame(msgType uint8, seq uint64, payload []byte) []byte {
	innerLen := uint32(frameHdrSize + len(payload))
	buf := make([]byte, frameLenSize+int(innerLen))
	binary.LittleEndian.PutUint32(buf[0:4], innerLen)
	buf[4] = msgType
	binary.LittleEndian.PutUint64(buf[5:13], seq)
	copy(buf[13:], payload)
	return buf
}

// EncodePlaceOrder serialises a PlaceOrderMsg into a wire frame.
func EncodePlaceOrder(seq uint64, msg *PlaceOrderMsg) []byte {
	payload := make([]byte, placeOrderPayloadSize)
	binary.LittleEndian.PutUint64(payload[0:8], msg.OrderID)
	binary.LittleEndian.PutUint16(payload[8:10], msg.SymbolID)
	payload[10] = uint8(msg.Side)
	payload[11] = uint8(msg.OrderType)
	payload[12] = uint8(msg.TimeInForce)
	binary.LittleEndian.PutUint64(payload[13:21], uint64(msg.Price.Raw()))
	binary.LittleEndian.PutUint64(payload[21:29], uint64(msg.StopPrice.Raw()))
	binary.LittleEndian.PutUint64(payload[29:37], msg.Qty.RawUnsigned())
	return EncodeFrame(typePlaceOrder, seq, payload)
}

// EncodeCancelOrder serialises a CancelOrderMsg into a wire frame.
func EncodeCancelOrder(seq uint64, msg *CancelOrderMsg) []byte {
	payload := make([]byte, cancelOrderPayloadSize)
	binary.LittleEndian.PutUint64(payload[0:8], msg.OrderID)
	binary.LittleEndian.PutUint16(payload[8:10], msg.SymbolID)
	return EncodeFrame(typeCancelOrder, seq, payload)
}

// EncodeAmendOrder serialises an AmendOrderMsg into a wire frame.
func EncodeAmendOrder(seq uint64, msg *AmendOrderMsg) []byte {
	// Base: OrderID(8) + SymbolID(2) + HasPrice(1) + [Price(8)] + HasQty(1) + [Qty(8)]
	size := 8 + 2 + 1
	if msg.HasPrice {
		size += 8
	}
	size += 1
	if msg.HasQty {
		size += 8
	}
	payload := make([]byte, size)
	off := 0
	binary.LittleEndian.PutUint64(payload[off:off+8], msg.OrderID)
	off += 8
	binary.LittleEndian.PutUint16(payload[off:off+2], msg.SymbolID)
	off += 2
	if msg.HasPrice {
		payload[off] = 1
		off++
		binary.LittleEndian.PutUint64(payload[off:off+8], uint64(msg.Price.Raw()))
		off += 8
	} else {
		payload[off] = 0
		off++
	}
	if msg.HasQty {
		payload[off] = 1
		off++
		binary.LittleEndian.PutUint64(payload[off:off+8], msg.Qty.RawUnsigned())
	} else {
		payload[off] = 0
	}
	return EncodeFrame(typeAmendOrder, seq, payload)
}

// ---------------------------------------------------------------------------
// Decoding helpers (engine -> client)
// ---------------------------------------------------------------------------

// ReadFrame reads a single length-prefixed frame from r and returns the
// message type, sequence number, and payload bytes. It blocks until a full
// frame is available or an error (including io.EOF) occurs.
func ReadFrame(r io.Reader) (msgType uint8, seq uint64, payload []byte, err error) {
	// Read length prefix.
	var lenBuf [frameLenSize]byte
	if _, err = io.ReadFull(r, lenBuf[:]); err != nil {
		return
	}
	innerLen := binary.LittleEndian.Uint32(lenBuf[:])
	if innerLen < uint32(frameHdrSize) {
		err = ErrIncompleteFrame
		return
	}

	inner := make([]byte, innerLen)
	if _, err = io.ReadFull(r, inner); err != nil {
		return
	}

	msgType = inner[0]
	seq = binary.LittleEndian.Uint64(inner[1:9])
	payload = inner[9:]
	return
}

// DecodeEvent decodes a raw frame (type + seq + payload) into a typed Event.
func DecodeEvent(msgType uint8, seq uint64, payload []byte) (*Event, error) {
	mt := MessageType(msgType)
	ev := &Event{Seq: seq, Type: mt}

	switch mt {
	case MsgOrderAccepted:
		if len(payload) < orderAcceptedPayloadSize {
			return nil, fmt.Errorf("%w: OrderAccepted expects %d bytes, got %d", ErrInvalidPayloadSize, orderAcceptedPayloadSize, len(payload))
		}
		ev.OrderAccepted = &OrderAccepted{
			OrderID:   binary.LittleEndian.Uint64(payload[0:8]),
			Timestamp: binary.LittleEndian.Uint64(payload[8:16]),
		}

	case MsgOrderRejected:
		if len(payload) < orderRejectedPayloadSize {
			return nil, fmt.Errorf("%w: OrderRejected expects %d bytes, got %d", ErrInvalidPayloadSize, orderRejectedPayloadSize, len(payload))
		}
		ev.OrderRejected = &OrderRejected{
			OrderID:   binary.LittleEndian.Uint64(payload[0:8]),
			Reason:    RejectReason(payload[8]),
			Timestamp: binary.LittleEndian.Uint64(payload[9:17]),
		}

	case MsgTrade:
		if len(payload) < tradePayloadSize {
			return nil, fmt.Errorf("%w: Trade expects %d bytes, got %d", ErrInvalidPayloadSize, tradePayloadSize, len(payload))
		}
		ev.Trade = &Trade{
			TradeID:      binary.LittleEndian.Uint64(payload[0:8]),
			SymbolID:     binary.LittleEndian.Uint16(payload[8:10]),
			Price:        DecimalFromRaw(int64(binary.LittleEndian.Uint64(payload[10:18]))),
			Qty:          UnsignedDecimalFromRaw(binary.LittleEndian.Uint64(payload[18:26])),
			MakerOrderID: binary.LittleEndian.Uint64(payload[26:34]),
			TakerOrderID: binary.LittleEndian.Uint64(payload[34:42]),
			MakerSide:    Side(payload[42]),
			Timestamp:    binary.LittleEndian.Uint64(payload[43:51]),
		}

	case MsgOrderFilled:
		if len(payload) < orderFilledPayloadSize {
			return nil, fmt.Errorf("%w: OrderFilled expects %d bytes, got %d", ErrInvalidPayloadSize, orderFilledPayloadSize, len(payload))
		}
		ev.OrderFilled = &OrderFilled{
			OrderID:   binary.LittleEndian.Uint64(payload[0:8]),
			FilledQty: UnsignedDecimalFromRaw(binary.LittleEndian.Uint64(payload[8:16])),
			Price:     DecimalFromRaw(int64(binary.LittleEndian.Uint64(payload[16:24]))),
			Timestamp: binary.LittleEndian.Uint64(payload[24:32]),
		}

	case MsgOrderPartiallyFilled:
		if len(payload) < orderPartiallyFilledPayloadSize {
			return nil, fmt.Errorf("%w: OrderPartiallyFilled expects %d bytes, got %d", ErrInvalidPayloadSize, orderPartiallyFilledPayloadSize, len(payload))
		}
		ev.OrderPartiallyFilled = &OrderPartiallyFilled{
			OrderID:      binary.LittleEndian.Uint64(payload[0:8]),
			FilledQty:    UnsignedDecimalFromRaw(binary.LittleEndian.Uint64(payload[8:16])),
			RemainingQty: UnsignedDecimalFromRaw(binary.LittleEndian.Uint64(payload[16:24])),
			Price:        DecimalFromRaw(int64(binary.LittleEndian.Uint64(payload[24:32]))),
			Timestamp:    binary.LittleEndian.Uint64(payload[32:40]),
		}

	case MsgOrderCancelled:
		if len(payload) < orderCancelledPayloadSize {
			return nil, fmt.Errorf("%w: OrderCancelled expects %d bytes, got %d", ErrInvalidPayloadSize, orderCancelledPayloadSize, len(payload))
		}
		ev.OrderCancelled = &OrderCancelled{
			OrderID:      binary.LittleEndian.Uint64(payload[0:8]),
			RemainingQty: UnsignedDecimalFromRaw(binary.LittleEndian.Uint64(payload[8:16])),
			Timestamp:    binary.LittleEndian.Uint64(payload[16:24]),
		}

	case MsgOrderAmended:
		if len(payload) < orderAmendedPayloadSize {
			return nil, fmt.Errorf("%w: OrderAmended expects %d bytes, got %d", ErrInvalidPayloadSize, orderAmendedPayloadSize, len(payload))
		}
		ev.OrderAmended = &OrderAmended{
			OrderID:   binary.LittleEndian.Uint64(payload[0:8]),
			NewPrice:  DecimalFromRaw(int64(binary.LittleEndian.Uint64(payload[8:16]))),
			NewQty:    UnsignedDecimalFromRaw(binary.LittleEndian.Uint64(payload[16:24])),
			Timestamp: binary.LittleEndian.Uint64(payload[24:32]),
		}

	case MsgStopTriggered:
		if len(payload) < stopTriggeredPayloadSize {
			return nil, fmt.Errorf("%w: StopTriggered expects %d bytes, got %d", ErrInvalidPayloadSize, stopTriggeredPayloadSize, len(payload))
		}
		ev.StopTriggered = &StopTriggered{
			OrderID:      binary.LittleEndian.Uint64(payload[0:8]),
			TriggerPrice: DecimalFromRaw(int64(binary.LittleEndian.Uint64(payload[8:16]))),
			Timestamp:    binary.LittleEndian.Uint64(payload[16:24]),
		}

	default:
		return nil, fmt.Errorf("%w: 0x%02x", ErrInvalidMessageType, msgType)
	}

	return ev, nil
}
