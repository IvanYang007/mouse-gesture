//! Integration tests for the mouse gesture daemon.
//! These tests verify cross-module behavior that can't be
//! tested in unit tests alone.

use mouse_gesture::config::{ConfigFile, ConfigSnapshot, Direction};
use mouse_gesture::gesture::{GestureBuffer, GestureResult, Point, classify};
use mouse_gesture::state_machine::{StateMachine, DownResult, UpResult, GestureContext};

// ── Config + Recognizer Integration ────────────────────────────

#[test]
fn full_pipeline_parse_compile_recognize() {
    let toml = r#"
[settings]
activation_threshold_dip = 3.0

[gestures.maximize]
pattern = "N E"
action = { type = "window", command = "maximize" }

[gestures.minimize]
pattern = "S W"
action = { type = "window", command = "minimize" }

[gestures.close_tab]
pattern = "S E"
action = { type = "key", combo = ["Ctrl", "W"] }
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let snapshot = config.compile(1, 96).unwrap();

    assert_eq!(snapshot.gestures.len(), 3);
    assert!(snapshot.key_map.contains_key("close_tab"));

    // Simulate a N-E gesture
    let mut buf = GestureBuffer::new(2);
    // Move up
    for y in (50..=0).step_by(5).rev() {
        buf.add_point(Point { x: 0, y });
    }
    // Move right
    for x in (0..50).step_by(5) {
        buf.add_point(Point { x, y: 0 });
    }

    let result = classify(&buf, &snapshot.gestures, snapshot.rdp_epsilon_sq, snapshot.min_gesture_length);
    match result {
        GestureResult::Matched { name, .. } => assert_eq!(name, "maximize"),
        other => panic!("expected Matched(maximize), got {:?}", other),
    }
}

// ── State Machine + Config Integration ─────────────────────────

fn dummy_ctx() -> GestureContext {
    GestureContext {
        target_hwnd: windows::Win32::Foundation::HWND(std::ptr::null_mut()),
        target_pid: 1234,
        foreground_hwnd: windows::Win32::Foundation::HWND(std::ptr::null_mut()),
        start_point: windows::Win32::Foundation::POINT { x: 100, y: 100 },
        origin_monitor: 0,
        origin_dpi: 96,
        config_generation: 1,
    }
}

#[test]
fn armed_below_threshold_replays_then_resets() {
    let mut sm = StateMachine::new(10);
    sm.on_right_down(false, true, Some(dummy_ctx()));
    assert_eq!(sm.state, mouse_gesture::state_machine::State::Armed);

    // Move 1px — below threshold
    sm.on_move(101, 100);

    let result = sm.on_right_up();
    match result {
        UpResult::ReplaySynthetic => {},
        _ => panic!("expected ReplaySynthetic"),
    }

    // State should reset
    assert_eq!(sm.state, mouse_gesture::state_machine::State::Idle);
}

#[test]
fn drawing_above_threshold_classifies_then_resets() {
    let mut sm = StateMachine::new(10);
    sm.on_right_down(false, true, Some(dummy_ctx()));
    sm.on_move(200, 200); // far past threshold

    let result = sm.on_right_up();
    match result {
        UpResult::GestureComplete => {},
        _ => panic!("expected GestureComplete"),
    }

    assert_eq!(sm.state, mouse_gesture::state_machine::State::Idle);
}

#[test]
fn native_pass_passes_through_and_resets() {
    let mut sm = StateMachine::new(10);
    sm.on_right_down(false, false, None);
    let result = sm.on_right_up();
    match result {
        UpResult::PassThrough => {},
        _ => panic!("expected PassThrough"),
    }
    assert_eq!(sm.state, mouse_gesture::state_machine::State::Idle);
}
