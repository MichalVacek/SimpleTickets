#!/usr/bin/env bash
# Installs SimpleTickets on macOS or Linux: binaries into a prefix, optional addons,
# and a per-user service that keeps the daemon running (launchd on macOS, a systemd
# user unit on Linux).
set -euo pipefail

REPO="MichalVacek/SimpleTickets"
LABEL="com.simpletickets.daemon"
UNIT="simpletickets.service"

TMP_DIR=""
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Addons come from the plugin manifests in this checkout (plugins/*/plugin.yml): every
# manifest with an `install:` block is offered. Each entry is
# name|package|binary|description|manifest. Without a checkout (the script run on its
# own) the built-in entry below is used and manifests come from the release archive.
ADDONS=()

# Prints name|package|binary|description for a manifest, from its top-level scalars and
# its install block; prints nothing when the manifest has no install block.
manifest_addon() {
  awk '
    function value(line,   v) {
      v = line
      sub(/^[^:]*:[ \t]*/, "", v)
      sub(/[ \t]+$/, "", v)
      gsub(/^["\047]|["\047]$/, "", v)
      gsub(/\|/, "/", v)
      return v
    }
    /^[A-Za-z_]/ { in_install = ($0 ~ /^install:/) }
    /^name:/ { name = value($0) }
    /^description:/ { description = value($0) }
    in_install && /^[ \t]+package:/ { package = value($0) }
    in_install && /^[ \t]+binary:/ { binary = value($0) }
    END {
      if (name != "" && package != "" && binary != "") {
        print name "|" package "|" binary "|" description
      }
    }
  ' "$1"
}

