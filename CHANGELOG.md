# Changelog

All notable changes to this project are documented in this file. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
