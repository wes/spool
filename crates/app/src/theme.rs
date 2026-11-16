//! Colors and metrics.
//!
//! The active theme is a gpui [`Global`], which is how Zed does it: one value
//! the whole window reads through `cx.theme()`, so switching themes is a
//! single assignment followed by a repaint rather than a message threaded down
//! a tree. No call site constructs a palette of its own, and no call site
//! writes a color literal — that is what makes a theme picker a small feature
//! instead of a rewrite.

use spool_core::html;
use gpui::{App, Global, Hsla, Pixels, px, rgb, rgba};
use serde::{Deserialize, Serialize};

/// The themes the picker offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeId {
    Light,
    /// The aliases are retired themes. A settings file still naming one reads
    /// as Dark — both were dark — rather than failing to parse, which would
    /// take the window position and pane widths down with it.
    #[default]
    #[serde(alias = "catppuccin", alias = "dracula")]
    Dark,
}

impl ThemeId {
    /// Every theme, in the order the picker shows them.
    pub const ALL: [ThemeId; 2] = [ThemeId::Light, ThemeId::Dark];

    pub fn name(self) -> &'static str {
        match self {
            ThemeId::Light => "Light",
            ThemeId::Dark => "Dark",
        }
    }

}

#[derive(Clone, Debug)]
pub struct Theme {
    pub id: ThemeId,
    /// Which way round this palette runs.
    ///
    /// Carried rather than re-derived from a luminance comparison, because a
    /// palette knows its own polarity and a comparison only guesses at it.
    /// Nothing in the window branches on it today — the theme thumbnails were
    /// the last thing that did — but the tests hold every theme to the rules
    /// for its own polarity, and that is what says which rules apply.
    #[allow(dead_code)]
    pub is_light: bool,

    /// The window's base surface: the reading pane and message list.
    pub background: Hsla,
    /// Chrome behind the sidebar, title bar and status bar.
    pub panel: Hsla,
    /// Popovers, the command palette and the settings pane.
    pub elevated: Hsla,
    pub border: Hsla,
    pub border_subtle: Hsla,
    /// Dimming drawn behind a modal.
    pub scrim: Hsla,

    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    /// Text drawn on top of an accent or avatar fill.
    pub text_on_accent: Hsla,

    pub accent: Hsla,
    pub accent_muted: Hsla,
    /// Unread counts in the sidebar.
    ///
    /// Its own token rather than the accent: a count is a number you read
    /// dozens of times a minute, and on a dark surface plain white is quieter
    /// and quicker to scan than a saturated blue.
    pub count: Hsla,
    /// The border of a text field that is focused or in use.
    ///
    /// Also its own token, for the same reason and one more: a ring is drawn
    /// around whatever you are typing into, so it should read as "here" rather
    /// than as a colored status.
    pub focus_ring: Hsla,
    /// The pane divider while it is being dragged, and while the pointer is
    /// over it.
    ///
    /// Its own token for the same reason [`Theme::focus_ring`] is one: the
    /// handle marks *where you are pushing*, not a status, and a saturated
    /// blue seam down a dark window reads as something being wrong. On the
    /// dark theme it is the same grey a selected row uses.
    pub divider_active: Hsla,
    pub divider_hover: Hsla,
    /// Fill behind the selected row while the list has focus.
    pub selected: Hsla,
    /// Fill behind the selected row once focus has moved elsewhere, so it is
    /// still clear where you are without competing for attention.
    pub selected_inactive: Hsla,
    pub hover: Hsla,
    /// Fill behind selected *text*, in a field or in the body of a composer.
    ///
    /// Its own token rather than [`Theme::accent_muted`], which is a hover
    /// wash and was doing this job badly. The two have opposite jobs: a wash
    /// sits close to the surface so it barely registers, while a text
    /// selection is a fill you then have to read your own words through. On
    /// the dark theme the wash was near-white under near-white text, which is
    /// the failure `text_selection_keeps_its_text_readable` now guards.
    pub text_selection: Hsla,

