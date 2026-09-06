package meclient

import (
	"net"
	"testing"
	"time"
)

func TestCloseInterruptsPendingPing(t *testing.T) {
	clientConn, serverConn := net.Pipe()
	defer serverConn.Close()
	defer clientConn.Close()
	client := NewClientFromConn(clientConn)
	pingDone := make(chan error, 1)
	go func() { pingDone <- client.Ping() }()
	if _, _, _, _, err := ReadFrame(serverConn); err != nil {
		t.Fatal(err)
	}
	closeDone := make(chan error, 1)
	go func() { closeDone <- client.Close() }()
	select {
	case err := <-closeDone:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("Close cannot interrupt Ping waiting for the peer")
	}
	select {
	case err := <-pingDone:
		if err == nil {
			t.Fatal("interrupted Ping unexpectedly succeeded")
		}
	case <-time.After(time.Second):
		t.Fatal("Ping remains blocked after Close")
	}
}

func TestReadEventsContinuesAfterFlowControlWarning(t *testing.T) {
	clientConn, serverConn := net.Pipe()
	defer serverConn.Close()
	client := NewClientFromConn(clientConn)
	defer client.Close()
	events, errs := client.ReadEvents()
	go func() {
		serverConn.Write(EncodeFrame(0x23, 800, 0, []byte{80, 0}))
		serverConn.Write(EncodeFrame(MsgOrderAccepted, 801, 10, make([]byte, 16)))
		serverConn.Close()
	}()
	count := 0
	for range events {
		count++
	}
	for err := range errs {
		t.Errorf("stream stopped on a valid control frame: %v", err)
	}
	if count != 2 {
		t.Errorf("received %d events, want warning and order accepted", count)
	}
}

func TestCloseStopsReadEventsWithFullChannel(t *testing.T) {
	clientConn, serverConn := net.Pipe()
	defer serverConn.Close()
	client := NewClientFromConn(clientConn)
	_, errs := client.ReadEvents()
	sent := make(chan struct{})
	go func() {
		for seq := uint64(1); seq <= 65; seq++ {
			if _, err := serverConn.Write(EncodeFrame(MsgOrderAccepted, seq, seq, make([]byte, 16))); err != nil {
				return
			}
		}
		close(sent)
	}()
	select {
	case <-sent:
	case <-time.After(time.Second):
		t.Fatal("frames were not read")
	}
	if err := client.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case _, ok := <-errs:
		if ok {
			t.Fatal("closing client unexpectedly returned a stream error")
		}
	case <-time.After(time.Second):
		t.Fatal("ReadEvents remains blocked on the full event channel after Close")
	}
}

func TestZeroValueClose(t *testing.T) {
	var client Client
	if err := client.Close(); err != nil {
		t.Fatal(err)
	}
}
