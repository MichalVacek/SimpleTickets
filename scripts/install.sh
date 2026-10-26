#!/usr/bin/env bash
# Installs SimpleTickets on macOS: binaries into a prefix, optional addons, and a
# launchd job that keeps the daemon running.
set -euo pipefail

REPO="MichalVacek/SimpleTickets"
LABEL="com.simpletickets.daemon"

# Addons: name|crate|binary|description. Add a line to offer another addon; the
# crate is built from source or its binary is taken from the release archive.
ADDONS=(
  "mcp|simpletickets-mcp|simpletickets-mcp|MCP server so a client can list, create and update tickets"
)

usage() {
  cat <<'USAGE'
Usage: scripts/install.sh [options]

  --prefix DIR           install binaries into DIR/bin (default: ~/.local)
  --tickets-dir DIR      tickets folder the daemon manages (default: ~/SimpleTickets)
  --port N               daemon port on 127.0.0.1 (default: 7878)
  --addons LIST          comma-separated addons to install, or "none" (default: ask)
  --from-source          build with cargo from this checkout
  --from-release         download the latest release archive
                         (default: from source when run inside a checkout with cargo)
  --launchagents-dir DIR where to write the plist (default: ~/Library/LaunchAgents)
  --no-service           do not write a launchd plist
  --no-load              write the plist but do not load it
  --yes                  accept defaults without asking
  --dry-run              print what would happen, change nothing
  -h, --help             show this help

Available addons:
USAGE
  local entry
  for entry in "${ADDONS[@]}"; do
    IFS='|' read -r name _ _ description <<<"$entry"
    printf '  %-8s %s\n' "$name" "$description"
  done
}

die() {
  echo "install.sh: $*" >&2
  exit 1
}

PREFIX="$HOME/.local"
TICKETS_DIR="$HOME/SimpleTickets"
PORT=7878
ADDON_LIST=""
MODE="auto"
AGENTS_DIR="$HOME/Library/LaunchAgents"
SERVICE=1
LOAD=1
ASSUME_YES=0
DRY_RUN=0

while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
    --tickets-dir) TICKETS_DIR="${2:?--tickets-dir needs a value}"; shift 2 ;;
    --port) PORT="${2:?--port needs a value}"; shift 2 ;;
    --addons) ADDON_LIST="${2:?--addons needs a value}"; shift 2 ;;
    --from-source) MODE="source"; shift ;;
    --from-release) MODE="release"; shift ;;
    --launchagents-dir) AGENTS_DIR="${2:?--launchagents-dir needs a value}"; shift 2 ;;
    --no-service) SERVICE=0; shift ;;
    --no-load) LOAD=0; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown option $1" ;;
  esac
done

case "$PORT" in
  ''|*[!0-9]*) die "--port must be a number" ;;
esac

if [ "$DRY_RUN" -eq 0 ] && [ "$(uname -s)" != "Darwin" ]; then
  die "this installer supports macOS only (use --dry-run to preview)"
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN_DIR="$PREFIX/bin"
PLIST="$AGENTS_DIR/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/SimpleTickets"

if [ "$MODE" = "auto" ]; then
  if [ -f "$REPO_ROOT/Cargo.toml" ] && command -v cargo >/dev/null 2>&1; then
    MODE="source"
  else
    MODE="release"
  fi
fi

# Runs a command, or only prints it under --dry-run.
run() {
  if [ "$DRY_RUN" -eq 1 ]; then
    printf 'would run:'
    printf ' %q' "$@"
    printf '\n'
  else
    "$@"
  fi
}

ask() {
  local prompt="$1" default="$2" reply
  if [ "$ASSUME_YES" -eq 1 ] || [ "$DRY_RUN" -eq 1 ] || [ ! -t 0 ]; then
    [ "$default" = "y" ]
    return
  fi
  read -r -p "$prompt [$([ "$default" = y ] && echo Y/n || echo y/N)] " reply
  case "$reply" in
    '') [ "$default" = "y" ] ;;
    [yY]*) return 0 ;;
    *) return 1 ;;
  esac
}

