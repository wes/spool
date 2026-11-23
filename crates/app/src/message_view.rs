//! The reading pane.
//!
//! Mail is laid out with the same text system as everything else in the
//! window: no embedded browser, no per-message stylesheet fighting the theme.
//! A block's spans become one `StyledText` with highlight runs, which means a
//! paragraph is a single shaped line-wrapped text object rather than a row of
//! inline divs — that is what makes wrapping work and what keeps a long
//! message cheap to scroll.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, ClickEvent, Context, FontStyle, FontWeight,
    HighlightStyle, InteractiveText, SharedString, StyledText, UnderlineStyle, div, ease_in_out,
    Window, list, prelude::*, px, relative, svg,
};
use spool_core::{
    Envelope, MessageId, format_date_long, format_size, html,
    html::{Block, Span},
};

use spool_core::sanitize;

use crate::{
    images::{Embedded, ImageStore, Remote},
    text_area::{AreaStyle, ReadOnly, TextArea},
    theme::{self, Theme},
    workspace::{AttachmentLoad, AttachmentNote, AttachmentPress, NoteTone, OpenMessage, Workspace},
};

impl Workspace {
    /// Whether this message is shown in the web view rather than as blocks.
    ///
    /// Only for a single HTML message with nothing over it: a conversation is
    /// several messages and one native view, and a modal has to be able to
    /// cover the reader — a subview sits above everything gpui draws, so
    /// anything that opens over the message has to take it off screen first.
    #[cfg(target_os = "macos")]
    fn web_view_shows(&self, open: &OpenMessage) -> bool {
        self.web_view.is_some() && self.web_view_wanted(open)
    }

    /// Whether this message would go to the web view, were there one.
    #[cfg(target_os = "macos")]
    fn web_view_wanted(&self, open: &OpenMessage) -> bool {
        // The *effective* source, not the raw field: a message whose HTML
        // part is a wrapper around one line is read as plain text, and
        // belongs in the reader's own colours rather than on a rendered
        // white page.
        open.source == html::Source::Rich
            && !(self.showing_thread() && self.thread.len() > 1)
            && self.settings_ui.is_none()
            && self.palette.is_none()
            && self.folder_prompt.is_none()
    }

    /// The message, rendered by WebKit.
    ///
    /// The header, the image bar and the attachments stay gpui elements; only
    /// the body is the web view, and it takes the space that is left.
    #[cfg(target_os = "macos")]
    fn render_web_message(
        &self,
        open: &OpenMessage,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(web_view) = self.web_view.as_ref() else { return div().into_any_element() };

        let envelope = &open.envelope;
        let allowed = self.images.is_allowed(envelope.id, envelope.from.domain().as_deref());
        let remote = match allowed {
            true => sanitize::Remote::Allowed,
            false => sanitize::Remote::Blocked,
        };

        // Keyed on the message and on consent, which are the only two things
        // that change what document should be on screen.
        let key = (envelope.id.0 as u64) << 1 | allowed as u64;
        let body = &open.body;
        let loading = web_view.borrow_mut().show(key, || web_document(body, remote));

        // The view is hidden until WebKit has swapped documents, and nothing
        // else is going to ask for the frame that reveals it: gpui draws on
        // events, and a page finishing loading is not one it knows about.
        if loading {
            let web_view = web_view.clone();
            cx.spawn(async move |this, cx| {
                for _ in 0..128 {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(16))
                        .await;
                    let settled = web_view.borrow_mut().settled();
                    if this.update(cx, |_, cx| cx.notify()).is_err() || settled {
                        break;
                    }
                }
            })
            .detach();
        }

