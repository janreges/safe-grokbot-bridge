---
name: safe-grokbot-bridge
description: Install, configure, update and troubleshoot safe-grokbot-bridge on this machine. The bridge is a small daemon that lets a remote AI agent such as Grok Bot read chosen folders and their git history and work with the Claude Code, Codex or Hermes sessions here, through an authenticated HTTPS API behind a Cloudflare Tunnel or TLS with key pinning. Use it when the user wants to set up the bridge, decide or change which folders it exposes and what the bot may do there, run it as a service, connect a Cloudflare Tunnel, start agent sessions the bot can reach, rotate a token, upgrade, or find out why the bot cannot connect.
---

# safe-grokbot-bridge

You help the user run safe-grokbot-bridge on this machine. A bot in the cloud, usually Grok Bot,
reaches the bridge over HTTPS with a bearer token. Through it the bot reads the folders the user
allows and their git history, sees the coding-agent sessions running here, sends them messages
and starts new ones. What the bot may do is set per folder in one TOML file. That access reaches
the user's code and accounts, so set it up carefully and never wider than the user asked for.

Project: https://github.com/janreges/safe-grokbot-bridge. Its `docs/` folder has the full
guides; read them there, or in an unpacked release archive, when a step needs more detail.

## How to work with the user

- Use the user's language and plain words. Keep each step short and say what comes next.
- Make sure the user knows what they set up: remote access to this machine. The bot reads what
  they expose and, through the agents it starts or messages, can make this machine run commands
  and change files. Recommend running it in an isolated environment rather than on their
  primary workstation (section 0). If they choose their workstation anyway, keep the setup
  narrow and leave the agents' permission prompts on.
- Look first, then propose. Find out yourself what is installed and which agents and folders
  exist (section 1). Then send one message with a complete proposal built from what you found,
  and ask only what you cannot know: the machine's name, the host name, the folders and what
  the bot may do in each, and whether agents may run without permission prompts. Go through
  the questions one at a time only when the user is unsure.
- The plan covers everything you will change: the folders and what the bot may do there, where
  the binary goes, services, system packages and Windows tasks. Show it as a table, say in
  words what each level allows, and wait for an explicit yes before you change anything.
- Never widen access on your own. Every folder, every capability beyond reading, and starting
  agents without permission prompts need the user's explicit yes. Prefer the narrowest setup
  that does the job; it is easy to extend later.
- Tell the user what the bridge does not control, when they choose folders:
  - It decides where the bot may start agents, not what an agent does once it runs. An agent
    started in a folder can read and change whatever the user can, including read-only folders
    nested inside and files the API hides, within the agent's own permission rules.
  - With `input`, the bot can answer an agent's permission prompts itself.
  - Whatever the bot reads leaves this machine for the bot provider's servers. Customer data
    and secrets belong outside the exposed folders unless that is allowed.
- Secrets never pass through this conversation. Never print, `cat`, echo or paste the bridge
  token, the tunnel token or a Cloudflare API token, never run a command whose output shows
  one, and never ask the user to paste one into the chat. Commands that show or read a secret
  are for the user's own terminal window, not for a `!` command in this chat, whose output
  would land in the conversation. If a secret shows up in the chat anyway, tell the user to
  replace it: a bridge token as in section 4, a tunnel or API token in Cloudflare.
- Commands that need `sudo` or another password are for the user to run in their own terminal:
  give them the exact command. Ask before you install system packages or change anything
  outside the user's config and data folders (`~/.config`, `~/.local`, `~/Library`,
  `%APPDATA%`, `%LOCALAPPDATA%`).
- After every change run `safe-grokbot-bridge check`, restart the bridge and verify it
  (section 5). Then report what you did and what the user still has to do.

Some steps only the user can do. Tell them exactly what to do, and wait until they are done:

