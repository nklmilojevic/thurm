// Command thurm-webtest serves the browser client over plain HTTP for end-to-end tests. It is a
// development harness and is never shipped: it stands in for the node's HTTPS listener, admitting
// one fixed owner and proxying /v1/connect to a thurmd socket the test starts.
//
//	thurm-webtest --socket PATH --dist DIR [--listen 127.0.0.1:0]
//
// Once listening it prints "listening on http://HOST:PORT" to stdout.
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"

	"github.com/nklmilojevic/thurm/tailnet/internal/admit"
	"github.com/nklmilojevic/thurm/tailnet/internal/gateway"
	"github.com/nklmilojevic/thurm/tailnet/internal/web"
)

// owner is the one caller the harness admits.
const owner = int64(1)

func main() {
	listen := flag.String("listen", "127.0.0.1:0", "the address to listen on")
	socket := flag.String("socket", "", "the thurmd socket to proxy to")
	dist := flag.String("dist", "", "serve the web client from this directory, not the embedded bundle")
	flag.Parse()
	log.SetFlags(0)
	log.SetPrefix("thurm-webtest: ")
	if *socket == "" || *dist == "" {
		log.Fatal("usage: thurm-webtest --socket PATH --dist DIR [--listen ADDR]")
	}
	ln, err := net.Listen("tcp", *listen)
	if err != nil {
		log.Fatal(err)
	}
	origin := "http://" + ln.Addr().String()
	gw := &gateway.Gateway{
		Via: "webtest",
		Identify: func(*http.Request) (admit.Caller, error) {
			return admit.Caller{UserID: owner, LoginName: "test@example", Node: "webtest"}, nil
		},
		Policy: func() admit.Policy { return admit.Policy{OwnerID: owner, AllowFlag: "webtest"} },
		Dial: func(ctx context.Context) (net.Conn, error) {
			var d net.Dialer
			return d.DialContext(ctx, "unix", *socket)
		},
		Logf: log.Printf,
	}
	gw.SetBrowser(origin, web.Handler(os.DirFS(*dist), origin))
	fmt.Printf("listening on %s\n", origin)
	if err := gw.Serve(ln); err != nil {
		log.Fatal(err)
	}
}
