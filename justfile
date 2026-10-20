# Release recipes. `just` lists them.
#
# Stable: `just release` runs `vesnik release --yes` (settings in .vesnik.yaml). vesnik bumps the
# version in Cargo.toml, Cargo.lock and macos/Resources/Info.plist, commits it to main, pushes,
# and publishes the GitHub release vX.Y.Z. Publishing starts .github/workflows/release.yaml,
# which waits for CI on that commit, then builds, signs, notarizes, uploads the assets and
# refreshes the appcast. The recipe first drops the local rolling `tip` tag: CI force-moves it,
# and a stale local copy makes vesnik's `git fetch --tags` fail.
# Tip: `just tip` asks the Release workflow for a tip build of the newest green main.

repo := "nklmilojevic/thurm"

default:
    @just --list

# Build the browser client into tailnet/internal/web/dist, which thurm-tailnet embeds
# (scripts/build-web.sh; needs the dev shell).
web:
    scripts/build-web.sh

# Print the current workspace version.
version:
    @sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1

[doc("Bump the version from the commits since the last tag, push it to main and publish vX.Y.Z")]
release:
    @test "$(git branch --show-current)" = main || { echo "release from main" >&2; exit 1; }
    @test -z "$(git status --porcelain)" || { echo "release from a clean tree: vesnik commits untracked files it writes to, such as CHANGELOG.md" >&2; git status --short >&2; exit 1; }
    @git tag -d tip >/dev/null 2>&1 || true
    vesnik release --yes

# FORCE=true builds even when tip is current or only docs changed.
[doc("Start a tip build of the newest main commit that passed CI")]
tip force="false":
    gh workflow run Release --repo {{ repo }} --ref main -f force={{ force }}
    @echo "==> tip build requested; follow it with: just watch"

# Follow the newest Release workflow run.
watch:
    #!/usr/bin/env bash
    set -euo pipefail
    sleep 5
    id="$(gh run list --repo {{ repo }} --workflow Release --limit 1 --json databaseId -q '.[0].databaseId')"
    gh run watch "$id" --repo {{ repo }} --exit-status

# Renovate bumps tailnet/go.mod and go.sum but not this hash. On its PR:
# `gh pr checkout N && just tailnet-hash && git commit -am "chore(nix): tailnet vendorHash" && git push`
[doc("Recompute flake.nix's vendorHash for tailnet/ and check that it builds")]
tailnet-hash:
    #!/usr/bin/env bash
    set -euo pipefail
    old="$(sed -nE 's/.*vendorHash = "(sha256-[^"]+)";.*/\1/p' flake.nix)"
    [[ -n "$old" ]] || { echo "no vendorHash in flake.nix" >&2; exit 1; }
    # Put the old hash back however this stops before the new one is written, Ctrl-C included.
    trap 'sed -i.bak "s|vendorHash = pkgs.lib.fakeHash;|vendorHash = \"$old\";|" flake.nix; rm -f flake.nix.bak' EXIT
    sed -i.bak "s|vendorHash = \"$old\";|vendorHash = pkgs.lib.fakeHash;|" flake.nix && rm flake.nix.bak
    new="$(nix build .#thurm.tailnet.goModules --no-link 2>&1 | sed -nE 's/^ *got: *(sha256-[^ ]+)$/\1/p' || true)"
    [[ -n "$new" ]] || { echo "nix gave no hash; flake.nix left as it was" >&2; exit 1; }
    sed -i.bak "s|vendorHash = pkgs.lib.fakeHash;|vendorHash = \"$new\";|" flake.nix && rm flake.nix.bak
    trap - EXIT
    echo "==> vendorHash $old -> $new"
    nix build .#thurm.tailnet --no-link