| Step | Why the user does it |
|---|---|
| Approve the plan | It decides what a remote bot can touch |
| Create the Cloudflare tunnel, or the API token that lets Grok Bot create it | It needs their Cloudflare login |
| Save the tunnel token into its file with the command you give them | It is a secret and must not appear in the chat |
| Run the commands that need `sudo` | They need the user's password |
| Store the bridge's URL and token in the bot's secret store, and install the Grok Bot skill | It happens in the bot's app, with a secret |
| On WSL 2, create the Windows task that starts WSL at log on | It is a Windows setting |

## 0. Where to run it

Ask, or tell from section 1, whether this machine is meant for agents. On a primary workstation
with personal data, password managers, browser profiles, SSH keys or production credentials,
recommend one of these instead, from the most isolated:

- **A virtual machine or a separate computer** (Hyper-V, VirtualBox, VMware, UTM or Parallels,
  Multipass, a cloud VM). Snapshots make mistakes easy to undo.
- **A dedicated WSL 2 distribution** on Windows. The user creates it in PowerShell with
  `wsl --install Ubuntu-24.04 --name agents` (WSL 2.4.4 or newer), then opens it, installs and
  logs in the agents there, and continues with this skill inside it. In that distribution, add
  this to `/etc/wsl.conf` (keep its `[boot]` section) and restart it with
  `wsl --terminate agents`, so that agents can neither read the Windows drives nor start Windows
  programs:

  ```ini
  [automount]
  enabled = false

  [interop]
  enabled = false
  appendWindowsPath = false
  ```

- **At least a separate user account** that owns only the projects the bot works on.

Either way, only the repositories and credentials the work needs belong there, for example a
GitHub token limited to those repositories. `docs/deployment.md` ("Where to run it") explains
the reasons.

## 1. Find out where things stand

Run what applies, then sum up the result in two or three lines:

```bash
safe-grokbot-bridge --version          # installed, and which version
safe-grokbot-bridge check              # config path and problems, folders, agents, tmux, git
curl -s http://127.0.0.1:4765/healthz  # "ok" when the bridge runs on the default address
systemctl --user status safe-grokbot-bridge safe-grokbot-bridge-tunnel --no-pager   # Linux, WSL 2
launchctl list | grep -i grokbot       # macOS
ls -l ~/.config/safe-grokbot-bridge/   # which secret files exist; never print their content
gh release view --repo janreges/safe-grokbot-bridge --json tagName -q .tagName   # latest release
uname -sm; ldd --version 2>/dev/null | head -1                    # platform and glibc
command -v claude codex hermes node git tmux cloudflared         # what is installed
find ~ -maxdepth 3 -name .git -prune 2>/dev/null | head -40      # git repositories near home
systemd-detect-virt 2>/dev/null        # vm (kvm, microsoft, oracle, …), wsl, or none (a real machine)
ls /mnt/c >/dev/null 2>&1 && echo "WSL: Windows drives are mounted"
ls /proc/sys/fs/binfmt_misc 2>/dev/null | grep -qi wslinterop && echo "WSL: can start Windows programs"
```

On WSL 2, `$WSL_DISTRO_NAME` is the distribution's name. On Windows, check the scheduled tasks
(`Get-ScheduledTask *grokbot*`) and use `Invoke-RestMethod http://127.0.0.1:4765/healthz`.

| What you found | Continue with |
|---|---|
| A primary workstation, not an isolated environment | 0 first, then go on where the user wants to run it |
| Not installed | 3 (agree on the plan), 2 (install), the end of 3 (write the config), 5, 6 |
| Installed, but `check` finds no config | 3, 5, 6 |
| A config exists and the user wants a change | 4 |
| Works locally, but does not run as a service or is not reachable from outside | 5 and 6 |
| The bot gets errors | 9 |
| A newer release exists | 8 |

## 2. Install

Download the release archive for this OS and CPU, check its checksum, and put the binary on
`PATH`:

