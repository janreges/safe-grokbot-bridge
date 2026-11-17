# safe-grokbot-bridge: design

Status: accepted for v0.1. This document describes what the bridge does, how it is built and why.
Keep it in sync with the code.

## 1. Purpose

Developers who live in Claude Code, Codex CLI or Hermes Agent usually have several machines,
each with dozens of project folders and a few agent sessions running in terminals. A remote
orchestrating agent such as a **Grok Bot** (xAI) runs on its own cloud computer. It can plan,
review and coordinate, but it cannot see or drive the work happening on those machines.

`safe-grokbot-bridge` is the missing piece: a small daemon on each machine that exposes a
narrow, authenticated HTTP API so that a remote agent can:

1. read the folders it was given access to, and their git history;
2. see which coding-agent sessions run in those folders and in what state;
3. send a session a message or keystrokes, and read its answer;
4. start a new session in an allowed folder, in tmux (a live terminal the human can attach to)
   or headless (one non-interactive turn per message), and stop it;
5. call the models configured on the machine: local LLMs, embedding, reranking, speech and
   image models on vLLM or similar servers, and paid frontier APIs through the owner's keys;
6. pass files both ways: upload files into a workspace's incoming folder, and download files of
   any kind (§10).

The bridge never executes arbitrary commands on behalf of the client. All work on the machine is
done by the coding agents, under their own permission systems, and the bridge decides which agent
may run where and with which flags.

Models are the fifth purpose because the client writes programs too, for example a "second
brain" built on an LLM, embeddings and semantic search. Grok Bot's cloud computer reaches only
HTTPS hostnames on port 443 (§2), so the owner's home models are out of its reach, and the paid
APIs need keys the bot should not hold. The bridge is already reachable and already
authenticates the client, so it becomes the one door to the models as well (§9).

### Non-goals (v0.1)

- No shell and no generic file-write endpoint. An upload only adds new files, under names the
  bridge generates, to a workspace's incoming folder (§10). Other changes happen through agents.
- No MCP endpoint. Grok Bot reaches HTTP APIs through `curl` on its cloud computer, and other
  clients can use the same REST API.
- No outbound notifications. Clients long-poll a session instead. A webhook to Grok routines
  (`https://api2.cursor.sh/automations/webhook/<id>`) is planned (§16).
- No multi-machine federation. Each machine runs its own bridge, and the client keeps a list.
- No generic HTTP proxy. A client names a model, never a URL, and reaches only the upstream paths
  of a fixed table (§9).
- No translation between the OpenAI and the Anthropic request formats, and no load balancing,
  fallbacks, retries, caching, usage accounting or budgets for models. A LiteLLM proxy does that
  and can itself be configured as a model's upstream.

## 2. Constraints that shaped the design

These facts were verified in October 2026:

- **Grok Bot's cloud computer can only open HTTPS connections to hostnames on port 443.**
  Outbound traffic passes an SNI-based gateway, so plain HTTP and non-443 ports fail. The bridge
  must therefore be reachable as `https://<hostname>/` on port 443, either through a Cloudflare
  Tunnel or directly with TLS.
- Grok Bot stores API tokens in a *secure secret card*. The value is exported as an environment
  variable to every shell session on the cloud computer and is not shown in chat. All of a user's
  bots share that computer, so a token there is shared by all of them.
- Grok Bot is xAI's agent product, operated by Cursor: its cloud computers run in Cursor's cloud,
  routines are triggered through Cursor's webhook endpoint, and custom MCP connectors are called
  from Cursor's backend, not from the cloud computer. Plain REST over `curl` from the cloud
  computer is the simplest reliable path.
- Cloudflare's proxy read timeout is 125 s, so long-polling is capped at 90 s.
- Claude Code (2.1.3xx) offers `claude agents --json` as the supported way to read session state
  from outside. Per-session hooks can be injected with `--settings`, and every session binds an
  inbox socket that local scripts may post to (cross-session messaging).
- Codex CLI (0.16x) offers `codex exec --json`, `codex exec resume`, a `notify` program called
  on every completed turn, and `codex queue` to queue a message for an existing thread.
- Hermes Agent (0.21) offers `hermes chat -Q` (quiet, scriptable) with `--resume`.
- There is no supported way to type into a terminal UI running outside a multiplexer. **tmux is
  the only uniform way to drive a live interactive session of any agent.** On native Windows the
  tmux-compatible `psmux` is supported on a best-effort basis.

## 3. Deployment topologies

```
Grok Bot cloud computer ── curl, HTTPS :443, Bearer token ──► Cloudflare edge
                                                                 │ Cloudflare Tunnel
                                                                 ▼
                     developer machine:  cloudflared ──► safe-grokbot-bridge (127.0.0.1:4765)
                                                           │      │
                                                           │      ├─ tmux ─► claude / codex / hermes (TUI)
                                                           │      ├─ headless ─► claude -p / codex exec / hermes chat -Q
                                                           │      └─ external (experimental) ─► running sessions outside tmux
                                                           ├─ read-only files and git in allowed folders
                                                           ├─ uploads into a workspace's .sgb-inbox/, downloads
                                                           └─ model calls ─► configured upstreams (vLLM, paid APIs)
```

1. **Cloudflare Tunnel (recommended).** The bridge listens on `127.0.0.1`. `cloudflared` runs on
   the same machine and publishes a hostname. Nothing extra is installed on the client side, and
   no inbound port is opened. A Cloudflare Access service-token policy can be added as a second
   factor, because the client simply sends two more headers.
