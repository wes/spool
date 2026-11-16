//! The way a message leaves: the composer window flies off the top of the
//! screen the instant Send is pressed.
//!
//! Sending used to be something you watched. The window sat there saying
//! "Sending…" for as long as the server took, and then vanished — so the one
//! moment worth feeling instant was the one that made you wait, and a slow
//! server made it look like the app had stalled rather than like the mail was
//! on its way.
//!
//! Now the press is the send: the draft is handed to the mailbox window, which
//! owns the delivery from there, and the composer takes off. Whether the
//! server has accepted it yet is news the status line brings later.
//!
//! The shape of it is a coil and a launch: the window sinks a fraction, then
//! leaves straight up the screen fast enough that most of the distance goes by
//! in the last few frames.
//!
//! The animation moves the real `NSWindow` rather than anything gpui draws.
//! A window cannot animate its own contents past its own edges, and a
//! composer sliding *inside* its frame would read as a bug; moving the window
//! itself is what makes it leave the screen. Each frame is set by hand rather
//! than handed to `NSAnimationContext`, because the point is the shape of the
//! motion — slow for a beat, then gone — and that is an easing curve we have
//! to own.

use std::time::Duration;

/// How long the whole flight takes: a coil and a launch, and both of them
/// over before anyone would call it an animation.
///
/// The send itself started before the first frame, so this is never a wait —
/// but it is still time the window is on screen after you have finished with
/// it, and the whole feeling being aimed at is *gone already*.
pub const DURATION: Duration = Duration::from_millis(220);

/// How often a frame is set. 120 Hz displays get every one of these; 60 Hz
/// displays drop half and the curve looks the same.
pub const FRAME: Duration = Duration::from_millis(8);

/// The share of the flight spent winding up rather than travelling.
const COIL: f32 = 0.3;

/// How far the window sinks while it coils, as a share of the distance it is
/// about to cover. Small on purpose: this reads as a crouch, and a window
/// that visibly *falls* first reads as a bug.
const CROUCH: f32 = 0.022;

/// Where the flight is at `t`, from 0 at the press to 1 when it is gone.
///
/// Two movements. For the first fraction the window sinks slightly and comes
/// back — the coil — and then it goes, on a curve steep enough that most of
/// the distance is covered in the last handful of frames. The wind-up is what
/// makes the launch read as force rather than as a slide: the eye needs a
/// moment of stillness to measure the speed against.
///
/// The result is negative during the coil, which is the point: the window is
/// below where it started.
pub fn eased(t: f32) -> f32 {
    let t = t.clamp(0., 1.);

    if t < COIL {
        // A half-turn of a sine: down and back to exactly where it began, with
        // no corner at either end of it.
        let turn = t / COIL * std::f32::consts::PI;
        return -CROUCH * turn.sin();
    }

    // Quartic: at the halfway point of the launch it has covered a sixteenth
    // of the distance, and the rest happens in the time it takes to notice.
    let u = (t - COIL) / (1. - COIL);
    u * u * u * u
}

/// How opaque the window is at `t`.
///
/// Solid through the coil and the first of the launch, then gone quickly. The
/// eye has to see it *move* before it sees it fade — a window that starts
/// dissolving on the press looks dismissed, not launched.
pub fn opacity(t: f32) -> f32 {
    const HOLD: f32 = 0.62;
    let t = t.clamp(0., 1.);
    match t < HOLD {
        true => 1.,
        false => 1. - (t - HOLD) / (1. - HOLD),
    }
}

#[cfg(target_os = "macos")]
pub use mac::Flight;

