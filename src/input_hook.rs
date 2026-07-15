//! Input hook thread — owns WH_MOUSE_LL, the message pump,
//! and the gesture buffer.

use crate::app_policy::{self, Eligibility};
use crate::config::ConfigSnapshot;
use crate::config::Direction;
use crate::gesture::{
    classify, direction_from_points, GestureBuffer, GestureResult, Point, MAX_POINTS,
};
pub use crate::state_machine::SELF_TAG;
use crate::state_machine::{DownResult, GestureContext, StateMachine, UpResult};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, HOOKPROC, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT,
    PM_NOREMOVE, WH_MOUSE_LL, WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_USER,
};

/// Custom messages posted to the hook thread's message queue.
pub const WM_HOOK_COMMAND: u32 = WM_USER + 1;
pub const WM_HOOK_SHUTDOWN: u32 = WM_USER + 2;

/// Event sent from the hook thread to the UI thread.
#[derive(Debug, Clone)]
pub enum HookEvent {
    GestureStarted {
        x: i32,
        y: i32,
        monitor: isize,
    },
    TrailPoint {
        x: i32,
        y: i32,
    },
    GestureEnded {
        matched: bool,
        gesture_name: Option<String>,
        target_hwnd: isize,
    },
    DirectionChanged {
        direction: Direction,
        x: i32,
        y: i32,
    },
    PatternCaptured {
        directions: Vec<Direction>,
    },
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
    Physical,
    SelfInjected,
    ForeignInjected,
}

/// Classification parameters baked into each completion packet
/// so the recognition worker never accesses thread-local state.
type PatternDef = (String, Vec<Direction>);

/// Compact completion packet — zero heap allocation in the hook callback.
/// Carries everything the recognition worker needs: points + classification params.
#[derive(Clone)]
struct GestureCompletion {
    points: [Point; MAX_POINTS],
    point_count: usize,
    release_point: Point,
    config_generation: u64,
    target_hwnd: isize,
    /// Pre-compiled gesture patterns for classification (owned by this packet).
    patterns: Arc<Vec<PatternDef>>,
    rdp_epsilon_sq: f64,
    min_gesture_length: u32,
}

/// Shared state between hook thread and main thread.
pub struct HookShared {
    pub shutdown: AtomicBool,
    pub ready: AtomicBool,
    pub interception_enabled: AtomicBool,
}

/// Controller for sending commands to the hook thread.
#[derive(Clone)]
pub struct HookController {
    cmd_tx: std::sync::mpsc::Sender<HookCommand>,
    hook_thread_id: u32,
}

