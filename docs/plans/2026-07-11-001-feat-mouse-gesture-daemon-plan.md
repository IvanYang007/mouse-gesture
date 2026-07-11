---
type: feat
status: active
created: 2026-07-11
origin: none
depth: deep
---

## Summary

A Windows system-tray daemon in Rust that intercepts right-click via `WH_MOUSE_LL`, recognizes 8-direction gesture patterns with exact matching, and dispatches window management, keyboard shortcuts, and app launches. Three-thread architecture (input hook → UI controller → action worker) with bounded queue communication, Per-Monitor V2 DPI awareness, and TOML-based hot-reloadable configuration. Delivery in five phases with mandatory feasibility spikes before architectural commitment. Visual overlay and fuzzy matching deferred to post-V1.

## Problem Frame

Power users on Windows 10/11 with multi-monitor mixed-DPI setups need fast, keyboard-free window management and shortcut triggering via mouse gestures. Existing tools (StrokesPlus, WGestures, AHK gesture scripts) vary in reliability, performance overhead, and DPI correctness. This daemon targets the intersection of: sub-5ms gesture recognition, sub-5MiB private working set, and robust application-aware input interception that never eats legitimate right-clicks.

---

## Requirements

| ID | Requirement |
|----|-------------|
| R1 | Hold right-click + draw gesture → recognize 8-direction pattern → dispatch action |
| R2 | Distinguish gesture from normal right-click via activation threshold (configurable, default 3 DIP) |
| R3 | Below-threshold: inject tagged synthetic right-click at release location |
| R4 | Matched gesture: consume right-click, dispatch action, provide visual feedback |
| R5 | Unmatched gesture: consume by default (no right-click replay); opt-in replay-at-release with known reliability caveats |
| R6 | App blacklist: disable interception in configured executables (case-insensitive basename match) |
| R7 | Window management: snap to half/quarter-screen, maximize/minimize/restore/close, move to monitor, toggle always-on-top, center |
| R8 | Keyboard shortcut injection via `SendInput` with modifier ordering and cleanup on partial failure |
| R9 | Application launching via `CreateProcessW` / `ShellExecuteExW` with optional focus-existing |
| R10 | TOML configuration with hot-reload via `ReadDirectoryChangesW`; invalid config retains last-known-good |
| R11 | System tray icon with Quit, Reload, Open Config, and status display |
| R12 | Per-Monitor V2 DPI awareness across mixed-resolution multi-monitor setups |
| R13 | Classifier CPU budget <250µs at max config; p99.9 hook-to-classification <5ms on defined test profile |
| R14 | Private working set <5MiB in idle and drawing states |
| R15 | 72-hour soak test with stable memory, handle, GDI, and USER-object counts |

---

## Key Technical Decisions

**K1. Right-click suppression with `consume` default for unmatched gestures.** The original product contract called for synthetic right-click replay on unmatched gestures. Replay via `SendInput` is "best effort" — it works on most apps but fails silently on some (e.g., raw-input consumers, elevated processes). Defaulting to `consume` avoids the worst failure mode (gesture that eats a legitimate right-click with no visible outcome). Replay remains available as opt-in for users who accept the reliability trade-off. *(see review finding: cross-reviewer consensus from ce-doc-review)*

**K2. Three-thread architecture with bounded queues.** Input hook thread owns `WH_MOUSE_LL` and the state machine with strict restrictions (no heap allocation, no filesystem access, no logging, no rendering). UI/controller thread owns the notification window, tray icon, overlay resources, and `ReadDirectoryChangesW`. Worker thread handles config parsing, process resolution, `EnumWindows`, and action dispatch. Communication via bounded SPSC/MPSC queues with wake-up messages prevents thread stalls.

**K3. Exact gesture matching only for V1.** Fuzzy weighted-edit-distance matching adds significant complexity (compile-time overlap detection, config rejection surface, dynamic-programming workspace at 256 gestures). Exact matching covers the 8-direction template use case; instrumentation on rejection rates during Phase 5 soak will inform whether fuzzy matching is needed. *(see review finding: adversarial P2)*

**K4. Visual trail overlay deferred to post-V1.** The GDI path/region-based overlay creates tension with the 5MiB memory contract (even a sparse approach allocates GDI resources proportional to monitor resolution). Core gesture recognition and action dispatch provide value without visual feedback — outcomes are observable (window moves, keys fire, apps launch). Tray icon status provides the essential daemon health signal. *(see review finding: adversarial P2, feasibility P2)*

**K5. Per-Monitor V2 DPI via manifest + `GetDpiForWindow`.** PMv2 manifest entry plus `SetProcessDpiAwarenessContext` before any HWND creation. All coordinate math in physical pixels; only configured DIPs (thresholds, trail thickness if enabled) converted via origin-monitor DPI.

