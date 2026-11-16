//! The title bar and the status bar.

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, Animation, AnimationExt, Context, Div, Hsla, Pixels, Render, SharedString,
    Stateful, Window, div, ease_in_out, prelude::*, px, relative,
};

use crate::{
    actions::{Archive, Compose, DeleteMessage, Forward, Reply, ReplyAll, ToggleFlag, ToggleSidebar},
    activity::ActivityKind,
    perf::{format_count, format_duration},
    sync_flow::Flow,
    theme::{Theme, ThemeAccess},
    workspace::{FRAME_BUDGET, Workspace},
};

/// Left inset for the macOS traffic lights, which are drawn over our own
/// chrome because the system title bar is transparent.
///
/// It applies to whichever pane is against the left edge: the sidebar's own
/// top strip normally, and the header bar when the sidebar is hidden.
#[cfg(target_os = "macos")]
pub(crate) const TRAFFIC_LIGHT_INSET: f32 = 78.;
#[cfg(not(target_os = "macos"))]
pub(crate) const TRAFFIC_LIGHT_INSET: f32 = 12.;

/// Height of the top strip, shared by the sidebar's header and the content's
/// so the two line up across the divider.
pub(crate) const HEADER_HEIGHT: f32 = 38.;

impl Workspace {
    pub(crate) fn render_title_bar(
        &self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // Below this there is not room for the buttons, the chips and a
        // search box at once, and the search box is the one worth keeping:
        // the chips report state the sidebar and the reader also carry, and a
        // toolbar with no way to search is missing a feature rather than a
        // label. The window's own minimum is 760, so this is the narrowest
        // the app can be plus the width of a field worth having.
        const ROOM_FOR_CHIPS: Pixels = px(940.);
        let chips = window.viewport_size().width >= ROOM_FOR_CHIPS;

        let search = self.search.clone();
        let searching = !self.search_text.is_empty();

        // Everything but the sidebar toggle acts on a message, and there is
        // not always one.
        let selected = self.selected_envelope();
        let has_message = selected.is_some();
        let flagged = selected.is_some_and(|envelope| envelope.flags.is_flagged());

        div()
            // The whole bar is the window's drag handle, since there is no
            // system titlebar behind it.
            .id("title-bar")
            .flex()
            .items_center()
            .gap_1()
            .h(px(HEADER_HEIGHT))
            .flex_shrink_0()
            // The traffic lights sit over the sidebar when there is one; this
            // bar only has to make room for them when it is against the left
            // edge itself.
            .pl(px(if self.sidebar_visible { 8. } else { TRAFFIC_LIGHT_INSET }))
            .pr_3()
            // The last line of defence for a narrow window: the buttons are a
            // fixed width and the row can run out of room, and a toolbar whose
            // contents paint past the window edge looks like a broken window
            // rather than a full one.
            .overflow_hidden()
            // The same panel as the sidebar it continues from. No rule under
            // it either: the content card below draws its own top edge, and a
            // second line above that reads as a seam.
            .bg(theme.panel)
            .child(tool_button(
                "toolbar-sidebar",
                "\u{25E7}",
                "Show or hide the sidebar",
                true,
                false,
                Box::new(ToggleSidebar),
                theme,
                cx,
            ))
            .child(separator(theme))
            .child(tool_button(
                "toolbar-delete",
                // The keystroke for this is backspace, and this is the glyph
                // for backspace. The sidebar's own Trash mark reads as "no
                // entry" out of that context.
                "\u{232B}",
                "Move to Trash",
                has_message,
                false,
                Box::new(DeleteMessage),
                theme,
                cx,
            ))
            .child(tool_button(
                "toolbar-archive",
                // A downward arrow into a bar: put this away. The sidebar's
                // own Archive mark is a striped box, which says "a box" and
                // not much else once it is out of a list of folder names.
                "\u{2913}",
                "Archive",
                has_message,
                false,
                Box::new(Archive),
                theme,
                cx,
            ))
            .child(separator(theme))
            .child(tool_button(
                "toolbar-new",
                "\u{270E}",
                "New message",
                true,
                false,
                Box::new(Compose),
                theme,
                cx,
            ))
            .child(separator(theme))
            .child(tool_button(
                "toolbar-reply",
                "\u{21A9}",
                "Reply",
                has_message,
                false,
                Box::new(Reply),
                theme,
                cx,
            ))
            .child(tool_button(
                "toolbar-reply-all",
                // A doubled arrow. Two of the reply glyph side by side reads
                // as a rendering accident rather than as a button, and every
                // "several people" glyph in Unicode is an emoji that would
                // arrive in colour.
                "\u{21D0}",
                "Reply all",
                has_message,
                false,
                Box::new(ReplyAll),
                theme,
                cx,
            ))
            .child(tool_button(
                "toolbar-forward",
                "\u{21AA}",
                "Forward",
                has_message,
                false,
                Box::new(Forward),
                theme,
                cx,
            ))
            .child(separator(theme))
            .child(tool_button(
                "toolbar-flag",
                FLAG,
                "Flag",
                has_message,
                // The one button that reports state as well as offering an
                // action: whether this message is flagged is worth seeing
                // without going to look at the row.
                flagged,
                Box::new(ToggleFlag),
                theme,
                cx,
            ))
            // No "syncing" chip here. A sync already announces itself in
            // three places — the bar at the foot of the sidebar, the dot on
            // each folder being fetched, and the folder counts moving — and a
            // fourth, in a row of buttons that are all things you can press,
            // reads as a control that does nothing.
            .children((self.showing_thread() && chips).then(|| {
                div()
                    .ml_2()
                    .px_1p5()
                    .rounded_sm()
                    .bg(theme.selected)
                    .text_size(theme.small_text_size)
                    .text_color(theme.accent)
                    .child("conversation")
            }))
            // The free space, with the field pinned to its right. The field
            // is sized *from* this rather than negotiating with the flex line:
            // it is exactly as wide as there is room for, up to the width it
            // wants — which is the whole of what stops it running off the edge
            // of a narrow window.
            .child(
                div().flex_1().min_w_0().flex().justify_end().child(
                div()
                    .id("search-field")
                    .flex()
                    .items_center()
                    .gap_2()
                    .w_full()
                    .max_w(px(280.))
                    // The floor is what still reads as a field: the glyph,
                    // and room for a word.
                    .min_w(px(84.))
                    // What is inside it is a line that can be longer than it
                    // is; the field is the window onto that line.
                    .overflow_hidden()
                    .px_2p5()
                    .py_1()
                    .rounded_md()
                    // Recessed against the panel it sits in.
                    .bg(theme.background)
                    .border_1()
                    .border_color(if searching { theme.focus_ring } else { theme.border })
                    .child(
                        div()
                            .flex_shrink_0()
                            // `⌕` sits small in its em, so it is set well
                            // above the text it labels; the pinned line height
                            // keeps it from making the field taller.
                            .text_size(px(17.))
                            .line_height(px(16.))
                            .text_color(theme.text_faint)
                            .child("\u{2315}"),
                    )
                    .child(search)
                    .children(searching.then(|| {
                        div()
                            .id("clear-search")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .text_color(theme.text_faint)
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.clear_search(window, cx)
                            }))
                            .child("\u{2715}")
                    })),
                ),
            )
            .children(self.settings.clients.then(|| clients_button(theme, cx)))
    }

    /// The status bar: position, activity, transient status, undo — and, in a
    /// debug build only, the performance readout. Timings are for whoever is
    /// working on the app; a release build neither records nor shows them.
    pub(crate) fn render_status_bar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let perf = &self.perf;

        let worst = perf.worst_frame();
        let frame_color = if worst > FRAME_BUDGET * 2 {
            theme.red
        } else if worst > FRAME_BUDGET {
            theme.yellow
        } else {
            theme.green
        };

        let position = if self.envelopes.is_empty() {
            String::new()
        } else if self.marked.len() > 1 {
            // With a multi-selection, how many are selected is the thing you
            // are about to act on — the cursor's position is not.
            format!(
                "{} of {} selected",
                format_count(self.marked.len()),
                format_count(self.envelopes.len())
            )
        } else {
            format!(
                "{} of {}",
                format_count(self.selected + 1),
                format_count(self.envelopes.len())
            )
        };

        div()
            .flex()
            .items_center()
            .gap_3()
            .h(px(24.))
            .flex_shrink_0()
            .px_3()
            .bg(theme.panel)
            .border_t_1()
            .border_color(theme.border)
            .text_size(theme.small_text_size)
            .text_color(theme.text_faint)
            .child(div().flex_shrink_0().child(position))
            .children(self.render_activity(theme))
            .child(
                // Transient feedback — "Archived …", an error — takes the
                // middle, where it is visible without displacing the metrics.
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(self.status.clone())),
            )
            // An undo you have to remember a keystroke for is one most people
            // never find, and filing mail is exactly where being able to say
            // "no, put that back" matters.
            .children((!self.undo_stack.is_empty()).then(|| {
                div()
                    .id("undo")
                    .flex_shrink_0()
                    .cursor_pointer()
                    .text_color(theme.accent)
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(|this, _, _, cx| this.undo_last_move(cx)))
                    .child("Undo")
            }))
            .when(cfg!(debug_assertions), |bar| bar.child(
                div()
                    .id("perf-readout")
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_shrink_0()
                    .font_family(theme.mono_font)
                    .child(
                        div()
                            .child(format!("query {}", format_duration(perf.last_query))),
                    )
                    .children((perf.last_open.as_micros() > 0).then(|| {
                        div().child(format!("open {}", format_duration(perf.last_open)))
                    }))
                    .child(
                        div().text_color(frame_color).child(format!(
                            "frame {} / max {}",
                            format_duration(perf.mean_frame()),
                            format_duration(worst),
                        )),
                    )
                    .child(
                        div()
                            .id("db-size")
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.text_muted))
                            .on_click(cx.listener(|this, _, _, cx| this.report_store_size(cx)))
                            .child(format!("{} indexed", format_count(perf.last_query_rows))),
                    ),
            ))
    }
}

