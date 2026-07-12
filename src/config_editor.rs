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
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, DeleteObject, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, HFONT,
    OUT_DEFAULT_PRECIS,
};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, LVCF_TEXT, LVCOLUMNW,
    LVIF_STATE, LVIF_TEXT, LVIS_SELECTED, LVITEMW, LVM_DELETEALLITEMS, LVM_DELETEITEM,
    LVM_GETITEMCOUNT, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMW, LVN_ITEMCHANGED, LVS_EX_FULLROWSELECT, LVS_REPORT,
    LVS_SHOWSELALWAYS, LVS_SINGLESEL, NMHDR, NMLISTVIEW, WC_LISTVIEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, IsWindow, MessageBoxW, PostMessageW, RegisterClassExW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_PUSHBUTTON, CBN_SELCHANGE,
    CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, EN_CHANGE, ES_AUTOHSCROLL, ES_LEFT,
    ES_MULTILINE, ES_WANTRETURN, GWLP_USERDATA, HMENU, IDCANCEL, IDNO, IDYES, MB_ICONERROR,
    MB_ICONQUESTION, MB_OK, MB_YESNOCANCEL, MINMAXINFO, SM_CXSCREEN, SM_CYSCREEN, SWP_NOZORDER,
    SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND,
    WM_GETMINMAXINFO, WM_NCDESTROY, WM_NOTIFY, WM_SETFONT, WM_SIZE, WNDCLASSEXW, WS_CHILD,
    WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_VISIBLE, WS_VSCROLL,
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
const ID_BTN_SAVE: u16 = 1008;
const ID_BTN_CANCEL: u16 = 1009;
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
    is_adding: bool,
    dirty: bool,
    h_placeholder: HWND,
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
    // Labels (for repositioning on resize)
    h_label_gesture_name: HWND,
    h_label_pattern: HWND,
    h_label_action_type: HWND,
    h_label_divider: HWND,
    h_label_settings: HWND,
    h_label_threshold: HWND,
    h_label_sample: HWND,
    h_label_epsilon: HWND,
    h_label_min_len: HWND,
    h_label_blacklist: HWND,
    h_label_blacklist_mode: HWND,
    h_label_blacklist_apps: HWND,
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
    let config: DocumentMut = config_text.parse().unwrap_or_else(|_| DocumentMut::new());

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
            PCWSTR::from_raw(class_name.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
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
        WS_CHILD.0 | WS_VISIBLE.0 | WS_VSCROLL.0 | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS,
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
        let col_defs = [("Name\0", 200i32), ("Pattern\0", 120), ("Action\0", 150)];
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

    let h_label_gesture_name = create_label(hwnd, "Gesture Name:", 545, 10, 100, 16)?;
    let h_edit_name = create_edit(hwnd, ID_EDIT_NAME, 545, 28, 245, 22)?;

    let h_label_pattern = create_label(hwnd, "Pattern:", 545, 55, 100, 16)?;
    let h_edit_pattern = create_edit(hwnd, ID_EDIT_PATTERN, 545, 73, 245, 22)?;

    let h_label_action_type = create_label(hwnd, "Action Type:", 545, 100, 100, 16)?;
    let h_combo_action_type = create_combo(hwnd, ID_COMBO_ACTION_TYPE, 545, 118, 245, 200)?;

    // Action-specific controls (initially hidden)
    let h_edit_action = create_edit(hwnd, ID_EDIT_ACTION, 545, 148, 245, 22)?;
    let h_combo_window_cmd = create_combo(hwnd, ID_COMBO_WINDOW_CMD, 545, 148, 245, 200)?;
    let h_edit_launch_path = create_edit(hwnd, ID_EDIT_LAUNCH_PATH, 545, 148, 245, 22)?;
    let h_edit_launch_args = create_edit(hwnd, ID_EDIT_LAUNCH_ARGS, 545, 178, 245, 22)?;

    // Placeholder label (shown when no gesture selected)
    let h_placeholder = create_label(hwnd, "Select a gesture or click Add", 545, 10, 245, 80)?;

    // Form buttons
    let h_btn_add = create_button(hwnd, ID_BTN_ADD, "&Add", 545, 340, 75, 25)?;
    let h_btn_clear = create_button(hwnd, ID_BTN_CLEAR, "&Clear", 628, 340, 75, 25)?;
    let h_btn_delete = create_button(hwnd, ID_BTN_DELETE, "&Delete", 711, 340, 75, 25)?;

    // ── Settings panel (bottom) ─────────────────────────────

    // Divider line
    let h_label_divider = create_label(
        hwnd,
        "──────────────────────────────────────",
        5,
        385,
        530,
        16,
    )?;
    let h_label_settings = create_label(hwnd, "Settings", 5, 405, 60, 16)?;

    // Row 1: Threshold, Sample, Epsilon
    let h_label_threshold = create_label(hwnd, "Threshold:", 5, 425, 60, 16)?;
    let h_edit_threshold = create_edit(hwnd, ID_EDIT_THRESHOLD, 68, 423, 60, 22)?;

    let h_label_sample = create_label(hwnd, "Sample:", 138, 425, 50, 16)?;
    let h_edit_sample = create_edit(hwnd, ID_EDIT_SAMPLE, 188, 423, 60, 22)?;

    let h_label_epsilon = create_label(hwnd, "Epsilon:", 258, 425, 50, 16)?;
    let h_edit_epsilon = create_edit(hwnd, ID_EDIT_EPSILON, 308, 423, 60, 22)?;

    // Row 2: Min Len, Debug, Startup
    let h_label_min_len = create_label(hwnd, "Min Len:", 5, 452, 55, 16)?;
    let h_edit_min_len = create_edit(hwnd, ID_EDIT_MIN_LEN, 60, 450, 50, 22)?;

    let h_check_debug = create_checkbox(hwnd, ID_CHECK_DEBUG, "Debug logging", 130, 450, 120, 22)?;
    let h_check_startup = create_checkbox(
        hwnd,
        ID_CHECK_STARTUP,
        "Start with Windows",
        260,
        450,
        140,
        22,
    )?;

    // ── Blacklist section ───────────────────────────────────

    let h_label_blacklist = create_label(hwnd, "Blacklist", 5, 480, 60, 16)?;
    let h_label_blacklist_mode = create_label(hwnd, "Mode:", 5, 498, 40, 16)?;
    let h_blacklist_mode = create_combo(hwnd, ID_COMBO_BLACKLIST_MODE, 45, 496, 120, 200)?;

    let h_label_blacklist_apps = create_label(hwnd, "Apps:", 175, 498, 40, 16)?;
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
                PCWSTR::from_raw(edit_class.as_ptr()),
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

    let h_btn_cancel = create_button(hwnd, ID_BTN_CANCEL, "Cancel", 620, 565, 80, 25)?;
    let h_btn_save = create_button(hwnd, ID_BTN_SAVE, "&Save", 710, 565, 80, 25)?;

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
                PCWSTR::from_raw(face.as_ptr()),
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
        is_adding: false,
        dirty: false,
        h_placeholder,
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
        h_label_gesture_name,
        h_label_pattern,
        h_label_action_type,
        h_label_divider,
        h_label_settings,
        h_label_threshold,
        h_label_sample,
        h_label_epsilon,
        h_label_min_len,
        h_label_blacklist,
        h_label_blacklist_mode,
        h_label_blacklist_apps,
        h_font,
    });
    let state_ptr = Box::into_raw(state);
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize) };

    // Store the editor HWND globally for single-editor enforcement
    EDITOR_HWND.store(hwnd.0 as isize, Ordering::Release);

    // Populate ComboBoxes
    populate_action_type_combo(h_combo_action_type);
    populate_window_cmd_combo(h_combo_window_cmd);
    populate_blacklist_combo(h_blacklist_mode);

    // Populate the ListView with gestures
    unsafe {
        populate_listview(&*state_ptr);
    }

    // Populate settings and blacklist from config
    populate_settings_from_doc(unsafe { &*state_ptr });
    populate_blacklist_from_doc(unsafe { &*state_ptr });

    // Initial form visibility
    unsafe {
        update_form_visibility(&*state_ptr);
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
    let label_wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0);
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(label_wide.as_ptr()),
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
    let style =
        WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (ES_LEFT as u32) | (ES_AUTOHSCROLL as u32));
    unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            PCWSTR::from_raw(class.as_ptr()),
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
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (CBS_DROPDOWNLIST as u32));
    unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            PCWSTR::from_raw(class.as_ptr()),
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
    let label_wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (BS_PUSHBUTTON as u32));
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(label_wide.as_ptr()),
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
    let label_wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let style = WINDOW_STYLE((WS_CHILD | WS_VISIBLE).0 | (BS_AUTOCHECKBOX as u32));
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR::from_raw(class.as_ptr()),
            PCWSTR::from_raw(label_wide.as_ptr()),
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
        lpszClassName: PCWSTR::from_raw(name.as_ptr()),
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
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if !state_ptr.is_null() {
                let state = unsafe { &mut *state_ptr };
                if state.dirty {
                    let title: Vec<u16> = "Unsaved Changes\0".encode_utf16().collect();
                    let msg: Vec<u16> = "Save changes to configuration?\0".encode_utf16().collect();
                    let result = unsafe {
                        MessageBoxW(
                            Some(hwnd),
                            PCWSTR::from_raw(msg.as_ptr()),
                            PCWSTR::from_raw(title.as_ptr()),
                            MB_YESNOCANCEL | MB_ICONQUESTION,
                        )
                    };
                    match result {
                        IDYES => unsafe {
                            handle_save(hwnd, state);
                            let _ = DestroyWindow(hwnd);
                        },
                        IDNO => unsafe {
                            let _ = DestroyWindow(hwnd);
                        },
                        IDCANCEL => return LRESULT(0),
                        _ => unsafe {
                            let _ = DestroyWindow(hwnd);
                        },
                    }
                } else {
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                }
            } else {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
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

        WM_GETMINMAXINFO => {
            let minmax = &mut *(lparam.0 as *mut MINMAXINFO);
            minmax.ptMinTrackSize = POINT { x: 700, y: 500 };
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

            // ── Top section ────────────────────────────────
            let top_h = client_h - 220;

            // ListView: left 2/3, fills from top to settings
            let lv_w = client_w * 2 / 3 - 15;
            unsafe {
                let _ = SetWindowPos(state.h_listview, None, 5, 5, lv_w, top_h - 10, SWP_NOZORDER);
            }

            // ── Form panel: right 1/3 ──────────────────────
            let form_x = client_w * 2 / 3 + 5;
            let form_w = client_w / 3 - 10;
            let ctrl_w = form_w - 5;

            unsafe {
                // Labels and edits
                let _ = SetWindowPos(
                    state.h_label_gesture_name,
                    None,
                    form_x,
                    10,
                    form_w,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_name,
                    None,
                    form_x,
                    28,
                    ctrl_w,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_pattern,
                    None,
                    form_x,
                    55,
                    form_w,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_pattern,
                    None,
                    form_x,
                    73,
                    ctrl_w,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_action_type,
                    None,
                    form_x,
                    100,
                    form_w,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_combo_action_type,
                    None,
                    form_x,
                    118,
                    ctrl_w,
                    200,
                    SWP_NOZORDER,
                );

                // Action-specific controls
                let _ = SetWindowPos(
                    state.h_edit_action,
                    None,
                    form_x,
                    148,
                    ctrl_w,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_combo_window_cmd,
                    None,
                    form_x,
                    148,
                    ctrl_w,
                    200,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_launch_path,
                    None,
                    form_x,
                    148,
                    ctrl_w,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_launch_args,
                    None,
                    form_x,
                    178,
                    ctrl_w,
                    22,
                    SWP_NOZORDER,
                );

                // Placeholder
                let _ = SetWindowPos(
                    state.h_placeholder,
                    None,
                    form_x,
                    10,
                    form_w,
                    80,
                    SWP_NOZORDER,
                );

                // Form buttons
                let _ = SetWindowPos(state.h_btn_add, None, form_x, 340, 75, 25, SWP_NOZORDER);
                let _ = SetWindowPos(
                    state.h_btn_clear,
                    None,
                    form_x + 83,
                    340,
                    75,
                    25,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_btn_delete,
                    None,
                    form_x + 166,
                    340,
                    75,
                    25,
                    SWP_NOZORDER,
                );
            }

            // ── Settings panel (bottom) ─────────────────────
            let settings_y = client_h - 220;

            unsafe {
                let _ = SetWindowPos(
                    state.h_label_divider,
                    None,
                    5,
                    settings_y + 5,
                    client_w - 10,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_settings,
                    None,
                    5,
                    settings_y + 25,
                    60,
                    16,
                    SWP_NOZORDER,
                );

                // Row 1: Threshold, Sample, Epsilon
                let _ = SetWindowPos(
                    state.h_label_threshold,
                    None,
                    5,
                    settings_y + 45,
                    60,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_threshold,
                    None,
                    68,
                    settings_y + 43,
                    60,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_sample,
                    None,
                    138,
                    settings_y + 45,
                    50,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_sample,
                    None,
                    188,
                    settings_y + 43,
                    60,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_epsilon,
                    None,
                    258,
                    settings_y + 45,
                    50,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_epsilon,
                    None,
                    308,
                    settings_y + 43,
                    60,
                    22,
                    SWP_NOZORDER,
                );

                // Row 2: Min Len, Debug, Startup
                let _ = SetWindowPos(
                    state.h_label_min_len,
                    None,
                    5,
                    settings_y + 72,
                    55,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_edit_min_len,
                    None,
                    60,
                    settings_y + 70,
                    50,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_check_debug,
                    None,
                    130,
                    settings_y + 70,
                    120,
                    22,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_check_startup,
                    None,
                    260,
                    settings_y + 70,
                    140,
                    22,
                    SWP_NOZORDER,
                );

                // ── Blacklist section ───────────────────────
                let _ = SetWindowPos(
                    state.h_label_blacklist,
                    None,
                    5,
                    settings_y + 100,
                    60,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_blacklist_mode,
                    None,
                    5,
                    settings_y + 118,
                    40,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_blacklist_mode,
                    None,
                    45,
                    settings_y + 116,
                    120,
                    200,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_label_blacklist_apps,
                    None,
                    175,
                    settings_y + 118,
                    40,
                    16,
                    SWP_NOZORDER,
                );
                let _ = SetWindowPos(
                    state.h_blacklist_apps,
                    None,
                    220,
                    settings_y + 116,
                    client_w - 230,
                    50,
                    SWP_NOZORDER,
                );

                // ── Save / Cancel buttons ───────────────────
                let btn_y = client_h - 35;
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
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if state_ptr.is_null() {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let state = unsafe { &mut *state_ptr };

            let cmd = (wparam.0 as u32 & 0xffff) as u16;
            let notify = (wparam.0 as u32 >> 16) as u32;

            match cmd {
                // ── Save / Cancel ─────────────────────────
                ID_BTN_SAVE if notify == BN_CLICKED => unsafe {
                    handle_save(hwnd, state);
                },
                ID_BTN_CANCEL if notify == BN_CLICKED => unsafe {
                    let _ = DestroyWindow(hwnd);
                },

                // ── Form buttons ──────────────────────────
                ID_BTN_ADD if notify == BN_CLICKED => unsafe {
                    handle_add_click(state);
                },
                ID_BTN_CLEAR if notify == BN_CLICKED => unsafe {
                    clear_form(state);
                },
                ID_BTN_DELETE if notify == BN_CLICKED => unsafe {
                    delete_selected_gesture(state);
                },

                // ── Action type ComboBox ──────────────────
                ID_COMBO_ACTION_TYPE if notify == CBN_SELCHANGE => unsafe {
                    handle_action_type_change(state);
                },

                // ── Name / Pattern edits ──────────────────
                ID_EDIT_NAME if notify == EN_CHANGE => unsafe {
                    handle_name_change(state);
                },
                ID_EDIT_PATTERN if notify == EN_CHANGE => unsafe {
                    handle_pattern_change(state);
                },

                // ── Window command ComboBox ───────────────
                ID_COMBO_WINDOW_CMD if notify == CBN_SELCHANGE => unsafe {
                    handle_window_cmd_change(state);
                },

                // ── Keyboard combo edit ───────────────────
                ID_EDIT_ACTION if notify == EN_CHANGE => unsafe {
                    handle_key_combo_change(state);
                },

                // ── Launch path / args ────────────────────
                ID_EDIT_LAUNCH_PATH if notify == EN_CHANGE => unsafe {
                    handle_launch_change(state);
                },
                ID_EDIT_LAUNCH_ARGS if notify == EN_CHANGE => unsafe {
                    handle_launch_change(state);
                },

                // ── Settings edits ────────────────────────
                ID_EDIT_THRESHOLD if notify == EN_CHANGE => unsafe {
                    handle_setting_change(
                        state,
                        "activation_threshold_dip",
                        state.h_edit_threshold,
                    );
                },
                ID_EDIT_SAMPLE if notify == EN_CHANGE => unsafe {
                    handle_setting_change(state, "sample_distance_dip", state.h_edit_sample);
                },
                ID_EDIT_EPSILON if notify == EN_CHANGE => unsafe {
                    handle_setting_change(state, "rdp_epsilon_dip", state.h_edit_epsilon);
                },
                ID_EDIT_MIN_LEN if notify == EN_CHANGE => unsafe {
                    handle_setting_change(state, "min_gesture_length", state.h_edit_min_len);
                },

                // ── Checkboxes ────────────────────────────
                ID_CHECK_DEBUG if notify == BN_CLICKED => unsafe {
                    handle_checkbox_change(state, "debug_logging", state.h_check_debug);
                },
                ID_CHECK_STARTUP if notify == BN_CLICKED => unsafe {
                    handle_checkbox_change(state, "start_with_windows", state.h_check_startup);
                },

                // ── Blacklist ─────────────────────────────
                ID_COMBO_BLACKLIST_MODE if notify == CBN_SELCHANGE => unsafe {
                    handle_blacklist_mode_change(state);
                },
                ID_EDIT_BLACKLIST_APPS if notify == EN_CHANGE => unsafe {
                    handle_blacklist_apps_change(state);
                },

                _ => {}
            }
            LRESULT(0)
        }

        WM_NOTIFY => {
            let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut EditorState };
            if state_ptr.is_null() {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            let state = unsafe { &mut *state_ptr };

            let nmhdr = unsafe { &*(lparam.0 as *const NMHDR) };
            if nmhdr.idFrom == ID_LISTVIEW as usize && nmhdr.code == LVN_ITEMCHANGED {
                let nmlv = unsafe { &*(lparam.0 as *const NMLISTVIEW) };
                let changed = nmlv.uChanged.0;
                if (changed & LVIF_STATE.0) != 0 {
                    let newly_selected = (nmlv.uNewState & 2u32) != 0;
                    let was_selected = (nmlv.uOldState & 2u32) != 0;
                    if newly_selected != was_selected {
                        unsafe {
                            handle_list_selection_change(state, newly_selected, nmlv.iItem);
                        }
                    }
                }
            }
            LRESULT(0)
        }

        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ── ComboBox Population Helpers ────────────────────────────────

/// Populate the action type ComboBox with entries.
fn populate_action_type_combo(combo: HWND) {
    let action_types = ["Window Command\0", "Keyboard Combo\0", "Launch\0"];
    for text in &action_types {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(wide.as_ptr() as isize)),
            );
        }
    }
}

