---
title: fix: Remediate P0 hook architecture issues
type: fix
status: active
date: 2026-07-12
---

# fix: Remediate P0 Hook Architecture Issues

## Summary

Fix seven architectural P0 bugs in the mouse-gesture hook thread: add a command wake mechanism using `PostThreadMessageW`, offload gesture classification from the `WH_MOUSE_LL` callback to a dedicated recognition worker, replace synchronous policy resolution with atomic snapshot swaps, remove the unconditional 5-second ForceReset timer with a state-aware watchdog, symmetrically filter injected events across all hook messages, preserve the gesture start point in the buffer, and move click replay to a high-priority worker with absolute coordinate injection.

---

## Problem Frame

Seven P0 architectural defects make the hook thread unreliable: commands don't wake `GetMessageW` (config updates and shutdown are delayed), classification allocates and blocks inside the low-level hook callback, policy resolution runs synchronous `WindowFromPoint`/`GetWindowThreadProcessId`/`resolve_pid` from the hook proc, the 5-second ForceReset timer cancels valid gestures and can swallow right-up events, injected events are only filtered on right-down (foreign injected movement contaminates gesture buffers and foreign injected right-up terminates physical captures), the gesture buffer lacks the right-down start point (first direction segment is biased or lost), and synthetic click replay runs on the UI thread with incorrect relative coordinates. Each of these issues independently causes gesture corruption, missed actions, or process hangs. Together they make the daemon unreliable under real-world usage.

---

## Requirements

- R1. Hook-thread commands (`UpdateConfig`, `SetInterception`, `ForceReset`, `Shutdown`) must take effect immediately without waiting for mouse input.
- R2. Config must be installed and interception enabled before any gesture can begin.
- R3. The `WH_MOUSE_LL` callback must return in under 150 µs for button events and under 100 µs for move events (p99.9); classification (RDP, quantize, match) must run off-hook.
- R4. Injected events must be symmetrically filtered: self-injected passes through without gesture interference; foreign-injected does not start, move, or terminate a physical gesture.
- R5. The gesture path must include the right-button-down point so the first direction segment is accurate.
- R6. No unconditional periodic ForceReset may interfere with active gestures or cause asymmetric up-events.
- R7. Shutdown must complete with no mouse activity — `PostThreadMessageW(WM_HOOK_SHUTDOWN)` must wake the hook thread.
- R8. Synthetic click replay must inject at the release coordinates, not the cursor's position when the UI thread eventually processes the event.
- R9. All 52 existing tests must continue to pass. No behavioral regression for working gestures.
- R10. The number of allocated bytes inside the hook callback (including stack) must be bounded and deterministic — no heap allocation in the callback path.

---

## Scope Boundaries

- Excluded: Adding new gesture features (fuzzy matching, weighted edit-distance, trail overlay, recording UI).
- Excluded: Full `OpenProcess` + `QueryFullProcessImageNameW` process-name resolution (policy snapshot architecture supports it, but the resolution function itself stays a stub — the async queue pattern and snapshot swap are the delivery).
- Excluded: Telemetry, performance monitoring, or benchmarking infrastructure.
- Excluded: Changes to config file format, tray icon, overlay rendering, or window operation commands.

---

## Context & Research

### Relevant Code and Patterns

**Current architecture** (`src/input_hook.rs`, `src/state_machine.rs`, `src/gesture.rs`):

The hook thread runs `GetMessageW(None, 0, 0)` which blocks until a Windows message or hook callback arrives. Commands arrive via `std::sync::mpsc::Receiver<HookCommand>` but are only drained with `while let Ok(cmd) = cmd_rx.try_recv()` **after** `GetMessageW` returns. This means commands are invisible until the next mouse event.

Classification in `handle_right_up` runs RDP simplification, direction quantization, collapse, and linear pattern scan — all inside the hook callback, including `Vec::with_capacity`, `vec![false]`, `string::clone()`, and collapsed-direction `Vec` allocation.

Right-down (`handle_right_down`) calls `WindowFromPoint` + `GetWindowThreadProcessId` + `resolve_pid` synchronously in the callback. The `is_injected` flag is only passed to `handle_right_down` — `handle_right_up` and `handle_mouse_move` receive no `is_injected` parameter.

The 5-second `WM_TIMER` on the UI notification window sends `HookCommand::ForceReset` unconditionally regardless of whether a gesture is active or the right button is physically held.

The gesture buffer starts empty on right-down; the first point is the first post-activation-threshold move event, not the button-down position. Spatial coalescing caps at `sample_distance_sq`.

Synthetic click replay sends `HookEvent::ReplaySyntheticClick` to the UI thread which calls `inject_synthetic_click` using `SendInput` with `MOUSEINPUT.dx`/`dy` as relative coordinates (no `MOUSEEVENTF_ABSOLUTE` flag).

**Key structs:**
- `HookShared` (AtomicBool shutdown, ready, interception_enabled) in `src/input_hook.rs`
- Thread-locals: `HOOK_CONFIG`, `HOOK_BUFFER`, `HOOK_STATE_MACHINE`, `HOOK_EVENT_TX`, `HOOK_INTERCEPTION_ENABLED`, `HOOK_UI_HWND`, `HOOK_LAST_DIRECTION`
- `GestureBuffer { points: [Point; 256], len, sample_distance_sq, last_sample }` in `src/gesture.rs`
- `StateMachine { state, context, activation_threshold, activated, generation }` in `src/state_machine.rs`
- `PolicyCache { entries: HashMap<u32, PolicyEntry>, mode }` in `src/app_policy.rs`

