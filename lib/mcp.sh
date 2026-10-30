# Installs the jodd-ai-mcp binary from a release, and the root-owned places it needs. Sourced by
# install/linux.sh and install/macos.sh (which define say / warn); every function returns 1 after
# saying why, and changes nothing it has not finished.
#
# Release assets (one per target, ruling R26):
#   jodd-ai-mcp-<version>-<target>.tar.gz          exactly one file, jodd-ai-mcp, at its root
#   jodd-ai-mcp-<version>-<target>.tar.gz.sha256   "<64 hex>  <asset name>" (sha256sum's format)
# Environment:
#   JODD_AI_MCP_VERSION   install this version (no leading v); default: the latest release
#   JODD_AI_MCP_BASE_URL  (tests) where the assets are, instead of the release's download URL;
#                         may be a file:// URL. Needs JODD_AI_MCP_VERSION.

MCP_REPO_URL=https://github.com/BBM-Co-ORG/jodd-ai-host

# The release target for this machine.
mcp_target() {
  _os=$(uname -s) _arch=$(uname -m)
  # A shell under Rosetta reports x86_64 on Apple silicon: install the native binary.
  if [ "$_os" = Darwin ] && [ "$_arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
    _arch=arm64
  fi
  case $_os:$_arch in
    Linux:x86_64|Linux:amd64)   echo x86_64-unknown-linux-musl ;;
    Linux:aarch64|Linux:arm64)  echo aarch64-unknown-linux-musl ;;
    Darwin:arm64|Darwin:aarch64) echo aarch64-apple-darwin ;;
    Darwin:x86_64)              echo x86_64-apple-darwin ;;
    *) warn "there is no jodd-ai-mcp release for $_os on $_arch (there is one for Linux and macOS, on x86_64 and arm64)."; return 1 ;;
  esac
}

# The latest release's version, from where GitHub redirects /releases/latest (ruling R27: no API,
# no token, no rate limit). Prints e.g. 1.0.0.
mcp_latest_version() {
  _u=$(curl --proto '=https' --tlsv1.2 -fsSLI -o /dev/null -w '%{url_effective}' "$MCP_REPO_URL/releases/latest") || {
    warn "cannot reach $MCP_REPO_URL/releases/latest to find the latest jodd-ai-mcp (no network?). Pin one with JODD_AI_MCP_VERSION=<version>."; return 1; }
  _v=${_u##*/}; _v=${_v#v}
  if ! mcp_version_ok "$_v"; then
    warn "cannot tell the latest jodd-ai-mcp release from $_u. Pin one with JODD_AI_MCP_VERSION=<version>."; return 1
  fi
  printf '%s\n' "$_v"
}
mcp_version_ok() {
  case $1 in [0-9]*.[0-9]*.[0-9]*) ;; *) return 1 ;; esac
  case $1 in *[!0-9A-Za-z.+-]*) return 1 ;; esac
}

# SHA-256 of a file, lower-case hex.
mcp_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{ print $1 }'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{ print $1 }'
  else return 1; fi
}

