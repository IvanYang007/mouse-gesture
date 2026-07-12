//! Input hook thread — owns WH_MOUSE_LL, the message pump,
//! and the gesture buffer.

use crate::config::ConfigSnapshot;
use crate::config::Direction;
use crate::gesture::{GestureBuffer, GestureResult, Point, classify, direction_from_points, encode_directions};
use crate::state_machine::{DownResult, GestureContext, StateMachine, UpResult};
pub use crate::state_machine::SELF_TAG;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    HOOKPROC, MSG, MSLLHOOKSTRUCT, GetForegroundWindow,
    WH_MOUSE_LL, WM_MOUSEMOVE,
    WM_RBUTTONDOWN, WM_RBUTTONUP,
    LLMHF_INJECTED,
};

/// Event sent from the hook thread to the UI thread.
#[derive(Debug, Clone)]
pub enum HookEvent {
    GestureStarted { x: i32, y: i32, monitor: isize },
    TrailPoint { x: i32, y: i32 },
    GestureEnded { matched: bool, gesture_name: Option<String> },
    ReplaySyntheticClick { x: i32, y: i32 },
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

/// Shared state between hook thread and main thread.
pub struct HookShared {
    pub shutdown: AtomicBool,
    pub ready: AtomicBool,
    pub interception_enabled: AtomicBool,
}

/// Spawn the input hook thread.
pub fn spawn_hook_thread(
    activation_threshold: i32,
    sample_distance: i32,
    ui_hwnd_raw: isize,
) -> (
    thread::JoinHandle<()>,
    Arc<HookShared>,
    std::sync::mpsc::Sender<HookCommand>,
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

    let handle = thread::Builder::new()
        .name("mouse-hook".into())
        .spawn(move || {
            let ui_hwnd = HWND(ui_hwnd_raw as *mut _);
            init_hook_state(activation_threshold, sample_distance);
            let hook_proc = create_hook_proc(event_tx.clone(), shared_clone.clone(), ui_hwnd);
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_MOUSE_LL,
                    hook_proc,
                    None,
                    0, // system-wide
                )
            };

            let hook = match hook {
                Ok(h) => h,
                Err(e) => {
                    log::error!("SetWindowsHookExW failed: {:?}", e);
                    return;
                }
            };

            shared_clone.ready.store(true, Ordering::SeqCst);

            let mut msg = MSG::default();
            loop {
                if shared_clone.shutdown.load(Ordering::SeqCst) {
                    break;
                }

                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        HookCommand::Shutdown => break,
                        HookCommand::UpdateConfig(snapshot) => {
                            HOOK_CONFIG.with(|c| {
                                *c.borrow_mut() = Some(snapshot);
                            });
                        }
                        HookCommand::SetInterception(enabled) => {
                            shared_clone.interception_enabled.store(enabled, Ordering::SeqCst);
                            HOOK_INTERCEPTION_ENABLED.with(|c| { *c.borrow_mut() = enabled; });
                        }
                        HookCommand::ForceReset => {
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

                unsafe {
                    let ret = GetMessageW(&mut msg, None, 0, 0);
                    if ret.0 == 0 || ret.0 == -1 {
                        break;
                    }
                }
            }

            unsafe { let _ = UnhookWindowsHookEx(hook); }
            log::info!("Hook thread shut down");
        })
        .expect("spawn hook thread");

    (handle, shared, cmd_tx, event_rx)
}

// ── Thread-local state ─────────────────────────────────────────

thread_local! {
    static HOOK_CONFIG: std::cell::RefCell<Option<ConfigSnapshot>> = const { std::cell::RefCell::new(None) };
    static HOOK_BUFFER: std::cell::RefCell<Option<GestureBuffer>> = const { std::cell::RefCell::new(None) };
    static HOOK_STATE_MACHINE: std::cell::RefCell<Option<StateMachine>> = const { std::cell::RefCell::new(None) };
    static HOOK_EVENT_TX: std::cell::OnceCell<std::sync::mpsc::Sender<HookEvent>> = const { std::cell::OnceCell::new() };
    static HOOK_INTERCEPTION_ENABLED: std::cell::RefCell<bool> = const { std::cell::RefCell::new(false) };
    static HOOK_UI_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    static HOOK_LAST_DIRECTION: std::cell::Cell<u8> = const { std::cell::Cell::new(8) };
}

// ── Hook Procedure ─────────────────────────────────────────────

fn create_hook_proc(event_tx: std::sync::mpsc::Sender<HookEvent>, shared: Arc<HookShared>, ui_hwnd: HWND) -> HOOKPROC {
    HOOK_EVENT_TX.with(|cell| {
        cell.set(event_tx).expect("HOOK_EVENT_TX already set");
    });
    HOOK_INTERCEPTION_ENABLED.with(|c| {
        *c.borrow_mut() = shared.interception_enabled.load(Ordering::Relaxed);
    });
    HOOK_UI_HWND.with(|c| { c.set(ui_hwnd.0 as isize); });
    Some(low_level_mouse_proc)
}

/// Send an event to the UI thread and wake its message pump immediately.
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

