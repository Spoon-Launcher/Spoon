#!/bin/bash
# scripts/build-linux.sh — build Spoon on a Linux (x86_64) host and package the
# results into dist/:
#
#   Spoon-Launcher-Linux-x86_64      (binary) + spoon_<version>_amd64.deb
#   Spoon-Launcher-Windows-x86_64.exe (+ .zip)
#
# The Windows cross toolchain is downloaded into tools/ on first run:
#   tools/llvm-mingw  Windows toolchain (https://github.com/mstorsjo/llvm-mingw)
#
# Notes:
#   - macOS artifacts cannot be built on Linux: linking them requires the
#     Apple SDK, which Apple only licenses for use on Apple hardware. Build
#     them with scripts/build-macos.sh or in CI.
#   - The Linux AppImage (the asset the updater downloads on Linux) needs
#     appimagetool, which must run on Linux with FUSE; this script ships the
#     bare binary and the .deb instead. Build the AppImage in CI or with
#     appimagetool manually.
#   - The downloaded llvm-mingw builds target ubuntu-22.04 (glibc >= 2.35).
#     On older distros, extract an llvm-mingw linux tarball into
#     tools/llvm-mingw yourself (or install a distro mingw-w64 LLVM toolchain
#     and make sure `clang` links against its sysroot); ensure_llvm_mingw
#     skips the download whenever tools/llvm-mingw is already populated.
#
# Requirements: rustup, curl, python3 (resolves toolchain download URLs,
# creates the Windows zip).
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
export PATH="$HOME/.cargo/bin:$PATH"

DIST=dist
MINGW=tools/llvm-mingw/bin

log() { printf '\n=== %s ===\n' "$*"; }

app_version() { cargo pkgid -p spoon-gui | sed 's/.*#//'; }

# Extract a downloaded toolchain archive (single top-level dir) in place.
flatten() { # $1 = archive, $2 = dest dir
    rm -rf "$2" && mkdir -p "$2"
    tar -xJf "$1" -C "$2"
    local inner
    inner=$(find "$2" -mindepth 1 -maxdepth 1 -type d | head -1)
    if [ -n "$inner" ]; then mv "$inner"/* "$2"/ && rmdir "$inner"; fi
}

ensure_targets() {
    rustup target add x86_64-unknown-linux-gnu x86_64-pc-windows-gnullvm
}

ensure_llvm_mingw() {
    [ -x "$MINGW/x86_64-w64-mingw32-clang" ] && return
    log "Downloading llvm-mingw (Windows toolchain)"
    local url
    url=$(curl -fsSL https://api.github.com/repos/mstorsjo/llvm-mingw/releases/latest | python3 -c "
import json, sys
for a in json.load(sys.stdin)['assets']:
    if 'ucrt-ubuntu' in a['name'] and 'x86_64' in a['name']:
        print(a['browser_download_url']); break
")
    local tmp
    tmp=$(mktemp -d)
    curl -fSL --retry 3 -o "$tmp/llvm-mingw.tar.xz" "$url"
    flatten "$tmp/llvm-mingw.tar.xz" tools/llvm-mingw
    rm -rf "$tmp"
}

build_linux() {
    log "Building Linux (x86_64)"
    cargo build --release -p spoon-gui --target x86_64-unknown-linux-gnu
}

build_windows() {
    log "Building Windows (x86_64)"
    export PATH="$PWD/$MINGW:$PATH"
    cargo build --release -p spoon-gui --target x86_64-pc-windows-gnullvm
}

package() {
    local version
    version=$(app_version)
    log "Packaging v$version into $DIST/"
    rm -rf "$DIST" && mkdir -p "$DIST"

    # Linux: bare binary + .deb.
    cp target/x86_64-unknown-linux-gnu/release/spoon-gui "$DIST/Spoon-Launcher-Linux-x86_64"
    make_deb "$version"

    # Windows: bare .exe is the release asset the updater downloads.
    cp target/x86_64-pc-windows-gnullvm/release/spoon-gui.exe "$DIST/Spoon-Launcher-Windows-x86_64.exe"
    (cd "$DIST" && python3 -m zipfile -c Spoon-Launcher-Windows-x86_64.zip Spoon-Launcher-Windows-x86_64.exe)
}

# Assemble a .deb by hand (no dpkg needed): debian-binary + control.tar.gz +
# data.tar.gz in an ar archive.
make_deb() { # $1 = version
    local pkg
    pkg=$(mktemp -d)
    mkdir -p "$pkg/pkg/usr/bin" "$pkg/pkg/usr/share/applications" \
             "$pkg/pkg/usr/share/icons/hicolor/256x256/apps" "$pkg/pkg/DEBIAN"
    install -m 755 target/x86_64-unknown-linux-gnu/release/spoon-gui "$pkg/pkg/usr/bin/Spoon-Launcher"
    install -m 644 assets/images/Spoon-No-Border.png "$pkg/pkg/usr/share/icons/hicolor/256x256/apps/spoon.png"
    cat > "$pkg/pkg/usr/share/applications/spoon.desktop" <<'EOF'
[Desktop Entry]
Name=Spoon
Comment=Minecraft server GUI manager
Exec=Spoon-Launcher
Icon=spoon
Terminal=false
Type=Application
Categories=Game;Utility;
EOF
    cat > "$pkg/pkg/DEBIAN/control" <<EOF
Package: spoon
Version: $1
Section: games
Priority: optional
Architecture: amd64
Maintainer: Spoon
Description: Spoon - Minecraft server GUI manager
 Spoon keeps your Minecraft servers in one place: create instances from
 vanilla, Paper, Fabric, Forge and more, watch live console output, and
 track CPU/RAM usage per server.
Depends: libegl1, libgl1, libxkbcommon0
EOF
    echo 2.0 > "$pkg/debian-binary"
    tar -czf "$pkg/control.tar.gz" -C "$pkg/pkg/DEBIAN" .
    tar -czf "$pkg/data.tar.gz" -C "$pkg/pkg" usr
    # Prefer llvm-ar from the downloaded toolchain; any GNU ar works for dpkg.
    local ar=ar
    [ -x "$ROOT/$MINGW/llvm-ar" ] && ar="$ROOT/$MINGW/llvm-ar"
    (cd "$pkg" && "$ar" r "$ROOT/$DIST/spoon_${1}_amd64.deb" debian-binary control.tar.gz data.tar.gz)
    rm -rf "$pkg"
}

main() {
    ensure_targets
    ensure_llvm_mingw
    build_linux
    build_windows
    package
    log "Done"; ls -lh "$DIST"
}

main "$@"
