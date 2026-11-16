//! Drawing what an extension described.
//!
//! An extension never gets a drawing surface. It returns a description — a
//! badge, a banner, a tree of rows and buttons — and this is where that
//! description becomes gpui elements, in the app's own theme, on the app's own
//! schedule. A frame is built in about 85µs and holds that number by never
//! letting arbitrary code run while one is being built; everything here draws
//! from data that is already in memory.
//!
//! The one thing an extension may not name is a colour. It names an
//! [`Emphasis`] — a role — and [`tone`] resolves it against the theme in front
//! of the user. An extension that could name a colour would look right in the
//! theme it was written against and wrong in the other three, forever.

use gpui::{
    AnyElement, Hsla, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, div, prelude::FluentBuilder, px,
};
use spool_ext::api::{Badge, Banner, Emphasis, Node, NodeKind, Panel};

use crate::{actions::RunExtensionCommand, theme::Theme};

/// What an emphasis means in this theme.
pub fn tone(emphasis: Emphasis, theme: &Theme) -> Hsla {
    match emphasis {
        Emphasis::Normal => theme.text,
        Emphasis::Muted => theme.text_muted,
        // Strong and heading are the same colour and differ in weight, which
        // the caller applies: the theme has one "most legible" text colour and
        // inventing a second would make one of them wrong.
        Emphasis::Strong | Emphasis::Heading => theme.text,
        Emphasis::Accent => theme.accent,
        Emphasis::Danger => theme.red,
    }
}

/// A mark on a row in the message list.
///
/// Text rather than an image, because rows are drawn twenty to a frame and an
/// extension able to supply a bitmap per row could make the list slow — which
/// is the one thing the list is not allowed to be.
pub fn badge(badge: &Badge, theme: &Theme) -> impl IntoElement {
    let colour = tone(badge.emphasis, theme);

    // A one-character badge is a mark, not a label, and is drawn bare.
    //
    // The distinction earns its keep on the quiet case. An extension that has
    // looked at a message and found nothing worth saying still wants to say it
    // looked — otherwise "scanned, nothing to report" and "never scanned" are
    // the same blank row, and an extension doing its job perfectly looks
    // broken. That mark goes on nearly every row, and a border around a single
    // dot is a box around a dot.
    let mark = badge.label.chars().count() == 1;

    div()
        .flex_shrink_0()
        .text_size(px(10.))
        .line_height(px(14.))
        .text_color(colour)
        .when(!mark, |s| {
            // A hairline in the same colour rather than a filled pill: a row of
            // filled badges reads as a second selection highlight running down
            // the list, and the list already has one of those.
            s.px_1().rounded_sm().border_1().border_color(colour.opacity(0.4))
        })
        .child(truncated(&badge.label, 18))
}

/// Every badge an extension put on one message.
///
/// Capped, because the row has a fixed width and the subject is what the row
/// is for. An extension with four things to say about a message has a panel
/// and a banner to say them in.
pub fn badges(badges: &[Badge], theme: &Theme) -> Vec<AnyElement> {
    badges.iter().take(MAX_BADGES).map(|b| badge(b, theme).into_any_element()).collect()
}

const MAX_BADGES: usize = 2;

/// A strip above a message in the reader.
pub fn banner(banner: &Banner, extension: &str, theme: &Theme) -> impl IntoElement {
    let colour = tone(banner.emphasis, theme);

    div()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1p5()
        .border_b_1()
        .border_color(theme.border_subtle)
        .bg(colour.opacity(0.08))
        .text_size(px(12.))
        .text_color(theme.text)
        // Drawn exactly as on a row, so a message reads the same in the list
        // and open.
        .children(banner.badges.iter().map(|b| badge(b, theme).into_any_element()))
        .child(div().flex_1().min_w_0().child(banner.text.clone()))
        .children(banner.action.as_ref().map(|action| {
            button(
                &action.label,
                format!("{extension}.{}", action.run),
                action.enabled,
                colour,
                theme,
            )
        }))
}

/// A banner with no text, drawn beside the subject rather than as a strip:
/// its badges, and its button if it has one.
pub fn tags(banner: &Banner, extension: &str, theme: &Theme) -> impl IntoElement {
    let colour = tone(banner.emphasis, theme);

    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .text_size(px(12.))
        .font_weight(gpui::FontWeight::NORMAL)
        .children(banner.badges.iter().map(|b| badge(b, theme).into_any_element()))
        .children(banner.action.as_ref().map(|action| {
            button(
                &action.label,
                format!("{extension}.{}", action.run),
                action.enabled,
                colour,
                theme,
            )
        }))
}

/// A button that runs one of an extension's commands.
///
/// The only thing a button can do, and it does it by dispatching the same
/// action the palette dispatches — so a button can never do something the
/// keyboard cannot. `qualified` is `spam-scorer.file`, which is what makes two
/// extensions with a command called `file` land in the right one.
fn button(
    label: &str,
    qualified: String,
    enabled: bool,
    colour: Hsla,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .id(gpui::SharedString::from(format!("ext-button-{qualified}")))
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded_sm()
        .border_1()
        .border_color(theme.border)
        .text_size(px(11.))
        .when(enabled, |s| s.text_color(colour).hover(|s| s.bg(theme.hover)).cursor_pointer())
        .when(!enabled, |s| s.text_color(theme.text_faint))
        .when(enabled, |s| {
            s.on_click(move |_, window, cx| {
                window.dispatch_action(Box::new(RunExtensionCommand { id: qualified.clone() }), cx);
            })
        })
        .child(label.to_string())
}

