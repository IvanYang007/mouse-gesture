#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use log::{error, info, warn, debug};
use mouse_gesture::config::{ConfigFile, ConfigSnapshot};
use mouse_gesture::input_hook::{self, HookCommand, HookController, HookEvent};
use mouse_gesture::tray::{TrayIcon, TrayState};
use mouse_gesture::lifecycle;
use mouse_gesture::overlay::OverlayWindow;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage,
    SetWindowLongPtrW, GetWindowLongPtrW, GWLP_USERDATA,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, MSG,
    WINDOW_EX_STYLE, WS_OVERLAPPED,
    WM_CLOSE, WM_DESTROY, WM_QUERYENDSESSION,
};

const WINDOW_CLASS: &str = "MouseGestureDaemon\0";

#[derive(Debug, Clone)]
enum ActionJob {
    Window { name: String, cmd: mouse_gesture::config::WindowCommand },
    Keyboard { name: String, inputs: Vec<mouse_gesture::config::CompiledInput> },
    Launch { name: String, path: String, args: Vec<String> },
}

struct DaemonState {
    config: Option<ConfigSnapshot>,
    hook_event_rx: Option<std::sync::mpsc::Receiver<HookEvent>>,
    worker_tx: Option<std::sync::mpsc::Sender<ActionJob>>,
    tray: Option<TrayIcon>,
    hook_ctrl: Option<HookController>,
    overlay: OverlayWindow,
}