| `uname -sm` | Archive for |
|---|---|
| Linux x86_64, also WSL 2 | `x86_64-unknown-linux-gnu` (glibc 2.35 or newer) |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| Darwin arm64 | `aarch64-apple-darwin` |
| Darwin x86_64 | `x86_64-apple-darwin` |
| Windows x64 | `x86_64-pc-windows-msvc` (a `.zip`) |

```bash
target=x86_64-unknown-linux-gnu
cd "$(mktemp -d)"
gh release download --repo janreges/safe-grokbot-bridge --pattern "*-$target.tar.gz*"
sha256sum -c ./*.sha256          # macOS: shasum -a 256 -c ./*.sha256
tar xzf ./*.tar.gz
mkdir -p ~/.local/bin && install -m 755 ./*/safe-grokbot-bridge ~/.local/bin/
safe-grokbot-bridge --version    # if not found, add ~/.local/bin to PATH
```

- `gh` also reaches a private repository once the user has run `gh auth login`.
- With an older glibc, or to build from source, use
  `cargo install --locked --git https://github.com/janreges/safe-grokbot-bridge` (Rust 1.95+).
- On Windows, unpack the zip to `%LOCALAPPDATA%\Programs\safe-grokbot-bridge\` and add that
  folder to the user's `PATH`. If the agents live in WSL, install the Linux build inside WSL
  instead: a bridge on Windows cannot see agents in WSL, and the other way round.

The bridge also needs `git`, and `tmux` 3.0 or newer for live sessions that a person can attach
to (`sudo apt install tmux` for the user to run, or `brew install tmux`). The agents (`claude`,
`codex`, `hermes`) must be installed and logged in for the same user.

## 3. Configure: agree with the user on what the bot gets

The proposal answers these questions. Fill in what you can from section 1. A setup message from
Grok Bot ("set up this machine for Grok Bot as …") already names the machine, its Cloudflare
Tunnel and its host name: use them and do not ask again. The tunnel then exists already, and
section 6 only needs its token.

1. **How does the bot reach this machine?**
   - *Cloudflare Tunnel* (recommended): no open port, the bridge stays on `127.0.0.1:4765`. Ask
     for the domain the user has on Cloudflare and a host name for this machine, such as
     `pc1-bridge.example.com`.
   - *A public IP* (a VPS, port forwarding): the bridge listens on port 443 with TLS, and the
     bot pins its key. Ask for the host name that points to this machine.
   - *Only this machine for now*, for a first test.
2. **What should the bot call this machine?** Default: the host name. A short name of letters,
   digits and `-`, such as `pc1`. The bot's secrets use it in capitals with `-` as `_`:
   `eva-pc` gives `SGB_EVA_PC_URL` and `SGB_EVA_PC_TOKEN`.
3. **Which folders should the bot see?** Suggest the folders with git repositories you found,
   such as `~/projects`, and let the user choose. Expose a repository at its root: a subfolder
   alone does not expose its git history. Advise against the whole home folder on a personal
   machine: the built-in deny list hides key files, but not mail, browser profiles or
   documents. `/` is meant for a dedicated agent machine.
4. **What may the bot do in each folder?** Offer three levels:

   | Level | `allow` | The bot can |
   |---|---|---|
   | Read (default) | `["files", "git", "sessions"]` | read files and git history, see the agent sessions there and their answers |
   | Work | `["files", "git", "sessions", "input", "start"]` | also message those agents, start new ones and stop the ones it started |
   | Sessions only | `["sessions"]` | only see the agent sessions, no files |

   Other combinations are fine when the user asks, such as `["files"]`. For a *Work* folder,
   also agree on:
   - Which agents may the bot start there? Default: all that `check` lists.
   - May it start them **without permission prompts** (`allow_bypass_permissions`)? Default: no.
     Recommend it only for a sandbox folder or a dedicated VM, because such an agent runs
     commands without asking anyone.

   A folder inside another one gets its own rules, which replace the outer folder's completely,
   `deny` and `agents` included. When a Read folder lies inside a Work folder, say that an agent
   started in the Work folder can still read it.
5. **Is there anything in these folders the bot must never read?** Keys, `.env` files and
   similar secrets are always hidden. Add the user's own patterns to `deny`, such as
   `"secrets/"` or `"*.sqlite"`. Say clearly that `deny` does not filter git history: whatever
   was ever committed is visible in a folder with `git`.

Only when it applies:
- **Several profiles of one agent**, such as two `CODEX_HOME` or `CLAUDE_CONFIG_DIR` folders:
  one `[agents.<name>]` entry per profile.
- **No tmux:** sessions then run headless, one run per message. Offer to install tmux.
- **Agents in plain terminal tabs or an IDE** that the bot should message as well:
  `[experimental] external_sessions = true`. It is off by default; say that it is experimental.

Show the plan and wait for a yes, for example:

```text
Machine "pc1" at https://pc1-bridge.example.com, through a Cloudflare Tunnel
(the bridge listens on 127.0.0.1:4765 only)

