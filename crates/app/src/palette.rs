//! The command palette.
//!
//! Because every operation is already an [`Action`](gpui::Action), this is not
//! a second implementation of the menu — it is a list that dispatches the same
//! actions the keymap does. Adding a command to `actions::commands()` makes it
//! reachable here, from a keystroke, and from a click, with no further wiring.

use gpui::{Context, Entity, SharedString, Window, div, prelude::*, px};
use spool_core::MailboxId;

use crate::{
    actions::{Command, commands},
    text_input::{InputChanged, TextInput},
    theme::Theme,
    workspace::Workspace,
};

/// What the palette is currently a list of.
///
/// Filing a message needs a picker over folders, and a palette is already a
/// picker — one that filters as you type, ranks by subsequence, and is opened
/// by the key everybody's fingers already know. Building a second one next to
/// it would be two of the same widget disagreeing about how ranking works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Commands,
    /// Picking where the selected messages should go.
    Folders,
}

/// A folder the current selection could be filed into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub id: MailboxId,
    /// The path as the server spells it, which is the unambiguous name —
    /// `[Gmail]/Spam` rather than a `Spam` that could be one of three.
    pub path: SharedString,
    /// The account it belongs to, shown where the row's own name does not say.
    pub account: SharedString,
}

pub struct PaletteState {
    pub input: Entity<TextInput>,
    pub filter: String,
    pub selected: usize,
    pub mode: PaletteMode,
    _subscription: gpui::Subscription,
}

impl PaletteState {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        Self::in_mode(PaletteMode::Commands, "Type a command\u{2026}", window, cx)
    }

    /// The palette as a folder picker.
    pub fn folders(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        Self::in_mode(PaletteMode::Folders, "Move to which folder?", window, cx)
    }

    fn in_mode(
        mode: PaletteMode,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new(placeholder, cx));

        let subscription =
            cx.subscribe(&input, |this: &mut Workspace, _, event: &InputChanged, cx| {
                if let Some(palette) = this.palette.as_mut() {
                    palette.filter = event.0.to_string();
                    // Any change to the filter invalidates the cursor, so
                    // start back at the best match.
                    palette.selected = 0;
                }
                cx.notify();
            });

        let handle = input.read(cx).focus_handle.clone();
        window.focus(&handle);

        Self { input, filter: String::new(), selected: 0, mode, _subscription: subscription }
    }
}

/// Subsequence match with a score, the same shape of matcher Zed's palette
/// uses: `tf` finds "Toggle flag", and consecutive hits rank above scattered
/// ones so the obvious answer stays on top.
///
/// Scored twice and given the better of the two, because the two things that
/// make a match good pull in opposite directions. Taking the *nearest* match
/// for each letter is what makes `arch` land on "Archive"; taking the one that
/// *starts a word* is what makes `mr` mean "Mark … read" rather than the `r`
/// two letters into "Mark". A single greedy pass has to pick one and gets the
/// other wrong, so both are run and the higher score wins.
fn score(needle: &str, haystack: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }

    let lowered = haystack.to_lowercase();
    let chars: Vec<char> = lowered.chars().collect();
    let needle: String = needle.to_lowercase();

    let nearest = walk(&needle, &chars, false);
    let boundaries = walk(&needle, &chars, true);
    nearest.into_iter().chain(boundaries).max()
}

/// One pass over the haystack.
///
/// `prefer_boundaries` takes the next match that begins a word when there is
/// one, rather than the next match of any kind.
fn walk(needle: &str, chars: &[char], prefer_boundaries: bool) -> Option<i32> {
    let starts_word = |i: usize| i == 0 || chars[i - 1] == ' ';

    let mut from = 0usize;
    let mut total = 0i32;
    let mut last: Option<usize> = None;

    for wanted in needle.chars() {
        if wanted.is_whitespace() {
            continue;
        }

        let nearest = (from..chars.len()).find(|&i| chars[i] == wanted)?;
        let found = match prefer_boundaries {
            true => (from..chars.len())
                .find(|&i| chars[i] == wanted && starts_word(i))
                .unwrap_or(nearest),
            false => nearest,
        };

        total += match last {
            // Adjacent characters are worth far more than distant ones.
            Some(prev) if found == prev + 1 => 8,
            Some(_) => 1,
            None => 4,
        };
        // A match at a word boundary usually means the user typed initials.
        if starts_word(found) {
            total += 6;
        }

        last = Some(found);
        from = found + 1;
    }

    Some(total)
}