impl HookController {
    pub fn send(&self, command: HookCommand) -> std::result::Result<(), String> {
        self.cmd_tx
            .send(command)
            .map_err(|e| format!("cmd send: {}", e))?;
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

/// Spawn the input hook thread, recognition worker, replay worker, and policy worker.
#[allow(clippy::type_complexity)]
pub fn spawn_hook_thread(
    activation_threshold: i32,
    sample_distance: i32,
    ui_hwnd_raw: isize,
) -> (
    thread::JoinHandle<()>,
    thread::JoinHandle<()>,
    thread::JoinHandle<()>,
    Arc<HookShared>,
    HookController,
    std::sync::mpsc::Receiver<HookEvent>,
) {
    let shared = Arc::new(HookShared {
        shutdown: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        interception_enabled: AtomicBool::new(false),
    });
    let shared_hook = shared.clone();
    let shared_recog = shared.clone();
    let shared_replay = shared.clone();

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<HookCommand>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<HookEvent>();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<u32>(0);

    // Recognition worker channel
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel::<GestureCompletion>(4);

    // Click replay channel
    let (replay_tx, replay_rx) = std::sync::mpsc::sync_channel::<ClickReplay>(1);

    let event_tx_recog = event_tx.clone();

    // ── Hook thread ────────────────────────────────────────────
    let hook_handle = thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(move || {
            let ui_hwnd = HWND(ui_hwnd_raw as *mut _);
            init_hook_state(activation_threshold, sample_distance);
            let hook_proc = create_hook_proc(
                event_tx.clone(),
                completion_tx,
                replay_tx,
                shared_hook.clone(),
                ui_hwnd,
            );

            // Create thread message queue before publishing thread ID
            let mut msg = MSG::default();
            unsafe {
                let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
            }
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

            shared_hook.ready.store(true, Ordering::SeqCst);
            log::info!("Hook installed (tid={})", thread_id);

            loop {
                let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if ret.0 == 0 || ret.0 == -1 {
                    break;
                }

                match msg.message {
                    WM_HOOK_COMMAND => {
                        while let Ok(cmd) = cmd_rx.try_recv() {
                            process_hook_command(cmd, &shared_hook);
                        }
                    }
                    // Only honour WM_HOOK_SHUTDOWN when authenticated through the
                    // channel path (shared.shutdown already set by Shutdown command).
                    WM_HOOK_SHUTDOWN if shared_hook.shutdown.load(Ordering::SeqCst) => break,
                    _ => {}
                }
            }

            // Drain final commands on exit
            while let Ok(cmd) = cmd_rx.try_recv() {
                process_hook_command(cmd, &shared_hook);
            }

            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
            log::info!("Hook thread shut down");
        })
        .expect("spawn hook thread");

    // ── Recognition worker ──────────────────────────────────────
    let recog_handle = thread::Builder::new()
        .name("mouse-recognize".into())
        .spawn(move || {
            while let Ok(completion) = completion_rx.recv() {
                if shared_recog.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // Reconstruct buffer from completion packet
                let mut buf = GestureBuffer::new(2);
                for i in 0..completion.point_count.min(MAX_POINTS) {
                    buf.add_force(completion.points[i]);
                }
                buf.add_force(completion.release_point);

                // Config generation check: 0 sentinel = mismatch from hook side
                let config_changed = completion.config_generation == 0;

                let classification = if config_changed {
                    GestureResult::NoMatch
                } else {
                    classify(
                        &buf,
                        &completion.patterns,
                        completion.rdp_epsilon_sq,
                        completion.min_gesture_length,
                    )
                };

                match classification {
                    GestureResult::Matched { name, .. } => {
                        let _ = event_tx_recog.send(HookEvent::GestureEnded {
                            matched: true,
                            gesture_name: Some(name),
                            target_hwnd: completion.target_hwnd,
                        });
                    }
                    _ => {
                        let _ = event_tx_recog.send(HookEvent::GestureEnded {
                            matched: false,
                            gesture_name: None,
                            target_hwnd: completion.target_hwnd,
                        });
                    }
                }

                // Wake the UI thread
                post_ui_wake(ui_hwnd_raw);
            }
            log::info!("Recognition worker shut down");
        })
        .expect("spawn recognition worker");

    // ── Click replay worker ─────────────────────────────────────
    let replay_handle = thread::Builder::new()
        .name("mouse-replay".into())
        .spawn(move || {
            while let Ok(replay) = replay_rx.recv() {
                if shared_replay.shutdown.load(Ordering::Relaxed) {
                    break;
                }
                inject_synthetic_click(replay.release_point);
            }
            log::info!("Replay worker shut down");
        })
        .expect("spawn replay worker");

    let hook_thread_id = ready_rx.recv().expect("hook thread not ready");
    let controller = HookController {
        cmd_tx,
        hook_thread_id,
    };

    (
        hook_handle,
        recog_handle,
        replay_handle,
        shared,
        controller,
        event_rx,
    )
}

/// Process a single hook command (called from the hook thread).
fn process_hook_command(cmd: HookCommand, shared: &HookShared) {
    match cmd {
        HookCommand::Shutdown => {
            shared.shutdown.store(true, Ordering::SeqCst);
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            unsafe {
                let _ =
                    PostThreadMessageW(tid, WM_HOOK_SHUTDOWN, WPARAM::default(), LPARAM::default());
            }
        }
        HookCommand::UpdateConfig(snapshot) => {
            let n = snapshot.gestures.len();
            // Pre-build Arc<Vec<PatternDef>> so gesture completion only
            // bumps a ref-count instead of allocating + cloning the pattern list.
            let patterns = Arc::new(snapshot.gestures.clone());
            HOOK_PATTERNS.with(|c| {
                *c.borrow_mut() = Some(patterns);
            });
            HOOK_CONFIG.with(|c| {
                *c.borrow_mut() = Some(snapshot);
            });
            log::info!("Hook config updated: {} gestures", n);
        }
        HookCommand::SetInterception(enabled) => {
            shared.interception_enabled.store(enabled, Ordering::SeqCst);
            HOOK_INTERCEPTION_ENABLED.with(|c| {
                *c.borrow_mut() = enabled;
            });
            log::info!("Hook interception: {}", if enabled { "ON" } else { "OFF" });
        }
        HookCommand::ForceReset => {
            // State-aware reset: only if button not physically held,
            // or during shutdown/session-lock.
            let can_reset = HOOK_STATE_MACHINE.with(|sm| {
                sm.borrow()
                    .as_ref()
                    .map(|s| {
                        !s.physical_button_down
                            || shared.shutdown.load(Ordering::Relaxed)
                            || !shared.interception_enabled.load(Ordering::Relaxed)
                    })
                    .unwrap_or(true)
            });

            if can_reset {
                HOOK_STATE_MACHINE.with(|sm| {
                    if let Some(ref mut sm) = *sm.borrow_mut() {
                        sm.force_reset();
                    }
                });
                HOOK_BUFFER.with(|buf| {
                    if let Some(ref mut b) = *buf.borrow_mut() {
                        b.clear();
                    }
                });
                HOOK_LAST_DIRECTION.with(|c| c.set(8));
            }
        }
    }
}

// ── Thread-local state ─────────────────────────────────────────

thread_local! {
    static HOOK_CONFIG: std::cell::RefCell<Option<ConfigSnapshot>> = const { std::cell::RefCell::new(None) };
    /// Pre-built gesture patterns wrapped in Arc, refreshed on config update.
    /// The hook callback clones this Arc (ref-count bump only) instead of
    /// allocating a new Arc<Vec<PatternDef>> on every gesture completion.
    static HOOK_PATTERNS: std::cell::RefCell<Option<Arc<Vec<PatternDef>>>> = const { std::cell::RefCell::new(None) };
    static HOOK_BUFFER: std::cell::RefCell<Option<GestureBuffer>> = const { std::cell::RefCell::new(None) };
    static HOOK_STATE_MACHINE: std::cell::RefCell<Option<StateMachine>> = const { std::cell::RefCell::new(None) };
    static HOOK_EVENT_TX: std::cell::OnceCell<std::sync::mpsc::Sender<HookEvent>> = const { std::cell::OnceCell::new() };
    static HOOK_COMPLETION_TX: std::cell::OnceCell<std::sync::mpsc::SyncSender<GestureCompletion>> = const { std::cell::OnceCell::new() };
    static HOOK_REPLAY_TX: std::cell::OnceCell<std::sync::mpsc::SyncSender<ClickReplay>> = const { std::cell::OnceCell::new() };
    static HOOK_INTERCEPTION_ENABLED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
    static HOOK_UI_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static HOOK_LAST_DIRECTION: std::cell::Cell<u8> = const { std::cell::Cell::new(8) };
}

// ── Hook Procedure ─────────────────────────────────────────────

fn create_hook_proc(
    event_tx: std::sync::mpsc::Sender<HookEvent>,
    completion_tx: std::sync::mpsc::SyncSender<GestureCompletion>,
    replay_tx: std::sync::mpsc::SyncSender<ClickReplay>,
    shared: Arc<HookShared>,
    ui_hwnd: HWND,
) -> HOOKPROC {
    HOOK_EVENT_TX.with(|c| {
        c.set(event_tx).expect("HOOK_EVENT_TX");
    });
    HOOK_COMPLETION_TX.with(|c| {
        c.set(completion_tx).expect("HOOK_COMPLETION_TX");
    });
    HOOK_REPLAY_TX.with(|c| {
        c.set(replay_tx).expect("HOOK_REPLAY_TX");
    });
    HOOK_INTERCEPTION_ENABLED.with(|c| {
        *c.borrow_mut() = shared.interception_enabled.load(Ordering::Relaxed);
    });
    HOOK_UI_HWND.with(|c| {
        c.set(ui_hwnd.0 as isize);
    });
    Some(low_level_mouse_proc)
}

/// Send an event to the UI thread and wake its message pump.
fn post_event(event: HookEvent) {
    HOOK_EVENT_TX.with(|tx| {
        if let Some(tx) = tx.get() {
            let _ = tx.send(event);
        }
    });
    HOOK_UI_HWND.with(|c| {
        let hwnd = c.get();
        if hwnd != 0 {
            unsafe {
                use windows::Win32::Foundation::{LPARAM, WPARAM};
                use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    0x8002,
                    WPARAM::default(),
                    LPARAM::default(),
                );
            }
        }
    });
}

/// Wake the UI thread message pump (used by recognition/replay workers).
fn post_ui_wake(ui_hwnd_raw: isize) {
    if ui_hwnd_raw == 0 {
        return;
    }
    unsafe {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
        let _ = PostMessageW(
            Some(HWND(ui_hwnd_raw as *mut _)),
            0x8002,
            WPARAM::default(),
            LPARAM::default(),
        );
    }
}

unsafe extern "system" fn low_level_mouse_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    if !HOOK_INTERCEPTION_ENABLED.with(|c| *c.borrow()) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    // Panic protection lives inside process_mouse_event, after the
    // non-gesture early-return — most mouse events never reach it.
    process_mouse_event(code, wparam, lparam)
}