Folder                      Level  The bot can                                 Without prompts
/home/me/projects           Work   read, message agents, start claude, codex   no
/home/me/projects/client-x  Read   read files and git history, see sessions    -

Changes: the binary goes to ~/.local/bin, the config and the token files to
~/.config/safe-grokbot-bridge (private), two systemd user services. You install
cloudflared with sudo in your own terminal.
Note: an agent started in /home/me/projects can also read client-x.
```

After the yes, and once the bridge is installed, create the config. `init` prints the bridge
token once, so its output goes straight into a file only the user can read:

```bash
mkdir -p -m 700 ~/.config/safe-grokbot-bridge
(umask 077; rm -f ~/.config/safe-grokbot-bridge/bridge-token.txt
 safe-grokbot-bridge init --workspace /home/me/projects > ~/.config/safe-grokbot-bridge/bridge-token.txt)
safe-grokbot-bridge check              # prints the path of the config file
```

`init` writes a commented config with one read-only workspace and a `[[tokens]]` entry with the
token's hash. Edit the file to match the plan: keep the `[[tokens]]` entry as it is, set
`[server] name`, and write one `[[workspaces]]` block per folder:

```toml
[server]
listen = "127.0.0.1:4765"
name = "pc1"

[[tokens]]                            # written by init; keep it as it is
name = "grok"
sha256 = "<64 hex digits>"

[[workspaces]]
path = "/home/me/projects"
allow = ["files", "git", "sessions", "input", "start"]
agents = ["claude", "codex"]
allow_bypass_permissions = false
deny = ["secrets/"]

