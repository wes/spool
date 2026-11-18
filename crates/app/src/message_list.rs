//! The message list.
//!
//! This is where the performance claim is either true or it isn't. The list is
//! a gpui `uniform_list`: rows have a known fixed height, so it can compute
//! which slice of the mailbox is on screen and build elements for those rows
//! only. Whether the mailbox holds forty messages or four hundred thousand,
//! the same twenty-odd rows get built per frame.
//!
//! The consequence for this file is a rule: nothing in a row may cost more
//! than a few string operations. No allocation of the full list, no scans, no
//! database access. Everything a row draws is already sitting in the
//! `Envelope` the store handed us.

use std::rc::Rc;

use gpui::{Context, Render, SharedString, Window, div, prelude::*, px, svg, uniform_list};
use spool_core::{Envelope, MessageId, format_date_parts};

use crate::{
    clock::{local_offset, now_seconds},
    perf::format_count,
    scrollbar,
    settings::{Column, Columns, Density, Preview},
    theme::{Theme, ThemeAccess},
    workspace::Workspace,
};

/// The things every visible row needs that are the same for all of them.
///
/// Read once per visible range rather than per row \u{2014} and passed as one
/// value, because a row builder with eight parameters is one nobody can call
/// correctly.
struct RowContext<'a> {
    now: i64,
    list_focused: bool,
    /// Shared by reference: a drag payload per row must not copy the marked
    /// set per row.
    selection: &'a Rc<Vec<MessageId>>,
    theme: &'a Theme,
}

/// What follows the cursor while messages are being dragged.
///
/// Its own entity because gpui renders the drag image outside the element it
/// started from — by the time it is painted, the row may well be scrolled off
/// the screen.
pub struct DragGhost {
    pub label: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(theme.selected)
            .border_1()
            .border_color(theme.focus_ring)
            .text_size(theme.small_text_size)
            .text_color(theme.text)
            .child(self.label.clone())
    }
}

/// Width available to a detail row's text column: the pane, less its border,
/// the row's horizontal padding, and the unread dot with its gap.
///
/// Computed from the pane's current width rather than left to flex. gpui
/// truncates text only when the element's width is already definite when it is
/// measured; a width arriving from `flex_1` is not, and the measurement taken
/// during taffy's intrinsic-sizing pass gets cached, so the ellipsis never
/// appears and the text is merely clipped by the pane.
///
/// This is what makes the pane resizable: the number has to follow the pane,
/// and a constant would ellipsize at yesterday's width.
fn detail_text_width(list_width: f32) -> f32 {
    (list_width - 1. - 24. - 7. - 10.).max(40.)
}

/// Width a slim row has to lay columns out in: the pane, less its border and
/// the row's horizontal padding.
fn slim_row_width(list_width: f32) -> f32 {
    (list_width - 1. - 24.).max(40.)
}

/// Gap between slim columns, matching the row's `gap_2p5`.
const SLIM_GAP: f32 = 10.;

/// Names the scrollbar's hit strip so the thumb inside it can light up when
/// the pointer is anywhere on the bar, which is a wider target than the thumb.
const SCROLLBAR_GROUP: &str = "message-scrollbar";

/// Constrains an element to a single ellipsized line.
///
/// Deliberately not gpui's `truncate()`, which also sets `whitespace_nowrap`.
/// With wrapping off, gpui has no wrap width to key its text-measurement cache
/// on, so the size measured during taffy's intrinsic-sizing pass — taken
/// before any width is known — is reused forever and the ellipsis never
/// appears; the text just gets clipped by the pane. Clamping to one line keeps
/// wrapping enabled, so the cache is invalidated once the real width arrives.
fn one_line(element: gpui::Div) -> gpui::Div {
    element.overflow_hidden().text_ellipsis().line_clamp(1)
}