fn process_mouse_event(_code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let msg = wparam.0 as u32;
    let is_right = msg == WM_RBUTTONDOWN || msg == WM_RBUTTONUP;
    let is_move = msg == WM_MOUSEMOVE;

    let in_gesture = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow()
            .as_ref()
            .map(|s| s.state != crate::state_machine::State::Idle)
            .unwrap_or(false)
    });

    if !is_right && !(is_move && in_gesture) {
        return unsafe { CallNextHookEx(None, _code, wparam, lparam) };
    }

    let hook_data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };

    let origin = if hook_data.dwExtraInfo == SELF_TAG {
        InputOrigin::SelfInjected
    } else if (hook_data.flags & LLMHF_INJECTED) != 0 {
        InputOrigin::ForeignInjected
    } else {
        InputOrigin::Physical
    };

    // Self-injected events: always pass through
    if origin == InputOrigin::SelfInjected {
        return unsafe { CallNextHookEx(None, _code, wparam, lparam) };
    }

    let x = hook_data.pt.x;
    let y = hook_data.pt.y;

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if msg == WM_RBUTTONDOWN {
            handle_right_down(_code, wparam, lparam, origin, x, y)
        } else if msg == WM_RBUTTONUP {
            handle_right_up(_code, wparam, lparam, origin, x, y)
        } else {
            handle_mouse_move(_code, wparam, lparam, origin, x, y)
        }
    }));
    match result {
        Ok(r) => r,
        Err(_) => unsafe { CallNextHookEx(None, _code, wparam, lparam) },
    }
}

