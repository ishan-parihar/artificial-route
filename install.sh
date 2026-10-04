#!/bin/sh
# Install or update `ar` (artificial-route) — a static musl binary, no runtime deps.
#
#   curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
#
# Cold install and update are the same command: it installs the newest release,
# and re-running it over an existing install replaces the binary. There is no
# separate updater to keep in sync.
#
# Env knobs: AR_VERSION (pin a tag; default: newest release),
#            AR_INSTALL_DIR (default $HOME/.local/bin).
# Flags: --version <tag>   pin an exact tag instead of the newest
#        --dir <path>      install somewhere else
#        --check           report installed vs newest, change nothing
#        --help
# Unknown flags fail loudly (exit 2).
set -eu

REPO="ishan-parihar/artificial-route"
API="https://api.github.com/repos/$REPO/releases/latest"
DIR="${AR_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${AR_VERSION:-}"
CHECK_ONLY=0
TARGET="$DIR/ar"

# Everything up to the first line of code, minus the shebang: editing the header
# comment must not require editing a line number here.
usage() { sed -n '2,/^set -eu$/p' "$0" | sed '$d' | sed 's/^#\{0,1\} \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --dir) DIR="$2"; shift 2 ;;
    --check) CHECK_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown flag '$1' (see --help)" >&2; exit 2 ;;
  esac
done

case "$(uname -s)" in
  Linux) ;;
  *) echo "error: no prebuilt binary for '$(uname -s)' (want Linux)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ;;
  *) echo "error: no prebuilt binary for arch '$(uname -m)' (want x86_64)" >&2; exit 1 ;;
esac

ASSET="ar-x86_64-unknown-linux-musl"
BASE="https://github.com/$REPO/releases/download/$VERSION"

# Newest release tag. Falls back to "v0.0.0" rather than aborting: an API that is
# rate-limited or briefly down should not make an upgrade path impossible, and a
# bogus tag fails loudly at the download step with a clearer message than
# "could not resolve latest".
latest_tag() {
  curl -fsSL -H 'Accept: application/vnd.github+json' "$API" 2>/dev/null \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -1
}

LATEST="$(latest_tag || true)"
[ -n "$LATEST" ] || LATEST="v0.0.0"
[ -n "$VERSION" ] || VERSION="$LATEST"

installed_tag() {
  [ -x "$TARGET" ] || return 1
  "$TARGET" --version 2>/dev/null | sed -n 's/^ar //p'
}

CURRENT="$(installed_tag || true)"
if [ "$CHECK_ONLY" -eq 1 ]; then
  [ -n "$CURRENT" ] || CURRENT="(none)"
  printf 'installed: %s\nlatest:    %s\n' "$CURRENT" "$LATEST"
  if [ "$CURRENT" = "(none)" ]; then
    echo "status:    not installed"
  elif [ "$CURRENT" = "$LATEST" ]; then
    echo "status:    up to date"
  else
    echo "status:    update available (run this script again to install $LATEST)"
  fi
  exit 0
fi

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT INT TERM
cd "$TMP"
echo "downloading ar $VERSION ..."
curl -fsSL -o "$ASSET" "$BASE/$ASSET"
curl -fsSL -o SHA256SUMS "https://github.com/$REPO/releases/download/$VERSION/SHA256SUMS"

# The checksum is checked before the binary is ever executable, so a corrupt or
# substituted download cannot run. Missing tooling is a warning rather than a
# skip: a verified install should not be the one that silently gives up.
if command -v sha256sum >/dev/null 2>&1; then
  (grep " $ASSET\$" SHA256SUMS | sha256sum -c -) || { echo "error: checksum mismatch" >&2; exit 1; }
elif command -v shasum >/dev/null 2>&1; then
  EXPECTED="$(grep " $ASSET\$" SHA256SUMS | cut -d' ' -f1)"
  ACTUAL="$(shasum -a 256 "$ASSET" | cut -d' ' -f1)"
  [ "$EXPECTED" = "$ACTUAL" ] || { echo "error: checksum mismatch" >&2; exit 1; }
else
  echo "warning: no sha256sum/shasum found, skipping checksum verify" >&2
fi

chmod +x "$ASSET"
# Install beside the target and rename into place: a reader that starts `ar`
# during the swap sees either the old binary or the new one, never a truncated
# file that fails to exec.
mkdir -p "$DIR"
cp "$ASSET" "$DIR/.ar.new"
chmod +x "$DIR/.ar.new"
mv "$DIR/.ar.new" "$TARGET"
INSTALLED="$("$TARGET" --version)"

if [ -n "$CURRENT" ] && [ "$CURRENT" != "$VERSION" ]; then
  echo "updated $CURRENT -> $INSTALLED"
else
  echo "installed $INSTALLED"
fi

cat <<EOF

installed to $TARGET — first run:

  export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
  ar doctor     # never prints secret values
  ar serve      # loopback :20128
  curl -s localhost:20128/healthz && curl -s localhost:20128/v1/models

Config defaults to ./config.yaml (see repo config.yaml for the shape).
Upgrade later:  sh install.sh        (or: sh install.sh --check)
EOF
case ":$PATH:" in *":$DIR:"*) ;; *) echo "note: $DIR is not on PATH" >&2 ;; esac
