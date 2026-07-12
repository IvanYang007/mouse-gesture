//! Input hook thread — owns WH_MOUSE_LL, the message pump,
//! and the gesture buffer.

use crate::config::ConfigSnapshot;
use crate::config::Direction;
use crate::gesture::{GestureBuffer, GestureResult, Point, classify, direction_from_points, MAX_POINTS};
use crate::state_machine::{DownResult, GestureContext, StateMachine, UpResult};
pub use crate::state_machine::SELF_TAG;
use crate::app_policy::{self, Eligibility};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx,
    HOOKPROC, MSG, MSLLHOOKSTRUCT, GetForegroundWindow,
    WH_MOUSE_LL, WM_MOUSEMOVE, WM_USER,
    WM_RBUTTONDOWN, WM_RBUTTONUP, PM_NOREMOVE,
    LLMHF_INJECTED,
};

/// Custom messages posted to the hook thread's message queue.
pub const WM_HOOK_COMMAND: u32 = WM_USER + 1;
pub const WM_HOOK_SHUTDOWN: u32 = WM_USER + 2;

/// Event sent from the hook thread to the UI thread.
#[derive(Debug, Clone)]
pub enum HookEvent {
    GestureStarted { x: i32, y: i32, monitor: isize },
    TrailPoint { x: i32, y: i32 },
    GestureEnded { matched: bool, gesture_name: Option<String> },
    DirectionChanged { direction: Direction, x: i32, y: i32 },
    PatternCaptured { directions: Vec<Direction> },
    Error(String),
}

/// Configuration push from UI thread to hook thread.
#[derive(Debug, Clone)]
pub enum HookCommand {
    UpdateConfig(ConfigSnapshot),
    SetInterception(bool),
    ForceReset,
    Shutdown,
}

/// Origin classification for every hook event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOrigin {
    /// Hardware-generated or non-injected event.
    Physical,
    /// Event injected by this daemon (dwExtraInfo == SELF_TAG).
    SelfInjected,
    /// Event injected by another process (LLMHF_INJECTED flagged).
    ForeignInjected,
}

/// Compact completion packet — no heap allocation in the hook callback.
/// Copied from the gesture buffer on right-up and enqueued to the
/// recognition worker for off-hook classification.
#[derive(Clone)]
struct GestureCompletion {
    points: [Point; MAX_POINTS],
    point_count: usize,
    release_point: Point,
    config_generation: u64,
}

/// Shared state between hook thread and main thread.
pub struct HookShared {
    pub shutdown: AtomicBool,
    pub ready: AtomicBool,
    pub interception_enabled: AtomicBool,
}

/// Controller for sending commands to the hook thread.
/// Wakes the hook thread via `PostThreadMessageW` so commands
/// take effect immediately without waiting for mouse input.
#[derive(Clone)]
pub struct HookController {
    cmd_tx: std::sync::mpsc::Sender<HookCommand>,
    hook_thread_id: u32,
}

impl HookController {
    /// Send a command and wake the hook thread.
    pub fn send(&self, command: HookCommand) -> std::result::Result<(), String> {
        self.cmd_tx.send(command).map_err(|e| format!("cmd send: {}", e))?;
        if self.hook_thread_id != 0 {
            unsafe {
                let ret = PostThreadMessageW(
                    self.hook_thread_id,
                    WM_HOOK_COMMAND,
                    WPARAM::default(),
                    LPARAM::default(),
                );
                if ret.is_err() {
                    return Err(format!("PostThreadMessageW: {:?}", ret));
                }
            }
        }
        Ok(())
    }
}

