# jodd-ai-host

An **AI host** is a machine you own where Jodd runs Claude Code or Codex for
you. Jodd connects over SSH with a key that can run one program only:
`jodd-ai-mcp`. That program runs the CLIs the host's policy allows, and
nothing else. This lets Jodd use them from Android, and from any computer that
does not have them installed.

This repository holds `jodd-ai-mcp`, the installers for Linux and macOS, the
Docker image, and the `jodd-ai-host` helper.

**Your own CLI, your own account, on your own server, under the provider's
terms.** You install the CLI and sign in to it yourself; Jodd never sees that
sign-in. It is for one person across their own devices: do not share a
personal subscription, pool accounts or resell access. Details and the
providers' terms are in the [install guide](docs/INSTALL.md).

## Install

**Docker:**

```sh
curl -fsSLO https://raw.githubusercontent.com/BBM-Co-ORG/jodd-ai-host/main/docker/compose.yaml
docker compose up -d
docker compose logs
```

**Linux** (Ubuntu, Debian, Fedora, Alpine) or **macOS 13+**: download the
installer, read it, then run it with `sudo`. The
[install guide](docs/INSTALL.md) has the commands for each, and what to
check first on a VPS or any machine reachable from the internet.

## Then

1. Sign in to the CLIs on the host: `sudo jodd-ai-host signin claude` and
   `sudo jodd-ai-host signin codex`.
2. In Jodd: **App settings** → **LLM provider** → **Agent CLI (headless)** →
   **Runs on: A server**. Check the fingerprint against `jodd-ai-host fingerprint`.
3. After Setup: `sudo jodd-ai-host adopt-keys`, then `sudo jodd-ai-host lock`.

In Docker, drop `sudo` and put `docker compose exec ai-host` in front.
**Upgrading a host set up before 1.0.0:** re-run the installer (or
`docker compose pull && docker compose up -d`), then `sudo jodd-ai-host adopt-keys`.

## Documents

- [Install guide](docs/INSTALL.md) ([ภาษาไทย](docs/INSTALL.th.md)): every path, networking, hardening, updating, removing, troubleshooting.
- [How it works](docs/HOW-IT-WORKS.md): the parts, the policy, what the key can and cannot do, where your data goes.
- [Protocol](docs/PROTOCOL.md): for client authors. The MCP tools, their errors, and the compatibility rules.
- [Harness flags](docs/HARNESS-FLAGS.md): why each CLI flag is there.
- [Jodd user guide](https://github.com/BBM-Co-ORG/Jodd-public/blob/main/docs/ai-host/USER-GUIDE.md): the Jodd side, and every message Jodd can show.
- [Changelog](CHANGELOG.md)

Licence: Apache-2.0.
