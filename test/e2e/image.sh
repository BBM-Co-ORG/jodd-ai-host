#!/bin/sh
# End to end, over real OpenSSH, against images built from this repository. Needs Docker and
# python3 (the client is test/e2e/mcp_client.py, stdlib only); CI runs it (the e2e job).
#
# Part A — jodd-ai-host-test (docker/Dockerfile --target test: the host plus fake claude / codex).
#   A Jodd-style key line, `restrict,command="/usr/local/bin/jodd-ai-mcp --key <id>"`, in the
#   account's authorized_keys; every request goes through sshd as a client sends it:
#     e2e_mcp_infer_over_forced_command        MCP over `ssh … jodd-ai-mcp`
#     e2e_legacy_v2_client_against_new_host    `jodd-ai-run 2 …` with plain ssh, framed as Jodd does
#     e2e_adopt_keys_then_both_protocols_work  a wrapper-v2 line, adopt-keys, then both protocols
#     e2e_restricted_key_gets_no_shell         no command but ours, no pty, no forwarding
#     e2e_cancel_by_hangup_kills_cli_within_6s the client vanishes mid-run (MCP and legacy)
#     e2e_policy_survives_image_update         disable codex, recreate the container, still disabled
# Part B — the host image (the default target): first start without network, lock / unlock against
#   a running sshd, recreate, password rules, a broken drop-in, a planted link. The CLIs are fake
#   ones planted in the home volume (so nothing is downloaded); E2E_REAL_CLIS=1 installs the real
#   ones from their official installers instead (needs network; local runs).
# shellcheck disable=SC1091,SC2154,SC2015  # lib.sh sets $repo; `A && B || fail` fails when either does, as meant
set -eu
. "$(dirname "$0")/lib.sh"
ver=$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/crates/jodd-ai-mcp/Cargo.toml" | head -n 1)
state=/var/lib/jodd-ai-host
tname=jodd-ai-e2e-mcp tport=2226 timg=jodd-ai-host-test:e2e
name=jodd-ai-e2e-image port=2224 pw=e2e-image-pw img=jodd-ai-host:e2e
cleanup_t() { docker rm -f "$tname" >/dev/null 2>&1 || :; docker volume rm -f "$tname-home" "$tname-state" >/dev/null 2>&1 || :; }
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || :; docker volume rm -f "$name-home" "$name-state" >/dev/null 2>&1 || :; }
trap 'cleanup_t; cleanup; e2e_keys_cleanup' EXIT
cleanup_t; cleanup

echo "== building $img and $timg (jodd-ai-mcp $ver, compiled in the image)"
docker build -q -t "$img" -f "$repo/docker/Dockerfile" "$repo" >/dev/null
docker build -q -t "$timg" --target test -f "$repo/docker/Dockerfile" "$repo" >/dev/null

# ================================ Part A ================================
K1=e2e00001 K2=e2e00002
export JODD_AI_PASSWORD=e2e-mcp-pw
tfail() { echo "FAIL [$tc]: $*"; docker logs "$tname" 2>&1 | tail -n 30; exit 1; }
trun() {
  docker run -d --name "$tname" --init -p "127.0.0.1:$tport:22" -e JODD_AI_PASSWORD -e JODD_AI_PORT="$tport" \
    -v "$tname-home:/home/jodd-ai" -v "$tname-state:$state" "$timg" >/dev/null
  i=0; until docker logs "$tname" 2>&1 | grep -q 'jodd-ai-host ready'; do
    i=$((i+1)); [ "$i" -gt 60 ] && tfail "the container never got ready"; sleep 1; done
}
tx() { docker exec "$tname" sh -c "$1"; }
# $1 file with the client's output, $2 a line it must hold exactly
want() { grep -qxF -- "$2" "$1" || { echo "--- got:"; cat "$1"; tfail "no line: $2"; }; }
out=$E2E_KD/out err=$E2E_KD/err

tc=setup
trun
e2e_pin_host "$tname" "$tport"
e2e_add_key "$tname" "$K1" mcp
e2e_wait_ssh "$K1" || tfail "the key never reached jodd-ai-mcp"
tx 'jodd-ai-host list'