/// Populate the window command ComboBox with all variants.
fn populate_window_cmd_combo(combo: HWND) {
    let commands = [
        "Maximize\0",
        "Minimize\0",
        "Restore\0",
        "Close\0",
        "SnapLeft\0",
        "SnapRight\0",
        "SnapTop\0",
        "SnapBottom\0",
        "SnapTopLeft\0",
        "SnapTopRight\0",
        "SnapBottomLeft\0",
        "SnapBottomRight\0",
        "Center\0",
        "ToggleAlwaysOnTop\0",
    ];
    for text in &commands {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(wide.as_ptr() as isize)),
            );
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────

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
                        let cmd = action.get("command").and_then(|v| v.as_str()).unwrap_or("");
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
                        let path = action.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        format!("Launch: {}", path)
                    }
                    other => format!("Unknown: {}", other),
                }
            })
            .unwrap_or_else(|| "Unknown".to_string());

        // Column 0: Name
        let mut name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
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
        let mut pat_wide: Vec<u16> = pattern.encode_utf16().chain(std::iter::once(0)).collect();
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
            PCWSTR::from_raw(msg.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

// ── Form visibility management ────────────────────────────────

/// Show/hide form controls vs placeholder based on selection/add state.
fn update_form_visibility(state: &EditorState) {
    let has_selection = state.selected_index.is_some() || state.is_adding;

    let show = if has_selection { SW_SHOW } else { SW_HIDE };
    let hide = if has_selection { SW_HIDE } else { SW_SHOW };

    unsafe {
        let _ = ShowWindow(state.h_placeholder, hide);
        let _ = ShowWindow(state.h_edit_name, show);
        let _ = ShowWindow(state.h_edit_pattern, show);
        let _ = ShowWindow(state.h_combo_action_type, show);

        if has_selection {
            let action_type = get_action_type_selection(state);
            update_action_control_visibility(state, action_type);
        } else {
            let _ = ShowWindow(state.h_edit_action, SW_HIDE);
            let _ = ShowWindow(state.h_combo_window_cmd, SW_HIDE);
            let _ = ShowWindow(state.h_edit_launch_path, SW_HIDE);
            let _ = ShowWindow(state.h_edit_launch_args, SW_HIDE);
        }
    }

    let delete_enabled = state.selected_index.is_some() && !state.is_adding;
    unsafe {
        let _ = EnableWindow(state.h_btn_delete, delete_enabled);
    }
}

/// Show the appropriate action-parameter control(s) for the given action type.
unsafe fn update_action_control_visibility(state: &EditorState, action_type: usize) {
    let _ = ShowWindow(state.h_edit_action, SW_HIDE);
    let _ = ShowWindow(state.h_combo_window_cmd, SW_HIDE);
    let _ = ShowWindow(state.h_edit_launch_path, SW_HIDE);
    let _ = ShowWindow(state.h_edit_launch_args, SW_HIDE);

    match action_type {
        0 => {
            let _ = ShowWindow(state.h_combo_window_cmd, SW_SHOW);
        }
        1 => {
            let _ = ShowWindow(state.h_edit_action, SW_SHOW);
        }
        2 => {
            let _ = ShowWindow(state.h_edit_launch_path, SW_SHOW);
            let _ = ShowWindow(state.h_edit_launch_args, SW_SHOW);
        }
        _ => {}
    }
}

/// Get the currently selected action type from the ComboBox.
unsafe fn get_action_type_selection(state: &EditorState) -> usize {
    let sel = SendMessageW(
        state.h_combo_action_type,
        CB_GETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    sel.0 as usize
}

/// Set edit control text.
unsafe fn set_edit_text(edit: HWND, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = SetWindowTextW(edit, PCWSTR::from_raw(wide.as_ptr()));
}

// ── Form population ───────────────────────────────────────────

/// Populate form fields from the gesture at the given index.
unsafe fn populate_form_for_gesture(state: &EditorState, index: usize) {
    let name = &state.gesture_names[index];

    let gestures = state.config.get("gestures").and_then(|g| g.as_table());
    let gesture = gestures
        .and_then(|t| t.get(name.as_str()))
        .and_then(|v| v.as_table());

    let Some(gesture) = gesture else {
        return;
    };

    let pattern = gesture
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    set_edit_text(state.h_edit_name, name);
    set_edit_text(state.h_edit_pattern, pattern);

    let action = gesture.get("action").and_then(|v| v.as_inline_table());
    if let Some(action) = action {
        let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match action_type {
            "window" => {
                SendMessageW(
                    state.h_combo_action_type,
                    CB_SETCURSEL,
                    Some(WPARAM(0)),
                    Some(LPARAM(0)),
                );

                let cmd = action.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let cmd_upper = cmd.to_uppercase();
                let window_cmds: [&str; 14] = [
                    "MAXIMIZE",
                    "MINIMIZE",
                    "RESTORE",
                    "CLOSE",
                    "SNAPLEFT",
                    "SNAPRIGHT",
                    "SNAPTOP",
                    "SNAPBOTTOM",
                    "SNAPTOPLEFT",
                    "SNAPTOPRIGHT",
                    "SNAPBOTTOMLEFT",
                    "SNAPBOTTOMRIGHT",
                    "CENTER",
                    "TOGGLEALWAYSONTOP",
                ];
                let idx = window_cmds
                    .iter()
                    .position(|&c| c == cmd_upper.as_str())
                    .unwrap_or(0);
                SendMessageW(
                    state.h_combo_window_cmd,
                    CB_SETCURSEL,
                    Some(WPARAM(idx)),
                    Some(LPARAM(0)),
                );
            }
            "key" => {
                SendMessageW(
                    state.h_combo_action_type,
                    CB_SETCURSEL,
                    Some(WPARAM(1)),
                    Some(LPARAM(0)),
                );

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
                set_edit_text(state.h_edit_action, &combo_str);
            }
            "launch" => {
                SendMessageW(
                    state.h_combo_action_type,
                    CB_SETCURSEL,
                    Some(WPARAM(2)),
                    Some(LPARAM(0)),
                );

                let path = action.get("path").and_then(|v| v.as_str()).unwrap_or("");
                let args_str = action
                    .get("args")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                set_edit_text(state.h_edit_launch_path, path);
                set_edit_text(state.h_edit_launch_args, &args_str);
            }
            _ => {}
        }
    }

    update_action_control_visibility(state, get_action_type_selection(state));
}

// ── Form clearing ─────────────────────────────────────────────

/// Clear all form fields, deselect list, exit add mode.
unsafe fn clear_form(state: &mut EditorState) {
    set_edit_text(state.h_edit_name, "");
    set_edit_text(state.h_edit_pattern, "");
    set_edit_text(state.h_edit_action, "");
    set_edit_text(state.h_edit_launch_path, "");
    set_edit_text(state.h_edit_launch_args, "");

    SendMessageW(
        state.h_combo_action_type,
        CB_SETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    SendMessageW(
        state.h_combo_window_cmd,
        CB_SETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );

    deselect_listview(state);

    state.is_adding = false;
    state.selected_index = None;
    set_edit_text(state.h_btn_add, "&Add");

    update_form_visibility(state);
}

/// Deselect all items in the ListView.
unsafe fn deselect_listview(state: &EditorState) {
    let count = SendMessageW(
        state.h_listview,
        LVM_GETITEMCOUNT,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    let item_count = count.0 as i32;
    for _i in 0..item_count {
        let mut item = LVITEMW::default();
        item.stateMask = LVIS_SELECTED;
        item.state = LVIS_SELECTED;
        SendMessageW(
            state.h_listview,
            LVM_SETITEMW,
            Some(WPARAM(0)),
            Some(LPARAM(&item as *const _ as isize)),
        );
    }
}

// ── ListView selection handling ───────────────────────────────

/// Handle a ListView selection change.
unsafe fn handle_list_selection_change(state: &mut EditorState, selected: bool, item: i32) {
    if selected && item >= 0 {
        if state.is_adding {
            state.is_adding = false;
            set_edit_text(state.h_btn_add, "&Add");
        }

        // Auto-flush current form values into DocumentMut before switching
        flush_current_form_to_doc(state);

        state.selected_index = Some(item as usize);
        populate_form_for_gesture(state, item as usize);
        update_form_visibility(state);
        let _ = EnableWindow(state.h_btn_delete, true);
    } else if !selected {
        let next = SendMessageW(
            state.h_listview,
            LVM_GETNEXTITEM,
            Some(WPARAM(usize::MAX)),
            Some(LPARAM(2u32 as isize)), // LVNI_SELECTED
        );
        if next.0 == -1 {
            if !state.is_adding {
                state.selected_index = None;
                update_form_visibility(state);
            }
        }
    }
}

// ── Add button ────────────────────────────────────────────────

/// Handle Add button click: enter add mode or save new gesture.
unsafe fn handle_add_click(state: &mut EditorState) {
    if state.is_adding {
        if validate_and_insert_gesture(state) {
            state.is_adding = false;
            set_edit_text(state.h_btn_add, "&Add");
            state.dirty = true;
            update_form_visibility(state);
        }
    } else {
        clear_form_fields(state);
        state.is_adding = true;
        state.selected_index = None;
        set_edit_text(state.h_btn_add, "&Save");
        update_form_visibility(state);
        let _ = SetFocus(Some(state.h_edit_name));
    }
}

/// Clear form fields without deselecting list or changing mode.
unsafe fn clear_form_fields(state: &EditorState) {
    set_edit_text(state.h_edit_name, "");
    set_edit_text(state.h_edit_pattern, "");
    set_edit_text(state.h_edit_action, "");
    set_edit_text(state.h_edit_launch_path, "");
    set_edit_text(state.h_edit_launch_args, "");

    SendMessageW(
        state.h_combo_action_type,
        CB_SETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    SendMessageW(
        state.h_combo_window_cmd,
        CB_SETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
}

// ── Delete button ─────────────────────────────────────────────

/// Delete the selected gesture from DocumentMut and ListView.
unsafe fn delete_selected_gesture(state: &mut EditorState) {
    let Some(index) = state.selected_index else {
        return;
    };

    if index >= state.gesture_names.len() {
        return;
    }

    let name = state.gesture_names[index].clone();

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            table.remove(&name);
        }
    }

    state.gesture_names.remove(index);

    SendMessageW(
        state.h_listview,
        LVM_DELETEITEM,
        Some(WPARAM(index)),
        Some(LPARAM(0)),
    );

    state.selected_index = None;
    state.dirty = true;

    clear_form(state);
}

// ── Action type switching ─────────────────────────────────────

/// Handle action type ComboBox selection change.
unsafe fn handle_action_type_change(state: &mut EditorState) {
    let action_type = get_action_type_selection(state);
    update_action_control_visibility(state, action_type);

    if let Some(index) = state.selected_index {
        if index < state.gesture_names.len() && !state.is_adding {
            let name = &state.gesture_names[index].clone();
            update_doc_action_type(state, name, action_type);
            state.dirty = true;
            update_listview_row(state, index);
        }
    }
}

/// Update the action type in DocumentMut for a gesture.
unsafe fn update_doc_action_type(state: &mut EditorState, name: &str, action_type: usize) {
    let Some(gestures) = state.config.get_mut("gestures") else {
        return;
    };
    let Some(table) = gestures.as_table_mut() else {
        return;
    };
    let Some(gesture) = table.get_mut(name) else {
        return;
    };
    let Some(gesture_table) = gesture.as_table_mut() else {
        return;
    };

    gesture_table.remove("action");

    let mut action = toml_edit::InlineTable::new();
    match action_type {
        0 => {
            action.insert("type", "window".into());
            action.insert("command", "Maximize".into());
        }
        1 => {
            action.insert("type", "key".into());
            let arr = toml_edit::Array::new();
            action.insert("combo", toml_edit::Value::Array(arr));
        }
        2 => {
            action.insert("type", "launch".into());
            action.insert("path", "".into());
            let arr = toml_edit::Array::new();
            action.insert("args", toml_edit::Value::Array(arr));
        }
        _ => return,
    }
    gesture_table.insert(
        "action",
        toml_edit::Item::Value(toml_edit::Value::InlineTable(action)),
    );
}

// ── Name field change ─────────────────────────────────────────

/// Handle name edit change: rename key in DocumentMut for existing gestures.
unsafe fn handle_name_change(state: &mut EditorState) {
    if state.is_adding {
        return;
    }

    let Some(index) = state.selected_index else {
        return;
    };
    if index >= state.gesture_names.len() {
        return;
    }

    let old_name = state.gesture_names[index].clone();
    let new_name = match read_edit_text(state.h_edit_name) {
        Ok(s) => s,
        Err(_) => return,
    };

    if new_name.is_empty() || new_name == old_name {
        return;
    }

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            let existing = table.get(&old_name).cloned();
            if let Some(item) = existing {
                table.remove(&old_name);
                table.insert(&new_name, item);
            }
        }
    }

    state.gesture_names[index] = new_name;
    state.dirty = true;

    update_listview_row(state, index);
}

// ── Pattern field change ──────────────────────────────────────

/// Handle pattern edit change: update in DocumentMut.
unsafe fn handle_pattern_change(state: &mut EditorState) {
    if state.is_adding {
        return;
    }

    let Some(index) = state.selected_index else {
        return;
    };
    if index >= state.gesture_names.len() {
        return;
    }

    let pattern = match read_edit_text(state.h_edit_pattern) {
        Ok(p) => p,
        Err(_) => return,
    };

    let name = &state.gesture_names[index];

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            if let Some(gesture) = table.get_mut(name.as_str()) {
                if let Some(gesture_table) = gesture.as_table_mut() {
                    gesture_table.insert("pattern", toml_edit::value(&*pattern));
                }
            }
        }
    }

    state.dirty = true;
    update_listview_row(state, index);
}

