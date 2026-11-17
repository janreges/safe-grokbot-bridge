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
name = "second-brain"              # a program that only calls models
sha256 = "51ab…"
models_only = true

[limits]
auth_failures = 10
auth_window_secs = 600
auth_block_secs = 900
requests_per_minute = 300
max_sessions = 8
max_model_requests = 16

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

[models.qwen]                      # the name clients send as `model`
type = "chat"
url = "http://10.0.0.5:8000/v1"
model = "Qwen/Qwen3-32B"
description = "Local Qwen3 32B, fast. Thinking is off unless you turn it on."
defaults = { chat_template_kwargs = { enable_thinking = false } }

[models.embed]
type = "embedding"
url = "http://10.0.0.5:8001/v1"
model = "Qwen/Qwen3-Embedding-4B"
defaults = { dimensions = 1024 }

[models.opus]
type = "chat"
api = "anthropic"
url = "https://api.anthropic.com/v1"
model = "claude-opus-5-5"
api_key = "sk-ant-…"
tokens = ["grok"]

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
| `state_dir` | platform data dir | TLS key pair, session turn events and the texts recently sent to tmux sessions, each in a subfolder created with mode `0700` on Unix. A relative path is relative to the config file's folder. |

### `[server.tls]`

Its presence enables TLS.

| Key | Meaning |
|---|---|
| `hostnames` | Names written into the self-signed certificate (default: host name and `localhost`). Clients that pin the key do not check names. |
| `cert`, `key` | PEM files of a certificate chain and its private key. Use both or neither. Without them a self-signed certificate is generated once in `<state_dir>/tls/`, and its pin is logged at start and printed by `check`. Delete that folder to get a new key; the pin then changes. |

## `[[tokens]]`

Every client authenticates with `Authorization: Bearer <token>`. A request without that header
may carry the token in `x-api-key` instead, which is where Anthropic's client library puts its
API key. Create tokens with `safe-grokbot-bridge token --name <name>`: it prints the token once
and the snippet with its SHA-256. Names appear in the audit log. Remove an entry to revoke a
token. At least one token is required.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | Shown in the audit log and used in a model's `tokens`. |
| `sha256` | required | SHA-256 of the token, from `safe-grokbot-bridge token`. |
| `models_only` | `false` | The token may call the model endpoints and read `GET /openapi.json`, nothing else. Give such a token to a program that only needs models, so that a leaked program key cannot reach folders, git or agents. |

## `[limits]`

