// Package admit decides which tailnet callers reach the daemon.
package admit

import (
	"errors"
	"fmt"
	"strings"
)

// Caller is who Tailscale says is on the other end of a connection.
type Caller struct {
	UserID      int64
	LoginName   string
	DisplayName string
	// Node is the calling device's name.
	Node string
	// Tags is non-empty for a tagged machine, which has no owner.
	Tags []string
	// RemoteAddr is the caller's tailnet address.
	RemoteAddr string
}

// Tagged reports whether the caller is a tagged machine rather than a person.
func (c Caller) Tagged() bool { return len(c.Tags) > 0 }

// Policy admits the owner, or with AllowAll every caller Tailscale identifies.
type Policy struct {
	// OwnerID is the account that owns the node or host.
	OwnerID int64
	// OwnerLogin identifies the owner where only a login is known (behind tailscale serve).
	OwnerLogin string
	// OwnerTags is non-empty when the node or host itself is tagged: it then has no owner.
	OwnerTags []string
	AllowAll  bool
	// AllowFlag names the setting that turns AllowAll on, for error messages.
	AllowFlag string
}

// ErrForbidden is wrapped by every refusal.
var ErrForbidden = errors.New("forbidden")

// Validate reports a policy that would admit nobody: owner-only on a tagged node or host.
func (p Policy) Validate() error {
	if !p.AllowAll && len(p.OwnerTags) > 0 {
		return fmt.Errorf("this host is tagged (%s); owner-only admission admits nobody; turn on %s",
			strings.Join(p.OwnerTags, ", "), p.AllowFlag)
	}
	return nil
}

// Check admits or refuses a caller.
func (p Policy) Check(c Caller) error {
	if p.AllowAll {
		return nil
	}
	if err := p.Validate(); err != nil {
		return fmt.Errorf("%w: %v", ErrForbidden, err)
	}
	if c.Tagged() {
		return fmt.Errorf("%w: tagged tailscale nodes are not the node owner identity", ErrForbidden)
	}
	if p.OwnerID != 0 && c.UserID == p.OwnerID {
		return nil
	}
	if p.OwnerID == 0 && p.OwnerLogin != "" && strings.EqualFold(c.LoginName, p.OwnerLogin) {
		return nil
	}
	who := c.LoginName
	if who == "" {
		who = "the caller"
	}
	return fmt.Errorf("%w: %s is not this host's owner; turn on %s to admit every identified caller",
		ErrForbidden, who, p.AllowFlag)
}
