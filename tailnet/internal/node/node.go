// Package node runs the embedded Tailscale node: its own device on the tailnet, serving the
// gateway over HTTPS on :443 with the tailnet's certificate.
package node

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"tailscale.com/client/local"
	"tailscale.com/ipn"
	"tailscale.com/ipn/ipnstate"
	"tailscale.com/tsnet"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
	"github.com/nklmilojevic/thurm/tailnet/internal/gateway"
	"github.com/nklmilojevic/thurm/tailnet/internal/status"
	"github.com/nklmilojevic/thurm/tailnet/internal/web"
)

// AllowFlag names the setting that admits every tailnet member.
const AllowFlag = "Allow tailnet members (tailscale.allow_tailnet_members)"

// Config is what the node runs with.
type Config struct {
	Hostname     string
	AllowMembers bool
	// Web serves the browser client on the node's HTTPS address.
	Web bool
	// Tags and AuthKey only matter when the node enrolls.
	Tags    []string
	AuthKey string
}

// Node holds the tsnet server while serving is on.
type Node struct {
	Dir      string
	Dial     func(ctx context.Context) (net.Conn, error)
	Logf     func(format string, args ...any)
	OnChange func()

	mu      sync.Mutex
	cfg     Config
	srv     *tsnet.Server
	lc      *local.Client
	cancel  context.CancelFunc
	ln      net.Listener
	gw      *gateway.Gateway
	gen     int
	backend string
	authURL string
	errMsg  string
	st      *ipnstate.Status
	https   status.HTTPS
	// loginAsked is set once a sign-in link was asked for in this NeedsLogin spell.
	loginAsked bool
}

func (n *Node) changed() {
	if n.OnChange != nil {
		n.OnChange()
	}
}

