package meclient

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

type memorySessions struct {
	mu    sync.Mutex
	state SessionState
}

func (s *memorySessions) Load(context.Context) (SessionState, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	data, _ := json.Marshal(s.state)
	var result SessionState
	err := json.Unmarshal(data, &result)
	return result, err
}
func (s *memorySessions) Save(_ context.Context, state SessionState) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	data, err := json.Marshal(state)
	if err != nil {
		return err
	}
	return json.Unmarshal(data, &s.state)
}
func recoveryHello(token []byte) []byte {
	payload := []byte{2}
	tlv := func(tag uint16, value []byte) {
		var h [4]byte
		binary.LittleEndian.PutUint16(h[:], tag)
		binary.LittleEndian.PutUint16(h[2:], uint16(len(value)))
		payload = append(payload, h[:]...)
		payload = append(payload, value...)
	}
	caps := make([]byte, 8)
	binary.LittleEndian.PutUint64(caps, 67)
	tlv(1, caps)
	tlv(2, token)
	return payload
}

type replayPeer struct {
	mu       sync.Mutex
	token    []byte
	sequence uint64
	frames   [][]byte
	commands map[uint64][]byte
	original map[uint64][]byte
	retries  int
	warning  bool
	store    *memorySessions
	errs     chan error
}

func (p *replayPeer) frame(kind uint8, id uint64, payload []byte) []byte {
	p.sequence++
	frame := EncodeFrame(kind, p.sequence, id, payload)
	p.frames = append(p.frames, frame)
	return frame
}
func (p *replayPeer) serve(conn net.Conn) {
	defer conn.Close()
	last := uint64(0)
	for {
		kind, seq, id, payload, err := ReadFrame(conn)
		if err != nil {
			return
		}
		if seq <= last && kind != MsgHandshake {
			p.errs <- fmt.Errorf("client sequence regressed: %d after %d", seq, last)
			return
		}
		last = seq
		p.mu.Lock()
		var output [][]byte
		disconnect := false
		switch kind {
		case MsgHandshake:
			output = append(output, p.frame(MsgHandshakeAck, 0, recoveryHello(p.token)))
		case recoveryResume:
			if len(payload) != 24 || string(payload[:16]) != string(p.token) {
				p.mu.Unlock()
				p.errs <- errors.New("wrong resumed identity")
				return
			}
			after := binary.LittleEndian.Uint64(payload[16:])
			var replay [][]byte
			for _, frame := range p.frames {
				if binary.LittleEndian.Uint64(frame[6:14]) > after {
					replay = append(replay, frame)
				}
			}
			count := make([]byte, 4)
			binary.LittleEndian.PutUint32(count, uint32(len(replay)))
			output = append(output, EncodeFrame(recoveryResumed, 0, 0, count))
			output = append(output, replay...)
		case MsgPing:
			body := make([]byte, 8)
			binary.LittleEndian.PutUint64(body, seq)
			output = append(output, p.frame(MsgPong, 0, body))
			if !p.warning {
				p.warning = true
				output = append(output, p.frame(MsgFlowControlWarning, 0, []byte{80, 0}))
			}
		case recoveryACK:
			ack := binary.LittleEndian.Uint64(payload)
			state, _ := p.store.Load(context.Background())
			if ack > state.AppliedSequence {
				p.mu.Unlock()
				p.errs <- errors.New("ACK preceded durable application checkpoint")
				return
			}
		case MsgPlaceOrder:
			current := EncodeFrame(kind, seq, id, payload)
			if old, ok := p.original[id]; ok {
				if string(old) != string(current) {
					p.mu.Unlock()
					p.errs <- errors.New("pending bytes changed across reconnect")
					return
				}
				p.retries++
				output = append(output, p.commands[id])
			} else {
				p.original[id] = current
				accepted := p.frame(MsgOrderAccepted, id, make([]byte, 16))
				filled := p.frame(MsgOrderFilled, id, make([]byte, 32))
				p.commands[id] = append(append([]byte(nil), accepted...), filled...)
				disconnect = true
			}
		default:
			p.mu.Unlock()
			p.errs <- fmt.Errorf("unexpected client frame 0x%x", kind)
			return
		}
		p.mu.Unlock()
		if disconnect {
			return
		}
		for _, frame := range output {
			if _, err := conn.Write(frame); err != nil {
				return
			}
		}
	}
}
func until(t *testing.T, predicate func() bool) {
	t.Helper()
	end := time.Now().Add(5 * time.Second)
	for time.Now().Before(end) {
		if predicate() {
			return
		}
		time.Sleep(5 * time.Millisecond)
	}
	t.Fatal("recovery condition timed out")
}
func TestRecoveringClientReplaysAppliesAndAcknowledgesOneStream(t *testing.T) {
	store := &memorySessions{state: FreshSessionState()}
	peer := &replayPeer{token: []byte("1234567890123456"), commands: map[uint64][]byte{}, original: map[uint64][]byte{}, store: store, errs: make(chan error, 10)}
	apply := func(_ context.Context, frame RecoveryFrame, previous json.RawMessage) (json.RawMessage, error) {
		counts := map[uint8]int{}
		if len(previous) > 0 {
			if err := json.Unmarshal(previous, &counts); err != nil {
				return nil, err
			}
		}
		if counts == nil {
			counts = map[uint8]int{}
		}
		counts[frame.Type]++
		return json.Marshal(counts)
	}
	client, err := NewRecoveringClient(RecoveryOptions{Endpoint: "test-engine", Store: store, Apply: apply, Heartbeat: 20 * time.Millisecond, Dial: func(context.Context) (net.Conn, error) {
		local, remote := net.Pipe()
		go peer.serve(remote)
		return local, nil
	}})
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 8*time.Second)
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- client.Run(ctx) }()
	request := &PlaceOrderRequest{OrderId: 1, Qty: Decimal{raw: 100000000}, Price: Decimal{raw: 10000000000}}
	id, err := client.PlaceOrder(ctx, request)
	if err != nil {
		t.Fatal(err)
	}
	if id == 0 {
		t.Fatal("zero request ID")
	}
	until(t, func() bool {
		state, _ := store.Load(ctx)
		peer.mu.Lock()
		retries := peer.retries
		peer.mu.Unlock()
		return len(state.Pending) == 0 && retries > 0
	})
	state, _ := store.Load(ctx)
	var counts map[uint8]int
	if err = json.Unmarshal(state.Application, &counts); err != nil {
		t.Fatal(err)
	}
	if counts[MsgOrderAccepted] != 1 || counts[MsgOrderFilled] != 1 || counts[MsgFlowControlWarning] != 1 || counts[MsgHandshakeAck] != 1 || counts[MsgPong] == 0 {
		t.Fatalf("wrong applied stream: %v", counts)
	}
	select {
	case err := <-peer.errs:
		t.Fatal(err)
	default:
	}
	cancel()
	if err = <-done; !errors.Is(err, context.Canceled) {
		t.Fatalf("run stop: %v", err)
	}
}
func TestReplayGapPersistsReconciliationWithoutNewSession(t *testing.T) {
	state := FreshSessionState()
	state.SessionID = []byte("1234567890123456")
	state.AppliedSequence = 5
	store := &memorySessions{state: state}
	dials := 0
	client, _ := NewRecoveringClient(RecoveryOptions{Endpoint: "engine", Store: store, Apply: func(context.Context, RecoveryFrame, json.RawMessage) (json.RawMessage, error) {
		return nil, errors.New("must not apply replay gap")
	}, Dial: func(context.Context) (net.Conn, error) {
		dials++
		local, remote := net.Pipe()
		go func() {
			defer remote.Close()
			kind, _, _, _, _ := ReadFrame(remote)
			if kind != recoveryResume {
				t.Error("silently created a new session")
			}
			remote.Write(EncodeFrame(recoveryGap, 1, 0, make([]byte, 16)))
		}()
		return local, nil
	}})
	if err := client.Run(context.Background()); !errors.Is(err, ErrReconciliationRequired) {
		t.Fatalf("gap result: %v", err)
	}
	if err := client.Run(context.Background()); !errors.Is(err, ErrReconciliationRequired) {
		t.Fatalf("lost reconciliation latch: %v", err)
	}
	if dials != 1 {
		t.Fatalf("redialed unreconciled session %d times", dials)
	}
}
func TestFileSessionStoreAtomicPrivateAndExclusive(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.json")
	store, err := OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = OpenFileSessionStore(path); err == nil {
		t.Fatal("two owners acquired the session")
	}
	state, err := store.Load(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	state.SessionID = []byte("1234567890123456")
	state.AppliedSequence = 9
	state.Application = json.RawMessage(`{"balance":"0.00000000000000000001"}`)
	if err = store.Save(context.Background(), state); err != nil {
		t.Fatal(err)
	}
	info, _ := os.Stat(path)
	if info.Mode().Perm() != 0600 {
		t.Fatalf("state permissions: %v", info.Mode())
	}
	store.Close()
	store, err = OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	recovered, _ := store.Load(context.Background())
	if recovered.AppliedSequence != 9 || string(recovered.Application) != string(state.Application) {
		t.Fatal("checkpoint changed")
	}
	store.Close()
	os.Remove(path)
	if _, err = OpenFileSessionStore(path); !errors.Is(err, ErrReconciliationRequired) {
		t.Fatalf("missing state silently reset: %v", err)
	}
}

func TestSessionStoreCrashChild(t *testing.T) {
	path := os.Getenv("ME_TEST_SESSION_CRASH_PATH")
	if path == "" {
		t.Skip("subprocess crash helper")
	}
	store, err := OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	state, err := store.Load(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	state.AppliedSequence = 99
	store.beforeRename = func() error { os.Exit(93); return nil }
	_ = store.Save(context.Background(), state)
	t.Fatal("store failpoint was not reached")
}
func TestSessionStoreCrashBeforeRenameRetainsCheckpoint(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.json")
	store, err := OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	state, _ := store.Load(context.Background())
	state.SessionID = []byte("1234567890123456")
	state.AppliedSequence = 7
	if err = store.Save(context.Background(), state); err != nil {
		t.Fatal(err)
	}
	store.Close()
	command := exec.Command(os.Args[0], "-test.run=^TestSessionStoreCrashChild$")
	command.Env = append(os.Environ(), "ME_TEST_SESSION_CRASH_PATH="+path)
	err = command.Run()
	var exited *exec.ExitError
	if !errors.As(err, &exited) || exited.ExitCode() != 93 {
		t.Fatalf("wrong crash result: %v", err)
	}
	recovered, err := OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	defer recovered.Close()
	state, err = recovered.Load(context.Background())
	if err != nil || state.AppliedSequence != 7 {
		t.Fatalf("lost previous atomic checkpoint: %+v %v", state, err)
	}
}
func TestSessionStoreDetectsReplacedLock(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.json")
	store, err := OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	state, _ := store.Load(context.Background())
	if err = os.Rename(path+".lock", path+".old-lock"); err != nil {
		t.Fatal(err)
	}
	second, err := OpenFileSessionStore(path)
	if err != nil {
		t.Fatal(err)
	}
	defer second.Close()
	if err = store.Save(context.Background(), state); !errors.Is(err, ErrReconciliationRequired) {
		t.Fatalf("replaced lock accepted a write: %v", err)
	}
	if _, err = store.Load(context.Background()); !errors.Is(err, ErrReconciliationRequired) {
		t.Fatalf("replaced lock accepted a read: %v", err)
	}
}

func TestRecoveringReplayDeadlineTracksProgress(t *testing.T) {
	state := FreshSessionState()
	state.SessionID = []byte("1234567890123456")
	store := &memorySessions{state: state}
	ctx, cancel := context.WithTimeout(context.Background(), 16*time.Second)
	defer cancel()
	client, err := NewRecoveringClient(RecoveryOptions{Endpoint: "slow-replay", Store: store, MaxReconnectAttempts: 1,
		Apply: func(_ context.Context, _ RecoveryFrame, s json.RawMessage) (json.RawMessage, error) { return s, nil },
		Dial: func(context.Context) (net.Conn, error) {
			local, remote := net.Pipe()
			go func() {
				defer remote.Close()
				kind, _, _, _, err := ReadFrame(remote)
				if err != nil || kind != recoveryResume {
					return
				}
				count := make([]byte, 4)
				binary.LittleEndian.PutUint32(count, 12)
				if _, err = remote.Write(EncodeFrame(recoveryResumed, 0, 0, count)); err != nil {
					return
				}
				for seq := uint64(1); seq <= 12; seq++ {
					select {
					case <-ctx.Done():
						return
					case <-time.After(900 * time.Millisecond):
					}
					if _, err = remote.Write(EncodeFrame(MsgFlowControlWarning, seq, 0, []byte{80, 0})); err != nil {
						return
					}
				}
				for {
					if _, _, _, _, err = ReadFrame(remote); err != nil {
						return
					}
				}
			}()
			return local, nil
		}})
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- client.Run(ctx) }()
	for !client.Ready() {
		select {
		case err := <-done:
			t.Fatalf("continuous replay was interrupted: %v", err)
		case <-ctx.Done():
			t.Fatal(ctx.Err())
		case <-time.After(20 * time.Millisecond):
		}
	}
	applied, err := store.Load(ctx)
	if err != nil || applied.AppliedSequence != 12 {
		t.Fatalf("incomplete replay: %+v %v", applied, err)
	}
	cancel()
	select {
	case <-done:
	case <-time.After(time.Second):
		t.Fatal("client failed to stop")
	}
}
