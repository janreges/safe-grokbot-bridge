# Configuration

The bridge reads one TOML file:

- `--config <path>`, or
- the `SGB_CONFIG` environment variable, or
- the platform config directory:
  - Linux: `~/.config/safe-grokbot-bridge/config.toml`
  - macOS: `~/Library/Application Support/safe-grokbot-bridge/config.toml`
  - Windows: `%APPDATA%\safe-grokbot-bridge\config.toml`

`safe-grokbot-bridge init` writes a commented starting point. Unknown keys are errors, so a typo
never silently disables a setting. Run `safe-grokbot-bridge check` after every change. It
validates the file and shows what the bridge will expose. The bridge reads the config only at
start, so restart it to apply changes.

## Complete example

```toml
[server]
listen = "127.0.0.1:4765"          # loopback: publish it through a tunnel or reverse proxy
name = "workstation"               # shown to clients; default: the host name
client_ip_header = "cf-connecting-ip"  # trusted only from loopback peers; "" disables
state_dir = "/home/me/.local/share/safe-grokbot-bridge"   # default: platform data dir

[server.tls]                       # required when `listen` is not a loopback address
hostnames = ["bridge.example.com"] # names in the self-signed certificate
# cert = "/etc/letsencrypt/live/bridge.example.com/fullchain.pem"   # or your own certificate …
# key = "/etc/letsencrypt/live/bridge.example.com/privkey.pem"      # … and its key

[[tokens]]
name = "grok"
sha256 = "9f2c…"                   # from `safe-grokbot-bridge token`

[[tokens]]
name = "laptop-scripts"
sha256 = "51ab…"

[limits]
auth_failures = 10
auth_window_secs = 600
auth_block_secs = 900
requests_per_minute = 300
max_sessions = 8

[sessions]
runner = "auto"                    # auto | tmux | headless
tmux = "tmux"                      # tmux-compatible binary, e.g. "psmux" on Windows
# tmux_socket = "sgb"              # use a dedicated tmux server (`tmux -L sgb`)
max_turn_secs = 3600

[experimental]
external_sessions = false

[agents.claude]
args = ["--strict-mcp-config"]

[agents.codex]
env = { CODEX_HOME = "/home/me/.codex" }

[agents.codex-sol]                 # a second Codex profile under its own name
kind = "codex"
env = { CODEX_HOME = "/home/me/.codex.work" }
args = ["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=\"xhigh\""]

[agents.aider]                     # any other terminal program (tmux only)
command = "aider"

[[workspaces]]
path = "/home/me/projects"
allow = ["files", "git", "sessions", "input", "start"]
agents = ["claude", "codex", "codex-sol"]
allow_bypass_permissions = false
deny = ["*.sqlite", "secrets/"]      # `secrets/` hides the root-level folder and its contents

[[workspaces]]
path = "/home/me/projects/client-x"   # nested: more specific rules win here
allow = ["files", "git", "sessions"]
```

## `[server]`

| Key | Default | Meaning |
|---|---|---|
| `listen` | `127.0.0.1:4765` | Address and port. A non-loopback address requires `[server.tls]`; the bridge refuses to start otherwise. |
| `name` | host name | Name reported in `GET /v1/info`. |
| `client_ip_header` | `cf-connecting-ip` | Header holding the real client IP when a proxy on the same machine forwards the request. It is used only when the TCP peer is a loopback address, so remote clients cannot forge it. It must hold a single IP (e.g. `CF-Connecting-IP`, `X-Real-IP`); the bridge logs a warning once when it holds something else, such as an `X-Forwarded-For` list. `""` disables it. |
| `state_dir` | platform data dir | TLS key pair and session turn events. Created with mode `0700` on Unix. A relative path is relative to the config file's folder. |

### `[server.tls]`

Its presence enables TLS.

| Key | Meaning |
|---|---|
| `hostnames` | Names written into the self-signed certificate (default: host name and `localhost`). Clients that pin the key do not check names. |
| `cert`, `key` | PEM files of a certificate chain and its private key. Use both or neither. Without them a self-signed certificate is generated once in `<state_dir>/tls/`, and its pin is logged at start and printed by `check`. Delete that folder to get a new key; the pin then changes. |

## `[[tokens]]`

Every client authenticates with `Authorization: Bearer <token>`. Create tokens with
`safe-grokbot-bridge token --name <name>`: it prints the token once and the snippet with its
SHA-256. Names appear in the audit log. Remove an entry to revoke a token. At least one token is
required.

## `[limits]`

| Key | Default | Meaning |
|---|---|---|
| `auth_failures` | 10 | Failed authentications from one client IP within `auth_window_secs` that block it … |
| `auth_window_secs` | 600 | … counted in this window … |
| `auth_block_secs` | 900 | … for this long (`429` with `Retry-After`). |
| `requests_per_minute` | 300 | Per token. A long-poll counts as one request. |
| `max_sessions` | 8 | Sessions started by the bridge that may run at the same time. |

## `[sessions]`

| Key | Default | Meaning |
|---|---|---|
| `runner` | `auto` | Runner for start requests that do not choose one: `tmux` when available, otherwise `headless`. |
| `tmux` | `tmux` | tmux-compatible binary. |
| `tmux_socket` | none | Use a dedicated tmux server (`tmux -L <name>`). By default the bridge uses your normal tmux server, so `tmux ls` shows its sessions. |
| `max_turn_secs` | 3600 | Timeout of one headless turn; the turn's whole process group is stopped. |