/// Spawn the input hook thread, recognition worker, and click-replay worker.
#[allow(clippy::type_complexity)]
pub fn spawn_hook_thread(
    activation_threshold: i32,
    sample_distance: i32,
    ui_hwnd_raw: isize,
) -> (
    thread::JoinHandle<()>,           // hook join handle
    thread::JoinHandle<()>,           // recognition worker join handle
    thread::JoinHandle<()>,           // replay worker join handle
    thread::JoinHandle<()>,           // policy worker join handle
    Arc<HookShared>,
    HookController,
    std::sync::mpsc::Receiver<HookEvent>,
) {
    let shared = Arc::new(HookShared {
        shutdown: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        interception_enabled: AtomicBool::new(false),
    });
    let shared_clone = shared.clone();

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<HookCommand>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<HookEvent>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<u32>(0);

    // Recognition worker channel — bounded, small capacity
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel::<GestureCompletion>(4);

    // Click replay channel — bounded, capacity 1 (newest overwrites oldest)
    let (replay_tx, replay_rx) = std::sync::mpsc::sync_channel::<ClickReplay>(1);

    // Policy resolution queue — unknown PIDs from hook → policy worker
    let (policy_tx, policy_rx) = std::sync::mpsc::channel::<u32>();

    let event_tx_clone = event_tx.clone();
    let shared_clone2 = shared.clone();

    // ── Hook thread ────────────────────────────────────────────
    let handle = thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(move || {
            let ui_hwnd = HWND(ui_hwnd_raw as *mut _);
            init_hook_state(activation_threshold, sample_distance);
            let hook_proc = create_hook_proc(
                event_tx.clone(),
                completion_tx,
                replay_tx,
                policy_tx,
                shared_clone.clone(),
                ui_hwnd,
            );

            // Create thread message queue before publishing thread ID
            let mut msg = MSG::default();
            unsafe { PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE); }
            let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            let _ = ready_tx.send(thread_id);

            let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, hook_proc, None, 0) };
            let hook = match hook {
                Ok(h) => h,
                Err(e) => {
                    log::error!("SetWindowsHookExW failed: {:?}", e);
                    return;
                }
            };

            shared_clone.ready.store(true, Ordering::SeqCst);
            log::info!("Hook installed, entering message loop (tid={})", thread_id);

            loop {
                let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if ret.0 == 0 || ret.0 == -1 {
                    break;
                }

                match msg.message {
                    WM_HOOK_COMMAND => {
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            process_hook_command(cmd, &shared_clone);
                        }
                    }
                    WM_HOOK_SHUTDOWN => break,
                    _ => {}
                }

                while let Ok(cmd) = cmd_rx.try_recv() {
                    process_hook_command(cmd, &shared_clone);
                }
            }

            while let Ok(cmd) = cmd_rx.try_recv() {
                process_hook_command(cmd, &shared_clone);
            }

            unsafe { let _ = UnhookWindowsHookEx(hook); }
            log::info!("Hook thread shut down");
        })
        .expect("spawn hook thread");

    // ── Recognition worker ──────────────────────────────────────
    let recog_handle = thread::Builder::new()
        .name("mouse-recognize".into())
        .spawn(move || {
            while let Ok(completion) = completion_rx.recv() {
                if shared_clone2.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Reconstruct a temporary buffer from the completion packet
                let mut buf = GestureBuffer::new(2);
                for i in 0..completion.point_count.min(MAX_POINTS) {
                    buf.add_force(completion.points[i]);
                }
                buf.add_force(completion.release_point);

                // Check config generation
                let classification = HOOK_CONFIG.with(|cfg_cell| {
                    let cfg = cfg_cell.borrow();
                    let cfg_inner = cfg.as_ref().expect("HOOK_CONFIG not initialized");

                    let config_changed = completion.config_generation != 0
                        && completion.config_generation != cfg_inner.generation;

                    if config_changed {
                        crate::gesture::GestureResult::NoMatch
                    } else {
                        classify(
                            &buf,
                            &cfg_inner.gestures,
                            cfg_inner.rdp_epsilon_sq,
                            cfg_inner.min_gesture_length,
                        )
                    }
                });

                match classification {
                    GestureResult::Matched { name, .. } => {
                        let _ = event_tx_clone.send(HookEvent::GestureEnded {
                            matched: true,
                            gesture_name: Some(name),
                        });
                    }
                    _ => {
                        let _ = event_tx_clone.send(HookEvent::GestureEnded {
                            matched: false,
                            gesture_name: None,
                        });
                    }
                }

                // Wake the UI thread
                let _ = post_ui_wake(ui_hwnd_raw);
            }
            log::info!("Recognition worker shut down");
        })
        .expect("spawn recognition worker");

    // ── Click replay worker ─────────────────────────────────────
    let shared_clone3 = shared.clone();
    let replay_handle = thread::Builder::new()
        .name("mouse-replay".into())
        .spawn(move || {
            while let Ok(replay) = replay_rx.recv() {
                if shared_clone3.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                inject_synthetic_click(replay.release_point);
            }
            log::info!("Replay worker shut down");
        })
        .expect("spawn replay worker");

    // ── Policy worker ──────────────────────────────────────────
    let shared_clone4 = shared.clone();
    let policy_handle = thread::Builder::new()
        .name("policy-resolver".into())
        .spawn(move || {
            while let Ok(pid) = policy_rx.recv() {
                if shared_clone4.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                // Read current config to resolve PID
                let snapshot = app_policy::get_snapshot();
                if let Some(current) = snapshot {
                    // Build updated snapshot and publish
                    let mut new_snapshot = app_policy::PolicySnapshot {
                        mode: current.mode.clone(),
                        generation: current.generation,
                        known_pids: current.known_pids.clone(),
                    };
                    // Use resolve_pid to determine eligibility
                    let entry = HOOK_CONFIG.with(|c| {
                        c.borrow().as_ref().and_then(|cfg| app_policy::resolve_pid(pid, cfg))
                    });
                    if let Some(entry) = entry {
                        let eligibility = if entry.is_eligible { Eligibility::Allowed } else { Eligibility::Denied };
                        new_snapshot.known_pids.insert(pid, eligibility);
                        log::debug!("Policy: PID {} -> {} ({})", pid, entry.basename, if entry.is_eligible { "allowed" } else { "denied" });
                    }
                    app_policy::publish_snapshot(new_snapshot);
                }
            }
            log::info!("Policy worker shut down");
        })
        .expect("spawn policy worker");

    let hook_thread_id = ready_rx.recv().expect("hook thread not ready");
    let controller = HookController { cmd_tx, hook_thread_id };

    (handle, recog_handle, replay_handle, policy_handle, shared, controller, event_rx)
}

