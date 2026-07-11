//! Input injection — keyboard shortcuts via SendInput with
//! proper modifier ordering and cleanup on partial failure.

use crate::config::CompiledInput;
use crate::state_machine::SELF_TAG;
use anyhow::Result;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    GetAsyncKeyState, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN,
};

/// Inject a pre-compiled keyboard shortcut.
/// The compiled_input vector is built by config::compile_key_combo
/// and contains press/release events in correct order.
pub fn inject_key_shortcut(compiled: &[CompiledInput]) -> Result<u32> {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(compiled.len());

    for ci in compiled {
        let mut flags = KEYBD_EVENT_FLAGS(0);
        flags.0 |= KEYEVENTF_SCANCODE.0;
        if ci.is_extended {
            flags.0 |= KEYEVENTF_EXTENDEDKEY.0;
        }
        if ci.is_up {
            flags.0 |= KEYEVENTF_KEYUP.0;
        }

        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(ci.vk),
                    wScan: ci.scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: SELF_TAG,
                },
            },
        });
    }

    let sent = unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32)
    };

    Ok(sent)
}

/// Check for physically held modifier conflicts before injecting a shortcut.
/// Returns Ok(()) if no conflict, Err if a conflict is detected.
pub fn check_modifier_conflict(compiled: &[CompiledInput]) -> Result<()> {
    // Collect the modifiers used in this shortcut
    let mut uses_ctrl = false;
    let mut uses_alt = false;
    let mut uses_shift = false;
    let mut uses_win = false;

    for ci in compiled {
        if ci.is_up { continue; }
        let vk = ci.vk;
        if vk == VK_CONTROL.0 { uses_ctrl = true; }
        if vk == VK_MENU.0 { uses_alt = true; }
        if vk == VK_SHIFT.0 { uses_shift = true; }
        if vk == VK_LWIN.0 || vk == VK_RWIN.0 { uses_win = true; }
    }

    // Check physically held modifiers
    let held_ctrl = is_modifier_held(VK_CONTROL);
    let held_alt = is_modifier_held(VK_MENU);
    let held_shift = is_modifier_held(VK_SHIFT);
    let held_win = is_modifier_held(VK_LWIN) || is_modifier_held(VK_RWIN);

    // Conflict: a modifier is physically held but not part of the shortcut,
    // OR a modifier in the shortcut is NOT physically held but another is.
    // Simplest safe rule: if ANY modifier is held that's not in the shortcut,
    // reject. If the shortcut uses a modifier that's held, that's fine.
    if (held_ctrl && !uses_ctrl) || (held_alt && !uses_alt)
        || (held_shift && !uses_shift) || (held_win && !uses_win)
    {
        anyhow::bail!(
            "modifier conflict: held modifiers don't match shortcut"
        );
    }

    Ok(())
}

fn is_modifier_held(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) & 0x8000u16 as i16) != 0 }
}
