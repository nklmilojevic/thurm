package gateway

import (
	"context"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strconv"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
)

type browserFixture struct {
	srv    *httptest.Server
	d      *fakeDaemon
	dials  *atomic.Int32
	origin string
}

// newBrowserGateway serves a gateway whose browser origin is the test server's own address, so
// the Host and Origin checks see the address a browser would.
func newBrowserGateway(t *testing.T, caller admit.Caller, identifyErr error, web http.Handler) browserFixture {
	t.Helper()
	d := newFakeDaemon(t)
	var dials atomic.Int32
	g := &Gateway{
		Via:      "webtest",
		Identify: func(*http.Request) (admit.Caller, error) { return caller, identifyErr },
		Policy:   func() admit.Policy { return admit.Policy{OwnerID: 7, AllowFlag: "allow"} },
		Dial: func(ctx context.Context) (net.Conn, error) {
			dials.Add(1)
			return d.dial(ctx)
		},
		Logf: t.Logf,
	}
	srv := httptest.NewUnstartedServer(g)
	srv.Start()
	t.Cleanup(srv.Close)
	origin := "http://" + srv.Listener.Addr().String()
	g.SetBrowser(origin, web)
	return browserFixture{srv: srv, d: d, dials: &dials, origin: origin}
}

func page() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		_, _ = io.WriteString(w, "page")
	})
}

// status gets path with the Host header set to host (the request's own when empty).
func status(t *testing.T, url, host string) (int, string) {
	t.Helper()
	req, err := http.NewRequest(http.MethodGet, url, nil)
	if err != nil {
		t.Fatal(err)
	}
	if host != "" {
		req.Host = host
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	body, _ := io.ReadAll(resp.Body)
	return resp.StatusCode, string(body)
}

func TestOriginMatrixOnConnect(t *testing.T) {
	f := newBrowserGateway(t, admit.Caller{UserID: 7, LoginName: "me@example.com"}, nil, page())
	host := f.srv.Listener.Addr().String()
	_, port, err := net.SplitHostPort(host)
	if err != nil {
		t.Fatal(err)
	}
	p, err := strconv.Atoi(port)
	if err != nil {
		t.Fatal(err)
	}
	cases := []struct {
		name, origin string
		admitted     bool
	}{
		{"absent (an app)", "", true},
		{"the node's own page", "http://" + host, true},
		{"another tailnet name", "https://other.tail1234.ts.net", false},
		{"null", "null", false},
		{"the same host on another port", "http://127.0.0.1:" + strconv.Itoa(p+1), false},
		{"the same host over https", "https://" + host, false},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			var opts *websocket.DialOptions
			if c.origin != "" {
				opts = &websocket.DialOptions{HTTPHeader: http.Header{"Origin": {c.origin}}}
			}
			before := f.dials.Load()
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			defer cancel()
			ws, resp, err := websocket.Dial(ctx, wsURL(f.srv, ConnectPath), opts)
			if !c.admitted {
				if err == nil {
					ws.CloseNow()
					t.Fatal("upgrade succeeded")
				}
				if resp == nil || resp.StatusCode != http.StatusForbidden {
					t.Fatalf("response %v, err %v", resp, err)
				}
				if f.dials.Load() != before {
					t.Fatal("refused origin reached the daemon")
				}
				return
			}
			if err != nil {
				t.Fatal(err)
			}
			defer ws.CloseNow()
			<-f.d.conns
			if f.dials.Load() != before+1 {
				t.Fatalf("admitted caller dialed %d times", f.dials.Load()-before)
			}
		})
	}
}