**Patterns to preserve:**
- Endpoint force-capture (commit `018292f`): `buf.add_point(Point{x,y})` in `handle_right_up` before classification prevents final-segment loss from spatial coalescing
- Config generation guard: `current_gen != captured_gen` returns `NoMatch` to prevent pattern mismatch during hot-reload mid-gesture
- Self-tag filtering: `dwExtraInfo == SELF_TAG` skips own injected events
- `catch_unwind` in hook proc with `CallNextHookEx` fallback

### Institutional Learnings

No formal `docs/solutions/` directory exists. The following are synthesized from plan documents, code analysis, and git history:

- **Shutdown deadlock** (confirmed): `HookCommand::Shutdown` breaks the inner `while let` loop, not the outer `loop`. `shared_clone.shutdown` is never set. The main thread's `hook_handle.join()` blocks forever without mouse input. The Phase 1 plan specified a 2-second join timeout that was never implemented.
- **ForceReset deadlock** (confirmed): The `WM_TIMER` fires on the UI thread but the hook thread's `GetMessageW` never wakes for channel messages. A lost button-up can leave the state machine dirty indefinitely.
- **Endpoint force-capture** (commit `018292f`): Must be preserved in any refactor — without it, fast-draw-immediate-release gestures lose their final direction segment.

### External References

- **Microsoft hook documentation** (learn.microsoft.com): `PostThreadMessageW` can wake a thread message queue; the receiving thread must create its queue via `PeekMessage` before the sender uses the thread ID. A dedicated hook thread should hand work off and immediately return.
- **SendInput UIPI constraints** (learn.microsoft.com): `SendInput` returns the number of inserted events and is subject to UIPI. `MOUSEINPUT.dx`/`dy` without `MOUSEEVENTF_ABSOLUTE` are interpreted as relative motion.
- **WGestures reference implementation** (github.com): Initializes recognition around the gesture start position, separately applying activation and effective-movement thresholds.

---

## Key Technical Decisions

- **PostThreadMessageW over MsgWaitForMultipleObjects** (P0.1): `PostThreadMessageW` with a thread-specific `WM_HOOK_COMMAND` message is the simplest correct wake mechanism. It requires the hook thread to call `PeekMessage` to create its message queue before publishing its thread ID, and the sender to call `PostThreadMessageW` after each `cmd_tx.send()`. The thread ID is communicated back to the spawner via a `ready_tx` oneshot.

- **Dedicated recognition worker over reusing action worker** (P0.2): A separate recognition thread keeps classification latency isolated from action dispatch latency. The hook callback enqueues a compact `CompletedGesture` packet and returns immediately. The recognition worker runs RDP → quantize → collapse → match and posts results to the UI thread. This meets the sub-150 µs callback budget and keeps action dispatch unaffected.

- **Atomic policy snapshot over hook-thread lock** (P0.3): A `PolicySnapshot` with `Arc<HashMap<u32, Eligibility>>` is atomically swapped via `Arc::swap` or `AtomicPtr`. The hook callback reads the current snapshot with a single atomic load and does a read-only PID lookup. A background policy worker builds new snapshots when config changes and publishes them atomically — no lock ever held by the hook thread.

- **State-aware watchdog over periodic timer** (P0.4): Delete `SetTimer(TIMER_GESTURE_ID)`. The hook thread's watchdog logic fires only when the physical right button transitions from down to unknown (session lock, shutdown, or watchdog expiration). A move-event timestamp comparison detects stale gestures. No timer-based interruption of valid gestures.

- **`InputOrigin` enum over boolean `is_injected`** (P0.5): `enum InputOrigin { Physical, SelfInjected, ForeignInjected, LowerIntegrityInjected }`. Every hook event (right-down, right-up, move) receives an `InputOrigin`. A capture started by physical input only ends on physical right-up. Self-injected events pass through without touching gesture state. Foreign-injected move events never add points to the buffer.

- **`GestureBuffer::add_force` over sampling tweaks** (P0.6): Add `add_force(point)` that skips spatial coalescing (only rejects exact duplicate coordinates). Seed the buffer with `add_force(start_point)` on right-down `Consumed`. On activation, `add_force(threshold_point)`. On release, `add_force(release_point)`. The existing `add_point` with spatial coalescing remains for move events.

- **Dedicated replay worker over inline UI-thread injection** (P0.7): A high-priority replay worker receives `ClickReplay` packets and immediately calls `SendInput` with `MOUSEEVENTF_ABSOLUTE` and normalized coordinates. Validates the `SendInput` return count. Self-tagging prevents re-entry. The hook callback only enqueues and returns.

---

## Open Questions

### Resolved During Planning

- Q: Should the recognition worker be a new thread or reuse the existing action worker?
  - A: New thread. The action worker handles `CreateProcessW`/`ShellExecuteW` which can block for seconds. Classification needs sub-5ms responsiveness. Different latency budgets, different threads.

