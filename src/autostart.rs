//! Windows auto-start via registry Run key.
//!
//! Uses the `reg` command-line tool for simplicity — avoids raw Win32
//! registry API type-compatibility issues. Toggles
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\MouseGestureDaemon`.
//!
//! Synced on daemon startup and config reload.

use std::process::Command;

/// Sync the registry Run key with the desired auto-start state.
pub fn sync(enabled: bool) {
    if enabled {
        if let Err(e) = enable() {
            log::warn!("Failed to enable auto-start: {}", e);
        }
    } else {
        if let Err(e) = disable() {
            log::warn!("Failed to disable auto-start: {}", e);
        }
    }
}

fn enable() -> Result<(), String> {
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("current_exe: {}", e))?;
    let exe_str = exe_path.to_string_lossy().to_string();

    let output = Command::new("reg")
        .args([
            "add",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v", "MouseGestureDaemon",
            "/t", "REG_SZ",
            "/d", &exe_str,
            "/f",  // force overwrite
        ])
        .output()
        .map_err(|e| format!("reg add: {}", e))?;

    if output.status.success() {
        log::info!("Auto-start enabled: {}", exe_str);
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("reg add failed: {}", stderr))
    }
}

fn disable() -> Result<(), String> {
    let output = Command::new("reg")
        .args([
            "delete",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v", "MouseGestureDaemon",
            "/f",  // force without prompt
        ])
        .output()
        .map_err(|e| format!("reg delete: {}", e))?;

    // Exit code 0 = deleted, 1 = key not found (already absent — fine)
    if output.status.success() || output.status.code() == Some(1) {
        log::info!("Auto-start disabled");
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("reg delete failed: {}", stderr))
    }
}
