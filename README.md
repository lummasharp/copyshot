# Copyshot

Copyshot is a lightweight native Windows screenshot utility written in Rust. It runs in the system tray and saves captures to `Pictures/Screenshots` while also copying them to the clipboard.

## Features

- Capture a selected screen region with `Print Screen`
- Capture the full virtual desktop with `Shift` + `Print Screen`
- Supports multi-monitor setups at any resolution and display scaling
- Saves PNG files and copies each capture to the clipboard
- Tray menu for capture controls, screenshot-folder access, and startup settings

## Requirements

- Windows 10 (1703) or later
- Rust toolchain (for building from source)

## Build and run

```powershell
cargo run --release
```

The release executable is created at `target/release/copyshot.exe`.

## Tests

```powershell
cargo test
```

## Notes

The app enables its Windows startup entry the first time it runs. If you turn this off from the tray menu, it stays off.
