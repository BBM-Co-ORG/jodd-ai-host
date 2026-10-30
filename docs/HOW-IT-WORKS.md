# How the Jodd AI host works

This page explains what happens between Jodd and your AI host, what Jodd's key
can and cannot do, and where your data goes. To set a host up, see the
[install guide](INSTALL.md). For the Jodd side, see the [user guide](https://github.com/BBM-Co-ORG/Jodd-public/blob/main/docs/ai-host/USER-GUIDE.md).
To write a client of your own, see the [protocol](PROTOCOL.md).

**Your own CLI, your own account, on your own server, under the provider's
terms.** You install Claude Code or Codex on a machine you control and sign in
to it yourself. Jodd never sees that sign-in and never offers one of its own.
You are responsible for using the CLI as Anthropic's terms
([consumer](https://www.anthropic.com/legal/consumer-terms),
[commercial](https://www.anthropic.com/legal/commercial-terms)) or
OpenAI's [consumer terms](https://openai.com/policies/terms-of-use) or
[business agreement](https://openai.com/policies/services-agreement/) allow.

**Scope and provider terms.** This setup is for one person using their own
provider account across their own devices. SSH connectivity and successful
sign-in do not establish permission for every app integration. Jodd submits
headless requests from its note features; it is more than a remote terminal.
Do not share a personal subscription, pool accounts, or resell access. Separate
OS accounts alone do not authorize a multi-user service. Account type,
authentication method, provider policies and any required provider agreement
still apply.

## The parts

```text
 Your device              Your AI host                                       The provider
 ───────────              ────────────                                       ────────────
 Jodd ── SSH, Jodd's key ──▶ sshd, account jodd-ai
                             └─ authorized_keys line for that key:
                                restrict,command="/usr/local/bin/jodd-ai-mcp --key <id>"
                                └─ jodd-ai-mcp  (root-owned program)
                                   ├─ reads the policy: /etc/jodd-ai-host/policy.toml
                                   │   (root-owned; changed by `sudo jodd-ai-host enable | disable`)
                                   └─ runs claude or codex as jodd-ai ── HTTPS ──▶ Anthropic / OpenAI
                                      (signed in by you)
 Jodd ◀── the answer ─────── jodd-ai-mcp ◀── CLI ◀───────────────────────────── answer

 Administrator, on the host:  sudo jodd-ai-host enable | disable | adopt-keys | lock | doctor
```

- **Jodd** (desktop or Android) opens a fresh SSH connection for each AI request.
- **The account** `jodd-ai` exists only for this. The installer or the Docker
  image creates it.
- **`jodd-ai-mcp`** is the program every Jodd key runs. sshd starts it in place
  of whatever the key asks for. It is installed by the host's installer (or
  built into the Docker image), owned by root, and the AI account cannot change
  it. It answers the [MCP protocol](PROTOCOL.md) and, for older Jodd releases,
  the older `jodd-ai-run 2` requests.
- **The policy** says which CLIs Jodd may run, where they are, and the limits:
  time per run, request size, runs at once. Only root can change it. If the
  file is missing or broken, nothing runs.
- **The CLI** is the official `claude` or `codex` program, installed by its
  vendor's own installer, signed in by you.
- **The provider** (Anthropic or OpenAI) answers the CLI under your account.

## The policy: which CLIs Jodd may run

The installer, and the Docker image on each start, enable every CLI they find
for `jodd-ai` that the policy has no decision for yet. After that, you decide:

```sh
jodd-ai-host list
```

```sh
sudo jodd-ai-host disable codex
```

```sh
sudo jodd-ai-host enable codex
```

- `disable` is a decision. It survives re-running the installer and
  `docker compose pull`; the installer and the image never enable that CLI
  again by themselves.
- `enable` finds the CLI as `jodd-ai`, the way a login shell does, and records
  its path. Run it again after you move or reinstall a CLI somewhere else.
- A change applies to the next request. A Jodd that is already connected sees
  the change at its next call.
- The file is `/etc/jodd-ai-host/policy.toml`. In Docker it lives in the
  `state` volume (`/var/lib/jodd-ai-host/policy.toml`), with a link from
  `/etc`, so an image update keeps it. Edit it with the commands above, not by
  hand: `jodd-ai-host doctor` says what is wrong with a hand-edited one.

A client cannot change any of this. It can ask for less (a shorter time
limit), never for more.

## What Setup does with your password

In Jodd you enter the host, the port, the user and the password once.

1. Jodd shows the host's fingerprint. You compare it with what the host prints
   (`jodd-ai-host fingerprint`) and confirm it. Jodd pins that key: it never
   connects to a server whose key differs.
2. Jodd makes a new SSH key on your device.
3. Jodd signs in **once** with the password and adds one line for its key to
   `~/.ssh/authorized_keys`. The line ends in `jodd-ai-` and 8 characters that
   identify this device's key.
4. Jodd disconnects, then connects again **with the key** and asks for the
   host's status. That proves the restricted key works.

The password is used for that one connection. Jodd does not store, log or show
it.

**Jodd releases from before the MCP client** also install their own helper,
`~/.jodd/ai-run-v2`, during Setup, and point the key's line at it. That line
does not go through `jodd-ai-mcp` or the policy. Point it at `jodd-ai-mcp`:

```sh
sudo jodd-ai-host adopt-keys
```

`adopt-keys` changes only Jodd's own lines that run that helper. It runs as
`jodd-ai`, never as root, and changes nothing else. `jodd-ai-host status` and
`doctor` list every Jodd key that does not run `jodd-ai-mcp`. You can run
`adopt-keys` as often as you like.

**Run `sudo jodd-ai-host lock` after Setup.** Jodd needs the password only
during Setup, Change server and Reinstall helper, so turn password login off in
between. `lock` sets `PasswordAuthentication no` for the `jodd-ai` account only
and reloads sshd; open sessions stay up. Before the next Setup, run
`sudo jodd-ai-host unlock`, and `lock` again after. While locked, an older
Jodd's **Reinstall helper** cannot point its line back at its own helper.

## What the key can do, and what it cannot stop

The line in `authorized_keys` starts with
`restrict,command="/usr/local/bin/jodd-ai-mcp --key …"`. That means:

- **No shell and no terminal.** Whatever command the key asks for, sshd runs
  `jodd-ai-mcp` instead, and `jodd-ai-mcp` refuses everything but its own
  requests.
- **No forwarding** of ports, agents or X11.
- **Only what the policy enables**, at the paths in the policy, with the
  command line fixed in `jodd-ai-mcp` ([HARNESS-FLAGS.md](HARNESS-FLAGS.md)).
  A request cannot add arguments, pick another program or raise a limit.

**One gap stays open in 1.0.** `~/.ssh/authorized_keys` belongs to the
`jodd-ai` account, so a program that could write files as `jodd-ai` could
remove `restrict,command=…` from a line and get a shell. Today nothing Jodd
runs can write there: Claude Code runs with all its tools turned off, and Codex
runs in its read-only sandbox. A later release will move the key file out of
the account's reach.

What the key cannot stop is what the CLI itself may do while it runs:

- **Claude Code** runs with all its built-in tools turned off, so it answers
  from the prompt alone.
- **Codex** runs in its read-only sandbox. Read-only blocks writing, not
  reading: **Codex can read any file the account can read**, and what it reads
  can end up in the answer that comes back to Jodd.

"Any file the account can read" is more than the account's own home. On a
normal Linux or Mac it includes everything readable by all users, such as most
of `/etc` and `/Users/Shared` on a Mac. It can include other people's home
folders too:

- Many Linux systems leave home folders readable by every account.
- On a Mac, every account is in the `staff` group by default, and every home
  folder lets `staff` in. Inside it only Desktop, Documents, Downloads, Movies,
  Music, Pictures and Library are closed; everything else that is not closed separately can be
  read. So the macOS installer gives `jodd-ai` a group of its own instead of
  `staff`, and closes `jodd-ai`'s own home folder to other accounts. Its
  summary lists the home folders that are still open, with the command that
  closes each one.

On a Mac you also use, close your own home folder (`chmod 700 /Users/<you>`)
and keep secrets such as API keys and `.env` files out of places other
accounts can read.

So the installers create a dedicated account, `jodd-ai`, with nothing else in
it. Do not put other files there. Do not give the account `sudo`. The
installers refuse an existing account that can become root. Beyond that, use a
machine or virtual machine that does nothing else, or the Docker image: in a
container, Codex sees only the container's files and its two volumes, not the
rest of the machine.

## Where your data goes

1. On your device, Jodd prepares the request. Masking and hidden folders apply
   exactly as for any other AI provider.
2. The request travels over SSH to your host. On the host, `jodd-ai-mcp`
   writes the prompt to a temporary folder for that run and deletes the folder
   when the run ends, whether it worked, failed or was stopped.
3. The CLI sends the request to Anthropic or OpenAI, under the account you
   signed in with, and receives the answer.
4. The answer travels back to your device the same way.

**Jodd's developers receive nothing.** There is no Jodd server in this path.
`jodd-ai-mcp` logs one line per request (which key, which CLI, how long, how
it ended), never the prompt or the answer. What each CLI keeps in its own
folders on the host (`~/.claude`, `~/.codex`) follows that CLI's own rules.
Codex is asked not to save a session file.

## Stopping a run

When you cancel an AI action in Jodd (for example **Cancel** in Ask Jodd),
Jodd ends the request or hangs up the connection. `jodd-ai-mcp` stops the CLI:
TERM, then KILL 5 seconds later, and the run's folder is deleted. That takes
**at most about 6 seconds**.

There are three limits:

- If the network drops silently (no hang-up reaches the host), the host
  notices only through SSH keepalive. It checks every 30 seconds and gives up
  after 4 missed answers: **about 2 minutes**.
- **Every run has a time limit**: by default 120 seconds for Claude Code and
  180 seconds for Codex. It is in the policy.
- **Two runs at a time** by default. A request when both are busy is refused
  at once, not queued.

## Versions and Update

`jodd-ai-mcp` has its own version (`jodd-ai-host status` shows it), separate
from Jodd's. A newer host keeps answering older Jodd releases: new features
are added as new tools, and nothing an older Jodd uses is removed without a
long notice ([protocol](PROTOCOL.md#versions-and-compatibility)).

To update the host, re-run the installer, or in Docker `docker compose pull`
and `docker compose up -d` ([install guide](INSTALL.md#update)). Your policy,
Jodd's keys, the host key and the lock are kept. A host set up before 1.0.0
needs `sudo jodd-ai-host adopt-keys` once after the update.

If a CLI was installed after Setup, run `sudo jodd-ai-host enable claude` (or
`codex`). Jodd learns about it at its next request; no Reinstall helper is
needed for that.

## What is stored where

| Where | What |
|---|---|
| Your device | Jodd's private key for this host and the host's pinned public key, in Jodd's own data folder. Nothing else about the host is secret. |
| The host, in `~/.ssh/authorized_keys` | One line per Jodd device, ending in `jodd-ai-` and an 8-character id. |
| The host, `/usr/local/bin/jodd-ai-mcp` | The program every Jodd key runs. Root-owned. |
| The host, `/etc/jodd-ai-host/policy.toml` | Which CLIs Jodd may run, and the limits. Root-owned. In Docker, in the `state` volume. |
| The host, in `~/.jodd/` | Only on hosts that an older Jodd set up: its helper, `ai-run-v2`. Unused after `adopt-keys`; you may delete it. |
| The host, in the CLIs' own folders | Their programs and your sign-in, written by the CLIs themselves. Jodd never reads them. |
| The host, in `/etc` | `/etc/jodd-ai-host.conf` and the sshd rule `/etc/ssh/sshd_config.d/50-jodd-ai.conf`. In Docker, the rule and the host key live in the `state` volume. |
| The host, syslog or `/var/log/jodd-ai-host/calls.log` | One line per request: key id, CLI, duration, outcome. Docker has no syslog, so it uses the file. |
| The host, in `/var/lib/jodd-ai-host` | Linux: only if you set `JODD_AI_PASSWORD`, a salted hash of it, readable by root only, so a later run can tell whether you changed it. Docker: this is the whole `state` volume: the host key (so the fingerprint survives upgrades), the sshd rule (so `lock` survives), the policy, and that password hash. |

Jodd stores the password nowhere. The host keeps the password of `jodd-ai`
the way the operating system keeps every account's password.

## Removing

- **Remove** in Jodd's settings takes effect at once. Jodd asks the host to
  delete its key's line from `authorized_keys`, then deletes the key on your
  device. If the host cannot be reached, Jodd only forgets it on your device;
  remove the line on the host yourself (below).
- On the host, `sudo jodd-ai-host keys` lists Jodd's keys by id, and
  `sudo jodd-ai-host revoke <id>` removes one. The device that used it will
  then report that the host no longer accepts its key.

To take everything off a machine, see
[Remove everything](INSTALL.md#remove-everything) in the install guide.
