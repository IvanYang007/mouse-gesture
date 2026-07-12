//! Integration tests for the mouse gesture daemon.

use mouse_gesture::config::ConfigFile;
use mouse_gesture::gesture::{classify, GestureBuffer, GestureResult, Point};
use mouse_gesture::state_machine::{GestureContext, StateMachine, UpResult};

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

    let mut buf = GestureBuffer::new(2);
    for y in (0..51).step_by(5).rev() {
        buf.add_point(Point { x: 0, y });
    }
    for x in (0..51).step_by(5) {
        buf.add_point(Point { x, y: 0 });
    }

    let result = classify(
        &buf,
        &snapshot.gestures,
        snapshot.rdp_epsilon_sq,
        snapshot.min_gesture_length,
    );
    match result {
        GestureResult::Matched { name, .. } => assert_eq!(name, "maximize"),
        other => panic!("expected Matched(maximize), got {:?}", other),
    }
}

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
    sm.on_move(101, 100);
    let result = sm.on_right_up();
    match result {
        UpResult::ReplaySynthetic => {}
        _ => panic!("expected ReplaySynthetic"),
    }
    assert_eq!(sm.state, mouse_gesture::state_machine::State::Idle);
}

#[test]
fn drawing_above_threshold_classifies_then_resets() {
    let mut sm = StateMachine::new(10);
    sm.on_right_down(false, true, Some(dummy_ctx()));
    sm.on_move(200, 200);
    let result = sm.on_right_up();
    match result {
        UpResult::GestureComplete => {}
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
        UpResult::PassThrough => {}
        _ => panic!("expected PassThrough"),
    }
    assert_eq!(sm.state, mouse_gesture::state_machine::State::Idle);
}

// ── U4: Policy Cache Tests ────────────────────────────────────

#[test]
fn policy_cache_blacklist_mode_unknown_pid_eligible() {
    use mouse_gesture::config::BlacklistMode;
    use std::collections::HashMap;
    let cache = mouse_gesture::app_policy::PolicyCache {
        entries: HashMap::new(),
        mode: BlacklistMode::Blacklist,
    };
    assert!(cache.is_eligible(1234));
}

#[test]
fn policy_cache_whitelist_mode_unknown_pid_ineligible() {
    use mouse_gesture::config::BlacklistMode;
    use std::collections::HashMap;
    let cache = mouse_gesture::app_policy::PolicyCache {
        entries: HashMap::new(),
        mode: BlacklistMode::Whitelist,
    };
    assert!(!cache.is_eligible(1234));
}

#[test]
fn policy_cache_blacklist_excluded_pid_ineligible() {
    use mouse_gesture::app_policy::PolicyEntry;
    use mouse_gesture::config::BlacklistMode;
    use std::collections::HashMap;
    let mut cache = mouse_gesture::app_policy::PolicyCache {
        entries: HashMap::new(),
        mode: BlacklistMode::Blacklist,
    };
    cache.insert(1234, "notepad.exe".into(), false);
    assert!(!cache.is_eligible(1234));
}

// ── U4: Direction Changed Event Tests ──────────────────────────

#[test]
fn direction_changed_event_carries_coordinates() {
    use mouse_gesture::config::Direction;
    let event = mouse_gesture::input_hook::HookEvent::DirectionChanged {
        direction: Direction::N,
        x: 100,
        y: 200,
    };
    match event {
        mouse_gesture::input_hook::HookEvent::DirectionChanged { direction, x, y } => {
            assert_eq!(direction, Direction::N);
            assert_eq!(x, 100);
            assert_eq!(y, 200);
        }
        _ => panic!("expected DirectionChanged"),
    }
}

#[test]
fn gesture_ended_event_carries_match_result() {
    let matched = mouse_gesture::input_hook::HookEvent::GestureEnded {
        matched: true,
        gesture_name: Some("maximize".into()),
        target_hwnd: 0,
    };
    match matched {
        mouse_gesture::input_hook::HookEvent::GestureEnded {
            matched,
            gesture_name,
            ..
        } => {
            assert!(matched);
            assert_eq!(gesture_name, Some("maximize".into()));
        }
        _ => panic!("expected GestureEnded"),
    }

    let unmatched = mouse_gesture::input_hook::HookEvent::GestureEnded {
        matched: false,
        gesture_name: None,
        target_hwnd: 0,
    };
    match unmatched {
        mouse_gesture::input_hook::HookEvent::GestureEnded {
            matched,
            gesture_name,
            ..
        } => {
            assert!(!matched);
            assert_eq!(gesture_name, None);
        }
        _ => panic!("expected GestureEnded"),
    }
}