**K6. Direct `windows` crate (0.58+) with minimal features.** No intermediary abstractions (no `winit`, no `tao`). The daemon owns its message loop and window classes directly. Feature flags limited to: `Win32_UI_WindowsAndMessaging`, `Win32_UI_Input_KeyboardAndMouse`, `Win32_UI_Shell`, `Win32_UI_HiDpi`, `Win32_Storage_FileSystem`, `Win32_System_Threading`, `Win32_Security`, `Win32_System_RemoteDesktop`, `Win32_Graphics_Gdi`.

---

## Output Structure

```
mouse-gesture/
├── Cargo.toml
├── build.rs                  # embed manifest (PMv2, execution level)
├── build.ps1                 # one-command release build
├── test.ps1                  # test runner with spike profiles
├── src/
│   ├── main.rs               # entry point, thread spawn, message pump dispatch
│   ├── config.rs             # TOML schema, parse, validate, compile to snapshot
│   ├── gesture.rs            # RDP simplification, 8-direction encode, exact matcher
│   ├── input_hook.rs         # WH_MOUSE_LL thread, hook proc, state machine
│   ├── state_machine.rs      # Idle/NativePass/Armed/Drawing state transitions
│   ├── app_policy.rs         # PID→policy cache, integrity check, app matching
│   ├── window_ops.rs         # monitor enum, tile/snap/min/max/close, DPI math
│   ├── input_inject.rs       # SendInput keyboard/mouse, modifier tracking
│   ├── launch.rs             # CreateProcessW, ShellExecuteExW, focus-existing
│   ├── tray.rs               # Shell_NotifyIconW, menu, TaskbarCreated re-register
│   ├── lifecycle.rs          # singleton mutex, WM_ENDSESSION, lock/unlock, logs
│   └── win_handles.rs        # unsafe Send/Sync wrappers for HHOOK, HANDLE
└── tests/
    ├── gesture_tests.rs      # unit + property tests for recognizer
    ├── config_tests.rs       # parse/validate/compile tests
    └── integration_tests.rs  # window ops coordinate math, injection smoke
```

---

## Implementation Units

### U1. Project scaffold and configuration system

- **Goal:** Establish the Rust project, `windows` crate dependencies, build manifest, TOML config schema, parse/validate/compile pipeline, and module skeleton.
- **Requirements:** R10
- **Dependencies:** None
- **Files:**
  - `Cargo.toml` — create with `windows` 0.58+ minimal features, `toml`, `anyhow`, `log`
  - `build.rs` — create, embed PMv2 DPI manifest + `asInvoker` execution level via `embed-manifest` or inline XML
  - `build.ps1` — create, `cargo build --release`
  - `test.ps1` — create, `cargo test` + spike profiles
  - `src/config.rs` — create, TOML schema: `[settings]` (activation_threshold_dip, trail_color, trail_opacity), `[blacklist]` (apps, mode: blacklist/whitelist), `[gestures.<name>]` (pattern, action type enum), `[keys]` (precompiled name→VK/scan map)
  - `src/main.rs` — create, thread spawn skeleton, panic hooks, `catch_unwind` wrappers
- **Approach:**
  - Config uses `serde` for TOML deserialization into typed structs
  - Validation runs on worker thread: check gesture count ≤256, token count ≤32, no duplicate exact patterns, keys resolve to valid `VIRTUAL_KEY` values
  - Validated config compiled into immutable `ConfigSnapshot` (gesture patterns as `Vec<Direction>`, keys as precomputed `Vec<INPUT>`, process filters as `HashSet<String>`)
  - Initial config load failure → daemon starts with interception disabled; tray icon shows error state
  - `build.rs` uses `embed_manifest` crate or direct XML string for PMv2 manifest
- **Patterns to follow:** Rust `windows` crate samples; `serde` derive patterns; manifest embedding from `winit`/`tao` crates
- **Test scenarios:**
  - Parse minimal valid config → all fields populated with defaults
  - Parse config with 257 gestures → rejected with clear error
  - Duplicate exact pattern → rejected at compile time
  - Invalid key name → rejected with error naming the offending key
  - Config with fuzzy overlap warnings → compiles with warnings logged
  - Empty config (no gestures) → parses successfully, daemon starts with passive mode
  - Missing config file → daemon starts with interception disabled, tray shows error
- **Verification:** `cargo build` succeeds; config parse/validate unit tests pass; manifest inspection shows PMv2 + asInvoker

---

### U2. Gesture recognition engine

- **Goal:** Implement the 8-direction gesture recognizer: spatial coalescing, Ramer-Douglas-Peucker simplification, direction quantization, exact pattern matching.
- **Requirements:** R1, R13
- **Dependencies:** U1 (config types)
- **Files:**
  - `src/gesture.rs` — create, recognition pipeline
  - `tests/gesture_tests.rs` — create, unit + property + fuzz tests
