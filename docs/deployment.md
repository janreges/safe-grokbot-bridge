# Deployment

Each machine runs its own bridge with its own config and token. This guide covers how to publish
it so that a cloud agent can reach it, and how to keep it running.

Grok Bot's cloud computer can only open HTTPS connections to host names on port 443. Either of
the two options below satisfies that.

## Where to run it

The bridge is remote access by design. A client with a token can read what you expose, and
through the coding agents it starts or messages it can make the machine run programs and
commands. Those agents run with your user's rights, and their permission prompts are the only
check on what they do; a client allowed to send input can answer those prompts too. Text in a
repository or on a web page can steer an agent as well (prompt injection).

So run the bridge and its agents where a mistake costs little, not on your primary workstation
with your personal data, password manager, browser profiles, SSH keys and production
credentials. From the most isolated:

- **A virtual machine or a separate computer**: Hyper-V, VirtualBox, VMware, UTM or Parallels on
  macOS, Multipass, a cloud VM. Snapshots make mistakes easy to undo.
- **A dedicated WSL 2 distribution** on Windows, used only for agents:

  ```powershell
  wsl --install Ubuntu-24.04 --name agents   # WSL 2.4.4 or newer; otherwise use wsl --export and --import
  ```

  By default WSL mounts the Windows drives under `/mnt` and lets Linux programs start Windows
  programs, which hands your Windows files and account to the agents. Add this to the new
  distribution's `/etc/wsl.conf` (keep its `[boot]` section), then restart it with
  `wsl --terminate agents`:

  ```ini
  [automount]
  enabled = false

  [interop]
  enabled = false
  appendWindowsPath = false
  ```

  All WSL 2 distributions share one virtual machine and its kernel, so this separates less than
  a virtual machine does.
- **At least a separate user account** that owns only the projects the bot works on.

Wherever it runs: keep only the repositories and credentials the work needs there (for example a
GitHub token limited to those repositories), keep `allow_bypass_permissions` off unless the
environment is disposable, and review what the agents changed before you merge or deploy it.

## Option A: Cloudflare Tunnel (recommended)

The bridge keeps listening on `127.0.0.1`, and `cloudflared` on the same machine opens an outbound
tunnel to Cloudflare. No inbound port is opened, and Cloudflare provides the public HTTPS
endpoint. The client side needs nothing but `curl`.

[Setup with Grok Bot and Cloudflare](setup.md) lets Grok Bot create the tunnel and a coding agent
on your machine do the rest. By hand:

1. In the Cloudflare dashboard, open **Zero Trust → Networks → Tunnels** and create a tunnel of
   type *Cloudflared*.
2. Install the connector with the command the dashboard shows, e.g.
   `sudo cloudflared service install <token>`; on Linux that is a systemd service. Or, without
   root, save the token in a file only you can read and run the connector as a service of your
   user (in WSL 2 enable systemd first: `[boot] systemd=true` in `/etc/wsl.conf`):

   ```bash
   mkdir -p -m 700 ~/.config/safe-grokbot-bridge
   bash -c 'umask 077; read -rsp "Tunnel token: " t && echo && [ -n "$t" ] && rm -f ~/.config/safe-grokbot-bridge/tunnel-token && printf "%s" "$t" > ~/.config/safe-grokbot-bridge/tunnel-token'
   cloudflared tunnel --no-autoupdate run --token-file ~/.config/safe-grokbot-bridge/tunnel-token
   ```

   `--token-file` keeps the token off the command line, where other local users could see it.
   It needs `cloudflared` 2025.4.0 or newer.
   The connector needs outbound access to Cloudflare on port 7844 (TCP or UDP); see Cloudflare's
   [firewall requirements](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/configure-tunnels/tunnel-with-firewall/).
3. Add a **public hostname**, e.g. `pc1-bridge.example.com`, with service `HTTP` and URL
   `127.0.0.1:4765`.
4. Test it from another machine:

   ```bash
   curl https://pc1-bridge.example.com/healthz                         # → ok
   curl -H "Authorization: Bearer $TOKEN" https://pc1-bridge.example.com/v1/info
   ```

Notes:

- The bridge sees every request coming from `127.0.0.1`. It takes the real client IP from
  `CF-Connecting-IP`, which is trusted only from loopback peers, so the per-IP lockout targets the
  attacker and not the tunnel.
- Cloudflare closes proxied requests after about 125 seconds without data. The bridge caps
  long-polls (`wait`) at 90 seconds for that reason.
- **Optional second factor:** add an Access application for the hostname. Give it a policy with
  action *Service Auth* that includes one specific service token. Clients then send
  `CF-Access-Client-Id` and `CF-Access-Client-Secret` headers in addition to the bearer token, and
  requests without them never reach your machine. Turn on strict service-token authentication,
  so that failures return `401`/`403` instead of a login page.
- If requests from your client are answered by a Cloudflare challenge page, a security feature
  blocks non-browser clients:
  - a WAF custom rule or *Super* Bot Fight Mode (Pro plans and up): add a WAF skip rule for the
    bridge's hostname;
  - the free plan's Bot Fight Mode cannot be skipped by rules: turn it off for the zone, or put the
    bridge on a hostname in a zone without it.

  Do not use country-based rules either: Grok Bot's outbound IP addresses change their apparent
  location.
- Quick tunnels (`cloudflared tunnel --url http://127.0.0.1:4765`) are handy for a first test,
  but their URL changes on every start and they have no Access support.

## Option B: Public IP with TLS (VPS, port forwarding)

The bridge listens on a public address itself. TLS is mandatory. By default the bridge generates
a self-signed certificate and clients pin its public key, so no certificate authority or domain
validation is needed.

```toml
[server]
listen = "0.0.0.0:443"

[server.tls]
hostnames = ["vps.example.com"]
```

