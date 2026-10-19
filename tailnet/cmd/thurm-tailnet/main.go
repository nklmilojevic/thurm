// Command thurm-tailnet serves the Thurm daemon over Tailscale. thurmd starts it and talks to it
// over an inherited socket (--control-fd) in JSON lines:
//
//	→ {"id":1,"cmd":"apply","node":{...},"host":{...}}   start, change or stop the listeners
//	→ {"id":2,"cmd":"login"} / "logout" / "status"
//	→ {"id":3,"cmd":"rename","hostname":"mac"}
//	← {"id":1,"error":null,"status":{...}}             the answer, with the status after it
//	← {"status":{...}}                                  whenever the status changes
//
// With --dial-socket it also lets this machine's processes dial tailnet peers through the
// embedded node (internal/dial).
//
// It exits when the control socket closes, so it never outlives the daemon.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log"
	"net"
	"os"
	"sync"
	"time"

	dialer "github.com/nklmilojevic/thurm/tailnet/internal/dial"
	"github.com/nklmilojevic/thurm/tailnet/internal/host"
	"github.com/nklmilojevic/thurm/tailnet/internal/node"
	"github.com/nklmilojevic/thurm/tailnet/internal/status"
)

type nodeConfig struct {
	Enabled             bool     `json:"enabled"`
	Hostname            string   `json:"hostname"`
	AllowTailnetMembers bool     `json:"allow_tailnet_members"`
	Web                 bool     `json:"web"`
	AdvertiseTags       []string `json:"advertise_tags"`
	AuthKey             string   `json:"auth_key"`
}

type hostConfig struct {
	Listen          string `json:"listen"`
	AllowIdentified bool   `json:"allow_identified"`
	Serve           bool   `json:"serve"`
}

type command struct {
	ID       uint64      `json:"id"`
	Cmd      string      `json:"cmd"`
	Node     *nodeConfig `json:"node"`
	Host     *hostConfig `json:"host"`
	Hostname string      `json:"hostname"`
}

type reply struct {
	ID     *uint64        `json:"id,omitempty"`
	Error  *string        `json:"error"`
	Status *status.Status `json:"status"`
	// DialSocket is where this helper dials out, when it does (older helpers never say).
	DialSocket string `json:"dial_socket,omitempty"`
}

type helper struct {
	node *node.Node
	host *host.Host
	dial *dialer.Server
	out  *json.Encoder
	mu   sync.Mutex
	// hostname is kept for the status while the node is off.
	hostname string
	allow    bool
}

func main() {
	controlFD := flag.Int("control-fd", -1, "inherited socket to thurmd")
	socket := flag.String("socket", "", "the daemon's socket")
	state := flag.String("state", "", "the embedded node's state directory")
	serveSocket := flag.String("serve-socket", "", "the socket offered to tailscale serve")
	dialSocket := flag.String("dial-socket", "", "the socket this machine's processes dial tailnet peers through")
	flag.Parse()
	log.SetFlags(0)
	log.SetPrefix("thurm-tailnet: ")
	if *controlFD < 0 || *socket == "" || *state == "" || *serveSocket == "" {
		log.Fatal("usage: thurm-tailnet --control-fd N --socket PATH --state DIR --serve-socket PATH")
	}
	ctl, err := net.FileConn(os.NewFile(uintptr(*controlFD), "control"))
	if err != nil {
		log.Fatalf("control socket: %v", err)
	}

	dial := func(ctx context.Context) (net.Conn, error) {
		var d net.Dialer
		return d.DialContext(ctx, "unix", *socket)
	}
	h := &helper{out: json.NewEncoder(ctl), hostname: "thurm"}
	h.node = &node.Node{Dir: *state, Dial: dial, Logf: log.Printf, OnChange: h.publish}
	h.host = &host.Host{ServePath: *serveSocket, Dial: dial, Logf: log.Printf}
	if *dialSocket != "" {
		h.dial = &dialer.Server{Path: *dialSocket, Peers: h.node.Peers, Dial: h.node.DialPeer, PeerOK: dialer.SameUser, Logf: log.Printf}
		if err := h.dial.Start(); err != nil {
			log.Printf("dial socket: %v", err)
			h.dial = nil
		}
	}

	in := bufio.NewScanner(ctl)
	in.Buffer(make([]byte, 64*1024), 1024*1024)
	for in.Scan() {
		var c command
		if err := json.Unmarshal(in.Bytes(), &c); err != nil {
			log.Printf("bad command: %v", err)
			continue
		}
		// In order: a later command must not finish before an earlier one (an apply turning
		// the node on after an off was answered). Status changes go out on their own.
		h.run(c)
	}
	// The daemon went away: stop serving, keeping the node signed in.
	if h.dial != nil {
		h.dial.Stop()
	}
	h.host.Stop()
	h.node.Disable()
}

func (h *helper) run(c command) {
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	var err error
	switch c.Cmd {
	case "apply":
		err = h.apply(c)
	case "login":
		err = h.node.Login(ctx)
	case "logout":
		err = h.node.Logout(ctx)
	case "rename":
		err = h.node.Rename(c.Hostname)
		if err == nil {
			h.mu.Lock()
			h.hostname = c.Hostname
			h.mu.Unlock()
		}
	case "status":
	default:
		err = fmt.Errorf("unknown command %q", c.Cmd)
	}
	r := reply{ID: &c.ID, Status: h.status()}
	if err != nil {
		msg := err.Error()
		r.Error = &msg
	}
	h.send(r)
}

func (h *helper) apply(c command) error {
	var errs []error
	if c.Host != nil {
		if err := h.host.Apply(host.Config{Listen: c.Host.Listen, AllowIdentified: c.Host.AllowIdentified, Serve: c.Host.Serve}); err != nil {
			errs = append(errs, err)
		}
	}
	if c.Node != nil {
		h.mu.Lock()
		if c.Node.Hostname != "" {
			h.hostname = c.Node.Hostname
		}
		h.allow = c.Node.AllowTailnetMembers
		h.mu.Unlock()
		if c.Node.Enabled {
			cfg := node.Config{
				Hostname:     c.Node.Hostname,
				AllowMembers: c.Node.AllowTailnetMembers,
				Web:          c.Node.Web,
				Tags:         c.Node.AdvertiseTags,
				AuthKey:      c.Node.AuthKey,
			}
			if err := h.node.Enable(cfg); err != nil {
				errs = append(errs, err)
			}
		} else {
			h.node.Disable()
		}
	}
	h.publish()
	return errors.Join(errs...)
}

func (h *helper) status() *status.Status {
	s := &status.Status{}
	h.node.Status(s)
	if s.State == "off" {
		h.mu.Lock()
		s.Hostname = h.hostname
		s.AllowTailnetMembers = h.allow
		h.mu.Unlock()
		// With only this host's listeners on, the state is theirs.
		if active, errMsg := h.host.Active(); active {
			s.State = "running"
		} else if errMsg != "" {
			s.State = "error"
		}
	}
	h.host.Status(s)
	// serde reads a JSON null as an error for a list.
	if s.Tags == nil {
		s.Tags = []string{}
	}
	if s.HostURLs == nil {
		s.HostURLs = []string{}
	}
	return s
}

func (h *helper) publish() {
	h.send(reply{Status: h.status()})
}

func (h *helper) send(r reply) {
	if h.dial != nil {
		r.DialSocket = h.dial.Path
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	if err := h.out.Encode(r); err != nil {
		// The daemon is gone; main notices the closed socket.
		log.Printf("control: %v", err)
	}
}