// ── Right-button handlers ──────────────────────────────────────

fn handle_right_down(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
    origin: InputOrigin,
    x: i32,
    y: i32,
) -> LRESULT {
    if origin != InputOrigin::Physical {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let (target_hwnd, target_pid) = resolve_window_under_cursor(x, y);

    // Determine eligibility from atomic policy snapshot
    let is_eligible = if target_pid == 0 || target_pid == std::process::id() {
        false
    } else {
        match app_policy::get_snapshot() {
            Some(snapshot) => match snapshot.lookup(target_pid) {
                Some(Eligibility::Allowed) => true,
                Some(Eligibility::Denied) => false,
                None => {
                    // PID unknown — resolve synchronously in the hook callback.
                    // Background resolution is too slow: by the time the policy
                    // worker finishes, the full gesture has already completed.
                    let eligibility = resolve_pid_sync(target_pid);
                    let mut snapshot_mut = app_policy::PolicySnapshot {
                        mode: snapshot.mode.clone(),
                        generation: snapshot.generation,
                        known_pids: snapshot.known_pids.clone(),
                    };
                    snapshot_mut.known_pids.insert(target_pid, eligibility);
                    app_policy::publish_snapshot(snapshot_mut);
                    eligibility == Eligibility::Allowed
                }
            },
            None => true,
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
            config_generation: HOOK_CONFIG
                .with(|c| c.borrow().as_ref().map(|cfg| cfg.generation).unwrap_or(0)),
        })
    } else {
        None
    };

    // Caller already guarantees Physical origin — pass false for is_injected
    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut()
            .expect("HOOK_STATE_MACHINE not initialized")
            .on_right_down(false, is_eligible, ctx)
    });

    match result {
        DownResult::Consumed => {
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
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, WindowFromPoint};
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
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
    origin: InputOrigin,
    x: i32,
    y: i32,
) -> LRESULT {
    // Foreign-injected up during physical capture: suppress it
    if origin != InputOrigin::Physical {
        let in_capture = HOOK_STATE_MACHINE.with(|sm| {
            sm.borrow()
                .as_ref()
                .map(|s| s.physical_button_down)
                .unwrap_or(false)
        });
        if in_capture {
            return LRESULT(1);
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut()
            .expect("HOOK_STATE_MACHINE not initialized")
            .on_right_up()
    });

    match result {
        UpResult::ReplaySynthetic => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));
            enqueue_replay(ClickReplay {
                release_point: Point { x, y },
            });
            LRESULT(1)
        }
        UpResult::GestureComplete => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));

            // Capture config generation for mismatch detection
            let config_gen =
                HOOK_CONFIG.with(|c| c.borrow().as_ref().map(|cfg| cfg.generation).unwrap_or(0));
            let captured_gen = HOOK_STATE_MACHINE.with(|sm| {
                sm.borrow()
                    .as_ref()
                    .map(|s| s.current_generation())
                    .unwrap_or(0)
            });

            // 0 sentinel = config changed mid-gesture
            let completion_gen = if captured_gen != 0 && captured_gen != config_gen {
                0
            } else {
                captured_gen
            };

            // Capture target window for action dispatch (P0 focus routing fix)
            let target_hwnd = HOOK_STATE_MACHINE.with(|sm| {
                sm.borrow()
                    .as_ref()
                    .and_then(|s| s.context.as_ref().map(|c| c.target_hwnd.0 as isize))
                    .unwrap_or(0)
            });

            // Snapshot rdp_epsilon and min_gesture_length from config.
            // Patterns come from HOOK_PATTERNS (pre-built Arc, ref-count bump only).
            let (rdp_epsilon_sq, min_gesture_length) = HOOK_CONFIG.with(|c| {
                let cfg = c.borrow();
                let cfg = cfg.as_ref().expect("HOOK_CONFIG not initialized");
                (cfg.rdp_epsilon_sq, cfg.min_gesture_length)
            });
            let patterns = HOOK_PATTERNS.with(|c| {
                c.borrow()
                    .as_ref()
                    .expect("HOOK_PATTERNS not initialized")
                    .clone() // Arc clone: ref-count bump, no allocation
            });

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
                        target_hwnd,
                        patterns,
                        rdp_epsilon_sq,
                        min_gesture_length,
                    });
                }
            });

            LRESULT(1)
        }
        _ => unsafe { CallNextHookEx(None, code, wparam, lparam) },
    }
}