# ---------------------------------------------------------------------------------------------
tc=e2e_mcp_infer_over_forced_command
echo "== $tc"
e2e_mcp "$K1" \
  --call infer '{"harness":"claude","prompt":"e2e-claude-marker"}' \
  --call infer '{"harness":"codex","prompt":"e2e-codex-marker","system":"be brief","timeout_secs":30}' \
  --call infer '{"harness":"codex","prompt":"x","temperature":1}' \
  --call infer '{"harness":"pi","prompt":"x"}' > "$out"
cat "$out"
want "$out" "server jodd-ai-host $ver"
want "$out" 'tool infer v1 harnesses=claude,codex'
want "$out" 'tool forget_key v1'
want "$out" 'call infer ok "e2e-claude-marker" {"exit_code": 0, "harness": "claude"}'
# system text goes ahead of the prompt on the CLI's stdin; the fake codex answers with its stdin
want "$out" 'call infer ok "be brief\n\n---\n\ne2e-codex-marker" {"exit_code": 0, "harness": "codex"}'
grep -q '^call infer rpc-error -32602 "unknown argument .temperature.' "$out" || tfail "an unknown argument was not refused as invalid params"
grep -q '^call infer error not_enabled ' "$out" || tfail "an unknown harness is not not_enabled"
want "$out" 'exit 0'
# MCP negotiation: a version the server supports is echoed; one it does not know gets its own
e2e_mcp "$K1" --protocol 2025-03-26 > "$out"; want "$out" 'protocol 2025-03-26'
e2e_mcp "$K1" --protocol 1999-01-01 > "$out"
grep -q '^protocol 1999-01-01$' "$out" && tfail "the server echoed a protocol version it does not support"
grep -Eq '^protocol 20[0-9]{2}-[0-9]{2}-[0-9]{2}$' "$out" || tfail "no protocol version in the answer"
# the request must be exactly the word jodd-ai-mcp
rc=0; "$E2E_KD/ssh-$K1" 'jodd-ai-mcp extra' </dev/null >/dev/null 2>"$err" || rc=$?
[ "$rc" = 64 ] && grep -qx 'unknown request' "$err" || tfail "'jodd-ai-mcp extra': rc $rc, $(cat "$err")"
# one metadata line per call in the log, and never the prompt
log=$(tx 'cat /var/log/jodd-ai-host/calls.log')
printf '%s\n' "$log" | grep -Eq "key=$K1 tool=infer harness=claude dur_ms=[0-9]+ outcome=ok\$" || { echo "$log"; tfail "no log line for the claude call"; }
printf '%s\n' "$log" | grep -Eq "key=$K1 tool=infer harness=codex dur_ms=[0-9]+ outcome=ok\$" || { echo "$log"; tfail "no log line for the codex call"; }
tx 'grep -rq e2e-claude-marker /var/log/jodd-ai-host' && tfail "the prompt reached the log"
echo "ok: MCP over the forced command (both harnesses, invalid params, not_enabled, negotiation, log)"

