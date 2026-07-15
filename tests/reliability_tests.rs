//! Reliability and stress tests for the mouse gesture daemon.
//! These tests validate resilience under edge cases, error recovery,
//! and resource stability.

use mouse_gesture::config::ConfigFile;
use mouse_gesture::gesture::{classify, GestureBuffer, GestureResult, Point};
use mouse_gesture::state_machine::{DownResult, State, StateMachine, UpResult};

// ── Gesture Buffer Stress Tests ────────────────────────────────

#[test]
fn gesture_buffer_handles_oversized_input_gracefully() {
    let mut buf = GestureBuffer::new(1);
    // Push 10,000 points — far beyond MAX_POINTS (256)
    for i in 0..10_000 {
        let ok = buf.add_point(Point {
            x: i % 1920,
            y: (i / 1920) % 1080,
        });
        // Buffer should never panic — may reject after decimation
        if !ok {
            // Acceptable: buffer overflow handled
        }
    }
    // Buffer should still be usable after stress
    let result = buf.add_point(Point { x: 100, y: 100 });
    assert!(
        result || !buf.is_empty(),
        "buffer should have points or accept new ones"
    );
}

#[test]
fn gesture_buffer_resets_correctly() {
    let mut buf = GestureBuffer::new(2);
    for _ in 0..5 {
        buf.clear();
        for i in 0..20 {
            buf.add_point(Point { x: i * 10, y: 50 });
        }
        assert!(!buf.is_empty());
    }
}

#[test]
fn classify_handles_empty_buffer() {
    let buf = GestureBuffer::new(2);
    let patterns = vec![];
    let result = classify(&buf, &patterns, 4.0, 2);
    match result {
        GestureResult::TooShort => {}
        _ => panic!("expected TooShort for empty buffer"),
    }
}

// ── State Machine Resilience Tests ─────────────────────────────

#[test]
fn state_machine_recovers_from_multiple_resets() {
    let mut sm = StateMachine::new(10);
    for _ in 0..100 {
        sm.force_reset();
        assert_eq!(sm.state, State::Idle);
        assert!(sm.context.is_none());
        assert!(!sm.activated);
    }
}

#[test]
fn state_machine_handles_spurious_events() {
    let mut sm = StateMachine::new(10);
    // Spurious right-up with no prior right-down
    let result = sm.on_right_up();
    match result {
        UpResult::Ignored => {}
        _ => panic!("expected Ignored for spurious right-up"),
    }
    // Spurious move in Idle
    assert!(!sm.on_move(100, 100));
    // Still in Idle
    assert_eq!(sm.state, State::Idle);
}

#[test]
fn state_machine_rejects_injected_events_when_ineligible() {
    let mut sm = StateMachine::new(10);
    // Injected event when not eligible
    let result = sm.on_right_down(true, false, None);
    match result {
        DownResult::Injected => {}
        _ => panic!("expected Injected"),
    }
}

#[test]
fn state_machine_excluded_app_does_native_pass() {
    let mut sm = StateMachine::new(10);
    sm.on_right_down(false, false, None);
    assert_eq!(sm.state, State::NativePass);
    let result = sm.on_right_up();
    match result {
        UpResult::PassThrough => {}
        _ => panic!("expected PassThrough"),
    }
    assert_eq!(sm.state, State::Idle);
}

// ── Config Resilience Tests ────────────────────────────────────

#[test]
fn config_handles_extreme_values() {
    let toml = r#"
[settings]
activation_threshold_dip = 0.1
sample_distance_dip = 0.1
rdp_epsilon_dip = 0.1
min_gesture_length = 1
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let snapshot = config.compile(1, 96);
    assert!(snapshot.is_ok(), "extreme values should compile");
}

#[test]
fn config_rejects_long_patterns() {
    let tokens = (0..33).map(|_| "E").collect::<Vec<_>>().join(" ");
    let toml = format!(
        r#"
[gestures.long]
pattern = "{}"
action = {{ type = "window", command = "maximize" }}
"#,
        tokens
    );
    let config: ConfigFile = toml::from_str(&toml).unwrap();
    let result = config.compile(1, 96);
    assert!(result.is_err());
}

// ── Key Code Resilience Tests ──────────────────────────────────

#[test]
fn key_resolution_handles_unknown_keys() {
    let toml = r#"
[gestures.bad_key]
pattern = "E"
action = { type = "key", combo = ["Ctrl", "NonExistentKey"] }
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let result = config.compile(1, 96);
    assert!(result.is_err());
}

#[test]
fn key_resolution_handles_all_letters() {
    for c in 'A'..='Z' {
        let toml = format!(
            r#"
[gestures.test]
pattern = "E"
action = {{ type = "key", combo = ["{}"] }}
"#,
            c
        );
        let config: ConfigFile = toml::from_str(&toml).unwrap();
        assert!(config.compile(1, 96).is_ok(), "letter {} should resolve", c);
    }
}

#[test]
fn key_resolution_handles_f_keys() {
    for n in 1..=24 {
        let toml = format!(
            r#"
[gestures.test]
pattern = "E"
action = {{ type = "key", combo = ["F{}"] }}
"#,
            n
        );
        let config: ConfigFile = toml::from_str(&toml).unwrap();
        assert!(config.compile(1, 96).is_ok(), "F{} should resolve", n);
    }
}

// ── Classification Accuracy Under Stress ───────────────────────

#[test]
fn classify_consistently_recognizes_pattern() {
    // Verify that the same gesture always produces the same result
    use mouse_gesture::config::Direction;
    let mut buf = GestureBuffer::new(2);
    for x in (0..100).step_by(5) {
        buf.add_point(Point { x, y: 50 });
    }
    for y in (50..100).step_by(5) {
        buf.add_point(Point { x: 100, y });
    }

    let patterns = vec![("test".into(), vec![Direction::E, Direction::S])];
    for _ in 0..100 {
        let result = classify(&buf, &patterns, 4.0, 2);
        match result {
            GestureResult::Matched { name, .. } => assert_eq!(name, "test"),
            other => panic!("expected match on iteration, got {:?}", other),
        }
    }
}
