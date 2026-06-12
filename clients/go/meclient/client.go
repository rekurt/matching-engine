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
//
// Read model: ALL socket reads are serialized by readMu, so concurrent reads
// can never corrupt the bufio.Reader. Note, however, that Handshake/Ping are
// synchronous request/reply calls that consume the next frame expecting a
// specific response; do NOT run them concurrently with the ReadEvents() loop,
// which also consumes frames (whichever wins the read gets the frame). The
// intended usage is: Handshake once at Dial, then either the synchronous
// PlaceOrder+ReadEvent style OR the ReadEvents() streaming style — not Ping
// interleaved with a running ReadEvents() loop.
type Client struct {
	// mu serializes writes to the connection (and the seq/reqID side effects of
	// the request/reply helpers).
	mu sync.Mutex
	// readMu serializes reads from r so concurrent readers cannot corrupt the
	// shared bufio.Reader. Lock ordering: mu is always acquired before readMu;
	// ReadEvent acquires only readMu.
	readMu sync.Mutex
	conn   net.Conn
	w      *bufio.Writer
	r      *bufio.Reader
	seq    atomic.Uint64
	reqID  atomic.Uint64
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
	// Handshake is a connection-management frame, not an inbound command, so
	// request_id is 0.
	frame := EncodeFrame(MsgHandshake, seq, 0, payload)
	if _, err := c.w.Write(frame); err != nil {
		return fmt.Errorf("meclient: handshake write: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return fmt.Errorf("meclient: handshake flush: %w", err)
	}

	// Read the response -- should be a HandshakeAck.
	c.readMu.Lock()
	msgType, _, _, respPayload, err := ReadFrame(c.r)
	c.readMu.Unlock()
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
	// Ping is a connection-management frame, not an inbound command (request_id 0).
	frame := EncodeFrame(MsgPing, seq, 0, nil)
	if _, err := c.w.Write(frame); err != nil {
		return fmt.Errorf("meclient: ping write: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return fmt.Errorf("meclient: ping flush: %w", err)
	}

	// Read the response -- should be a Pong.
	c.readMu.Lock()
	msgType, _, _, respPayload, err := ReadFrame(c.r)
	c.readMu.Unlock()
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

// nextReqID returns the next request id. It is always non-zero (the counter
// starts at 0 and is pre-incremented), because the v2 server rejects inbound
// commands whose request_id is 0.
func (c *Client) nextReqID() uint64 {
	return c.reqID.Add(1)
}

// sendFrame encodes and sends a command frame with a fresh non-zero request id,
// returning that id so the caller can correlate the server's response (which
// echoes it). Caller must hold c.mu.
func (c *Client) sendFrame(msgType uint8, payload []byte) (uint64, error) {
	seq := c.nextSeq()
	requestID := c.nextReqID()
	frame := EncodeFrame(msgType, seq, requestID, payload)
	if _, err := c.w.Write(frame); err != nil {
		return 0, fmt.Errorf("meclient: write frame: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return 0, fmt.Errorf("meclient: flush: %w", err)
	}
	return requestID, nil
}

// PlaceOrder sends a PlaceOrder request and returns the request id (echoed by the matching response).
func (c *Client) PlaceOrder(req *PlaceOrderRequest) (uint64, error) {
	payload := EncodePlaceOrder(req)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sendFrame(MsgPlaceOrder, payload)
}

// CancelOrder sends a CancelOrder request and returns the request id (echoed by the matching response).
func (c *Client) CancelOrder(req *CancelOrderRequest) (uint64, error) {
	payload := EncodeCancelOrder(req)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sendFrame(MsgCancelOrder, payload)
}

// AmendOrder sends an AmendOrder request and returns the request id (echoed by the matching response).
func (c *Client) AmendOrder(req *AmendOrderRequest) (uint64, error) {
	payload := EncodeAmendOrder(req)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.sendFrame(MsgAmendOrder, payload)
}

// ReadEvent reads and decodes the next event from the engine. It blocks until
// an event is available or an error occurs. The returned uint64 is the request
// id the server echoed — the originating command's id, or 0 for unsolicited
// frames — letting callers correlate replies to PlaceOrder/CancelOrder/
// AmendOrder. Returns io.EOF when the connection is closed.
func (c *Client) ReadEvent() (*Event, uint64, error) {
	c.readMu.Lock()
	msgType, _, requestID, payload, err := ReadFrame(c.r)
	c.readMu.Unlock()
	if err != nil {
		if err == io.EOF {
			return nil, 0, io.EOF
		}
		return nil, 0, fmt.Errorf("meclient: read event: %w", err)
	}
	event, err := DecodeEvent(msgType, payload)
	if err != nil {
		return nil, requestID, err
	}
	return event, requestID, nil
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
