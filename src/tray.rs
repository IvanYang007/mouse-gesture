//! System tray icon — Shell_NotifyIconW wrapper, popup menu,
//! and TaskbarCreated re-registration.

use anyhow::{Context, Result};
use log;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW,
    NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIF_ICON, NIF_MESSAGE, NIF_TIP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    LoadImageW, IMAGE_ICON, LR_LOADFROMFILE, LR_DEFAULTSIZE,
    HICON, WM_APP,
};

/// Tray icon states for visual status display.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Helper to convert BOOL return from Shell_NotifyIconW to Result.
fn notify_result(ok: windows::core::BOOL) -> Result<()> {
    if ok.as_bool() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("Shell_NotifyIconW failed"))
    }
}

impl TrayIcon {
    pub fn new(hwnd: HWND) -> Result<Self> {
        let uid = 1;
        let callback_msg = WM_APP + 1;

        let icon_path = std::env::var("APPDATA")
            .map(|d| std::path::PathBuf::from(d).join("mouse-gesture").join("icon.ico"))
            .unwrap_or_else(|_| std::path::PathBuf::from("icon.ico"));
        let icon_path_str = icon_path.to_string_lossy().to_string();

        let icon_path_wide: Vec<u16> = icon_path_str.encode_utf16().chain(std::iter::once(0)).collect();
        let icon = unsafe {
            LoadImageW(
                None,
                windows::core::PCWSTR::from_raw(icon_path_wide.as_ptr()),
                IMAGE_ICON,
                0, 0,
                LR_LOADFROMFILE | LR_DEFAULTSIZE,
            )
        };
        let hicon = match icon {
            Ok(handle) => {
                log::info!("Custom icon loaded from {}", icon_path_str);
                HICON(handle.0)
            }
            Err(e) => {
                log::warn!("Failed to load custom icon: {:?}", e);
                HICON(std::ptr::null_mut())
            }
        };

        let tip: Vec<u16> = "Mouse Gesture Daemon\0".encode_utf16().collect();

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: uid,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: callback_msg,
            hIcon: hicon,
            ..Default::default()
        };

        let tip_slice = &mut nid.szTip;
        let copy_len = tip.len().min(tip_slice.len());
        tip_slice[..copy_len].copy_from_slice(&tip[..copy_len]);

        notify_result(unsafe { Shell_NotifyIconW(NIM_ADD, &nid) })?;

        Ok(TrayIcon { hwnd, uid, callback_msg })
    }

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

        notify_result(unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) })?;
        Ok(())
    }

    pub fn remove(&self) -> Result<()> {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: self.uid,
            ..Default::default()
        };
        notify_result(unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) })?;
        Ok(())
    }

    pub fn reregister(&self) -> Result<()> {
        let tip: Vec<u16> = "Mouse Gesture Daemon\0".encode_utf16().collect();
        let icon_path = std::env::var("APPDATA")
            .map(|d| std::path::PathBuf::from(d).join("mouse-gesture").join("icon.ico"))
            .unwrap_or_else(|_| std::path::PathBuf::from("icon.ico"));
        let icon_path_wide: Vec<u16> = icon_path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let icon = unsafe {
            LoadImageW(None, windows::core::PCWSTR::from_raw(icon_path_wide.as_ptr()), IMAGE_ICON, 32, 32, LR_LOADFROMFILE)
        };
        let hicon = match icon {
            Ok(handle) => HICON(handle.0),
            Err(_) => HICON(std::ptr::null_mut()),
        };

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: self.uid,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: self.callback_msg,
            hIcon: hicon,
            ..Default::default()
        };

        let tip_slice = &mut nid.szTip;
        let copy_len = tip.len().min(tip_slice.len());
        tip_slice[..copy_len].copy_from_slice(&tip[..copy_len]);

        notify_result(unsafe { Shell_NotifyIconW(NIM_ADD, &nid) })?;
        Ok(())
    }

    pub fn callback_msg(&self) -> u32 {
        self.callback_msg
    }
}
