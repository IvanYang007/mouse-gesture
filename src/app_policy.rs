//! Application policy — PID-to-eligibility cache and integrity checks.
//!
//! Uses a lock-free `PolicySnapshot` published via `AtomicPtr` so the
//! hook callback can perform a read-only PID lookup with zero allocation.
//! A background policy worker builds new snapshots when config changes or
//! unknown PIDs are resolved, then atomically publishes them.

use crate::config::{BlacklistMode, ConfigSnapshot};
use std::collections::HashMap;
use std::sync::atomic::{AtomicPtr, Ordering};
use windows::Win32::Foundation::HWND;

/// Whether a PID is eligible for gesture interception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Allowed,
    Denied,
}

/// Cached policy entry for a process.
#[derive(Debug, Clone)]
pub struct PolicyEntry {
    pub basename: String,
    pub is_eligible: bool,
}

/// Immutable, atomically-published policy snapshot.
/// Safe for lock-free reads from the hook callback.
pub struct PolicySnapshot {
    pub mode: BlacklistMode,
    pub generation: u64,
    pub known_pids: HashMap<u32, Eligibility>,
}

/// Global policy snapshot — atomically swapped by the policy worker,
/// read by the hook callback.
static POLICY_SNAPSHOT: AtomicPtr<PolicySnapshot> = AtomicPtr::new(std::ptr::null_mut());

impl PolicySnapshot {
    /// Build the initial snapshot from a config.
    pub fn from_snapshot(snapshot: &ConfigSnapshot) -> Self {
        PolicySnapshot {
            mode: snapshot.blacklist_mode.clone(),
            generation: snapshot.generation,
            known_pids: HashMap::new(),
        }
    }

    /// Look up PID eligibility. Called from the hook callback — no allocation, no lock.
    ///
    /// Returns `None` if the PID is unknown (the caller should apply default policy
    /// and optionally enqueue async resolution).
    pub fn lookup(&self, pid: u32) -> Option<Eligibility> {
        self.known_pids.get(&pid).copied()
    }

    /// Default eligibility for unknown PIDs based on mode.
    pub fn default_for_unknown(&self) -> Eligibility {
        match self.mode {
            BlacklistMode::Blacklist => Eligibility::Allowed,
            BlacklistMode::Whitelist => Eligibility::Denied,
        }
    }

    /// Build a new snapshot from a config, carrying over known PIDs from a previous snapshot.
    pub fn from_config(snapshot: &ConfigSnapshot, carry_over: &HashMap<u32, Eligibility>) -> Self {
        PolicySnapshot {
            mode: snapshot.blacklist_mode.clone(),
            generation: snapshot.generation,
            known_pids: carry_over.clone(),
        }
    }
}

/// Publish a new policy snapshot atomically.
/// The old snapshot is intentionally leaked — snapshots are infrequent
/// and freeing would require a grace period through concurrent readers.
pub fn publish_snapshot(snapshot: PolicySnapshot) {
    let ptr = Box::into_raw(Box::new(snapshot));
    let old = POLICY_SNAPSHOT.swap(ptr, Ordering::Release);
    // Safety: old pointer is leaked. This is intentional — the hook callback
    // may still hold a reference to the old snapshot, and we cannot safely
    // free it without epoch-based reclamation.
    let _ = old; // explicitly leak
}

/// Load the current policy snapshot for read-only access.
/// Returns a reference valid until the next `publish_snapshot` call
/// (the old snapshot is leaked, so the reference never dangles).
///
/// Safety: the returned reference is valid as long as snapshots are leaked
/// on publish. Callers must not hold the reference across yield points where
/// a new snapshot could be published — but since the old memory is leaked,
/// even that is technically safe (just potentially stale).
pub fn get_snapshot() -> Option<&'static PolicySnapshot> {
    let ptr = POLICY_SNAPSHOT.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        unsafe { Some(&*ptr) }
    }
}

