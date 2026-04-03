package meclient

import (
	"encoding/binary"
	"errors"
	"fmt"
	"io"
)

// Frame header sizes.
const (
	frameLenSize    = 4 // u32 length prefix
	frameVerSize    = 1 // u8 protocol version
	frameTypeSize   = 1 // u8 message type
	frameSeqSize    = 8 // u64 sequence number
	frameHeaderSize = frameLenSize + frameVerSize + frameTypeSize + frameSeqSize // 14 bytes
)

// ProtocolVersion is the current wire protocol version.
const ProtocolVersion uint8 = 1

// Inbound message type IDs (client to engine).
const (
	MsgPlaceOrder  uint8 = 0x01
	MsgCancelOrder uint8 = 0x02
	MsgAmendOrder  uint8 = 0x03
)

// Connection management message type IDs (bidirectional).
const (
	MsgPing      uint8 = 0x04
	MsgHandshake uint8 = 0x05
)

// Outbound message type IDs (engine to client).
const (
	MsgOrderAccepted        uint8 = 0x10
	MsgOrderRejected        uint8 = 0x11
	MsgTrade                uint8 = 0x12
	MsgOrderFilled          uint8 = 0x13
	MsgOrderPartiallyFilled uint8 = 0x14
	MsgOrderCancelled       uint8 = 0x15
	MsgOrderAmended         uint8 = 0x16
	MsgStopTriggered        uint8 = 0x17
	MsgPong                 uint8 = 0x18
	MsgError                uint8 = 0x19
	MsgHandshakeAck         uint8 = 0x1A
)

// Expected payload sizes for outbound messages.
const (
	placeOrderPayloadSize  = 37
	cancelOrderPayloadSize = 10

	orderAcceptedPayloadSize      = 16
	orderRejectedPayloadSize      = 17
	tradePayloadSize              = 51
	orderFilledPayloadSize        = 32
	orderPartiallyFilledPayloadSize = 40
	orderCancelledPayloadSize     = 24
	orderAmendedPayloadSize       = 32
	stopTriggeredPayloadSize      = 24
)

// MaxFrameSize is the maximum allowed frame size to prevent excessive memory
// allocation from malformed data.
const MaxFrameSize = 1 << 20 // 1 MiB

var (
	// ErrFrameTooLarge is returned when a frame exceeds MaxFrameSize.
	ErrFrameTooLarge = errors.New("meclient: frame too large")
	// ErrInvalidPayloadSize is returned when a payload has an unexpected size.
	ErrInvalidPayloadSize = errors.New("meclient: invalid payload size")
	// ErrUnknownMessageType is returned for unrecognized message types.
	ErrUnknownMessageType = errors.New("meclient: unknown message type")
	// ErrHandshakeFailed is returned when the server rejects the handshake.
	ErrHandshakeFailed = errors.New("meclient: handshake failed")
	// ErrProtocolError is returned when the server sends an Error frame.
	ErrProtocolError = errors.New("meclient: protocol error")
)

// EncodeFrame encodes a frame with the given type, sequence number, and
// payload into a byte slice ready for transmission.
//
// Wire layout: len(u32 LE, payload bytes only) + ver(u8) + type(u8) + seq(u64 LE) + payload
func EncodeFrame(msgType uint8, seq uint64, payload []byte) []byte {
	payloadLen := uint32(len(payload))
	buf := make([]byte, frameHeaderSize+len(payload))
	binary.LittleEndian.PutUint32(buf[0:4], payloadLen)
	buf[4] = ProtocolVersion
	buf[5] = msgType
	binary.LittleEndian.PutUint64(buf[6:14], seq)
	copy(buf[14:], payload)
	return buf
}

