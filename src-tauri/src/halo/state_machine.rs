//! FocusHalo dwell/switch state machine (P2): the pure decision core that
//! turns a steady, slow gaze on a target window into a switch event.
//!
//! Pure by design — no clock, no window enumeration, no overlay. The engine
//! feeds one [`DwellInput`] per tick and applies the returned [`DwellPhase`]
//! to the visual; P2 only *emits* [`SwitchEvent`] (logged + debug panel), a
//! later phase consumes it for real activation.
//!
//! Rule table (product definition):
//! - `suspend` → every timer clears, back to idle (the engine renders its own
//!   `Suspended` visual; the machine contributes nothing).
//! - No window hit (blank desktop / taskbar / menu bar) → clear, idle.
//! - Hit on a foreground window → clear, idle (the user is already there).
//! - Hit on a blacklisted process or exact title (case-insensitive) → clear,
//!   idle.
//! - `switch_zone == TitleBar` → only points within the top 40 logical pixels
//!   of the window frame may become candidates.
//! - Settle: the same window hit while `speed_norm < enter_speed` for
//!   `settle_ms` → candidate; a speed spike or a window change restarts it.
//! - Candidate (dwell): window change → settle restarts; speed ≥ enter_speed
//!   ("scanned past") → clear, idle; leaving the window → idle;
//!   `progress = min(1, dwell_elapsed / dwell_ms)`.
//! - `dwell_elapsed >= dwell_ms` → one [`SwitchEvent`], then cooldown: no new
//!   candidates for `cooldown_ms` (the halo keeps following, nothing times).
//! - All timing is `now_ms` differences with saturating subtraction, so a
//!   clock glitch backwards degrades to "age 0" instead of panicking.

use crate::halo::params::{FocusHaloSettings, SwitchZone};
use crate::halo::windows_layer::WindowInfo;

/// Title-bar band height for `SwitchZone::TitleBar`, in logical pixels.
pub const TITLE_BAR_BAND_PX: f64 = 40.0;

/// One tick's observations for [`advance`]. All coordinates are logical and
/// share the window layer's space: `halo_local` is the halo center in the
/// layout board (identical to the tracked screen's local coordinates when
/// that screen sits at the board origin), matching [`WindowInfo::frame`] and
/// the window service's hit-test input.
pub struct DwellInput<'a> {
    pub now_ms: u64,
    pub halo_local: (f64, f64),
    /// Engine-computed cursor speed in normalized screen-width units/s.
    pub speed_norm: f64,
    /// Topmost window under the halo, or `None` for blank/desktop area.
    pub hit: Option<&'a WindowInfo>,
    /// Foreground window id when the hit record does not already carry
    /// `is_foreground` (the platform layer usually marks it, so this stays
    /// `None` in production).
    pub foreground_id: Option<u64>,
    /// Engine suspension (face lost / physical input): force idle.
    pub suspend: bool,
}

/// Tunables, snapped from [`FocusHaloSettings`] each tick.
#[derive(Debug, Clone, PartialEq)]
pub struct DwellConfig {
    pub dwell_ms: u64,
    pub settle_ms: u64,
    pub enter_speed: f64,
    pub cooldown_ms: u64,
    pub switch_zone: SwitchZone,
    /// Exact window titles that never become candidates (case-insensitive).
    pub title_blacklist: Vec<String>,
    /// Exact process names that never become candidates (case-insensitive).
    pub process_blacklist: Vec<String>,
}

impl DwellConfig {
    /// Build from the sanitized settings snapshot. u32 settings widen to the
    /// u64 tick domain.
    pub fn from_settings(settings: &FocusHaloSettings) -> Self {
        DwellConfig {
            dwell_ms: settings.dwell_ms as u64,
            settle_ms: settings.settle_ms as u64,
            enter_speed: settings.enter_speed,
            cooldown_ms: settings.cooldown_ms as u64,
            switch_zone: settings.switch_zone,
            title_blacklist: settings.title_blacklist.clone(),
            process_blacklist: settings.process_blacklist.clone(),
        }
    }
}

