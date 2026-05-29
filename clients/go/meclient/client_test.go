package meclient

import (
	"encoding/binary"
	"io"
	"net"
	"testing"
)

// A command must go out as a v2 frame (version byte 2) carrying a non-zero
// request_id: the v2 server rejects inbound commands whose request_id is 0
// (InvalidRequestId), so a v1-shaped frame could not place orders at all.
func TestCommandFrameIsV2WithNonZeroRequestID(t *testing.T) {
	cConn, sConn := net.Pipe()
	t.Cleanup(func() { _ = cConn.Close(); _ = sConn.Close() })
	client := NewClientFromConn(cConn)

	type sendResult struct {
		reqID uint64
		err   error
	}
	sent := make(chan sendResult, 1)
	go func() {
		reqID, err := client.PlaceOrder(&PlaceOrderRequest{OrderId: 1, Qty: NewDecimal(1.0)})
		sent <- sendResult{reqID, err}
	}()

	// Read the frame the client wrote.
	var hdr [frameHeaderSize]byte
	if _, err := io.ReadFull(sConn, hdr[:]); err != nil {
		t.Fatalf("read header: %v", err)
	}
	if plen := binary.LittleEndian.Uint32(hdr[0:4]); plen > 0 {
		if _, err := io.CopyN(io.Discard, sConn, int64(plen)); err != nil {
			t.Fatalf("drain payload: %v", err)
		}
	}
	res := <-sent
	if res.err != nil {
		t.Fatalf("PlaceOrder: %v", res.err)
	}

	if hdr[4] != 2 {
		t.Fatalf("frame version = %d, want 2", hdr[4])
	}
	if hdr[5] != MsgPlaceOrder {
		t.Fatalf("msg type = 0x%02x, want PlaceOrder", hdr[5])
	}
	wireReqID := binary.LittleEndian.Uint64(hdr[14:22])
	if wireReqID == 0 {
		t.Fatal("command request_id must be non-zero (v2 server rejects 0)")
	}
	if wireReqID != res.reqID {
		t.Fatalf("wire request_id %d != returned %d", wireReqID, res.reqID)
	}
}

// Connection-management frames (Ping/Handshake) are not inbound commands, so
// they carry request_id 0.
func TestPingFrameCarriesZeroRequestID(t *testing.T) {
	cConn, sConn := net.Pipe()
	t.Cleanup(func() { _ = cConn.Close(); _ = sConn.Close() })
	client := NewClientFromConn(cConn)

	type pingFrame struct {
		ver   byte
		reqID uint64
		err   error
	}
	got := make(chan pingFrame, 1)
	go func() {
		var hdr [frameHeaderSize]byte
		if _, err := io.ReadFull(sConn, hdr[:]); err != nil {
			got <- pingFrame{err: err}
			return
		}
		// Reply with a Pong (no payload) so client.Ping() returns.
		_, werr := sConn.Write(EncodeFrame(MsgPong, 1, 0, nil))
		got <- pingFrame{
			ver:   hdr[4],
			reqID: binary.LittleEndian.Uint64(hdr[14:22]),
			err:   werr,
		}
	}()

	if err := client.Ping(); err != nil {
		t.Fatalf("Ping: %v", err)
	}
	pf := <-got
	if pf.err != nil {
		t.Fatalf("server side: %v", pf.err)
	}
	if pf.ver != 2 {
		t.Fatalf("ping frame version = %d, want 2", pf.ver)
	}
	if pf.reqID != 0 {
		t.Fatalf("ping request_id = %d, want 0", pf.reqID)
	}
}