- **Approach:**
  - **Spatial coalescing:** Incoming `(x, y)` points from the hook sampled at configurable distance (default ~2 physical px). Fixed 256-point buffer; on overflow, adaptive decimate (keep every Nth point) or mark unmatched.
  - **RDP simplification:** Iterative fixed-buffer Ramer-Douglas-Peucker using squared Euclidean distances. Epsilon from config (default ~2 DIP, converted to physical px via origin monitor DPI). In-place algorithm, no heap allocation.
  - **Direction quantization:** For each consecutive simplified-point pair, compute angle via `atan2(dy, dx)`, discretize into one of 8 directions: N (67.5°–112.5°), NE (22.5°–67.5°), E (−22.5°–22.5°), SE (−67.5°–−22.5°), S (−112.5°–−67.5°), SW (−157.5°–−112.5°), W (157.5°–180° and −180°–−157.5°), NW (112.5°–157.5°). Angular hysteresis: ±11.25° dead zones at boundaries.
  - **Collapse:** Adjacent identical directions merged (N,N,N → N).
  - **Exact matching:** Compare collapsed direction sequence against each compiled gesture pattern. Return first match; no match → `None`. No fuzzy matching in V1.
  - **Minimum gesture length:** Configurable (default 2 tokens). Single-token gestures (e.g., just "E") supported for snap actions but flagged as potentially ambiguous in docs.
  - **Performance:** Entire pipeline (coalesce → RDP → quantize → collapse → match) must complete within the 250µs CPU budget at max config (256 points, 32 tokens, 256 gestures). Benchmarked in isolation before integration.
- **Patterns to follow:** Standard computational geometry; `$1 recognizer` literature for direction encoding
- **Test scenarios:**
  - Straight horizontal rightward gesture → "E" token, matches exact pattern
  - Diagonal gesture → "SE" token
  - Complex gesture (right, down, left) → "E S W" tokens
  - Near-threshold jitter points → RDP filters noise, produces clean direction sequence
  - 300 points exceeding buffer → adaptive decimate or unmatched
  - Zero-length gesture (no movement) → empty token list, unmatched
  - Single-point gesture → empty after collapse, unmatched
  - Property test: round-trip (generate direction sequence → synthesize points → recognize → original sequence preserved)
  - Fuzz test: random (x, y) point clouds, no panic, bounded output
  - Bench: 256 points, 256 gestures, 32 tokens → <250µs on test machine
- **Verification:** All unit tests pass; benchmark meets budget; fuzz runs 10k iterations without panic

---

### U3. Input hook and mouse state machine

- **Goal:** Set up `WH_MOUSE_LL` on dedicated thread, implement the four-state mouse machine (Idle/NativePass/Armed/Drawing), and handle tagged synthetic click replay.
- **Requirements:** R1, R2, R3, R4, R5
- **Dependencies:** U2 (gesture recognizer), U1 (config snapshot)
- **Files:**
  - `src/input_hook.rs` — create, hook thread spawn, hook proc, event filtering, gesture buffer management
  - `src/state_machine.rs` — create, state enum, transition logic, tagged replay scheduling
  - `src/win_handles.rs` — create, `Send` wrapper for `HHOOK`
- **Approach:**
  - **Hook thread:** Spawn dedicated thread. Call `SetWindowsHookExW(WH_MOUSE_LL, hook_proc, HINSTANCE::default(), 0)` for system-wide hook. Enter `GetMessage` loop — required for low-level hooks even without a window.
  - **`HHOOK` is `!Send` in the `windows` crate.** Wrap in a newtype with `unsafe impl Send`; the hook is only ever accessed from its owning thread. Store in thread-local or owned by the hook thread exclusively.
  - **State machine:**

    | State | Right-Down | Movement | Right-Up |
    |-------|-----------|----------|----------|
    | Idle | Resolve target HWND/PID. If eligible: snapshot (HWND, PID, foreground, point, monitor, DPI, config gen), enter **Armed**, suppress down. If excluded/unknown: enter **NativePass**, call next hook. | N/A | N/A |
    | NativePass | N/A | Allow through | Pass through natively, return to **Idle** |
    | Armed | N/A | Track movement. On leaving activation rect: enter **Drawing**, store coalesced points in fixed buffer | Suppress, schedule tagged synthetic right-down/right-up, return to **Idle** |
    | Drawing | N/A | Store coalesced points | Suppress, classify. Match → queue action. No match → consume. Return to **Idle** |

  - **Injected event detection:** Check `MSLLHOOKSTRUCT.flags & LLMHF_INJECTED` (bit 0). Ignore injected events for gesture purposes.
  - **Self-tagging:** All synthetic events injected by the daemon carry a unique `dwExtraInfo` marker (e.g., `0xDAE0_0001`). Hook proc ignores events with this marker.
  - **Tagged replay (Armed → below-threshold):** The input thread schedules synthetic right-down + right-up via `SendInput` on the **same thread** (allowed from hook context for mouse events). Inject with the daemon's `dwExtraInfo` marker, then reset state.
  - **Maximum gesture timer:** 5-second timer. If button-up never arrives (lost event, desktop switch, session lock), reset to Idle and consume the suppressed down. Timer uses `SetTimer` on the notification window, not a thread sleep.
  - **Hook restrictions enforced:** No heap allocation in hook proc (all buffers pre-allocated). No filesystem, no logging, no mutexes, no rendering. Move processing only updates bounded state and publishes coalesced points via atomic flag.
