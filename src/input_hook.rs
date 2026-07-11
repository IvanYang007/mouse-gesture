//! Input hook thread — owns WH_MOUSE_LL, the message pump,
//! and the gesture buffer. The hook callback runs in the
//! context of the message pump and must return quickly.
//!
//! Restrictions in the hook proc:
//!   - No heap allocation
//!   - No filesystem or process access
//!   - No logging
//!   - No mutexes
//!   - No window actions
//!   - No rendering

use crate::config::ConfigSnapshot;
use crate::gesture::{GestureBuffer, GestureResult, classify};
use crate::state_machine::{DownResult, GestureContext, StateMachine, UpResult, SELF_TAG};
use crate::win_handles::SendHHOOK;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
    HHOOK, HOOKPROC, MSG, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_MOUSEMOVE, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    LLMHF_INJECTED,
};

/// Event sent from the hook thread to the UI thread.
#[derive(Debug, Clone)]
pub enum HookEvent {
    /// Gesture lifecycle event.
    GestureStarted { x: i32, y: i32, monitor: isize },
    /// Trail point update (coalesced).
    TrailPoint { x: i32, y: i32 },
    /// Gesture completed with match result.
    GestureEnded {
        matched: bool,
        gesture_name: Option<String>,
    },
    /// Synthetic click scheduling request.
    ReplaySyntheticClick { x: i32, y: i32 },
    /// Error — hook or recognition failure.
    Error(String),
}

/// Configuration push from UI thread to hook thread.
#[derive(Debug, Clone)]
pub enum HookCommand {
    /// Push a new config snapshot.
    UpdateConfig(ConfigSnapshot),
    /// Enable/disable interception.
    SetInterception(bool),
    /// Request shutdown.
    Shutdown,
}

/// Shared state between hook thread and main thread.
pub struct HookShared {
    /// Set by main thread to signal shutdown.
    pub shutdown: AtomicBool,
    /// Set by hook thread when ready.
    pub ready: AtomicBool,
    /// Set when interception is enabled.
    pub interception_enabled: AtomicBool,
}

/// Spawn the input hook thread. Returns a JoinHandle and the shared state.
///
/// The hook thread runs its own message pump. The main thread can send
/// commands via the returned channel sender.
pub fn spawn_hook_thread(
    activation_threshold: i32,
    sample_distance: i32,
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
            let hook_proc = Some(create_hook_proc(event_tx.clone()));
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_MOUSE_LL,
                    hook_proc,
                    windows::Win32::Foundation::HINSTANCE::default(),
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

            // Message pump — required for low-level hooks
            let mut msg = MSG::default();
            loop {
                if shared_clone.shutdown.load(Ordering::SeqCst) {
                    break;
                }

                // Check for incoming commands (non-blocking)
                if let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        HookCommand::Shutdown => break,
                        HookCommand::UpdateConfig(snapshot) => {
                            // Stored in a thread-local for access by the hook proc
                            HOOK_CONFIG.with(|c| {
                                *c.borrow_mut() = Some(snapshot);
                            });
                        }
                        HookCommand::SetInterception(enabled) => {
                            shared_clone.interception_enabled.store(enabled, Ordering::SeqCst);
                        }
                    }
                }

                // Process one message (non-blocking poll)
                unsafe {
                    let ret = GetMessageW(&mut msg, HWND::default(), 0, 0);
                    if ret.0 == 0 || ret.0 == -1 {
                        // WM_QUIT or error
                        break;
                    }
                    // The hook proc handles messages via the hook chain,
                    // but we still need to dispatch for timer messages etc.
                }
            }

            // Cleanup
            unsafe {
                let _ = UnhookWindowsHookEx(hook);
            }
            log::info!("Hook thread shut down");
        })
        .expect("spawn hook thread");

    (handle, shared, cmd_tx, event_rx)
}

// ── Thread-local config for the hook proc ──────────────────────

thread_local! {
    static HOOK_CONFIG: std::cell::RefCell<Option<ConfigSnapshot>> = const { std::cell::RefCell::new(None) };
    static HOOK_BUFFER: std::cell::RefCell<Option<GestureBuffer>> = const { std::cell::RefCell::new(None) };
    static HOOK_STATE_MACHINE: std::cell::RefCell<Option<StateMachine>> = const { std::cell::RefCell::new(None) };
}