func TestHostMustBeTheNodesOwnAddress(t *testing.T) {
	f := newBrowserGateway(t, admit.Caller{UserID: 7}, nil, page())
	if code, _ := status(t, f.origin+"/", "evil.example.net"); code != http.StatusForbidden {
		t.Fatalf("page under a foreign host: status %d", code)
	}
	// A browser's upgrade under a foreign host, even with the node's Origin.
	before := f.dials.Load()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	ws, resp, err := websocket.Dial(ctx, wsURL(f.srv, ConnectPath), &websocket.DialOptions{
		Host:       "evil.example.net",
		HTTPHeader: http.Header{"Origin": {f.origin}},
	})
	if err == nil {
		ws.CloseNow()
		t.Fatal("upgrade under a foreign host succeeded")
	}
	if resp == nil || resp.StatusCode != http.StatusForbidden {
		t.Fatalf("connect under a foreign host: response %v, err %v", resp, err)
	}
	if f.dials.Load() != before {
		t.Fatal("a foreign host reached the daemon")
	}
	if code, body := status(t, f.origin+"/", ""); code != http.StatusOK || body != "page" {
		t.Fatalf("own host: status %d, body %q", code, body)
	}
}

func TestSameHost(t *testing.T) {
	cases := []struct {
		host, origin string
		want         bool
	}{
		{"thurm.tail1234.ts.net", "https://thurm.tail1234.ts.net", true},
		{"thurm.tail1234.ts.net:443", "https://thurm.tail1234.ts.net", true},
		{"THURM.tail1234.ts.net.", "https://thurm.tail1234.ts.net", true},
		{"thurm.tail1234.ts.net:8443", "https://thurm.tail1234.ts.net", false},
		{"other.tail1234.ts.net", "https://thurm.tail1234.ts.net", false},
		{"127.0.0.1:5000", "http://127.0.0.1:5000", true},
		{"127.0.0.1", "http://127.0.0.1:5000", false},
	}
	for _, c := range cases {
		if got := sameHost(c.host, c.origin); got != c.want {
			t.Errorf("sameHost(%q, %q) = %v", c.host, c.origin, got)
		}
	}
}

// Apps send no Origin and are not held to the node's name (a client that writes the default
// port, or reaches the node by another name it resolves to, keeps working).
func TestAppsAreNotHeldToTheHost(t *testing.T) {
	f := newBrowserGateway(t, admit.Caller{UserID: 7}, nil, page())
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	ws, _, err := websocket.Dial(ctx, wsURL(f.srv, ConnectPath), &websocket.DialOptions{Host: "another.name"})
	if err != nil {
		t.Fatal(err)
	}
	defer ws.CloseNow()
	<-f.d.conns
}

func TestHostIsNotCheckedWithoutAnOrigin(t *testing.T) {
	// With no browser origin set, Host is not checked: the gateway serves whatever name reaches
	// it, as it always has.
	srv, _ := newGateway(t, admit.Caller{UserID: 7})
	if code, _ := status(t, srv.URL+"/", "anything.example"); code != http.StatusNotFound {
		t.Fatalf("status %d", code)
	}
}

func TestBrowserClientNeedsAnAdmittedCaller(t *testing.T) {
	cases := []struct {
		name        string
		caller      admit.Caller
		identifyErr error
		want        int
	}{
		{"not identified", admit.Caller{}, errors.New("no such peer"), http.StatusForbidden},
		{"identified, not admitted", admit.Caller{UserID: 8, LoginName: "you@example.com"}, nil, http.StatusForbidden},
		{"admitted", admit.Caller{UserID: 7, LoginName: "me@example.com"}, nil, http.StatusOK},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			f := newBrowserGateway(t, c.caller, c.identifyErr, page())
			code, body := status(t, f.origin+"/", "")
			if code != c.want {
				t.Fatalf("status %d, want %d", code, c.want)
			}
			if c.want == http.StatusOK && body != "page" {
				t.Fatalf("body %q", body)
			}
		})
	}
}

func TestNoBrowserClientIs404BeforeIdentify(t *testing.T) {
	f := newBrowserGateway(t, admit.Caller{}, errors.New("no such peer"), nil)
	if code, _ := status(t, f.origin+"/", ""); code != http.StatusNotFound {
		t.Fatalf("status %d", code)
	}
}
