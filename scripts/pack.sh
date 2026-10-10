#!/usr/bin/env bash
# Build app-claude and pack an installable Space App zip:
#   release/app-claude-<version>-<platform>.zip
#     senclaw-manifest.json, senclaw-hub.json, README.md, app-claude
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) platform=darwin-arm64 ;;
  Darwin-x86_64) platform=darwin-x64 ;;
  Linux-x86_64) platform=linux-x64 ;;
  Linux-aarch64) platform=linux-arm64 ;;
  *) platform="$(uname -s)-$(uname -m)" ;;
esac

cargo build --release
stage="release/claude"
rm -rf "$stage" && mkdir -p "$stage"
cp senclaw-manifest.json senclaw-hub.json README.md "$stage/"
cp target/release/app-claude "$stage/"

out="release/app-claude-${version}-${platform}.zip"
rm -f "$out"
(cd "$stage" && zip -qr "../$(basename "$out")" .)
echo "[pack] $out ($(du -h "$out" | cut -f1))"
