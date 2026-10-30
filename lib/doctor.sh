# `jodd-ai-host doctor`: one ok/FIX line per check; returns 1 if any check needs fixing.
# Sourced by jodd-ai-host, so it uses that script's helpers (as_user, sshd_bin, fingerprint...).
# Read-only. The checks that need root (sshd -t / -T) say so instead of failing when run without it.
_d_bad=0
_d_ok()   { echo "ok    $*"; }
_d_note() { echo "note  $*"; }
_d_fix()  { echo "FIX   $*"; _d_bad=1; }
_d_warn() { echo "WARN  $*"; }
# The uid that owns PATH (following a link), and its mode string.
# shellcheck disable=SC2012  # one named file; only the owner and mode columns are read
_d_uid()  { ls -lnL "$1" 2>/dev/null | awk 'NR == 1 { print $3 }'; }
# shellcheck disable=SC2012
_d_mode() { ls -lL "$1" 2>/dev/null | awk 'NR == 1 { print substr($1, 1, 10) }'; }

doctor() {
  _d_bad=0
  _port=${JODD_AI_PORT:-22}
  # In the Docker image JODD_AI_PORT is the PUBLISHED port (compose maps it to 22); inside the
  # container sshd listens on 22, so that is the port to check here.
  _lport=$_port _where=
  if [ "$JODD_AI_KIND" = docker ]; then _lport=22 _where=" inside the container (published as $_port)"; fi

  # 1. tools Jodd's Setup and Update scripts use on the host
  for _t in sh base64 mktemp; do
    if command -v "$_t" >/dev/null 2>&1; then _d_ok "$_t found"; else _d_fix "$_t is missing; Jodd's Setup needs it. Install it with your package manager."; fi
  done
  if command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1; then _d_ok "sha256sum/shasum found"; else _d_fix "neither sha256sum nor shasum found; install coreutils (or perl)."; fi
  if (head -c 1 </dev/null) >/dev/null 2>&1; then _d_ok "head -c works"; else _d_fix "head -c is not supported by this system's head; install GNU coreutils."; fi
  if command -v setsid >/dev/null 2>&1 || sh -c 'set -m' >/dev/null 2>&1; then _d_ok "setsid or job control available"; else _d_fix "neither setsid nor 'set -m' works; install util-linux (setsid)."; fi

  # 2. sshd installed, running, listening
  _sshd=$(sshd_bin) || _sshd=
  if [ -n "$_sshd" ]; then _d_ok "sshd installed ($_sshd)"; else _d_fix "sshd is not installed. Install the OpenSSH server (e.g. apt install openssh-server)."; fi
  if [ "$JODD_AI_KIND" = macos ]; then
    _d_note "macOS starts sshd on demand; only the port is checked"
  elif command -v pgrep >/dev/null 2>&1; then
    if pgrep -x sshd >/dev/null 2>&1; then _d_ok "sshd is running"; else _d_fix "sshd is not running. Start it (e.g. systemctl enable --now ssh)."; fi
  else
    _d_note "pgrep not found; skipping the sshd-running check"
  fi
  if command -v nc >/dev/null 2>&1; then
    if nc -z 127.0.0.1 "$_lport" >/dev/null 2>&1; then _d_ok "something is listening on port $_lport$_where"; else _d_fix "nothing is listening on port $_lport$_where. Check sshd's Port setting and any firewall."; fi
  else
    _d_note "nc not found; skipping the port check"
  fi

  # 3. sshd accepts the drop-in and the AI account's password setting is what we expect
  if [ -n "${JODD_AI_SSHD_TEST:-}" ] || { [ -n "$_sshd" ] && [ "$(id -u)" = 0 ]; }; then
    if sshd_test >/dev/null 2>&1; then _d_ok "sshd -t accepts the configuration"; else _d_fix "sshd -t rejects the configuration. Run it by hand to see why; the Jodd drop-in is $JODD_AI_DROPIN."; fi
  else
    _d_note "sshd -t needs root; run: sudo jodd-ai-host doctor"
  fi
  if [ -f "$JODD_AI_DROPIN" ]; then _d_ok "drop-in present ($JODD_AI_DROPIN)"; else _d_fix "drop-in $JODD_AI_DROPIN is missing. Re-run the installer, or: sudo jodd-ai-host unlock"; fi
  _locked=no
  if grep -q 'PasswordAuthentication no' "$JODD_AI_DROPIN" 2>/dev/null; then _locked=yes; fi
  _dump=
  if [ -n "${JODD_AI_SSHD_DUMP:-}" ] || { [ -n "$_sshd" ] && [ "$(id -u)" = 0 ]; }; then
    _dump=$(sshd_effective 2>/dev/null)
    _eff=$(printf '%s\n' "$_dump" | awk '$1 == "passwordauthentication" { print $2 }')
    if [ "$_locked" = yes ]; then
      _d_note "password login for $U is locked (off); unlock it before Setup or Update in Jodd: sudo jodd-ai-host unlock"
    elif [ "$_eff" = yes ]; then
      _d_ok "sshd allows password login for $U"
    else
      _d_fix "sshd does not allow password login for $U (effective: ${_eff:-unknown}). The drop-in may not be included by sshd_config (needs 'Include /etc/ssh/sshd_config.d/*.conf')."
    fi
  elif [ "$_locked" = yes ]; then
    _d_note "password login for $U is locked (off); unlock it before Setup or Update in Jodd: sudo jodd-ai-host unlock"
  else
    _d_note "the effective sshd setting needs root (sshd -T); run: sudo jodd-ai-host doctor"
  fi
  # ... and nothing else in sshd's configuration keeps the account out (AuthenticationMethods,
  # AllowUsers / AllowGroups / DenyUsers / DenyGroups). The kit never changes those: say which to edit.
  if [ -n "$_dump" ]; then
    _probs=$(printf '%s\n' "$_dump" | sshd_login_problems "$U" "$(id -Gn "$U" 2>/dev/null)")
    if [ -z "$_probs" ]; then
      _d_ok "sshd's AuthenticationMethods and AllowUsers/AllowGroups/DenyUsers/DenyGroups let $U sign in"
    else
      while IFS= read -r _l; do _d_fix "$_l"; done <<EOP
$_probs
EOP
    fi
  fi

  # 4. the 'restrict' key option needs OpenSSH 7.2 or newer
  _ver=$( { ssh -V 2>&1; [ -n "$_sshd" ] && "$_sshd" -V 2>&1; } | sed -n 's/.*OpenSSH_\([0-9][0-9]*\)\.\([0-9][0-9]*\).*/\1 \2/p' | head -n 1)
  if [ -z "$_ver" ]; then
    _d_note "could not read the OpenSSH version; Jodd's keys use 'restrict', which needs 7.2 or newer"
  else
    _maj=${_ver% *} _min=${_ver#* }
    if [ "$_maj" -gt 7 ] || { [ "$_maj" -eq 7 ] && [ "$_min" -ge 2 ]; }; then _d_ok "OpenSSH $_maj.$_min supports 'restrict'"; else _d_fix "OpenSSH $_maj.$_min is too old for 'restrict' (needs 7.2+). Upgrade OpenSSH."; fi
  fi

  # 5. both CLIs reachable the way Jodd's Setup looks: the account's login shell, `-lc` then `-lic`
  for _c in claude codex; do
    _how=-lc; _p=$(cli_path_in lc "$_c")
    if [ -z "$_p" ]; then _how=-lic; _p=$(cli_path_in lic "$_c"); fi
    if [ -n "$_p" ]; then _d_ok "$_c found (login shell $_how): $_p"; continue; fi
    _found=
    for _dir in "$HOMEDIR/.local/bin" "$HOMEDIR/.codex/bin"; do
      if [ -x "$_dir/$_c" ]; then _found=$_dir; break; fi
    done
    if [ -n "$_found" ]; then
      _d_fix "$_c is installed in $_found but the account's login shell doesn't search it: add export PATH=\"$_found:\$PATH\" to the file that shell reads at login (~/.profile for sh; ~/.bash_profile or ~/.profile for bash; ~/.zprofile for zsh) in $HOMEDIR (the installer does this)."
    else
      _d_fix "$_c is not installed. Run: sudo jodd-ai-host update-clis"
    fi
  done

  # 6. jodd-ai-mcp, the forced command of every Jodd key: present, root's, and only root's to change
  if [ ! -x "$MCP" ]; then
    _d_fix "jodd-ai-mcp is not installed at $MCP. Re-run the installer (it downloads and verifies it)."
  else
    _mv=$("$MCP" --version 2>/dev/null) || _mv=
    case $_mv in
      "jodd-ai-mcp "*) _d_ok "$_mv installed ($MCP)" ;;
      *) _d_fix "$MCP does not run on this machine. Re-run the installer." ;;
    esac
    _mu=$(_d_uid "$MCP") _mm=$(_d_mode "$MCP")
    if [ "$_mu" != 0 ]; then _d_fix "$MCP is owned by uid ${_mu:-?}, not root: whoever owns it decides what Jodd's keys run. Re-run the installer (it installs it root-owned)."
    else case $_mm in ?????w????|????????w?) _d_fix "$MCP ($_mm) can be changed by others than root. Run: sudo chmod 755 $MCP" ;; *) _d_ok "$MCP is root-owned ($_mm)" ;; esac; fi
  fi

  # 7. the policy: valid, readable by the account (the server reads it as the account), and not
  # writable by it (else the account decides what Jodd may run). In Docker it is a link into the
  # state volume; the checks follow it, and the directory that holds the real file counts too.
  _pr=$POLICY
  if [ -L "$_pr" ]; then
    _pt=$(readlink "$_pr")
    case $_pt in /*) _pr=$_pt ;; *) _pr=$(dirname "$POLICY")/$_pt ;; esac
  fi
  if [ ! -e "$POLICY" ]; then
    _d_fix "there is no policy at $POLICY, so Jodd may run nothing. Run: sudo jodd-ai-host enable claude (or codex)"
  else
    if [ -x "$MCP" ]; then
      if _pe=$("$MCP" admin list 2>&1 >/dev/null); then
        _d_ok "the policy at $POLICY is valid"
        "$MCP" admin list 2>/dev/null | while read -r _h _st _hp; do _d_note "$_h: $_st${_hp:+ ($_hp)}"; done
      else
        _d_fix "$(printf '%s' "$_pe" | tr '\n' ' ' | tr -s ' ')"
      fi
    fi
    if run_as_account "[ -r $(sq "$POLICY") ]" </dev/null >/dev/null 2>&1; then _d_ok "$U can read the policy"
    else _d_fix "$U cannot read $POLICY, so jodd-ai-mcp (which runs as $U) enables nothing. Run: sudo chmod 644 $_pr"; fi
    if run_as_account "[ -w $(sq "$_pr") ] || [ -w $(sq "$(dirname "$_pr")") ]" </dev/null >/dev/null 2>&1; then
      _d_fix "$U can change the policy ($_pr or its directory is writable by it), so the account decides what Jodd may run. Make both root's: sudo chown root $_pr $(dirname "$_pr"); sudo chmod 644 $_pr; sudo chmod 755 $(dirname "$_pr")"
    else
      _d_ok "$U cannot change the policy"
    fi
    if [ "$(_d_uid "$_pr")" != 0 ]; then _d_fix "$_pr is not owned by root. Run: sudo chown root $_pr"; fi
    if grep -Eq '^[[:space:]]*max_concurrent_runs[[:space:]]*=[[:space:]]*0[[:space:]]*(#.*)?$' "$_pr" 2>/dev/null; then
      _d_fix "the policy sets max_concurrent_runs = 0, so every call is refused as busy. Remove the line (default 2) or raise it."
    fi
  fi

  # 8. Jodd's keys: each should run jodd-ai-mcp. Read as the account (root never opens its file).
  if [ -x "$MCP" ]; then
    if _ks=$(mcp_as_account scan-keys </dev/null 2>&1); then
      _kn=0
      while read -r _id _kind _kc; do
        [ -n "$_id" ] || continue
        _kn=$((_kn + 1))
        case $_kind in
          mcp) ;;
          legacy) _d_warn "key jodd-ai-$_id still runs an older helper ($_kc). Run: sudo jodd-ai-host adopt-keys" ;;
          *) _d_warn "key jodd-ai-$_id does not run jodd-ai-mcp (${_kc}); adopt-keys cannot re-point it. Edit it by hand, or: sudo jodd-ai-host revoke $_id" ;;
        esac
      done <<EOK
$_ks
EOK
      _d_ok "Jodd keys read as $U ($_kn found)"
    else
      _d_fix "cannot read $U's keys: $_ks"
    fi
  fi

  # 9. the fingerprint Jodd will ask you to confirm
  if _fp=$(fingerprint); then _d_ok "host key: $_fp"; else _d_fix "cannot read the host key at $JODD_AI_HOSTKEY."; fi

  return $_d_bad
}
