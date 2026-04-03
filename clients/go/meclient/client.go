package meclient

import (
	"bufio"
	"fmt"
	"io"
	"net"
	"sync"
	"sync/atomic"
)

// Client is a thread-safe client for the matching engine wire protocol.
// It supports both TCP and Unix domain socket connections.
type Client struct {
	mu   sync.Mutex
	conn net.Conn
	w    *bufio.Writer
	r    *bufio.Reader
	seq  atomic.Uint64
}

// Dial connects to a matching engine over TCP at the given address
// (e.g. "127.0.0.1:9090") and performs the required protocol handshake.
func Dial(addr string) (*Client, error) {
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("meclient: dial tcp %s: %w", addr, err)
	}
	c := newClient(conn)
	if err := c.Handshake(); err != nil {
		conn.Close()
		return nil, err
	}
	return c, nil
}

// DialUDS connects to a matching engine over a Unix domain socket at the
// given path (e.g. "/tmp/matching-engine.sock") and performs the required
// protocol handshake.
func DialUDS(path string) (*Client, error) {
	conn, err := net.Dial("unix", path)
	if err != nil {
		return nil, fmt.Errorf("meclient: dial unix %s: %w", path, err)
	}
	c := newClient(conn)
	if err := c.Handshake(); err != nil {
		conn.Close()
		return nil, err
	}
	return c, nil
}

// NewClientFromConn creates a Client from an existing net.Conn without
// performing a handshake. The caller must call Handshake() before sending
// commands, unless the connection is used for testing only.
func NewClientFromConn(conn net.Conn) *Client {
	return newClient(conn)
}

func newClient(conn net.Conn) *Client {
	return &Client{
		conn: conn,
		w:    bufio.NewWriter(conn),
		r:    bufio.NewReader(conn),
	}
}

// Close closes the underlying connection.
func (c *Client) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.conn != nil {
		return c.conn.Close()
	}
	return nil
}

// Handshake sends a Handshake frame and waits for a HandshakeAck response.
// This must be called (or is called automatically by Dial/DialUDS) before
// sending any commands.
func (c *Client) Handshake() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	payload := EncodeHandshake()
	seq := c.nextSeq()
	frame := EncodeFrame(MsgHandshake, seq, payload)
	if _, err := c.w.Write(frame); err != nil {
		return fmt.Errorf("meclient: handshake write: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return fmt.Errorf("meclient: handshake flush: %w", err)
	}

	// Read the response -- should be a HandshakeAck.
	msgType, _, respPayload, err := ReadFrame(c.r)
	if err != nil {
		return fmt.Errorf("meclient: handshake read: %w", err)
	}

	switch msgType {
	case MsgHandshakeAck:
		_, err := DecodeHandshakeAck(respPayload)
		if err != nil {
			return fmt.Errorf("meclient: handshake ack decode: %w", err)
		}
		return nil
	case MsgError:
		ev, decErr := DecodeErrorFrame(respPayload)
		if decErr != nil {
			return fmt.Errorf("%w: failed to decode error response: %v", ErrHandshakeFailed, decErr)
		}
		return fmt.Errorf("%w: code=%d message=%s", ErrHandshakeFailed, ev.Code, ev.Message)
	default:
		return fmt.Errorf("%w: unexpected response type 0x%02x", ErrHandshakeFailed, msgType)
	}
}

// Ping sends a Ping frame and waits for a Pong response.
func (c *Client) Ping() error {
	c.mu.Lock()
	defer c.mu.Unlock()

	seq := c.nextSeq()
	frame := EncodeFrame(MsgPing, seq, nil)
	if _, err := c.w.Write(frame); err != nil {
		return fmt.Errorf("meclient: ping write: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return fmt.Errorf("meclient: ping flush: %w", err)
	}

	// Read the response -- should be a Pong.
	msgType, _, respPayload, err := ReadFrame(c.r)
	if err != nil {
		return fmt.Errorf("meclient: ping read: %w", err)
	}

	switch msgType {
	case MsgPong:
		return nil
	case MsgError:
		ev, decErr := DecodeErrorFrame(respPayload)
		if decErr != nil {
			return fmt.Errorf("%w: failed to decode error response: %v", ErrProtocolError, decErr)
		}
		return fmt.Errorf("%w: code=%d message=%s", ErrProtocolError, ev.Code, ev.Message)
	default:
		return fmt.Errorf("meclient: ping: unexpected response type 0x%02x", msgType)
	}
}

// nextSeq returns the next sequence number.
func (c *Client) nextSeq() uint64 {
	return c.seq.Add(1)
}

// sendFrame encodes and sends a frame. Caller must hold c.mu.
func (c *Client) sendFrame(msgType uint8, payload []byte) (uint64, error) {
	seq := c.nextSeq()
	frame := EncodeFrame(msgType, seq, payload)
	if _, err := c.w.Write(frame); err != nil {
		return 0, fmt.Errorf("meclient: write frame: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return 0, fmt.Errorf("meclient: flush: %w", err)
	}
	return seq, nil
}

// PlaceOrder sends a PlaceOrder request and returns the sequence number.
func (c *Client) PlaceOrder(req *PlaceOrderRequest) (uint64, error) {
	payload := EncodePlaceOrder(req)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sendFrame(MsgPlaceOrder, payload)
}

// CancelOrder sends a CancelOrder request and returns the sequence number.
func (c *Client) CancelOrder(req *CancelOrderRequest) (uint64, error) {
	payload := EncodeCancelOrder(req)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sendFrame(MsgCancelOrder, payload)
}

// AmendOrder sends an AmendOrder request and returns the sequence number.
func (c *Client) AmendOrder(req *AmendOrderRequest) (uint64, error) {
	payload := EncodeAmendOrder(req)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sendFrame(MsgAmendOrder, payload)
}

// ReadEvent reads and decodes the next event from the engine.
// It blocks until an event is available or an error occurs.
// Returns io.EOF when the connection is closed.
func (c *Client) ReadEvent() (*Event, uint64, error) {
	msgType, seq, payload, err := ReadFrame(c.r)
	if err != nil {
		if err == io.EOF {
			return nil, 0, io.EOF
		}
		return nil, 0, fmt.Errorf("meclient: read event: %w", err)
	}
	event, err := DecodeEvent(msgType, payload)
	if err != nil {
		return nil, seq, err
	}
	return event, seq, nil
}

// ReadEvents starts reading events in a loop and sends them to the returned
// channel. The channel is closed when the connection is closed or an error
// occurs. Errors are sent on the error channel.
func (c *Client) ReadEvents() (<-chan *Event, <-chan error) {
	events := make(chan *Event, 64)
	errs := make(chan error, 1)

	go func() {
		defer close(events)
		defer close(errs)
		for {
			event, _, err := c.ReadEvent()
			if err != nil {
				if err != io.EOF {
					errs <- err
				}
				return
			}
			events <- event
		}
	}()

	return events, errs
}
