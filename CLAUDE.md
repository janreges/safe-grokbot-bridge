# CLAUDE.md

Guidance for AI coding agents (Claude Code, Codex, …) working in this repository.

## What this project is

`safe-grokbot-bridge` is a small, security-focused HTTP daemon written in Rust. It lets a remote AI
agent, primarily a [Grok Bot](https://x.ai) running on its cloud computer, work with a developer's
own machines:

- browse configured folders and their git history (read-only),
- see the coding agents (Claude Code, Codex CLI, Hermes Agent) running there,
- send them messages and read their answers,
- start new agent sessions in an allowed folder, in tmux or headless, and stop them.

One binary runs per machine as a daemon. It is reached through a Cloudflare Tunnel, or directly on
a public IP using a self-signed certificate with public-key pinning. Every request needs a bearer
token. What the client may touch is defined per folder in a TOML config file.

**Read [`docs/design.md`](docs/design.md) before changing behaviour.** It is the source of truth for
the architecture, the security model and the API.

## Working principles

These four principles are adapted from the community "Karpathy guidelines" for LLM coding agents.
Follow them on every task.

1. **Think before coding.** State your assumptions. If a request has more than one reasonable
   reading, name them instead of silently picking one. Say so when there is a simpler approach.
   When something is unclear, stop and ask.
2. **Simplicity first.** Write the minimum code that solves the problem. No speculative features,
   no single-use abstractions, no configurability nobody asked for, no handling of impossible
   states. If 200 lines could be 50, rewrite them.
3. **Surgical changes.** Touch only what the task needs and match the surrounding style. Don't
   refactor or reformat unrelated code; mention it instead. Remove only the dead code your own
   change created.
4. **Goal-driven execution.** Turn the task into verifiable success criteria, such as a failing
   test or a `curl` call against a running bridge. Loop until they pass, and verify before
   claiming success.

Project-specific rules:

- **Pragmatic Rust.** Keep the module tree flat and the call paths short, so a bug can be traced
  in one or two files. Prefer plain functions and structs to traits; add a trait only when two
  real implementations exist. Prefer `std`, and add a dependency only when it removes real
  complexity.
- **Security is the product.** Every endpoint goes through authentication and a workspace
  capability check. Never add a generic shell or file-write endpoint. Never log tokens. Spawn
  subprocesses with an argument vector, never through a shell. Validate every client-controlled
  value that reaches a command line, and use `--` / `--end-of-options` to end option parsing.
- **External CLIs change weekly.** Claude Code, Codex, Hermes and tmux evolve fast. Keep
  per-agent knowledge in `src/agents.rs`, keep integrations thin, and fail with an error that says
  what to check.
- **The API's users are LLM agents.** Responses must explain themselves. Say explicitly when
  output was truncated or a list is empty, and make error messages say what to do next. Doc
  comments on API types and handlers become the OpenAPI descriptions, so write them for that
  reader.

## Commands

```bash
cargo build                                    # debug build
cargo run -- check --config dev.toml           # validate a config and the environment
cargo run -- serve --config dev.toml           # run the bridge
cargo test                                     # all tests (tmux tests skip when tmux is missing)
cargo clippy --all-targets -- -D warnings      # lint, must be clean
cargo fmt                                      # format
cargo run -q -- openapi > docs/openapi.json    # regenerate the committed OpenAPI document
```

A test fails when `docs/openapi.json` is out of date, so regenerate it after any API change.

## Layout

| Path | Purpose |
|---|---|
| `src/main.rs` | CLI entry point and subcommands |
| `src/config.rs` | Config file types, defaults, validation, `init` template |
| `src/server.rs` | HTTP and TLS listeners, router, graceful shutdown |
| `src/tls.rs` | Self-signed certificate generation, persistence, key pin |
| `src/auth.rs` | Bearer tokens, auth-failure lockout, rate limiting, client IP |
| `src/api.rs` | HTTP handlers, request/response types, OpenAPI document |
| `src/error.rs` | `ApiError`: stable error codes and messages for HTTP responses |
| `src/paths.rs` | Mapping paths to workspaces, capability checks, deny patterns |
| `src/files.rs`, `src/git.rs` | Read-only file and git access |
| `src/agents.rs` | Everything agent-specific: command lines, flags, output parsing |
| `src/tmux.rs` | Thin wrapper around the tmux CLI |
| `src/sessions.rs` | Session registry: list, start, input, output, stop, keep-alive |
| `src/headless.rs` | Non-interactive agent turns (`claude -p`, `codex exec`, `hermes chat -Q`) |
| `src/external.rs` | Experimental access to agent sessions running outside tmux |
| `src/hook.rs` | `hook` subcommand: agent turn-complete events |
| `src/procs.rs`, `src/exec.rs` | Process inspection and subprocess helpers |
| `tests/` | Integration tests using fake agents |
| `skills/safe-grokbot-bridge/` | Agent skill for setting up the bridge; `.claude-plugin/marketplace.json` publishes it to Claude Code and Codex |
| `skills/safe-grokbot-bridge-client/` | Skill for Grok Bot and other clients: using the API and connecting new machines |

## Testing

Test what matters: security boundaries (auth, lockout, path resolution, deny rules, TLS), parsing
of agent output, command-line construction, and one end-to-end path per session runner, using fake
agent scripts. Don't write tests for trivial code. Tests that need tmux skip themselves when it is
missing, and Unix-only tests are `#[cfg(unix)]`.

## Conventions

- English everywhere: code, comments, docs and commit messages.
- Comments explain *why*, not *what*.
- `anyhow` in CLI and startup code; `ApiError` (stable `code` plus a helpful `message`) in handlers.
- Commits follow [Conventional Commits](https://www.conventionalcommits.org/) (`feat:`, `fix:`,
  `docs:`, `test:`, `refactor:`, `ci:`, `chore:`). One logical change per commit, in the imperative
  mood. The repository is public, so every message must make sense to a reader with no other
  context.
- Record user-visible changes in `CHANGELOG.md` under *Unreleased*.
- Keep `README.md`, `docs/`, `docs/openapi.json` and the agent skill in `skills/` in sync with the
  code.