impl Workspace {
    /// What the window is busy with, in the status bar.
    ///
    /// It used to live at the foot of the sidebar, which is always on screen —
    /// so a sync put a band of chrome under the folder list every time mail
    /// arrived, whether or not anybody wanted to watch it. Here it costs
    /// nothing when the status bar is off, which is the default, and is there
    /// to be looked at when it is on.
    ///
    /// Nothing at all when nothing is running. The bar is a readout of the
    /// window's state and an empty track reserved for news that has not
    /// arrived is a permanent fixture rather than a signal.
    ///
    /// The label is deliberately absent: the status text immediately to the
    /// right of this *is* the activity's label — the sync writes the same
    /// string to both — and showing it twice in a 24-point strip would be
    /// saying one thing with two thirds of the space.
    fn render_activity(&self, theme: &Theme) -> Option<AnyElement> {
        let activity = self.activities.current()?;
        let running = self.activities.len();

        // A sync is the one activity with a shape worth drawing: folders are
        // fetched over several connections, so there is a queue behind the
        // work and a wake in front of it. Everything else — a send, a folder
        // rename — is one thing happening once, and a bar split into three
        // parts would be two empty parts and a lie.
        let flow = match activity.kind {
            ActivityKind::Sync => {
                let counts = self.sync_flow.snapshot(self.syncing_folders.len(), Instant::now());
                counts.any().then_some(counts)
            }
            _ => None,
        };

        // What else is going on, for the tooltip. The sidebar used to expand
        // into a row per activity; a 24-point strip has nowhere to put a list,
        // and the note beside the bar can only say *how many* others there
        // are. Naming them costs no space at all.
        let others: Vec<String> = match running > 1 {
            true => self.activities.newest_first().map(|a| a.label.clone()).collect(),
            false => Vec::new(),
        };

        Some(
            div()
                .id("activity")
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                // Fixed, so the status text beside it does not shuffle
                // sideways every time the segments move.
                .w(px(96.))
                .when(!others.is_empty(), |this| {
                    let tip = SharedString::from(others.join("\n"));
                    this.tooltip(move |_, cx| cx.new(|_| Tip { text: tip.clone() }).into())
                })
                .child(div().flex_1().child(match flow {
                    Some(flow) => render_flow_bar(theme, flow),
                    None => render_progress_bar(theme, activity.fraction),
                }))
                .children(activity_note(activity.fraction, running).map(|note| {
                    div()
                        .flex_shrink_0()
                        .font_family(theme.mono_font)
                        .child(SharedString::from(note))
                }))
                .into_any_element(),
        )
    }
}

