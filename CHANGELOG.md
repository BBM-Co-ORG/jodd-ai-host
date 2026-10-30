# Changelog

All notable changes to jodd-ai-host are recorded here. Versions follow
[semver](https://semver.org); they are independent of Jodd's.

## [Unreleased]

## [1.0.0]

The first release of the AI host as its own project. Jodd's keys now run a
program the host owns, under a policy only the administrator can change.

### Added

- `jodd-ai-mcp`, the forced command of every Jodd key
  (`restrict,command="/usr/local/bin/jodd-ai-mcp --key <id>"`). Root-owned,
  static for Linux (x86_64, arm64) and built for macOS (Apple silicon, Intel).
- An MCP server on stdio over SSH, with two tools: `infer` (run one prompt
  through Claude Code or Codex and return the answer text) and `forget_key`
  (a device removes its own key). The contract, the error kinds and the
  compatibility rules are in [docs/PROTOCOL.md](docs/PROTOCOL.md).
- The policy, `/etc/jodd-ai-host/policy.toml` (in Docker, in the `state`
  volume): which CLIs Jodd may run, their paths, time limits, the request size
  limit (8 MiB) and runs at once (2). Missing or invalid means nothing runs.
- Helper commands: `jodd-ai-host list`, `enable`, `disable`, `auto-enable` and
  `adopt-keys`. `doctor` and `status` check `jodd-ai-mcp`, the policy, and every
  Jodd key that does not run `jodd-ai-mcp` yet.
- One log line per request (key id, CLI, duration, outcome) to syslog, or to
  `/var/log/jodd-ai-host/calls.log` where there is none. Never the prompt or
  the answer.
- `jodd-ai-host-test`, the image with stand-in CLIs, for client CI.
- End-to-end tests over real OpenSSH in CI (`test/e2e/image.sh`).

### Changed

- The installers download `jodd-ai-mcp` from the release, verify its SHA-256
  and require it to report the version asked for, before they change anything
  else. Re-running an installer upgrades it.
- The Docker image builds `jodd-ai-mcp` in, and keeps the policy across
  `docker compose pull`.
- A run that is cancelled, times out or loses its client stops the CLI's whole
  process group (TERM, KILL 5 s later) and removes its scratch directory.

### Compatibility

- Jodd releases that speak the older `jodd-ai-run 2` requests keep working:
  `jodd-ai-mcp` answers them exactly as Jodd's wrapper v2 did, for the CLIs
  the policy enables.
- **Upgrading a host set up before 1.0.0:** re-run the installer (or
  `docker compose pull && docker compose up -d`), then run
  `sudo jodd-ai-host adopt-keys` once. It points Jodd's existing keys at
  `jodd-ai-mcp`; nothing is needed in Jodd.

### Known gap

- `~/.ssh/authorized_keys` still belongs to the AI account, so a program that
  could write as that account could loosen a key line. Nothing Jodd runs can
  write there today (Claude Code runs with its tools off, Codex in its
  read-only sandbox). See [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md).
