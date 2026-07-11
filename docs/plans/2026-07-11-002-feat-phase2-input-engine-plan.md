---
type: feat
status: active
created: 2026-07-11
origin: docs/plans/2026-07-11-001-feat-mouse-gesture-daemon-plan.md
depth: standard
---

## Summary

Wire the Phase 1 library modules (config, gesture recognizer, state machine, hook thread, app policy) into a working daemon. Create a notification window, start the hook thread, resolve real target HWNDs, populate the blacklist policy cache from startup config, and establish the gesture → action dispatch pipeline to the worker thread. The daemon intercepts right-clicks, recognizes gestures, and queues actions. Config loaded once at startup from `%APPDATA%\mouse-gesture\config.toml`; hot-reload deferred to Phase 4.

## Problem Frame

Phase 1 built the library — config parsing, gesture recognition, state machine, hook thread, window ops, injection, launching, tray, lifecycle. All 27 tests pass. But the modules sit in isolation: the hook thread is spawned but never wired to real HWND resolution, the policy cache is empty, config is never pushed to the hook thread, and action dispatch has no destination. Phase 2 bridges these pieces into a runnable daemon.

---

## Requirements

| ID | Requirement | Origin |
|----|-------------|--------|
| R1 | Daemon starts, loads config from `%APPDATA%\mouse-gesture\config.toml`, initializes hook state | U3, U4 |
| R2 | Notification window created with PMv2 DPI awareness, receives timer and queue messages | K2 |
| R3 | Hook thread resolves target HWND via `WindowFromPoint` + `GetWindowThreadProcessId` at right-button-down | U3 |
| R4 | Policy cache pre-warmed from config blacklist; PID→eligibility checked in hook proc | U4 |
| R5 | Config snapshot pushed to hook thread on startup; `init_hook_state` called inside hook thread | fix P1#2 |
| R6 | Gesture classification completed in hook proc, result sent to UI thread via event channel | U3 |
| R7 | Action dispatch pipeline: UI thread receives gesture match → queues action to worker thread | K2 |
| R8 | Worker thread receives actions; logs action name (no-op execution — Phase 3) | U6 |
| R9 | Daemon runs headless (no console window) in release builds | K6 |
| R10 | No config found → daemon starts with interception disabled, all right-clicks pass through | call-out #2 |

---

## Key Technical Decisions

**K1. Hidden notification window, not HWND_MESSAGE.** The master plan specifies a top-level window because `WTSRegisterSessionNotification` and `TaskbarCreated` require one. For Phase 2, the window exists solely for message routing (timer messages, synthetic click scheduling). It does not create a tray icon yet (Phase 4).

**K2. Worker thread is spawned but executes no-ops.** The worker thread receives `ActionJob` messages containing gesture names and compiled action data. In Phase 2, it logs the action name and returns success. Phase 3 will implement actual `SendInput`, `SetWindowPos`, `CreateProcessW` dispatch.

**K3. Config file is read once at startup.** No `ReadDirectoryChangesW` watching in Phase 2. If the config file is missing or invalid, the daemon starts with interception disabled — all right-clicks pass through. The user must restart to pick up config changes.

**K4. `WindowFromPoint` in the hook proc.** The hook callback runs in the context of the message pump. `WindowFromPoint` returns the HWND under the cursor at that moment — it must be called before any state changes. The daemon's own windows are excluded by checking the window class or process ID.

**K5. Release builds use `#![windows_subsystem = "windows"]`.** Debug builds keep the console for `env_logger` output.

---

## Implementation Units

### U1. Notification window and message pump wiring

- **Goal:** Create the hidden notification window, wire it to the hook thread, and establish the main message pump.
- **Requirements:** R1, R2, R9
- **Dependencies:** None (all library modules exist)
- **Files:**
  - `src/main.rs` — modify, add `WindowClass` registration, `CreateWindowExW`, message pump loop
