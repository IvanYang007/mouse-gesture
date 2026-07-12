---
title: feat: Route gesture actions to right-clicked window instead of foreground
type: feat
status: active
date: 2026-07-12
---

# feat: Route Gesture Actions to Right-Clicked Window

## Summary

Plumb `target_hwnd` (captured at right-button-down) from the hook callback through gesture classification and action dispatch, so window commands operate on the window the user right-clicked on — not the previously-focused foreground window.

---

## Problem Frame

When the user right-clicks on an unfocused window and draws a gesture, the gesture action (close, maximize, snap, keyboard shortcut) operates on the previously-focused foreground window — not the window under the cursor. The `target_hwnd` is captured in `GestureContext` at right-down time but is discarded before action dispatch. The user must left-click first to focus the target window before gestures work correctly.

---

## Requirements

- R1. Window commands (close, maximize, restore, snap) must target the `target_hwnd` captured at gesture start, not `GetForegroundWindow()`.
- R2. Keyboard shortcuts must focus the target window via `SetForegroundWindow` before injecting keystrokes, so the keystrokes land in the intended window.
- R3. Launch actions are unaffected — they spawn new processes and don't need a target window.
- R4. If `target_hwnd` is invalid (window closed mid-gesture), fall back to current behavior gracefully (log warning, use foreground).
- R5. All existing tests continue to pass.

---

## Scope Boundaries

- Excluded: Changing focus behavior on right-button-down (the gesture itself must not trigger window focus changes).
- Excluded: Multi-monitor DPI-aware focus handling (target_hwnd already carries the correct HWND regardless of DPI).
- Excluded: `SetForegroundWindow` permission workarounds (if it fails, keyboard shortcuts silently fall through — this is a known Windows constraint documented in `launch.rs`).

---

## Context & Research

### Relevant Code

**Current flow** (focus broken):

```
handle_right_down → GestureContext { target_hwnd, ... } → StateMachine.context (stored)
    ↓
handle_right_up → recognition worker → HookEvent::GestureEnded { name }
    ↓
dispatch_hook_event → ActionJob::Window { cmd } / Keyboard { inputs }
    ↓
execute_window_action(cmd) → GetForegroundWindow()  ← WRONG WINDOW
execute_keyboard_action(inputs) → inject into focused window  ← WRONG WINDOW
```

**Target flow** (fixed):

```
handle_right_down → GestureContext { target_hwnd, ... } → StateMachine.context → completion
    ↓
recognition worker → HookEvent::GestureEnded { name, target_hwnd }
    ↓
dispatch_hook_event → ActionJob::Window { cmd, target_hwnd } / Keyboard { inputs, target_hwnd }
    ↓
execute_window_action(cmd, target_hwnd) → target_hwnd (correct window)
execute_keyboard_action(inputs, target_hwnd) → SetForegroundWindow(target_hwnd) → inject
```

### Existing Patterns

- `GestureContext` already stores `target_hwnd: HWND` — no new capture needed
- `launch.rs` already has `focus_existing(hwnd)` which calls `SetForegroundWindow` + `FlashWindowEx` fallback
- `ActionJob` enum already carries `name: String` — adding `target_hwnd` follows the same pattern
- `HookEvent::GestureEnded` already carries `gesture_name: Option<String>` — adding `target_hwnd: isize` is consistent

---

## Key Technical Decisions

- **Store `target_hwnd` as `isize` in HookEvent, `HWND` in ActionJob:** `HookEvent` is `#[derive(Clone)]` and stays on the mpsc channel; `isize` is `Copy` + `Send`. `ActionJob` goes to the worker thread where `HWND` is the natural type.
- **Extract `target_hwnd` from completion packet, not from state machine:** The recognition worker can't access hook thread-local state. Add `target_hwnd: isize` to `GestureCompletion` so it's baked into the packet.
- **Keyboard focus is best-effort:** `SetForegroundWindow` may fail due to Windows restrictions. Log a warning and still inject keystrokes — they'll land wherever focus currently is.
- **HWND validation before dispatch:** Check `IsWindow(target_hwnd)` before using it. If invalid, fall back to `GetForegroundWindow()`.

---

## Implementation Units

### U1. Plumb `target_hwnd` through completion → recognition → HookEvent

**Goal:** Add `target_hwnd: isize` to `GestureCompletion` and `HookEvent::GestureEnded` so the target window handle flows from the hook callback to the UI thread.

**Requirements:** R1, R4

**Dependencies:** None

**Files:**
- Modify: `src/input_hook.rs`

