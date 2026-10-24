#!/usr/bin/env bash
# Install, update, or remove `ar` (artificial-route) — a static musl binary with
# no runtime dependencies.
#
#   curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | bash
#
# One command does the whole job: installs the newest release, writes a working
# config, installs and starts a systemd unit so the proxy comes up on boot, and
# proves the result with `aroute doctor`. Re-running it over an existing install
# updates the binary and leaves your config alone. There is no separate updater
# to drift out of sync with this one.
#
# Flags:
#   --version <tag>        pin an exact tag instead of the newest release
#   --dir <path>           install somewhere else (default $HOME/.local/bin)
#   --service <scope>      systemd unit scope: system | user | none
#                          (default: system when run as root, else user)
#   --no-service           same as --service none
#   --lite                 rust binary only — no web dashboard (the default)
#   --with-dashboard       also install the web dashboard from a local build.
#                          The dist (~2 GiB of compiled Next.js plus its traced
#                          node_modules) is too large for a release asset, so it
#                          is built on this machine (`dashboard/rebrand-dist.sh`)
#                          and laid down from that tree — nothing is downloaded
#                          from npm at install time.
#   --dashboard-src <path>  the dist to install: the `dashboard/dist` directory
#                          or a tarball of it. Default: `dashboard/dist` in the
#                          checkout this script lives in.
#   --check                report installed vs newest and service state; change nothing
#   --uninstall            remove the binary, unit, autostart, and dashboard dist;
#                          keep the config, credentials, and dashboard data
#   --force-root           allow running as root (default: refuse)
#   --no-cta               suppress the star/donate CTA
#   --all-harnesses        register the MCP server in every detected harness
#                          (claude and codex are registered; opencode gets its JSON printed)
#   --no-harness           skip harness/MCP registration (set SKIP_HARNESS=1)
#   --verify-attestation   also check the binary's GitHub build attestation with
#                          `gh attestation verify`; fatal on failure. Opt-in: needs
#                          the gh CLI, and releases published before attestations
#                          existed have none to check.
#   --help
# Unknown flags fail loudly (exit 2).
#
# Env knobs: AR_VERSION (pin a tag), AR_INSTALL_DIR (install dir),
#            AR_SERVICE (unit scope), AR_CONFIG (config path for the unit),
#            AR_DASHBOARD (1 = --with-dashboard), AR_DASHBOARD_SRC (dist source),
#            AR_DASHBOARD_PORT (the dashboard unit's port, default 20149),
#            AR_DASHBOARD_BASE_PATH (subpath for the one-way base-path patch,
#            default /dash; empty serves at /).
#            SKIP_CTA=1, FORCE_ROOT=1, SKIP_HARNESS=1, ALL_HARNESS=1.
#
# Runtime on/off, once installed: the dashboard is its own systemd unit so the
# ~960 MB Node child is started only on demand —
#   systemctl --user start aroute-dashboard   # bring the UI up
#   systemctl --user stop  aroute-dashboard   # reclaim the RSS
# and it is deliberately NOT started at install time; the proxy never needs it.
set -eu
# pipefail is a bash/ksh extension, not POSIX: this script installs via
# `curl … | sh` where sh is dash, and `set -o pipefail` is an illegal
# option there. Every pipeline's failure path is guarded explicitly
# instead (`latest_tag || true`, `grep -q … || true`).
# `usage()` greps this header up to the marker line below — keep both
# lines in sync if this block moves.
IFS='
	'

REPO="ishan-parihar/artificial-route"
API="https://api.github.com/repos/$REPO/releases/latest"
ASSET="aroute-x86_64-unknown-linux-musl"
SERVICE_NAME="aroute"

# A bare `curl | sh` inherits whatever HOME the caller's shell had, which is not
# a safe thing to assume: some CI images and some `su -c` invocations unset it,
# and `set -u` then dies with "HOME: unbound variable" before printing a single
# useful line. Derive it from the passwd entry instead of trusting the
# environment, and only fall back to the environment if that fails.
resolve_home() {
  if [ -n "${HOME:-}" ]; then
    printf '%s' "$HOME"
    return 0
  fi
  _h="$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f6 || true)"
  [ -n "$_h" ] || _h="$(id -un 2>/dev/null | sed 's|^|/home/|' || true)"
  [ -n "$_h" ] || _h="/root"
  printf '%s' "$_h"
}
HOME="$(resolve_home)"

DIR="${AR_INSTALL_DIR:-$HOME/.local/bin}"
CONFIG="${AR_CONFIG:-$HOME/.config/ar/config.yaml}"
VERSION="${AR_VERSION:-}"
SERVICE="${AR_SERVICE:-auto}"
DASHBOARD="${AR_DASHBOARD:-0}"
DASH_SRC="${AR_DASHBOARD_SRC:-}"
DASH_PORT="${AR_DASHBOARD_PORT:-20149}"
# The subpath the installed dist serves under, when the tarball was built for
# the domain root and the installer converts it (the one-way patch). A dist
# already baked for /dash carries a BUILD_OMNIROUTE_BASE_PATH sentinel and
# the patch is skipped; empty means serve at /.
DASH_BASE_PATH="${AR_DASHBOARD_BASE_PATH:-/dash}"
CHECK_ONLY=0
UNINSTALL=0
SKIP_CTA="${SKIP_CTA:-0}"
FORCE_ROOT="${FORCE_ROOT:-0}"
SKIP_HARNESS="${SKIP_HARNESS:-0}"
ALL_HARNESS="${ALL_HARNESS:-0}"
VERIFY_ATTESTATION=0

# Absolute, resolved before anything cd's away from the caller's directory.
# A relative $0 stops existing the moment the script does `cd "$TMP"`, which
# silently turned the update timer into "skipped" on every install run from a
# file — the one case it was written for.
SCRIPT_SRC=""
if [ -f "${0:-}" ]; then
  SCRIPT_SRC="$(cd "$(dirname "$0")" 2>/dev/null && pwd)/$(basename "$0")" || SCRIPT_SRC="$0"
fi

# Defaults for every path the summary prints. Each of these is assigned only
# inside the service branch, and the summary is printed unconditionally, so
# `--service none` aborted on an unbound variable without one.
SERVICE_STATE="not requested"
UPDATE_STATE="not requested"
DASH_UNIT_STATE="not installed"
UNIT=""
WANTED_BY=""
TIMER=""

# Not set here: --dir rewrites $DIR below, and a TARGET computed before the flag
# loop silently points at the default dir, so --check reports the wrong install.
TARGET=""