/// Process a single hook command (called from the hook thread).
fn process_hook_command(cmd: HookCommand, shared: &HookShared) {
    match cmd {
        HookCommand::Shutdown => {
            shared.shutdown.store(true, Ordering::SeqCst);
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            unsafe {
                let _ = PostThreadMessageW(tid, WM_HOOK_SHUTDOWN, WPARAM::default(), LPARAM::default());
            }
        }
        HookCommand::UpdateConfig(snapshot) => {
            let n = snapshot.gestures.len();
            HOOK_CONFIG.with(|c| { *c.borrow_mut() = Some(snapshot); });
            log::info!("Hook config updated: {} gestures", n);
        }
        HookCommand::SetInterception(enabled) => {
            shared.interception_enabled.store(enabled, Ordering::SeqCst);
            HOOK_INTERCEPTION_ENABLED.with(|c| { *c.borrow_mut() = enabled; });
            log::info!("Hook interception: {}", if enabled { "ON" } else { "OFF" });
        }
        HookCommand::ForceReset => {
            // State-aware reset: only if the physical button is not held
            // (lost-release recovery) or during shutdown/session-lock.
            let can_reset = HOOK_STATE_MACHINE.with(|sm| {
                sm.borrow().as_ref().map(|s| {
                    !s.physical_button_down
                        || shared.shutdown.load(Ordering::Relaxed)
                        || !shared.interception_enabled.load(Ordering::Relaxed)
                }).unwrap_or(true)
            });

            if can_reset {
                HOOK_STATE_MACHINE.with(|sm| {
                    if let Some(ref mut sm) = *sm.borrow_mut() { sm.force_reset(); }
                });
                HOOK_BUFFER.with(|buf| {
                    if let Some(ref mut b) = *buf.borrow_mut() { b.clear(); }
                });
                HOOK_LAST_DIRECTION.with(|c| c.set(8));
            }
        }
    }
}

