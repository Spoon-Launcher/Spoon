#!/bin/bash
# scripts/build-macos.sh — build Spoon on a macOS host for every platform under
# "Platform / Packaging" in FEATURES.md and package the results into dist/:
#
#   Spoon-Launcher-Windows-x86_64.exe (+ .zip)
#   Spoon-Launcher-Linux-x86_64      (binary) + spoon_<version>_amd64.deb
#   Spoon-Launcher-macOS-Intel.zip, Spoon-Launcher-macOS-AppleSilicon.zip
#
# Cross-compilation toolchains are downloaded into tools/ on first run:
#   tools/llvm-mingw  Windows toolchain (https://github.com/mstorsjo/llvm-mingw)
#   tools/zig         Linux toolchain   (https://ziglang.org, via cargo-zigbuild)
#
# Note: the Linux AppImage cannot be produced on macOS (appimagetool runs on
# Linux only); build it with scripts/build-linux.sh or in CI. The updater on
# Linux looks for the AppImage asset, so publish one before expecting updates
# there.
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
export PATH="$HOME/.cargo/bin:$PATH"

DIST=dist
MINGW=tools/llvm-mingw/bin
ZIG=tools/zig

log() { printf '\n=== %s ===\n' "$*"; }

app_version() { cargo pkgid -p spoon-gui | sed 's/.*#//'; }

# Extract a downloaded toolchain archive (single top-level dir) in place.
flatten() { # $1 = archive, $2 = dest dir
    rm -rf "$2" && mkdir -p "$2"
    case "$1" in
        *.zip) unzip -q "$1" -d "$2" ;;
        *) tar -xJf "$1" -C "$2" ;;
    esac
    local inner
    inner=$(find "$2" -mindepth 1 -maxdepth 1 -type d | head -1)
    if [ -n "$inner" ]; then mv "$inner"/* "$2"/ && rmdir "$inner"; fi
}

ensure_targets() {
    rustup target add x86_64-apple-darwin x86_64-pc-windows-gnullvm x86_64-unknown-linux-gnu
}

ensure_llvm_mingw() {
    [ -x "$MINGW/x86_64-w64-mingw32-clang" ] && return
    log "Downloading llvm-mingw (Windows toolchain)"
    local url
    url=$(curl -fsSL https://api.github.com/repos/mstorsjo/llvm-mingw/releases/latest | python3 -c "
import json, sys
for a in json.load(sys.stdin)['assets']:
    if 'macos-universal' in a['name']:
        print(a['browser_download_url']); break
")
    curl -fSL --retry 3 -o /tmp/llvm-mingw.tar.xz "$url"
    flatten /tmp/llvm-mingw.tar.xz tools/llvm-mingw
    rm -f /tmp/llvm-mingw.tar.xz
}

ensure_zig() {
    [ -x "$ZIG/zig" ] && return
    log "Downloading zig (Linux toolchain)"
    local url
    url=$(python3 - <<'EOF'
import json, platform, urllib.request
d = json.load(urllib.request.urlopen('https://ziglang.org/download/index.json'))
latest = max((k for k in d if k != 'master'), key=lambda v: [int(x) for x in v.split('.')])
host = 'aarch64-macos' if platform.machine() == 'arm64' else 'x86_64-macos'
print(d[latest][host]['tarball'])
EOF
)
    curl -fSL --retry 3 -o /tmp/zig.tar.xz "$url"
    flatten /tmp/zig.tar.xz tools/zig
    rm -f /tmp/zig.tar.xz
}

build_macos() {
    log "Building macOS (Apple Silicon)"
    cargo build --release -p spoon-gui
    log "Building macOS (Intel)"
    cargo build --release -p spoon-gui --target x86_64-apple-darwin
}

build_windows() {
    log "Building Windows (x86_64)"
    export PATH="$PWD/$MINGW:$PATH"
    cargo build --release -p spoon-gui --target x86_64-pc-windows-gnullvm
}

build_linux() {
    log "Building Linux (x86_64)"
    export PATH="$PWD/$ZIG:$PATH"
    cargo zigbuild --release -p spoon-gui --target x86_64-unknown-linux-gnu
}

package() {
    local version
    version=$(app_version)
    log "Packaging v$version into $DIST/"
    rm -rf "$DIST" && mkdir -p "$DIST"

    # Windows: bare .exe is the release asset the updater downloads.
    cp target/x86_64-pc-windows-gnullvm/release/spoon-gui.exe "$DIST/Spoon-Launcher-Windows-x86_64.exe"
    (cd "$DIST" && zip -q Spoon-Launcher-Windows-x86_64.zip Spoon-Launcher-Windows-x86_64.exe)

    # Linux: bare binary + .deb.
    cp target/x86_64-unknown-linux-gnu/release/spoon-gui "$DIST/Spoon-Launcher-Linux-x86_64"
    make_deb "$version"

    # macOS: one zip per architecture (updater assets).
    (cd target/release && zip -q "$ROOT/$DIST/Spoon-Launcher-macOS-AppleSilicon.zip" spoon-gui)
    (cd target/x86_64-apple-darwin/release && zip -q "$ROOT/$DIST/Spoon-Launcher-macOS-Intel.zip" spoon-gui)
}

# Assemble a .deb by hand (no dpkg needed): debian-binary + control.tar.gz +
# data.tar.gz in an ar archive.
make_deb() { # $1 = version
    local pkg=/tmp/spoon-deb
    rm -rf "$pkg"
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
    # GNU-style ar (llvm-ar) if the downloaded toolchain provides it; the
    # BSD ar shipped with macOS produces archives some dpkg builds reject.
    local ar=ar
    [ -x "$ROOT/$MINGW/llvm-ar" ] && ar="$ROOT/$MINGW/llvm-ar"
    (cd "$pkg" && "$ar" r "$ROOT/$DIST/spoon_${1}_amd64.deb" debian-binary control.tar.gz data.tar.gz)
    rm -rf "$pkg"
}

main() {
    ensure_targets
    ensure_llvm_mingw
    ensure_zig
    build_macos
    build_windows
    build_linux
    package
    log "Done"; ls -lh "$DIST"
}

main "$@"