# ---------------------------------------------------------------------------------------------
tc=e2e_legacy_v2_client_against_new_host
echo "== $tc"
rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 status' > "$out" || rc=$?
printf 'version=2\npreset=claude path=/usr/local/bin/claude\npreset=codex path=/usr/local/bin/codex\n' > "$E2E_KD/want"
[ "$rc" = 0 ] && cmp -s "$E2E_KD/want" "$out" || { cat "$out"; tfail "status: rc $rc, not wrapper v2's output"; }
# a prompt with non-ASCII text, newlines and a line that is just "-": the length is in bytes
p="สวัสดี e2e legacy
-
end"
rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 run codex' --prompt "$p" > "$out" 2> "$err" || rc=$?
printf '%s' "$p" > "$E2E_KD/want"
[ "$rc" = 0 ] || tfail "run codex: rc $rc ($(cat "$err"))"
cmp -s "$E2E_KD/want" "$out" || { od -c "$out" | head; tfail "run codex: stdout is not the answer, byte for byte"; }
[ "$(cat "$err")" = 'codex stderr line' ] || tfail "run codex: stderr '$(cat "$err")'"
rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 run codex' --prompt 'with schema' --schema '{"type":"object"}' > "$out" || rc=$?
[ "$rc" = 0 ] && [ "$(cat "$out")" = 'with schema' ] || tfail "run codex with a schema: rc $rc, '$(cat "$out")'"
# claude's answer goes back raw (the JSON envelope): old Jodd unwraps it itself
rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 run claude' --prompt 'e2e-legacy-claude' > "$out" || rc=$?
[ "$rc" = 0 ] && grep -q '^{"result":"e2e-legacy-claude",' "$out" || tfail "run claude: rc $rc, '$(cat "$out")'"
rc=0; e2e_legacy "$K1" 'jodd-ai-run 3 run codex' --prompt x > "$out" 2> "$err" || rc=$?
[ "$rc" = 65 ] && [ "$(cat "$err")" = 'version mismatch: this server has 2, Jodd sent 3' ] || tfail "version 3: rc $rc, '$(cat "$err")'"
rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 run custom' --prompt x > "$out" 2> "$err" || rc=$?
[ "$rc" = 64 ] && [ "$(cat "$err")" = 'unknown request' ] || tfail "preset custom: rc $rc, '$(cat "$err")'"
echo "ok: wrapper v2's status, run (both presets, schema), version mismatch and unknown preset"

# ---------------------------------------------------------------------------------------------
tc=e2e_adopt_keys_then_both_protocols_work
echo "== $tc"
e2e_add_key "$tname" "$K2" legacy
ak() { docker exec -u jodd-ai "$tname" cat /home/jodd-ai/.ssh/authorized_keys; }
k1_line=$(ak | grep " jodd-ai-$K1\$")
# before: the line runs the old wrapper, which is not there (as on a host whose kit was replaced)
rc=0; e2e_legacy "$K2" 'jodd-ai-run 2 status' >/dev/null 2>&1 || rc=$?
[ "$rc" != 0 ] || tfail "the wrapper-v2 line answered before adopt-keys"
tx 'jodd-ai-host status' | grep -q "key jodd-ai-$K2: not on jodd-ai-mcp" || tfail "status does not name the legacy line"
tx 'jodd-ai-host adopt-keys'
ak | grep " jodd-ai-$K2\$" | grep -q "^restrict,command=\"/usr/local/bin/jodd-ai-mcp --key $K2\" ssh-ed25519 " \
  || { ak; tfail "adopt-keys did not re-point the line"; }
[ "$(ak | grep " jodd-ai-$K1\$")" = "$k1_line" ] || tfail "adopt-keys changed another key's line"
tx 'jodd-ai-host status' | grep -q 'not on jodd-ai-mcp' && tfail "status still names a legacy line"
rc=0; e2e_legacy "$K2" 'jodd-ai-run 2 run codex' --prompt 'adopted-legacy' > "$out" || rc=$?
[ "$rc" = 0 ] && [ "$(cat "$out")" = adopted-legacy ] || tfail "legacy run with the adopted key: rc $rc, '$(cat "$out")'"
e2e_mcp "$K2" --call infer '{"harness":"codex","prompt":"adopted-mcp"}' --call forget_key '{}' > "$out"
cat "$out"
want "$out" 'call infer ok "adopted-mcp" {"exit_code": 0, "harness": "codex"}'
want "$out" 'call forget_key ok "this device'"'"'s key was removed from the AI host" {"removed": true}'
want "$out" 'exit 0'
ak | grep -q " jodd-ai-$K2\$" && tfail "forget_key left the line"
rc=0; e2e_legacy "$K2" 'jodd-ai-run 2 status' >/dev/null 2>&1 || rc=$?
[ "$rc" = 255 ] || tfail "the forgotten key still logs in (rc $rc)"
[ "$(ak | grep " jodd-ai-$K1\$")" = "$k1_line" ] || tfail "forget_key changed another key's line"
e2e_legacy "$K1" 'jodd-ai-run 2 status' >/dev/null || tfail "the other key stopped working"
echo "ok: adopt-keys re-pointed the wrapper-v2 line; legacy and MCP both work with it; forget_key removed only it"