/// Turns a validated panel into elements.
///
/// The tree crossed the boundary as an arena — a flat list of nodes naming
/// their children by index — and was validated on the way in: no out-of-range
/// index, no node reachable twice, no more than 2,048 nodes or 24 levels. So
/// this walk can be a plain recursion without a visited set, and cannot be
/// made to run forever by anything an extension returns.
pub fn panel(panel: &Panel, extension: &str, theme: &Theme) -> AnyElement {
    let Some(root) = panel.root() else {
        return div().into_any_element();
    };
    node(panel, root, extension, theme)
}

fn node(tree: &Panel, node: &Node, extension: &str, theme: &Theme) -> AnyElement {
    let children = |tree: &Panel, node: &Node| -> Vec<AnyElement> {
        tree.children(node).map(|child| self::node(tree, child, extension, theme)).collect()
    };

    match &node.kind {
        NodeKind::Row => div()
            .flex()
            .items_center()
            .gap_2()
            .children(children(tree, node))
            .into_any_element(),

        NodeKind::Column => div()
            .flex()
            .flex_col()
            .gap_1p5()
            .children(children(tree, node))
            .into_any_element(),

        NodeKind::Text { content, emphasis } => div()
            .min_w_0()
            .truncate()
            .text_color(tone(*emphasis, theme))
            .text_size(match emphasis {
                Emphasis::Heading => px(13.),
                _ => px(12.),
            })
            .when(matches!(emphasis, Emphasis::Heading | Emphasis::Strong), |s| {
                s.font_weight(gpui::FontWeight::SEMIBOLD)
            })
            .child(content.clone())
            .into_any_element(),

        NodeKind::Button(spec) => button(
            &spec.label,
            format!("{extension}.{}", spec.run),
            spec.enabled,
            theme.accent,
            theme,
        )
        .into_any_element(),

        NodeKind::Divider => div()
            .h(px(1.))
            .w_full()
            .bg(theme.border_subtle)
            .into_any_element(),

        NodeKind::Spacer => div().flex_1().into_any_element(),
    }
}

/// Shortens a label to fit a row, on a character boundary.
fn truncated(text: &str, chars: usize) -> gpui::SharedString {
    match text.char_indices().nth(chars) {
        Some((cut, _)) => gpui::SharedString::from(format!("{}\u{2026}", &text[..cut])),
        None => gpui::SharedString::from(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeId;

    #[test]
    fn an_emphasis_resolves_to_something_in_every_theme() {
        // The promise the vocabulary makes: an extension names a role and gets
        // a colour that belongs to whatever theme is in front of the user.
        for choice in ThemeId::ALL {
            let theme = Theme::new(choice);
            for emphasis in [
                Emphasis::Normal,
                Emphasis::Muted,
                Emphasis::Strong,
                Emphasis::Heading,
                Emphasis::Accent,
                Emphasis::Danger,
            ] {
                let colour = tone(emphasis, &theme);
                assert!(colour.a > 0.0, "{emphasis:?} is invisible in {choice:?}");
            }
        }
    }

    #[test]
    fn a_badge_label_that_would_eat_the_row_is_cut() {
        assert_eq!(truncated("Spam 3.4", 18).to_string(), "Spam 3.4");
        let long = truncated("an extension with an enormous opinion", 18);
        assert!(long.ends_with('\u{2026}'), "{long}");
        assert!(long.chars().count() <= 19, "{long}");
    }

    #[test]
    fn a_single_glyph_is_drawn_as_a_mark_rather_than_a_label() {
        // Not a rendering detail: this is what lets an extension say "looked,
        // nothing to report" on nearly every row without the list acquiring a
        // column of little boxes.
        assert!(is_mark("\u{00b7}"), "a dot is a mark");
        assert!(is_mark("\u{2713}"), "so is a tick");
        assert!(!is_mark("Bulk"), "a word is a label");
        assert!(!is_mark("Spam 3.4"), "and so is a word with a number");
        // Counted in characters rather than bytes, or a glyph outside ASCII
        // would be boxed for being three bytes long.
        assert!(is_mark("\u{26a0}"), "a multi-byte glyph is still one character");
    }

    /// The rule [`badge`] applies, pulled out so it can be stated once.
    fn is_mark(label: &str) -> bool {
        label.chars().count() == 1
    }

    #[test]
    fn only_so_many_badges_reach_a_row() {
        let theme = Theme::new(ThemeId::ALL[0]);
        let many: Vec<Badge> = (0..6)
            .map(|n| Badge { label: format!("b{n}"), emphasis: Emphasis::Muted })
            .collect();

        assert_eq!(badges(&many, &theme).len(), MAX_BADGES, "the subject is what the row is for");
    }
}
