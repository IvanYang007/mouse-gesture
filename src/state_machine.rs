//! Mouse state machine — tracks the lifecycle of a right-click gesture
//! from initial button-down through activation, drawing, and release.
//!
//! Designed for the input hook thread: no heap allocation, no locks.

use windows::Win32::Foundation::{HWND, POINT};

/// Self-tag for injected events to prevent self-triggering.
pub const SELF_TAG: usize = 0xDAE0_0001;

/// Mouse button state tracked by the hook thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No gesture in progress. Awaiting right-button-down.
    Idle,
    /// Right-click on an excluded/unknown app — pass through natively.
    NativePass,
    /// Right-button-down on an eligible app, but cursor hasn't moved
    /// past the activation threshold yet.
    Armed,
    /// Cursor moved past activation threshold — gesture is being drawn.
    Drawing,
}

/// Snapshot of application context captured at right-button-down.
#[derive(Debug, Clone)]
pub struct GestureContext {
    pub target_hwnd: HWND,
    pub target_pid: u32,
    pub foreground_hwnd: HWND,
    pub start_point: POINT,
    pub origin_monitor: isize,
    pub origin_dpi: u32,
    pub config_generation: u64,
}

/// Result of processing a right-button-down event.
#[derive(Debug)]
pub enum DownResult {
    /// Event consumed (entered Armed state), suppression recommended.
    Consumed,
    /// Event passed through natively (entered NativePass state).
    PassThrough,
    /// Event is injected — ignored entirely.
    Injected,
}

/// Result of processing a right-button-up event.
#[derive(Debug)]
pub enum UpResult {
    /// Below activation threshold — inject synthetic right-click.
    ReplaySynthetic,
    /// Gesture completed — classify and dispatch.
    GestureComplete,
    /// Pass through natively.
    PassThrough,
    /// Event ignored (injected or invalid state).
    Ignored,
}

/// The state machine, owned by the input hook thread.
pub struct StateMachine {
    pub state: State,
    pub context: Option<GestureContext>,
    /// Activation threshold in physical pixels (from config, DPI-converted).
    pub activation_threshold: i32,
    /// Whether activation has occurred in the current gesture.
    pub activated: bool,
    /// True from right-down Consumed to right-up — used by the
    /// ForceReset watchdog to avoid cancelling valid gestures.
    pub physical_button_down: bool,
    /// Config generation snapshot for the current gesture.
    generation: u64,
}

impl StateMachine {
    pub fn new(activation_threshold: i32) -> Self {
        StateMachine {
            state: State::Idle,
            context: None,
            activation_threshold,
            activated: false,
            physical_button_down: false,
            generation: 0,
        }
    }

    /// Process right-button-down. Returns whether the event was consumed.
    pub fn on_right_down(
        &mut self,
        is_injected: bool,
        is_eligible: bool,
        ctx: Option<GestureContext>,
    ) -> DownResult {
        // Guard against double-click: reject if already in active gesture
        if self.state != State::Idle && self.state != State::NativePass {
            return DownResult::Injected;
        }
        if is_injected {
            return DownResult::Injected;
        }

        if !is_eligible {
            self.state = State::NativePass;
            return DownResult::PassThrough;
        }

        let ctx = match ctx {
            Some(c) => c,
            None => {
                self.state = State::NativePass;
                return DownResult::PassThrough;
            }
        };

        self.generation = ctx.config_generation;
        self.context = Some(ctx);
        self.activated = false;
        self.physical_button_down = true;
        self.state = State::Armed;
        DownResult::Consumed
    }

    /// Process mouse movement. Returns true if the gesture has activated
    /// (crossed the threshold and transitioned to Drawing).
    pub fn on_move(&mut self, x: i32, y: i32) -> bool {
        if self.state != State::Armed && self.state != State::Drawing {
            return false;
        }

        if self.activated {
            // Already drawing — no state change
            return false;
        }

        let ctx = match &self.context {
            Some(c) => c,
            None => return false,
        };

        let dx = x - ctx.start_point.x;
        let dy = y - ctx.start_point.y;
        let dist_sq = dx * dx + dy * dy;
        let threshold_sq = self.activation_threshold * self.activation_threshold;

        if dist_sq >= threshold_sq {
            self.activated = true;
            self.state = State::Drawing;
            true // activation occurred
        } else {
            false
        }
    }