impl Workspace {
    pub(crate) fn render_message_list(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let count = self.envelopes.len();
        // Cloned into the row builder, which outlives this borrow of `theme`.
        let row_theme = theme.clone();
        let row_height = self.settings.messages.row_height();

        let list = uniform_list(
            "messages",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                let selection: Rc<Vec<MessageId>> = Rc::new(this.selection());
                let row_cx = RowContext {
                    now: now_seconds(),
                    list_focused: this.list_focus.is_focused(window),
                    selection: &selection,
                    theme: &row_theme,
                };

                range
                    .map(|ix| match this.envelopes.get(ix).cloned() {
                        Some(envelope) => {
                            this.render_row(ix, &envelope, &row_cx, cx).into_any_element()
                        }
                        None => div().h(row_height).into_any_element(),
                    })
                    .collect()
            }),
        )
        .track_scroll(self.list_scroll.clone())
        .flex_grow();

        div()
            .flex()
            .flex_col()
            .w(px(self.list_width()))
            .flex_shrink_0()
            .h_full()
            // No background of its own: the card it sits in already paints
            // one, with the rounded corner the list starts in. gpui clips a
            // child to its parent's rectangle and not to its corners, so a
            // fill here was a square drawn over that curve — visible as a
            // corner poking out past the border (wes/spool#1).
            .border_r_1()
            .border_color(theme.border)
            .child(self.render_list_header(theme, cx))
            .children(self.render_label_bar(theme, cx))
            .child(
                div()
                    .key_context("MessageList")
                    .track_focus(&self.list_focus)
                    // Clicking the list takes the keyboard back from the
                    // sidebar, which takes it when clicked in.
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, _| window.focus(&this.list_focus)),
                    )
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .when(count == 0, |this| this.child(self.render_empty_state(theme)))
                    .when(count > 0, |this| {
                        // The list and its scrollbar share a box, because the
                        // bar is positioned against the viewport's edges and
                        // the footer below must not count as part of it.
                        this.child(
                            div()
                                .relative()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_h_0()
                                .child(list)
                                .children(self.render_list_scrollbar(theme, cx)),
                        )
                    })
                    .children(self.render_load_older(theme, cx)),
            )
    }

    /// The message list's scrollbar.
    ///
    /// An overlay on the right edge rather than a column beside the list:
    /// that is what macOS draws, and it means the rows are the same width
    /// whether the bar is there or not — a list that reflowed the moment it
    /// grew past one screen would be worse than no bar at all.
    ///
    /// Nothing is drawn when the mailbox fits on screen, which is the other
    /// half of what makes it read as native.
    fn render_list_scrollbar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let (viewport, content, _) = self.list_viewport()?;
        let thumb = scrollbar::thumb(viewport, content, self.list_offset())?;
        // A thumb being dragged stays lit even when the pointer has wandered
        // off the bar, which it does constantly during a drag.
        let held = self.scroll_drag.is_some();

        Some(
            div()
                .group(SCROLLBAR_GROUP)
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .w(px(scrollbar::WIDTH))
                // The press belongs to the bar, not to whichever row it is
                // floating over.
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.begin_scrollbar_drag(event.position.y, cx);
                    }),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(thumb.top))
                        .right(px(scrollbar::THUMB_INSET))
                        .w(px(scrollbar::THUMB_WIDTH))
                        .h(px(thumb.height))
                        .rounded(px(scrollbar::THUMB_WIDTH / 2.))
                        .bg(if held { theme.scrollbar_hover } else { theme.scrollbar })
                        .when(!held, |this| {
                            this.group_hover(SCROLLBAR_GROUP, |s| s.bg(theme.scrollbar_hover))
                        }),
                ),
        )
    }

    /// The list's viewport height, the height of everything in it, and where
    /// the viewport starts in window coordinates.
    ///
    /// All three are measurements gpui took while laying the list out last
    /// frame; before the first one there is nothing to draw a scrollbar from.
    fn list_viewport(&self) -> Option<(f32, f32, f32)> {
        let state = self.list_scroll.0.borrow();
        let size = state.last_item_size?;
        Some((
            f32::from(size.item.height),
            f32::from(size.contents.height),
            f32::from(state.base_handle.bounds().origin.y),
        ))
    }

    /// How far the list is scrolled, counted downward from the top.
    ///
    /// gpui stores it as the offset of the first row, which grows negative on
    /// the way down; everything else here reads better the other way round.
    fn list_offset(&self) -> f32 {
        -f32::from(self.list_scroll.0.borrow().base_handle.offset().y)
    }

    /// Takes hold of the scrollbar at `y`, in window coordinates.
    ///
    /// A press on the thumb keeps the grip where it landed, so the thumb does
    /// not jump under the pointer. A press on the empty track jumps there —
    /// what macOS does by default, and the reason the bar is worth having on a
    /// mailbox of forty thousand messages.
    fn begin_scrollbar_drag(&mut self, y: gpui::Pixels, cx: &mut Context<Self>) {
        let Some((viewport, content, top)) = self.list_viewport() else { return };
        let Some(thumb) = scrollbar::thumb(viewport, content, self.list_offset()) else { return };

        let within = f32::from(y) - top - thumb.top;
        let grip = if (0. ..=thumb.height).contains(&within) {
            within
        } else {
            thumb.height / 2.
        };

        self.scroll_drag = Some(grip);
        self.drag_scrollbar_to(y, cx);
    }

    /// Applies a scrollbar drag. Called from the root, which is where the
    /// pointer is once it has left the seven pixels it grabbed.
    pub(crate) fn drag_scrollbar_to(&mut self, y: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(grip) = self.scroll_drag else { return };
        let Some((viewport, content, top)) = self.list_viewport() else { return };

        let offset = scrollbar::offset_for(viewport, content, f32::from(y) - top - grip);
        let handle = self.list_scroll.0.borrow();
        let x = handle.base_handle.offset().x;
        handle.base_handle.set_offset(gpui::point(x, px(-offset)));
        drop(handle);

        cx.notify();
    }

    /// Lets go of the scrollbar. Nothing to save: where a list is scrolled to
    /// is not a preference.
    pub(crate) fn end_scrollbar_drag(&mut self, cx: &mut Context<Self>) {
        if self.scroll_drag.take().is_some() {
            cx.notify();
        }
    }

    /// The foot of the list: how much of this folder is here, and a way to
    /// fetch more.
    ///
    /// Only for a single folder, and only when the server said it holds more
    /// than the local copy does. Both halves matter: "2,000 messages" with no
    /// context reads as "that is all your mail", which for a first sync of a
    /// decade-old archive is untrue in the most alarming direction.
    fn render_load_older(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let mailbox = self.open_mailbox()?;
        let missing = mailbox.not_downloaded()?;
        let here = mailbox.total;
        // A sync fills the folder on its own now. While one is running the
        // button would be an invitation to do what is already happening, so
        // the row is a progress line instead.
        let filling = self.syncing || self.backfilling;

        Some(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .text_size(theme.small_text_size)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "{} of {} here",
                            format_count(here as usize),
                            format_count((here + missing) as usize),
                        ))),
                )
                .child(
                    div()
                        .id("load-older")
                        .flex_shrink_0()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .when(!filling, |s| {
                            s.cursor_pointer()
                                .text_color(theme.text_muted)
                                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                                .on_click(cx.listener(|this, _, _, cx| this.load_older(cx)))
                        })
                        .when(filling, |s| s.text_color(theme.text_faint))
                        .child(match filling {
                            true => "Downloading\u{2026}".to_string(),
                            false => format!(
                                "Load {} older",
                                format_count(missing.min(1_000) as usize)
                            ),
                        }),
                ),
        )
    }

    /// The bar above the list: how many messages it holds, and — above the
    /// date column it controls — which way they run.
    fn render_list_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.envelopes.len();
        let filter = self.filter;
        let searching = !self.search_text.trim().is_empty();

        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_3()
            .h(px(34.))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_shrink()
                    .min_w_0()
                    .truncate()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    // Rows, and they are not always messages: with grouping
                    // on, "1,767 messages" over a list of 1,767 conversations
                    // covering 2,032 messages is simply the wrong number.
                    .child(match (searching, self.settings.messages.conversations) {
                        (true, _) => format!("{} results", format_count(count)),
                        (false, true) => format!("{} conversations", format_count(count)),
                        (false, false) => format!("{} messages", format_count(count)),
                    }),
            )
            // Beside the count, in this order: what narrows the list sits
            // next to the number it changes.
            .child(self.render_flag_toggle(theme, cx))
            .children(self.render_scan_switch(theme, cx))
            .child(div().flex_1())
            // An active filter shows itself here, and clicking clears it.
            // Cycling through the three lives on `tab` and in the menu, where
            // it cannot be hit by someone reaching for the sort control.
            // Flagged is left to the flag button, which already says so.
            .children((!matches!(filter, crate::workspace::Filter::All | crate::workspace::Filter::Flagged)).then(|| {
                div()
                    .id("filter-chip")
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(theme.small_text_size)
                    .bg(theme.selected)
                    .text_color(theme.accent)
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(|this, _, _, cx| this.clear_filter(cx)))
                    .child(filter.label())
            }))
            .child(
                // Sits over the date column, and flips the direction it
                // reads in — the only order a mailbox sorts by.
                div()
                    .id("sort-chip")
                    .px_1p5()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .hover(|s| s.bg(theme.hover).text_color(theme.text_muted))
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_sort(cx)))
                    .child(self.sort.label()),
            )
    }

    /// The flag button: shows only flagged mail while it is on.
    fn render_flag_toggle(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let on = self.filter == crate::workspace::Filter::Flagged;
        crate::chrome::toggle_button(
            "flag-filter",
            crate::chrome::FLAG.into(),
            if on { "Show all mail".into() } else { "Show flagged mail".into() },
            on,
            theme,
        )
        .on_click(cx.listener(|this, _, _, cx| this.toggle_flagged_filter(cx)))
        .into_any_element()
    }

    /// An extension's mailbox switch, beside the count in the header.
    ///
    /// Only in a single mailbox, because that is what the switch is for — or
    /// on the account row, which stands for its inbox.
    fn render_scan_switch(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let mailbox = self.scanned_mailbox()?;
        let host = self.extensions.as_ref()?;
        let installed = host.scanners().into_iter().next()?;
        let scan = installed.manifest.scan.clone()?;
        let extension = installed.id().to_string();
        let on = self.is_scanning(&extension, mailbox);

        // The manifest's glyph, else the title's first letter — something to
        // tell the button apart by, with the title itself on hover.
        let glyph: SharedString = match scan.glyph.as_deref().map(str::trim).filter(|g| !g.is_empty()) {
            Some(glyph) => glyph.to_string().into(),
            None => scan.title.chars().next().map(String::from).unwrap_or_default().into(),
        };
        let tip: SharedString = match on {
            true => format!("{} is on \u{2014} click to turn it off here", scan.title).into(),
            false => format!("Turn on {} for this mailbox", scan.title.to_lowercase()).into(),
        };
        Some(
            crate::chrome::toggle_button("scan-switch", glyph, tip, on, theme)
                .on_click(cx.listener(move |this, _, _, cx| this.set_scanning(&extension, !on, cx)))
                .into_any_element(),
        )
    }

    /// The strip under the header while an extension's switch is on for this
    /// mailbox: a chip per kind of message it tells apart, to narrow the list
    /// to it, and how far through the backlog it has got.
    ///
    /// A chip is shown for a kind there is at least one of, and for the one
    /// chosen even when a change has left it empty, so it can be unchosen.
    fn render_label_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let mailbox = self.scanned_mailbox()?;
        let host = self.extensions.as_ref()?;
        let installed = host.scanners().into_iter().next()?;
        let extension = installed.id().to_string();
        if !self.is_scanning(&extension, mailbox) {
            return None;
        }

        let chip = |id: SharedString, label: String, chosen: bool| {
            div()
                .id(id)
                .flex_shrink_0()
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .when(chosen, |s| s.bg(theme.selected).text_color(theme.accent))
                .when(!chosen, |s| s.text_color(theme.text_muted).hover(|s| s.bg(theme.hover).text_color(theme.text)))
                .child(label)
        };

        let mut bar = div()
            .id("label-bar")
            .flex()
            .items_center()
            .gap_1()
            .px_3()
            .h(px(30.))
            .flex_shrink_0()
            .overflow_x_scroll()
            .border_b_1()
            .border_color(theme.border_subtle);

        let chosen = self.label_filter.as_ref().filter(|(e, _)| *e == extension).map(|(_, f)| f.clone());
        bar = bar.child(
            chip("label-all".into(), "All".into(), chosen.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.set_label_filter(None, cx))),
        );

        for (filter, count) in &self.label_counts {
            let is_chosen = chosen.as_deref() == Some(filter.as_str());
            if *count == 0 && !is_chosen {
                continue;
            }
            let title = match installed.manifest.filters.iter().find(|f| f.id == *filter.as_str()) {
                Some(decl) => decl.title.clone(),
                None => match filter.strip_prefix("badge:") {
                    Some(badge) => badge.to_string(),
                    None => continue,
                },
            };
            let wanted = (extension.clone(), filter.clone());
            bar = bar.child(
                chip(format!("label-{filter}").into(), format!("{title} {}", format_count(*count)), is_chosen)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_label_filter(Some(wanted.clone()), cx))),
            );
        }

        if let Some((done, total)) = self.scan_progress.get(&(extension, mailbox)) {
            bar = bar.child(div().flex_1()).child(
                div()
                    .flex_shrink_0()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(format!("Scanning {} of {}\u{2026}", format_count(*done), format_count(*total))),
            );
        }

        Some(bar.into_any_element())
    }

    fn render_empty_state(&self, theme: &Theme) -> impl IntoElement {
        let searching = !self.search_text.trim().is_empty();
        let (title, hint) = if searching {
            ("No matches", "Try fewer words, or narrow with an operator")
        } else if self.label_filter.is_some() {
            ("None of these", "Nothing here is this kind of message \u{2014} choose All to see the rest")
        } else if self.is_first_run() {
            ("No mail yet", "Your mail shows here once an account is added")
        } else {
            match self.filter {
                crate::workspace::Filter::Unread => ("All caught up", "Nothing unread here"),
                crate::workspace::Filter::Flagged => ("Nothing flagged", "Press f to flag"),
                crate::workspace::Filter::All => ("Empty", "No messages in this mailbox"),
            }
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_1()
            .child(div().text_color(theme.text_muted).child(title))
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(hint),
            )
            // A search that found nothing is the moment the operators are
            // worth knowing about, and the only moment there is room to say
            // so. They come from the parser's own list, so this cannot drift
            // from what the search box actually understands.
            .children(searching.then(|| {
                div()
                    .mt_4()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_size(theme.small_text_size)
                    .children(spool_store::search::OPERATORS.iter().map(
                        |(operator, meaning)| {
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    div()
                                        .w(px(112.))
                                        .flex_shrink_0()
                                        .font_family(theme.mono_font)
                                        .text_color(theme.text_muted)
                                        .child(*operator),
                                )
                                .child(div().text_color(theme.text_faint).child(*meaning))
                        },
                    ))
            }))
    }

    /// One row: three lines, fixed height.
    ///
    /// Layout mirrors how the eye reads a mailbox — who, what, and a glance at
    /// the content.
    ///
    /// Unread is carried by the dot alone. Every row reads at the same size,
    /// weight and colour whether it has been opened or not: a list where half
    /// the lines are bolder and brighter than the other half is a list that is
    /// hard to scan for anything *except* unread, and the dot already answers
    /// that question without shouting it in four places at once.
    fn render_row(
        &self,
        index: usize,
        envelope: &Envelope,
        row_cx: &RowContext<'_>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let RowContext { now, list_focused, selection, theme } = *row_cx;
        let selected = self.is_selected(index);
        let settings = &self.settings.messages;
        let list_width = self.list_width();

        // Everything common to both shapes: the hit target, the selection
        // background, and the fixed height the virtualized list depends on.
        let row = div()
            .id(index)
            // Without an explicit width the row grows to fit its content,
            // which pushes the date column past the edge of the pane and turns
            // the snippet's ellipsis into a hard clip.
            .w_full()
            .h(settings.row_height())
            .overflow_hidden()
            .flex()
            .gap_2p5()
            .px_3()
            .border_b_1()
            .border_color(theme.border_subtle)
            // Selection is the only thing that colours a row. A mailbox is
            // read by running your eye down it, and a wash that follows the
            // mouse moves a second highlight around underneath the one that
            // means something — so the list answers "where am I" with one
            // mark, and it only moves when you move it.
            //
            // No pointer cursor either, for the same reason the Finder has
            // none over a file: a row is a thing being pointed at, not a link
            // being followed, and the arrow is what points.
            .when(selected && list_focused, |s| s.bg(theme.selected))
            .when(selected && !list_focused, |s| s.bg(theme.selected_inactive))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                let modifiers = event.modifiers();
                if modifiers.shift {
                    this.extend_to(index, cx);
                } else if modifiers.secondary() {
                    this.toggle_mark(index, cx);
                } else {
                    this.select(index, cx);
                    this.open_selected(cx);
                }
            }));

        // Filing by dragging onto a folder is the gesture people arrive with,
        // and the payload has to be built now: gpui takes the value at render
        // time, not when the drag starts.
        let row = match self.drag_payload(index, selection) {
            Some(payload) => {
                let label = payload.label.clone();
                row.on_drag(payload, move |_, _, _, cx| {
                    cx.new(|_| DragGhost { label: label.clone() })
                })
            }
            None => row,
        };

        // What an extension wants shown on this message, if one has said. A
        // map lookup against a cache filled off the UI thread — nothing here
        // enters a sandbox, and a machine with no extensions does not even
        // look.
        let marks = self
            .extensions
            .as_ref()
            .and_then(|host| host.decoration(envelope.id))
            .map(|decorations| crate::extension_ui::badges(&decorations.badges, theme))
            .unwrap_or_default();

        match settings.density {
            Density::Slim => {
                slim_row(row, envelope, now, &settings.columns, list_width, theme, marks)
                    .into_any_element()
            }
            Density::Detail => {
                detail_row(row, envelope, now, settings.preview, list_width, theme, marks)
                    .into_any_element()
            }
        }
    }
}