// ── Phase 3: Action Dispatch Tests ────────────────────────────

#[test]
fn gesture_ended_carries_target_hwnd() {
    // Verify the new target_hwnd field flows through correctly
    let event = mouse_gesture::input_hook::HookEvent::GestureEnded {
        matched: true,
        gesture_name: Some("close".into()),
        target_hwnd: 0x12345678,
    };
    match event {
        mouse_gesture::input_hook::HookEvent::GestureEnded { target_hwnd, .. } => {
            assert_eq!(target_hwnd, 0x12345678);
        }
        _ => panic!("expected GestureEnded"),
    }
}

#[test]
fn gesture_ended_zero_target_hwnd_default() {
    let event = mouse_gesture::input_hook::HookEvent::GestureEnded {
        matched: false,
        gesture_name: None,
        target_hwnd: 0,
    };
    match event {
        mouse_gesture::input_hook::HookEvent::GestureEnded { target_hwnd, .. } => {
            assert_eq!(target_hwnd, 0);
        }
        _ => panic!("expected GestureEnded"),
    }
}

#[test]
fn config_compiles_window_commands() {
    let toml = r#"
[gestures.snap_right]
pattern = "E"
action = { type = "window", command = "snap-right" }
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let snapshot = config.compile(1, 96).unwrap();
    assert_eq!(snapshot.window_commands.len(), 1);
    assert_eq!(snapshot.window_commands[0].0, "snap_right");
}

#[test]
fn config_compiles_keyboard_actions() {
    let toml = r#"
[gestures.close_tab]
pattern = "S E"
action = { type = "key", combo = ["Ctrl", "W"] }
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let snapshot = config.compile(1, 96).unwrap();
    assert!(snapshot.key_map.contains_key("close_tab"));
    let inputs = &snapshot.key_map["close_tab"];
    // Should have Ctrl down, W down, W up, Ctrl up = 4 events
    assert_eq!(inputs.len(), 4);
}

#[test]
fn config_compiles_launch_actions() {
    let toml = r#"
[gestures.open_terminal]
pattern = "W N E"
action = { type = "launch", path = "wt.exe" }
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let snapshot = config.compile(1, 96).unwrap();
    assert_eq!(snapshot.launch_actions.len(), 1);
    assert_eq!(snapshot.launch_actions[0].1, "wt.exe");
}

#[test]
fn snap_rect_calculations() {
    use mouse_gesture::window_ops::{snap_rect, MonitorInfo, SnapPosition};
    use windows::Win32::Foundation::RECT;
    let monitor = MonitorInfo {
        handle: 0,
        name: "test".into(),
        rect: RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        },
        work_rect: RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        },
        dpi: 96,
        is_primary: true,
    };

    let right = snap_rect(&monitor, SnapPosition::Right);
    assert_eq!(right.left, 960);
    assert_eq!(right.right, 1920);

    let left = snap_rect(&monitor, SnapPosition::Left);
    assert_eq!(left.left, 0);
    assert_eq!(left.right, 960);

    let top_left = snap_rect(&monitor, SnapPosition::TopLeft);
    assert_eq!(top_left.right, 960);
    assert_eq!(top_left.bottom, 520);

    let center = snap_rect(&monitor, SnapPosition::Center);
    assert!(center.left > 0);
    assert!(center.right < 1920);
}

#[test]
fn gesture_action_lookup_finds_window_command() {
    let toml = r#"
[gestures.maximize]
pattern = "N E"
action = { type = "window", command = "maximize" }
"#;
    let config: ConfigFile = toml::from_str(toml).unwrap();
    let snapshot = config.compile(1, 96).unwrap();
    // Verify the action is compiled and findable
    let found = snapshot
        .window_commands
        .iter()
        .any(|(name, _)| name == "maximize");
    assert!(found);
}
