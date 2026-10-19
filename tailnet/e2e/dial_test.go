// Package e2e runs the helper against Tailscale's in-process coordination server: a module of
// its own, so the test-only dependencies of tailscale.com/tstest stay out of the helper's.
package e2e

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"net"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"tailscale.com/ipn/store/mem"
	"tailscale.com/net/netns"
	"tailscale.com/tailcfg"
	"tailscale.com/tsnet"
	"tailscale.com/tstest/integration"
	"tailscale.com/tstest/integration/testcontrol"
	"tailscale.com/types/logger"

	"github.com/nklmilojevic/thurm/tailnet/internal/dial"
	"github.com/nklmilojevic/thurm/tailnet/internal/node"
)

// The node dials out to a peer for this machine's processes: through Node.Peers and
// Node.DialPeer directly, and through the dial socket, against Tailscale's in-process
// coordination server and DERP.
func TestNodeDialsPeersThroughTheDialSocket(t *testing.T) {
	if testing.Short() {
		t.Skip("starts a coordination server and two tailnet nodes")
	}
	netns.SetEnabled(false)
	t.Cleanup(func() { netns.SetEnabled(true) })
	derpMap := integration.RunDERPAndSTUN(t, logger.Discard, "127.0.0.1")
	control := &testcontrol.Server{
		DERPMap:        derpMap,
		DNSConfig:      &tailcfg.DNSConfig{Proxied: true},
		MagicDNSDomain: "tail-scale.ts.net",
		Logf:           logger.Discard,
	}
	control.HTTPTestServer = httptest.NewUnstartedServer(control)
	control.HTTPTestServer.Start()
	t.Cleanup(control.HTTPTestServer.Close)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()

	// The peer: a plain tsnet node echoing on :7297.
	peer := &tsnet.Server{
		Dir:        filepath.Join(t.TempDir(), "peer"),
		ControlURL: control.HTTPTestServer.URL,
		Hostname:   "bastion",
		Store:      new(mem.Store),
		Ephemeral:  true,
		Logf:       logger.Discard,
	}
	t.Cleanup(func() { peer.Close() })
	if _, err := peer.Up(ctx); err != nil {
		t.Fatal(err)
	}
	ln, err := peer.Listen("tcp", ":7297")
	if err != nil {
		t.Fatal(err)
	}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go func() {
				_, _ = io.Copy(c, c)
				c.Close()
			}()
		}
	}()

	// Ours, as the helper runs it.
	n := &node.Node{
		Dir:        filepath.Join(t.TempDir(), "node"),
		ControlURL: control.HTTPTestServer.URL,
		Logf:       t.Logf,
	}
	if err := n.Enable(node.Config{Hostname: "mac"}); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(n.Disable)

	var peers []dial.Peer
	for {
		peers, err = n.Peers(ctx)
		if err == nil && len(peers) > 0 {
			break
		}
		if ctx.Err() != nil {
			t.Fatalf("no peers: %v", err)
		}
		time.Sleep(200 * time.Millisecond)
	}
	p, err := dial.Resolve(peers, "bastion")
	if err != nil {
		t.Fatalf("resolving bastion among %+v: %v", peers, err)
	}

	dir, err := os.MkdirTemp("", "nd")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	s := &dial.Server{
		Path:   filepath.Join(dir, "d.sock"),
		Peers:  n.Peers,
		Dial:   n.DialPeer,
		PeerOK: dial.SameUser,
		Logf:   t.Logf,
	}
	if err := s.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(s.Stop)

	// The first packets may race the peers' DERP handshake: try again for a while.
	for {
		got, err := echoThrough(s.Path, "bastion")
		if err == nil && got == "hello tailnet\n" {
			break
		}
		if ctx.Err() != nil {
			t.Fatalf("dialing %s through the socket: %q, %v", p.DNSName, got, err)
		}
		time.Sleep(500 * time.Millisecond)
	}
}

func echoThrough(path, host string) (string, error) {
	c, err := net.Dial("unix", path)
	if err != nil {
		return "", err
	}
	defer c.Close()
	_ = c.SetDeadline(time.Now().Add(15 * time.Second))
	if err := json.NewEncoder(c).Encode(map[string]any{"host": host, "port": 7297}); err != nil {
		return "", err
	}
	r := bufio.NewReader(c)
	line, err := r.ReadString('\n')
	if err != nil {
		return "", err
	}
	var rep struct {
		OK    bool   `json:"ok"`
		Error string `json:"error"`
	}
	if err := json.Unmarshal([]byte(line), &rep); err != nil {
		return "", err
	}
	if !rep.OK {
		return "", &net.OpError{Op: "dial", Err: errString(rep.Error)}
	}
	if _, err := io.WriteString(c, "hello tailnet\n"); err != nil {
		return "", err
	}
	return r.ReadString('\n')
}

type errString string

func (e errString) Error() string { return string(e) }
