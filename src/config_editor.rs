//! Native Win32 structured editor window for editing `config.toml`.
//!
//! Provides a structured GUI with a ListView for gestures, a form panel
//! for adding/editing/deleting gesture mappings, and a settings panel —
//! using `toml_edit` for comment-preserving TOML round-trips.
//!
//! Single-editor enforcement via a global `AtomicIsize`.

use anyhow::Result;
use log::info;
use std::mem::size_of;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use toml_edit::DocumentMut;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, HFONT,
    OUT_DEFAULT_PRECIS,
};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCF_TEXT, LVCOLUMNW,
    LVIF_TEXT, LVITEMW, LVM_DELETEALLITEMS, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMW, LVS_EX_FULLROWSELECT, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, WC_LISTVIEW,
};
use windows::core::PWSTR;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, IsWindow, MessageBoxW, RegisterClassExW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    BS_AUTOCHECKBOX, BS_PUSHBUTTON, CBS_DROPDOWNLIST, ES_AUTOHSCROLL, ES_LEFT, ES_MULTILINE,
    ES_WANTRETURN, GWLP_USERDATA, HMENU, MB_ICONERROR, MB_OK, SM_CXSCREEN, SM_CYSCREEN,
    SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_NCDESTROY, WM_NOTIFY, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_CHILD, WS_EX_CLIENTEDGE,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
};

const EDITOR_CLASS: &str = "MouseGestureEditor\0";
const EDITOR_W: i32 = 800;
const EDITOR_H: i32 = 600;

/// Reload config message, must match the constant in main.rs.
const WM_APP_RELOAD_CONFIG: u32 = WM_APP + 20;

/// Global handle to the single editor window (or 0 if none).
static EDITOR_HWND: AtomicIsize = AtomicIsize::new(0);

// ── Control IDs ────────────────────────────────────────────────

const ID_LISTVIEW: u16 = 1000;
const ID_EDIT_NAME: u16 = 1001;
const ID_EDIT_PATTERN: u16 = 1002;
const ID_COMBO_ACTION_TYPE: u16 = 1003;
const ID_EDIT_ACTION: u16 = 1004;
const ID_BTN_ADD: u16 = 1005;
const ID_BTN_CLEAR: u16 = 1006;
const ID_BTN_DELETE: u16 = 1007;
const ID_EDIT_THRESHOLD: u16 = 1010;
const ID_EDIT_SAMPLE: u16 = 1011;
const ID_EDIT_EPSILON: u16 = 1012;
const ID_EDIT_MIN_LEN: u16 = 1013;
const ID_CHECK_DEBUG: u16 = 1014;
const ID_CHECK_STARTUP: u16 = 1015;
const ID_COMBO_BLACKLIST_MODE: u16 = 1020;
const ID_EDIT_BLACKLIST_APPS: u16 = 1021;
const ID_COMBO_WINDOW_CMD: u16 = 1030;
const ID_EDIT_LAUNCH_PATH: u16 = 1031;
const ID_EDIT_LAUNCH_ARGS: u16 = 1032;

// ── EditorState ────────────────────────────────────────────────

#[allow(dead_code)]
struct EditorState {
    owner: HWND,
    path: PathBuf,
    config: DocumentMut,
    gesture_names: Vec<String>,
    selected_index: Option<usize>,
    dirty: bool,
    h_listview: HWND,
    // Settings panel
    h_edit_threshold: HWND,
    h_edit_sample: HWND,
    h_edit_epsilon: HWND,
    h_edit_min_len: HWND,
    h_check_debug: HWND,
    h_check_startup: HWND,
    // Blacklist
    h_blacklist_mode: HWND,
    h_blacklist_apps: HWND,
    // Gesture form
    h_edit_name: HWND,
    h_edit_pattern: HWND,
    h_combo_action_type: HWND,
    // Action-specific controls
    h_edit_action: HWND,
    h_combo_window_cmd: HWND,
    h_edit_launch_path: HWND,
    h_edit_launch_args: HWND,
    // Buttons
    h_btn_add: HWND,
    h_btn_clear: HWND,
    h_btn_delete: HWND,
    h_btn_save: HWND,
    h_btn_cancel: HWND,
    // Font
    h_font: HFONT,
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

