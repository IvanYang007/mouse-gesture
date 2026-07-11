# Mouse Gesture Daemon

A fast, lightweight Windows mouse gesture daemon written in Rust. Hold right-click and draw to trigger window management, keyboard shortcuts, or app launches.

## Features

- **8-direction gesture recognition** — draw shapes with right-click, matched via exact pattern matching
- **Sub-5ms classification** — p99.9 hook-to-classification under 5ms
- **<5MiB memory** — private working set under 5MiB in idle and active states
- **Per-Monitor V2 DPI** — correct behavior across mixed-resolution multi-monitor setups
- **TOML configuration** — hot-reloadable config via file watching
- **System tray** — persistent tray icon with status, reload, and quit

## Quick Start

```powershell
# Build
.\build.ps1

# Run
.\target\release\mouse-gesture.exe

# Config lives at
# %APPDATA%\mouse-gesture\config.toml
```

## Requirements

- Windows 10 or 11 (x64)
- Rust stable 1.80+

## Project Structure

```
src/
  main.rs          — entry point, thread spawn, message pump dispatch
  config.rs        — TOML schema, parse, validate, compile to snapshot
  gesture.rs       — RDP simplification, 8-direction encode, exact matcher
  input_hook.rs    — WH_MOUSE_LL thread, hook proc, gesture buffer
  state_machine.rs — Idle/NativePass/Armed/Drawing state transitions
  app_policy.rs    — PID→policy cache, integrity check, app matching
  window_ops.rs    — monitor enum, tile/snap/min/max/close, DPI math
  input_inject.rs  — SendInput keyboard/mouse, modifier tracking
  launch.rs        — CreateProcessW, ShellExecuteExW, focus-existing
  tray.rs          — Shell_NotifyIconW, menu, TaskbarCreated
  lifecycle.rs     — singleton mutex, WM_ENDSESSION, lock/unlock, logs
  win_handles.rs   — unsafe Send/Sync wrappers for HHOOK, HANDLE
tests/
  gesture_tests.rs
  config_tests.rs
  integration_tests.rs
```

## License

MIT
