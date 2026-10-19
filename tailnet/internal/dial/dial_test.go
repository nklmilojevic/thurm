package dial

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/netip"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

var peers = []Peer{
	{Name: "bastion", DNSName: "bastion.tail1234.ts.net", Addrs: []netip.Addr{netip.MustParseAddr("fd7a:115c:a1e0::1"), netip.MustParseAddr("100.64.0.1")}, Online: true},
	{Name: "laptop", DNSName: "laptop.tail1234.ts.net", Addrs: []netip.Addr{netip.MustParseAddr("100.64.0.2")}},
	// Two machines that call themselves "box": only their labels tell them apart.
	{Name: "box", DNSName: "box.tail1234.ts.net", Addrs: []netip.Addr{netip.MustParseAddr("100.64.0.3")}},
	{Name: "box", DNSName: "box-1.tail1234.ts.net", Addrs: []netip.Addr{netip.MustParseAddr("100.64.0.4")}},
	{Name: "twin", DNSName: "twin-a.tail1234.ts.net", Addrs: []netip.Addr{netip.MustParseAddr("100.64.0.5")}},
	{Name: "twin", DNSName: "twin-b.tail1234.ts.net", Addrs: []netip.Addr{netip.MustParseAddr("100.64.0.6")}},
}

func TestResolve(t *testing.T) {
	for host, want := range map[string]string{
		"bastion":                  "bastion.tail1234.ts.net",
		"BASTION":                  "bastion.tail1234.ts.net",
		"bastion.tail1234.ts.net":  "bastion.tail1234.ts.net",
		"bastion.tail1234.ts.net.": "bastion.tail1234.ts.net",
		"100.64.0.1":               "bastion.tail1234.ts.net",
		"fd7a:115c:a1e0::1":        "bastion.tail1234.ts.net",
		"box":                      "box.tail1234.ts.net",
		"box-1":                    "box-1.tail1234.ts.net",
	} {
		p, err := Resolve(peers, host)
		if err != nil || p.DNSName != want {
			t.Errorf("Resolve(%q) = %q, %v; want %q", host, p.DNSName, err, want)
		}
	}
	for _, host := range []string{"", "nope", "1.1.1.1", "100.64.0.99", "twin", "example.com"} {
		if p, err := Resolve(peers, host); err == nil {
			t.Errorf("Resolve(%q) = %q; want an error", host, p.DNSName)
		}
	}
}

func TestAddrPrefersIPv4(t *testing.T) {
	addr, err := peers[0].Addr(443)
	if err != nil || addr != "100.64.0.1:443" {
		t.Fatalf("Addr = %q, %v", addr, err)
	}
	v6 := Peer{DNSName: "v6", Addrs: []netip.Addr{netip.MustParseAddr("fd7a:115c:a1e0::9")}}
	if addr, _ := v6.Addr(443); addr != "[fd7a:115c:a1e0::9]:443" {
		t.Fatalf("Addr = %q", addr)
	}
	if _, err := (Peer{DNSName: "none"}).Addr(443); err == nil {
		t.Fatal("a peer without addresses has no Addr")
	}
}

// server starts a dial socket whose dials reach an in-memory echo, recording each address.
func server(t *testing.T, peersErr error, dialErr error) (string, chan string) {
	t.Helper()
	// Short: Unix socket paths are limited to about 100 bytes.
	dir, err := os.MkdirTemp("", "dial")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(dir) })
	dialed := make(chan string, 4)
	s := &Server{
		Path: filepath.Join(dir, "d.sock"),
		Peers: func(context.Context) ([]Peer, error) {
			return peers, peersErr
		},
		Dial: func(ctx context.Context, addr string) (net.Conn, error) {
			dialed <- addr
			if dialErr != nil {
				return nil, dialErr
			}
			a, b := net.Pipe()
			go func() {
				_, _ = io.Copy(b, b)
				b.Close()
			}()
			return a, nil
		},
		PeerOK: SameUser,
		Logf:   t.Logf,
	}
	if err := s.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(s.Stop)
	fi, err := os.Stat(s.Path)
	if err != nil || fi.Mode().Perm() != 0o600 {
		t.Fatalf("socket mode: %v %v", fi, err)
	}
	return s.Path, dialed
}

