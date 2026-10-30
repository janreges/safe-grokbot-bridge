# Changelog

All notable changes to this project are documented in this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/janreges/safe-grokbot-bridge/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/janreges/safe-grokbot-bridge/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/janreges/safe-grokbot-bridge/releases/tag/v0.1.0