load_addons() {
  local manifest line
  for manifest in "$REPO_ROOT"/plugins/*/plugin.yml; do
    [ -f "$manifest" ] || continue
    line="$(manifest_addon "$manifest")"
    if [ -n "$line" ]; then
      ADDONS+=("$line|$manifest")
    fi
  done
  if [ "${#ADDONS[@]}" -eq 0 ]; then
    ADDONS=("mcp|simpletickets-mcp|simpletickets-mcp|MCP server so a client can list, create and update tickets|")
  fi
}

load_addons

usage() {
  cat <<'USAGE'
Usage: scripts/install.sh [options]

  --prefix DIR           install binaries into DIR/bin (default: ~/.local)
  --tickets-dir DIR      tickets folder the daemon manages (default: ~/SimpleTickets)
  --port N               daemon port on 127.0.0.1 (default: 7878)
  --addons LIST          comma-separated addons to install, or "none" (default: ask)
  --plugins-dir DIR      where addon manifests are installed for the daemon to find
                         (default: $SIMPLETICKETS_PLUGINS_DIR or
                         ${XDG_CONFIG_HOME:-~/.config}/simpletickets/plugins)
  --from-source          build with cargo from this checkout
  --from-release         download the latest release archive
                         (default: from source when run inside a checkout with cargo)
  --launchagents-dir DIR macOS: where to write the plist (default: ~/Library/LaunchAgents)
  --systemd-dir DIR      Linux: where to write the unit
                         (default: ${XDG_CONFIG_HOME:-~/.config}/systemd/user)
  --no-service           do not write a service definition
  --no-load              write the service definition but do not load or start it
  --os macos|linux       preview another OS's plan (only together with --dry-run)
  --yes                  accept defaults without asking
  --dry-run              print what would happen, change nothing
  -h, --help             show this help

Available addons:
USAGE
  local entry
  for entry in "${ADDONS[@]}"; do
    IFS='|' read -r name _ _ description _ <<<"$entry"
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
PLUGINS_DIR="${SIMPLETICKETS_PLUGINS_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/simpletickets/plugins}"
MODE="auto"
AGENTS_DIR="$HOME/Library/LaunchAgents"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
OS=""
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
    --plugins-dir) PLUGINS_DIR="${2:?--plugins-dir needs a value}"; shift 2 ;;
    --from-source) MODE="source"; shift ;;
    --from-release) MODE="release"; shift ;;
    --launchagents-dir) AGENTS_DIR="${2:?--launchagents-dir needs a value}"; shift 2 ;;
    --systemd-dir) SYSTEMD_DIR="${2:?--systemd-dir needs a value}"; shift 2 ;;
    --os) OS="${2:?--os needs a value}"; shift 2 ;;
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

case "$(uname -s)" in
  Darwin) HOST_OS="macos" ;;
  Linux) HOST_OS="linux" ;;
  *) HOST_OS="" ;;
esac
if [ -n "$OS" ]; then
  [ "$DRY_RUN" -eq 1 ] || die "--os is only for previews and needs --dry-run"
  case "$OS" in
    macos|linux) ;;
    *) die "--os must be macos or linux" ;;
  esac
else
  OS="$HOST_OS"
  [ -n "$OS" ] || die "unsupported system $(uname -s); use macOS or Linux (Windows: scripts/install.ps1)"
fi

BIN_DIR="$PREFIX/bin"
PLIST="$AGENTS_DIR/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/SimpleTickets"
UNIT_FILE="$SYSTEMD_DIR/$UNIT"

if [ "$MODE" = "auto" ]; then
  if [ -f "$REPO_ROOT/Cargo.toml" ] && command -v cargo >/dev/null 2>&1 && [ "$OS" = "$HOST_OS" ]; then
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

# Fills SELECTED with the package|binary|name|manifest entries of the chosen addons.
SELECTED=()
select_addons() {
  local entry name crate binary description manifest
  for entry in "${ADDONS[@]}"; do
    IFS='|' read -r name crate binary description manifest <<<"$entry"
    local chosen=0
    if [ -n "$ADDON_LIST" ]; then
      case ",$ADDON_LIST," in
        *",$name,"*) chosen=1 ;;
      esac
    elif ask "Install addon '$name' ($description)?" y; then
      chosen=1
    fi
    if [ "$chosen" -eq 1 ]; then
      SELECTED+=("$crate|$binary|$name|$manifest")
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

# Quotes one systemd ExecStart word: backslash, double quote, % and $ are special.
systemd_quote() {
  local s="$1"
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  s="${s//%/%%}"
  s="${s//\$/\$\$}"
  printf '"%s"' "$s"
}

unit_text() {
  cat <<UNITFILE
[Unit]
Description=SimpleTickets daemon

[Service]
ExecStart=$(systemd_quote "$BIN_DIR/simpletickets") --dir $(systemd_quote "$TICKETS_DIR") daemon --port $PORT
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
UNITFILE
}

# Prints the release asset name for this OS and CPU.
release_asset() {
  local arch
  arch="$(uname -m)"
  case "$arch" in
    arm64|aarch64) arch="aarch64" ;;
    x86_64|amd64) arch="x86_64" ;;
    *) die "no release archive for CPU $arch" ;;
  esac
  case "$OS" in
    macos) echo "simpletickets-$arch-apple-darwin.tar.gz" ;;
    linux) echo "simpletickets-$arch-unknown-linux-gnu.tar.gz" ;;
  esac
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# Copies the manifests of the selected addons into PLUGINS_DIR so the daemon finds them.
# $1: folder of an extracted release archive, used when an addon has no checkout manifest.
install_manifests() {
  local entry name manifest
  for entry in "${SELECTED[@]+"${SELECTED[@]}"}"; do
    IFS="|" read -r _ _ name manifest <<<"$entry"
    if [ -z "$manifest" ] && [ -n "${1:-}" ] && [ -f "$1/plugins/$name/plugin.yml" ]; then
      manifest="$1/plugins/$name/plugin.yml"
    fi
    if [ -z "$manifest" ]; then
      if [ "$DRY_RUN" -eq 1 ]; then
        echo "would install the manifest of '$name' into $PLUGINS_DIR/$name"
      else
        echo "note: no manifest found for addon '$name'; the daemon will not list it"
      fi
      continue
    fi
    run mkdir -p "$PLUGINS_DIR/$name"
    run install -m 0644 "$manifest" "$PLUGINS_DIR/$name/plugin.yml"
  done
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
    install_manifests
  else
    local asset base
    asset="$(release_asset)"
    base="https://github.com/$REPO/releases/latest/download"
    if [ "$DRY_RUN" -eq 1 ]; then
      echo "would download $base/$asset, verify it against $base/SHA256SUMS and install: ${names[*]}"
      install_manifests
      return
    fi
    TMP_DIR="$(mktemp -d)"
    trap 'rm -rf "$TMP_DIR"' EXIT
    local tmp="$TMP_DIR"
    curl -fsSL "$base/$asset" -o "$tmp/$asset" || die "cannot download $base/$asset (no release published yet? try --from-source)"
    curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "cannot download $base/SHA256SUMS"
    local expected actual
    expected="$(awk -v a="$asset" '$2 == a || $2 == "*" a { print $1 }' "$tmp/SHA256SUMS")"
    [ -n "$expected" ] || die "$asset is not listed in SHA256SUMS"
    actual="$(sha256_of "$tmp/$asset")"
    [ "$expected" = "$actual" ] || die "checksum mismatch for $asset"
    tar -xzf "$tmp/$asset" -C "$tmp"
    local name
    for name in "${names[@]}"; do
      [ -f "$tmp/$name" ] || die "$name missing from $asset"
      install -m 0755 "$tmp/$name" "$BIN_DIR/$name"
    done
    install_manifests "$tmp"
  fi
}

select_addons

NAMES=(simpletickets)
for entry in "${SELECTED[@]+"${SELECTED[@]}"}"; do
  NAMES+=("$(cut -d'|' -f2 <<<"$entry")")
done

echo "Install mode: $MODE"
echo "Binaries:     $BIN_DIR (${NAMES[*]})"
echo "Tickets:      $TICKETS_DIR"
if [ "${#SELECTED[@]}" -gt 0 ]; then
  echo "Plugins:      $PLUGINS_DIR"
fi
if [ "$SERVICE" -eq 1 ]; then
  if [ "$OS" = "macos" ]; then
    echo "Service:      $PLIST (port $PORT)"
  else
    echo "Service:      $UNIT_FILE (port $PORT)"
  fi
fi

run mkdir -p "$BIN_DIR" "$TICKETS_DIR"
obtain_binaries "${NAMES[@]}"

install_launchd() {
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "would write $PLIST:"
    plist_text
    return
  fi
  mkdir -p "$AGENTS_DIR" "$LOG_DIR"
  plist_text >"$PLIST"
  command -v plutil >/dev/null 2>&1 && plutil -lint "$PLIST" >/dev/null
  echo "wrote $PLIST"
  if [ "$LOAD" -eq 1 ] && ask "Start the daemon now and at every login?" y; then
    local domain
    domain="gui/$(id -u)"
    launchctl bootout "$domain/$LABEL" >/dev/null 2>&1 || true
    launchctl bootstrap "$domain" "$PLIST"
    echo "daemon loaded as $LABEL"
  else
    echo "not loaded; start it with: launchctl bootstrap gui/$(id -u) $PLIST"
  fi
}

install_systemd() {
  if [ "$DRY_RUN" -eq 1 ]; then
    echo "would write $UNIT_FILE:"
    unit_text
    echo "would run: systemctl --user daemon-reload"
    echo "would run: systemctl --user enable --now $UNIT"
    return
  fi
  command -v systemctl >/dev/null 2>&1 || die "systemctl not found; use --no-service and run the daemon yourself"
  mkdir -p "$SYSTEMD_DIR"
  unit_text >"$UNIT_FILE"
  echo "wrote $UNIT_FILE"
  if [ "$LOAD" -eq 1 ] && ask "Start the daemon now and at every login?" y; then
    systemctl --user daemon-reload
    systemctl --user enable --now "$UNIT"
    echo "daemon started as $UNIT"
    echo "note: to keep it running while logged out, run: loginctl enable-linger $(id -un)"
  else
    echo "not started; start it with: systemctl --user enable --now $UNIT"
  fi
}

if [ "$SERVICE" -eq 1 ]; then
  if [ "$OS" = "macos" ]; then
    install_launchd
  else
    install_systemd
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
  if [ "${entry%%|*}" = "simpletickets-ui" ]; then
    echo "Board: run '$BIN_DIR/simpletickets-ui --dir $TICKETS_DIR' and open the address it prints, or run 'simpletickets plugins enable ui' so the daemon starts it."
  fi
done
echo "done"