/// One line of columns.
fn slim_row(
    row: gpui::Stateful<gpui::Div>,
    envelope: &Envelope,
    now: i64,
    columns: &Columns,
    list_width: f32,
    theme: &Theme,
    marks: Vec<gpui::AnyElement>,
) -> impl IntoElement {
    let unread = envelope.any_unread();
    let accent = theme.avatar(envelope.from.color_seed());
    let subject: SharedString = if envelope.subject.trim().is_empty() {
        "(no subject)".into()
    } else {
        envelope.subject.clone().into()
    };

    // Widths are computed rather than left to flex for the reason stated at
    // ROW_TEXT_WIDTH: gpui only ellipsizes when the width is definite at
    // measurement time, and a width arriving from `flex_1` is not.
    let (from_width, subject_width) = slim_widths(columns, list_width);

    row.items_center()
        .py_1()
        .children(columns.shows(Column::Unread).then(|| {
            div()
                .flex_shrink_0()
                .size(px(UNREAD_WIDTH))
                .rounded_full()
                .when(unread, |s| s.bg(accent))
                .when(!unread, |s| s.bg(gpui::transparent_black()))
        }))
        .children(columns.shows(Column::Flag).then(|| {
            div()
                .w(px(FLAG_WIDTH))
                .flex_shrink_0()
                .flex()
                .items_center()
                .children(envelope.flags.is_flagged().then(|| flag_icon(theme.yellow)))
        }))
        .children(columns.shows(Column::From).then(|| {
            one_line(div().w(px(from_width)).flex_shrink_0())
                .text_color(theme.text)
                .child(senders(envelope))
        }))
        .children(columns.shows(Column::Subject).then(|| {
            one_line(div().w(px(subject_width)).flex_shrink_0())
                .flex()
                .items_baseline()
                .gap_1p5()
                .text_color(theme.text)
                // Ahead of the subject rather than after it: the subject is
                // what gets truncated, and a mark on the far side of the
                // ellipsis is a mark that disappears exactly when the column
                // is narrow enough for one to be useful.
                .children(reply_markers(envelope, theme))
                .child(div().min_w_0().truncate().child(subject))
                .children(conversation_count(envelope, theme))
                // After the subject and before the fixed columns, so a badge
                // takes room from the subject rather than pushing the date off
                // the edge of the pane.
                .children(marks)
        }))
        .children(columns.shows(Column::Attachments).then(|| {
            // A fixed slot whether or not this message has one, so the date
            // stays in the same place down the whole list.
            div()
                .w(px(ATTACHMENT_WIDTH))
                .flex_shrink_0()
                .flex()
                .items_center()
                .children(envelope.has_attachments.then(|| {
                    svg()
                        .path(crate::icons::PAPERCLIP)
                        .size(px(11.))
                        .text_color(theme.text_faint)
                }))
        }))
        .children(columns.shows(Column::Date).then(|| {
            timestamp(envelope, now, theme).w(px(DATE_WIDTH)).flex_shrink_0()
        }))
}

