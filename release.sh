#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

NDP=navidrome-listenbrainz-plugin.ndp

sign() {
  local key
  key="$(git config --get user.signingkey)"
  key="${key/#\~/$HOME}"
  if [ -n "$key" ] && [ -f "$key" ]; then
    ssh-keygen -Y sign -f "$key" -n file "$NDP"
  else
    echo "release.sh: no signing key configured, publishing unsigned" >&2
  fi
}

validate() {
  local ndp="$PWD/$NDP" src="${NAVIDROME_SRC:-$HOME/Git/navidrome}"
  if command -v navidrome >/dev/null; then
    navidrome plugin validate "$ndp"
  elif [ -f "$src/go.mod" ]; then
    (cd "$src" && go run -tags netgo,sqlite_fts5 . plugin validate "$ndp")
  else
    echo "release.sh: need a navidrome binary, or its source at $src, to validate" >&2
    exit 1
  fi
}

build() {
  cargo build --release --target wasm32-wasip1
  rm -f "$NDP" "$NDP.sig"
  zip -j "$NDP" manifest.json target/wasm32-wasip1/release/plugin.wasm
  sign
  validate
}

if [ "${1:-}" = "--build" ]; then
  build
  exit
fi

VERSION="${1:-}"
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $(basename "$0") [--build | X.Y.Z]" >&2
  exit 2
fi
if [ -n "$(git status --porcelain)" ]; then
  echo "release.sh: working tree is dirty, commit it first" >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/$VERSION" >/dev/null; then
  echo "release.sh: tag $VERSION already exists" >&2
  exit 1
fi

perl -pi -e "s/^  \"version\": \"[^\"]*\"/  \"version\": \"$VERSION\"/" manifest.json
perl -pi -e "s/^version = \"[^\"]*\"/version = \"$VERSION\"/" Cargo.toml

NOTES="$(git-cliff --unreleased --tag "$VERSION")"

build

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
git commit -am "chore(release): $VERSION"
git tag "$VERSION"
git push origin "$BRANCH" "$VERSION"
git push github "$BRANCH" "$VERSION"

ASSETS=("$NDP")
if [ -f "$NDP.sig" ]; then
  ASSETS+=("$NDP.sig")
fi

printf '%s\n' "$NOTES" | gh release create "$VERSION" --title "$VERSION" --notes-file - --prerelease "${ASSETS[@]}"
