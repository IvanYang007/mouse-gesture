//! Library crate for mouse-gesture daemon.
//! Re-exports all modules for integration tests.

pub mod app_policy;
pub mod autostart;
pub mod config;
pub mod gesture;
pub mod input_hook;
pub mod input_inject;
pub mod launch;
pub mod lifecycle;
pub mod overlay;
pub mod state_machine;
pub mod tray;
pub mod win_handles;
pub mod window_ops;
