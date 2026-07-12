# Mouse Gesture Daemon

A fast Windows mouse gesture daemon for window management, keyboard shortcuts, and app launching.

## Installation

```powershell
git clone https://github.com/IvanYang007/mouse-gesture.git
cd mouse-gesture
cargo build --release
```

The binary is at `target/release/mouse-gesture.exe`.

Requires Rust 1.80+ and Windows 10/11 64-bit.

## Getting Started

Run the daemon:

```powershell
.\target\release\mouse-gesture.exe
```

A tray icon appears. Right-click it to open the config file.

A default config is created at `%APPDATA%\mouse-gesture\config.toml`.
Edit it to add gestures. The daemon hot-reloads changes every 2 seconds.

To stop the daemon:

```powershell
taskkill /f /im mouse-gesture.exe
```

Use `restart.bat` to kill and restart in one step.

To start automatically with Windows:

```toml
[settings]
start_with_windows = true
```

## Configuration

### Settings

```toml
[settings]
activation_threshold_dip = 3.0     # pixels before activation (default 3)
sample_distance_dip = 2.0          # min distance between sampled points
rdp_epsilon_dip = 2.0              # RDP simplification tolerance
min_gesture_length = 2             # min direction tokens after collapse
debug_logging = false              # set true for verbose daemon.log
start_with_windows = false         # auto-start via registry
```

| Setting | Default | Purpose |
|---------|---------|---------|
| `activation_threshold_dip` | 3.0 | Pixels of cursor movement before a gesture activates |
| `sample_distance_dip` | 2.0 | Minimum pixels between stored points — thins raw mouse data |
| `rdp_epsilon_dip` | 2.0 | Maximum deviation from a straight line before RDP keeps a point as a corner |
| `min_gesture_length` | 2 | Minimum direction tokens after collapse — 1 allows single strokes, 2 requires a turn |
| `debug_logging` | false | Writes verbose output to `daemon.log` when true |
| `start_with_windows` | false | Registers the daemon in `HKCU\...\Run` for login auto-start |

The recognition pipeline processes raw cursor points in order:

```
activation threshold  →  gates whether gesture starts
sample distance       →  thins raw mouse-move events
RDP epsilon           →  removes wobble, keeps corners
direction quantize    →  converts points to 8-way (U/D/L/R/UR/DR/DL/UL)
collapse              →  merges consecutive duplicates: [E,E,S,S] → [E,S]
min gesture length    →  rejects gestures with too few tokens
exact match           →  compares against configured patterns
```

### Blacklist

```toml
[blacklist]
mode = "blacklist"
apps = ["chrome.exe", "firefox.exe"]
```

Use `mode = "whitelist"` to only intercept on listed apps.

### Gestures

Each gesture has a name, a draw pattern, and an action.
Direction tokens: `U`, `D`, `L`, `R`, `UR`, `DR`, `DL`, `UL`.

```toml
[gestures.maximize]
pattern = "U"
action = { type = "window", command = "maximize" }

[gestures.restore]
pattern = "D"
action = { type = "window", command = "restore" }

[gestures.close_window]
pattern = "L D"
action = { type = "window", command = "close" }

[gestures.go_back]
pattern = "L"
action = { type = "key", combo = ["Alt", "Left"] }

[gestures.open_explorer]
pattern = "U R"
action = { type = "launch", path = "explorer.exe" }
```

### Launch action args

```toml
[gestures.search]
pattern = "D R"
action = { type = "launch", path = "https://google.com" }
```

URLs are opened via `ShellExecuteW`. Executables use `CreateProcessW`.

## Reference

### Window commands

`maximize`, `minimize`, `restore`, `close`, `snap-left`, `snap-right`,
`snap-top`, `snap-bottom`, `snap-top-left`, `snap-top-right`,
`snap-bottom-left`, `snap-bottom-right`, `center`,
`move-to-monitor-1`, `move-to-monitor-2`, `toggle-always-on-top`

Window commands target the window below the cursor at gesture start.

### Key names

`ctrl`, `alt`, `shift`, `win`, `enter`, `escape`, `tab`, `space`,
`backspace`, `delete`, `insert`, `left`, `right`, `up`, `down`,
`home`, `end`, `pgup`, `pgdn`, `F1`-`F24`, `A`-`Z`, `0`-`9`

Key combos focus the target window before injection.

### Direction tokens

| Token | Meaning |
|-------|---------|
| `U`   | Up      |
| `D`   | Down    |
| `L`   | Left    |
| `R`   | Right   |
| `UR`  | Up-Right |
| `DR`  | Down-Right |
| `DL`  | Down-Left |
| `UL`  | Up-Left |

Also accepts compass: `N`, `S`, `W`, `E`, `NE`, `SE`, `SW`, `NW`.

### Recognition pipeline

```
raw points → spatial coalesce → RDP simplify
  → 8-direction quantize → collapse → exact match
```

Direction indicators appear near the cursor while drawing.

### Limits

| Limit | Value |
|-------|-------|
| Max gestures | 256 |
| Max tokens per gesture | 32 |
| Max action queue depth | 16 |
| Config file size | 1 MiB |
| Point buffer (with adaptive decimation) | 256 |
| Log file (auto-truncates) | 1 MiB |

## Troubleshooting

**Daemon does not respond to gestures:**
- Ensure no other app consumes right-click (game overlays, remote desktop)
- Set `debug_logging = true`, restart, and check `daemon.log`
- Verify gestures exist in `config.toml` and patterns are valid

**Tray icon missing:**
- Place a custom `.ico` at `%APPDATA%\mouse-gesture\icon.ico`
- Check the Windows tray overflow area (^)

**Auto-start not working:**
- Verify `start_with_windows = true` in config
- Check `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\MouseGestureDaemon`

**Another instance running:**
- The daemon uses a singleton mutex — kill the existing process first

## History

View the [changelog](https://github.com/IvanYang007/mouse-gesture/releases).

## Contributing

```powershell
git clone https://github.com/IvanYang007/mouse-gesture.git
cd mouse-gesture
cargo build
cargo test
```

Logs write to `%APPDATA%\mouse-gesture\daemon.log`.

66 tests cover gesture recognition, config parsing, and policy lookups.

## License

MIT
