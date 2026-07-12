---
title: feat: Add tray context menu, native config editor, and centralized config reload
type: feat
status: active
date: 2026-07-12
---

# feat: Add Tray Context Menu, Native Config Editor, and Centralized Config Reload

## Summary

Add a Windows tray right-click context menu (Configure, Reload Config, Open Folder, Exit), a native Win32 configuration editor with validation and atomic save, centralized configuration reload through a dedicated window message that fixes the stale `DaemonState.config` bug, and Explorer restart tray re-registration via `TaskbarCreated`.

---

## Problem Frame

The daemon currently has no tray context menu — right-clicking the icon directly opens `config.toml` in an external editor. Users must manually kill the process via Task Manager to exit, and configuration changes require either a manual restart or waiting up to 2 seconds for the file-watcher poll. The file watcher has a latent bug: it sends the updated snapshot to the hook thread but never updates `DaemonState.config`, so gesture action dispatch (`dispatch_hook_event`) continues using stale mappings after a hot reload. Replacing the watcher's in-thread config application with a centralized message-driven reload on the UI thread fixes this and ensures the hook, daemon state, application policy, autostart, and tray status all stay in sync.

---

## Requirements

- R1. Right-clicking the tray icon displays a context menu at the cursor with four items: Configure..., Reload configuration, Open configuration folder, and Exit.
- R2. Clicking elsewhere dismisses the menu (standard Windows behavior).
- R3. Configure... opens a native Win32 editor window containing the current `config.toml` content.
- R4. Only one editor window can exist at a time — opening again brings the existing window to the foreground.
- R5. Saving invalid TOML or a config that fails `compile(1, 96)` is rejected with an error dialog and does not overwrite the valid file.
- R6. A valid save writes the file atomically (temp file + rename) and immediately reloads the daemon's configuration.
- R7. After reload, `DaemonState.config`, the hook, application policy, autostart, and tray tooltip all use the new snapshot.
- R8. Reload configuration manually reloads from disk and applies the same centralized path.
- R9. Open configuration folder opens `%APPDATA%\mouse-gesture` in Explorer.
- R10. Exit shuts down the daemon through the existing cleanup path (`WM_CLOSE` → `PostQuitMessage`).
- R11. Closing the editor window does not terminate the daemon.
- R12. No `DaemonState` mutex is held while `TrackPopupMenuEx` displays the menu (prevents modal-loop deadlock).
- R13. After Explorer/taskbar restart, the tray icon is re-registered via `TaskbarCreated`.
- R14. `cargo fmt`, `cargo check`, and `cargo clippy` pass without warnings.

---

## Scope Boundaries

- Excluded: Changing gesture recognition, overlay rendering, or hook thread architecture beyond config reload.
- Excluded: Replacing the tray icon with a different visual style.
- Excluded: Adding a GUI framework dependency (egui, slint, etc.) — the editor uses raw Win32.
- Excluded: Fixing the DPI and generation counter on config reload — the watcher currently uses hardcoded `compile(1, 96)`; this is a separate concern from the stale-state fix and should be addressed in a follow-up.
- Excluded: Unit tests for Win32 GUI code — the existing test infrastructure is focused on pure-logic modules (config parsing, gesture recognition, state machine). GUI testing would require a UI automation framework beyond scope.

---

## Context & Research

### Relevant Code and Patterns