# Fills SELECTED with the crate|binary pairs of the chosen addons.
SELECTED=()
select_addons() {
  local entry name crate binary description
  for entry in "${ADDONS[@]}"; do
    IFS='|' read -r name crate binary description <<<"$entry"
    local chosen=0
    if [ -n "$ADDON_LIST" ]; then
      case ",$ADDON_LIST," in
        *",$name,"*) chosen=1 ;;
      esac
    elif ask "Install addon '$name' ($description)?" y; then
      chosen=1
    fi
    if [ "$chosen" -eq 1 ]; then
      SELECTED+=("$crate|$binary")
    fi
  done
  if [ -n "$ADDON_LIST" ] && [ "$ADDON_LIST" != "none" ]; then
    local requested known
    for requested in $(echo "$ADDON_LIST" | tr ',' ' '); do
      known=0
      for entry in "${ADDONS[@]}"; do
        [ "${entry%%|*}" = "$requested" ] && known=1
      done
      [ "$known" -eq 1 ] || die "unknown addon '$requested'"
    done
  fi
}

xml_escape() {
  local s="$1"
  s="${s//&/&amp;}"
  s="${s//</&lt;}"
  s="${s//>/&gt;}"
  printf '%s' "$s"
}

plist_text() {
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$(xml_escape "$BIN_DIR/simpletickets")</string>
    <string>--dir</string>
    <string>$(xml_escape "$TICKETS_DIR")</string>
    <string>daemon</string>
    <string>--port</string>
    <string>$PORT</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>$(xml_escape "$LOG_DIR/daemon.log")</string>
  <key>StandardErrorPath</key>
  <string>$(xml_escape "$LOG_DIR/daemon.log")</string>
</dict>
</plist>
PLIST
}

obtain_binaries() {
  # $@: binary names to install; core binary first.
  local names=("$@")
  if [ "$MODE" = "source" ]; then
    local args=(build --release --manifest-path "$REPO_ROOT/Cargo.toml" -p simpletickets)
    local entry
    for entry in "${SELECTED[@]+"${SELECTED[@]}"}"; do
      args+=(-p "${entry%%|*}")
    done
    run cargo "${args[@]}"
    local name
    for name in "${names[@]}"; do
      run install -m 0755 "$REPO_ROOT/target/release/$name" "$BIN_DIR/$name"
    done
  else
    local arch asset tmp
    arch="$(uname -m)"
    [ "$arch" = "arm64" ] && arch="aarch64"
    asset="simpletickets-$arch-apple-darwin.tar.gz"
    local url="https://github.com/$REPO/releases/latest/download/$asset"
    if [ "$DRY_RUN" -eq 1 ]; then
      echo "would download $url and install: ${names[*]}"
      return
    fi
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    curl -fsSL "$url" -o "$tmp/$asset" || die "cannot download $url (no release published yet? try --from-source)"
    tar -xzf "$tmp/$asset" -C "$tmp"
    local name
    for name in "${names[@]}"; do
      [ -f "$tmp/$name" ] || die "$name missing from $asset"
      install -m 0755 "$tmp/$name" "$BIN_DIR/$name"
    done
  fi
}

select_addons

NAMES=(simpletickets)
for entry in "${SELECTED[@]+"${SELECTED[@]}"}"; do
  NAMES+=("${entry#*|}")
done

echo "Install mode: $MODE"
echo "Binaries:     $BIN_DIR (${NAMES[*]})"
echo "Tickets:      $TICKETS_DIR"
[ "$SERVICE" -eq 1 ] && echo "Service:      $PLIST (port $PORT)"

run mkdir -p "$BIN_DIR" "$TICKETS_DIR"
obtain_binaries "${NAMES[@]}"

if [ "$SERVICE" -eq 1 ]; then
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "would write $PLIST:"
    plist_text
  else
    mkdir -p "$AGENTS_DIR" "$LOG_DIR"
    plist_text >"$PLIST"
    command -v plutil >/dev/null 2>&1 && plutil -lint "$PLIST" >/dev/null
    echo "wrote $PLIST"
    if [ "$LOAD" -eq 1 ] && ask "Start the daemon now and at every login?" y; then
      domain="gui/$(id -u)"
      launchctl bootout "$domain/$LABEL" >/dev/null 2>&1 || true
      launchctl bootstrap "$domain" "$PLIST"
      echo "daemon loaded as $LABEL"
    else
      echo "not loaded; start it with: launchctl bootstrap gui/$(id -u) $PLIST"
    fi
  fi
fi

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "note: $BIN_DIR is not on your PATH" ;;
esac
for entry in "${SELECTED[@]+"${SELECTED[@]}"}"; do
  if [ "${entry%%|*}" = "simpletickets-mcp" ]; then
    echo "MCP server: register '$BIN_DIR/simpletickets-mcp --dir $TICKETS_DIR' as a stdio server in your MCP client."
  fi
done
echo "done"
