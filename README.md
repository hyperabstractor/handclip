# Handclip

Handclip shares files and clipboard content between your own Macs and Windows
PCs. One always-on device runs a small relay; every device keeps an outbound,
full-duplex connection to it.

The menu bar / system tray and keyboard shortcuts send the clipboard:
copy normally, then choose **Send Clipboard to…**. Text, images, files, and
folders remain supported. File contents are streamed in bounded chunks and
verified with SHA-256 before becoming visible. A sender's **Files delivered**
notification requires confirmation from the receiver after verification.

## Security

Handclip authenticates devices with a shared token but does **not** encrypt
traffic. Run it over an encrypted private network such as
[Tailscale](https://tailscale.com) or WireGuard, or a LAN you trust. Never expose
the relay port (24871) to the internet.

## Install

Download the latest `Handclip-macos.zip` or `Handclip-windows.zip` from
[Releases](https://github.com/hyperabstractor/handclip/releases).

### macOS

Unzip `Handclip-macos.zip` and run the installer from Terminal:

```sh
cd ~/Downloads/Handclip && ./install.sh
```

It installs `Handclip.app` in `/Applications`, starts it at every login, and
links `handclip-cli` into your PATH. Re-run it to upgrade. The app isn't
notarized by Apple, so use the installer rather than opening `Handclip.app`
directly from the download. It runs on Apple silicon and Intel Macs.

### Windows

Unzip `Handclip-windows.zip` and run the installer from PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File .\Handclip\install-agent.ps1
```

It installs the agent for the current user and starts it at every sign-in.

### Building from source

With a Rust toolchain, `packaging/macos/package.sh` builds
`target/dist/Handclip-macos.zip`. On Windows, build with
`cargo build --release -p handclip-agent` and put `handclip-agent.exe` next to
`packaging/windows/install-agent.ps1`. Pushing a `v*` tag builds both zips and
publishes a GitHub release.

### Upgrading to 0.2

Upgrade the relay and **every device** together. This release uses protocol 4;
protocol 3 peers are rejected. Existing device IDs, tokens, endpoints,
and shortcuts are preserved.

Version 0.2.1 removes the experimental Finder Share extension. The macOS installer
unregisters and disables the old extension when upgrading. Clipboard sending and
the large-file transfer improvements remain available.

### First launch

On a new install Handclip opens its Settings window:

1. **Device ID**: this device's name, e.g. `laptop` (prefilled from the
   computer name). Other devices send to it by this name.
2. **Shared token**: on the first device, click **Generate**; paste the same
   token on every other device.
3. **Relay endpoints**: the relay device's address, e.g.
   `relay-host.your-tailnet.ts.net:24871`. On the relay device itself, use
   `127.0.0.1:24871` and tick **Run the relay on this device**.
4. **Send-to shortcuts**: add each device you want to send to, with a shortcut
   such as `Control+Alt+1`. `Control+Alt+0` sends to all connected devices.

Save, and Handclip restarts with the new settings.

For local and remote use, list the **same relay's** LAN address first and Tailscale
IP or MagicDNS hostname second, e.g. `192.168.1.10:24871` followed by
`relay.your-tailnet.ts.net:24871`. Handclip tries them in order on connection and
reconnection. It keeps a healthy connection until it disconnects; it does not
perform LAN discovery or switch routes during an active transfer. Tailscale is
not required when the LAN endpoint is reachable. Allow TCP 24871 on the relay's
local/private network firewall.

## Using Handclip

The Handclip icon in the macOS menu bar or Windows system tray shows the
connection status and last clipboard activity, and provides:

- Send Clipboard to each configured device, or to all devices
- Settings
- Open Received Files and Open Logs
- Restart and Quit

Sends update the selected recipient's clipboard. If a target is offline, the
sender reports that nothing was delivered. Clipboard file transfers are staged in:

- macOS: `~/Library/Caches/Handclip/received`
- Windows: `%LOCALAPPDATA%\Handclip\received`

**Open Received Files** opens this clipboard staging directory. Each transfer gets its
own subfolder, avoiding overwrites. Failed integrity checks leave files hidden
in a partial staging directory.

Large transfers use bounded per-connection queues with backpressure, independent
socket reads/writes, and cancellation-safe framing. This avoids dropping chunks
when a slower receiver falls behind. Transfer resumption across a receiver or
relay restart is not implemented: retry an interrupted transfer. Do not treat a
queued request or relay acknowledgement as successful delivery; wait for the
receiver's confirmation.

File and folder names must be portable across macOS and Windows. Symlinks are
rejected instead of being followed.

Settings live in `config.json` in the Handclip application-data directory
(`~/Library/Application Support/Handclip` or `%LOCALAPPDATA%\Handclip`). The token
is stored separately in a `token` file there and never in `config.json`.

## Workspace

- `handclip-core`: wire protocol and framed transport
- `handclip-relay`: authenticated targeted-delivery relay (library and binary)
- `handclip-agent`: menu-bar/tray clipboard agent; can also host the relay
- `handclip-cli`: diagnostic client with `send`, `watch`, and `status`

## Local development

Start a relay and two command-line clients:

```sh
export HANDCLIP_TOKEN="replace-with-a-random-secret"
cargo run -p handclip-relay
cargo run -p handclip-cli -- --device laptop watch
cargo run -p handclip-cli -- --device desktop send --target laptop "hello"
```

Run the agent against the local relay:

```sh
cargo run -p handclip-agent -- --device laptop
```

The standalone relay also runs without the agent, e.g. bound to a Tailscale
address:

```sh
HANDCLIP_LISTEN=100.x.y.z:24871 HANDCLIP_TOKEN_FILE=/path/to/token handclip-relay
```

## License

MIT. See [LICENSE](LICENSE).
