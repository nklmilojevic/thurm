package node

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/netip"
	"testing"
	"time"

	"github.com/coder/websocket"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
	"github.com/nklmilojevic/thurm/tailnet/internal/gateway"
)

func TestPlainURLs(t *testing.T) {
	ips := []netip.Addr{netip.MustParseAddr("fd7a:115c::1"), netip.MustParseAddr("100.64.0.7")}
	v6Only := []netip.Addr{netip.MustParseAddr("fd7a:115c::1")}
	cases := []struct {
		name, listen, dns string
		ips               []netip.Addr
		want              []string
	}{
		{"every address", ":7297", "thurm.hs.example", ips, []string{
			"ws://100.64.0.7:7297/v1/connect", "ws://[fd7a:115c::1]:7297/v1/connect",
			"ws://thurm.hs.example:7297/v1/connect"}},
		{"no name yet", ":7297", "", ips, []string{
			"ws://100.64.0.7:7297/v1/connect", "ws://[fd7a:115c::1]:7297/v1/connect"}},
		{"ipv6 only, no name", ":7297", "", v6Only, []string{"ws://[fd7a:115c::1]:7297/v1/connect"}},
		{"one address, no name advertised", "100.64.0.7:7297", "thurm.hs.example", ips, []string{
			"ws://100.64.0.7:7297/v1/connect"}},
		{"one ipv6 address", "[fd7a:115c::1]:7297", "thurm.hs.example", ips, []string{
			"ws://[fd7a:115c::1]:7297/v1/connect"}},
		{"address not the node's", "100.64.0.99:7297", "thurm.hs.example", ips, nil},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			got := plainURLs(c.listen, c.ips, c.dns)
			if len(got) != len(c.want) {
				t.Fatalf("got %v, want %v", got, c.want)
			}
			for i := range got {
				if got[i] != c.want[i] {
					t.Fatalf("got %v, want %v", got, c.want)
				}
			}
		})
	}
	if got := plainURLs("not-a-port", ips, "x"); got != nil {
		t.Fatalf("bad listen gave %v", got)
	}
}

func TestCoordinationServer(t *testing.T) {
	cfg := Config{ControlURL: "https://headscale.example"}
	if got := (&Node{}).coordination(cfg); got != "https://headscale.example" {
		t.Fatalf("configured server %q", got)
	}
	if got := (&Node{}).coordination(Config{}); got != "" {
		t.Fatalf("default should be Tailscale's, got %q", got)
	}
	if got := (&Node{ControlURL: "http://test"}).coordination(cfg); got != "http://test" {
		t.Fatalf("test override lost: %q", got)
	}
	n := &Node{cfg: cfg}
	if got := n.adminWhere(); got != "on your coordination server" {
		t.Fatalf("admin hint %q", got)
	}
}

// The plain listener serves the gateway like HTTPS does: an unidentified caller is refused, and
// a browser page (which always sends an Origin) is refused even when identified.
func TestPlainListenerAdmitsLikeHTTPS(t *testing.T) {
	cases := []struct {
		name     string
		identify error
		origin   string
		want     int
	}{
		{"identified app", nil, "", http.StatusServiceUnavailable},
		{"unidentified", errors.New("no such peer"), "", http.StatusForbidden},
		{"browser page", nil, "https://elsewhere.example", http.StatusForbidden},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			gw := &gateway.Gateway{
				Via: "node",
				Identify: func(*http.Request) (admit.Caller, error) {
					return admit.Caller{LoginName: "owner@example.com"}, c.identify
				},
				Policy: func() admit.Policy { return admit.Policy{AllowAll: true} },
				Dial: func(context.Context) (net.Conn, error) {
					return nil, errors.New("no daemon in this test")
				},
				Logf: t.Logf,
			}
			ln, err := net.Listen("tcp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			n := &Node{Logf: t.Logf, gen: 1}
			n.servePlain(1, ln, gw)
			defer ln.Close()
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			opts := &websocket.DialOptions{}
			if c.origin != "" {
				opts.HTTPHeader = http.Header{"Origin": []string{c.origin}}
			}
			url := "ws://" + ln.Addr().String() + gateway.ConnectPath
			_, resp, err := websocket.Dial(ctx, url, opts)
			if resp == nil {
				t.Fatalf("no answer: %v", err)
			}
			if resp.StatusCode != c.want {
				t.Fatalf("status %d, want %d", resp.StatusCode, c.want)
			}
		})
	}
}