// ── Window command change ─────────────────────────────────────

/// Handle window command ComboBox change.
unsafe fn handle_window_cmd_change(state: &mut EditorState) {
    if state.is_adding {
        return;
    }

    let Some(index) = state.selected_index else {
        return;
    };
    if index >= state.gesture_names.len() {
        return;
    }

    let sel = SendMessageW(
        state.h_combo_window_cmd,
        CB_GETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    if sel.0 < 0 {
        return;
    }

    let cmd = get_window_cmd_name(sel.0 as usize);
    let name = &state.gesture_names[index];

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            if let Some(gesture) = table.get_mut(name.as_str()) {
                if let Some(gesture_table) = gesture.as_table_mut() {
                    if let Some(action) = gesture_table.get_mut("action") {
                        if let Some(action_table) = action.as_inline_table_mut() {
                            action_table.insert("command", cmd.into());
                        }
                    }
                }
            }
        }
    }

    state.dirty = true;
    update_listview_row(state, index);
}

/// Map ComboBox index to WindowCommand string.
fn get_window_cmd_name(idx: usize) -> &'static str {
    match idx {
        0 => "Maximize",
        1 => "Minimize",
        2 => "Restore",
        3 => "Close",
        4 => "SnapLeft",
        5 => "SnapRight",
        6 => "SnapTop",
        7 => "SnapBottom",
        8 => "SnapTopLeft",
        9 => "SnapTopRight",
        10 => "SnapBottomLeft",
        11 => "SnapBottomRight",
        12 => "Center",
        13 => "ToggleAlwaysOnTop",
        _ => "Maximize",
    }
}