2. **Direct TLS on a public IP (VPS, port forward).** The bridge listens on a public address with
   TLS (HTTP/1.1). By default it generates a self-signed certificate once, stores it, and prints its
   public-key pin. Clients pin it with `curl --pinnedpubkey "sha256//<pin>"`, which gives
   certificate-authority-free authentication of the server. A user-provided certificate (for
   example Let's Encrypt) can be configured instead. For Grok Bot the port must be 443.
3. **Private network (LAN, Tailscale, WireGuard).** Same as 2. TLS is still required on any
   non-loopback address.

The bridge refuses to start when it would serve plain HTTP on a non-loopback address. A bearer
token sent in clear text over the internet is a full compromise.

## 4. Architecture

One binary, one process, one TOML config. Built with tokio, axum 0.8, and utoipa for an
OpenAPI 3.1 document generated from the code. The module tree is flat; see the layout table in
`CLAUDE.md`. Key flows:

- **Request path:** listener (plain or TLS) → client-IP resolution → auth middleware (lockout,
  token check, per-token rate limit) → handler → path/workspace resolution and capability check
  → domain module (`files`, `git`, `sessions`) → JSON response. Every authenticated request is
  logged with the token name, client IP, method, path, status and duration.
- **State:** in memory, except for files in the state directory (TLS key pair and per-session
  turn events). Sessions the bridge starts in tmux outlive a bridge restart and are rediscovered
  by name. Headless sessions are bridge-owned records; their agent conversations stay resumable
  by agent session id.
- **Subprocesses** (git, tmux, agents) are always spawned with an argument vector and never
  through a shell. They have timeouts and output caps, and child processes are killed when their
  owner goes away.
- **Model calls** are the only outbound requests. They go to the upstream URLs in the config,
  through one shared HTTP client (reqwest on rustls with `ring`, like the server), which ignores
  proxy variables from the environment and never follows a redirect (§9).
- **Writes:** outside its state directory, the running bridge writes only uploads, into a
  workspace's incoming folder (§10).

State directory: `[server] state_dir`, defaulting to the platform data dir
(`~/.local/share/safe-grokbot-bridge` on Linux,
`~/Library/Application Support/safe-grokbot-bridge` on macOS,
`%LOCALAPPDATA%\safe-grokbot-bridge` on Windows). Its subfolders, which hold the TLS key, turn
events and the texts recently sent to tmux sessions, are created with mode `0700` on Unix.

## 5. Configuration

Default location: `~/.config/safe-grokbot-bridge/config.toml` (platform config dir), or
`--config <path>`. `safe-grokbot-bridge init` writes a commented template with a fresh token.
Unknown keys are rejected, so typos fail loudly.

```toml
[server]
listen = "127.0.0.1:4765"             # loopback: put cloudflared or a reverse proxy in front
# name = "workstation"                # shown to clients; defaults to the hostname
# client_ip_header = "cf-connecting-ip"   # trusted only when the TCP peer is loopback
# state_dir = "/var/lib/sgb"

# Required when `listen` is not a loopback address.
# [server.tls]
# hostnames = ["bridge.example.com"]  # SANs for the self-signed certificate
# cert = "/etc/ssl/bridge.pem"        # optional: use your own certificate chain …
# key  = "/etc/ssl/bridge.key"        # … and private key instead

[[tokens]]
name = "grok"                         # appears in the audit log
sha256 = "<64 hex chars>"             # SHA-256 of the token; the token itself is never stored

[[tokens]]
name = "second-brain"                 # a token for a program
sha256 = "<64 hex chars>"
models_only = true                    # may call the models and nothing else

[limits]
auth_failures = 10                    # failed auths per client IP within the window …
auth_window_secs = 600
auth_block_secs = 900                 # … block that IP for this long
requests_per_minute = 300             # per token
max_sessions = 8                      # live sessions started by the bridge
max_model_requests = 16               # model calls running at once

[sessions]
runner = "auto"                       # default for new sessions: auto | tmux | headless
tmux = "tmux"                         # tmux-compatible binary (psmux on Windows)
# tmux_socket = "sgb"                 # a dedicated tmux server (`tmux -L sgb`)
max_turn_secs = 3600                  # headless turn timeout

[experimental]
external_sessions = false             # message sessions running outside tmux (see §8.4)

[agents.claude]                       # built-in; listed here only to override defaults
command = "claude"
args = []                             # added to every launch
env = {}                              # extra environment variables
headless_args = ["--permission-mode", "auto"]          # headless, unless bypass is requested
bypass_args = ["--dangerously-skip-permissions"]       # when bypass_permissions = true

[agents.codex-sol]                    # an extra agent: a second Codex profile
kind = "codex"
env = { CODEX_HOME = "/home/me/.codex.mnt" }
args = ["-m", "gpt-6.1-sol", "-c", "model_reasoning_effort=\"xhigh\""]

[models.qwen]                         # the name a client sends as `model`
type = "chat"
url = "http://10.0.0.5:8000/v1"       # base URL of the upstream API, with its version prefix
model = "Qwen/Qwen3-32B"              # the upstream's name for it (default: the table name)
description = "Local Qwen3 32B, fast. Thinking is off unless you turn it on."
defaults = { chat_template_kwargs = { enable_thinking = false } }

[models.opus]
type = "chat"
api = "anthropic"
url = "https://api.anthropic.com/v1"
model = "claude-opus-5-5"
api_key = "sk-ant-..."
tokens = ["grok"]                     # only these tokens may call it

[[workspaces]]
path = "/home/me/projects"            # this folder and everything below it
name = "projects"                     # optional label; defaults to the folder name
allow = ["files", "git", "sessions", "input", "start"]
agents = ["claude", "codex", "codex-sol"]   # may be started here; default: all
allow_bypass_permissions = false      # may clients start agents without permission prompts?
deny = ["*.sqlite"]                   # extra deny patterns for file reads (defaults always apply)
```

### Agents

Three agent kinds are built in: `claude`, `codex` and `hermes`. They know how to pass a model,
reasoning effort, initial prompt, resume id and turn-complete hook, and how to run headless. Each
is available under its own name with the defaults below. Any other `[agents.<name>]` entry
either sets `kind` to reuse a built-in kind (e.g. a second Codex profile) or is a **generic** agent:
a TUI program that can only run in tmux, with no model, effort, prompt or headless support.

| Kind | `headless_args` default | `bypass_args` default |
|---|---|---|
| claude | `--permission-mode auto` | `--dangerously-skip-permissions` |
| codex | `--sandbox workspace-write` | `--dangerously-bypass-approvals-and-sandbox` |
| hermes | *(none; dangerous commands are denied)* | `--yolo` |

`args` and `env` are always applied. The bridge itself never adds a permission-relaxing flag
unless the client asked for `bypass_permissions` **and** the workspace allows it.

### Workspaces and capabilities

A workspace is an absolute folder; it covers everything below it. `path = "/"` (or a drive root)
means "any folder", which is intended for a dedicated agent machine. A path is resolved by
canonicalizing it, which follows symlinks, and picking the **most specific** workspace that
contains it. Nested workspaces can therefore narrow or widen access.

| Capability | Grants |
|---|---|
| `files` | list directories, read text files |
| `git` | `git status`, `log`, `diff`, `show` |
| `sessions` | list sessions and read their output |
| `input` | send text or keys to sessions, stop sessions the bridge started |
| `start` | start new sessions |
| `download` | download any regular file, binary ones included |
| `upload` | add files to the workspace's incoming folder, `.sgb-inbox/` |

The default is `["files", "git", "sessions"]`, which is read-only. Controlling agents is an
explicit opt-in, and so are `download` and `upload`. `files` alone still reads only text, so
binary data in a broadly configured workspace (a browser's cookie database, say) stays out of
reach unless `download` is allowed.

**Deny patterns** hide secrets from `files` endpoints, both in listings and in reads. Defaults:
`.git`, `.env`, `.env.*`, `*.pem`, `*.key`, `*.p12`, `*.pfx`, `*.kdbx`, `id_rsa*`, `id_ecdsa*`,
`id_ed25519*`, `.ssh`, `.gnupg`, `.aws`, `.azure`, `.kube`, `.docker`, `.netrc`, `.npmrc`,
`.pypirc`, `.git-credentials`, `.credentials.json`, `auth.json`.

- A pattern without `/` matches any path component.
- A pattern with `/` is anchored at the workspace root and matches the path or any of its
  ancestors, so a denied folder also hides its contents. A trailing `/` is ignored.
- Each listed entry is judged by the most specific workspace it belongs to, so a listing of a
  parent never shows what a nested workspace hides.

Git history is not filtered: anything ever committed is visible through `git` endpoints.

`start` and `input` require `sessions`, because the API answers both with the session.

**Git is authorized per repository.** Git output covers a whole repository, not a folder.
`dir` must therefore be in a git repository whose root lies in a workspace with `git`, and no
workspace nested inside that repository may lack `git`.

### Models

One `[models.<name>]` table per model; the table name is the name clients use.

| Key | Required | Meaning |
|---|---|---|
| `type` | yes | `chat`, `vision`, `embedding`, `reranker`, `speech-to-text`, `text-to-speech` or `image-generator` |
| `api` | no | `openai` (default) or `anthropic`. Only `chat` and `vision` models may be `anthropic` |
| `url` | yes | Base URL of the upstream API including its version prefix. `http` or `https`, no query, fragment or user info. A trailing `/` is ignored |
| `model` | no | The upstream's model name. Default: the table name |
| `api_key` | no | Sent to the upstream. Without it the upstream is called without credentials |
| `defaults` | no | Request parameters filled in when the client did not send them (§9.1) |
| `tokens` | no | Names of `[[tokens]]` that may call the model. Default: every token. Unknown names and an empty list are config errors |
| `description` | no | Free text for the caller: what the model is good for |

Model names follow the rule for agent names (lowercase letters, digits, `-` and `_`, starting
with a letter or digit), with up to 64 characters. A `.` is not allowed, because TOML would read
`[models.gpt-4.1]` as nested tables; such a model gets a name like `gpt-4-1` and
`model = "gpt-4.1"`. The `defaults` of a `speech-to-text` model must be strings, numbers or
booleans, because they are sent as form fields.

`models_only = true` on a token limits it to the model endpoints and `GET /openapi.json`.
`max_model_requests` in `[limits]` (default 16) is the number of model calls running at once.

## 6. Security model

**Assets:** the developer's machine and accounts. A bridge token lets its holder make coding
agents run on that machine, possibly with permission prompts disabled. Treat the token like an SSH
key.

| Threat | Mitigation |
|---|---|
| Internet scanner finds the endpoint | 256-bit random token required on every route except `/healthz`; constant-time comparison of SHA-256 digests; only hashes are stored in config |
| Brute force / credential stuffing | per-IP lockout after N failures (default 10 per 10 min → blocked 15 min) with `Retry-After`; bounded memory (10 000 addresses; when all of them are blocked, the blocks stay and new addresses are not tracked); with 256-bit tokens brute force is infeasible anyway |
| Spoofed client IP to dodge lockout | `CF-Connecting-IP` is honoured only when the TCP peer is loopback |
| Token sniffing | TLS mandatory on non-loopback listeners; self-signed with pinning or user certificate |
| Runaway client loop | per-token request rate limit; `max_sessions` cap; headless turn timeout |
| The bridge used as a proxy into the private network | upstream URLs come from the config only; a client picks a model by name; the path is one of a fixed table; redirects are not followed; proxies from the environment are not used |
| An upstream API key leaks through the API | keys are never returned or logged; only a fixed set of headers is forwarded; the file endpoints never serve the bridge's config file (below) |
| The bridge token reaches an upstream | the client's `Authorization` and `x-api-key` headers are never forwarded |
| A runaway program burns money or the GPU | per-token rate limit; `max_model_requests`; `tokens` on paid models; failed calls are marked as not worth retrying (`x-should-retry: false`) |
| A program's key is stolen | `models_only` tokens cannot reach folders, git or agents |
| An upload runs by itself, or replaces a file | uploads go only to `<workspace>/.sgb-inbox/`; the bridge generates the name and the client controls nothing but a sanitized extension; files are created exclusively and, where the file system has modes, with `0600` |
| An upload is redirected through a link | the incoming folder is refused when it is a symlink or resolves outside the workspace root; files and the `.gitignore` are created exclusively, which does not follow a link at the final name |
| Binary data read from a broad workspace | `download` is a separate capability, off by default; deny patterns and the checked file handle apply |
| A full disk | 96 MB per request, 20 files per upload, 4 transfers at once; there is no quota |
| Path traversal / symlink escape | canonicalize then prefix-check against canonical workspace roots; a file is opened without following a final symlink or blocking on a FIFO, and the open handle must still be the authorized file (Linux: the handle's own path; elsewhere: the path resolved again and compared with the handle), so swapping a file or folder during a request is refused |
| Secret files exfiltrated via file API | default deny patterns plus user patterns |
| Argument injection into git / agents / tmux | argv only, no shell; `--end-of-options` for git revisions; strict validation of revisions, model names, key names; prompts go through stdin or as the final argument after the options; messages for `codex queue` are one `--message=…` argument; the tmux start directory is escaped for tmux's format language (`#(…)` there would run a command) and arguments ending in `;` are escaped |
| A project's own program run instead of the agent | an agent's command is resolved to an absolute path before the agent starts in the project folder, and relative `PATH` entries (`.`, `bin`) are skipped, so a repository cannot put its own `claude` in the way |
| Malicious repo git config (fsmonitor, external diff, textconv, filter drivers, submodules, transports) | git runs with `--no-pager`, `-c core.fsmonitor=false`, `--no-ext-diff`, `--no-textconv`, `--ignore-submodules=all`, `-c diff.submodule=short`, every configured filter driver blanked (`-c filter.<name>.clean=` …; git is not run when the filter list cannot be read completely, or when a driver's name contains `=` and so cannot be overridden), all transports disabled (`GIT_ALLOW_PROTOCOL=` and `GIT_NO_LAZY_FETCH=1`, so a partial clone cannot fetch through a configured `core.sshCommand`), the bridge's own `GIT_*` variables removed and `GIT_OPTIONAL_LOCKS=0` |
| Git output from outside the allowed folders | the repository root must be in a workspace with `git`, and so must every place its history is read from: the git directory, the common directory of linked worktrees, the object store, its alternates and links inside it (loose-object folders and objects, `pack` and pack files, `info` and its commit graphs; a `.git` file can point anywhere; see §5). A store that cannot be listed completely is refused, and so are alternates in Git's quoted form |
| Text reaching a shell instead of an agent | the bridge types only into panes where nothing but the agent can receive it: its own sessions, panes whose program is the file of the agent's configured command, and panes where no process between the pane and the agent can read the terminal after its child exits. Only a shell running one simple command (`sh -c "claude"`) ends with its program; an interactive shell, a script or a compound command (`claude; exec bash`) does not. An agent is its executable, or a script an interpreter runs (`vim codex` is not Codex). A pane takes no typed input while `synchronize-panes` is on in its window and the window has other panes, because tmux would copy every key to them; with psmux on Windows, neither does a session with several panes, because psmux addresses the active pane. The pane's process is checked again right before the paste, before every Enter and before every key, so a pane respawned meanwhile gets nothing. A shell-hosted agent is read-only, or takes text through its own messaging when experimental external sessions are on |
| Reading conversations from other folders | `resume` requires the conversation to have been held in the requested `cwd`: the first `cwd` of the Claude transcript (not its lossy project folder name), the Codex rollout's `cwd`, the Hermes session record; Hermes is pinned to `cwd` with `--in`. External sessions show transcripts and take messages only when their conversation began in their current folder (a Claude conversation resumed or moved there with `/cd` does not), and a running Codex that resumed a conversation from another folder is not matched to it. A compressed Hermes conversation, which Hermes would resume in a newer session, is refused |
| Unwanted permission bypass | `bypass_permissions` needs a per-workspace opt-in; a Hermes conversation saved in YOLO mode, which Hermes restores on resume, can only be resumed with `bypass_permissions` |
| Slow-loris / connection exhaustion on a public TLS port | HTTP/1.1 only; per-connection task with a 10 s TLS handshake timeout; first request within 15 s; idle connections closed after 150 s; header read timeout; at most 512 connections, 32 per client address, where an IPv6 client's /64 network counts as one address; `/healthz` answers and rejected requests close their connection |
| Memory exhaustion | file reads only of regular files, capped while reading, at most 8 file operations at once; listings read `.gitignore` files themselves, through a handle that must be a regular file, read up to 1 MiB, never following a final link or blocking on a pipe; agent output read line by line with caps; request bodies capped at 1 MiB, except model calls (32 MiB for JSON, 96 MB for a transcription form), whose bodies are read only after one of the `max_model_requests` slots is taken; uploads and downloads are streamed, at most 4 at once, and a malformed upload form is held in memory at most up to the 96 MB limit |
| Log flooding by unauthenticated clients | rejected requests are logged with a shortened method and path, requests from a blocked address are not logged one by one, and dropped connections are logged at most once a minute with their count |

**Command lines are visible locally.** The first prompt of a tmux session is passed on the
agent's command line, and so are messages queued for external Codex sessions (`codex queue`). On
a machine shared with other local users, they can read these with `ps`. Headless prompts and
input to running tmux sessions go through stdin and paste buffers instead. The last ten texts
sent to each tmux session are also kept in the state folder (mode `0700`), until the session's
data is removed an hour after it ended.

**Listings follow paths.** A file is read through a handle that must still be the authorized
file, but a listing walks folders by path. If someone with write access to a workspace replaces
a subfolder with a link while a deep listing runs, the listing can show names, sizes and times
from the link's target. It never shows file contents.

**The config file is a secret once it holds model keys.** A workspace at `/` or at the home
folder, both of which §5 allows, contains the default config file, and a config kept in a
project folder is inside that project's workspace. Therefore:

- The file endpoints never serve the config file the bridge was started with: a listing leaves
  it out, and reading it answers 403 `denied`. This holds whether or not it contains a key.
- `check` warns when the config file lies inside a workspace, and on Unix when it contains an
  `api_key` and is readable by group or others.
- Owners keep copies of the config (backups, examples with real keys) outside the workspaces.

This protects the keys from tokens that may only read, and from `models_only` tokens. It does
not protect them from a token that may start or drive agents: an agent runs as the bridge's user
and can read what that user can. Such a token is worth as much as an SSH key, and with models
configured it is also worth money at the paid providers behind them.

**Limits of the upload protection,** stated rather than hidden:

- Whoever can write in the workspace is not an adversary here. An agent working there could
  swap the incoming folder for a link between the bridge's check and its write. It gains nothing
  by that: it runs as the same user and can write the same file anywhere itself. The check for a
  link guards against accidents and stale set-ups.
- File modes exist only on Unix file systems. On Windows, and on mounts without modes (a Windows
  drive inside WSL, exFAT, SMB), a file's extension decides what opening it does, and the client
  chooses the extension. The protection there is the dedicated folder and the generated name.
- A program that runs files by extension must not look into the incoming folder. Owners do not
  allow `upload` in a folder that a web server publishes or that a tool scans for files to run.

**Out of scope:** a compromised client (the Grok Bot account and its cloud computer), and
malicious agents or prompt injection inside repositories. Agents keep their own permission
systems, and the bridge does not try to judge prompts. It cannot make an agent's work safe, so
every installation should run in an isolated environment: a virtual machine, a dedicated WSL 2
distribution or a separate computer ([where to run it](deployment.md#where-to-run-it)).

**Defense in depth for Cloudflare deployments:** an Access application with a *Service Auth*
policy bound to one service token. The client adds the `CF-Access-Client-Id` and
`CF-Access-Client-Secret` headers. Unauthenticated requests then never reach the machine.

## 7. HTTP API

JSON over HTTPS. The OpenAPI 3.1 document is served at `GET /openapi.json` and committed as
`docs/openapi.json`. All routes except `/healthz` require `Authorization: Bearer <token>`, or
the token in `x-api-key` when that header is absent (Anthropic's client library sends its API
key there).
Absolute paths are used throughout, so a client can take a session's `cwd` and pass it straight to
the file and git endpoints.

| Method and path | Capability | Purpose |
|---|---|---|
| `GET /healthz` | none | liveness probe, returns `ok` |
| `GET /openapi.json` | any token | API description |
| `GET /v1/info` | any token | machine, version, runners, agents, workspaces, limits |
| `GET /v1/files?path=&depth=&limit=` | files | list a directory; respects the `.gitignore` files from the workspace root down |
| `GET /v1/files/content?path=&start_line=&max_lines=` | files | read a text file by line range |
| `GET /v1/git/status?dir=` | git | `git status --short --branch` |
| `GET /v1/git/log?dir=&limit=&rev=&path=` | git | commit list |
| `GET /v1/git/diff?dir=&rev=&staged=&path=` | git | working-tree or revision diff |
| `GET /v1/git/show?dir=&rev=&path=` | git | one commit with patch |
| `GET /v1/sessions?path=` | sessions | sessions whose cwd is in allowed workspaces (optionally under `path`) |
| `POST /v1/sessions` | start | start a session |
| `GET /v1/sessions/{id}?lines=&wait=` | sessions | session state and output; `wait` long-polls up to 90 s while it works |
| `POST /v1/sessions/{id}/input` | input | send text (`submit` presses Enter) or keys |
| `DELETE /v1/sessions/{id}` | input | stop a session the bridge started |
| `GET /v1/files/download?path=` | download | download a file of any kind, up to 96 MB (§10.2) |
| `POST /v1/files/upload?dir=` | upload | store files in the workspace's incoming folder (§10.1) |
| `GET /v1/inbox?state=&to=&limit=` | any but `models_only` | messages left for Grok Bot, without bodies (§11.4) |
| `GET /v1/inbox/{id}` | any but `models_only` | one inbox message with its body |
| `GET /v1/inbox/{id}/attachments/{name}` | any but `models_only` | an inbox attachment (§11.4) |
| `POST /v1/inbox/{id}/claim` | any but `models_only` | take a message for a bot (§11.2) |
| `POST /v1/inbox/{id}/release` | any but `models_only` | give a message back; only the holder may (§11.2) |
| `GET /v1/models?type=` | any token | the models this token may call (§9.2) |
| `POST /v1/chat/completions` | model's `tokens` | `chat` and `vision` models with `api = "openai"` |
| `POST /v1/messages` | model's `tokens` | `chat` and `vision` models with `api = "anthropic"` |
| `POST /v1/embeddings` | model's `tokens` | `embedding` models |
| `POST /v1/rerank` | model's `tokens` | `reranker` models |
| `POST /v1/audio/transcriptions` | model's `tokens` | `speech-to-text` models (multipart form) |
| `POST /v1/audio/speech` | model's `tokens` | `text-to-speech` models |
| `POST /v1/images/generations` | model's `tokens` | `image-generator` models |

A `models_only` token reaches the model endpoints and `GET /openapi.json`; everything else,
`/v1/info` included, answers 403 `forbidden`.

Errors use one shape: `{"error": {"code": "<stable_code>", "message": "<what happened and what to
do>"}}`, returned with statuses 400, 401, 403, 404, 409, 413, 415, 429 (with `Retry-After`), 500,
502, 503, 504 or 507. Model calls also pass the upstream's own errors through in the upstream's shape
(§9.1).

Results never pretend to be complete. Truncated output says so (`truncated: true` plus a way to
get more, if one exists). A session list whose sources (tmux, `claude agents --json`) failed
carries `warnings`. Advice only the machine's owner can follow is marked as such.

### Starting a session

```json
POST /v1/sessions
{
  "cwd": "/home/me/projects/app",   // existing directory inside a workspace with `start`
  "agent": "claude",                // a configured agent allowed in that workspace
  "runner": "tmux",                 // optional: tmux | headless (default from config)
  "prompt": "Run the tests and fix failures",   // optional first message
  "model": "opus",                  // optional, passed with the agent's model flag
  "effort": "high",                 // optional: low | medium | high | xhigh | max
  "bypass_permissions": false,      // optional, needs allow_bypass_permissions
  "keep_alive": true,               // optional: false = close once the first answer is in
  "resume": "<agent session id>"    // optional: continue an earlier conversation
}
```

### The session object

```json
{
  "id": "sgb-app-claude-3f9a1c",
  "agent": "claude",
  "runner": "tmux",                    // tmux | headless | external
  "cwd": "/home/me/projects/app",
  "workspace": "projects",
  "state": "working",                  // working | waiting | idle | finished | exited | failed | unknown
  "waiting_for": null,                 // e.g. "permission prompt" when state = waiting
  "controllable": true,                // can it take input through the bridge?
  "started_by_bridge": true,
  "keep_alive": true,
  "agent_session_id": "4b8e…",         // Claude session id, Codex thread id or Hermes session id
  "started_at": "2026-10-30T08:00:00Z",
  "last_activity_at": "2026-10-30T08:03:12Z",
  "last_message": "All 42 tests pass …",   // last completed answer; while working, the latest progress message when known
  "last_message_at": "2026-10-30T08:03:10Z",
  "attach_command": "tmux attach -t sgb-app-claude-3f9a1c",
  "screen": "…",                       // GET /v1/sessions/{id} only: last `lines` lines of the terminal
  "error": null
}
```

## 8. Sessions

### 8.1 tmux runner

Used for live, interactive sessions. Requires tmux 3.0 or newer, or psmux on Windows.

- **Start:** `tmux new-session -d -s sgb-<dir>-<agent>-<6hex> -c <cwd> -x 200 -y 50 -e KEY=VAL -- /usr/bin/env -u CLAUDECODE … <argv>`.
  The initial prompt is the agent's positional argument (Claude, Codex) or `chat -q` (Hermes),
  so there is no race with the TUI starting up. tmux limits a command to 16 KiB in all, so a
  first prompt in tmux has at most 8 000 bytes; longer text goes in as input once the session
  runs. Session names are unique, and a human attaches with `tmux attach -t <name>`. A running
  tmux server hands its own environment to new sessions, and it may come from an agent session
  or another profile. So the profile variables (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
  `HERMES_HOME`) are set to the bridge's values, unless the agent's `env` sets them, and the
  variables of an enclosing agent session are removed. An argument ending in `;` is escaped,
  because tmux would read it as a command separator.
- **Targeting:** the bridge remembers the pane id the agent runs in and the pane's process, and
  always targets that pane. Panes someone adds to a bridge session (a split, a new window) are
  ignored. A pane whose process changed (respawned, for example with a shell) no longer runs the
  agent the bridge started, so the session counts as exited.
- **Discovery:** `tmux list-panes -a` plus a process-tree check (`procs.rs`) finds every pane
  that runs a configured agent inside an allowed workspace, including panes the user started.
  Such a pane takes input only when no interactive shell sits between the pane and the agent
  (the agent, or a wrapper script running it, is the pane's program): text sent after the agent
  exits would otherwise reach the shell. A shell-hosted agent is listed read-only; with
  experimental external sessions on, a Claude Code or Codex agent there takes text through its
  own messaging instead (§8.4).
- **Restarts:** bridge-named sessions (`sgb-…`, including those from
  `safe-grokbot-bridge launch`) are adopted at start and on sight, unless an interactive shell
  sits between the pane and the agent. Their `keep_alive` setting, the time of the last input
  and the last ten texts sent to them (Codex's main thread is recognized by them) are kept in
  `<state_dir>/sessions/<id>.json`, so a `keep_alive: false` session is still closed after its
  answer. Turn events and saved data of sessions that ended while the bridge was stopped are
  removed at start, once the retention period has passed.
- **Ids:** a session the bridge started keeps its tmux session name as its id
  (`sgb-<dir>-<agent>-<6hex>`). A pane the user started gets `tmux-<n>` for tmux pane `%<n>`.
  With psmux on Windows, pane ids are not unique across sessions, so only bridge sessions are
  listed there, and every command targets the session by name (`=<name>:`), which routes it to
  that session's server; the buffer for a paste is loaded there too.
- **Text input:** control characters except tab and newline are stripped and CRLF is normalized.
  Then `load-buffer` (from stdin) and `paste-buffer -p -d` deliver a bracketed paste. The bridge
  waits until the pane content changes, then presses `Enter`. Copy mode is cancelled first. One
  request's paste, `Enter` and keys are not interleaved with another request's.
- **Keys:** an allowlist of tmux key names (`Enter`, `Escape`, `Tab`, `BTab`, `Up`, `Down`,
  `Left`, `Right`, `PageUp`, `PageDown`, `Home`, `End`, `Space`, `BSpace`, `DC`, `C-c`, `C-d`) plus
  single printable characters. This lets a client answer permission prompts and menus.
- **Output:** `capture-pane -p -J -S -<lines>`.
- **Turn tracking (hooks):** bridge-started Claude and Codex sessions get a per-session hook that
  calls `safe-grokbot-bridge hook <agent> <events-file>` at the end of every turn:
  - Claude: `--settings '{"hooks":{"Stop":[…command…]}}'`; the payload carries
    `last_assistant_message` and `session_id`.
  - Codex: `-c notify=[…]`; the `agent-turn-complete` payload carries `last-assistant-message`
    and `thread-id`. Codex also reports internal helper threads, such as title generation; the
    session's main thread is the one whose input contains the text the bridge sent.

  The hook appends one JSON line to `<state_dir>/events/<session>.jsonl` and always exits 0, so it
  can never block an agent.
- **State:**
  - Claude: `claude agents --json` (`busy`/`waiting`/`idle` plus `waitingFor`), cached for 5 s
    and refreshed after every start or input. It runs only for listings and Claude sessions; a
    long-poll looks at its session every 2 s. Right after input Claude may still report `idle`,
    so a turn started by the bridge stays `working` until its Stop hook event arrives. If the
    pane is quiet for 15 s and Claude says `idle`, the turn counts as done even without an event.
  - Codex: a turn started by the bridge's input is `working` until its hook event arrives. If the
    pane stays quiet for 15 s first, it is `waiting`, typically an approval prompt.
  - A turn starts when the bridge submits text; its time is taken in milliseconds before the text
    is submitted, so the turn's event always comes later. Text pasted with `submit: false` is a
    draft and starts no turn, and neither do `keys`.
  - Hermes and generic agents: `working` while the pane produced output within the last 3 s
    (`#{window_activity}`), otherwise `idle`. This is an estimate, and the session's `note` says
    so.
  - tmux measures activity per window, so another pane in the agent's window that keeps
    printing makes the agent look busy, also after a Codex turn has ended. A `keep_alive: false`
    session is then closed only once that pane goes quiet.
  - A vanished pane is `exited`, but only when tmux answered: if the pane list cannot be read,
    sessions are reported as `unknown` and the list carries a warning. A session counts as
    vanished only if it was running before tmux was queried, so a start that completes during a
    listing is never mistaken for one. A dead pane kept by `remain-on-exit` counts as exited too.
- **keep_alive = false:** once a turn completes (a hook event after the last text the bridge
  submitted, also for a turn started with keys, or Claude's own idle status after being busy)
  and the session is idle, the bridge stores the final screen, kills the tmux session and marks
  it `finished`. Only Claude Code and Codex report finished turns, so other agents must use the
  headless runner for this. A failed kill is reported instead of being recorded as success. The
  conversation stays resumable through `resume`.

### 8.2 Headless runner

Used when tmux is unavailable (native Windows without psmux) or when the client asks for it. Each
input is one non-interactive turn. The prompt goes through stdin.

| Kind | Command (plus `args`, model, effort, permission args) |
|---|---|
| claude | `claude -p --output-format stream-json --verbose (--session-id <new uuid> \| --resume <id>)` |
| codex | `codex exec --json --skip-git-repo-check [resume <id>] -` |
| hermes | `hermes chat -Q --query-file - --in <cwd> [--resume <id>]` |

Streams are parsed line by line, with a cap per line and per message. The bridge keeps the agent
session id, the latest assistant message (shown as progress while the turn runs) and the final
answer; sending a message clears the previous answer. Claude and Codex must end a turn with a
result event; a process that exits without one counts as failed. On failure the bridge keeps the
error message and the end of stderr. A line longer than 4 MiB is cut; since a cut event cannot
be parsed, the turn then counts as failed with an explanation instead of silently losing the
answer. A turn is killed with its whole process group after `max_turn_secs` or on `DELETE`, and
the deadline covers draining its output. When the agent exits, whatever it left running in its
process group is stopped as well, so nothing outlives the turn unnoticed. Windows has no
process groups: there the turn's processes are collected in a job object, which ends whatever
is left when the agent exits.

Starting a turn checks and changes the session's state under one lock, so concurrent messages
cannot start two turns: the second gets 409 `busy`. The identity of the session's folder is kept
from its start, and every turn enters the folder through a handle checked against it (`fchdir`
on Unix), so a turn never runs in whatever replaced the authorized folder. Likewise,
`max_sessions` is checked and the
new session reserved in one step. With `keep_alive = false` the session becomes `finished` after
its first turn.

### 8.3 Choosing the runner

`runner` in the request, else `[sessions] runner`. `auto` (in either place) means tmux when the
tmux binary works, headless otherwise. Generic agents need tmux.

### 8.4 External sessions (experimental)

Sessions running outside tmux, for example in a plain terminal tab or an IDE.

- **Always (read-only):** Claude sessions from `claude agents --json` whose cwd lies in an allowed
  workspace, with their state, appear with `runner = "external"` and `controllable = false`. A
  client can then see that someone is already working in a folder. Every configured Claude agent
  with its own `CLAUDE_CONFIG_DIR` is queried, so separate Claude profiles are covered.
- **With `[experimental] external_sessions = true`:**
  - Claude: the session becomes controllable. Text is posted to its inbox socket (path from
    `~/.claude/sessions/<pid>.json`, auth token from the published key file) as
    `{"type":"user","message":{"role":"user","content":"…"}}`. The last answer is read from the
    session transcript. The receiving session applies its own `crossSessionInbound` rules, see
    *Messages held for approval* below.
  - Codex: live Codex TUIs found by process scan are matched to their thread through the rollout
    files of their `CODEX_HOME`. On Linux the match is exact: the native Codex binary keeps the
    rollouts of its thread and its subagents open, and the newest terminal thread among them
    (`source: "cli"`) is the session's. Elsewhere, and for a TUI attached to a separate
    `codex app-server` daemon (which keeps the rollouts open itself), it is the thread named by
    `codex resume <id>`, or else a guess: the newest terminal thread (not a `codex exec` run or
    a subagent's thread) in the same folder that started after the process. A thread held in
    another folder never counts, and a TUI that has not started a thread yet is not listed. When
    a guess could be wrong (several Codex sessions or several recent conversations in the
    folder, and any guess on Linux), the session is listed as `codex-pid-<pid>` without state,
    answers or input. Text is queued with `codex queue --thread <id> --message <text>`, and
    Codex delivers it when the thread is idle. The last answer is read from the rollout file.

- **Shell-hosted tmux panes:** an agent started from a shell inside tmux is not typed into
  (§8.1). With `external_sessions` on, such a Claude Code or Codex session takes `text` through
  the same inbox or queue, keeps its `tmux-<n>` id, and shows its last answer from the
  transcript or rollout. `keys` are not supported there.
- **Delivery:** a message handed to an inbox or queue shows nowhere until the agent takes it up.
  Until its answer arrives (for at most 10 minutes) the session counts as `working`, so a
  long-poll waits for the new answer instead of returning the previous one. For Codex the answer
  must come from a turn that started after the message was sent.
- **Message note:** every text sent this way starts with a short note (`external::framed`) saying
  that it comes from a remote client connected through the bridge, not from another session, and
  asking the agent to answer in that conversation and not to send its answer to other sessions.
  Claude Code presents the message as coming from a peer session without saying who is behind
  it, and an agent that cannot tell guesses a sender and sends its answer there, where the
  client never reads it. The note is advice to the agent, not a control: what the client may do
  is still decided by the workspace capabilities alone.
- **Messages held for approval (Claude Code):** the bridge cannot see what the receiving session
  does with a message once it is posted. This follows Claude Code's documentation of
  [cross-session messaging](https://code.claude.com/docs/en/cross-session-messaging.md#control-inbound-messages)
  and of the [`crossSessionInbound` setting](https://code.claude.com/docs/en/settings-reference#crosssessioninbound),
  which were not tested end to end here. With no `crossSessionInbound` value in effect, Claude
  Code decides per message from the permission modes of the two sessions. The bridge's plain
  `user` messages state no mode, so a session that runs with permission prompts bypassed *holds*
  them for approval, and any other session receives them. So:
  - Nothing needs to be changed in sessions that run with permission prompts.
  - In a session that runs without them, the person at its terminal approves the message in the
    dialog Claude Code opens there. A dialog left unanswered for `dialogExpiry` (5 minutes by
    default, set in user or managed settings) is closed and the message dropped. A session that
    cannot show the dialog, such as one in the VS Code extension or the Desktop app, keeps the
    message until the same deadline.
  - For unattended use, the owner can set `"crossSessionInbound": "accept"`. That is a choice,
    not a default: it applies to every cross-session message the session receives, not only the
    bridge's, and these are the sessions that run commands without asking. Claude Code reads
    managed settings first, then the `--settings` flag, then user settings, and applies the
    first value it finds. A project or local settings file counts only when it is stricter
    (`hold` or `refuse`), so an `accept` there has no effect. Two places work:
    `~/.claude/settings.json`, which applies to every session of that user, and a `--settings`
    value for one session (`claude --settings '{"crossSessionInbound":"accept"}'`), which
    applies only to a session started with it, so an already running session has to be
    restarted (`claude --resume` continues the conversation).
  - A held message is released when a mode or settings change allows it before the deadline.
  - A held message looks like a delivered one: the session shows `working` for up to 10 minutes
    and `last_message_at` does not change. The `note` of the session says so, and the client
    skill tells the client to report it after waiting and not to send the message again.

External sessions have the ids `claude-<session id>` and `codex-<thread id>`. These paths rely
on formats that are not stable public interfaces. They are isolated in `external.rs` and fail
soft: when parsing fails, the session simply shows less.

## 9. Models

The bridge offers the paths of the OpenAI and Anthropic APIs (§7), so a program points a stock
client library at it, uses the bridge token as its API key and calls a configured model by
name. Switching between a local and a paid model is a change of the `model` name only. The
bridge reads nothing of a request but `model`; the upstream validates the rest. Everything the
bridge knows about upstream APIs (paths, auth headers, the Anthropic version) lives in
`src/models.rs`.

An OpenAI client library uses `<bridge URL>/v1` as its base URL, an Anthropic one uses
`<bridge URL>`. A `vision` model is a chat model that also accepts images; the type exists so
that the list tells a caller which models can see.

### 9.1 Handling a call

The request passes the auth middleware like any other. It then takes one of the
`max_model_requests` slots **before its body is read**, which bounds memory to that many bodies
and protects a home GPU from a runaway loop. When all slots are taken it waits up to 10 s and then
answers 429 `models_busy` with `Retry-After: 5`, after reading and discarding the body: answering
with a large body still in flight would make the HTTP server close the connection, and the client
would see a reset instead of the error.

The body is read with its limit (32 MiB for JSON, 96 MB for a form). A JSON body must be an
object with a string `model`; a form needs a text field `model`. The model is looked up: an
unknown name gives 404 `model_not_found` naming the models of this endpoint the token may call; a
model whose `tokens` exclude the caller gives 403 `forbidden`; a model of another endpoint gives
400 `wrong_model_type` naming the endpoint to use.

The body is then rewritten. `model` becomes the upstream's name, and the model's `defaults` are
merged in: the client's value wins, objects are merged key by key at every depth, arrays and
other values are replaced whole. A default `chat_template_kwargs = { enable_thinking = false }`
therefore survives a client that sends another key in `chat_template_kwargs`, and yields to a
client that sends `enable_thinking` itself. **The order of object members is preserved**, because
a model emits structured output in the order of the JSON schema's `properties`. A form is read
completely and rebuilt in the client's order, with `model` replaced, defaults the client did not
send added as text fields, and file parts kept with their file name and content type.

The request goes to `url` plus the endpoint's path with `Content-Type`, the key
(`Authorization: Bearer` for `openai`, `x-api-key` for `anthropic`) and, for `anthropic`, the
client's `anthropic-version` (default `2023-06-01`) and `anthropic-beta`. No other header of the
client travels on, and neither does the query string: the bridge token, Cloudflare headers and
cookies never reach an upstream.

The upstream's status, `Content-Type`, `Retry-After` and body come back as they arrive: JSON,
server-sent events, audio or images. The bridge answers itself in four cases, each with
`x-should-retry: false`, because client libraries retry a 5xx twice and such a call cannot
succeed on a retry:

| Upstream | Bridge answer | Meaning |
|---|---|---|
| 401 or 403 | 502 `upstream_auth_failed` | `api_key` in the model's table is wrong |
| 404 or a redirect | 502 `upstream_misconfigured`, with the upstream's status and the start of its answer | `url` or `model` is wrong (a typo, a missing `/v1`, `http` for `https`); passed through, it would look like the bridge's own "no such endpoint" |
| cannot be reached | 502 `upstream_unreachable` | the server is off or `url` is wrong |
| has not started to answer in time | 504 `upstream_timeout` | the message advises `stream: true` where the endpoint has a stream mode |

Other upstream errors (400, 429, 5xx) pass through in the upstream's shape. Every call adds one
log line with the model name, the upstream's status and the duration; bodies, keys and upstream
URLs are never logged. When the client hangs up, the upstream request is dropped and its slot is
free again.

**Pauses in an event stream.** A reasoning model can stay silent for minutes in the middle of a
stream, and Cloudflare cuts a connection that carries nothing for 125 s. While a
`text/event-stream` answer is silent and everything sent so far ends at an event boundary (a
blank line), the bridge sends a comment line `: keep-alive` every 20 s. Comment lines are part of
the event-stream format and parsers skip them; this is the one addition the bridge makes to an
answer.

### 9.2 The model list

`GET /v1/models` answers in OpenAI's list shape, so `client.models.list()` works, with the
fields `type`, `api`, `endpoint`, `upstream_model`, `description` and `defaults` added. It lists
only the models the calling token may call, in name order, and `?type=` filters by type. The
upstream URL and key are never shown; `upstream_model` and `defaults` are, because a program
needs to know which model and which dimensions it works with. `created` is the bridge's start
time. An empty list carries a `note` saying whether no model is configured, none of the type
exists, or none is open to this token. The list reports what is configured and does not contact
the upstreams. `GET /v1/info` carries the same entries in `models`.

### 9.3 Limits and time

| Limit | Value | Why |
|---|---|---|
| JSON body of a model call | 32 MiB | Images travel inside chat requests as base64 |
| Form body (`speech-to-text`) | 96 MB (96 000 000 bytes) | Under Cloudflare's 100 MB ceiling in either unit |
| Model calls at once | `max_model_requests`, default 16 | Bounds memory and protects a home GPU |
| Wait for a free slot | 10 s | Back-pressure for a parallel indexing job instead of an immediate error |
| Connect to an upstream | 5 s | An upstream that is off is reported quickly |
| Until the upstream starts to answer | 90 s | With the slot wait at most 100 s, under Cloudflare's 125 s, so the caller gets the bridge's error and not Cloudflare's page |
| Silence inside an answer that is not an event stream | 90 s | Then the answer is cut and the cut is logged |
| Silence inside an event stream | 10 min, bridged by keep-alive lines every 20 s | A stuck stream still ends |

Answers must start within 90 s because of Cloudflare's timeout. A generation that takes longer
needs `stream: true`; calls without a stream mode (embeddings, rerank, images, a transcription
without streaming) cannot take longer through the bridge. A cut answer ends with an aborted
connection, so a client never takes it for a complete one.

## 10. File transfer

Grok Bot and the agents make files as part of their work: archives, reports, images, data sets.
The bot uploads files to a machine and downloads files from it. The agents never call the
bridge: they read an uploaded file from the path the bot gives them, and tell the bot the path
of a file they made. Code: `src/transfer.rs`.

### 10.1 Upload

`POST /v1/files/upload?dir=<folder>` with a multipart form, one part per file
(`curl -F 'file=@report.pdf'`). `dir` is an existing folder in a workspace that allows `upload`,
typically a session's `cwd`. It only selects the workspace: the files go to that workspace's
incoming folder, `<workspace>/.sgb-inbox/`, never into `dir` itself.

- **Names** are generated: `<date>.<time>.<random>.<extension>` in UTC, for example
  `2026-11-19.14-32-53.k7q2.zip`, with four random characters from `a-z0-9`. The extension is
  the uploaded name's last dot-separated part, lowercased, when it is 1 to 10 ASCII letters or
  digits (`tar.gz` keeps both parts); anything else gives `bin`. Nothing else of the client's
  file name reaches the file system; the response returns it as `name`.
- **No file is ever replaced.** Each file is created under its final name with exclusive
  creation (mode `0600` on Unix), and a taken name draws another random part. There are no
  temporary names and no renames, because a rename replaces an existing file.
- **The incoming folder** is created by the first upload (mode `0700` on Unix) and gets a
  `.gitignore` with `*` whenever it has none, so git never offers uploads for a commit and the
  file listing shows the folder but not its contents. An existing `.gitignore` is never
  overwritten. A folder that is a symlink or resolves outside the workspace is refused with 409
  `inbox_unusable`, and so is a workspace at a file-system root, which gets no incoming folder.
- **All or nothing.** When any file of a request fails (the limit, a broken body, a full disk,
  the client hanging up), the files the request created are removed and the error says that
  nothing was stored. If the bridge itself dies during an upload, a partly written file may stay;
  nobody was given its path.
- **Refusals reach the client.** A request refused before its body was read (no permission, an
  unusable folder, too many transfers, not a form) has its body read and discarded first, up to
  the limit, and is answered then; otherwise the HTTP server would close the connection with the
  upload in flight and the client would see a reset. A wrong token, a `models_only` token and a
  `Content-Length` above the limit are answered at once.
- **Response 201:** `files` (`name`, `path`, `size` per file, in the order of the request),
  `inbox` and a `note`. Uploads stay until someone deletes them on the machine; no endpoint
  lists or deletes them, and there is no quota. A client whose answer was lost uploads again.

### 10.2 Download

`GET /v1/files/download?path=<file>` sends a regular file of a workspace that allows
`download`, binary ones included. It is resolved and opened exactly like a text read: the path
is canonicalized, so a link is judged by where it points; the target's workspace and deny
patterns decide; and a file swapped while it is opened is 409 `path_changed`. The bytes are
streamed from the checked handle with `Content-Type: application/octet-stream`,
`Content-Length` and `Content-Disposition: attachment` with the file's name; a file that gets
shorter while it is sent ends with an aborted connection. An uploaded file can be downloaded
again where the workspace allows `download`, unless its extension matches a deny pattern.

### 10.3 Limits

| Limit | Value | Why |
|---|---|---|
| Upload request | 96 MB (96 000 000 bytes) | Under Cloudflare's 100 MB ceiling in either unit |
| Downloaded file | 96 MB | The same limit in both directions |
| Files per upload | 20 | Bounds the files one request creates |
| Transfers at once, both directions together | 4 | Bounds open files, disk load and memory; further requests get 429 `transfers_busy` with `Retry-After: 5` |

`GET /v1/info` reports the byte limit as `limits.max_transfer_bytes`. Every upload adds a log
line with the number of files, their total size and the incoming folder; every download adds one
when it ends, with the bytes sent and whether the file was complete. Client file names and
contents are never logged.

## 11. Inbox

The owner, or an agent on the same machine, leaves messages for Grok Bot. A bot picks them up when
the owner asks it in a chat ("look in the bridge inbox"). Nothing pushes to a bot: a webhook
routine has no access to the chat, so it cannot choose messages by topic (§16). Code:
`src/inbox.rs`, the `/v1/inbox` handlers in `src/api.rs`, and the `inbox` subcommand.

A message has one of two kinds. A `task` is new work: the bot first picks the most suitable bot for
it, which may be itself. A `followup` is input for an agenda a bot already works on: the bot finds
the earlier discussion and continues there.

### 11.1 Storage

A message is a folder `<state_dir>/inbox/<id>/`, outside every workspace, so the file endpoints
never reach it. The id is the UTC time and four random characters, such as
`2026-11-17.16-40-12.k7q2`, and the folder holds:

- `message.json`: the manifest. It has the id, the creation time in UTC, the kind, the subject,
  the project folder, the bot the message is for (`to`), the state, the attachments with name,
  size and SHA-256, and the claim and release history. The manifest is written last, through a
  temporary file and a rename, so a listing never shows a half-written message.
- `body.md`: the Markdown body, at most 256 KB.
- `attachments/<n>`: the attached files, stored under their index. A name the sender gave is only
  ever shown, never used as a path.

Folders are created with mode 0700 and files with 0600, as uploads are. A lock file,
`inbox/.lock`, serializes every change, so the CLI and the running bridge can both write.

### 11.2 States

A message is `unread`, until a bot `claim`s it; then it is `claimed` by that bot. A bot gives it
back with `release`, and it becomes `released`, and any bot may claim it again. Only the holder can
release. A second claim gets 409 `already_claimed` and names the holder. A message with `to` set can
be claimed only by that bot. Reading changes nothing; the claim is the "marked as read" step.

### 11.3 Writing (CLI, this machine only)

`safe-grokbot-bridge inbox send --kind task|followup --subject <line> [--body-file <file>]
[--project <dir>] [--to <bot>] [--attach <file>]…` stores a message. The body is read from
`--body-file`, or from stdin. `--project` defaults to the current folder when it lies in a workspace.
`inbox list [--all]` shows the open messages, or all of them, `inbox show <id>` prints one in full,
and `inbox delete <id>` removes one with its attachments. The subject is one line of at most 200
bytes; a message takes at most 20 attachments and 96 MB in all. No HTTP endpoint writes messages.
The only write the API offers is still the upload (§10.1).

### 11.4 Reading and claiming (API)

| Endpoint | Purpose |
|---|---|
| `GET /v1/inbox?state=&to=&limit=` | the messages, newest first, without bodies; `state` is `unread` (default), `claimed`, `released` or `all`; `to` shows the messages for that bot and for every bot; `limit` is 1–200 (default 50); the answer says whether it is `truncated` and what to do next |
| `GET /v1/inbox/{id}` | one message with its `body` |
| `GET /v1/inbox/{id}/attachments/{name}` | an attachment, with the file download's limits (§10.2) |
| `POST /v1/inbox/{id}/claim` `{"bot", "note"}` | take the message for a bot |
| `POST /v1/inbox/{id}/release` `{"bot", "note"}` | give it back; only the bot that holds it may |

Any token except a `models_only` one may use the inbox. The inbox never touches workspaces, git or
sessions, so a workspace's `allow` list does not apply to it.

### 11.5 Limits and security

- A bot's name is free text chosen by the token holder: 1 to 64 characters, no control characters.
  The bridge trusts the holder to name the bot honestly, as it trusts the holder with everything
  else the token reaches.
- The inbox is shared by every token that may use it. Put in a message only what all the bots of
  the bridge may read.
- The audit log records each request with its token name, as for every request. A message's body
  and attachments are never written to a log.
- A claim is atomic under the lock: when two bots race for one message, one of them gets 409.

### 11.6 How a bot uses it

The protocol is written into the client skill (`skills/safe-grokbot-bridge-client`) and into
`docs/grok-bot.md`. On the owner's request the bot lists the `unread` messages and the `released`
ones. If the owner gave a topic, the bot claims only the messages that match the conversation it is
in and leaves the others untouched. Without a topic it lists the subjects and asks which to take.
It reads a message in full before it claims it. A `task` for another bot is released again with a
note that names the bot it belongs to.

## 12. CLI

| Command | Purpose |
|---|---|
| `serve [--config]` | run the daemon |
| `init [--config] [--workspace <dir>]` | write a config template with a new token and commented model examples, print next steps |
| `token [--name]` | generate a token, print it and the `[[tokens]]` snippet |
| `check [--config]` | validate the config and environment: tmux, agents, git, TLS pin, each model's upstream (reachable, key accepted, model known, 5 s per model), warnings (also when the config file lies in a workspace or is readable by others while it holds keys) |
| `inbox send`, `inbox list`, `inbox show`, `inbox delete` | leave a message for Grok Bot in the inbox, or list, show or delete messages (§11) |
| `openapi` | print the OpenAPI document |
| `launch <agent> [-- args…]` | start an agent in a new tmux session in the current folder and attach, so the bridge can reach it |
| `hook <agent> <file> [payload]` | internal: record a turn-complete event (used by agent hooks) |

## 13. Platform support

| Feature | Linux | macOS | Windows |
|---|---|---|---|
| API, auth, TLS, files, git | yes | yes | yes |
| Uploads and downloads | yes | yes | yes (no file modes; see §6) |
| Headless sessions | yes | yes | yes |
| tmux sessions | yes | yes | best effort with psmux (bridge-started sessions only) |
| Discovery of user-started tmux panes | yes | yes | no |
| External Claude sessions (read-only) | yes | yes | yes |
| External sessions, experimental input | yes | yes | Claude: yes (named pipe with auth); Codex: yes |

WSL 2 counts as Linux. A bridge inside WSL sees only the agents inside WSL.

## 14. Testing strategy

Focus on behaviour that would hurt if it broke:

- auth: token verification, lockout and expiry, rate limiting, client-IP trust rules;
- path resolution: traversal, symlink escape, most-specific workspace, deny patterns;
- config validation: plain HTTP on non-loopback, unknown agents, invalid capabilities;
- agent command lines for each kind and option;
- parsing: headless streams (Claude, Codex, Hermes), hook payloads, Claude registry and
  transcripts, Codex rollouts, using fixtures;
- integration: the in-process router with temporary workspaces and git repos; the tmux and
  headless runners driven end to end with fake agent scripts (Unix only, skipped without tmux);
- the real binary: TLS with the pinned key and the handshake timeout, turn hooks of Claude Code
  and Codex through `safe-grokbot-bridge hook`, closing one-shot sessions, and taking over tmux
  sessions after a restart;
- TLS: certificate generation and pin computation;
- models, against small upstream servers started in the test: config validation, what reaches
  the upstream (rewritten `model`, merged defaults, member order, key headers, no client
  credentials), the endpoint table, body limits, error mapping, busy slots, keep-alive lines and
  cut answers, hang-ups, the rebuilt transcription form, the `check` probes, and that a
  `models_only` token reaches nothing else and the config file is never served;
- file transfer: generated names and extensions, exclusive creation and modes, the incoming
  folder's checks and `.gitignore`, all-or-nothing uploads (a broken body, the limit, a hang-up),
  refusals that arrive as JSON over a real connection, the transfer cap, and byte-exact
  downloads with their refusals.

CI runs `fmt`, `clippy -D warnings` and the tests on Linux, macOS and Windows.

Fake agents cannot follow the real CLIs, which change often, so the runners are also checked by
hand against real agents before a release. For 0.1 (Linux under WSL 2, Claude Code 2.1.312,
Codex CLI 0.166.0, tmux 3.4) these passed:

- Claude Code in tmux with `model` and `effort`, a follow-up message, and `DELETE`;
- Claude Code headless with `bypass_permissions`, and a follow-up that remembers the first turn;
- resuming that conversation in a tmux session in the same folder, and refusing it in another;
- Codex in tmux with `bypass_permissions` and `keep_alive: false`, and headless with a follow-up;
- a Claude Code session started from a shell inside tmux, messaged through its inbox;
- a running Codex TUI matched to its own thread, next to finished sessions in the same folder;
- a bridge restart with a running tmux session, which kept its state and took input afterwards.

For 0.2, Hermes Agent 0.21.1 passed these: a headless turn and a follow-up that remembers it,
resuming the conversation in the same folder and refusing it from another, refusing to resume a
conversation saved in YOLO mode without `bypass_permissions`, and a tmux session with a prompt.

Not checked by hand: native Windows with psmux, and macOS.

## 15. Operational notes

- Run the bridge as the same OS user that runs the agents and owns the tmux server.
- When the bridge stops, open requests get 10 s to finish (long-polls are cut short), then it
  stops its headless turns. A stop therefore takes about 10 s at most, well within systemd's
  default timeout.
- Under systemd, give the bridge's unit `KillMode=process`. A tmux server the bridge started runs
  in the unit's control group, and by default stopping the unit would end it with every session.
  The bridge stops its own headless turns when it stops.
- Under systemd, launchd or Windows services, `PATH` is minimal. Use absolute `command` paths or
  set `PATH`, and run `safe-grokbot-bridge check` with the same environment.
- Binding port 443 on Linux needs `CAP_NET_BIND_SERVICE`, or a firewall redirect from 443 to the
  bridge port.
- The first launch of an agent in a new folder may show a trust dialog. Clients answer it with
  keys (`Enter`).

## 16. Future work

- Push notifications to Grok routines when a session finishes or waits for input.
- An MCP endpoint (Streamable HTTP) for clients that prefer tools over REST.
- `git grep` search endpoint.
- Hermes `serve` integration for its headless backend sessions.
