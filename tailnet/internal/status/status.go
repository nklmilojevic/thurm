// Package status is the helper's report to the daemon, mirroring
// thurm_proto::tailnet::TailscaleStatus as JSON, and helpers shared by the listeners.
package status

import (
	"context"
	"fmt"

	"tailscale.com/client/local"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
)

// Status mirrors TailscaleStatus.
type Status struct {
	State               string   `json:"state"`
	Error               *string  `json:"error"`
	Hostname            string   `json:"hostname"`
	DNSName             *string  `json:"dns_name"`
	AuthURL             *string  `json:"auth_url"`
	Account             *Account `json:"account"`
	TailnetName         *string  `json:"tailnet_name"`
	AllowTailnetMembers bool     `json:"allow_tailnet_members"`
	Tags                []string `json:"tags"`
	HTTPS               HTTPS    `json:"https"`
	URL                 *string  `json:"url"`
	HostURLs            []string `json:"host_urls"`
	ServeSocket         *string  `json:"serve_socket"`
}

// Account mirrors TailscaleAccount.
type Account struct {
	LoginName   string `json:"login_name"`
	DisplayName string `json:"display_name"`
}

// HTTPS mirrors TailscaleHttps; State is unavailable, magic_dns_off, certs_off, provisioning
// or ready.
type HTTPS struct {
	State string  `json:"state"`
	Error *string `json:"error"`
}

// FromBackend maps Tailscale's backend state to TailscaleState.
func FromBackend(s string) string {
	switch s {
	case "NeedsLogin":
		return "needs_login"
	case "NeedsMachineAuth":
		return "needs_machine_auth"
	case "Running":
		return "running"
	case "Stopped":
		return "stopped"
	default:
		return "starting"
	}
}

// ValidHostname is thurm_proto::tailnet::valid_hostname: one DNS label.
func ValidHostname(name string) bool {
	if name == "" || len(name) > 63 || name[0] == '-' || name[len(name)-1] == '-' {
		return false
	}
	for i := 0; i < len(name); i++ {
		c := name[i]
		if !(c == '-' || c >= '0' && c <= '9' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z') {
			return false
		}
	}
	return true
}

// WhoIs asks Tailscale who is at remoteAddr.
func WhoIs(ctx context.Context, lc *local.Client, remoteAddr string) (admit.Caller, error) {
	who, err := lc.WhoIs(ctx, remoteAddr)
	if err != nil {
		return admit.Caller{}, fmt.Errorf("looking up %s: %w", remoteAddr, err)
	}
	if who.Node == nil {
		return admit.Caller{}, fmt.Errorf("%s is not a tailnet node", remoteAddr)
	}
	c := admit.Caller{
		Node:       who.Node.ComputedName,
		Tags:       who.Node.Tags,
		RemoteAddr: remoteAddr,
	}
	if c.Node == "" {
		c.Node = who.Node.Name
	}
	if who.UserProfile != nil && !who.Node.IsTagged() {
		c.UserID = int64(who.UserProfile.ID)
		c.LoginName = who.UserProfile.LoginName
		c.DisplayName = who.UserProfile.DisplayName
	}
	return c, nil
}
