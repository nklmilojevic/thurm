# Development and documentation

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

Install [uv](https://docs.astral.sh/uv/getting-started/installation/).
Use Python 3.11 or later. CI uses Python 3.12. Run these commands from the
repository root:

```sh
uv run --no-project python scripts/prepare-docs.py
uv run --no-project --with-requirements requirements-docs.txt mkdocs serve
```

uv manages the command environment and installs the documentation dependencies
from `requirements-docs.txt`. No manual environment activation is needed.

Open the local address printed by MkDocs. Before you commit:

```sh
uv run --no-project python scripts/prepare-docs.py
uv run --no-project --with-requirements requirements-docs.txt mkdocs build --strict
```

Edit Markdown in `docs/`. Add pages to `mkdocs.yml`. Use relative Markdown
links so MkDocs can check them. Edit `config.example.toml` to change the
configuration reference; `scripts/prepare-docs.py` generates that page on each
build. Do not edit the generated page directly.

## Publish to GitHub Pages

The repository Pages source must be **GitHub Actions**. The `Pages` workflow
builds and checks the docs, generates `appcast.xml` from published release
metadata, and uploads one site artifact. This keeps documentation and app
updates at the same site. With no published releases, it publishes only the
docs. If releases exist but feed generation fails, deployment stops.

Changes to docs and their build files on `main` publish automatically.
A successful `Release` workflow also starts `Pages`, so releases made with the
workflow token update the feed. Maintainers can start `Pages` manually from
the Actions tab. All deployments share one concurrency group.

The release workflow only publishes release assets. It must not deploy a
separate site artifact, because a Pages deployment replaces the complete site.

The site build uses [MkDocs](https://www.mkdocs.org/user-guide/configuration/)
and the standard [GitHub Pages Actions workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages).
