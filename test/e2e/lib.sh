# Shared by the e2e drivers. Source it; do not run it.
# Every check here talks to the host the way a client does: real OpenSSH, a Jodd-style
# restricted key line, and test/e2e/mcp_client.py (stdlib Python, run with -I) for MCP and for
# the legacy `jodd-ai-run 2 …` framing. Nothing here needs the Jodd app repo.
repo=$(cd "$(dirname "$0")/../.." && pwd)
E2E_CLIENT=$repo/test/e2e/mcp_client.py
# Keys, known_hosts and ssh wrappers for this run; removed by e2e_keys_cleanup.
E2E_KD=$(mktemp -d)
E2E_PORT=
e2e_keys_cleanup() { rm -rf "$E2E_KD"; }

e2e_wait_port() { # $1 port — up to 60 s
  i=0
  while ! nc -z 127.0.0.1 "$1" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -gt 60 ] && return 1
    sleep 1
  done
}

# Milliseconds since the epoch (GNU date has %N; this works wherever python3 does).
e2e_now_ms() { python3 -I -c 'import time; print(int(time.time() * 1000))'; }

# A real password login through the ssh client's askpass, as Jodd's Setup does it. Succeeds only
# if the server accepted the password. The password goes by environment, not argv.
# shellcheck disable=SC2016  # $E2E_PW is for the askpass script
e2e_ssh_pw() { # $1 port, $2 user, $3 password
  _ap=$(mktemp); printf '#!/bin/sh\nprintf "%%s\\n" "$E2E_PW"\n' > "$_ap"; chmod 700 "$_ap"
  _rc=0
  E2E_PW=$3 SSH_ASKPASS=$_ap SSH_ASKPASS_REQUIRE=force DISPLAY=none ssh -o PubkeyAuthentication=no \
    -o PreferredAuthentications=password,keyboard-interactive -o NumberOfPasswordPrompts=1 \
    -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 \
    -p "$1" "$2@127.0.0.1" true </dev/null >/dev/null 2>&1 || _rc=$?
  rm -f "$_ap"; return $_rc
}

# Pin the container's host key, as Jodd does after the user confirms the fingerprint: ssh then
# refuses any other key (StrictHostKeyChecking=yes in the wrappers below).
e2e_pin_host() { # $1 container, $2 published port
  E2E_PORT=$2
  _k=$(docker exec "$1" sh -c '. /etc/jodd-ai-host.conf && cut -d" " -f1-2 "$JODD_AI_HOSTKEY"') || return 1
  printf '[127.0.0.1]:%s %s\n' "$2" "$_k" > "$E2E_KD/known_hosts"
}

# A per-device key, as Jodd makes one: ed25519, comment jodd-ai-<id>. Also writes
# $E2E_KD/ssh-<id>, which runs ssh with that key alone (no agent, no password) against the
# pinned host; its arguments are the request. E2E_SSH_EXTRA adds client options (e.g. -tt).
e2e_make_key() { # $1 id (8 lowercase hex)
  [ -f "$E2E_KD/id_$1" ] || ssh-keygen -q -t ed25519 -N '' -C "jodd-ai-$1" -f "$E2E_KD/id_$1"
  cat > "$E2E_KD/ssh-$1" <<EOS
#!/bin/sh
# shellcheck disable=SC2086
exec ssh -i '$E2E_KD/id_$1' -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes \\
  -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no \\
  -o StrictHostKeyChecking=yes -o UserKnownHostsFile='$E2E_KD/known_hosts' \\
  -o LogLevel=ERROR -o ConnectTimeout=10 \${E2E_SSH_EXTRA:-} -p '$E2E_PORT' jodd-ai@127.0.0.1 "\$@"
EOS
  chmod 700 "$E2E_KD/ssh-$1"
}

# Add the key's line to the account's authorized_keys, written AS THE ACCOUNT (root never writes
# in its home), unless a line for that id is already there. $3 is the forced command:
#   mcp     restrict,command="/usr/local/bin/jodd-ai-mcp --key <id>"   (what adopt-keys writes)
#   legacy  restrict,command="/home/jodd-ai/.jodd/ai-run-v2 --key <id>" (what Jodd's Setup wrote
#           before its MCP client: the wrapper-v2 line adopt-keys re-points)
e2e_add_key() { # $1 container, $2 id, $3 mcp|legacy
  e2e_make_key "$2"
  case $3 in
    mcp) _cmd="/usr/local/bin/jodd-ai-mcp --key $2" ;;
    legacy) _cmd="/home/jodd-ai/.jodd/ai-run-v2 --key $2" ;;
    *) return 2 ;;
  esac
  _line="restrict,command=\"$_cmd\" $(cut -d' ' -f1-2 "$E2E_KD/id_$2.pub") jodd-ai-$2"
  # shellcheck disable=SC2016  # the $ are for the container's shell
  printf '%s\n' "$_line" | docker exec -i -u jodd-ai -e HOME=/home/jodd-ai -e "ID=$2" "$1" sh -c '
    umask 077; mkdir -p "$HOME/.ssh"
    grep -qs " jodd-ai-$ID\$" "$HOME/.ssh/authorized_keys" && exit 0
    cat >> "$HOME/.ssh/authorized_keys"'
}

