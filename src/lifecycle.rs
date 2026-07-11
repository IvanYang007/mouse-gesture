//! Process lifecycle — singleton mutex, session event handling,
//! shutdown coordination, and bounded rotating logs.

use anyhow::Result;
use std::path::PathBuf;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{
    CreateMutexW,
};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
    NOTIFY_FOR_THIS_SESSION, WM_WTSSESSION_CHANGE,
    WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};
use windows::Win32::System::SystemServices::ERROR_ALREADY_EXISTS;
use windows::core::PCWSTR;

// ── Singleton Mutex ────────────────────────────────────────────

const MUTEX_NAME: &str = "Global\\MouseGestureDaemon_Singleton\0";

/// Ensure only one instance of the daemon runs per user session.
/// Returns Ok(()) if this is the first instance, Err if another
/// instance is already running.
pub fn acquire_singleton() -> Result<()> {
    let name: Vec<u16> = MUTEX_NAME.encode_utf16().collect();

    unsafe {
        let handle = CreateMutexW(
            None,
            true, // initial owner
            PCWSTR::from_raw(name.as_ptr()),
        );

        match handle {
            Ok(_) => {
                let err = windows::Win32::Foundation::GetLastError();
                if err.0 == ERROR_ALREADY_EXISTS.0 {
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
    wparam == WTS_SESSION_LOCK.0 as usize
}

/// Check if a WM_WTSSESSION_CHANGE message indicates an unlock event.
pub fn is_session_unlock(wparam: usize) -> bool {
    wparam == WTS_SESSION_UNLOCK.0 as usize
}

// ── Logging ────────────────────────────────────────────────────

/// Bounded rotating file logger.
pub struct RotatingLogger {
    dir: PathBuf,
    max_files: usize,
    max_size: u64,
    current_size: u64,
    current_file: Option<std::fs::File>,
}

impl RotatingLogger {
    /// Create a new rotating logger.
    /// `dir` — log directory (typically %LOCALAPPDATA%\mouse-gesture\logs)
    /// `max_files` — maximum files to retain (default 5)
    /// `max_size` — max bytes per file before rotation (default 1MB)
    pub fn new(dir: PathBuf, max_files: usize, max_size: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(RotatingLogger {
            dir,
            max_files,
            max_size,
            current_size: 0,
            current_file: None,
        })
    }

    /// Open the current log file for appending.
    pub fn open_current(&mut self) -> Result<()> {
        let path = self.dir.join("daemon.log");
        self.current_file = Some(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?
        );
        self.current_size = std::fs::metadata(&path)
            .map(|m| m.len())
            .unwrap_or(0);
        Ok(())
    }

    /// Rotate logs: rename daemon.log → daemon.1.log, shift existing numbered files.
    pub fn rotate(&mut self) -> Result<()> {
        self.current_file = None;

        // Remove oldest file
        let oldest = self.dir.join(format!("daemon.{}.log", self.max_files));
        let _ = std::fs::remove_file(&oldest);

        // Shift numbered files
        for i in (0..self.max_files).rev() {
            let src = if i == 0 {
                self.dir.join("daemon.log")
            } else {
                self.dir.join(format!("daemon.{}.log", i))
            };
            let dst = self.dir.join(format!("daemon.{}.log", i + 1));
            let _ = std::fs::rename(&src, &dst);
        }

        self.current_size = 0;
        self.open_current()?;
        Ok(())
    }
}