```bash
safe-grokbot-bridge check      # generates the certificate and prints its pin
```

```
Listen       https://0.0.0.0:443
TLS          self-signed /home/me/.local/share/safe-grokbot-bridge/tls/cert.pem
Pin          sha256//I5OyAVdp8M6X9EbFaIVo329WcO44uADWma6dZ1MzLh8=
```

Clients verify the server by that pin. `-k` only skips the certificate-authority check; curl
still refuses to connect when the key differs:

```bash
curl --pinnedpubkey "sha256//I5OyAVdp8M6X9EbFaIVo329WcO44uADWma6dZ1MzLh8=" -k \
     -H "Authorization: Bearer $TOKEN" https://vps.example.com/v1/info
```

- **Port 443 on Linux** needs a privilege: `sudo setcap cap_net_bind_service=+ep
  $(command -v safe-grokbot-bridge)` (repeat after every upgrade). Alternatively listen on a high
  port and redirect 443 to it in the firewall.
- **A CA-issued certificate** (e.g. Let's Encrypt via certbot) works too. Set `cert` and `key` in
  `[server.tls]`; clients then use normal certificate validation, and the bridge reloads the files
  only on restart.
- Grok Bot reaches the bridge by host name, so give the server a DNS name.
- Open only the bridge port. The bridge needs nothing else from the internet.

## Running as a service

Run the bridge as **the same user** who runs the agents. Agents use that user's logins, and tmux
sessions must live in that user's tmux server.

### Linux and WSL 2: systemd user service

`~/.config/systemd/user/safe-grokbot-bridge.service`:

```ini
[Unit]
Description=safe-grokbot-bridge

[Service]
ExecStart=%h/.cargo/bin/safe-grokbot-bridge serve
Restart=on-failure
RestartSec=5
# Services get a minimal PATH; include where claude, codex, node and git live.
Environment=PATH=%h/.local/bin:%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin
# Stop only the bridge itself. It stops its own headless turns, while a tmux server it started
# runs in this service's control group and must survive restarts and upgrades.
KillMode=process

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now safe-grokbot-bridge
loginctl enable-linger "$USER"     # keep it running without a login session
journalctl --user -u safe-grokbot-bridge -f
```

Without systemd, run `safe-grokbot-bridge serve` in a tmux window of its own.

The tunnel connector can run the same way, as
`~/.config/systemd/user/safe-grokbot-bridge-tunnel.service` (use the path of your `cloudflared`):

```ini
[Unit]
Description=Cloudflare Tunnel for safe-grokbot-bridge

[Service]
ExecStart=/usr/bin/cloudflared tunnel --no-autoupdate run --token-file %h/.config/safe-grokbot-bridge/tunnel-token
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
```

### macOS: launchd

`~/Library/LaunchAgents/com.github.janreges.safe-grokbot-bridge.plist`. launchd does not expand
`~`, so replace `/Users/me` with your home folder, and the program path with the output of
`which safe-grokbot-bridge` (`cargo install` puts it in `~/.cargo/bin`). `PATH` must include the
folders of your agents (`which claude codex`).

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.github.janreges.safe-grokbot-bridge</string>
  <key>ProgramArguments</key>
  <array><string>/Users/me/.cargo/bin/safe-grokbot-bridge</string><string>serve</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/Users/me/.local/bin:/Users/me/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/tmp/safe-grokbot-bridge.log</string>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.github.janreges.safe-grokbot-bridge.plist
```

### When the bridge runs

- **Linux:** the systemd user services above start at boot, without a login, once lingering is
  enabled.
- **WSL 2:** the services run while the distribution runs. Windows does not start it at boot,
  and systemd services do not keep it running. Start it when you log on to Windows with a Task
  Scheduler task (trigger *At log on*, action `wsl.exe` with the arguments
  `-d <distribution> --exec sleep infinity`, and *Stop the task if it runs longer than* cleared),
  and leave that task running. The bridge is then reachable while you are logged on to Windows.
- **macOS:** a launchd agent runs while you are logged in. To have the bridge back after a
  restart without logging in, turn on automatic login (not available with FileVault). Agents
  need your user session, Claude Code for example keeps its login in your keychain, so the
  bridge does not run as a system daemon.
- **Windows:** the Task Scheduler task below runs while you are logged on.

### Windows

- Run `safe-grokbot-bridge.exe serve` from Task Scheduler ("At log on", "Run only when user is
  logged on"), so it runs as you and sees your agent logins. On the task's *Settings* tab, clear
  *Stop the task if it runs longer than*: its default of 3 days would stop the bridge.
- Without a tmux-compatible multiplexer, sessions run **headless**. Install
  [psmux](https://github.com/psmux/psmux) (`winget install psmux`) and set
  `[sessions] tmux = "psmux"` for tmux sessions; this is best effort.
- Agents installed through npm are `.cmd` shims. Point `command` at them explicitly, e.g.
  `command = 'C:\Users\me\AppData\Roaming\npm\codex.cmd'`.
- If your agents live in WSL, run the Linux build of the bridge inside WSL instead. A bridge on
  Windows cannot see agents inside WSL, and vice versa.

## Several machines

Install one bridge per machine, each with its own host name (e.g. `pc1-bridge.example.com`,
`vps-bridge.example.com`) and its own token. Give the client one URL and token pair per machine.
[docs/grok-bot.md](grok-bot.md) shows a naming scheme for Grok Bot secrets.

## Upgrading

Stop the service, replace the binary, run `safe-grokbot-bridge check`, start the service again.
tmux sessions keep running during the upgrade (under systemd, thanks to `KillMode=process`). The bridge takes them over again by name on
start, together with their turn history. Headless turns that were running are stopped.