# Until a request with the key gets through sshd to jodd-ai-mcp (up to 60 s). A port that
# accepts TCP is not enough: Docker's proxy accepts before sshd listens.
e2e_wait_ssh() { # $1 id
  _i=0
  until "$E2E_KD/ssh-$1" 'jodd-ai-run 2 status' </dev/null >/dev/null 2>&1; do
    _i=$((_i + 1)); [ "$_i" -gt 60 ] && return 1
    sleep 1
  done
}

# An MCP session over ssh with key $1: `e2e_mcp <id> [mcp_client.py mcp options]`.
e2e_mcp() { _id=$1; shift; python3 -I "$E2E_CLIENT" mcp "$@" -- "$E2E_KD/ssh-$_id" jodd-ai-mcp; }
# A legacy request over ssh with key $1, framed as Jodd frames it:
#   e2e_legacy <id> '<jodd-ai-run 2 verb [preset]>' [--prompt TEXT] [--schema JSON]
e2e_legacy() { _id=$1 _req=$2; shift 2; python3 -I "$E2E_CLIENT" legacy "$@" -- "$E2E_KD/ssh-$_id" "$_req"; }

# What Jodd needs of a host after Setup, checked as a client: the password logs in (Setup), a
# restricted key reaches jodd-ai-mcp, the legacy status lists both harnesses, and an MCP
# session offers `infer` for both. Prints why and returns 1 on the first thing that fails.
e2e_client_checks() { # $1 container, $2 port, $3 password, $4 key id
  e2e_ssh_pw "$2" jodd-ai "$3" || { echo "password login failed for jodd-ai"; return 1; }
  e2e_pin_host "$1" "$2" || { echo "cannot read the host key"; return 1; }
  e2e_add_key "$1" "$4" mcp || { echo "cannot add the key line"; return 1; }
  e2e_wait_ssh "$4" || { echo "the restricted key never reached jodd-ai-mcp"; return 1; }
  _st=$(e2e_legacy "$4" 'jodd-ai-run 2 status') || { echo "legacy status failed"; return 1; }
  for _c in claude codex; do
    printf '%s\n' "$_st" | grep -Eq "^preset=$_c path=/" || { echo "legacy status: no path for $_c: $_st"; return 1; }
  done
  _m=$(e2e_mcp "$4") || { echo "MCP session failed: $_m"; return 1; }
  printf '%s\n' "$_m" | grep -qx 'tool infer v1 harnesses=claude,codex' || { echo "MCP: infer does not offer both: $_m"; return 1; }
  echo "ok: password login, restricted key, legacy status and MCP tools/list"
}

# A real, long-lived ssh session (password login through askpass) that runs `cat`, so the test
# can tell whether it is still alive later: it is, if a word sent now comes back.
# Used to prove that reloading sshd (lock / unlock / the installer's re-run) does not kill
# open sessions - before OpenSSH 9.8 a `pkill -HUP -x sshd` does.
e2e_session_start() { # $1 port, $2 user, $3 password
  E2E_SD=$(mktemp -d)
  mkfifo "$E2E_SD/in"; : > "$E2E_SD/out"
  # shellcheck disable=SC2016  # $E2E_PW is for the askpass script
  printf '#!/bin/sh\nprintf "%%s\\n" "$E2E_PW"\n' > "$E2E_SD/ask"; chmod 700 "$E2E_SD/ask"
  ( E2E_PW=$3 SSH_ASKPASS="$E2E_SD/ask" SSH_ASKPASS_REQUIRE=force DISPLAY=none exec ssh \
      -o PubkeyAuthentication=no -o PreferredAuthentications=password,keyboard-interactive \
      -o NumberOfPasswordPrompts=1 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      -o LogLevel=ERROR -o ConnectTimeout=10 -p "$1" "$2@127.0.0.1" cat \
      < "$E2E_SD/in" > "$E2E_SD/out" 2>/dev/null ) &
  E2E_SPID=$!
  exec 3> "$E2E_SD/in"
  e2e_session_ping hello
}
e2e_session_ping() { # $1 word: send it, wait up to 20 s for it to come back
  # in a subshell: writing to a dead session raises SIGPIPE, which must not end the whole driver
  ( printf '%s\n' "$1" >&3 ) 2>/dev/null || return 1
  _i=0
  until grep -qx "$1" "$E2E_SD/out" 2>/dev/null; do
    _i=$((_i + 1)); [ "$_i" -gt 40 ] && return 1
    kill -0 "$E2E_SPID" 2>/dev/null || return 1
    sleep 0.5
  done
}
e2e_session_end() {
  exec 3>&-
  wait "$E2E_SPID" 2>/dev/null || :
  rm -rf "$E2E_SD"
}
