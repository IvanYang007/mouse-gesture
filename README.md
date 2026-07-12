# Mouse Gesture Daemon

A fast, lightweight Windows mouse gesture daemon written in Rust. Hold right-click
and draw to trigger window management, keyboard shortcuts, or app launches.

## Prerequisites

- **Windows 10 or 11** (64-bit)
- **Rust** 1.80+ — [install via rustup](https://rustup.rs)

## Installation

Clone and build:

```powershell
git clone https://github.com/IvanYang007/mouse-gesture.git
cd mouse-gesture
cargo build --release
```

The binary is at `target/release/mouse-gesture.exe`.

## Quick Start

Run the daemon:

```powershell
.\target\release\mouse-gesture.exe
```

A tray icon appears. The daemon creates a default config at
`%APPDATA%\mouse-gesture\config.toml`. Edit it to add gestures.

To start automatically with Windows, add to your config:

```toml
[settings]
start_with_windows = true
```

To stop, right-click the tray icon and choose exit, or run:

```powershell
taskkill /f /im mouse-gesture.exe
```

Use `restart.bat` to kill and restart in one step.

## Configuration

Config lives at `%APPDATA%\mouse-gesture\config.toml`. It hot-reloads every 2
seconds — no restart needed.

### Settings

```toml
[settings]
activation_threshold_dip = 50.0    # pixels before gesture activates (default 70)
sample_distance_dip = 2.0          # min distance between sampled points
rdp_epsilon_dip = 2.0              # RDP simplification tolerance
min_gesture_length = 1             # min direction tokens after collapse
debug_logging = false              # set true for verbose daemon.log
start_with_windows = false         # auto-start at login via registry
```

### Blacklist

Exclude apps from gesture interception:

```toml
[blacklist]
mode = "blacklist"
apps = ["chrome.exe", "firefox.exe"]
```

Use `mode = "whitelist"` to only intercept on listed apps.

### Gestures

Each gesture has a name, a draw pattern, and an action. Pattern directions use
`U`, `D`, `L`, `R` for single strokes and `UR`, `DR`, `DL`, `UL` for diagonals.

```toml
# Window commands
[gestures.maximize]
pattern = "U"
action = { type = "window", command = "maximize" }

[gestures.restore]
pattern = "D"
action = { type = "window", command = "restore" }

[gestures.close_window]
pattern = "L D"
action = { type = "window", command = "close" }

# Keyboard shortcuts
[gestures.go_back]
pattern = "L"
action = { type = "key", combo = ["Alt", "Left"] }

# Launch programs
[gestures.open_explorer]
pattern = "U R"
action = { type = "launch", path = "explorer.exe" }
```

**Window commands:** `maximize`, `minimize`, `restore`, `close`,
`snap-left`, `snap-right`, `snap-top`, `snap-bottom`,
`snap-top-left`, `snap-top-right`, `snap-bottom-left`, `snap-bottom-right`,
`center`, `move-to-monitor-1`, `move-to-monitor-2`, `toggle-always-on-top`.

**Key names:** `ctrl`, `alt`, `win`, `shift`, `enter`, `escape`, `tab`,
`space`, `backspace`, `delete`, `left`, `right`, `up`, `down`, `home`, `end`,
`pgup`, `pgdn`, `insert`, `F1`-`F24`, and letters `A`-`Z`.

## How It Works

Hold right-click and draw a shape. The daemon tracks cursor movement and
classifies the direction sequence:

```
Raw points → spatial coalescing → RDP simplification
    → 8-direction quantization → collapse duplicates → exact match
```

Direction indicators (U, D, L, R) appear near the cursor while drawing. Release
to trigger the matched action.

Gesture actions target the window you right-clicked on — not the previously
focused window. Window commands work on unfocused windows; keyboard shortcuts
focus the target window first.

## Architecture

Six threads communicate via MPSC channels:

| Thread | Purpose |
|--------|---------|
| **Hook** | `WH_MOUSE_LL` callback, state machine, gesture buffer |
| **Recognition** | RDP, quantization, pattern matching (off-hook) |
| **Replay** | Synthetic right-click injection with absolute coordinates |
| **Policy** | Async PID-to-eligibility resolution |
| **UI** | Notification window, tray icon, overlay, message pump |
| **Worker** | Action dispatch — `CreateProcessW`, `SendInput`, `ShowWindowAsync` |

## Troubleshooting

**Daemon doesn't respond to gestures:**
- Ensure no other app is consuming right-click (game overlays, remote desktop)
- Check `debug_logging = true` in config, restart, and inspect
  `%APPDATA%\mouse-gesture\daemon.log`
- Verify gestures exist in `config.toml` and patterns match

**Tray icon missing:**
- Place a custom `.ico` at `%APPDATA%\mouse-gesture\icon.ico`
- Windows may hide tray icons — check the overflow area (^)

**Auto-start not working:**
- Verify `start_with_windows = true` in config
- Check `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\MouseGestureDaemon`
  in Registry Editor

**Another instance is already running:**
- The daemon uses a singleton mutex. Kill the existing process first.

## Development

```powershell
# Build
cargo build --release

# Run tests (66 tests)
cargo test

# Deploy
.\restart.bat
```

Logs write to `%APPDATA%\mouse-gesture\daemon.log`. Enable with
`debug_logging = true`.

## License

MIT
