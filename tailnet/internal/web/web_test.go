package web

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"testing/fstest"
)

const testOrigin = "https://thurm.tail1234.ts.net"

func testFS() fstest.MapFS {
	return fstest.MapFS{
		"index.html":          {Data: []byte("<!doctype html>page")},
		"assets/app.js":       {Data: []byte("app")},
		"assets/app.js.br":    {Data: []byte("app-br")},
		"assets/app.js.gz":    {Data: []byte("app-gz")},
		"assets/app.css":      {Data: []byte("a{}")},
		"assets/core.wasm":    {Data: []byte("\x00asm")},
		"assets/font.woff2":   {Data: []byte("woff")},
		"assets/data.json":    {Data: []byte("{}")},
		"assets/icon.svg":     {Data: []byte("<svg/>")},
		"assets/icon.png":     {Data: []byte("png")},
		"assets/only.js.gz":   {Data: []byte("gz-only")},
		"secret.txt":          {Data: []byte("not served")},
		"notes/readme.txt":    {Data: []byte("not served")},
		"nested/index.html":   {Data: []byte("nested")},
		"nested/.hidden.html": {Data: []byte("hidden")},
	}
}

func serve(t *testing.T, h http.Handler, method, path, acceptEncoding string) *httptest.ResponseRecorder {
	t.Helper()
	req := httptest.NewRequest(method, path, nil)
	if acceptEncoding != "" {
		req.Header.Set("Accept-Encoding", acceptEncoding)
	}
	rec := httptest.NewRecorder()
	h.ServeHTTP(rec, req)
	return rec
}