    // Initialize common controls for ListView
    let icc = INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES,
    };
    unsafe {
        let _ = InitCommonControlsEx(&icc);
    }

    // Read config file and parse into DocumentMut
    let config_text = std::fs::read_to_string(&path).unwrap_or_default();
    let config: DocumentMut = config_text
        .parse()
        .unwrap_or_else(|_| DocumentMut::new());

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
            x,
            y,
            EDITOR_W,
            EDITOR_H,
            None,
            None,
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW failed: {:?}", e))?;

    // ── Create child controls ───────────────────────────────

    // ListView on the left
    let lv_style = WINDOW_STYLE(
        WS_CHILD.0 | WS_VISIBLE.0 | WS_VSCROLL.0
            | LVS_REPORT
            | LVS_SINGLESEL
            | LVS_SHOWSELALWAYS,
    );
    let h_listview = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            WC_LISTVIEW,
            windows::core::w!(""),
            lv_style,
            5,
            5,
            530,
            375,
            Some(hwnd),
            Some(HMENU(ID_LISTVIEW as isize as *mut _)),
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (ListView) failed: {:?}", e))?;

    // ── ListView columns ────────────────────────────────
    {
        let col_defs = [
            ("Name\0", 200i32),
            ("Pattern\0", 120),
            ("Action\0", 150),
        ];
        for (i, (name, width)) in col_defs.iter().enumerate() {
            let wide: Vec<u16> = name.encode_utf16().collect();
            let mut col = LVCOLUMNW::default();
            col.mask = LVCF_TEXT;
            col.pszText = PWSTR(wide.as_ptr() as *mut _);
            col.cx = *width;
            unsafe {
                SendMessageW(
                    h_listview,
                    LVM_INSERTCOLUMNW,
                    Some(WPARAM(i)),
                    Some(LPARAM(&col as *const _ as isize)),
                );
            }
        }
    }

    // Enable full-row select
    unsafe {
        SendMessageW(
            h_listview,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            Some(WPARAM(LVS_EX_FULLROWSELECT as usize)),
            Some(LPARAM(LVS_EX_FULLROWSELECT as isize)),
        );
    }

    // Extract gesture names from parsed config
    let gesture_names: Vec<String> = config
        .get("gestures")
        .and_then(|g| g.as_table())
        .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
        .unwrap_or_default();

    // ── Right-side gesture form ─────────────────────────────

    create_label(hwnd, "Gesture Name:", 545, 10, 100, 16)?;
    let h_edit_name = create_edit(hwnd, ID_EDIT_NAME, 545, 28, 245, 22)?;

    create_label(hwnd, "Pattern:", 545, 55, 100, 16)?;
    let h_edit_pattern = create_edit(hwnd, ID_EDIT_PATTERN, 545, 73, 245, 22)?;

    create_label(hwnd, "Action Type:", 545, 100, 100, 16)?;
    let h_combo_action_type = create_combo(hwnd, ID_COMBO_ACTION_TYPE, 545, 118, 245, 200)?;

    // Action-specific controls (initially hidden)
    let h_edit_action = create_edit(hwnd, ID_EDIT_ACTION, 545, 148, 245, 22)?;
    let h_combo_window_cmd = create_combo(hwnd, ID_COMBO_WINDOW_CMD, 545, 148, 245, 200)?;
    let h_edit_launch_path = create_edit(hwnd, ID_EDIT_LAUNCH_PATH, 545, 148, 245, 22)?;
    let h_edit_launch_args = create_edit(hwnd, ID_EDIT_LAUNCH_ARGS, 545, 178, 245, 22)?;

    // Form buttons
    let h_btn_add = create_button(hwnd, ID_BTN_ADD, "&Add", 545, 340, 75, 25)?;
    let h_btn_clear = create_button(hwnd, ID_BTN_CLEAR, "&Clear", 628, 340, 75, 25)?;
    let h_btn_delete = create_button(hwnd, ID_BTN_DELETE, "&Delete", 711, 340, 75, 25)?;

    // ── Settings panel (bottom) ─────────────────────────────

    // Divider line
    create_label(hwnd, "──────────────────────────────────────", 5, 385, 530, 16)?;
    create_label(hwnd, "Settings", 5, 405, 60, 16)?;

    // Row 1: Threshold, Sample, Epsilon
    create_label(hwnd, "Threshold:", 5, 425, 60, 16)?;
    let h_edit_threshold = create_edit(hwnd, ID_EDIT_THRESHOLD, 68, 423, 60, 22)?;

    create_label(hwnd, "Sample:", 138, 425, 50, 16)?;
    let h_edit_sample = create_edit(hwnd, ID_EDIT_SAMPLE, 188, 423, 60, 22)?;

    create_label(hwnd, "Epsilon:", 258, 425, 50, 16)?;
    let h_edit_epsilon = create_edit(hwnd, ID_EDIT_EPSILON, 308, 423, 60, 22)?;

    // Row 2: Min Len, Debug, Startup
    create_label(hwnd, "Min Len:", 5, 452, 55, 16)?;
    let h_edit_min_len = create_edit(hwnd, ID_EDIT_MIN_LEN, 60, 450, 50, 22)?;

    let h_check_debug = create_checkbox(hwnd, ID_CHECK_DEBUG, "Debug logging", 130, 450, 120, 22)?;
    let h_check_startup =
        create_checkbox(hwnd, ID_CHECK_STARTUP, "Start with Windows", 260, 450, 140, 22)?;

    // ── Blacklist section ───────────────────────────────────

    create_label(hwnd, "Blacklist", 5, 480, 60, 16)?;
    create_label(hwnd, "Mode:", 5, 498, 40, 16)?;
    let h_blacklist_mode = create_combo(hwnd, ID_COMBO_BLACKLIST_MODE, 45, 496, 120, 200)?;

    create_label(hwnd, "Apps:", 175, 498, 40, 16)?;
    let h_blacklist_apps = {
        let edit_class: Vec<u16> = "EDIT\0".encode_utf16().collect();
        let style = WINDOW_STYLE(
            (WS_CHILD | WS_VISIBLE | WS_VSCROLL).0
                | (ES_LEFT as u32)
                | (ES_MULTILINE as u32)
                | (ES_AUTOHSCROLL as u32)
                | (ES_WANTRETURN as u32),
        );
        unsafe {
            CreateWindowExW(
                WS_EX_CLIENTEDGE,
                windows::core::PCWSTR::from_raw(edit_class.as_ptr()),
                windows::core::w!(""),
                style,
                220,
                496,
                300,
                50,
                Some(hwnd),
                Some(HMENU(ID_EDIT_BLACKLIST_APPS as isize as *mut _)),
                None,
                None,
            )
        }
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (blacklist apps EDIT) failed: {:?}", e))?;

    // ── Save / Cancel buttons (bottom-right) ────────────────

    let h_btn_cancel = create_button(hwnd, 2, "Cancel", 620, 565, 80, 25)?;
    let h_btn_save = create_button(hwnd, 1, "&Save", 710, 565, 80, 25)?;

    // ── Font ────────────────────────────────────────────────

    let h_font = {
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        unsafe {
            CreateFontW(
                16,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                DEFAULT_QUALITY,
                0,
                windows::core::PCWSTR::from_raw(face.as_ptr()),
            )
        }
    };

    // Apply font to relevant controls
    if !h_font.is_invalid() {
        unsafe {
            SendMessageW(
                h_listview,
                WM_SETFONT,
                Some(WPARAM(h_font.0 as usize)),
                Some(LPARAM(1)),
            );
        }
    }

    // ── Store state ─────────────────────────────────────────

    let path_display = path.display().to_string();
    let state = Box::new(EditorState {
        owner,
        path,
        config,
        gesture_names,
        selected_index: None,
        dirty: false,
        h_listview,
        h_edit_threshold,
        h_edit_sample,
        h_edit_epsilon,
        h_edit_min_len,
        h_check_debug,
        h_check_startup,
        h_blacklist_mode,
        h_blacklist_apps,
        h_edit_name,
        h_edit_pattern,
        h_combo_action_type,
        h_edit_action,
        h_combo_window_cmd,
        h_edit_launch_path,
        h_edit_launch_args,
        h_btn_add,
        h_btn_clear,
        h_btn_delete,
        h_btn_save,
        h_btn_cancel,
        h_font,
    });
    let state_ptr = Box::into_raw(state);
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };

    // Store the editor HWND globally for single-editor enforcement
    EDITOR_HWND.store(hwnd.0 as isize, Ordering::Release);

    // Populate the ListView with gestures
    unsafe {
        populate_listview(&*state_ptr);
    }

    // Show the window
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    info!("Config editor opened: {}", path_display);
    Ok(())
}