# ---------------------------------------------------------------------------------------------
tc=e2e_restricted_key_gets_no_shell
echo "== $tc"
for req in 'sh -c id' 'id' 'jodd-ai-mcp; id' 'jodd-ai-mcp admin list' 'admin list' 'admin disable codex' '/bin/sh'; do
  rc=0; "$E2E_KD/ssh-$K1" "$req" </dev/null > "$out" 2> "$err" || rc=$?
  [ "$rc" = 64 ] && [ "$(cat "$err")" = 'unknown request' ] && [ ! -s "$out" ] \
    || tfail "'$req': rc $rc, stdout '$(cat "$out")', stderr '$(cat "$err")'"
done
tx 'jodd-ai-host list' | grep -qx 'codex  enabled  /usr/local/bin/codex' || tfail "an admin request through the key changed the policy"
# a terminal is refused (restrict = no-pty; a client that asked for one with -t then gives up)
rc=0; E2E_SSH_EXTRA=-tt "$E2E_KD/ssh-$K1" 'jodd-ai-run 2 status' </dev/null > "$out" 2> "$err" || rc=$?
grep -q 'PTY allocation request failed' "$err" || tfail "-tt: no pty refusal (rc $rc, stderr '$(cat "$err")')"
# no forwarding (restrict = no-port-forwarding): sshd refuses ssh -W, and says so in its log
rc=0; E2E_SSH_EXTRA='-W 127.0.0.1:22' "$E2E_KD/ssh-$K1" </dev/null > "$out" 2> "$err" || rc=$?
[ "$rc" = 255 ] && grep -q 'stdio forwarding failed' "$err" || tfail "-W: rc $rc, stderr '$(cat "$err")'"
docker logs "$tname" 2>&1 | grep -q 'refused local port forward' || tfail "-W: sshd did not log the refused forward"
echo "ok: every other command gets 64, no terminal, no forwarding"

# ---------------------------------------------------------------------------------------------
tc=e2e_cancel_by_hangup_kills_cli_within_6s
echo "== $tc"
# The fake codex, given the prompt `stubborn`, ignores TERM and sleeps; it records the pid of that
# sleep in /tmp/fake.pid. The client then vanishes (its ssh is KILLed): the CLI's process group
# must be gone (TERM, then KILL 5 s later) and its scratch dir removed within 6 s.
started() { # until the fake records its pid (up to 20 s); prints it
  i=0; until tx 'test -s /tmp/fake.pid'; do i=$((i+1)); [ "$i" -gt 100 ] && return 1; sleep 0.2; done
  tx 'cat /tmp/fake.pid'
}
# $1 pid in the container, $2 ms: until it is gone (or a zombie) and no scratch is left. One shell
# loop inside the container, timed by the container's clock from just after the hangup, so a
# `docker exec` per poll adds no detection lag. Prints the elapsed ms (or what is left).
gone_within() {
  # shellcheck disable=SC2016  # the $ are for the container's shell
  tx 'end=$(( $(date +%s%3N) + '"$2"' )); t0=$(date +%s%3N)
    while :; do
      st=$(ps -o stat= -p '"$1"' 2>/dev/null | tr -d " ")
      sc=$(ls -d /tmp/jodd-ai.* 2>/dev/null)
      case $st in ""|Z*) [ -z "$sc" ] && { echo "$(( $(date +%s%3N) - t0 ))"; exit 0; } ;; esac
      [ "$(date +%s%3N)" -lt "$end" ] || { echo "still there: pid '"$1"' [$st], scratch [$sc]"; exit 1; }
      sleep 0.05
    done'
}
hangup_check() { # $1 local ssh pid, $2 what
  fpid=$(started) || tfail "$2: the fake CLI never started"
  tx "ps -o stat= -p $fpid" >/dev/null || tfail "$2: the CLI is not running before the hangup"
  [ -n "$(tx 'ls -d /tmp/jodd-ai.* 2>/dev/null' || :)" ] || tfail "$2: no scratch dir while the CLI runs"
  kill -9 "$1"
  ms=$(gone_within "$fpid" 6000) || tfail "$2: the CLI or its scratch outlived the hangup by 6 s ($ms)"
  echo "ok: $2: CLI and scratch gone $ms ms after the client vanished"
}
# MCP
tx 'rm -f /tmp/fake.pid'
e2e_mcp "$K1" --call infer '{"harness":"codex","prompt":"stubborn"}' > "$out" 2>/dev/null &
cpid=$!
i=0; until grep -q '^listed$' "$out"; do i=$((i+1)); [ "$i" -gt 100 ] && tfail "the MCP session did not start"; sleep 0.2; done
hangup_check "$(sed -n 's/^pid //p' "$out")" "MCP"
wait "$cpid" || :
# legacy: Jodd's framing, stdin left open (as Jodd leaves it) until ssh is KILLed
tx 'rm -f /tmp/fake.pid'
{ printf -- '-\n8\nstubborn'; sleep 20; } | "$E2E_KD/ssh-$K1" 'jodd-ai-run 2 run codex' >/dev/null 2>&1 &
hangup_check "$!" "legacy"
# a slot came back: the next call runs
e2e_mcp "$K1" --call infer '{"harness":"codex","prompt":"after-cancel"}' > "$out"
want "$out" 'call infer ok "after-cancel" {"exit_code": 0, "harness": "codex"}'

