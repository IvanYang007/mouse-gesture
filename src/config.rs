//! Configuration system — TOML schema, parsing, validation, and
//! compilation into immutable snapshots for lock-free access from
//! the input hook thread.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

// ── TOML Schema ────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub blacklist: Blacklist,
    #[serde(default)]
    pub gestures: std::collections::BTreeMap<String, GestureDef>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_activation_threshold")]
    pub activation_threshold_dip: f64,
    #[serde(default = "default_sample_distance")]
    pub sample_distance_dip: f64,
    #[serde(default = "default_rdp_epsilon")]
    pub rdp_epsilon_dip: f64,
    #[serde(default = "default_min_gesture_length")]
    pub min_gesture_length: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Blacklist {
    #[serde(default)]
    pub mode: BlacklistMode,
    #[serde(default)]
    pub apps: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BlacklistMode {
    Blacklist,
    Whitelist,
}

impl Default for BlacklistMode {
    fn default() -> Self {
        BlacklistMode::Blacklist
    }
}

impl Default for Blacklist {
    fn default() -> Self {
        Blacklist {
            mode: BlacklistMode::Blacklist,
            apps: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct GestureDef {
    pub pattern: String,
    pub action: ActionDef,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ActionDef {
    #[serde(rename = "window")]
    Window { command: WindowCommand },
    #[serde(rename = "key")]
    Key { combo: Vec<String> },
    #[serde(rename = "launch")]
    Launch { path: String, #[serde(default)] args: Vec<String> },
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum WindowCommand {
    Maximize,
    Minimize,
    Restore,
    Close,
    SnapLeft,
    SnapRight,
    SnapTop,
    SnapBottom,
    SnapTopLeft,
    SnapTopRight,
    SnapBottomLeft,
    SnapBottomRight,
    Center,
    ToggleAlwaysOnTop,
    MoveToMonitor(u32),
}

// ── Direction type ─────────────────────────────────────────────

/// Eight cardinal/intercardinal directions for gesture encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    N, NE, E, SE, S, SW, W, NW,
}

impl Direction {
    /// Parse a direction token from config text.
    pub fn from_str(s: &str) -> Result<Self> {
        match s.to_uppercase().as_str() {
            "N" => Ok(Direction::N),
            "NE" => Ok(Direction::NE),
            "E" => Ok(Direction::E),
            "SE" => Ok(Direction::SE),
            "S" => Ok(Direction::S),
            "SW" => Ok(Direction::SW),
            "W" => Ok(Direction::W),
            "NW" => Ok(Direction::NW),
            _ => anyhow::bail!("unknown direction: '{}' (expected N/NE/E/SE/S/SW/W/NW)", s),
        }
    }
}

// ── Immutable Config Snapshot ──────────────────────────────────

/// Pre-compiled, immutable configuration snapshot.
/// Sent from the UI thread to the input thread between gestures.
/// All fields are plain data — no locks, no indirection.
#[derive(Debug, Clone)]
pub struct ConfigSnapshot {
    pub generation: u64,
    pub activation_threshold_physical: i32,
    pub sample_distance_physical: i32,
    pub rdp_epsilon_sq: f64,
    pub min_gesture_length: u32,
    pub blacklist_mode: BlacklistMode,
    pub blacklist_apps: HashSet<String>,
    /// Compiled gesture patterns: Vec<(name, sequence)>
    pub gestures: Vec<(String, Vec<Direction>)>,
    /// Compiled keyboard shortcuts: key_name → inputs
    pub key_map: std::collections::HashMap<String, Vec<CompiledInput>>,
    /// Compiled window commands
    pub window_commands: Vec<(String, WindowCommand)>,
    /// Compiled launch actions
    pub launch_actions: Vec<(String, String, Vec<String>)>,
}

/// Pre-compiled SendInput keyboard entry.
#[derive(Debug, Clone)]
pub struct CompiledInput {
    pub vk: u16,
    pub scan: u16,
    pub is_extended: bool,
    pub is_up: bool,
}

// ── Config loading pipeline ────────────────────────────────────

/// Maximum configurable gestures.
pub const MAX_GESTURES: usize = 256;
/// Maximum encoded tokens per gesture.
pub const MAX_TOKENS: usize = 32;
/// Maximum action queue depth.
pub const MAX_ACTION_QUEUE: usize = 16;

impl ConfigFile {
    /// Parse a TOML config file.
    pub fn load(path: &Path) -> Result<Self> {
        // Limit config file size to 1 MiB to prevent OOM on malformed/symlink files
        const MAX_CONFIG_SIZE: u64 = 1_048_576;
        let meta = std::fs::metadata(path)
            .with_context(|| format!("reading config metadata from {}", path.display()))?;
        if meta.len() > MAX_CONFIG_SIZE {
            anyhow::bail!("config file too large ({} bytes, max {} bytes)", meta.len(), MAX_CONFIG_SIZE);
        }
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("reading config from {}", path.display()))?;
        toml::from_str(&content).context("parsing config TOML")
    }

    /// Validate and compile the config into an immutable snapshot.
    pub fn compile(&self, generation: u64, dpi: u32) -> Result<ConfigSnapshot> {
        // Validate gesture count
        if self.gestures.len() > MAX_GESTURES {
            anyhow::bail!(
                "{} gestures configured (max {})",
                self.gestures.len(),
                MAX_GESTURES
            );
        }

        // Convert DIP settings to physical pixels
        let activation_threshold_physical =
            (self.settings.activation_threshold_dip * dpi as f64 / 96.0) as i32;
        let sample_distance_physical =
            (self.settings.sample_distance_dip * dpi as f64 / 96.0) as i32;
        let rdp_epsilon_physical =
            self.settings.rdp_epsilon_dip * dpi as f64 / 96.0;
        let rdp_epsilon_sq = rdp_epsilon_physical * rdp_epsilon_physical;

        // Compile gestures
        let mut gestures = Vec::with_capacity(self.gestures.len());
        let mut exact_patterns: HashSet<Vec<Direction>> = HashSet::new();

        for (name, def) in &self.gestures {
            let pattern: Vec<Direction> = def
                .pattern
                .split_whitespace()
                .map(Direction::from_str)
                .collect::<Result<Vec<_>>>()
                .with_context(|| format!("gesture '{}': invalid pattern", name))?;

            if pattern.len() > MAX_TOKENS {
                anyhow::bail!(
                    "gesture '{}': {} tokens (max {})",
                    name,
                    pattern.len(),
                    MAX_TOKENS
                );
            }

            if !exact_patterns.insert(pattern.clone()) {
                anyhow::bail!("gesture '{}': duplicate exact pattern", name);
            }

            gestures.push((name.clone(), pattern));
        }

        // Compile blacklist
        let blacklist_apps: HashSet<String> = self
            .blacklist
            .apps
            .iter()
            .map(|a| a.to_lowercase())
            .collect();

        // Compile keyboard shortcuts into pre-built INPUT structures
        let mut key_map = std::collections::HashMap::new();
        let mut window_commands = Vec::new();
        let mut launch_actions = Vec::new();

        for (name, def) in &self.gestures {
            match &def.action {
                ActionDef::Key { combo } => {
                    key_map.insert(name.clone(), compile_key_combo(combo)?);
                }
                ActionDef::Window { command } => {
                    window_commands.push((name.clone(), command.clone()));
                }
                ActionDef::Launch { path, args } => {
                    launch_actions.push((
                        name.clone(),
                        path.clone(),
                        args.clone(),
                    ));
                }
            }
        }

        Ok(ConfigSnapshot {
            generation,
            activation_threshold_physical,
            sample_distance_physical,
            rdp_epsilon_sq,
            min_gesture_length: self.settings.min_gesture_length,
            blacklist_mode: self.blacklist.mode.clone(),
            blacklist_apps,
            gestures,
            key_map,
            window_commands,
            launch_actions,
        })
    }
}

impl Default for ConfigFile {
    fn default() -> Self {
        ConfigFile {
            settings: Settings::default(),
            blacklist: Blacklist::default(),
            gestures: std::collections::BTreeMap::new(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            activation_threshold_dip: default_activation_threshold(),
            sample_distance_dip: default_sample_distance(),
            rdp_epsilon_dip: default_rdp_epsilon(),
            min_gesture_length: default_min_gesture_length(),
        }
    }
}

fn default_activation_threshold() -> f64 { 3.0 }
fn default_sample_distance() -> f64 { 2.0 }
fn default_rdp_epsilon() -> f64 { 2.0 }
fn default_min_gesture_length() -> u32 { 2 }

// ── Keyboard Shortcut Compilation ──────────────────────────────

use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT, VK_LWIN, VK_RWIN,
    VK_LEFT, VK_RIGHT, VK_UP, VK_DOWN,
    VK_HOME, VK_END, VK_PRIOR, VK_NEXT,
    VK_INSERT, VK_DELETE, VK_RETURN,
    VK_TAB, VK_ESCAPE, VK_SPACE, VK_BACK,
};

/// Compile a key combo string list into SendInput-ready key events.
fn compile_key_combo(combo: &[String]) -> Result<Vec<CompiledInput>> {
    let mut inputs = Vec::new();

    // Parse all keys, separate modifiers from main keys
    let mut modifiers: Vec<(u16, u16, bool)> = Vec::new();
    let mut main_keys: Vec<(u16, u16, bool)> = Vec::new();

    for key in combo {
        let (vk, scan, is_ext) = resolve_key(key)?;
        if is_modifier(vk) {
            modifiers.push((vk, scan, is_ext));
        } else {
            main_keys.push((vk, scan, is_ext));
        }
    }

    // Press modifiers (Ctrl before Alt before Shift before Win)
    modifiers.sort_by_key(|&(vk, _, _)| modifier_order(vk));
    for &(vk, scan, is_ext) in &modifiers {
        inputs.push(CompiledInput { vk, scan, is_extended: is_ext, is_up: false });
    }

    // Press main keys
    for &(vk, scan, is_ext) in &main_keys {
        inputs.push(CompiledInput { vk, scan, is_extended: is_ext, is_up: false });
    }

    // Release main keys (reverse order)
    for &(vk, scan, is_ext) in main_keys.iter().rev() {
        inputs.push(CompiledInput { vk, scan, is_extended: is_ext, is_up: true });
    }

    // Release modifiers (reverse order)
    for &(vk, scan, is_ext) in modifiers.iter().rev() {
        inputs.push(CompiledInput { vk, scan, is_extended: is_ext, is_up: true });
    }

    Ok(inputs)
}

fn modifier_order(vk: u16) -> u8 {
    if vk == VK_CONTROL.0 { 0 }
    else if vk == VK_MENU.0 { 1 }
    else if vk == VK_SHIFT.0 { 2 }
    else { 3 } // Win
}

fn is_modifier(vk: u16) -> bool {
    vk == VK_CONTROL.0 || vk == VK_MENU.0 || vk == VK_SHIFT.0
        || vk == VK_LWIN.0 || vk == VK_RWIN.0
}

/// Resolve a key name to (VIRTUAL_KEY, scan_code, is_extended).
fn resolve_key(name: &str) -> Result<(u16, u16, bool)> {
    let lower = name.to_lowercase();
    match lower.as_str() {
        // Modifiers
        "ctrl" | "control" => Ok((VK_CONTROL.0, 0x1D, false)),
        "alt" | "menu" => Ok((VK_MENU.0, 0x38, false)),
        "shift" => Ok((VK_SHIFT.0, 0x2A, false)),
        "win" | "lwin" | "windows" => Ok((VK_LWIN.0, 0x5B, true)),
        "rwin" => Ok((VK_RWIN.0, 0x5C, true)),

        // Navigation
        "left" => Ok((VK_LEFT.0, 0x4B, true)),
        "right" => Ok((VK_RIGHT.0, 0x4D, true)),
        "up" => Ok((VK_UP.0, 0x48, true)),
        "down" => Ok((VK_DOWN.0, 0x50, true)),
        "home" => Ok((VK_HOME.0, 0x47, true)),
        "end" => Ok((VK_END.0, 0x4F, true)),
        "pgup" | "pageup" => Ok((VK_PRIOR.0, 0x49, true)),
        "pgdn" | "pagedown" => Ok((VK_NEXT.0, 0x51, true)),

        // Editing
        "insert" | "ins" => Ok((VK_INSERT.0, 0x52, true)),
        "delete" | "del" => Ok((VK_DELETE.0, 0x53, true)),
        "backspace" | "back" => Ok((VK_BACK.0, 0x0E, false)),
        "enter" | "return" => Ok((VK_RETURN.0, 0x1C, false)),
        "tab" => Ok((VK_TAB.0, 0x0F, false)),
        "escape" | "esc" => Ok((VK_ESCAPE.0, 0x01, false)),
        "space" => Ok((VK_SPACE.0, 0x39, false)),

        // Letters A-Z
        s if s.len() == 1 => {
            let c = s.chars().next().unwrap();
            if c.is_ascii_alphabetic() {
                let upper = c.to_ascii_uppercase();
                let vk = upper as u16;
                // MapVirtualKey for scan code
                let scan = key_to_scan(vk);
                Ok((vk, scan, false))
            } else if c.is_ascii_digit() {
                let vk = c as u16;
                let scan = key_to_scan(vk);
                Ok((vk, scan, false))
            } else {
                anyhow::bail!("unknown key: '{}'", name);
            }
        }

        // F-keys
        s if s.starts_with('f') => {
            if let Ok(n) = s[1..].parse::<u16>() {
                if (1..=24).contains(&n) {
                    let vk = VK_F1.0 + n - 1;
                    Ok((vk, key_to_scan(vk), false))
                } else {
                    anyhow::bail!("F-key out of range: F{}", n)
                }
            } else {
                anyhow::bail!("unknown key: '{}'", name);
            }
        }

        _ => anyhow::bail!("unknown key: '{}'", name),
    }
}

fn key_to_scan(vk: u16) -> u16 {
    use windows::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW;
    use windows::Win32::UI::Input::KeyboardAndMouse::MAPVK_VK_TO_VSC;
    unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
}

// We need VK_F1 constant for F-key mapping
const VK_F1: VIRTUAL_KEY = VIRTUAL_KEY(0x70);

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[settings]
activation_threshold_dip = 3.0

[gestures.up_right]
pattern = "N E"
action = { type = "window", command = "maximize" }

[gestures.down_left]
pattern = "S W"
action = { type = "window", command = "minimize" }
"#;
        let config: ConfigFile = toml::from_str(toml).unwrap();
        assert_eq!(config.gestures.len(), 2);
        assert_eq!(config.settings.activation_threshold_dip, 3.0);
    }

    #[test]
    fn duplicate_exact_patterns_rejected() {
        let toml = r#"
[gestures.g1]
pattern = "N E"
action = { type = "window", command = "maximize" }

[gestures.g2]
pattern = "N E"
action = { type = "window", command = "minimize" }
"#;
        let config: ConfigFile = toml::from_str(toml).unwrap();
        let result = config.compile(1, 96);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn invalid_direction_rejected() {
        let toml = r#"
[gestures.bad]
pattern = "N X"
action = { type = "window", command = "maximize" }
"#;
        let config: ConfigFile = toml::from_str(toml).unwrap();
        let result = config.compile(1, 96);
        assert!(result.is_err());
    }

    #[test]
    fn too_many_tokens_rejected() {
        let tokens = (0..33).map(|_| "E").collect::<Vec<_>>().join(" ");
        let toml = format!(r#"
[gestures.long]
pattern = "{}"
action = {{ type = "window", command = "maximize" }}
"#, tokens);
        let config: ConfigFile = toml::from_str(&toml).unwrap();
        let result = config.compile(1, 96);
        assert!(result.is_err());
    }

    #[test]
    fn empty_config_is_valid() {
        let config = ConfigFile::default();
        let snapshot = config.compile(0, 96).unwrap();
        assert!(snapshot.gestures.is_empty());
    }

    #[test]
    fn blacklist_config() {
        let toml = r#"
[blacklist]
mode = "blacklist"
apps = ["notepad.exe", "devenv.exe"]
"#;
        let config: ConfigFile = toml::from_str(toml).unwrap();
        let snapshot = config.compile(1, 96).unwrap();
        assert_eq!(snapshot.blacklist_mode, BlacklistMode::Blacklist);
        assert!(snapshot.blacklist_apps.contains("notepad.exe"));
        assert!(snapshot.blacklist_apps.contains("devenv.exe"));
    }
}
