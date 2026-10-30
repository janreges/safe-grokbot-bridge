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
   or headless (one non-interactive turn per message), and stop it.

The bridge never executes arbitrary commands on behalf of the client. All work on the machine is
done by the coding agents, under their own permission systems, and the bridge decides which agent
may run where and with which flags.

### Non-goals (v0.1)

- No shell, file-write or file-upload endpoint. Changes happen through agents.
- No MCP endpoint. Grok Bot reaches HTTP APIs through `curl` on its cloud computer, and other
  clients can use the same REST API.
- No outbound notifications. Clients long-poll a session instead. A webhook to Grok routines
  (`https://api2.cursor.sh/automations/webhook/<id>`) is planned (§13).
- No multi-machine federation. Each machine runs its own bridge, and the client keeps a list.

## 2. Constraints that shaped the design

These facts were verified in October 2026:

- **Grok Bot's cloud computer can only open HTTPS connections to hostnames on port 443.**
  Outbound traffic passes an SNI-based gateway, so plain HTTP and non-443 ports fail. The bridge
  must therefore be reachable as `https://<hostname>/` on port 443, either through a Cloudflare
  Tunnel or directly with TLS.
- Grok Bot stores API tokens in a *secure secret card*. The value is exported as an environment
  variable to every shell session on the cloud computer and is not shown in chat. All of a user's
  bots share that computer, so a token there is shared by all of them.
- Grok Bot's custom MCP connectors are called from Cursor's backend, not from the cloud computer.
  Plain REST over `curl` is the simplest reliable path.
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
                                                           └─ read-only files and git in allowed folders
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

State directory: `[server] state_dir`, defaulting to the platform data dir
(`~/.local/share/safe-grokbot-bridge` on Linux,
`~/Library/Application Support/safe-grokbot-bridge` on macOS,
`%LOCALAPPDATA%\safe-grokbot-bridge` on Windows). It is created with mode `0700` on Unix.

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

[limits]
auth_failures = 10                    # failed auths per client IP within the window …
auth_window_secs = 600
auth_block_secs = 900                 # … block that IP for this long
requests_per_minute = 300             # per token
max_sessions = 8                      # live sessions started by the bridge

[sessions]
runner = "auto"                       # default for new sessions: auto | tmux | headless
tmux = "tmux"                         # tmux-compatible binary (psmux on Windows)
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

The default is `["files", "git", "sessions"]`, which is read-only. Controlling agents is an
explicit opt-in.

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
| Memory exhaustion | file reads only of regular files, capped while reading, at most 8 file operations at once; listings read `.gitignore` files themselves, through a handle that must be a regular file, read up to 1 MiB, never following a final link or blocking on a pipe; agent output read line by line with caps; request bodies capped at 1 MiB |
| Log flooding by unauthenticated clients | rejected requests are logged with a shortened method and path, requests from a blocked address are not logged one by one, and dropped connections are logged at most once a minute with their count |

**Command lines are visible locally.** The first prompt of a tmux session is passed on the
agent's command line, and so are messages queued for external Codex sessions (`codex queue`). On
a machine shared with other local users, they can read these with `ps`. Headless prompts and
input to running tmux sessions go through stdin and paste buffers instead.

**Listings follow paths.** A file is read through a handle that must still be the authorized
file, but a listing walks folders by path. If someone with write access to a workspace replaces
a subfolder with a link while a deep listing runs, the listing can show names, sizes and times
from the link's target. It never shows file contents.

**Out of scope:** a compromised client (the Grok Bot account and its cloud computer), and
malicious agents or prompt injection inside repositories. Agents keep their own permission
systems, and the bridge does not try to judge prompts. Users who need hard isolation should run
the bridge on a dedicated VM.

**Defense in depth for Cloudflare deployments:** an Access application with a *Service Auth*
policy bound to one service token. The client adds the `CF-Access-Client-Id` and
`CF-Access-Client-Secret` headers. Unauthenticated requests then never reach the machine.

## 7. HTTP API

JSON over HTTPS. The OpenAPI 3.1 document is served at `GET /openapi.json` and committed as
`docs/openapi.json`. All routes except `/healthz` require `Authorization: Bearer <token>`.
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

Errors use one shape: `{"error": {"code": "<stable_code>", "message": "<what happened and what to
do>"}}`, returned with statuses 400, 401, 403, 404, 409, 413, 415, 429 (with `Retry-After`), 500,
502 or 503.

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
  sits between the pane and the agent. Their `keep_alive` setting and the last input are kept in
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
  `Left`, `Right`, `PageUp`, `PageDown`, `Home`, `End`, `Space`, `BSpace`, `C-c`, `C-d`) plus
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
    session transcript. The receiving session applies its own `crossSessionInbound` rules: by
    default a session running with permission prompts bypassed *holds* such messages for local
    approval, so set `"crossSessionInbound": "accept"` in those sessions' settings for unattended
    use.
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

External sessions have the ids `claude-<session id>` and `codex-<thread id>`. These paths rely
on formats that are not stable public interfaces. They are isolated in `external.rs` and fail
soft: when parsing fails, the session simply shows less.

## 9. CLI

| Command | Purpose |
|---|---|
| `serve [--config]` | run the daemon |
| `init [--config] [--workspace <dir>]` | write a config template with a new token, print next steps |
| `token [--name]` | generate a token, print it and the `[[tokens]]` snippet |
| `check [--config]` | validate the config and environment: tmux, agents, git, TLS pin, warnings |
| `openapi` | print the OpenAPI document |
| `launch <agent> [-- args…]` | start an agent in a new tmux session in the current folder and attach, so the bridge can reach it |
| `hook <agent> <file> [payload]` | internal: record a turn-complete event (used by agent hooks) |

## 10. Platform support

| Feature | Linux | macOS | Windows |
|---|---|---|---|
| API, auth, TLS, files, git | yes | yes | yes |
| Headless sessions | yes | yes | yes |
| tmux sessions | yes | yes | best effort with psmux (bridge-started sessions only) |
| Discovery of user-started tmux panes | yes | yes | no |
| External Claude sessions (read-only) | yes | yes | yes |
| External sessions, experimental input | yes | yes | Claude: yes (named pipe with auth); Codex: yes |

WSL 2 counts as Linux. A bridge inside WSL sees only the agents inside WSL.

## 11. Testing strategy

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
- TLS: certificate generation and pin computation.

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

Not checked by hand: native Windows with psmux, and macOS.

## 12. Operational notes

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

## 13. Future work

- Push notifications to Grok routines when a session finishes or waits for input.
- An MCP endpoint (Streamable HTTP) for clients that prefer tools over REST.
- `git grep` search endpoint.
- Hermes `serve` integration for its headless backend sessions.