/// What the machine reports for this tick; consumed by the engine's visual
/// mapping and the debug panel. Note there is no settling variant: while
/// settle accumulates the machine is still [`DwellPhase::Idle`] (nothing to
/// render yet); the pending window is visible via [`DwellState::candidate_window`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DwellPhase {
    Idle,
    Candidate {
        window_id: u64,
        progress: f32,
    },
    /// Transient: reported exactly on the tick a switch fired, then the state
    /// is cooldown.
    Switched {
        window_id: u64,
    },
    Cooldown,
}

/// A dwell switch fired; P2 only logs/telemeters it, a later phase activates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchEvent {
    pub window_id: u64,
}

/// Internal stage. Settling is real state (it owns the pending window and its
/// timer) even though it renders as idle.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    Idle,
    Settling { window_id: u64, start_ms: u64 },
    Candidate { window_id: u64, start_ms: u64 },
    Cooldown { start_ms: u64 },
}

/// All dwell machine state; starts fully idle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DwellState {
    stage: Stage,
}

impl Default for DwellState {
    fn default() -> Self {
        DwellState { stage: Stage::Idle }
    }
}

impl DwellState {
    /// The window currently being settled or dwelled on, `None` otherwise.
    /// Exposed so the debug panel can show *why* nothing is happening yet.
    pub fn candidate_window(&self) -> Option<u64> {
        match self.stage {
            Stage::Settling { window_id, .. } | Stage::Candidate { window_id, .. } => {
                Some(window_id)
            }
            Stage::Idle | Stage::Cooldown { .. } => None,
        }
    }

    /// Internal stage name for the debug panel (`idle`/`settling`/`candidate`
    /// /`cooldown`). The transient `switched` phase comes from the tick's
    /// [`DwellPhase`], not from here.
    pub fn debug_phase_str(&self) -> &'static str {
        match self.stage {
            Stage::Idle => "idle",
            Stage::Settling { .. } => "settling",
            Stage::Candidate { .. } => "candidate",
            Stage::Cooldown { .. } => "cooldown",
        }
    }
}

/// Advance the machine one tick. Returns the phase to render and the switch
/// event when one fired this tick.
pub fn advance(
    state: &mut DwellState,
    input: &DwellInput,
    cfg: &DwellConfig,
) -> (DwellPhase, Option<SwitchEvent>) {
    // Suspension wins over everything: clear and stay idle.
    if input.suspend {
        state.stage = Stage::Idle;
        return (DwellPhase::Idle, None);
    }

    // Cooldown first: while it lasts the halo keeps following but no timing
    // may start. When it expires the normal rules apply on this same tick.
    if let Stage::Cooldown { start_ms } = state.stage {
        if input.now_ms.saturating_sub(start_ms) < cfg.cooldown_ms {
            return (DwellPhase::Cooldown, None);
        }
        state.stage = Stage::Idle;
    }

    // Blank / desktop / shell area: nothing to dwell on.
    let Some(hit) = input.hit else {
        state.stage = Stage::Idle;
        return (DwellPhase::Idle, None);
    };

    // The foreground window is where the user already works: never a target.
    if hit.is_foreground || input.foreground_id == Some(hit.id) {
        state.stage = Stage::Idle;
        return (DwellPhase::Idle, None);
    }

    // Blacklists: exact process or exact title match, case-insensitive.
    if matches_any(&hit.process, &cfg.process_blacklist)
        || matches_any(&hit.title, &cfg.title_blacklist)
    {
        state.stage = Stage::Idle;
        return (DwellPhase::Idle, None);
    }

    // TitleBar zone: the point must sit in the window's top band.
    if cfg.switch_zone == SwitchZone::TitleBar && !in_title_bar(hit, input.halo_local) {
        state.stage = Stage::Idle;
        return (DwellPhase::Idle, None);
    }

    let slow = input.speed_norm < cfg.enter_speed;

    match state.stage {
        Stage::Idle => {
            if slow {
                state.stage = Stage::Settling {
                    window_id: hit.id,
                    start_ms: input.now_ms,
                };
            }
            (DwellPhase::Idle, None)
        }
        Stage::Settling {
            window_id,
            start_ms,
        } if window_id == hit.id => {
            if !slow {
                // Scanned past while settling: restart from scratch.
                state.stage = Stage::Idle;
                return (DwellPhase::Idle, None);
            }
            if input.now_ms.saturating_sub(start_ms) >= cfg.settle_ms {
                state.stage = Stage::Candidate {
                    window_id,
                    start_ms: input.now_ms,
                };
                (
                    DwellPhase::Candidate {
                        window_id,
                        progress: 0.0,
                    },
                    None,
                )
            } else {
                (DwellPhase::Idle, None)
            }
        }
        Stage::Settling { .. } => {
            // Window changed mid-settle: the timer restarts on the new window.
            state.stage = if slow {
                Stage::Settling {
                    window_id: hit.id,
                    start_ms: input.now_ms,
                }
            } else {
                Stage::Idle
            };
            (DwellPhase::Idle, None)
        }
        Stage::Candidate {
            window_id,
            start_ms,
        } if window_id == hit.id => {
            if !slow {
                state.stage = Stage::Idle;
                return (DwellPhase::Idle, None);
            }
            let elapsed = input.now_ms.saturating_sub(start_ms);
            if elapsed >= cfg.dwell_ms {
                state.stage = Stage::Cooldown {
                    start_ms: input.now_ms,
                };
                (
                    DwellPhase::Switched { window_id },
                    Some(SwitchEvent { window_id }),
                )
            } else {
                (
                    DwellPhase::Candidate {
                        window_id,
                        progress: (elapsed as f32 / cfg.dwell_ms as f32).min(1.0),
                    },
                    None,
                )
            }
        }
        Stage::Candidate { .. } => {
            // Window changed mid-dwell: settle resets and restarts.
            state.stage = if slow {
                Stage::Settling {
                    window_id: hit.id,
                    start_ms: input.now_ms,
                }
            } else {
                Stage::Idle
            };
            (DwellPhase::Idle, None)
        }
        Stage::Cooldown { .. } => unreachable!("cooldown is handled before the hit checks"),
    }
}