- **Approach:**
  - Register a window class with `RegisterClassExW` and a simple `WindowProc` that handles: `WM_TIMER` (gesture timeout), `WM_APP+1` (tray callback — stubbed for Phase 4), `WM_CLOSE` / `WM_DESTROY` (shutdown), `WM_QUERYENDSESSION` (return `TRUE`).
  - Create the window with `CreateWindowExW(0, class_name, "MouseGestureDaemon", WS_OVERLAPPED, 0, 0, 0, 0, None, None, hinstance, None)` — invisible, zero-size.
  - In `main()`: register class → create window → load config → push config snapshot to hook thread → spawn worker thread → spawn hook thread → enter `GetMessage` loop.
  - Release builds: add `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` to disable console.
  - Shutdown: on `WM_DESTROY`, send `HookCommand::Shutdown` to hook thread, join threads, `PostQuitMessage(0)`.
- **Patterns to follow:** Standard Win32 message pump pattern; existing `windows` crate usage in the codebase
- **Test scenarios:**
  - Daemon starts and creates window → `GetMessage` loop running, hook thread spawned
  - Release build runs without console window
  - Ctrl+C or window close triggers clean shutdown → threads joined, hook unhooked
  - Missing config file → daemon starts with interception disabled, logs warning
- **Verification:** `cargo build --release` produces a binary; run it — no console appears, process starts and stays resident; close via task manager → clean exit in logs

---

### U2. Hook thread HWND resolution and policy lookup

- **Goal:** Replace `HWND::default()` placeholders with real `WindowFromPoint` + `GetWindowThreadProcessId` calls in the hook proc. Wire the policy cache into eligibility checks.
- **Requirements:** R3, R4, R5
- **Dependencies:** U1 (notification window exists, config loaded)
- **Files:**
  - `src/input_hook.rs` — modify, `handle_right_down` to call `WindowFromPoint`, `GetWindowThreadProcessId`
  - `src/app_policy.rs` — modify, export a lightweight `is_pid_eligible(pid, cache)` function callable from the hook thread
  - `src/main.rs` — modify, pass compiled policy cache to hook thread via `HookCommand::UpdateConfig`
- **Approach:**
  - In `process_mouse_event`, at `WM_RBUTTONDOWN`:
    1. Call `WindowFromPoint(POINT{x, y})` to get the HWND under the cursor
    2. Call `GetWindowThreadProcessId(hwnd, &mut pid)` to get the PID
    3. Exclude daemon's own windows by comparing PID against `GetCurrentProcessId()`
    4. Look up PID in the policy cache (thread-local snapshot from config)
    5. Store HWND and PID in `GestureContext`
  - Policy cache: before spawning the hook thread, compile a `HashMap<u32, bool>` of PID→eligible from the config blacklist. Send it as part of the initial `HookCommand::UpdateConfig`.
  - The hook proc accesses policy via `HOOK_CONFIG` thread-local. The `ConfigSnapshot` carries the compiled blacklist as `HashSet<String>` (basenames). In Phase 2, PID eligibility is determined at button-down time by resolving the basename and checking against the set.
  - If `WindowFromPoint` returns `NULL` or the daemon's own HWND → treat as NativePass.
- **Patterns to follow:** Existing `HOOK_CONFIG`/`HOOK_STATE_MACHINE` thread-local pattern in `input_hook.rs`
- **Test scenarios:**
  - Right-click on Notepad → HWND resolved, PID captured, policy lookup returns eligible
  - Right-click on blacklisted app → policy returns ineligible, NativePass
  - Right-click on daemon's own window → excluded, NativePass
  - `WindowFromPoint` returns NULL → NativePass
  - PIDs already in cache → instant eligibility check (no worker roundtrip needed in Phase 2 since cache is pre-warmed)
- **Verification:** Add `trace!` logging to hook proc; run daemon, right-click on various windows; logs show correct PIDs and eligibility decisions

---

### U3. Gesture → action dispatch pipeline

- **Goal:** When a gesture is classified as matched in the hook proc, route the match result through the event channel to the UI thread, and queue the action to the worker thread.
- **Requirements:** R6, R7, R8
- **Dependencies:** U2 (hook thread resolves HWNDs and classifies gestures)
- **Files:**
  - `src/input_hook.rs` — no changes (already sends `HookEvent::GestureEnded { matched, gesture_name }`)
  - `src/main.rs` — modify, in the message pump: receive `HookEvent` from the hook thread's event channel, forward matched gestures to worker
  - `src/main.rs` — modify, spawn worker thread with an mpsc channel receiving `ActionJob`
