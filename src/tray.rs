//! System tray icon — Shell_NotifyIconW wrapper, popup menu,
//! and TaskbarCreated re-registration.

use anyhow::Result;
use log;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW,
    NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIF_ICON, NIF_MESSAGE, NIF_TIP,
};
use windows::Win32::Foundation::{POINT, WPARAM, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos,
    LoadImageW, PostMessageW, SetForegroundWindow, TrackPopupMenuEx,
    HMENU, IMAGE_ICON, LR_LOADFROMFILE, LR_DEFAULTSIZE,
    HICON, MF_SEPARATOR, MF_STRING,
    TPM_RIGHTBUTTON, TPM_RETURNCMD, TPM_NONOTIFY,
    WM_APP, WM_NULL,
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

/// Context menu command identifiers
pub const IDM_CONFIGURE: u32 = 1001;
pub const IDM_RELOAD_CONFIG: u32 = 1002;
pub const IDM_OPEN_CONFIG_FOLDER: u32 = 1003;
pub const IDM_EXIT: u32 = 1004;

/// Commands selectable from the tray right-click context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Configure,
    ReloadConfig,
    OpenConfigFolder,
    Exit,
}

/// Show the tray context menu at the cursor position.
///
/// Returns the selected command or `None` if the user clicked away.
///
/// # Safety
///
/// `TrackPopupMenuEx` runs an internal modal message loop that re-enters
/// `window_proc`. Callers must NOT hold the `DaemonState` mutex across
/// this call — deadlock will occur if the re-entrant proc tries to lock.
pub fn show_context_menu(hwnd: HWND) -> Result<Option<TrayCommand>> {
    unsafe {
        let menu = CreatePopupMenu()?;

        // Guard so DestroyMenu runs on all exit paths (error, cancel, select).
        struct MenuGuard(HMENU);
        impl Drop for MenuGuard {
            fn drop(&mut self) {
                unsafe { DestroyMenu(self.0); }
            }
        }
        let menu = MenuGuard(menu);

        AppendMenuW(menu.0, MF_STRING, IDM_CONFIGURE as usize, windows::core::w!("Configure..."));
        AppendMenuW(menu.0, MF_STRING, IDM_RELOAD_CONFIG as usize, windows::core::w!("Reload configuration"));
        AppendMenuW(menu.0, MF_SEPARATOR, 0, None);
        AppendMenuW(menu.0, MF_STRING, IDM_OPEN_CONFIG_FOLDER as usize, windows::core::w!("Open configuration folder"));
        AppendMenuW(menu.0, MF_SEPARATOR, 0, None);
        AppendMenuW(menu.0, MF_STRING, IDM_EXIT as usize, windows::core::w!("Exit"));

        let mut pt = POINT::default();
        GetCursorPos(&mut pt)?;

        // Required: set foreground so the menu can be dismissed by clicking away.
        SetForegroundWindow(hwnd);

        let cmd = TrackPopupMenuEx(
            menu.0,
            (TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY).0,
            pt.x, pt.y,
            hwnd,
            None,
        );

        // Post benign message so the menu window finishes cleaning up.
        PostMessageW(Some(hwnd), WM_NULL, WPARAM::default(), LPARAM::default());

        match cmd.0 as u32 {
            0 => Ok(None),
            IDM_CONFIGURE => Ok(Some(TrayCommand::Configure)),
            IDM_RELOAD_CONFIG => Ok(Some(TrayCommand::ReloadConfig)),
            IDM_OPEN_CONFIG_FOLDER => Ok(Some(TrayCommand::OpenConfigFolder)),
            IDM_EXIT => Ok(Some(TrayCommand::Exit)),
            _ => Ok(None),
        }
    }
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