/// Case-insensitive exact membership. Unicode-aware (`to_lowercase`), so
/// localized titles and process names match their configured blacklist form.
fn matches_any(value: &str, list: &[String]) -> bool {
    let lowered = value.to_lowercase();
    list.iter().any(|entry| entry.to_lowercase() == lowered)
}

/// Whether `point` lies within the top [`TITLE_BAR_BAND_PX`] logical pixels
/// of the window frame. Containment is re-checked defensively: callers normally
/// only pass points the hit-test already confirmed inside the frame.
fn in_title_bar(hit: &WindowInfo, point: (f64, f64)) -> bool {
    let (_, frame_y, _, _) = hit.frame;
    point.1 >= frame_y && point.1 - frame_y < TITLE_BAR_BAND_PX
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64) -> WindowInfo {
        WindowInfo {
            id,
            title: format!("win{id}"),
            process: "app".into(),
            frame: (0.0, 0.0, 800.0, 600.0),
            is_foreground: false,
        }
    }

    fn cfg() -> DwellConfig {
        DwellConfig {
            dwell_ms: 900,
            settle_ms: 150,
            enter_speed: 0.15,
            cooldown_ms: 500,
            switch_zone: SwitchZone::Anywhere,
            title_blacklist: vec![],
            process_blacklist: vec![],
        }
    }

    fn input<'a>(now_ms: u64, hit: Option<&'a WindowInfo>) -> DwellInput<'a> {
        DwellInput {
            now_ms,
            halo_local: (100.0, 20.0),
            speed_norm: 0.0,
            hit,
            foreground_id: None,
            suspend: false,
        }
    }

    fn progress_of(phase: &DwellPhase) -> f32 {
        match phase {
            DwellPhase::Candidate { progress, .. } => *progress,
            other => panic!("expected Candidate, got {other:?}"),
        }
    }

    #[test]
    fn full_flow_settle_dwell_switch_cooldown_recover() {
        let mut state = DwellState::default();
        let w = window(7);

        // Settling accumulates on a slow hit but still renders idle.
        let (phase, event) = advance(&mut state, &input(0, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(event, None);
        assert_eq!(state.debug_phase_str(), "settling");
        assert_eq!(state.candidate_window(), Some(7));

        let (phase, event) = advance(&mut state, &input(100, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(event, None);

        // Settle completes at 150 ms.
        let (phase, event) = advance(&mut state, &input(150, Some(&w)), &cfg());
        assert_eq!(
            phase,
            DwellPhase::Candidate {
                window_id: 7,
                progress: 0.0
            }
        );
        assert_eq!(event, None);
        assert_eq!(state.debug_phase_str(), "candidate");

        // Dwell progresses proportionally.
        let (phase, _) = advance(&mut state, &input(600, Some(&w)), &cfg());
        assert!((progress_of(&phase) - 450.0 / 900.0).abs() < 1e-6);

        // Dwell completes at start (150) + dwell (900) = 1050: one event.
        let (phase, event) = advance(&mut state, &input(1050, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Switched { window_id: 7 });
        assert_eq!(event, Some(SwitchEvent { window_id: 7 }));
        assert_eq!(state.debug_phase_str(), "cooldown");

        // Exactly once: the next tick is cooldown with no event.
        let (phase, event) = advance(&mut state, &input(1060, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Cooldown);
        assert_eq!(event, None);

        // Still suppressed inside the cooldown window (until 1550).
        let (phase, event) = advance(&mut state, &input(1400, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Cooldown);
        assert_eq!(event, None);

        // Cooldown expired at 1600: back to settling (idle phase), and a new
        // candidate only after another settle.
        let (phase, event) = advance(&mut state, &input(1600, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(event, None);
        let (phase, _) = advance(&mut state, &input(1760, Some(&w)), &cfg());
        assert_eq!(
            phase,
            DwellPhase::Candidate {
                window_id: 7,
                progress: 0.0
            }
        );
    }

    #[test]
    fn fast_motion_never_builds_timing() {
        let mut state = DwellState::default();
        let w = window(1);
        let mut fast = input(0, Some(&w));
        fast.speed_norm = cfg().enter_speed; // >= threshold counts as fast
        for now in [0u64, 100, 10_000] {
            fast.now_ms = now;
            let (phase, event) = advance(&mut state, &fast, &cfg());
            assert_eq!(phase, DwellPhase::Idle, "tick {now}");
            assert_eq!(event, None);
            assert_eq!(state.debug_phase_str(), "idle");
        }
        // Just below the threshold settles normally.
        let mut slow = input(20_000, Some(&w));
        slow.speed_norm = 0.149;
        let (phase, _) = advance(&mut state, &slow, &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        slow.now_ms = 20_150;
        let (phase, _) = advance(&mut state, &slow, &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));
    }

    #[test]
    fn window_change_resets_settle_and_dwell() {
        let mut state = DwellState::default();
        let a = window(1);
        let b = window(2);

        // Halfway through settling on A, the target switches to B: the timer
        // restarts, so 150 ms after the *first* touch is not enough.
        advance(&mut state, &input(0, Some(&a)), &cfg());
        let (phase, _) = advance(&mut state, &input(100, Some(&b)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(state.candidate_window(), Some(2), "settling restarts on B");
        let (phase, _) = advance(&mut state, &input(149, Some(&b)), &cfg());
        assert_eq!(phase, DwellPhase::Idle, "old A timer must not leak");
        let (phase, _) = advance(&mut state, &input(250, Some(&b)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 2, .. }));

        // Same for dwell: switching windows mid-dwell drops back to settling,
        // restarting the full chain (no carried-over dwell timer).
        let (phase, _) = advance(&mut state, &input(500, Some(&b)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 2, .. }));
        let (phase, _) = advance(&mut state, &input(600, Some(&a)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(state.debug_phase_str(), "settling");
        // Settle restarts at 600, completes at 750, dwell from 750: a switch
        // fires at 1650, while a carried-over B dwell would have fired 1150.
        let (phase, _) = advance(&mut state, &input(750, Some(&a)), &cfg());
        assert_eq!(
            phase,
            DwellPhase::Candidate {
                window_id: 1,
                progress: 0.0
            }
        );
        let (_, event) = advance(&mut state, &input(1150, Some(&a)), &cfg());
        assert_eq!(event, None, "carried-over dwell would have fired here");
        let (_, event) = advance(&mut state, &input(1650, Some(&a)), &cfg());
        assert_eq!(event, Some(SwitchEvent { window_id: 1 }));
    }

    #[test]
    fn leaving_the_window_clears_all_timing() {
        let mut state = DwellState::default();
        let w = window(3);

        // Mid-settle, the halo drifts off the window: timers clear, so when it
        // comes back the settle restarts from that moment, not the old start.
        advance(&mut state, &input(0, Some(&w)), &cfg());
        let (phase, _) = advance(&mut state, &input(100, None), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(state.candidate_window(), None);
        let (phase, _) = advance(&mut state, &input(140, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle, "old timer must be gone");
        let (phase, _) = advance(&mut state, &input(290, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));

        // Same check mid-dwell.
        advance(&mut state, &input(1200, None), &cfg());
        let (phase, _) = advance(&mut state, &input(1300, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(1449, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(1450, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));
    }

    #[test]
    fn foreground_windows_never_become_candidates() {
        let mut state = DwellState::default();
        let mut fg = window(4);
        fg.is_foreground = true;
        for now in [0u64, 100, 1000] {
            let (phase, event) = advance(&mut state, &input(now, Some(&fg)), &cfg());
            assert_eq!(phase, DwellPhase::Idle);
            assert_eq!(event, None);
        }

        // The explicit foreground id is honored even when the platform mark
        // is missing.
        let mut plain = window(5);
        plain.is_foreground = false;
        let mut flagged = input(0, Some(&plain));
        flagged.foreground_id = Some(5);
        for now in [0u64, 100, 1000] {
            flagged.now_ms = now;
            let (phase, _) = advance(&mut state, &flagged, &cfg());
            assert_eq!(phase, DwellPhase::Idle);
        }

        // A different foreground id does not affect an unrelated window.
        let mut ok = input(0, Some(&plain));
        ok.foreground_id = Some(99);
        let (phase, _) = advance(&mut state, &ok, &cfg());
        assert_eq!(phase, DwellPhase::Idle, "first tick only settles");
        let mut ok2 = ok;
        ok2.now_ms = 150;
        let (phase, _) = advance(&mut state, &ok2, &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 5, .. }));
    }

    #[test]
    fn blacklisted_process_and_title_are_skipped_case_insensitively() {
        let mut state = DwellState::default();
        let mut by_process = window(6);
        by_process.process = "Finder".into();
        let mut by_title = window(8);
        by_title.title = "Settings".into();

        let mut black_cfg = cfg();
        black_cfg.process_blacklist = vec!["finder".into()];
        black_cfg.title_blacklist = vec!["SETTINGS".into()];

        for hit in [&by_process, &by_title] {
            for now in [0u64, 100, 1000] {
                let (phase, event) = advance(&mut state, &input(now, Some(hit)), &black_cfg);
                assert_eq!(phase, DwellPhase::Idle, "{hit:?} tick {now}");
                assert_eq!(event, None);
            }
        }

        // Partial matches do NOT blacklist: only exact (ignoring case).
        let mut partial = window(9);
        partial.title = "Settings — General".into();
        partial.process = "finder-helper".into();
        let (phase, _) = advance(&mut state, &input(0, Some(&partial)), &black_cfg);
        assert_eq!(phase, DwellPhase::Idle);
        let mut later = input(150, Some(&partial));
        later.speed_norm = 0.0;
        let (phase, _) = advance(&mut state, &later, &black_cfg);
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 9, .. }));
    }

    #[test]
    fn titlebar_zone_requires_the_top_band() {
        let mut state = DwellState::default();
        let w = window(10); // frame (0, 0, 800, 600)
        let mut zone_cfg = cfg();
        zone_cfg.switch_zone = SwitchZone::TitleBar;

        // Inside the band: settles and dwells normally.
        let mut hit = input(0, Some(&w));
        hit.halo_local = (400.0, 39.0);
        let (phase, _) = advance(&mut state, &hit, &zone_cfg);
        assert_eq!(phase, DwellPhase::Idle);
        let mut hit2 = hit;
        hit2.now_ms = 150;
        let (phase, _) = advance(&mut state, &hit2, &zone_cfg);
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 10, .. }));

        // Below the band: never settles.
        let mut low = input(1000, Some(&w));
        low.halo_local = (400.0, 40.0); // exactly at the band edge: out
        let (phase, _) = advance(&mut state, &low, &zone_cfg);
        assert_eq!(phase, DwellPhase::Idle);
        low.now_ms = 2000;
        let (phase, _) = advance(&mut state, &low, &zone_cfg);
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(state.debug_phase_str(), "idle");

        // Anywhere ignores the band entirely.
        let mut any_cfg = cfg();
        any_cfg.switch_zone = SwitchZone::Anywhere;
        let mut deep = input(3000, Some(&w));
        deep.halo_local = (400.0, 500.0);
        let (phase, _) = advance(&mut state, &deep, &any_cfg);
        assert_eq!(phase, DwellPhase::Idle);
        let mut deep2 = deep;
        deep2.now_ms = 3150;
        let (phase, _) = advance(&mut state, &deep2, &any_cfg);
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 10, .. }));
    }

    #[test]
    fn cooldown_suppresses_new_candidates_until_expired() {
        let mut state = DwellState::default();
        let w = window(11);
        // Reach cooldown: settle (150) + dwell (900) done by 1050.
        advance(&mut state, &input(0, Some(&w)), &cfg());
        advance(&mut state, &input(150, Some(&w)), &cfg());
        let (_, event) = advance(&mut state, &input(1050, Some(&w)), &cfg());
        assert!(event.is_some());

        // During cooldown a slow, steady hit on another window does nothing.
        let other = window(12);
        for now in [1100u64, 1300, 1549] {
            let (phase, event) = advance(&mut state, &input(now, Some(&other)), &cfg());
            assert_eq!(phase, DwellPhase::Cooldown, "tick {now}");
            assert_eq!(event, None);
            assert_eq!(state.candidate_window(), None);
        }
        // At 1550 the cooldown expires and the same steady hit re-settles.
        let (phase, event) = advance(&mut state, &input(1550, Some(&other)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(event, None);
        assert_eq!(state.debug_phase_str(), "settling");
    }

    #[test]
    fn overspeed_during_settle_restarts_the_timer() {
        let mut state = DwellState::default();
        let w = window(2);
        advance(&mut state, &input(0, Some(&w)), &cfg());

        // Speed spike at 100 resets; coming back slow restarts from 200.
        let mut fast = input(100, Some(&w));
        fast.speed_norm = 1.0;
        let (phase, _) = advance(&mut state, &fast, &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(state.debug_phase_str(), "idle");

        let (phase, _) = advance(&mut state, &input(200, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        // With the stale timer this tick would already be a candidate.
        let (phase, _) = advance(&mut state, &input(240, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle, "old settle timer must not leak");
        let (phase, _) = advance(&mut state, &input(350, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { window_id: 2, .. }));
    }

    #[test]
    fn overspeed_during_dwell_clears_to_idle() {
        let mut state = DwellState::default();
        let w = window(13);
        advance(&mut state, &input(0, Some(&w)), &cfg());
        advance(&mut state, &input(150, Some(&w)), &cfg());
        // Halfway through the dwell, the user scans past: clear, and the same
        // slow window needs a full settle + dwell again (no partial carry).
        let mut fast = input(600, Some(&w));
        fast.speed_norm = 0.9;
        let (phase, event) = advance(&mut state, &fast, &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(event, None);
        let (phase, _) = advance(&mut state, &input(1000, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(1150, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));
    }

    #[test]
    fn clock_rollback_does_not_panic_and_keeps_timing_sane() {
        let mut state = DwellState::default();
        let w = window(3);

        // Settle starts at 10_000, then the clock jumps backwards: saturating
        // subtraction yields age 0, no panic, no premature completion.
        advance(&mut state, &input(10_000, Some(&w)), &cfg());
        let (phase, _) = advance(&mut state, &input(9_000, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(9_050, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(10_150, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));

        // Backwards time inside the dwell saturates to progress 0, never a
        // spurious switch.
        let (phase, event) = advance(&mut state, &input(10_100, Some(&w)), &cfg());
        assert_eq!(event, None);
        assert_eq!(progress_of(&phase), 0.0);
        let (phase, _) = advance(&mut state, &input(10_900, Some(&w)), &cfg());
        assert!((progress_of(&phase) - 750.0 / 900.0).abs() < 1e-6);
        let (phase, event) = advance(&mut state, &input(11_050, Some(&w)), &cfg());
        assert_eq!(event, Some(SwitchEvent { window_id: 3 }));
        assert_eq!(phase, DwellPhase::Switched { window_id: 3 });
    }

    #[test]
    fn suspend_clears_everything_immediately() {
        let mut state = DwellState::default();
        let w = window(14);
        // Mid-dwell, the engine suspends (face lost / physical input).
        advance(&mut state, &input(0, Some(&w)), &cfg());
        advance(&mut state, &input(150, Some(&w)), &cfg());
        let mut suspended = input(300, Some(&w));
        suspended.suspend = true;
        let (phase, event) = advance(&mut state, &suspended, &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        assert_eq!(event, None);
        assert_eq!(state.candidate_window(), None);
        // Recovery restarts from scratch, not from the pre-suspend progress.
        let (phase, _) = advance(&mut state, &input(400, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(500, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle);
        let (phase, _) = advance(&mut state, &input(550, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));
    }

    #[test]
    fn suspend_during_cooldown_ends_it() {
        let mut state = DwellState::default();
        let w = window(15);
        advance(&mut state, &input(0, Some(&w)), &cfg());
        advance(&mut state, &input(150, Some(&w)), &cfg());
        let (_, event) = advance(&mut state, &input(1050, Some(&w)), &cfg());
        assert!(event.is_some());
        let mut suspended = input(1100, Some(&w));
        suspended.suspend = true;
        let (phase, _) = advance(&mut state, &suspended, &cfg());
        assert_eq!(phase, DwellPhase::Idle, "suspend also clears cooldown");
        // And timing may start again immediately (no residual cooldown): the
        // next tick starts a fresh settle, completing 150 ms later.
        let (phase, _) = advance(&mut state, &input(1250, Some(&w)), &cfg());
        assert_eq!(phase, DwellPhase::Idle, "fresh settle starts at once");
        let (phase, _) = advance(&mut state, &input(1400, Some(&w)), &cfg());
        assert!(matches!(phase, DwellPhase::Candidate { .. }));
    }

    #[test]
    fn progress_is_clamped_and_monotonic_within_a_dwell() {
        let mut state = DwellState::default();
        let w = window(16);
        advance(&mut state, &input(0, Some(&w)), &cfg());
        let (phase, _) = advance(&mut state, &input(150, Some(&w)), &cfg());
        assert_eq!(progress_of(&phase), 0.0);

        let mut last = 0.0;
        for now in [200u64, 400, 600, 800, 1000, 1049] {
            let (phase, event) = advance(&mut state, &input(now, Some(&w)), &cfg());
            assert_eq!(event, None, "switch must fire only at >= dwell_ms");
            let progress = progress_of(&phase);
            assert!(progress >= last, "progress regressed at tick {now}");
            assert!((0.0..=1.0).contains(&progress));
            last = progress;
        }
    }

    #[test]
    fn from_settings_snapshots_the_sanitized_fields() {
        let settings = FocusHaloSettings {
            dwell_ms: 1200,
            settle_ms: 200,
            enter_speed: 0.2,
            cooldown_ms: 700,
            switch_zone: SwitchZone::TitleBar,
            title_blacklist: vec!["Settings".into()],
            process_blacklist: vec!["finder".into()],
            ..FocusHaloSettings::default()
        };
        let cfg = DwellConfig::from_settings(&settings);
        assert_eq!(cfg.dwell_ms, 1200);
        assert_eq!(cfg.settle_ms, 200);
        assert_eq!(cfg.enter_speed, 0.2);
        assert_eq!(cfg.cooldown_ms, 700);
        assert_eq!(cfg.switch_zone, SwitchZone::TitleBar);
        assert_eq!(cfg.title_blacklist, vec!["Settings".to_string()]);
        assert_eq!(cfg.process_blacklist, vec!["finder".to_string()]);
    }

    #[test]
    fn constants_match_design() {
        assert_eq!(TITLE_BAR_BAND_PX, 40.0);
    }
}