# ---------------------------------------------------------------------------------------------
tc=e2e_policy_survives_image_update
echo "== $tc"
fp1=$(tx 'jodd-ai-host fingerprint')
# A live session listed both harnesses; codex is disabled before its call: the call obeys the
# policy as it is now, and says how to enable it.
hold=$E2E_KD/hold; rm -f "$hold"
e2e_mcp "$K1" --hold "$hold" --call infer '{"harness":"codex","prompt":"x"}' > "$out" &
cpid=$!
i=0; until grep -q '^listed$' "$out"; do i=$((i+1)); [ "$i" -gt 100 ] && tfail "the MCP session did not start"; sleep 0.2; done
want "$out" 'tool infer v1 harnesses=claude,codex'
tx 'jodd-ai-host disable codex'
touch "$hold"; wait "$cpid" || tfail "the held MCP session failed"
cat "$out"
grep -q '^call infer error not_enabled .*sudo jodd-ai-host enable codex' "$out" || tfail "a call after disable was not not_enabled"
policy_checks() { # $1 when
  tx 'jodd-ai-host list' | grep -qx 'codex  disabled  /usr/local/bin/codex' || tfail "$1: codex is not disabled in the policy"
  tx 'jodd-ai-host list' | grep -qx 'claude  enabled  /usr/local/bin/claude' || tfail "$1: claude is not enabled"
  e2e_mcp "$K1" --call infer '{"harness":"codex","prompt":"x"}' > "$out"
  want "$out" 'tool infer v1 harnesses=claude'
  grep -q '^call infer error not_enabled ' "$out" || tfail "$1: infer codex is not not_enabled"
  rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 status' > "$out" || rc=$?
  want "$out" 'preset=codex path=-'
  want "$out" 'preset=claude path=/usr/local/bin/claude'
  rc=0; e2e_legacy "$K1" 'jodd-ai-run 2 run codex' --prompt x > "$out" 2> "$err" || rc=$?
  [ "$rc" = 64 ] && [ "$(cat "$err")" = 'unknown request' ] || tfail "$1: legacy run codex: rc $rc, '$(cat "$err")'"
}
policy_checks "after disable"
# an image update: the container is recreated from the same two volumes (`docker compose pull`
# then `up -d` does exactly this); the entrypoint's auto-enable must respect the decision
docker rm -f "$tname" >/dev/null
trun
e2e_wait_ssh "$K1" || tfail "the key stopped working after the recreate"
[ "$(tx 'jodd-ai-host fingerprint')" = "$fp1" ] || tfail "the fingerprint changed"
tx 'test -L /etc/jodd-ai-host/policy.toml' || tfail "/etc/jodd-ai-host/policy.toml is not the link to the state volume"
policy_checks "after the recreate"
echo "ok: codex stays disabled across a recreate; the key and the fingerprint are kept"
cleanup_t
echo "part A (MCP and legacy over real sshd): OK"