- Q: Should P0.3 implement `OpenProcess` + `QueryFullProcessImageNameW` or just the atomic snapshot architecture?
  - A: Architecture only. Build the `PolicySnapshot`, `Arc`-swap publication, async resolution queue, and hook-side read-only lookup. The PID→basename resolution function itself stays a stub (`format!("pid_{}", pid)`). The architecture is the P0 fix — full process-name resolution is a separate enhancement.

- Q: Should synthetic click replay at down position or release position?
  - A: Release position. For a sub-threshold right-click, the user expects the context menu at the release point, matching native Windows behavior.

### Deferred to Implementation

- Exact `MAX_COMPLETION_POINTS` constant for the `CompletedGesture` packet — depends on profiling typical gesture point counts.
- Whether the policy worker needs a dedicated thread or can poll from the config watcher thread — depends on `OpenProcess` latency when the stub is replaced.

---

## Output Structure

Changes are limited to existing files. No new modules. The recognition worker and replay worker live in `src/input_hook.rs` (recognition path) and `src/main.rs` (replay path) respectively — no new source files.

---

## High-Level Technical Design

### Post-Change Architecture (4 threads)

```
┌──────────────────┐  CompletedGesture (spsc)   ┌──────────────────┐
│  Hook Thread     │ ────────────────────────→  │ Recognition      │
│  (mouse-hook)    │                             │ Worker Thread    │
│                  │ ←───────────────────────   │                  │
│ WH_MOUSE_LL      │  WM_HOOK_COMMAND wake      │ classify → post  │
│ StateMachine     │                             │ result to UI     │
│ GestureBuffer    │                             └────────┬─────────┘
│ InputOrigin      │                                      │
│ PolicySnapshot*  │                            HookEvent (mpsc)
└────────┬─────────┘                                      │
         │ HookEvent (mpsc)                               │
         │                                                ▼
         │                                      ┌──────────────────┐
         │  HookCommand (mpsc)                   │  UI Thread (Main)│
         │ ←─────────────────────────────────── │                  │
         │  PostThreadMessageW(hook_tid,         │ Notification Win │
         │    WM_HOOK_COMMAND) wake              │ OverlayWindow    │
         │                                       │ TrayIcon         │
         └──────────────────────────────────────→│ ConfigWatcher    │
            ClickReplay (spsc queue)             └────────┬─────────┘
                                                          │
                                                ActionJob (mpsc)
                                                          │
                                                          ▼
                                                 ┌──────────────────┐
                                                 │  Action Worker   │
                                                 │  Window/Key/     │
                                                 │  Launch dispatch │
                                                 └──────────────────┘
```

### Hook Thread Message Loop (P0.1)

```
PeekMessageW(PM_NOREMOVE)   // create thread message queue
ready_tx.send(thread_id)    // publish thread ID

loop:
    GetMessageW(0, 0)
    if ret <= 0 → break
    match msg.message:
        WM_HOOK_COMMAND → drain cmd_rx.try_recv()
            UpdateConfig → store + update policy snapshot ref
            SetInterception → flip thread-local
            ForceReset → state-aware: only if button not physically held
            Shutdown → set shutdown flag, drain, break outer
        WM_HOOK_SHUTDOWN → break
```

### Recognition Worker (P0.2)

```
// Fixed-size completion packet — no heap allocation in callback
struct GestureCompletion {
    points: [CapturePoint; MAX_POINTS],
    point_count: usize,
    release_point: Point,
    config_gen: u64,
}

// Recognition worker loop:
for completion in completion_rx:
    run RDP → quantize → collapse → match
    send HookEvent::GestureEnded to UI
    return buffer slot to pool
```

### Policy Snapshot (P0.3)

```
struct PolicySnapshot {
    mode: BlacklistMode,
    generation: u64,
    known_pids: HashMap<u32, Eligibility>,
}

// Hook thread: atomic load
let snapshot = POLICY_SNAPSHOT.load(Ordering::Acquire);

// Hook callback:
match snapshot.known_pids.get(&pid) {
    Some(Allowed) → capture,
    Some(Denied)  → pass_through,
    None → queue_pid_resolution(pid);
            match snapshot.mode {
                Blacklist → capture,  // default allow
                Whitelist → pass_through, // default deny
            }
}
```

### Watchdog Logic (P0.4)

```
struct ActiveCapture {
    started_at: Instant,
    last_event_at: Instant,
    physical_button_down: bool,
}

on right-down:
    capture.physical_button_down = true

on right-up:
    capture.physical_button_down = false → classify

shutdown / session-lock:
    if capture.physical_button_down:
        synthesize right-up → classify → reset

WM_HOOK_COMMAND(ForceReset):
    // no-op — ForceReset no longer resets mid-gesture
    // preserved as no-op to avoid breaking session-lock path
```

### InputOrigin Classification (P0.5)

```
enum InputOrigin { Physical, SelfInjected, ForeignInjected }

struct CaptureIdentity {
    origin: InputOrigin,
    button: MouseButton,
}

// Every hook event receives origin
fn handle_right_down(origin: InputOrigin, ...):
    Physical → StateMachine::on_right_down
    SelfInjected → CallNextHookEx (pass through, no state change)
    ForeignInjected → CallNextHookEx (pass through, no state change)

fn handle_mouse_move(origin: InputOrigin, ...):
    if origin != current_capture.origin → ignore
    // never add foreign points to a physical gesture buffer

fn handle_right_up(origin: InputOrigin, ...):
    if origin != current_capture.origin → ignore
    // foreign up cannot terminate a physical capture
```