/// Resolve a PID to its basename and eligibility.
/// Called by the policy worker thread for unknown PIDs encountered by the hook.
pub fn resolve_pid(pid: u32, snapshot: &ConfigSnapshot) -> Option<PolicyEntry> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::Foundation::CloseHandle;

    // 1. Open the process
    let handle = unsafe {
        OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
    };
    let handle = match handle {
        Ok(h) => h,
        Err(_) => {
            // Process may have exited or we lack permission — treat as unknown,
            // let the default policy for the mode apply.
            return None;
        }
    };

    // 2. Query the full process image name
    let basename = unsafe {
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0), // PROCESS_NAME_WIN32
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        if result.is_err() {
            CloseHandle(handle);
            return None;
        }
        CloseHandle(handle);

        let full = String::from_utf16_lossy(&buf[..len as usize]);
        // 3. Extract basename
        std::path::Path::new(&full)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&full)
            .to_lowercase()
    };

    // 4. Match against blacklist_apps (case-insensitive)
    let is_eligible = match &snapshot.blacklist_mode {
        BlacklistMode::Blacklist => {
            !snapshot
                .blacklist_apps
                .iter()
                .any(|app| app.to_lowercase() == basename)
        }
        BlacklistMode::Whitelist => {
            snapshot
                .blacklist_apps
                .iter()
                .any(|app| app.to_lowercase() == basename)
        }
    };

    log::info!(
        "Policy resolved: pid={} basename={} eligible={}",
        pid,
        basename,
        is_eligible
    );

    Some(PolicyEntry {
        basename,
        is_eligible,
    })
}

/// The PID→policy cache (legacy type, retained for compatibility with
/// existing tests and the PolicyCache API).
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

    pub fn is_eligible(&self, pid: u32) -> bool {
        match &self.mode {
            BlacklistMode::Blacklist => self
                .entries
                .get(&pid)
                .map(|e| e.is_eligible)
                .unwrap_or(true),
            BlacklistMode::Whitelist => self
                .entries
                .get(&pid)
                .map(|e| e.is_eligible)
                .unwrap_or(false),
        }
    }

    pub fn insert(&mut self, pid: u32, basename: String, is_eligible: bool) {
        self.entries.insert(
            pid,
            PolicyEntry {
                basename,
                is_eligible,
            },
        );
    }
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
        if !IsWindow(Some(target_hwnd)).as_bool() {
            return false;
        }

        let mut current_pid: u32 = 0;
        GetWindowThreadProcessId(target_hwnd, Some(&mut current_pid));
        if current_pid != target_pid {
            return false;
        }

        if is_keyboard_action {
            let fg = GetForegroundWindow();
            if fg != foreground_hwnd {
                return false;
            }
        }
    }

    true
}

// ── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BlacklistMode;

    #[test]
    fn snapshot_lookup_known_pid() {
        let mut snapshot = PolicySnapshot {
            mode: BlacklistMode::Blacklist,
            generation: 1,
            known_pids: std::collections::HashMap::new(),
        };
        snapshot.known_pids.insert(1234, Eligibility::Denied);
        assert_eq!(snapshot.lookup(1234), Some(Eligibility::Denied));
        assert_eq!(snapshot.lookup(5678), None);
    }

    #[test]
    fn snapshot_default_blacklist_allows_unknown() {
        let snapshot = PolicySnapshot {
            mode: BlacklistMode::Blacklist,
            generation: 1,
            known_pids: std::collections::HashMap::new(),
        };
        assert_eq!(snapshot.default_for_unknown(), Eligibility::Allowed);
    }

    #[test]
    fn snapshot_default_whitelist_denies_unknown() {
        let snapshot = PolicySnapshot {
            mode: BlacklistMode::Whitelist,
            generation: 1,
            known_pids: std::collections::HashMap::new(),
        };
        assert_eq!(snapshot.default_for_unknown(), Eligibility::Denied);
    }

    #[test]
    fn publish_and_read_snapshot() {
        let snapshot = PolicySnapshot {
            mode: BlacklistMode::Blacklist,
            generation: 42,
            known_pids: std::collections::HashMap::new(),
        };
        publish_snapshot(snapshot);
        let loaded = get_snapshot().expect("snapshot should be published");
        assert_eq!(loaded.generation, 42);
        assert_eq!(loaded.mode, BlacklistMode::Blacklist);
    }
}
