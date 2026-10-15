package node

import (
	"testing"

	"github.com/nklmilojevic/thurm/tailnet/internal/status"
)

// Host-only serving never enables the node; its status must still be one the daemon reads.
func TestNeverEnabledReportsHTTPSUnavailable(t *testing.T) {
	var s status.Status
	(&Node{}).Status(&s)
	if s.State != "off" || s.HTTPS.State != "unavailable" {
		t.Fatalf("got state %q, https %q", s.State, s.HTTPS.State)
	}
}