        let files: Vec<(usize, spool_core::Attachment)> =
            open.body.files().map(|(index, a)| (index, a.clone())).collect();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_header(envelope, &open.body, theme, window, cx))
            // Directly under who sent it, where it is seen; at the foot of a
            // long newsletter it was easy to never scroll to.
            .children((!files.is_empty()).then(|| {
                render_attachments(
                    envelope.id,
                    &files,
                    &open.stored_attachments,
                    self.attachment_note.as_ref(),
                    &self.attachment_progress,
                    theme,
                    cx,
                )
            }))
            .children(self.render_priority_bar(envelope.id, theme, cx))
            .children(self.render_extension_banners(envelope.id, theme))
            .children(self.render_unsubscribe_bar(envelope.id, theme, cx))
            .children(self.render_source_chip(open, theme, cx))
            .children(self.render_image_bar(open, theme, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(crate::webview::WebViewElement::new(web_view.clone())),
            )
            .into_any_element()
    }

    pub(crate) fn render_reader(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {

        // A marked set of more than one is what the toolbar will act on, so
        // it is what the reader has to describe. Showing one of the messages
        // instead — whichever the cursor happens to sit on — reads as "this is
        // the message", and archive then takes all twenty.
        let marked = self.marked_envelopes();
        if marked.len() > 1 {
            #[cfg(target_os = "macos")]
            if let Some(web_view) = self.web_view.as_ref() {
                web_view.borrow_mut().hide();
            }

            return div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(theme.background)
                .child(self.render_selection_summary(&marked, theme));
        }

        let content = match self.open.as_ref() {
            None => {
                #[cfg(target_os = "macos")]
                if let Some(web_view) = self.web_view.as_ref() {
                    web_view.borrow_mut().hide();
                }

                // A new install has no message to select, and "Select a
                // message" over an empty list is a dead end.
                if self.is_first_run() {
                    self.render_welcome(theme, cx)
                } else {
                    div()
                        .flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .child(div().text_color(theme.text_faint).child("Select a message"))
                        .into_any_element()
                }
            }

            #[cfg(target_os = "macos")]
            Some(open) if self.web_view_shows(open) => self.render_web_message(open, theme, window, cx),

            // An HTML message opened in the moment before the web view exists
            // — at launch, when the first message opens with the first list.
            // Blank for a frame or two, rather than drawn as blocks and then
            // replaced.
            #[cfg(target_os = "macos")]
            Some(open) if self.web_view_pending() && self.web_view_wanted(open) => {
                div().flex_1().into_any_element()
            }

            Some(open) => {
                // Nothing is going through the web view this frame, so it must
                // not be left sitting over the reader.
                #[cfg(target_os = "macos")]
                if let Some(web_view) = self.web_view.as_ref() {
                    web_view.borrow_mut().hide();
                }

                // In conversation mode the thread replaces the single message,
                // oldest first, so a reply reads in the order it was written.
                let messages: Vec<&OpenMessage> = if self.showing_thread() && !self.thread.is_empty() {
                    self.thread.iter().collect()
                } else {
                    vec![open]
                };
                let multiple = messages.len() > 1;
                let focused_id = open.envelope.id;

                if multiple {
                    // A conversation is laid out a message at a time, as it
                    // scrolls into view, rather than all at once: a long
                    // thread is twenty or thirty messages, nearly all of them
                    // below the fold, and laying every one of them out was
                    // most of what opening it cost.
                    let key = (focused_id, messages.len());
                    if self.reader_list_key.get() != Some(key) {
                        // A different conversation, or this one grew: from the
                        // top, oldest first, as it always opened.
                        self.reader_list.reset(messages.len());
                        self.reader_list_key.set(Some(key));
                    }
                    let theme = theme.clone();
                    list(
                        self.reader_list.clone(),
                        cx.processor(move |this: &mut Workspace, ix: usize, window, cx| {
                            let (Some(open), Some(message)) = (this.open.as_ref(), this.thread.get(ix))
                            else {
                                return div().into_any_element();
                            };
                            let focused = message.envelope.id == open.envelope.id;
                            this.render_message(message, true, focused, &theme, window, cx)
                        }),
                    )
                    .flex_1()
                    .min_h_0()
                    .into_any_element()
                } else {
                    let rendered = self.render_message(open, false, true, theme, window, cx);
                    div()
                        .id("reader-scroll")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(rendered)
                        .into_any_element()
                }
            }
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme.background)
            .child(content)
    }

    /// The rows currently marked, in list order.
    ///
    /// Read from the list rather than from the marked set: a mark on something
    /// the current filter no longer shows is not something the reader can
    /// describe, and counting it would put a number on screen that does not
    /// match what is highlighted.
    fn marked_envelopes(&self) -> Vec<&Envelope> {
        if self.marked.is_empty() {
            return Vec::new();
        }
        self.envelopes.iter().filter(|e| self.marked.contains(&e.id)).collect()
    }

    /// What is selected, in place of one of the messages that is.
    fn render_selection_summary(&self, marked: &[&Envelope], theme: &Theme) -> AnyElement {
        let rows = marked.len();
        // With conversations grouped, a row stands for a thread, and the two
        // numbers are both true and both worth saying: the toolbar acts on the
        // messages, and the list shows the rows.
        let messages: usize = marked.iter().map(|e| e.message_count()).sum();
        let oldest = marked.iter().map(|e| e.date).min().unwrap_or(0);
        let newest = marked.iter().map(|e| e.date).max().unwrap_or(0);

        let when = |ts: i64| {
            SharedString::from(format_date_long(ts, crate::clock::local_offset(ts)))
        };

        let span = match oldest == newest {
            // Everything selected landed in the same minute, so a range would
            // print the same thing twice.
            true => vec![("Sent", when(oldest))],
            false => vec![("Oldest", when(oldest)), ("Newest", when(newest))],
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .p_8()
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(format!(
                        "{} messages selected",
                        crate::perf::format_count(messages),
                    ))),
            )
            .children((messages != rows).then(|| {
                div().text_color(theme.text_muted).child(SharedString::from(format!(
                    "in {} conversations",
                    crate::perf::format_count(rows),
                )))
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .text_size(theme.small_text_size)
                    .children(span.into_iter().map(|(label, value)| {
                        div()
                            .flex()
                            .gap_3()
                            .child(
                                div()
                                    .w(px(52.))
                                    .flex_shrink_0()
                                    .text_right()
                                    .text_color(theme.text_faint)
                                    .child(label),
                            )
                            .child(div().text_color(theme.text_muted).child(value))
                    })),
            )
            .into_any_element()
    }

    fn render_message(
        &self,
        message: &OpenMessage,
        in_thread: bool,
        focused: bool,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let envelope = &message.envelope;
        let images = BlockImages {
            embedded: &message.embedded,
            store: &self.images,
            allowed: self.images.is_allowed(envelope.id, envelope.from.domain().as_deref()),
        };

        // An image the message draws is already visible in it; listing it
        // again as a file is noise. One that only looks embedded is a file
        // (see `Body::draws`).
        // Paired with its original index, because attachment bytes are keyed
        // by position in the message's *full* list and this one is filtered.
        let files: Vec<(usize, spool_core::Attachment)> =
            message.body.files().map(|(index, a)| (index, a.clone())).collect();

        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_6()
            .py_4()
            .max_w(px(760.))
            // A plain-text message was written in a monospace face and laid
            // out by hand — the quote markers, the underlines made of dashes,
            // the tables made of spaces. Rendering it proportionally throws
            // away the only layout it has.
            .when(message.source == html::Source::Plain, |this| {
                this.font_family(theme.mono_font)
            })
            .map(|this| match message.source {
                // One selectable text for the whole body, so it can be
                // selected across paragraphs and copied. HTML mail goes
                // through the web view, which selects on its own.
                html::Source::Plain => {
                    this.child(self.plain_body(message, theme, window, cx))
                }
                html::Source::Rich => this.children(
                    message
                        .blocks
                        .iter()
                        .enumerate()
                        // `filter_map`, because a blocked image with nothing
                        // to say about itself renders as no element at all —
                        // an empty one would still take the column's row gap.
                        .filter_map(|(ix, block)| render_block(ix, block, theme, &images)),
                ),
            });

        div()
            .flex()
            .flex_col()
            // Full width, said outright: in a conversation each message is
            // laid out on its own as the reader scrolls, and on its own a box
            // shrinks to fit its content — headers wrapped at whatever width
            // the shortest line gave them.
            .w_full()
            .when(in_thread, |s| s.border_b_1().border_color(theme.border))
            // In a conversation, the message you actually selected keeps a
            // subtle accent rail so it stays findable while you scroll.
            .when(in_thread && focused, |s| {
                s.border_l_2().border_color(theme.accent)
            })
            .child(self.render_header(envelope, &message.body, theme, window, cx))
            // Under the header, as in the web view: see `render_web_message`.
            .children((!files.is_empty()).then(|| {
                render_attachments(
                    envelope.id,
                    &files,
                    &message.stored_attachments,
                    self.attachment_note.as_ref(),
                    &self.attachment_progress,
                    theme,
                    cx,
                )
            }))
            .children(self.render_priority_bar(envelope.id, theme, cx))
            .children(self.render_extension_banners(envelope.id, theme))
            .children(self.render_unsubscribe_bar(envelope.id, theme, cx))
            .children(self.render_source_chip(message, theme, cx))
            .children(self.render_image_bar(message, theme, cx))
            .child(body)
            .into_any_element()
    }

    /// A plain-text body as one read-only text area.
    ///
    /// Kept as element state keyed on the message, so a selection survives
    /// the re-renders a sync or a timer causes, and goes away with the
    /// message.
    fn plain_body(
        &self,
        message: &OpenMessage,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = ("plain-body", message.envelope.id.0 as usize);
        plain_body_area(key, &message.blocks, theme, self.list_focus.clone(), window, cx)
    }

    /// A header line that can be selected and copied, the way the body under
    /// it can.
    ///
    /// The face and leading are spelled out rather than inherited, for the
    /// reason given on [`AreaStyle`]; they match what the line was drawn with
    /// before it was selectable.
    fn header_text(
        &self,
        key: (&'static str, usize),
        text: String,
        style: AreaStyle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let return_focus = self.list_focus.clone();
        let initial = (text.clone(), style.clone());
        let area = window.use_keyed_state(key, cx, move |window, cx| {
            let (text, style) = initial;
            let read_only = ReadOnly { return_focus: Some(return_focus), ..ReadOnly::default() };
            let mut area = TextArea::read_only(text, read_only, window, cx);
            area.set_style(style, cx);
            area
        });
        area.update(cx, |area, cx| {
            area.set_style(style, cx);
            if area.text() != text {
                area.set_text(text, cx);
            }
        });

        div().w_full().child(area).into_any_element()
    }

    fn render_header(
        &self,
        envelope: &Envelope,
        body: &spool_core::Body,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let accent = theme.avatar(envelope.from.color_seed());
        let id = envelope.id;
        let flagged = envelope.flags.is_flagged();

        let recipients = if body.to.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = body.to.iter().map(|a| a.full()).collect();
            format!("to {}", names.join(", "))
        };

        // The window's own leading, which the lines below were drawn with
        // before they were selectable.
        let line_height = px(18.);
        let subject = self.header_text(
            ("header-subject", id.0 as usize),
            if envelope.subject.trim().is_empty() {
                "(no subject)".to_string()
            } else {
                envelope.subject.clone()
            },
            AreaStyle {
                font_family: Some(theme.font.into()),
                font_size: Some(px(17.)),
                font_weight: Some(FontWeight::SEMIBOLD),
                line_height: Some(px(23.)),
                ink: Some(theme.text),
                ..AreaStyle::default()
            },
            window,
            cx,
        );
        let from = self.header_text(
            ("header-from", id.0 as usize),
            envelope.from.full(),
            AreaStyle {
                font_family: Some(theme.font.into()),
                font_size: Some(theme.text_size),
                font_weight: Some(FontWeight::MEDIUM),
                line_height: Some(line_height),
                ink: Some(theme.text),
                ..AreaStyle::default()
            },
            window,
            cx,
        );
        let to = self.header_text(
            ("header-to", id.0 as usize),
            recipients,
            AreaStyle {
                font_family: Some(theme.font.into()),
                font_size: Some(theme.small_text_size),
                line_height: Some(line_height),
                ink: Some(theme.text_faint),
                ..AreaStyle::default()
            },
            window,
            cx,
        );

        div()
            .flex()
            .flex_col()
            .gap_3()
            .px_6()
            .pt_5()
            .pb_3()
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(subject))
                    .children(self.render_extension_tags(id, theme)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        // Initials rather than a fetched avatar: no network
                        // request, no tracking pixel, and it is legible at
                        // 28 pixels.
                        div()
                            .flex_shrink_0()
                            .size(px(28.))
                            .rounded_full()
                            .bg(accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text_on_accent)
                            .child(SharedString::from(envelope.from.initials())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(from)
                            .child(to),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .children(self.render_client_chip(envelope, body, theme, cx))
                            .children(self.render_unsubscribe_button(envelope, body, theme, cx))
                            .child(
                                div()
                                    .id(("flag", id.0 as u64))
                                    .cursor_pointer()
                                    .px_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme.hover))
                                    .text_color(if flagged { theme.yellow } else { theme.text_faint })
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.toggle_flag_on_selected(cx)
                                    }))
                                    .child("\u{2691}"),
                            )
                            .child(
                                div()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .child(format_date_long(envelope.date, crate::clock::local_offset(envelope.date))),
                            ),
                    ),
            )
            .into_any_element()
    }
}

