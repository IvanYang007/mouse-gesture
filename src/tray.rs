//! System tray icon — Shell_NotifyIconW wrapper, popup menu,
//! and TaskbarCreated re-registration.

use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW,
    NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIF_ICON, NIF_MESSAGE, NIF_TIP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    LoadIconW, IDI_APPLICATION, WM_APP,
};

/// Tray icon states for visual status display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Active { gesture_count: usize },
    Disabled { reason: String },
    Error { message: String },
}

/// System tray icon manager.
pub struct TrayIcon {
    hwnd: HWND,
    uid: u32,
    callback_msg: u32,
}

impl TrayIcon {
    /// Create and add a tray icon.
    pub fn new(hwnd: HWND) -> Result<Self> {
        let uid = 1;
        let callback_msg = WM_APP + 1;

        let icon = unsafe { LoadIconW(None, IDI_APPLICATION)? };

        let tip: Vec<u16> = "Mouse Gesture Daemon\0".encode_utf16().collect();

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: uid,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: callback_msg,
            hIcon: icon,
            ..Default::default()
        };

        // Copy tip string
        let tip_slice = &mut nid.szTip;
        let copy_len = tip.len().min(tip_slice.len());
        tip_slice[..copy_len].copy_from_slice(&tip[..copy_len]);

        unsafe { Shell_NotifyIconW(NIM_ADD, &nid)?; }

        Ok(TrayIcon { hwnd, uid, callback_msg })
    }

    /// Update the tray icon tooltip with current status.
    pub fn update_status(&self, state: &TrayState) -> Result<()> {
        let tip_str = match state {
            TrayState::Active { gesture_count } => {
                format!("Mouse Gesture — {} gestures active", gesture_count)
            }
            TrayState::Disabled { reason } => {
                format!("Mouse Gesture — Disabled ({})", reason)
            }
            TrayState::Error { message } => {
                format!("Mouse Gesture — Error: {}", message)
            }
        };

        let tip: Vec<u16> = format!("{}\0", tip_str).encode_utf16().collect();

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: self.uid,
            uFlags: NIF_TIP,
            ..Default::default()
        };

        let tip_slice = &mut nid.szTip;
        let copy_len = tip.len().min(tip_slice.len());
        tip_slice[..copy_len].copy_from_slice(&tip[..copy_len]);

        unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid)?; }
        Ok(())
    }

    /// Remove the tray icon (called on shutdown).
    pub fn remove(&self) -> Result<()> {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: self.uid,
            ..Default::default()
        };
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid)?; }
        Ok(())
    }

    /// Re-register after Explorer restart (TaskbarCreated message).
    pub fn reregister(&self) -> Result<()> {
        let tip: Vec<u16> = "Mouse Gesture Daemon\0".encode_utf16().collect();
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION)? };

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: self.uid,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: self.callback_msg,
            hIcon: icon,
            ..Default::default()
        };

        let tip_slice = &mut nid.szTip;
        let copy_len = tip.len().min(tip_slice.len());
        tip_slice[..copy_len].copy_from_slice(&tip[..copy_len]);

        unsafe { Shell_NotifyIconW(NIM_ADD, &nid)?; }
        Ok(())
    }

    pub fn callback_msg(&self) -> u32 {
        self.callback_msg
    }
}
