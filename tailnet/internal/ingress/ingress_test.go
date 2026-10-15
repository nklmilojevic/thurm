package ingress

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"testing"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
)

func TestWrite(t *testing.T) {
	p := FromCaller(admit.Caller{UserID: 42, LoginName: "me@example.com", Node: "phone", RemoteAddr: "100.64.0.2:5000"}, "node")
	var buf bytes.Buffer
	if err := Write(&buf, p); err != nil {
		t.Fatal(err)
	}
	b := buf.Bytes()
	if got := binary.LittleEndian.Uint32(b[0:4]); got != 0xFFFFFFFF {
		t.Fatalf("magic %#x", got)
	}
	n := binary.LittleEndian.Uint32(b[4:8])
	if int(n) != len(b)-8 {
		t.Fatalf("length %d, body %d", n, len(b)-8)
	}
	var got map[string]any
	if err := json.Unmarshal(b[8:], &got); err != nil {
		t.Fatal(err)
	}
	if got["kind"] != "tailscale" || got["display"] != "me@example.com" || got["user_id"] != float64(42) || got["via"] != "node" {
		t.Fatalf("principal %v", got)
	}
}

func TestTaggedDisplaysTheNode(t *testing.T) {
	p := FromCaller(admit.Caller{Node: "ci-runner", Tags: []string{"tag:ci"}}, "host")
	if p.Display != "ci-runner" || p.UserID != nil {
		t.Fatalf("%+v", p)
	}
}

// shortWriter takes at most three bytes a call, like a socket under pressure.
type shortWriter struct{ bytes.Buffer }

func (w *shortWriter) Write(p []byte) (int, error) {
	return w.Buffer.Write(p[:min(len(p), 3)])
}

func TestWriteSendsEveryByteThroughShortWrites(t *testing.T) {
	var short shortWriter
	var whole bytes.Buffer
	p := Principal{Kind: "tailscale", Display: "me@example.com", Via: "node"}
	if err := Write(&short, p); err != nil {
		t.Fatal(err)
	}
	if err := Write(&whole, p); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(short.Bytes(), whole.Bytes()) {
		t.Fatalf("got %d bytes, want %d", short.Len(), whole.Len())
	}
}

type stuckWriter struct{}

func (stuckWriter) Write([]byte) (int, error) { return 0, nil }

func TestWriteFailsWhenNothingIsWritten(t *testing.T) {
	if err := Write(stuckWriter{}, Principal{Kind: "tailscale"}); !errors.Is(err, io.ErrShortWrite) {
		t.Fatalf("got %v", err)
	}
}
