package meclient

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/binary"
	"encoding/json"
	"io"
	"net"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

// cutAccepted drops a committed response before the recovering reader can apply it.
type cutAccepted struct {
	net.Conn
	cut       *atomic.Bool
	header    []byte
	remaining int
}

func (c *cutAccepted) Read(p []byte) (int, error) {
	n, err := c.Conn.Read(p)
	for _, b := range p[:n] {
		if c.remaining > 0 {
			c.remaining--
			continue
		}
		c.header = append(c.header, b)
		if len(c.header) == 22 {
			kind := c.header[5]
			c.remaining = int(binary.LittleEndian.Uint32(c.header))
			c.header = c.header[:0]
			if kind == MsgOrderAccepted && c.cut.CompareAndSwap(false, true) {
				c.Conn.Close()
				return 0, io.EOF
			}
		}
	}
	return n, err
}
func TestRecoveringRuntimeTLS(t *testing.T) {
	address := os.Getenv("ME_RECOVERY_ENDPOINT")
	if address == "" {
		t.Skip("runtime TLS fixture is not running")
	}
	ca, err := os.ReadFile(os.Getenv("ME_RECOVERY_CA"))
	if err != nil {
		t.Fatal(err)
	}
	roots := x509.NewCertPool()
	if !roots.AppendCertsFromPEM(ca) {
		t.Fatal("invalid test CA")
	}
	certificate, err := tls.LoadX509KeyPair(os.Getenv("ME_RECOVERY_CERT"), os.Getenv("ME_RECOVERY_KEY"))
	if err != nil {
		t.Fatal(err)
	}
	dialer := tls.Dialer{Config: &tls.Config{RootCAs: roots, Certificates: []tls.Certificate{certificate}, MinVersion: tls.VersionTLS13}}
	store, err := OpenFileSessionStore(filepath.Join(t.TempDir(), "session.json"))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	var cut atomic.Bool
	client, err := NewRecoveringClient(RecoveryOptions{Endpoint: address, Store: store, Heartbeat: 100 * time.Millisecond,
		Dial: func(ctx context.Context) (net.Conn, error) {
			conn, err := dialer.DialContext(ctx, "tcp", address)
			if err != nil {
				return nil, err
			}
			return &cutAccepted{Conn: conn, cut: &cut}, nil
		},
		Apply: func(_ context.Context, frame RecoveryFrame, state json.RawMessage) (json.RawMessage, error) {
			var applied map[string]int
			if len(state) > 0 {
				if err := json.Unmarshal(state, &applied); err != nil {
					return nil, err
				}
			}
			if applied == nil {
				applied = map[string]int{}
			}
			switch frame.Type {
			case MsgOrderAccepted:
				applied["accepted"]++
			case MsgOrderCancelled:
				applied["cancelled"]++
			case MsgOrderRejected:
				applied["rejected"]++
			}
			return json.Marshal(applied)
		}})
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- client.Run(ctx) }()
	defer func() {
		cancel()
		select {
		case <-done:
		case <-time.After(time.Second):
			t.Error("client did not stop")
		}
	}()
	await := func(predicate func(SessionState) bool) {
		t.Helper()
		for {
			state, err := store.Load(ctx)
			if err != nil {
				t.Fatal(err)
			}
			if predicate(state) {
				return
			}
			select {
			case err := <-done:
				t.Fatalf("recovering client stopped: %v", err)
			case <-ctx.Done():
				t.Fatal(ctx.Err())
			case <-time.After(10 * time.Millisecond):
			}
		}
	}
	await(func(SessionState) bool { return client.Ready() })
	price, _ := NewDecimalFromString("100")
	quantity, _ := NewDecimalFromString("1")
	if _, err = client.PlaceOrder(ctx, &PlaceOrderRequest{OrderId: 900001, SymbolId: 0, Side: SideBuy, OrderType: OrderTypeLimit, TimeInForce: TimeInForceGTC, Price: price, Qty: quantity}); err != nil {
		t.Fatal(err)
	}
	await(func(s SessionState) bool {
		var applied map[string]int
		_ = json.Unmarshal(s.Application, &applied)
		return cut.Load() && applied["accepted"] == 1 && len(s.Pending) == 0 && client.Ready()
	})
	if _, err = client.CancelOrder(ctx, &CancelOrderRequest{OrderId: 900001, SymbolId: 0}); err != nil {
		t.Fatal(err)
	}
	await(func(s SessionState) bool {
		var applied map[string]int
		_ = json.Unmarshal(s.Application, &applied)
		return applied["accepted"] == 1 && applied["cancelled"] == 1 && applied["rejected"] == 0 && len(s.Pending) == 0
	})
}