// ── Hook Procedure ─────────────────────────────────────────────

fn create_hook_proc(
    event_tx: std::sync::mpsc::Sender<HookEvent>,
) -> HOOKPROC {
    // Wrap in catch_unwind for panic safety
    let tx = event_tx;

    // We use a static channel since the hook proc is an extern "system" fn
    // that can't capture closures. Instead we use a thread-local channel.
    HOOK_EVENT_TX.with(|cell| {
        cell.set(Some(tx)).expect("HOOK_EVENT_TX already set");
    });

    Some(low_level_mouse_proc)
}

thread_local! {
    static HOOK_EVENT_TX: std::cell::OnceCell<std::sync::mpsc::Sender<HookEvent>> = const { std::cell::OnceCell::new() };
}

unsafe extern "system" fn low_level_mouse_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(HHOOK::default(), code, wparam, lparam);
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_mouse_event(code, wparam, lparam)
    }));

    match result {
        Ok(lresult) => lresult,
        Err(_) => {
            // Panic in hook proc — pass through to avoid breaking input
            CallNextHookEx(HHOOK::default(), code, wparam, lparam)
        }
    }
}

fn process_mouse_event(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let msg = wparam.0 as u32;

    // We only care about right-button events and mouse move during a gesture.
    // All other events pass through immediately.
    let is_right = msg == WM_RBUTTONDOWN || msg == WM_RBUTTONUP;
    let is_move = msg == WM_MOUSEMOVE;

    let in_gesture = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow().as_ref().map(|s| s.state != crate::state_machine::State::Idle)
            .unwrap_or(false)
    });

    if !is_right && !(is_move && in_gesture) {
        return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
    }

    // Parse the MSLLHOOKSTRUCT
    let hook_data = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let is_injected = (hook_data.flags & LLMHF_INJECTED.0 as u32) != 0;
    let is_self = hook_data.dwExtraInfo == SELF_TAG;

    if is_self {
        return unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) };
    }

    let x = hook_data.pt.x;
    let y = hook_data.pt.y;

    if msg == WM_RBUTTONDOWN {
        return handle_right_down(code, wparam, lparam, is_injected, x, y);
    } else if msg == WM_RBUTTONUP {
        return handle_right_up(code, wparam, lparam);
    } else if msg == WM_MOUSEMOVE {
        return handle_mouse_move(code, wparam, lparam, x, y);
    }

    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

fn handle_right_down(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    is_injected: bool, x: i32, y: i32,
) -> LRESULT {
    // Check if we have a valid config
    let is_eligible = HOOK_CONFIG.with(|c| {
        c.borrow().as_ref().map(|cfg| {
            // In a full implementation, we'd look up the PID from the
            // target HWND and check the blacklist. For now, all apps
            // are eligible when a config is loaded.
            true
        }).unwrap_or(false)
    });

    // Determine the target window (simplified — full impl uses WindowFromPoint)
    let target_hwnd = HWND::default();

    let ctx = if is_eligible {
        Some(GestureContext {
            target_hwnd,
            target_pid: 0, // filled by WindowFromPoint + GetWindowThreadProcessId
            foreground_hwnd: HWND::default(),
            start_point: POINT { x, y },
            origin_monitor: 0, // filled by MonitorFromPoint
            origin_dpi: 96,     // filled by GetDpiForWindow
            config_generation: HOOK_CONFIG.with(|c| {
                c.borrow().as_ref().map(|cfg| cfg.generation).unwrap_or(0)
            }),
        })
    } else {
        None
    };

    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        let sm = sm.as_mut().unwrap();
        sm.on_right_down(is_injected, is_eligible, ctx)
    });

    match result {
        DownResult::Consumed => {
            // Suppress the event
            LRESULT(1)
        }
        DownResult::PassThrough | DownResult::Injected => unsafe {
            CallNextHookEx(HHOOK::default(), code, wparam, lparam)
        },
    }
}