## `[experimental]`

| Key | Default | Meaning |
|---|---|---|
| `external_sessions` | `false` | Let clients message agent sessions the bridge cannot type into: sessions running **outside** tmux, and Claude Code or Codex started from a shell inside a tmux pane. Claude Code sessions get messages through their inbox socket, Codex sessions through `codex queue`, and their answers are read from transcripts. Without it, Claude Code sessions outside tmux are still listed read-only. See the [design](design.md#84-external-sessions-experimental) for the details and limits. |

## `[agents.<name>]`

`claude`, `codex` and `hermes` are built in and need no entry. An entry overrides individual
fields; fields left out keep their defaults. Agent names may contain lowercase letters, digits,
`-` and `_`.

| Key | Meaning |
|---|---|
| `kind` | `claude`, `codex`, `hermes` or `generic`. Built-in names have their own kind. Other names default to `generic` (a terminal program that runs only in tmux and accepts no prompt, model, effort, bypass or resume). |
| `command` | Program, looked up on `PATH`, or an absolute path. Services often have a minimal `PATH`, so prefer absolute paths there. |
| `args` | Added to every launch, interactive and headless. They must be valid in both modes. |
| `env` | Extra environment variables, e.g. `CODEX_HOME` or `CLAUDE_CONFIG_DIR`. |
| `headless_args` | Added to headless runs that do not bypass permissions. |
| `bypass_args` | Added when a client starts a session with `bypass_permissions: true` (needs `allow_bypass_permissions` on the workspace). |

Built-in defaults:

| Kind | `headless_args` | `bypass_args` |
|---|---|---|
| `claude` | `--permission-mode auto` | `--dangerously-skip-permissions` |
| `codex` | `--sandbox workspace-write` | `--dangerously-bypass-approvals-and-sandbox` |
| `hermes` | none (dangerous commands are denied) | `--yolo` |

Interactive sessions get no permission flags unless bypass is requested, so the agent uses its own
default mode (for Claude Code 2.1.3xx that is `auto`). If you always run an agent with certain
flags, for example a model or `--strict-mcp-config`, put them in `args`.

The bridge adds the flags for `model` and `effort` itself:

- Claude Code: `--model`, `--effort`
- Codex: `-m`, `-c model_reasoning_effort="…"`
- Hermes: `-m`, `--reasoning`

## `[[workspaces]]`

| Key | Default | Meaning |
|---|---|---|
| `path` | required | Absolute folder; the workspace covers everything below it. `/` (or a drive root on Windows) means "any folder", which is meant for a dedicated agent machine. |
| `name` | folder name | Label shown to clients; must be unique. |
| `allow` | `["files", "git", "sessions"]` | Capabilities, see below. |
| `agents` | all agents | Agents that may be started here. |
| `allow_bypass_permissions` | `false` | Whether clients may start agents without permission prompts here. |
| `deny` | none | Extra deny patterns for the file endpoints. |

A path belongs to the **most specific** workspace that contains it, after resolving symlinks. A
nested workspace can therefore have stricter or looser rules than its parent, and listings of a
parent leave out what a nested workspace hides.

`start` and `input` require `sessions`, so that a client can see the sessions it starts and
drives.

**Git works on whole repositories.** A git request is allowed only when the repository's root
lies in a workspace with `git`, and no workspace inside the repository forbids it. Exposing just
a subfolder of a repository therefore does not expose its git history; expose the repository
root instead. The places git reads history from must lie in workspaces with `git` as well: a
linked worktree (`git worktree add`) needs its main repository exposed, and a repository whose
`.git` file or object alternates point elsewhere needs that location exposed.

| Capability | Grants |
|---|---|
| `files` | `GET /v1/files`, `GET /v1/files/content` |
| `git` | `GET /v1/git/*` |
| `sessions` | listing sessions and reading their state, answers and screen |
| `input` | sending text and keys; stopping sessions the bridge started |
| `start` | starting new sessions |

### Deny patterns

Files matching a deny pattern are hidden from listings and refused by `/v1/files/content` with
`403 denied`. These patterns always apply:

```
.git  .env  .env.*  *.pem  *.key  *.p12  *.pfx  *.kdbx  id_rsa*  id_ecdsa*  id_ed25519*
.ssh  .gnupg  .aws  .azure  .kube  .docker  .netrc  .npmrc  .pypirc  .git-credentials
.credentials.json  auth.json
```

- A pattern **without** `/` matches any single path component, so `.env` also hides
  `app/.env`, and `.ssh` hides everything inside it.
- A pattern **with** `/` is anchored at the workspace root and also covers everything inside a
  matching directory: `build/*.log`, `private/secrets` (or `private/secrets/`).
- Matching is case-insensitive.

Deny patterns do not filter git history. Anything that was ever committed is visible through the
git endpoints, and agents started in a folder can read what they are allowed to read.

## Examples

**A dedicated agent VM where any folder is fine:**

```toml
[[workspaces]]
path = "/"
allow = ["files", "git", "sessions", "input", "start"]
allow_bypass_permissions = true
```

**Read-only overview of everything, full control only in one folder:**

```toml
[[workspaces]]
path = "/home/me"
allow = ["files", "git", "sessions"]

[[workspaces]]
path = "/home/me/projects/sandbox"
allow = ["files", "git", "sessions", "input", "start"]
```
