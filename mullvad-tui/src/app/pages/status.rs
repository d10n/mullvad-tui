// SPDX-License-Identifier: GPL-3.0-or-later

//! Status-page transient UI state.
//!
//! - `connection_details_expanded`: the `[Hide]`/`[Expand]` toggle for the WireGuard / In / Out
//!   detail block.
//! - `camera_anim`: in-flight animation between successive globe-camera targets. The renderer asks
//!   for the current camera each frame and the run-loop advances the target when state changes.
//!
//! The camera state lives here (rather than in `tui::pages::status`)
//! so the renderer can stay stateless and the run loop can drive the
//! animation deterministically. Conversion to `tui_globe::Camera`
//! happens at render time.

use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct PageState {
    pub connection_details_expanded: bool,
    pub camera_anim: CameraAnimation,
}

/// Renderer-agnostic camera state. Mirrors `tui_globe::Camera` but
/// without the dependency - keeping `app::pages::*` free of UI-crate
/// imports lets the renderer side own the conversion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraState {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
}

impl Default for CameraState {
    fn default() -> Self {
        // Whole-globe view, Greenwich/equator. Matches `tui_globe::Camera`'s
        // default so a fresh animation starts from the same orientation
        // the renderer would draw without any state.
        Self {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
        }
    }
}

/// One second per transition. Long enough to read as motion (rather
/// than a snap), short enough that the user isn't waiting on the
/// camera before the new state settles.
const ANIMATION_DURATION: Duration = Duration::from_secs(1);

/// Tolerance below which two camera states are treated as equal -
/// avoids restarting the animation from rounding noise on every frame
/// when the daemon's reported lat/lon jitters by a microdegree.
const APPROX_EPSILON: f32 = 1e-3;

/// Target zoom at the midpoint of every camera transition. Pulling
/// the zoom out toward this value mid-flight gives a "fly out,
/// traverse, fly back in" feel - when both endpoints are higher zooms
/// (e.g. Hostname -> Hostname at 50.0 each) the camera dips down to
/// take in more globe before swinging onto the new location.
const ZOOM_MIDPOINT: f32 = 10.0;

/// Grace window past [`ANIMATION_DURATION`] during which
/// [`CameraAnimation::is_active`] still returns `true`. Sized to a
/// comfortable multiple of the run-loop ticker's 33 ms cadence so
/// even a slow draw call can't squeeze out the final tick.
const SETTLE_GRACE: Duration = Duration::from_millis(100);

/// Tracks an in-flight camera transition. The fields describe a
/// linear segment from `from` to `to` over [`ANIMATION_DURATION`]
/// starting at `started_at`; [`Self::current`] computes the
/// interpolated point at any time.
#[derive(Debug)]
pub struct CameraAnimation {
    from: CameraState,
    to: CameraState,
    started_at: Instant,
}

impl Default for CameraAnimation {
    fn default() -> Self {
        // Initialize with the animation already "completed" (started
        // long enough ago that the first `current` call returns `to`),
        // so the first `set_target` triggers a clean transition from
        // the default view rather than mid-animation noise.
        Self {
            from: CameraState::default(),
            to: CameraState::default(),
            started_at: Instant::now()
                .checked_sub(ANIMATION_DURATION)
                .unwrap_or_else(Instant::now),
        }
    }
}

impl CameraAnimation {
    /// Update the target. If the new target matches the current `to`
    /// (within [`APPROX_EPSILON`]), this is a no-op - the animation
    /// keeps progressing toward the same destination. Otherwise the
    /// animation restarts from the currently-displayed camera.
    pub fn set_target(&mut self, target: CameraState, now: Instant) {
        if approx_equal(self.to, target) {
            return;
        }
        let current = self.current(now);
        self.from = current;
        self.to = target;
        self.started_at = now;
    }

    /// The currently-displayed camera. Lerps from `from` to `to` over
    /// [`ANIMATION_DURATION`] using a smoothstep easing so the camera
    /// settles in instead of stopping abruptly.
    pub fn current(&self, now: Instant) -> CameraState {
        let elapsed = now.saturating_duration_since(self.started_at);
        let t = (elapsed.as_secs_f32() / ANIMATION_DURATION.as_secs_f32()).clamp(0.0, 1.0);
        let t = smoothstep(t);
        lerp_camera(self.from, self.to, t)
    }

