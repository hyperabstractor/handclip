# Handclip

Handclip is a private, cross-platform clipboard bus for your own Macs and Windows
PCs. One always-on device runs a small relay; every device keeps an outbound,
full-duplex connection to it.

Copy normally, then press a shortcut to send the clipboard to a specific device
(or to all of them). Text, images, files, and folders are supported. File
contents are streamed in bounded chunks, verified with SHA-256 on each receiver,
and staged locally before Finder or Explorer receives the new file clipboard.

## Security

Handclip authenticates devices with a shared token but does **not** encrypt
traffic. Run it over an encrypted private network such as
[Tailscale](https://tailscale.com) or WireGuard, or a LAN you trust. Never expose
the relay port (24871) to the internet.

## Install

### macOS

Build the installer bundle on a Mac with a Rust toolchain:

```sh
packaging/macos/package.sh
```

Copy `target/dist/Handclip-macos.zip` to each Mac, unzip it, and run
`./install.sh`. It installs `Handclip.app` in `/Applications`, starts it at every
login, and links `handclip-cli` into your PATH. Re-run it to upgrade.

### Windows

Build `handclip-agent.exe` with `cargo build --release -p handclip-agent`, put it
next to `packaging/windows/install-agent.ps1`, and run the script. It installs
the agent for the current user and starts it at every sign-in.

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

## Using Handclip

The Handclip icon in the macOS menu bar or Windows system tray shows the
connection status and last clipboard activity, and provides:

- Send Clipboard to each configured device, or to all devices
- Settings
- Open Received Files and Open Logs
- Restart and Quit

Only the selected recipient's clipboard changes. If a target is offline, the
sender reports that nothing was delivered. Received files are stored in:

- macOS: `~/Library/Caches/Handclip/received`
- Windows: `%LOCALAPPDATA%\Handclip\received`

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
