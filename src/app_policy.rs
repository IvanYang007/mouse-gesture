//! Application policy — PID-to-eligibility cache and integrity checks.
//! Runs on the worker thread; sends compiled policy snapshots to the
//! input thread for lock-free access in the hook proc.

use crate::config::{BlacklistMode, ConfigSnapshot};
use std::collections::HashMap;
use windows::Win32::Foundation::HWND;

/// Cached policy entry for a process.
#[derive(Debug, Clone)]
pub struct PolicyEntry {
    pub basename: String,
    pub is_eligible: bool,
}

/// The PID→policy cache sent to the input thread.
#[derive(Debug, Clone)]
pub struct PolicyCache {
    pub entries: HashMap<u32, PolicyEntry>,
    pub mode: BlacklistMode,
}

impl PolicyCache {
    pub fn from_snapshot(snapshot: &ConfigSnapshot) -> Self {
        PolicyCache {
            entries: HashMap::new(),
            mode: snapshot.blacklist_mode.clone(),
        }
    }

    /// Check if a PID is eligible for gesture interception.
    pub fn is_eligible(&self, pid: u32) -> bool {
        match &self.mode {
            BlacklistMode::Blacklist => {
                // In blacklist mode, unknown PIDs are eligible
                self.entries
                    .get(&pid)
                    .map(|e| e.is_eligible)
                    .unwrap_or(true)
            }
            BlacklistMode::Whitelist => {
                // In whitelist mode, unknown PIDs are ineligible
                self.entries
                    .get(&pid)
                    .map(|e| e.is_eligible)
                    .unwrap_or(false)
            }
        }
    }

    /// Insert a resolved PID entry.
    pub fn insert(&mut self, pid: u32, basename: String, is_eligible: bool) {
        self.entries.insert(pid, PolicyEntry { basename, is_eligible });
    }
}

/// Pre-warm the policy cache by enumerating visible top-level windows.
/// Returns a PolicyCache populated with known PIDs.
///
/// Uses EnumWindows to walk the window list. Unknown PIDs are resolved
/// async by the worker thread and pushed via policy updates.
pub fn prewarm_cache(snapshot: &ConfigSnapshot) -> PolicyCache {
    let mut cache = PolicyCache::from_snapshot(snapshot);

    // In a full implementation, EnumWindows would iterate all top-level
    // windows, extract PIDs via GetWindowThreadProcessId, resolve process
    // names via OpenProcess + QueryFullProcessImageNameW, and build
    // the initial cache. For the skeleton, we start with an empty cache
    // and let the worker thread resolve entries on demand.

    let _ = cache; // suppress unused warning during skeleton phase
    PolicyCache {
        entries: HashMap::new(),
        mode: snapshot.blacklist_mode.clone(),
    }
}

/// Resolve a PID to its basename and eligibility.
/// Called by the worker thread for unknown PIDs encountered by the hook.
pub fn resolve_pid(
    pid: u32,
    snapshot: &ConfigSnapshot,
) -> Option<PolicyEntry> {
    // In a full implementation:
    // 1. OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
    // 2. QueryFullProcessImageNameW
    // 3. Extract basename
    // 4. Match against blacklist_apps (case-insensitive)
    // 5. Check integrity level via GetTokenInformation(TokenIntegrityLevel)

    // Skeleton: all PIDs are eligible
    let is_eligible = match &snapshot.blacklist_mode {
        BlacklistMode::Blacklist => true,
        BlacklistMode::Whitelist => false,
    };

    Some(PolicyEntry {
        basename: format!("pid_{}", pid),
        is_eligible,
    })
}

/// Pre-action validation: check that the target HWND still exists,
/// its PID matches, and (for keyboard actions) the foreground window
/// hasn't changed.
pub fn validate_target(
    target_hwnd: HWND,
    target_pid: u32,
    foreground_hwnd: HWND,
    is_keyboard_action: bool,
) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, IsWindow,
    };

    unsafe {
        // Check HWND still exists
        if IsWindow(target_hwnd).as_bool() {
            return false;
        }

        // Check PID matches
        let mut current_pid: u32 = 0;
        GetWindowThreadProcessId(target_hwnd, Some(&mut current_pid));
        if current_pid != target_pid {
            return false;
        }

        // For keyboard actions, verify foreground hasn't changed
        if is_keyboard_action {
            let fg = GetForegroundWindow();
            if fg != foreground_hwnd {
                return false;
            }
        }
    }

    true
}