/// The note beside the bar.
///
/// A percentage when there is one, because it answers "how much longer" better
/// than the bar's own length does at this width. Otherwise a count of what
/// else is running — rare, and worth saying rather than silently hiding behind
/// the newest one. A free function so the rule can be tested: it is the only
/// judgement in the strip, and everything around it is layout.
fn activity_note(fraction: Option<f32>, running: usize) -> Option<String> {
    match (fraction, running) {
        (Some(fraction), _) => Some(format!("{}%", (fraction.clamp(0., 1.) * 100.).round() as u32)),
        (None, count) if count > 1 => Some(format!("+{}", count - 1)),
        (None, _) => None,
    }
}

/// The bar itself.
///
/// Determinate work fills from the left. Indeterminate work sweeps a short
/// segment across instead, because the alternative — a bar sitting at some
/// invented fraction — claims to know something it does not, and a bar sitting
/// at zero reads as stuck.
fn render_progress_bar(theme: &Theme, fraction: Option<f32>) -> AnyElement {
    let track = div()
        .relative()
        .h(px(3.))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(theme.border);

    match fraction {
        Some(fraction) => track
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .h_full()
                    .w(relative(fraction.clamp(0., 1.)))
                    .rounded_full()
                    .bg(theme.accent),
            )
            .into_any_element(),

        None => track
            .child(
                div()
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(relative(SWEEP_WIDTH))
                    .rounded_full()
                    .bg(theme.accent)
                    .with_animation(
                        "activity-sweep",
                        Animation::new(Duration::from_millis(1400))
                            .repeat()
                            .with_easing(ease_in_out),
                        |bar, delta| {
                            // Travels from fully off the left edge to fully
                            // off the right, so the segment enters and leaves
                            // rather than appearing and vanishing.
                            bar.left(relative(delta * (1. + SWEEP_WIDTH) - SWEEP_WIDTH))
                        },
                    ),
            )
            .into_any_element(),
    }
}