/// The mark on the save control.
///
/// A downward arrow to a line — the same shape every browser puts on a
/// download — as a text glyph rather than another icon file, because at this
/// size it is one stroke and a bar, and it is the only place it appears.
const SAVE_GLYPH: &str = "\u{2913}";

fn render_attachments(
    message: MessageId,
    attachments: &[(usize, spool_core::Attachment)],
    stored: &[usize],
    note: Option<&AttachmentNote>,
    loading: &std::collections::HashMap<(MessageId, usize), AttachmentLoad>,
    theme: &Theme,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
    let chips: Vec<_> = attachments
        .iter()
        .map(|(index, attachment)| {
            let index = *index;
            // Every chip is clickable. Anything past the size cap kept its
            // name and lost its bytes, and used to be drawn faded and inert —
            // which made a 15 MB PDF something the reader could tell you about
            // and not give you. It is fetched from the server instead, so the
            // only difference here is that it takes a moment and says so.
            let on_disk = stored.contains(&index);
            // Fetching already. The chip stops answering until it lands, so a
            // second press cannot start a second download of the same file.
            let progress = loading.get(&(message, index)).map(|load| load.fraction);
            let name = attachment.name.clone();
            let save_name = attachment.name.clone();

            div()
                .id(("attachment", index))
                .flex()
                .items_center()
                .gap_2()
                .px_2p5()
                .py_1p5()
                .rounded_md()
                .bg(theme.elevated)
                .border_1()
                .border_color(theme.border)
                .when(progress.is_none(), |s| {
                    // The chip itself previews. Looking is the common case by
                    // a wide margin — "what is this" gets asked of every
                    // attachment, "keep this" of a few — so it is the one that
                    // costs no aim.
                    // And it drags out: pressed and moved, it is a file
                    // Finder can take. See `attachment_drag`.
                    let drag_name = name.clone();
                    s.cursor_pointer()
                        .hover(|s| s.border_color(theme.accent))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, _, _| {
                                this.attachment_press = Some(AttachmentPress {
                                    message,
                                    index,
                                    name: drag_name.clone(),
                                    at: event.position,
                                });
                            }),
                        )
                        .on_mouse_move(cx.listener(|this, event, window, cx| {
                            this.drag_attachment_out(event, window, cx)
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.attachment_press = None;
                            this.preview_attachment(message, index, name.clone(), cx);
                        }))
                })
                .when(progress.is_some(), |s| s.border_color(theme.accent))
                .child(
                    svg()
                        .path(crate::icons::PAPERCLIP)
                        .size(px(13.))
                        .flex_shrink_0()
                        .text_color(theme.text_faint),
                )
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .child(SharedString::from(attachment.name.clone())),
                )
                .children(progress.map(|fraction| download_bar(theme, fraction)))
                .child({
                    // The note displaces the size while it is up. The size is
                    // what the chip says when it has nothing else to say, and
                    // "1.2 MB" is not the answer to a click.
                    let mine = note.filter(|n| n.message == message && n.index == index);
                    let (text, color) = match mine {
                        Some(note) => (
                            note.text.clone(),
                            match note.tone {
                                NoteTone::Working => theme.text_muted,
                                NoteTone::Done => theme.green,
                                NoteTone::Failed => theme.red,
                            },
                        ),
                        None if on_disk => (format_size(attachment.size), theme.text_faint),
                        // Still worth saying: it is the difference between
                        // opening at once and waiting on a download.
                        None => (
                            format!(
                                "{} \u{00b7} on the server",
                                format_size(attachment.size)
                            ),
                            theme.text_faint,
                        ),
                    };
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(color)
                        .child(SharedString::from(text))
                })
                // Saving needs its own target, now that the chip is a preview.
                // Drawn only when there is something to save: on an
                // attachment past the size cap it would be a button that
                // cannot work, next to text explaining why.
                .children(progress.is_none().then(|| {
                    div()
                        .id(("save-attachment", index))
                        .flex_shrink_0()
                        .px_1()
                        .rounded_sm()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.accent).bg(theme.hover))
                        .child(SharedString::from(SAVE_GLYPH))
                        // The chip underneath previews on click, and this sits
                        // inside it: without stopping here, saving would also
                        // open a preview of what it just saved.
                        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.save_attachment(message, index, save_name.clone(), cx);
                        }))
                }))
        })
        .collect();

    div()
        .flex()
        .flex_wrap()
        .gap_2()
        .px_6()
        .py_2p5()
        .border_b_1()
        .border_color(theme.border_subtle)
        .children(chips)
}

