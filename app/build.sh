#!/bin/zsh
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
app="$root/app/Voice Assistant.app"
mkdir -p "$app/Contents/MacOS"
sdk=$(xcrun --show-sdk-path)
swiftc -sdk "$sdk" -target arm64-apple-macosx13.0 -O -parse-as-library \
  -o "$app/Contents/MacOS/VoiceAssistant" \
  "$root/app/Sources/Database.swift" \
  "$root/app/Sources/Session.swift" \
  "$root/app/Sources/VoiceAssistantApp.swift" \
  -framework SwiftUI -framework AppKit -lsqlite3
cp "$root/app/Info.plist" "$app/Contents/Info.plist"
echo "$app"