- **Patterns to follow:** `willhook-rs` crate structure; `enigo` for `dwExtraInfo` self-tagging pattern; `MouserInRust` for `Send` wrapper on HHOOK
- **Test scenarios:**
  - Right-click on eligible app → down suppressed, state Armed
  - Move <3px then release in Armed → synthetic right-click injected at release point
  - Move >3px then release in Drawing → gesture classified, right-click consumed
  - Right-click on blacklisted app → NativePass, right-click passes through natively
  - Right-click on unknown PID → NativePass (cache miss, worker resolves async)
  - Injected event (LLMHF_INJECTED) → ignored for gesture activation
  - Self-injected replay → filtered by dwExtraInfo, no infinite loop
  - Lost button-up (timer fires) → state resets to Idle
  - Desktop switch mid-gesture → timer fires, state resets
- **Verification:** State machine unit tests cover all transitions; synthetic replay smoke test on Explorer context menu

---

### U4. Application policy and action targeting

- **Goal:** Cache application identity (exe path) and integrity level outside the hook; prewarm cache from visible windows; snap policy for each gesture; verify target validity before action dispatch.
- **Requirements:** R6
- **Dependencies:** U3 (state machine uses policy cache), U1 (config process filters)
- **Files:**
  - `src/app_policy.rs` — create, cache, integrity check, app matching, pre-action validation
- **Approach:**
  - **PID→Policy lookup:** Input thread's fixed-size `HashMap<u32, PolicyEntry>`. Entry contains: executable basename (`String`), integrity level (`u32`, Medium = 0x2000), eligibility (`bool`), resolution state (`Known`/`Pending`).
  - **Cache prewarm:** On startup, worker thread calls `EnumWindows` to enumerate visible top-level windows. For each: `GetWindowThreadProcessId` → `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` → `QueryFullProcessImageNameW` → extract basename → match against config filters → insert into policy cache. Cache sent to input thread as part of config snapshot.
  - **Unknown entries** (cache miss during hook): Input thread treats as ineligible (NativePass). Worker thread resolves asynchronously and sends update via `UI→input` queue.
  - **Integrity check:** `OpenProcessToken` + `GetTokenInformation(TokenIntegrityLevel)`. Only apps at same or lower integrity are eligible for interception. Elevated apps automatically excluded.
  - **Case-insensitive basename matching** against config's `[blacklist].apps` list. Mode: `blacklist` (gestures disabled in listed apps) or `whitelist` (gestures enabled only in listed apps). Default: blacklist mode, empty list (all apps eligible).
  - **Optional full-path matching:** Configurable per-app entry. Match against full executable path for disambiguation (e.g., two different `python.exe` instances).
  - **Policy snapshot for entire gesture:** Config generation number captured at right-button-down. Hot-reload cannot change policy mid-gesture.
  - **Pre-action validation** (worker thread, before dispatch):
    1. `IsWindow(hwnd)` — HWND still exists
    2. `GetWindowThreadProcessId(hwnd)` → PID matches snapshot
    3. For keyboard actions: `GetForegroundWindow()` matches expected foreground HWND from snapshot. If not, abort with feedback.
- **Patterns to follow:** `sysinfo` crate's process enumeration approach; `nt_token` crate for integrity level
- **Test scenarios:**
  - Known eligible app → policy returns Eligible
  - Blacklisted app → policy returns Excluded
  - Unknown PID → policy returns Pending (input thread treats as ineligible)
  - Elevated app → integrity check returns Excluded (cannot intercept)
  - HWND destroyed before action → pre-action validation fails, action aborted
  - Foreground changed during gesture (keyboard action) → abort, no Ctrl+W to wrong window
  - Case-insensitive match: "Notepad.exe" matches "notepad.exe"
  - Config reload mid-gesture → policy unchanged (snapshot from config gen at button-down)
- **Verification:** Unit tests for cache resolution and matching; integration test for pre-action validation with real windows

---

### U5. Window operations with DPI awareness

- **Goal:** Implement window tiling (half/quarter-screen snap), state operations (maximize/minimize/restore/close), monitor-aware positioning, and Per-Monitor V2 DPI coordinate conversions.
- **Requirements:** R7, R12
- **Dependencies:** U1 (config types for monitor selectors)
- **Files:**
  - `src/window_ops.rs` — create, monitor enumeration, tile calculations, window state operations, DPI conversion utilities