// ── Keyboard combo change ─────────────────────────────────────

/// Handle keyboard combo edit change.
unsafe fn handle_key_combo_change(state: &mut EditorState) {
    if state.is_adding {
        return;
    }

    let Some(index) = state.selected_index else {
        return;
    };
    if index >= state.gesture_names.len() {
        return;
    }

    let combo_text = match read_edit_text(state.h_edit_action) {
        Ok(c) => c,
        Err(_) => return,
    };

    let name = &state.gesture_names[index];

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            if let Some(gesture) = table.get_mut(name.as_str()) {
                if let Some(gesture_table) = gesture.as_table_mut() {
                    if let Some(action) = gesture_table.get_mut("action") {
                        if let Some(action_table) = action.as_inline_table_mut() {
                            let mut arr = toml_edit::Array::new();
                            for token in combo_text.split('+').map(|s| s.trim()) {
                                if !token.is_empty() {
                                    arr.push(toml_edit::Value::String(toml_edit::Formatted::new(
                                        token.to_string(),
                                    )));
                                }
                            }
                            action_table.insert("combo", toml_edit::Value::Array(arr));
                        }
                    }
                }
            }
        }
    }

    state.dirty = true;
    update_listview_row(state, index);
}

// ── Launch path/args change ───────────────────────────────────

