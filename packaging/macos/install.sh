#!/bin/bash
# Installs Handclip.app from an unpacked Handclip-macos.zip and starts it at every login.
# On first launch Handclip opens Settings to finish setup.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
uid="$(id -u)"
label=io.github.handclip.agent
data="$HOME/Library/Application Support/Handclip"
config="$data/config.json"
plist="$HOME/Library/LaunchAgents/$label.plist"
apps=/Applications
[ -w "$apps" ] || apps="$HOME/Applications"
app="$apps/Handclip.app"

# Upgrade from pre-release "Connet" installs: stop old services, move data, and keep
# the relay on the Mac that ran the old relay service.
agents="$HOME/Library/LaunchAgents"
for old in com.neil.connet.agent com.neil.connet.relay io.github.connet.agent; do
    launchctl bootout "gui/$uid/$old" 2>/dev/null || true
done
for dir in "Application Support" Caches; do
    if [ -d "$HOME/Library/$dir/Connet" ] && [ ! -e "$HOME/Library/$dir/Handclip" ]; then
        mv "$HOME/Library/$dir/Connet" "$HOME/Library/$dir/Handclip"
    fi
done
if [ -f "$config" ]; then
    sed -i '' 's#/Connet/#/Handclip/#g' "$config"
    if [ -f "$agents/com.neil.connet.relay.plist" ]; then
        plutil -replace run_relay -bool YES "$config"
    fi
fi
rm -f "$agents"/com.neil.connet.{agent,relay}.plist "$agents/io.github.connet.agent.plist" \
    "$data/bin/connet-agent" "$data/bin/connet-relay" "$data/bin/connet-cli"
for link in /usr/local/bin/connet-cli "$HOME/.local/bin/connet-cli"; do
    if [ -L "$link" ]; then rm -f "$link"; fi
done
rm -rf "$apps/Connet.app"

mkdir -p "$data/logs" "$data/bin" "$agents" "$apps"

launchctl bootout "gui/$uid/$label" 2>/dev/null || true
rm -rf "$app"
ditto "$here/Handclip.app" "$app"
xattr -dr com.apple.quarantine "$app" 2>/dev/null || true

cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$label</string>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProgramArguments</key>
    <array>
        <string>$app/Contents/MacOS/handclip-agent</string>
        <string>--config-file</string>
        <string>$config</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_LOG</key>
        <string>info</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>$data/logs/agent.log</string>
    <key>StandardErrorPath</key>
    <string>$data/logs/agent.error.log</string>
</dict>
</plist>
EOF
launchctl bootstrap "gui/$uid" "$plist" 2>/dev/null \
    || { sleep 2 && launchctl bootstrap "gui/$uid" "$plist"; }

cp "$here/handclip-cli" "$data/bin/handclip-cli"
xattr -d com.apple.quarantine "$data/bin/handclip-cli" 2>/dev/null || true
linked=false
for dir in /usr/local/bin "$HOME/.local/bin"; do
    if [ -d "$dir" ] && [ -w "$dir" ]; then
        ln -sf "$data/bin/handclip-cli" "$dir/handclip-cli"
        linked=true
        break
    fi
done
$linked || echo "handclip-cli installed at $data/bin/handclip-cli (add it to your PATH)."

echo "Handclip is installed at $app and running. On a new install, finish setup in its Settings window."