**Approach:**
- Add `target_hwnd: isize` field to `GestureCompletion` struct
- In `handle_right_up` (GestureComplete path): read `target_hwnd` from `HOOK_STATE_MACHINE.with(|sm| sm.borrow().as_ref().and_then(|s| s.context.as_ref().map(|c| c.target_hwnd.0 as isize)).unwrap_or(0))` and include it in the completion packet
- Add `target_hwnd: isize` field to `HookEvent::GestureEnded` variant
- Recognition worker: include `completion.target_hwnd` in the `GestureEnded` event
- Default to `0` if target_hwnd is unavailable (no capture context)

**Test scenarios:**
- Happy path: `GestureCompletion` carries `target_hwnd != 0` when a valid capture context exists
- Edge case: `target_hwnd == 0` when gesture completes without a context (theoretical — shouldn't happen in practice)
- Existing tests: all `HookEvent::GestureEnded` constructors updated with `target_hwnd: 0` for test fixtures

**Verification:**
- `cargo test` — all tests pass after updating match arms

---

### U2. Plumb `target_hwnd` through ActionJob to worker dispatch

**Goal:** Add `target_hwnd: HWND` to `ActionJob::Window` and `ActionJob::Keyboard` variants, populate from `HookEvent::GestureEnded.target_hwnd`.

**Requirements:** R1, R2, R4

**Dependencies:** U1

**Files:**
- Modify: `src/main.rs`

**Approach:**
- Add `target_hwnd: HWND` to `ActionJob::Window { name, cmd, target_hwnd }`
- Add `target_hwnd: HWND` to `ActionJob::Keyboard { name, inputs, target_hwnd }`
- In `dispatch_hook_event`: read `event.target_hwnd` and convert to `HWND(event.target_hwnd as *mut _)` before sending ActionJob
- Worker thread: pass `target_hwnd` to `execute_window_action(&cmd, target_hwnd)` and `execute_keyboard_action(&inputs, target_hwnd)`
- Add `target_hwnd.is_invalid()` guard: if `HWND::default()` or `HWND(0)`, use `GetForegroundWindow()` as fallback

**Test scenarios:**
- Happy path: ActionJob carries valid target_hwnd, window action operates on correct window
- Edge case: target_hwnd is null (0) → falls back to GetForegroundWindow()

**Verification:**
- `cargo build --release` compiles
- `cargo test` — all tests pass

---

### U3. Route window actions to target_hwnd, add keyboard focus

**Goal:** `execute_window_action` uses `target_hwnd` instead of `GetForegroundWindow()`. `execute_keyboard_action` calls `SetForegroundWindow(target_hwnd)` before injecting keystrokes.

**Requirements:** R1, R2

**Dependencies:** U2

**Files:**
- Modify: `src/main.rs`

**Approach:**
- Change `execute_window_action(cmd: &WindowCommand)` → `execute_window_action(cmd: &WindowCommand, target_hwnd: HWND)`
- Remove `let hwnd = unsafe { GetForegroundWindow() }` — use `target_hwnd` directly
- Validate HWND: `if target_hwnd.is_invalid() || !IsWindow(target_hwnd) { target_hwnd = GetForegroundWindow(); }`
- Change `execute_keyboard_action(inputs)` → `execute_keyboard_action(inputs: &[CompiledInput], target_hwnd: HWND)`
- Before injecting keystrokes: call `unsafe { SetForegroundWindow(target_hwnd) }` with `FlashWindowEx` fallback via `launch::focus_existing(hwnd)`
- If `SetForegroundWindow` fails, log a warning and still inject (keystrokes fall through to current focus)

**Test scenarios:**
- Happy path: draw gesture on unfocused Notepad window, gesture action affects Notepad
- Edge case: target_hwnd closed mid-gesture → IsWindow check returns false → falls back to foreground
- Manual: verify maximize/close/snap work on unfocused windows

**Verification:**
- `cargo build --release` + `cargo test`
- Manual: open two windows, right-click-draw on unfocused one → action targets correct window

---

## System-Wide Impact

- **Interaction graph:** `HookEvent::GestureEnded` gains one field — all match arms updated. `ActionJob` gains one field on 2 variants — worker dispatch updated.
- **Error propagation:** Invalid HWND falls back to `GetForegroundWindow()` with `debug!` log. `SetForegroundWindow` failure logged at `warn!` level.
- **State lifecycle risks:** None — `target_hwnd` is captured at right-down and remains valid for the gesture duration. Window destruction mid-gesture is handled by `IsWindow` check.
- **Unchanged invariants:** Launch actions do not receive `target_hwnd`. `ActionJob::Launch` is unchanged.

---

## Sources & References

- Current code: `src/main.rs:356` (`let hwnd = unsafe { GetForegroundWindow() }`)
- Current code: `src/input_hook.rs:484` (`target_hwnd` already captured in `GestureContext`)
- `src/launch.rs:81` (`focus_existing` — existing SetForegroundWindow + FlashWindowEx pattern)
