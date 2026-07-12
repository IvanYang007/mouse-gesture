//! Application launching — CreateProcessW for executables,
//! ShellExecuteExW for URLs/folders/documents, and best-effort
//! focus-existing logic.

use anyhow::{Context, Result};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::{
    CreateProcessW, CREATE_NO_WINDOW, CREATE_NEW_PROCESS_GROUP,
    STARTUPINFOW,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    ShowWindowAsync, SetForegroundWindow, FlashWindowEx, FLASHWINFO,
    FLASHW_TRAY, FLASHW_TIMERNOFG, SW_RESTORE,
};
use windows::core::PWSTR;

/// Launch an executable with optional arguments.
pub fn launch_executable(path: &str, args: &[String]) -> Result<()> {
    let cmdline = if args.is_empty() {
        path.to_string()
    } else {
        let quoted_args: Vec<String> = args.iter().map(|a| {
            if a.contains(' ') || a.contains('"') {
                format!("\"{}\"", a.replace('"', "\\\""))
            } else {
                a.clone()
            }
        }).collect();
        format!("\"{}\" {}", path, quoted_args.join(" "))
    };

    let mut cmdline_wide: Vec<u16> = cmdline.encode_utf16().chain(std::iter::once(0)).collect();
    let mut si = STARTUPINFOW::default();
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut pi = Default::default();

    unsafe {
        CreateProcessW(
            None,
            Some(PWSTR::from_raw(cmdline_wide.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP,
            None,
            None,
            &si,
            &mut pi,
        ).context("CreateProcessW failed")?;
    }

    Ok(())
}

use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;

/// Open a URL, folder, or document via ShellExecuteW.
pub fn shell_open(path: &str) -> Result<()> {
    let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let operation: Vec<u16> = "open\0".encode_utf16().collect();

    unsafe {
        let result = ShellExecuteW(
            None,
            windows::core::PCWSTR::from_raw(operation.as_ptr()),
            windows::core::PCWSTR::from_raw(path_wide.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            SHOW_WINDOW_CMD(1),
        );
        if result.0 as isize > 32 {
            Ok(())
        } else {
            Err(anyhow::anyhow!("ShellExecuteW failed with code {}", result.0 as isize))
        }
    }
}

/// Focus an existing window (best effort).
pub fn focus_existing(hwnd: HWND) {
    unsafe {
        ShowWindowAsync(hwnd, SW_RESTORE);

        let _ = SetForegroundWindow(hwnd);

        // Flash taskbar as fallback signal
        let mut fwi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
            uCount: 3,
            dwTimeout: 0,
        };
        FlashWindowEx(&mut fwi);
    }
}
