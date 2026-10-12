#!/bin/bash
# Build Tusk.app: release binary + Info.plist + the app icon
# (assets/icon/compiled/, made from assets/icon/Tusk.icon by compile-icon.sh).
#   scripts/bundle.sh            → target/release/bundle/Tusk.app
set -euo pipefail
cd "$(dirname "$0")/.."
root="$(pwd)"
cargo build --release
app="$root/target/release/bundle/Tusk.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/tusk "$app/Contents/MacOS/Tusk"
# The icon, precompiled from assets/icon/Tusk.icon by scripts/compile-icon.sh
# (CI's actool can't compile Icon Composer icons). Never ship a placeholder.
for f in Tusk.icns Assets.car; do
  [ -f "$root/assets/icon/compiled/$f" ] || { echo "bundle: missing assets/icon/compiled/$f (run scripts/compile-icon.sh)" >&2; exit 1; }
  cp "$root/assets/icon/compiled/$f" "$app/Contents/Resources/$f"
done
# pg_dump / pg_restore / psql + their libraries, relocatable (Backup / Restore
# work without Homebrew or PostgreSQL on the Mac).
"$root/scripts/bundle-pgtools.sh" "$app/Contents/Resources/pgtools"
# SQL completions + linter (postgres-language-server), no npm needed.
"$root/scripts/bundle-pgls.sh" "$app/Contents/Resources/bin"
"$root/scripts/bundle-sqls.sh" "$app/Contents/Resources/bin"
# In-app updates (src/updater.rs): Sparkle.framework, loaded at runtime.
sparkle="$("$root/scripts/fetch-sparkle.sh")"
mkdir -p "$app/Contents/Frameworks"
ditto "$sparkle/Sparkle.framework" "$app/Contents/Frameworks/Sparkle.framework"
# Licenses of everything bundled (PostgreSQL tools, readline, OpenSSL, …).
"$root/scripts/notices.sh" > "$app/Contents/Resources/THIRD_PARTY_NOTICES.md"
version="${TUSK_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)}"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Tusk</string>
  <key>CFBundleDisplayName</key><string>Tusk</string>
  <key>CFBundleIdentifier</key><string>ai.reyz.tusk</string>
  <key>CFBundleExecutable</key><string>Tusk</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>CFBundleIconFile</key><string>Tusk</string>
  <key>CFBundleIconName</key><string>Tusk</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSHumanReadableCopyright</key><string>© 2026 Alpcan Aydın. Free and open source (MIT).</string>
  <key>SUFeedURL</key><string>https://github.com/alpcanaydin/tusk/releases/latest/download/appcast.xml</string>
  <key>SUPublicEDKey</key><string>wFCNHPe+tnFm2E71og2InjbeJqLB+lpWg3vYmiTlmWY=</string>
  <key>SUEnableAutomaticChecks</key><true/>
  <key>SUAutomaticallyUpdate</key><true/>
  <key>SUVerifyUpdateBeforeExtraction</key><true/>
</dict></plist>
PLIST
# Distribution signature: a Developer ID, hardened runtime,
# timestamped (what notarization needs). $TUSK_DIST_IDENTITY overrides.
# The SHA-1 hash is extracted, not the name: names can hold non-ASCII,
# which mangles between shells (see sign-dev.sh).
dist="${TUSK_DIST_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null \
  | awk '/Developer ID Application/ { print $2; exit }')}"
if [ -n "$dist" ]; then
  # Nested code first (the pg tools, Sparkle's helpers inside out), then the app.
  for f in "$app"/Contents/Resources/pgtools/lib/* "$app"/Contents/Resources/pgtools/bin/* \
           "$app"/Contents/Resources/bin/*; do
    codesign --force --options runtime --timestamp --sign "$dist" "$f"
  done
  fw="$app/Contents/Frameworks/Sparkle.framework/Versions/B"
  for f in "$fw/XPCServices/Installer.xpc" "$fw/XPCServices/Downloader.xpc" "$fw/Autoupdate" "$fw/Updater.app"; do
    codesign --force --options runtime --timestamp --sign "$dist" "$f"
  done
  codesign --force --options runtime --timestamp --sign "$dist" "$app/Contents/Frameworks/Sparkle.framework"
  codesign --force --options runtime --timestamp --sign "$dist" \
    --identifier ai.reyz.tusk "$app"
else
  "$root/scripts/sign-dev.sh" "$app" || true
fi
echo "$app"