/// How much of the track the sweeping segment covers.
const SWEEP_WIDTH: f32 = 0.35;

/// How tall the flow bar is.
///
/// Twice the plain bar. Three segments and a seam between each pair need the
/// height to be read as regions rather than as one line that changes colour.
const FLOW_HEIGHT: f32 = 6.;

/// The three-part bar a sync gets instead of a progress bar.
///
/// Left to right: finished, being fetched, waiting. The order is the direction
/// the work travels, and it is the same direction a progress bar fills, so the
/// bar still reads as "further right is further along" even though none of the
/// three segments is a position.
///
/// What it is *not* is a fraction of the whole sync. The right-hand end is the
/// queue as it stands now, and the queue grows as accounts finish listing
/// their folders — so the boundaries move both ways, and the bar is a picture
/// of the present rather than a promise about the end.
fn render_flow_bar(theme: &Theme, flow: Flow) -> AnyElement {
    let (done, loading, pending) = flow.shares();

    div()
        .relative()
        .flex()
        .h(px(FLOW_HEIGHT))
        .w_full()
        .rounded_full()
        .overflow_hidden()
        .bg(theme.border)
        .child(
            // Finished, and the glow that rides at its head. The segment is
            // the anchor for the glow rather than the glow being placed on the
            // track, so the two cannot come apart as the boundary moves.
            div()
                .relative()
                .h_full()
                .w(relative(done))
                .bg(theme.accent)
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .h_full()
                        .w(px(2.))
                        .bg(fade(theme.text, flow.freshness * 0.85)),
                ),
        )
        .child(
            // Being fetched. The pulse is the one piece of motion: a bar of
            // three still colours cannot distinguish work in progress from
            // work that has stopped, and telling those apart is most of what
            // this is for.
            div()
                .h_full()
                .w(relative(loading))
                .bg(theme.accent_muted)
                .with_animation(
                    "sync-flow-loading",
                    Animation::new(Duration::from_millis(1100)).repeat().with_easing(ease_in_out),
                    |segment, delta| {
                        segment.opacity(0.5 + 0.5 * (delta * std::f32::consts::TAU).sin().abs())
                    },
                ),
        )
        // Waiting. Drawn rather than left as bare track so that a queue of
        // nothing and a queue of six hundred do not look the same at the
        // moment the other two segments happen to be small.
        .child(div().h_full().w(relative(pending)).bg(theme.border))
        .child(sheen(theme))
        .into_any_element()
}