/// The flag on a flagged row: the toolbar's glyph, in the flag colour.
fn flag_icon(color: gpui::Hsla) -> gpui::Div {
    div().flex_shrink_0().text_size(px(11.)).text_color(color).child(crate::chrome::FLAG)
}

/// The little arrow that says this message has been dealt with.
///
/// `\u{21A9}` for a reply and `\u{21AA}` for a forward — the same pair of
/// hooked arrows every mail client has used for thirty years, pointing back at
/// the sender or on past them. Both when both happened, in that order, because
/// that is the order they happened in far more often than not.
///
/// Glyphs rather than artwork, unlike the paperclip: these two are drawn in
/// every system font, they carry no colour or perspective to smudge at eleven
/// points, and a hooked arrow is already the shape everyone means.
fn reply_markers(envelope: &Envelope, theme: &Theme) -> Option<gpui::Div> {
    let mark = reply_mark(envelope.flags)?;
    Some(div().flex_shrink_0().text_color(theme.text_faint).child(mark))
}

/// Which arrows a message has earned. Split out from the element so the
/// four cases can be read back in a test.
fn reply_mark(flags: spool_core::Flags) -> Option<&'static str> {
    match (flags.is_answered(), flags.is_forwarded()) {
        (false, false) => None,
        (true, false) => Some("\u{21A9}"),
        (false, true) => Some("\u{21AA}"),
        (true, true) => Some("\u{21A9}\u{21AA}"),
    }
}