/// How far a download has got, on the chip that started it.
///
/// Determinate wherever it can be: the message's size was recorded by the sync
/// that listed it, and the transport counts what it reads, so this is a real
/// fraction rather than a spinner standing in for one. It falls back to a
/// sweep only before the first bytes land, or when the server never said how
/// big the message was.
fn download_bar(theme: &Theme, fraction: f32) -> AnyElement {
    let track =
        div().flex_shrink_0().w(px(46.)).h(px(3.)).rounded_full().overflow_hidden().bg(theme.border);

    match fraction > 0. {
        true => track
            .child(div().h_full().w(relative(fraction.clamp(0., 1.))).bg(theme.accent))
            .into_any_element(),
        false => track
            .child(div().h_full().w(relative(0.4)).bg(theme.accent).with_animation(
                "attachment-download",
                Animation::new(Duration::from_millis(1100)).repeat().with_easing(ease_in_out),
                |bar, delta| bar.ml(relative(delta * 0.6)),
            ))
            .into_any_element(),
    }
}

impl Workspace {
    /// A marker for a message being read as plain text.
    ///
    /// Only when the choice was real: showing "plain text" on a message that
    /// only ever had a plain body would be labelling the absence of an option.
    /// The most banners one message may be covered in.
    ///
    /// Three extensions each with something urgent to say is a reader with no
    /// message in it. Past this they are dropped rather than stacked, and the
    /// panel is where an extension says more.
    const MAX_BANNERS: usize = 3;

    /// What extensions want said about this message, above everything else.
    ///
    /// A map lookup against the decoration cache, exactly as a list row is: the
    /// reader must not be the one place that waits on a sandbox. `None` until
    /// an extension has been asked, which for a message you have just opened is
    /// very often the frame after.
    ///
    /// The banners the app raises itself — unsubscribe, remote images, the
    /// plain-text chip — come after these, because those are about the mail and
    /// these are about whether to read it at all.
    fn render_extension_banners(
        &self,
        message: spool_core::MessageId,
        theme: &Theme,
    ) -> Vec<gpui::AnyElement> {
        let Some(host) = &self.extensions else { return Vec::new() };
        let Some(decorations) = host.decoration(message) else { return Vec::new() };

        decorations
            .banners
            .iter()
            // Those go beside the subject: see `render_extension_tags`.
            .filter(|owned| !owned.banner.is_tags())
            .take(Self::MAX_BANNERS)
            .map(|owned| {
                crate::extension_ui::banner(&owned.banner, &owned.extension, theme)
                    .into_any_element()
            })
            .collect()
    }