func ask(t *testing.T, path, req string) (net.Conn, *bufio.Reader, reply) {
	t.Helper()
	c, err := net.Dial("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { c.Close() })
	_ = c.SetDeadline(time.Now().Add(5 * time.Second))
	if _, err := io.WriteString(c, req); err != nil {
		t.Fatal(err)
	}
	r := bufio.NewReader(c)
	line, err := r.ReadString('\n')
	if err != nil {
		t.Fatalf("reading the answer: %v", err)
	}
	var rep reply
	if err := json.Unmarshal([]byte(line), &rep); err != nil {
		t.Fatalf("answer %q: %v", line, err)
	}
	return c, r, rep
}

func TestDialRelays(t *testing.T) {
	path, dialed := server(t, nil, nil)
	// Bytes sent right behind the request belong to the peer.
	c, r, rep := ask(t, path, "{\"host\":\"bastion\",\"port\":443}\nhello ")
	if !rep.OK || rep.DNSName != "bastion.tail1234.ts.net" || rep.Addr != "100.64.0.1:443" {
		t.Fatalf("answer: %+v", rep)
	}
	if got := <-dialed; got != "100.64.0.1:443" {
		t.Fatalf("dialed %q", got)
	}
	if _, err := io.WriteString(c, "world\n"); err != nil {
		t.Fatal(err)
	}
	line, err := r.ReadString('\n')
	if err != nil || line != "hello world\n" {
		t.Fatalf("echo: %q %v", line, err)
	}
}

func TestDialRefusals(t *testing.T) {
	path, dialed := server(t, nil, nil)
	for req, code := range map[string]string{
		"{\"host\":\"nope\",\"port\":443}\n":    CodeNoSuchPeer,
		"{\"host\":\"8.8.8.8\",\"port\":443}\n": CodeNoSuchPeer,
		"{\"host\":\"bastion\"}\n":              CodeBadRequest,
		"not json\n":                            CodeBadRequest,
	} {
		c, r, rep := ask(t, path, req)
		if rep.OK || rep.Code != code || rep.Error == "" {
			t.Errorf("%q: %+v; want %s", req, rep, code)
		}
		// The connection ends with the refusal.
		if _, err := r.ReadByte(); !errors.Is(err, io.EOF) {
			t.Errorf("%q: still open after a refusal: %v", req, err)
		}
		c.Close()
	}
	select {
	case addr := <-dialed:
		t.Fatalf("dialed %s for a refused request", addr)
	default:
	}
}

func TestDialNodeOffAndFailures(t *testing.T) {
	path, _ := server(t, ErrNodeOff, nil)
	if _, _, rep := ask(t, path, "{\"host\":\"bastion\",\"port\":443}\n"); rep.Code != CodeNodeOff {
		t.Fatalf("node off: %+v", rep)
	}
	path, _ = server(t, nil, errors.New("connection refused"))
	_, _, rep := ask(t, path, "{\"host\":\"bastion\",\"port\":443}\n")
	if rep.Code != CodeUnreachable || !strings.Contains(rep.Error, "bastion.tail1234.ts.net") {
		t.Fatalf("unreachable: %+v", rep)
	}
	path, _ = server(t, nil, context.DeadlineExceeded)
	if _, _, rep := ask(t, path, "{\"host\":\"bastion\",\"port\":443}\n"); rep.Code != CodeTimeout {
		t.Fatalf("timeout: %+v", rep)
	}
}

func TestPeersListed(t *testing.T) {
	path, _ := server(t, nil, nil)
	_, _, rep := ask(t, path, "{\"peers\":true}\n")
	if !rep.OK || len(rep.Peers) != len(peers) || rep.Peers[0].DNSName != "bastion.tail1234.ts.net" {
		t.Fatalf("peers: %+v", rep)
	}
}

func TestSameUserAcceptsThisProcess(t *testing.T) {
	dir, _ := os.MkdirTemp("", "cred")
	defer os.RemoveAll(dir)
	ln, err := net.Listen("unix", filepath.Join(dir, "s"))
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		c, err := net.Dial("unix", filepath.Join(dir, "s"))
		if err == nil {
			defer c.Close()
			time.Sleep(200 * time.Millisecond)
		}
	}()
	c, err := ln.Accept()
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if !SameUser(c.(*net.UnixConn)) {
		t.Fatal("this process's own connection was refused")
	}
}

// A client that has said everything (its write half closed) still gets the peer's answer.
func TestDialKeepsTheAnswerAfterTheClientIsDone(t *testing.T) {
	// A peer that answers once its caller is done, after a pause.
	peer, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer peer.Close()
	go func() {
		c, err := peer.Accept()
		if err != nil {
			return
		}
		defer c.Close()
		got, _ := io.ReadAll(c)
		time.Sleep(200 * time.Millisecond)
		_, _ = io.WriteString(c, strings.ToUpper(string(got)))
	}()
	dir, _ := os.MkdirTemp("", "dial")
	defer os.RemoveAll(dir)
	s := &Server{
		Path:  filepath.Join(dir, "d.sock"),
		Peers: func(context.Context) ([]Peer, error) { return peers, nil },
		Dial: func(ctx context.Context, _ string) (net.Conn, error) {
			return (&net.Dialer{}).DialContext(ctx, "tcp", peer.Addr().String())
		},
		PeerOK: SameUser,
		Logf:   t.Logf,
	}
	if err := s.Start(); err != nil {
		t.Fatal(err)
	}
	defer s.Stop()
	c, r, rep := ask(t, s.Path, "{\"host\":\"bastion\",\"port\":22}\nquestion")
	if !rep.OK {
		t.Fatalf("answer: %+v", rep)
	}
	if err := c.(*net.UnixConn).CloseWrite(); err != nil {
		t.Fatal(err)
	}
	got, err := io.ReadAll(r)
	if err != nil || string(got) != "QUESTION" {
		t.Fatalf("got %q, %v", got, err)
	}
}
