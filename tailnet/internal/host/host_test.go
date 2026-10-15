package host

import (
	"net/http"
	"testing"
)

func TestServeVersion(t *testing.T) {
	for v, ok := range map[string]bool{
		"1.44.0": true, "1.44.0-t1234abcd-g5678": true, "1.104.1": true,
		"1.43.9": false, "1.42.0-tabc": false, "": false,
	} {
		if err := CheckServeVersion(v); (err == nil) != ok {
			t.Errorf("CheckServeVersion(%q) = %v", v, err)
		}
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
	h := &Host{}
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
}