- **Approach:**
  - **PMv2 initialization:** `SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)` called in `main()` before any HWND creation.
  - **Coordinate discipline:** All internal calculations in physical screen coordinates. Only configured DIPs (thresholds from config) converted to physical via `GetDpiForWindow(daemon_owned_window_on_target_monitor)`. `GetDpiForMonitor` as compatibility fallback.
  - **Monitor enumeration:** `EnumDisplayMonitors` → `GetMonitorInfoW` → collect `MONITORINFOEXW` structures. Deterministic ordering: left-to-right, top-to-bottom by `rcMonitor` origin.
  - **Monitor selectors:**
    - `primary` — monitor at (0,0) in virtual screen space
    - `next` / `previous` — cycle through current-topology ordering from current monitor
    - `1`, `2`, ... — zero-based index in topology order
    - `\\.\DISPLAY1` — device name match
  - **Tile calculations:** Use `MONITORINFO.rcWork` (excludes taskbar). Calculate target rects:
    - Half: split `rcWork` vertically or horizontally at midpoint
    - Quarter: split each half into top/bottom
    - Center: window sized to 80% of `rcWork`, centered
  - **Invisible border compensation:** `DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS)` to get actual visible rect. Difference from `GetWindowRect` is the invisible border — add to target rect dimensions.
  - **Window operations:**
    - Snap/tile: `SetWindowPos(hwnd, HWND_TOP, x, y, w, h, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE)`
    - Maximize: `ShowWindowAsync(hwnd, SW_MAXIMIZE)`
    - Minimize: `ShowWindowAsync(hwnd, SW_MINIMIZE)`
    - Restore: `ShowWindowAsync(hwnd, SW_RESTORE)`
    - Close: `PostMessageW(hwnd, WM_CLOSE, 0, 0)`
    - Always-on-top toggle: `SetWindowPos(hwnd, HWND_TOPMOST/HWND_NOTOPMOST, ...)`
  - **Revalidate after async ops:** After `SWP_ASYNCWINDOWPOS` or `ShowWindowAsync`, the target window may have been destroyed or reparented. Do not assume the HWND is valid for subsequent operations without rechecking.
  - **Min/max tracking:** Let target applications enforce their own constraints — do not clamp rects beyond `GetWindowPlacement`'s `ptMinTrackSize`/`ptMaxTrackSize`.
- **Patterns to follow:** `komorebi` tiling WM's window operations module; `Seelen-UI` DPI handling
- **Test scenarios:**
  - Snap window to right half of 1920×1080 monitor (100% DPI) → rect: (960, 0, 960, 1040 assuming 40px taskbar)
  - Same on 4K monitor at 150% DPI → physical coordinates: (2560, 0, 2560, ...) using rcWork in physical px
  - Move window to monitor 2 → `SetWindowPos` with monitor 2's rcWork-derived rect
  - Maximize via ShowWindowAsync → window maximizes, test with real Notepad window
  - Close via PostMessageW(WM_CLOSE) → window receives close message
  - Window with extended frame bounds → invisible border compensated in target size
  - Target window destroyed between tile calculation and SetWindowPos → operation fails gracefully, no crash
  - Monitor removed mid-operation → DPI query falls back to primary monitor
- **Verification:** Unit tests for rect math and DPI conversion; integration tests with real windows on multi-monitor setup

---

### U6. Input injection and application launching

- **Goal:** Implement `SendInput`-based keyboard shortcut injection with modifier ordering/cleanup, and application launching with focus-existing logic.
- **Requirements:** R8, R9
- **Dependencies:** U5 (window ops for focus-existing), U1 (config precompiled key definitions)
- **Files:**
  - `src/input_inject.rs` — create, keyboard injection, modifier tracking, cleanup
  - `src/launch.rs` — create, CreateProcessW, ShellExecuteExW, focus-existing search
- **Approach:**
  - **Keyboard injection pipeline:**
    1. Resolve configured key name to `(VIRTUAL_KEY, u16 scan_code, bool is_extended)` from precompiled map
    2. Build `Vec<INPUT>`: press modifiers in order (Ctrl→Alt→Shift→Win, each with `KEYEVENTF_SCANCODE`), press main key, release main key, release modifiers in reverse order
    3. `KEYEVENTF_EXTENDEDKEY` set for extended keys (arrows, Ins, Del, Home, End, PgUp, PgDn, NumPad Enter, right-side modifiers)
    4. Call `SendInput(&inputs, size_of::<INPUT>() as i32)`
    5. Track which keys were successfully injected (by `SendInput` return count). If partial failure, send cleanup releases for injected keys. Log the failure.
  - **Modifier conflict detection:** Before injecting, check if physically-held modifiers conflict with the target shortcut. Use `GetAsyncKeyState` for each modifier VK. If any modifier in the shortcut is already physically held by the user AND not part of the shortcut, reject the shortcut with feedback. Safest default: reject when any non-matching modifier is down.
  - **Self-tagging:** All injected `INPUT` structs carry `dwExtraInfo = SELF_TAG` so the hook proc ignores them.
  - **No keyboard hook.** The daemon does not install `WH_KEYBOARD_LL`. Modifier detection via `GetAsyncKeyState` is sufficient for pre-injection conflict checking, even if it has known edge cases with `WH_KEYBOARD_LL` ordering.
  - **Application launching:**
    - Executable with args: `CreateProcessW(lpApplicationName, lpCommandLine, ...)` with `CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP` for background launch
    - URLs, folders, documents: `ShellExecuteExW` with `SEE_MASK_FLAG_NO_UI` on COM-initialized thread
    - Application aliases (e.g., `wt.exe`): resolved via `ShellExecuteExW` which handles PATH lookup
  - **Focus-existing (best effort):**
    1. `EnumWindows` → filter: visible (`IsWindowVisible`), non-cloaked, top-level, PID matches target
    2. Prefer `GetGUIThreadInfo` to find most recently active candidate
    3. `ShowWindowAsync(hwnd, SW_RESTORE)` if minimized
    4. `SetForegroundWindow(hwnd)` — may fail due to Windows foreground lock
    5. On failure: `FlashWindowEx` to signal user attention; log the failure
    6. Document prominently: "focus-existing is best effort; Windows may prevent foreground activation"
  - **Action feedback:** On action completion (or failure), worker sends result to UI thread for tray/overlay feedback.