/// Handle launch path or args edit change.
unsafe fn handle_launch_change(state: &mut EditorState) {
    if state.is_adding {
        return;
    }

    let Some(index) = state.selected_index else {
        return;
    };
    if index >= state.gesture_names.len() {
        return;
    }

    let path = read_edit_text(state.h_edit_launch_path).unwrap_or_default();
    let args_text = read_edit_text(state.h_edit_launch_args).unwrap_or_default();

    let name = &state.gesture_names[index];

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            if let Some(gesture) = table.get_mut(name.as_str()) {
                if let Some(gesture_table) = gesture.as_table_mut() {
                    if let Some(action) = gesture_table.get_mut("action") {
                        if let Some(action_table) = action.as_inline_table_mut() {
                            action_table.insert("path", (path.as_str()).into());

                            let mut arr = toml_edit::Array::new();
                            for token in args_text.split_whitespace() {
                                arr.push(toml_edit::Value::String(toml_edit::Formatted::new(
                                    token.to_string(),
                                )));
                            }
                            action_table.insert("args", toml_edit::Value::Array(arr));
                        }
                    }
                }
            }
        }
    }

    state.dirty = true;
    update_listview_row(state, index);
}

// ── ListView row update ───────────────────────────────────────

/// Refresh a single ListView row to reflect current DocumentMut state.
unsafe fn update_listview_row(state: &EditorState, index: usize) {
    if index >= state.gesture_names.len() {
        return;
    }

    let name = &state.gesture_names[index];
    let gestures = state.config.get("gestures").and_then(|g| g.as_table());
    let gesture = gestures
        .and_then(|t| t.get(name.as_str()))
        .and_then(|v| v.as_table());
    let Some(gesture) = gesture else {
        return;
    };

    let pattern = gesture
        .get("pattern")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let action_label = gesture
        .get("action")
        .and_then(|v| v.as_inline_table())
        .map(|action| {
            let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match action_type {
                "window" => {
                    let cmd = action.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    format!("Window: {}", cmd)
                }
                "key" => {
                    let combo_str = action
                        .get("combo")
                        .and_then(|v| v.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join("+")
                        })
                        .unwrap_or_default();
                    format!("Key: {}", combo_str)
                }
                "launch" => {
                    let path = action.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    format!("Launch: {}", path)
                }
                other => format!("Unknown: {}", other),
            }
        })
        .unwrap_or_else(|| "Unknown".to_string());

    let mut item = LVITEMW::default();
    item.mask = LVIF_TEXT;

    // Column 0: Name
    let mut name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    item.iItem = index as i32;
    item.iSubItem = 0;
    item.pszText = PWSTR(name_wide.as_mut_ptr());
    SendMessageW(
        state.h_listview,
        LVM_SETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&item as *const _ as isize)),
    );

    // Column 1: Pattern
    let mut pat_wide: Vec<u16> = pattern.encode_utf16().chain(std::iter::once(0)).collect();
    item.iSubItem = 1;
    item.pszText = PWSTR(pat_wide.as_mut_ptr());
    SendMessageW(
        state.h_listview,
        LVM_SETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&item as *const _ as isize)),
    );

    // Column 2: Action
    let mut action_wide: Vec<u16> = action_label
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    item.iSubItem = 2;
    item.pszText = PWSTR(action_wide.as_mut_ptr());
    SendMessageW(
        state.h_listview,
        LVM_SETITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&item as *const _ as isize)),
    );
}

