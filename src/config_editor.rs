//! Native Win32 editor window for editing `config.toml`.
//!
//! Uses EDIT + BUTTON child controls. Single-editor enforcement
//! via a global `AtomicIsize`. Saves atomically (temp file + rename)
//! and posts `WM_APP_RELOAD_CONFIG` to the owner daemon window.

use crate::config::ConfigFile;
use anyhow::Result;
use log::{error, info};
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CLIP_DEFAULT_PRECIS, CreateFontW, DEFAULT_CHARSET,
    DEFAULT_QUALITY, OUT_DEFAULT_PRECIS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsWindow,
    MessageBoxW, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW,
    ShowWindow, BS_PUSHBUTTON, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_LEFT,
    ES_MULTILINE, ES_WANTRETURN, GWLP_USERDATA, HMENU, MB_ICONERROR,
    MB_OK, SM_CXSCREEN, SM_CYSCREEN, SWP_NOZORDER, SW_SHOW, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_NCDESTROY, WM_SETFONT, WM_SIZE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSEXW, WS_CHILD,
    WS_EX_CLIENTEDGE, WS_HSCROLL, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    WS_VSCROLL,
};

const EDITOR_CLASS: &str = "MouseGestureEditor\0";
const EDITOR_W: i32 = 800;
const EDITOR_H: i32 = 600;

/// Reload config message, must match the constant in main.rs.
const WM_APP_RELOAD_CONFIG: u32 = WM_APP + 20;

/// Global handle to the single editor window (or 0 if none).
static EDITOR_HWND: AtomicIsize = AtomicIsize::new(0);

/// Per-editor state stored via `GWLP_USERDATA`.
struct EditorState {
    /// Daemon window to post `WM_APP_RELOAD_CONFIG` to after save.
    owner: HWND,
    /// Path to the config file.
    path: PathBuf,
    /// Handle to the EDIT child control.
    edit: HWND,
    /// Handle to the Save button.
    save_button: HWND,
    /// Handle to the Cancel button.
    cancel_button: HWND,
}

// ── Public API ─────────────────────────────────────────────────

