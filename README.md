# WinKeeper

WinKeeper is a lightweight cross-platform desktop process supervisor for managing persistent command-line tools. It provides tray management, lifecycle control, automatic restart, stdout/stderr collection, persistent logs, process-tree cleanup, and on-demand memory sampling.

## Platforms

Release artifacts are produced for:

| Platform | Architecture | Process tree | Memory source | Autostart |
| --- | --- | --- | --- | --- |
| Windows 10/11 | x86_64, ARM64 | Job Object | Working Set | `HKCU\\...\\Run` |
| Linux Desktop | x86_64, ARM64 | Process Group | `/proc/<pid>/status` VmRSS | XDG autostart |

Linux targets Debian/Ubuntu desktop environments, KDE Plasma and GNOME, on X11 or Wayland. macOS is not supported.

## Features

- Shared Slint manager UI and system tray on Windows and Linux
- Configuration-driven direct process execution without a shell
- Start, stop, restart, and batch actions
- stdout and stderr capture with timestamped per-tool logs
- In-memory live log buffer
- Automatic restart with delay and restart-window limiting
- Windows Job Objects and Linux process groups for descendant cleanup
- One-shot asynchronous memory sampling when the manager opens or Memory is clicked
- Native per-user autostart integration
- Hidden-to-tray startup and close-to-tray behavior

The first-stage MVP intentionally rejects `admin = true`. Windows UAC/elevated helper and Linux Polkit integration are planned separately so WinKeeper never silently runs an administrator-designated tool without elevation.

## Configuration

The default configuration paths are:

| Platform | Configuration | Logs |
| --- | --- | --- |
| Windows | `%APPDATA%\\WinKeeper\\config.toml` | `%APPDATA%\\WinKeeper\\logs` |
| Linux | `~/.config/winkeeper/config.toml` | `~/.local/state/winkeeper/logs` |

WinKeeper creates an empty valid configuration on first launch. Use [config.example.toml](config.example.toml) as a reference. The same TOML schema is used on both platforms.

```toml
[manager]
start_with_system = true
minimize_to_tray = true
log_buffer_lines = 10000
stop_timeout_ms = 5000

[[tools]]
name = "worker"
command = "C:\\Tools\\worker.exe"
args = ["--config", "worker.toml"]
workdir = "C:\\Tools"
admin = false
auto_start = true
auto_restart = true
restart_delay_ms = 3000
max_restart_count = 5
restart_window_seconds = 60
```

After changing `config.toml`, exit and restart WinKeeper. Settings opens the active configuration in the operating system's associated editor.

## Command line

```text
win-keeper [--config PATH] [--check-config] [--show]
```

`--show` opens the manager immediately. Without it, WinKeeper starts in the tray when `minimize_to_tray` is enabled.

## Build

Rust 1.92 or newer is required by Slint 1.17.

```bash
cargo build --release --locked
```

Cross-compile Windows x86_64 from Linux with MinGW:

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --locked --target x86_64-pc-windows-gnu
```

CI uses native GitHub runners for Windows x86_64/ARM64 and Linux x86_64/ARM64. Tagged builds package a single executable together with this README and the example configuration.

## Architecture

```text
src/core       platform-neutral config, logs, state machine, supervisor
src/platform   Windows Job/registry/memory and Linux process-group/XDG/proc adapters
ui             shared Slint manager and SystemTrayIcon
```

Core never calls Win32, Unix signals, or `/proc` directly. Platform differences implement shared lifecycle and memory semantics without leaking operating-system details into the UI.

## License

Apache-2.0. Slint is used under its applicable royalty-free/open-source licensing terms.

