#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use log::{error, info, warn};
use mouse_gesture::config::{ConfigFile, ConfigSnapshot};
use mouse_gesture::input_hook::{self, HookCommand, HookEvent};
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage,
    SetWindowLongPtrW, GetWindowLongPtrW, GWLP_USERDATA,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, MSG, PM_REMOVE,
    WINDOW_EX_STYLE, WS_OVERLAPPED,
    WM_CLOSE, WM_DESTROY, WM_QUERYENDSESSION, WM_TIMER,
};

const WINDOW_CLASS: &str = "MouseGestureDaemon\0";
const TIMER_GESTURE_ID: usize = 1;
const TIMER_GESTURE_MS: u32 = 5000;

struct DaemonState {
    config: Option<ConfigSnapshot>,
    hook_event_rx: Option<std::sync::mpsc::Receiver<HookEvent>>,
    worker_tx: Option<std::sync::mpsc::Sender<String>>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("Mouse Gesture Daemon v{} starting (Phase 2)", env!("CARGO_PKG_VERSION"));

    if let Err(e) = run() {
        error!("Fatal: {}", e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // PMv2
    set_pmv2_dpi_awareness()?;
    info!("PMv2 DPI awareness enabled");

    // Config
    let config = load_startup_config()?;

    // Window class
    register_window_class()?;

    // Hidden window
    let hwnd = create_notification_window()?;

    // Worker thread
    let (worker_tx, worker_rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new().name("worker".into()).spawn(move || {
        for name in worker_rx { info!("Action: {}", name); }
        info!("Worker stopped");
    })?;

    // Hook thread
    let (hook_handle, _hook_shared, hook_cmd_tx, hook_event_rx) =
        input_hook::spawn_hook_thread(3, 2);

    if let Some(ref cfg) = config {
        hook_cmd_tx.send(HookCommand::UpdateConfig(cfg.clone()))?;
        hook_cmd_tx.send(HookCommand::SetInterception(true))?;
        info!("Config loaded: {} gestures active", cfg.gestures.len());
    } else {
        warn!("No config — interception disabled");
    }

    // Timer
    unsafe { windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), TIMER_GESTURE_ID, TIMER_GESTURE_MS, None); }

    // State
    let state = Arc::new(Mutex::new(DaemonState {
        config,
        hook_event_rx: Some(hook_event_rx),
        worker_tx: Some(worker_tx),
    }));
    let state_ptr = Arc::into_raw(state);
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }

    info!("Message pump running");
    let code = message_pump(hwnd);

    // Cleanup
    hook_cmd_tx.send(HookCommand::Shutdown).ok();
    let _ = hook_handle.join();
    drop(unsafe { Arc::from_raw(state_ptr) });

    info!("Exit (code {})", code);
    Ok(())
}

// ── Window ────────────────────────────────────────────────────

fn register_window_class() -> Result<()> {
    let name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let wc = windows::Win32::UI::WindowsAndMessaging::WNDCLASSEXW {
        cbSize: std::mem::size_of::<windows::Win32::UI::WindowsAndMessaging::WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: HINSTANCE(std::ptr::null_mut()),
        lpszClassName: windows::core::PCWSTR::from_raw(name.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
    Ok(())
}

fn create_notification_window() -> Result<HWND> {
    let name: Vec<u16> = WINDOW_CLASS.encode_utf16().collect();
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR::from_raw(name.as_ptr()),
            windows::core::w!("MouseGestureDaemon"),
            WS_OVERLAPPED,
            CW_USEDEFAULT, CW_USEDEFAULT, 0, 0,
            None, None, None, None,
        )?
    };
    Ok(hwnd)
}

// ── Message Pump ───────────────────────────────────────────────

fn message_pump(hwnd: HWND) -> i32 {
    let mut msg = MSG::default();
    loop {
        // Process hook events
        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
        if !state_ptr.is_null() {
            let state = unsafe { &*state_ptr };
            if let Ok(guard) = state.lock() {
                if let Some(ref rx) = guard.hook_event_rx {
                    while let Ok(event) = rx.try_recv() {
                        handle_hook_event(&guard, &event);
                    }
                }
            }
        }

        unsafe {
            if PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == 0x0012 { return msg.wParam.0 as i32; }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        if msg.message == 0x0012 { return msg.wParam.0 as i32; }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn handle_hook_event(state: &DaemonState, event: &HookEvent) {
    match event {
        HookEvent::GestureEnded { matched: true, gesture_name } => {
            let name = gesture_name.as_deref().unwrap_or("?");
            info!("Gesture: {}", name);
            if let Some(ref tx) = state.worker_tx {
                tx.send(name.to_string()).ok();
            }
        }
        HookEvent::GestureEnded { matched: false, .. } => info!("Gesture: unmatched"),
        HookEvent::ReplaySyntheticClick { x, y } => {
            inject_synthetic_click(*x, *y);
        }
        HookEvent::GestureStarted { .. } | HookEvent::TrailPoint { .. } => {}
        HookEvent::Error(msg) => error!("Hook: {}", msg),
    }
}

fn inject_synthetic_click(x: i32, y: i32) {
    use mouse_gesture::state_machine::SELF_TAG;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_MOUSE, MOUSEINPUT,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    };
    let inputs = [
        INPUT { r#type: INPUT_MOUSE, Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            mi: MOUSEINPUT { dx: x, dy: y, mouseData: 0, dwFlags: MOUSEEVENTF_RIGHTDOWN, time: 0, dwExtraInfo: SELF_TAG }
        }},
        INPUT { r#type: INPUT_MOUSE, Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
            mi: MOUSEINPUT { dx: x, dy: y, mouseData: 0, dwFlags: MOUSEEVENTF_RIGHTUP, time: 0, dwExtraInfo: SELF_TAG }
        }},
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
}

// ── Window Proc ────────────────────────────────────────────────

unsafe extern "system" fn window_proc(
    hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY | WM_CLOSE => { PostQuitMessage(0); LRESULT(0) }
        WM_QUERYENDSESSION => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ── Config ─────────────────────────────────────────────────────

fn config_path() -> std::path::PathBuf {
    std::env::var("APPDATA")
        .map(|d| std::path::PathBuf::from(d).join("mouse-gesture").join("config.toml"))
        .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"))
}

fn load_startup_config() -> Result<Option<ConfigSnapshot>> {
    let path = config_path();
    match ConfigFile::load(&path) {
        Ok(cfg) => match cfg.compile(1, 96) {
            Ok(s) => { info!("Config: {}", path.display()); Ok(Some(s)) }
            Err(e) => { error!("Invalid config: {}. Interception disabled.", e); Ok(None) }
        },
        Err(e) => { warn!("No config at {}: {}. Interception disabled.", path.display(), e); Ok(None) }
    }
}

// ── DPI ────────────────────────────────────────────────────────

fn set_pmv2_dpi_awareness() -> Result<()> {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)?; }
    Ok(())
}