/// A light travelling the length of the bar, over and over.
///
/// The segments only move when a folder starts or finishes, which during a
/// large folder is a bar that has not changed for two minutes — and a bar
/// sitting still looks exactly like a sync that has died. The sheen is what
/// says the thing is running: it is the only part that moves on its own, and
/// it costs the bar no width to say it.
///
/// Soft on both edges rather than a bar sliding past, because a hard edge
/// reads as an object crossing the track and this should read as light on it.
/// Two halves, since a gradient here takes one pair of stops.
fn sheen(theme: &Theme) -> impl IntoElement {
    let glow = fade(theme.text, 0.30);
    let clear = fade(theme.text, 0.);

    div()
        .absolute()
        .top_0()
        .h_full()
        .w(relative(SHEEN_WIDTH))
        .flex()
        .child(div().h_full().w(relative(0.5)).bg(gpui::linear_gradient(
            90.,
            gpui::linear_color_stop(clear, 0.),
            gpui::linear_color_stop(glow, 1.),
        )))
        .child(div().h_full().w(relative(0.5)).bg(gpui::linear_gradient(
            90.,
            gpui::linear_color_stop(glow, 0.),
            gpui::linear_color_stop(clear, 1.),
        )))
        .with_animation(
            "sync-flow-sheen",
            // Unhurried, and linear: an eased sweep slows at both ends, which
            // on a repeat looks like a pause rather than a rhythm.
            Animation::new(Duration::from_millis(1800)).repeat(),
            |light, delta| {
                // Fully off one end to fully off the other, so it enters and
                // leaves rather than appearing and vanishing mid-bar.
                light.left(relative(delta * (1. + SHEEN_WIDTH) - SHEEN_WIDTH))
            },
        )
}

/// How much of the bar the travelling light covers.
const SHEEN_WIDTH: f32 = 0.38;

/// A colour at a different alpha.
fn fade(color: Hsla, alpha: f32) -> Hsla {
    Hsla { a: alpha.clamp(0., 1.), ..color }
}


/// One toolbar button.
///
/// It dispatches the action rather than calling anything, so a button, its
/// keystroke and its menu item are three ways into one path — the same rule
/// the command palette follows.
///
/// Icon-only, so every one of them carries a tooltip. A row of glyphs nobody
/// can name is a row of glyphs nobody presses.
#[allow(clippy::too_many_arguments)]
fn tool_button(
    id: &'static str,
    glyph: &'static str,
    tip: &'static str,
    enabled: bool,
    active: bool,
    action: Box<dyn gpui::Action>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> Stateful<Div> {
    let color = match (enabled, active) {
        (false, _) => theme.text_faint.opacity(0.4),
        (true, true) => theme.accent,
        (true, false) => theme.text_muted,
    };

    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.))
        .h(px(24.))
        .flex_shrink_0()
        .rounded_md()
        // A shade larger than body text: these carry their meaning in their
        // shape, and at reading size the shapes are not distinct.
        .text_size(px(14.))
        .text_color(color)
        .when(enabled, |this| {
            this.cursor_pointer()
                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                .tooltip(move |_, cx| cx.new(|_| Tip { text: tip.into() }).into())
                .on_click(cx.listener(move |_, _, window, cx| {
                    window.dispatch_action(action.boxed_clone(), cx);
                }))
        })
        .child(glyph)
}