// ── Control Creation Helpers ───────────────────────────────────

/// Create a STATIC label control.
fn create_label(parent: HWND, text: &str, x: i32, y: i32, w: i32, h: i32) -> Result<HWND> {
    let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let label_wide: Vec<u16> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0);
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR::from_raw(class.as_ptr()),
            windows::core::PCWSTR::from_raw(label_wide.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            None,
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (STATIC) failed: {:?}", e))
}

/// Create a single-line EDIT control.
fn create_edit(parent: HWND, id: u16, x: i32, y: i32, w: i32, h: i32) -> Result<HWND> {
    let class: Vec<u16> = "EDIT\0".encode_utf16().collect();
    let style = WINDOW_STYLE(
        (WS_CHILD | WS_VISIBLE).0 | (ES_LEFT as u32) | (ES_AUTOHSCROLL as u32),
    );
    unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            windows::core::PCWSTR::from_raw(class.as_ptr()),
            windows::core::w!(""),
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (EDIT) failed: {:?}", e))
}

/// Create a dropdown COMBOBOX control.
fn create_combo(parent: HWND, id: u16, x: i32, y: i32, w: i32, h: i32) -> Result<HWND> {
    let class: Vec<u16> = "COMBOBOX\0".encode_utf16().collect();
    let style = WINDOW_STYLE(
        (WS_CHILD | WS_VISIBLE).0 | (CBS_DROPDOWNLIST as u32),
    );
    unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            windows::core::PCWSTR::from_raw(class.as_ptr()),
            windows::core::w!(""),
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (COMBOBOX) failed: {:?}", e))
}

