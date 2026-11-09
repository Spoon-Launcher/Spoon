# Spoon — Agent Guide

Spoon is a free, open-source desktop GUI for creating and managing Minecraft servers. It is written
in Rust (stable, edition 2021) for performance and cross-platform compatibility (Windows, macOS,
Linux).

## Read first

- [FEATURES.md](./FEATURES.md) — feature list and status (✅ Done · 🟡 In Progress · ⬜ Planned).
  Check it before starting work and **update it when a feature's status changes**.
- [README.md](./README.md) — user-facing overview and build instructions. Keep it in sync if you
  change build steps or user-visible capabilities.

## Project layout

Cargo workspace (`resolver = "2"`) with two crates:

| Path | Role |
|------|------|
| `crates/spoon-core` | UI-free library: all logic and I/O. |
| `crates/spoon-gui` | `eframe`/`egui` 0.31 desktop app; binary is `spoon-gui`. |
| `assets/` | Fonts and images embedded by the GUI. |
| `scripts/build-macos.sh` | Release build + packaging into `dist/` for all platforms (macOS host). |
| `scripts/build-linux.sh` | Release build + packaging into `dist/` for Linux + Windows (Linux host). |
| `scripts/build-windows.ps1` | Native Windows release build + packaging into `dist/`. |
| `.cargo/config.toml` | Static CRT on Windows only. |

`spoon-core` modules: `config` (server/app settings persistence), `process` (server child process,
log stream, state), `mcjars` (MCJars API: software/version lists, jar download),
`modrinth` (Modrinth API: plugin/mod search filtered by server software, version lists,
installs into plugins//mods), `java` (private JDK
runtimes), `eula`, `files` (file browser/editor helpers), `highlight` (syntax highlighting),
`backup` (server folder snapshots under `Spoon/backups/<server-id>/`, with
restore), `updater` (GitHub Releases
update check/download).

`spoon-gui`: `app.rs` (all screens and app state — very large), `theme.rs` (colors, card styling,
fonts), `main.rs` (entry point), `lib.rs` (exposes `app` and `theme` so tests can use them).

## Architecture rules

- **Logic belongs in `spoon-core`; `spoon-gui` only renders and wires events.** No egui types in
  `spoon-core`. New non-UI behavior goes in core (new module → declare it in `lib.rs`, re-export
  commonly used types there).
- Never block the UI thread: network, process, and disk-heavy work runs on background threads and
  reports back to the UI (see how existing features in `app.rs` do it). `reqwest` is used in
  `blocking` mode with `rustls-tls` (no OpenSSL) — keep it that way.
- Persisted data lives under `dirs::config_dir()/Spoon` (`servers.json`, `settings.json`,
  `servers/<name>/`, `backups/<server-id>/`, private JDKs). Use the `ServerStore`/`AppSettings` APIs and
  `sanitize_folder_name`/`unique_server_dir` rather than building paths by hand.
- Keep persisted structs backward compatible: new fields need `#[serde(default)]` so older
  `servers.json`/`settings.json` still load (there are tests for this in `config.rs`).
- Server jars and icons come from the MCJars API at runtime; don't hardcode download URLs per
  software.
- Must work on Windows, macOS, and Linux: use `std::path`/`PathBuf`, avoid shell-specific
  commands, and gate platform code with `cfg(target_os = ...)`.
- Don't add a dependency unless needed; reuse what's in the crate's `Cargo.toml` first. Prefer
  `default-features = false` with only required features, matching existing entries.

## Commands

```console
cargo run -p spoon-gui                  # run the app
cargo build --release -p spoon-gui      # release build -> target/release/
cargo test --workspace                  # unit + egui_kittest layout tests
cargo clippy --workspace --all-targets  # lint
cargo fmt --all                         # format
./scripts/build-macos.sh                  # all platforms (macOS host only)
./scripts/build-linux.sh                  # Linux + Windows (Linux host)
.\scripts\build-windows.ps1               # Windows exe + zip -> dist/ (Windows host)
```

Before finishing a change: `cargo fmt --all`, `cargo clippy --workspace --all-targets` with no new
warnings, and `cargo test --workspace` passing.

## Code style

- Clean and consistent; no wasted operations or steps (no redundant clones, allocations, repeated
  work per frame, or dead code). Match the surrounding code's patterns and naming.
- Idiomatic Rust: `Result`/`Option` and `?` over `unwrap()`/`expect()` in non-test code; handle
  I/O and network failures gracefully and surface useful errors to the user instead of panicking.
- egui runs immediate-mode: keep per-frame work cheap, cache anything expensive, and use `theme.rs`
  constants/helpers (`theme::card()`, `theme::PRIMARY`, `theme::TEXT`, …) instead of ad-hoc colors.
- Comment only what needs clarification (the *why*); doc-comment public items in `spoon-core`.
- When adding to `app.rs`, prefer extracting focused helpers/functions over growing existing ones;
  put logic that can be unit tested in `spoon-core`.

## Testing

- Unit tests live next to the code in `#[cfg(test)] mod tests` (see `config.rs`).
- GUI layout regressions use `egui_kittest` in `crates/spoon-gui/tests/` (see
  `dashboard_layout.rs`). Add one when changing layout that has previously broken.
- Tests must not hit the network or write outside a temp dir.
- Add or update tests for any bug fix or new core logic.

## Packaging and release

- Release asset names are consumed by `spoon-core/src/updater.rs` (`Spoon-Launcher-Windows-x86_64.exe`,
  `Spoon-Launcher-Linux-x86_64.AppImage`, `Spoon-Launcher-macOS-{Intel,AppleSilicon}.zip`). Changing
  names in the `build-*` scripts requires changing the updater too. Note the macOS
  script can't produce the AppImage (it's built on Linux/CI), while the Linux
  script can't produce the macOS artifacts at all (Apple SDK is macOS-only) — it
  ships the bare `Spoon-Launcher-Linux-x86_64` binary and the `.deb`.
- `tools/` and `dist/` are generated and gitignored; never commit them or `target/`.
- App version comes from `crates/spoon-gui/Cargo.toml`.

## Working agreements

- Keep changes focused; don't refactor unrelated code or reformat files you aren't changing.
- Don't commit secrets, and don't add telemetry or network calls beyond MCJars and GitHub Releases
  without explicit approval.
- Ask before making design decisions that change user-facing behavior or persisted formats.