usage() { sed -n '2,/^set -eu$/p' "$0" | sed '$d' | sed 's/^#\{0,1\} \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --dir) DIR="$2"; shift 2 ;;
    --service) SERVICE="$2"; shift 2 ;;
    --no-service) SERVICE=none; shift ;;
    --lite) DASHBOARD=0; shift ;;
    --with-dashboard) DASHBOARD=1; shift ;;
    --dashboard-src) DASH_SRC="$2"; shift 2 ;;
    --check) CHECK_ONLY=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --force-root) FORCE_ROOT=1; shift ;;
    --no-cta) SKIP_CTA=1; shift ;;
    --all-harnesses) ALL_HARNESS=1; shift ;;
    --no-harness) SKIP_HARNESS=1; shift ;;
    --verify-attestation) VERIFY_ATTESTATION=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown flag '$1' (see --help)" >&2; exit 2 ;;
  esac
done
TARGET="$DIR/aroute"
DASH_DEST="$HOME/.config/ar/dashboard"

if [ -n "$DASH_SRC" ] && [ "$DASHBOARD" -ne 1 ]; then
  echo "error: --dashboard-src needs --with-dashboard (see --help)" >&2; exit 2
fi
# Fail fast: a bad source is better raised before the 15 MB binary download,
# because the dashboard was the point of the invocation.
if [ "$DASHBOARD" -eq 1 ] && [ -n "$DASH_SRC" ]; then
  case "$DASH_SRC" in
    *.tar.gz|*.tgz) [ -f "$DASH_SRC" ] || { echo "error: no tarball at $DASH_SRC" >&2; exit 2; } ;;
    *) [ -f "$DASH_SRC/server.js" ] || { echo "error: $DASH_SRC is not a dashboard dist (want server.js inside)" >&2; exit 2; } ;;
  esac
fi

case "$SERVICE" in
  auto) if [ "$(id -u)" -eq 0 ]; then SERVICE=system; else SERVICE=user; fi ;;
  system|user|none) ;;
  *) echo "error: --service takes system|user|none (got '$SERVICE')" >&2; exit 2 ;;
esac

# Resolved here — after --service is parsed and `auto` is settled, before
# anything can read it. --uninstall and the summary both need it, and neither
# runs the service branch that used to assign it, so a variable assigned there
# was unbound on exactly those paths. (Five `set -u` aborts in this file traced
# back to this one habit: assigning a value only inside the branch that happens
# to be taken.)
if [ "$SERVICE" = system ]; then
  ENV_FILE="/etc/ar/ar.env"
else
  ENV_FILE="$HOME/.config/ar/ar.env"
fi
ENV_DIR="${ENV_FILE%/*}"
[ "$ENV_DIR" = "$ENV_FILE" ] && ENV_DIR="."

die() { echo "error: $*" >&2; exit 1; }

# ── Root guard ───────────────────────────────────────────────────────────────
if [ "$(id -u)" -eq 0 ] && [ "${FORCE_ROOT:-0}" != "1" ]; then
  die "Don't run as root — installs to \$HOME. Re-run as your user (override: --force-root)."
fi

# ── OS / distro detection ──────────────────────────────────────────────────────
case "$(uname -s)" in
  Linux) # shellcheck disable=SC1091  # os-release is standard on Linux; the fallback below covers its absence
    DISTRO="$(. /etc/os-release 2>/dev/null && echo "${ID:-unknown}")" ;;
  Darwin) DISTRO="macos" ;;
  *) die "no prebuilt binary for '$(uname -s)' (want Linux or macOS)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ;;
  *) die "no prebuilt binary for arch '$(uname -m)' (want x86_64)" ;;
esac

# ── Package-manager matrix ═══════════════════════════════════════════════════
pm_install() {
  case "$DISTRO" in
    ubuntu|debian|linuxmint|pop)      sudo apt-get update -qq && sudo apt-get install -y "$@" ;;
    fedora|rhel|centos|rocky|alma)     sudo dnf install -y "$@" ;;
    arch|manjaro|cachyos|endeavouros) sudo pacman -Sy --needed --noconfirm "$@" ;;
    opensuse*)                        sudo zypper -n install "$@" ;;
    alpine)                           sudo apk add "$@" ;;
    macos)
      command -v brew >/dev/null || die "Homebrew required: https://brew.sh (install it, re-run)"
      brew install "$@" ;;
    *) die "Unknown distro '$DISTRO' — install manually: $*" ;;
  esac
}
need() {
  command -v "$1" >/dev/null 2>&1 && return 0
  echo "error: installing prerequisite: $1" >&2
  # POSIX has no ${@:2} slicing: shift past the tool name and pass the rest.
  _tool="$1"
  shift
  pm_install "$@"
  command -v "$_tool" >/dev/null 2>&1 || die "$_tool still missing after install — install it, re-run"
}

# ── Idempotent PATH injection ──────────────────────────────────────────────────
ensure_path() {
  dir="$1"; mkdir -p "$dir"
  case ":$PATH:" in *":$dir:"*) return 0 ;; esac
  rc="$HOME/.bashrc"
  if [ -f "$HOME/.zshrc" ] || [ -n "${ZSH_VERSION:-}" ]; then rc="$HOME/.zshrc"; fi
  if ! grep -qs "export PATH=\"${dir}:\$PATH\"" "$rc"; then
    # The literal \$PATH is the point: the rc file must re-expand it at login.
    # shellcheck disable=SC2016
    printf '\n# added by aroute installer\nexport PATH="%s:$PATH"\n' "$dir" >> "$rc"
    printf '>>> generated: %s (PATH export) <<<\n' "$rc"
  fi
  export PATH="$dir:$PATH"
}

# ── CTA epilogue ───────────────────────────────────────────────────────────────
print_cta() {
  [ "${SKIP_CTA:-0}" = "1" ] && return 0
  printf '\n%s\n' "──────────────────────────────────────────────────────"
  printf ' ⭐  Found aroute useful? Star it:  https://github.com/ishan-parihar/artificial-route\n'
  printf ' 💜  Support development:\n'
  printf '     GitHub Sponsors : https://github.com/sponsors/ishan-parihar\n'
  printf '     Razorpay (UPI)  : https://rzp.io/rzp/ishan-parihar\n'
  # shellcheck disable=SC2016  # literal backticks in displayed copy, not a command substitution
  printf '     Or run `aroute donate` anytime.\n'
  printf '%s\n' "──────────────────────────────────────────────────────"
}