// ── Thread-local state ─────────────────────────────────────────

thread_local! {
    static HOOK_CONFIG: std::cell::RefCell<Option<ConfigSnapshot>> = const { std::cell::RefCell::new(None) };
    static HOOK_BUFFER: std::cell::RefCell<Option<GestureBuffer>> = const { std::cell::RefCell::new(None) };
    static HOOK_STATE_MACHINE: std::cell::RefCell<Option<StateMachine>> = const { std::cell::RefCell::new(None) };
    static HOOK_EVENT_TX: std::cell::OnceCell<std::sync::mpsc::Sender<HookEvent>> = const { std::cell::OnceCell::new() };
    static HOOK_COMPLETION_TX: std::cell::OnceCell<std::sync::mpsc::SyncSender<GestureCompletion>> = const { std::cell::OnceCell::new() };
    static HOOK_REPLAY_TX: std::cell::OnceCell<std::sync::mpsc::SyncSender<ClickReplay>> = const { std::cell::OnceCell::new() };
    static HOOK_POLICY_QUEUE: std::cell::OnceCell<std::sync::mpsc::Sender<u32>> = const { std::cell::OnceCell::new() };
    static HOOK_INTERCEPTION_ENABLED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
    static HOOK_UI_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static HOOK_LAST_DIRECTION: std::cell::Cell<u8> = const { std::cell::Cell::new(8) };
}

// ── Hook Procedure ─────────────────────────────────────────────

fn create_hook_proc(
    event_tx: std::sync::mpsc::Sender<HookEvent>,
    completion_tx: std::sync::mpsc::SyncSender<GestureCompletion>,
    replay_tx: std::sync::mpsc::SyncSender<ClickReplay>,
    policy_queue: std::sync::mpsc::Sender<u32>,
    shared: Arc<HookShared>,
    ui_hwnd: HWND,
) -> HOOKPROC {
    HOOK_EVENT_TX.with(|cell| { cell.set(event_tx).expect("HOOK_EVENT_TX already set"); });
    HOOK_COMPLETION_TX.with(|cell| { cell.set(completion_tx).expect("HOOK_COMPLETION_TX already set"); });
    HOOK_REPLAY_TX.with(|cell| { cell.set(replay_tx).expect("HOOK_REPLAY_TX already set"); });
    HOOK_POLICY_QUEUE.with(|cell| { cell.set(policy_queue).expect("HOOK_POLICY_QUEUE already set"); });
    HOOK_INTERCEPTION_ENABLED.with(|c| {
        *c.borrow_mut() = shared.interception_enabled.load(Ordering::Relaxed);
    });
    HOOK_UI_HWND.with(|c| { c.set(ui_hwnd.0 as isize); });
    Some(low_level_mouse_proc)
}

/// Send an event to the UI thread and wake its message pump.
fn post_event(event: HookEvent) {
    HOOK_EVENT_TX.with(|tx| {
        if let Some(tx) = tx.get() { let _ = tx.send(event); }
    });
    HOOK_UI_HWND.with(|c| {
        let hwnd = c.get();
        if hwnd != 0 {
            unsafe {
                use windows::Win32::Foundation::{WPARAM, LPARAM};
                use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
                let _ = PostMessageW(Some(HWND(hwnd as *mut _)), 0x8002, WPARAM::default(), LPARAM::default());
            }
        }
    });
}