---

## Implementation Units

### U1. Add Command Wake Mechanism and Fix Shutdown

**Goal:** Make hook-thread commands (`UpdateConfig`, `SetInterception`, `ForceReset`, `Shutdown`) take effect immediately by waking `GetMessageW` via `PostThreadMessageW`. Fix the shutdown bug where `HookCommand::Shutdown` breaks only the inner `while let` loop.

**Requirements:** R1, R2, R7, R9

**Dependencies:** None

**Files:**
- Modify: `src/input_hook.rs`

**Approach:**
- Define `WM_HOOK_COMMAND = WM_APP + 1` and `WM_HOOK_SHUTDOWN = WM_APP + 2` in `HookShared` or as module constants.
- Create `HookController` struct wrapping `cmd_tx: Sender<HookCommand>` and `hook_thread_id: u32`. Its `send()` method calls `cmd_tx.send(command)?` then `PostThreadMessageW(hook_thread_id, WM_HOOK_COMMAND, ...)?`.
- In the hook thread, before publishing readiness: call `PeekMessageW(PM_NOREMOVE)` to create the thread message queue, then `GetCurrentThreadId()`, then send the thread ID to the spawner via a `std::sync::mpsc::sync_channel(0)` (rendezvous) or a oneshot.
- Replace `cmd_rx` drain location: move command processing into the `match msg.message` arm for `WM_HOOK_COMMAND`. The `while let Ok(cmd)` drain remains but now fires immediately after the wake.
- `HookCommand::Shutdown`: set `shared_clone.shutdown.store(true, Ordering::SeqCst)`, drain remaining commands, then `break` the outer `loop`. Remove the old inner-`break` that only exited the `while let`.
- `WM_HOOK_SHUTDOWN`: also break the outer loop (fallback path).
- Update `spawn_hook_thread` return type: return `HookController` instead of raw `Sender<HookCommand>`.
- Update all call sites in `src/main.rs` (`run()`, `window_proc`, config watcher) to use `HookController::send()`.

**Patterns to follow:**
- Existing `post_event` function which already calls `PostMessageW` for UI wake-up (the same pattern, but `PostThreadMessageW` for the hook thread itself).
- The `spawn_hook_thread` signature: return `(JoinHandle, Arc<HookShared>, HookController, Receiver<HookEvent>)`.

**Test scenarios:**
- Happy path: Send `HookCommand::UpdateConfig` and verify the hook thread processes it before the next mouse event (use `HookShared::ready` as a synchronization point; after the sender calls `HookController::send()`, poll `ready` — it should be updated without generating mouse input).
- Happy path: Send `HookCommand::Shutdown` and verify the `JoinHandle` joins within 1 second with no mouse input.
- Happy path: Send multiple rapid commands and verify all are drained in order.
- Edge case: Hook thread not yet ready (thread ID not published) — `HookController::send()` should return an error or panic-safe before the oneshot resolves.
- Edge case: `PostThreadMessageW` failure — `HookController::send()` propagates the error; caller in `main.rs` logs and continues.

**Verification:**
- Manual: start daemon, verify "Hook installed" appears and config is active without moving the mouse.
- Manual: send taskkill to the daemon; verify it exits cleanly within 1 second.
- All existing tests pass.

---

### U2. Add `add_force` API and Seed Start Point in Buffer

**Goal:** Preserve the right-button-down point and activation-threshold-crossing point in the gesture buffer so the first direction segment is never lost. Add `GestureBuffer::add_force` that skips spatial coalescing.

**Requirements:** R5, R9

**Dependencies:** None (can land before or after U1)

**Files:**
- Modify: `src/gesture.rs`
- Modify: `src/input_hook.rs` (call sites in `handle_right_down`, `handle_mouse_move` activation path, and `handle_right_up`)
- Test: `src/gesture.rs` (existing `#[cfg(test)]` module)

**Approach:**
- Add `GestureBuffer::add_force(&mut self, p: Point) -> bool` that:
  - Skips spatial coalescing check (`last_sample` distance comparison)
  - Only rejects exact duplicate coordinates (`p.x == last.x && p.y == last.y`)
  - Otherwise identical to `add_point` (overflow decimation, bounds check)
- In `handle_right_down` on `DownResult::Consumed`: call `buf.add_force(Point { x, y })` to seed the down point.
- In `handle_mouse_move` on activation (`activated == true`): call `buf.add_force(Point { x, y })` for the threshold-crossing point (since it may have been dropped by spatial coalescing on the preceding move event).
- Keep the existing `buf.add_point(Point { x, y })` call in `handle_right_up` (endpoint force-capture) — this already exists from commit `018292f`.
- `add_force` marks `last_sample` so subsequent `add_point` calls continue spatial coalescing from the forced point.

**Patterns to follow:**
- Existing `add_point` method signature and overflow behavior.
- Existing `clear()` which resets `self.len = 0` and `self.last_sample = None`.

