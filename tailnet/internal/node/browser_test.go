package node

import (
	"testing"

	"tailscale.com/ipn/ipnstate"

	"github.com/nklmilojevic/thurm/tailnet/internal/gateway"
	"github.com/nklmilojevic/thurm/tailnet/internal/status"
)

func browserNode(web bool, https string, dnsName string) (*Node, *gateway.Gateway) {
	gw := &gateway.Gateway{}
	n := &Node{
		gw:    gw,
		cfg:   Config{Web: web},
		https: status.HTTPS{State: https},
		st:    &ipnstate.Status{Self: &ipnstate.PeerStatus{DNSName: dnsName}},
	}
	return n, gw
}

// The node's own origin is allowed as soon as its name is known; the client is served only with
// Web on and HTTPS ready.
func TestBrowserOriginAndClient(t *testing.T) {
	cases := []struct {
		name       string
		web        bool
		https, dns string
		origin     string
		client     bool
	}{
		{"web on, https ready", true, "ready", "thurm.tail1234.ts.net.", "https://thurm.tail1234.ts.net", true},
		{"web on, https not ready", true, "provisioning", "thurm.tail1234.ts.net.", "https://thurm.tail1234.ts.net", false},
		{"web off", false, "ready", "thurm.tail1234.ts.net.", "https://thurm.tail1234.ts.net", false},
		{"no name yet", true, "ready", "", "", false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			n, gw := browserNode(c.web, c.https, c.dns)
			n.serveBrowser()
			if gw.Origin != c.origin {
				t.Fatalf("origin %q, want %q", gw.Origin, c.origin)
			}
			if (gw.Web != nil) != c.client {
				t.Fatalf("client served %v, want %v", gw.Web != nil, c.client)
			}
		})
	}
}