// ── Validation ────────────────────────────────────────────────

/// Validate form fields and insert new gesture into DocumentMut.
/// Returns true on success.
unsafe fn validate_and_insert_gesture(state: &mut EditorState) -> bool {
    let name = match read_edit_text(state.h_edit_name) {
        Ok(n) => n.trim().to_string(),
        Err(_) => {
            show_error(state.h_listview, "Failed to read gesture name.");
            return false;
        }
    };

    if name.is_empty() {
        show_error(state.h_listview, "Gesture name cannot be empty.");
        let _ = SetFocus(Some(state.h_edit_name));
        return false;
    }

    if state.gesture_names.contains(&name) {
        show_error(
            state.h_listview,
            &format!("A gesture named '{}' already exists.", name),
        );
        let _ = SetFocus(Some(state.h_edit_name));
        return false;
    }

    let pattern = match read_edit_text(state.h_edit_pattern) {
        Ok(p) => p.trim().to_string(),
        Err(_) => {
            show_error(state.h_listview, "Failed to read gesture pattern.");
            return false;
        }
    };

    if pattern.is_empty() {
        show_error(state.h_listview, "Pattern cannot be empty.");
        let _ = SetFocus(Some(state.h_edit_pattern));
        return false;
    }
    if !validate_pattern_tokens(&pattern) {
        show_error(
            state.h_listview,
            "Pattern contains invalid direction tokens. Valid: N/S/E/W/NE/SE/SW/NW (or U/D/L/R/UR/DR/DL/UL).",
        );
        let _ = SetFocus(Some(state.h_edit_pattern));
        return false;
    }

    let action_type = get_action_type_selection(state);
    match action_type {
        0 => {} // Window command — already validated by ComboBox
        1 => {
            let combo = read_edit_text(state.h_edit_action).unwrap_or_default();
            if combo.trim().is_empty() {
                show_error(
                    state.h_listview,
                    "Keyboard combo cannot be empty. Enter at least one key (e.g., Ctrl+W).",
                );
                let _ = SetFocus(Some(state.h_edit_action));
                return false;
            }
        }
        2 => {
            let path = read_edit_text(state.h_edit_launch_path).unwrap_or_default();
            if path.trim().is_empty() {
                show_error(state.h_listview, "Launch path cannot be empty.");
                let _ = SetFocus(Some(state.h_edit_launch_path));
                return false;
            }
        }
        _ => {}
    }

    ensure_gestures_section(&mut state.config);

    if let Some(gestures) = state.config.get_mut("gestures") {
        if let Some(table) = gestures.as_table_mut() {
            let mut gesture_table = toml_edit::Table::new();
            gesture_table.insert("pattern", toml_edit::value(pattern.as_str()));

            let mut action = toml_edit::InlineTable::new();
            match action_type {
                0 => {
                    let cmd_idx = SendMessageW(
                        state.h_combo_window_cmd,
                        CB_GETCURSEL,
                        Some(WPARAM(0)),
                        Some(LPARAM(0)),
                    );
                    let cmd = get_window_cmd_name(if cmd_idx.0 >= 0 {
                        cmd_idx.0 as usize
                    } else {
                        0
                    });
                    action.insert("type", "window".into());
                    action.insert("command", cmd.into());
                }
                1 => {
                    action.insert("type", "key".into());
                    let combo_text = read_edit_text(state.h_edit_action).unwrap_or_default();
                    let mut arr = toml_edit::Array::new();
                    for token in combo_text.split('+').map(|s| s.trim()) {
                        if !token.is_empty() {
                            arr.push(toml_edit::Value::String(toml_edit::Formatted::new(
                                token.to_string(),
                            )));
                        }
                    }
                    action.insert("combo", toml_edit::Value::Array(arr));
                }
                2 => {
                    action.insert("type", "launch".into());
                    let path = read_edit_text(state.h_edit_launch_path).unwrap_or_default();
                    action.insert("path", (path.as_str()).into());

                    let args_text = read_edit_text(state.h_edit_launch_args).unwrap_or_default();
                    let mut arr = toml_edit::Array::new();
                    for token in args_text.split_whitespace() {
                        arr.push(toml_edit::Value::String(toml_edit::Formatted::new(
                            token.to_string(),
                        )));
                    }
                    action.insert("args", toml_edit::Value::Array(arr));
                }
                _ => {}
            }
            gesture_table.insert(
                "action",
                toml_edit::Item::Value(toml_edit::Value::InlineTable(action)),
            );

            table.insert(name.as_str(), toml_edit::Item::Table(gesture_table));
        }
    }

    state.gesture_names.push(name);
    populate_listview(state);

    // Select the newly added item (last)
    let count = SendMessageW(
        state.h_listview,
        LVM_GETITEMCOUNT,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    if count.0 > 0 {
        let mut item = LVITEMW::default();
        item.stateMask = LVIS_SELECTED;
        item.state = LVIS_SELECTED;
        SendMessageW(
            state.h_listview,
            LVM_SETITEMW,
            Some(WPARAM(count.0 as usize - 1)),
            Some(LPARAM(&item as *const _ as isize)),
        );
    }

    true
}

/// Ensure "gestures" section exists in DocumentMut.
fn ensure_gestures_section(doc: &mut DocumentMut) {
    if doc.get("gestures").is_none() {
        doc.insert("gestures", toml_edit::table());
    }
}

/// Validate that all whitespace-separated tokens are valid direction names.
fn validate_pattern_tokens(pattern: &str) -> bool {
    if pattern.trim().is_empty() {
        return false;
    }
    pattern.split_whitespace().all(|token| {
        let upper = token.to_uppercase();
        matches!(
            upper.as_str(),
            "N" | "U"
                | "NE"
                | "UR"
                | "E"
                | "R"
                | "SE"
                | "DR"
                | "S"
                | "D"
                | "SW"
                | "DL"
                | "W"
                | "L"
                | "NW"
                | "UL"
        )
    })
}

// ── Settings & blacklist population ───────────────────────────

/// Populate the blacklist mode ComboBox with "Blacklist" and "Whitelist" entries.
fn populate_blacklist_combo(combo: HWND) {
    let modes = ["Blacklist\0", "Whitelist\0"];
    for text in &modes {
        let wide: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(wide.as_ptr() as isize)),
            );
        }
    }
}

