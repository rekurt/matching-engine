// Package meclient provides a Go client library for the matching engine binary
// wire protocol. It supports TCP and Unix domain socket connections, and
// exposes typed helpers for every inbound and outbound message defined by the
// me-io crate.
package meclient

import "fmt"

// ---------------------------------------------------------------------------
// Enumerations
// ---------------------------------------------------------------------------

// Side represents the side of an order (Buy or Sell).
type Side uint8

const (
	// SideBuy is a buy order.
	SideBuy Side = 0
	// SideSell is a sell order.
	SideSell Side = 1
)

// String returns a human-readable representation of the Side.
func (s Side) String() string {
	switch s {
	case SideBuy:
		return "Buy"
	case SideSell:
		return "Sell"
	default:
		return fmt.Sprintf("Side(%d)", uint8(s))
	}
}

// OrderType describes the type of an order.
type OrderType uint8

const (
	// OrderTypeLimit is a limit order.
	OrderTypeLimit OrderType = 0
	// OrderTypeMarket is a market order.
	OrderTypeMarket OrderType = 1
	// OrderTypeStopLimit is a stop-limit order.
	OrderTypeStopLimit OrderType = 2
	// OrderTypeStopMarket is a stop-market order.
	OrderTypeStopMarket OrderType = 3
)

// String returns a human-readable representation of the OrderType.
func (o OrderType) String() string {
	switch o {
	case OrderTypeLimit:
		return "Limit"
	case OrderTypeMarket:
		return "Market"
	case OrderTypeStopLimit:
		return "StopLimit"
	case OrderTypeStopMarket:
		return "StopMarket"
	default:
		return fmt.Sprintf("OrderType(%d)", uint8(o))
	}
}

// TimeInForce describes how long an order remains active.
type TimeInForce uint8

const (
	// TimeInForceGTC (Good-Til-Cancelled) remains active until filled or cancelled.
	TimeInForceGTC TimeInForce = 0
	// TimeInForceIOC (Immediate-Or-Cancel) fills what it can and cancels the rest.
	TimeInForceIOC TimeInForce = 1
	// TimeInForceFOK (Fill-Or-Kill) must be filled entirely or not at all.
	TimeInForceFOK TimeInForce = 2
)

// String returns a human-readable representation of the TimeInForce.
func (t TimeInForce) String() string {
	switch t {
	case TimeInForceGTC:
		return "GTC"
	case TimeInForceIOC:
		return "IOC"
	case TimeInForceFOK:
		return "FOK"
	default:
		return fmt.Sprintf("TimeInForce(%d)", uint8(t))
	}
}

// RejectReason describes why an order was rejected.
type RejectReason uint8

const (
	RejectNoLiquidity           RejectReason = 0
	RejectInsufficientLiquidity RejectReason = 1
	RejectInvalidPrice          RejectReason = 2
	RejectInvalidQuantity       RejectReason = 3
	RejectInvalidLotSize        RejectReason = 4
	RejectInvalidTickSize       RejectReason = 5
	RejectOrderNotFound         RejectReason = 6
	RejectDuplicateOrderId      RejectReason = 7
	RejectSymbolNotFound        RejectReason = 8
)

// String returns a human-readable representation of the RejectReason.
func (r RejectReason) String() string {
	switch r {
	case RejectNoLiquidity:
		return "NoLiquidity"
	case RejectInsufficientLiquidity:
		return "InsufficientLiquidity"
	case RejectInvalidPrice:
		return "InvalidPrice"
	case RejectInvalidQuantity:
		return "InvalidQuantity"
	case RejectInvalidLotSize:
		return "InvalidLotSize"
	case RejectInvalidTickSize:
		return "InvalidTickSize"
	case RejectOrderNotFound:
		return "OrderNotFound"
	case RejectDuplicateOrderId:
		return "DuplicateOrderId"
	case RejectSymbolNotFound:
		return "SymbolNotFound"
	default:
		return fmt.Sprintf("RejectReason(%d)", uint8(r))
	}
}

// ---------------------------------------------------------------------------
// Inbound message types (client -> engine)
// ---------------------------------------------------------------------------

// PlaceOrderMsg is the inbound message for placing a new order.
type PlaceOrderMsg struct {
	OrderID     uint64
	SymbolID    uint16
	Side        Side
	OrderType   OrderType
	TimeInForce TimeInForce
	Price       Decimal // fixed-point, scale 10^8
	StopPrice   Decimal // fixed-point, scale 10^8
	Qty         Decimal // fixed-point, scale 10^8
}

// CancelOrderMsg is the inbound message for cancelling an order.
type CancelOrderMsg struct {
	OrderID  uint64
	SymbolID uint16
}

// AmendOrderMsg is the inbound message for amending an order.
// Price and Qty are optional; set the corresponding Has* flag to true to
// include the field.
type AmendOrderMsg struct {
	OrderID  uint64
	SymbolID uint16
	HasPrice bool
	Price    Decimal
	HasQty   bool
	Qty      Decimal
}

// ---------------------------------------------------------------------------
// Outbound message types (engine -> client)
// ---------------------------------------------------------------------------

// MessageType identifies the kind of outbound engine event.
type MessageType uint8

const (
	MsgOrderAccepted       MessageType = 0x10
	MsgOrderRejected       MessageType = 0x11
	MsgTrade               MessageType = 0x12
	MsgOrderFilled         MessageType = 0x13
	MsgOrderPartiallyFilled MessageType = 0x14
	MsgOrderCancelled      MessageType = 0x15
	MsgOrderAmended        MessageType = 0x16
	MsgStopTriggered       MessageType = 0x17
)

// OrderAccepted is sent when the engine accepts an order.
type OrderAccepted struct {
	OrderID   uint64
	Timestamp uint64
}

// OrderRejected is sent when the engine rejects an order.
type OrderRejected struct {
	OrderID   uint64
	Reason    RejectReason
	Timestamp uint64
}

// Trade is sent when a trade occurs.
type Trade struct {
	TradeID      uint64
	SymbolID     uint16
	Price        Decimal
	Qty          Decimal
	MakerOrderID uint64
	TakerOrderID uint64
	MakerSide    Side
	Timestamp    uint64
}

// OrderFilled is sent when an order is fully filled.
type OrderFilled struct {
	OrderID   uint64
	FilledQty Decimal
	Price     Decimal
	Timestamp uint64
}

// OrderPartiallyFilled is sent when an order is partially filled.
type OrderPartiallyFilled struct {
	OrderID      uint64
	FilledQty    Decimal
	RemainingQty Decimal
	Price        Decimal
	Timestamp    uint64
}

// OrderCancelled is sent when an order is cancelled.
type OrderCancelled struct {
	OrderID      uint64
	RemainingQty Decimal
	Timestamp    uint64
}

// OrderAmended is sent when an order is amended.
type OrderAmended struct {
	OrderID  uint64
	NewPrice Decimal
	NewQty   Decimal
	Timestamp uint64
}

// StopTriggered is sent when a stop order is triggered.
type StopTriggered struct {
	OrderID      uint64
	TriggerPrice Decimal
	Timestamp    uint64
}

// Event wraps any outbound message from the engine together with the sequence
// number from the frame header.
type Event struct {
	Seq  uint64
	Type MessageType

	// Exactly one of the following will be non-nil, corresponding to Type.
	OrderAccepted       *OrderAccepted
	OrderRejected       *OrderRejected
	Trade               *Trade
	OrderFilled         *OrderFilled
	OrderPartiallyFilled *OrderPartiallyFilled
	OrderCancelled      *OrderCancelled
	OrderAmended        *OrderAmended
	StopTriggered       *StopTriggered
}