    /// What extensions want said about this message in a word or two, drawn
    /// to the right of its subject: every banner that has no text.
    fn render_extension_tags(
        &self,
        message: spool_core::MessageId,
        theme: &Theme,
    ) -> Vec<gpui::AnyElement> {
        let Some(host) = &self.extensions else { return Vec::new() };
        let Some(decorations) = host.decoration(message) else { return Vec::new() };

        decorations
            .banners
            .iter()
            .filter(|owned| owned.banner.is_tags())
            .take(Self::MAX_BANNERS)
            .map(|owned| {
                crate::extension_ui::tags(&owned.banner, &owned.extension, theme)
                    .into_any_element()
            })
            .collect()
    }

    fn render_source_chip(
        &self,
        message: &OpenMessage,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if message.source != html::Source::Plain || !message.has_both_bodies() {
            return None;
        }

        Some(
            div()
                .flex()
                .mx_6()
                .mt_3()
                .child(
                    div()
                        .id("body-source")
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(theme.selected)
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dispatch_toggle_body_source(window, cx)
                        }))
                        .child("Plain text \u{00b7} show formatted"),
                )
                .into_any_element(),
        )
    }

    /// The bar offering to load a message's remote images.
    ///
    /// Shown only when there is a decision to make: a message with no remote
    /// images, or one whose images are already loaded, gets nothing. The one
    /// exception is a sender the reader has said yes to for good — that gets
    /// a line, because a standing decision you cannot find is one you cannot
    /// undo.
    fn render_image_bar(
        &self,
        message: &OpenMessage,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let count = message.remote_images().len();
        if count == 0 {
            return None;
        }

        let domain = message.envelope.from.domain();

        if self.images.trusts(domain.as_deref()) {
            return Some(self.render_trusted_sender_bar(domain?, theme, cx));
        }
        if self.images.is_allowed(message.envelope.id, domain.as_deref()) {
            return None;
        }

        Some(
            div()
                .flex()
                .items_center()
                .gap_3()
                .mx_6()
                .mt_3()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border)
                .text_size(theme.small_text_size)
                .child(div().text_color(theme.text_faint).child("\u{25A7}"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(match count {
                            1 => "1 image was not loaded. Loading it tells the sender you \
                                  opened this message."
                                .to_string(),
                            n => format!(
                                "{n} images were not loaded. Loading them tells the sender \
                                 you opened this message."
                            ),
                        })),
                )
                // The standing answer first and the one-off second, in that
                // order because the quiet option is the one being offered as
                // an aside — the button on the right is what a press of
                // return would have hit in a dialog, and "just this once" is
                // the safe thing to land on.
                .children(domain.map(|domain| {
                    div()
                        .id("always-load-images")
                        .flex_shrink_0()
                        .px_2p5()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(theme.text_muted)
                        .hover(|s| s.bg(theme.hover).text_color(theme.text))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.always_load_images_from_sender(cx)
                        }))
                        .child(SharedString::from(format!("Always for {domain}")))
                }))
                .child(
                    div()
                        .id("load-images")
                        .flex_shrink_0()
                        .px_2p5()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .bg(theme.accent)
                        .text_color(theme.text_on_accent)
                        .hover(|s| s.bg(theme.accent_muted))
                        .on_click(cx.listener(|this, _, _, cx| this.load_remote_images(cx)))
                        .child("Just this once"),
                )
                .into_any_element(),
        )
    }

    /// The line shown on mail from a sender whose images always load.
    ///
    /// Deliberately quiet — it is not asking anything — but present on every
    /// such message, because "back out of it again" has to be findable where
    /// the decision shows its effect, not only in a settings pane the reader
    /// would have to remember to go looking for.
    fn render_trusted_sender_bar(
        &self,
        domain: String,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_3()
            .mx_6()
            .mt_3()
            .px_3()
            .py_1p5()
            .text_size(theme.small_text_size)
            .child(div().text_color(theme.text_faint).child("\u{25A6}"))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!("Images load for {domain}."))),
            )
            .child(
                div()
                    .id("stop-loading-images")
                    .flex_shrink_0()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|s| s.bg(theme.hover).text_color(theme.text))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_loading_images_from_sender(cx)
                    }))
                    .child("Stop"),
            )
            .into_any_element()
    }
}

/// The message's own image parts as `data:` URLs, keyed by `Content-ID`.
///
/// This is what lets a web view show the images that arrived *with* the
/// message while every remote URL in it is still blocked: they cost nothing
/// and tell the sender nothing.
#[cfg(target_os = "macos")]
/// An HTML body as the web view is handed it: sanitized, remote content
/// loaded or not as `remote` says, and on a white page.
///
/// The reader's, and anything else that shows a message the way the reader
/// does — a client's history, in Clients.
pub(crate) fn web_document(body: &spool_core::Body, remote: sanitize::Remote) -> String {
    let inline = inline_data_urls(body);
    let html = sanitize::for_web_view(&body.html, remote, &inline);
    // Mail is written for a white page. A message that says otherwise — and a
    // status email usually does — overrides this in its own stylesheet, and
    // the ones that say nothing stay legible instead of inheriting a dark
    // background their black text vanishes into.
    sanitize::document(&html, "#ffffff", "#1f2328")
}