fn handle_mouse_move(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
    origin: InputOrigin,
    x: i32,
    y: i32,
) -> LRESULT {
    if origin != InputOrigin::Physical {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let activated = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut()
            .expect("HOOK_STATE_MACHINE not initialized")
            .on_move(x, y)
    });

    if activated {
        HOOK_BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            if let Some(ref mut b) = *buf {
                b.add_force(Point { x, y });
            }
        });
        post_event(HookEvent::GestureStarted { x, y, monitor: 0 });
    }

    let in_drawing = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow()
            .as_ref()
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
                        Direction::N => 0,
                        Direction::NE => 1,
                        Direction::E => 2,
                        Direction::SE => 3,
                        Direction::S => 4,
                        Direction::SW => 5,
                        Direction::W => 6,
                        Direction::NW => 7,
                    };
                    let prev = HOOK_LAST_DIRECTION.with(|c| c.replace(dir_idx));
                    if prev != dir_idx {
                        post_event(HookEvent::DirectionChanged {
                            direction: dir,
                            x,
                            y,
                        });
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
            // Non-blocking send: if channel is full (4 pending),
            // drop the new completion rather than blocking the hook callback.
            if tx.try_send(completion).is_err() {
                log::warn!("Completion channel full — dropping gesture");
            }
        }
    });
}

