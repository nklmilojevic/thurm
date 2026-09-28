---
title: Development and documentation
sidebar:
  label: Development
---

## Source layout

| Path | Purpose |
| --- | --- |
| `macos/` | Swift app, AppKit interface, Metal rendering, and app packaging |
| `crates/thurm-daemon/` | Processes, panes, agent tracking, and saved sessions |
| `crates/thurm-term/` | Terminal state, graphics, input encoding, and scrollback |
| `crates/thurm-config/` | Configuration, themes, and agent definitions |
| `crates/thurm-proto/` | Shared messages and layout data |
| `crates/thurm-client/` | Rust daemon client |
| `crates/thurm-ffi/` | C interface for the Swift app |
| `crates/thurm-cli/` | Command-line interface |
| `shell-integration/` | zsh, bash, and fish integration |
| `vendor/libghostty-vt-sys/` | Bindings and build code for pinned Ghostty source |

The app connects to the daemon over a Unix socket. The daemon owns terminal
processes and state. The Swift app reads screen grids through the Rust FFI and
draws them with Metal and CoreText.

See the [macOS developer guide](https://github.com/nklmilojevic/thurm/blob/main/macos/README.md)
for app builds, rendering, signing, notarization, and release setup.

## Project checks

With Zig 0.16.0 on `PATH`, run from the repository root:

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
uv run --no-project python -m unittest discover -s macos/release -v
```

CI runs Rust checks on macOS. It also tests release scripts and builds the
documentation. Build the app separately with `./macos/build.sh`.

## Edit and preview these docs

The site uses [Starlight](https://starlight.astro.build/) with the
[Catppuccin theme](https://github.com/catppuccin/starlight). Install
[Bun](https://bun.sh/), or use `nix develop`. Run these commands from `docs/`:

```sh
bun install
bun run dev
```

Open the local address printed by Astro. Before you commit, run
`bun run build`. The build fails on broken internal links and anchors.

Edit pages in `docs/src/content/docs/`. Each page needs a `title` in its
frontmatter. Add new pages to `sidebar` in `docs/astro.config.mjs`. Link to other
pages by their full path, such as `/thurm/usage/`. Edit `config.example.toml` to
change the configuration reference; the page includes that file on each build.

## Publish to GitHub Pages

The repository Pages source must be **GitHub Actions**. The `Pages` workflow
builds and checks the docs, generates `appcast.xml` from published release
metadata, and uploads one site artifact. This keeps documentation and app
updates at the same site. With no published releases, it publishes only the
docs. If releases exist but feed generation fails, deployment stops.

Changes to docs and their build files on `main` publish automatically.
The `Release` workflow starts `Pages` after it publishes a build, so each
release and tip build updates the feed. A night without a tip build deploys
nothing. Maintainers can start `Pages` manually from
the Actions tab. All deployments share one concurrency group.

The release workflow only publishes release assets. It must not deploy a
separate site artifact, because a Pages deployment replaces the complete site.

The site build uses the standard [GitHub Pages Actions workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages).
