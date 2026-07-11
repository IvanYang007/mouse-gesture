//! Application launching — CreateProcessW for executables,
//! ShellExecuteExW for URLs/folders/documents, and best-effort
//! focus-existing logic.

use anyhow::Result;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{
    CreateProcessW, PROCESS_CREATION_FLAGS, CREATE_NO_WINDOW, CREATE_NEW_PROCESS_GROUP,
    STARTUPINFOW,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SHELLEXECUTEINFOW, SEE_MASK_FLAG_NO_UI};
use windows::Win32::UI::WindowsAndMessaging::{
    ShowWindowAsync, SetForegroundWindow, FlashWindowEx, FLASHWINFO,
    FLASHW_TRAY, FLASHW_TIMERNOFG, SW_RESTORE,
};
use windows::core::PCWSTR;

/// Launch an executable with optional arguments.
pub fn launch_executable(path: &str, args: &[String]) -> Result<()> {
    let cmdline = if args.is_empty() {
        path.to_string()
    } else {
        format!("\"{}\" {}", path, args.join(" "))
    };

    let mut cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi = Default::default();

    unsafe {
        CreateProcessW(
            PCWSTR::null(),               // lpApplicationName
            PCWSTR::from_raw(cmdline_wide.as_ptr()), // lpCommandLine
            None,                         // lpProcessAttributes
            None,                         // lpThreadAttributes
            false,                        // bInheritHandles
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            None,                         // lpEnvironment
            PCWSTR::null(),               // lpCurrentDirectory
            &si,
            &mut pi,
        )?;
    }

    Ok(())
}

/// Open a URL, folder, or document via ShellExecuteExW.
pub fn shell_open(path: &str) -> Result<()> {
    let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let operation: Vec<u16> = "open\0".encode_utf16().collect();

    let mut sei = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_FLAG_NO_UI,
        hwnd: HWND::default(),
        lpVerb: PCWSTR::from_raw(operation.as_ptr()),
        lpFile: PCWSTR::from_raw(path_wide.as_ptr()),
        lpParameters: PCWSTR::null(),
        lpDirectory: PCWSTR::null(),
        nShow: 1, // SW_SHOWNORMAL
        ..Default::default()
    };

    unsafe {
        ShellExecuteExW(&mut sei)?;
    }

    Ok(())
}

/// Focus an existing window (best effort). Windows foreground lock
/// may prevent SetForegroundWindow from succeeding; in that case,
/// flash the taskbar button as a visual signal.
pub fn focus_existing(hwnd: HWND) {
    unsafe {
        // Restore if minimized
        ShowWindowAsync(hwnd, SW_RESTORE);

        // Try to bring to foreground
        let result = SetForegroundWindow(hwnd);

        if result.is_err() {
            // Foreground lock active — flash taskbar button
            let mut fwi = FLASHWINFO {
                cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
                hwnd,
                dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
                uCount: 3,
                dwTimeout: 0,
            };
            unsafe { FlashWindowEx(&mut fwi); }
        }
    }
}
