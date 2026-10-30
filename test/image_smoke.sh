#!/bin/sh
# Builds the image and checks what it does without the CLIs or Jodd: jodd-ai-mcp is baked in
# root-owned and static, and the entrypoint sets up the policy (in the state volume, linked from
# /etc), the log directory and auto-enable - a `disable` surviving a restart. No network for the
# container (so the CLI installers fail at once). CI runs it; needs only Docker.
# Run: sh test/image_smoke.sh   (it removes everything it creates)
set -eu
repo=$(cd "$(dirname "$0")/.." && pwd)
tag=jodd-ai-host:smoke name=jodd-ai-smoke
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || :; docker volume rm -f "$name-home" "$name-state" >/dev/null 2>&1 || :; }
trap cleanup EXIT; cleanup
fail() { echo "FAIL: $*"; docker logs "$name" 2>&1 | tail -n 40; exit 1; }
wait_log() { # $1 pattern, $2 how many times it must have appeared, $3 tries (1 s each)
  i=0; until [ "$(docker logs "$name" 2>&1 | grep -Ec "$1")" -ge "$2" ]; do i=$((i+1)); [ "$i" -gt "$3" ] && fail "timed out waiting for: $1 (x$2)"; sleep 1; done
}
# Each start runs auto-enable twice: in the foreground, then after the background CLI install
# (which fails at once here). Wait for both, so nothing below races the second.
settled() { wait_log 'jodd-ai-host ready' "$1" 60; wait_log '^codex: ' "$((2 * $1))" 60; }
docker build -q -t "$tag" -f "$repo/docker/Dockerfile" "$repo" >/dev/null

# The binary: root:root 0755, runs, and is static (the musl build needs no loader).
got=$(docker run --rm --entrypoint sh "$tag" -c 'stat -c "%U:%G %a" /usr/local/bin/jodd-ai-mcp; /usr/local/bin/jodd-ai-mcp --version; ldd /usr/local/bin/jodd-ai-mcp 2>&1 || :')
echo "$got"
printf '%s\n' "$got" | grep -qx 'root:root 755' || fail "jodd-ai-mcp is not root:root 0755"
printf '%s\n' "$got" | grep -q '^jodd-ai-mcp [0-9]' || fail "jodd-ai-mcp --version"
printf '%s\n' "$got" | grep -Eq 'not a dynamic executable|statically linked' || fail "jodd-ai-mcp is not static"

# The entrypoint: an empty policy in the state volume, root's; /etc links to it; the log dir is
# the account's. A fake claude in the account's home volume is what auto-enable finds.
docker run -d --name "$name" --init --network none -e JODD_AI_PASSWORD=smoke-test-pw \
  -v "$name-home:/home/jodd-ai" -v "$name-state:/var/lib/jodd-ai-host" "$tag" >/dev/null
settled 1
got=$(docker exec "$name" sh -c 'stat -c "%n %U:%G %a" /var/lib/jodd-ai-host /var/lib/jodd-ai-host/policy.toml /var/log/jodd-ai-host; readlink /etc/jodd-ai-host/policy.toml')
echo "$got"
printf '%s\n' "$got" | grep -qx '/var/lib/jodd-ai-host root:root 755' || fail "state dir"
printf '%s\n' "$got" | grep -qx '/var/lib/jodd-ai-host/policy.toml root:root 644' || fail "policy file"
printf '%s\n' "$got" | grep -qx '/var/log/jodd-ai-host jodd-ai:jodd-ai 700' || fail "log dir"
printf '%s\n' "$got" | grep -qx '/var/lib/jodd-ai-host/policy.toml' || fail "/etc link"
docker exec "$name" su -s /bin/sh jodd-ai -c 'mkdir -p ~/.local/bin && printf "#!/bin/sh\necho fake\n" > ~/.local/bin/claude && chmod 755 ~/.local/bin/claude'
docker exec "$name" jodd-ai-host auto-enable
docker exec "$name" jodd-ai-host list | tee /dev/stderr | grep -qx 'claude  enabled  /home/jodd-ai/.local/bin/claude' || fail "auto-enable did not enable claude"
# disable writes THROUGH the /etc link (the link stays) and survives a restart's auto-enable
docker exec "$name" jodd-ai-host disable claude
docker restart "$name" >/dev/null
settled 2
docker exec "$name" jodd-ai-host list | grep -qx 'claude  disabled  /home/jodd-ai/.local/bin/claude' || fail "disable did not survive a restart"
docker exec "$name" sh -c 'test -L /etc/jodd-ai-host/policy.toml' || fail "the /etc link was replaced by a file"
docker exec "$name" su -s /bin/sh jodd-ai -c 'test ! -w /var/lib/jodd-ai-host/policy.toml && test ! -w /var/lib/jodd-ai-host' || fail "jodd-ai can change the policy"
docker exec "$name" jodd-ai-host doctor || :
# A login profile that hangs (it is the account's to edit) must not keep sshd from starting:
# the entrypoint's probes through the login shell are bounded.
docker exec "$name" su -s /bin/sh jodd-ai -c 'echo "sleep 600" >> ~/.profile'
docker restart "$name" >/dev/null
wait_log 'jodd-ai-host ready' 3 90
docker logs "$name" 2>&1 | grep -q 'took over 20 s' || fail "no timeout message for the hanging profile"
docker exec "$name" sh -c 'pgrep -x sshd >/dev/null' || fail "sshd is not running after a hanging profile"
echo "image smoke: ok"
