package meclient

import (
	"bufio"
	"fmt"
	"net"
	"sync"
	"sync/atomic"
)

// Client is a thread-safe client for the matching engine wire protocol.
// It supports both TCP and Unix domain socket connections.
type Client struct {
	mu   sync.Mutex
	conn net.Conn
	r    *bufio.Reader
	w    *bufio.Writer
	seq  atomic.Uint64
}

// Dial connects to the matching engine over TCP at the given address
// (e.g. "127.0.0.1:9090").
func Dial(addr string) (*Client, error) {
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("meclient: tcp dial: %w", err)
	}
	return newClient(conn), nil
}

// DialUDS connects to the matching engine over a Unix domain socket at path.
func DialUDS(path string) (*Client, error) {
	conn, err := net.Dial("unix", path)
	if err != nil {
		return nil, fmt.Errorf("meclient: uds dial: %w", err)
	}
	return newClient(conn), nil
}

// NewClientFromConn creates a Client from an existing net.Conn. This is
// useful for testing or when the caller manages the connection lifecycle.
func NewClientFromConn(conn net.Conn) *Client {
	return newClient(conn)
}

func newClient(conn net.Conn) *Client {
	return &Client{
		conn: conn,
		r:    bufio.NewReaderSize(conn, 64*1024),
		w:    bufio.NewWriterSize(conn, 64*1024),
	}
}

// Close closes the underlying connection.
func (c *Client) Close() error {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.conn.Close()
}

// nextSeq returns the next sequence number.
func (c *Client) nextSeq() uint64 {
	return c.seq.Add(1)
}

// send writes raw bytes under the mutex and flushes the buffer.
func (c *Client) send(data []byte) error {
	c.mu.Lock()
	defer c.mu.Unlock()
	if _, err := c.w.Write(data); err != nil {
		return fmt.Errorf("meclient: write: %w", err)
	}
	if err := c.w.Flush(); err != nil {
		return fmt.Errorf("meclient: flush: %w", err)
	}
	return nil
}

// PlaceOrder sends a PlaceOrder message and returns the sequence number used.
func (c *Client) PlaceOrder(msg *PlaceOrderMsg) (uint64, error) {
	seq := c.nextSeq()
	data := EncodePlaceOrder(seq, msg)
	return seq, c.send(data)
}

// CancelOrder sends a CancelOrder message and returns the sequence number used.
func (c *Client) CancelOrder(msg *CancelOrderMsg) (uint64, error) {
	seq := c.nextSeq()
	data := EncodeCancelOrder(seq, msg)
	return seq, c.send(data)
}

// AmendOrder sends an AmendOrder message and returns the sequence number used.
func (c *Client) AmendOrder(msg *AmendOrderMsg) (uint64, error) {
	seq := c.nextSeq()
	data := EncodeAmendOrder(seq, msg)
	return seq, c.send(data)
}

// ReadEvent reads and decodes the next event from the engine. This method
// blocks until a complete frame is available. It is safe to call from a
// dedicated goroutine while other goroutines send messages.
func (c *Client) ReadEvent() (*Event, error) {
	// Reading is not protected by mu; the bufio.Reader is only accessed from
	// this method, and callers are expected to read from a single goroutine.
	msgType, seq, payload, err := ReadFrame(c.r)
	if err != nil {
		return nil, fmt.Errorf("meclient: read event: %w", err)
	}
	return DecodeEvent(msgType, seq, payload)
}
