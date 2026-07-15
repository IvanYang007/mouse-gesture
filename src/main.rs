//! Mouse Gesture Daemon — Windows entry point, message pump,
//! config loading, action dispatch, and custom file logger.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use log::{debug, error, info, warn};
use mouse_gesture::config::{ConfigFile, ConfigSnapshot};
use mouse_gesture::input_hook::{self, HookCommand, HookController, HookEvent};
use mouse_gesture::lifecycle;
use mouse_gesture::overlay::OverlayWindow;
use mouse_gesture::tray::{TrayCommand, TrayIcon, TrayState};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowLongPtrW, MessageBoxW,
    PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW, SetWindowLongPtrW,
    TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, MB_ICONERROR, MB_OK,
    MSG, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_QUERYENDSESSION,
    WM_RBUTTONUP, WS_OVERLAPPED,
};

const WINDOW_CLASS: &str = "MouseGestureDaemon\0";
const WM_APP_RELOAD_CONFIG: u32 = WM_APP + 20;
static TASKBAR_CREATED_MSG: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone)]
enum ActionJob {
    Window {
        name: String,
        cmd: mouse_gesture::config::WindowCommand,
        target_hwnd: isize,
    },
    Keyboard {
        name: String,
        inputs: Vec<mouse_gesture::config::CompiledInput>,
        target_hwnd: isize,
    },
    Launch {
        name: String,
        path: String,
        args: Vec<String>,
    },
}

struct DaemonState {
    config: Option<Arc<ConfigSnapshot>>,
    hook_event_rx: Option<std::sync::mpsc::Receiver<HookEvent>>,
    worker_tx: Option<std::sync::mpsc::Sender<ActionJob>>,
    tray: Option<TrayIcon>,
    hook_ctrl: Option<HookController>,
    overlay: OverlayWindow,
}

// ── File Logger ────────────────────────────────────────────────

/// Minimal file logger replacing env_logger. Writes timestamped
/// entries to the daemon log file. No regex/RUST_LOG dependency.
struct FileLogger {
    file: std::sync::Mutex<std::io::LineWriter<std::fs::File>>,
    level: log::LevelFilter,
}

impl FileLogger {
    fn new(file: std::fs::File, level: &str) -> Self {
        let level = match level {
            "debug" => log::LevelFilter::Debug,
            "info" => log::LevelFilter::Info,
            _ => log::LevelFilter::Warn,
        };
        FileLogger {
            file: std::sync::Mutex::new(std::io::LineWriter::new(file)),
            level,
        }
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        use std::io::Write;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let _ = writeln!(
            self.file.lock().unwrap(),
            "{} [{:>5}] {}: {}",
            format_log_timestamp(now),
            record.level(),
            record.target(),
            record.args(),
        );
    }

    fn flush(&self) {
        use std::io::Write;
        let _ = self.file.lock().unwrap().flush();
    }
}

/// Format a Duration since UNIX epoch as a compact log timestamp.
fn format_log_timestamp(dur: std::time::Duration) -> String {
    let secs = dur.as_secs();
    let ms = dur.subsec_millis();
    // days since UNIX epoch
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;
    let s = time_secs % 60;
    format!("{days}.{h:02}:{m:02}:{s:02}.{ms:03}")
}

