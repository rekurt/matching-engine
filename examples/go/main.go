// Matching Engine — Go Client Example
//
// Demonstrates a full interaction with the matching engine using the meclient
// library: connect (with automatic handshake), place two crossing limit orders,
// read the resulting trade events, send a ping, and cancel a resting order.
//
// Usage:
//
//	go run main.go [addr]
//
// Defaults to 127.0.0.1:9090. The matching engine must be running with at
// least one symbol configured (symbol_id=0, e.g. BTC/USDT).
package main

import (
	"fmt"
	"io"
	"os"
	"time"

	"github.com/rekurt/matching-engine/clients/go/meclient"
)

func main() {
	addr := "127.0.0.1:9090"
	if len(os.Args) > 1 {
		addr = os.Args[1]
	}

	fmt.Printf("=== Matching Engine Go Client Example ===\n\n")

	// ---------------------------------------------------------------
	// 1. Connect (Dial performs the protocol handshake automatically)
	// ---------------------------------------------------------------
	fmt.Printf("Connecting to %s ...\n", addr)
	client, err := meclient.Dial(addr)
	if err != nil {
		fmt.Fprintf(os.Stderr, "ERROR: %v\n", err)
		os.Exit(1)
	}
	defer client.Close()
	fmt.Println("Connected and handshake completed.")
	fmt.Println()

	// ---------------------------------------------------------------
	// 2. Ping — verify the connection is alive
	// ---------------------------------------------------------------
	fmt.Println("Sending Ping ...")
	if err := client.Ping(); err != nil {
		fmt.Fprintf(os.Stderr, "ERROR ping: %v\n", err)
		os.Exit(1)
	}
	fmt.Println("Pong received — connection is healthy.")
	fmt.Println()
	done := make(chan struct{})
	defer close(done)
	events := readStream(client, done)

	// ---------------------------------------------------------------
	// 3. Place a Buy Limit order: 1.5 BTC @ $50,000.00
	// ---------------------------------------------------------------
	buyOrder := &meclient.PlaceOrderRequest{
		OrderId:     1001,
		SymbolId:    0, // BTC/USDT
		Side:        meclient.SideBuy,
		OrderType:   meclient.OrderTypeLimit,
		TimeInForce: meclient.TimeInForceGTC,
		Price:       meclient.NewDecimal(50_000.00),
		StopPrice:   meclient.Decimal{}, // zero — not a stop order
		Qty:         meclient.NewDecimal(1.5),
	}

	fmt.Printf("Placing BUY LIMIT: order_id=%d, price=%s, qty=%s\n",
		buyOrder.OrderId, buyOrder.Price, buyOrder.Qty)

	buySeq, err := client.PlaceOrder(buyOrder)
	if err != nil {
		fmt.Fprintf(os.Stderr, "ERROR place buy: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("  → sent (request_id=%d)\n", buySeq)

	// Read the response: should be OrderAccepted (the buy rests on the book
	// because there are no asks yet).
	readAndPrint(events)
	fmt.Println()

	// ---------------------------------------------------------------
	// 4. Place a Sell Limit order: 0.5 BTC @ $49,999.00
	//    This CROSSES the buy order and will generate a trade.
	// ---------------------------------------------------------------
	sellOrder := &meclient.PlaceOrderRequest{
		OrderId:     1002,
		SymbolId:    0,
		Side:        meclient.SideSell,
		OrderType:   meclient.OrderTypeLimit,
		TimeInForce: meclient.TimeInForceGTC,
		Price:       meclient.NewDecimal(49_999.00),
		StopPrice:   meclient.Decimal{},
		Qty:         meclient.NewDecimal(0.5),
	}

	fmt.Printf("Placing SELL LIMIT: order_id=%d, price=%s, qty=%s\n",
		sellOrder.OrderId, sellOrder.Price, sellOrder.Qty)

	sellSeq, err := client.PlaceOrder(sellOrder)
	if err != nil {
		fmt.Fprintf(os.Stderr, "ERROR place sell: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("  → sent (request_id=%d)\n", sellSeq)

	// Read all response events. The matching engine will emit:
	//   - OrderAccepted (for the sell)
	//   - Trade (0.5 BTC @ $50,000 — executes at maker's price)
	//   - OrderPartiallyFilled (buy order 1001 partially filled)
	//   - OrderFilled (sell order 1002 completely filled)
	// Read until we hit a timeout (no more events).
	readAllEvents(events)
	fmt.Println()

	// ---------------------------------------------------------------
	// 5. Cancel the remaining buy order (1.0 BTC left)
	// ---------------------------------------------------------------
	cancelReq := &meclient.CancelOrderRequest{
		OrderId:  1001,
		SymbolId: 0,
	}

	fmt.Printf("Cancelling order %d ...\n", cancelReq.OrderId)
	cancelSeq, err := client.CancelOrder(cancelReq)
	if err != nil {
		fmt.Fprintf(os.Stderr, "ERROR cancel: %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("  → sent (request_id=%d)\n", cancelSeq)

	readAndPrint(events)
	fmt.Println()

	// ---------------------------------------------------------------
	// 6. Place a Market order to demonstrate IOC behavior
	// ---------------------------------------------------------------
	marketOrder := &meclient.PlaceOrderRequest{
		OrderId:     1003,
		SymbolId:    0,
		Side:        meclient.SideBuy,
		OrderType:   meclient.OrderTypeMarket,
		TimeInForce: meclient.TimeInForceIOC,
		Price:       meclient.Decimal{}, // no price for market orders
		StopPrice:   meclient.Decimal{},
		Qty:         meclient.NewDecimal(1.0),
	}

	fmt.Printf("Placing BUY MARKET (IOC): order_id=%d, qty=%s\n",
		marketOrder.OrderId, marketOrder.Qty)

	_, err = client.PlaceOrder(marketOrder)
	if err != nil {
		fmt.Fprintf(os.Stderr, "ERROR place market: %v\n", err)
		os.Exit(1)
	}

	// This will likely be rejected with NoLiquidity since the book is empty.
	readAllEvents(events)

	fmt.Println("\n=== Done ===")
}

type readResult struct {
	event     *meclient.Event
	requestID uint64
	err       error
}

func readStream(client *meclient.Client, done <-chan struct{}) <-chan readResult {
	results := make(chan readResult)
	go func() {
		defer close(results)
		for {
			event, requestID, err := client.ReadEvent()
			select {
			case results <- readResult{event, requestID, err}:
			case <-done:
				return
			}
			if err != nil {
				return
			}
		}
	}()
	return results
}

func printResult(r readResult) bool {
	if r.err != nil {
		if r.err == io.EOF {
			fmt.Println("  [connection closed]")
		} else {
			fmt.Fprintf(os.Stderr, "  ERROR read: %v\n", r.err)
		}
		return false
	}
	printEvent(r.event, r.requestID)
	return true
}

func readAndPrint(events <-chan readResult) {
	if r, ok := <-events; ok {
		printResult(r)
	}
}

func readAllEvents(events <-chan readResult) {
	timer := time.NewTimer(500 * time.Millisecond)
	defer timer.Stop()
	for {
		select {
		case r, ok := <-events:
			if !ok || !printResult(r) {
				return
			}
			if !timer.Stop() {
				select {
				case <-timer.C:
				default:
				}
			}
			timer.Reset(500 * time.Millisecond)
		case <-timer.C:
			return
		}
	}
}

// printEvent formats and prints a matching engine event.
func printEvent(event *meclient.Event, seq uint64) {
	switch {
	case event.OrderAccepted != nil:
		e := event.OrderAccepted
		fmt.Printf("  [request_id=%d] OrderAccepted: order_id=%d, ts=%d\n",
			seq, e.OrderId, e.Timestamp)

	case event.OrderRejected != nil:
		e := event.OrderRejected
		fmt.Printf("  [request_id=%d] OrderRejected: order_id=%d, reason=%s, ts=%d\n",
			seq, e.OrderId, e.Reason, e.Timestamp)

	case event.Trade != nil:
		e := event.Trade
		fmt.Printf("  [request_id=%d] Trade: trade_id=%d, symbol=%d, price=%s, qty=%s, "+
			"maker=%d, taker=%d, maker_side=%s\n",
			seq, e.TradeId, e.SymbolId, e.Price, e.Qty,
			e.MakerOrderId, e.TakerOrderId, e.MakerSide)

	case event.OrderFilled != nil:
		e := event.OrderFilled
		fmt.Printf("  [request_id=%d] OrderFilled: order_id=%d, filled_qty=%s, price=%s\n",
			seq, e.OrderId, e.FilledQty, e.Price)

	case event.OrderPartiallyFilled != nil:
		e := event.OrderPartiallyFilled
		fmt.Printf("  [request_id=%d] OrderPartiallyFilled: order_id=%d, filled=%s, remaining=%s, price=%s\n",
			seq, e.OrderId, e.FilledQty, e.RemainingQty, e.Price)

	case event.OrderCancelled != nil:
		e := event.OrderCancelled
		fmt.Printf("  [request_id=%d] OrderCancelled: order_id=%d, remaining_qty=%s\n",
			seq, e.OrderId, e.RemainingQty)

	case event.OrderAmended != nil:
		e := event.OrderAmended
		fmt.Printf("  [request_id=%d] OrderAmended: order_id=%d, new_price=%s, new_qty=%s\n",
			seq, e.OrderId, e.NewPrice, e.NewQty)

	case event.StopTriggered != nil:
		e := event.StopTriggered
		fmt.Printf("  [request_id=%d] StopTriggered: order_id=%d, trigger_price=%s\n",
			seq, e.OrderId, e.TriggerPrice)

	case event.Pong != nil:
		fmt.Printf("  [request_id=%d] Pong\n", seq)

	case event.Error != nil:
		e := event.Error
		fmt.Printf("  [request_id=%d] Error: code=%d, message=%q\n",
			seq, e.Code, e.Message)

	case event.HandshakeAck != nil:
		e := event.HandshakeAck
		fmt.Printf("  [request_id=%d] HandshakeAck: server_version=%d\n",
			seq, e.ServerVersion)

	default:
		fmt.Printf("  [request_id=%d] <unknown event>\n", seq)
	}
}