/// A plain-text body as one read-only text area, in the theme's colours.
///
/// Kept as element state under `key`, so a selection survives the re-renders
/// a sync or a timer causes, and goes away with the message.
pub(crate) fn plain_body_area(
    key: (&'static str, usize),
    blocks: &[Block],
    theme: &Theme,
    return_focus: gpui::FocusHandle,
    window: &mut Window,
    cx: &mut gpui::App,
) -> AnyElement {
    let (text, colors, links) = plain_document(blocks, theme);
    let style = AreaStyle {
        font_family: Some(theme.mono_font.into()),
        font_size: Some(theme.text_size),
        ink: Some(theme.text),
        ..AreaStyle::default()
    };
    let initial = style.clone();
    let area = window.use_keyed_state(key, cx, move |window, cx| {
        let read_only = ReadOnly { colors, links, return_focus: Some(return_focus) };
        let mut area = TextArea::read_only(text, read_only, window, cx);
        area.set_style(initial, cx);
        area
    });
    // The theme can change under an open message.
    area.update(cx, |area, cx| area.set_style(style, cx));

    div().w_full().child(area).into_any_element()
}

pub(crate) fn inline_data_urls(body: &spool_core::Body) -> std::collections::HashMap<String, String> {
    body.inline_images
        .iter()
        .map(|image| {
            (
                spool_core::normalize_content_id(&image.content_id),
                format!(
                    "data:{};base64,{}",
                    image.mime,
                    spool_net::encoding::base64(&image.bytes)
                ),
            )
        })
        .collect()
}

/// A plain-text message's blocks as one string, with the ranges to colour
/// and the ranges that are links.
///
/// Quote rails become `>` markers, which is what they were in the message
/// to begin with, and what a copied quote should still say.
fn plain_document(blocks: &[Block], theme: &Theme) -> PlainDocument {
    let mut text = String::new();
    let mut colors = Vec::new();
    let mut links = Vec::new();
    let mut previous: Option<&Block> = None;

    for block in blocks {
        let (prefix, spans, muted): (String, &[Span], bool) = match block {
            Block::Paragraph(spans) | Block::Heading { spans, .. } => (String::new(), spans, false),
            Block::Quote { depth, spans } => ("> ".repeat(*depth as usize), spans, true),
            Block::ListItem { depth, ordered, index, spans } => {
                let marker = if *ordered { format!("{index}.") } else { "\u{2022}".into() };
                (format!("{}{marker} ", "  ".repeat(*depth as usize)), spans, false)
            }
            Block::Rule => ("-- ".into(), &[][..], true),
            Block::Code(_) | Block::Table(_) | Block::Image(_) => continue,
        };

        if let Some(previous) = previous {
            // A list reads as a list with its items on consecutive lines;
            // anything else is a paragraph and gets a blank line.
            let both_items =
                matches!(previous, Block::ListItem { .. }) && matches!(block, Block::ListItem { .. });
            text.push_str(if both_items { "\n" } else { "\n\n" });
        }
        previous = Some(block);

        let start = text.len();
        text.push_str(&prefix);
        for span in spans {
            let from = text.len();
            text.push_str(&span.text);
            if let Some(url) = &span.link {
                links.push((from..text.len(), url.clone()));
                colors.push((from..text.len(), theme.accent));
            }
        }
        if muted && text.len() > start {
            colors.push((start..text.len(), theme.text_muted));
        }
    }

    (text, colors, links)
}

type PlainDocument = (String, Vec<(std::ops::Range<usize>, gpui::Hsla)>, Vec<(std::ops::Range<usize>, String)>);

/// Whether a whole block asked for a monospace face.
///
/// Per block rather than per span because a text run carries weight, colour
/// and slant but not a family — gpui shapes one family per element. Mail that
/// sets monospace usually sets it on everything (a status page, a log line, a
/// service name), so the block is the useful granularity and a mixed block
/// stays in the reading font rather than switching mid-sentence.
fn all_mono(spans: &[Span]) -> bool {
    !spans.is_empty() && spans.iter().all(|span| span.style.mono || span.text.trim().is_empty())
}

/// What a block needs to draw an image: what arrived with the message, what
/// has been fetched, and whether fetching was permitted at all.
pub(crate) struct BlockImages<'a> {
    pub embedded: &'a Embedded,
    pub store: &'a ImageStore,
    pub allowed: bool,
}

/// Renders one parsed block.
fn render_block(
    index: usize,
    block: &Block,
    theme: &Theme,
    images: &BlockImages<'_>,
) -> Option<gpui::AnyElement> {
    Some(match block {
        Block::Image(image) => return render_image(index, image, theme, images),
        Block::Table(table) => render_table(index, table, theme),

        Block::Rule => div()
            .h(px(1.))
            .my_2()
            .w(px(120.))
            .bg(theme.border)
            .into_any_element(),

        Block::Code(code) => div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border_subtle)
            .font_family(theme.mono_font)
            .text_size(px(12.))
            .text_color(theme.text)
            .child(SharedString::from(code.clone()))
            .into_any_element(),

        Block::Heading { level, spans } => {
            let size = match level {
                1 => 18.,
                2 => 16.,
                3 => 15.,
                _ => 14.,
            };
            div()
                .mt_2()
                .text_size(px(size))
                .line_height(px(size + 8.))
                .font_weight(FontWeight::SEMIBOLD)
                .when(all_mono(spans), |s| s.font_family(theme.mono_font))
                .child(styled_spans(index, spans, theme, theme.text))
                .into_any_element()
        }

        Block::Quote { depth, spans } => {
            // One rail per level of nesting, so a deep quote is visibly deep
            // without indenting the text off the pane.
            let rails = (*depth).min(4);
            let mut element = div()
                .flex()
                .gap_2()
                .text_color(theme.text_muted);

            for _ in 0..rails {
                element = element
                    .child(div().w(px(2.)).flex_shrink_0().rounded_full().bg(theme.border));
            }

            element
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(all_mono(spans), |s| s.font_family(theme.mono_font))
                        .child(styled_spans(index, spans, theme, theme.text_muted)),
                )
                .into_any_element()
        }

        Block::ListItem { depth, ordered, index: number, spans } => {
            let marker = if *ordered {
                format!("{number}.")
            } else {
                "\u{2022}".to_string()
            };

            div()
                .flex()
                .gap_2()
                .pl(px(*depth as f32 * 16.))
                .child(
                    div()
                        .w(px(16.))
                        .flex_shrink_0()
                        .text_color(theme.text_faint)
                        .child(marker),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(all_mono(spans), |s| s.font_family(theme.mono_font))
                        .child(styled_spans(index, spans, theme, theme.text)),
                )
                .into_any_element()
        }

        Block::Paragraph(spans) => div()
            .when(all_mono(spans), |s| s.font_family(theme.mono_font))
            .child(styled_spans(index, spans, theme, theme.text))
            .into_any_element(),
    })
}