| Key | Default | Meaning |
|---|---|---|
| `auth_failures` | 10 | Failed authentications from one client IP within `auth_window_secs` that block it … |
| `auth_window_secs` | 600 | … counted in this window … |
| `auth_block_secs` | 900 | … for this long (`429` with `Retry-After`). |
| `requests_per_minute` | 300 | Per token. A long-poll counts as one request. |
| `max_sessions` | 8 | Sessions started by the bridge that may run at the same time. |
| `max_model_requests` | 16 | Model calls running at the same time. A further call waits up to 10 seconds for a free slot, then gets `429 models_busy`. |

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
| `external_sessions` | `false` | Let clients message agent sessions the bridge cannot type into: sessions running **outside** tmux, and Claude Code or Codex started from a shell inside a tmux pane. Claude Code sessions get messages through their inbox socket, Codex sessions through `codex queue`, and their answers are read from transcripts. Every message starts with a short note saying that it comes from a remote client and asking the agent to answer in that conversation. Without it, Claude Code sessions outside tmux are still listed read-only. See the [design](design.md#84-external-sessions-experimental) for the details and limits. |

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

## `[models.<name>]`

Models the clients may call through the bridge, with stock OpenAI and Anthropic client libraries
and the bridge token as their API key. The table name is the name clients send as `model`.

| Key | Default | Meaning |
|---|---|---|
| `type` | required | `chat`, `vision` (a chat model that also accepts images), `embedding`, `reranker`, `speech-to-text`, `text-to-speech` or `image-generator`. It decides the endpoint that calls the model. |
| `api` | `openai` | `openai` or `anthropic`, the request format the upstream speaks. Only `chat` and `vision` models can be `anthropic`. |
| `url` | required | Base URL of the upstream API with its version prefix, e.g. `http://10.0.0.5:8000/v1` or `https://api.anthropic.com/v1`. `http` or `https`; no query, fragment or user name and password. A trailing `/` is ignored. |
| `model` | the table name | The upstream's name for the model. |
| `api_key` | none | Sent to the upstream (`Authorization: Bearer` for `openai`, `x-api-key` for `anthropic`). Clients never see it. |
| `defaults` | none | Request parameters filled in when a request does not set them, e.g. `chat_template_kwargs` or `dimensions`. |
| `tokens` | every token | Names of `[[tokens]]` that may call the model, typically for a paid model. |
| `description` | none | Free text shown in the model list: what the model is good for. |

**Names.** Lowercase letters, digits, `-` and `_`, starting with a letter or digit, up to 64
characters. A `.` is not allowed, because TOML reads `[models.gpt-4.1]` as nested tables: name
such a model `gpt-4-1` and set `model = "gpt-4.1"`.

**Defaults.** The client's value always wins. Objects are merged key by key at every depth;
arrays and other values are replaced whole. A default `chat_template_kwargs = {
enable_thinking = false }` therefore stays when a client sends another key in
`chat_template_kwargs`, and gives way when the client sends `enable_thinking` itself. Defaults
are per model: set `dimensions = 1024` only on embedding models that support Matryoshka
dimensions, because others reject the parameter. For `speech-to-text` models the defaults must be
strings, numbers or booleans, because they are sent as form fields (`language = "cs"`).

`check` asks every model's upstream once whether it is reachable, accepts the key and knows the
model, and reports problems as warnings: a home GPU server may be off while `check` runs.

> **The config file holds keys now.** Keep it, and every copy of it (backups, examples with real
> keys), outside the workspaces, and readable only by you (`chmod 600`). The bridge never serves
> its own config file through the file endpoints, and `check` warns when the file lies in a
> workspace or others can read it. A token that may start or drive agents can still read it
> through an agent, because agents run as your user: such a token is worth as much as an SSH key,
> and with models configured it is also worth money at the paid providers behind them.

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
| `download` | `GET /v1/files/download`: files of any kind, binary ones included, up to 96 MB |
| `upload` | `POST /v1/files/upload`: adding files to the workspace's incoming folder |

### The incoming folder

Uploads go to `<workspace>/.sgb-inbox/`, never anywhere else, under names the bridge generates
(`2026-11-19.14-32-53.k7q2.zip`), and never replace a file. The response tells the client each
file's path, which it passes to an agent. The bridge creates the folder on the first upload (mode
`0700` on Unix) and puts a `.gitignore` with `*` into it, so git never offers uploads for a
commit. For uploads next to one project, add a workspace for that project: the most specific
workspace of the client's `dir` decides. A workspace at `/` takes no uploads.

The bridge never deletes uploads and has no quota: agents or you remove what is no longer
needed. `check` warns when the folder is a link, is not writable, or can be written by others.

> **Where not to allow `upload`.** Never in a folder that a web server publishes, or that a tool
> scans for files to run (plugin, start-up or drop-in folders). On Windows, and on mounts
> without file modes (a Windows drive inside WSL, exFAT, SMB), a file's extension decides what
> opening it does, and the client chooses the extension; there the protection is only the
> dedicated folder and the generated name.

`download` is separate from `files` on purpose: `files` reads only text, so binary data in a
broad workspace, such as a browser's cookie database, stays out of reach unless you allow
`download`. Deny patterns apply to downloads as well.

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

**Passing files between the bot and the agents in one project:**

```toml
[[workspaces]]
path = "/home/me/projects/app"
allow = ["files", "git", "sessions", "input", "start", "download", "upload"]
```

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

**Models on a vLLM server at home:**

```toml
[models.qwen]
type = "chat"
url = "http://10.0.0.5:8000/v1"
model = "Qwen/Qwen3-32B"
defaults = { chat_template_kwargs = { enable_thinking = false } }

[models.embed]
type = "embedding"
url = "http://10.0.0.5:8001/v1"
model = "Qwen/Qwen3-Embedding-4B"
defaults = { dimensions = 1024 }

[models.rerank]
type = "reranker"
url = "http://10.0.0.5:8002/v1"
model = "Qwen/Qwen3-Reranker-4B"

[models.whisper]
type = "speech-to-text"
url = "http://10.0.0.5:8003/v1"
model = "openai/whisper-large-v3"
defaults = { language = "cs" }
```

**A paid model only one token may call:**

```toml
[[tokens]]
name = "second-brain"
sha256 = "51ab…"
models_only = true

[models.gpt-4-1]                   # `.` is not allowed in the table name
type = "chat"
url = "https://api.openai.com/v1"
model = "gpt-4.1"
api_key = "sk-…"
tokens = ["second-brain"]
```
