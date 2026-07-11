//! Safe Send/Sync wrappers for Windows handle types that are !Send/!Sync
//! in the windows crate but are thread-safe when used correctly.
//!
//! These wrappers are narrow and auditable — each documents why the
//! unsafe Send/Sync impl is sound.

use windows::Win32::UI::WindowsAndMessaging::HHOOK;

/// Wrapper around HHOOK that makes it Send.
///
/// # Safety
/// HHOOK is `!Send` in the windows crate because it wraps a raw pointer.
/// However, low-level hooks (WH_MOUSE_LL) registered with `dwThreadId=0`
/// are system-wide and serviced by a dedicated message-pump thread.
/// The hook handle is never accessed from multiple threads simultaneously.
#[repr(transparent)]
pub struct SendHHOOK(pub HHOOK);

unsafe impl Send for SendHHOOK {}
unsafe impl Sync for SendHHOOK {}
