//! The blinking caret, shared by the single-line field and the body area.
//!
//! A caret that never blinks is a thin static line the eye stops finding. In a
//! field it is merely fiddly; in a message body it can be on any of forty
//! wrapped rows, and the only reliable way to learn where the next character
//! will land is to type one and look. Blinking is what makes it findable, and
//! it is what every other field on the machine does.
//!
//! The second rule is the one that is easy to get wrong: the caret goes solid
//! the moment you touch it. A blink that keeps its own rhythm through a burst
//! of typing spends half of that burst invisible, which is worse than not
//! blinking at all.

use std::time::Duration;

use gpui::{Context, Pixels, Task, px};

/// How wide the caret is drawn.
///
/// Two rather than the one and a half it was: at this size the difference
/// between a hairline and a mark is most of what makes it findable, and two
/// is what the rest of the machine draws.
pub const CARET_WIDTH: Pixels = px(2.);

/// Half a cycle: on for this long, then off for this long.
///
/// macOS's own rate. There is nothing to be gained by a caret that keeps a
/// different rhythm from the one in every other window.
const PHASE: Duration = Duration::from_millis(530);

/// The widget a [`Blink`] lives in, so its timer can find the way back.
///
/// [`Blink`] has to reach into the entity that owns it to toggle itself, and
/// it is generic over that entity — this is how it gets there.
pub trait Blinks: 'static {
    fn blink(&mut self) -> &mut Blink;
}

/// Which half of the cycle the caret is in, and the timer moving it along.
pub struct Blink {
    on: bool,
    /// What the last paint saw. Paint is the only place that knows whether
    /// the field has focus — a field is built long before there is a window
    /// to ask — so it tells us, and this is what makes that once per change
    /// rather than once per frame.
    focused: bool,
    /// Both the running timer and the off switch: a gpui `Task` cancels when
    /// it is dropped, so replacing this is also how a keystroke restarts the
    /// phase rather than joining one already half spent.
    timer: Option<Task<()>>,
}

impl Default for Blink {
    fn default() -> Self {
        // Solid, and not yet blinking: an unfocused field draws no caret, and
        // the first paint that has focus starts the timer.
        Self { on: true, focused: false, timer: None }
    }
}

impl Blink {
    /// Whether to draw the caret this frame.
    pub fn visible(&self) -> bool {
        self.on
    }

    /// Puts the caret back in its solid half and starts the phase again.
    ///
    /// Every edit and every caret move calls this. It does not notify: the
    /// callers are already notifying for the change that brought them here.
    pub fn wake<T: Blinks>(&mut self, cx: &mut Context<T>) {
        self.on = true;

        // Nothing to drive while the field is unfocused, and nothing to draw
        // either. Dropping the timer is what stops a composer left open in the
        // background repainting twice a second for nobody.
        if !self.focused {
            self.timer = None;
            return;
        }

        self.timer = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PHASE).await;

                let stepped = this.update(cx, |this, cx| {
                    let blink = this.blink();
                    blink.on = !blink.on;
                    cx.notify();
                });
                // The field is gone — a closed composer, most often.
                if stepped.is_err() {
                    break;
                }
            }
        }));
    }

    /// Told by paint whether the field has focus.
    pub fn set_focused<T: Blinks>(&mut self, focused: bool, cx: &mut Context<T>) {
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        // Solid either way: focus should show the caret at once rather than
        // wherever the last cycle left it, and a field losing focus should
        // leave it ready for when focus comes back.
        self.wake(cx);
    }
}