/// When the message arrived: the day against the left of its column, the time
/// against the right.
///
/// Pushed apart rather than run together, because the times are the half worth
/// comparing and a ragged column of `Today 9:50pm` / `Aug 30 12:00pm` puts
/// them at a different place on every line. Held apart by a flexible gap, so
/// the day stays put as the column changes width.
///
/// `text_muted` rather than `text_faint`: it is the third thing anyone reads
/// on a row, after who and what, and one step down from the text is enough to
/// say so without making it work to read.
fn timestamp(envelope: &Envelope, now: i64, theme: &Theme) -> gpui::Div {
    let (day, time) = format_date_parts(envelope.date, now, local_offset(envelope.date));

    div()
        .flex()
        .items_baseline()
        .text_size(theme.small_text_size)
        .text_color(theme.text_muted)
        .child(div().flex_shrink_0().child(SharedString::from(day)))
        .child(div().flex_1())
        .child(div().flex_shrink_0().child(SharedString::from(time)))
}

/// Who the row is from: the conversation's participants, or one sender.
///
/// Named rather than numbered — "Ana, Bo, Cass" says more at a glance than
/// "3 people", and the count is next to the subject for the cases where it
/// does not fit.
fn senders(envelope: &Envelope) -> SharedString {
    match &envelope.group {
        Some(group) if group.participants.len() > 1 => group.participants.join(", ").into(),
        _ => envelope.from.short().to_string().into(),
    }
}