# mcp_install DEST GROUP: download the release for this machine, verify its SHA-256 BEFORE
# unpacking, unpack into a fresh temp dir, check the binary runs, and only then put it at DEST
# (owner root, group GROUP, mode 0755) with a rename. On any failure DEST is left as it was.
# Re-running installs the release again: that is the upgrade.
mcp_install() {
  _dest=$1 _grp=$2
  _target=$(mcp_target) || return 1
  _ver=${JODD_AI_MCP_VERSION:-}
  if [ -n "${JODD_AI_MCP_BASE_URL:-}" ]; then
    [ -n "$_ver" ] || { warn "JODD_AI_MCP_BASE_URL needs JODD_AI_MCP_VERSION."; return 1; }
    _base=${JODD_AI_MCP_BASE_URL%/} _proto='=https,file'
  else
    if [ -z "$_ver" ]; then _ver=$(mcp_latest_version) || return 1; fi
    _base=$MCP_REPO_URL/releases/download/v$_ver _proto='=https'
  fi
  _ver=${_ver#v}
  mcp_version_ok "$_ver" || { warn "'$_ver' is not a jodd-ai-mcp version (like 1.0.0)."; return 1; }
  _asset=jodd-ai-mcp-$_ver-$_target.tar.gz
  _w=$(mktemp -d) || { warn "cannot create a temp directory."; return 1; }
  _r=1
  if mcp_install_in "$_w"; then _r=0; fi
  rm -rf "$_w"
  return $_r
}
# The body of mcp_install, working in the fresh, private directory $1.
mcp_install_in() {
  say "downloading jodd-ai-mcp $_ver for $_target"
  if ! { curl --proto "$_proto" --tlsv1.2 -fsSL "$_base/$_asset" -o "$1/$_asset" \
         && curl --proto "$_proto" --tlsv1.2 -fsSL "$_base/$_asset.sha256" -o "$1/$_asset.sha256"; }; then
    warn "cannot download $_base/$_asset (or its .sha256); no network, or no such release? Nothing was installed."; return 1
  fi
  # "<64 hex>  <asset name>" (a leading * marks sha256sum's binary mode).
  _want=$(awk 'NR == 1 { print tolower($1) }' "$1/$_asset.sha256")
  _name=$(awk 'NR == 1 { sub(/^\*/, "", $2); print $2 }' "$1/$_asset.sha256")
  case $_want in
    *[!0-9a-f]*|'') _want= ;;
    *) [ "${#_want}" -eq 64 ] || _want= ;;
  esac
  if [ -z "$_want" ] || [ "$_name" != "$_asset" ]; then
    warn "$_asset.sha256 is not a checksum for $_asset; nothing was installed."; return 1
  fi
  _got=$(mcp_sha256 "$1/$_asset") || { warn "neither sha256sum nor shasum is installed, so the download cannot be verified; nothing was installed."; return 1; }
  if [ "$_got" != "$_want" ]; then
    warn "SHA-256 mismatch for $_asset (expected $_want, got $_got): the download is damaged or not the release. Nothing was installed${_dest:+; $_dest is unchanged}."
    return 1
  fi
  # Exactly one entry, the binary, before anything is unpacked.
  _list=$(tar -tzf "$1/$_asset" 2>/dev/null) || { warn "$_asset is not a readable archive; nothing was installed."; return 1; }
  case $_list in
    jodd-ai-mcp|./jodd-ai-mcp) ;;
    *) warn "$_asset does not hold just the jodd-ai-mcp binary; nothing was installed."; return 1 ;;
  esac
  if ! { mkdir "$1/x" && chmod 700 "$1/x" && tar -xzf "$1/$_asset" -C "$1/x"; }; then
    warn "cannot unpack $_asset; nothing was installed."; return 1
  fi
  _bin=$1/x/jodd-ai-mcp
  if [ -L "$_bin" ] || [ ! -f "$_bin" ]; then warn "$_asset does not hold a regular file; nothing was installed."; return 1; fi
  _new=$(dirname "$_dest")/.jodd-ai-mcp.new.$$
  rm -f "$_new"
  if ! install -o root -g "$_grp" -m 0755 "$_bin" "$_new"; then
    rm -f "$_new"; warn "cannot write $_new; nothing was installed."; return 1
  fi
  # It must run here (right platform, not truncated) before it replaces anything.
  _said=$("$_new" --version 2>/dev/null) || _said=
  case $_said in
    "jodd-ai-mcp "*) ;;
    *) rm -f "$_new"; warn "the downloaded jodd-ai-mcp does not run on this machine; nothing was installed."; return 1 ;;
  esac
  [ "$_said" = "jodd-ai-mcp $_ver" ] || warn "note: the release $_ver says it is '$_said'."
  if ! mv -f "$_new" "$_dest"; then rm -f "$_new"; warn "cannot move the new binary to $_dest; nothing was installed."; return 1; fi
  say "installed $_dest ($_said)"
}

# mcp_policy_init GROUP: /etc/jodd-ai-host (root, 0755) and an empty policy (root, 0644) if there
# is none. An existing policy keeps its content: it holds the administrator's decisions.
mcp_policy_init() {
  _pd=/etc/jodd-ai-host _pf=/etc/jodd-ai-host/policy.toml
  if [ -L "$_pd" ] || { [ -e "$_pd" ] && [ ! -d "$_pd" ]; }; then warn "$_pd exists and is not a directory; move it away and run this again."; return 1; fi
  if ! { mkdir -p "$_pd" && chown "root:$1" "$_pd" && chmod 755 "$_pd"; }; then warn "cannot create $_pd."; return 1; fi
  if [ ! -e "$_pf" ] && [ ! -L "$_pf" ]; then
    if ! { ( umask 022; : > "$_pf.new" ) && chown "root:$1" "$_pf.new" && chmod 644 "$_pf.new" && mv "$_pf.new" "$_pf"; }; then
      rm -f "$_pf.new"; warn "cannot write $_pf."; return 1
    fi
    say "wrote an empty policy, $_pf (nothing enabled yet)"
  elif [ -f "$_pf" ] && [ ! -L "$_pf" ]; then
    # Root-owned and read-only to everyone else, whatever an earlier hand edit left.
    if ! { chown "root:$1" "$_pf" && chmod 644 "$_pf"; }; then warn "cannot set the owner and mode of $_pf."; return 1; fi
  fi
}

# mcp_log_dir USER: /var/log/jodd-ai-host, where jodd-ai-mcp (running as USER) writes its one
# metadata line per call when syslog is not there. Owned by USER, mode 0700.
mcp_log_dir() {
  _ld=/var/log/jodd-ai-host
  if [ -L "$_ld" ] || { [ -e "$_ld" ] && [ ! -d "$_ld" ]; }; then warn "$_ld exists and is not a directory; move it away and run this again."; return 1; fi
  if ! { mkdir -p "$_ld" && chown "$1" "$_ld" && chmod 700 "$_ld"; }; then warn "cannot create $_ld for $1."; return 1; fi
}
