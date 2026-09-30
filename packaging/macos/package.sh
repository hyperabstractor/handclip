#!/bin/bash
# Builds target/dist/Handclip-macos.zip: Handclip.app, handclip-cli, and install.sh.
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
here="$repo/packaging/macos"
dist="$repo/target/dist/Handclip"
app="$dist/Handclip.app"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$repo/Cargo.toml" | head -1)"

# Set HANDCLIP_TARGETS="aarch64-apple-darwin x86_64-apple-darwin" for a universal build.
bin="$repo/target/release"
if [ -z "${HANDCLIP_TARGETS:-}" ]; then
    cargo build --release --manifest-path "$repo/Cargo.toml" -p handclip-agent -p handclip-cli
else
    bin="$repo/target/universal"
    mkdir -p "$bin"
    for target in $HANDCLIP_TARGETS; do
        cargo build --release --manifest-path "$repo/Cargo.toml" --target "$target" \
            -p handclip-agent -p handclip-cli
    done
    for name in handclip-agent handclip-cli; do
        lipo -create -output "$bin/$name" \
            $(for target in $HANDCLIP_TARGETS; do echo "$repo/target/$target/release/$name"; done)
    done
fi

rm -rf "$dist" "$repo/target/dist/Handclip-macos.zip"
mkdir -p "$app/Contents/MacOS"
cp "$bin/handclip-agent" "$app/Contents/MacOS/"
cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>io.github.handclip</string>
    <key>CFBundleName</key>
    <string>Handclip</string>
    <key>CFBundleExecutable</key>
    <string>handclip-agent</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$version</string>
    <key>CFBundleVersion</key>
    <string>$version</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
EOF
codesign --force --sign - "$app"

cp "$bin/handclip-cli" "$here/install.sh" "$dist/"
ditto -c -k --norsrc --noextattr --keepParent "$dist" "$repo/target/dist/Handclip-macos.zip"
echo "Built $repo/target/dist/Handclip-macos.zip"