    /// Process right-button-up. Returns the action to take.
    pub fn on_right_up(&mut self) -> UpResult {
        match self.state {
            State::Idle => UpResult::Ignored,
            State::NativePass => {
                self.state = State::Idle;
                UpResult::PassThrough
            }
            State::Armed => {
                // Below threshold: replay synthetic right-click
                self.reset();
                UpResult::ReplaySynthetic
            }
            State::Drawing => {
                // Gesture completed: classify
                self.reset();
                UpResult::GestureComplete
            }
        }
    }

    /// Force-reset the state machine (lost button-up, desktop switch, timeout).
    pub fn force_reset(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.state = State::Idle;
        self.context = None;
        self.activated = false;
        self.physical_button_down = false;
    }

    /// The config generation for the current gesture (used to verify
    /// hot-reload hasn't changed config mid-gesture).
    pub fn current_generation(&self) -> u64 {
        self.generation
    }
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_ctx() -> GestureContext {
        GestureContext {
            target_hwnd: HWND(std::ptr::null_mut()),
            target_pid: 1234,
            foreground_hwnd: HWND(std::ptr::null_mut()),
            start_point: POINT { x: 100, y: 100 },
            origin_monitor: 0,
            origin_dpi: 96,
            config_generation: 1,
        }
    }

    #[test]
    fn eligible_app_enters_armed() {
        let mut sm = StateMachine::new(10);
        let result = sm.on_right_down(false, true, Some(dummy_ctx()));
        match result {
            DownResult::Consumed => assert_eq!(sm.state, State::Armed),
            _ => panic!("expected Consumed"),
        }
    }

    #[test]
    fn excluded_app_passes_through() {
        let mut sm = StateMachine::new(10);
        let result = sm.on_right_down(false, false, None);
        match result {
            DownResult::PassThrough => assert_eq!(sm.state, State::NativePass),
            _ => panic!("expected PassThrough"),
        }
    }

    #[test]
    fn injected_event_ignored() {
        let mut sm = StateMachine::new(10);
        let result = sm.on_right_down(true, true, Some(dummy_ctx()));
        match result {
            DownResult::Injected => assert_eq!(sm.state, State::Idle),
            _ => panic!("expected Injected"),
        }
    }

    #[test]
    fn below_threshold_replays() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        // Move only 1px — below threshold
        sm.on_move(101, 100);
        let result = sm.on_right_up();
        match result {
            UpResult::ReplaySynthetic => {},
            _ => panic!("expected ReplaySynthetic, got {:?}", result),
        }
    }

    #[test]
    fn above_threshold_classifies() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        // Move past threshold
        let activated = sm.on_move(200, 200);
        assert!(activated);
        assert_eq!(sm.state, State::Drawing);
        let result = sm.on_right_up();
        match result {
            UpResult::GestureComplete => {},
            _ => panic!("expected GestureComplete, got {:?}", result),
        }
    }

    #[test]
    fn activation_remains_after_returning_to_start() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        sm.on_move(200, 200); // activate
        sm.on_move(100, 100); // return to start — still activated
        assert!(sm.activated);
        assert_eq!(sm.state, State::Drawing);
    }

    #[test]
    fn force_reset_returns_to_idle() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        sm.on_move(200, 200);
        sm.force_reset();
        assert_eq!(sm.state, State::Idle);
        assert!(sm.context.is_none());
    }

    #[test]
    fn physical_button_down_set_on_consumed() {
        let mut sm = StateMachine::new(10);
        let result = sm.on_right_down(false, true, Some(dummy_ctx()));
        assert!(matches!(result, DownResult::Consumed));
        assert!(sm.physical_button_down);
    }

    #[test]
    fn physical_button_down_cleared_on_reset() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        assert!(sm.physical_button_down);
        sm.force_reset();
        assert!(!sm.physical_button_down);
    }

    #[test]
    fn physical_button_down_cleared_on_right_up() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        sm.on_move(200, 200);
        sm.on_right_up();
        assert!(!sm.physical_button_down);
    }

    #[test]
    fn physical_button_down_cleared_on_replay() {
        let mut sm = StateMachine::new(10);
        sm.on_right_down(false, true, Some(dummy_ctx()));
        sm.on_move(101, 100); // below threshold (threshold=10, dist=1)
        // Oops — we moved 1px and threshold is 10, so still Armed.
        // This test verifies physical_button_down resets on ReplaySynthetic.
        // Actually the threshold is squared: (101-100)² + 0² = 1 < 100, so Armed.
        let result = sm.on_right_up();
        assert!(matches!(result, UpResult::ReplaySynthetic));
        assert!(!sm.physical_button_down);
    }
}