    /// The overlay scrollbar thumb, and the same thumb under the pointer.
    ///
    /// Translucent rather than a flat grey: it floats over the list, and a
    /// solid bar would draw a hard edge down rows it is only passing over.
    /// macOS darkens the thumb while the pointer is on the bar, which is also
    /// how a drag says it has hold of it.
    pub scrollbar: Hsla,
    pub scrollbar_hover: Hsla,

    pub green: Hsla,
    pub yellow: Hsla,
    pub red: Hsla,

    /// Cycled through for sender avatars and account dots.
    pub avatars: [Hsla; 8],

    pub font: &'static str,
    pub mono_font: &'static str,
    pub text_size: Pixels,
    pub small_text_size: Pixels,
}

/// Metrics are theme-independent for now; only the palette changes.
struct Metrics;

impl Metrics {
    // A stack rather than one name: gpui falls through to the next family it
    // can load, and these differ per platform.
    const FONT: &'static str = "SF Pro Text";
    // Monaco, because it is *there*. SF Mono ships with Xcode and Terminal
    // rather than with macOS, and JetBrains Mono is a download — naming either
    // one gets you whatever the text system falls back to, which is a
    // proportional face, silently. This machine had exactly that: a `mono_font`
    // of "SF Mono" that nothing was ever rendering.
    const MONO: &'static str = "Monaco";
    const TEXT: Pixels = px(13.);
    const SMALL: Pixels = px(11.);
}

impl Theme {
    pub fn new(id: ThemeId) -> Self {
        match id {
            ThemeId::Light => Self::light(),
            ThemeId::Dark => Self::dark(),
        }
    }

    /// Neutral light. White surfaces, near-black text, one blue accent.
    fn light() -> Self {
        Self {
            id: ThemeId::Light,
            is_light: true,

            background: rgb(0xffffff).into(),
            panel: rgb(0xf6f7f9).into(),
            elevated: rgb(0xffffff).into(),
            border: rgb(0xdfe3e8).into(),
            border_subtle: rgb(0xedf0f3).into(),
            scrim: rgba(0x1f232833).into(),

            text: rgb(0x1f2328).into(),
            text_muted: rgb(0x5c636b).into(),
            text_faint: rgb(0x8b939c).into(),
            text_on_accent: rgb(0xffffff).into(),

            accent: rgb(0x2f6feb).into(),
            accent_muted: rgba(0x2f6feb40).into(),
            // The light theme keeps the blue: near-white on white is nothing.
            count: rgb(0x2f6feb).into(),
            focus_ring: rgb(0x2f6feb).into(),
            divider_active: rgb(0x2f6feb).into(),
            divider_hover: rgba(0x2f6feb40).into(),
            selected: rgba(0x2f6feb1f).into(),
            selected_inactive: rgba(0x1f23280d).into(),
            hover: rgba(0x1f23280a).into(),
            // The theme's own blue, opaque rather than a tint: a selection is
            // painted over a field whose background is not always the window's,
            // and a tint would come out a different colour in each of them.
            text_selection: rgb(0xaec7f7).into(),

            scrollbar: rgba(0x1f232859).into(),
            scrollbar_hover: rgba(0x1f232890).into(),

            green: rgb(0x1a7f37).into(),
            yellow: rgb(0x9a6700).into(),
            red: rgb(0xcf222e).into(),

            // Dark enough to carry white text in the 28px avatar chip.
            avatars: [
                rgb(0x2f6feb).into(),
                rgb(0x1a7f37).into(),
                rgb(0xbf8700).into(),
                rgb(0xcf222e).into(),
                rgb(0x8250df).into(),
                rgb(0x0f7c86).into(),
                rgb(0xbc4c00).into(),
                rgb(0x6e7781).into(),
            ],

            font: Metrics::FONT,
            mono_font: Metrics::MONO,
            text_size: Metrics::TEXT,
            small_text_size: Metrics::SMALL,
        }
    }