**Test scenarios:**
- Happy path: Seed buffer with `add_force(start_point)`, then add move points with `add_point` — first direction from `start_point` to first move point is correct.
- Happy path: `add_force` stores a point that `add_point` would reject (within `sample_distance_sq` of last sample) — verify `buffer.len()` increments and the point is in `stored_points()`.
- Edge case: `add_force` with exact duplicate coordinates — verify it returns `true` but does not duplicate the point, or returns `false`. Behavior should be documented.
- Edge case: Buffer overflow with `add_force` — adaptive decimation, same as `add_point`.
- Edge case: Empty buffer → `add_force` → `add_force` → `add_point` sequence — verify `len` and direction computation correctness.
- Integration: A fast short gesture (30px total movement) should classify correctly when the down point is preserved (previously it may have had zero direction tokens).

**Verification:**
- Existing gesture tests pass: `straight_right_gesture`, `right_then_down_gesture`, `no_match`, `spatial_coalescing`, `buffer_overflow_decimates`. New test: `start_point_preserved`.
- `cargo test` — 52 tests pass.

---

### U3. Move Classification Off the Hook Callback

**Goal:** The `WH_MOUSE_LL` callback must never run RDP simplification, direction quantization, pattern scanning, or string cloning. It finalizes a compact `GestureCompletion` packet and posts it to a recognition worker thread.

**Requirements:** R3, R10

**Dependencies:** U1 (hook thread message loop is needed for the recognition worker thread coordination)

**Files:**
- Modify: `src/input_hook.rs`
- Modify: `src/gesture.rs` (classify function — no change to the algorithm, only the call site moves)
- Test: `src/gesture.rs` (existing tests)

**Approach:**
- Define `GestureCompletion` packet with fixed-size arrays:
  ```rust
  struct CapturePoint { x: i32, y: i32 }
  struct GestureCompletion {
      points: [CapturePoint; MAX_POINTS],
      point_count: usize,
      release_point: Point,
      config_generation: u64,
  }
  ```
- The `CompletedGesture` replaces the old in-hook `classify()` call. In `handle_right_up` on `UpResult::GestureComplete`:
  1. `add_force(release_point)` (endpoint force-capture — preserve existing fix)
  2. Copy buffer points into a `GestureCompletion` packet
  3. Clear buffer immediately
  4. Send `GestureCompletion` through an `std::sync::mpsc::sync_channel` (bounded, small capacity) to the recognition worker
  5. Return `LRESULT(1)`
- Spawn recognition worker thread in `spawn_hook_thread` (returns its `JoinHandle`). The worker:
  1. Receives `GestureCompletion` from the bounded channel
  2. Reconstructs a temporary `GestureBuffer` from the completion packet
  3. Runs `classify()` (RDP → quantize → collapse → match)
  4. Sends `HookEvent::GestureEnded` through the existing `event_tx` to the UI thread
  5. Posts `WM_APP + 2` to wake the UI thread
- Config generation check moves to the recognition worker: if `completion.config_generation != current_config.generation`, return `NoMatch`.
- The recognition worker holds its own `Arc<HookShared>` for shutdown coordination.

**Patterns to follow:**
- Existing `spawn_hook_thread` pattern: `thread::Builder::new().name("mouse-recognize").spawn(...)`.
- Existing `post_event` pattern for UI wake-up: `PostMessageW(ui_hwnd, WM_APP + 2, 0, 0)`.
- Existing config generation guard logic (preserved, moved from `handle_right_up` to recognition worker).

**Test scenarios:**
- Happy path: Complete a gesture, verify the recognition worker classifies it and the UI receives `HookEvent::GestureEnded { matched: true }`.
- Happy path: Complete a gesture with no match, verify `GestureEnded { matched: false }`.
- Edge case: Recognition worker channel full — the `sync_channel` should have capacity ≥ 1. If full, `try_send` returns error; the callback logs and discards (avoids blocking the hook). Capacity of 4 is conservative.
- Edge case: Config hot-reload between hook capture and recognition — config generation mismatch returns `NoMatch`.
- Edge case: Empty gesture buffer (point_count < 2) — recognition worker returns `TooShort`, no event posted.
- Performance: Hook callback for right-up must return in under 150 µs (measure by removing the old `classify()` call path — the new path only copies points into a stack buffer and does an mpsc send).

**Verification:**
- All existing gesture classification tests pass.
- `straight_right_gesture` and `right_then_down_gesture` produce the same results as before.
- Manual: draw a gesture and verify the action fires. Draw an unmatched gesture and verify no action fires.
- Hook callback latency: right-up no longer includes classification.

---

### U4. Replace Synchronous Policy Resolution with Atomic Snapshot

**Goal:** Remove `WindowFromPoint`, `GetWindowThreadProcessId`, and `resolve_pid` calls from `handle_right_down`. The hook callback performs only a read-only PID lookup against an atomically published `PolicySnapshot`.

**Requirements:** R3, R9

**Dependencies:** U1 (for the `WM_HOOK_COMMAND` wake path to update the snapshot when config changes)

**Files:**
- Modify: `src/app_policy.rs`
- Modify: `src/input_hook.rs`

**Approach:**
- Define `PolicySnapshot`:
  ```rust
  pub struct PolicySnapshot {
      pub mode: BlacklistMode,
      pub generation: u64,
      pub known_pids: HashMap<u32, Eligibility>,
  }
  pub enum Eligibility { Allowed, Denied }
  ```