# ================================ Part B ================================
# run [extra docker args]: --network none cannot publish a port, so it is added only without it.
run() {
  case " $* " in *' --network none '*) pub= ;; *) pub="-p 127.0.0.1:$port:22" ;; esac
  # The password goes in by inheritance (-e NAME), so it is neither on a command line nor echoed;
  # unset JODD_AI_PASSWORD (pw empty) means the image generates one.
  if [ -n "$pw" ]; then export JODD_AI_PASSWORD="$pw"; else unset JODD_AI_PASSWORD; fi
  # shellcheck disable=SC2086
  docker run -d --name "$name" --init $pub -e JODD_AI_PASSWORD -e JODD_AI_PORT="$port" \
    -v "$name-home:/home/jodd-ai" -v "$name-state:$state" "$@" "$img" >/dev/null
}
fail() { echo "FAIL: $*"; docker logs "$name" 2>&1 | tail -n 40; exit 1; }
wait_log() { # $1 pattern, $2 tries (2 s each)
  i=0; until docker logs "$name" 2>&1 | grep -Eq "$1"; do i=$((i+1)); [ "$i" -gt "$2" ] && fail "timed out waiting for log line: $1"; sleep 2; done
}
# The CLIs, before a start that would install them: fakes in the home volume's ~/.local/bin, where
# the official installers put them (written there by a throwaway container, as the account owns
# them). With E2E_REAL_CLIS=1 nothing is planted, and the start installs the real ones.
plant_clis() {
  [ "${E2E_REAL_CLIS:-}" = 1 ] && return 0
  docker run --rm --entrypoint sh -v "$name-home:/home/jodd-ai" -v "$repo/test/docker:/fakes:ro" "$img" -c '
    install -d -o jodd-ai -g jodd-ai -m 755 /home/jodd-ai/.local /home/jodd-ai/.local/bin
    install -o jodd-ai -g jodd-ai -m 755 /fakes/fake-claude /home/jodd-ai/.local/bin/claude
    install -o jodd-ai -g jodd-ai -m 755 /fakes/fake-codex /home/jodd-ai/.local/bin/codex'
}
clis_ready() { # and enabled in the policy (after a real install, the background job enables them)
  if [ "${E2E_REAL_CLIS:-}" = 1 ]; then
    wait_log 'CLIs installed|could not install the CLIs' 300
    docker logs "$name" 2>&1 | grep -q 'CLIs installed' || fail "the CLIs did not install"
  fi
  i=0; until [ "$(docker exec "$name" jodd-ai-host list 2>/dev/null | grep -Ec '^(claude|codex)  enabled  /')" = 2 ]; do
    i=$((i+1)); [ "$i" -gt 30 ] && fail "the CLIs were not enabled in the policy"; sleep 2; done
}
checks() { # $1 password: what Jodd needs of the host, as a client
  e2e_client_checks "$name" "$port" "$1" e2e0000b || fail "client checks"
}
# What sshd applies to the AI account: `sshd -T -C user=…` evaluates the config files on disk
# (Include + Match) for that connection. The reload itself is checked separately (SIGHUP in the log).
pw_auth() { docker exec "$name" sh -c '/usr/sbin/sshd -T -C user=jodd-ai,host=localhost,addr=127.0.0.1 2>/dev/null | grep -i "^passwordauthentication"'; }
shadow() { docker exec "$name" sh -c "grep '^jodd-ai:' /etc/shadow | cut -d: -f2"; }
sighups() { docker logs "$name" 2>&1 | grep -c 'Received SIGHUP' || :; }

# 1. First start with no network: sshd must still come up and say how to retry.
echo "== B1. first start, no network"
run --network none
wait_log 'jodd-ai-host ready' 30
wait_log 'could not install the CLIs' 60
docker logs "$name" 2>&1 | grep -q 'jodd-ai-host update-clis' || fail "no retry hint without network"
docker exec "$name" sh -c 'pgrep -x sshd >/dev/null' || fail "sshd is not running without network"
echo "ok: sshd up, retry hint printed"
docker rm -f "$name" >/dev/null

