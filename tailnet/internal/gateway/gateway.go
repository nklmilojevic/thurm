// Package gateway serves the daemon's protocol to admitted tailnet callers over a WebSocket.
//
// A client opens GET /v1/connect and upgrades it; binary messages then carry the daemon's
// byte stream (length-prefixed postcard frames) unchanged, in both directions.
package gateway

import (
	"context"
	"errors"
	"io"
	"log"
	"net"
	"net/http"
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

	mu    sync.Mutex
	conns map[*websocket.Conn]struct{}
}

func (g *Gateway) logf(format string, args ...any) {
	if g.Logf != nil {
		g.Logf(format, args...)
	} else {
		log.Printf(format, args...)
	}
}

// ServeHTTP answers /v1/connect, and 404 elsewhere so the endpoint is easy to tell apart.
func (g *Gateway) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != ConnectPath {
		http.NotFound(w, r)
		return
	}
	caller, err := g.Identify(r)
	if err != nil {
		g.logf("%s: %s: not identified: %v", g.Via, r.RemoteAddr, err)
		http.Error(w, "forbidden: the caller could not be identified", http.StatusForbidden)
		return
	}
	if err := g.Policy().Check(caller); err != nil {
		g.logf("%s: refused %s (%s): %v", g.Via, caller.LoginName, caller.Node, err)
		http.Error(w, err.Error(), http.StatusForbidden)
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
	// Clients are apps, not browsers: they send no Origin, and a browser page can't reach
	// this without one.
	ws, err := websocket.Accept(w, r, nil)
	if err != nil {
		daemon.Close()
		return
	}
	ws.SetReadLimit(-1)
	g.track(ws, true)
	defer g.track(ws, false)
	g.logf("%s: admitted %s (%s)", g.Via, caller.LoginName, caller.Node)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	conn := websocket.NetConn(ctx, ws, websocket.MessageBinary)
	pipe(conn, daemon)
	ws.Close(websocket.StatusNormalClosure, "")
}

// CloseAll drops every proxied connection (the listener was turned off, or the policy
// changed); the daemon sees each client go away.
func (g *Gateway) CloseAll(reason string) {
	g.mu.Lock()
	conns := g.conns
	g.conns = nil
	g.mu.Unlock()
	for ws := range conns {
		ws.Close(websocket.StatusGoingAway, reason)
	}
}

func (g *Gateway) track(ws *websocket.Conn, add bool) {
	g.mu.Lock()
	defer g.mu.Unlock()
	if add {
		if g.conns == nil {
			g.conns = map[*websocket.Conn]struct{}{}
		}
		g.conns[ws] = struct{}{}
	} else {
		delete(g.conns, ws)
	}
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
