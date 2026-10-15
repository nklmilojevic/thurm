// Package host serves the gateway through this machine's own Tailscale, without a node of
// its own: on the machine's tailnet addresses, and on a Unix socket for `tailscale serve`.
package host

import (
	"context"
	"errors"
	"fmt"
	"mime"
	"net"
	"net/http"
	"net/netip"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"time"

	"tailscale.com/client/local"
	"tailscale.com/ipn"
	"tailscale.com/ipn/ipnstate"
	"tailscale.com/util/cmpver"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
	"github.com/nklmilojevic/thurm/tailnet/internal/gateway"
	"github.com/nklmilojevic/thurm/tailnet/internal/status"
)

// AllowFlag names the setting that admits every identified caller.
const AllowFlag = "Admit every identified caller (tailscale.host_allow_identified)"

// MinServeVersion is the first tailscaled whose serve can proxy HTTP to a Unix socket. Its
// proxy deletes the identity headers a caller sent before setting them (since 1.44.0), for
// Unix-socket backends too, so on the socket they are tailscaled's word.
const MinServeVersion = "1.94.0"

// Config is what the host listeners run with.
type Config struct {
	// Listen is ":port" (every tailnet address of this machine) or "addr:port".
	Listen          string
	AllowIdentified bool
	Serve           bool
}

// Host runs the listeners that go through this machine's Tailscale.
type Host struct {
	// ServePath is the Unix socket offered to `tailscale serve`.
	ServePath string
	Dial      func(ctx context.Context) (net.Conn, error)
	Logf      func(format string, args ...any)

	lc local.Client

	mu      sync.Mutex
	cfg     Config
	self    *ipnstate.Status
	lns     []net.Listener
	serveLn net.Listener
	gw      *gateway.Gateway
	gwServe *gateway.Gateway
	urls    []string
	errMsg  string
}

// Apply starts, changes or stops the listeners.
func (h *Host) Apply(cfg Config) error {
	h.mu.Lock()
	same := cfg == h.cfg && (cfg.Listen == "" || len(h.lns) > 0) && (!cfg.Serve || h.serveLn != nil)
	h.mu.Unlock()
	if same {
		return nil
	}
	h.stop()
	h.mu.Lock()
	h.cfg = cfg
	h.errMsg = ""
	h.mu.Unlock()
	if cfg.Listen == "" && !cfg.Serve {
		return nil
	}
	err := h.start(cfg)
	if err != nil {
		h.stop()
		h.mu.Lock()
		h.errMsg = err.Error()
		h.mu.Unlock()
	}
	return err
}

func (h *Host) start(cfg Config) error {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	st, err := h.lc.StatusWithoutPeers(ctx)
	if err != nil {
		return fmt.Errorf("this machine's Tailscale is not reachable: %w", err)
	}
	if st.BackendState != "Running" || st.Self == nil {
		return fmt.Errorf("this machine's Tailscale is not running (%s)", st.BackendState)
	}
	h.mu.Lock()
	h.self = st
	h.mu.Unlock()
	if err := h.policy().Validate(); err != nil {
		return err
	}
	if cfg.Listen != "" {
		if err := h.listen(st, cfg.Listen); err != nil {
			return err
		}
	}
	if cfg.Serve {
		if err := CheckServeVersion(st.Version); err != nil {
			return err
		}
		if err := h.checkServeConfig(ctx); err != nil {
			return err
		}
		if err := h.serve(); err != nil {
			return err
		}
	}
	return nil
}