// ReadFrame reads a single frame from the reader. It returns the message type,
// sequence number, and payload. It returns io.EOF if the connection is closed
// cleanly.
//
// Wire layout: len(u32 LE, payload bytes only) + ver(u8) + type(u8) + seq(u64 LE) + payload(len bytes)
func ReadFrame(r io.Reader) (msgType uint8, seq uint64, payload []byte, err error) {
	// Read the 14-byte header: len(4) + ver(1) + type(1) + seq(8)
	var hdr [frameHeaderSize]byte
	if _, err = io.ReadFull(r, hdr[:]); err != nil {
		return 0, 0, nil, err
	}

	payloadLen := binary.LittleEndian.Uint32(hdr[0:4])
	// hdr[4] is the version byte (ignored for forward compat)
	msgType = hdr[5]
	seq = binary.LittleEndian.Uint64(hdr[6:14])

	if payloadLen > uint32(MaxFrameSize) {
		return 0, 0, nil, ErrFrameTooLarge
	}

	if payloadLen == 0 {
		return msgType, seq, nil, nil
	}

	// Read the payload.
	payload = make([]byte, payloadLen)
	if _, err = io.ReadFull(r, payload); err != nil {
		return 0, 0, nil, err
	}

	return msgType, seq, payload, nil
}

// EncodePlaceOrder encodes a PlaceOrder request into payload bytes.
func EncodePlaceOrder(req *PlaceOrderRequest) []byte {
	buf := make([]byte, placeOrderPayloadSize)
	binary.LittleEndian.PutUint64(buf[0:8], req.OrderId)
	binary.LittleEndian.PutUint16(buf[8:10], req.SymbolId)
	buf[10] = uint8(req.Side)
	buf[11] = uint8(req.OrderType)
	buf[12] = uint8(req.TimeInForce)
	binary.LittleEndian.PutUint64(buf[13:21], uint64(req.Price.Raw()))
	binary.LittleEndian.PutUint64(buf[21:29], uint64(req.StopPrice.Raw()))
	binary.LittleEndian.PutUint64(buf[29:37], req.Qty.RawUint64())
	return buf
}

// DecodePlaceOrder decodes a PlaceOrder payload.
func DecodePlaceOrder(data []byte) (*PlaceOrderRequest, error) {
	if len(data) != placeOrderPayloadSize {
		return nil, fmt.Errorf("%w: PlaceOrder expected %d bytes, got %d", ErrInvalidPayloadSize, placeOrderPayloadSize, len(data))
	}
	return &PlaceOrderRequest{
		OrderId:     binary.LittleEndian.Uint64(data[0:8]),
		SymbolId:    binary.LittleEndian.Uint16(data[8:10]),
		Side:        Side(data[10]),
		OrderType:   OrderType(data[11]),
		TimeInForce: TimeInForce(data[12]),
		Price:       NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[13:21]))),
		StopPrice:   NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[21:29]))),
		Qty:         NewDecimalFromUint64(binary.LittleEndian.Uint64(data[29:37])),
	}, nil
}

// EncodeCancelOrder encodes a CancelOrder request into payload bytes.
func EncodeCancelOrder(req *CancelOrderRequest) []byte {
	buf := make([]byte, cancelOrderPayloadSize)
	binary.LittleEndian.PutUint64(buf[0:8], req.OrderId)
	binary.LittleEndian.PutUint16(buf[8:10], req.SymbolId)
	return buf
}

// DecodeCancelOrder decodes a CancelOrder payload.
func DecodeCancelOrder(data []byte) (*CancelOrderRequest, error) {
	if len(data) != cancelOrderPayloadSize {
		return nil, fmt.Errorf("%w: CancelOrder expected %d bytes, got %d", ErrInvalidPayloadSize, cancelOrderPayloadSize, len(data))
	}
	return &CancelOrderRequest{
		OrderId:  binary.LittleEndian.Uint64(data[0:8]),
		SymbolId: binary.LittleEndian.Uint16(data[8:10]),
	}, nil
}

