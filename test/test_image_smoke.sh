#!/bin/sh
# Checks a built jodd-ai-host-test image (docker/Dockerfile, --target test): what client CI
# (Jodd's) relies on when it pins it. The fake CLIs are enabled at start, without the real ones
# being installed, and jodd-ai-mcp, run the way sshd runs a Jodd key's forced command, answers
# MCP initialize and tools/list (infer offering both harnesses) and an infer through each fake.
# No network for the container. The release workflow runs it before anything is pushed.
# Run: sh test/test_image_smoke.sh <image>   (it removes the container it creates)
set -eu
img=${1:?usage: sh test/test_image_smoke.sh <image>}
name=jodd-ai-test-smoke
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || :; }
trap cleanup EXIT; cleanup
fail() { echo "FAIL: $*"; docker logs "$name" 2>&1 | tail -n 40; exit 1; }

docker run -d --name "$name" --init --network none -e JODD_AI_PASSWORD=smoke-test-pw "$img" >/dev/null
i=0
until docker logs "$name" 2>&1 | grep -q 'jodd-ai-host ready'; do
  i=$((i+1)); [ "$i" -gt 90 ] && fail "the container never got ready"; sleep 1
done
# Both fakes are on the account's PATH, so the first-start install of the real CLIs never runs.
docker logs "$name" 2>&1 | grep -q 'installing claude and codex' && fail "the test image started installing the real CLIs"
list=$(docker exec "$name" jodd-ai-host list)
echo "$list"
printf '%s\n' "$list" | grep -qx 'claude  enabled  /usr/local/bin/claude' || fail "the fake claude is not enabled"
printf '%s\n' "$list" | grep -qx 'codex  enabled  /usr/local/bin/codex' || fail "the fake codex is not enabled"

# jodd-ai-mcp as sshd runs it for a Jodd key: as the account, with the client's request in
# SSH_ORIGINAL_COMMAND. stdin stays open until every answer is in (EOF cancels a running call).
python3 - "$name" <<'PY' || fail "the MCP exchange"
import json, signal, subprocess, sys

signal.alarm(120)
name = sys.argv[1]
p = subprocess.Popen(
    ["docker", "exec", "-i", "-u", "jodd-ai", "-w", "/home/jodd-ai",
     "-e", "HOME=/home/jodd-ai", "-e", "SSH_ORIGINAL_COMMAND=jodd-ai-mcp",
     name, "/usr/local/bin/jodd-ai-mcp", "--key", "smoke"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)

def send(msg):
    p.stdin.write(json.dumps(msg) + "\n")
    p.stdin.flush()

def answer(req_id):
    while True:
        line = p.stdout.readline()
        if not line:
            sys.exit("jodd-ai-mcp closed its output before answering request %d" % req_id)
        msg = json.loads(line)
        if msg.get("id") == req_id:
            if "result" not in msg:
                sys.exit("request %d failed: %s" % (req_id, line.strip()))
            return msg["result"]

send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "protocolVersion": "2025-06-18", "capabilities": {},
    "clientInfo": {"name": "test-image-smoke", "version": "1"}}})
init = answer(1)
print("initialize:", json.dumps(init["serverInfo"]), init["protocolVersion"])
send({"jsonrpc": "2.0", "method": "notifications/initialized"})

send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
tools = {t["name"]: t for t in answer(2)["tools"]}
print("tools/list:", sorted(tools))
if "infer" not in tools:
    sys.exit("tools/list has no infer")
harnesses = tools["infer"]["inputSchema"]["properties"]["harness"]["enum"]
print("infer harnesses:", harnesses)
if sorted(harnesses) != ["claude", "codex"]:
    sys.exit("infer does not offer both harnesses: %r" % harnesses)

prompt = "ping from the test image smoke"
for req_id, harness in ((3, "claude"), (4, "codex")):
    send({"jsonrpc": "2.0", "id": req_id, "method": "tools/call",
          "params": {"name": "infer", "arguments": {"harness": harness, "prompt": prompt}}})
    res = answer(req_id)
    text = res["content"][0]["text"] if res.get("content") else None
    print("infer %s:" % harness, json.dumps(res))
    if res.get("isError") or text != prompt:
        sys.exit("infer through the fake %s did not answer the prompt" % harness)

p.stdin.close()
code = p.wait(timeout=30)
print("jodd-ai-mcp exited", code, "at end of input")
if code != 0:
    sys.exit("jodd-ai-mcp exited %d at end of input" % code)
PY
echo "test image smoke: ok"