- **`src/tray.rs`** — `TrayIcon` struct with `new()`, `update_status()`, `remove()`, `reregister()`, `callback_msg()`. Uses `Shell_NotifyIconW` with `NIM_ADD`/`NIM_MODIFY`/`NIM_DELETE`. `callback_msg` is `WM_APP + 1`.
- **`src/main.rs`** — `DaemonState` holds `config: Option<ConfigSnapshot>`, tray, hook controller, worker channel, overlay. `window_proc` handles `WM_DESTROY`, `WM_CLOSE`, `WM_QUERYENDSESSION`, `WM_WTSSESSION_CHANGE`, and tray callbacks. `dispatch_hook_event()` reads `DaemonState.config` for action lookup. `watch_config()` polls file and applies config in-thread (does NOT update `DaemonState.config`).
- **`src/config.rs`** — `ConfigFile::load(path)` reads TOML from disk, `ConfigFile::compile(generation, dpi)` produces `ConfigSnapshot`. Uses `serde(deny_unknown_fields)`.
- **`src/overlay.rs`** — Established pattern for a separate window class with its own `window_proc`, `RegisterClassExW`, `CreateWindowExW`, `GWLP_USERDATA` for state storage, `DestroyWindow` in cleanup. The overlay proc does NOT call `PostQuitMessage`.
- **`src/launch.rs`** — `shell_open(path)` wraps `ShellExecuteW`. `focus_existing(hwnd)` restores + foregrounds + flashes.
- **`src/app_policy.rs`** — `AtomicPtr<PolicySnapshot>` lock-free snapshot pattern. `publish_snapshot()` atomically swaps and intentionally leaks old allocation.
- **`src/autostart.rs`** — `sync(enabled)` toggles registry Run key via `reg` CLI.
- **`src/lifecycle.rs`** — `WM_WTSSESSION_CHANGE = 0x02B1`, session lock/unlock constants, `register_session_notifications()`.

### Institutional Learnings

No `docs/solutions/` directory exists — zero captured learnings. Key informal knowledge from plan documents:

