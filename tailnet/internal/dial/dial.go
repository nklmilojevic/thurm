// Package dial lets this machine's own processes reach other Thurm hosts through the embedded
// node: a Unix socket where a same-user process asks for a TCP connection to a tailnet peer.
//
// A connection starts with one JSON line and gets one back; after an ok answer to a dial, the
// socket carries the peer connection's bytes both ways:
//
//	→ {"host":"bastion","port":443}
//	← {"ok":true,"dns_name":"bastion.tail1234.ts.net","addr":"100.101.102.103:443"}
//	→ {"peers":true}
//	← {"ok":true,"peers":[{"name":"bastion","dns_name":"…","addrs":["100.…"],"online":true}]}
//	← {"error":"…","code":"node_off|no_such_peer|unreachable|timeout|bad_request"}
//
// Only peers in the node's netmap are dialed, on their own tailnet addresses: no other
// addresses, exit-node or subnet routes. It's a way into Thurm hosts, not a proxy.
package dial

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/netip"
	"os"
	"strings"
	"sync"
	"time"

	"tailscale.com/ipn/ipnstate"
)

// Timeout bounds resolving and dialing (thurm_tailnet_client::CONNECT_TIMEOUT).
const Timeout = 8 * time.Second

// MaxRequest is the longest request line.
const MaxRequest = 4096

// Error codes in a refusal.
const (
	CodeNodeOff     = "node_off"
	CodeNoSuchPeer  = "no_such_peer"
	CodeUnreachable = "unreachable"
	CodeTimeout     = "timeout"
	CodeBadRequest  = "bad_request"
)

// ErrNodeOff is returned by Peers while the node isn't running.
var ErrNodeOff = errors.New("the embedded Tailscale node is not running")

// Peer is a tailnet peer the node may dial.
type Peer struct {
	Name    string       `json:"name"`
	DNSName string       `json:"dns_name"`
	Addrs   []netip.Addr `json:"addrs"`
	Online  bool         `json:"online"`
}

// FromStatus lists st's peers.
func FromStatus(st *ipnstate.Status) []Peer {
	if st == nil {
		return nil
	}
	peers := make([]Peer, 0, len(st.Peer))
	for _, p := range st.Peer {
		if p == nil {
			continue
		}
		peers = append(peers, Peer{
			Name:    p.HostName,
			DNSName: strings.TrimSuffix(p.DNSName, "."),
			Addrs:   p.TailscaleIPs,
			Online:  p.Online,
		})
	}
	return peers
}

// Resolve finds host among peers: a peer's MagicDNS name, its first label, its host name, or
// one of its tailnet addresses.
func Resolve(peers []Peer, host string) (Peer, error) {
	host = strings.TrimSuffix(strings.TrimSpace(host), ".")
	if host == "" {
		return Peer{}, errors.New("no host given")
	}
	if ip, err := netip.ParseAddr(host); err == nil {
		for _, p := range peers {
			for _, a := range p.Addrs {
				if a == ip.Unmap() {
					return p, nil
				}
			}
		}
		return Peer{}, fmt.Errorf("%s is not a peer's tailnet address", host)
	}
	// The full name first, then the first label, then the host name: a label is unique in
	// the tailnet, a host name need not be.
	matchers := []func(Peer) bool{
		func(p Peer) bool { return strings.EqualFold(p.DNSName, host) },
		func(p Peer) bool {
			label, _, _ := strings.Cut(p.DNSName, ".")
			return strings.EqualFold(label, host)
		},
		func(p Peer) bool { return strings.EqualFold(p.Name, host) },
	}
	for _, match := range matchers {
		var found []Peer
		for _, p := range peers {
			if match(p) {
				found = append(found, p)
			}
		}
		switch len(found) {
		case 0:
			continue
		case 1:
			return found[0], nil
		default:
			return Peer{}, fmt.Errorf("%q names %d peers; use the full name", host, len(found))
		}
	}
	return Peer{}, fmt.Errorf("no peer named %q on this tailnet", host)
}

// Addr is where to dial p: its IPv4 address when it has one.
func (p Peer) Addr(port uint16) (string, error) {
	var pick netip.Addr
	for _, a := range p.Addrs {
		if !pick.IsValid() || (a.Is4() && !pick.Is4()) {
			pick = a
		}
	}
	if !pick.IsValid() {
		return "", fmt.Errorf("%s has no tailnet address", p.DNSName)
	}
	return netip.AddrPortFrom(pick, port).String(), nil
}

type request struct {
	Host  string `json:"host"`
	Port  uint16 `json:"port"`
	Peers bool   `json:"peers"`
}

type reply struct {
	OK      bool   `json:"ok,omitempty"`
	DNSName string `json:"dns_name,omitempty"`
	Addr    string `json:"addr,omitempty"`
	Peers   []Peer `json:"peers,omitempty"`
	Error   string `json:"error,omitempty"`
	Code    string `json:"code,omitempty"`
}

