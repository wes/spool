//! Overlay scrollbar geometry.
//!
//! macOS draws a scrollbar as a floating thumb over the content rather than a
//! sunken track beside it, and the numbers here are that shape: a 7pt thumb
//! with a small margin off the right edge, never shorter than about a line and
//! a half however long the list is.
//!
//! Only the arithmetic lives here — where the thumb sits given a viewport, a
//! content height and a scroll offset, and the inverse for a drag. Keeping it
//! out of the element means the one part that can be wrong in a way you can
//! only see by squinting at pixels is the part that has tests.

/// Width of the strip the pointer can grab, measured from the pane's right
/// edge.
///
/// Wider than the thumb it draws, because a 7px target is a target you miss.
pub const WIDTH: f32 = 15.;

/// Width of the thumb itself.
pub const THUMB_WIDTH: f32 = 7.;

/// Gap between the thumb and the right edge of the pane.
pub const THUMB_INSET: f32 = 3.;

/// Shortest a thumb may be drawn.
///
/// Without a floor, a mailbox of forty thousand messages gets a thumb a
/// fraction of a pixel tall — proportionally honest and impossible to grab.
pub const MIN_THUMB: f32 = 24.;

/// Gap at the top and bottom of the track, so the thumb reads as floating over
/// the list rather than jammed into its corners.
pub const TRACK_INSET: f32 = 2.;

/// Where the thumb is drawn, in pixels from the top of the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    pub top: f32,
    pub height: f32,
}

/// The length the thumb travels along, viewport less the two insets.
fn track(viewport: f32) -> f32 {
    viewport - 2. * TRACK_INSET
}

/// How far the list can scroll: everything that does not fit.
fn overflow(viewport: f32, content: f32) -> f32 {
    content - viewport
}

/// Where the thumb sits for a given scroll position, or `None` when the list
/// fits and there is nothing to draw.
///
/// `offset` is the distance already scrolled, counted downward from the top —
/// the negation of what gpui's scroll handle stores.
pub fn thumb(viewport: f32, content: f32, offset: f32) -> Option<Thumb> {
    let track = track(viewport);
    let overflow = overflow(viewport, content);
    // Half a pixel of overflow is a rounding error, not a scrollable list.
    if overflow <= 0.5 || track < MIN_THUMB {
        return None;
    }

    let height = (track * viewport / content).clamp(MIN_THUMB, track);
    let travel = track - height;
    let progress = (offset / overflow).clamp(0., 1.);

    Some(Thumb { top: TRACK_INSET + travel * progress, height })
}

/// The scroll offset that puts the thumb's top at `thumb_top`.
///
/// The inverse of [`thumb`], and the whole of what dragging the thumb does:
/// the pointer moves the thumb, and the list follows from where the thumb
/// landed. Clamped, because a drag runs off both ends of the track.
pub fn offset_for(viewport: f32, content: f32, thumb_top: f32) -> f32 {
    let Some(thumb) = thumb(viewport, content, 0.) else { return 0. };
    let travel = track(viewport) - thumb.height;
    if travel <= 0. {
        return 0.;
    }

    let progress = ((thumb_top - TRACK_INSET) / travel).clamp(0., 1.);
    progress * overflow(viewport, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_that_fits_has_no_thumb() {
        assert_eq!(thumb(400., 400., 0.), None);
        assert_eq!(thumb(400., 380., 0.), None);
        // A hair of overflow is rounding, not a scrollbar.
        assert_eq!(thumb(400., 400.4, 0.), None);
    }

    #[test]
    fn a_pane_too_short_for_a_thumb_has_none() {
        assert_eq!(thumb(20., 400., 0.), None);
    }

    #[test]
    fn the_thumb_is_the_visible_fraction_of_the_list() {
        // Half the content on screen: half the track, less the insets.
        let thumb = thumb(400., 800., 0.).unwrap();
        assert!((thumb.height - track(400.) / 2.).abs() < 0.01, "{thumb:?}");
        assert_eq!(thumb.top, TRACK_INSET);
    }

    #[test]
    fn the_thumb_reaches_the_bottom_of_the_track() {
        let viewport = 400.;
        let content = 8_000.;
        let thumb = thumb(viewport, content, content - viewport).unwrap();
        assert!(
            (thumb.top + thumb.height - (viewport - TRACK_INSET)).abs() < 0.01,
            "scrolled to the end, the thumb should sit against the bottom inset: {thumb:?}",
        );
    }

    #[test]
    fn an_enormous_mailbox_still_gets_a_grabbable_thumb() {
        let thumb = thumb(400., 4_000_000., 0.).unwrap();
        assert_eq!(thumb.height, MIN_THUMB);
    }

    #[test]
    fn scrolling_past_the_ends_does_not_push_the_thumb_out_of_its_track() {
        let (viewport, content) = (400., 2_000.);
        let above = thumb(viewport, content, -500.).unwrap();
        assert_eq!(above.top, TRACK_INSET);

        let below = thumb(viewport, content, 5_000.).unwrap();
        assert!(below.top + below.height <= viewport - TRACK_INSET + 0.01, "{below:?}");
    }

    #[test]
    fn a_drag_and_the_thumb_it_moves_agree() {
        let (viewport, content) = (400., 2_000.);
        for offset in [0., 1., 250., 799., 1_600.] {
            let thumb = thumb(viewport, content, offset).unwrap();
            let round_tripped = offset_for(viewport, content, thumb.top);
            assert!(
                (round_tripped - offset).abs() < 0.01,
                "{offset} came back as {round_tripped}",
            );
        }
    }

    #[test]
    fn dragging_past_the_ends_lands_on_the_ends() {
        let (viewport, content) = (400., 2_000.);
        assert_eq!(offset_for(viewport, content, -300.), 0.);
        assert_eq!(offset_for(viewport, content, 10_000.), content - viewport);
    }

    #[test]
    fn a_list_that_fits_cannot_be_dragged_anywhere() {
        assert_eq!(offset_for(400., 400., 100.), 0.);
    }
}