func body(t *testing.T, rec *httptest.ResponseRecorder) string {
	t.Helper()
	b, err := io.ReadAll(rec.Result().Body)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func TestIndexIsServedAtRoot(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	rec := serve(t, h, http.MethodGet, "/", "")
	if rec.Code != http.StatusOK || body(t, rec) != "<!doctype html>page" {
		t.Fatalf("status %d", rec.Code)
	}
	if ct := rec.Header().Get("Content-Type"); ct != "text/html; charset=utf-8" {
		t.Fatalf("content type %q", ct)
	}
}

func TestContentTypes(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	cases := map[string]string{
		"/assets/app.js":     "text/javascript; charset=utf-8",
		"/assets/app.css":    "text/css; charset=utf-8",
		"/assets/core.wasm":  "application/wasm",
		"/assets/font.woff2": "font/woff2",
		"/assets/data.json":  "application/json",
		"/assets/icon.svg":   "image/svg+xml",
		"/assets/icon.png":   "image/png",
		"/index.html":        "text/html; charset=utf-8",
	}
	for path, want := range cases {
		rec := serve(t, h, http.MethodGet, path, "")
		if rec.Code != http.StatusOK {
			t.Fatalf("%s: status %d", path, rec.Code)
		}
		if got := rec.Header().Get("Content-Type"); got != want {
			t.Errorf("%s: content type %q, want %q", path, got, want)
		}
	}
}

func TestPrecompressedVariantsAreChosenByAcceptEncoding(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	cases := []struct {
		accept, body, encoding string
	}{
		{"gzip, br", "app-br", "br"},
		{"br;q=1.0, gzip;q=0.5", "app-br", "br"},
		{"gzip", "app-gz", "gzip"},
		{"br;q=0, gzip", "app-gz", "gzip"},
		{"", "app", ""},
		{"identity", "app", ""},
	}
	for _, c := range cases {
		rec := serve(t, h, http.MethodGet, "/assets/app.js", c.accept)
		if got := body(t, rec); got != c.body {
			t.Errorf("accept %q: body %q, want %q", c.accept, got, c.body)
		}
		if got := rec.Header().Get("Content-Encoding"); got != c.encoding {
			t.Errorf("accept %q: Content-Encoding %q, want %q", c.accept, got, c.encoding)
		}
		if ct := rec.Header().Get("Content-Type"); ct != "text/javascript; charset=utf-8" {
			t.Errorf("accept %q: content type %q", c.accept, ct)
		}
		if v := rec.Header().Get("Vary"); v != "Accept-Encoding" {
			t.Errorf("accept %q: Vary %q", c.accept, v)
		}
	}
}

func TestPrecompressedFallsBackToTheFileWhenNoVariantExists(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	rec := serve(t, h, http.MethodGet, "/assets/app.css", "br, gzip")
	if rec.Header().Get("Content-Encoding") != "" || body(t, rec) != "a{}" {
		t.Fatalf("encoding %q", rec.Header().Get("Content-Encoding"))
	}
	rec = serve(t, h, http.MethodGet, "/assets/only.js", "gzip")
	if rec.Code != http.StatusNotFound {
		t.Fatalf("status %d for a file that is not there", rec.Code)
	}
}

func TestVariantFilesAreNotServedByTheirOwnNames(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	for _, path := range []string{"/assets/app.js.br", "/assets/app.js.gz", "/assets/only.js.gz"} {
		if rec := serve(t, h, http.MethodGet, path, "gzip, br"); rec.Code != http.StatusNotFound {
			t.Errorf("%s: status %d", path, rec.Code)
		}
	}
}

func TestOnlyTheBundleIsServed(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	for _, path := range []string{"/secret.txt", "/notes/readme.txt", "/nope.js", "/assets/", "/nested", "/nested/", "/../secret.txt"} {
		rec := serve(t, h, http.MethodGet, path, "")
		if rec.Code != http.StatusNotFound {
			t.Errorf("%s: status %d", path, rec.Code)
		}
	}
	if rec := serve(t, h, http.MethodGet, "/nested/.hidden.html", ""); rec.Code != http.StatusOK {
		t.Errorf("a dotted name in a directory: status %d", rec.Code)
	}
}

func TestOnlyGetAndHead(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	for _, m := range []string{http.MethodPost, http.MethodPut, http.MethodDelete, http.MethodOptions} {
		rec := serve(t, h, m, "/", "")
		if rec.Code != http.StatusMethodNotAllowed || rec.Header().Get("Allow") != "GET, HEAD" {
			t.Errorf("%s: status %d, Allow %q", m, rec.Code, rec.Header().Get("Allow"))
		}
	}
}

func TestHeadHasHeadersButNoBody(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	rec := serve(t, h, http.MethodHead, "/assets/app.js", "gzip")
	if rec.Code != http.StatusOK || rec.Body.Len() != 0 {
		t.Fatalf("status %d, body %q", rec.Code, rec.Body.String())
	}
	if rec.Header().Get("Content-Encoding") != "gzip" || rec.Header().Get("Content-Length") != "6" {
		t.Fatalf("headers %v", rec.Header())
	}
}

func TestCacheHeaders(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	for _, path := range []string{"/", "/index.html", "/nested/index.html"} {
		if got := serve(t, h, http.MethodGet, path, "").Header().Get("Cache-Control"); got != "no-cache" {
			t.Errorf("%s: Cache-Control %q", path, got)
		}
	}
	for _, path := range []string{"/assets/app.js", "/assets/app.css", "/assets/font.woff2"} {
		want := "public, max-age=31536000, immutable"
		if got := serve(t, h, http.MethodGet, path, "").Header().Get("Cache-Control"); got != want {
			t.Errorf("%s: Cache-Control %q", path, got)
		}
	}
}

func TestContentSecurityPolicy(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	want := "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; " +
		"img-src 'self' data: blob:; font-src 'self'; connect-src 'self' wss://thurm.tail1234.ts.net; " +
		"worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
	rec := serve(t, h, http.MethodGet, "/", "")
	if got := rec.Header().Get("Content-Security-Policy"); got != want {
		t.Fatalf("CSP %q", got)
	}
	dev := Handler(testFS(), "http://127.0.0.1:7297")
	if got := serve(t, dev, http.MethodGet, "/", "").Header().Get("Content-Security-Policy"); !strings.Contains(got, "connect-src 'self' ws://127.0.0.1:7297;") {
		t.Fatalf("dev CSP %q", got)
	}
}

func TestSecurityHeadersOnEveryResponse(t *testing.T) {
	h := Handler(testFS(), testOrigin)
	checks := map[string]string{
		"X-Content-Type-Options":       "nosniff",
		"Referrer-Policy":              "no-referrer",
		"Cross-Origin-Opener-Policy":   "same-origin",
		"Cross-Origin-Resource-Policy": "same-origin",
	}
	for _, r := range []struct {
		method, path string
	}{
		{http.MethodGet, "/"},
		{http.MethodGet, "/missing"},
		{http.MethodPost, "/"},
	} {
		rec := serve(t, h, r.method, r.path, "")
		for k, v := range checks {
			if got := rec.Header().Get(k); got != v {
				t.Errorf("%s %s: %s %q", r.method, r.path, k, got)
			}
		}
		if rec.Header().Get("Content-Security-Policy") == "" {
			t.Errorf("%s %s: no CSP", r.method, r.path)
		}
	}
}

func TestServesThePlaceholderWithoutABundle(t *testing.T) {
	h := Handler(fstest.MapFS{}, testOrigin)
	rec := serve(t, h, http.MethodGet, "/", "")
	if rec.Code != http.StatusOK || !strings.Contains(body(t, rec), "not built into this binary") {
		t.Fatalf("status %d", rec.Code)
	}
}

func TestAcceptsParsesQValues(t *testing.T) {
	cases := []struct {
		header, coding string
		want           bool
	}{
		{"gzip, br", "br", true},
		{"br;q=0.0", "br", false},
		{"br;q=0", "br", false},
		{"br; q=0.5", "br", true},
		{"BR", "br", true},
		{"gzip", "br", false},
		{"", "gzip", false},
	}
	for _, c := range cases {
		if got := accepts(c.header, c.coding); got != c.want {
			t.Errorf("accepts(%q, %q) = %v", c.header, c.coding, got)
		}
	}
}