/// Ranks the app's own commands against the current filter.
///
/// What the palette actually shows is [`matching_in`] over this list plus
/// whatever extensions contribute; this is the built-in half, and the one the
/// ranking tests are written against.
#[cfg_attr(not(test), allow(dead_code))]
pub fn matching(filter: &str) -> Vec<(Command, i32)> {
    matching_in(commands(), filter)
}

/// The same ranking, over a list somebody else assembled.
///
/// Extension commands arrive here having been appended to the built-ins, so
/// they are ranked by the same subsequence scoring and sit in one list rather
/// than in a section of their own. A command is a command: whether it came
/// from this binary or from a component should not change how it is found.
pub fn matching_in(list: Vec<Command>, filter: &str) -> Vec<(Command, i32)> {
    let mut matches: Vec<(Command, i32)> = list
        .into_iter()
        .filter_map(|command| score(filter, &command.name).map(|s| (command, s)))
        .collect();

    // Sort by score, then by the original ordering, which is roughly
    // frequency of use — a stable sort keeps that as the tiebreak.
    matches.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    matches
}

/// The same ranking, over folders rather than commands.
///
/// Matched against the whole path, so typing `gs` finds `[Gmail]/Spam` and
/// typing `spam` finds it too.
pub fn matching_folders(
    destinations: &[Destination],
    filter: &str,
) -> Vec<(Destination, i32)> {
    let mut matches: Vec<(Destination, i32)> = destinations
        .iter()
        .filter_map(|folder| score(filter, &folder.path).map(|s| (folder.clone(), s)))
        .collect();
    matches.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    matches
}

/// How many rows the palette shows. The arrow keys wrap within this, so it
/// has to be the same number in both places.
pub const VISIBLE: usize = 12;