func (h *Host) listen(st *ipnstate.Status, listen string) error {
	addr, port, err := net.SplitHostPort(listen)
	if err != nil {
		return fmt.Errorf("invalid host_listen %q: %w", listen, err)
	}
	if len(st.TailscaleIPs) == 0 {
		return errors.New("this host has no tailnet addresses")
	}
	ips := st.TailscaleIPs
	if addr != "" {
		ip, err := netip.ParseAddr(addr)
		if err != nil || !slices.Contains(st.TailscaleIPs, ip) {
			return fmt.Errorf("%s is not one of this host's tailnet addresses: %v", addr, st.TailscaleIPs)
		}
		ips = []netip.Addr{ip}
	}
	gw := &gateway.Gateway{Via: "host", Identify: h.whois, Policy: h.policy, Dial: h.Dial, Logf: h.Logf}
	var lns []net.Listener
	var urls []string
	if name := strings.TrimSuffix(st.Self.DNSName, "."); name != "" {
		urls = append(urls, "ws://"+net.JoinHostPort(name, port)+gateway.ConnectPath)
	}
	for _, ip := range ips {
		hp := net.JoinHostPort(ip.String(), port)
		ln, err := net.Listen("tcp", hp)
		if err != nil {
			for _, l := range lns {
				l.Close()
			}
			return fmt.Errorf("listening on %s: %w", hp, err)
		}
		lns = append(lns, ln)
		urls = append(urls, "ws://"+hp+gateway.ConnectPath)
		go func() {
			if err := gw.Serve(ln); err != nil {
				h.Logf("host listener %s: %v", hp, err)
			}
		}()
	}
	h.mu.Lock()
	h.lns, h.gw, h.urls = lns, gw, urls
	h.mu.Unlock()
	h.Logf("listening through this host's Tailscale on %v", urls)
	return nil
}

func (h *Host) serve() error {
	if err := os.Remove(h.ServePath); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("removing the old %s: %w", h.ServePath, err)
	}
	ln, err := net.Listen("unix", h.ServePath)
	if err != nil {
		return fmt.Errorf("listening on %s: %w", h.ServePath, err)
	}
	if err := os.Chmod(h.ServePath, 0o600); err != nil {
		ln.Close()
		return err
	}
	gw := &gateway.Gateway{Via: "serve", Identify: h.fromHeaders, Policy: h.servePolicy, Dial: h.Dial, Logf: h.Logf}
	h.mu.Lock()
	h.serveLn, h.gwServe = ln, gw
	h.mu.Unlock()
	go func() {
		if err := gw.Serve(ln); err != nil {
			h.Logf("serve socket: %v", err)
		}
	}()
	h.Logf("serving %s for tailscale serve", h.ServePath)
	return nil
}

// Stop closes every listener and connection.
func (h *Host) Stop() {
	h.stop()
	h.mu.Lock()
	h.cfg = Config{}
	h.errMsg = ""
	h.mu.Unlock()
}

func (h *Host) stop() {
	h.mu.Lock()
	lns, serveLn, gw, gwServe := h.lns, h.serveLn, h.gw, h.gwServe
	h.lns, h.serveLn, h.gw, h.gwServe, h.urls = nil, nil, nil, nil, nil
	h.mu.Unlock()
	for _, ln := range lns {
		ln.Close()
	}
	if gw != nil {
		gw.CloseAll("tailscale listener turned off")
	}
	if serveLn != nil {
		serveLn.Close()
		os.Remove(h.ServePath)
	}
	if gwServe != nil {
		gwServe.CloseAll("tailscale listener turned off")
	}
}

// Status fills the host listeners' part of the status.
func (h *Host) Status(s *status.Status) {
	h.mu.Lock()
	defer h.mu.Unlock()
	s.HostURLs = append(s.HostURLs, h.urls...)
	if h.serveLn != nil {
		p := h.ServePath
		s.ServeSocket = &p
	}
	if h.errMsg != "" {
		msg := h.errMsg
		if s.Error != nil {
			msg = *s.Error + "; " + msg
		}
		s.Error = &msg
	}
}

// Active reports whether a listener is up, and the error that stopped one.
func (h *Host) Active() (bool, string) {
	h.mu.Lock()
	defer h.mu.Unlock()
	return len(h.lns) > 0 || h.serveLn != nil, h.errMsg
}

func (h *Host) policy() admit.Policy {
	h.mu.Lock()
	defer h.mu.Unlock()
	p := admit.Policy{AllowAll: h.cfg.AllowIdentified, AllowFlag: AllowFlag}
	if h.self != nil && h.self.Self != nil {
		p.OwnerID = int64(h.self.Self.UserID)
		if h.self.Self.Tags != nil {
			p.OwnerTags = h.self.Self.Tags.AsSlice()
		}
	}
	return p
}

// servePolicy matches by login: behind tailscale serve only the caller's login is known.
func (h *Host) servePolicy() admit.Policy {
	p := h.policy()
	h.mu.Lock()
	defer h.mu.Unlock()
	if h.self != nil && h.self.Self != nil {
		if u, ok := h.self.User[h.self.Self.UserID]; ok {
			p.OwnerLogin = u.LoginName
		}
	}
	p.OwnerID = 0
	return p
}