/// Clients, once it has been added: past the search field, apart from the
/// buttons that act on a message, because this one opens a window of its own.
///
/// The person mark from the reader's header rather than a glyph — the same
/// way in, so the same shape — and an svg takes its colour from itself, not
/// the row, which is why the hover reaches it through the group.
fn clients_button(theme: &Theme, cx: &mut Context<Workspace>) -> Stateful<Div> {
    div()
        .id("toolbar-clients")
        .group("toolbar-clients")
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.))
        .h(px(24.))
        .ml_1()
        .flex_shrink_0()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(theme.hover))
        .tooltip(|_, cx| cx.new(|_| Tip { text: "Clients".into() }).into())
        .on_click(cx.listener(|this, _, _, cx| this.open_clients(cx)))
        .child(
            gpui::svg()
                .path(crate::icons::PERSON)
                .size(px(15.))
                .text_color(theme.text_muted)
                .group_hover("toolbar-clients", |s| s.text_color(theme.text)),
        )
}

/// The flag, wherever one is drawn: the toolbar, a flagged row, and the
/// button that shows only flagged mail.
///
/// U+2690 rather than the filled U+2691: the system draws the filled one from
/// the emoji font, in blue, whatever the variation selector asks for — the
/// only colour in a row of monochrome glyphs, and unable to show state.
pub(crate) const FLAG: &str = "\u{2690}";

/// A small button that turns a way of looking at the list on and off: the
/// same glyph-in-a-square as the toolbar's, a size down to sit in the bar
/// above the list, and filled while it is on.
pub(crate) fn toggle_button(
    id: impl Into<gpui::ElementId>,
    glyph: SharedString,
    tip: SharedString,
    on: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(24.))
        .h(px(20.))
        .flex_shrink_0()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(13.))
        .when(on, |s| s.bg(theme.selected).text_color(theme.accent))
        .when(!on, |s| s.text_color(theme.text_muted).hover(|s| s.bg(theme.hover).text_color(theme.text)))
        .tooltip(move |_, cx| cx.new(|_| Tip { text: tip.clone() }).into())
        .child(glyph)
}

/// A hairline between groups of buttons, so eight glyphs read as four things.
fn separator(theme: &Theme) -> Div {
    div().w(px(1.)).h(px(14.)).mx_1().flex_shrink_0().bg(theme.border)
}

/// The label shown on hovering a toolbar button.
pub(crate) struct Tip {
    pub(crate) text: SharedString,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        div()
            .px_2()
            .py_0p5()
            .rounded_md()
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .text_size(theme.small_text_size)
            .text_color(theme.text)
            .child(self.text.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::activity_note;

    #[test]
    fn a_percentage_wins_because_it_answers_how_much_longer() {
        // The bar's own length says it too, but at 214 points wide it says it
        // vaguely, and "how much longer" is the question being asked.
        assert_eq!(activity_note(Some(0.42), 1).as_deref(), Some("42%"));
        assert_eq!(activity_note(Some(0.0), 1).as_deref(), Some("0%"));
        assert_eq!(activity_note(Some(1.0), 1).as_deref(), Some("100%"));
    }

    #[test]
    fn a_percentage_still_wins_when_other_things_are_running() {
        // The row is about the activity it is showing. What else is running
        // is the lesser fact, and there is only one slot.
        assert_eq!(activity_note(Some(0.42), 3).as_deref(), Some("42%"));
    }

    #[test]
    fn work_that_cannot_say_where_it_is_counts_what_else_is_going_on() {
        // Rare — two network operations at once means someone filed a folder
        // mid-sync — and worth saying rather than silently hiding behind the
        // newest one.
        assert_eq!(activity_note(None, 2).as_deref(), Some("+1"));
        assert_eq!(activity_note(None, 4).as_deref(), Some("+3"));
    }

    #[test]
    fn one_indeterminate_activity_has_nothing_to_add() {
        // "+0" is a slot filled with the absence of news.
        assert_eq!(activity_note(None, 1), None);
        assert_eq!(activity_note(None, 0), None);
    }

    #[test]
    fn a_fraction_outside_the_bar_is_not_reported_as_a_percentage_outside_it() {
        // The activity set clamps too, but the readout must not depend on
        // that: a bar showing 100% beside the text "170%" is worse than
        // either alone.
        assert_eq!(activity_note(Some(1.7), 1).as_deref(), Some("100%"));
        assert_eq!(activity_note(Some(-0.2), 1).as_deref(), Some("0%"));
    }
}
