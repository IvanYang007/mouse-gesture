#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use log::{error, info, warn};
use mouse_gesture::config::{ConfigFile, ConfigSnapshot};
use mouse_gesture::input_hook::{self, HookCommand, HookEvent};
use mouse_gesture::tray::{TrayIcon, TrayState};
use mouse_gesture::lifecycle;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage,
    SetWindowLongPtrW, GetWindowLongPtrW, GWLP_USERDATA,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, MSG,
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
    tray: Option<TrayIcon>,
    hook_cmd_tx: Option<std::sync::mpsc::Sender<HookCommand>>,
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
    // Singleton
    if let Err(e) = lifecycle::acquire_singleton() {
        warn!("{}", e);
        std::process::exit(0);
    }

    // PMv2
    set_pmv2_dpi_awareness()?;
    info!("PMv2 DPI awareness enabled");

    // Config
    let config = load_startup_config()?;

    // Window class
    register_window_class()?;

    // Hidden window
    let hwnd = create_notification_window()?;

    // Session notifications
    lifecycle::register_session_notifications(hwnd)?;

    // Tray
    let tray = TrayIcon::new(hwnd)?;
    let gesture_count = config.as_ref().map(|c| c.gestures.len()).unwrap_or(0);
    if config.is_some() {
        tray.update_status(&TrayState::Active { gesture_count })?;
    } else {
        tray.update_status(&TrayState::Disabled { reason: "No valid config".into() })?;
    }

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

    // Config watcher thread
    let cfg_path = config_path();
    let cfg_cmd_tx = hook_cmd_tx.clone();
    std::thread::Builder::new().name("config-watcher".into()).spawn(move || {
        watch_config(cfg_path, cfg_cmd_tx);
    })?;

    // Timer
    unsafe { windows::Win32::UI::WindowsAndMessaging::SetTimer(Some(hwnd), TIMER_GESTURE_ID, TIMER_GESTURE_MS, None); }

    // State
    let state = Arc::new(Mutex::new(DaemonState {
        config,
        hook_event_rx: Some(hook_event_rx),
        worker_tx: Some(worker_tx),
        tray: Some(tray),
        hook_cmd_tx: Some(hook_cmd_tx.clone()),
    }));
    let state_ptr = Arc::into_raw(state);
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize); }

    info!("Message pump running");
    let code = message_pump(hwnd);

    // Cleanup
    let _state = unsafe { Arc::from_raw(state_ptr) };
    if let Ok(guard) = _state.lock() {
        if let Some(ref tray) = guard.tray {
            let _ = tray.remove();
        }
    }
    hook_cmd_tx.send(HookCommand::Shutdown).ok();
    let _ = hook_handle.join();
    lifecycle::unregister_session_notifications(hwnd);

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
    loop {
        let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
        if !state_ptr.is_null() {
            let state = unsafe { &*state_ptr };
            let mut pending_events = Vec::new();
            let (worker_tx, config) = if let Ok(guard) = state.lock() {
                if let Some(ref rx) = guard.hook_event_rx {
                    while let Ok(event) = rx.try_recv() {
                        pending_events.push(event);
                    }
                }
                (guard.worker_tx.clone(), guard.config.clone())
            } else {
                (None, None)
            };
            for event in &pending_events {
                dispatch_hook_event(event, &worker_tx, &config);
            }
        }

        // Block on GetMessage for Windows messages
        let mut msg = MSG::default();
        unsafe {
            let ret = GetMessageW(&mut msg, None, 0, 0);
            if ret.0 == 0 || ret.0 == -1 {
                return msg.wParam.0 as i32;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn dispatch_hook_event(
    event: &HookEvent,
    worker_tx: &Option<std::sync::mpsc::Sender<String>>,
    config: &Option<ConfigSnapshot>,
) {
    match event {
        HookEvent::GestureEnded { matched: true, gesture_name } => {
            let name = gesture_name.as_deref().unwrap_or("?");
            info!("Gesture: {}", name);
            if let Some(ref tx) = worker_tx {
                tx.send(name.to_string()).ok();
            }
            // Phase 3: Execute the action
            if let Some(ref cfg) = config {
                execute_gesture_action(cfg, name);
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

/// Look up a gesture by name in the config and execute its action.
fn execute_gesture_action(config: &ConfigSnapshot, gesture_name: &str) {
    // Try window commands
    for (name, cmd) in &config.window_commands {
        if name == gesture_name {
            info!("Window action: {:?}", cmd);
            execute_window_action(cmd);
            return;
        }
    }
    // Try keyboard shortcuts
    if let Some(inputs) = config.key_map.get(gesture_name) {
        info!("Keyboard action: {} inputs", inputs.len());
        execute_keyboard_action(inputs);
        return;
    }
    // Try launch actions
    for (name, path, args) in &config.launch_actions {
        if name == gesture_name {
            info!("Launch action: {} {:?}", path, args);
            execute_launch_action(path, args);
            return;
        }
    }
    warn!("Gesture '{}' has no compiled action", gesture_name);
}

use mouse_gesture::config::WindowCommand;

fn execute_window_action(cmd: &WindowCommand) {
    use mouse_gesture::window_ops::{enumerate_monitors, snap_rect, SnapPosition};
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, SetWindowPos, ShowWindowAsync,
        PostMessageW, SW_MINIMIZE, SW_MAXIMIZE, SW_RESTORE, HWND_TOP, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE};
    use windows::Win32::UI::WindowsAndMessaging::WM_CLOSE;
    use windows::Win32::Foundation::{WPARAM, LPARAM};

    let hwnd = unsafe { GetForegroundWindow() };
    let monitors = enumerate_monitors().unwrap_or_default();
    let current_monitor = monitors.first();

    match cmd {
        WindowCommand::Maximize => { unsafe { ShowWindowAsync(hwnd, SW_MAXIMIZE); } }
        WindowCommand::Minimize => { unsafe { ShowWindowAsync(hwnd, SW_MINIMIZE); } }
        WindowCommand::Restore => { unsafe { ShowWindowAsync(hwnd, SW_RESTORE); } }
        WindowCommand::Close => {
            unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM::default(), LPARAM::default()); }
        }
        WindowCommand::SnapLeft => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::Left);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapRight => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::Right);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapTop => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::Top);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapBottom => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::Bottom);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapTopLeft => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::TopLeft);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapTopRight => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::TopRight);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapBottomLeft => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::BottomLeft);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::SnapBottomRight => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::BottomRight);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::Center => {
            if let Some(m) = current_monitor {
                let r = snap_rect(m, SnapPosition::Center);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
        WindowCommand::ToggleAlwaysOnTop => {
            info!("ToggleAlwaysOnTop not yet implemented");
        }
        WindowCommand::MoveToMonitor(n) => {
            if let Some(m) = monitors.get(*n as usize) {
                let r = snap_rect(m, SnapPosition::Center);
                unsafe { SetWindowPos(hwnd, Some(HWND_TOP), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE); }
            }
        }
    }
}

