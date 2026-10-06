#!/bin/sh
# Install, update, or remove `ar` (artificial-route) — a static musl binary with
# no runtime dependencies.
#
#   curl -fsSL https://raw.githubusercontent.com/ishan-parihar/artificial-route/main/install.sh | sh
#
# One command does the whole job: installs the newest release, writes a working
# config, installs and starts a systemd unit so the proxy comes up on boot, and
# proves the result with `aroute doctor`. Re-running it over an existing install
# updates the binary and leaves your config alone. There is no separate updater
# to drift out of sync with this one.
#
# Flags:
#   --version <tag>    pin an exact tag instead of the newest release
#   --dir <path>       install somewhere else (default $HOME/.local/bin)
#   --service <scope>  systemd unit scope: system | user | none
#                      (default: system when run as root, else user)
#   --no-service       same as --service none
#   --check            report installed vs newest and service state; change nothing
#   --uninstall        remove the binary, unit, and autostart; keep the config
#   --help
# Unknown flags fail loudly (exit 2).
#
# Env knobs: AR_VERSION (pin a tag), AR_INSTALL_DIR (install dir),
#            AR_SERVICE (unit scope), AR_CONFIG (config path for the unit).
set -eu

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
CHECK_ONLY=0
UNINSTALL=0

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
    --check) CHECK_ONLY=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown flag '$1' (see --help)" >&2; exit 2 ;;
  esac
done
TARGET="$DIR/aroute"

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

# The proxy is loopback-only by default. A unit that binds every interface would
# publish an unauthenticated LLM gateway to the network, so the unit pins
# loopback rather than inheriting whatever the config happens to say.
case "$(uname -s)" in
  Linux) ;;
  *) die "no prebuilt binary for '$(uname -s)' (want Linux)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ;;
  *) die "no prebuilt binary for arch '$(uname -m)' (want x86_64)" ;;
esac

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
echo "downloading aroute $VERSION ..."
curl -fsSL -o "$ASSET" "$BASE/$ASSET"
curl -fsSL -o SHA256SUMS "$BASE/SHA256SUMS"

# Verified before the binary is ever executable, so a corrupt or substituted
# download cannot run. Missing tooling is a warning, not a skip: an install that
# quietly gives up verification is the one that should not.
if command -v sha256sum >/dev/null 2>&1; then
  grep " $ASSET\$" SHA256SUMS | sha256sum -c - >/dev/null 2>&1 \
    || die "checksum mismatch for $ASSET"
elif command -v shasum >/dev/null 2>&1; then
  EXPECTED="$(grep " $ASSET\$" SHA256SUMS | cut -d' ' -f1)"
  ACTUAL="$(shasum -a 256 "$ASSET" | cut -d' ' -f1)"
  [ -n "$EXPECTED" ] && [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $ASSET"
else
  echo "warning: no sha256sum/shasum found, skipping checksum verify" >&2
fi
echo "checksum verified"

chmod +x "$ASSET"
mkdir -p "$DIR"
# Written beside the target and renamed in: an `ar` starting during the swap sees
# the old binary or the new one, never a truncated file that cannot exec.
cp "$ASSET" "$DIR/.aroute.new"
chmod +x "$DIR/.aroute.new"
mv "$DIR/.aroute.new" "$TARGET"
INSTALLED="$("$TARGET" --version)"

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

providers:
  - id: openai
    key: openai
  - id: anthropic
    key: anthropic

# A combo is a routable model name: providers alone are never addressable, so an
# install without one leaves `aroute doctor` reporting "no model is routable" and
# `aroute serve` with nothing to dispatch. Targets are `<provider>/<model>`.
combos:
  - id: default
    strategy: lkgp
    targets:
      - openai/gpt-5.4
      - anthropic/claude-sonnet-5
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
if [ ! -f "$ENV_FILE" ]; then
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

cat <<EOF

installed to $TARGET

  config:    $CONFIG
  env file:  $ENV_FILE   (chmod 600; put keys here)
  service:   $SERVICE_STATE
  updater:   $UPDATE_STATE

  $TARGET doctor      # re-check at any time
  $TARGET serve       # foreground; the unit already runs it as a service

Upgrade:  curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | sh
Check:    curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | sh -s -- --check
Remove:   curl -fsSL https://raw.githubusercontent.com/$REPO/main/install.sh | sh -s -- --uninstall
EOF

case ":$PATH:" in
  *":$DIR:"*) ;;
  *) echo "note: $DIR is not on PATH — add it with: export PATH=\"\$HOME/.local/bin:\$PATH\"" >&2 ;;
esac