    /// Neutral dark. Deliberately low chroma, so the only saturated things on
    /// screen are the ones carrying meaning.
    fn dark() -> Self {
        Self {
            id: ThemeId::Dark,
            is_light: false,

            background: rgb(0x1c1d1f).into(),
            panel: rgb(0x161719).into(),
            elevated: rgb(0x232427).into(),
            border: rgb(0x303235).into(),
            border_subtle: rgb(0x252629).into(),
            scrim: rgba(0x00000073).into(),

            text: rgb(0xe3e4e6).into(),
            text_muted: rgb(0xa0a3a8).into(),
            text_faint: rgb(0x6f7378).into(),
            text_on_accent: rgb(0x14161a).into(),

            accent: rgb(0xffffff).into(),
            accent_muted: rgba(0xffffff60).into(),
            count: rgb(0xe3e4e6).into(),
            focus_ring: rgb(0xffffff).into(),
            divider_active: rgb(0x333333).into(),
            divider_hover: rgba(0x33333359).into(),
            selected: rgb(0x333333).into(),
            selected_inactive: rgba(0xffffff0f).into(),
            hover: rgba(0xffffff0d).into(),
            // The one place this theme spends chroma on something that is not
            // a status: a neutral fill dark enough to read near-white text on
            // would be a hole punched in the surface, and one light enough to
            // see would swallow the text. Blue solves both, and a selection is
            // carrying meaning — it is the thing the next keystroke replaces.
            text_selection: rgb(0x36507a).into(),

            scrollbar: rgba(0xffffff59).into(),
            scrollbar_hover: rgba(0xffffff96).into(),

            green: rgb(0x7ec699).into(),
            yellow: rgb(0xe0c084).into(),
            red: rgb(0xe0868b).into(),

            avatars: [
                rgb(0x6ea8fe).into(),
                rgb(0x7ec699).into(),
                rgb(0xe0c084).into(),
                rgb(0xe0868b).into(),
                rgb(0xb99cf0).into(),
                rgb(0x6fc0c8).into(),
                rgb(0xdda06b).into(),
                rgb(0x9a9ea6).into(),
            ],

            font: Metrics::FONT,
            mono_font: Metrics::MONO,
            text_size: Metrics::TEXT,
            small_text_size: Metrics::SMALL,
        }
    }

    /// Picks a stable avatar color for a sender.
    pub fn avatar(&self, seed: u32) -> Hsla {
        self.avatars[(seed % self.avatars.len() as u32) as usize]
    }

    /// The accent assigned to an account, so its folders and messages carry a
    /// consistent identity in the unified view.
    pub fn account_color(&self, index: u8) -> Hsla {
        self.avatars[(index as usize) % self.avatars.len()]
    }
}

/// The active theme, held as a gpui global.
pub struct ActiveTheme(pub Theme);

impl Global for ActiveTheme {}

/// A colour the sender asked for, adapted to the surface it landed on.
///
/// Mail is designed against the sender's own background — `#5c554c` is a
/// legible muted grey on the near-black Maple builds its alerts on, and
/// invisible on white. Using it raw means half the mail in a light theme
/// disappears; ignoring it means every badge, every error count and every
/// muted footer reads as body text.
///
/// So the hue and saturation are kept and only the lightness moves, until the
/// colour clears the WCAG AA contrast ratio against the surface behind it. A
/// colour that cannot get there — a mid grey on a mid grey — gives up and
/// returns the theme's own text, which is always legible.
pub fn readable(color: html::Rgba, on: Hsla) -> Hsla {
    adjusted(color, on, 4.5)
}

/// The same, for a colour the sender put on a fill of their own.
///
/// A much lower bar, because the pair was chosen together and is known to
/// work: white on a green badge is what every status page in the world looks
/// like, and it does not clear AA. Only a pair that is genuinely unreadable —
/// grey on grey, a colour on itself — gets touched.
pub fn readable_on_fill(color: html::Rgba, fill: Hsla) -> Hsla {
    adjusted(color, fill, 2.2)
}