/// How many messages a conversation row stands for, or nothing for one.
///
/// Beside the subject rather than after the sender, because it belongs to the
/// conversation and the subject is what names it. Absent at one, which is most
/// rows: a column of "1"s is noise.
fn conversation_count(envelope: &Envelope, theme: &Theme) -> Option<gpui::Div> {
    let count = envelope.message_count();
    if count < 2 {
        return None;
    }

    Some(
        div()
            .flex_shrink_0()
            .text_size(theme.small_text_size)
            .text_color(theme.text_faint)
            .font_weight(gpui::FontWeight::NORMAL)
            .child(SharedString::from(count.to_string())),
    )
}

const UNREAD_WIDTH: f32 = 7.;
const FROM_WIDTH: f32 = 148.;
const DATE_WIDTH: f32 = 96.;
const ATTACHMENT_WIDTH: f32 = 14.;

/// The flag column: one icon, the same slot on every row whether or not
/// that message is flagged, so the sender's name starts in one place.
const FLAG_WIDTH: f32 = 11.;

/// Smallest a subject may be squeezed to before `From` starts giving way.
const SUBJECT_MIN: f32 = 90.;
/// Smallest a sender column may be squeezed to. Below this even initials stop
/// being useful.
const FROM_MIN: f32 = 64.;