fn execute_keyboard_action(inputs: &[mouse_gesture::config::CompiledInput]) {
    use mouse_gesture::input_inject::{check_modifier_conflict, inject_key_shortcut};
    if let Err(e) = check_modifier_conflict(inputs) {
        warn!("Modifier conflict: {}. Skipping keyboard action.", e);
        return;
    }
    if let Err(e) = inject_key_shortcut(inputs) {
        error!("Keyboard injection failed: {}", e);
    }
}

fn execute_launch_action(path: &str, args: &[String]) {
    use mouse_gesture::launch::{launch_executable, shell_open};
    if args.is_empty() && (path.starts_with("http://") || path.starts_with("https://") || path.contains('.')) {
        // Treat URLs and paths with extensions as shell open
        if let Err(e) = shell_open(path) {
            error!("Shell open failed for '{}': {}", path, e);
        }
    } else {
        if let Err(e) = launch_executable(path, args) {
            error!("Launch failed for '{}': {}", path, e);
        }
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
        msg if msg == lifecycle::WM_WTSSESSION_CHANGE => {
            let event = wparam.0 as usize;
            if lifecycle::is_session_lock(event) {
                info!("Session locked — disabling interception");
                let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
                if !state_ptr.is_null() {
                    if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                        if let Some(ref tray) = guard.tray {
                            let _ = tray.update_status(&TrayState::Disabled { reason: "Session locked".into() });
                        }
                        if let Some(ref tx) = guard.hook_cmd_tx {
                            let _ = tx.send(HookCommand::SetInterception(false));
                        }
                    }
                }
            } else if lifecycle::is_session_unlock(event) {
                info!("Session unlocked — re-enabling interception");
                let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
                if !state_ptr.is_null() {
                    if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                        if let Some(ref tray) = guard.tray {
                            let _ = tray.update_status(&TrayState::Active { gesture_count: guard.config.as_ref().map(|c| c.gestures.len()).unwrap_or(0) });
                        }
                        if let Some(ref tx) = guard.hook_cmd_tx {
                            let _ = tx.send(HookCommand::SetInterception(true));
                        }
                    }
                }
            }
            LRESULT(0)
        }
        _ => {
            // Handle tray callback
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
            if !state_ptr.is_null() {
                if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                    if let Some(ref tray) = guard.tray {
                        if msg == tray.callback_msg() {
                            match lparam.0 as u32 {
                                0x0205 => { // WM_RBUTTONUP on tray
                                    info!("Tray right-click — opening config");
                                    let path = config_path();
                                    let _ = mouse_gesture::launch::shell_open(&path.to_string_lossy());
                                }
                                _ => {}
                            }
                            return LRESULT(0);
                        }
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
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

/// Poll the config file periodically for changes.
fn watch_config(path: std::path::PathBuf, cmd_tx: std::sync::mpsc::Sender<HookCommand>) {
    use std::time::Duration;
    let mut last_modified = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
    loop {
        std::thread::sleep(Duration::from_secs(2));
        match std::fs::metadata(&path) {
            Ok(meta) => {
                let modified = meta.modified().ok();
                if modified != last_modified {
                    last_modified = modified;
                    match ConfigFile::load(&path) {
                        Ok(cfg) => match cfg.compile(1, 96) {
                            Ok(snapshot) => {
                                info!("Config reloaded: {} gestures", snapshot.gestures.len());
                                let _ = cmd_tx.send(HookCommand::UpdateConfig(snapshot));
                                let _ = cmd_tx.send(HookCommand::SetInterception(true));
                            }
                            Err(e) => warn!("Config reload failed: {}. Keeping previous.", e),
                        },
                        Err(e) => warn!("Config read failed: {}", e),
                    }
                }
            }
            Err(_) => {} // file removed, keep polling
        }
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