/// Read settings from DocumentMut and populate the settings form fields.
fn populate_settings_from_doc(state: &EditorState) {
    let settings = state.config.get("settings").and_then(|s| s.as_table());

    let threshold = settings
        .and_then(|s| s.get("activation_threshold_dip"))
        .and_then(|v| v.as_float())
        .unwrap_or(30.0);
    let sample = settings
        .and_then(|s| s.get("sample_distance_dip"))
        .and_then(|v| v.as_float())
        .unwrap_or(5.0);
    let epsilon = settings
        .and_then(|s| s.get("rdp_epsilon_dip"))
        .and_then(|v| v.as_float())
        .unwrap_or(2.0);
    let min_len = settings
        .and_then(|s| s.get("min_gesture_length"))
        .and_then(|v| v.as_integer())
        .unwrap_or(3);
    let debug = settings
        .and_then(|s| s.get("debug_logging"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let startup = settings
        .and_then(|s| s.get("start_with_windows"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    unsafe {
        set_edit_text(state.h_edit_threshold, &format!("{:.1}", threshold));
        set_edit_text(state.h_edit_sample, &format!("{:.1}", sample));
        set_edit_text(state.h_edit_epsilon, &format!("{:.1}", epsilon));
        set_edit_text(state.h_edit_min_len, &min_len.to_string());

        SendMessageW(
            state.h_check_debug,
            BM_SETCHECK,
            Some(WPARAM(if debug { 1usize } else { 0usize })),
            Some(LPARAM(0)),
        );
        SendMessageW(
            state.h_check_startup,
            BM_SETCHECK,
            Some(WPARAM(if startup { 1usize } else { 0usize })),
            Some(LPARAM(0)),
        );
    }
}

/// Read blacklist from DocumentMut and populate the blacklist form fields.
fn populate_blacklist_from_doc(state: &EditorState) {
    let blacklist = state.config.get("blacklist").and_then(|b| b.as_table());

    let mode = blacklist
        .and_then(|b| b.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("blacklist");
    let mode_idx = if mode == "whitelist" { 1usize } else { 0usize };

    let apps_text = blacklist
        .and_then(|b| b.get("apps"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join("\r\n")
        })
        .unwrap_or_default();

    unsafe {
        SendMessageW(
            state.h_blacklist_mode,
            CB_SETCURSEL,
            Some(WPARAM(mode_idx)),
            Some(LPARAM(0)),
        );
        set_edit_text(state.h_blacklist_apps, &apps_text);
    }
}

// ── Settings & blacklist change handlers ──────────────────────

/// Handle a settings EDIT control change: parse the text and update
/// DocumentMut with the parsed numeric value.
unsafe fn handle_setting_change(state: &mut EditorState, key: &str, edit: HWND) {
    let text = match read_edit_text(edit) {
        Ok(t) => t,
        Err(_) => return,
    };

    let trimmed = text.trim();
    if trimmed.is_empty() {
        state.dirty = true;
        return;
    }

    ensure_settings_section(&mut state.config);
    let Some(settings) = state.config.get_mut("settings") else {
        return;
    };
    let Some(table) = settings.as_table_mut() else {
        return;
    };

    match key {
        "min_gesture_length" => {
            if let Ok(v) = trimmed.parse::<i64>() {
                table.insert(key, toml_edit::value(v));
                state.dirty = true;
            }
        }
        _ => {
            if let Ok(v) = trimmed.parse::<f64>() {
                table.insert(key, toml_edit::value(v));
                state.dirty = true;
            }
        }
    }
}

/// Handle a checkbox toggle: read `BM_GETCHECK` and update DocumentMut.
unsafe fn handle_checkbox_change(state: &mut EditorState, key: &str, checkbox: HWND) {
    let checked = SendMessageW(checkbox, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0)));
    let is_checked = checked.0 as u32 == 1; // BST_CHECKED = 1

    ensure_settings_section(&mut state.config);
    if let Some(settings) = state.config.get_mut("settings") {
        if let Some(table) = settings.as_table_mut() {
            table.insert(key, toml_edit::value(is_checked));
        }
    }
    state.dirty = true;
}

/// Handle blacklist mode ComboBox selection change.
unsafe fn handle_blacklist_mode_change(state: &mut EditorState) {
    let sel = SendMessageW(
        state.h_blacklist_mode,
        CB_GETCURSEL,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    let mode = if sel.0 == 1 { "whitelist" } else { "blacklist" };

    ensure_blacklist_section(&mut state.config);
    if let Some(blacklist) = state.config.get_mut("blacklist") {
        if let Some(table) = blacklist.as_table_mut() {
            table.insert("mode", toml_edit::value(mode));
        }
    }
    state.dirty = true;
}

/// Handle blacklist apps multi-line EDIT change.
unsafe fn handle_blacklist_apps_change(state: &mut EditorState) {
    let text = match read_edit_text(state.h_blacklist_apps) {
        Ok(t) => t,
        Err(_) => return,
    };

    let mut arr = toml_edit::Array::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            arr.push(toml_edit::Value::String(toml_edit::Formatted::new(
                trimmed.to_string(),
            )));
        }
    }

    ensure_blacklist_section(&mut state.config);
    if let Some(blacklist) = state.config.get_mut("blacklist") {
        if let Some(table) = blacklist.as_table_mut() {
            table.insert("apps", toml_edit::Item::Value(toml_edit::Value::Array(arr)));
        }
    }
    state.dirty = true;
}

/// Ensure the `[settings]` table exists in the document.
fn ensure_settings_section(doc: &mut DocumentMut) {
    if doc.get("settings").is_none() {
        doc.insert("settings", toml_edit::table());
    }
}

/// Ensure the `[blacklist]` table exists in the document.
fn ensure_blacklist_section(doc: &mut DocumentMut) {
    if doc.get("blacklist").is_none() {
        doc.insert("blacklist", toml_edit::table());
    }
}

/// Read and validate all settings/blacklist values from the UI controls.
/// Used before saving to ensure all numeric fields contain valid values.
/// The individual change handlers already update DocumentMut incrementally;
/// this function validates the final state before commit.
unsafe fn read_settings_from_ui(state: &EditorState) -> Result<()> {
    let threshold_text = read_edit_text(state.h_edit_threshold)?;
    threshold_text
        .trim()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("Activation threshold must be a valid number"))?;

    let sample_text = read_edit_text(state.h_edit_sample)?;
    sample_text
        .trim()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("Sample distance must be a valid number"))?;

    let epsilon_text = read_edit_text(state.h_edit_epsilon)?;
    epsilon_text
        .trim()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("RDP epsilon must be a valid number"))?;

    let min_len_text = read_edit_text(state.h_edit_min_len)?;
    min_len_text
        .trim()
        .parse::<i64>()
        .map_err(|_| anyhow::anyhow!("Min gesture length must be a valid integer"))?;

    Ok(())
}

