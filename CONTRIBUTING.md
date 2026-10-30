# Contributing

Thanks for helping. This project values small, clear, well-tested changes over clever ones.

## Before you start

- Read [docs/design.md](docs/design.md) (architecture, security model, API) and
  [CLAUDE.md](CLAUDE.md) (working principles and module layout, which apply to human and AI
  contributors alike).
- For larger changes, open an issue first, so we can agree on the approach.

## Development

You need stable Rust (1.95 or newer), git, and tmux for the tmux tests.

```bash
cargo build
cargo test                                   # tmux tests skip themselves without tmux
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo run -q -- openapi > docs/openapi.json  # after any API change; a test checks it
```

To try the bridge locally:

```bash
cargo run -- init --config dev.toml --workspace "$PWD"
cargo run -- serve --config dev.toml
```

## Guidelines

- **Keep it simple.** Write the minimum code that solves the problem, keep the module tree flat,
  and avoid abstractions until there is a second user.
- **Security first.** Every endpoint checks authentication and workspace capabilities.
  Subprocesses get argument vectors, never a shell. Validate everything a client controls.
- **Write for LLM clients.** Error messages and notes must be true and actionable, and they must
  say so when a result is incomplete.
- **Test what matters.** Security boundaries, parsing of agent output, and end-to-end paths with
  fake agents. No tests for trivial code.
- **Document user-visible changes** in `CHANGELOG.md` under *Unreleased*, and keep the README and
  `docs/` in sync.

## Commits and pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org/): `feat:`, `fix:`, `docs:`,
  `test:`, `refactor:`, `ci:`, `chore:`. Use the imperative mood ("add", not "added") and make one
  logical change per commit.
- A pull request should pass CI (format, clippy and tests on Linux, macOS and Windows) and
  explain *why* the change is needed.