# ── Harness integration (MCP registration) ─────────────────────────────────────
detect_harnesses() {
  found=""
  for h in claude codex opencode gemini qwen grok goose hermes zeroclaw omp omo pi droid; do
    command -v "$h" >/dev/null 2>&1 && found="$found $h"
  done
  echo "$found"
}
run_harness_integration() {
  [ "${SKIP_HARNESS:-0}" = "1" ] && { echo "Harness integration: skipped (SKIP_HARNESS)"; return 0; }
  found="$(detect_harnesses)"
  # POSIX has no ${var//pat/rep}: test for any non-space via a case glob.
  case "$found" in *[!\ ]*) : ;; *) echo "No AI harnesses detected — skipping agent integration"; return 0 ;; esac
  if ! [ -t 0 ] && [ "${ALL_HARNESS:-0}" != "1" ]; then
    echo "Detected harnesses:${found} — non-interactive, skipping (re-run with --all-harnesses)"
    return 0
  fi
  if [ "${ALL_HARNESS:-0}" = "1" ]; then answer="y"
  else
    printf '%s' "Configure MCP server in harnesses?${found} [Y/n] "
    read -r answer || answer="y"
  fi
  case "$answer" in n|N) return 0 ;; esac
  # Claude Code
  if command -v claude >/dev/null 2>&1; then
    claude mcp add aroute -s user -- "$TARGET" mcp 2>/dev/null \
      && echo "  registered: claude mcp add aroute" || echo "  (claude mcp add skipped — register by hand)"
    claude mcp list 2>/dev/null | grep -q aroute && echo "  verified: claude mcp list" || true
  fi
  # OpenCode
  if command -v opencode >/dev/null 2>&1; then
    oc_conf="$HOME/.config/opencode/opencode.json"
    mkdir -p "$(dirname "$oc_conf")"
    echo "  OpenCode: add this to $oc_conf:"
    printf '    { "mcp": { "aroute": { "type": "local", "command": ["%s","mcp"], "enabled": true } } }\n' "$TARGET"
  fi
  # Codex
  if command -v codex >/dev/null 2>&1; then
    # `codex mcp add` is the supported writer on current builds; an older codex
    # lacks it, and then the TOML is printed rather than edited by hand here.
    if codex mcp add aroute -- "$TARGET" mcp >/dev/null 2>&1; then
      echo "  registered: codex mcp add aroute"
      codex mcp list 2>/dev/null | grep -q aroute && echo "  verified: codex mcp list" || true
    else
      echo "  Codex: add to ~/.codex/config.toml: [mcp_servers.aroute] command = \"$TARGET\" args = [\"mcp\"]"
    fi
  fi
  echo "Harness integration complete"
}

# ── Portable .mcp.json (printed, never written) ───────────────────────────────
# Printed rather than saved: the file used to be written beside this script, so
# a checkout (or ~/Downloads) picked up an untracked `.mcp.json` whose absolute
# command pointed at wherever the binary landed that run — a stale path the
# moment the install dir changed, and a broken server entry for every harness
# that reads project-root `.mcp.json`.
print_mcp_json() {
  echo "Portable .mcp.json (paste into a project root to use aroute's MCP tools there):"
  cat <<MCPEOF
{
  "mcpServers": {
    "artificial-route": {
      "command": "$TARGET",
      "args": ["mcp"]
    }
  }
}
MCPEOF
}

have_systemd() { command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; }

# Where the unit file lives, per scope. A system unit is the only one that
# starts without a login session, so it is the default for root.
unit_path() {
  if [ "$SERVICE" = system ]; then
    printf '%s' "/etc/systemd/system/$SERVICE_NAME.service"
  else
    printf '%s' "$HOME/.config/systemd/user/$SERVICE_NAME.service"
  fi
}
systemctl_cmd() {
  if [ "$SERVICE" = system ]; then systemctl "$@"; else systemctl --user "$@"; fi
}

latest_tag() {
  curl -fsSL -H 'Accept: application/vnd.github+json' "$API" 2>/dev/null \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -1
}

LATEST="$(latest_tag || true)"
[ -n "$LATEST" ] || LATEST=""
[ -n "$VERSION" ] || VERSION="$LATEST"

installed_tag() {
  [ -x "$TARGET" ] || return 1
  "$TARGET" --version 2>/dev/null | sed -n 's/^aroute //p'
}

# ---------------------------------------------------------------- uninstall --
if [ "$UNINSTALL" -eq 1 ]; then
  echo "removing $SERVICE_NAME ..."
  if [ "$SERVICE" != none ] && have_systemd; then
    systemctl_cmd disable --now "$SERVICE_NAME.service" >/dev/null 2>&1 || true
    rm -f "$(unit_path)" "$HOME/.config/systemd/user/$SERVICE_NAME.service" \
            /etc/systemd/system/$SERVICE_NAME.service 2>/dev/null || true
    systemctl_cmd daemon-reload >/dev/null 2>&1 || true
  fi
  rm -f "$TARGET" "$DIR/.aroute.new"
  for unit in "$HOME/.config/systemd/user/${SERVICE_NAME}-dashboard.service" \
              "/etc/systemd/user/${SERVICE_NAME}-dashboard.service"; do
    [ -f "$unit" ] || continue
    if [ "$unit" = "$HOME/.config/systemd/user/${SERVICE_NAME}-dashboard.service" ]; then
      systemctl --user disable --now "$SERVICE_NAME-dashboard.service" >/dev/null 2>&1 || true
    else
      systemctl disable --now "$SERVICE_NAME-dashboard.service" >/dev/null 2>&1 || true
    fi
    rm -f "$unit"
  done
  if [ -d "$DASH_DEST" ]; then
    rm -rf "$DASH_DEST" "$DASH_DEST.old"
    echo "removed dashboard dist."
    echo "kept:   $HOME/.config/ar/dashboard-data   (the dashboard's DB; delete by hand if unwanted)"
  fi
  # The config and the credentials stay: uninstalling a binary should not
  # destroy the operator's routing setup, which is the expensive part to
  # rebuild. Named explicitly, because an env file full of real keys outliving
  # the binary silently is the kind of thing nobody remembers later.
  echo "removed binary, service unit and update timer."
  echo "kept: $CONFIG"
  echo "kept: $ENV_FILE   (credentials; delete by hand if unwanted)"
  exit 0
fi