/// How the two flexible columns share what the fixed ones leave.
///
/// `From` gives up its width before the subject does: a truncated name is
/// still recognisable, while a subject cut to three characters tells you
/// nothing. Both are returned together so their sum can be guaranteed not to
/// exceed the row — a resizable pane makes overflow reachable by dragging,
/// where a fixed one only ever hit it by turning every column on.
fn slim_widths(columns: &Columns, list_width: f32) -> (f32, f32) {
    let others: f32 = [
        (Column::Unread, UNREAD_WIDTH),
        (Column::Flag, FLAG_WIDTH),
        (Column::Date, DATE_WIDTH),
        (Column::Attachments, ATTACHMENT_WIDTH),
    ]
    .iter()
    .filter(|(column, _)| columns.shows(*column))
    .map(|(_, width)| width)
    .sum();

    let gaps = SLIM_GAP * (columns.count().saturating_sub(1)) as f32;
    let available = (slim_row_width(list_width) - others - gaps).max(0.);

    match (columns.shows(Column::From), columns.shows(Column::Subject)) {
        (true, true) => {
            let from = (available - SUBJECT_MIN).clamp(FROM_MIN, FROM_WIDTH).min(available);
            (from, available - from)
        }
        (true, false) => (available, 0.),
        (false, true) => (0., available),
        (false, false) => (0., 0.),
    }
}