- **Patterns to follow:** `enigo` keyboard injection implementation; `rustdesk` `dwExtraInfo` self-tagging
- **Test scenarios:**
  - Inject Ctrl+C → modifier press order correct, keys released in reverse, no stuck modifiers
  - Inject extended key (Right arrow, KEYEVENTF_EXTENDEDKEY) → flag set on key down and up
  - User holds physical Shift, gesture requests Ctrl+T → no conflict, injection proceeds
  - User holds physical Shift, gesture requests Shift+W → conflict, injection rejected
  - Partial SendInput failure → cleanup releases sent for successfully injected keys
  - Launch notepad.exe via CreateProcessW → process starts
  - Open https://example.com via ShellExecuteExW → default browser opens
  - Focus existing notepad window → window brought to foreground (or flashes if foreground lock active)
  - Focus existing with no matching windows → graceful no-op, logged
- **Verification:** Injection smoke tests with real key combos (Ctrl+W in Notepad); launch tests with known executables; focus-existing integration test

---

### U7. System tray, config management, and lifecycle

- **Goal:** Implement system tray icon with menu, config hot-reload via `ReadDirectoryChangesW`, and session/lifecycle handling.
- **Requirements:** R10, R11, R15
- **Dependencies:** U1 (config), U3 (input thread needs config snapshots), U4 (policy cache updates)
- **Files:**
  - `src/tray.rs` — create, Shell_NotifyIconW wrapper, menu handling, icon state management
  - `src/lifecycle.rs` — create, singleton mutex, session events, WM_ENDSESSION, bounded logging, graceful shutdown
