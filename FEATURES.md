# Features

Status: ✅ Done · 🟡 In Progress · ⬜ Planned

## Core

- ✅ Multiple server instance management (rename, duplicate, remove)
- ✅ Create server instance (name, software, version, memory, JVM args)
- ✅ Start / stop / kill server
- ✅ Restart server
- ✅ Persist instance list between launches
- ⬜ Native UPNP
- ✅ Port Assigner (Start at 25565 and work up. Check for ports that are already assigned/in use to make sure there are no conflicts)
- ⬜ Headless Mode / Multi-Node local link
- ⬜ Support for Rust based server softwares
- ✅ Java version management

## Console

- ✅ Live console output (stdout/stderr, colorized)
- ✅ Send console commands (stdin)
- ✅ Console history (Up/Down) / search / filter
- ✅ Copy console output
- ✅ Server IP Address viewer

## Server Setup

- ✅ Server jar download / version picker (MCJars API — vanilla, Paper, Fabric, Forge, 20+ others)
- ✅ EULA acceptance helper
- ✅ Edit server.properties from the GUI
- ✅ Version changer in server settings
- ✅ Import existing server folder into Spoon's server directory

## Monitoring

- ✅ CPU / RAM usage per server
- ✅ Online player list
- ✅ Uptime display

## Quality of Life

- ✅ In-app update checker & downloader (Pulls from GitHub Releases)
- ⬜ Auto-restart on crash
- ⬜ Scheduled restarts / tasks
- ✅ Backups (backup the server root folder under Spoon/backups)
- ⬜ Databases (Hook into a database server to allow creation of server specific databases)
- ⬜ Desktop notifications (server up / down)
- ⬜ Modrinth/Spigot Plugin Installer
- ⬜ Modrinth/Curseforge Mod Installer
- ✅ Player Management
- ✅ File Browser / File editor / Open server folder
- ✅ Private Java runtimes
- ⬜ Playit.gg Tunnel Support

## Platform / Packaging

- ✅ Windows 10+ (via `.\scripts\build-windows.ps1` or `./scripts/build-{macos,linux}.sh`)
- 🟡 Linux (.deb via `./scripts/build-linux.sh` or `./scripts/build-macos.sh`; AppImage needs Linux tooling/CI)
- ✅ macOS (Intel & Apple Silicon, via `./scripts/build-macos.sh`)