- **Approach:**
  - **Hook event handling in UI thread:**
    - In the main message pump, use `PeekMessage` to check for hook events alongside Windows messages
    - On `HookEvent::GestureEnded { matched: true, gesture_name }`: look up the gesture's compiled action from the config snapshot (stored in UI thread). Build an `ActionJob` containing the action type and gesture name. Send to worker thread via channel.
    - On `HookEvent::GestureEnded { matched: false }`: log the event (no action).
    - On `HookEvent::ReplaySyntheticClick { x, y }`: call `SendInput` with a synthetic right-click at the given coordinates (this is simple enough to do from the UI thread).
  - **Worker thread:**
    - Receives `ActionJob` via bounded mpsc channel (16 deep, per plan spec)
    - In Phase 2: logs `"Action: {gesture_name} (type: {action_type})"` at info level
    - Queue full → log warning, drop action
    - Worker thread loops on `rx.recv()` until channel closes (shutdown)
  - **Config access in UI thread:** After loading and compiling config in `main()`, store the `ConfigSnapshot` in a `Mutex<Option<ConfigSnapshot>>` owned by the main thread. The message pump handler borrows it to look up actions when `GestureEnded` events arrive.
- **Patterns to follow:** Existing `std::sync::mpsc` channel pattern from `input_hook.rs`
- **Test scenarios:**
  - Matched gesture → worker thread logs action name
  - Unmatched gesture → log entry, no action queued
  - Synthetic click replay → `SendInput` called with correct coordinates
  - Worker queue full → warning logged, action dropped
  - Shutdown → worker thread receives channel close, exits cleanly
- **Verification:** Run daemon in debug mode (console visible); perform a configured gesture → worker log output confirms action dispatch

---

### U4. End-to-end integration test

- **Goal:** Add integration tests that simulate the full hook → classify → dispatch pipeline without requiring real mouse input.
- **Requirements:** R1–R8 (verification)
- **Dependencies:** U1, U2, U3
- **Files:**
  - `tests/integration_tests.rs` — modify, add pipeline tests
- **Approach:**
  - Test: load a config with gestures → compile snapshot → initialize hook thread-locals with the snapshot → simulate points in a `GestureBuffer` → call `classify` → verify matched gesture name
  - Test: policy cache lookup → known PID returns eligible, blacklisted PID returns ineligible
  - Test: synthetic replay event → `HookEvent::ReplaySyntheticClick` is constructed correctly with x,y
  - These tests exercise the Rust types and logic without needing a running Windows message pump
- **Test scenarios:**
  - Full classify pipeline: config → compile → buffer points → classify → matched gesture name matches config
  - Policy cache: PID 1234 in blacklist → ineligible; PID 5678 not in blacklist → eligible (blacklist mode)
  - Synthetic replay event carries correct coordinates
- **Verification:** `cargo test` passes all new tests alongside existing 27

---

## Scope Boundaries

### Deferred to Phase 3 (Actions and DPI)
- Actual `SetWindowPos` / `ShowWindowAsync` window tiling execution
- Actual `SendInput` keyboard shortcut injection execution
- Actual `CreateProcessW` / `ShellExecuteW` app launching
- `focus_existing` best-effort window activation

### Deferred to Phase 4 (Tray and Lifecycle)
- System tray icon with menu
- `ReadDirectoryChangesW` config hot-reload
- `WTSRegisterSessionNotification` session lock/unlock
- Singleton mutex
- Rotating logs

### Deferred to Phase 5 (Reliability)
- 72-hour soak test
- Latency benchmarks
- Handle/GDI/USER leak detection

---

## Dependencies / Prerequisites

- Rust 1.97+ (installed)
- `windows` crate 0.62 (already in Cargo.toml)
- All Phase 1 modules compile and pass tests (confirmed: 27/27)

---

## System-Wide Impact

- **Input pipeline:** `WH_MOUSE_LL` system-wide hook intercepts all right-clicks. Non-gesture clicks pass through with <50µs added latency. Eligible windows undergo PID resolution + policy lookup (~10µs).
- **Window creation:** One invisible top-level window. No taskbar entry, no tray icon.
- **Thread count:** 3 threads (main/UI, hook, worker). All join on shutdown.
- **Memory:** Private working set target <5MiB. Config snapshot + policy cache + gesture buffers = ~100KB. Remaining ~4.9MiB budget for runtime overhead.