CURRENT="$(installed_tag || true)"
CURRENT_NUM="${CURRENT#v}"
LATEST_NUM="${LATEST#v}"

if [ "$CHECK_ONLY" -eq 1 ]; then
  [ -n "$LATEST" ] || LATEST="(unknown — releases API unreachable)"
  [ -n "$CURRENT" ] || CURRENT="(none)"
  printf 'installed: %s\nlatest:    %s\n' "$CURRENT" "$LATEST"
  if [ "$CURRENT" = "(none)" ]; then
    echo "status:    not installed"
  elif [ -z "${LATEST_NUM:-}" ]; then
    echo "status:    cannot tell (no network)"
  elif [ "$CURRENT_NUM" = "$LATEST_NUM" ]; then
    echo "status:    up to date"
  else
    echo "status:    update available (run this script again)"
  fi
  printf 'config:    %s%s\n' "$CONFIG" "$([ -f "$CONFIG" ] && echo '' || echo '  (missing)')"
  if [ -d "$DASH_DEST" ] && [ -f "$DASH_DEST/server.js" ]; then
    echo "dashboard: installed ($(du -sh "$DASH_DEST" 2>/dev/null | cut -f1)) — run: aroute dashboard"
  else
    echo "dashboard: not installed (--with-dashboard installs it from a local build)"
  fi
  # The dashboard's own unit is the on/off switch for its ~960 MB Node child;
  # report it by whichever scope exists.
  DASH_UNIT_CHECK=""
  for unit in "$HOME/.config/systemd/user/${SERVICE_NAME}-dashboard.service" \
              "/etc/systemd/user/${SERVICE_NAME}-dashboard.service"; do
    [ -f "$unit" ] && DASH_UNIT_CHECK="$unit" && break
  done
  if [ -n "$DASH_UNIT_CHECK" ]; then
    case "$DASH_UNIT_CHECK" in
      "$HOME"/*) dctl="systemctl --user" ;;
      *) dctl="systemctl" ;;
    esac
    echo "dashsvc:   $($dctl is-active "$SERVICE_NAME-dashboard.service" 2>/dev/null || echo inactive) — $dctl start|stop $SERVICE_NAME-dashboard"
  fi
  if [ "$SERVICE" = none ]; then
    echo "service:   not managed"
  elif have_systemd; then
    state="$(systemctl_cmd is-active "$SERVICE_NAME.service" 2>/dev/null || true)"
    enab="$(systemctl_cmd is-enabled "$SERVICE_NAME.service" 2>/dev/null || true)"
    echo "service:   $SERVICE scope, active=$state enabled=$enab"
  else
    echo "service:   no systemd on this host"
  fi
  exit 0
fi

# ------------------------------------------------------------- dashboard --
# The web dashboard is a compiled Next.js standalone — ~2 GiB with its traced
# node_modules — far too large to ship as a release asset, so it is built on
# this machine (dashboard/rebrand-dist.sh) and installed from that local tree.
# Nothing is fetched from npm: the dist carries every dependency it needs.
install_dashboard() {
  need node nodejs
  NODE_MAJOR="$(node --version 2>/dev/null | sed 's/^v\([0-9][0-9]*\).*/\1/')"
  case "$NODE_MAJOR" in
    ''|*[!0-9]*) die "cannot read the node version" ;;
  esac
  [ "$NODE_MAJOR" -ge 18 ] || die "--with-dashboard needs Node.js >= 18 (found $(node --version))"

  if [ -z "$DASH_SRC" ]; then
    # The common case: this script run from a checkout of the repo, where
    # <repo>/dashboard/dist is the freshly built tree.
    guess="$(dirname "$SCRIPT_SRC")/dashboard/dist"
    if [ -z "$SCRIPT_SRC" ] || [ ! -f "$guess/server.js" ]; then
      die "pass --dashboard-src <dist-dir-or-tarball> — build it with dashboard/rebrand-dist.sh"
    fi
    DASH_SRC="$guess"
  fi
  case "$DASH_SRC" in
    *.tar.gz|*.tgz) [ -f "$DASH_SRC" ] || die "no tarball at $DASH_SRC" ;;
    *) [ -f "$DASH_SRC/server.js" ] || die "$DASH_SRC is not a dashboard dist (want server.js inside)" ;;
  esac

  mkdir -p "$(dirname "$DASH_DEST")"
  rm -rf "${DASH_DEST}.new"
  mkdir "${DASH_DEST}.new"
  echo "installing dashboard from $DASH_SRC ..."
  case "$DASH_SRC" in
    *.tar.gz|*.tgz) tar -xzf "$DASH_SRC" -C "${DASH_DEST}.new" ;;
    *) cp -a "$DASH_SRC/." "${DASH_DEST}.new/" ;;
  esac
  [ -f "${DASH_DEST}.new/server.js" ] || die "the unpacked dist has no server.js — bad source?"

  # ── the node_modules the 277 MB tarball deliberately omits ──────────────────
  # The release asset ships without node_modules (277 MB instead of ~330 MB,
  # docs/41 §4), so the host resolves the traced tree once, here, where npm
  # can verify it. A tarball WITH node_modules (or a directory source, which
  # was copied wholesale above) skips this: `npm ci` over an already-resolved
  # tree is a no-op that still costs minutes on a 2-vCPU box.
  if [ ! -d "${DASH_DEST}.new/node_modules" ] && [ -f "${DASH_DEST}.new/package.json" ]; then
    echo "resolving the dashboard's node_modules (npm ci --omit=dev --legacy-peer-deps) ..."
    # --legacy-peer-deps: upstream's own .npmrc sets it for their tree (a real
    # marked@18 vs marked-terminal@<16 peer conflict); a plain npm ci hard-fails.
    # The dist's .npmrc (copied by the rebrand pipeline) also carries
    # onnxruntime-node-install=skip — without it the postinstall pulls
    # ~300 MB of CUDA binaries on linux/x64.
    ( cd "${DASH_DEST}.new" && npm ci --omit=dev --legacy-peer-deps --no-audit --no-fund ) \
      || die "npm ci failed inside the unpacked dist — the release tarball and its package-lock are out of sync"
  fi

  # ── the one-way base-path patch, once, on a root-built dist ──────────────
  # A dist baked for /dash at BUILD time already carries it in the manifests
  # and its BUILD_OMNIROUTE_BASE_PATH sentinel says so; the patcher refuses
  # those (a mismatch is a hard error by design). A ROOT-built dist (the
  # local-operator shape, and any pre-rebuild artifact) converts here, once —
  # the only place the file exists to do it.
  if [ -f "${DASH_DEST}.new/patch-base-path.mjs" ] && [ ! -f "${DASH_DEST}.new/BUILD_OMNIROUTE_BASE_PATH" ]; then
    echo "patching the dist's base path to ${DASH_BASE_PATH:-/dash} (one-way, once) ..."
    ( cd "${DASH_DEST}.new" && OMNIROUTE_BASE_PATH="${DASH_BASE_PATH:-/dash}" \
        node patch-base-path.mjs ) \
      || die "the base-path patch failed — the dist stays root-built; re-run with DASH_BASE_PATH empty to serve at /"
  fi

  rm -rf "${DASH_DEST}.old"
  [ -d "$DASH_DEST" ] && mv "$DASH_DEST" "${DASH_DEST}.old"
  mv "${DASH_DEST}.new" "$DASH_DEST"
  rm -rf "${DASH_DEST}.old"
  DASH_STATE="installed -> $DASH_DEST"
  echo "dashboard: installed $(du -sh "$DASH_DEST" 2>/dev/null | cut -f1) -> $DASH_DEST"
}