# 2. Normal start: the CLIs are found (planted, or installed on this start) and enabled.
echo "== B2. normal start"
plant_clis
run
wait_log 'jodd-ai-host ready' 30
clis_ready
docker exec "$name" jodd-ai-host status | grep -q '^codex: */' || fail "codex not found by status"
docker exec "$name" jodd-ai-host status
docker exec "$name" jodd-ai-host list | grep -Eq '^codex  enabled  /' || fail "codex was not enabled at start"
fp1=$(docker exec "$name" jodd-ai-host fingerprint)
checks "$pw"

# 3. Lock: the running sshd must change (HUP reload), and the symlink must still point at the state file.
echo "== B3. lock / unlock against the running sshd"
[ "$(pw_auth)" = 'passwordauthentication yes' ] || fail "unlocked: sshd -T says '$(pw_auth)'"
h0=$(sighups)
# An open ssh session must survive the reload (before OpenSSH 9.8 `pkill -HUP -x sshd` kills them all).
e2e_session_start "$port" jodd-ai "$pw" || fail "could not open a test ssh session"
docker exec "$name" jodd-ai-host lock
[ "$(pw_auth)" = 'passwordauthentication no' ] || fail "locked: sshd -T says '$(pw_auth)'"
sleep 1; [ "$(sighups)" -gt "$h0" ] || fail "sshd did not log a SIGHUP reload after lock"
[ "$(docker exec "$name" readlink /etc/ssh/sshd_config.d/50-jodd-ai.conf)" = "$state/50-jodd-ai.conf" ] || fail "drop-in symlink moved"
docker exec "$name" grep -q 'PasswordAuthentication no' /etc/ssh/sshd_config.d/50-jodd-ai.conf || fail "symlink does not show the locked state"
e2e_session_ping alive || { e2e_session_end; fail "an open ssh session died when lock reloaded sshd"; }
e2e_session_end
e2e_ssh_pw "$port" jodd-ai "$pw" && fail "password login works while locked"
e2e_legacy e2e0000b 'jodd-ai-run 2 status' >/dev/null || fail "the key stopped working when password login was locked"
echo "ok: locked (sshd -T: $(pw_auth)), the open ssh session survived, SIGHUP logged, symlink -> $state/50-jodd-ai.conf, the key still works"

# 4. Recreate (same volumes): same fingerprint, CLIs still there, no reinstall, still locked.
echo "== B4. recreate"
docker rm -f "$name" >/dev/null; run
e2e_wait_ssh e2e0000b || fail "sshd did not come back after recreate"
fp2=$(docker exec "$name" jodd-ai-host fingerprint)
[ "$fp1" = "$fp2" ] || fail "fingerprint changed: $fp1 -> $fp2"
docker exec "$name" jodd-ai-host status | grep -q '^claude: */' || fail "claude lost on recreate"
docker exec "$name" jodd-ai-host status | grep -q '^codex: */' || fail "codex lost on recreate"
docker logs "$name" 2>&1 | grep -q 'installing claude and codex' && fail "recreate reinstalled the CLIs"
docker exec "$name" jodd-ai-host status | grep -q '^password: *off' || fail "lock did not survive the recreate"
[ "$(pw_auth)" = 'passwordauthentication no' ] || fail "locked after recreate: sshd -T says '$(pw_auth)'"
docker exec "$name" jodd-ai-host unlock
[ "$(pw_auth)" = 'passwordauthentication yes' ] || fail "unlocked: sshd -T says '$(pw_auth)'"
[ "$(docker exec "$name" readlink /etc/ssh/sshd_config.d/50-jodd-ai.conf)" = "$state/50-jodd-ai.conf" ] || fail "drop-in symlink moved after unlock"
echo "ok: fingerprint $fp2 kept, CLIs kept, lock survived, unlock works (sshd -T: $(pw_auth))"