/// Draws an image, or the honest placeholder for one that is not there.
///
/// A remote image before consent is a box with its alt text, not a gap: the
/// reader should be able to see that something was withheld and roughly what
/// it was.
fn render_image(
    index: usize,
    image: &html::Image,
    theme: &Theme,
    images: &BlockImages<'_>,
) -> Option<AnyElement> {
    // The sender's declared size is a hint, not an instruction: it is often
    // the intrinsic pixel size of a 2x asset, and always unconstrained by this
    // window. Cap it at the column width and let the aspect ratio follow.
    let width = image.width.map(|w| px((w as f32).min(MAX_IMAGE_WIDTH)));

    let ready = match &image.source {
        html::ImageSource::Remote(url) => match images.store.remote(url) {
            Some(Remote::Ready(image)) => Some(image.clone()),
            _ => None,
        },
        source => Embedded::key(source)
            .and_then(|key| images.embedded.get(&key))
            .cloned(),
    };

    if let Some(ready) = ready {
        return Some(
            div()
                .my_1()
                .child(
                    gpui::img(ready)
                        .max_w_full()
                        .when_some(width, |element, width| element.w(width))
                        .object_fit(gpui::ObjectFit::Contain),
                )
                .into_any_element(),
        );
    }

    let (glyph, note, color) = match &image.source {
        html::ImageSource::Remote(url) => match images.store.remote(url) {
            Some(Remote::Loading) => ("\u{25CC}", "Loading\u{2026}".to_string(), theme.text_faint),
            Some(Remote::Failed(why)) => ("\u{26A0}", why.clone(), theme.yellow),
            None if !images.allowed => {
                // Blocked, and the bar above already says how many. A named
                // image is worth a line; an unnamed one is a spacer or a
                // slice of a sliced banner, and a column of forty identical
                // boxes is worse than the gap they would leave.
                if image.alt.trim().is_empty() {
                    return None;
                }
                ("\u{25A7}", String::new(), theme.text_faint)
            }
            Some(Remote::Ready(_)) | None => {
                ("\u{25CC}", "Loading\u{2026}".to_string(), theme.text_faint)
            }
        },
        // Referenced but absent: the part did not arrive, or was too large to
        // keep. Saying so beats a blank space.
        _ => ("\u{26A0}", "Image not available".to_string(), theme.text_faint),
    };

    let label = match image.alt.trim() {
        "" => note,
        alt => alt.to_string(),
    };
    if label.is_empty() {
        return None;
    }

    // A row, so the placeholder is as wide as it needs to be rather than as
    // wide as the reader.
    Some(
        div()
        .flex()
        .my_1()
        .child(
            div()
                .id(("image-placeholder", index))
                .flex()
                .items_center()
                .gap_2()
                .max_w(px(360.))
                .px_2p5()
                .py_1()
                .rounded_md()
                .border_1()
                .border_dashed()
                .border_color(theme.border)
                .text_size(theme.small_text_size)
                .text_color(color)
                .child(div().flex_shrink_0().child(glyph))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .line_clamp(1)
                        .child(SharedString::from(label)),
                ),
        )
        .into_any_element(),
    )
}

/// The widest an image is allowed to draw, matching the text column.
const MAX_IMAGE_WIDTH: f32 = 700.;