/// Open the config editor window.
///
/// If an editor is already open, foregrounds the existing window
/// instead of creating a second one.
pub fn open(owner: HWND, path: PathBuf) -> Result<()> {
    let existing = EDITOR_HWND.load(Ordering::Acquire);
    if existing != 0 {
        let existing_hwnd = HWND(existing as *mut _);
        if unsafe { IsWindow(Some(existing_hwnd)).as_bool() } {
            info!("Config editor already open — bringing to foreground");
            unsafe {
                let _ = SetForegroundWindow(existing_hwnd);
            }
            return Ok(());
        }
        // Stale handle — window was destroyed without clearing (shouldn't happen)
        EDITOR_HWND.store(0, Ordering::Release);
    }

    // Register the editor window class
    register_class()?;

    // Read config file as raw UTF-8 (not ConfigFile::load — that would
    // reject invalid TOML and prevent the editor from opening for recovery).
    let config_text = std::fs::read_to_string(&path).unwrap_or_default();

    // Center on primary monitor
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let x = (screen_w - EDITOR_W) / 2;
    let y = (screen_h - EDITOR_H) / 2;

    let class_name: Vec<u16> = EDITOR_CLASS.encode_utf16().collect();
    let title: Vec<u16> = "Mouse Gesture Configuration\0".encode_utf16().collect();

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR::from_raw(class_name.as_ptr()),
            windows::core::PCWSTR::from_raw(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            x, y, EDITOR_W, EDITOR_H,
            None, None, None, None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW failed: {:?}", e))?;

    // Create the EDIT control (combine WINDOW_STYLE + EDIT_CONTROL_STYLE via raw u32)
    let edit: HWND = {
        let edit_class: Vec<u16> = "EDIT\0".encode_utf16().collect();
        let style = WINDOW_STYLE(
            (WS_CHILD | WS_VISIBLE | WS_VSCROLL | WS_HSCROLL).0
                | (ES_LEFT as u32)
                | (ES_MULTILINE as u32)
                | (ES_AUTOVSCROLL as u32)
                | (ES_AUTOHSCROLL as u32)
                | (ES_WANTRETURN as u32),
        );
        unsafe {
            CreateWindowExW(
                WS_EX_CLIENTEDGE,
                windows::core::PCWSTR::from_raw(edit_class.as_ptr()),
                windows::core::w!(""),
                style,
                0, 0, 0, 0,
                Some(hwnd), None, None, None,
            )
        }
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (EDIT) failed: {:?}", e))?;

    // Create Save button (ID 1)
    let save_button: HWND = {
        let btn_class: Vec<u16> = "BUTTON\0".encode_utf16().collect();
        let label: Vec<u16> = "&Save\0".encode_utf16().collect();
        let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (BS_PUSHBUTTON as u32));
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::PCWSTR::from_raw(btn_class.as_ptr()),
                windows::core::PCWSTR::from_raw(label.as_ptr()),
                style,
                0, 0, 0, 0,
                Some(hwnd),
                Some(HMENU(1isize as *mut _)),
                None, None,
            )
        }
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (Save button) failed: {:?}", e))?;

    // Create Cancel button (ID 2)
    let cancel_button: HWND = {
        let btn_class: Vec<u16> = "BUTTON\0".encode_utf16().collect();
        let label: Vec<u16> = "Cancel\0".encode_utf16().collect();
        let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (BS_PUSHBUTTON as u32));
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                windows::core::PCWSTR::from_raw(btn_class.as_ptr()),
                windows::core::PCWSTR::from_raw(label.as_ptr()),
                style,
                0, 0, 0, 0,
                Some(hwnd),
                Some(HMENU(2isize as *mut _)),
                None, None,
            )
        }
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (Cancel button) failed: {:?}", e))?;

    // Set a reasonable monospace font on the edit control
    let font = {
        let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
        unsafe {
            CreateFontW(
                18, 0, 0, 0, 400, 0, 0, 0,
                DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS, DEFAULT_QUALITY, 0,
                windows::core::PCWSTR::from_raw(face.as_ptr()),
            )
        }
    };
    if !font.is_invalid() {
        unsafe {
            SendMessageW(
                edit,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
    }

    // Fill the edit control with the config text
    let text_wide: Vec<u16> = config_text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        let _ = SetWindowTextW(
            edit,
            windows::core::PCWSTR::from_raw(text_wide.as_ptr()),
        );
    }

    // Create and store EditorState
    let path_display = path.display().to_string();
    let state = Box::new(EditorState {
        owner,
        path,
        edit,
        save_button,
        cancel_button,
    });
    let state_ptr = Box::into_raw(state);
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };

    // Store the editor HWND globally for single-editor enforcement
    EDITOR_HWND.store(hwnd.0 as isize, Ordering::Release);

    // Show the window
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    info!("Config editor opened: {}", path_display);
    Ok(())
}

// ── Window Class Registration ──────────────────────────────────

fn register_class() -> Result<()> {
    let name: Vec<u16> = EDITOR_CLASS.encode_utf16().collect();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: windows::Win32::UI::WindowsAndMessaging::CS_HREDRAW
            | windows::Win32::UI::WindowsAndMessaging::CS_VREDRAW,
        lpfnWndProc: Some(editor_proc),
        hInstance: windows::Win32::Foundation::HINSTANCE(std::ptr::null_mut()),
        lpszClassName: windows::core::PCWSTR::from_raw(name.as_ptr()),
        ..Default::default()
    };
    unsafe { RegisterClassExW(&wc) };
    Ok(())
}

// ── Window Procedure ───────────────────────────────────────────