// Enable starts the node, or applies cfg to the running one (rename, who it admits).
func (n *Node) Enable(cfg Config) error {
	if !status.ValidHostname(cfg.Hostname) {
		return fmt.Errorf("invalid tailnet hostname %q: one DNS label of 1 to 63 letters, digits or hyphens", cfg.Hostname)
	}
	if len(cfg.Tags) > 0 && !cfg.AllowMembers {
		return errors.New("tailscale: advertising tags gives this node a machine identity with no owner, so owner-only access would admit nobody. Allow tailnet members or drop the tags")
	}
	n.mu.Lock()
	if n.srv != nil {
		if !cfg.AllowMembers && n.st != nil && n.st.Self != nil && n.st.Self.IsTagged() {
			tags := strings.Join(n.st.Self.Tags.AsSlice(), ", ")
			n.mu.Unlock()
			return fmt.Errorf("this node is tagged (%s), so it has no owner and owner-only access would admit nobody. Allow tailnet members, or sign out and enroll without tags", tags)
		}
		old, gw := n.cfg, n.gw
		// The hostname changes once the rename succeeds, so a failed one is tried again.
		next := cfg
		next.Hostname = old.Hostname
		n.cfg = next
		n.mu.Unlock()
		if cfg.AllowMembers != old.AllowMembers && !cfg.AllowMembers && gw != nil {
			// Narrowing who may connect drops everyone; the owner reconnects.
			gw.CloseAll("access changed")
		}
		n.serveBrowser()
		if cfg.Hostname != old.Hostname {
			if err := n.Rename(cfg.Hostname); err != nil {
				n.changed()
				return err
			}
		}
		n.changed()
		return nil
	}
	if err := os.MkdirAll(n.Dir, 0o700); err != nil {
		n.mu.Unlock()
		return fmt.Errorf("creating tailscale state directory: %w", err)
	}
	srv := &tsnet.Server{
		Dir:           n.Dir,
		Hostname:      cfg.Hostname,
		AuthKey:       cfg.AuthKey,
		AdvertiseTags: cfg.Tags,
		UserLogf:      n.userLogf,
		Logf:          func(string, ...any) {},
	}
	if err := srv.Start(); err != nil {
		n.mu.Unlock()
		return fmt.Errorf("starting tailscale node: %w", err)
	}
	lc, err := srv.LocalClient()
	if err != nil {
		srv.Close()
		n.mu.Unlock()
		return fmt.Errorf("starting tailscale node: %w", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	n.gen++
	gen := n.gen
	n.cfg, n.srv, n.lc, n.cancel = cfg, srv, lc, cancel
	n.backend, n.authURL, n.errMsg, n.st, n.loginAsked = "Starting", "", "", nil, false
	n.https = status.HTTPS{State: "unavailable"}
	n.gw = &gateway.Gateway{
		Via:      "node",
		Identify: n.identify,
		Policy:   n.policy,
		Dial:     n.Dial,
		Logf:     n.Logf,
	}
	n.mu.Unlock()
	go n.watch(ctx, gen, lc)
	n.changed()
	return nil
}

// serveBrowser points the gateway at the browser client. Its origin is the node's own once the
// DNS name is known, so a page from anywhere else is refused either way; the client itself is
// served only with Web on and HTTPS ready. n.mu is held through the gateway's update, so a
// stale call can't put back what a newer one changed (lock order: n.mu, then the gateway's).
func (n *Node) serveBrowser() {
	n.mu.Lock()
	defer n.mu.Unlock()
	gw := n.gw
	origin := ""
	if n.st != nil && n.st.Self != nil {
		if name := strings.TrimSuffix(n.st.Self.DNSName, "."); name != "" {
			origin = "https://" + name
		}
	}
	serve := n.cfg.Web && n.https.State == "ready" && origin != ""
	if gw == nil {
		return
	}
	var client http.Handler
	if serve {
		client = web.Handler(web.Embedded(), origin)
	}
	gw.SetBrowser(origin, client)
}

// Disable stops serving: tailnet connections close, and the node stays signed in (its state
// is kept), so the next Enable needs no sign-in.
func (n *Node) Disable() {
	n.mu.Lock()
	srv, ln, gw, cancel := n.srv, n.ln, n.gw, n.cancel
	n.srv, n.lc, n.ln, n.gw, n.cancel, n.st = nil, nil, nil, nil, nil, nil
	n.backend, n.authURL, n.errMsg = "", "", ""
	n.https = status.HTTPS{State: "unavailable"}
	n.gen++
	n.mu.Unlock()
	if srv == nil {
		return
	}
	cancel()
	if ln != nil {
		ln.Close()
	}
	gw.CloseAll("tailscale serving turned off")
	srv.Close()
	n.changed()
}

// Login asks Tailscale for a sign-in link; it arrives in a status change.
func (n *Node) Login(ctx context.Context) error {
	n.mu.Lock()
	lc := n.lc
	n.loginAsked = true
	n.mu.Unlock()
	if lc == nil {
		return errors.New("enable Tailscale before requesting a sign-in link")
	}
	return lc.StartLoginInteractive(ctx)
}

// Logout signs the node out and forgets it. A failed logout leaves the node running. A node
// that is off but signed in is started, without serving, to sign it out.
func (n *Node) Logout(ctx context.Context) error {
	n.mu.Lock()
	lc, hostname := n.lc, n.cfg.Hostname
	n.mu.Unlock()
	if lc == nil {
		if _, err := os.Stat(filepath.Join(n.Dir, "tailscaled.state")); errors.Is(err, os.ErrNotExist) {
			return nil // never signed in
		}
		srv := &tsnet.Server{Dir: n.Dir, Hostname: hostname, UserLogf: n.userLogf, Logf: func(string, ...any) {}}
		if err := srv.Start(); err != nil {
			return fmt.Errorf("signing out of Tailscale: starting the node: %w", err)
		}
		defer srv.Close()
		slc, err := srv.LocalClient()
		if err != nil {
			return fmt.Errorf("signing out of Tailscale: %w", err)
		}
		if err := slc.Logout(ctx); err != nil {
			return fmt.Errorf("signing out of Tailscale: %w", err)
		}
		return nil
	}
	if err := lc.Logout(ctx); err != nil {
		return fmt.Errorf("signing out of Tailscale: %w", err)
	}
	n.Disable()
	return nil
}

// Rename changes the node's name on the tailnet without signing in again. The new DNS name
// may appear after this returns.
func (n *Node) Rename(hostname string) error {
	if !status.ValidHostname(hostname) {
		return fmt.Errorf("invalid tailnet hostname %q: one DNS label of 1 to 63 letters, digits or hyphens", hostname)
	}
	n.mu.Lock()
	lc := n.lc
	if lc == nil {
		n.cfg.Hostname = hostname
	}
	n.mu.Unlock()
	if lc == nil {
		n.changed()
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	_, err := lc.EditPrefs(ctx, &ipn.MaskedPrefs{Prefs: ipn.Prefs{Hostname: hostname}, HostnameSet: true})
	if err != nil {
		return fmt.Errorf("renaming the tailscale node: %w", err)
	}
	n.mu.Lock()
	if n.lc == lc {
		n.cfg.Hostname = hostname
	}
	n.mu.Unlock()
	n.changed()
	return nil
}

// Enabled reports whether the node runs.
func (n *Node) Enabled() bool {
	n.mu.Lock()
	defer n.mu.Unlock()
	return n.srv != nil
}

// Status fills the node's part of the status.
func (n *Node) Status(s *status.Status) {
	n.mu.Lock()
	defer n.mu.Unlock()
	s.Hostname = n.cfg.Hostname
	s.AllowTailnetMembers = n.cfg.AllowMembers
	if n.srv == nil {
		// Never enabled leaves n.https empty, which the daemon can't read.
		s.State = "off"
		s.HTTPS = status.HTTPS{State: "unavailable"}
		return
	}
	s.HTTPS = n.https
	s.State = status.FromBackend(n.backend)
	if n.errMsg != "" {
		s.Error = &n.errMsg
		if s.State == "starting" {
			s.State = "error"
		}
	}
	if n.authURL != "" && s.State == "needs_login" {
		s.AuthURL = &n.authURL
	}
	st := n.st
	if st == nil {
		return
	}
	if st.Self != nil {
		if name := strings.TrimSuffix(st.Self.DNSName, "."); name != "" {
			s.DNSName = &name
			if n.https.State == "ready" {
				url := "wss://" + name + gateway.ConnectPath
				s.URL = &url
			}
		}
		if st.Self.Tags != nil {
			s.Tags = st.Self.Tags.AsSlice()
		}
		if u, ok := st.User[st.Self.UserID]; ok && !st.Self.IsTagged() {
			s.Account = &status.Account{LoginName: u.LoginName, DisplayName: u.DisplayName}
		}
	}
	if st.CurrentTailnet != nil && st.CurrentTailnet.Name != "" {
		name := st.CurrentTailnet.Name
		s.TailnetName = &name
	}
	if len(s.Tags) > 0 && !n.cfg.AllowMembers && s.Error == nil {
		msg := fmt.Sprintf("this node is tagged (%s), so it has no owner and owner-only access admits nobody. Allow tailnet members or drop the tags.", strings.Join(s.Tags, ", "))
		s.Error = &msg
	}
}

func (n *Node) policy() admit.Policy {
	n.mu.Lock()
	defer n.mu.Unlock()
	p := admit.Policy{AllowAll: n.cfg.AllowMembers, AllowFlag: AllowFlag}
	if n.st != nil && n.st.Self != nil {
		p.OwnerID = int64(n.st.Self.UserID)
		if n.st.Self.Tags != nil {
			p.OwnerTags = n.st.Self.Tags.AsSlice()
		}
	}
	return p
}

func (n *Node) identify(r *http.Request) (admit.Caller, error) {
	n.mu.Lock()
	lc := n.lc
	n.mu.Unlock()
	if lc == nil {
		return admit.Caller{}, errors.New("tailscale serving is off")
	}
	return status.WhoIs(r.Context(), lc, r.RemoteAddr)
}

// watch follows the node's state: sign-in links, running, certificates.
func (n *Node) watch(ctx context.Context, gen int, lc *local.Client) {
	for ctx.Err() == nil {
		w, err := lc.WatchIPNBus(ctx, ipn.NotifyInitialState|ipn.NotifyNoPrivateKeys)
		if err != nil {
			if !n.fail(gen, fmt.Sprintf("watching the tailscale node: %v", err)) {
				return
			}
			sleep(ctx, time.Second)
			continue
		}
		for {
			msg, err := w.Next()
			if err != nil {
				break
			}
			n.apply(ctx, gen, lc, msg)
		}
		w.Close()
	}
}

func (n *Node) fail(gen int, msg string) bool {
	n.mu.Lock()
	ok := gen == n.gen
	if ok {
		n.errMsg = msg
	}
	n.mu.Unlock()
	if ok {
		n.changed()
	}
	return ok
}

func (n *Node) apply(ctx context.Context, gen int, lc *local.Client, msg ipn.Notify) {
	st, _ := lc.StatusWithoutPeers(ctx)
	n.mu.Lock()
	if gen != n.gen {
		n.mu.Unlock()
		return
	}
	if msg.ErrMessage != nil {
		n.errMsg = *msg.ErrMessage
	}
	if msg.BrowseToURL != nil {
		n.authURL = *msg.BrowseToURL
		n.Logf("tailscale: waiting for sign-in")
	}
	if st != nil {
		n.st = st
		n.backend = st.BackendState
		if st.AuthURL != "" {
			n.authURL = st.AuthURL
		}
	}
	if msg.State != nil {
		n.backend = msg.State.String()
	}
	askLogin := false
	switch n.backend {
	case "NeedsLogin":
		// A node without a valid auth key, or whose key was rejected, signs in in a browser.
		askLogin = n.authURL == "" && !n.loginAsked
		n.loginAsked = n.loginAsked || askLogin
	case "Running":
		n.authURL, n.loginAsked, n.errMsg = "", false, ""
	}
	bind := n.backend == "Running" && n.ln == nil && st != nil
	n.mu.Unlock()

	if askLogin {
		go n.askLogin(ctx, gen, lc)
	}
	if bind {
		n.bindTLS(ctx, gen, st)
	}
	n.serveBrowser()
	n.changed()
}

// askLogin asks for a sign-in link, again with backoff while asking fails and the node still
// needs one.
func (n *Node) askLogin(ctx context.Context, gen int, lc *local.Client) {
	delay := 2 * time.Second
	for {
		err := lc.StartLoginInteractive(ctx)
		if err == nil || ctx.Err() != nil {
			return
		}
		n.Logf("tailscale: requesting a sign-in link: %v", err)
		if !n.fail(gen, fmt.Sprintf("requesting a sign-in link: %v", err)) {
			return
		}
		sleep(ctx, delay)
		delay = min(delay*2, time.Minute)
		n.mu.Lock()
		still := gen == n.gen && n.backend == "NeedsLogin" && n.authURL == ""
		n.mu.Unlock()
		if !still {
			return
		}
	}
}

// bindTLS serves the gateway over HTTPS once the tailnet allows it, reporting why not
// otherwise; it's tried again on the next state change.
func (n *Node) bindTLS(ctx context.Context, gen int, st *ipnstate.Status) {
	https := status.HTTPS{State: "provisioning"}
	switch {
	case st.CurrentTailnet == nil || !st.CurrentTailnet.MagicDNSEnabled:
		msg := "MagicDNS is off for this tailnet. Turn it on under DNS in the Tailscale admin console."
		https = status.HTTPS{State: "magic_dns_off", Error: &msg}
	case len(st.CertDomains) == 0:
		msg := "HTTPS certificates are off for this tailnet. Turn them on under DNS in the Tailscale admin console."
		https = status.HTTPS{State: "certs_off", Error: &msg}
	}
	n.mu.Lock()
	if gen != n.gen {
		n.mu.Unlock()
		return
	}
	n.https = https
	srv, gw, lc := n.srv, n.gw, n.lc
	n.mu.Unlock()
	if https.State != "provisioning" {
		return
	}
	ln, err := srv.ListenTLS("tcp", ":443")
	if err != nil {
		msg := fmt.Sprintf("listening on the tailnet: %v", err)
		n.setHTTPS(gen, status.HTTPS{State: "unavailable", Error: &msg})
		return
	}
	n.mu.Lock()
	if gen != n.gen {
		n.mu.Unlock()
		ln.Close()
		return
	}
	n.ln = ln
	n.mu.Unlock()
	n.Logf("tailnet https listener ready")
	go func() {
		if err := gw.Serve(ln); err != nil {
			n.Logf("tailscale: serving: %v", err)
		}
	}()
	// The first certificate takes a while; fetch it now rather than on the first connection.
	go func() {
		cctx, cancel := context.WithTimeout(ctx, 2*time.Minute)
		defer cancel()
		if _, _, err := lc.CertPair(cctx, st.CertDomains[0]); err != nil {
			if ctx.Err() == nil {
				msg := fmt.Sprintf("getting the HTTPS certificate: %v", err)
				n.setHTTPS(gen, status.HTTPS{State: "provisioning", Error: &msg})
			}
			return
		}
		n.setHTTPS(gen, status.HTTPS{State: "ready"})
	}()
}

func (n *Node) setHTTPS(gen int, h status.HTTPS) {
	n.mu.Lock()
	ok := gen == n.gen
	if ok {
		n.https = h
	}
	n.mu.Unlock()
	if ok {
		n.serveBrowser()
		n.changed()
	}
}

func sleep(ctx context.Context, d time.Duration) {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
	case <-t.C:
	}
}

// userLogf passes tsnet's messages on, except its sign-in prompt: that repeats every few seconds
// with the link, which the status carries and the logs should not.
func (n *Node) userLogf(format string, args ...any) {
	if strings.HasPrefix(format, "To start this tsnet server") {
		return
	}
	n.Logf(format, args...)
}