/// Wake the UI thread message pump (used by recognition/replay workers).
fn post_ui_wake(ui_hwnd_raw: isize) -> std::result::Result<(), windows::core::Error> {
    if ui_hwnd_raw == 0 {
        return Ok(());
    }
    unsafe {
        use windows::Win32::Foundation::{WPARAM, LPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        PostMessageW(Some(HWND(ui_hwnd_raw as *mut _)), 0x8002, WPARAM::default(), LPARAM::default())
    }
}

unsafe extern "system" fn low_level_mouse_proc(
    code: i32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    if !HOOK_INTERCEPTION_ENABLED.with(|c| *c.borrow()) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_mouse_event(code, wparam, lparam)
    }));
    match result {
        Ok(lresult) => lresult,
        Err(_) => CallNextHookEx(None, code, wparam, lparam),
    }
}

fn process_mouse_event(_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let msg = wparam.0 as u32;
    let is_right = msg == WM_RBUTTONDOWN || msg == WM_RBUTTONUP;
    let is_move = msg == WM_MOUSEMOVE;

    let in_gesture = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow().as_ref()
            .map(|s| s.state != crate::state_machine::State::Idle)
            .unwrap_or(false)
    });

    if !is_right && !(is_move && in_gesture) {
        return unsafe { CallNextHookEx(None, _code, wparam, lparam) };
    }

    let hook_data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };

    // Determine InputOrigin
    let origin = if hook_data.dwExtraInfo == SELF_TAG {
        InputOrigin::SelfInjected
    } else if (hook_data.flags & LLMHF_INJECTED) != 0 {
        InputOrigin::ForeignInjected
    } else {
        InputOrigin::Physical
    };

    // Self-injected events: always pass through, never interact with gesture state
    if origin == InputOrigin::SelfInjected {
        return unsafe { CallNextHookEx(None, _code, wparam, lparam) };
    }

    let x = hook_data.pt.x;
    let y = hook_data.pt.y;

    if msg == WM_RBUTTONDOWN {
        handle_right_down(_code, wparam, lparam, origin, x, y)
    } else if msg == WM_RBUTTONUP {
        handle_right_up(_code, wparam, lparam, origin, x, y)
    } else {
        handle_mouse_move(_code, wparam, lparam, origin, x, y)
    }
}

// ── Right-button handlers ──────────────────────────────────────

fn handle_right_down(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    origin: InputOrigin, x: i32, y: i32,
) -> LRESULT {
    // Foreign-injected down: pass through, don't start a gesture
    if origin != InputOrigin::Physical {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let (target_hwnd, target_pid) = resolve_window_under_cursor(x, y);

    // Determine eligibility from the atomic policy snapshot (P0.3).
    // Zero-allocation, lock-free read from the hook callback.
    let is_eligible = if target_pid == 0 || target_pid == std::process::id() {
        false
    } else {
        match app_policy::get_snapshot() {
            Some(snapshot) => {
                match snapshot.lookup(target_pid) {
                    Some(Eligibility::Allowed) => true,
                    Some(Eligibility::Denied) => false,
                    None => {
                        // Unknown PID — enqueue async resolution, apply default
                        enqueue_policy_resolution(target_pid);
                        snapshot.default_for_unknown() == Eligibility::Allowed
                    }
                }
            }
            None => true, // no snapshot yet — default allow
        }
    };

    let foreground_hwnd = unsafe { GetForegroundWindow() };

    let ctx = if is_eligible {
        Some(GestureContext {
            target_hwnd,
            target_pid,
            foreground_hwnd,
            start_point: POINT { x, y },
            origin_monitor: 0,
            origin_dpi: 96,
            config_generation: HOOK_CONFIG.with(|c| {
                c.borrow().as_ref().map(|cfg| cfg.generation).unwrap_or(0)
            }),
        })
    } else { None };

    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut().expect("HOOK_STATE_MACHINE not initialized")
            .on_right_down(origin == InputOrigin::Physical, is_eligible, ctx)
    });

    match result {
        DownResult::Consumed => {
            // Seed the gesture buffer with the start point (P0.6)
            HOOK_BUFFER.with(|buf_cell| {
                let mut buf = buf_cell.borrow_mut();
                if let Some(ref mut b) = *buf {
                    b.add_force(Point { x, y });
                }
            });
            HOOK_LAST_DIRECTION.with(|c| c.set(8));
            LRESULT(1)
        }
        _ => unsafe { CallNextHookEx(None, code, wparam, lparam) },
    }
}

