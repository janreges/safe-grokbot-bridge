# Changelog

All notable changes to this project are documented in this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A skill for Grok Bot and other clients, `safe-grokbot-bridge-client`. Besides using the API,
  it connects a new machine: it creates the Cloudflare Tunnel and writes a short setup message
  for the coding agent on the machine. It finds machines by their secrets, so it needs no
  editing, and Grok Bot can install it from its URL.
- A quick setup at the top of the guided setup: install the two skills, tell Grok Bot to connect
  a machine, and paste its message into Claude Code, Codex or Hermes on the machine. The
  installation commands for each agent, Hermes included, are in collapsible sections.

### Changed

- The README, the guides, the security policy and the agent skill say plainly that the bridge is
  remote access to the machine it runs on, and recommend running it, with the agents it drives,
  in an isolated environment: a virtual machine, a dedicated WSL 2 distribution or a separate
  computer. The deployment guide shows how to set up a WSL 2 distribution without access to the
  Windows drives and programs.

## [0.2.0] - 2026-10-30

### Added

- An agent skill, `safe-grokbot-bridge`, that lets Claude Code, Codex and other coding agents
  install, configure, upgrade and troubleshoot the bridge. It agrees with the user on the
  folders to expose and what the bot may do there, before it writes or changes the config. The
  repository is a plugin marketplace for Claude Code and Codex, and `npx skills add` installs
  the skill for other agents. Release archives contain it too.

### Changed

- Waiting for a session costs far less CPU: a long-poll looks every 2 s instead of every
  second, `claude agents --json` is cached for 5 s and runs only for listings and Claude
  sessions, and a headless session is read from its record alone.
- A tmux session that cannot be created is answered with 502 `tmux_failed` and a hint to use
  the headless runner, instead of an internal error. Internal errors say that repeating the
  request will not help and where the details are logged.
- The bridge logs a warning once when `client_ip_header` holds something other than a single
  address, such as an `X-Forwarded-For` list.
- Only the release job of the release workflow may write to the repository.
- Hermes Agent 0.21.1 has passed the same checks against the real CLI as Claude Code and Codex.

### Fixed

- Stopping the bridge behind a Cloudflare Tunnel no longer waits for open long-polls: requests
  get 10 s, then the bridge stops its headless turns.
- Typing into a tmux session completes even when the client hangs up between the paste and
  `Enter`.
- Sessions start in tmux also in a folder whose name ends in `;`.
- A file deleted while it is read is answered with 404 instead of 500.
- The Windows instructions say to clear Task Scheduler's *Stop the task if it runs longer than*,
  whose default of 3 days would stop the bridge.
- The Grok Bot skill's `sgb` helper works with machine names that contain `-`.
- The systemd units in the guides wait 5 s between restarts instead of giving up after a burst
  of failures.
- Documentation: `--token-file` needs cloudflared 2025.4.0, the state folder keeps the texts
  recently sent to tmux sessions, and several smaller corrections.

## [0.1.1] - 2026-10-30

### Security

- The bridge no longer types into a tmux pane whose window has `synchronize-panes` on and other
  panes, since tmux would copy every key to them, and it checks the pane before every key of a
  batch.
- Git access also checks loose objects and commit graphs that link into another repository,
  refuses object alternates containing a NUL byte, and refuses object stores that cannot be
  resolved.
- Agent programs are resolved to absolute paths, and relative `PATH` entries are skipped, so a
  project cannot put its own `claude` or `codex` in the way.
- The public TLS listener counts an IPv6 /64 network as one client for its connection limit.
  `/healthz` answers and rejected requests close their connection.

### Fixed

- Windows: processes a headless turn leaves behind are ended when the agent exits.
- A first prompt longer than 8 000 bytes for a tmux session is refused with an explanation
  instead of failing inside tmux.
- A `keep_alive: false` session whose turn was started with keys is closed after its answer.
- A tmux socket that cannot be reached (for example for lack of permission) no longer marks live
  sessions as exited.
- A shell-hosted Claude Code session of a second profile uses that profile's inbox and
  transcripts.
- A Codex TUI attached to a separate `codex app-server` daemon is listed, read-only, instead of
  missing.
- A model or effort configured in attached form (`-mopus`, `-ckey=value`, `--config=key=value`)
  is replaced by the requested one.
- `safe-grokbot-bridge openapi | head` no longer panics.

### Changed

- The systemd unit in the deployment guide sets `KillMode=process`, so tmux sessions survive
  restarts and upgrades of the bridge.
- The guided setup asks for the Cloudflare permission *Zone · Zone · Read*, reads the tunnel
  token in Bash (also from zsh), creates secret files private from the start, and names the
  outbound port 7844 that `cloudflared` needs.
- The deployment guide says when the bridge is reachable after a reboot on Linux, WSL 2, macOS
  and Windows.
- Release archives contain the documentation.

## [0.1.0] - 2026-10-30

### Added

- HTTP API with an OpenAPI 3.1 description (`/openapi.json`, `docs/openapi.json`).
- Bearer-token authentication with hashed tokens, per-IP lockout after failed attempts and
  per-token rate limits.
- Plain HTTP on loopback for tunnels and reverse proxies. On public addresses, TLS with an
  automatically generated, pinned self-signed certificate, or a certificate you provide, with
  handshake, first-request and idle timeouts and per-address connection limits.
- Workspaces with per-folder capabilities (`files`, `git`, `sessions`, `input`, `start`), deny
  patterns for secrets, and an "any folder" mode for dedicated machines.
- Read-only file listing (paged) and reading, and `git status`, `log`, `diff` and `show`,
  authorized per repository, including the places its history is stored, and run without
  repository-configured programs or network access.
- Agent sessions for Claude Code, Codex CLI and Hermes Agent, plus generic terminal programs:
  - in tmux, as live terminals a person can attach to, with turn-complete hooks;
  - headless, as one non-interactive run per message;
  - with model, reasoning effort, permission bypass (per workspace), resume and `keep_alive`
    options;
  - with long-polling until an agent stops working.
- Discovery of agent sessions running outside tmux (Claude Code), and experimental messaging into
  them and into agents started from a shell inside tmux (Claude Code inbox socket,
  `codex queue`).
- CLI commands `serve`, `init`, `token`, `check`, `launch` and `openapi`.
- A ready-made Grok Bot skill and deployment guides for Cloudflare Tunnel, VPS and services.
- A guided setup in which Grok Bot creates the Cloudflare Tunnel and a coding agent on the
  machine installs and configures the bridge, with prompts to paste on both sides.
- Release archives for Linux (x86_64, aarch64), macOS (Apple silicon, Intel) and Windows, with
  SHA-256 checksums.

[Unreleased]: https://github.com/janreges/safe-grokbot-bridge/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/janreges/safe-grokbot-bridge/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/janreges/safe-grokbot-bridge/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/janreges/safe-grokbot-bridge/releases/tag/v0.1.0