/// Click replay packet.
#[derive(Debug, Clone)]
struct ClickReplay {
    release_point: Point,
}

fn enqueue_replay(replay: ClickReplay) {
    HOOK_REPLAY_TX.with(|tx| {
        if let Some(tx) = tx.get() {
            if tx.try_send(replay).is_err() {
                log::warn!("Replay channel full — dropping click");
            }
        }
    });
}

/// Resolve a PID to eligibility synchronously in the hook callback.
/// Called inline when the PID is not yet in the PolicySnapshot cache.
/// Uses OpenProcess + QueryFullProcessImageNameW + blacklist check.
fn resolve_pid_sync(pid: u32) -> Eligibility {
    use crate::config::BlacklistMode;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(_) => return Eligibility::Allowed,
    };

    let basename = unsafe {
        let mut len: u32 = 0;
        let _ = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(std::ptr::null_mut()),
            &mut len,
        );
        let mut buf: Vec<u16> = vec![0u16; len as usize + 1];
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return Eligibility::Allowed;
        }
        let slice = &buf[..len as usize];
        let sep = slice
            .iter()
            .rposition(|&c| c == b'\\' as u16 || c == b'/' as u16);
        let name_slice = match sep {
            Some(pos) => &slice[pos + 1..],
            None => slice,
        };
        String::from_utf16_lossy(name_slice).to_lowercase()
    };

    HOOK_CONFIG.with(|c| {
        if let Some(ref cfg) = *c.borrow() {
            let in_list = cfg.blacklist_apps.contains(&basename);
            match cfg.blacklist_mode {
                BlacklistMode::Blacklist => {
                    if in_list {
                        Eligibility::Denied
                    } else {
                        Eligibility::Allowed
                    }
                }
                BlacklistMode::Whitelist => {
                    if in_list {
                        Eligibility::Allowed
                    } else {
                        Eligibility::Denied
                    }
                }
            }
        } else {
            Eligibility::Allowed
        }
    })
}

/// Inject a synthetic right-click at absolute screen coordinates.
fn inject_synthetic_click(point: Point) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEINPUT,
    };

    let screen_w = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN,
        )
    };
    let screen_h = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
        )
    };
    let screen_x = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_XVIRTUALSCREEN,
        )
    };
    let screen_y = unsafe {
        windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_YVIRTUALSCREEN,
        )
    };

    // Validate screen dimensions; abort on zero
    if screen_w <= 0 || screen_h <= 0 {
        log::warn!(
            "inject_synthetic_click: screen dimensions invalid ({}x{}), aborting",
            screen_w,
            screen_h
        );
        return;
    }

    let x_abs = (((point.x - screen_x) as i64 * 65535i64) / screen_w as i64).clamp(0, 65535) as i32;
    let y_abs = (((point.y - screen_y) as i64 * 65535i64) / screen_h as i64).clamp(0, 65535) as i32;

    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x_abs,
                    dy: y_abs,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_RIGHTDOWN,
                    time: 0,
                    dwExtraInfo: SELF_TAG,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x_abs,
                    dy: y_abs,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_RIGHTUP,
                    time: 0,
                    dwExtraInfo: SELF_TAG,
                },
            },
        },
    ];

    let count = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if count != 2 {
        log::warn!(
            "SendInput returned {} of 2 events for synthetic click at ({},{})",
            count,
            point.x,
            point.y
        );
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
