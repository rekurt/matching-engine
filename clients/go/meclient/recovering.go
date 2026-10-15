package meclient

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"net"
	"sort"
	"sync/atomic"
	"time"
)

var ErrReconciliationRequired = errors.New("meclient: reconciliation required")

const (
	recoveryResume  uint8 = 0x06
	recoveryACK     uint8 = 0x08
	recoveryResumed uint8 = 0x1b
	recoveryGap     uint8 = 0x20
)

// RecoveryFrame includes control and passive frames. Positive server sequences form
// one stream; RequestID correlates command replies and is zero for passive events.
type RecoveryFrame struct {
	Type      uint8
	Sequence  uint64
	RequestID uint64
	Payload   []byte
	Event     *Event
}

// ApplySession must return the application state after this frame. It must not
// synchronously call the same client's Submit methods. Save commits state+cursor
// together; ACK follows that durable commit. External side effects need an outbox.
type ApplySession func(context.Context, RecoveryFrame, json.RawMessage) (json.RawMessage, error)
type RecoveryOptions struct {
	Endpoint             string
	Dial                 func(context.Context) (net.Conn, error)
	Store                SessionStore
	Apply                ApplySession
	Heartbeat            time.Duration
	MaxReconnectAttempts int
}
type recoverySubmission struct {
	kind    uint8
	payload []byte
	reply   chan recoveryReceipt
}
type recoveryReceipt struct {
	id  uint64
	err error
}
type RecoveringClient struct {
	options     RecoveryOptions
	submissions chan recoverySubmission
	running     atomic.Bool
	ready       atomic.Bool
}

func NewRecoveringClient(options RecoveryOptions) (*RecoveringClient, error) {
	if options.Endpoint == "" || options.Dial == nil || options.Store == nil || options.Apply == nil {
		return nil, errors.New("recovering client requires endpoint, dialer, store and application callback")
	}
	if options.Heartbeat <= 0 {
		options.Heartbeat = time.Second
	}
	if options.MaxReconnectAttempts <= 0 {
		options.MaxReconnectAttempts = 5
	}
	return &RecoveringClient{options: options, submissions: make(chan recoverySubmission)}, nil
}
func (c *RecoveringClient) Ready() bool { return c.ready.Load() }

// Submit persists a mutation before transmission. A timeout is not cancellation;
// inspect the durable Pending list before deciding to submit a new mutation.
func (c *RecoveringClient) Submit(ctx context.Context, kind uint8, payload []byte) (uint64, error) {
	if kind != MsgPlaceOrder && kind != MsgCancelOrder && kind != MsgAmendOrder && kind != 0x0b && kind != 0x0c {
		return 0, errors.New("Submit accepts trading mutations only")
	}
	if len(payload) > 4096 {
		return 0, ErrFrameTooLarge
	}
	request := recoverySubmission{kind, append([]byte(nil), payload...), make(chan recoveryReceipt, 1)}
	select {
	case c.submissions <- request:
	case <-ctx.Done():
		return 0, ctx.Err()
	}
	select {
	case result := <-request.reply:
		return result.id, result.err
	case <-ctx.Done():
		return 0, ctx.Err()
	}
}
func (c *RecoveringClient) PlaceOrder(ctx context.Context, request *PlaceOrderRequest) (uint64, error) {
	return c.Submit(ctx, MsgPlaceOrder, EncodePlaceOrder(request))
}
func (c *RecoveringClient) CancelOrder(ctx context.Context, request *CancelOrderRequest) (uint64, error) {
	return c.Submit(ctx, MsgCancelOrder, EncodeCancelOrder(request))
}
func (c *RecoveringClient) AmendOrder(ctx context.Context, request *AmendOrderRequest) (uint64, error) {
	return c.Submit(ctx, MsgAmendOrder, EncodeAmendOrder(request))
}
func (c *RecoveringClient) Run(ctx context.Context) error {
	if !c.running.CompareAndSwap(false, true) {
		return errors.New("recovering client is already running")
	}
	defer c.running.Store(false)
	defer c.ready.Store(false)
	state, err := c.options.Store.Load(ctx)
	if err != nil {
		return err
	}
	if err = validateSessionState(state); err != nil {
		return err
	}
	if state.ReconciliationRequired {
		return fmt.Errorf("%w: %s", ErrReconciliationRequired, state.ReconciliationReason)
	}
	if state.Endpoint != "" && state.Endpoint != c.options.Endpoint {
		return fmt.Errorf("%w: endpoint changed", ErrReconciliationRequired)
	}
	state.Endpoint = c.options.Endpoint
	if err = c.options.Store.Save(ctx, state); err != nil {
		return err
	}
	attempts := 0
	for {
		if err = ctx.Err(); err != nil {
			return err
		}
		connection, dialErr := c.options.Dial(ctx)
		retry, active := true, false
		if dialErr == nil {
			retry, active, err = c.connection(ctx, connection, &state)
		} else {
			err = dialErr
		}
		c.ready.Store(false)
		if !retry {
			return err
		}
		if active {
			attempts = 0
		}
		attempts++
		if attempts >= c.options.MaxReconnectAttempts {
			return fmt.Errorf("session recovery failed; durable state retained: %w", err)
		}
		timer := time.NewTimer(time.Duration(attempts) * 100 * time.Millisecond)
		select {
		case <-ctx.Done():
			timer.Stop()
			return ctx.Err()
		case <-timer.C:
		}
	}
}

