"""A small client for the e2e tests: speaks to jodd-ai-mcp the way a client does over SSH.

Standard library only; run it with `python3 -I` (isolated: nothing from the current directory
or the environment is imported). The transport is the argv after `--`, normally
`ssh ... jodd-ai@host jodd-ai-mcp`; any program that speaks the protocol on stdio works.

  mcp_client.py mcp [--protocol V] [--hold FILE] [--call NAME JSON]... -- ARGV...
      initialize, notifications/initialized, tools/list, then each call in order. Prints one
      line per fact, for the shell to grep:
        pid <transport pid>
        server <serverInfo.name> <serverInfo.version>
        protocol <protocolVersion the server chose>
        tool <name> v<_meta tool-version> [harnesses=<enum, comma-separated>]
        listed
        call <name> ok <text as JSON> <structuredContent as JSON>
        call <name> error <kind> <text as JSON> <structuredContent as JSON>
        call <name> rpc-error <code> <message as JSON>
        call <name> closed                (the transport ended before the answer)
        exit <transport exit code>
      --hold FILE: after `listed`, wait until FILE exists before the first call.
      stdin stays open until every answer is in, then closes (EOF ends the session).

  mcp_client.py legacy [--prompt TEXT] [--schema JSON] -- ARGV...
      Sends protocol v2 framing on stdin exactly as Jodd does (`<schema line>\\n<length>\\n
      <prompt>`, schema line `-` or base64) and never closes stdin before the transport exits,
      as Jodd never does. Copies the transport's stdout and stderr byte for byte and exits
      with its exit code.
"""

import base64
import json
import os
import subprocess
import sys
import threading
import time


def split_argv(argv):
    if "--" not in argv:
        sys.exit("usage: mcp_client.py mcp|legacy [options] -- TRANSPORT ARGV...")
    i = argv.index("--")
    if not argv[i + 1:]:
        sys.exit("mcp_client.py: no transport after --")
    return argv[:i], argv[i + 1:]


def say(*words):
    print(*words, flush=True)


def mcp(opts, transport):
    calls, protocol, hold = [], "2025-06-18", None
    while opts:
        o = opts.pop(0)
        if o == "--call":
            calls.append((opts.pop(0), json.loads(opts.pop(0))))
        elif o == "--protocol":
            protocol = opts.pop(0)
        elif o == "--hold":
            hold = opts.pop(0)
        else:
            sys.exit("mcp_client.py: unknown option %s" % o)

    p = subprocess.Popen(transport, stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    say("pid", p.pid)
    next_id = [0]

    def send(msg):
        p.stdin.write((json.dumps(msg) + "\n").encode())
        p.stdin.flush()

    def request(method, params):
        next_id[0] += 1
        send({"jsonrpc": "2.0", "id": next_id[0], "method": method, "params": params})
        while True:
            line = p.stdout.readline()
            if not line:
                return None
            msg = json.loads(line)
            if msg.get("id") == next_id[0]:
                return msg

    def finish(code=None):
        try:
            p.stdin.close()
        except OSError:
            pass
        say("exit", p.wait(timeout=60) if code is None else code)
        sys.exit(0)

    init = request("initialize", {
        "protocolVersion": protocol, "capabilities": {},
        "clientInfo": {"name": "jodd-ai-host-e2e", "version": "1"}})
    if init is None or "result" not in init:
        say("initialize failed", json.dumps(init))
        finish()
    info = init["result"]
    say("server", info["serverInfo"]["name"], info["serverInfo"]["version"])
    say("protocol", info["protocolVersion"])
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    listed = request("tools/list", {})
    if listed is None or "result" not in listed:
        say("tools/list failed", json.dumps(listed))
        finish()
    for t in listed["result"]["tools"]:
        words = ["tool", t["name"], "v%s" % (t.get("_meta") or {}).get("jodd-ai-host/tool-version")]
        enum = t["inputSchema"].get("properties", {}).get("harness", {}).get("enum")
        if enum is not None:
            words.append("harnesses=" + ",".join(enum))
        say(*words)
    say("listed")

    if hold:
        while not os.path.exists(hold):
            time.sleep(0.1)

    for name, args in calls:
        r = request("tools/call", {"name": name, "arguments": args})
        if r is None:
            say("call", name, "closed")
            break
        if "error" in r:
            say("call", name, "rpc-error", r["error"]["code"], json.dumps(r["error"]["message"]))
            continue
        res = r["result"]
        text = "".join(c.get("text", "") for c in res.get("content", []))
        sc = res.get("structuredContent") or {}
        if res.get("isError"):
            say("call", name, "error", sc.get("kind"), json.dumps(text), json.dumps(sc, sort_keys=True))
        else:
            say("call", name, "ok", json.dumps(text), json.dumps(sc, sort_keys=True))
    finish()


def legacy(opts, transport):
    prompt, schema = "", None
    while opts:
        o = opts.pop(0)
        if o == "--prompt":
            prompt = opts.pop(0)
        elif o == "--schema":
            schema = opts.pop(0)
        else:
            sys.exit("mcp_client.py: unknown option %s" % o)
    data = prompt.encode()
    line = base64.b64encode(schema.encode()).decode() if schema is not None else "-"
    framed = ("%s\n%d\n" % (line, len(data))).encode() + data

    p = subprocess.Popen(transport, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def copy(src, dst):
        while True:
            b = src.read1(65536)
            if not b:
                return
            dst.write(b)
            dst.flush()

    threads = [threading.Thread(target=copy, args=(p.stdout, sys.stdout.buffer)),
               threading.Thread(target=copy, args=(p.stderr, sys.stderr.buffer))]
    for t in threads:
        t.start()
    try:
        p.stdin.write(framed)
        p.stdin.flush()
    except OSError:
        pass  # the server refused before reading: its exit code is the answer
    code = p.wait()
    for t in threads:
        t.join()
    try:
        p.stdin.close()
    except OSError:
        pass
    sys.exit(code)


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("mcp", "legacy"):
        sys.exit(__doc__)
    opts, transport = split_argv(sys.argv[2:])
    (mcp if sys.argv[1] == "mcp" else legacy)(opts, transport)


if __name__ == "__main__":
    main()