[[workspaces]]
path = "/home/me/projects/client-x"   # nested: its own rules replace the outer folder's
allow = ["files", "git", "sessions"]
```

For a public IP, also see section 6. Run `safe-grokbot-bridge check` until it prints `OK`; it
names every problem it finds. `docs/configuration.md` explains every key.

## 4. Change an existing config

1. Read the config file (`check` prints its path) and show its folders as the table from
   section 3. Do not show token hashes.
2. Ask what should change: a folder to add or remove, another level, the agents, permission
   prompts, deny patterns, or tokens.
3. Show the change as before and after, point out anything that widens access, and wait for a
   yes.
4. Back up the file (`cp config.toml config.toml.bak`), edit it, run `safe-grokbot-bridge check`,
   and restart the bridge (section 5). The bridge reads its config only when it starts.

Tokens:
- **Another client:** create the token in a private file, then append only its `[[tokens]]`
  snippet, which holds the hash, to the config. The token itself stays in the file until the
  user moves it to the client and deletes the file:

  ```bash
  (umask 077; safe-grokbot-bridge token --name laptop > ~/.config/safe-grokbot-bridge/laptop-token.txt)
  sed -n '/^\[\[tokens\]\]/,$p' ~/.config/safe-grokbot-bridge/laptop-token.txt >> "<config file>"
  ```
- **Revoke a token:** remove its `[[tokens]]` entry and restart.
- **Replace a leaked token:** create a new one, let the user give it to the client, then revoke
  the old one.

## 5. Run it as a service

Run the bridge as the same user who runs the agents. [references/services.md](references/services.md)
has the service files for every platform:

- Linux and WSL 2: systemd user units for the bridge and `cloudflared`, plus
  `loginctl enable-linger`. The bridge's unit needs `KillMode=process`, or stopping it ends every
  tmux session it started. Enable the tunnel's unit once its token file exists.
- macOS: launchd agents. They run while the user is logged in.
- Windows: Task Scheduler tasks at log on.

Services start with a minimal `PATH`. Put the folders of `claude`, `codex`, `node`, `git` and
`tmux` into the service's `PATH`, or set absolute `command` paths in the config, and read the log
after the first start.

```bash
curl -s http://127.0.0.1:4765/healthz                                   # ok
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:4765/v1/info  # 401 without a token
journalctl --user -u safe-grokbot-bridge -n 50 --no-pager               # the log on Linux
```

On WSL 2 the services run only while the distribution runs, and Windows does not start it by
itself: the user creates the Windows task described in references/services.md. The computer
must also be awake; a sleeping PC is unreachable.

## 6. Make it reachable for the bot

**Cloudflare Tunnel.** The tunnel routes the host name to `http://127.0.0.1:4765`. It is created
in one of two ways:
- Grok Bot creates it through the Cloudflare API. Steps 1 and 2 of `docs/setup.md` have the
  prompts for the bot and the API token's permissions.
- The user creates it in the Cloudflare dashboard: *Zero Trust → Networks → Tunnels → Create a
  tunnel → Cloudflared*, then adds a public host name with service `HTTP` and URL
  `127.0.0.1:4765`.

Either way, the user then copies the tunnel token from the dashboard: open the tunnel, choose to
install a connector, and the install command shown there ends with the token. Give the user
this command for their own terminal window. It reads the token without showing it, works from
zsh too, and writes nothing when the input is empty:

```bash
mkdir -p -m 700 ~/.config/safe-grokbot-bridge
bash -c 'umask 077; read -rsp "Tunnel token: " t && echo && [ -n "$t" ] && rm -f ~/.config/safe-grokbot-bridge/tunnel-token && printf "%s" "$t" > ~/.config/safe-grokbot-bridge/tunnel-token'
```

Then install `cloudflared` 2025.4.0 or newer, either from Cloudflare's packages
(https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/downloads/;
the `sudo` commands are for the user), or without `sudo` as a single file:

```bash
curl -fsSL -o ~/.local/bin/cloudflared https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64
chmod 755 ~/.local/bin/cloudflared && cloudflared --version   # use cloudflared-linux-arm64 on ARM
```

On macOS, `brew install cloudflared`. Run it as a service (section 5) with
`cloudflared tunnel --no-autoupdate run --token-file <absolute path of tunnel-token>`. Never put
the token itself on a command line, where other local users could read it. The connector needs
outbound port 7844 (TCP or UDP) to Cloudflare.

**Public IP.** Set `listen = "0.0.0.0:443"` and add `[server.tls]` with
`hostnames = ["<host name>"]`. Port 443 on Linux needs
`sudo setcap cap_net_bind_service=+ep "$(command -v safe-grokbot-bridge)"`, again after every
upgrade. `check` prints the certificate's pin, which the bot uses with `curl --pinnedpubkey`.
Open only that port.

Verify from outside: `https://<host name>/healthz` prints `ok`, and `/v1/info` without a token
answers `401`.

**Connect the bot.** `bridge-token.txt` holds the whole output of `init`; the token is the value
that starts with `sgb_`. You can put it on the clipboard without showing it, so that the user
pastes it straight into the bot:

```bash
grep -o 'sgb_[A-Za-z0-9_-]*' ~/.config/safe-grokbot-bridge/bridge-token.txt | tr -d '\n' | clip.exe
```

