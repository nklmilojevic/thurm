// Package gateway serves the daemon's protocol to admitted tailnet callers over a WebSocket.
//
// A client opens GET /v1/connect and upgrades it; binary messages then carry the daemon's
// byte stream (length-prefixed postcard frames) unchanged, in both directions. The browser
// client, when served, is on every other path.
package gateway

import (
	"context"
	"errors"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"time"

	"github.com/coder/websocket"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
	"github.com/nklmilojevic/thurm/tailnet/internal/ingress"
)

// ConnectPath is thurm_proto::tailnet::CONNECT_PATH.
const ConnectPath = "/v1/connect"

// Identify tells who sent a request.
type Identify func(r *http.Request) (admit.Caller, error)

// Gateway proxies admitted callers to the daemon.
type Gateway struct {
	// Via names the listener in the principal: "node", "host" or "serve".
	Via      string
	Identify Identify
	// Policy returns the current admission policy (it changes with the settings).
	Policy func() admit.Policy
	// Dial connects to the daemon's socket.
	Dial func(ctx context.Context) (net.Conn, error)
	Logf func(format string, args ...any)

	// Origin is the one browser origin allowed, such as https://thurm.tail1234.ts.net; empty
	// allows none. Requests must be addressed to it too.
	Origin string
	// Web serves the browser client on every other path; nil serves none, which is 404.
	Web http.Handler

	mu    sync.Mutex
	conns map[*websocket.Conn]struct{}
	// epoch counts CloseAll calls: a caller admitted before one is not proxied after it.
	epoch uint64
}

func (g *Gateway) logf(format string, args ...any) {
	if g.Logf != nil {
		g.Logf(format, args...)
	} else {
		log.Printf(format, args...)
	}
}

// SetBrowser sets the browser origin allowed and the client served to it (nil: none). It may
// change while serving.
func (g *Gateway) SetBrowser(origin string, web http.Handler) {
	g.mu.Lock()
	defer g.mu.Unlock()
	g.Origin, g.Web = origin, web
}

// ServeHTTP answers /v1/connect, the browser client when Web is set, and 404 elsewhere so the
// endpoint is easy to tell apart.
func (g *Gateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	g.mu.Lock()
	origin, web := g.Origin, g.Web
	g.mu.Unlock()
	// A browser reaches only the node's own address: a page on another name (a DNS rebinding
	// one, say) gets nothing. Apps (no Origin) are not held to it, as before browsers came.
	browser := r.URL.Path != ConnectPath || r.Header.Get("Origin") != ""
	if origin != "" && browser && !sameHost(r.Host, origin) {
		g.logf("%s: %s: refused host %q", g.Via, r.RemoteAddr, r.Host)
		http.Error(w, "forbidden: this address is not the node's", http.StatusForbidden)
		return
	}
	if r.URL.Path != ConnectPath {
		if web == nil {
			http.NotFound(w, r)
			return
		}
		if _, _, ok := g.authorize(w, r); !ok {
			return
		}
		// The page's own origin decides where its WebSocket may connect: a page loaded as
		// NAME., NAME:443 or in other letters goes to the canonical address first.
		if u, err := url.Parse(origin); err == nil && origin != "" && r.Host != u.Host {
			http.Redirect(w, r, origin+r.URL.RequestURI(), http.StatusPermanentRedirect)
			return
		}
		web.ServeHTTP(w, r)
		return
	}
	caller, epoch, ok := g.authorize(w, r)
	if !ok {
		return
	}
	// Apps send no Origin. A browser page sends its own, and only the node's page may connect;
	// the daemon is not dialed for any other.
	if o := r.Header.Get("Origin"); o != "" && o != origin {
		g.logf("%s: refused %s: browser origin %q", g.Via, caller.LoginName, o)
		http.Error(w, "forbidden: this page may not connect", http.StatusForbidden)
		return
	}
	daemon, err := g.Dial(r.Context())
	if err != nil {
		g.logf("%s: daemon unreachable: %v", g.Via, err)
		http.Error(w, "the Thurm daemon is not reachable", http.StatusServiceUnavailable)
		return
	}
	if err := ingress.Write(daemon, ingress.FromCaller(caller, g.Via)); err != nil {
		daemon.Close()
		http.Error(w, "the Thurm daemon is not reachable", http.StatusServiceUnavailable)
		return
	}
	// The Origin check above is the only one, so the library's own is off.
	ws, err := websocket.Accept(w, r, &websocket.AcceptOptions{InsecureSkipVerify: true})
	if err != nil {
		daemon.Close()
		return
	}
	ws.SetReadLimit(-1)
	if !g.track(ws, epoch) {
		// Serving was turned off, or who may connect changed, while this caller was admitted.
		ws.Close(websocket.StatusGoingAway, "access changed")
		daemon.Close()
		return
	}
	defer g.untrack(ws)
	g.logf("%s: admitted %s (%s)", g.Via, caller.LoginName, caller.Node)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	conn := websocket.NetConn(ctx, ws, websocket.MessageBinary)
	pipe(conn, daemon)
	ws.Close(websocket.StatusNormalClosure, "")
}

