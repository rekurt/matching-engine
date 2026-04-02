// Package meclient provides a Go client library for the matching engine binary
// wire protocol. It supports TCP and Unix domain socket connections with
// thread-safe operations for placing, cancelling, and amending orders.
package meclient

import "fmt"

// Side represents the order side (buy or sell).
type Side uint8

const (
	// SideBuy represents a buy order.
	SideBuy Side = 0
	// SideSell represents a sell order.
	SideSell Side = 1
)

// String returns the human-readable name of the side.
func (s Side) String() string {
	switch s {
	case SideBuy:
		return "Buy"
	case SideSell:
		return "Sell"
	default:
		return fmt.Sprintf("Side(%d)", s)
	}
}

// OrderType represents the type of order.
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

// String returns the human-readable name of the order type.
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
		return fmt.Sprintf("OrderType(%d)", o)
	}
}

// TimeInForce specifies how long the order remains active.
type TimeInForce uint8

const (
	// TimeInForceGTC (Good-Til-Cancelled) remains active until filled or cancelled.
	TimeInForceGTC TimeInForce = 0
	// TimeInForceIOC (Immediate-Or-Cancel) fills immediately or is cancelled.
	TimeInForceIOC TimeInForce = 1
	// TimeInForceFOK (Fill-Or-Kill) must be filled entirely or not at all.
	TimeInForceFOK TimeInForce = 2
)

// String returns the human-readable name of the time-in-force.
func (t TimeInForce) String() string {
	switch t {
	case TimeInForceGTC:
		return "GTC"
	case TimeInForceIOC:
		return "IOC"
	case TimeInForceFOK:
		return "FOK"
	default:
		return fmt.Sprintf("TimeInForce(%d)", t)
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
	RejectTradingHalted         RejectReason = 9
	RejectRiskCheckFailed       RejectReason = 10
)

// String returns the human-readable name of the reject reason.
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
	case RejectTradingHalted:
		return "TradingHalted"
	case RejectRiskCheckFailed:
		return "RiskCheckFailed"
	default:
		return fmt.Sprintf("RejectReason(%d)", r)
	}
}

// PlaceOrderRequest represents a request to place a new order.
type PlaceOrderRequest struct {
	OrderId     uint64
	SymbolId    uint16
	Side        Side
	OrderType   OrderType
	TimeInForce TimeInForce
	Price       Decimal // fixed-point price (scale 10^8)
	StopPrice   Decimal // fixed-point stop price (scale 10^8)
	Qty         Decimal // fixed-point quantity (scale 10^8)
}

// CancelOrderRequest represents a request to cancel an existing order.
type CancelOrderRequest struct {
	OrderId  uint64
	SymbolId uint16
}

// AmendOrderRequest represents a request to amend an existing order.
// Price and Qty are optional; set the corresponding pointer to nil to omit.
type AmendOrderRequest struct {
	OrderId  uint64
	SymbolId uint16
	Price    *Decimal // optional new price
	Qty      *Decimal // optional new quantity
}

// OrderAccepted is received when the engine accepts an order.
type OrderAccepted struct {
	OrderId   uint64
	Timestamp uint64
}

// OrderRejected is received when the engine rejects an order.
type OrderRejected struct {
	OrderId   uint64
	Reason    RejectReason
	Timestamp uint64
}

// Trade is received when a trade occurs.
type Trade struct {
	TradeId      uint64
	SymbolId     uint16
	Price        Decimal
	Qty          Decimal
	MakerOrderId uint64
	TakerOrderId uint64
	MakerSide    Side
	Timestamp    uint64
}

// OrderFilled is received when an order is completely filled.
type OrderFilled struct {
	OrderId   uint64
	FilledQty Decimal
	Price     Decimal
	Timestamp uint64
}

// OrderPartiallyFilled is received when an order is partially filled.
type OrderPartiallyFilled struct {
	OrderId      uint64
	FilledQty    Decimal
	RemainingQty Decimal
	Price        Decimal
	Timestamp    uint64
}

// OrderCancelled is received when an order is cancelled.
type OrderCancelled struct {
	OrderId      uint64
	RemainingQty Decimal
	Timestamp    uint64
}

// OrderAmended is received when an order is successfully amended.
type OrderAmended struct {
	OrderId  uint64
	NewPrice Decimal
	NewQty   Decimal
	Timestamp uint64
}

// StopTriggered is received when a stop order is triggered.
type StopTriggered struct {
	OrderId      uint64
	TriggerPrice Decimal
	Timestamp    uint64
}

// Event is a union type for all outbound messages from the engine.
// Exactly one field will be non-nil.
type Event struct {
	OrderAccepted      *OrderAccepted
	OrderRejected      *OrderRejected
	Trade              *Trade
	OrderFilled        *OrderFilled
	OrderPartiallyFilled *OrderPartiallyFilled
	OrderCancelled     *OrderCancelled
	OrderAmended       *OrderAmended
	StopTriggered      *StopTriggered
}