// EncodeAmendOrder encodes an AmendOrder request into payload bytes.
func EncodeAmendOrder(req *AmendOrderRequest) []byte {
	// Base: 8 (OrderId) + 2 (SymbolId) + 1 (HasPrice) + optional 8 + 1 (HasQty) + optional 8
	size := 10 + 1 // orderId + symbolId + hasPrice
	if req.Price != nil {
		size += 8
	}
	size += 1 // hasQty
	if req.Qty != nil {
		size += 8
	}

	buf := make([]byte, size)
	off := 0
	binary.LittleEndian.PutUint64(buf[off:off+8], req.OrderId)
	off += 8
	binary.LittleEndian.PutUint16(buf[off:off+2], req.SymbolId)
	off += 2

	if req.Price != nil {
		buf[off] = 1
		off++
		binary.LittleEndian.PutUint64(buf[off:off+8], uint64(req.Price.Raw()))
		off += 8
	} else {
		buf[off] = 0
		off++
	}

	if req.Qty != nil {
		buf[off] = 1
		off++
		binary.LittleEndian.PutUint64(buf[off:off+8], req.Qty.RawUint64())
		off += 8
	} else {
		buf[off] = 0
		off++
	}
	return buf
}

// DecodeAmendOrder decodes an AmendOrder payload.
func DecodeAmendOrder(data []byte) (*AmendOrderRequest, error) {
	if len(data) < 12 { // minimum: 8 + 2 + 1 (hasPrice=0) + 1 (hasQty=0)
		return nil, fmt.Errorf("%w: AmendOrder payload too short: %d bytes", ErrInvalidPayloadSize, len(data))
	}
	req := &AmendOrderRequest{
		OrderId:  binary.LittleEndian.Uint64(data[0:8]),
		SymbolId: binary.LittleEndian.Uint16(data[8:10]),
	}
	off := 10

	hasPrice := data[off]
	off++
	if hasPrice == 1 {
		if off+8 > len(data) {
			return nil, fmt.Errorf("%w: AmendOrder payload too short for price", ErrInvalidPayloadSize)
		}
		p := NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[off : off+8])))
		req.Price = &p
		off += 8
	}

	if off >= len(data) {
		return nil, fmt.Errorf("%w: AmendOrder payload missing HasQty", ErrInvalidPayloadSize)
	}
	hasQty := data[off]
	off++
	if hasQty == 1 {
		if off+8 > len(data) {
			return nil, fmt.Errorf("%w: AmendOrder payload too short for qty", ErrInvalidPayloadSize)
		}
		q := NewDecimalFromUint64(binary.LittleEndian.Uint64(data[off : off+8]))
		req.Qty = &q
		off += 8
	}

	return req, nil
}

// DecodeEvent decodes an outbound message payload into an Event.
func DecodeEvent(msgType uint8, payload []byte) (*Event, error) {
	switch msgType {
	case MsgOrderAccepted:
		e, err := decodeOrderAccepted(payload)
		if err != nil {
			return nil, err
		}
		return &Event{OrderAccepted: e}, nil
	case MsgOrderRejected:
		e, err := decodeOrderRejected(payload)
		if err != nil {
			return nil, err
		}
		return &Event{OrderRejected: e}, nil
	case MsgTrade:
		e, err := decodeTrade(payload)
		if err != nil {
			return nil, err
		}
		return &Event{Trade: e}, nil
	case MsgOrderFilled:
		e, err := decodeOrderFilled(payload)
		if err != nil {
			return nil, err
		}
		return &Event{OrderFilled: e}, nil
	case MsgOrderPartiallyFilled:
		e, err := decodeOrderPartiallyFilled(payload)
		if err != nil {
			return nil, err
		}
		return &Event{OrderPartiallyFilled: e}, nil
	case MsgOrderCancelled:
		e, err := decodeOrderCancelled(payload)
		if err != nil {
			return nil, err
		}
		return &Event{OrderCancelled: e}, nil
	case MsgOrderAmended:
		e, err := decodeOrderAmended(payload)
		if err != nil {
			return nil, err
		}
		return &Event{OrderAmended: e}, nil
	case MsgStopTriggered:
		e, err := decodeStopTriggered(payload)
		if err != nil {
			return nil, err
		}
		return &Event{StopTriggered: e}, nil
	case MsgPong:
		return &Event{Pong: &PongEvent{}}, nil
	case MsgError:
		e, err := DecodeErrorFrame(payload)
		if err != nil {
			return nil, err
		}
		return &Event{Error: e}, nil
	case MsgHandshakeAck:
		e, err := DecodeHandshakeAck(payload)
		if err != nil {
			return nil, err
		}
		return &Event{HandshakeAck: e}, nil
	default:
		return nil, fmt.Errorf("%w: 0x%02x", ErrUnknownMessageType, msgType)
	}
}

