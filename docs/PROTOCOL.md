# jodd-ai-host protocol

This is the contract between an AI host and its clients. Jodd is the first
client; anyone can write another. It describes what `jodd-ai-mcp` 1.0.0 does.
When this file and the code disagree, the code in `crates/jodd-ai-mcp` is
right and this file has a bug.

Contents: [transport](#transport) · [entry](#entry) · [MCP session](#mcp-session) ·
[`infer`](#infer) · [`forget_key`](#forget_key) · [cancelling](#cancelling) ·
[versions and compatibility](#versions-and-compatibility) ·
[discovery](#discovery-what-a-client-should-say) · [legacy shim](#the-legacy-shim-jodd-ai-run-2)

## Transport

| Transport | Status |
|---|---|
| stdio over SSH | The only transport in 1.x. |
| HTTP | Reserved. Not implemented. |

The client connects with SSH as the host's AI account (`jodd-ai` by default),
with a key of its own. The key's line in that account's
`~/.ssh/authorized_keys` is:

```text
restrict,command="/usr/local/bin/jodd-ai-mcp --key <id>" ssh-ed25519 AAAA… jodd-ai-<id>
```

- `<id>` is 8 lowercase hex digits. It names the key in the host's log and is
  what `forget_key` removes.
- `restrict` means no terminal, no port, agent or X11 forwarding. The client
  gets `jodd-ai-mcp` and nothing else, whatever command it asks for.
- `jodd-ai-mcp` is root-owned. It reads the root-owned policy
  `/etc/jodd-ai-host/policy.toml`. A client cannot change either.

## Entry

sshd puts the command the client asked for in `SSH_ORIGINAL_COMMAND`.
`jodd-ai-mcp` splits it on spaces, tabs and newlines. There is no quoting,
no globbing and no variable expansion.

| The client asks for | What runs |
|---|---|
| `jodd-ai-mcp` (exactly this one word) | An MCP session on stdin/stdout. |
| `jodd-ai-run <version> <verb> [preset]` | The [legacy shim](#the-legacy-shim-jodd-ai-run-2). |
| anything else, including `jodd-ai-mcp` plus more words | Nothing. Exit 64, `unknown request` on stderr. |

A client that sends `jodd-ai-mcp` to a host without this server (an older
host) gets the same exit 64. That is how a client tells the two apart.

`--key <id>` comes from the forced command only, never from the client.

## MCP session

The session is [MCP](https://modelcontextprotocol.io) over stdio: JSON-RPC
2.0, one message per line, UTF-8.

**Handshake.** The first request must be `initialize` (a `ping` before it is
answered). If stdin ends first, or the first message is something else, the
server exits with code 76 without an answer.

**Protocol version.** The server answers `initialize` with the client's
`protocolVersion` when it supports it. In 1.0.0 it supports `2024-11-05`,
`2025-03-26`, `2025-06-18`, `2025-11-25` and `2026-07-28`. For any other
version it answers `2025-11-25`. This is MCP's own negotiation: a client that
cannot use the version in the answer must disconnect.

**Server info.** `serverInfo.name` is `jodd-ai-host`. `serverInfo.version` is
the version of `jodd-ai-mcp`, for example `1.0.0`. The server offers the
`tools` capability only.

**Lines.**

- A line longer than `4 × max_request_bytes + 64 KiB` ends the connection
  (default about 32 MiB). The limit is fixed when the session starts.
- A line that is not a JSON-RPC message the server understands (bad JSON,
  the wrong shape) is skipped without an answer. A request on such a line
  gets no response, so the client times out.

**`tools/list`.**

- `forget_key` is always listed.
- `infer` is listed only while at least one harness is enabled. Its
  `harness` enum lists the enabled harnesses.
- Each tool carries `_meta: { "jodd-ai-host/tool-version": 1 }`.
- The list is the policy as it was when the session started. `infer` reads
  the policy again on every call, so a harness the administrator disabled
  since then gets `not_enabled`.

**Tool results.** A tool that ran and failed returns a normal result with
`isError: true` and `structuredContent.kind` set. Only malformed arguments
and an unknown tool name are JSON-RPC errors (`-32602`, invalid params).

**Logging.** The server writes one line per call: key id, tool, harness,
duration and outcome. It never logs the prompt, the system text, the schema
or the answer.

## `infer`

Runs one prompt through one CLI on the host and returns the answer text.

Input schema, as `tools/list` returns it (the `harness` enum shows the
enabled harnesses; here both):

```json
{
  "type": "object",
  "properties": {
    "harness": {
      "type": "string",
      "enum": ["claude", "codex"],
      "description": "Which CLI runs the prompt; only the harnesses this host enables."
    },
    "prompt": {
      "type": "string",
      "description": "The prompt, fed to the CLI on stdin."
    },
    "system": {
      "type": "string",
      "description": "Optional system text, sent ahead of the prompt as `{system}\\n\\n---\\n\\n{prompt}`."
    },
    "schema": {
      "type": "object",
      "description": "Optional JSON Schema for the harness's structured-output mode. Used by Codex; Claude Code ignores it (it still counts toward max_request_bytes)."
    },
    "timeout_secs": {
      "type": "integer",
      "minimum": 1,
      "description": "Optional; may only lower this host's limit for the harness."
    }
  },
  "required": ["harness", "prompt"],
  "additionalProperties": false
}
```

The rules:

- **Clients can only narrow.** An argument the host does not know is refused
  (invalid params), never ignored. `timeout_secs` can lower the host's limit
  for that harness, never raise it.
- `null` counts as absent for `system`, `schema` and `timeout_secs`.
- Invalid params (`-32602`): a missing `harness` or `prompt`, a value of the
  wrong type, `schema` that is not a JSON object, `timeout_secs` that is not
  an integer of at least 1, or an unknown argument.
- A `harness` that is a string but not enabled (or not known) is **not**
  invalid params. It is the tool error `not_enabled`, because a client may
  hold a stale list.
- With `system`, the CLI gets `{system}\n\n---\n\n{prompt}` on stdin.
  Without it, the CLI gets the prompt.
- `schema` is handed to Codex (`--output-schema`). Claude Code ignores it,
  but it still counts toward `max_request_bytes`.
- The size checked against `max_request_bytes` (default 8 MiB) is that stdin
  text plus the schema's JSON text, in bytes.
- The host owns each CLI's command line, how the answer is extracted (Claude
  Code's JSON envelope, Codex's last-message file) and the isolation flags.
  The client sees only the answer text. See [HARNESS-FLAGS.md](HARNESS-FLAGS.md).

**Success:**

```json
{
  "content": [{ "type": "text", "text": "<the answer>" }],
  "structuredContent": { "harness": "codex", "exit_code": 0 }
}
```

**Failure:** `isError: true`, one text item saying what happened, and
`structuredContent: { "kind": …, … }`:

| `kind` | When | Other fields |
|---|---|---|
| `not_enabled` | The harness is not enabled now: no policy, an invalid policy, disabled, or a harness this host does not offer. The text says what to run (`jodd-ai-host doctor`, or `sudo jodd-ai-host enable <name>`). | none |
| `too_large` | The request is over `max_request_bytes`. The CLI never starts. | none |
| `busy` | All `max_concurrent_runs` slots (default 2) are taken. Refused at once, never queued. | none |
| `cli_missing` | The CLI is not at the path in the policy. | none |
| `timeout` | The run hit its time limit (policy default: 120 s Claude Code, 180 s Codex). | `stderr` |
| `cli_failed` | The CLI exited with a non-zero code; or it exited 0 but no answer could be read; or the host could not run it at all. | `exit_code`, `stdout`, `stderr` when the CLI ran |

- `stdout` and `stderr` are the CLI's raw output (invalid UTF-8 replaced), so
  a client can recognise a CLI's own messages, such as "not signed in".
- **A client must treat a `kind` it does not know as `cli_failed`.** New
  kinds may be added in any release (for example a future `not_signed_in`).

## `forget_key`

Removes the calling key's line from the AI account's `authorized_keys`, then
ends the session. It is always listed and never subject to the policy: a
client can always revoke itself.

- Arguments: none (`{}` or absent). Any argument is invalid params.
- It removes every line ending in ` jodd-ai-<id>`, where `<id>` is the
  calling key's `--key`. Other lines keep their bytes and their place.
- After the answer to a handled call (any row of the table below) is sent,
  the server exits. A call refused as invalid params leaves the session open.

| Result | `structuredContent` |
|---|---|
| The line was removed | `{ "removed": true }` |
| There was no line for this key | `{ "removed": false }` |
| Error `no_key`: the forced command has no valid `--key <id>` | `{ "kind": "no_key" }` |
| Error `forget_failed`: `authorized_keys` could not be edited (for example it is a symlink) | `{ "kind": "forget_failed" }` |

As with `infer`, a client must treat an unknown error `kind` as a failure.

## Cancelling

A run stops when:

- the client sends `notifications/cancelled` with that request's id;
- stdin ends (the client went away, or the SSH channel closed). This stops
  every run of the session;
- the server gets TERM, HUP or INT (a container stop, a `pkill`). This stops
  every run of the session, and the process exits 143 once they are cleaned up;
- its time limit is reached (`timeout`).

Stopping means: the CLI's whole process group gets TERM, then KILL 5 seconds
later, and the run's scratch directory is removed. A cancelled request gets
no response. After stdin ends, the process exits once every stopped run has
been cleaned up, within about 6 seconds.

If the network drops without a hang-up, the host notices only through SSH
keepalive. The installers and the image set it to about 2 minutes.

## Versions and compatibility

The host and its clients are released separately. These rules let them do
that:

1. **Additive only.** A new capability is a new tool name. A change that
   would break an existing caller ships as a new tool (for example
   `infer_v2`). New optional fields in a result, and new error kinds, are
   additive.
2. **Deprecation window.** A tool that is replaced stays available for at
   least 12 months after the release that deprecates it, and this file marks
   it as deprecated with that release. Phones update late; the window is
   long on purpose.
3. **Hosts first.** The host ships a tool first; clients use it after.

Each tool's `_meta` `jodd-ai-host/tool-version` is a whole number. It goes up
only for an additive change to that tool that a client may want to detect.

| Tool | Introduced in | `tool-version` in 1.0.0 |
|---|---|---|
| `infer` | 1.0.0 | 1 |
| `forget_key` | 1.0.0 | 1 |

## Discovery: what a client should say

Decide from the host's own answers, not from version numbers alone:

| Situation | How the client learns it | What to tell the user |
|---|---|---|
| The host has no MCP server (an older kit) | The `jodd-ai-mcp` request exits 64 | Upgrade the host, then run `sudo jodd-ai-host adopt-keys` there. Meanwhile, use the legacy protocol if the client has it. |
| The host is too old for a tool | The tool is not in `tools/list`, and `serverInfo.version` is older than its "Introduced in" | Upgrade the AI host. |
| The administrator turned it off | The tool is not in `tools/list` (or `infer` lacks the harness), and the version is new enough | Ask the administrator to run `sudo jodd-ai-host enable <name>`. |
| The host's tool is newer than the client | `tool-version` is higher than the client knows | Update the client. Keep using what the client understands, if the host still offers it. |

After a `not_enabled` error, list the tools again.

## The legacy shim: `jodd-ai-run 2 …`

This exists so that Jodd releases from before the MCP client keep working
against this server. It is not for new clients.

**Scope.** It does what wrapper v2 (Jodd's `src-tauri/src/llm/remote/wrapper.rs`
at commit `d514d03d`) does, byte for byte for UTF-8 output: the same stdout,
exit codes and messages. The differences:

- Only enabled harnesses run. `status` reports `path=-` for a harness that is
  disabled or not installed; old Jodd shows it as not installed. `run` for it
  exits 64 with `unknown request`.
- A full house gets exit 70 with `the AI host is busy` at once.
- A prompt over `max_request_bytes` gets exit 64 with `unknown request`.
- On a timeout, CLI stderr written after the TERM is not included.
- CLI output that is not valid UTF-8 is passed on with each invalid sequence
  replaced by U+FFFD, where wrapper v2 passed the raw bytes.
- Each `run` and `forget` writes one log line, like an MCP call.

**Request:** `jodd-ai-run <version> <verb> [preset]`. Words after the fourth
are ignored.

| Verb | Does |
|---|---|
| `status` | Prints `version=2`, then `preset=<name> path=<path or ->` for `claude` and `codex`. Answers any version. Exit 0. |
| `run` | Runs the preset (`claude` or `codex`). Needs version `2`, else exit 65 with `version mismatch: this server has 2, Jodd sent <v>`. |
| `forget` | Removes this key's line. Answers any version. |

**`run` input** on stdin, in this order:

1. a schema line: `-` for none, or the JSON schema in base64, then `\n`;
2. the prompt's length in bytes, in decimal (1 to 9 digits), then `\n`;
3. exactly that many bytes of prompt.

The client keeps stdin open. End of input after the prompt, or TERM, HUP or
INT, cancels the run (exit 143, no output).

**`run` output:** the CLI's stderr on stderr, then the answer on stdout.
For `codex` the answer is the last-message file; for `claude` it is the CLI's
stdout as it is (the JSON envelope; the client unwraps it). The exit code is
the CLI's.

| Exit | Meaning |
|---|---|
| 0, or the CLI's code | The CLI ran. |
| 64 | `unknown request`: an unknown verb or preset, a harness not enabled, a malformed header, a prompt over the limit. `forget` without a key id: `no key id`. |
| 65 | Version mismatch. |
| 70 | The upload was incomplete, the schema was not base64, the host is busy, or `forget` could not edit the file (a missing file prints grep's message, as wrapper v2 did). |
| 124 | Timed out: `timed out after <n>s` on stderr. |
| 127 | `<preset> is not installed on this server`. |
| 141 | The client stopped reading. |
| 143 | Cancelled. |

Removing the legacy shim is a later decision. It will be announced here with
a date.
