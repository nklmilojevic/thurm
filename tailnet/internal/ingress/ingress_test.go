package ingress

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
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
