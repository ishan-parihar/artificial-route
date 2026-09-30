#!/bin/sh
# Install `ar` (artificial-route) — a static musl binary, no runtime deps.
#
#   curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
#
# Env knobs: AR_VERSION (default v0.1.1), AR_INSTALL_DIR (default $HOME/.local/bin).
# Flags: --version <tag> --dir <path>. Unknown flags fail loudly (exit 2).
set -eu

REPO="ishan-parihar/artificial-route"
VERSION="${AR_VERSION:-v0.1.1}"
DIR="${AR_INSTALL_DIR:-$HOME/.local/bin}"

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --dir) DIR="$2"; shift 2 ;;
    -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
    *) echo "error: unknown flag '$1' (see --help)" >&2; exit 2 ;;
  esac
done

ARCH="$(uname -m)"
[ "$ARCH" = "x86_64" ] || { echo "error: no prebuilt binary for arch '$ARCH' (want x86_64)" >&2; exit 1; }
[ "$(uname -s)" = "Linux" ] || { echo "error: no prebuilt binary for '$(uname -s)' (want Linux)" >&2; exit 1; }

ASSET="ar-x86_64-unknown-linux-musl"
BASE="https://github.com/$REPO/releases/download/$VERSION"

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT INT TERM
cd "$TMP"
echo "downloading ar $VERSION ..."
curl -fsSL -o "$ASSET" "$BASE/$ASSET"
curl -fsSL -o SHA256SUMS "$BASE/SHA256SUMS"

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
mkdir -p "$DIR"
mv "$ASSET" "$DIR/ar"
"$DIR/ar" --version

cat <<EOF

installed to $DIR/ar — cold start:

  export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
  ar doctor     # 9 checks, never prints secret values
  ar serve      # loopback :20128
  curl -s localhost:20128/healthz && curl -s localhost:20128/v1/models

Config defaults to ./config.yaml (see repo config.yaml for the shape).
EOF
case ":$PATH:" in *":$DIR:"*) ;; *) echo "note: $DIR is not on PATH" >&2 ;; esac
