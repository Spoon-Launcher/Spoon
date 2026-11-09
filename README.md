<p align="center">
  <img src="https://cdn.galactiq.net/Spoon/images/Spoon-Banner.png" alt="Spoon">
</p>

# Spoon

🚧 **Development notice:** Spoon is still in development so expect bugs. Features may also be incomplete or change during development.

Spoon is a free, open-source desktop app for creating and managing Minecraft servers. Built with
Rust, Spoon brings server setup, live console control, monitoring, and file management together in
one straightforward interface.

Join the [Discord](https://discord.gg/7UEZnsMMkf) for updates and to talk with the community!

## What you can do

- **Manage server instances** — create servers with your chosen software, version, memory, and JVM
  arguments.
- **Set up server software** — choose from vanilla, Paper, Fabric, Forge, and 20+ other server
  types.
- **Use a live console** — view colorized output, send commands, search and filter the console, and
  browse command history.
- **Monitor activity** — check CPU and memory usage, uptime, online players, and the server IP.
- **Manage players** — browse live online and offline player lists
- **Install plugins & mods** — browse Modrinth and download content matching your server's software.
- **Work with server files** — browse and edit files without leaving Spoon.
- **Back up server files** — snapshot a server's folder into Spoon's backups
  directory and restore a snapshot later.
- **Built In Updates** — quickly and easily update from within the app.

See [FEATURES.md](./FEATURES.md) for the full feature list and development status.

## Building from source

### Requirements

- [Rust](https://www.rust-lang.org/tools/install) — stable toolchain via rustup.
- **Single-platform build:** any macOS, Windows, or Linux host.
- **`./scripts/build-macos.sh` (all platforms):** a macOS host, plus:
  - `cargo-zigbuild` — install with `cargo install cargo-zigbuild` (used for the
    Linux cross-build),
  - `python3` — used to resolve toolchain download URLs,
  - network access — the script downloads the Windows and Linux cross toolchains
    on first run (into the gitignored `tools/` directory).
- **`./scripts/build-linux.sh` (Linux + Windows):** a Linux host, plus `curl`
  and `python3` (the Windows cross toolchain downloads into `tools/` on first
  run; macOS artifacts can't be built on Linux — the Apple SDK is macOS-only).
- **`.\scripts\build-windows.ps1` (Windows only):** a Windows host with the
  Visual Studio Build Tools.
- Spoon preloads server software versions and icons from the
  [MCJars](https://mcjars.app) API during startup. An internet connection is
  needed to populate that catalog and to create servers; if MCJars is
  unavailable, Spoon continues with the UI and retries the catalog later.

### Run from source

```console
cargo run -p spoon-gui
```

### Release build for the current platform

```console
cargo build --release -p spoon-gui
```

The executable lands in `target/release/`.

### Windows release build

On a Windows host (Rust plus the Visual Studio Build Tools), run:

```powershell
.\scripts\build-windows.ps1
```

It builds with the MSVC toolchain and writes
`dist/Spoon-Launcher-Windows-x86_64.exe` and its `.zip`.

### Platform release builds

Each host has a script that builds everything that host can produce, packaged
into `dist/`:

- **macOS host** — `./scripts/build-macos.sh` builds every target under
  *Platform / Packaging* in [FEATURES.md](./FEATURES.md):
  `Spoon-Launcher-Windows-x86_64.exe` (+ `.zip`),
  `Spoon-Launcher-Linux-x86_64` and `spoon_<version>_amd64.deb`, and
  `Spoon-Launcher-macOS-Intel.zip` + `Spoon-Launcher-macOS-AppleSilicon.zip`.
- **Linux host** — `./scripts/build-linux.sh` builds the Linux binary, the
  `.deb`, and the Windows `.exe`/`.zip`. macOS artifacts require the Apple SDK
  and can't be built on Linux; build them on a Mac or in CI. The Linux
  AppImage (the asset the in-app updater downloads) needs `appimagetool` —
  build it on Linux in CI alongside the `.deb`.
- **Windows host** — `.\scripts\build-windows.ps1` builds the Windows `.exe`
  and `.zip` only.

The first run downloads the cross toolchains (llvm-mingw for Windows, zig for
Linux on macOS hosts) and installs the Rust targets; later runs reuse
everything and only rebuild what changed.

### Cross-compiling manually

```console
rustup target add x86_64-apple-darwin x86_64-pc-windows-gnullvm x86_64-unknown-linux-gnu

# Windows (llvm-mingw toolchain, fetched into tools/ by the build scripts or from
# https://github.com/mstorsjo/llvm-mingw/releases)
export PATH="$PWD/tools/llvm-mingw/bin:$PATH"
cargo build --release -p spoon-gui --target x86_64-pc-windows-gnullvm

# Linux (zig toolchain + cargo-zigbuild)
export PATH="$PWD/tools/zig:$PATH"
cargo zigbuild --release -p spoon-gui --target x86_64-unknown-linux-gnu

# macOS Intel (builds directly from any Apple Silicon Mac)
cargo build --release -p spoon-gui --target x86_64-apple-darwin
```

Cross-compiled binaries land in `target/<triple>/release/`.

## Project docs

- [Feature list and roadmap](./FEATURES.md)
