# Harness flags: what each one is for

Jodd's local presets may drift from these now that the host is its own project; this file and `harness.rs` are what the host runs.

These are the exact argv values in `crates/jodd-ai-mcp/src/harness.rs`. They were measured against real binaries in Jodd's `src-tauri/src/llm/presets.rs` (commit d514d03d). Do not change a flag without re-running the real CLI; a flag that looks redundant is usually there because of a failure that was observed.

The prompt (system text, then `\n\n---\n\n`, then the request) is always sent on stdin. The child runs in an empty scratch directory.

## Claude Code

```
claude -p --output-format json --strict-mcp-config --setting-sources "" --tools ""
```

- `-p --output-format json`: verified 2026-06-13 (claude 1.0.24). Emits ONE JSON object on stdout whose `result` field holds the model's text as a string. The answer is read from `result`. Measured 2026-08-30 with a schema: `result` came back a string, not a parsed object. A non-string `result` is still handled (it is returned as its JSON text) as insurance against a future change.
- `--strict-mcp-config --setting-sources ""`: isolation. Without them `claude -p` loads the user's settings.json, hooks, skills, plugins and every registered MCP server before reading the prompt. Added 2026-08-29 against claude 2.1.246 and verified live 2026-08-30 on an authenticated session: this exact argv answers `is_error: false` with the model's text. Measured over two rounds of a headless timing script, local startup (wall time minus `duration_api_ms`) fell from about 5.3-5.5 s to about 2 s. The isolated variant was faster in 4 runs out of 4, but one run drifted to about 4.6 s and the control itself varied by 2.4 s that round, so treat roughly 3 s saved as typical, not guaranteed.
- Not `--bare`: it reaches the same ground but stops reading OAuth/keychain entirely and demands `ANTHROPIC_API_KEY`, which breaks the sign-in the user already has.
- `--tools ""`: disables all built-in tools. The prompt arrives whole on stdin and the child runs in an empty scratch directory, so there is nothing for a file tool to find, and a turn spent looking is a turn wasted.
- `--tools` must be last. `--tools <tools...>` is variadic. Anything appended after it is silently swallowed as another tool name unless it starts with `-`. Claude's `schema_args` is empty today, but the ordering must be kept so a future addition that does not begin with `-` cannot be eaten.
- No `--json-schema`, and that is a measurement, not an omission. It was added 2026-08-29 on the strength of a one-line probe. Run against Jodd's real Extract prompt on 2026-08-30 it broke the call two ways: with model `sonnet` the run ended with subtype `error_max_structured_output_retries` (`terminal_reason: structured_output_retry_exhausted`, 6 turns burned, `result: null`); with `opus-5[1m]` the request tripped a safety classifier ("safeguards flagged this message ... Details: [reasoning_extraction]"). This is not caused by `--tools ""`: tested as a matrix, the schema fails with tools enabled too, and the same prompt on the same model succeeds once the schema is dropped. The system prompt already demands a single JSON object; layering Claude's structured output on top is what fights. Callers must parse the JSON out of the text leniently.

## Codex

```
codex exec --ignore-user-config --ephemeral --sandbox read-only --skip-git-repo-check --output-last-message <out file> -
codex ... --output-schema <schema file>      # only when a schema is supplied
```

- `--output-last-message <file>`: verified 2026-07-28 (codex on macOS). stdout is full of hook chatter and token counts, but this flag writes only the final assistant message to the file, with no wrapper and no ANSI. That is why the answer is read from the file and stdout is ignored.
- `--skip-git-repo-check`: without it `codex exec` refuses to start outside a trusted directory and the run dies immediately with "Not inside a trusted directory".
- `--ignore-user-config`: skips `$CODEX_HOME/config.toml`, which is also where `mcp_servers` lives. Unlike Claude's `--bare` this keeps the user's sign-in: the help text says "auth still uses CODEX_HOME", and that held in practice (the run authenticated normally). Added 2026-08-29 against codex-cli 0.147.0 and verified live the same day: this exact argv exits 0 and writes the answer to the file.
- `--ephemeral`: a one-shot call should not persist a session file per request; they only accumulate on the user's disk.
- `--sandbox read-only`: blocks writes. It does not block reads, so Codex can still read files the user can read.
- `-`: read the prompt from stdin.
- `--output-schema <file>`: added 2026-08-29 from `codex exec --help` and verified live. It is stricter than plain JSON Schema: it enforces OpenAI's strict structured-output rules, answering `400 invalid_json_schema` ("'additionalProperties' is required to be supplied and to be false") and exiting 1 with an EMPTY last-message file when the schema does not meet them. An empty last-message file is therefore an error, not an empty answer. Schemas must set `additionalProperties: false` and satisfy the strict rules; re-run Codex against any schema change.

## Answer extraction

- Claude: ANSI escapes are stripped from stdout, empty output is an error, the JSON is parsed (a single object, or an array of events where the last element with the field wins), and the `result` field is returned. Non-JSON stdout is an error. A missing field is an error ("no string field 'result'"). A non-string field is returned as its JSON text.
- Codex: the out file's content is returned with ANSI stripped; a missing, empty or whitespace-only file is an error.