fn handle_right_up(
    code: i32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    let result = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        let sm = sm.as_mut().unwrap();
        sm.on_right_up()
    });

    match result {
        UpResult::ReplaySynthetic => {
            // Schedule synthetic click via event channel
            HOOK_EVENT_TX.with(|tx| {
                if let Some(tx) = tx.get() {
                    let _ = tx.send(HookEvent::ReplaySyntheticClick { x: 0, y: 0 });
                }
            });
            // Consume the original event
            LRESULT(1)
        }
        UpResult::GestureComplete => {
            // Classify the gesture — extract data from thread-locals
            // before nesting closures to avoid deep borrow chains.
            let classification = HOOK_BUFFER.with(|buf_cell| {
                // First, classify using an immutable borrow
                let result = {
                    let buf = buf_cell.borrow();
                    let buf_ref = buf.as_ref().expect("HOOK_BUFFER not initialized");
                    HOOK_CONFIG.with(|cfg_cell| {
                        let cfg = cfg_cell.borrow();
                        let cfg_inner = cfg.as_ref().expect("HOOK_CONFIG not initialized");
                        classify(buf_ref, &cfg_inner.gestures, cfg_inner.rdp_epsilon_sq, cfg_inner.min_gesture_length)
                    })
                };
                // Now clear the buffer with a mutable borrow (immutable borrow dropped above)
                let mut buf = buf_cell.borrow_mut();
                if let Some(ref mut b) = *buf {
                    b.clear();
                }
                result
            });

            match classification {
                GestureResult::Matched { name, .. } => {
                    HOOK_EVENT_TX.with(|tx| {
                        if let Some(tx) = tx.get() {
                            let _ = tx.send(HookEvent::GestureEnded {
                                matched: true,
                                gesture_name: Some(name),
                            });
                        }
                    });
                }
                _ => {
                    HOOK_EVENT_TX.with(|tx| {
                        if let Some(tx) = tx.get() {
                            let _ = tx.send(HookEvent::GestureEnded {
                                matched: false,
                                gesture_name: None,
                            });
                        }
                    });
                }
            }

            LRESULT(1)
        }
        UpResult::PassThrough => unsafe {
            CallNextHookEx(HHOOK::default(), code, wparam, lparam)
        },
        UpResult::Ignored => unsafe {
            CallNextHookEx(HHOOK::default(), code, wparam, lparam)
        },
    }
}

fn handle_mouse_move(
    code: i32, wparam: WPARAM, lparam: LPARAM,
    x: i32, y: i32,
) -> LRESULT {
    let activated = HOOK_STATE_MACHINE.with(|sm| {
        let mut sm = sm.borrow_mut();
        let sm = sm.as_mut().unwrap();
        sm.on_move(x, y)
    });

    if activated {
        // Gesture is now in Drawing state — start buffering points
        let _ = HOOK_EVENT_TX.with(|tx| {
            if let Some(tx) = tx.get() {
                let _ = tx.send(HookEvent::GestureStarted {
                    x, y,
                    monitor: 0,
                });
            }
        });
    }

    // Always buffer points when in Drawing state
    let in_drawing = HOOK_STATE_MACHINE.with(|sm| {
        sm.borrow().as_ref()
            .map(|s| s.state == crate::state_machine::State::Drawing)
            .unwrap_or(false)
    });

    if in_drawing {
        HOOK_BUFFER.with(|buf| {
            let mut buf = buf.borrow_mut();
            if let Some(ref mut buf) = *buf {
                buf.add_point(crate::gesture::Point { x, y });
            }
        });
    }

    // Always allow cursor movement to continue
    unsafe { CallNextHookEx(HHOOK::default(), code, wparam, lparam) }
}

// ── Initialization ─────────────────────────────────────────────

/// Initialize the hook thread-local state with current config values.
pub fn init_hook_state(
    activation_threshold: i32,
    sample_distance: i32,
) {
    HOOK_STATE_MACHINE.with(|sm| {
        *sm.borrow_mut() = Some(StateMachine::new(activation_threshold));
    });
    HOOK_BUFFER.with(|buf| {
        *buf.borrow_mut() = Some(GestureBuffer::new(sample_distance));
    });
}