fn resolve_window_under_cursor(x: i32, y: i32) -> (HWND, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{WindowFromPoint, GetWindowThreadProcessId};
    unsafe {
        let hwnd = WindowFromPoint(POINT { x, y });
        if hwnd.0.is_null() {
            return (HWND::default(), 0);
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        (hwnd, pid)
    }
}

fn handle_right_up(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    origin: InputOrigin, x: i32, y: i32,
) -> LRESULT {
    // Foreign-injected up during a physical capture: suppress it.
    // The physical up will arrive separately to complete the gesture.
    if origin != InputOrigin::Physical {
        let in_capture = HOOK_STATE_MACHINE.with(|sm| {
            sm.borrow().as_ref()
                .map(|s| s.physical_button_down)
                .unwrap_or(false)
        });
        if in_capture {
            return LRESULT(1); // suppress foreign up
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut().expect("HOOK_STATE_MACHINE not initialized").on_right_up()
    });

    match result {
        UpResult::ReplaySynthetic => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));
            // Enqueue to replay worker — no longer posts HookEvent
            enqueue_replay(ClickReplay {
                release_point: Point { x, y },
                requested_at: std::time::Instant::now(),
            });
            LRESULT(1)
        }
        UpResult::GestureComplete => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));

            // Build completion packet with zero heap allocation
            let config_gen = HOOK_CONFIG.with(|c| {
                c.borrow().as_ref().map(|cfg| cfg.generation).unwrap_or(0)
            });

            let captured_gen = HOOK_STATE_MACHINE.with(|sm| {
                sm.borrow().as_ref().map(|s| s.current_generation()).unwrap_or(0)
            });

            // Config hot-reload mid-gesture? Still enqueue — the recognition
            // worker handles generation mismatch
            let completion_gen = if captured_gen != 0 && captured_gen != config_gen {
                0 // signal mismatch
            } else {
                captured_gen
            };

            HOOK_BUFFER.with(|buf_cell| {
                let mut buf = buf_cell.borrow_mut();
                if let Some(ref mut b) = *buf {
                    let mut points = [Point { x: 0, y: 0 }; MAX_POINTS];
                    let stored = b.stored_points();
                    let count = stored.len().min(MAX_POINTS);
                    points[..count].copy_from_slice(&stored[..count]);
                    b.clear();

                    enqueue_completion(GestureCompletion {
                        points,
                        point_count: count,
                        release_point: Point { x, y },
                        config_generation: completion_gen,
                    });
                }
            });

            LRESULT(1)
        }
        _ => unsafe { CallNextHookEx(None, code, wparam, lparam) },
    }
}