func decodeOrderAccepted(data []byte) (*OrderAccepted, error) {
	if len(data) != orderAcceptedPayloadSize {
		return nil, fmt.Errorf("%w: OrderAccepted expected %d bytes, got %d", ErrInvalidPayloadSize, orderAcceptedPayloadSize, len(data))
	}
	return &OrderAccepted{
		OrderId:   binary.LittleEndian.Uint64(data[0:8]),
		Timestamp: binary.LittleEndian.Uint64(data[8:16]),
	}, nil
}

func decodeOrderRejected(data []byte) (*OrderRejected, error) {
	if len(data) != orderRejectedPayloadSize {
		return nil, fmt.Errorf("%w: OrderRejected expected %d bytes, got %d", ErrInvalidPayloadSize, orderRejectedPayloadSize, len(data))
	}
	return &OrderRejected{
		OrderId:   binary.LittleEndian.Uint64(data[0:8]),
		Reason:    RejectReason(data[8]),
		Timestamp: binary.LittleEndian.Uint64(data[9:17]),
	}, nil
}

func decodeTrade(data []byte) (*Trade, error) {
	if len(data) != tradePayloadSize {
		return nil, fmt.Errorf("%w: Trade expected %d bytes, got %d", ErrInvalidPayloadSize, tradePayloadSize, len(data))
	}
	return &Trade{
		TradeId:      binary.LittleEndian.Uint64(data[0:8]),
		SymbolId:     binary.LittleEndian.Uint16(data[8:10]),
		Price:        NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[10:18]))),
		Qty:          NewDecimalFromUint64(binary.LittleEndian.Uint64(data[18:26])),
		MakerOrderId: binary.LittleEndian.Uint64(data[26:34]),
		TakerOrderId: binary.LittleEndian.Uint64(data[34:42]),
		MakerSide:    Side(data[42]),
		Timestamp:    binary.LittleEndian.Uint64(data[43:51]),
	}, nil
}

func decodeOrderFilled(data []byte) (*OrderFilled, error) {
	if len(data) != orderFilledPayloadSize {
		return nil, fmt.Errorf("%w: OrderFilled expected %d bytes, got %d", ErrInvalidPayloadSize, orderFilledPayloadSize, len(data))
	}
	return &OrderFilled{
		OrderId:   binary.LittleEndian.Uint64(data[0:8]),
		FilledQty: NewDecimalFromUint64(binary.LittleEndian.Uint64(data[8:16])),
		Price:     NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[16:24]))),
		Timestamp: binary.LittleEndian.Uint64(data[24:32]),
	}, nil
}

