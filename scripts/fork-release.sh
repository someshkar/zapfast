#!/usr/bin/env bash
# Builds the fork's release binaries without CI runners, then publishes them.
#
#   scripts/fork-release.sh v0.20.1-fork.2 [--no-publish]
#
# Linux is built here, on the machine the binary will run on. macOS arm64 and
# Windows x86_64 are cross-built on the MacBook over SSH (see MAC_HOST), which
# has the Apple SDK for the first and mingw-w64 for the second.
#
# Artifacts land in ./dist and are attached to a GitHub release on this fork.
set -euo pipefail

tag="${1:?usage: scripts/fork-release.sh <tag> [--no-publish]}"
publish=1
[ "${2:-}" = "--no-publish" ] && publish=0
mac="${MAC_HOST:-m5-max-macbook}"
here="$(cd "$(dirname "$0")/.." && pwd)"
version="${tag#v}"
dist="$here/dist"

# Where the Mac keeps its checkout, its toolchain shims, and its logs.
mac_dir="${MAC_DIR:-zapfast-fork}"

step() { printf '\n== %s\n' "$*"; }

step "Linux x86_64 (here)"
cd "$here"
cargo build --locked --release
linux_name="zapfast-${tag}-x86_64-unknown-linux-gnu"
rm -rf "$dist/$linux_name"
mkdir -p "$dist/$linux_name"
cp target/release/zapfast "$dist/$linux_name/"
cp README.md LICENSE THIRD-PARTY-NOTICES.md packaging/zapfast-portable.txt "$dist/$linux_name/"
cp --parents packaging/applications/zapfast.desktop packaging/icons/zapfast.svg "$dist/$linux_name/"
tar -C "$dist" -czf "$dist/$linux_name.tar.gz" "$linux_name"

step "macOS arm64 and Windows x86_64 (on $mac)"
rsync -az --delete --exclude 'target/' --exclude '.git/' --exclude 'dist/' \
    "$here/" "$mac:$mac_dir/"

# mingw-w64 from Homebrew, and cmake for the Opus build script.
ssh "$mac" 'bash -s' <<'REMOTE_SETUP'
export PATH=/opt/homebrew/bin:$PATH
set -euo pipefail
command -v cmake >/dev/null || brew install cmake
command -v x86_64-w64-mingw32-gcc >/dev/null || brew install mingw-w64
rustup target add x86_64-pc-windows-gnu --toolchain 1.98.0-aarch64-apple-darwin >/dev/null 2>&1 || true
mkdir -p /tmp/zapfast-mingw
cat > /tmp/zapfast-mingw/env.sh <<'EOF'
export PATH=/opt/homebrew/bin:$PATH
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
export CARGO_TARGET_X86_64_PC_WINDOWS_GNU_AR=x86_64-w64-mingw32-ar
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar
export RC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-windres
export CMAKE_TOOLCHAIN_FILE_x86_64_pc_windows_gnu=/tmp/zapfast-mingw/toolchain.cmake
EOF
cat > /tmp/zapfast-mingw/toolchain.cmake <<'EOF'
set(CMAKE_SYSTEM_NAME Windows)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER x86_64-w64-mingw32-gcc)
set(CMAKE_CXX_COMPILER x86_64-w64-mingw32-g++)
set(CMAKE_RC_COMPILER x86_64-w64-mingw32-windres)
set(CMAKE_FIND_ROOT_PATH /opt/homebrew/x86_64-w64-mingw32)
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
EOF
REMOTE_SETUP

ssh "$mac" "cd $mac_dir && bash -c 'source /tmp/zapfast-mingw/env.sh; set -e; \
    cargo build --locked --release && \
    cargo build --locked --release --target x86_64-pc-windows-gnu'" \
    2>&1 | tail -5

step "Packaging on $mac"
ssh "$mac" "cd $mac_dir && bash -s" <<REMOTE_PACKAGE
set -euo pipefail
mkdir -p dist
rm -rf dist/ZapFast.app "dist/zapfast-${tag}-macos-arm64" "dist/zapfast-${tag}-x86_64-pc-windows-gnu"
# Ad-hoc signature only: this fork has no Developer ID.
bash packaging/macos/bundle.sh target/release/zapfast dist/ZapFast.app "$version"
cp README.md LICENSE THIRD-PARTY-NOTICES.md dist/
(cd dist && zip -qry "zapfast-${tag}-macos-arm64.zip" ZapFast.app README.md LICENSE THIRD-PARTY-NOTICES.md)
rm -rf dist/ZapFast.app dist/README.md dist/LICENSE dist/THIRD-PARTY-NOTICES.md
mkdir -p "dist/zapfast-${tag}-x86_64-pc-windows-gnu"
cp target/x86_64-pc-windows-gnu/release/zapfast.exe "dist/zapfast-${tag}-x86_64-pc-windows-gnu/"
cp README.md LICENSE THIRD-PARTY-NOTICES.md packaging/zapfast-portable.txt "dist/zapfast-${tag}-x86_64-pc-windows-gnu/"
(cd dist && rm -f "zapfast-${tag}-x86_64-pc-windows-gnu.zip" && zip -qry "zapfast-${tag}-x86_64-pc-windows-gnu.zip" "zapfast-${tag}-x86_64-pc-windows-gnu")
REMOTE_PACKAGE

step "Collecting artifacts"
scp -q "$mac:$mac_dir/dist/zapfast-${tag}-macos-arm64.zip" "$dist/"
scp -q "$mac:$mac_dir/dist/zapfast-${tag}-x86_64-pc-windows-gnu.zip" "$dist/"
ls -la "$dist"

if (( ! publish )); then
    exit 0
fi

step "Publishing $tag"
notes="$(mktemp)"
cat > "$notes" <<NOTES
Unsigned builds of this fork, which adds link previews the sender's own machine
generates.

* The composer reads the page behind a link in your draft and shows the card
  before you send it.
* The sent message carries that title, description and thumbnail, so every
  recipient and every linked device sees the preview.
* Only a link in your own draft is fetched. Links other people send are never
  opened, and private, loopback and link-local addresses are refused on every
  redirect hop.
* **Settings → Chats → Preview links** turns it off.

Builds: \`x86_64-unknown-linux-gnu\`, \`aarch64-apple-darwin\`,
\`x86_64-pc-windows-gnu\` (mingw-w64, cross-built).

macOS is ad-hoc signed: on first launch, right-click → Open, or run
\`xattr -dr com.apple.quarantine /Applications/ZapFast.app\`.
NOTES
if gh release view "$tag" --repo "$(git remote get-url origin | sed 's#.*[:/]\([^/]*/[^/]*\)\.git#\1#')" >/dev/null 2>&1; then
    gh release upload "$tag" "$dist/$linux_name.tar.gz" "$dist/zapfast-${tag}-macos-arm64.zip" \
        "$dist/zapfast-${tag}-x86_64-pc-windows-gnu.zip" --clobber
else
    gh release create "$tag" "$dist/$linux_name.tar.gz" "$dist/zapfast-${tag}-macos-arm64.zip" \
        "$dist/zapfast-${tag}-x86_64-pc-windows-gnu.zip" \
        --title "ZapFast fork $tag (link previews)" --prerelease --notes-file "$notes"
fi
rm -f "$notes"
