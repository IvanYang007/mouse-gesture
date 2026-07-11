//! Mouse Gesture Daemon — Windows system-tray daemon for
//! right-click mouse gesture recognition and action dispatch.
//!
//! Architecture: three-thread model
//!   1. Input hook thread  — WH_MOUSE_LL, state machine, gesture buffers
//!   2. UI/controller thread — notification window, tray icon, config watching
//!   3. Action worker thread — config validation, process resolution,
//!      window actions, keyboard injection, app launching

use mouse_gesture::input_hook;

use anyhow::Result;
use log::info;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("Mouse Gesture Daemon v{} starting", env!("CARGO_PKG_VERSION"));

    // Set Per-Monitor V2 DPI awareness before any HWND creation
    set_pmv2_dpi_awareness()?;

    info!("PMv2 DPI awareness enabled");

    // Initialize hook thread-local state with default config values
    input_hook::init_hook_state(3, 2);

    // TODO: Phase 2 — spawn input hook thread
    // TODO: Phase 2 — spawn action/config worker thread
    // TODO: Phase 4 — create notification window, tray icon
    // TODO: Phase 4 — enter message pump on UI thread

    info!("Shutting down");
    Ok(())
}

/// Set Per-Monitor V2 DPI awareness for the process.
fn set_pmv2_dpi_awareness() -> Result<()> {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    unsafe {
        let result = SetProcessDpiAwarenessContext(
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
        if result.is_err() {
            anyhow::bail!("SetProcessDpiAwarenessContext failed: {:?}", result);
        }
    }
    Ok(())
}
