# Copyshot

Copyshot is a lightweight native Windows screenshot utility written in Rust. It runs in the system tray and saves captures to `Pictures/Screenshots` while also copying them to the clipboard.

## Features

- Capture a selected screen region with `Print Screen`
- Capture the full virtual desktop with `Shift` + `Print Screen`
- Supports multi-monitor setups
- Saves PNG files and copies each capture to the clipboard
- Tray menu for capture controls, screenshot-folder access, and startup settings

## Requirements

- Windows or Linux with an XDG desktop portal backend
- Rust toolchain (for building from source)

## Build and run

```powershell
cargo run --release
```

The release executable is created at `target/release/copyshot.exe`.

### Linux (Fedora, X11, and Wayland)

Copyshot uses the XDG Screenshot Portal on Linux. This gives Wayland compositors control over the capture permission and selection UI, and works wherever a compatible `xdg-desktop-portal` backend is installed (including Fedora GNOME).

```bash
cargo run --release
```

The portal asks you to choose or confirm the capture, then Copyshot saves the resulting PNG in `~/Pictures/Screenshots`. Linux global shortcuts and tray integration will be added separately because Wayland requires compositor-approved portal support for those features.

## Tests

```powershell
cargo test
```

## Notes

The app enables its Windows startup entry when it runs. You can turn this off from the tray menu.
