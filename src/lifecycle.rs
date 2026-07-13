//! Process lifecycle — singleton mutex, session event handling,
//! shutdown coordination, and bounded rotating logs.

use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::System::Threading::CreateMutexW;
const ERROR_ALREADY_EXISTS: windows::Win32::Foundation::WIN32_ERROR =
    windows::Win32::Foundation::WIN32_ERROR(183);
use windows::core::PCWSTR;

// ── Singleton Mutex ────────────────────────────────────────────

const MUTEX_NAME: &str = "Local\\MouseGestureDaemon_Singleton\0";

/// Ensure only one instance of the daemon runs per user session.
pub fn acquire_singleton() -> Result<()> {
    let name: Vec<u16> = MUTEX_NAME.encode_utf16().collect();

    unsafe {
        let handle = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr()));

        match handle {
            Ok(_) => {
                let err = windows::Win32::Foundation::GetLastError();
                if err == ERROR_ALREADY_EXISTS {
                    anyhow::bail!("Another instance is already running");
                }
                Ok(())
            }
            Err(e) => {
                anyhow::bail!("Failed to create mutex: {:?}", e);
            }
        }
    }
}

// ── Session Events ─────────────────────────────────────────────

/// Session change notification constants from wtsapi32.h
pub const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
pub const WTS_SESSION_LOCK: u32 = 0x7;
pub const WTS_SESSION_UNLOCK: u32 = 0x8;

/// Register for session change notifications (lock/unlock, etc.).
pub fn register_session_notifications(hwnd: HWND) -> Result<()> {
    unsafe {
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)?;
    }
    Ok(())
}

/// Unregister session notifications.
pub fn unregister_session_notifications(hwnd: HWND) {
    unsafe {
        let _ = WTSUnRegisterSessionNotification(hwnd);
    }
}

/// Check if a WM_WTSSESSION_CHANGE message indicates a lock event.
pub fn is_session_lock(wparam: usize) -> bool {
    wparam == WTS_SESSION_LOCK as usize
}

/// Check if a WM_WTSSESSION_CHANGE message indicates an unlock event.
pub fn is_session_unlock(wparam: usize) -> bool {
    wparam == WTS_SESSION_UNLOCK as usize
}