/// Draws a data table as an aligned grid.
///
/// Columns share the width evenly rather than being measured: measuring means
/// laying the text out twice, and an even split reads correctly for the
/// receipts and summaries that reach this path.
fn render_table(index: usize, table: &html::Table, theme: &Theme) -> AnyElement {
    let columns = table.columns().max(1);

    let row = |key: usize, cells: &[html::Cell], header: bool| {
        let mut children: Vec<AnyElement> = Vec::with_capacity(columns);
        for column in 0..columns {
            let spans = cells.get(column).map(Vec::as_slice).unwrap_or_default();
            children.push(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .py_1()
                    // The first column is the label; the rest are usually
                    // figures, and figures read better against the number
                    // beside them when they hug the same edge.
                    .when(column > 0, |s| s.text_right())
                    .when(all_mono(spans), |s| s.font_family(theme.mono_font))
                    .child(styled_spans(
                        index * 64 + key * 8 + column,
                        spans,
                        theme,
                        if header { theme.text } else { theme.text_muted },
                    ))
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .items_start()
            .when(header, |s| {
                s.font_weight(FontWeight::MEDIUM).border_b_1().border_color(theme.border)
            })
            .when(!header && key > 0, |s| {
                s.border_t_1().border_color(theme.border_subtle)
            })
            .children(children)
    };

    div()
        .my_1()
        .flex()
        .flex_col()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .overflow_hidden()
        .text_size(theme.small_text_size)
        .children((!table.headers.is_empty()).then(|| row(0, &table.headers, true)))
        .children(
            table
                .rows
                .iter()
                .enumerate()
                .map(|(ix, cells)| row(ix + 1, cells, false)),
        )
        .into_any_element()
}

/// Turns a block's spans into one wrapped, styled, link-clickable text run.
///
/// Concatenating into a single string first is the important part: gpui shapes
/// and wraps it as one unit, so a bold word mid-sentence does not force a line
/// break the way separate inline elements would.
/// The text of a run of spans, its highlight ranges, and its link ranges.
///
/// Split out from the element it becomes so the ranges can be checked without
/// a window: gpui slices the string by them at shape time, so two that overlap
/// or one that lands mid-character is a panic in the text system rather than a
/// visual glitch — which is exactly what a badge that stopped short of the
/// space after it caused.
type Runs = (String, Vec<(std::ops::Range<usize>, HighlightStyle)>, Vec<(std::ops::Range<usize>, String)>);

fn runs_for(spans: &[Span], theme: &Theme, default_color: gpui::Hsla) -> Runs {
    let mut text = String::new();
    let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    let mut links: Vec<(std::ops::Range<usize>, String)> = Vec::new();

    for span in spans {
        let start = text.len();
        text.push_str(&span.text);
        let range = start..text.len();
        if range.is_empty() {
            continue;
        }

        let is_link = span.link.is_some();

        // A fill the sender drew behind words is a badge, blended into this
        // window rather than punched through it.
        let background = span
            .style
            .background
            .map(|fill| theme::fill(fill, theme.background))
            .or_else(|| span.style.code.then_some(theme.panel));

        // Text and fill were chosen together, so a coloured span sitting on a
        // fill is measured against that fill and not against the page.
        let surface = background.unwrap_or(theme.background);
        let color = match span.style.color {
            Some(color) if background.is_some() => theme::readable_on_fill(color, surface),
            Some(color) => theme::readable(color, surface),
            None if is_link => theme.accent,
            // A fill of their own but no colour to go on it: the theme's text
            // may be invisible against it.
            None if background.is_some() => theme::on_fill(surface),
            None => default_color,
        };

        let style = HighlightStyle {
            color: Some(color),
            font_weight: span.style.bold.then_some(FontWeight::BOLD),
            font_style: span.style.italic.then_some(FontStyle::Italic),
            background_color: background,
            // A link with a fill behind it is a button, and buttons are not
            // underlined.
            underline: (span.style.underline || (is_link && background.is_none())).then(|| {
                UnderlineStyle { thickness: px(1.), color: Some(color), wavy: false }
            }),
            strikethrough: span.style.strike.then(|| gpui::StrikethroughStyle {
                thickness: px(1.),
                color: Some(color),
            }),
            fade_out: None,
        };

        // A fill is painted behind the words and not behind the space that
        // follows them, so two buttons in a row do not merge into one pill.
        // The span is cut into up to three ranges rather than layered, because
        // these are consumed as a sequence and an overlapping pair produces
        // runs that slice the string in the wrong places.
        let bare = HighlightStyle { background_color: None, ..style };
        let words = match background.is_some() {
            true => {
                let leading = span.text.len() - span.text.trim_start().len();
                let trailing = span.text.len() - span.text.trim_end().len();
                (start + leading)..(range.end - trailing)
            }
            false => range.clone(),
        };

        if words.is_empty() || words == range {
            highlights.push((range.clone(), style));
        } else {
            if start < words.start {
                highlights.push((start..words.start, bare));
            }
            highlights.push((words.clone(), style));
            if words.end < range.end {
                highlights.push((words.end..range.end, bare));
            }
        }

        if let Some(url) = span.link.clone() {
            links.push((range, url));
        }
    }

    (text, highlights, links)
}

fn styled_spans(
    index: usize,
    spans: &[Span],
    theme: &Theme,
    default_color: gpui::Hsla,
) -> gpui::AnyElement {
    let (text, highlights, links) = runs_for(spans, theme, default_color);

    if text.is_empty() {
        return div().into_any_element();
    }

    let styled = StyledText::new(SharedString::from(text)).with_highlights(highlights);

    if links.is_empty() {
        return styled.into_any_element();
    }

    let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();

    InteractiveText::new(("body", index), styled)
        .on_click(ranges, move |clicked, _window, cx| {
            // Handing the URL to the browser is the safe default: this
            // renderer deliberately has no way to execute what it is given.
            if let Some(url) = urls.get(clicked) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use spool_core::html::{Rgba, Span, SpanStyle};

    use super::runs_for;
    use crate::theme::{Theme, ThemeId};

    fn span(text: &str, style: SpanStyle) -> Span {
        Span { text: text.into(), style, link: None }
    }

    fn badge() -> SpanStyle {
        SpanStyle {
            background: Some(Rgba { r: 0x2e, g: 0xb6, b: 0x7d, a: 255 }),
            color: Some(Rgba { r: 0xff, g: 0xff, b: 0xff, a: 255 }),
            bold: true,
            ..SpanStyle::default()
        }
    }

    /// gpui slices the string by these ranges when it shapes the line, so a
    /// pair that overlaps or a boundary inside a character is a panic in the
    /// text system rather than a visual glitch. This is the check that a badge
    /// stopping short of the space after it once failed.
    fn check(spans: &[Span]) -> String {
        let theme = Theme::new(ThemeId::Dark);
        let (text, highlights, links) = runs_for(spans, &theme, theme.text);

        let mut previous_end = 0usize;
        for (range, _) in &highlights {
            assert!(range.start >= previous_end, "{range:?} overlaps the run before it");
            assert!(range.start <= range.end && range.end <= text.len(), "{range:?} is out of bounds");
            assert!(text.is_char_boundary(range.start), "{range:?} starts inside a character");
            assert!(text.is_char_boundary(range.end), "{range:?} ends inside a character");
            previous_end = range.end;
        }
        for (range, _) in &links {
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
        }
        text
    }

    #[test]
    fn a_badge_beside_a_multibyte_character_keeps_its_boundaries() {
        // `↑` is three bytes, and it is what every stat in a status email is
        // written with. A range landing inside it aborts the process.
        let text = check(&[
            span("Open in Maple ", badge()),
            span("→", SpanStyle::default()),
            span(" 171.4K ↑ 89.1%", badge()),
        ]);
        assert_eq!(text, "Open in Maple → 171.4K ↑ 89.1%");
    }

    #[test]
    fn a_span_that_is_only_whitespace_is_still_one_run() {
        check(&[span("  ", badge()), span("\u{a0}", badge()), span("x", badge())]);
    }

    #[test]
    fn an_ordinary_run_of_spans_is_left_whole() {
        let theme = Theme::new(ThemeId::Dark);
        let spans = [span("plain ", SpanStyle::default()), span("bold", SpanStyle { bold: true, ..Default::default() })];
        let (_, highlights, _) = runs_for(&spans, &theme, theme.text);
        assert_eq!(highlights.len(), 2, "a span with no fill should not be cut up");
    }

    #[test]
    fn a_badge_paints_the_words_and_not_the_space_after_them() {
        let theme = Theme::new(ThemeId::Dark);
        let spans = [span("Resolved ", badge())];
        let (text, highlights, _) = runs_for(&spans, &theme, theme.text);

        assert_eq!(highlights.len(), 2);
        assert_eq!(&text[highlights[0].0.clone()], "Resolved");
        assert!(highlights[0].1.background_color.is_some());
        assert_eq!(&text[highlights[1].0.clone()], " ");
        assert!(highlights[1].1.background_color.is_none(), "the trailing space kept the fill");
    }
}
