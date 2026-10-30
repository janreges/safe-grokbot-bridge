# safe-grokbot-bridge

[![CI](https://github.com/janreges/safe-grokbot-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/janreges/safe-grokbot-bridge/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**Let a remote AI agent work on your machines, safely.** `safe-grokbot-bridge` is a small daemon
that gives an agent such as a [Grok Bot](https://x.ai) a narrow, authenticated HTTP API to your
computers. Through it the agent can read your project folders and git history, see the Claude
Code, Codex and Hermes sessions running there, talk to them, and start new ones.

You keep using your agents as before. The remote agent becomes a coordinator that can hand work to
them and follow up while you are away from the keyboard, for example from your phone.

```
 Grok Bot (cloud) ──curl, HTTPS + bearer token──► Cloudflare Tunnel ──► safe-grokbot-bridge
                                                                         │
                         ┌───────────────────────────────────────────────┤
                         │ read-only files and git in the folders you allow
                         │ tmux sessions: live Claude Code / Codex / Hermes terminals you can attach to
                         │ headless sessions: claude -p, codex exec, hermes chat -Q
                         └ sessions already running outside tmux (read-only, or experimental input)
```

## Features

- **Agent sessions.** List, start, message and stop Claude Code, Codex CLI and Hermes Agent
  sessions, or any other terminal program in tmux.
  - **tmux:** the agent runs in a live terminal you can attach to at any time
    (`tmux attach -t …`).
  - **Headless:** every message is one non-interactive run, continued by the agent's session id.
  - **Start options:** model, reasoning effort, permission bypass (only where you allow it),
    resume of an earlier conversation, and `keep_alive: false` to close a session once it has
    answered.
  - **Reliable state:** state comes from Claude Code's own status, per-session turn-complete hooks
    and terminal activity, so the client knows when an agent is working, waiting for an answer or
    done. It long-polls instead of guessing.
- **Read-only project access.** Directory listings (respecting `.gitignore`), text files by line
  range, `git status`, `log`, `diff` and `show`. The file endpoints never serve secrets such as
  `.env` files and private keys. Git output is not filtered, so anything committed to a
  repository you expose is visible.
- **Security first.**
  - 256-bit bearer tokens (only their hashes are stored), with per-IP lockout after failed
    attempts and per-token rate limits.
  - Per-folder capabilities (`files`, `git`, `sessions`, `input`, `start`).
  - TLS required on any public address. A self-signed certificate with public-key pinning works
    out of the box, no CA needed.
  - Every request is written to an audit log, and no generic shell endpoint exists.
- **Made for LLM clients.** A complete [OpenAPI 3.1](docs/openapi.json) description. Every
  response says when it is incomplete and what to do next, and a
  [ready-made skill](docs/grok-bot.md) teaches Grok Bot to use it.
- **One binary** for Linux, macOS and Windows. Configuration is one TOML file.

## Quick start

```bash
# 1. Install (or download a binary from the releases page)
cargo install --git https://github.com/janreges/safe-grokbot-bridge

# 2. Create a config with a fresh token (printed once) and your projects folder as a workspace
safe-grokbot-bridge init --workspace ~/projects

# 3. Allow driving agents there: add "input" and "start" to `allow` in the file `init` wrote
#    (Linux: ~/.config/safe-grokbot-bridge/config.toml,
#     macOS: ~/Library/Application Support/safe-grokbot-bridge/config.toml), then review everything
safe-grokbot-bridge check

# 4. Run it, and publish it with a Cloudflare Tunnel (see docs/deployment.md)
safe-grokbot-bridge serve
```

Then [connect Grok Bot](docs/grok-bot.md), or call the API from anything that speaks HTTP.

Tip: start your agents with `safe-grokbot-bridge launch claude` (or `codex`) instead of `claude`.
They then run in tmux with the bridge's turn hooks, so the remote agent can follow and continue
your sessions.

## Using the API

Every request except `GET /healthz` needs `Authorization: Bearer <token>`. All paths are absolute.

```bash
B=https://bridge.example.com; H="Authorization: Bearer $SGB_TOKEN"

curl -sH "$H" $B/v1/info                                  # machine, agents, workspaces, limits
curl -sH "$H" "$B/v1/sessions?path=/home/me/projects/app"  # who is already working there?

# Start Claude Code in tmux with a task, then wait (up to 90 s per call) for the answer
curl -sH "$H" -H 'content-type: application/json' $B/v1/sessions -d @- <<'EOF'
{"cwd": "/home/me/projects/app", "agent": "claude", "prompt": "Run the tests and fix failures."}
EOF
curl -sH "$H" "$B/v1/sessions/sgb-app-claude-3f9a1c?wait=60"   # state, last_message, screen

# Follow up, or answer a permission prompt with keys
curl -sH "$H" -H 'content-type: application/json' $B/v1/sessions/sgb-app-claude-3f9a1c/input \
     -d '{"text": "Also update the changelog."}'
curl -sH "$H" -H 'content-type: application/json' $B/v1/sessions/sgb-app-claude-3f9a1c/input \
     -d '{"keys": ["Enter"]}'

# Read-only access
curl -sH "$H" "$B/v1/files?path=/home/me/projects/app&depth=2"
curl -sH "$H" "$B/v1/files/content?path=/home/me/projects/app/README.md"
curl -sH "$H" "$B/v1/git/log?dir=/home/me/projects/app&limit=10"
```

| Endpoint | Purpose |
|---|---|
| `GET /v1/info` | Machine, version, agents, workspaces and their capabilities, limits |
| `GET /v1/files`, `GET /v1/files/content` | Directory listing, text file by line range |
| `GET /v1/git/{status,log,diff,show}` | Read-only git |
| `GET /v1/sessions` | Sessions in the workspaces you may see |
| `POST /v1/sessions` | Start a session (`cwd`, `agent`, `runner`, `prompt`, `model`, `effort`, `bypass_permissions`, `keep_alive`, `resume`) |
| `GET /v1/sessions/{id}?wait=&lines=` | State, last answer, terminal screen; long-poll while working |
| `POST /v1/sessions/{id}/input` | `text` and/or `keys` |
| `DELETE /v1/sessions/{id}` | Stop a session the bridge started |
| `GET /openapi.json` | The OpenAPI document ([also in the repo](docs/openapi.json)) |

### How sessions work

| Runner | When | How you get answers |
|---|---|---|
| `tmux` | Default when tmux is installed. A live terminal UI; a person can attach. | `last_message` from the turn hook (Claude Code, Codex), plus the terminal `screen` |
| `headless` | Without tmux (e.g. native Windows), or on request. One non-interactive run per message. | `last_message`; progress while the turn runs |
| `external` | Sessions started outside tmux, e.g. in a terminal tab. | Claude Code: state always; with `[experimental] external_sessions` also messaging and answers. Codex: experimental. |

Session states: `working`, `waiting` (a permission prompt or question; read `screen` and answer
with `keys`), `idle`, `finished` (closed after its answer with `keep_alive: false`), `exited`,
`failed`, `unknown`.

The bridge types only into panes that close with the agent: sessions it started, and panes
created with the agent as their command. An agent you started from a shell inside tmux is listed
but not controllable, because text sent after the agent exits would reach the shell. `resume`
continues a conversation only in the folder where it was held.

## Security

A token lets its holder make coding agents work on your machine, possibly without permission
prompts if you allow that. Treat it like an SSH key. In short:

- **Tokens:** 256-bit, stored as SHA-256 hashes, compared in constant time. A client IP is
  blocked after repeated failures, and each token has a request rate limit.
- **Network:** plain HTTP only on loopback. Publish it through a Cloudflare Tunnel, optionally
  with an Access service token as a second factor, or enable TLS with key pinning.
- **Folders:** capabilities per folder; access is read-only by default. Paths are canonicalized,
  so `..` and symlinks cannot escape.
- **No shell endpoint:** the bridge never runs arbitrary commands. Git runs with options that keep
  a repository's own config from executing programs, and agents run under their own permission
  systems.

Read the [threat model](docs/design.md#6-security-model) and [SECURITY.md](SECURITY.md).

## Platform support

| | Linux / WSL 2 | macOS | Windows |
|---|---|---|---|
| API, TLS, files, git, headless sessions | ✓ | ✓ | ✓ |
| tmux sessions | ✓ | ✓ | best effort with [psmux](https://github.com/psmux/psmux) |
| External Claude Code sessions | ✓ | ✓ | ✓ |

## Documentation

- [Configuration reference](docs/configuration.md)
- [Deployment: Cloudflare Tunnel, VPS, running as a service](docs/deployment.md)
- [Connecting Grok Bot, with a ready-made skill](docs/grok-bot.md)
- [Design and security model](docs/design.md)
- [OpenAPI document](docs/openapi.json)

## Troubleshooting

- **`agent_exited` when starting a session.** Run the agent once by hand in that folder. It may
  need a login, or the agent's `args` in the config contain a flag it does not know.
- **A new session waits on a "trust this folder" dialog.** That is normal on the first start in a
  folder. Answer with `{"keys": ["Enter"]}`.
- **The bridge runs as a service and cannot find `claude` or `codex`.** Services get a minimal
  `PATH`. Set absolute `command` paths in `[agents.*]`, or set `PATH` in the service definition,
  and run `safe-grokbot-bridge check` in the same environment.
- **A Claude Code session outside tmux does not react to messages (experimental).** Sessions
  that run without permission prompts hold messages from other processes for local approval. Set
  `"crossSessionInbound": "accept"` in that session's Claude Code settings.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)
