use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// ─── Settings ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WobblerSettings {
    pub idle_threshold_secs: u64,
    pub wobble_interval_ms: u64,
    pub wobble_radius: i32,
    pub auto_mode: bool,
}

impl Default for WobblerSettings {
    fn default() -> Self {
        WobblerSettings {
            idle_threshold_secs: 60,
            wobble_interval_ms: 1000,
            wobble_radius: 5,
            auto_mode: false,
        }
    }
}

// ─── Status snapshot (serialised and sent to the frontend) ───────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStatus {
    pub is_wobbling: bool,
    pub is_manual: bool,
    pub auto_mode: bool,
    pub idle_seconds: u64,
    pub settings: WobblerSettings,
    pub error: Option<String>,
}

// ─── Core state ───────────────────────────────────────────────────────────────

/// Movement past this many pixels (on either axis) counts as the user touching
/// the mouse, both for idle-timer reset and for taking over an active wobble.
pub const MOVE_THRESHOLD_PX: i32 = 3;

pub struct WobblerCore {
    /// True while manual start has been requested (tray, button, shortcut).
    pub is_manual: bool,
    pub settings: WobblerSettings,
    /// Timestamp of last confirmed user-initiated mouse movement.
    pub last_user_activity: Instant,
    /// Absolute position the wobble orbits around.
    pub anchor_pos: Option<(i32, i32)>,
    /// Position we last moved the cursor to during a wobble. While wobbling this
    /// is the reference for "did the user take over?".
    pub expected_pos: Option<(i32, i32)>,
    /// Cursor position observed on the previous tick. Used to detect real user
    /// movement while idle (when `expected_pos` is None), so the idle timer
    /// resets whenever the user actually moves the mouse.
    pub last_seen_pos: Option<(i32, i32)>,
    /// Index into the 8-point orbit (0–7).
    pub wobble_step: u64,
    /// When the most recent wobble move was issued.
    pub last_wobble: Instant,
    pub is_wobbling: bool,
    pub last_error: Option<String>,
}

impl Default for WobblerCore {
    fn default() -> Self {
        WobblerCore {
            is_manual: false,
            settings: WobblerSettings::default(),
            last_user_activity: Instant::now(),
            anchor_pos: None,
            expected_pos: None,
            last_seen_pos: None,
            wobble_step: 0,
            // Back-date so the very first wobble fires on the first tick.
            last_wobble: Instant::now() - Duration::from_secs(10),
            is_wobbling: false,
            last_error: None,
        }
    }
}

/// Convenience alias used in both the core and the Tauri layer.
pub type SharedCore = Arc<Mutex<WobblerCore>>;

// ─── Pure wobble math ─────────────────────────────────────────────────────────

/// Returns the (dx, dy) offset for one step of the 8-point circular orbit.
/// `step` wraps naturally via sin/cos periodicity; no modulo required.
pub fn wobble_offset(step: u64, radius: i32) -> (i32, i32) {
    let angle = step as f64 * std::f64::consts::TAU / 8.0;
    let dx = (angle.cos() * radius as f64).round() as i32;
    let dy = (angle.sin() * radius as f64).round() as i32;
    (dx, dy)
}

// ─── State-machine tick (pure, no I/O) ───────────────────────────────────────

/// What the caller should do after a tick.
#[derive(Debug, PartialEq)]
pub struct TickResult {
    /// `Some((x, y))` → move the cursor to this absolute screen position.
    pub move_to: Option<(i32, i32)>,
}