unsafe extern "system" fn editor_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }

        WM_NCDESTROY => {
            // Reclaim EditorState
            let state_ptr =
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if !state_ptr.is_null() {
                unsafe {
                    let _ = Box::from_raw(state_ptr);
                }
            }
            // Clear global editor handle
            EDITOR_HWND.store(0, Ordering::Release);
            LRESULT(0)
        }

        WM_SIZE => {
            let state_ptr =
                unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if state_ptr.is_null() {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let state = unsafe { &*state_ptr };

            let client_w = (lparam.0 as u32 & 0xffff) as i32;
            let client_h = ((lparam.0 as u32 >> 16) & 0xffff) as i32;

            // Edit control fills the client area above the button row
            unsafe {
                let _ = SetWindowPos(
                    state.edit,
                    None,
                    5,
                    5,
                    client_w - 10,
                    client_h - 45,
                    SWP_NOZORDER,
                );
            }

            // Button row at bottom-right
            let btn_y = client_h - 35;
            // Cancel button (left of Save)
            unsafe {
                let _ = SetWindowPos(
                    state.cancel_button,
                    None,
                    client_w - 180,
                    btn_y,
                    80,
                    25,
                    SWP_NOZORDER,
                );
            }
            // Save button (rightmost)
            unsafe {
                let _ = SetWindowPos(
                    state.save_button,
                    None,
                    client_w - 90,
                    btn_y,
                    80,
                    25,
                    SWP_NOZORDER,
                );
            }

            LRESULT(0)
        }

        WM_COMMAND => {
            let cmd = (wparam.0 as u32 & 0xffff) as u16;
            match cmd {
                1 => handle_save(hwnd),
                2 => {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ── Save Logic ─────────────────────────────────────────────────

fn handle_save(hwnd: HWND) {
    let state_ptr =
        unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
    if state_ptr.is_null() {
        error!("handle_save: state_ptr is null");
        return;
    }
    let state = unsafe { &*state_ptr };

    // Read text from the edit control
    let text = match read_edit_text(state.edit) {
        Ok(t) => t,
        Err(e) => {
            error!("Failed to read editor text: {}", e);
            show_error(hwnd, &format!("Failed to read text: {}", e));
            return;
        }
    };

    // Validate: parse TOML + compile
    let cfg = match ConfigFile::parse(&text) {
        Ok(c) => c,
        Err(e) => {
            show_error(hwnd, &format!("Invalid TOML:\n\n{}", e));
            return;
        }
    };

    match cfg.compile(1, 96) {
        Ok(_) => {}
        Err(e) => {
            show_error(hwnd, &format!("Invalid configuration:\n\n{}", e));
            return;
        }
    }

    // Atomic save: write to temp, then rename
    let tmp_path = state.path.with_extension("toml.tmp");
    if let Some(parent) = state.path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!("Failed to create config directory: {}", e);
            show_error(hwnd, &format!("Failed to create directory:\n\n{}", e));
            return;
        }
    }

    if let Err(e) = std::fs::write(&tmp_path, &text) {
        error!("Failed to write config: {}", e);
        show_error(hwnd, &format!("Failed to write file:\n\n{}", e));
        return;
    }

    if let Err(e) = std::fs::rename(&tmp_path, &state.path) {
        error!("Failed to rename config: {}", e);
        show_error(hwnd, &format!("Failed to save file:\n\n{}", e));
        return;
    }

    // Post reload message to the daemon window
    unsafe {
        let _ = PostMessageW(
            Some(state.owner),
            WM_APP_RELOAD_CONFIG,
            WPARAM::default(),
            LPARAM::default(),
        );
    }

    info!("Config saved: {}", state.path.display());
}

// ── Helpers ────────────────────────────────────────────────────

/// Read the entire text content of an EDIT control.
fn read_edit_text(edit: HWND) -> Result<String> {
    let len = unsafe { GetWindowTextLengthW(edit) as usize };
    let mut buf: Vec<u16> = vec![0u16; len + 1];
    let actual = unsafe { GetWindowTextW(edit, &mut buf) as usize };
    Ok(String::from_utf16_lossy(&buf[..actual.min(len)]))
}

/// Show an error dialog.
fn show_error(hwnd: HWND, message: &str) {
    let title: Vec<u16> = "Configuration Error\0".encode_utf16().collect();
    let msg: Vec<u16> = message
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        MessageBoxW(
            Some(hwnd),
            windows::core::PCWSTR::from_raw(msg.as_ptr()),
            windows::core::PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