# 5. A password changed inside the container survives a restart; a deliberate env change is applied.
echo "== B5. password survives restart"
newpw=e2e-image-changed-pw
docker exec "$name" sh -c "printf 'jodd-ai:%s\n' '$newpw' | chpasswd"
sh1=$(shadow)
docker restart "$name" >/dev/null
e2e_wait_ssh e2e0000b || fail "sshd did not come back after restart"
[ "$(shadow)" = "$sh1" ] || fail "restart changed the password hash (a changed password was reverted)"
docker exec "$name" sh -c 'test ! -e /var/lib/jodd-ai-host/initial-password' || fail "plain-text initial-password is in the state volume"
checks "$newpw"
pw=e2e-image-env-changed-pw
docker rm -f "$name" >/dev/null; run
e2e_wait_ssh e2e0000b || fail "sshd did not come back after recreate with a new env password"
checks "$pw"
echo "ok: restart kept the changed password; a new JODD_AI_PASSWORD is applied"

# 6. No JODD_AI_PASSWORD: a password is generated once, shown once, and not stored in plain text.
echo "== B6. generated password"
cleanup; pw=
plant_clis
run
wait_log 'jodd-ai-host ready' 30
clis_ready
gen=$(docker logs "$name" 2>&1 | sed -n 's/.*generated a password for jodd-ai: \([^ ]*\) .*/\1/p')
[ -n "$gen" ] || fail "no generated password in the log"
docker exec "$name" sh -c 'test ! -e /var/lib/jodd-ai-host/initial-password' || fail "plain-text initial-password is in the state volume"
checks "$gen"
sh1=$(shadow)
docker restart "$name" >/dev/null
e2e_wait_ssh e2e0000b || fail "sshd did not come back after restart"
[ "$(shadow)" = "$sh1" ] || fail "restart regenerated the password"
[ "$(docker logs "$name" 2>&1 | grep -c 'generated a password')" = 1 ] || fail "the generated password was shown more than once"
echo "ok: generated password works, kept across restart, shown once"

# 7. A line break in JODD_AI_PASSWORD is refused with a message; sshd still starts.
echo "== B7. password with a line break"
cleanup; pw=$(printf 'line1\nline2')
run --network none
wait_log 'jodd-ai-host ready' 30
docker logs "$name" 2>&1 | grep -q 'contains a line break' || fail "no line-break message"
docker exec "$name" sh -c 'pgrep -x sshd >/dev/null' || fail "sshd is not running"
echo "ok: refused with a message, sshd up"

# 8. A broken drop-in stops the container with a message that names the file.
echo "== B8. broken drop-in"
docker rm -f "$name" >/dev/null
docker run --rm --entrypoint sh -v "$name-state:$state" "$img" -c "echo 'NotAnSshdKeyword yes' > $state/50-jodd-ai.conf"
run --network none
i=0; while [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ]; do i=$((i+1)); [ "$i" -gt 30 ] && fail "container kept running with a broken drop-in"; sleep 1; done
[ "$(docker inspect -f '{{.State.ExitCode}}' "$name")" != 0 ] || fail "exit code 0 with a broken drop-in"
docker logs "$name" 2>&1 | grep -q "$state/50-jodd-ai.conf" || fail "message does not name the drop-in"
echo "ok: stopped with a message naming $state/50-jodd-ai.conf"

# 9. The entrypoint repairs the account's login PATH as the account: a ~/.profile that is a link to a
#    root-owned file (planted by the account) is not written through.
echo "== B9. link in the account's home"
cleanup; pw=e2e-image-pw
docker run --rm --entrypoint sh -v "$name-home:/home/jodd-ai" -v "$name-state:$state" "$img" -c \
  "printf 'orig\n' > $state/e2e-target && chmod 600 $state/e2e-target && ln -sf $state/e2e-target /home/jodd-ai/.profile"
run --network none
wait_log 'jodd-ai-host ready' 30
docker logs "$name" 2>&1 | grep -q 'could not add ~/.local/bin' || fail "no warning about the linked ~/.profile"
[ "$(docker exec "$name" sh -c "cat $state/e2e-target; stat -c '%U %a' $state/e2e-target")" = "orig
root 600" ] || fail "the entrypoint wrote through the link in the account's home (target changed, or its owner/mode)"
echo "ok: root did not write through a link in the account's home"
echo "image e2e: OK"