That is for WSL 2 and needs its interop; on macOS end it with `| pbcopy`, on a Linux desktop with
`| wl-copy` or `| xclip -selection clipboard`. Without a clipboard, for example in a WSL
distribution with interop off, the user copies the token from the file in their own terminal
window.

Then send the user back to the bot. If Grok Bot created the tunnel, they tell it *"<name> is
ready"*: the bot asks them to paste the URL (`https://<host name>`) and the token into a secure
secret card, and tests the connection in a new chat. Otherwise they install the bot's skill,
`safe-grokbot-bridge-client` (`docs/grok-bot.md`), store the two secrets `SGB_<NAME>_URL` and
`SGB_<NAME>_TOKEN` in a secure secret card, and ask the bot in a new chat: "Use the
safe-grokbot-bridge-client skill: show me what machine <name> offers." Once the bot reaches the
machine, delete `bridge-token.txt` and check that it is gone. If the clipboard keeps a history
(Windows: Win+V), the user clears it.

## 7. Everyday use

- `safe-grokbot-bridge launch claude` (or `codex`, `hermes`) starts the agent in a new tmux
  session in the current folder and attaches to it. The bot sees that session and, in a folder
  with `input`, can message it. An agent started from a plain shell inside tmux stays read-only
  for the bot, because typed text could reach the shell once the agent exits.
- `tmux attach -t <id>` shows a session the bot started; its id is the tmux session name. Add
  `-L <socket>` when the config sets `[sessions] tmux_socket`.
- `safe-grokbot-bridge check` shows what is exposed right now.

## 8. Upgrade

Compare `safe-grokbot-bridge --version` with the latest release and read that release's entry in
`CHANGELOG.md`. Download and verify the new archive as in section 2, stop the service, replace
the binary, run `check`, and start the service again. tmux sessions keep running (under systemd
thanks to `KillMode=process`); headless turns that were running are stopped. Repeat `setcap` for
the new binary if the bridge listens on port 443.

## 9. When something does not work

| Symptom | What to check |
|---|---|
| `530` or `1033` from the host name | `cloudflared` is not running, or its token file is wrong: read the tunnel service's log |
| `cloudflared` logs timeouts or `failed to dial` | A firewall blocks outbound port 7844 (TCP and UDP) |
| `502` from the host name | The bridge does not answer on `127.0.0.1:4765`: is its service running, is `listen` unchanged? |
| `401 unauthorized` | The bot sends another token: create a new one (section 4) |
| `429` with `Retry-After` | Too many failed logins from the bot's address, or too many requests: wait as long as it says |
| A Cloudflare challenge page | Bot Fight Mode or a WAF rule blocks non-browser clients; see `docs/deployment.md` |
| An agent works in the terminal but is "not found" by the service | The service's `PATH` lacks the agent's folder: extend it, or set `command` to an absolute path |
| tmux sessions vanish when the bridge restarts | The systemd unit lacks `KillMode=process` |
| A session is listed, but the bot cannot type into it | It runs in a shell inside tmux, or its folder lacks `input`: start it with `safe-grokbot-bridge launch` |
| The bridge is gone after a reboot | On WSL 2 and macOS it needs more than a service: see "When the bridge runs" in references/services.md |
| `403 forbidden` for git | The repository's root, or the place its history is stored, lies outside a folder with `git` |

## 10. Remove the bridge

Stop and disable the services and delete their files (section 5), delete the binary, the config
file, the state folder (`~/.local/share/safe-grokbot-bridge` on Linux) and
`~/.config/safe-grokbot-bridge/`. Tell the user to delete the tunnel and its DNS record in
Cloudflare and the machine's secrets and table row in the bot. tmux sessions the bridge started
keep running until they are closed.

The repository's guides go deeper: `docs/setup.md` (the guided setup with Grok Bot and
Cloudflare), `docs/configuration.md`, `docs/deployment.md`, `docs/grok-bot.md`, and
`docs/design.md` (the security model).