/// Advances the state machine by one step given the current cursor position.
/// All side effects (moving the mouse, sleeping) are the caller's concern.
pub fn tick(c: &mut WobblerCore, current: (i32, i32)) -> TickResult {
    // ── Detect user-initiated cursor movement ────────────────────────────────
    // Reference = where the cursor "should" be if the user did nothing.
    // While wobbling we just placed it at `expected_pos`; otherwise it should
    // still be wherever we last saw it. Comparing against `last_seen_pos` is
    // what lets the idle timer reset on real movement while not wobbling.
    let reference = c.expected_pos.or(c.last_seen_pos);
    let user_moved = reference.map_or(false, |r| {
        (current.0 - r.0).abs() > MOVE_THRESHOLD_PX || (current.1 - r.1).abs() > MOVE_THRESHOLD_PX
    });

    if user_moved {
        c.last_user_activity = Instant::now();
        c.expected_pos = None;
        if c.is_manual {
            // Manual mode: keep wobbling but orbit the new position.
            c.anchor_pos = Some(current);
            c.wobble_step = 0;
        } else if c.is_wobbling {
            // Auto mode: cede control to the user.
            c.is_wobbling = false;
            c.anchor_pos = None;
        }
    }

    let idle_secs = c.last_user_activity.elapsed().as_secs();

    let should_wobble = if c.is_manual {
        true
    } else if c.settings.auto_mode {
        idle_secs >= c.settings.idle_threshold_secs
    } else {
        false
    };

    // ── State transitions ─────────────────────────────────────────────────────
    if should_wobble && !c.is_wobbling {
        // Not yet wobbling → start.
        c.is_wobbling = true;
        c.anchor_pos = Some(current);
        c.wobble_step = 0;
        // Back-date so the first move fires this tick.
        c.last_wobble = Instant::now() - Duration::from_millis(c.settings.wobble_interval_ms);
    }

    if !should_wobble && c.is_wobbling && !c.is_manual {
        // Auto mode idle timer fell below threshold → stop.
        c.is_wobbling = false;
        c.anchor_pos = None;
    }

    // Recovery: `is_wobbling` was set externally (command/tray/shortcut) which
    // skips the transition above.  Initialise anchor from current position.
    if c.is_wobbling && c.anchor_pos.is_none() {
        c.anchor_pos = Some(current);
        c.wobble_step = 0;
        c.last_wobble = Instant::now() - Duration::from_millis(c.settings.wobble_interval_ms);
    }

    // ── Decide whether to move this tick ─────────────────────────────────────
    let interval = Duration::from_millis(c.settings.wobble_interval_ms);
    let move_to = if c.is_wobbling && c.last_wobble.elapsed() >= interval {
        c.anchor_pos.map(|anchor| {
            let (dx, dy) = wobble_offset(c.wobble_step, c.settings.wobble_radius);
            (anchor.0 + dx, anchor.1 + dy)
        })
    } else {
        None
    };

    // Remember where the cursor was this tick so the next tick can detect real
    // user movement even when we are not wobbling.
    c.last_seen_pos = Some(current);

    TickResult { move_to }
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── wobble_offset ──────────────────────────────────────────────────────────

    #[test]
    fn wobble_offset_step0_points_right() {
        assert_eq!(wobble_offset(0, 5), (5, 0));
    }

    #[test]
    fn wobble_offset_step2_points_up() {
        assert_eq!(wobble_offset(2, 5), (0, 5));
    }

    #[test]
    fn wobble_offset_step4_points_left() {
        assert_eq!(wobble_offset(4, 5), (-5, 0));
    }

    #[test]
    fn wobble_offset_step6_points_down() {
        assert_eq!(wobble_offset(6, 5), (0, -5));
    }

    #[test]
    fn wobble_offset_full_revolution_is_identity() {
        for r in [1, 5, 10, 20] {
            assert_eq!(
                wobble_offset(0, r),
                wobble_offset(8, r),
                "radius={r}: step 0 != step 8"
            );
        }
    }

    #[test]
    fn wobble_offset_zero_radius_is_zero() {
        for step in 0..16 {
            assert_eq!(wobble_offset(step, 0), (0, 0), "step {step}");
        }
    }

    #[test]
    fn wobble_offset_is_periodic_with_period_8() {
        for step in 0..8u64 {
            for r in [3, 7, 15] {
                assert_eq!(
                    wobble_offset(step, r),
                    wobble_offset(step + 8, r),
                    "step={step} r={r}"
                );
            }
        }
    }

    // ── WobblerSettings ────────────────────────────────────────────────────────

    #[test]
    fn default_settings_values() {
        let s = WobblerSettings::default();
        assert_eq!(s.idle_threshold_secs, 60);
        assert_eq!(s.wobble_interval_ms, 1000);
        assert_eq!(s.wobble_radius, 5);
        assert!(!s.auto_mode);
    }

    #[test]
    fn settings_round_trips_through_json() {
        let orig = WobblerSettings {
            idle_threshold_secs: 120,
            wobble_interval_ms: 500,
            wobble_radius: 8,
            auto_mode: true,
        };
        let parsed: WobblerSettings =
            serde_json::from_str(&serde_json::to_string(&orig).unwrap()).unwrap();
        assert_eq!(parsed.idle_threshold_secs, 120);
        assert_eq!(parsed.wobble_interval_ms, 500);
        assert_eq!(parsed.wobble_radius, 8);
        assert!(parsed.auto_mode);
    }

    // ── tick() ─────────────────────────────────────────────────────────────────

    fn core_idle_for(secs: u64, settings: WobblerSettings) -> WobblerCore {
        let mut c = WobblerCore { settings, ..WobblerCore::default() };
        c.last_user_activity = Instant::now() - Duration::from_secs(secs);
        c
    }

    #[test]
    fn tick_inactive_produces_no_move() {
        let mut c = WobblerCore::default();
        assert_eq!(tick(&mut c, (100, 100)).move_to, None);
        assert!(!c.is_wobbling);
    }

    #[test]
    fn tick_manual_start_moves_immediately() {
        let mut c = WobblerCore::default();
        c.is_manual = true;
        let r = tick(&mut c, (200, 300));
        assert!(r.move_to.is_some(), "first manual tick must produce a move");
        assert!(c.is_wobbling);
        assert_eq!(c.anchor_pos, Some((200, 300)));
    }

    #[test]
    fn tick_recovery_when_wobbling_set_externally() {
        // Simulate toggle_wobble setting is_wobbling=true without anchor.
        let mut c = WobblerCore::default();
        c.is_manual = true;
        c.is_wobbling = true; // set externally; anchor still None
        let r = tick(&mut c, (50, 60));
        assert!(c.anchor_pos.is_some(), "recovery block must set anchor");
        assert!(r.move_to.is_some(), "should move on the recovery tick");
    }

    #[test]
    fn tick_interval_not_elapsed_produces_no_move() {
        let mut c = WobblerCore::default();
        c.is_manual = true;

        // First tick: starts wobbling, fires first move immediately.
        let r1 = tick(&mut c, (100, 100));
        assert!(r1.move_to.is_some());

        // Reset last_wobble to "just now" to simulate the move having happened.
        c.last_wobble = Instant::now();

        // Second tick right after: interval has not elapsed.
        assert_eq!(tick(&mut c, (100, 100)).move_to, None);
    }

    #[test]
    fn tick_auto_mode_below_threshold_stays_quiet() {
        let mut settings = WobblerSettings::default();
        settings.auto_mode = true;
        settings.idle_threshold_secs = 9999;
        let mut c = core_idle_for(0, settings);
        assert_eq!(tick(&mut c, (100, 100)).move_to, None);
        assert!(!c.is_wobbling);
    }

    #[test]
    fn tick_auto_mode_above_threshold_starts_wobbling() {
        let mut settings = WobblerSettings::default();
        settings.auto_mode = true;
        settings.idle_threshold_secs = 5;
        // Idle for 10 seconds → above threshold.
        let mut c = core_idle_for(10, settings);
        let r = tick(&mut c, (100, 100));
        assert!(c.is_wobbling);
        assert!(r.move_to.is_some());
    }

    #[test]
    fn tick_user_movement_stops_auto_wobble() {
        let mut settings = WobblerSettings::default();
        settings.auto_mode = true;
        settings.idle_threshold_secs = 1;
        // Start idle so the first tick kicks off wobbling.
        let mut c = core_idle_for(5, settings);

        let r1 = tick(&mut c, (100, 100));
        assert!(c.is_wobbling);
        let moved_to = r1.move_to.unwrap();

        // Record the move as if we physically moved the cursor.
        c.expected_pos = Some(moved_to);
        c.last_wobble = Instant::now();

        // User moves to a different position.
        tick(&mut c, (500, 500));

        assert!(!c.is_wobbling, "user movement must stop auto wobble");
        assert!(c.expected_pos.is_none());
    }

    #[test]
    fn tick_manual_mode_reanchors_after_user_move() {
        let mut c = WobblerCore::default();
        c.is_manual = true;

        tick(&mut c, (100, 100));
        let first_move = c.anchor_pos.unwrap();

        // Record expected_pos and simulate user moving the cursor.
        c.expected_pos = Some(first_move);
        c.last_wobble = Instant::now();
        tick(&mut c, (400, 400));

        // Manual mode must re-anchor to the new position and keep wobbling.
        assert_eq!(c.anchor_pos, Some((400, 400)));
        assert!(c.is_wobbling, "manual mode must stay active after user moves");
    }

    #[test]
    fn tick_real_movement_resets_idle_when_not_wobbling() {
        // Regression for the logged bug: the idle timer climbed forever while
        // not wobbling because user movement was only detected via expected_pos,
        // which is set only during a wobble. Real movement must reset idle.
        let mut settings = WobblerSettings::default();
        settings.auto_mode = true;
        settings.idle_threshold_secs = 30;
        let mut c = core_idle_for(29, settings);

        // Establish a baseline cursor position.
        tick(&mut c, (100, 100));
        // Pretend the idle timer is almost at the threshold.
        c.last_user_activity = Instant::now() - Duration::from_secs(29);

        // User physically moves the cursor far away.
        let r = tick(&mut c, (900, 700));

        assert!(
            c.last_user_activity.elapsed().as_secs() < 2,
            "idle timer must reset when the user moves the mouse"
        );
        assert!(!c.is_wobbling, "must not auto-wobble while the user is moving");
        assert_eq!(r.move_to, None);
    }

    #[test]
    fn tick_idle_accumulates_when_cursor_stays_put() {
        // Counterpart: if the cursor genuinely does not move, idle accrues and
        // auto-wobble eventually engages once the threshold is crossed.
        let mut settings = WobblerSettings::default();
        settings.auto_mode = true;
        settings.idle_threshold_secs = 5;
        let mut c = core_idle_for(4, settings);

        tick(&mut c, (100, 100)); // baseline, below threshold → no wobble
        assert!(!c.is_wobbling);

        c.last_user_activity = Instant::now() - Duration::from_secs(6);
        let r = tick(&mut c, (100, 100)); // identical position → no user movement

        assert!(c.is_wobbling, "must auto-wobble after idle threshold with no movement");
        assert!(r.move_to.is_some());
    }

    // ── AppStatus serialisation ────────────────────────────────────────────────

    #[test]
    fn app_status_serialises_all_fields() {
        let s = AppStatus {
            is_wobbling: true,
            is_manual: false,
            auto_mode: true,
            idle_seconds: 42,
            settings: WobblerSettings::default(),
            error: Some("test error".into()),
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"is_wobbling\":true"));
        assert!(json.contains("\"auto_mode\":true"));
        assert!(json.contains("\"idle_seconds\":42"));
        assert!(json.contains("\"error\":\"test error\""));
    }
}