// Server answers the dial socket.
type Server struct {
	// Path of the socket.
	Path string
	// Peers lists the netmap's peers, or ErrNodeOff.
	Peers func(ctx context.Context) ([]Peer, error)
	// Dial connects to addr ("ip:port") through the node.
	Dial func(ctx context.Context, addr string) (net.Conn, error)
	// PeerOK reports whether the process on the other end of c may use the socket.
	PeerOK func(c *net.UnixConn) bool
	Logf   func(format string, args ...any)

	mu sync.Mutex
	ln net.Listener
}

func (s *Server) logf(format string, args ...any) {
	if s.Logf != nil {
		s.Logf(format, args...)
	} else {
		log.Printf(format, args...)
	}
}

// Start listens on Path (0600), replacing a stale socket.
func (s *Server) Start() error {
	if err := os.Remove(s.Path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("removing the old %s: %w", s.Path, err)
	}
	ln, err := net.Listen("unix", s.Path)
	if err != nil {
		return fmt.Errorf("listening on %s: %w", s.Path, err)
	}
	if err := os.Chmod(s.Path, 0o600); err != nil {
		ln.Close()
		return err
	}
	s.mu.Lock()
	s.ln = ln
	s.mu.Unlock()
	go s.serve(ln)
	return nil
}

// Stop closes the listener; connections already relayed keep going until either end closes.
func (s *Server) Stop() {
	s.mu.Lock()
	ln := s.ln
	s.ln = nil
	s.mu.Unlock()
	if ln != nil {
		ln.Close()
		os.Remove(s.Path)
	}
}

func (s *Server) serve(ln net.Listener) {
	for {
		c, err := ln.Accept()
		if err != nil {
			return
		}
		go s.handle(c)
	}
}

func (s *Server) handle(c net.Conn) {
	uc, ok := c.(*net.UnixConn)
	if !ok || (s.PeerOK != nil && !s.PeerOK(uc)) {
		s.logf("dial: refused a connection from another user")
		c.Close()
		return
	}
	_ = c.SetReadDeadline(time.Now().Add(Timeout))
	in := bufio.NewReaderSize(c, MaxRequest)
	line, err := in.ReadSlice('\n')
	if err != nil {
		c.Close()
		return
	}
	_ = c.SetReadDeadline(time.Time{})
	var req request
	if err := json.Unmarshal(line, &req); err != nil {
		s.refuse(c, CodeBadRequest, fmt.Errorf("unreadable request: %v", err))
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), Timeout)
	defer cancel()
	peers, err := s.Peers(ctx)
	if err != nil {
		code := CodeUnreachable
		if errors.Is(err, ErrNodeOff) {
			code = CodeNodeOff
		}
		s.refuse(c, code, err)
		return
	}
	if req.Peers {
		s.answer(c, reply{OK: true, Peers: peers})
		c.Close()
		return
	}
	if req.Port == 0 {
		s.refuse(c, CodeBadRequest, errors.New("no port given"))
		return
	}
	peer, err := Resolve(peers, req.Host)
	if err != nil {
		s.refuse(c, CodeNoSuchPeer, err)
		return
	}
	addr, err := peer.Addr(req.Port)
	if err != nil {
		s.refuse(c, CodeNoSuchPeer, err)
		return
	}
	conn, err := s.Dial(ctx, addr)
	if err != nil {
		code := CodeUnreachable
		if errors.Is(err, context.DeadlineExceeded) || ctx.Err() != nil {
			code = CodeTimeout
		}
		s.refuse(c, code, fmt.Errorf("dialing %s (%s): %w", peer.DNSName, addr, err))
		return
	}
	if err := s.answer(c, reply{OK: true, DNSName: peer.DNSName, Addr: addr}); err != nil {
		conn.Close()
		c.Close()
		return
	}
	// What the client sent after its request line is the peer's.
	if n := in.Buffered(); n > 0 {
		buf, _ := in.Peek(n)
		if _, err := conn.Write(buf); err != nil {
			conn.Close()
			c.Close()
			return
		}
	}
	pipe(c, conn)
}

func (s *Server) answer(c net.Conn, r reply) error {
	b, err := json.Marshal(r)
	if err != nil {
		return err
	}
	_ = c.SetWriteDeadline(time.Now().Add(Timeout))
	_, err = c.Write(append(b, '\n'))
	_ = c.SetWriteDeadline(time.Time{})
	return err
}

func (s *Server) refuse(c net.Conn, code string, err error) {
	_ = s.answer(c, reply{Error: err.Error(), Code: code})
	c.Close()
}

// pipe copies both ways until both sides are done, then closes both. One side finishing
// (`thurm tailscale nc` at the end of its input) closes only the other side's write half, so the
// answer still comes back; an error, or a connection that can't half-close, ends both.
func pipe(a, b net.Conn) {
	done := make(chan struct{}, 2)
	cp := func(dst, src net.Conn) {
		_, err := io.Copy(dst, src)
		if cw, ok := dst.(interface{ CloseWrite() error }); ok && err == nil && cw.CloseWrite() == nil {
			done <- struct{}{}
			return
		}
		a.Close()
		b.Close()
		done <- struct{}{}
	}
	go cp(a, b)
	go cp(b, a)
	<-done
	<-done
	a.Close()
	b.Close()
}