#[cfg(target_os = "macos")]
mod mac {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindow};
    use objc2_foundation::NSPoint;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// How far past the top edge to travel, so the window is fully gone
    /// before the last frame rather than clipped at it.
    const CLEARANCE: f64 = 80.;

    /// One composer window in flight.
    pub struct Flight {
        window: Retained<NSWindow>,
        from: NSPoint,
        to: NSPoint,
    }

    impl Flight {
        /// Reads the window's starting place and works out where it is going.
        ///
        /// `None` when there is no window to move — a headless test, or a
        /// platform that is not this one — and the caller then simply closes,
        /// which is the behaviour this replaced.
        pub fn ready(window: &gpui::Window) -> Option<Flight> {
            let ns_window = ns_window(window)?;
            let frame = ns_window.frame();
            let from = frame.origin;

            // Off the top of whichever screen this window is on, not the main
            // one: a composer on a second display has to leave that display.
            let top = ns_window
                .screen()
                .map(|screen| {
                    let visible = screen.frame();
                    visible.origin.y + visible.size.height
                })
                .unwrap_or(from.y + frame.size.height + 1_000.);

            // Straight up: the line is the shot.
            let to = NSPoint::new(from.x, top + CLEARANCE);
            Some(Flight { window: ns_window, from, to })
        }

        /// Puts the window where it should be at `t`, from 0 to 1.
        pub fn step(&self, t: f32) {
            let eased = f64::from(super::eased(t));
            let at = NSPoint::new(
                self.from.x + (self.to.x - self.from.x) * eased,
                self.from.y + (self.to.y - self.from.y) * eased,
            );

            self.window.setFrameOrigin(at);
            self.window.setAlphaValue(f64::from(super::opacity(t)));
        }
    }

    /// The `NSWindow` behind a gpui window.
    ///
    /// Reached through the content view, which is the only handle gpui hands
    /// out — the same route `webview` takes to hang a native subview.
    fn ns_window(window: &gpui::Window) -> Option<Retained<NSWindow>> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };

        // SAFETY: gpui hands out a pointer to the window's own NSView, which
        // lives as long as the window does, and this is called on the main
        // thread from the window's own update.
        let view: &NSView = unsafe { &*(handle.ns_view.as_ptr() as *const NSView) };
        view.window()
    }
}

/// Everywhere else there is no window to fly: the caller closes instead.
#[cfg(not(target_os = "macos"))]
pub struct Flight;

#[cfg(not(target_os = "macos"))]
impl Flight {
    pub fn ready(_: &gpui::Window) -> Option<Flight> {
        None
    }

    pub fn step(&self, _: f32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flight_starts_where_the_window_is_and_ends_gone() {
        assert_eq!(eased(0.), 0.);
        assert_eq!(eased(1.), 1.);
        assert_eq!(opacity(0.), 1.);
        assert_eq!(opacity(1.), 0.);
    }

    #[test]
    fn it_coils_before_it_goes() {
        // Below where it started, for the whole wind-up and no longer.
        let middle_of_the_coil = eased(COIL / 2.);
        assert!(middle_of_the_coil < 0., "{middle_of_the_coil}");
        assert!(middle_of_the_coil.abs() < CROUCH * 1.01, "a crouch, not a fall");

        // And back to exactly the starting place as the launch begins, so
        // there is no jump between the two movements.
        assert!(eased(COIL).abs() < 0.001, "{}", eased(COIL));
    }

    #[test]
    fn nearly_all_of_the_distance_is_covered_at_the_very_end() {
        // What makes it read as a launch rather than a slide: halfway through
        // the launch it has barely left, and then it is gone.
        let half_way_through_the_launch = eased(COIL + (1. - COIL) / 2.);
        assert!(half_way_through_the_launch < 0.1, "{half_way_through_the_launch}");
        assert!(eased(0.95) > 0.6, "{}", eased(0.95));
    }

    #[test]
    fn nothing_is_ever_asked_for_outside_the_flight() {
        // `t` comes from a timer against a deadline, and a slow frame can put
        // it past the end.
        assert_eq!(eased(1.4), 1.);
        assert_eq!(opacity(1.4), 0.);
        assert_eq!(eased(-0.2), 0.);
        assert_eq!(opacity(-0.2), 1.);
    }

    #[test]
    fn the_window_is_solid_while_it_is_still_visibly_moving() {
        assert_eq!(opacity(COIL), 1., "solid through the coil");
        assert_eq!(opacity(0.5), 1., "and into the launch");
        assert!(opacity(0.8) < 1. && opacity(0.8) > 0., "fading as it leaves");
    }

    #[test]
    fn the_whole_thing_is_over_before_anyone_could_call_it_a_wait() {
        assert!(DURATION <= Duration::from_millis(260));
        assert!(FRAME.as_millis() * 8 < DURATION.as_millis(), "enough frames to be motion");
    }
}