// sameHost reports whether a Host header names origin's host: case aside, with or without the
// scheme's default port and a final dot.
func sameHost(host, origin string) bool {
	u, err := url.Parse(origin)
	if err != nil {
		return false
	}
	def := "443"
	if u.Scheme == "http" {
		def = "80"
	}
	norm := func(h string) string {
		name, port, err := net.SplitHostPort(h)
		if err != nil {
			name, port = h, ""
		}
		if port == "" {
			port = def
		}
		return strings.ToLower(strings.TrimSuffix(name, ".")) + ":" + port
	}
	return norm(host) == norm(u.Host)
}

// authorize identifies the caller and checks it against the policy, answering 403 when either
// refuses. It returns the epoch the caller is admitted at.
func (g *Gateway) authorize(w http.ResponseWriter, r *http.Request) (admit.Caller, uint64, bool) {
	caller, err := g.Identify(r)
	if err != nil {
		g.logf("%s: %s: not identified: %v", g.Via, r.RemoteAddr, err)
		http.Error(w, "forbidden: the caller could not be identified", http.StatusForbidden)
		return admit.Caller{}, 0, false
	}
	g.mu.Lock()
	epoch := g.epoch
	g.mu.Unlock()
	if err := g.Policy().Check(caller); err != nil {
		g.logf("%s: refused %s (%s): %v", g.Via, caller.LoginName, caller.Node, err)
		http.Error(w, err.Error(), http.StatusForbidden)
		return admit.Caller{}, 0, false
	}
	return caller, epoch, true
}

// CloseAll drops every proxied connection (the listener was turned off, or the policy
// changed); the daemon sees each client go away.
func (g *Gateway) CloseAll(reason string) {
	g.mu.Lock()
	conns := g.conns
	g.conns = nil
	g.epoch++
	g.mu.Unlock()
	for ws := range conns {
		ws.Close(websocket.StatusGoingAway, reason)
	}
}

// track registers a connection admitted at epoch, unless CloseAll ran since.
func (g *Gateway) track(ws *websocket.Conn, epoch uint64) bool {
	g.mu.Lock()
	defer g.mu.Unlock()
	if epoch != g.epoch {
		return false
	}
	if g.conns == nil {
		g.conns = map[*websocket.Conn]struct{}{}
	}
	g.conns[ws] = struct{}{}
	return true
}

func (g *Gateway) untrack(ws *websocket.Conn) {
	g.mu.Lock()
	defer g.mu.Unlock()
	delete(g.conns, ws)
}

// pipe copies both ways until either side ends, then closes both.
func pipe(a, b net.Conn) {
	done := make(chan struct{}, 2)
	cp := func(dst, src net.Conn) {
		_, _ = io.Copy(dst, src)
		done <- struct{}{}
	}
	go cp(a, b)
	go cp(b, a)
	<-done
	a.Close()
	b.Close()
	<-done
}

// Serve runs the gateway on ln until it closes.
func (g *Gateway) Serve(ln net.Listener) error {
	srv := &http.Server{Handler: g, ReadHeaderTimeout: 10 * time.Second}
	err := srv.Serve(ln)
	if errors.Is(err, http.ErrServerClosed) || errors.Is(err, net.ErrClosed) {
		return nil
	}
	return err
}