- **Approach:**
  - **Tray icon:** Use `Shell_NotifyIconW` directly from the `windows` crate (no `notify-icon` dependency — the API is simple enough).
    - `NIM_ADD` with `NOTIFYICONDATAW`: `uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP`, `uCallbackMessage = WM_APP + 1`, `hIcon` loaded from embedded resource or `LoadIconW` with `IDI_APPLICATION` as fallback
    - Menu on `WM_RBUTTONUP`: Create popup menu via `CreatePopupMenu` + `AppendMenuW` with items: "Status: Active (N gestures loaded)", separator, "Open Config", "Reload Config", separator, "Quit"
    - "Show disabled/error status": icon variant (grayed or with red overlay), tooltip updated with error message
    - `TaskbarCreated` message: re-register tray icon after Explorer restart (register `TaskbarCreated` via `RegisterWindowMessageW`)
  - **Config watching:** `CreateFileW` on config directory with `FILE_FLAG_OVERLAPPED | FILE_FLAG_BACKUP_SEMANTICS`. `ReadDirectoryChangesW` with `FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_LAST_WRITE`. Watch the **directory**, not the file (editors save via rename).
    - Debounce: 200ms after last notification before re-reading
    - Buffer overflow: re-read file unconditionally
    - Parse + validate on worker thread. On valid reload: compile new snapshot, send to UI thread, UI pushes to input thread between gestures. On invalid: retain last-known-good, update tray to error state with parse error in tooltip.
  - **Singleton mutex:** `CreateMutexW` with name `Global\MouseGestureDaemon_Singleton`. If `GetLastError() == ERROR_ALREADY_EXISTS`, exit.
  - **Session handling:** `WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)`. On `WM_WTSSESSION_CHANGE`:
    - `WTS_SESSION_LOCK` / `WTS_CONSOLE_DISCONNECT`: disable interception, update tray
    - `WTS_SESSION_UNLOCK` / `WTS_CONSOLE_CONNECT`: re-enable interception
  - **Shutdown:** Handle `WM_QUERYENDSESSION` (return `TRUE` to allow shutdown). `WM_ENDSESSION`: trigger graceful shutdown. `WM_DISPLAYCHANGE` / `WM_SETTINGCHANGE`: re-enumerate monitors, push updated topology to window_ops.
  - **Desktop switch:** `SetWinEventHook(EVENT_SYSTEM_DESKTOPSWITCH, ...)` or check desktop name on timer reset events. Reset gesture state machine on desktop switch.
  - **Bounded rotating logs:** Log to `%LOCALAPPDATA%\mouse-gesture\logs\`. Rotate: keep last 5 files of ≤1MB each. Log level configurable. Log format: `[timestamp] [level] [module] message`.
  - **Graceful shutdown:** `Drop` implementations release hook (`UnhookWindowsHookEx`), tray icon (`NIM_DELETE`), mutex (`CloseHandle`). Thread join with timeout (2s), then detach.
- **Patterns to follow:** `tiennm99/claude-code-usage-bubble` tray implementation; `notify-rs` `ReadDirectoryChangesW` overlay pattern; `winit` session handling
- **Test scenarios:**
  - Tray icon appears on startup, responds to right-click menu
  - "Open Config" → ShellExecute on config.toml, opens in default editor
  - "Reload Config" → valid reload updates gesture set within 200ms
  - Edit config file externally (save via rename) → config reloads within debounce window
  - Edit config with syntax error → retains last-known-good, tray shows error tooltip
  - Second instance launched → exits immediately (singleton mutex)
  - Session lock → interception disabled, tray updated
  - Session unlock → interception re-enabled
  - System shutdown → WM_QUERYENDSESSION returns TRUE, graceful cleanup
  - Explorer restart → tray icon re-registers on TaskbarCreated
  - Display hot-plug → monitor list updated
- **Verification:** Manual tray interaction test; config reload cycle test; session lock/unlock cycle; 72-hour soak log inspection

---

### U8. Testing and reliability qualification

- **Goal:** Implement the Phase 0 feasibility spikes (input replay, overlay memory, latency), unit/integration test suites, and the Phase 5 72-hour soak qualification.
- **Requirements:** R13, R14, R15
- **Dependencies:** U3–U7 (all features implemented)
- **Files:**
  - `tests/integration_tests.rs` — create, full-pipeline tests
  - `test.ps1` — update with spike profiles and soak runner
- **Approach:**
  - **Phase 0 feasibility spikes** (run after U3, before U5–U7 commitment):
    1. **Input replay spike:** Suppress and reinject right-clicks via `SendInput` on Explorer, Chromium, Electron (VS Code), Office, Windows Terminal, IntelliJ, Java UI (Swing), elevated Notepad, RDP session, and a raw-input test app. Measure: percentage of apps where replay produces correct native right-click behavior. Gate: ≥90% of test apps pass replay; if not, switch default to `consume` only.
    2. **Overlay memory spike (deferred post-V1):** If overlay is enabled, measure private working set, total working set, GDI handles, and USER handles on 4K@150% and 1920×1080@100% dual-monitor setups. Test long diagonal gesture and rapid scribble gesture. Gate: private working set <5MiB during drawing. This spike is **deferred** per K4.
    3. **Latency spike:** Inject high-rate mouse events (1000Hz polling mouse, rapid movement). Measure hook callback wall time (target <1ms p99) and classifier CPU time (target <250µs). Run under CPU stress (background compilation). Gate: p99.9 hook-to-classification <5ms.
  - **Unit tests:** Every module has unit tests covering happy path, edge cases, and error paths (see per-unit test scenarios).
  - **Integration tests:**
    - Full gesture pipeline: simulated right-click events → state machine → recognizer → action dispatch
    - DPI coordinate math across mixed-resolution monitors (simulated via test DPI values)
    - Config reload cycle: write → detect → parse → snapshot → push to input thread
    - Safety nets: hook panic → catch_unwind → disable interception → call next hook
  - **Phase 5 reliability qualification (72-hour soak):**
    - Run daemon continuously for 72 hours
    - High-rate input stream: 1 gesture every 2 seconds for 1 hour
    - Repeated config replacement: reload config 100× with valid and invalid files
    - Display hot-plug: connect/disconnect monitor 20×
    - Session lock/unlock: 50 cycles
    - Explorer restart: 10 cycles
    - Hung and exiting target windows: start action, kill target mid-operation
    - Metrics tracked: memory (private bytes, working set, GDI/USER handles), crash count, log errors
    - Pass criteria: memory stable (±10% baseline), handles stable (no leaks >5 handles/hour), zero crashes
- **Patterns to follow:** Standard Rust test organization; property-based testing with `proptest` for gesture recognizer
- **Test scenarios:** (see per-unit test scenarios above; integration and soak tests cover cross-module behavior)
- **Verification:** All unit tests pass; integration tests pass on multi-monitor setup; spike gates met; 72-hour soak passes all criteria

---

## Scope Boundaries

### Deferred for later (post-V1)

- **Visual trail overlay** — GDI path/region-based transparent overlay rendering gesture trails. Deferred to de-risk the 5MiB memory contract. Core gesture recognition works without visual feedback; tray icon provides daemon health signal. *(K4)*
- **Fuzzy gesture matching** — Weighted edit-distance matching with direction adjacency costs. Deferred until exact-match rejection-rate data from production use justifies the complexity. *(K3)*
- **Accessibility tool compatibility** — Explicit testing with screen readers, sticky keys, mouse keys. Daemon does not register as an accessibility tool. *(see review finding: design-lens P1)*
- **ARM64 build target** — x64-only for V1.
- **Installer / auto-start** — Manual unzip-and-run for V1.
- **Overlay trail appearance customization** — Color, opacity, thickness configurable post-V1 when overlay is enabled.

### Deferred to Follow-Up Work

- **`notify-icon` crate dependency evaluation** — For V1, direct `Shell_NotifyIconW` calls suffice. Evaluate `notify-icon` crate for V2 if tray complexity increases.
- **`enigo` crate dependency evaluation** — For V1, direct `SendInput` calls suffice. Evaluate `enigo` for cross-platform abstractions if needed.

### Outside this product's identity

- UAC secure desktop gesture support
- Elevated/administrative process interception
- Raw-input application support
- Multi-user / terminal services support
- Cross-platform (macOS/Linux) gesture recognition
- Touch/trackpad gesture recognition
- Macro recording or replay

---

## Dependencies / Prerequisites

- **Rust toolchain:** stable 1.80+ with `x86_64-pc-windows-msvc` target
- **`windows` crate:** 0.58+ with minimal feature flags
- **Test machine:** Windows 10/11, ≥2 monitors with at least one mixed-DPI pair (e.g., 4K@150% + 1080p@100%)
- **Build tools:** `embed-manifest` crate or inline XML for PMv2 manifest

---

## Risk Analysis & Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| `SendInput` replay unreliable for unmatched gestures | Medium | High | Default to `consume`; replay opt-in with documented caveats. Phase 0 input replay spike gates architectural commitment. |
| `WH_MOUSE_LL` hook chain interference from other hooks | Medium | High | `catch_unwind` in hook callback disables interception on panic. Timer recovery for lost events. Test with known hook-heavy apps (gaming overlays, macro recorders). |
| 5MiB private working set exceeded under all conditions | Low | High | Phase 0 memory spike gates commitment. Worst-case memory measured on 4K@150%. Deferred overlay removes largest variable. |
| `SetForegroundWindow` silently fails for focus-existing | High | Low | Documented as best-effort. FlashWindowEx fallback provides visual signal. P99.9 latency unaffected since action dispatch is async. |
| Mixed-DPI coordinate errors cause mispositioned windows | Medium | Medium | PMv2 manifest + physical-coordinate discipline. Integration tests cover DPI conversion math. Manual testing on real mixed-DPI setup required. |
| Config reload race condition with active gesture | Low | Low | Config snapshot applied between gestures only. Generation number captured at button-down. Explicit guard in state machine prevents mid-gesture reload. |

---

## System-Wide Impact

- **Input pipeline:** All mouse right-click events pass through `WH_MOUSE_LL` hook chain. 0µs added for non-gesture eligible apps (NativePass). ~10µs added for eligible apps (PID lookup + state machine check). Hook callback designed to return in <50µs for move events.
- **Window management:** `SetWindowPos` and `ShowWindowAsync` calls affect target application windows. No global window hooks installed. No subclassing of foreign windows.
- **System tray:** One icon in notification area. Right-click menu with 4 items. TaskbarCreated re-registration on Explorer restart.
- **Filesystem:** Config file read on startup and on directory change. Logs written to `%LOCALAPPDATA%\mouse-gesture\logs\`. No other filesystem access.
- **Network:** None.
- **GPU:** No GPU allocation in V1 (overlay deferred). If overlay enabled post-V1, GDI path/region rendering uses CPU rasterization.

---

## Phased Delivery

### Phase 0: Feasibility Spikes (gate before Phase 1)
- Input replay spike: test SendInput right-click replay on 10 app categories
- Overlay memory spike: deferred per K4
- Latency spike: measure hook callback and classifier times under CPU stress
- Gate: if any spike invalidates the product contract, revise contract before coding

### Phase 1: Core (U1, U2)
- Project scaffold, config system → gesture recognition engine
- Verification: `cargo build`, unit tests pass, benchmark meets 250µs budget

### Phase 2: Input Engine (U3, U4)
- Hook thread, state machine, app policy cache, synthetic replay
- Verification: state machine transitions tested, replay smoke test on Explorer

### Phase 3: Actions and DPI (U5, U6)
- Window operations, keyboard injection, app launching, focus-existing
- Verification: manual tile on multi-monitor, Ctrl+W injection in Notepad

### Phase 4: Tray and Lifecycle (U7)
- Tray icon, config hot-reload, session handling, logging, graceful shutdown
- Verification: config reload cycle, lock/unlock cycle, 24-hour preliminary soak

### Phase 5: Reliability Qualification (U8)
- 72-hour soak, spike measurements, integration tests, crash recovery tests
- Verification: all metrics stable, zero crashes, spikes meet gates
