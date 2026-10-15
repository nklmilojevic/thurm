package gateway

import (
	"bufio"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
)

// fakeDaemon is a buffered socket, like the daemon's, handing each accepted connection to the
// test.
type fakeDaemon struct {
	ln    net.Listener
	conns chan net.Conn
}

func newFakeDaemon(t *testing.T) *fakeDaemon {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	d := &fakeDaemon{ln: ln, conns: make(chan net.Conn, 4)}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			d.conns <- c
		}
	}()
	t.Cleanup(func() { ln.Close() })
	return d
}

func (d *fakeDaemon) dial(ctx context.Context) (net.Conn, error) {
	var dialer net.Dialer
	return dialer.DialContext(ctx, "tcp", d.ln.Addr().String())
}

func newGateway(t *testing.T, caller admit.Caller) (*httptest.Server, *fakeDaemon) {
	t.Helper()
	d := newFakeDaemon(t)
	g := &Gateway{
		Via:      "node",
		Identify: func(*http.Request) (admit.Caller, error) { return caller, nil },
		Policy:   func() admit.Policy { return admit.Policy{OwnerID: 7, AllowFlag: "allow"} },
		Dial:     d.dial,
		Logf:     t.Logf,
	}
	srv := httptest.NewServer(g)
	t.Cleanup(srv.Close)
	return srv, d
}

func wsURL(srv *httptest.Server, path string) string {
	return "ws" + strings.TrimPrefix(srv.URL, "http") + path
}

func TestProxiesTheOwnerWithIngress(t *testing.T) {
	srv, d := newGateway(t, admit.Caller{UserID: 7, LoginName: "me@example.com", Node: "phone"})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	ws, _, err := websocket.Dial(ctx, wsURL(srv, ConnectPath), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer ws.CloseNow()
	daemon := <-d.conns
	r := bufio.NewReader(daemon)

	var head [8]byte
	if _, err := io.ReadFull(r, head[:]); err != nil {
		t.Fatal(err)
	}
	if binary.LittleEndian.Uint32(head[:4]) != 0xFFFFFFFF {
		t.Fatalf("no ingress magic: %x", head)
	}
	body := make([]byte, binary.LittleEndian.Uint32(head[4:]))
	if _, err := io.ReadFull(r, body); err != nil {
		t.Fatal(err)
	}
	var p map[string]any
	if err := json.Unmarshal(body, &p); err != nil || p["login_name"] != "me@example.com" {
		t.Fatalf("principal %s (%v)", body, err)
	}

	// Client bytes reach the daemon after the preamble, and the daemon's come back.
	if err := ws.Write(ctx, websocket.MessageBinary, []byte("hello")); err != nil {
		t.Fatal(err)
	}
	got := make([]byte, 5)
	if _, err := io.ReadFull(r, got); err != nil || string(got) != "hello" {
		t.Fatalf("daemon got %q (%v)", got, err)
	}
	go daemon.Write([]byte("world"))
	_, msg, err := ws.Read(ctx)
	if err != nil || string(msg) != "world" {
		t.Fatalf("client got %q (%v)", msg, err)
	}
}

func TestRefusesAnotherUser(t *testing.T) {
	srv, d := newGateway(t, admit.Caller{UserID: 8, LoginName: "you@example.com"})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	_, resp, err := websocket.Dial(ctx, wsURL(srv, ConnectPath), nil)
	if err == nil {
		t.Fatal("upgrade succeeded")
	}
	if resp == nil || resp.StatusCode != http.StatusForbidden {
		t.Fatalf("response %v, err %v", resp, err)
	}
	select {
	case <-d.conns:
		t.Fatal("refused caller reached the daemon")
	case <-time.After(100 * time.Millisecond):
	}
}

func TestOtherPathsAre404(t *testing.T) {
	srv, _ := newGateway(t, admit.Caller{UserID: 7})
	resp, err := http.Get(srv.URL + "/")
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusNotFound {
		t.Fatalf("status %d", resp.StatusCode)
	}
}

func TestUnidentifiedIsForbidden(t *testing.T) {
	d := newFakeDaemon(t)
	g := &Gateway{
		Via:      "host",
		Identify: func(*http.Request) (admit.Caller, error) { return admit.Caller{}, errors.New("no such peer") },
		Policy:   func() admit.Policy { return admit.Policy{AllowAll: true} },
		Dial:     d.dial,
		Logf:     t.Logf,
	}
	srv := httptest.NewServer(g)
	defer srv.Close()
	resp, err := http.Get(srv.URL + ConnectPath)
	if err != nil {
		t.Fatal(err)
	}
	resp.Body.Close()
	if resp.StatusCode != http.StatusForbidden {
		t.Fatalf("status %d", resp.StatusCode)
	}
}

func TestCloseAllDropsACallerStillBeingAdmitted(t *testing.T) {
	d := newFakeDaemon(t)
	dialing := make(chan struct{})
	release := make(chan struct{})
	g := &Gateway{
		Via:      "node",
		Identify: func(*http.Request) (admit.Caller, error) { return admit.Caller{UserID: 7}, nil },
		Policy:   func() admit.Policy { return admit.Policy{OwnerID: 7, AllowFlag: "allow"} },
		Dial: func(ctx context.Context) (net.Conn, error) {
			close(dialing)
			<-release
			return d.dial(ctx)
		},
		Logf: t.Logf,
	}
	srv := httptest.NewServer(g)
	t.Cleanup(srv.Close)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	type result struct {
		ws  *websocket.Conn
		err error
	}
	done := make(chan result, 1)
	go func() {
		ws, _, err := websocket.Dial(ctx, wsURL(srv, ConnectPath), nil)
		done <- result{ws, err}
	}()
	<-dialing
	// Admitted, still dialing the daemon: serving is turned off now.
	g.CloseAll("tailscale serving turned off")
	close(release)

	r := <-done
	if r.err == nil {
		defer r.ws.CloseNow()
		_, _, err := r.ws.Read(ctx)
		if websocket.CloseStatus(err) != websocket.StatusGoingAway {
			t.Fatalf("a caller admitted before CloseAll was proxied after it: %v", err)
		}
	}
	g.mu.Lock()
	n := len(g.conns)
	g.mu.Unlock()
	if n != 0 {
		t.Fatalf("%d connections tracked after CloseAll", n)
	}
}
