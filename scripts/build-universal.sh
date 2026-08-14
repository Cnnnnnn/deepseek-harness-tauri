#!/usr/bin/env bash
# 构建 Tauri universal 包（arm64 + x86_64 → lipo → 签名 → zip + dmg）
set -euo pipefail
cd "$(dirname "$0")/.."

TAURI="./node_modules/.bin/tauri"
ARM64="src-tauri/target/aarch64-apple-darwin/release/bundle/macos/DeepSeek Harness.app"
X64="src-tauri/target/x86_64-apple-darwin/release/bundle/macos/DeepSeek Harness.app"
UNI="src-tauri/target/universal-apple-darwin/release/bundle/macos/DeepSeek Harness.app"

echo "[build] 构建 arm64..."
"$TAURI" build --target aarch64-apple-darwin
echo "[build] 构建 x86_64..."
"$TAURI" build --target x86_64-apple-darwin

mkdir -p "$(dirname "$UNI")"
rm -rf "$UNI"
cp -R "$ARM64" "$UNI"
lipo -create "$ARM64/Contents/MacOS/dsh-tauri" "$X64/Contents/MacOS/dsh-tauri" -output "$UNI/Contents/MacOS/dsh-tauri"
codesign --force --deep --sign - "$UNI"

echo "[build] 生成 zip ..."
ditto -c -k --keepParent "$UNI" "DeepSeek.Harness-universal-mac.zip"
echo "[build] 生成 dmg ..."
hdiutil create -volname "DeepSeek Harness" -srcfolder "$UNI" -ov -format UDZO "DeepSeek.Harness-universal-mac.dmg"

echo "[build] 完成："
ls -la DeepSeek.Harness-universal-mac.zip DeepSeek.Harness-universal-mac.dmg