    /// True while the run loop should keep the animation ticker
    /// firing. Strictly larger than the animation's logical duration:
    /// we extend the active window by [`SETTLE_GRACE`] so the loop
    /// is guaranteed to render at least one frame *after* `t = 1.0`,
    /// where [`Self::current`] clamps to `to`.
    ///
    /// Without that grace, a slow iteration body (the globe
    /// rasteriser plus a normal `terminal.draw`) crossing the
    /// `started_at + ANIMATION_DURATION` boundary between the last
    /// in-window tick and the next `select!` re-entry can flip
    /// `is_active` false before the would-be final tick fires. The
    /// camera then visibly freezes at ~99% of the way to its target
    /// until *any* other event (key, daemon push, log entry) wakes
    /// the loop.
    pub fn is_active(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) < ANIMATION_DURATION + SETTLE_GRACE
    }
}

fn smoothstep(t: f32) -> f32 {
    // Classic cubic Hermite (3t*t - 2t*t*t). Zero derivative at both
    // endpoints, monotonic in between - gentle ease-in/ease-out.
    t * t * (3.0 - 2.0 * t)
}

fn lerp_camera(from: CameraState, to: CameraState, t: f32) -> CameraState {
    CameraState {
        yaw: lerp_angle(from.yaw, to.yaw, t),
        pitch: lerp(from.pitch, to.pitch, t),
        zoom: lerp_zoom_through_midpoint(from.zoom, to.zoom, t),
    }
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// Quadratic interpolation that hits `from` at `t=0`, [`ZOOM_MIDPOINT`]
/// at `t=0.5`, and `to` at `t=1`. Built as a Lagrange basis on the
/// three sample points; no `if` branches, no derivatives to track.
///
/// Why not linear: when both endpoints share roughly the same zoom
/// level (e.g. swapping between two specific hostnames at 50.0), a
/// linear lerp would hold the zoom flat and the user just sees the
/// globe rotate at full crop. Routing through a wider midpoint shows
/// off the journey - the camera pulls back, traverses, and zooms
/// back in.
fn lerp_zoom_through_midpoint(from: f32, to: f32, t: f32) -> f32 {
    from * (1.0 - t) * (1.0 - 2.0 * t)
        + ZOOM_MIDPOINT * 4.0 * t * (1.0 - t)
        + to * t * (2.0 * t - 1.0)
}

/// Linear interpolation along the *shorter* arc on the circle, so a
/// jump from 170deg to -170deg rotates 20deg across the antimeridian instead
/// of 340deg the other way.
fn lerp_angle(from: f32, to: f32, t: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut delta = (to - from) % TAU;
    if delta > PI {
        delta -= TAU;
    } else if delta < -PI {
        delta += TAU;
    }
    from + delta * t
}

fn approx_equal(a: CameraState, b: CameraState) -> bool {
    (a.yaw - b.yaw).abs() < APPROX_EPSILON
        && (a.pitch - b.pitch).abs() < APPROX_EPSILON
        && (a.zoom - b.zoom).abs() < APPROX_EPSILON
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lerp_angle_takes_the_short_way_across_pi() {
        // Going from 170deg to -170deg via the +180deg antimeridian: 20deg
        // of motion, not 340deg.
        use std::f32::consts::PI;
        let from = 170.0_f32.to_radians();
        let to = (-170.0_f32).to_radians();
        let mid = lerp_angle(from, to, 0.5);
        // Halfway should be ~180deg (PI), within rounding.
        assert!(
            (mid.abs() - PI).abs() < 1e-4,
            "midpoint = {mid} rad (expected ~pi)"
        );
    }

    #[test]
    fn smoothstep_endpoints_and_midpoint() {
        assert!((smoothstep(0.0)).abs() < 1e-6);
        assert!((smoothstep(1.0) - 1.0).abs() < 1e-6);
        // Smoothstep at 0.5 is exactly 0.5.
        assert!((smoothstep(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn animation_completes_at_target_after_duration() {
        let mut anim = CameraAnimation::default();
        let start = Instant::now();
        let target = CameraState {
            yaw: 1.0,
            pitch: 0.5,
            zoom: 50.0,
        };
        anim.set_target(target, start);
        // Halfway through, neither at start nor at target.
        let half = anim.current(start + ANIMATION_DURATION / 2);
        assert!(half.yaw > 0.0 && half.yaw < target.yaw);
        // Past the duration, exactly at target.
        let after = anim.current(start + ANIMATION_DURATION + Duration::from_millis(50));
        assert!((after.yaw - target.yaw).abs() < 1e-5);
        assert!((after.pitch - target.pitch).abs() < 1e-5);
        assert!((after.zoom - target.zoom).abs() < 1e-5);
    }

    #[test]
    fn zoom_passes_through_midpoint_at_t_half() {
        // Hostname-to-Hostname-style transition: both endpoints high,
        // midpoint should dip down to ZOOM_MIDPOINT (25).
        let mut anim = CameraAnimation::default();
        let start = Instant::now();
        // Set an initial target so the animation starts settled at zoom=50.
        let primed = CameraState {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 50.0,
        };
        anim.set_target(primed, start - ANIMATION_DURATION * 2);

        // Now retarget to a different yaw, same zoom - we're animating
        // from zoom=50 to zoom=50.
        let new_target = CameraState {
            yaw: 1.0,
            pitch: 0.0,
            zoom: 50.0,
        };
        anim.set_target(new_target, start);
        let mid = anim.current(start + ANIMATION_DURATION / 2);
        assert!(
            (mid.zoom - ZOOM_MIDPOINT).abs() < 1e-3,
            "midpoint zoom = {} (expected ~ {ZOOM_MIDPOINT})",
            mid.zoom
        );
    }

    #[test]
    fn changing_target_mid_flight_restarts_from_current() {
        let mut anim = CameraAnimation::default();
        let t0 = Instant::now();
        let target_a = CameraState {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 50.0,
        };
        anim.set_target(target_a, t0);
        let mid = anim.current(t0 + ANIMATION_DURATION / 2);

        // Retarget mid-flight; the new `from` should equal the mid-flight value.
        let target_b = CameraState {
            yaw: 1.0,
            pitch: 0.5,
            zoom: 25.0,
        };
        let t1 = t0 + ANIMATION_DURATION / 2;
        anim.set_target(target_b, t1);
        // Immediately querying at t1 returns `from` (smoothstep(0) = 0).
        let at_restart = anim.current(t1);
        assert!((at_restart.zoom - mid.zoom).abs() < 1e-4);
    }

    #[test]
    fn no_op_set_target_when_within_epsilon() {
        let mut anim = CameraAnimation::default();
        let t0 = Instant::now();
        let target = CameraState {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 25.0,
        };
        anim.set_target(target, t0);
        let original_started = anim.started_at;
        // A jitter under epsilon shouldn't restart the animation.
        let target_jitter = CameraState {
            yaw: APPROX_EPSILON / 2.0,
            pitch: 0.0,
            zoom: 25.0,
        };
        anim.set_target(target_jitter, t0 + Duration::from_millis(100));
        assert_eq!(anim.started_at, original_started);
    }

    #[test]
    fn is_active_extends_past_duration_via_settle_grace() {
        // Repro for the cold-start "stuck at ~99%" bug: a slow loop
        // iteration crossing the ANIMATION_DURATION boundary between
        // the last in-window tick and the next `select!` re-entry
        // would flip `is_active` false before the final tick could
        // fire. The grace window keeps the gate open long enough for
        // the run loop to render at least one frame at t=1.0.
        let mut anim = CameraAnimation::default();
        let t0 = Instant::now();
        let target = CameraState {
            yaw: 1.0,
            pitch: 0.0,
            zoom: 25.0,
        };
        anim.set_target(target, t0);

        // Active throughout the animation proper.
        assert!(anim.is_active(t0));
        assert!(anim.is_active(t0 + ANIMATION_DURATION / 2));
        // Active at the boundary (would have been false before the fix).
        assert!(anim.is_active(t0 + ANIMATION_DURATION));
        // Active partway into the grace window.
        assert!(anim.is_active(t0 + ANIMATION_DURATION + Duration::from_millis(50)));
        // Inactive once the grace expires.
        assert!(!anim.is_active(t0 + ANIMATION_DURATION + Duration::from_millis(200)));
    }
}
