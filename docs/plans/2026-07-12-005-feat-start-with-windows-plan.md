---
title: feat: Start with Windows 11 via registry Run key
type: feat
status: active
date: 2026-07-12
---

# feat: Start with Windows 11 Reliably

## Summary

Add a `start_with_windows` config option that toggles a `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` registry entry. When enabled, the daemon auto-starts at login. Synced on daemon startup and config reload.

## Requirements

- R1. `start_with_windows = true` in config writes the binary path to the registry Run key on daemon start.
- R2. `start_with_windows = false` removes the registry entry.
- R3. Config hot-reload syncs the registry state (adds or removes the key as needed).
- R4. No additional dependencies — use existing `windows` crate Win32 API calls.
- R5. Existing tests continue to pass.

## Scope Boundaries

- Excluded: Tray menu toggle UI (config file only for now).
- Excluded: Windows Task Scheduler or Service approaches (Registry Run key is the standard for per-user tray apps).
- Excluded: Installer (the binary path is `std::env::current_exe()` which works from any stable location).

## Implementation Units

### U1. Add `start_with_windows` to config Settings

**Files:** Modify: `src/config.rs`

**Approach:** Add `#[serde(default)] pub start_with_windows: bool` to Settings struct (defaults to false).

### U2. Create `autostart` module with registry sync

**Files:** Create: `src/autostart.rs`, Modify: `src/lib.rs`

**Approach:**
- `pub fn set(enabled: bool)` — writes or deletes `MouseGestureDaemon` value under the Run key using `RegOpenKeyExW`/`RegSetValueExW`/`RegDeleteValueW`/`RegCloseKey`
- `pub fn sync(enabled: bool)` — idempotent: calls `set(enabled)`
- Use `std::env::current_exe()` for the binary path (reliable, no argv[0] footguns)
- Log at info level when toggling

### U3. Wire autostart sync on daemon startup and config reload

**Files:** Modify: `src/main.rs`

**Approach:**
- After loading startup config: call `autostart::sync(config.settings.start_with_windows)`
- In `watch_config()` after successful reload: call `autostart::sync(snapshot.settings.start_with_windows)`
- Note: `ConfigSnapshot` needs to carry settings — currently it carries compiled gesture data but not raw settings. Solution: add `settings: Settings` to `ConfigSnapshot`, or read `start_with_windows` from the raw `ConfigFile` separately in the watcher.

## Verification

- `cargo build --release` + `cargo test`
- Manual: set `start_with_windows = true`, restart daemon, verify registry key exists. Set to false, restart, verify key removed.
- Windows sign-out/sign-in: daemon starts automatically when key is present.