- **Plan 003 (P0 hook architecture)**: `GetMessageW` does not wake for `mpsc` channel messages. Modal message loops (like `TrackPopupMenuEx`'s internal pump) re-enter `window_proc`. Holding `Arc<Mutex<DaemonState>>` across a modal loop risks deadlock if the re-entrant `window_proc` tries to lock the same mutex.
- **Plan 005 (start-with-Windows)**: `ConfigSnapshot.start_with_windows` already exists and `autostart::sync()` is already called on startup and in the watcher. The centralized reload must preserve this.

### External References

None — the task specification provides exact Win32 API calls and the codebase has strong adjacent patterns. No external research needed.

---

## Key Technical Decisions

- **Centralized reload via `WM_APP_RELOAD_CONFIG`**: Rather than having the watcher thread compile and apply config (which caused the stale-state bug), the watcher only detects file modification and posts a window message. The UI thread's `reload_config()` is the single place that updates `DaemonState.config`, the hook, application policy, autostart, and tray status — preventing version skew.

- **Separate editor window class**: The editor must not reuse the daemon's `window_proc` because its `WM_DESTROY` handler calls `PostQuitMessage(0)`, terminating the entire daemon. A dedicated window class (`"MouseGestureEditor"`) with its own procedure avoids this.

- **Single-editor enforcement via atomic handle**: An `AtomicIsize` stores the editor HWND. Before creating a new editor, `open()` checks whether the stored handle is still a valid window (`IsWindow`) and focuses it instead. The handle is cleared on `WM_NCDESTROY`.

- **Atomic config save**: Write to a `.tmp` file in the same directory, then `rename` to the target path. This prevents partial writes from corrupting the config file. Optionally preserve a `.bak` copy.

- **Validation uses the real compiler**: Editor save validates through `ConfigFile::parse(text)?` + `compile(1, 96)?` — the same pipeline as startup. Parsing as bare `toml::Value` would miss gesture-specific constraints (duplicate patterns, token limits, invalid directions).

- **Lock-release-before-menu pattern**: In `window_proc`, determine whether the message is the tray callback while briefly holding the mutex, then release before calling `show_context_menu()`. This prevents the modal menu loop from re-entering `window_proc` and deadlocking on the same mutex.

---

## Implementation Units

### U1. Tray context menu infrastructure

**Goal:** Add command enum, menu IDs, and `show_context_menu()` to `tray.rs`. Replace the hard-coded right-click behavior in `window_proc` with symbolic constants and the new menu.

**Requirements:** R1, R2, R12

**Dependencies:** None

**Files:**
- Modify: `src/tray.rs`
- Modify: `src/main.rs`

**Approach:**
1. Add `TrayCommand` enum and command ID constants (`IDM_CONFIGURE = 1001`, etc.) to `tray.rs`.
2. Implement `pub fn show_context_menu(hwnd: HWND) -> Result<Option<TrayCommand>>` using `CreatePopupMenu`, `AppendMenuW`, `GetCursorPos`, `SetForegroundWindow`, `TrackPopupMenuEx` (with `TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY`), `PostMessageW(hwnd, WM_NULL, ...)`, and `DestroyMenu` in all paths.
3. In `main.rs::window_proc`, replace `0x0205` with `WM_RBUTTONUP` and `WM_CONTEXTMENU` symbolic constants. Use `lparam.0 as u32 & 0xffff` to extract the tray event. Match on `TrayCommand` variants with `match` — action wiring deferred to U4.

**Patterns to follow:**
- `tray.rs` existing `notify_result()` helper for `BOOL → Result`
- `launch.rs` `shell_open()` string encoding pattern for menu strings
- `overlay.rs` `RegisterClassExW` pattern for Win32 API usage style

**Test scenarios:**
- Happy path: Right-click tray icon → menu appears at cursor position. Select "Exit" → command returned as `Some(TrayCommand::Exit)`. Click elsewhere → menu dismissed, `Ok(None)` returned.
- Edge case: `CreatePopupMenu` or `AppendMenuW` fails → `Err` returned, menu destroyed if partially created.
- Edge case: `TrackPopupMenuEx` returns 0 (no selection) → `Ok(None)`, menu destroyed.
- Integration: `SetForegroundWindow(hwnd)` called before `TrackPopupMenuEx`. `PostMessageW(hwnd, WM_NULL, ...)` called after menu dismisses.

**Verification:**
- `cargo check` on `tray.rs` additions — no new `unsafe` warnings beyond existing patterns.
- Manual: right-click tray icon, menu appears. Verify all four items render. Verify menu dismisses on click-away.

---

### U2. Centralized config reload and stale-state fix

**Goal:** Add `WM_APP_RELOAD_CONFIG` message, implement `reload_config()` on the UI thread, and change `watch_config()` to only post the message instead of applying config in-thread. Fixes the stale `DaemonState.config` bug.

**Requirements:** R7, R8, R14

**Dependencies:** U1 (reload_config updates the tray tooltip, which becomes user-visible through U1's context menu entries)

**Files:**
- Modify: `src/main.rs`

**Approach:**
1. Add `const WM_APP_RELOAD_CONFIG: u32 = WM_APP + 20;` in `main.rs`.
2. Implement `fn reload_config(hwnd: HWND)` that reads state via `GWLP_USERDATA`, loads `ConfigFile::load(&config_path())`, compiles with `compile(1, 96)`, and on success: publishes `PolicySnapshot`, syncs autostart, sends `HookCommand::UpdateConfig` + `SetInterception(true)`, sets `DaemonState.config = Some(snapshot)`, and updates tray to `TrayState::Active { gesture_count }`. On failure: logs error, does not replace valid existing config, sets tray to `TrayState::Error { message }`, keeps interception disabled if no prior valid config existed.
3. Handle `WM_APP_RELOAD_CONFIG` in `window_proc`.
4. Rewrite `watch_config()` to accept `hwnd` as `isize`, poll for modification time changes, and post `WM_APP_RELOAD_CONFIG` via `PostMessageW`. Remove all `ConfigFile::load`/`compile`/`HookCommand`/`PolicySnapshot`/`autostart` calls from the watcher.
5. Update `main()` to pass `hwnd.0 as isize` to the watcher thread.

**Patterns to follow:**
- `window_proc`'s existing state access pattern: `GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>`
- `load_startup_config()` for the config loading + error handling pattern
- Existing `HookController::send()` calls in `run()` for command dispatch pattern

**Test scenarios:**
- Happy path: `WM_APP_RELOAD_CONFIG` posted → `reload_config()` runs → `DaemonState.config` updated, hook receives `UpdateConfig`, tray tooltip reflects new gesture count, autostart synced, policy snapshot published.
- Error path: Config file contains invalid TOML → error logged, existing valid `DaemonState.config` preserved, tray shows error status.
- Error path: Config file missing → error logged, existing valid config preserved if one was loaded.
- Error path: No prior valid config and reload fails → interception remains disabled.
- Integration: Watcher detects file change → posts `WM_APP_RELOAD_CONFIG` → only the UI thread compiles and applies config.
- Integration: `dispatch_hook_event()` after reload uses the new snapshot (verified by gesture action executing the updated mapping).

**Verification:**
- `cargo check` — `WM_APP + 20` does not conflict with `WM_APP + 1` (tray callback).
- Manual: Edit `config.toml`, wait 2 seconds, verify new gesture works. Verify log shows reload. Change tray tooltip reflects new count.
- Manual: Introduce syntax error in config, verify daemon continues with previous config, tray shows error status.

---

### U3. Native config editor window

**Goal:** Create `src/config_editor.rs` with a native Win32 editor window using EDIT and BUTTON child controls. Includes single-editor enforcement, atomic save with validation, and proper cleanup.

**Requirements:** R3, R4, R5, R6, R11

**Dependencies:** U2 (posts `WM_APP_RELOAD_CONFIG` after save)

**Files:**
- Create: `src/config_editor.rs`
- Modify: `src/lib.rs` (add `pub mod config_editor;`)
- Modify: `src/config.rs` (add `ConfigFile::parse`)

**Approach:**
1. Add `ConfigFile::parse(text: &str) -> Result<Self>` to `config.rs` — a `toml::from_str` wrapper using the same serde configuration as `ConfigFile::load`.
2. Register a dedicated window class `"MouseGestureEditor"` with its own `editor_proc`. The proc handles `WM_SIZE` (resize/reposition EDIT and buttons), `WM_COMMAND` (Save/Cancel button clicks), `WM_CLOSE` (calls `DestroyWindow`), and `WM_NCDESTROY` (releases boxed `EditorState` and clears the global editor handle). Crucially, `WM_DESTROY` does NOT call `PostQuitMessage`.
3. `EditorState` stored via `GWLP_USERDATA`: `owner: HWND`, `path: PathBuf`, `edit: HWND`, `save_button: HWND`, `cancel_button: HWND`.
4. `pub fn open(owner: HWND, path: PathBuf) -> Result<()>`:
   - Checks global `AtomicIsize` for existing editor HWND — if `IsWindow` confirms it's still valid, calls `SetForegroundWindow` + returns.
   - Reads config file as raw UTF-8 via `std::fs::read_to_string(&path)` (not `ConfigFile::load`, which would reject invalid TOML and prevent the editor from opening for recovery editing). If the file is missing, the editor opens with an empty text area.
   - Converts text to UTF-16, calls `SetWindowTextW` on the EDIT control.
   - Creates editor window (800×600, title "Mouse Gesture Configuration"), centered on the primary monitor using `GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)`.
   - Creates EDIT child (`WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL | ES_LEFT | ES_MULTILINE | ES_AUTOVSCROLL | ES_AUTOHSCROLL | ES_WANTRETURN | WS_EX_CLIENTEDGE`). Note: Tab key will navigate to the Save button rather than inserting a tab character — this is accepted for the initial implementation; TOML does not require tab indentation.
   - Creates Save and Cancel BUTTON children.
   - Shows window with `ShowWindow(SW_SHOW)`.
   - Stores the editor HWND in the global `AtomicIsize`.
5. Save behavior: reads EDIT text via `GetWindowTextLengthW` + `GetWindowTextW`, converts UTF-16 → `String`, validates with `ConfigFile::parse` + `compile(1, 96)`. On failure: `MessageBoxW` with error, editor stays open. On success: ensures parent directory exists (`create_dir_all`), writes to `.tmp` in the same directory, renames to target path (atomic on NTFS), optionally preserves `.bak`, posts `WM_APP_RELOAD_CONFIG` to owner.

**Patterns to follow:**
- `overlay.rs` for window class registration, `CreateWindowExW`, `GWLP_USERDATA` pattern
- `launch.rs` for `SetForegroundWindow` usage
- `config.rs` `ConfigFile::load` for error context patterns with `anyhow::Context`
- Existing `unsafe { ... }` block style for all Win32 API calls

**Test scenarios:**
- Happy path: `open()` with valid config path → editor window appears with TOML content. Edit content, click Save → validation passes → file written → `WM_APP_RELOAD_CONFIG` posted to owner.
- Happy path: Click Cancel → window closes, no file written, daemon unaffected.
- Happy path: `open()` called while editor already open → existing window brought to foreground, no second window created.
- Error path: Invalid TOML in editor → Save clicked → `MessageBoxW` with parse error, editor stays open, original file intact.
- Error path: Valid TOML but fails `compile()` (e.g., duplicate patterns, too many tokens) → `MessageBoxW` with compile error, editor stays open.
- Error path: Config file missing or unreadable → editor opens empty, user can compose new config.
- Edge case: Editor window closed via X button → `WM_CLOSE` → `DestroyWindow` → `WM_NCDESTROY` releases state, clears global handle. Daemon continues running.
- Edge case: Save destination directory does not exist → `create_dir_all` before write.

**Verification:**
- `cargo check --lib` on `config_editor.rs` — no `unused import` or missing feature warnings.
- Manual: Open editor from tray, edit a gesture, save. Verify gesture works immediately (proves `WM_APP_RELOAD_CONFIG` was posted and processed).
- Manual: Open editor, introduce `pattern = "N X"`, save. Verify `MessageBoxW` appears, file unchanged, gesture still works.
- Manual: Open editor, close via X. Verify daemon still running (check tray icon + gestures).
- Manual: Open editor, then open again from tray. Verify only one window exists.

---

### U4. Wire tray commands to actions

**Goal:** Connect each `TrayCommand` from the context menu to its implementation: Configure opens the editor, Reload posts `WM_APP_RELOAD_CONFIG`, Open Folder calls `shell_open` on the config directory, Exit posts `WM_CLOSE`.

**Requirements:** R3, R8, R9, R10

**Dependencies:** U1 (menu infrastructure), U2 (reload message), U3 (editor)

**Files:**
- Modify: `src/main.rs`

**Approach:**
1. In `window_proc`, match on the `show_context_menu()` result for `WM_RBUTTONUP`/`WM_CONTEXTMENU`:
   - `TrayCommand::Configure` → `config_editor::open(hwnd, config_path())` with `MessageBoxW` on error.
   - `TrayCommand::ReloadConfig` → `PostMessageW(hwnd, WM_APP_RELOAD_CONFIG, ...)`.
   - `TrayCommand::OpenConfigFolder` → `shell_open(&config_path().parent().unwrap().to_string_lossy())`.
   - `TrayCommand::Exit` → `PostMessageW(hwnd, WM_CLOSE, ...)`.
2. Remove the old `shell_open(config.toml)` call that fired directly on tray right-click.

**Patterns to follow:**
- Existing `PostMessageW` call pattern in action dispatch: `PostMessageW(Some(hwnd), WM_CLOSE, WPARAM::default(), LPARAM::default())`
- `launch::shell_open` usage in existing right-click handler

**Test scenarios:**
- Happy path: Tray → Configure... → editor opens with current config.
- Happy path: Tray → Reload configuration → config reloaded from disk, tray tooltip updates.
- Happy path: Tray → Open configuration folder → Explorer opens at `%APPDATA%\mouse-gesture`.
- Happy path: Tray → Exit → daemon shuts down cleanly (tray icon removed, hook stopped, worker joined).
- Error path: Editor fails to open (e.g., config path unavailable) → `MessageBoxW` shown, daemon continues.
- Edge case: Open Folder when config directory does not exist → `shell_open` handles the error gracefully (Explorer shows "path not found" or creates it).

**Verification:**
- Manual: Exercise all four menu items. Verify each produces the expected behavior.
- `cargo check` — remove the old `shell_open(config.toml)` call, no dead code warnings.
- Manual: After Exit, verify process no longer running (`tasklist | findstr mouse-gesture`).

---

### U5. TaskbarCreated tray re-registration

**Goal:** Handle Explorer/taskbar restart by re-registering the tray icon when `TaskbarCreated` is broadcast.

**Requirements:** R13, R14

**Dependencies:** U1 (tray menu infrastructure established)

**Files:**
- Modify: `src/main.rs`

**Approach:**
1. `RegisterWindowMessageW` lives in `Win32_UI_WindowsAndMessaging` — already enabled in `Cargo.toml`. No new feature needed.
2. In `run()`, after creating the notification window, call `RegisterWindowMessageW("TaskbarCreated")` and store the returned message ID in a `static AtomicU32` (avoids adding a field to `DaemonState` and the initialization-order constraint that would entail).
3. In `window_proc`, match the stored message ID and call `tray.reregister()`.
4. `reregister()` already exists in `tray.rs` and correctly re-creates the icon via `NIM_ADD` — no changes needed there.

**Patterns to follow:**
- `lifecycle.rs` `WM_WTSSESSION_CHANGE` registration + handling pattern
- `tray.rs` existing `reregister()` method

**Test scenarios:**
- Happy path: Kill `explorer.exe`, restart it → tray icon reappears within seconds.
- Happy path: Right-click the re-registered icon → context menu still works (callback message preserved).
- Edge case: `RegisterWindowMessageW` returns 0 → log warning, continue without re-registration support (non-fatal).

**Verification:**
- Manual: `taskkill /f /im explorer.exe && start explorer.exe` — verify tray icon returns.
- `cargo check` — `RegisterWindowMessageW` resolves under the already-enabled `Win32_UI_WindowsAndMessaging` feature.
- Manual: Right-click the re-registered icon and verify all menu items still work.

---

## System-Wide Impact

- **Interaction graph:** `window_proc` gains two new message handlers (`WM_APP_RELOAD_CONFIG`, `TaskbarCreated`) and replaces the hard-coded tray right-click with context menu routing. `watch_config()` thread changes from config-applier to file-change notifier. New `config_editor` module introduces a second top-level window with its own window procedure, processed by the daemon's single message pump (same pattern as the overlay window).
- **Error propagation:** `reload_config()` logs errors and updates tray status. Editor save errors display `MessageBoxW`. Menu creation failures return `Err` and log. `TaskbarCreated` failure is non-fatal (log warning).
- **State lifecycle risks:** The centralized reload is the single writer for `DaemonState.config` — no risk of two threads racing on config updates. The editor stores its HWND in a global `AtomicIsize` — cleared on `WM_NCDESTROY`; no leak if the daemon exits while editor is open (the daemon's `WM_DESTROY` + `PostQuitMessage` terminates the process, which destroys all windows).
- **Unchanged invariants:** Gesture recognition, overlay rendering, hook thread architecture, worker thread, singleton mutex — none of these are modified. `ConfigFile::load` and `compile` are unchanged; `parse` is additive.
- **Integration coverage:** The editor → save → `WM_APP_RELOAD_CONFIG` → gesture-dispatch chain is end-to-end — the key integration scenario where manual testing is essential.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `TrackPopupMenuEx` modal loop re-enters `window_proc` and deadlocks on `DaemonState` mutex | Lock is released before calling `show_context_menu()` — only held long enough to check `msg == tray.callback_msg()` |
| Editor `WM_DESTROY` accidentally quits daemon | Separate window class with its own proc that never calls `PostQuitMessage` |
| Config watcher thread panics and loses file monitoring silently | Existing watcher uses `match` with `Err` arms — errors are logged, loop continues |
| `TaskbarCreated` message value collides with an existing message | `RegisterWindowMessageW` returns a system-assigned value in the `0xC000`–`0xFFFF` range, well above `WM_APP` range |
| Atomic config save fails mid-write (disk full, permission) | Temp file write + rename is atomic at the filesystem level. If temp write fails, original file is untouched |

---

## Sources & References

- **Feature specification:** User-provided detailed task description (tray context menu, config reload, editor, TaskbarCreated)
- Related code: `src/tray.rs` (tray icon, `reregister()`), `src/main.rs` (`DaemonState`, `window_proc`, `watch_config`, `dispatch_hook_event`), `src/config.rs` (`ConfigFile`, `ConfigSnapshot`), `src/overlay.rs` (window class pattern), `src/launch.rs` (`shell_open`), `src/app_policy.rs` (`AtomicPtr` snapshot), `src/autostart.rs` (registry sync)
- Related plans: `docs/plans/2026-07-12-003-fix-p0-hook-architecture-issues-plan.md` (modal loop deadlock analysis), `docs/plans/2026-07-12-005-feat-start-with-windows-plan.md` (autostart sync integration)