type recoveryRead struct {
	frame RecoveryFrame
	err   error
}

func readRecoveryFrame(reader io.Reader) (RecoveryFrame, error) {
	var header [22]byte
	if _, err := io.ReadFull(reader, header[:]); err != nil {
		return RecoveryFrame{}, err
	}
	if header[4] != ProtocolVersion {
		return RecoveryFrame{}, fmt.Errorf("unsupported recovery protocol version %d", header[4])
	}
	kind, seq, id, payload, err := ReadFrame(io.MultiReader(bytes.NewReader(header[:]), reader))
	return RecoveryFrame{Type: kind, Sequence: seq, RequestID: id, Payload: payload}, err
}
func bytesReader(data []byte) *bytes.Reader { return bytes.NewReader(data) }
func validateSessionState(s SessionState) error {
	if s.Version != SessionStorageVersion || s.NextClientSequence < 2 || s.NextRequestID == 0 {
		return errors.New("incompatible/invalid session checkpoint")
	}
	if len(s.SessionID) != 0 && len(s.SessionID) != 16 {
		return errors.New("invalid saved session identity")
	}
	if len(s.SessionID) == 0 && (s.AppliedSequence != 0 || len(s.Pending) != 0) {
		return errors.New("session identity missing from nonempty checkpoint")
	}
	if len(s.SessionID) == 16 && bytes.Equal(s.SessionID, make([]byte, 16)) {
		return errors.New("zero session identity")
	}
	ids := map[uint64]bool{}
	seqs := map[uint64]bool{}
	for _, p := range s.Pending {
		if p.RequestID == 0 || p.RequestID >= s.NextRequestID || p.Sequence < 2 || p.Sequence >= s.NextClientSequence || ids[p.RequestID] || seqs[p.Sequence] {
			return errors.New("invalid pending command counters")
		}
		ids[p.RequestID] = true
		seqs[p.Sequence] = true
		frame, err := readRecoveryFrame(bytes.NewReader(p.Frame))
		if err != nil || frame.RequestID != p.RequestID || frame.Sequence != p.Sequence || len(p.Frame) != 22+len(frame.Payload) {
			return errors.New("invalid pending command bytes")
		}
		if frame.Type != MsgPlaceOrder && frame.Type != MsgCancelOrder && frame.Type != MsgAmendOrder && frame.Type != 0x0b && frame.Type != 0x0c {
			return errors.New("nonmutation in pending commands")
		}
	}
	return nil
}
func (c *RecoveringClient) connection(ctx context.Context, conn net.Conn, state *SessionState) (retry bool, wasActive bool, result error) {
	readerCtx, cancel := context.WithCancel(ctx)
	reads := make(chan recoveryRead, 64)
	writes := make(chan []byte)
	writeErrors := make(chan error, 1)
	readDone := make(chan struct{})
	writeDone := make(chan struct{})
	go func() {
		defer close(readDone)
		for {
			frame, err := readRecoveryFrame(conn)
			select {
			case reads <- recoveryRead{frame, err}:
			case <-readerCtx.Done():
				return
			}
			if err != nil {
				return
			}
		}
	}()
	go func() {
		defer close(writeDone)
		for {
			select {
			case <-readerCtx.Done():
				return
			case data := <-writes:
				conn.SetWriteDeadline(time.Now().Add(10 * time.Second))
				for len(data) > 0 {
					n, err := conn.Write(data)
					if err == nil && n == 0 {
						err = io.ErrShortWrite
					}
					if err != nil {
						select {
						case writeErrors <- err:
						case <-readerCtx.Done():
						}
						return
					}
					data = data[n:]
				}
			}
		}
	}()
	defer func() { cancel(); conn.Close(); <-readDone; <-writeDone }()
	conn.SetReadDeadline(time.Now().Add(10 * time.Second))
	mode := "handshake"
	outgoing := [][]byte{EncodeFrame(MsgHandshake, 0, 0, EncodeHandshake())}
	if len(state.SessionID) != 0 {
		mode = "resume"
		payload := append(append([]byte(nil), state.SessionID...), make([]byte, 8)...)
		binary.LittleEndian.PutUint64(payload[16:], state.AppliedSequence)
		outgoing = [][]byte{EncodeFrame(recoveryResume, 1, 0, payload)}
	}
	ticker := time.NewTicker(c.options.Heartbeat)
	defer ticker.Stop()
	var replay uint32
	save := func() error { return c.options.Store.Save(ctx, *state) }
	control := func(kind uint8, payload []byte) error {
		if state.NextClientSequence == math.MaxUint64 {
			return errors.New("client sequence exhausted")
		}
		seq := state.NextClientSequence
		state.NextClientSequence++
		if err := save(); err != nil {
			return err
		}
		outgoing = append(outgoing, EncodeFrame(kind, seq, 0, payload))
		return nil
	}
	ack := func() error {
		payload := make([]byte, 8)
		binary.LittleEndian.PutUint64(payload, state.AppliedSequence)
		for i := len(outgoing) - 1; i >= 0; i-- {
			if outgoing[i][5] == recoveryACK {
				seq := binary.LittleEndian.Uint64(outgoing[i][6:14])
				outgoing[i] = EncodeFrame(recoveryACK, seq, 0, payload)
				return nil
			}
		}
		return control(recoveryACK, payload)
	}
	activate := func() error {
		sort.Slice(state.Pending, func(i, j int) bool { return state.Pending[i].Sequence < state.Pending[j].Sequence })
		for _, p := range state.Pending {
			outgoing = append(outgoing, append([]byte(nil), p.Frame...))
		}
		mode = "active"
		wasActive = true
		c.ready.Store(true)
		readTimeout := 3 * c.options.Heartbeat
		if readTimeout < 5*time.Second {
			readTimeout = 5 * time.Second
		}
		conn.SetReadDeadline(time.Now().Add(readTimeout))
		payload := make([]byte, 8)
		binary.LittleEndian.PutUint64(payload, state.AppliedSequence)
		return control(MsgPing, payload)
	}
	reconcile := func(reason string) (bool, bool, error) {
		state.ReconciliationRequired = true
		state.ReconciliationReason = reason
		if err := save(); err != nil {
			return false, wasActive, err
		}
		return false, wasActive, fmt.Errorf("%w: %s", ErrReconciliationRequired, reason)
	}
	for {
		var submissions chan recoverySubmission
		if mode == "active" {
			submissions = c.submissions
		}
		var output chan []byte
		var next []byte
		if len(outgoing) > 0 {
			output = writes
			next = outgoing[0]
		}
		select {
		case <-ctx.Done():
			return false, wasActive, ctx.Err()
		case err := <-writeErrors:
			return true, wasActive, err
		case output <- next:
			outgoing = outgoing[1:]
		case request := <-submissions:
			if len(state.Pending) >= 1024 {
				request.reply <- recoveryReceipt{err: errors.New("pending command limit reached")}
				continue
			}
			if state.NextClientSequence == math.MaxUint64 || state.NextRequestID == math.MaxUint64 {
				return false, wasActive, errors.New("session counters exhausted")
			}
			pending := PendingCommand{RequestID: state.NextRequestID, Sequence: state.NextClientSequence}
			state.NextRequestID++
			state.NextClientSequence++
			pending.Frame = EncodeFrame(request.kind, pending.Sequence, pending.RequestID, request.payload)
			state.Pending = append(state.Pending, pending)
			if err := save(); err != nil {
				request.reply <- recoveryReceipt{err: err}
				return false, wasActive, err
			}
			outgoing = append(outgoing, pending.Frame)
			request.reply <- recoveryReceipt{id: pending.RequestID}
		case <-ticker.C:
			if mode == "active" {
				payload := make([]byte, 8)
				binary.LittleEndian.PutUint64(payload, state.AppliedSequence)
				if err := control(MsgPing, payload); err != nil {
					return false, wasActive, err
				}
			}
		case item := <-reads:
			if item.err != nil {
				return true, wasActive, item.err
			}
			frame := item.frame
			readTimeout := 3 * c.options.Heartbeat
			if readTimeout < 5*time.Second {
				readTimeout = 5 * time.Second
			}
			conn.SetReadDeadline(time.Now().Add(readTimeout))
			if frame.Type == recoveryGap {
				return reconcile("ReplayGap: reconcile executions; GetOpenOrders cannot restore lost settlements")
			}
			if mode == "handshake" {
				if frame.Type != MsgHandshakeAck {
					return false, false, ErrHandshakeFailed
				}
				session, heartbeat, err := parseRecoveryHandshake(frame.Payload)
				if err != nil {
					return false, false, err
				}
				state.SessionID = session
				if heartbeat > 0 && heartbeat/2 < c.options.Heartbeat {
					ticker.Reset(heartbeat / 2)
				}
			} else if mode == "resume" {
				if frame.Type != recoveryResumed || len(frame.Payload) != 4 {
					return reconcile("ResumeSession was rejected")
				}
				replay = binary.LittleEndian.Uint32(frame.Payload)
				mode = "replay"
				if frame.Sequence != 0 {
					return reconcile("SessionResumed must be out of band")
				}
				if replay == 0 {
					if err := activate(); err != nil {
						return false, wasActive, err
					}
				}
				continue
			}
			if frame.Type == MsgError && frame.Sequence == 0 {
				return reconcile("uncorrelated protocol error; mutation result is indeterminate")
			}
			if frame.Sequence > state.AppliedSequence {
				if frame.Sequence != state.AppliedSequence+1 {
					return reconcile("server sequence gap")
				}
				event, err := DecodeEvent(frame.Type, frame.Payload)
				if err != nil && !errors.Is(err, ErrUnknownMessageType) {
					return false, wasActive, err
				}
				frame.Event = event
				application, err := c.options.Apply(ctx, frame, append(json.RawMessage(nil), state.Application...))
				if err != nil {
					return false, wasActive, err
				}
				state.Application = append(json.RawMessage(nil), application...)
				state.AppliedSequence = frame.Sequence
				if frame.Type == MsgPong {
					if len(frame.Payload) != 8 {
						return reconcile("invalid Pong barrier")
					}
					barrier := binary.LittleEndian.Uint64(frame.Payload)
					kept := state.Pending[:0]
					for _, p := range state.Pending {
						if p.Sequence >= barrier {
							kept = append(kept, p)
						}
					}
					state.Pending = kept
				}
				if err := save(); err != nil {
					return false, wasActive, err
				}
				if mode == "active" {
					if err := ack(); err != nil {
						return false, wasActive, err
					}
				}
			} else if frame.Sequence == 0 && frame.Type != MsgPong {
				return reconcile("unexpected out-of-band frame")
			}
			if mode == "handshake" {
				if err := activate(); err != nil {
					return false, wasActive, err
				}
			}
			if mode == "replay" {
				if replay == 0 {
					return reconcile("excess replay frames")
				}
				replay--
				if replay == 0 {
					if err := activate(); err != nil {
						return false, wasActive, err
					}
				}
			}
		}
	}
}
func parseRecoveryHandshake(payload []byte) ([]byte, time.Duration, error) {
	if len(payload) == 0 || payload[0] != ProtocolVersion {
		return nil, 0, ErrHandshakeFailed
	}
	var session []byte
	var capabilities uint64
	var heartbeat time.Duration
	for offset := 1; offset < len(payload); {
		if len(payload)-offset < 4 {
			return nil, 0, ErrInvalidPayloadSize
		}
		tag := binary.LittleEndian.Uint16(payload[offset:])
		size := int(binary.LittleEndian.Uint16(payload[offset+2:]))
		offset += 4
		if len(payload)-offset < size {
			return nil, 0, ErrInvalidPayloadSize
		}
		value := payload[offset : offset+size]
		offset += size
		switch tag {
		case 1:
			if size != 8 {
				return nil, 0, ErrInvalidPayloadSize
			}
			capabilities = binary.LittleEndian.Uint64(value)
		case 2:
			if size != 16 || session != nil {
				return nil, 0, ErrInvalidPayloadSize
			}
			session = append([]byte(nil), value...)
		case 3:
			if size != 4 {
				return nil, 0, ErrInvalidPayloadSize
			}
			heartbeat = time.Duration(binary.LittleEndian.Uint32(value)) * time.Millisecond
		}
	}
	if len(session) != 16 || bytes.Equal(session, make([]byte, 16)) || capabilities&67 != 67 {
		return nil, 0, fmt.Errorf("%w: server lacks durable recovery capabilities", ErrHandshakeFailed)
	}
	return session, heartbeat, nil
}