/// Create a push BUTTON control.
fn create_button(
    parent: HWND,
    id: u16,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<HWND> {
    let class: Vec<u16> = "BUTTON\0".encode_utf16().collect();
    let label_wide: Vec<u16> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (BS_PUSHBUTTON as u32));
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR::from_raw(class.as_ptr()),
            windows::core::PCWSTR::from_raw(label_wide.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (BUTTON) failed: {:?}", e))
}

/// Create a checkbox (BUTTON with `BS_AUTOCHECKBOX` style).
fn create_checkbox(
    parent: HWND,
    id: u16,
    text: &str,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Result<HWND> {
    let class: Vec<u16> = "BUTTON\0".encode_utf16().collect();
    let label_wide: Vec<u16> = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (BS_AUTOCHECKBOX as u32));
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            windows::core::PCWSTR::from_raw(class.as_ptr()),
            windows::core::PCWSTR::from_raw(label_wide.as_ptr()),
            style,
            x,
            y,
            w,
            h,
            Some(parent),
            Some(HMENU(id as isize as *mut _)),
            None,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("CreateWindowExW (CHECKBOX) failed: {:?}", e))
}

// ── Window Class Registration ──────────────────────────────────

fn register_class() -> Result<()> {
    let name: Vec<u16> = EDITOR_CLASS.encode_utf16().collect();
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
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
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if !state_ptr.is_null() {
                unsafe {
                    let state = Box::from_raw(state_ptr);
                    if !state.h_font.is_invalid() {
                        let _ = DeleteObject(state.h_font.into());
                    }
                }
            }
            // Clear global editor handle
            EDITOR_HWND.store(0, Ordering::Release);
            LRESULT(0)
        }

        WM_SIZE => {
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if state_ptr.is_null() {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let state = unsafe { &*state_ptr };

            let client_w = (lparam.0 as u32 & 0xffff) as i32;
            let client_h = ((lparam.0 as u32 >> 16) & 0xffff) as i32;

            // ListView: left side, fills most of the height
            let lv_w = client_w * 2 / 3;
            let lv_h = client_h - 220;
            unsafe {
                let _ = SetWindowPos(
                    state.h_listview,
                    None,
                    5,
                    5,
                    lv_w - 10,
                    lv_h,
                    SWP_NOZORDER,
                );
            }

            // Save / Cancel buttons: bottom-right
            let btn_y = client_h - 35;
            unsafe {
                let _ = SetWindowPos(
                    state.h_btn_cancel,
                    None,
                    client_w - 180,
                    btn_y,
                    80,
                    25,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_btn_save,
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
                1 => {
                    // Save — placeholder
                }
                2 => unsafe {
                    let _ = DestroyWindow(hwnd);
                },
                _ => {}
            }
            LRESULT(0)
        }

        WM_NOTIFY => {
            // Placeholder — will handle ListView selection changes in U3
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ── Helpers ────────────────────────────────────────────────────

#[allow(dead_code)]
/// Read the entire text content of an EDIT control.
fn read_edit_text(edit: HWND) -> Result<String> {
    let len = unsafe { GetWindowTextLengthW(edit) as usize };
    let mut buf: Vec<u16> = vec![0u16; len + 1];
    let actual = unsafe { GetWindowTextW(edit, &mut buf) as usize };
    Ok(String::from_utf16_lossy(&buf[..actual.min(len)]))
}

/// Populate the ListView from `gesture_names` and the parsed `DocumentMut`.
fn populate_listview(state: &EditorState) {
    // Clear all existing items
    unsafe {
        let _ = SendMessageW(
            state.h_listview,
            LVM_DELETEALLITEMS,
            Some(WPARAM(0)),
            Some(LPARAM(0)),
        );
    }

    // Get gestures table
    let gestures = match state.config.get("gestures").and_then(|g| g.as_table()) {
        Some(t) => t,
        None => return,
    };

    for (index, name) in state.gesture_names.iter().enumerate() {
        let gesture = match gestures.get(name.as_str()).and_then(|g| g.as_table()) {
            Some(g) => g,
            None => continue,
        };

        // Pattern
        let pattern = gesture
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Action type label
        let action_label = gesture
            .get("action")
            .and_then(|v| v.as_inline_table())
            .map(|action| {
                let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match action_type {
                    "window" => {
                        let cmd =
                            action.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        format!("Window: {}", cmd)
                    }
                    "key" => {
                        let combo_str = action
                            .get("combo")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join("+")
                            })
                            .unwrap_or_default();
                        format!("Key: {}", combo_str)
                    }
                    "launch" => {
                        let path =
                            action.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        format!("Launch: {}", path)
                    }
                    other => format!("Unknown: {}", other),
                }
            })
            .unwrap_or_else(|| "Unknown".to_string());

        // Column 0: Name
        let mut name_wide: Vec<u16> =
            name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut item = LVITEMW::default();
        item.mask = LVIF_TEXT;
        item.iItem = index as i32;
        item.iSubItem = 0;
        item.pszText = PWSTR(name_wide.as_mut_ptr());
        unsafe {
            SendMessageW(
                state.h_listview,
                LVM_INSERTITEMW,
                Some(WPARAM(0)),
                Some(LPARAM(&item as *const _ as isize)),
            );
        }

        // Column 1: Pattern
        let mut pat_wide: Vec<u16> =
            pattern.encode_utf16().chain(std::iter::once(0)).collect();
        item.iSubItem = 1;
        item.pszText = PWSTR(pat_wide.as_mut_ptr());
        unsafe {
            SendMessageW(
                state.h_listview,
                LVM_SETITEMW,
                Some(WPARAM(0)),
                Some(LPARAM(&item as *const _ as isize)),
            );
        }

        // Column 2: Action
        let mut action_wide: Vec<u16> = action_label
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        item.iSubItem = 2;
        item.pszText = PWSTR(action_wide.as_mut_ptr());
        unsafe {
            SendMessageW(
                state.h_listview,
                LVM_SETITEMW,
                Some(WPARAM(0)),
                Some(LPARAM(&item as *const _ as isize)),
            );
        }
    }
}

/// Show an error dialog.
fn show_error(hwnd: HWND, message: &str) {
    let title: Vec<u16> = "Configuration Error\0".encode_utf16().collect();
    let msg: Vec<u16> = message.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            Some(hwnd),
            windows::core::PCWSTR::from_raw(msg.as_ptr()),
            windows::core::PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}