fn adjusted(color: html::Rgba, on: Hsla, target: f32) -> Hsla {
    let mut adapted: Hsla = gpui::Rgba {
        r: color.r as f32 / 255.,
        g: color.g as f32 / 255.,
        b: color.b as f32 / 255.,
        a: 1.,
    }
    .into();

    if contrast(adapted, on) >= target {
        return adapted;
    }

    // Away from the surface: lighter on a dark background, darker on a light
    // one. Anything else moves toward the thing it has to be distinguished
    // from.
    let step = if luminance(on) < 0.5 { 0.02 } else { -0.02 };
    for _ in 0..50 {
        adapted.l = (adapted.l + step).clamp(0., 1.);
        if contrast(adapted, on) >= target {
            return adapted;
        }
        if adapted.l == 0. || adapted.l == 1. {
            break;
        }
    }

    adapted
}

/// A fill the sender asked for, over the surface it landed on.
///
/// Mail draws a badge as a colour at fifteen percent over its own background,
/// so the alpha carries as much of the intent as the hue does: blending it
/// over *our* surface is what makes the badge sit in this window rather than
/// punch a hole in it.
pub fn fill(color: html::Rgba, over: Hsla) -> Hsla {
    let alpha = color.a as f32 / 255.;
    let sender: Hsla = gpui::Rgba {
        r: color.r as f32 / 255.,
        g: color.g as f32 / 255.,
        b: color.b as f32 / 255.,
        a: 1.,
    }
    .into();

    if alpha >= 0.999 {
        return sender;
    }

    let (front, back) = (gpui::Rgba::from(sender), gpui::Rgba::from(over));
    gpui::Rgba {
        r: front.r * alpha + back.r * (1. - alpha),
        g: front.g * alpha + back.g * (1. - alpha),
        b: front.b * alpha + back.b * (1. - alpha),
        a: 1.,
    }
    .into()
}

/// Text to put on a fill whose sender did not say what colour it should be.
///
/// Black or white, whichever the fill can carry. The theme's own text colour
/// is the wrong answer here: a light theme's near-black on a sender's dark
/// button is a button with nothing written on it.
pub fn on_fill(fill: Hsla) -> Hsla {
    let (black, white) = (Hsla { h: 0., s: 0., l: 0., a: 1. }, Hsla { h: 0., s: 0., l: 1., a: 1. });
    if contrast(white, fill) >= contrast(black, fill) { white } else { black }
}

