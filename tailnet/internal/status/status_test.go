package status

import (
	"encoding/json"
	"strings"
	"testing"
)

func TestHostnames(t *testing.T) {
	for name, ok := range map[string]bool{
		"thurm": true, "my-mac-2": true, "": false, "-mac": false, "mac-": false,
		"my.mac": false, strings.Repeat("a", 63): true, strings.Repeat("a", 64): false,
	} {
		if ValidHostname(name) != ok {
			t.Errorf("ValidHostname(%q) = %v", name, !ok)
		}
	}
}

func TestBackendStates(t *testing.T) {
	for in, want := range map[string]string{
		"NeedsLogin": "needs_login", "NeedsMachineAuth": "needs_machine_auth",
		"Running": "running", "Stopped": "stopped", "NoState": "starting", "Starting": "starting",
	} {
		if got := FromBackend(in); got != want {
			t.Errorf("FromBackend(%q) = %q, want %q", in, got, want)
		}
	}
}

// The daemon decodes this with serde: field names must match TailscaleStatus.
func TestJSONFieldNames(t *testing.T) {
	b, err := json.Marshal(Status{State: "off", HTTPS: HTTPS{State: "unavailable"}})
	if err != nil {
		t.Fatal(err)
	}
	var m map[string]any
	json.Unmarshal(b, &m)
	for _, k := range []string{"state", "error", "hostname", "dns_name", "auth_url", "account",
		"tailnet_name", "allow_tailnet_members", "tags", "https", "url", "host_urls", "serve_socket"} {
		if _, ok := m[k]; !ok {
			t.Errorf("missing %q in %s", k, b)
		}
	}
}