// ── Flush form to DocumentMut (auto-sync on gesture switch) ─────

/// Auto-flush the current form field values into DocumentMut for the
/// currently selected gesture. Called when switching to a different
/// gesture in the ListView so in-memory state stays consistent.
/// Does NOT set dirty — the incremental handlers already set it on edit.
unsafe fn flush_current_form_to_doc(state: &mut EditorState) {
    if state.is_adding {
        return;
    }
    let Some(index) = state.selected_index else {
        return;
    };
    if index >= state.gesture_names.len() {
        return;
    }

    // The individual handlers already update DocumentMut on every
    // EN_CHANGE/CBN_SELCHANGE, so in theory DocumentMut is in sync.
    // We call them here as a safety net for edge cases where a
    // notification might have been missed.
    let saved_dirty = state.dirty;
    handle_name_change(state);
    handle_pattern_change(state);
    let action_type = get_action_type_selection(state);
    match action_type {
        0 => handle_window_cmd_change(state),
        1 => handle_key_combo_change(state),
        2 => handle_launch_change(state),
        _ => {}
    }
    state.dirty = saved_dirty;
}

// ── Validation ────────────────────────────────────────────────

/// Validate all fields before saving. Returns the first error found.
fn validate_form(state: &EditorState) -> Result<()> {
    // Gesture names: non-empty and unique
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in &state.gesture_names {
        if name.is_empty() {
            return Err(anyhow::anyhow!("Gesture name cannot be empty."));
        }
        if !seen.insert(name.as_str()) {
            return Err(anyhow::anyhow!("Duplicate gesture name: '{}'.", name));
        }
    }

    // Validate patterns and actions for each gesture
    if let Some(gestures) = state.config.get("gestures").and_then(|g| g.as_table()) {
        for (name, value) in gestures.iter() {
            let gesture = match value.as_table() {
                Some(t) => t,
                None => continue,
            };

            let pattern = gesture
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !validate_pattern_tokens(pattern) {
                return Err(anyhow::anyhow!(
                    "Invalid pattern '{}' for gesture '{}'.\n\nValid direction tokens: N/S/E/W/NE/SE/SW/NW or U/D/L/R/UR/DR/DL/UL.",
                    pattern,
                    name
                ));
            }

            if let Some(action) = gesture.get("action").and_then(|v| v.as_inline_table()) {
                let action_type = action.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match action_type {
                    "window" => {
                        let cmd = action.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        if cmd.is_empty() || !is_valid_window_command(cmd) {
                            return Err(anyhow::anyhow!(
                                "Invalid window command '{}' for gesture '{}'.",
                                cmd,
                                name
                            ));
                        }
                    }
                    "key" => {
                        let has_combo = action
                            .get("combo")
                            .and_then(|v| v.as_array())
                            .map_or(false, |arr| !arr.is_empty());
                        if !has_combo {
                            return Err(anyhow::anyhow!(
                                "Keyboard combo is empty for gesture '{}'.",
                                name
                            ));
                        }
                    }
                    "launch" => {
                        let path = action.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        if path.is_empty() {
                            return Err(anyhow::anyhow!(
                                "Launch path is empty for gesture '{}'.",
                                name
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Validate settings thresholds are positive
    if let Some(settings) = state.config.get("settings").and_then(|s| s.as_table()) {
        if let Some(v) = settings
            .get("activation_threshold_dip")
            .and_then(|v| v.as_float())
        {
            if v <= 0.0 {
                return Err(anyhow::anyhow!(
                    "Activation threshold must be a positive number."
                ));
            }
        }
        if let Some(v) = settings
            .get("sample_distance_dip")
            .and_then(|v| v.as_float())
        {
            if v <= 0.0 {
                return Err(anyhow::anyhow!(
                    "Sample distance must be a positive number."
                ));
            }
        }
        if let Some(v) = settings.get("rdp_epsilon_dip").and_then(|v| v.as_float()) {
            if v <= 0.0 {
                return Err(anyhow::anyhow!("RDP epsilon must be a positive number."));
            }
        }
        if let Some(v) = settings
            .get("min_gesture_length")
            .and_then(|v| v.as_integer())
        {
            if v <= 0 {
                return Err(anyhow::anyhow!(
                    "Min gesture length must be a positive integer."
                ));
            }
        }
    }

    Ok(())
}

/// Check whether a window command string is a valid variant.
fn is_valid_window_command(cmd: &str) -> bool {
    let valid: [&str; 14] = [
        "Maximize",
        "Minimize",
        "Restore",
        "Close",
        "SnapLeft",
        "SnapRight",
        "SnapTop",
        "SnapBottom",
        "SnapTopLeft",
        "SnapTopRight",
        "SnapBottomLeft",
        "SnapBottomRight",
        "Center",
        "ToggleAlwaysOnTop",
    ];
    valid.contains(&cmd)
}

// ── Save ──────────────────────────────────────────────────────

/// Save the config. Validates all fields, serializes to TOML,
/// writes atomically, and posts a reload message.
/// Returns true on success (dirty flag reset), false if validation
/// or I/O fails (error shown to user).
unsafe fn handle_save(hwnd: HWND, state: &mut EditorState) -> bool {
    // 1. Validate settings UI fields (ensure parseable)
    if let Err(e) = read_settings_from_ui(state) {
        show_error(hwnd, &e.to_string());
        return false;
    }

    // 2. Validate all gestures, patterns, actions, and settings
    if let Err(e) = validate_form(state) {
        show_error(hwnd, &e.to_string());
        return false;
    }

    // 3. Serialize DocumentMut to TOML string
    let toml_string = state.config.to_string();

    // 4. Parse + compile safety net (catches inconsistencies)
    if let Err(e) =
        crate::config::ConfigFile::parse(&toml_string).and_then(|cfg| cfg.compile(1, 96))
    {
        show_error(
            hwnd,
            &format!(
                "Config validation failed — this is an internal error.\n\n{}",
                e
            ),
        );
        return false;
    }

    // 5. Ensure parent directory exists
    if let Some(parent) = state.path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            show_error(hwnd, &format!("Failed to create directory: {}", e));
            return false;
        }
    }

    // 6. Write to temp file, then rename (atomic on NTFS)
    let tmp_path = state.path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp_path, &toml_string) {
        show_error(hwnd, &format!("Failed to write config: {}", e));
        return false;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &state.path) {
        let _ = std::fs::remove_file(&tmp_path);
        show_error(hwnd, &format!("Failed to save config: {}", e));
        return false;
    }

    // 7. Post reload to daemon
    let _ = PostMessageW(
        Some(state.owner),
        WM_APP_RELOAD_CONFIG,
        WPARAM(0),
        LPARAM(0),
    );

    // 8. Reset dirty flag
    state.dirty = false;

    info!("Config saved: {}", state.path.display());
    true
}