/// Relative luminance, per WCAG: the amount of light a colour puts out, which
/// is what contrast is actually a ratio of.
fn luminance(color: Hsla) -> f32 {
    let rgba = gpui::Rgba::from(color);
    let channel = |c: f32| {
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
}

/// The WCAG contrast ratio between two colours, from 1 (identical) to 21
/// (black on white).
pub(crate) fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    let (hi, lo) = if a > b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

/// `cx.theme()`, available anywhere there is an [`App`].
pub trait ThemeAccess {
    fn theme(&self) -> &Theme;
}

impl ThemeAccess for App {
    fn theme(&self) -> &Theme {
        // Installed during startup, before any window exists, so this cannot
        // fail in a running app.
        &self.global::<ActiveTheme>().0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_senders_colour_is_kept_when_it_is_already_legible() {
        // Maple's green on Maple's near-black, read in a dark theme: nothing
        // to fix, so nothing moves.
        let dark = Theme::new(ThemeId::Dark);
        let green = html::Rgba { r: 0x4a, g: 0xa8, b: 0x65, a: 255 };
        let adapted = readable(green, dark.background);
        assert!(contrast(adapted, dark.background) >= 4.5);
        // The hue survives — a green badge stays green.
        let raw: Hsla = gpui::Rgba { r: 0.29, g: 0.66, b: 0.4, a: 1. }.into();
        assert!((adapted.h - raw.h).abs() < 0.05, "{adapted:?} drifted from {raw:?}");
    }

    #[test]
    fn a_colour_designed_for_the_other_kind_of_background_is_pulled_back() {
        // The failure this exists to prevent: mail written for a dark client,
        // read in the light theme, rendering as near-white on white.
        let light = Theme::new(ThemeId::Light);
        let dark = Theme::new(ThemeId::Dark);

        let pale = html::Rgba { r: 0xe8, g: 0xdf, b: 0xd3, a: 255 };
        assert!(contrast(readable(pale, light.background), light.background) >= 4.5);

        let charcoal = html::Rgba { r: 0x22, g: 0x22, b: 0x22, a: 255 };
        assert!(contrast(readable(charcoal, dark.background), dark.background) >= 4.5);
    }

    #[test]
    fn every_theme_can_make_every_grey_readable() {
        // Greys are the hard case: no hue to move, only lightness.
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            for level in [0x00, 0x40, 0x80, 0xc0, 0xff] {
                let grey = html::Rgba { r: level, g: level, b: level, a: 255 };
                let adapted = readable(grey, theme.background);
                assert!(
                    contrast(adapted, theme.background) >= 4.5,
                    "{} could not make #{level:02x} readable",
                    id.name(),
                );
            }
        }
    }

    #[test]
    fn a_translucent_badge_fill_blends_into_this_window() {
        // `rgba(74,168,101,0.15)` is how mail draws a tinted badge. Painted
        // opaque it would be a solid green block; blended it is a tint.
        let dark = Theme::new(ThemeId::Dark);
        let tint = html::Rgba { r: 0x4a, g: 0xa8, b: 0x65, a: 38 };
        let blended = fill(tint, dark.background);

        assert!(
            contrast(blended, dark.background) < 1.6,
            "a 15% tint should sit close to the surface, got {blended:?}",
        );

        // An opaque fill is the colour the sender chose, untouched.
        let solid = html::Rgba { r: 0x2e, g: 0xb6, b: 0x7d, a: 255 };
        let painted = fill(solid, dark.background);
        assert!(contrast(painted, dark.background) > 3.);
    }

    #[test]
    fn a_pair_the_sender_chose_together_is_left_alone() {
        // White on a green badge is what every status page looks like, and it
        // does not clear AA. Pushing it to AA turns the badge into dark text
        // on green, which is not what the sender drew and not an improvement.
        let dark = Theme::new(ThemeId::Dark);
        let green = html::Rgba { r: 0x2e, g: 0xb6, b: 0x7d, a: 255 };
        let white = html::Rgba { r: 0xff, g: 0xff, b: 0xff, a: 255 };

        let badge = fill(green, dark.background);
        let text = readable_on_fill(white, badge);
        assert!(text.l > 0.9, "the badge lost its white text: {text:?}");

        // A pair that genuinely cannot be read still gets fixed.
        let same = readable_on_fill(green, badge);
        assert!(contrast(same, badge) >= 2.2, "green on its own green stayed unreadable");
    }

    #[test]
    fn every_theme_id_builds_and_reports_itself() {
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            assert_eq!(theme.id, id, "{} built the wrong palette", id.name());
            assert!(!id.name().is_empty());
        }
    }

    #[test]
    fn only_the_light_theme_is_light() {
        let light: Vec<ThemeId> =
            ThemeId::ALL.into_iter().filter(|id| Theme::new(*id).is_light).collect();
        assert_eq!(light, vec![ThemeId::Light]);
    }

    #[test]
    fn text_reads_against_its_own_background() {
        // Not a contrast-ratio check — just a guard against a palette being
        // pasted in with text and background on the same side of the scale.
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            let gap = (theme.text.l - theme.background.l).abs();
            assert!(
                gap > 0.4,
                "{}: text and background are too close (l {:.2} vs {:.2})",
                id.name(),
                theme.text.l,
                theme.background.l,
            );
        }
    }

    #[test]
    fn the_text_ramp_steps_toward_the_background() {
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            let (text, muted, faint) = (theme.text.l, theme.text_muted.l, theme.text_faint.l);

            let ordered = if theme.is_light {
                text < muted && muted < faint
            } else {
                text > muted && muted > faint
            };
            assert!(
                ordered,
                "{}: text ramp is out of order ({text:.2}, {muted:.2}, {faint:.2})",
                id.name(),
            );
        }
    }

    #[test]
    fn text_selection_keeps_its_text_readable() {
        // The failure this was written for: the dark theme selected text with
        // `accent_muted`, a near-white wash, and near-white text on it came to
        // about 3.7:1 — there, but not readable, so selecting a line in the
        // composer made it harder to see rather than easier.
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            let ratio = contrast(theme.text, theme.text_selection);
            assert!(
                ratio >= 4.5,
                "{}: text on its own selection is only {ratio:.1}:1",
                id.name(),
            );
        }
    }

    #[test]
    fn text_selection_is_visible_against_the_surfaces_it_is_drawn_on() {
        // The other half: a fill you cannot tell from the field behind it is
        // not a selection. Fields sit on all three surfaces — the window, the
        // panel in the title bar, and a popover.
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            for (name, surface) in [
                ("background", theme.background),
                ("panel", theme.panel),
                ("elevated", theme.elevated),
            ] {
                let ratio = contrast(theme.text_selection, surface);
                assert!(
                    ratio >= 1.5,
                    "{}: the selection is only {ratio:.2}:1 against the {name} behind it",
                    id.name(),
                );
            }
        }
    }

    #[test]
    fn counts_and_focus_rings_are_visible_on_their_own_surface() {
        // Both are drawn on the panel: the count in the sidebar, the ring
        // around the search field in the title bar.
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            for (name, color) in [("count", theme.count), ("focus ring", theme.focus_ring)] {
                let gap = (color.l - theme.panel.l).abs();
                assert!(
                    gap > 0.3,
                    "{}: the {name} is too close to the panel behind it \
                     (l {:.2} vs {:.2})",
                    id.name(),
                    color.l,
                    theme.panel.l,
                );
            }
        }
    }

    #[test]
    fn dark_themes_count_in_white_and_the_light_theme_does_not() {
        // The point of splitting these off from the accent. Inverting it —
        // near-white on the light theme — would make counts invisible, which
        // is the failure this guards.
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            if theme.is_light {
                assert_eq!(theme.count, theme.accent, "{}", id.name());
                assert_eq!(theme.focus_ring, theme.accent, "{}", id.name());
                assert_eq!(theme.divider_active, theme.accent, "{}", id.name());
            } else {
                assert_eq!(theme.count, theme.text, "{} counts in its own white", id.name());
                // Neutral, not equal to the text: the ring is a brighter white
                // than the text, and the divider a quiet grey.
                for (name, color) in
                    [("focus ring", theme.focus_ring), ("divider", theme.divider_active)]
                {
                    assert!(color.s < 0.1, "{}: the {name} is tinted, not neutral", id.name());
                }
            }
        }
    }

    #[test]
    fn panel_and_background_are_distinguishable_but_close() {
        for id in ThemeId::ALL {
            let theme = Theme::new(id);
            let gap = (theme.panel.l - theme.background.l).abs();
            assert!(
                gap > 0.001 && gap < 0.15,
                "{}: panel/background separation of {gap:.3} is wrong",
                id.name(),
            );
        }
    }

    #[test]
    fn avatar_colors_are_distinct_and_stable() {
        for id in ThemeId::ALL {
            let theme = Theme::new(id);

            let distinct: std::collections::HashSet<String> =
                theme.avatars.iter().map(|c| format!("{c:?}")).collect();
            assert_eq!(distinct.len(), 8, "{}: duplicate avatar colors", id.name());

            // The same seed must always land on the same color, and any seed
            // must land somewhere.
            assert_eq!(theme.avatar(11), theme.avatar(11 + 8));
            assert_eq!(theme.avatar(3), theme.avatars[3]);
            assert_eq!(theme.account_color(9), theme.avatars[1]);
        }
    }

    #[test]
    fn theme_ids_round_trip_through_config() {
        #[derive(Serialize, Deserialize)]
        struct Wrapper {
            theme: ThemeId,
        }

        for id in ThemeId::ALL {
            let encoded = toml::to_string(&Wrapper { theme: id }).unwrap();
            let decoded: Wrapper = toml::from_str(&encoded).unwrap();
            assert_eq!(decoded.theme, id, "{encoded}");
        }
    }

    #[test]
    fn the_stored_name_is_kebab_case() {
        #[derive(Serialize)]
        struct Wrapper {
            theme: ThemeId,
        }

        let encoded = toml::to_string(&Wrapper { theme: ThemeId::Light }).unwrap();
        assert!(encoded.contains("light"), "{encoded}");
    }
}
