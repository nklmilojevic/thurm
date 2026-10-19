// Package web serves the browser client: the bundle built into the binary, with headers that
// keep the page to its own origin and to the node it was loaded from.
package web

import (
	"embed"
	"io"
	"io/fs"
	"net/http"
	"path"
	"strconv"
	"strings"
)

// dist is the client bundle. `just web` writes it there; only the .gitignore is tracked, and
// a binary built without the bundle serves [placeholder] as its page.
//
//go:embed all:dist
var dist embed.FS

// Embedded is the bundle built into this binary.
func Embedded() fs.FS {
	sub, err := fs.Sub(dist, "dist")
	if err != nil {
		panic(err) // dist is embedded above, so it exists
	}
	return sub
}

// placeholder is the page of a binary built without the bundle.
const placeholder = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Thurm</title>
</head>
<body>
<p>The Thurm web client was not built into this binary.</p>
</body>
</html>
`

// contentTypes covers what the bundle holds; other files are not served.
var contentTypes = map[string]string{
	".html":  "text/html; charset=utf-8",
	".js":    "text/javascript; charset=utf-8",
	".css":   "text/css; charset=utf-8",
	".wasm":  "application/wasm",
	".woff2": "font/woff2",
	".json":  "application/json",
	".svg":   "image/svg+xml",
	".png":   "image/png",
}

// precompressed are the encodings a build may leave beside a file (name.br, name.gz), in the
// order they are preferred.
var precompressed = []struct{ coding, ext string }{
	{"br", ".br"},
	{"gzip", ".gz"},
}

// Handler serves fsys over GET and HEAD. origin is the page's own origin, such as
// https://thurm.tail1234.ts.net: the page may connect back to it, and nowhere else.
func Handler(fsys fs.FS, origin string) http.Handler {
	return &handler{fsys: fsys, csp: contentSecurityPolicy(origin)}
}

type handler struct {
	fsys fs.FS
	csp  string
}

func contentSecurityPolicy(origin string) string {
	// The page connects back over a WebSocket, so the origin's scheme is swapped for its
	// WebSocket one.
	ws := strings.Replace(origin, "https://", "wss://", 1)
	ws = strings.Replace(ws, "http://", "ws://", 1)
	return "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; " +
		"img-src 'self' data: blob:; font-src 'self'; connect-src 'self' " + ws + "; " +
		"worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
}

func (h *handler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	hd := w.Header()
	hd.Set("Content-Security-Policy", h.csp)
	hd.Set("X-Content-Type-Options", "nosniff")
	hd.Set("Referrer-Policy", "no-referrer")
	hd.Set("Cross-Origin-Opener-Policy", "same-origin")
	hd.Set("Cross-Origin-Resource-Policy", "same-origin")
	hd.Set("Vary", "Accept-Encoding")
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		hd.Set("Allow", "GET, HEAD")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	name := strings.TrimPrefix(path.Clean("/"+r.URL.Path), "/")
	if name == "" {
		name = "index.html"
	}
	ctype, ok := contentTypes[path.Ext(name)]
	if !ok || strings.HasSuffix(name, ".br") || strings.HasSuffix(name, ".gz") {
		http.NotFound(w, r)
		return
	}
	if info, err := fs.Stat(h.fsys, name); err != nil || info.IsDir() {
		if name == "index.html" {
			hd.Set("Content-Type", ctype)
			hd.Set("Cache-Control", "no-cache")
			_, _ = io.WriteString(w, placeholder)
			return
		}
		http.NotFound(w, r)
		return
	}

	file, coding := name, ""
	for _, p := range precompressed {
		if !accepts(r.Header.Get("Accept-Encoding"), p.coding) {
			continue
		}
		if info, err := fs.Stat(h.fsys, name+p.ext); err == nil && !info.IsDir() {
			file, coding = name+p.ext, p.coding
			break
		}
	}
	f, err := h.fsys.Open(file)
	if err != nil {
		http.NotFound(w, r)
		return
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		http.Error(w, "reading the web client", http.StatusInternalServerError)
		return
	}

	hd.Set("Content-Type", ctype)
	hd.Set("Content-Length", strconv.FormatInt(info.Size(), 10))
	if coding != "" {
		hd.Set("Content-Encoding", coding)
	}
	// Built assets are named by their content hash, so they never change; everything else,
	// the page included, is revalidated.
	if strings.HasPrefix(name, "assets/") {
		hd.Set("Cache-Control", "public, max-age=31536000, immutable")
	} else {
		hd.Set("Cache-Control", "no-cache")
	}
	w.WriteHeader(http.StatusOK)
	if r.Method == http.MethodGet {
		_, _ = io.Copy(w, f)
	}
}

// accepts reports whether an Accept-Encoding header allows coding: listed, and not with q=0.
func accepts(header, coding string) bool {
	for _, part := range strings.Split(header, ",") {
		fields := strings.Split(part, ";")
		if !strings.EqualFold(strings.TrimSpace(fields[0]), coding) {
			continue
		}
		for _, param := range fields[1:] {
			k, v, ok := strings.Cut(strings.TrimSpace(param), "=")
			if !ok || !strings.EqualFold(strings.TrimSpace(k), "q") {
				continue
			}
			if q, err := strconv.ParseFloat(strings.TrimSpace(v), 64); err == nil && q == 0 {
				return false
			}
		}
		return true
	}
	return false
}