- Store a `static POLICY_SNAPSHOT: AtomicPtr<PolicySnapshot>` (or use `Arc::as_ptr` + `AtomicPtr` for lock-free publication). Access pattern: `unsafe { &*POLICY_SNAPSHOT.load(Ordering::Acquire) }`.
- A `PolicyWorker` (new thread or function on the config watcher thread) builds new `PolicySnapshot` instances when:
  - Config is loaded/changed
  - Unknown PIDs are resolved asynchronously
- Publication: `POLICY_SNAPSHOT.store(Box::into_raw(new_snapshot), Ordering::Release)`. Old snapshot is leaked (acceptable — snapshots are infrequent).
- Hook callback in `handle_right_down`:
  1. `WindowFromPoint` + `GetWindowThreadProcessId` still needed (must resolve HWND+PID to check blacklist) — this is the minimum synchronous work.
  2. Read `POLICY_SNAPSHOT` atomically.
  3. `match snapshot.known_pids.get(&pid)` — read-only, no allocation.
  4. Unknown PIDs: apply default policy based on mode, enqueue PID for async resolution via a `Sender<u32>` to the policy worker.
- `resolve_pid` becomes async: called by the policy worker thread, builds a new `PolicySnapshot` with updated `known_pids`, atomically publishes.
- `prewarm_cache` is called by the policy worker to build an initial snapshot.

**Technical design:**
```
// Hook callback (read-only, no allocation):
let snapshot = unsafe { &*POLICY_SNAPSHOT.load(Ordering::Acquire) };
let eligibility = snapshot.known_pids.get(&target_pid)
    .copied()
    .unwrap_or_else(|| {
        // Enqueue for async resolution (fire-and-forget)
        let _ = policy_queue.send(target_pid);
        match snapshot.mode {
            BlacklistMode::Blacklist => Eligibility::Allowed,
            BlacklistMode::Whitelist => Eligibility::Denied,
        }
    });

if eligibility == Eligibility::Denied {
    return CallNextHookEx(...);
}
```

**Patterns to follow:**
- Existing `ConfigSnapshot` pattern: immutable, published once, shared via `Arc` (but here we use `AtomicPtr` for lock-free reads on the hook thread).

**Test scenarios:**
- Happy path: Known allowed PID → gesture capture proceeds. Known denied PID → passes through.
- Happy path: Unknown PID in blacklist mode → capture proceeds, PID queued for resolution.
- Happy path: Unknown PID in whitelist mode → passes through, PID queued for resolution.
- Edge case: Policy snapshot updated mid-gesture — the hook thread's current gesture continues with the old snapshot (snapshot is read at right-down only; stored in `GestureContext`).
- Edge case: Policy queue full → `send` fails; PID resolution is dropped (best-effort). No panic, no blocking.
- Edge case: `POLICY_SNAPSHOT` null (before first publication) — treat as empty snapshot (blacklist mode, all allowed).

**Verification:**
- Existing policy tests pass.
- Manual: add `chrome.exe` to blacklist apps, verify right-click passes through in Chrome.

---

### U5. Symmetrical Injected Event Filtering

**Goal:** Every hook event (right-down, right-up, move) must receive and respect the `LLMHF_INJECTED` flag. Foreign-injected events must not start, move, or terminate a physical gesture. Self-injected events must always pass through without interacting with gesture state.

**Requirements:** R4, R9

**Dependencies:** U1 (message loop changes touch the same callback functions)

**Files:**
- Modify: `src/input_hook.rs`

