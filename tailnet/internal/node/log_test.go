package node

import (
	"fmt"
	"strings"
	"testing"
)

func TestUserLogfDropsTheSignInPrompt(t *testing.T) {
	var got []string
	n := &Node{Logf: func(format string, args ...any) { got = append(got, fmt.Sprintf(format, args...)) }}
	n.userLogf("To start this tsnet server, restart with TS_AUTHKEY set, or go to: %s", "https://login.tailscale.com/a/abc")
	n.userLogf("AuthLoop: state is %s; done", "Starting")
	if len(got) != 1 || strings.Contains(got[0], "login.tailscale.com") {
		t.Fatalf("got %q", got)
	}
}
