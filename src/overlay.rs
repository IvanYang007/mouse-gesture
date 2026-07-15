//! Transparent overlay window for direction indicators during
//! gesture drawing. Uses a layered window with GDI text rendering
//! for zero-flicker, zero-alloc updates.

use crate::config::Direction;
use anyhow::Result;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
    PatBlt, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, BITMAPINFO,
    BITMAPINFOHEADER, BLACKNESS, BLENDFUNCTION, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DEFAULT_QUALITY, DIB_RGB_COLORS, DT_CENTER, DT_NOCLIP, DT_SINGLELINE, DT_VCENTER,
    HBITMAP, HDC, HFONT,
    OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, RegisterClassExW, ShowWindow,
    UpdateLayeredWindow, CS_HREDRAW, CS_VREDRAW, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const OVERLAY_CLASS: &str = "MouseGestureOverlay\0";
const OVERLAY_W: i32 = 200;
const OVERLAY_H: i32 = 100;
const FONT_SIZE: i32 = 48;
const OVERLAY_ALPHA: u8 = 210;

pub struct OverlayWindow {
    hwnd: HWND,
    hdc: HDC,
    bitmap: HBITMAP,
    font: HFONT,
}

impl OverlayWindow {
    pub fn new() -> Result<Self> {
        let name: Vec<u16> = OVERLAY_CLASS.encode_utf16().collect();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_proc),
            hInstance: windows::Win32::Foundation::HINSTANCE(std::ptr::null_mut()),
            lpszClassName: windows::core::PCWSTR::from_raw(name.as_ptr()),
            ..Default::default()
        };
        unsafe { RegisterClassExW(&wc) };

        let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let w = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                windows::core::PCWSTR::from_raw(name.as_ptr()),
                windows::core::w!(""),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                None,
                None,
            )?
        };

        let screen_dc = unsafe { GetDC(None) };
        let hdc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
        unsafe { ReleaseDC(None, screen_dc) };

        let bitmap = Self::create_bitmap(hdc)?;
        unsafe { SelectObject(hdc, bitmap.into()) };

        let font = Self::create_font();
        unsafe { SelectObject(hdc, font.into()) };

        unsafe {
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
        }

        unsafe { let _ = ShowWindow(hwnd, SW_HIDE); };

        Ok(OverlayWindow {
            hwnd,
            hdc,
            bitmap,
            font,
        })
    }

    fn create_bitmap(hdc: HDC) -> Result<HBITMAP> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: OVERLAY_W,
                biHeight: -OVERLAY_H,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [unsafe { std::mem::zeroed() }; 1],
        };

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        unsafe { CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) }
            .map_err(|e| anyhow::anyhow!("CreateDIBSection: {:?}", e))
    }

    fn create_font() -> HFONT {
        let face: Vec<u16> = "Segoe UI\0".encode_utf16().collect();
        unsafe {
            CreateFontW(
                FONT_SIZE,
                0,
                0,
                0,
                700,
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
    }

    pub fn show_direction(&self, direction: Direction, cursor_x: i32, cursor_y: i32) {
        let label: &str = match direction {
            Direction::N => "U",
            Direction::NE => "UR",
            Direction::E => "R",
            Direction::SE => "DR",
            Direction::S => "D",
            Direction::SW => "DL",
            Direction::W => "L",
            Direction::NW => "UL",
        };

        let mut label_wide: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();

        unsafe {
            let _ = PatBlt(self.hdc, 0, 0, OVERLAY_W, OVERLAY_H, BLACKNESS);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: OVERLAY_W,
                bottom: OVERLAY_H,
            };
            DrawTextW(
                self.hdc,
                &mut label_wide,
                &mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP,
            );

            let screen_dc = GetDC(None);
            let pos = POINT {
                x: cursor_x - OVERLAY_W / 2,
                y: cursor_y - OVERLAY_H - 20,
            };
            let size = windows::Win32::Foundation::SIZE {
                cx: OVERLAY_W,
                cy: OVERLAY_H,
            };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_ALPHA as u8,
                BlendFlags: 0,
                SourceConstantAlpha: OVERLAY_ALPHA,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };

            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(screen_dc),
                Some(&pos),
                Some(&size),
                Some(self.hdc),
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            let _ = ReleaseDC(None, screen_dc);
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }

    pub fn hide(&self) {
        unsafe { let _ = ShowWindow(self.hwnd, SW_HIDE); };
    }

    pub fn destroy(&self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.hdc);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
