#!/usr/bin/env bash
# Builds the browser client (specs/web.md) into tailnet/internal/web/dist, which thurm-tailnet
# embeds: the wasm session (thurm-web, size-optimized), its JS bindings, and the page.
# Needs Zig 0.16, wasm-bindgen-cli (the version thurm-web pins), wasm-opt (binaryen) and bun:
# the dev shell has them. THURM_WEB_BUDGET_KIB fails the build above that many KiB (brotli).
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO_PROFILE_RELEASE_OPT_LEVEL=s cargo build -p thurm-web --target wasm32-unknown-unknown --release --locked
wasm-bindgen --target web --out-dir web/pkg --out-name thurm_web \
    target/wasm32-unknown-unknown/release/thurm_web.wasm
wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types --enable-multivalue --enable-simd \
    -o web/pkg/thurm_web_bg.wasm web/pkg/thurm_web_bg.wasm
cd web
bun install --frozen-lockfile
bun build.ts