**Approach:**
- Add `InputOrigin` enum to `src/input_hook.rs` (or `src/state_machine.rs`):
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum InputOrigin {
      Physical,
      SelfInjected,
      ForeignInjected,
  }
  ```
- In `process_mouse_event`: compute `origin` from `hook_data.flags & LLMHF_INJECTED` and `hook_data.dwExtraInfo`:
  - `dwExtraInfo == SELF_TAG` → `SelfInjected`
  - `(flags & LLMHF_INJECTED) != 0` → `ForeignInjected`
  - Otherwise → `Physical`
- Pass `origin` to all three handlers: `handle_right_down(origin, ...)`, `handle_right_up(origin, ...)`, `handle_mouse_move(origin, ...)`.
- `handle_right_down` with `origin != Physical`: return `CallNextHookEx` immediately (do not enter state machine). Self-injected clicks pass through natively. Foreign-injected right-down does not start a gesture.
- `handle_right_up` with `origin != Physical`: if state machine is `Idle`, return `CallNextHookEx`. If state machine is Armed/Drawing (physical capture in progress), **ignore** the up event (return `LRESULT(1)` — suppress the foreign up so the physical right-up can still arrive).
- `handle_mouse_move` with `origin != Physical`: ignore — do not add points to buffer, do not trigger activation. Return `CallNextHookEx`.
- `StateMachine::on_right_down` no longer takes `is_injected: bool` — the origin check happens in the handler, not the state machine. The state machine only sees `Physical` events.
- `DownResult::Injected` variant is removed — the handler short-circuits before reaching `on_right_down`.
- Self-injected events: check `dwExtraInfo == SELF_TAG` before the origin check. Returns `CallNextHookEx` with no state interaction.

**Patterns to follow:**
- Existing self-tag check at the top of `process_mouse_event`.
- `DownResult` enum variants — remove `Injected`, add documentation that origin filtering happens in the handler.

**Test scenarios:**
- Happy path: Physical right-down starts Armed state, physical right-up completes gesture.
- Happy path: Self-injected right-down passes through (callback returns `CallNextHookEx`), state remains Idle.
- Happy path: Self-injected right-up passes through when state is Idle.
- Edge case: Physical right-down → Armed → foreign-injected right-up. State machine should NOT transition. Physical right-up later should still complete the gesture.
- Edge case: Physical right-down → Drawing → foreign-injected mouse move. Buffer should NOT receive the foreign point. Direction should be unchanged.
- Edge case: Foreign-injected right-down while already Drawing (physical). Should be ignored — physical capture continues.
- Integration: Synthetic click replay (P0.7 fix) injects self-tagged events → they pass through without re-entering the state machine.

**Verification:**
- Existing state machine tests pass.
- `injected_event_ignored` test updated to use `InputOrigin`.
- New test: `foreign_up_does_not_terminate_physical_capture`.
- New test: `foreign_move_does_not_contaminate_buffer`.

---

### U6. Remove Unconditional ForceReset Timer, Add State-Aware Watchdog

**Goal:** Delete the 5-second `WM_TIMER` → `HookCommand::ForceReset` path. Replace with hook-thread-internal watchdog that only resets state when the physical right button is no longer held (lost-release recovery) or on session lock/shutdown.

**Requirements:** R6, R7, R9

**Dependencies:** U1 (ForceReset command path changed; watchdog runs in the hook thread's command handler)

**Files:**
- Modify: `src/main.rs`
- Modify: `src/input_hook.rs`
- Modify: `src/state_machine.rs`

**Approach:**
- Remove `SetTimer(hwnd, TIMER_GESTURE_ID, TIMER_GESTURE_MS, None)` from `run()`.
- Remove `WM_TIMER` arm from `window_proc`.
- Remove `TIMER_GESTURE_ID` and `TIMER_GESTURE_MS` constants.
- In `StateMachine`, add tracking:
  ```rust
  physical_button_down: bool,      // true from right-down Consumed to right-up
  last_move_timestamp: Instant,    // for stale-gesture detection
  ```
- `physical_button_down` is set `true` in `on_right_down(Consumed)`, set `false` in `on_right_up(GestureComplete/ReplaySynthetic)`.
- `HookCommand::ForceReset` handler in the hook thread:
  - If `physical_button_down == false` → no-op (state is already clean).
  - If `physical_button_down == true` → check `last_move_timestamp`. If no move in > 30 seconds → force reset (synthesize recovery up, clear buffer). If recent moves → no-op (valid gesture in progress). **Emergency path only:** if the hook thread receives ForceReset during shutdown/session-lock, always force-reset regardless of state (the `WM_WTSSESSION_CHANGE` lock handler sends both `ForceReset` and `SetInterception(false)`).
- `StateMachine::force_reset()` remains but is now gated: only called when `physical_button_down == true` AND (shutdown/session-lock OR stale gesture).

**Technical design:**
```
HookCommand::ForceReset =>
    if sm.physical_button_down {
        let stale = sm.last_move_timestamp.elapsed() > Duration::from_secs(30);
        if stale || shared.shutdown.load(...) || !shared.interception_enabled.load(...) {
            sm.force_reset();
            buffer.clear();
        }
        // else: valid gesture in progress, ignore
    }
