package admit

import (
	"errors"
	"testing"
)

func TestOwnerOnly(t *testing.T) {
	p := Policy{OwnerID: 7, AllowFlag: "allow_tailnet_members"}
	if err := p.Check(Caller{UserID: 7, LoginName: "me@example.com"}); err != nil {
		t.Fatalf("owner refused: %v", err)
	}
	err := p.Check(Caller{UserID: 8, LoginName: "you@example.com"})
	if !errors.Is(err, ErrForbidden) {
		t.Fatalf("other user admitted: %v", err)
	}
	if err := p.Check(Caller{UserID: 7, Tags: []string{"tag:ci"}}); !errors.Is(err, ErrForbidden) {
		t.Fatalf("tagged caller admitted: %v", err)
	}
}

func TestAllowAll(t *testing.T) {
	p := Policy{OwnerID: 7, AllowAll: true}
	for _, c := range []Caller{{UserID: 8}, {Tags: []string{"tag:ci"}}} {
		if err := p.Check(c); err != nil {
			t.Fatalf("%+v refused: %v", c, err)
		}
	}
}

func TestTaggedOwnerAdmitsNobody(t *testing.T) {
	p := Policy{OwnerID: 7, OwnerTags: []string{"tag:thurm"}, AllowFlag: "allow_tailnet_members"}
	if p.Validate() == nil {
		t.Fatal("owner-only on a tagged node validated")
	}
	if err := p.Check(Caller{UserID: 7}); !errors.Is(err, ErrForbidden) {
		t.Fatalf("admitted on a tagged node: %v", err)
	}
	p.AllowAll = true
	if p.Validate() != nil {
		t.Fatal("allow-all on a tagged node refused")
	}
}

func TestOwnerByLogin(t *testing.T) {
	p := Policy{OwnerLogin: "Me@Example.com"}
	if err := p.Check(Caller{LoginName: "me@example.com"}); err != nil {
		t.Fatalf("owner refused: %v", err)
	}
	if err := p.Check(Caller{LoginName: "you@example.com"}); !errors.Is(err, ErrForbidden) {
		t.Fatalf("other login admitted: %v", err)
	}
	if err := (Policy{}).Check(Caller{}); !errors.Is(err, ErrForbidden) {
		t.Fatalf("unknown owner admitted an anonymous caller: %v", err)
	}
}