fn main() {
    // Check config for debug_logging setting before initializing logger
    let config_path = std::env::var("APPDATA")
        .map(|d| std::path::PathBuf::from(d).join("mouse-gesture").join("config.toml"))
        .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"));
    let debug_logging = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("settings")?.get("debug_logging")?.as_bool())
        .unwrap_or(false);

    let log_level = if debug_logging { "debug" } else { "warn" };

    // Log to a file
    let log_path = std::env::var("APPDATA")
        .map(|d| std::path::PathBuf::from(d).join("mouse-gesture").join("daemon.log"))
        .unwrap_or_else(|_| std::path::PathBuf::from("daemon.log"));
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Truncate log if over 1 MiB to prevent unbounded growth
    if let Ok(meta) = std::fs::metadata(&log_path) {
        if meta.len() > 1_048_576 {
            let _ = std::fs::write(&log_path, "");
        }
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open(&log_path)
        .unwrap();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .format_timestamp_millis()
        .target(env_logger::Target::Pipe(Box::new(std::io::LineWriter::new(log_file))))
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
    set_pmv2_dpi_awareness();

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
    let (worker_tx, worker_rx) = std::sync::mpsc::channel::<ActionJob>();
    std::thread::Builder::new().name("worker".into()).spawn(move || {
        for job in worker_rx {
            match job {
                ActionJob::Window { name, cmd } => {
                    info!("Action: {} (window)", name);
                    execute_window_action(&cmd);
                }
                ActionJob::Keyboard { name, inputs } => {
                    info!("Action: {} (keyboard)", name);
                    execute_keyboard_action(&inputs);
                }
                ActionJob::Launch { name, path, args } => {
                    info!("Action: {} (launch)", name);
                    execute_launch_action(&path, &args);
                }
            }
        }
        info!("Worker stopped");
    })?;

    // Hook thread + recognition worker + replay worker + policy worker
    let (_hook_handle, _recog_handle, _replay_handle, _policy_handle, _hook_shared, hook_ctrl, hook_event_rx) =
        input_hook::spawn_hook_thread(3, 2, hwnd.0 as isize);

    // Publish initial policy snapshot
    if let Some(ref cfg) = config {
        mouse_gesture::app_policy::publish_snapshot(
            mouse_gesture::app_policy::PolicySnapshot::from_snapshot(cfg)
        );
    }

    if let Some(ref cfg) = config {
        hook_ctrl.send(HookCommand::UpdateConfig(cfg.clone())).ok();
        hook_ctrl.send(HookCommand::SetInterception(true)).ok();
        info!("Config loaded: {} gestures active", cfg.gestures.len());
    } else {
        warn!("No config — interception disabled");
    }

    // Config watcher thread
    let cfg_path = config_path();
    let cfg_ctrl = hook_ctrl.clone();
    std::thread::Builder::new().name("config-watcher".into()).spawn(move || {
        watch_config(cfg_path, cfg_ctrl);
    })?;

    // Overlay
    let overlay = OverlayWindow::new()?;
    info!("Direction overlay created");

    // State
    let state = Arc::new(Mutex::new(DaemonState {
        config,
        hook_event_rx: Some(hook_event_rx),
        worker_tx: Some(worker_tx),
        tray: Some(tray),
        hook_ctrl: Some(hook_ctrl.clone()),
        overlay,
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
    if let Ok(guard) = _state.lock() {
        guard.overlay.destroy();
    }
    hook_ctrl.send(HookCommand::Shutdown).ok();
    // Join hook first — its TLS destructors drop channel senders,
    // unblocking the worker threads' recv() calls.
    let _ = _hook_handle.join();
    let _ = _policy_handle.join();
    let _ = _replay_handle.join();
    let _ = _recog_handle.join();
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
                // Handle overlay display
                if let Ok(guard) = state.lock() {
                    match event {
                        HookEvent::DirectionChanged { direction, x, y } => {
                            guard.overlay.show_direction(*direction, *x, *y);
                        }
                        HookEvent::GestureEnded { .. } | HookEvent::GestureStarted { .. } => {
                            guard.overlay.hide();
                        }
                        _ => {}
                    }
                }
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
    worker_tx: &Option<std::sync::mpsc::Sender<ActionJob>>,
    config: &Option<ConfigSnapshot>,
) {
    match event {
        HookEvent::GestureEnded { matched: true, gesture_name } => {
            let name = gesture_name.as_deref().unwrap_or("?");
            debug!("Gesture: {}", name);
            // Enqueue action to worker thread
            if let (Some(ref cfg), Some(ref tx)) = (config, worker_tx) {
                for (gesture_name, cmd) in &cfg.window_commands {
                    if gesture_name == name {
                        tx.send(ActionJob::Window { name: name.to_string(), cmd: cmd.clone() }).ok();
                        return;
                    }
                }
                if let Some(inputs) = cfg.key_map.get(name) {
                    tx.send(ActionJob::Keyboard { name: name.to_string(), inputs: inputs.clone() }).ok();
                    return;
                }
                for (launch_name, path, args) in &cfg.launch_actions {
                    if launch_name == name {
                        tx.send(ActionJob::Launch { name: name.to_string(), path: path.clone(), args: args.clone() }).ok();
                        return;
                    }
                }
                warn!("Gesture '{}' has no compiled action", name);
            }
        }
        HookEvent::GestureEnded { matched: false, .. } => debug!("Gesture: unmatched"),
        HookEvent::GestureStarted { .. } | HookEvent::TrailPoint { .. }
        | HookEvent::DirectionChanged { .. } | HookEvent::PatternCaptured { .. } => {}
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
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>;
                if !state_ptr.is_null() {
                    if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                        if let Some(ref tray) = guard.tray {
                            let _ = tray.update_status(&TrayState::Disabled { reason: "Session locked".into() });
                        }
                        if let Some(ref ctrl) = guard.hook_ctrl {
                            // Send SetInterception BEFORE ForceReset so the
                            // ForceReset handler sees interception=false and
                            // can reset even if physical_button_down is true.
                            let _ = ctrl.send(HookCommand::SetInterception(false));
                            let _ = ctrl.send(HookCommand::ForceReset);
                        }
                    }
                }
            } else if lifecycle::is_session_unlock(event) {
                info!("Session unlocked — re-enabling interception");
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>;
                if !state_ptr.is_null() {
                    if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                        if let Some(ref tray) = guard.tray {
                            let _ = tray.update_status(&TrayState::Active { gesture_count: guard.config.as_ref().map(|c| c.gestures.len()).unwrap_or(0) });
                        }
                        if let Some(ref ctrl) = guard.hook_ctrl {
                            let _ = ctrl.send(HookCommand::SetInterception(true));
                        }
                    }
                }
            }
            LRESULT(0)
        }
        _ => {
            // Handle tray callback
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>;
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
fn watch_config(path: std::path::PathBuf, ctrl: HookController) {
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
                                // Publish updated policy snapshot
                                mouse_gesture::app_policy::publish_snapshot(
                                    mouse_gesture::app_policy::PolicySnapshot::from_snapshot(&snapshot)
                                );
                                let _ = ctrl.send(HookCommand::UpdateConfig(snapshot));
                                let _ = ctrl.send(HookCommand::SetInterception(true));
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

fn set_pmv2_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    info!("PMv2 DPI awareness enabled (via manifest or runtime)");
}