/// One row: a name on the left, a dimmer hint on the right.
fn row(
    theme: &Theme,
    index: usize,
    is_selected: bool,
    name: SharedString,
    hint: SharedString,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(("command", index))
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .px_3()
        .py_1p5()
        .rounded_md()
        .cursor_pointer()
        .when(is_selected, |s| s.bg(theme.selected))
        .when(!is_selected, |s| s.hover(|s| s.bg(theme.hover)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(if is_selected { theme.text } else { theme.text_muted })
                .child(name),
        )
        // No chip where there is nothing to put in it. A command with no
        // keystroke, or a folder with only one account to belong to, would
        // otherwise get an empty pill on every row — a column of grey blanks
        // that reads as something missing.
        .when(!hint.is_empty(), |this| {
            this.child(
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .rounded_sm()
                    .bg(theme.panel)
                    .font_family(theme.mono_font)
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(hint),
            )
        })
}

impl Workspace {
    pub(crate) fn render_palette(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let palette = self.palette.as_ref()?;
        let input = palette.input.clone();
        let selected = palette.selected;
        let mode = palette.mode;
        let filter = palette.filter.clone();

        // Both modes draw the same row; only what a click does differs.
        let rows: Vec<gpui::AnyElement> = match mode {
            PaletteMode::Commands => matching_in(self.palette_commands(), &filter)
                .into_iter()
                .take(VISIBLE)
                .enumerate()
                .map(|(ix, (command, _))| {
                    let action = command.action.boxed_clone();
                    row(theme, ix, ix == selected, command.name, command.keystroke.into())
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.palette = None;
                            window.focus(&this.list_focus);
                            window.dispatch_action(action.boxed_clone(), cx);
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),

            PaletteMode::Folders => matching_folders(&self.move_destinations(), &filter)
                .into_iter()
                .take(VISIBLE)
                .enumerate()
                .map(|(ix, (folder, _))| {
                    let target = folder.id;
                    row(theme, ix, ix == selected, folder.path, folder.account)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.palette = None;
                            window.focus(&this.list_focus);
                            this.move_selection_to(target, cx);
                            cx.notify();
                        }))
                        .into_any_element()
                })
                .collect(),
        };

        let empty = rows.is_empty();

        Some(
            // A full-window scrim: clicking anywhere outside dismisses, which
            // is the behaviour every palette has trained people to expect.
            div()
                .id("palette-scrim")
                .absolute()
                .inset_0()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.palette = None;
                    window.focus(&this.list_focus);
                    cx.notify();
                }))
                .flex()
                .flex_col()
                .items_center()
                .pt(px(96.))
                .bg(theme.scrim)
                .child(
                    div()
                        .id("palette")
                        .w(px(520.))
                        .max_h(px(420.))
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        // See the settings pane: swallowing the press is what
                        // stops a click inside from reaching the scrim.
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation()
                        })
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_2p5()
                                .border_b_1()
                                .border_color(theme.border)
                                .child(div().text_color(theme.text_faint).child("\u{203A}"))
                                .child(input),
                        )
                        .child(
                            div()
                                .id("palette-list")
                                .flex()
                                .flex_col()
                                .gap_px()
                                .p_1p5()
                                .overflow_y_scroll()
                                .when(empty, |this| {
                                    this.child(
                                        div()
                                            .px_3()
                                            .py_2()
                                            .text_color(theme.text_faint)
                                            .child(match mode {
                                                PaletteMode::Commands => "No matching commands",
                                                // Distinct from "no folder
                                                // matched what you typed":
                                                // a selection spanning two
                                                // accounts has nowhere legal
                                                // to go, and saying so is
                                                // more use than an empty list.
                                                PaletteMode::Folders => {
                                                    "No folder this can be moved to"
                                                }
                                            }),
                                    )
                                })
                                .children(rows),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_keeps_every_command_in_order() {
        let all = matching("");
        assert_eq!(all.len(), commands().len());
        assert_eq!(all[0].0.name, commands()[0].name);
    }

    #[test]
    fn subsequences_match() {
        let names: Vec<String> = matching("tf").iter().map(|(c, _)| c.name.to_string()).collect();
        assert!(names.iter().any(|n| n == "Toggle flag"), "got {names:?}");
    }

    #[test]
    fn exact_prefixes_rank_first() {
        let best = matching("arch").first().map(|(c, _)| c.name.to_string());
        assert_eq!(best.as_deref(), Some("Archive message"));
    }

    #[test]
    fn a_nearer_match_still_wins_where_it_should() {
        // The other half of the two-pass scoring. Preferring word boundaries
        // must not cost the adjacent match that makes prefixes work.
        let best = matching("mov").first().map(|(c, _)| c.name.to_string());
        assert_eq!(best.as_deref(), Some("Move to folder\u{2026}"));
    }

    #[test]
    fn folders_rank_the_same_way_commands_do() {
        let folders = [
            Destination {
                id: MailboxId(1),
                path: "[Gmail]/Spam".into(),
                account: "Work".into(),
            },
            Destination { id: MailboxId(2), path: "Archive".into(), account: "Work".into() },
        ];

        let best = matching_folders(&folders, "spam").first().map(|(d, _)| d.id);
        assert_eq!(best, Some(MailboxId(1)), "matched on the whole path, brackets and all");

        assert!(matching_folders(&folders, "zzz").is_empty());
        assert_eq!(matching_folders(&folders, "").len(), 2, "no filter shows everything");
    }

    #[test]
    fn nonsense_matches_nothing() {
        assert!(matching("zzzqqq").is_empty());
    }

    #[test]
    fn word_initials_beat_scattered_letters() {
        // Initials, rather than the letters merely appearing somewhere.
        //
        // Not "mr": there are now three commands of the shape "Mark … read"
        // and they score identically on it, which is the honest answer — the
        // query really is ambiguous between them. "mmr" is not.
        let best = matching("mmr").first().map(|(c, _)| c.name.to_string());
        assert_eq!(best.as_deref(), Some("Mark mailbox read"));

        // And the ambiguous one still puts a "Mark … read" command on top
        // rather than something that merely contains an m and an r.
        let best = matching("mr").first().map(|(c, _)| c.name.to_string()).unwrap_or_default();
        assert!(best.starts_with("Mark"), "got {best:?}");
        assert!(best.ends_with("read"), "got {best:?}");
    }
}