func decodeOrderPartiallyFilled(data []byte) (*OrderPartiallyFilled, error) {
	if len(data) != orderPartiallyFilledPayloadSize {
		return nil, fmt.Errorf("%w: OrderPartiallyFilled expected %d bytes, got %d", ErrInvalidPayloadSize, orderPartiallyFilledPayloadSize, len(data))
	}
	return &OrderPartiallyFilled{
		OrderId:      binary.LittleEndian.Uint64(data[0:8]),
		FilledQty:    NewDecimalFromUint64(binary.LittleEndian.Uint64(data[8:16])),
		RemainingQty: NewDecimalFromUint64(binary.LittleEndian.Uint64(data[16:24])),
		Price:        NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[24:32]))),
		Timestamp:    binary.LittleEndian.Uint64(data[32:40]),
	}, nil
}

func decodeOrderCancelled(data []byte) (*OrderCancelled, error) {
	if len(data) != orderCancelledPayloadSize {
		return nil, fmt.Errorf("%w: OrderCancelled expected %d bytes, got %d", ErrInvalidPayloadSize, orderCancelledPayloadSize, len(data))
	}
	return &OrderCancelled{
		OrderId:      binary.LittleEndian.Uint64(data[0:8]),
		RemainingQty: NewDecimalFromUint64(binary.LittleEndian.Uint64(data[8:16])),
		Timestamp:    binary.LittleEndian.Uint64(data[16:24]),
	}, nil
}

func decodeOrderAmended(data []byte) (*OrderAmended, error) {
	if len(data) != orderAmendedPayloadSize {
		return nil, fmt.Errorf("%w: OrderAmended expected %d bytes, got %d", ErrInvalidPayloadSize, orderAmendedPayloadSize, len(data))
	}
	return &OrderAmended{
		OrderId:   binary.LittleEndian.Uint64(data[0:8]),
		NewPrice:  NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[8:16]))),
		NewQty:    NewDecimalFromUint64(binary.LittleEndian.Uint64(data[16:24])),
		Timestamp: binary.LittleEndian.Uint64(data[24:32]),
	}, nil
}

func decodeStopTriggered(data []byte) (*StopTriggered, error) {
	if len(data) != stopTriggeredPayloadSize {
		return nil, fmt.Errorf("%w: StopTriggered expected %d bytes, got %d", ErrInvalidPayloadSize, stopTriggeredPayloadSize, len(data))
	}
	return &StopTriggered{
		OrderId:      binary.LittleEndian.Uint64(data[0:8]),
		TriggerPrice: NewDecimalRaw(int64(binary.LittleEndian.Uint64(data[8:16]))),
		Timestamp:    binary.LittleEndian.Uint64(data[16:24]),
	}, nil
}

// ─── Connection management codecs ─────────────────────────────────────────

// EncodeHandshake builds a Handshake payload: a single byte with the client
// protocol version.
func EncodeHandshake() []byte {
	return []byte{ProtocolVersion}
}

// DecodeHandshakeAck decodes a HandshakeAck payload (1 byte: server version).
func DecodeHandshakeAck(data []byte) (*HandshakeAckEvent, error) {
	if len(data) < 1 {
		return nil, fmt.Errorf("%w: HandshakeAck expected at least 1 byte, got %d", ErrInvalidPayloadSize, len(data))
	}
	return &HandshakeAckEvent{
		ServerVersion: data[0],
	}, nil
}

// DecodeErrorFrame decodes an Error frame payload:
// error_code(u16 LE) + msg_len(u16 LE) + message(bytes).
func DecodeErrorFrame(data []byte) (*ErrorEvent, error) {
	if len(data) < 4 {
		return nil, fmt.Errorf("%w: Error frame expected at least 4 bytes, got %d", ErrInvalidPayloadSize, len(data))
	}
	code := binary.LittleEndian.Uint16(data[0:2])
	msgLen := binary.LittleEndian.Uint16(data[2:4])
	msg := ""
	if msgLen > 0 {
		if len(data) < 4+int(msgLen) {
			return nil, fmt.Errorf("%w: Error frame message truncated", ErrInvalidPayloadSize)
		}
		msg = string(data[4 : 4+msgLen])
	}
	return &ErrorEvent{
		Code:    code,
		Message: msg,
	}, nil
}