# ----------------------------------------------------- dashboard unit ----
# On/off for the dashboard, as a service rather than a nohup: `systemctl
# --user start aroute-dashboard` brings the ~960 MB Next.js child up, `stop`
# reclaims it, and the default after install is *stopped* — the proxy has
# never needed it, and the operator's box (OOM history) wants the choice.
# Same discipline as the proxy unit: env file outside the unit, secrets never
# in the process table.
#
# THE ENV FILE IS THE DASHBOARD'S OWN, NOT ar.env (docs/41 §3). ar.env carries
# the proxy's AR master keys — a credential the dashboard must never hold —
# so the dashboard unit points at dashboard.env, which carries only the
# dashboard's own launch posture. The supervisor (`aroute dashboard`) reads
# the same names, so the interactive verb and the unit cannot drift.
install_dashboard_unit() {
  DASH_UNIT=""
  DASH_UNIT_STATE="not managed"
  DASH_CTL="systemctl --user"
  [ -f "$DASH_DEST/server.js" ] || return 0
  have_systemd || return 0
  if [ "$SERVICE" = system ]; then
    DASH_UNIT="/etc/systemd/user/${SERVICE_NAME}-dashboard.service"
    DASH_CTL="systemctl"
  else
    DASH_UNIT="$HOME/.config/systemd/user/${SERVICE_NAME}-dashboard.service"
  fi
  DASH_UNIT_DIR="${DASH_UNIT%/*}"
  [ "$DASH_UNIT_DIR" = "$DASH_UNIT" ] && DASH_UNIT_DIR="."
  mkdir -p "$DASH_UNIT_DIR"

  # The dashboard's env file. Written when absent, kept when present — the
  # operator's secrets and the pinned peer-stamp token must survive an
  # upgrade. First write generates the secrets the dashboard demands (a weak
  # JWT_SECRET is on its own block list and login 500s; INITIAL_PASSWORD
  # CHANGEME is refused at login) plus the pinned stamp token, and prints
  # the password ONCE, here, because it is a secret nobody can recover later.
  DASH_ENV_FILE="$HOME/.config/ar/dashboard.env"
  if [ ! -f "$DASH_ENV_FILE" ]; then
    gen_secret() {
      # 32 hex chars from /dev/urandom: no openssl dependency, POSIX-portable.
      od -An -tx1 -N32 /dev/urandom 2>/dev/null | tr -d ' \n' \
        || head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'
    }
    DASH_JWT="$(gen_secret)"
    DASH_API_KEY_SECRET="$(gen_secret)"
    DASH_INITIAL_PASSWORD="$(gen_secret)"
    DASH_STAMP_TOKEN="$(gen_secret)"
    cat > "$DASH_ENV_FILE" <<DASHENVEOF
# The dashboard's launch environment. chmod 600 this file; it holds secrets.
# This file is the DASHBOARD's own — the proxy's ar.env is deliberately not
# inherited here (it carries the proxy's credential-store master key).
#
# The supervisor (aroute dashboard) already defaults REQUIRE_API_KEY=true,
# AUTH_COOKIE_SECURE=true and OMNIROUTE_MEMORY_MB=1024; they are written out
# explicitly so this file is the single source of truth an operator can read.

# The gateway tier: refuse anonymous completions. Upstream ships "false".
REQUIRE_API_KEY=true

# The login cookie's secure flag, by configuration rather than inference.
AUTH_COOKIE_SECURE=true

# The child's heap ceiling in MiB, measured for a 2.4 GiB box with the proxy
# resident (docs/41 §3). MemoryMax below is the hard kill at 1.5x.
OMNIROUTE_MEMORY_MB=1024

# The pinned peer-stamp token: without it every restart mints a new one and
# the first-password window reopens. Generated once, kept forever.
OMNIROUTE_PEER_STAMP_TOKEN=$DASH_STAMP_TOKEN

# The public origin, when the dashboard is published at a path (docs/41 §1).
#OMNIROUTE_PUBLIC_BASE_URL=https://aroute.example.com/dash
#OMNIROUTE_BASE_PATH=/dash

# Secrets generated at install. JWT_SECRET signs the login cookie (login
# 500s without it); API_KEY_SECRET validates the dashboard's own sk- keys;
# INITIAL_PASSWORD is the FIRST login's password — change it after the first
# login, and this file is the only record of the generated value.
JWT_SECRET=$DASH_JWT
API_KEY_SECRET=$DASH_API_KEY_SECRET
INITIAL_PASSWORD=$DASH_INITIAL_PASSWORD
DASHENVEOF
    chmod 600 "$DASH_ENV_FILE"
    echo "dashenv:   wrote $DASH_ENV_FILE (secrets generated)"
    echo "           FIRST LOGIN PASSWORD: $DASH_INITIAL_PASSWORD"
    echo "           (shown once; change it from inside the dashboard)"
  else
    echo "dashenv:   kept existing $DASH_ENV_FILE"
  fi

  cat > "$DASH_UNIT" <<DASHEOF
[Unit]
Description=artificial-route (ar) — web dashboard
Documentation=https://github.com/$REPO
After=network-online.target

[Service]
Type=simple
# Same dist-resolution chain as any other aroute dashboard run: \$AR_DASHBOARD_DIR,
# a dist beside the binary, then the install location. No --path needed.
ExecStart=$TARGET dashboard --port $DASH_PORT
# The DASHBOARD's env file — never ar.env, which carries the proxy's master.
EnvironmentFile=$DASH_ENV_FILE
# Same as the proxy unit: HOME decides where \$HOME/.config/ar/dashboard-data
# lives, and it stays out of the process table.
Environment=HOME=$HOME
Restart=on-failure
RestartSec=5s
# The dashboard shares the box with the proxy; the memory guard is what
# keeps a misbehaving Node child from OOM-killing the proxy (docs/41 §3:
# MemoryHigh starts reclaiming at 1280M, MemoryMax is the hard kill at 1536M,
# 1.5x the heap ceiling the env pins).
MemoryHigh=1280M
MemoryMax=1536M
# The Node child peaks at ~959 MB RSS while the UI is served (measured
# 2026-10-24, docs/36 §9.2.1); stopping this unit is how that is given back.
# An earlier comment here said 700 MB, which understated it by ~2x — an
# operator sizing MemoryHigh from that number would have set it below the real
# peak. MemoryHigh=1280M sits above the measured peak, so no guard value
# changes; only the comment's number does.
KillSignal=SIGTERM
TimeoutStopSec=20s
StandardOutput=journal
StandardError=journal

[Install]
# Not enabled at install time — the default is "off" so the ~960 MB Node
# child costs nothing until asked for. "systemctl --user enable" flips that
# per-machine; stop/start reclaims it on demand either way.
WantedBy=default.target
DASHEOF
  systemctl_cmd daemon-reload
  DASH_UNIT_STATE="installed, not started"
  echo "dashsvc:   wrote $DASH_UNIT (not started)"
  echo "          start:  $DASH_CTL start $SERVICE_NAME-dashboard"
  echo "          stop:   $DASH_CTL stop $SERVICE_NAME-dashboard   (reclaims ~960 MB RSS)"
  echo "          memory: MemoryHigh=1280M MemoryMax=1536M (the proxy is protected)"
}