fn main() {
    // Check config for debug_logging setting before initializing logger
    let cfg_path = config_path();
    let debug_logging = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| toml::from_str::<toml::Value>(&s).ok())
        .and_then(|v| v.get("settings")?.get("debug_logging")?.as_bool())
        .unwrap_or(false);

    let log_level = if debug_logging { "debug" } else { "warn" };

    // Log to a file
    let log_path = std::env::var("APPDATA")
        .map(|d| {
            std::path::PathBuf::from(d)
                .join("mouse-gesture")
                .join("daemon.log")
        })
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
        .create(true)
        .append(true)
        .open(&log_path)
        .unwrap();

    // Thin custom logger — avoids pulling in the regex stack (~600 KiB)
    // that env_logger brings for RUST_LOG filtering we never use.
    let file_logger = FileLogger::new(log_file, log_level);
    log::set_boxed_logger(Box::new(file_logger))
        .map(|()| log::set_max_level(log::LevelFilter::max()))
        .expect("logger init");

    info!(
        "Mouse Gesture Daemon v{} starting (Phase 2)",
        env!("CARGO_PKG_VERSION")
    );

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

    // Register TaskbarCreated message for Explorer restart re-registration
    let taskbar_msg = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    if taskbar_msg == 0 {
        warn!("RegisterWindowMessageW('TaskbarCreated') returned 0 — tray re-registration after Explorer restart unavailable");
    }
    TASKBAR_CREATED_MSG.store(taskbar_msg, Ordering::Release);

    // Session notifications
    lifecycle::register_session_notifications(hwnd)?;

    // Tray
    let tray = TrayIcon::new(hwnd)?;
    let gesture_count = config.as_ref().map(|c| c.gestures.len()).unwrap_or(0);
    if config.is_some() {
        tray.update_status(&TrayState::Active { gesture_count })?;
    } else {
        tray.update_status(&TrayState::Disabled {
            reason: "No valid config".into(),
        })?;
    }

    // Worker thread
    let (worker_tx, worker_rx) = std::sync::mpsc::channel::<ActionJob>();
    std::thread::Builder::new()
        .name("worker".into())
        .spawn(move || {
            for job in worker_rx {
                match job {
                    ActionJob::Window {
                        name,
                        cmd,
                        target_hwnd,
                    } => {
                        info!("Action: {} (window)", name);
                        execute_window_action(&cmd, HWND(target_hwnd as *mut _));
                    }
                    ActionJob::Keyboard {
                        name,
                        inputs,
                        target_hwnd,
                    } => {
                        info!("Action: {} (keyboard)", name);
                        execute_keyboard_action(&inputs, HWND(target_hwnd as *mut _));
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
    let (_hook_handle, _recog_handle, _replay_handle, _hook_shared, hook_ctrl, hook_event_rx) =
        input_hook::spawn_hook_thread(3, 2, hwnd.0 as isize);

    // Publish initial policy snapshot
    if let Some(ref cfg) = config {
        mouse_gesture::app_policy::publish_snapshot(
            mouse_gesture::app_policy::PolicySnapshot::from_snapshot(cfg),
        );
    }

    if let Some(ref cfg) = config {
        hook_ctrl
            .send(HookCommand::UpdateConfig(Arc::new(cfg.clone())))
            .ok();
        hook_ctrl.send(HookCommand::SetInterception(true)).ok();
        mouse_gesture::autostart::sync(cfg.start_with_windows);
        info!("Config loaded: {} gestures active", cfg.gestures.len());
    } else {
        warn!("No config — interception disabled");
    }

    // Config watcher thread
    let cfg_path = config_path();
    let hwnd_raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("config-watcher".into())
        .spawn(move || {
            watch_config(cfg_path, hwnd_raw);
        })?;

    // Overlay
    let overlay = OverlayWindow::new()?;
    info!("Direction overlay created");

    // State
    // DaemonState stays on the owning thread. The Arc is used only for
    // ref-counted cleanup when reclaimed from GWLP_USERDATA — it never
    // crosses threads. OverlayWindow's raw GDI handles are not Send but
    // that's irrelevant here.
    #[allow(clippy::arc_with_non_send_sync)]
    let state = Arc::new(Mutex::new(DaemonState {
        config: config.map(Arc::new),
        hook_event_rx: Some(hook_event_rx),
        worker_tx: Some(worker_tx),
        tray: Some(tray),
        hook_ctrl: Some(hook_ctrl.clone()),
        overlay,
    }));
    let state_ptr = Arc::into_raw(state);
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
    }

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
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            None,
            None,
        )?
    };
    Ok(hwnd)
}

// ── Message Pump ───────────────────────────────────────────────

fn message_pump(hwnd: HWND) -> i32 {
    loop {
        let state_ptr =
            unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
        if !state_ptr.is_null() {
            let state = unsafe { &*state_ptr };
            let mut pending_events = Vec::new();
            let (worker_tx, config) = if let Ok(guard) = state.lock() {
                if let Some(ref rx) = guard.hook_event_rx {
                    while let Ok(event) = rx.try_recv() {
                        pending_events.push(event);
                    }
                }
                (
                    guard.worker_tx.clone(),
                    guard.config.as_ref().map(Arc::clone),
                )
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
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn dispatch_hook_event(
    event: &HookEvent,
    worker_tx: &Option<std::sync::mpsc::Sender<ActionJob>>,
    config: &Option<Arc<ConfigSnapshot>>,
) {
    match event {
        HookEvent::GestureEnded {
            matched: true,
            gesture_name,
            target_hwnd,
        } => {
            let name = gesture_name.as_deref().unwrap_or("?");
            let hwnd = *target_hwnd;
            debug!("Gesture: {} (target=0x{:x})", name, hwnd);
            // Enqueue action to worker thread
            if let (Some(ref cfg), Some(ref tx)) = (config, worker_tx) {
                for (gesture_name, cmd) in &cfg.window_commands {
                    if gesture_name == name {
                        tx.send(ActionJob::Window {
                            name: name.to_string(),
                            cmd: cmd.clone(),
                            target_hwnd: hwnd,
                        })
                        .ok();
                        return;
                    }
                }
                if let Some(inputs) = cfg.key_map.get(name) {
                    tx.send(ActionJob::Keyboard {
                        name: name.to_string(),
                        inputs: inputs.clone(),
                        target_hwnd: hwnd,
                    })
                    .ok();
                    return;
                }
                for (launch_name, path, args) in &cfg.launch_actions {
                    if launch_name == name {
                        tx.send(ActionJob::Launch {
                            name: name.to_string(),
                            path: path.clone(),
                            args: args.clone(),
                        })
                        .ok();
                        return;
                    }
                }
                warn!("Gesture '{}' has no compiled action", name);
            }
        }
        HookEvent::GestureEnded { matched: false, .. } => debug!("Gesture: unmatched"),
        HookEvent::GestureStarted { .. }
        | HookEvent::TrailPoint { .. }
        | HookEvent::DirectionChanged { .. }
        | HookEvent::PatternCaptured { .. } => {}
        HookEvent::Error(msg) => error!("Hook: {}", msg),
    }
}

fn execute_window_action(cmd: &mouse_gesture::config::WindowCommand, target_hwnd: HWND) {
    use mouse_gesture::config::WindowCommand;
    use mouse_gesture::window_ops::{enumerate_monitors, SnapPosition};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, IsWindow, PostMessageW, ShowWindowAsync, SW_MAXIMIZE, SW_MINIMIZE,
        SW_RESTORE, WM_CLOSE,
    };

    // Validate target_hwnd — fall back to foreground if invalid
    let hwnd = if target_hwnd.is_invalid() || !unsafe { IsWindow(Some(target_hwnd)).as_bool() } {
        debug!("target_hwnd invalid, falling back to foreground");
        unsafe { GetForegroundWindow() }
    } else {
        target_hwnd
    };

    // Defer monitor enumeration — only needed for snap/center/move operations
    // Basic window commands (maximize, minimize, restore, close) skip this entirely.
    let monitors: std::cell::LazyCell<Vec<mouse_gesture::window_ops::MonitorInfo>> =
        std::cell::LazyCell::new(|| enumerate_monitors().unwrap_or_default());

    match cmd {
        WindowCommand::Maximize => unsafe {
            let _ = ShowWindowAsync(hwnd, SW_MAXIMIZE);
        },
        WindowCommand::Minimize => unsafe {
            let _ = ShowWindowAsync(hwnd, SW_MINIMIZE);
        },
        WindowCommand::Restore => unsafe {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        },
        WindowCommand::Close => unsafe {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        },
        WindowCommand::SnapLeft => do_snap(hwnd, monitors.first(), SnapPosition::Left),
        WindowCommand::SnapRight => do_snap(hwnd, monitors.first(), SnapPosition::Right),
        WindowCommand::SnapTop => do_snap(hwnd, monitors.first(), SnapPosition::Top),
        WindowCommand::SnapBottom => do_snap(hwnd, monitors.first(), SnapPosition::Bottom),
        WindowCommand::SnapTopLeft => do_snap(hwnd, monitors.first(), SnapPosition::TopLeft),
        WindowCommand::SnapTopRight => do_snap(hwnd, monitors.first(), SnapPosition::TopRight),
        WindowCommand::SnapBottomLeft => do_snap(hwnd, monitors.first(), SnapPosition::BottomLeft),
        WindowCommand::SnapBottomRight => {
            do_snap(hwnd, monitors.first(), SnapPosition::BottomRight)
        }
        WindowCommand::Center => do_snap(hwnd, monitors.first(), SnapPosition::Center),
        WindowCommand::ToggleAlwaysOnTop => {
            info!("ToggleAlwaysOnTop not yet implemented");
        }
        WindowCommand::MoveToMonitor(n) => {
            if let Some(m) = monitors.get(*n as usize) {
                do_snap(hwnd, Some(m), SnapPosition::Center);
            }
        }
    }
}

/// Execute a snap/positioning operation on a window.
fn do_snap(
    hwnd: HWND,
    monitor: Option<&mouse_gesture::window_ops::MonitorInfo>,
    position: mouse_gesture::window_ops::SnapPosition,
) {
    use mouse_gesture::window_ops::snap_rect;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_TOP, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    };
    if let Some(m) = monitor {
        let r = snap_rect(m, position);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE,
            );
        }
    }
}

fn execute_keyboard_action(inputs: &[mouse_gesture::config::CompiledInput], target_hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;

    // Try to focus the target window so keystrokes land in the right place
    if !target_hwnd.is_invalid() && unsafe { IsWindow(Some(target_hwnd)).as_bool() } {
        mouse_gesture::launch::focus_existing(target_hwnd);
    }

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
    if args.is_empty()
        && (path.starts_with("http://") || path.starts_with("https://") || path.contains('.'))
    {
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
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY | WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        WM_QUERYENDSESSION => LRESULT(1),
        msg if msg == lifecycle::WM_WTSSESSION_CHANGE => {
            let event = wparam.0;
            if lifecycle::is_session_lock(event) {
                info!("Session locked — disabling interception");
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>;
                if !state_ptr.is_null() {
                    if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                        if let Some(ref tray) = guard.tray {
                            let _ = tray.update_status(&TrayState::Disabled {
                                reason: "Session locked".into(),
                            });
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
                            let _ = tray.update_status(&TrayState::Active {
                                gesture_count: guard
                                    .config
                                    .as_ref()
                                    .map(|c| c.gestures.len())
                                    .unwrap_or(0),
                            });
                        }
                        if let Some(ref ctrl) = guard.hook_ctrl {
                            let _ = ctrl.send(HookCommand::SetInterception(true));
                        }
                    }
                }
            }
            LRESULT(0)
        }
        msg if msg == TASKBAR_CREATED_MSG.load(Ordering::Relaxed) && msg != 0 => {
            // Re-register tray icon after Explorer restart
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>;
            if !state_ptr.is_null() {
                if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                    if let Some(ref tray) = guard.tray {
                        if let Err(e) = tray.reregister() {
                            error!("TaskbarCreated: tray re-register failed: {}", e);
                        } else {
                            info!("Tray icon re-registered after Explorer restart");
                        }
                    }
                }
            }
            LRESULT(0)
        }
        msg if msg == WM_APP_RELOAD_CONFIG => {
            reload_config(hwnd);
            LRESULT(0)
        }
        _ => {
            // Handle tray callback — lock briefly only to check if this message
            // belongs to the tray icon, then release before any modal operation.
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState>;
            if !state_ptr.is_null() {
                let (is_tray_msg, tray_event) = {
                    if let Ok(guard) = unsafe { &*state_ptr }.lock() {
                        if let Some(ref tray) = guard.tray {
                            if msg == tray.callback_msg() {
                                (true, (lparam.0 as u32) & 0xffff)
                            } else {
                                (false, 0)
                            }
                        } else {
                            (false, 0)
                        }
                    } else {
                        (false, 0)
                    }
                };

                if is_tray_msg {
                    if tray_event == WM_RBUTTONUP || tray_event == WM_CONTEXTMENU {
                        // Mutex released — safe to call show_context_menu (modal loop).
                        match mouse_gesture::tray::show_context_menu(hwnd) {
                            Ok(Some(cmd)) => match cmd {
                                TrayCommand::Configure => {
                                    if let Err(e) =
                                        mouse_gesture::config_editor::open(hwnd, config_path())
                                    {
                                        let error_msg: Vec<u16> =
                                            format!("Failed to open editor: {}\0", e)
                                                .encode_utf16()
                                                .collect();
                                        MessageBoxW(
                                            None,
                                            windows::core::PCWSTR::from_raw(error_msg.as_ptr()),
                                            w!("Editor Error"),
                                            MB_OK | MB_ICONERROR,
                                        );
                                        error!("Failed to open editor: {}", e);
                                    }
                                }
                                TrayCommand::ReloadConfig => {
                                    let _ = PostMessageW(
                                        Some(hwnd),
                                        WM_APP_RELOAD_CONFIG,
                                        WPARAM::default(),
                                        LPARAM::default(),
                                    );
                                }
                                TrayCommand::OpenConfigFolder => {
                                    let parent = config_path()
                                        .parent()
                                        .map(|p| p.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    if let Err(e) = mouse_gesture::launch::shell_open(&parent) {
                                        error!("Failed to open config folder: {}", e);
                                    }
                                }
                                TrayCommand::Exit => {
                                    let _ = PostMessageW(
                                        Some(hwnd),
                                        WM_CLOSE,
                                        WPARAM::default(),
                                        LPARAM::default(),
                                    );
                                }
                            },
                            Ok(None) => debug!("Tray: menu dismissed"),
                            Err(e) => error!("Tray: menu error: {}", e),
                        }
                    }
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

// ── Config ─────────────────────────────────────────────────────

fn config_path() -> std::path::PathBuf {
    std::env::var("APPDATA")
        .map(|d| {
            std::path::PathBuf::from(d)
                .join("mouse-gesture")
                .join("config.toml")
        })
        .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"))
}

fn load_startup_config() -> Result<Option<ConfigSnapshot>> {
    let path = config_path();
    match ConfigFile::load(&path) {
        Ok(cfg) => match cfg.compile(1, 96) {
            Ok(s) => {
                info!("Config: {}", path.display());
                Ok(Some(s))
            }
            Err(e) => {
                error!("Invalid config: {}. Interception disabled.", e);
                Ok(None)
            }
        },
        Err(e) => {
            warn!(
                "No config at {}: {}. Interception disabled.",
                path.display(),
                e
            );
            Ok(None)
        }
    }
}

fn reload_config(hwnd: HWND) {
    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Mutex<DaemonState> };
    if state_ptr.is_null() {
        error!("reload_config: state_ptr is null");
        return;
    }
    let state = unsafe { &*state_ptr };
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(e) => {
            error!("reload_config: mutex poisoned: {}", e);
            return;
        }
    };

    match ConfigFile::load(&config_path()) {
        Ok(cfg) => match cfg.compile(1, 96) {
            Ok(snapshot) => {
                let start_with_windows = snapshot.start_with_windows;
                let gesture_count = snapshot.gestures.len();
                // Publish updated policy snapshot
                mouse_gesture::app_policy::publish_snapshot(
                    mouse_gesture::app_policy::PolicySnapshot::from_snapshot(&snapshot),
                );
                // Update hook
                if let Some(ref ctrl) = guard.hook_ctrl {
                    let _ = ctrl.send(HookCommand::UpdateConfig(Arc::new(snapshot.clone())));
                    let _ = ctrl.send(HookCommand::SetInterception(true));
                }
                // Update state
                guard.config = Some(Arc::new(snapshot));
                // Update tray
                if let Some(ref tray) = guard.tray {
                    let _ = tray.update_status(&TrayState::Active { gesture_count });
                }
                drop(guard);
                // Sync autostart on a background thread — `reg.exe` can block
                // for seconds when the registry is contended; do NOT block the UI.
                std::thread::spawn(move || {
                    mouse_gesture::autostart::sync(start_with_windows);
                });
                info!("Config reloaded: {} gestures", gesture_count);
            }
            Err(e) => {
                error!("Config reload failed: {}", e);
                // Do NOT replace existing valid config.
                // Set tray to error state.
                if let Some(ref tray) = guard.tray {
                    let _ = tray.update_status(&TrayState::Error {
                        message: e.to_string(),
                    });
                }
                // Keep interception as-is: prior valid config stays active;
                // if none, interception stays disabled.
            }
        },
        Err(e) => {
            error!("Config reload failed: {}", e);
            // Set tray to error state.
            if let Some(ref tray) = guard.tray {
                let _ = tray.update_status(&TrayState::Error {
                    message: e.to_string(),
                });
            }
            // Keep interception as-is.
        }
    }
}

/// Poll the config file periodically for changes.
fn watch_config(path: std::path::PathBuf, owner: isize) {
    use std::time::Duration;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

    let hwnd = HWND(owner as *mut _);
    let mut last_modified = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok());
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if let Ok(meta) = std::fs::metadata(&path) {
            let modified = meta.modified().ok();
            if modified != last_modified {
                last_modified = modified;
                debug!("Config file changed, posting reload message");
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_APP_RELOAD_CONFIG,
                        WPARAM::default(),
                        LPARAM::default(),
                    );
                }
            }
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