```

**Patterns to follow:**
- Existing `ForceReset` handler structure in input_hook.rs — modify, don't rewrite.
- Session-lock path in `main.rs` window_proc: keep `ForceReset` + `SetInterception(false)` sequence. The modified handler now correctly handles shutdown/lock scenarios while ignoring spurious resets.

**Test scenarios:**
- Happy path: Draw a slow gesture (5+ seconds) — gesture completes normally, no ForceReset interruption.
- Happy path: Right-down → right-up quickly (sub-threshold) → synthetic click replays. No ForceReset interference.
- Edge case: Lost button-up (physical button state unknown) → 30 seconds elapse → ForceReset cleans up state. Next right-click works.
- Edge case: Session lock during active gesture → ForceReset fires as part of lock sequence → state cleans up, interception disabled.
- Edge case: ForceReset during idle → no-op, no state corruption.
- Regression: Existing `force_reset_returns_to_idle` test continues to pass (the function itself is unchanged; only the calling condition changes).

**Verification:**
- Timer constants removed from `src/main.rs`.
- No `WM_TIMER` handler for gesture timeout.
- `cargo test` — 52 tests pass.
- Manual: draw a gesture lasting 10 seconds → completes normally. Lock Windows → daemon disables interception cleanly.

---

### U7. Move Click Replay to Dedicated Worker with Absolute Coordinates

**Goal:** Synthetic click replay must not run on the UI thread and must inject at the release coordinates using `MOUSEEVENTF_ABSOLUTE`. A high-priority replay worker receives `ClickReplay` packets and executes `SendInput` immediately.

**Requirements:** R8, R9

**Dependencies:** U1 (recognition worker integration), U5 (self-tag filtering for replay injection)

**Files:**
- Modify: `src/main.rs`
- Modify: `src/input_hook.rs` (the hook callback enqueues `ClickReplay` instead of posting `HookEvent::ReplaySyntheticClick`)

**Approach:**
- Define `ClickReplay` struct:
  ```rust
  struct ClickReplay {
      release_point: Point,
      target_hwnd: isize,      // HWND captured at right-down
      requested_at: Instant,
  }
  ```
- The hook callback on `UpResult::ReplaySynthetic` enqueues `ClickReplay` to a `crossbeam::channel::bounded(1)` or `std::sync::mpsc::sync_channel(1)` — a bounded channel with capacity 1, so old replays are dropped if a new one arrives before processing (no queue buildup).
- Remove the `HookEvent::ReplaySyntheticClick` variant entirely — click replay no longer goes through the hook-event channel.
- Spawn a replay worker thread:
  ```rust
  thread::Builder::new().name("mouse-replay").spawn(move || {
      for replay in replay_rx {
          inject_synthetic_click(replay.release_point);
      }
  });
  ```
- `inject_synthetic_click` updated:
  - Compute normalized absolute coordinates: `x_abs = (x * 65535) / screen_width`, `y_abs = (y * 65535) / screen_height`.
  - Set `MOUSEEVENTF_ABSOLUTE` flag.
  - `SendInput` for `RIGHTDOWN` + `RIGHTUP`.
  - Validate return count: if `SendInput` returns < 2, log warning.
- Self-tag: set `mi.dwExtraInfo = SELF_TAG` so the hook proc ignores the injected events (P0.5/U5 guards this).
- Remove `HookEvent::ReplaySyntheticClick` from the `HookEvent` enum and all match arms.

**Patterns to follow:**
- Existing `inject_synthetic_click` in `src/main.rs` — modify, don't rewrite.
- `SendInput` error handling pattern from `src/input_inject.rs`.
- Worker thread spawn pattern from existing `spawn_hook_thread` and the action worker.

**Test scenarios:**
- Happy path: Right-click without moving → synthetic click injected at release point. Context menu appears at cursor.
- Happy path: Synthetic click passes through hook without re-entering gesture state (self-tag check).
- Happy path: Multiple rapid right-clicks → each replay processed serially; no queue buildup (channel capacity 1, oldest dropped on overflow).
- Edge case: Cursor moves between hook capture and replay injection → absolute coordinates ensure injection at the original release point.
- Edge case: `SendInput` returns 0 (UIPI blocked) → log warning, no crash, no retry loop.
- Edge case: `SendInput` returns 1 (partial injection) → log warning. Context menu may not appear but daemon continues.

**Verification:**
- Remove all references to `HookEvent::ReplaySyntheticClick` from the codebase.
- `cargo test` — 52 tests pass (tests that use `HookEvent` enum match arms updated).
- Manual: right-click in any app without moving → context menu appears at click location. Move cursor quickly and right-click → context menu at release location, not at new cursor position.

---

## System-Wide Impact

- **Interaction graph:** The `HookEvent` enum loses `ReplaySyntheticClick`. The `dispatch_hook_event` function in `main.rs` no longer handles click replay. The recognition worker posts `HookEvent::GestureEnded` through the same `event_tx` channel (no change to the UI event processing contract). The hook callback now enqueues directly to the replay worker's channel instead of the `event_tx`.

- **Error propagation:** `SendInput` failures in the replay worker are logged at `warn` level. Recognition worker panics (should never happen — the classify function is pure and covered by tests) must not crash the daemon; wrap the worker in `catch_unwind`.

- **State lifecycle risks:** `PolicySnapshot` allocation via `Box::new()` and publication via `AtomicPtr::store` permanently leaks old snapshots. This is intentional — snapshots are infrequent (config changes, new PIDs), and freeing would require a grace period or epoch-based reclamation. Risk accepted.

- **API surface parity:** `spawn_hook_thread` return type changes from `Sender<HookCommand>` to `HookController`. All call sites in `src/main.rs` updated.

- **Unchanged invariants:** The `HookEvent` channel between hook thread and UI thread remains the same MPSC type and posting pattern. The `WM_APP + 2` UI wake message is unchanged. The action worker is unchanged. The overlay and tray are unchanged.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `PostThreadMessageW` from non-hook thread may fail if the hook thread's message queue is full or the thread exits. | Bounded command channel + error propagation via `HookController::send()`. The hook thread only exits after `WM_HOOK_SHUTDOWN`. |
| Recognition worker thread adds a fourth thread — increased memory footprint. | Worker is mostly idle (sub-5ms bursts per gesture). Stack size is default. Bounded channel avoids unbounded queue growth. |
| `AtomicPtr<PolicySnapshot>` requires `unsafe`. | Isolated to two functions: `publish_snapshot` and `load_snapshot`. Both are tiny and well-documented. Leak is intentional. |
| Deleting the ForceReset timer removes the only recovery path for truly lost button-up events. | Watchdog with 30-second staleness check retains recovery. Real lost-button-up events are rare on Windows 11. |
| `InputOrigin::SelfInjected` requires `dwExtraInfo == SELF_TAG` — if any external app injects with the same tag, it would be classified as self. | `SELF_TAG = 0xDAE0_0001` is high-entropy enough to be unique in practice. |

---

## Sources & References

- Advanced AI code review (7 P0 issues) — user-provided
- Microsoft Learn: `PostThreadMessageW`, `SendInput`, `WH_MOUSE_LL` callback timing guidelines
- WGestures reference: gesture buffer initialization at start position
- Commit `018292f`: endpoint force-capture pattern
- Existing plan docs: `docs/plans/2026-07-11-001-*`, `docs/plans/2026-07-11-002-*`
- Repository: `D:/Github/mouse-gesture`