func (h *Host) whois(r *http.Request) (admit.Caller, error) {
	return status.WhoIs(r.Context(), &h.lc, r.RemoteAddr)
}

// CheckServeVersion refuses a tailscaled whose serve proxy can't be trusted with identity.
func CheckServeVersion(version string) error {
	if cmpver.Less(version, MinServeVersion) {
		return fmt.Errorf("this machine's Tailscale (%s) is too old to serve the socket: tailscale serve sets the caller's identity from %s on", version, MinServeVersion)
	}
	return nil
}

// checkServeConfig refuses a serve config that forwards raw TCP to the socket: that path
// passes the caller's bytes, headers included, through untouched (`tailscale serve --tcp`
// takes unix: targets from 1.102.0 on), so anyone could claim the owner's login.
func (h *Host) checkServeConfig(ctx context.Context) error {
	cfg, err := h.lc.GetServeConfig(ctx)
	if err != nil {
		return fmt.Errorf("reading this machine's tailscale serve config: %w", err)
	}
	if where := RawForward(cfg, h.ServePath); where != "" {
		return fmt.Errorf("tailscale serve forwards raw TCP to %s (%s), which would let callers claim any identity: proxy it as HTTP instead (tailscale serve unix:%s)", h.ServePath, where, h.ServePath)
	}
	return nil
}

// RawForward names a handler in cfg that forwards raw TCP to the Unix socket at path, or
// returns "".
func RawForward(cfg *ipn.ServeConfig, path string) string {
	if cfg == nil {
		return ""
	}
	check := func(scope string, tcp map[uint16]*ipn.TCPPortHandler) string {
		for port, th := range tcp {
			if th == nil {
				continue
			}
			if target, ok := strings.CutPrefix(th.TCPForward, "unix:"); ok && sameSocket(target, path) {
				return fmt.Sprintf("%sport %d", scope, port)
			}
		}
		return ""
	}
	if where := check("", cfg.TCP); where != "" {
		return where
	}
	for name, svc := range cfg.Services {
		if svc == nil {
			continue
		}
		if where := check(fmt.Sprintf("service %s ", name), svc.TCP); where != "" {
			return where
		}
	}
	for _, fg := range cfg.Foreground {
		if where := RawForward(fg, path); where != "" {
			return "foreground " + where
		}
	}
	return ""
}

func sameSocket(a, b string) bool {
	if filepath.Clean(a) == filepath.Clean(b) {
		return true
	}
	fa, errA := os.Stat(a)
	fb, errB := os.Stat(b)
	return errA == nil && errB == nil && os.SameFile(fa, fb)
}

// fromHeaders reads who tailscale serve says the caller is. tailscaled deletes these headers
// from the incoming request before setting them (MinServeVersion), and sets them only for a
// person on the tailnet: a tagged or Funnel caller has none and is refused. The serve config
// is checked on every connection, since it can change while the socket is up.
func (h *Host) fromHeaders(r *http.Request) (admit.Caller, error) {
	if err := h.checkServeConfig(r.Context()); err != nil {
		return admit.Caller{}, err
	}
	if r.Header.Get("Tailscale-Funnel-Request") != "" {
		return admit.Caller{}, errors.New("the caller came through Tailscale Funnel, from outside the tailnet")
	}
	logins := r.Header.Values("Tailscale-User-Login")
	if len(logins) != 1 || logins[0] == "" {
		return admit.Caller{}, errors.New("no single Tailscale-User-Login header: the caller is tagged, or not proxied by tailscale serve")
	}
	// Non-ASCII values arrive as MIME encoded-words.
	var dec mime.WordDecoder
	login, err := dec.DecodeHeader(logins[0])
	if err != nil || login == "" {
		return admit.Caller{}, fmt.Errorf("unreadable Tailscale-User-Login header %q", logins[0])
	}
	name, err := dec.DecodeHeader(r.Header.Get("Tailscale-User-Name"))
	if err != nil {
		name = ""
	}
	return admit.Caller{
		LoginName:   login,
		DisplayName: name,
		RemoteAddr:  r.Header.Get("X-Forwarded-For"),
	}, nil
}