fn handle_mouse_move(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    origin: InputOrigin, x: i32, y: i32,
) -> LRESULT {
    // Foreign-injected move: never contribute to a physical gesture buffer
    if origin != InputOrigin::Physical {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let activated = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut().expect("HOOK_STATE_MACHINE not initialized").on_move(x, y)
    });

    if activated {
        // Seed the activation-crossing point (P0.6)
        HOOK_BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            if let Some(ref mut b) = *buf {
                b.add_force(Point { x, y });
            }
        });
        post_event(HookEvent::GestureStarted { x, y, monitor: 0 });
    }

    let in_drawing = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow().as_ref()
            .map(|s| s.state == crate::state_machine::State::Drawing)
            .unwrap_or(false)
    });

    if in_drawing {
        HOOK_BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            if let Some(ref mut buf) = *buf {
                buf.add_point(Point { x, y });
                if let Some((from, to)) = buf.last_two() {
                    let dir = direction_from_points(from, to);
                    let dir_idx = match dir {
                        Direction::N => 0, Direction::NE => 1, Direction::E => 2,
                        Direction::SE => 3, Direction::S => 4, Direction::SW => 5,
                        Direction::W => 6, Direction::NW => 7,
                    };
                    let prev = HOOK_LAST_DIRECTION.with(|c| c.replace(dir_idx));
                    if prev != dir_idx {
                        post_event(HookEvent::DirectionChanged { direction: dir, x, y });
                    }
                }
            }
        });
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ── Completion and replay enqueue ──────────────────────────────

fn enqueue_completion(completion: GestureCompletion) {
    HOOK_COMPLETION_TX.with(|tx| {
        if let Some(tx) = tx.get() {
            // try_send: non-blocking. If channel is full (4 pending),
            // drop the oldest — better than blocking the hook callback.
            let _ = tx.try_send(completion);
        }
    });
}

/// Click replay packet for the replay worker.
#[derive(Debug, Clone)]
struct ClickReplay {
    release_point: Point,
    requested_at: std::time::Instant,
}

fn enqueue_replay(replay: ClickReplay) {
    HOOK_REPLAY_TX.with(|tx| {
        if let Some(tx) = tx.get() {
            let _ = tx.try_send(replay);
        }
    });
}

/// Enqueue an unknown PID for async resolution by the policy worker.
/// Fire-and-forget — if the channel is full, drop silently.
fn enqueue_policy_resolution(pid: u32) {
    HOOK_POLICY_QUEUE.with(|tx| {
        if let Some(tx) = tx.get() {
            let _ = tx.send(pid);
        }
    });
}

/// Inject a synthetic right-click at absolute screen coordinates.
/// Called from the replay worker thread, NOT the hook callback.
fn inject_synthetic_click(point: Point) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEINPUT,
        MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    };

    // Get virtual screen dimensions for absolute coordinate normalization
    let screen_w = unsafe { windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
        windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN) };
    let screen_h = unsafe { windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
        windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN) };
    let screen_x = unsafe { windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
        windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN) };
    let screen_y = unsafe { windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
        windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN) };

    // Normalize to 0..65535 range
    let x_abs = if screen_w > 0 {
        (((point.x - screen_x) as i64 * 65535i64) / screen_w as i64) as i32
    } else { 0 };
    let y_abs = if screen_h > 0 {
        (((point.y - screen_y) as i64 * 65535i64) / screen_h as i64) as i32
    } else { 0 };

    let inputs = [
        INPUT { r#type: INPUT_MOUSE, Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            mi: MOUSEINPUT {
                dx: x_abs, dy: y_abs, mouseData: 0,
                dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_RIGHTDOWN,
                time: 0, dwExtraInfo: SELF_TAG,
            }
        }},
        INPUT { r#type: INPUT_MOUSE, Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            mi: MOUSEINPUT {
                dx: x_abs, dy: y_abs, mouseData: 0,
                dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_RIGHTUP,
                time: 0, dwExtraInfo: SELF_TAG,
            }
        }},
    ];

    let count = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if count != 2 {
        log::warn!("SendInput returned {} of 2 events for synthetic click at ({},{})",
            count, point.x, point.y);
    }
}

/// Initialize the hook thread-local state.
pub fn init_hook_state(activation_threshold: i32, sample_distance: i32) {
    HOOK_STATE_MACHINE.with(|sm| {
        *sm.borrow_mut() = Some(StateMachine::new(activation_threshold));
    });
    HOOK_BUFFER.with(|buf| {
        *buf.borrow_mut() = Some(GestureBuffer::new(sample_distance));
    });
}
