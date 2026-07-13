//! Window operations — monitor enumeration, DPI-aware coordinate
//! conversions, and window tiling (half/quarter snap).

use anyhow::Result;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;

/// A monitor descriptor used for tiling calculations.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub handle: isize,
    pub name: String,
    pub rect: RECT,
    pub work_rect: RECT,
    pub dpi: u32,
    pub is_primary: bool,
}

/// Enumerate all monitors in deterministic left-to-right, top-to-bottom order.
pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    let mut monitors = Vec::new();

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitors_callback),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }

    monitors.sort_by(|a: &MonitorInfo, b: &MonitorInfo| {
        a.rect
            .left
            .cmp(&b.rect.left)
            .then(a.rect.top.cmp(&b.rect.top))
    });

    Ok(monitors)
}

use windows::Win32::Foundation::LPARAM;

unsafe extern "system" fn enum_monitors_callback(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> windows::core::BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<MonitorInfo>);

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };

    if GetMonitorInfoW(hmonitor, &mut info).as_bool() {
        monitors.push(MonitorInfo {
            handle: hmonitor.0 as isize,
            name: String::new(),
            rect: info.rcMonitor,
            work_rect: info.rcWork,
            dpi: 96,
            is_primary: (info.dwFlags & 1) != 0,
        });
    }

    windows::core::BOOL::from(true)
}

/// Get the DPI of the monitor containing a window.
pub fn window_dpi(hwnd: HWND) -> u32 {
    unsafe { GetDpiForWindow(hwnd) }
}

/// Get the monitor handle for a point.
pub fn monitor_from_point(x: i32, y: i32) -> isize {
    unsafe {
        let hmon = MonitorFromPoint(
            windows::Win32::Foundation::POINT { x, y },
            MONITOR_DEFAULTTONEAREST,
        );
        hmon.0 as isize
    }
}

/// Snap position for window tiling.
#[derive(Debug, Clone, Copy)]
pub enum SnapPosition {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Center,
}

/// Compute the target window rect for a snap operation.
pub fn snap_rect(monitor: &MonitorInfo, position: SnapPosition) -> RECT {
    let work = monitor.work_rect;
    let w = work.right - work.left;
    let h = work.bottom - work.top;

    match position {
        SnapPosition::Left => RECT {
            left: work.left,
            top: work.top,
            right: work.left + w / 2,
            bottom: work.bottom,
        },
        SnapPosition::Right => RECT {
            left: work.left + w / 2,
            top: work.top,
            right: work.right,
            bottom: work.bottom,
        },
        SnapPosition::Top => RECT {
            left: work.left,
            top: work.top,
            right: work.right,
            bottom: work.top + h / 2,
        },
        SnapPosition::Bottom => RECT {
            left: work.left,
            top: work.top + h / 2,
            right: work.right,
            bottom: work.bottom,
        },
        SnapPosition::TopLeft => RECT {
            left: work.left,
            top: work.top,
            right: work.left + w / 2,
            bottom: work.top + h / 2,
        },
        SnapPosition::TopRight => RECT {
            left: work.left + w / 2,
            top: work.top,
            right: work.right,
            bottom: work.top + h / 2,
        },
        SnapPosition::BottomLeft => RECT {
            left: work.left,
            top: work.top + h / 2,
            right: work.left + w / 2,
            bottom: work.bottom,
        },
        SnapPosition::BottomRight => RECT {
            left: work.left + w / 2,
            top: work.top + h / 2,
            right: work.right,
            bottom: work.bottom,
        },
        SnapPosition::Center => {
            let cw = (w as f64 * 0.8) as i32;
            let ch = (h as f64 * 0.8) as i32;
            RECT {
                left: work.left + (w - cw) / 2,
                top: work.top + (h - ch) / 2,
                right: work.left + (w - cw) / 2 + cw,
                bottom: work.top + (h - ch) / 2 + ch,
            }
        }
    }
}