unsafe extern "system" fn low_level_mouse_proc(
    code: i32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    if !HOOK_INTERCEPTION_ENABLED.with(|c| *c.borrow()) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_mouse_event(code, wparam, lparam)
    }));
    match result {
        Ok(lresult) => lresult,
        Err(_) => CallNextHookEx(None, code, wparam, lparam),
    }
}

fn process_mouse_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let msg = wparam.0 as u32;
    let is_right = msg == WM_RBUTTONDOWN || msg == WM_RBUTTONUP;
    let is_move = msg == WM_MOUSEMOVE;

    let in_gesture = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow().as_ref()
            .map(|s| s.state != crate::state_machine::State::Idle)
            .unwrap_or(false)
    });

    if !is_right && !(is_move && in_gesture) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let hook_data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let is_injected = (hook_data.flags & LLMHF_INJECTED) != 0;
    if hook_data.dwExtraInfo == SELF_TAG {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let x = hook_data.pt.x;
    let y = hook_data.pt.y;

    if msg == WM_RBUTTONDOWN {
        handle_right_down(code, wparam, lparam, is_injected, x, y)
    } else if msg == WM_RBUTTONUP {
        handle_right_up(code, wparam, lparam, x, y)
    } else {
        handle_mouse_move(code, wparam, lparam, x, y)
    }
}

fn handle_right_down(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    is_injected: bool, x: i32, y: i32,
) -> LRESULT {
    // Resolve the window under cursor
    let (target_hwnd, target_pid) = resolve_window_under_cursor(x, y);

    // Determine eligibility (including blacklist/whitelist)
    let is_eligible = HOOK_CONFIG.with(|c| {
        c.borrow().as_ref().map(|cfg| {
            let basic = target_pid != 0 && target_pid != std::process::id();
            if !basic { return false; }
            crate::app_policy::resolve_pid(target_pid, cfg)
                .map(|e| e.is_eligible)
                .unwrap_or(true)
        }).unwrap_or(false)
    });

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
        sm.as_mut().expect("HOOK_STATE_MACHINE not initialized").on_right_down(is_injected, is_eligible, ctx)
    });

    match result {
        DownResult::Consumed => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));
            LRESULT(1)
        }
        _ => unsafe { CallNextHookEx(None, code, wparam, lparam) },
    }
}

/// Resolve HWND and PID of the window under cursor coordinates.
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
    x: i32, y: i32,
) -> LRESULT {
    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut().expect("HOOK_STATE_MACHINE not initialized").on_right_up()
    });

    match result {
        UpResult::ReplaySynthetic => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));
            post_event(HookEvent::ReplaySyntheticClick { x, y });
            LRESULT(1)
        }
        UpResult::GestureComplete => {
            HOOK_LAST_DIRECTION.with(|c| c.set(8));
            // Check if config was hot-reloaded mid-gesture
            let current_gen = HOOK_CONFIG.with(|c| {
                c.borrow().as_ref().map(|cfg| cfg.generation).unwrap_or(0)
            });
            let captured_gen = HOOK_STATE_MACHINE.with(|sm| {
                sm.borrow().as_ref().map(|s| s.current_generation()).unwrap_or(0)
            });
            let config_changed = current_gen != captured_gen;

            let classification = HOOK_BUFFER.with(|buf_cell| {
                let result = {
                    let buf = buf_cell.borrow();
                    let buf_ref = buf.as_ref().expect("HOOK_BUFFER not initialized");
                    if config_changed {
                        crate::gesture::GestureResult::NoMatch
                    } else {
                        HOOK_CONFIG.with(|cfg_cell| {
                            let cfg = cfg_cell.borrow();
                            let cfg_inner = cfg.as_ref().expect("HOOK_CONFIG not initialized");
                            classify(buf_ref, &cfg_inner.gestures, cfg_inner.rdp_epsilon_sq, cfg_inner.min_gesture_length)
                        })
                    }
                };
                let mut buf = buf_cell.borrow_mut();
                if let Some(ref mut b) = *buf { b.clear(); }
                result
            });

            match classification {
                GestureResult::Matched { name, .. } => {
                    post_event(HookEvent::GestureEnded {
                        matched: true, gesture_name: Some(name),
                    });
                }
                _ => {
                    post_event(HookEvent::GestureEnded {
                        matched: false, gesture_name: None,
                    });
                }
            }
            LRESULT(1)
        }
        _ => unsafe { CallNextHookEx(None, code, wparam, lparam) },
    }
}

fn handle_mouse_move(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    x: i32, y: i32,
) -> LRESULT {
    let activated = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.as_mut().expect("HOOK_STATE_MACHINE not initialized").on_move(x, y)
    });

    if activated {
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

/// Initialize the hook thread-local state.
pub fn init_hook_state(activation_threshold: i32, sample_distance: i32) {
    HOOK_STATE_MACHINE.with(|sm| {
        *sm.borrow_mut() = Some(StateMachine::new(activation_threshold));
    });
    HOOK_BUFFER.with(|buf| {
        *buf.borrow_mut() = Some(GestureBuffer::new(sample_distance));
    });
}
