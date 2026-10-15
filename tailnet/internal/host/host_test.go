package host

import (
	"context"
	"encoding/json"
	"net"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"testing"

	"tailscale.com/ipn"
	"tailscale.com/tailcfg"
)

func TestServeVersion(t *testing.T) {
	for v, ok := range map[string]bool{
		"1.94.0": true, "1.94.0-t1234abcd-g5678": true, "1.104.1": true,
		"1.93.0-pre": false, "1.44.0": false, "1.42.0-tabc": false, "": false,
	} {
		if err := CheckServeVersion(v); (err == nil) != ok {
			t.Errorf("CheckServeVersion(%q) = %v", v, err)
		}
	}
}

func TestRawForward(t *testing.T) {
	sock := filepath.Join(t.TempDir(), "tailnet-serve.sock")
	raw := map[uint16]*ipn.TCPPortHandler{443: {TCPForward: "unix:" + sock, TerminateTLS: "thurm.example.ts.net"}}
	proxied := &ipn.ServeConfig{
		TCP: map[uint16]*ipn.TCPPortHandler{443: {HTTPS: true}, 22: {TCPForward: "127.0.0.1:22"}, 80: nil},
		Web: map[ipn.HostPort]*ipn.WebServerConfig{"thurm.example.ts.net:443": {
			Handlers: map[string]*ipn.HTTPHandler{"/": {Proxy: "unix:" + sock}},
		}},
	}
	for name, tc := range map[string]struct {
		cfg  *ipn.ServeConfig
		want string
	}{
		"nil":          {nil, ""},
		"http proxy":   {proxied, ""},
		"other socket": {&ipn.ServeConfig{TCP: map[uint16]*ipn.TCPPortHandler{443: {TCPForward: "unix:/tmp/other.sock"}}}, ""},
		"raw":          {&ipn.ServeConfig{TCP: raw}, "port 443"},
		"unclean path": {&ipn.ServeConfig{TCP: map[uint16]*ipn.TCPPortHandler{7: {TCPForward: "unix:" + filepath.Dir(sock) + "/./tailnet-serve.sock"}}}, "port 7"},
		"service":      {&ipn.ServeConfig{Services: map[tailcfg.ServiceName]*ipn.ServiceConfig{"svc:thurm": {TCP: raw}}}, "service svc:thurm port 443"},
		"foreground":   {&ipn.ServeConfig{Foreground: map[string]*ipn.ServeConfig{"session": {TCP: raw}}}, "foreground port 443"},
	} {
		if got := RawForward(tc.cfg, sock); got != tc.want {
			t.Errorf("%s: RawForward = %q, want %q", name, got, tc.want)
		}
	}
}

// fakeLocalAPI answers this machine's serve config as *cfg, or fails while it is nil.
func fakeLocalAPI(t *testing.T, h *Host, cfg **ipn.ServeConfig) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/localapi/v0/serve-config" || *cfg == nil {
			http.Error(w, "nope", http.StatusInternalServerError)
			return
		}
		json.NewEncoder(w).Encode(*cfg)
	}))
	t.Cleanup(srv.Close)
	h.lc.Dial = func(ctx context.Context, _, _ string) (net.Conn, error) {
		var d net.Dialer
		return d.DialContext(ctx, "tcp", srv.Listener.Addr().String())
	}
}

func TestFromHeaders(t *testing.T) {
	req := func(h map[string][]string) *http.Request {
		r, _ := http.NewRequest("GET", "/v1/connect", nil)
		for k, vs := range h {
			for _, v := range vs {
				r.Header.Add(k, v)
			}
		}
		return r
	}
	owner := map[string][]string{"Tailscale-User-Login": {"owner@example.com"}}
	h := &Host{ServePath: filepath.Join(t.TempDir(), "tailnet-serve.sock")}
	cfg := &ipn.ServeConfig{}
	fakeLocalAPI(t, h, &cfg)
	c, err := h.fromHeaders(req(map[string][]string{
		"Tailscale-User-Login": {"=?utf-8?q?j=C3=B6rg@example.com?="},
		"Tailscale-User-Name":  {"=?utf-8?q?J=C3=B6rg?="},
		"X-Forwarded-For":      {"100.64.0.2"},
	}))
	if err != nil || c.LoginName != "jörg@example.com" || c.DisplayName != "Jörg" || c.RemoteAddr != "100.64.0.2" {
		t.Fatalf("got %+v, %v", c, err)
	}
	for name, hdr := range map[string]map[string][]string{
		"none":     {},
		"empty":    {"Tailscale-User-Login": {""}},
		"two":      {"Tailscale-User-Login": {"me@example.com", "owner@example.com"}},
		"funnel":   {"Tailscale-User-Login": {"owner@example.com"}, "Tailscale-Funnel-Request": {"?1"}},
		"bad word": {"Tailscale-User-Login": {"=?x-unknown?q?a?="}},
	} {
		if c, err := h.fromHeaders(req(hdr)); err == nil {
			t.Errorf("%s: admitted %+v", name, c)
		}
	}

	// Raw TCP to the socket passes a forged login through: refused, as is an unreadable config.
	cfg = &ipn.ServeConfig{TCP: map[uint16]*ipn.TCPPortHandler{443: {TCPForward: "unix:" + h.ServePath}}}
	if c, err := h.fromHeaders(req(owner)); err == nil {
		t.Errorf("raw forward: admitted %+v", c)
	}
	cfg = nil
	if c, err := h.fromHeaders(req(owner)); err == nil {
		t.Errorf("no serve config: admitted %+v", c)
	}
}