# ------------------------------------------------------------------ install --
# A system-wide --dir needs root, and saying so plainly beats a permission
# error from `mkdir` three lines later.
if [ "$(id -u)" -ne 0 ]; then
  case "$DIR" in
    /usr/*|/opt/*|/etc/*)
      die "installing to $DIR needs root — re-run under sudo, or pass --dir ~/.local/bin" ;;
  esac
fi

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT INT TERM

if [ -n "$VERSION" ]; then
  BASE="https://github.com/$REPO/releases/download/$VERSION"
else
  die "cannot reach $API, so there is no newest release to install.
      Pass --version <tag> to pin one, e.g. --version v0.1.2"
fi

cd "$TMP"
need curl curl
echo "downloading aroute $VERSION ..."
# The release workflow renames the binary from `ar` to `aroute`, but the latest
# published release may still ship the old asset name. Try the new name first,
# then the old one, so the smoke test can install whatever the latest release
# actually contains.
ASSET_FILE=""
for CANDIDATE in aroute-x86_64-unknown-linux-musl ar-x86_64-unknown-linux-musl; do
  if curl -fsSL -o "$CANDIDATE" "$BASE/$CANDIDATE"; then
    ASSET_FILE="$CANDIDATE"
    break
  fi
done
[ -n "$ASSET_FILE" ] || die "no release asset found at $BASE"
ASSET="$ASSET_FILE"
curl -fsSL -o SHA256SUMS "$BASE/SHA256SUMS"

# Verified before the binary is ever executable, so a corrupt or substituted
# download cannot run. Missing tooling is a refusal, not a skip: an install that
# quietly gives up verification is the one that should not. There is no silent
# package install here: on Linux sha256sum is coreutils, and on macOS shasum
# ships with the OS, so a missing tool means the system is unusual enough to
# be fixed by the operator, not by a sudo run from a piped install.
if command -v sha256sum >/dev/null 2>&1; then
  grep " $ASSET\$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1 \
    || die "checksum mismatch for $ASSET — report at github.com/${REPO}/issues"
elif command -v shasum >/dev/null 2>&1; then
  EXPECTED="$(grep " $ASSET\$" SHA256SUMS | cut -d' ' -f1)"
  ACTUAL="$(shasum -a 256 "$ASSET" | cut -d' ' -f1)"
  [ -n "$EXPECTED" ] && [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $ASSET — report at github.com/${REPO}/issues"
else
  # Refused, not warned: an install that runs an unverified binary because a
  # tool is missing is the case verification exists for.
  die "no sha256sum or shasum found; install coreutils and re-run, the binary was not installed"
fi
echo "checksum verified"

# A checksum only proves the bytes match the release; it does not prove they
# will run on this host. The published asset is static (musl) precisely so it
# runs on any glibc — a dynamically-linked build fails on an older host with
# `GLIBC_2.39 not found` and takes the service into a crash-loop, which is
# worse than a refusal. Caught before the binary is ever installed.
if command -v file >/dev/null 2>&1; then
  if file "$ASSET" | grep -qiE 'dynamically linked'; then
    die "$ASSET is dynamically linked; this host needs the static build — report at github.com/${REPO}/issues"
  fi
  echo "static link verified"
fi

# Opt-in provenance check. A checksum only proves the file matches SHA256SUMS,
# and that file is published beside it; the attestation is signed by the release
# workflow's identity, so it also proves which workflow built the bytes.
if [ "$VERIFY_ATTESTATION" = 1 ]; then
  command -v gh >/dev/null 2>&1 || die "--verify-attestation needs the GitHub CLI (gh); install it, or re-run without the flag"
  gh attestation verify "$ASSET" --repo "$REPO" >/dev/null 2>&1 \
    || die "build attestation check failed for $ASSET (gh attestation verify --repo $REPO); the binary was not installed"
  echo "build attestation verified"
fi

chmod +x "$ASSET"
mkdir -p "$DIR"
# A unit running the old binary keeps it busy: stop it for the swap, and bring
# it back on the new one if it was running. The proxy unit is restarted by its
# own branch below.
DASH_WAS_ACTIVE=0
if command -v systemctl >/dev/null 2>&1; then
  if systemctl --user is-active --quiet "$SERVICE_NAME-dashboard.service" 2>/dev/null; then
    DASH_WAS_ACTIVE=1
    systemctl --user stop "$SERVICE_NAME-dashboard.service" >/dev/null 2>&1 || DASH_WAS_ACTIVE=0
  fi
fi
# Written beside the target and renamed in: an `ar` starting during the swap sees
# the old binary or the new one, never a truncated file that cannot exec.
cp "$ASSET" "$DIR/.aroute.new"
chmod +x "$DIR/.aroute.new"
# The binary must run on THIS host before it replaces a working one. A
# dynamically-linked build against an older glibc exits here with a loader
# error instead of silently replacing a good binary and crash-looping the
# unit; the previous install stays in place because the swap never happened.
if ! "$DIR/.aroute.new" --version >/dev/null 2>&1; then
  rm -f "$DIR/.aroute.new"
  die "the downloaded binary does not run on this host (loader mismatch?) — the previous install was left untouched"
fi
mv "$DIR/.aroute.new" "$TARGET"
INSTALLED="$("$TARGET" --version)"
if [ "$DASH_WAS_ACTIVE" -eq 1 ]; then
  systemctl --user start "$SERVICE_NAME-dashboard.service" >/dev/null 2>&1 \
    || echo "warning: dashboard unit did not restart (start it by hand)" >&2
  echo "dashsvc:   restarted on $INSTALLED"
fi

if [ -n "$CURRENT" ] && [ "$CURRENT_NUM" != "${VERSION#v}" ]; then
  echo "updated $CURRENT -> $INSTALLED"
else
  echo "installed $INSTALLED"
fi

# --------------------------------------------------------------- config ------
# Without this the binary is inert: `aroute doctor` and `aroute serve` both refuse to
# start when no config.yaml is reachable, so "installed" would mean "present but
# unable to run". Never overwritten — an existing config is the operator's work.
# Parent directory via parameter expansion rather than `dirname`: one fewer
# subprocess, and no dependency on a coreutils name being present.
CONFIG_DIR="${CONFIG%/*}"
[ "$CONFIG_DIR" = "$CONFIG" ] && CONFIG_DIR="."
mkdir -p "$CONFIG_DIR"
if [ -f "$CONFIG" ]; then
  echo "config:    kept existing $CONFIG"
else
  cat > "$CONFIG" <<'YAML'
# Written by install.sh. Keys stay in the environment; no secret lives here.
#
# Expansion covers this whole file, comments included, so a dollar-sign followed
# by a name anywhere is read as an environment reference even in prose — which
# is why this comment spells it out instead of writing it as a symbol.
# `ar` refuses to start if a referenced variable is unset. Put real values in
# the env file the service unit reads, or export them before running `ar`.
server:
  host: 127.0.0.1
  port: 20128

keys:
  openai: $OPENAI_API_KEY
  anthropic: $ANTHROPIC_API_KEY
  nvidia: $NVIDIA_API_KEY

providers:
  - id: openai
    key: openai
  - id: anthropic
    key: anthropic
  - id: nvidia
    key: nvidia

# A combo is a routable model name: providers alone are never addressable, so an
# install without one leaves `aroute doctor` reporting "no model is routable" and
# `aroute serve` with nothing to dispatch. Targets are `<provider>/<model>`.
#
# The standalone single-target combos after `default` are the models this
# project operates on its own gateway; exposing each as its own combo is what
# puts a bare id in `/v1/models` and keeps harness model pickers working when a
# stack combo changes shape. `glm-5.3` rides nvidia's free NIM lane.
combos:
  - id: default
    strategy: lkgp
    targets:
      - openai/gpt-5.4
      - anthropic/claude-sonnet-5
  - id: glm-5.3
    strategy: priority
    targets:
      - nvidia/z-ai/glm-5.3
YAML
  echo "config:    wrote $CONFIG"
fi

# ----------------------------------------------------------------- service ---
# An env file the unit sources, so credentials can be added after install
# without touching the unit or the config. ENV_FILE and ENV_DIR are resolved up
# with the other early defaults, because --uninstall and the summary both read
# them before this branch would have run.
# A saved copy of this installer, so the update timer below runs the audited
# artifact rather than re-fetching a URL that could change under it.
SCRIPT_COPY="$HOME/.config/ar/install.sh"
mkdir -p "$ENV_DIR"
if [ ! -f "${ENV_FILE:-$HOME/.config/ar/ar.env}" ]; then
  cat > "$ENV_FILE" <<ENVEOF
# Credentials for ar. chmod 600 this file before filling it in.
#
# These are COMMENTED OUT on purpose, and the distinction matters: a line
# reading KEY= sets the variable to the empty string, ar expands it happily,
# doctor reports every check green and the unit sits active — while every
# upstream request 401s. Left unset, ar refuses to start and says why.
# Uncomment and fill one line per key the config references.
#OPENAI_API_KEY=sk-...
#ANTHROPIC_API_KEY=sk-ant-...
#NVIDIA_API_KEY=nvapi-...
ENVEOF
  chmod 600 "$ENV_FILE"
  echo "env file:  wrote $ENV_FILE"
else
  echo "env file:  kept existing $ENV_FILE"
fi

if [ "$SERVICE" != none ]; then
  if have_systemd; then
    UNIT="$(unit_path)"
    UNIT_DIR="${UNIT%/*}"
    [ "$UNIT_DIR" = "$UNIT" ] && UNIT_DIR="."
    # A user unit wants default.target; a system unit wants multi-user. Chosen
    # here rather than fixed afterwards with `sed -i`: in-place editing is
    # GNU-only, it forks for something a variable already decides, and the
    # draft-then-rewrite shape hid which target was actually wanted.
    if [ "$SERVICE" = system ]; then WANTED_BY="multi-user.target"; else WANTED_BY="default.target"; fi
    mkdir -p "$UNIT_DIR"
    cat > "$UNIT" <<UNITEOF
[Unit]
Description=artificial-route (ar) — OpenAI-compatible LLM proxy
Documentation=https://github.com/$REPO
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=$TARGET --config $CONFIG serve
EnvironmentFile=$ENV_FILE
# Secrets are in the environment; keep them out of the process table.
Environment=HOME=$HOME
Restart=on-failure
RestartSec=5s
# The proxy holds no state that must survive a restart, and its caches are
# memory-bounded, so there is nothing to gain from a longer linger.
KillSignal=SIGTERM
TimeoutStopSec=20s
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=$WANTED_BY
UNITEOF

    systemctl_cmd daemon-reload
    systemctl_cmd enable "$SERVICE_NAME.service" >/dev/null 2>&1 \
      || echo "warning: could not enable $SERVICE_NAME (continuing)" >&2
    if command -v loginctl >/dev/null 2>&1 && [ "$SERVICE" = user ]; then
      # Without lingering a user unit dies at logout, which makes "starts on
      # boot" false for every session that logs out first.
      loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || true
    fi
    # A missing key makes `aroute serve` exit immediately, so a failed start here
    # usually means empty credentials rather than a broken unit. Reported, not
    # hidden, and not treated as a failed install.
    if systemctl_cmd restart "$SERVICE_NAME.service" >/dev/null 2>&1; then
      sleep 1
      if systemctl_cmd is-active --quiet "$SERVICE_NAME.service"; then
        SERVICE_STATE="active ($SERVICE unit, enabled)"
      else
        SERVICE_STATE="failed to stay up — most likely empty credentials in $ENV_FILE"
      fi
    else
      SERVICE_STATE="installed but not started — most likely empty credentials in $ENV_FILE"
    fi
    echo "service:   wrote $UNIT"
  else
    SERVICE_STATE="skipped — no systemd on this host"
  fi

  # ---------------------------------------------------------------- updater --
  # "Automatically update" is only true if something runs the installer, so a
  # timer does. It re-runs a saved copy of this very script: same checksum
  # verification, same tag resolution, no second download path to keep in sync.
  # Installing a new binary restarts the service, so the running proxy changes
  # too rather than serving the old build until someone notices.
  TIMER="${UNIT%.service}.update.timer"
  TIMER_SVC="${TIMER%.timer}.service"
  # Only writable when this script is a real file. `curl | sh` has no $0 on disk,
  # and silently installing a timer that cannot run is worse than not having one.
  if [ -n "$SCRIPT_SRC" ]; then
    [ -f "$SCRIPT_COPY" ] || { cp "$SCRIPT_SRC" "$SCRIPT_COPY"; chmod +x "$SCRIPT_COPY"; }
    cat > "$TIMER_SVC" <<TIMERSVC
[Unit]
Description=Check for an artificial-route release and install it
Documentation=https://github.com/$REPO

[Service]
Type=oneshot
# The saved installer, not a URL: this is the audited script, and it verifies
# the release checksum before it replaces the binary.
ExecStart=$SCRIPT_COPY --dir $DIR --service none
TIMERSVC
    cat > "$TIMER" <<TIMEREOF
[Unit]
Description=Weekly artificial-route update check

[Timer]
# A fresh install gets its first re-check shortly after boot rather than waiting
# out the full interval. Randomized delay so many machines do not all poll the
# release API in the same minute.
OnBootSec=15min
OnUnitActiveSec=7d
RandomizedDelaySec=1h
Persistent=true

[Install]
WantedBy=$WANTED_BY
TIMEREOF
    systemctl_cmd daemon-reload >/dev/null 2>&1 || true
    systemctl_cmd enable --now "$(basename "$TIMER")" >/dev/null 2>&1 \
      || echo "warning: could not enable the update timer (continuing)" >&2
    UPDATE_STATE="enabled ($(basename "$TIMER"), weekly)"
  else
    UPDATE_STATE="skipped (run from a file, not a pipe, to enable unattended updates)"
  fi
fi

# The dashboard installs independently of the service scope, so --service none
# still gets it, and the weekly update timer (which never passes a dashboard
# flag) leaves an installed dist alone rather than deleting it.
if [ "$DASHBOARD" -eq 1 ]; then
  install_dashboard
  install_dashboard_unit
else
  if [ -d "$DASH_DEST" ] && [ -f "$DASH_DEST/server.js" ]; then
    DASH_STATE="kept — $DASH_DEST (upgrades: --with-dashboard --dashboard-src <dist>)"
  else
    DASH_STATE="not installed (add it: --with-dashboard --dashboard-src <dist>)"
  fi
fi

# ---------------------------------------------------------------- verify -----
# Prove the install rather than asserting it. `doctor` exits non-zero when the
# credentials are absent, which is expected on a first run, so this reports and
# never gates the install.
DOCTOR_OUT="$("$TARGET" --config "$CONFIG" doctor 2>&1 || true)"
if printf '%s' "$DOCTOR_OUT" | grep -q '^count:'; then
  echo "verify:    doctor -> $(printf '%s' "$DOCTOR_OUT" | sed -n '1p')"
elif printf '%s' "$DOCTOR_OUT" | grep -q 'cannot expand'; then
  # The overwhelmingly common first-run case, and it reads like a crash if left
  # as a raw error line. It is not: ar refuses to start when a credential the
  # config references is unset, which is the designed behaviour.
  MISSING="$(printf '%s' "$DOCTOR_OUT" | sed -n "s/.*looking key '\\([A-Z0-9_]*\\)'.*/\\1/p" | head -1)"
  echo "verify:    doctor -> not run (no credentials in this shell; set $MISSING)"
  echo "            this is expected on a fresh install — the service reads them"
  echo "            from $ENV_FILE instead. Put real keys there."
