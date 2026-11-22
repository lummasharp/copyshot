# Copyshot v0.2.0

Copyshot is a lightweight native Windows screenshot utility written in Rust. It runs in the system tray, saves captures to `Pictures/Screenshots` and copies them to the clipboard.

Website: <https://lummasharp.github.io/copyshot/>

## Features

- Capture a selected region with `Print Screen`, with a live size readout while you drag
- Capture the active window with `Alt` + `Print Screen`
- Capture the full virtual desktop with `Shift` + `Print Screen`
- Supports multi-monitor setups at any resolution and display scaling
- Saves PNG files and copies each capture to the clipboard
- Captures taken within the same second get numbered names instead of overwriting each other
- A quiet chime confirms each capture (turn it off from the tray menu)
- Tray menu for capture controls, the screenshots folder, startup and sound settings
- Automatic update checks against GitHub releases, installed with one click
- Only one copy runs at a time

## Controls

| Action | Control |
| --- | --- |
| Capture region | `Print Screen` or double-click the tray icon |
| Capture active window | `Alt` + `Print Screen` |
| Capture full desktop | `Shift` + `Print Screen` |
| Cancel region selection | `Esc` or right-click |

## Install

Download `Copyshot-Setup-<version>.exe` from the [latest release](https://github.com/lummasharp/copyshot/releases/latest). It installs per user without administrator access, starts with Windows, and updates itself when a new release is published.

## Requirements

- Windows 10 (1703) or later
- Rust toolchain (for building from source)

## Build and run

```powershell
cargo run --release
```

The release executable is created at `target/release/copyshot.exe`. Debug builds skip the update check.

## Build the installer

Install [Inno Setup 6](https://jrsoftware.org/isinfo.php), then run:

```powershell
.\scripts\build-installer.ps1
```

The installer is written to `dist\Copyshot-Setup-0.2.0.exe`.

## Release

Bump the version in `Cargo.toml`, `installer\copyshot.iss` and this README, build the installer, and publish a GitHub release tagged `vX.Y.Z` with the installer attached. Running copies offer the update within 12 hours.

## Tests

```powershell
cargo test
```

## Notes

The app enables its Windows startup entry the first time it runs. If you turn this off from the tray menu, it stays off. Settings live in `%LOCALAPPDATA%\Copyshot`.