/// Sender, subject, and optionally a preview of the body.
fn detail_row(
    row: gpui::Stateful<gpui::Div>,
    envelope: &Envelope,
    now: i64,
    preview: Preview,
    list_width: f32,
    theme: &Theme,
    marks: Vec<gpui::AnyElement>,
) -> impl IntoElement {
    let text_width = detail_text_width(list_width);
    let unread = envelope.any_unread();
    let accent = theme.avatar(envelope.from.color_seed());
    let subject: SharedString = if envelope.subject.trim().is_empty() {
        "(no subject)".into()
    } else {
        envelope.subject.clone().into()
    };

    row.items_start()
        .py_2()
            // The unread marker doubles as the sender's avatar color, so an
            // unread row reads at a glance without a legend.
            .child(
                div()
                    .flex_shrink_0()
                    .mt(px(4.))
                    .size(px(7.))
                    .rounded_full()
                    .when(unread, |s| s.bg(accent))
                    .when(!unread, |s| s.bg(gpui::transparent_black())),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w(px(text_width))
                    .flex_shrink_0()
                    .gap_px()
                    // Line one: sender and date.
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                one_line(div().flex_1().min_w_0())
                                    .text_color(theme.text)
                                    .child(senders(envelope)),
                            )
                            .children(reply_markers(envelope, theme))
                            .children(envelope.has_attachments.then(|| {
                                svg()
                                    .path(crate::icons::PAPERCLIP)
                                    .size(px(11.))
                                    .flex_shrink_0()
                                    .text_color(theme.text_faint)
                            }))
                            .children(envelope.flags.is_flagged().then(|| flag_icon(theme.yellow)))
                            .child(
                                timestamp(envelope, now, theme)
                                    .w(px(DATE_WIDTH))
                                    .flex_shrink_0(),
                            ),
                    )
                    // Line two: subject, and how many messages carry it.
                    .child(
                        one_line(div().w_full())
                            .flex()
                            .items_baseline()
                            .gap_1p5()
                            .text_color(theme.text)
                            .child(div().min_w_0().truncate().child(subject))
                            .children(conversation_count(envelope, theme))
                            .children(marks),
                    )
                    // Line three: the preview computed once, at ingest.
                    .children((preview.lines() > 0).then(|| {
                        div()
                            .w_full()
                            .overflow_hidden()
                            .text_ellipsis()
                            .line_clamp(preview.lines())
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(SharedString::from(envelope.snippet.clone()))
                    })),
            )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default slim width, which the width-dependent tests measure against.
    fn default_slim_width() -> f32 {
        crate::settings::PaneWidths::default().list_slim
    }

    fn envelope(from: &str, subject: &str) -> Envelope {
        Envelope {
            id: spool_core::MessageId(1),
            account_id: spool_core::AccountId(1),
            mailbox_id: spool_core::MailboxId(1),
            thread_id: spool_core::ThreadId(1),
            from: spool_core::Address::new(from, "someone@example.com"),
            subject: subject.into(),
            date: 0,
            snippet: String::new(),
            flags: spool_core::Flags::default(),
            has_attachments: false,
            size: 0,
            group: None,
        }
    }

    #[test]
    fn the_arrows_say_what_was_done_with_a_message() {
        use spool_core::Flags;

        assert_eq!(reply_mark(Flags::default()), None, "an untouched message is unmarked");
        assert_eq!(reply_mark(Flags(Flags::ANSWERED)), Some("\u{21A9}"));
        assert_eq!(reply_mark(Flags(Flags::FORWARDED)), Some("\u{21AA}"));

        // Replied to and then passed on: both, in the order they happened.
        assert_eq!(reply_mark(Flags(Flags::ANSWERED | Flags::FORWARDED)), Some("\u{21A9}\u{21AA}"));

        // The flags that say nothing about this say nothing about this.
        assert_eq!(reply_mark(Flags(Flags::SEEN | Flags::FLAGGED)), None);
    }

    #[test]
    fn a_conversation_row_names_who_is_in_it() {
        let mut row = envelope("Cass Ando", "Re: The proposal");
        row.group = Some(spool_core::Group {
            ids: vec![spool_core::MessageId(1), spool_core::MessageId(2)],
            unread: 0,
            participants: vec!["Ana".into(), "Bo".into(), "Cass".into()],
        });

        assert_eq!(senders(&row).to_string(), "Ana, Bo, Cass");
    }

    #[test]
    fn a_conversation_of_one_reads_as_its_sender() {
        // A thread with a single message is not a conversation, and listing
        // one participant where every other row shows a sender would be a
        // difference that means nothing.
        let mut row = envelope("Cass Ando", "The proposal");
        row.group = Some(spool_core::Group {
            ids: vec![spool_core::MessageId(1)],
            unread: 0,
            participants: vec!["Cass".into()],
        });

        assert_eq!(senders(&row).to_string(), envelope("Cass Ando", "").from.short());
        assert_eq!(row.message_count(), 1, "so no count is drawn either");
    }

    #[test]
    fn an_ungrouped_row_reads_as_its_sender() {
        let row = envelope("Cass Ando", "The proposal");
        assert_eq!(senders(&row).to_string(), row.from.short());
    }

    #[test]
    fn the_sender_column_gives_way_before_the_subject_does() {
        let columns = Columns::default();

        // With room to spare, From takes its preferred width and the subject
        // gets everything else.
        let (from, subject) = slim_widths(&columns, 700.);
        assert_eq!(from, FROM_WIDTH);
        assert!(subject > SUBJECT_MIN);

        // Squeezed, From is the one at its floor — it yields first.
        let (from, subject) = slim_widths(&columns, crate::settings::PaneWidths::LIST_RANGE.0);
        assert_eq!(from, FROM_MIN, "From should bottom out before the subject does");
        assert!(subject > 0., "the subject should still get something");

        // At the narrowest, with every column on, both are at their limits and
        // the subject dips under its preferred minimum. That is the honest
        // outcome of asking for five columns in 280 points, and it degrades
        // rather than overflowing.
        assert!(subject < SUBJECT_MIN);
    }

    #[test]
    fn the_two_flexible_columns_never_exceed_what_is_left() {
        // The property that makes dragging safe, checked across the whole
        // range a pane can be dragged to.
        for pane in [280., 320., 400., 560., 900.] {
            for mask in 0..(1 << Column::ALL.len()) {
                let columns = Columns {
                    unread: mask & 1 != 0,
                    from: mask & 2 != 0,
                    subject: mask & 4 != 0,
                    date: mask & 8 != 0,
                    attachments: mask & 16 != 0,
                    flag: mask & 32 != 0,
                };
                let (from, subject) = slim_widths(&columns, pane);
                assert!(from >= 0. && subject >= 0., "negative width at {pane}");
                assert!(
                    from + subject <= slim_row_width(pane) + 0.5,
                    "columns {columns:?} overflow a {pane}pt pane",
                );
            }
        }
    }

    #[test]
    fn the_subject_gets_the_room_left_over() {
        let pane = default_slim_width();
        let all = Columns::default();
        let width = slim_widths(&all, pane).1;

        // Every column on: what is left has to be enough to read a subject in.
        assert!(width > 200., "subject column collapsed to {width}");

        // Turning columns off gives the subject their width back.
        let fewer = Columns { from: false, ..all };
        assert!(slim_widths(&fewer, pane).1 > width);
    }

    #[test]
    fn the_columns_never_overflow_the_pane() {
        // Walk every combination, at the narrowest the pane can be dragged —
        // a layout that overflows turns the date column's ellipsis into a hard
        // clip past the edge of the pane.
        let pane = crate::settings::PaneWidths::LIST_RANGE.0;

        for mask in 0..(1 << Column::ALL.len()) {
            let columns = Columns {
                unread: mask & 1 != 0,
                from: mask & 2 != 0,
                subject: mask & 4 != 0,
                date: mask & 8 != 0,
                attachments: mask & 16 != 0,
                flag: mask & 32 != 0,
            };
            if columns.count() == 0 {
                continue;
            }

            let fixed: f32 = Column::ALL
                .iter()
                .filter(|c| columns.shows(**c))
                .map(|c| match c {
                    Column::Unread => UNREAD_WIDTH,
                    Column::Flag => FLAG_WIDTH,
                    Column::From => slim_widths(&columns, pane).0,
                    Column::Date => DATE_WIDTH,
                    Column::Attachments => ATTACHMENT_WIDTH,
                    Column::Subject => slim_widths(&columns, pane).1,
                })
                .sum();
            let gaps = SLIM_GAP * (columns.count() - 1) as f32;

            assert!(
                fixed + gaps <= slim_row_width(pane) + 0.5,
                "columns {columns:?} need {} of {}",
                fixed + gaps,
                slim_row_width(pane),
            );
        }
    }

    #[test]
    fn the_slim_pane_is_wider_because_its_columns_share_one_line() {
        let panes = crate::settings::PaneWidths::default();
        assert!(
            panes.list(Density::Slim) > panes.list(Density::Detail),
            "slim shares one line between five columns, so it needs the room",
        );
    }
}