else
  echo "verify:    doctor -> $(printf '%s' "$DOCTOR_OUT" | head -1)"
fi

# The control page rides the proxy's own listener, so its address is the
# configured one. Read from the file rather than assumed: the sample config
# binds 127.0.0.1:20128 but an operator may have changed either.
UI_HOST="$(sed -n 's/^[[:space:]]*host:[[:space:]]*\([^ ,}]*\).*/\1/p' "$CONFIG" | head -1)"
UI_PORT="$(sed -n 's/.*port:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$CONFIG" | head -1)"
UI_URL="http://${UI_HOST:-127.0.0.1}:${UI_PORT:-20128}/ui/"

cat <<EOF

installed to $TARGET

  config:    $CONFIG
  env file:  $ENV_FILE   (chmod 600; put keys here)
  service:   $SERVICE_STATE
  updater:   $UPDATE_STATE
  dashboard: $DASH_STATE
  dashsvc:   $DASH_UNIT_STATE
  /ui:       $UI_URL   (combos, keys, limits, usage log, playground; edits config.yaml)

  $TARGET doctor      # re-check at any time
  $TARGET serve       # foreground; the unit already runs it as a service

Upgrade:  curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | bash
  Check:    curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | bash -s -- --check
  Remove:   curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | bash -s -- --uninstall
  Dashboard: bash install.sh --with-dashboard --dashboard-src <dashboard/dist-or-tarball>
EOF

# ── PATH injection ─────────────────────────────────────────────────────────────
ensure_path "$DIR"

# ── Print portable .mcp.json + offer harness registration ────────────────────
print_mcp_json
run_harness_integration

# ── CTA ────────────────────────────────────────────────────────────────────────
print_cta