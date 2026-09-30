# Release recipes. `just` lists them.
#
# Stable: `just release patch|minor|major` bumps the workspace version, commits it to main,
# and publishes the GitHub release vX.Y.Z. Publishing starts .github/workflows/release.yaml,
# which builds, signs, notarizes, uploads the assets and refreshes the appcast.
# Tip: `just tip` asks the Release workflow for a tip build of the newest green main.

repo := "nklmilojevic/thurm"

default:
    @just --list

# Print the current workspace version.
version:
    @sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1

# Print the version that `just release LEVEL` would publish.
next-version level:
    #!/usr/bin/env bash
    set -euo pipefail
    current="$(just version)"
    IFS=. read -r major minor patch <<<"$current"
    case "{{ level }}" in
        major) echo "$((major + 1)).0.0" ;;
        minor) echo "$major.$((minor + 1)).0" ;;
        patch) echo "$major.$minor.$((patch + 1))" ;;
        *) echo "level must be major, minor or patch, not '{{ level }}'" >&2; exit 2 ;;
    esac

# NOTES is a markdown file for the release notes (they also show in the update dialog);
# without it GitHub generates them. The release waits for CI on the bump commit, because
# release.yaml only builds a tag whose commit passed CI on main.
[confirm("This commits to main, pushes, and publishes a stable release. Continue?")]
[doc("Bump the version (major, minor or patch), push it to main and publish vX.Y.Z")]
release level notes="":
    #!/usr/bin/env bash
    set -euo pipefail
    just _check-main "{{ notes }}"
    version="$(just next-version {{ level }})"
    just _check-tag "v$version"

    echo "==> $(just version) -> $version"
    just _set-version "$version"
    git add Cargo.toml Cargo.lock macos/Resources/Info.plist
    git commit -q -m "chore(release): v$version"
    git push -q origin main
    just _publish "{{ notes }}"

# Use it to retry when CI failed on the bump commit: fix main, then publish the same version.
[confirm("This publishes a stable release of the current version. Continue?")]
[doc("Publish the version in Cargo.toml as vX.Y.Z once main passes CI")]
publish notes="":
    #!/usr/bin/env bash
    set -euo pipefail
    just _check-main "{{ notes }}"
    just _check-tag "v$(just version)"
    just _publish "{{ notes }}"

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

# Write VERSION into Cargo.toml, Cargo.lock and Info.plist.
_set-version version:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ "{{ version }}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "not a version: {{ version }}" >&2; exit 2; }
    sed -i.bak 's/^version = ".*"/version = "{{ version }}"/' Cargo.toml && rm Cargo.toml.bak
    perl -0pi -e 's|(<key>CFBundleShortVersionString</key>\s*<string>)[^<]*(</string>)|${1}{{ version }}${2}|' \
        macos/Resources/Info.plist
    grep -q '<string>{{ version }}</string>' macos/Resources/Info.plist
    if command -v cargo >/dev/null; then
        cargo update --workspace --offline -q
    else
        nix develop -c cargo update --workspace --offline -q
    fi
    grep -q '^version = "{{ version }}"' Cargo.toml

# Wait for CI on HEAD (the same filter as release.yaml), then create the GitHub release.
_publish notes:
    #!/usr/bin/env bash
    set -euo pipefail
    version="$(just version)"
    tag="v$version"
    sha="$(git rev-parse HEAD)"
    echo "==> waiting for CI on ${sha:0:7}"
    id=""
    for _ in $(seq 1 30); do
        id="$(gh run list --repo {{ repo }} --workflow CI --commit "$sha" --branch main --event push \
            --limit 1 --json databaseId -q '.[0].databaseId // empty')"
        [[ -n "$id" ]] && break
        sleep 10
    done
    [[ -n "$id" ]] || { echo "no CI run found for ${sha:0:7}; nothing was published" >&2; exit 1; }
    if ! gh run watch "$id" --repo {{ repo }} --exit-status --interval 30 >/dev/null; then
        echo "CI failed on ${sha:0:7}; nothing was published. Fix main, then run: just publish" >&2
        exit 1
    fi
    passed="$(gh run list --repo {{ repo }} --workflow CI --commit "$sha" --branch main --event push \
        --status success --limit 1 --json headSha -q '.[0].headSha // empty')"
    [[ "$passed" == "$sha" ]] || { echo "CI did not pass on ${sha:0:7}; nothing was published" >&2; exit 1; }

    echo "==> CI passed; publishing $tag at ${sha:0:7}"
    if [[ -n "{{ notes }}" ]]; then
        gh release create "$tag" --repo {{ repo }} --target "$sha" --title "Thurm $version" --notes-file "{{ notes }}"
    else
        gh release create "$tag" --repo {{ repo }} --target "$sha" --title "Thurm $version" --generate-notes
    fi
    echo "==> the Release workflow is building $tag; follow it with: just watch"

# Stop unless main is clean, matches origin/main, and the notes file (if any) exists.
_check-main notes:
    #!/usr/bin/env bash
    set -euo pipefail
    [[ "$(git rev-parse --abbrev-ref HEAD)" == main ]] || { echo "switch to main first" >&2; exit 1; }
    [[ -z "$(git status --porcelain)" ]] || { echo "the working tree is not clean" >&2; exit 1; }
    git fetch -q origin main --tags
    [[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] \
        || { echo "main is not the same as origin/main; pull or push first" >&2; exit 1; }
    if [[ -n "{{ notes }}" && ! -f "{{ notes }}" ]]; then
        echo "notes file not found: {{ notes }}" >&2; exit 1
    fi

# Stop if TAG exists locally, on the remote, or as a GitHub release.
_check-tag tag:
    #!/usr/bin/env bash
    set -euo pipefail
    if git rev-parse -q --verify "refs/tags/{{ tag }}" >/dev/null \
        || gh release view "{{ tag }}" --repo {{ repo }} >/dev/null 2>&1; then
        echo "{{ tag }} already exists" >&2; exit 1
    fi
