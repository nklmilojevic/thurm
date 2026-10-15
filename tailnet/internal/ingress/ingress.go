// Package ingress starts a connection on the daemon's socket for a tailnet caller.
//
// The preamble is the daemon's `thurm_proto::tailnet::write_ingress`: the magic u32
// 0xFFFFFFFF (little-endian; never a frame length), the JSON length as a u32, then the
// caller's Principal as JSON. The daemon reads it before the first frame.
package ingress

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
)

const (
	magic = 0xFFFFFFFF
	// MaxLen is the daemon's MAX_INGRESS.
	MaxLen = 64 * 1024
)

// Principal mirrors thurm_proto::tailnet::Principal.
type Principal struct {
	Kind       string   `json:"kind"`
	Display    string   `json:"display"`
	UserID     *int64   `json:"user_id,omitempty"`
	LoginName  string   `json:"login_name,omitempty"`
	Node       string   `json:"node,omitempty"`
	Tags       []string `json:"tags"`
	RemoteAddr string   `json:"remote_addr,omitempty"`
	Via        string   `json:"via"`
}

// FromCaller describes an admitted caller; via is the listener ("node", "host" or "serve").
func FromCaller(c admit.Caller, via string) Principal {
	p := Principal{
		Kind:       "tailscale",
		LoginName:  c.LoginName,
		Node:       c.Node,
		Tags:       c.Tags,
		RemoteAddr: c.RemoteAddr,
		Via:        via,
	}
	if p.Tags == nil {
		p.Tags = []string{}
	}
	if c.UserID != 0 {
		id := c.UserID
		p.UserID = &id
	}
	switch {
	case c.Tagged():
		p.Display = c.Node
	case c.LoginName != "":
		p.Display = c.LoginName
	default:
		p.Display = c.DisplayName
	}
	if p.Display == "" {
		p.Display = c.RemoteAddr
	}
	return p
}

// Write writes the preamble.
func Write(w io.Writer, p Principal) error {
	body, err := json.Marshal(p)
	if err != nil {
		return err
	}
	if len(body) > MaxLen {
		return errors.New("ingress preamble too large")
	}
	buf := make([]byte, 8, 8+len(body))
	binary.LittleEndian.PutUint32(buf[0:4], magic)
	binary.LittleEndian.PutUint32(buf[4:8], uint32(len(body)))
	_, err = w.Write(append(buf, body...))
	return err
}
