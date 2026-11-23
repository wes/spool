//! The composer: writing and sending a message.
//!
//! The rules for *what* to send live in `spool_core::draft` and the rules for
//! how to encode it live in `spool_net::compose`, both already tested without
//! a UI. This file is the part that needs a window: fields, focus, and the
//! send, which is the only irreversible thing the app does.
//!
//! **A composer is a window of its own.** Writing a message is not a mode the
//! mailbox enters — it is a second thing you are doing, usually while looking
//! at the first. A panel over the list means you cannot read the message you
//! are answering, cannot start a second reply, and cannot put the draft on
//! another display, and people do all three constantly.
//!
//! So the composer is a root view rather than a modal, holding its own draft,
//! its own autosave and its own send. It talks back to the mailbox through a
//! weak handle, for the two things it cannot do alone: marking the message it
//! answered, and saying what happened.

use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use gpui::{
    AnyElement, App, Bounds, Context, Entity, ExternalPaths, FocusHandle, Focusable, ScrollHandle,
    SharedString, Subscription, Task, TitlebarOptions, WeakEntity, Window, WindowBounds, WindowHandle,
    WindowOptions, div, point, prelude::*, px, size,
};
use spool_core::{
    Address, MessageId, assist, format_size,
    draft::{self, Attached, Draft, Format, Original, ReplyKind},
    format_date_long,
};
use spool_store::{Store, suggest::Suggestion};

use crate::settings::Settings;

use crate::{
    actions::{
        AcceptCompletion, Cancel, CompletionNext, CompletionPrev, DecreaseComposeFont,
        DiscardDraft, IncreaseComposeFont, NextField, PrevField, ResetComposeFont, SendMessage,
    },
    menus::CloseWindow,
    text_area::{AreaChanged, AreaStyle, ImagePasted, TextArea},
    recipients::{RecipientField, RecipientsChanged},
    text_input::{InputChanged, TextInput},
    theme::{Theme, ThemeAccess, ThemeId},
    workspace::Workspace,
};

/// The page a message is written on.
///
/// White in every theme, which is the one place this app does not follow its
/// own palette: a message is composed on the page the *recipient* will read it
/// on, and that page is white in essentially every mail client there is.
/// Writing pale text on a dark ground and posting it into a white one is how
/// people end up sending unreadable mail without knowing it.
mod group;
pub(crate) use group::Group;

const PAGE: gpui::Hsla = gpui::Hsla { h: 0., s: 0., l: 1., a: 1. };

/// The ink on that page: the Light theme's own text colour, so the pairing is
/// one that was already chosen rather than invented here.
const INK: gpui::Hsla = gpui::Hsla { h: 0.593, s: 0.127, l: 0.139, a: 1. };

/// How long typing has to stop before the draft is written out.
///
/// Long enough that a fast typist writes one row rather than a hundred, short
/// enough that a crash costs a sentence at most.
const DRAFT_AUTOSAVE_DELAY: std::time::Duration = std::time::Duration::from_millis(700);

/// Which field the composer opens focused on.
///
/// A reply already knows its recipient, so the caret belongs in the body; a new
/// message does not, so it belongs in the To field. Getting this wrong means
/// every reply starts with a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Recipients,
    /// For a message whose recipients are already decided.
    Subject,
    Body,
}

pub struct Composer {
    to: Entity<RecipientField>,
    cc: Entity<RecipientField>,
    bcc: Entity<RecipientField>,
    subject: Entity<TextInput>,
    body: Entity<TextArea>,
    /// The box the body scrolls in, which the body keeps its caret inside.
    body_scroll: ScrollHandle,
    /// How long this waits for an answer once sent, in days, or `None` for
    /// not at all. Only while follow-up reminders are on.
    remind: Option<u32>,
    /// Whether `remind` was changed by hand from the default.
    remind_chosen: bool,
    /// Whether follow-up reminders were on when this opened, which is
    /// whether it offers one.
    reminders: bool,

    /// Who this is from — fixed at open time from the account, so a draft can
    /// never name a sender the account is not entitled to use.
    from: Address,
    in_reply_to: Option<String>,
    references: Vec<String>,
    /// The message being answered, so the mailbox can mark it answered once
    /// the send succeeds.
    replying_to: Option<MessageId>,
    /// The message being passed on, so the mailbox can mark it forwarded once
    /// the send succeeds.
    forwarding: Option<MessageId>,
    /// Text or HTML, as it will be sent.
    format: Format,
    /// The HTML of what is being answered or passed on; see
    /// [`Draft::quoted_html`].
    quoted_html: Option<String>,
    /// The body as edited in the HTML editor; see [`Draft::html`].
    html: Option<String>,
    /// The text of the writing part of the HTML editor, as the page last
    /// reported it — what the writing tools work on in HTML.
    writing_text: String,
    /// The HTML editor, while the message is being written as HTML.
    #[cfg(target_os = "macos")]
    editor: Option<crate::html_editor::SharedEditor>,
    /// Empties the editor's inbox while there is an editor.
    editor_poll: Option<Task<()>>,
    /// Whether the editor takes the keyboard when it is built.
    focus_editor: bool,
    /// Whether the last thing the writing tools did was in the HTML editor,
    /// where taking it back is the page's own undo.
    html_undo: bool,

    /// The files travelling with this message.
    ///
    /// Bytes, read at the moment they were attached — see `draft::Attached`.
    /// Held here rather than on the stored draft so that attaching a file to a
    /// composer that has never been saved works like attaching one to a
    /// composer that has.
    attachments: Vec<Attached>,
    /// The last file that could not be read, kept on screen until the next
    /// attempt.
    attach_error: Option<String>,
    /// Whether the files differ from what is stored.
    ///
    /// Tracked so an autosave can say so. Attachments are the largest thing on
    /// a draft and the autosave fires every few seconds while somebody types;
    /// rewriting them because a word changed is a stall in the one place this
    /// app must not have one. See `spool_store::DraftFiles`.
    attachments_changed: bool,

    /// Whether Cc is showing. Hidden until asked for, because most messages
    /// do not need it and an empty field is one more thing to look past.
    show_cc: bool,
    show_bcc: bool,
    sending: bool,
    /// The last failure, kept on screen until the next attempt.
    error: Option<String>,

    /// The draft this composer opened with.
    ///
    /// What counts as "nothing worth saving" is not an empty message — a reply
    /// opens with recipients, a quote and a signature already in it. It is a
    /// message nobody has touched, which is exactly this.
    opened: Draft,

    /// The stored row behind this composer, so an autosave replaces the draft
    /// rather than leaving a copy per keystroke.
    draft_id: Option<i64>,
    /// Every address this window could send as, in the sidebar's order.
    ///
    /// Handed over at construction rather than read from the config while
    /// drawing: a composer is a window of its own and the list does not change
    /// under it, and reading a file every frame to fill a menu that is usually
    /// closed is work for nothing.
    identities: Vec<Address>,
    /// Whether the From menu is open.
    picking_from: bool,
    store: Store,
    /// The mailbox. Weak, because a composer must not be the reason the main
    /// window's state stays alive.
    workspace: WeakEntity<Workspace>,
    /// Who it goes to, when it is to a group of clients rather than to the
    /// people in To. See [`Group`].
    group: Option<Group>,

    /// This composer's window, so it can close itself when a send lands and
    /// title itself as the subject is typed.
    window: Option<WindowHandle<Self>>,
    /// Whether the draft has already been dealt with, shared with the
    /// window's close hook.
    ///
    /// Outside the entity on purpose: the hook runs while the window is being
    /// torn down, and by then reaching back into the view it belongs to is
    /// either an error or a use of something half-gone.
    settled: std::rc::Rc<std::cell::Cell<bool>>,
    /// The title currently on the window, so it is only set when it changes
    /// rather than on every frame.
    titled: Option<String>,

    focus_handle: FocusHandle,
    /// Point size of the body.
    ///
    /// Held here rather than read from the workspace each frame, and that is
    /// not a caching decision: the composer draws while the workspace is
    /// mid-update — a settings change notifies it, which repaints every
    /// window — and reading an entity that is already being updated is a
    /// panic, not a stale value.
    text_size: f32,
    /// The recipient suggestions currently on offer, if any.
    completion: Option<Completion>,
    /// The pending suggestion query. Dropped by the next keystroke, which is
    /// what keeps one query in flight per pause rather than one per letter.
    completion_task: Option<Task<()>>,
    /// The pending autosave. Held so a new keystroke can drop it, which is the
    /// whole of the debounce.
    save_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    /// The writing tool that is running, and its request. Dropping the task
    /// is what cancels it.
    assist: Option<(Job, Task<()>)>,
    /// The line for describing an email to have written, while it is open.
    writer: Option<Writer>,
    /// What the last tool had to say instead of a change — that it failed,
    /// or found nothing to change — and whether it was a failure.
    assist_note: Option<(String, bool)>,
    /// The body before and after the last tool changed it.
    ///
    /// Undo is offered only while the body is still exactly what the tool
    /// left: once somebody has typed into it, putting the old text back would
    /// throw their typing away along with the tool's.
    assist_undo: Option<(String, String)>,
    /// The subject before a written email filled it in, for the same undo.
    subject_undo: Option<String>,
    _subscriptions: Vec<Subscription>,
}

/// What the AI is doing for the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    /// Changing what is there.
    Tool(assist::Tool),
    /// Writing it from a description.
    Write,
}

/// The line for describing an email and having it written.
struct Writer {
    input: Entity<TextInput>,
    /// Directions for this reply in particular, once they have been thought
    /// of. `None` for a new message, and while asking.
    suggestions: Option<Vec<String>>,
    /// Whether those are still being asked for.
    thinking: bool,
    _suggesting: Option<Task<()>>,
}

/// Which recipient field a completion belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipients {
    To,
    Cc,
    /// Delivered to, and named nowhere: a blind copy is an envelope
    /// recipient with no header of its own. See [`spool_net::compose`].
    Bcc,
}

/// The suggestion list under one of the recipient fields.
struct Completion {
    field: Recipients,
    matches: Vec<Suggestion>,
    selected: usize,
}

/// How many suggestions are offered.
///
/// Short on purpose: a list you have to read is slower than typing the
/// address, and the ranking is only trustworthy at the top.
const SUGGESTIONS: usize = 6;

/// How long the field has to be still before it is asked.
///
/// The query reads several thousand messages, so it should happen between
/// words rather than between letters.
const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(120);

impl Composer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        draft: &Draft,
        focus: Focus,
        draft_id: Option<i64>,
        identities: Vec<Address>,
        store: Store,
        workspace: WeakEntity<Workspace>,
        group: Option<Group>,
        text_size: f32,
        reminders: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let to = cx.new(|cx| {
            let mut field = RecipientField::new("Recipients", cx);
            field.set_addresses(&draft.to, cx);
            field
        });
        let cc = cx.new(|cx| {
            let mut field = RecipientField::new("Cc", cx);
            field.set_addresses(&draft.cc, cx);
            field
        });
        let bcc = cx.new(|cx| {
            let mut field = RecipientField::new("Bcc \u{2014} nobody else sees these", cx);
            field.set_addresses(&draft.bcc, cx);
            field
        });
        let subject = cx.new(|cx| {
            let mut input = TextInput::new("Subject", cx);
            input.set_content(draft.subject.clone(), cx);
            input
        });
        let body_scroll = ScrollHandle::new();
        let body = cx.new(|cx| {
            let mut area = TextArea::new("Write your message\u{2026}", cx);
            area.follow_with(body_scroll.clone());
            area.set_style(body_style(text_size, cx), cx);
            area.set_text(draft.body.clone(), cx);
            // A reply's body opens with two blank lines above the quote; the
            // caret belongs on the first of them, not after the quoted text.
            area.move_to_start(cx);
            area
        });

        // Any edit clears a stale failure — the message on screen should
        // describe the draft as it is now, not as it was when it last failed —
        // and starts the clock on an autosave.
        let mut subscriptions = Vec::new();
        for (field, which) in
            [(&to, Recipients::To), (&cc, Recipients::Cc), (&bcc, Recipients::Bcc)]
        {
            subscriptions.push(cx.subscribe(
                field,
                move |this: &mut Self, field, _: &RecipientsChanged, cx| {
                    this.edited(cx);
                    // The token is what completion answers, and the field owns
                    // it: everything before it is a pill and already decided.
                    let token = field.read(cx).token(cx);
                    this.suggest(which, &token, cx);
                },
            ));
        }
        subscriptions.push(cx.subscribe(&subject, |this: &mut Self, _, _: &InputChanged, cx| {
            this.edited(cx);
        }));
        subscriptions.push(cx.subscribe(&body, |this: &mut Self, _, _: &AreaChanged, cx| {
            this.edited(cx);
        }));
        // Signing in from Settings shows the tools here without the composer
        // being reopened.
        subscriptions.push(cx.observe_global::<crate::assist::Assistant>(|_, cx| cx.notify()));
        // A screenshot on the clipboard is an attachment. The body cannot show
        // a picture and has nowhere to put one, so it hands it up here.
        subscriptions.push(cx.subscribe(
            &body,
            |this: &mut Self, _, pasted: &ImagePasted, cx| {
                this.attach_image(&pasted.0, cx);
            },
        ));

        match focus {
            Focus::Recipients => {
                let handle = to.read(cx).focus_handle(cx);
                window.focus(&handle);
            }
            Focus::Subject => {
                let handle = subject.read(cx).focus_handle.clone();
                window.focus(&handle);
            }
            Focus::Body => {
                let handle = body.read(cx).focus_handle.clone();
                window.focus(&handle);
            }
        }

        Self {
            to,
            cc,
            bcc,
            subject,
            body,
            body_scroll,
            remind: reminders.then_some(spool_core::follow_up::DEFAULT_WAIT),
            remind_chosen: false,
            reminders,
            from: draft.from.clone(),
            in_reply_to: draft.in_reply_to.clone(),
            references: draft.references.clone(),
            replying_to: draft.replying_to,
            forwarding: draft.forwarding,
            format: draft.format,
            quoted_html: draft.quoted_html.clone(),
            html: draft.html.clone(),
            writing_text: String::new(),
            #[cfg(target_os = "macos")]
            editor: None,
            editor_poll: None,
            focus_editor: focus == Focus::Body,
            html_undo: false,
            attachments: draft.attachments.clone(),
            attach_error: None,
            // False even when the composer opens with files — a forward, or a
            // draft read back from the store. The first save writes them
            // regardless, because a draft with no row yet has nothing stored
            // for them to be in step with.
            attachments_changed: false,
            show_cc: !draft.cc.is_empty(),
            show_bcc: !draft.bcc.is_empty(),
            sending: false,
            error: None,
            opened: draft.clone(),
            draft_id,
            identities,
            picking_from: false,
            store,
            workspace,
            group,
            window: None,
            settled: std::rc::Rc::new(std::cell::Cell::new(false)),
            titled: None,
            focus_handle: cx.focus_handle(),
            text_size,
            completion: None,
            completion_task: None,
            save_task: None,
            send_task: None,
            assist: None,
            writer: None,
            assist_note: None,
            assist_undo: None,
            subject_undo: None,
            _subscriptions: subscriptions,
        }
    }

    /// Reads the fields back into a draft.
    fn draft(&self, cx: &App) -> Draft {
        Draft {
            attachments: self.attachments.clone(),
            from: self.from.clone(),
            to: self.to.read(cx).addresses(cx),
            cc: self.cc.read(cx).addresses(cx),
            bcc: self.bcc.read(cx).addresses(cx),
            subject: self.subject.read(cx).text().to_string(),
            body: self.body.read(cx).text().to_string(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            replying_to: self.replying_to,
            forwarding: self.forwarding,
            format: self.format,
            quoted_html: self.quoted_html.clone(),
            html: match self.format {
                Format::Html => self.html.clone(),
                Format::Plain => None,
            },
        }
    }

    /// Text or HTML, picked in the bar at the bottom.
    ///
    /// Said in words rather than with an icon, because the difference is not
    /// visible in the editor — both are typed the same way — and the words
    /// are what the recipient's client will be handed.
    /// How long the message waits for an answer once sent, by Send. Clicking
    /// steps through the waits and off. Not for a group send, which is many
    /// messages rather than one waiting on a person.
    fn render_remind(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.reminders || self.group.is_some() {
            return None;
        }
        let label = match self.remind {
            None => "No reminder".to_string(),
            Some(1) => "Remind in 1 day".to_string(),
            Some(7) => "Remind in 1 week".to_string(),
            Some(days) => format!("Remind in {days} days"),
        };
        Some(
            div()
                .id("compose-remind")
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .text_color(theme.text_faint)
                .hover(|s| s.text_color(theme.text))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.remind = next_wait(this.remind);
                    this.remind_chosen = true;
                    cx.notify();
                }))
                .child(format!("\u{21a9} {label}"))
                .into_any_element(),
        )
    }

    fn render_format_toggle(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let option = |format: Format, label: &'static str| {
            let chosen = self.format == format;
            div()
                .id(label)
                .px_1p5()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .when(chosen, |s| s.bg(theme.text).text_color(theme.background))
                .when(!chosen, |s| s.text_color(theme.text_faint).hover(|s| s.text_color(theme.text)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_format(format, cx)))
                .child(label)
        };
        div()
            .id("compose-format")
            .flex()
            .items_center()
            .gap_0p5()
            .p_0p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .tooltip(|_, cx| {
                cx.new(|_| crate::chrome::Tip {
                    text: "HTML: **bold**, *italic*, - lists and links are formatted; a \
                           formatted message you reply to keeps its formatting"
                        .into(),
                })
                .into()
            })
            .child(option(Format::Plain, "Text"))
            .child(option(Format::Html, "HTML"))
            .into_any_element()
    }

    /// Sends it as text or as HTML from here on.
    fn set_format(&mut self, format: Format, cx: &mut Context<Self>) {
        if self.format == format {
            return;
        }
        self.format = format;
        if format == Format::Plain {
            // Back to the text, which the editor has kept up to date. What
            // was formatted is not carried over: text has nowhere to put it,
            // and HTML made again from the text starts from what is there.
            self.html = None;
            self.close_editor();
        }
        self.edited(cx);
        cx.notify();
    }

    /// Whether the message is being written in the HTML editor.
    fn editing_html(&self) -> bool {
        self.active_editor().is_some()
    }

    /// Puts what a writing tool produced into the HTML editor's writing part,
    /// when there is an editor. Whether it did.
    fn write_into_editor(&mut self, text: &str) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(editor) = self.active_editor() {
            editor.borrow().set_writing(&spool_core::markup::to_html(text));
            editor.borrow().focus();
            self.html_undo = true;
            return true;
        }
        let _ = text;
        false
    }

    /// A writing tool, on the HTML editor's writing part — all of it, since
    /// the page's selection is its own.
    fn run_tool_on_writing(&mut self, tool: assist::Tool, cx: &mut Context<Self>) {
        let passage = self.writing_text.trim().to_string();
        if passage.is_empty() {
            self.assist_note = Some(("Write something first.".into(), false));
            cx.notify();
            return;
        }
        let request = assist::Request { tool, passage };
        self.assist_note = None;

        let task = cx.spawn(async move |this, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { crate::assist::run(&request).await })
                .await;
            this.update(cx, |this, cx| {
                match reply {
                    Ok(crate::assist::Outcome::Changed(text)) => {
                        this.write_into_editor(&text);
                    }
                    Ok(crate::assist::Outcome::Unchanged) => {
                        this.assist_note = Some((
                            match tool {
                                assist::Tool::Proofread => "No mistakes found.".into(),
                                _ => format!(
                                    "{} didn\u{2019}t suggest any changes to this.",
                                    spool_net::ai::provider().name()
                                ),
                            },
                            false,
                        ));
                    }
                    Err(e) => this.assist_note = Some((format!("{e:#}"), true)),
                }
                this.assist = None;
                cx.notify();
            })
            .ok();
        });
        self.assist = Some((Job::Tool(tool), task));
        cx.notify();
    }

    /// Takes the HTML editor away.
    fn close_editor(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.editor = None;
        }
        self.editor_poll = None;
    }

    /// The HTML editor, when the message is being written in one.
    #[cfg(target_os = "macos")]
    fn active_editor(&self) -> Option<crate::html_editor::SharedEditor> {
        self.editor.clone().filter(|_| self.format == Format::Html)
    }

    #[cfg(not(target_os = "macos"))]
    fn active_editor(&self) -> Option<()> {
        None
    }

    /// Builds the HTML editor if the message is HTML and there is none yet.
    ///
    /// On the first frame that needs it, because it needs the window. Its
    /// content is what was edited before, or HTML made from the text.
    #[cfg(target_os = "macos")]
    fn ensure_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<crate::html_editor::SharedEditor> {
        if self.format != Format::Html {
            return None;
        }
        if let Some(editor) = &self.editor {
            return Some(editor.clone());
        }

        let content = match &self.html {
            Some(html) => html.clone(),
            None => {
                let content = draft::editor_content(&self.draft(cx));
                // Made, not written: a reply nobody has touched is still
                // untouched with its HTML worked out.
                if self.opened.format == Format::Html && self.opened.html.is_none() {
                    self.opened.html = Some(content.clone());
                }
                self.html = Some(content.clone());
                content
            }
        };
        let focus = std::mem::take(&mut self.focus_editor);
        let editor = crate::html_editor::HtmlEditor::new(
            window,
            &spool_core::markup::hold_remote(&content),
            focus,
        )?;
        if focus {
            editor.borrow().focus();
        }
        self.writing_text = spool_core::markup::to_text(&content);
        self.editor = Some(editor.clone());

        let polled = editor.clone();
        self.editor_poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(80)).await;
                let events = polled.borrow_mut().take_events();
                if events.is_empty() {
                    continue;
                }
                if this.update_in(cx, |this, window, cx| this.editor_events(events, window, cx)).is_err() {
                    break;
                }
            }
        }));
        Some(editor)
    }

    /// What the page said.
    #[cfg(target_os = "macos")]
    fn editor_events(
        &mut self,
        events: Vec<crate::html_editor::EditorEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::html_editor::EditorEvent;
        for event in events {
            match event {
                EditorEvent::Changed { html, writing } => {
                    let html = spool_core::markup::release_remote(&html);
                    // The text version follows the HTML, so the two parts of
                    // the message say the same thing — and switching back to
                    // text shows what was written.
                    let text = spool_core::markup::to_text(&html);
                    self.html = Some(html);
                    self.writing_text = writing;
                    self.body.update(cx, |area, cx| area.set_text(text, cx));
                    self.edited(cx);
                }
                EditorEvent::Send => self.send(window, cx),
                EditorEvent::Discard => self.discard(window, cx),
                EditorEvent::Escape => self.cancel(&Cancel, window, cx),
                // Off whatever field gpui thought had the keyboard, so none
                // of its keys — ⌘A, ⌘Z, the arrows — are taken before the
                // page sees them. The composer's own keys still work.
                EditorEvent::Focused => window.focus(&self.focus_handle),
                EditorEvent::Back => {
                    crate::html_editor::release_keyboard(window);
                    window.focus(&self.subject.read(cx).focus_handle.clone());
                    cx.notify();
                }
                EditorEvent::Attach { name, bytes } => {
                    self.attachments.push(Attached::new(name, bytes));
                    self.attach_error = None;
                    self.attachments_changed = true;
                    self.edited(cx);
                }
            }
        }
    }


    /// Opens the file picker, and attaches whatever comes back.
    fn choose_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });

        cx.spawn(async move |this, cx| {
            // Three layers of "maybe": the channel, the dialog being
            // cancelled, and the platform refusing. All three mean the same
            // thing here — no files — so none of them is an error worth
            // showing.
            let Ok(Ok(Some(paths))) = paths.await else { return };

            this.update(cx, |this, cx| this.attach_paths(&paths, cx)).ok();
        })
        .detach();
    }

    /// Reads files from disk and attaches them.
    ///
    /// Read on the spot rather than remembered as paths: a draft outlives the
    /// state of the filesystem around it, and the bytes are what actually get
    /// sent. See `draft::Attached`.
    fn attach_paths(&mut self, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        let mut failed = Vec::new();

        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string());

            match std::fs::read(path) {
                Ok(bytes) => self.attachments.push(Attached::new(name, bytes)),
                // Named rather than counted. "1 file could not be read" is a
                // sentence that sends somebody looking through their own
                // selection to work out which.
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }

        self.attach_error = (!failed.is_empty()).then(|| failed.join(", "));
        self.attachments_changed = true;
        self.edited(cx);
        cx.notify();
    }

    /// Attaches an image from the clipboard.
    ///
    /// Named rather than left nameless: a clipboard image has no file name, and
    /// a part called `attachment` arrives in the recipient's client as a thing
    /// they cannot tell from any other. Numbered within the message so pasting
    /// three screenshots does not produce three files with the same name.
    fn attach_image(&mut self, image: &gpui::Image, cx: &mut Context<Self>) {
        let extension = match image.format {
            gpui::ImageFormat::Png => "png",
            gpui::ImageFormat::Jpeg => "jpg",
            gpui::ImageFormat::Webp => "webp",
            gpui::ImageFormat::Gif => "gif",
            gpui::ImageFormat::Svg => "svg",
            gpui::ImageFormat::Bmp => "bmp",
            gpui::ImageFormat::Tiff => "tiff",
        };

        let pasted = self
            .attachments
            .iter()
            .filter(|a| a.name.starts_with("Pasted "))
            .count();
        let name = format!("Pasted {}.{extension}", pasted + 1);

        self.attachments.push(Attached::new(name, image.bytes.clone()));
        self.attach_error = None;
        self.attachments_changed = true;
        self.edited(cx);
        cx.notify();
    }

    fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.attachments.len() {
            self.attachments.remove(index);
            self.attachments_changed = true;
            self.edited(cx);
            cx.notify();
        }
    }

    /// Whether nothing has been typed since this composer opened.
    fn untouched(&self, cx: &App) -> bool {
        self.draft(cx) == self.opened
    }

    /// What this window is called: what you are writing, not what it is.
    fn title(&self, cx: &App) -> String {
        if let Some(group) = &self.group
            && self.subject.read(cx).text().trim().is_empty()
        {
            return format!("Message to {}", group.name);
        }
        match self.subject.read(cx).text().trim() {
            "" if self.replying_to.is_some() => "Reply".into(),
            "" => "New Message".into(),
            subject => subject.to_string(),
        }
    }

    /// Adopts the window this composer was opened in.
    pub fn attach(&mut self, window: WindowHandle<Self>) {
        self.window = Some(window);
    }

    /// The flag the window's close hook checks before doing anything.
    pub fn settled(&self) -> std::rc::Rc<std::cell::Cell<bool>> {
        self.settled.clone()
    }

    /// Closes this composer's window, from wherever.
    ///
    /// Deferred: this is called from inside the composer's own update, and a
    /// window cannot be removed while the entity it is rendering is borrowed.
    fn close_window(&self, cx: &mut Context<Self>) {
        let Some(window) = self.window else { return };
        cx.defer(move |cx| {
            window.update(cx, |_, window, _| window.remove_window()).ok();
        });
    }

    /// Something changed: clear the stale error and start the autosave clock.
    fn edited(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        // A message to a group is never a draft; a change only means the
        // send it was about to make is not the one being asked for any more.
        if let Some(group) = self.group.as_mut() {
            group.confirm = false;
            group.closing = false;
            cx.notify();
            return;
        }
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DRAFT_AUTOSAVE_DELAY).await;
            this.update(cx, |this, cx| this.save_now(cx)).ok();
        }));
        cx.notify();
    }

    /// Writes the draft out now.
    pub fn save_now(&mut self, cx: &mut Context<Self>) {
        self.save_task = None;
        if self.group.is_some() {
            return;
        }

        // A composer nobody has typed into is not a draft, even though a reply
        // opens with recipients, a quote and a signature already in it. Saving
        // one would mean opening the next session with "unsent draft" over a
        // message the user never wrote.
        if self.untouched(cx) {
            self.forget();
            return;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default();

        // The first save writes everything; later ones write the files only
        // when they moved.
        let files = match self.attachments_changed || self.draft_id.is_none() {
            true => spool_store::DraftFiles::Replace,
            false => spool_store::DraftFiles::Unchanged,
        };

        match self.store.save_draft(self.draft_id, &self.draft(cx), now, files) {
            Ok(id) => {
                self.draft_id = Some(id);
                self.attachments_changed = false;
            }
            Err(e) => log::error!("failed to save the draft: {e}"),
        }
    }

    /// Drops the stored copy: the draft was sent, discarded, or emptied.
    fn forget(&mut self) {
        self.save_task = None;
        if let Some(id) = self.draft_id.take()
            && let Err(e) = self.store.delete_draft(id)
        {
            log::error!("failed to delete draft {id}: {e}");
        }
    }

    /// Saves and closes.
    ///
    /// What escape and ⌘W do, and what the close button does: closing a
    /// composer has never meant throwing the message away.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hand_back(cx);
        window.remove_window();
    }

    /// Saves, and hands an unfinished draft back to the mailbox.
    ///
    /// Without this a draft closed mid-session is in the store but out of
    /// reach until the app is restarted — there is no drafts list to open it
    /// from yet. The mailbox holds it for the next composer, which is what it
    /// already does for one left over from the last session.
    pub fn hand_back(&mut self, cx: &mut Context<Self>) {
        if self.settled.replace(true) || self.group.is_some() {
            return;
        }
        self.save_now(cx);

        let Some(id) = self.draft_id else { return };
        let draft = self.draft(cx);
        let subject = draft.subject.trim().to_string();

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.restored_draft = Some((id, draft));
                workspace.status = match subject.as_str() {
                    "" => "Draft saved \u{2014} cmd-shift-d to keep writing".into(),
                    subject => format!(
                        "Draft saved: \u{201c}{}\u{201d} \u{2014} cmd-shift-d to keep writing",
                        subject
                    ),
                };
                cx.notify();
            })
            .ok();
    }

    /// Throws the draft away for good.
    fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settled.set(true);
        if self.group.is_none() {
            self.forget();
            self.tell_mailbox("Draft discarded".into(), cx);
        }
        window.remove_window();
    }

    /// Sends, and leaves.
    ///
    /// The order here is the feature: the draft goes to the mailbox window
    /// first, then this window takes off. Nothing waits for the server —
    /// pressing send *is* the send, and what the server says about it arrives
    /// in the mailbox's status line whenever it arrives.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        if self.group.is_some() {
            self.send_group(window, cx);
            return;
        }

        // Every recipient is committed first. Someone who typed one address
        // and pressed send never typed the comma that would have made it a
        // pill, and the field should look the way the message is about to be
        // sent rather than the way it was being written.
        for field in [&self.to, &self.cc, &self.bcc] {
            field.update(cx, |field, cx| field.commit(cx));
        }

        if let Some(bad) = self.bad_recipient(cx) {
            self.error = Some(format!("Cannot send: {bad} is not an address"));
            cx.notify();
            return;
        }

        let draft = self.draft(cx);
        if let Some(problem) = draft.problem() {
            self.error = Some(format!("Cannot send: {problem}"));
            cx.notify();
            return;
        }

        // Written down before it leaves. If the machine loses power between
        // this line and the server's acceptance, the message is still a draft
        // rather than nothing at all.
        self.save_now(cx);

        // Settled, so the window's close hook does not save it a second time
        // or hand it back to the mailbox as an abandoned draft.
        self.settled.set(true);
        self.sending = true;
        self.error = None;

        let draft_id = self.draft_id;
        self.save_task = None;
        let wait = self.remind.map(|days| crate::follow_up::Wait { days, asked: self.remind_chosen });
        self.workspace
            .update(cx, |workspace, cx| workspace.send_draft(draft, draft_id, wait, cx))
            .ok();

        self.take_off(window, cx);
    }

    /// Flies the window off the screen, then closes it.
    ///
    /// Frame by frame rather than by handing the window to Cocoa's own
    /// animator: the curve is the point — see [`crate::flight`] — and a
    /// window that has to be *closed* at the end of it needs something that
    /// says when the end is.
    fn take_off(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(flight) = crate::flight::Flight::ready(window) else {
            // No window to move, so nothing to watch: close, which is what
            // this replaced.
            self.close_window(cx);
            return;
        };

        let handle = self.window;
        self.send_task = Some(cx.spawn(async move |_, cx| {
            let started = std::time::Instant::now();

            loop {
                cx.background_executor().timer(crate::flight::FRAME).await;

                let elapsed = started.elapsed();
                let t = elapsed.as_secs_f32() / crate::flight::DURATION.as_secs_f32();
                flight.step(t);

                if elapsed >= crate::flight::DURATION {
                    break;
                }
            }

            if let Some(handle) = handle {
                handle.update(cx, |_, window, _| window.remove_window()).ok();
            }
        }));
    }

    /// Says something in the mailbox's status line.
    ///
    /// The composer has no status bar of its own, and the outcome of a send is
    /// news the mailbox window wants anyway.
    fn tell_mailbox(&self, message: String, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.status = message;
                cx.notify();
            })
            .ok();
    }

    /// The first recipient that is not an address, if there is one.
    ///
    /// The pill is already red, which is where this is meant to be caught. It
    /// is checked again here because `Draft::problem` cannot see it: by then
    /// the recipients are a list of parsed addresses, and `ana@` parses. The
    /// alternative is a message accepted by the composer and refused by a
    /// server, hours later, to somebody who has stopped thinking about it.
    fn bad_recipient(&self, cx: &App) -> Option<SharedString> {
        [&self.to, &self.cc, &self.bcc]
            .into_iter()
            .flat_map(|field| field.read(cx).pills())
            .find(|pill| !pill.is_valid())
            .map(|pill| pill.label())
    }

    fn send_action(&mut self, _: &SendMessage, window: &mut Window, cx: &mut Context<Self>) {
        self.send(window, cx);
    }

    fn discard_action(&mut self, _: &DiscardDraft, window: &mut Window, cx: &mut Context<Self>) {
        self.discard(window, cx);
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Escape unwinds one layer at a time. Dismissing a suggestion list
        // should not also throw away the message it was helping to address.
        if self.picking_from {
            self.picking_from = false;
            cx.notify();
            return;
        }
        if self.completion.take().is_some() {
            self.completion_task = None;
            cx.notify();
            return;
        }
        if self.writer.is_some() {
            self.toggle_writer(window, cx);
            return;
        }
        if self.keep_group_open(cx) {
            return;
        }
        self.close(window, cx);
    }

    fn close_action(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.keep_group_open(cx) {
            return;
        }
        self.close(window, cx);
    }

    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        // Tab completes when there is something to complete, which is what
        // every mail client has trained people to expect. Only then: tab is
        // the way out of the field the rest of the time.
        if self.accept_completion(cx) {
            return;
        }
        window.focus_next();
    }

    fn prev_field(&mut self, _: &PrevField, window: &mut Window, _: &mut Context<Self>) {
        window.focus_prev();
    }

    /// The field that is being completed, and its input.
    fn input_for(&self, field: Recipients) -> &Entity<RecipientField> {
        match field {
            Recipients::To => &self.to,
            Recipients::Cc => &self.cc,
            Recipients::Bcc => &self.bcc,
        }
    }

    /// Asks the store who this might be, once the typing pauses.
    ///
    /// The prefix is the field's token and nothing else. Everything before it
    /// is a pill, which is to say an address already decided — the old field
    /// had to find the last comma to work out the same thing.
    fn suggest(&mut self, field: Recipients, token: &str, cx: &mut Context<Self>) {
        let prefix = token.trim().to_string();

        // A complete address is not a question. Once there is an `@` with
        // something after it, the person has said who they mean.
        if prefix.len() < 2 || looks_complete(&prefix) {
            self.completion = None;
            self.completion_task = None;
            cx.notify();
            return;
        }

        let store = self.store.clone();
        self.completion_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COMPLETION_DEBOUNCE).await;

            let wanted = prefix.clone();
            let found = cx
                .background_executor()
                .spawn(async move { store.address_suggestions(&wanted, SUGGESTIONS) })
                .await;

            let Ok(matches) = found else { return };

            this.update(cx, |this, cx| {
                // The field may have moved on while this was in flight. What
                // is on screen has to answer what is in the field now.
                if this.input_for(field).read(cx).token(cx) != prefix {
                    return;
                }

                this.completion =
                    (!matches.is_empty()).then(|| Completion { field, matches, selected: 0 });
                cx.notify();
            })
            .ok();
        }));
    }

    fn completion_next(&mut self, _: &CompletionNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_completion(1, cx);
    }

    fn completion_prev(&mut self, _: &CompletionPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_completion(-1, cx);
    }

    fn move_completion(&mut self, delta: isize, cx: &mut Context<Self>) {
        // With no list open the arrows are not this action's: let them reach
        // the text, which in the HTML editor is behind this window's keys.
        let Some(completion) = self.completion.as_mut() else { return cx.propagate() };
        let count = completion.matches.len() as isize;
        if count > 0 {
            completion.selected = (completion.selected as isize + delta).rem_euclid(count) as usize;
        }
        cx.notify();
    }

    fn accept_action(&mut self, _: &AcceptCompletion, window: &mut Window, cx: &mut Context<Self>) {
        // Enter in the line describing an email is asking for it to be written.
        if let Some(writer) = &self.writer
            && writer.input.read(cx).focus_handle.is_focused(window)
        {
            let prompt = writer.input.read(cx).text().to_string();
            self.write_from_prompt(prompt, window, cx);
            return;
        }
        if !self.accept_completion(cx) {
            cx.propagate();
        }
    }

    fn bigger(&mut self, _: &IncreaseComposeFont, _: &mut Window, cx: &mut Context<Self>) {
        self.step_text(Some(1.), cx);
    }

    fn smaller(&mut self, _: &DecreaseComposeFont, _: &mut Window, cx: &mut Context<Self>) {
        self.step_text(Some(-1.), cx);
    }

    fn reset_text(&mut self, _: &ResetComposeFont, _: &mut Window, cx: &mut Context<Self>) {
        self.step_text(None, cx);
    }

    /// Changes this composer's text size, or puts it back to the default.
    ///
    /// It is a preference rather than a per-window zoom: the new size is saved
    /// through the workspace, so the next message you write opens at the size
    /// you chose. Composers already open keep theirs, which is the right
    /// answer for a window you have been reading at one size.
    fn step_text(&mut self, delta: Option<f32>, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |workspace, cx| {
                self.text_size = match delta {
                    Some(delta) => workspace.settings.step_compose_text(delta),
                    None => {
                        workspace.settings.compose_text_size = Settings::COMPOSE_TEXT;
                        Settings::COMPOSE_TEXT
                    }
                };
                workspace.save_settings(cx);
            })
            .ok();

        let style = body_style(self.text_size, cx);
        self.body.update(cx, |body, cx| body.set_style(style, cx));
        cx.notify();
    }

    /// Puts the highlighted address into the field. Returns whether there was
    /// one to put.
    fn accept_completion(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(completion) = self.completion.take() else { return false };
        let Some(chosen) = completion.matches.get(completion.selected) else { return false };

        let field = self.input_for(completion.field).clone();
        let address = chosen.address.clone();
        // Accepted as a pill rather than as text. The old field wrote the
        // address back into the string and added a comma to say another might
        // follow; the pill says that by existing.
        field.update(cx, |field, cx| field.accept(&address, cx));

        // Accepting emits, which asks for suggestions again — for the empty
        // token that is now there, which answers nothing. Clearing here rather
        // than relying on that keeps the list from flickering.
        self.completion = None;
        self.completion_task = None;
        cx.notify();
        true
    }
}

/// How a composer's body is shaped.
///
/// Monospace, because this *is* plain text: what is typed here is what
/// arrives, and a proportional face hides the alignment, the quote markers and
/// the wrapping that the recipient will actually see.
// ---- writing tools ----

impl Composer {
    /// Runs a writing tool on the selection, or on everything written so far
    /// when nothing is selected.
    fn run_tool(&mut self, tool: assist::Tool, cx: &mut Context<Self>) {
        if !cx.global::<crate::assist::Assistant>().is_ready() {
            self.open_writing_settings(cx);
            return;
        }
        if self.editing_html() {
            self.run_tool_on_writing(tool, cx);
            return;
        }

        let body = self.body.read(cx).text().to_string();
        let selection = self.body.read(cx).selection();
        let selected = !body[selection.clone()].trim().is_empty();

        // The selection, or with none, the part above the quote and the
        // signature: never the message being answered.
        let range = match selected {
            true => selection,
            false => assist::written_part(&body),
        };
        if range.is_empty() {
            self.assist_note =
                Some(("Write something first, or select the text to change.".into(), false));
            cx.notify();
            return;
        }

        let request = assist::Request { tool, passage: body[range.clone()].to_string() };
        self.assist_note = None;

        let task = cx.spawn(async move |this, cx| {
            let reply = cx
                .background_executor()
                .spawn(async move { crate::assist::run(&request).await })
                .await;

            this.update(cx, |this, cx| {
                let current = this.body.read(cx).text().to_string();
                match reply {
                    // Only onto the text it was asked about. Anything typed
                    // while the request was out would be overwritten by a
                    // splice at offsets that no longer mean the same thing.
                    Ok(_) if current != body => {
                        log::info!("writing tool {tool:?}: draft changed while it ran; not applied");
                        this.assist_note = Some((
                            "The message changed while it was being worked on, so nothing was \
                             replaced. Try again."
                                .into(),
                            true,
                        ));
                    }
                    Ok(crate::assist::Outcome::Unchanged) => {
                        log::info!("writing tool {tool:?}: no change suggested");
                        this.assist_note = Some((
                            match tool {
                                assist::Tool::Proofread => "No mistakes found.".into(),
                                _ => format!(
                                    "{} didn\u{2019}t suggest any changes to this.",
                                    spool_net::ai::provider().name()
                                ),
                            },
                            false,
                        ));
                    }
                    Ok(crate::assist::Outcome::Changed(reply)) => {
                        log::info!("writing tool {tool:?}: applied ({} bytes)", reply.len());
                        let (changed, placed) = assist::splice(&body, range, &reply);
                        this.body.update(cx, |area, cx| {
                            area.set_text(changed.clone(), cx);
                            // Selected, so what changed is visible and the
                            // next tool runs on the same passage.
                            area.select(placed, cx);
                        });
                        this.assist_undo = Some((body, changed));
                    }
                    Err(e) => {
                        log::warn!("writing tool {tool:?} failed: {e:#}");
                        this.assist_note = Some((format!("{e:#}"), true));
                    }
                }
                // Last, not first: this is the task being dropped, from
                // inside itself.
                this.assist = None;
                cx.notify();
            })
            .ok();
        });
        self.assist = Some((Job::Tool(tool), task));
        cx.notify();
    }

    /// Opens the line for describing an email, or closes it.
    ///
    /// For a reply it also asks for directions specific to the message being
    /// answered, which arrive a moment later and replace the general ones.
    fn toggle_writer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.writer.take().is_some() {
            window.focus(&self.body.read(cx).focus_handle.clone());
            cx.notify();
            return;
        }
        if !cx.global::<crate::assist::Assistant>().is_ready() {
            self.open_writing_settings(cx);
            return;
        }

        let placeholder = match self.replying_to {
            Some(_) => "What should the reply say?",
            None => "Describe the email \u{2014} \"a welcome email for Sam\"",
        };
        let input = cx.new(|cx| TextInput::new(placeholder, cx));
        window.focus(&input.read(cx).focus_handle.clone());

        let suggesting = self.replying_to.map(|message| {
            let store = self.store.clone();
            cx.spawn(async move |this, cx| {
                let found = cx
                    .background_executor()
                    .spawn(async move {
                        let answering = answering(&store, message).ok_or_else(|| anyhow::anyhow!("gone"))?;
                        crate::assist::suggest(&answering).await
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if let Some(writer) = this.writer.as_mut() {
                        writer.thinking = false;
                        match found {
                            Ok(found) if !found.is_empty() => writer.suggestions = Some(found),
                            // The general directions are already showing, and
                            // they are a fine answer to not getting better ones.
                            Ok(_) => {}
                            Err(e) => log::info!("no reply suggestions: {e:#}"),
                        }
                        cx.notify();
                    }
                })
                .ok();
            })
        });

        let thinking = suggesting.is_some();
        self.writer = Some(Writer { input, suggestions: None, thinking, _suggesting: suggesting });
        self.assist_note = None;
        cx.notify();
    }

    /// Writes the email the prompt describes, in place of whatever has been
    /// written so far — above the quote and the signature, never in them.
    fn write_from_prompt(&mut self, prompt: String, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        if !cx.global::<crate::assist::Assistant>().is_ready() {
            self.open_writing_settings(cx);
            return;
        }

        let body = self.body.read(cx).text().to_string();
        let subject = self.subject.read(cx).text().to_string();
        let to = self
            .to
            .read(cx)
            .addresses(cx)
            .iter()
            .map(|a| match a.name.trim() {
                "" => a.addr.clone(),
                name => format!("{name} <{}>", a.addr),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let author = self.from.name.split_whitespace().next().unwrap_or_default().to_string();
        let mut request = assist::Write {
            prompt,
            author,
            to,
            subject: subject.clone(),
            draft: match self.editing_html() {
                true => self.writing_text.clone(),
                false => body[assist::written_part(&body)].to_string(),
            },
            answering: None,
        };
        let replying_to = self.replying_to;
        let store = self.store.clone();
        self.assist_note = None;
        if !self.editing_html() {
            window.focus(&self.body.read(cx).focus_handle.clone());
        }

        let task = cx.spawn(async move |this, cx| {
            let written = cx
                .background_executor()
                .spawn(async move {
                    request.answering = replying_to.and_then(|message| answering(&store, message));
                    crate::assist::write(&request).await
                })
                .await;

            this.update(cx, |this, cx| {
                let current = this.body.read(cx).text().to_string();
                match written {
                    // In the HTML editor it replaces the writing part as one
                    // step the page's undo can take back, so a change made
                    // meanwhile is not lost for good.
                    Ok((new_subject, text)) if this.editing_html() => {
                        this.write_into_editor(&text);
                        this.subject_undo = None;
                        if let Some(new_subject) = new_subject
                            && this.subject.read(cx).text().trim().is_empty()
                        {
                            this.subject.update(cx, |input, cx| input.set_content(new_subject, cx));
                            this.subject_undo = Some(subject);
                        }
                        this.writer = None;
                    }
                    Ok(_) if current != body => {
                        this.assist_note = Some((
                            "The message changed while it was being written, so nothing was \
                             replaced. Try again."
                                .into(),
                            true,
                        ));
                    }
                    Ok((new_subject, text)) => {
                        log::info!("wrote an email from a prompt ({} bytes)", text.len());
                        let (changed, placed) = assist::place_written(&body, &text);
                        this.body.update(cx, |area, cx| {
                            area.set_text(changed.clone(), cx);
                            area.select(placed, cx);
                        });
                        this.assist_undo = Some((body, changed));
                        this.subject_undo = None;
                        // Only into an empty field: a subject somebody typed
                        // while this was out is theirs.
                        if let Some(new_subject) = new_subject
                            && this.subject.read(cx).text().trim().is_empty()
                        {
                            this.subject.update(cx, |input, cx| input.set_content(new_subject, cx));
                            this.subject_undo = Some(subject);
                        }
                        this.writer = None;
                    }
                    Err(e) => {
                        log::warn!("writing from a prompt failed: {e:#}");
                        this.assist_note = Some((format!("{e:#}"), true));
                    }
                }
                this.assist = None;
                cx.notify();
            })
            .ok();
        });
        self.assist = Some((Job::Write, task));
        cx.notify();
    }

    fn cancel_tool(&mut self, cx: &mut Context<Self>) {
        self.assist = None;
        cx.notify();
    }

    fn undo_tool(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if std::mem::take(&mut self.html_undo) {
            if let Some(editor) = self.active_editor() {
                editor.borrow().command(crate::html_editor::Command::Undo);
            }
            if let Some(subject) = self.subject_undo.take() {
                self.subject.update(cx, |input, cx| input.set_content(subject, cx));
            }
            cx.notify();
            return;
        }
        let Some((before, after)) = self.assist_undo.take() else { return };
        if self.body.read(cx).text() != after {
            return;
        }
        self.body.update(cx, |area, cx| area.set_text(before, cx));
        if let Some(subject) = self.subject_undo.take() {
            self.subject.update(cx, |input, cx| input.set_content(subject, cx));
        }
        cx.notify();
    }

    fn open_writing_settings(&mut self, cx: &mut Context<Self>) {
        crate::assist::open_settings(&self.workspace, cx);
    }

    /// The row of tools between the subject and the body.
    fn render_tools(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        // Nothing until the keychain has answered: a notice flashing up for a
        // moment on every composer that can in fact use the tools is noise.
        let assistant = cx.global::<crate::assist::Assistant>();
        if assistant.is_checking() {
            return None;
        }
        let ready = assistant.is_ready();
        let connect = format!("{} to use the writing tools", assistant.what_to_connect());

        let link = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .text_color(theme.text_muted)
                .hover(|s| s.text_color(theme.text))
                .child(label)
        };

        let mut row = div()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(div().text_color(theme.accent).text_size(theme.small_text_size).child("\u{2726}"));

        if !ready {
            return Some(
                row.child(
                    link("assist-unavailable", connect.into())
                        .on_click(cx.listener(|this, _, _, cx| this.open_writing_settings(cx))),
                )
                .into_any_element(),
            );
        }

        let selected = !self.body.read(cx).selection().is_empty();
        let body = self.body.read(cx).text().to_string();
        let can_undo =
            self.html_undo || self.assist_undo.as_ref().is_some_and(|(_, after)| *after == body);

        if let Some((job, _)) = &self.assist {
            let working = match job {
                Job::Tool(tool) => format!(
                    "{} {}\u{2026}",
                    tool.working(),
                    if selected { "the selection" } else { "your message" }
                ),
                Job::Write => "Writing your email\u{2026}".to_string(),
            };
            return Some(
                row.child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_muted)
                        .child(SharedString::from(working)),
                )
                .child(
                    link("assist-cancel", "Cancel".into())
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_tool(cx))),
                )
                .into_any_element(),
            );
        }

        // First, and in the accent: writing the whole thing is the tool for an
        // empty composer, which is where every message starts.
        let writing = self.writer.is_some();
        row = row.child(
            div()
                .id("assist-write")
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .text_color(if writing { theme.text } else { theme.accent })
                .hover(|s| s.text_color(theme.text))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_writer(window, cx)))
                .child("Write\u{2026}"),
        );

        for tool in assist::Tool::ALL {
            row = row.child(
                div()
                    .id(("assist-tool", tool as usize))
                    .cursor_pointer()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Back to the body, so typing carries on where the
                        // tool leaves its result rather than in no field.
                        window.focus(&this.body.read(cx).focus_handle.clone());
                        this.run_tool(tool, cx)
                    }))
                    .child(tool.label()),
            );
        }

        // Styled as a tool but run as a write: it replaces what is written
        // with a whole email rather than editing it.
        let (onboard_label, onboard_prompt) = assist::ONBOARD_PROMPT;
        row = row.child(
            div()
                .id("assist-onboard")
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .text_color(theme.text_muted)
                .hover(|s| s.text_color(theme.text))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.write_from_prompt(onboard_prompt.to_string(), window, cx)
                }))
                .child(onboard_label),
        );

        row = row.child(div().flex_1()).child(match can_undo {
            true => link("assist-undo", "Undo".into())
                .on_click(cx.listener(|this, _, window, cx| {
                    window.focus(&this.body.read(cx).focus_handle.clone());
                    this.undo_tool(cx)
                }))
                .into_any_element(),
            false => div()
                .flex_shrink_0()
                .text_size(theme.small_text_size)
                .text_color(theme.text_faint)
                .child(if selected { "on the selection" } else { "on your message" })
                .into_any_element(),
        });

        // On a line of its own, where it wraps: squeezed in beside the tools
        // it could be truncated to nothing in a narrow window, and a tool
        // that failed would look like one that did nothing.
        let note = self.assist_note.as_ref().map(|(text, failed)| {
            div()
                .px_4()
                .py_1p5()
                .border_b_1()
                .border_color(theme.border_subtle)
                .text_size(theme.small_text_size)
                .text_color(if *failed { theme.red } else { theme.text_muted })
                .child(SharedString::from(text.clone()))
        });

        Some(
            div()
                .flex()
                .flex_col()
                .child(row)
                .children(self.render_writer(theme, cx))
                .children(note)
                .into_any_element(),
        )
    }

    /// The line for describing an email: the prompt, and requests that can
    /// be clicked instead of typed.
    ///
    /// For a reply the requests are directions for this message in particular
    /// once they have been thought of, and general ones until then.
    fn render_writer(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let writer = self.writer.as_ref()?;
        let input = writer.input.clone();

        let prompts: Vec<(String, String)> = match (&writer.suggestions, self.replying_to) {
            (Some(found), _) => found.iter().map(|s| (s.clone(), s.clone())).collect(),
            (None, Some(_)) => {
                assist::REPLY_PROMPTS.iter().map(|(l, p)| (l.to_string(), p.to_string())).collect()
            }
            (None, None) => assist::NEW_PROMPTS.iter().map(|(l, p)| (l.to_string(), p.to_string())).collect(),
        };
        let thinking = writer.thinking;

        let write_button = div()
            .id("writer-go")
            .flex_shrink_0()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_size(theme.small_text_size)
            .bg(theme.accent)
            .text_color(theme.text_on_accent)
            .hover(|s| s.opacity(0.9))
            .on_click(cx.listener({
                let input = input.clone();
                move |this, _, window, cx| {
                    let prompt = input.read(cx).text().to_string();
                    this.write_from_prompt(prompt, window, cx)
                }
            }))
            .child("Write");

        let chips = prompts.into_iter().enumerate().map(|(ix, (label, prompt))| {
            div()
                .id(("writer-prompt", ix))
                .flex_shrink_0()
                .px_1p5()
                .py_0p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .text_color(theme.text_muted)
                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.write_from_prompt(prompt.clone(), window, cx)
                }))
                .child(SharedString::from(label))
        });

        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(theme.border_subtle)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .text_size(theme.small_text_size)
                                .child(input),
                        )
                        .child(write_button),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_1()
                        .children(chips)
                        .children(thinking.then(|| {
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(theme.text_faint)
                                .child("Thinking of replies\u{2026}")
                        })),
                )
                .into_any_element(),
        )
    }
}

/// The message being answered, as the writing tools show it to a model.
///
/// From the store rather than from the quote in the body, which may have been
/// cut down or deleted — and which is the one part of a reply the author is
/// free to have edited into something the sender never said.
fn answering(store: &Store, message: MessageId) -> Option<assist::Answering> {
    let envelope = store.envelope(message).ok().flatten()?;
    let body = store.body(message).ok().flatten()?;
    let text = body_text(&body);
    let from = match envelope.from.name.trim() {
        "" => envelope.from.addr.clone(),
        name => format!("{name} <{}>", envelope.from.addr),
    };
    Some(assist::Answering { from, subject: envelope.subject, text })
}

fn body_style(text_size: f32, cx: &App) -> AreaStyle {
    let (caret, selection) = page_marks();
    AreaStyle {
        font_family: Some(cx.theme().mono_font.into()),
        font_size: Some(px(text_size)),
        font_weight: None,
        line_height: None,
        caret: Some(caret),
        selection: Some(selection),
        ink: None,
    }
}

/// The caret, and the fill behind selected text, as drawn on [`PAGE`].
///
/// These are drawn on the page rather than on the window, so they come from
/// the palette designed for a white ground rather than from the one the rest
/// of the window is using. On the dark theme the window's own would put a
/// white caret on white paper and a fill dark enough for near-white text
/// under near-black [`INK`] — which is how both of them came to be invisible.
fn page_marks() -> (gpui::Hsla, gpui::Hsla) {
    (INK, Theme::new(ThemeId::Light).text_selection)
}

/// The address currently being typed: whatever follows the last comma.
/// Whether a token is already a whole address, so there is nothing to offer.
fn looks_complete(token: &str) -> bool {
    match token.rsplit_once('@') {
        Some((local, domain)) => !local.trim().is_empty() && domain.contains('.'),
        None => false,
    }
}

impl Composer {
    /// The From field: who this is being sent as, and the menu to change it.
    ///
    /// A row like the others rather than a line of grey text off to the right,
    /// because it is a field: it has a value, the value matters, and with more
    /// than one account it is a value you sometimes have to change before you
    /// hit send rather than after.
    fn render_from(&self, theme: &crate::theme::Theme, cx: &mut Context<Self>) -> AnyElement {
        let choices = self.identities.len();
        let label = identity_label(&self.from);

        // One account is not a choice. Shown as plain text, so nothing offers
        // a menu that would open with a single item in it.
        if choices < 2 {
            return div()
                .text_color(theme.text_muted)
                .child(SharedString::from(label))
                .into_any_element();
        }

        div()
            .relative()
            .child(
                div()
                    .id("compose-from")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .w_full()
                    .cursor_pointer()
                    // Swallowed, or the dismissal on the window behind would
                    // close the menu on the way down and this would reopen it
                    // on the way up: a toggle that never turns off.
                    .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .text_color(theme.text)
                    .hover(|s| s.text_color(theme.accent))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picking_from = !this.picking_from;
                        cx.notify();
                    }))
                    .child(div().min_w_0().truncate().child(SharedString::from(label)))
                    // Down-pointing triangle: the one mark that says "there
                    // are others" without needing a label to say it.
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child("\u{25BE}"),
                    ),
            )
            // Deferred, or the rows below this one paint over it: they are
            // later siblings, and an absolutely positioned child is still
            // painted with its parent. This is what `deferred` is for — the
            // menu is laid out here and drawn last.
            .children(
                self.picking_from
                    .then(|| gpui::deferred(self.render_identities(theme, cx)).into_any_element()),
            )
            .into_any_element()
    }

    /// The open From menu.
    ///
    /// Hangs below the row like the address suggestions do, and for the same
    /// reason: a menu that pushed the form down would move the fields out from
    /// under the pointer on its way open.
    fn render_identities(
        &self,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.from.addr.clone();

        div()
            .absolute()
            .top(px(24.))
            .left_0()
            .w(px(360.))
            .max_w_full()
            .py_1()
            .rounded_md()
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .children(self.identities.iter().enumerate().map(|(index, identity)| {
                let chosen = identity.addr.eq_ignore_ascii_case(&current);
                let picked = identity.clone();

                div()
                    .id(("identity", index))
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_identity(picked.clone(), cx);
                    }))
                    .child(
                        div()
                            .w(px(12.))
                            .flex_shrink_0()
                            .text_color(theme.accent)
                            .child(if chosen { "\u{2713}" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text)
                            .child(SharedString::from(identity_label(identity))),
                    )
            }))
            .into_any_element()
    }

    fn signature_for(&self, from: &Address) -> String {
        signature_of(&from.addr)
    }

    /// Sends as somebody else from here on.
    ///
    /// The signature goes with the identity, but only when it is still the one
    /// this composer put there: a signature that has been typed over is the
    /// writer's text, and swapping accounts is not permission to rewrite it.
    fn choose_identity(&mut self, identity: Address, cx: &mut Context<Self>) {
        self.picking_from = false;
        if identity.addr.eq_ignore_ascii_case(&self.from.addr) {
            cx.notify();
            return;
        }

        let was = self.signature_for(&self.from);
        let now = self.signature_for(&identity);
        self.from = identity;

        if was != now {
            let body = self.body.read(cx).text().to_string();
            if let Some(swapped) = draft::resign(&body, &was, &now) {
                self.body.update(cx, |area, cx| area.set_text(swapped, cx));
            }
            // The same swap in the HTML, where the signature is still the one
            // that was put there; the editor is built again to show it.
            if let Some(html) = &self.html
                && let Some(swapped) = draft::resign_html(html, &was, &now)
            {
                self.html = Some(swapped);
                self.close_editor();
            }
        }

        self.edited(cx);
        cx.notify();
    }

    /// The suggestion list, hanging under its field.
    ///
    /// Absolutely positioned, so the form underneath stays exactly where it
    /// was — a list that reflows the fields as you type moves the thing you
    /// are aiming at.
    /// The files travelling with this message, and what they weigh.
    ///
    /// Drawn only when there are some: an empty strip reserved for files
    /// nobody attached is a band of nothing above every message forever.
    fn render_attachments(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.attachments.is_empty() && self.attach_error.is_none() {
            return None;
        }

        let encoded: usize = self.attachments.iter().map(Attached::encoded_size).sum();
        // The encoded size, not the size on disk. It is what the server
        // weighs, and it is about a third larger — quoting the smaller number
        // would be telling somebody their 24MB message is fine when it is
        // 32MB on the wire.
        let large = encoded > draft::LARGE_MESSAGE;

        let chips: Vec<AnyElement> = self
            .attachments
            .iter()
            .enumerate()
            .map(|(index, file)| {
                let label = format!("{}  {}", file.name, format_size(file.bytes.len() as u32));
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .bg(theme.elevated)
                    .border_1()
                    .border_color(theme.border)
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .child(div().max_w(px(260.)).truncate().child(SharedString::from(label)))
                    .child(
                        div()
                            .id(("remove-attachment", index))
                            .cursor_pointer()
                            .px_1()
                            .text_color(theme.text_faint)
                            .hover(|s| s.text_color(theme.red))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_attachment(index, cx)
                            }))
                            .child("\u{00D7}"),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .px_4()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .child(div().flex().flex_wrap().gap_1p5().children(chips))
                .children((!self.attachments.is_empty()).then(|| {
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(match large {
                            true => theme.yellow,
                            false => theme.text_faint,
                        })
                        .child(SharedString::from(match large {
                            // Said plainly, and still sendable. Spool does not
                            // know what the server at the other end accepts.
                            true => format!(
                                "{} attached \u{2014} many servers refuse messages this large",
                                format_size(encoded.min(u32::MAX as usize) as u32),
                            ),
                            false => format!(
                                "{} attached",
                                format_size(encoded.min(u32::MAX as usize) as u32)
                            ),
                        }))
                }))
                .children(self.attach_error.clone().map(|message| {
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.red)
                        .child(SharedString::from(format!("Could not attach {message}")))
                }))
                .into_any_element(),
        )
    }

    fn render_completion(
        &self,
        field: Recipients,
        theme: &crate::theme::Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let completion = self.completion.as_ref().filter(|c| c.field == field)?;
        let selected = completion.selected;

        let rows = completion.matches.iter().enumerate().map(|(index, suggestion)| {
            let address = suggestion.address.clone();
            let is_selected = index == selected;
            let name = address.name.trim().to_string();

            div()
                .id(("suggestion", index))
                .flex()
                .items_baseline()
                .gap_2()
                .px_3()
                .py_1p5()
                .cursor_pointer()
                .when(is_selected, |s| s.bg(theme.selected))
                .when(!is_selected, |s| s.hover(|s| s.bg(theme.hover)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(completion) = this.completion.as_mut() {
                        completion.selected = index;
                    }
                    this.accept_completion(cx);
                }))
                // The name first and in the readable colour: it is what
                // someone is scanning for. The address is the confirmation.
                .when(!name.is_empty(), |row| {
                    row.child(
                        div()
                            .flex_shrink_0()
                            .text_color(if is_selected { theme.text } else { theme.text_muted })
                            .child(SharedString::from(name.clone())),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(SharedString::from(suggestion.address.addr.clone())),
                )
        });

        // `deferred` because painting order is document order: the Subject
        // row is a later sibling of this one and would otherwise be drawn on
        // top of the list, which is exactly what it did.
        Some(
            gpui::deferred(
                div()
                    .absolute()
                    .top_full()
                    .left(px(76.))
                    .right(px(16.))
                    .mt_1()
                    .py_1()
                    .rounded_md()
                    .bg(theme.elevated)
                    .border_1()
                    .border_color(theme.border)
                    .shadow_lg()
                    .children(rows),
            )
            .into_any_element(),
        )
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // The HTML editor stands in for the text editor while the message is
        // HTML. It is a native view over the window, so on a frame where
        // something is drawn over the body — the address suggestions, the From
        // menu — it steps aside, or it would cover them.
        #[cfg(target_os = "macos")]
        let editor = self.ensure_editor(window, cx);
        #[cfg(target_os = "macos")]
        let html_body = editor.as_ref().map(|editor| {
            let covered = self.completion.is_some() || self.picking_from;
            if covered {
                editor.borrow_mut().hide();
            }
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .bg(PAGE)
                .child(match covered {
                    true => div().flex_1().into_any_element(),
                    false => crate::html_editor::EditorElement::new(editor.clone()).into_any_element(),
                })
        });
        #[cfg(not(target_os = "macos"))]
        let html_body: Option<gpui::Div> = None;
        let editing_html = html_body.is_some();

        // The window is titled with the subject, the way every mail client
        // does it, so a stack of composers can be told apart from the Window
        // menu alone. Set here rather than on a subscription because this is
        // the one place that has both the window and the current text — and
        // only when it changed, since it crosses into AppKit.
        let title = self.title(cx);
        if self.titled.as_deref() != Some(title.as_str()) {
            window.set_window_title(&title);
            self.titled = Some(title);
        }

        let draft = self.draft(cx);
        let grouped = self.group.is_some();
        let problem: Option<String> = match grouped {
            true => self.group_problem(cx),
            false => draft.problem().map(str::to_string),
        };
        let recipients = draft.recipients().len();
        let sending = self.sending;
        let error = self.error.clone();
        let show_cc = self.show_cc && !grouped;
        let show_bcc = self.show_bcc && !grouped;
        let group_to = self.render_group_to(&theme);
        let fill_row = self.render_fill_row(&theme, cx);
        let group_panel = self.render_group_panel(&theme, cx);
        let (count_label, send_label) = match self.group_send_labels() {
            Some((count, send)) => (Some(count), send),
            None => (None, "Send".to_string()),
        };

        let field = |label: &'static str, input: AnyElement| {
            div()
                // Positioned, so a suggestion list can hang below the row
                // rather than pushing the rest of the form down every time a
                // letter is typed.
                .relative()
                .flex()
                .items_center()
                .gap_3()
                .px_4()
                .py_2()
                .border_b_1()
                .border_color(theme.border_subtle)
                .child(
                    div()
                        .w(px(56.))
                        .flex_shrink_0()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(label),
                )
                .child(div().flex_1().min_w_0().child(input))
        };

        // Tab order follows reading order. Without explicit stops gpui has no
        // opinion about what "next" means.
        // Reading order, and only the fields that are on screen: tabbing into
        // a Cc that is not showing would move the caret somewhere invisible.
        let mut stop = 0;
        let mut next = || {
            stop += 1;
            stop - 1
        };
        let _stops = [
            self.to.read(cx).focus_handle(cx).tab_index(next()).tab_stop(!grouped),
            self.cc.read(cx).focus_handle(cx).tab_index(next()).tab_stop(show_cc),
            self.bcc.read(cx).focus_handle(cx).tab_index(next()).tab_stop(show_bcc),
            self.subject.read(cx).focus_handle.clone().tab_index(next()).tab_stop(true),
            self.body.read(cx).focus_handle.clone().tab_index(next()).tab_stop(true),
        ];

        div()
            // Its own context: `tab` moves between fields here, and the
            // mailbox's single-letter bindings belong to a window showing a
            // list, not to one you are typing into.
            .key_context("Compose")
            .track_focus(&self.focus_handle)
            // A click anywhere gpui draws is a click outside the HTML editor —
            // the editor's own clicks go straight to the page — so the
            // keyboard comes back from it. Without this a click on To or Cc
            // moves gpui's focus there while AppKit goes on sending every key
            // to the page. In the capture phase, so a control that stops the
            // click from bubbling still gets it.
            .capture_any_mouse_down(cx.listener(|this, _, window, _| {
                if this.editing_html() {
                    #[cfg(target_os = "macos")]
                    crate::html_editor::release_keyboard(window);
                    let _ = window;
                }
            }))
            // Anywhere else is a dismissal, which is what every menu does and
            // what stops one being left open behind the message being written.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.picking_from {
                        this.picking_from = false;
                        cx.notify();
                    }
                }),
            )
            .on_action(cx.listener(Self::send_action))
            .on_action(cx.listener(Self::discard_action))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::close_action))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::prev_field))
            .on_action(cx.listener(Self::completion_next))
            .on_action(cx.listener(Self::completion_prev))
            .on_action(cx.listener(Self::accept_action))
            .on_action(cx.listener(Self::bigger))
            .on_action(cx.listener(Self::smaller))
            .on_action(cx.listener(Self::reset_text))
            // Dropped on the window rather than on a target drawn for the
            // purpose. There is nowhere in a composer a file could be dropped
            // and mean anything else, and a drop zone that only accepts files
            // in one rectangle is one people miss.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.attach_paths(paths.paths(), cx);
            }))
            .size_full()
            .flex()
            .bg(theme.background)
            .font_family(theme.font)
            .text_size(theme.text_size)
            .text_color(theme.text)
            .line_height(px(18.))
            // The message, and beside it — for a group — who it goes to.
            .child(div().flex().flex_col().flex_1().min_w_0().h_full()
            .child(field("From", self.render_from(&theme, cx)))
            .child(match group_to {
                Some(group_to) => field("To", group_to),
                None => field("To", self.to.clone().into_any_element())
                    .children(self.render_completion(Recipients::To, &theme, cx)),
            })
            .children(show_cc.then(|| {
                field("Cc", self.cc.clone().into_any_element())
                    .children(self.render_completion(Recipients::Cc, &theme, cx))
            }))
            .children(show_bcc.then(|| {
                field("Bcc", self.bcc.clone().into_any_element())
                    .children(self.render_completion(Recipients::Bcc, &theme, cx))
            }))
            .child(field("Subject", self.subject.clone().into_any_element()))
            .children(fill_row.map(|row| field("Fill in", row)))
            .children(self.render_tools(&theme, cx))
            .children(html_body)
            .when(!editing_html, |this| this.child(
                // A block, not a flex row: a flex row stretches the area to
                // exactly its own height, and then there is nothing to scroll.
                div()
                    .id("compose-body")
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .py_3()
                    .overflow_y_scroll()
                    .track_scroll(&self.body_scroll)
                    .bg(PAGE)
                    .text_color(INK)
                    // The face and size are set on the area itself, not here:
                    // it shapes during layout, where a style set on this div
                    // has already gone out of scope. See `AreaStyle`.
                    .child(self.body.clone()),
            ))
            .children(self.render_attachments(&theme, cx))
            .children(error.map(|message| {
                div()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_size(theme.small_text_size)
                    .text_color(theme.red)
                    .child(SharedString::from(message))
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .px_4()
                    .py_2p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .when(!grouped, |row| row.child(
                                div()
                                    .id("compose-cc")
                                    .cursor_pointer()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .hover(|s| s.text_color(theme.text))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_cc = !this.show_cc;
                                        cx.notify();
                                    }))
                                    .child(if show_cc { "Hide Cc" } else { "Add Cc" }),
                            ))
                            .when(!grouped, |row| row.child(
                                div()
                                    .id("compose-bcc")
                                    .cursor_pointer()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .hover(|s| s.text_color(theme.text))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.show_bcc = !this.show_bcc;
                                        cx.notify();
                                    }))
                                    .child(if show_bcc { "Hide Bcc" } else { "Add Bcc" }),
                            ))
                            .child(self.render_format_toggle(&theme, cx))
                            .child(
                                div()
                                    .id("compose-attach")
                                    .cursor_pointer()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .hover(|s| s.text_color(theme.text))
                                    .on_click(cx.listener(|this, _, _, cx| this.choose_files(cx)))
                                    .child("Attach\u{2026}"),
                            )
                            .child(
                                div()
                                    .id("compose-discard")
                                    .cursor_pointer()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .hover(|s| s.text_color(theme.red))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.discard(window, cx)
                                    }))
                                    .child("Discard"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .children(self.render_remind(&theme, cx))
                            .child(
                                div()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(match (&problem, count_label) {
                                        (Some(reason), _) => reason.clone(),
                                        (None, _) if sending => "Sending\u{2026}".into(),
                                        (None, Some(count)) => count,
                                        (None, None) => format!(
                                            "{recipients} recipient{}",
                                            if recipients == 1 { "" } else { "s" }
                                        ),
                                    })),
                            )
                            .child(
                                div()
                                    .id("compose-send")
                                    .px_3()
                                    .py_1p5()
                                    .rounded_md()
                                    .when(problem.is_none() && !sending, |s| {
                                        s.cursor_pointer()
                                            .bg(theme.accent)
                                            .text_color(theme.text_on_accent)
                                            .hover(|s| s.opacity(0.9))
                                    })
                                    // Disabled rather than absent: the reason
                                    // it cannot send is shown next to it.
                                    .when(problem.is_some() || sending, |s| {
                                        s.bg(theme.selected_inactive).text_color(theme.text_faint)
                                    })
                                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
                                    .child(send_label),
                            ),
                    ),
            ))
            .children(group_panel)
    }
}

impl Workspace {
    /// Opens a blank message from the selected account.
    pub fn open_compose(&mut self, cx: &mut Context<Self>) {
        let Some(from) = self.sending_identity() else {
            self.status = "No account configured to send from".into();
            cx.notify();
            return;
        };

        // A blank message, always. This used to hand back whatever draft was
        // waiting — one left over from the last session, one closed with
        // escape, one whose send failed — which meant "New Message" could open
        // holding a half-written reply to something selected an hour ago, with
        // its recipients and its quote still in it. A command called New has
        // to be the one command nobody has to check the output of.
        //
        // The waiting draft is reached with `ResumeDraft` instead, which says
        // what it does.
        let mut draft = Draft::new(from);
        draft.body = draft::sign(&draft.body, &self.signature_for(&draft.from));
        self.open_composer(&draft, Focus::Recipients, None, cx);
    }

    /// Reopens the draft that was last put down.
    ///
    /// One draft, not a list: this holds whatever was most recently handed
    /// back — closed, left over from the last session, or kept because its send
    /// failed. A real drafts list is BACKLOG #9; until then this is what keeps
    /// a closed draft reachable at all.
    pub fn resume_draft(&mut self, cx: &mut Context<Self>) {
        let Some((id, draft)) = self.restored_draft.take() else {
            self.status = "No saved draft".into();
            cx.notify();
            return;
        };

        // Focused in the body rather than the recipients: a draft being
        // resumed already has its recipients, and the cursor belongs where the
        // writing stopped.
        self.open_composer(&draft, Focus::Body, Some(id), cx);
    }

    /// Opens a reply to the selected message.
    pub fn open_reply(&mut self, kind: ReplyKind, cx: &mut Context<Self>) {
        let Some(envelope) = self.selected_envelope().cloned() else { return };

        // The reply needs the original's recipients and its Message-ID, none of
        // which the envelope carries — the body is where they live.
        let body = self.open.as_ref().map(|open| open.body.clone()).unwrap_or_default();
        let html = self.quotable_html(&body);
        self.reply_to_message(envelope, body, html, kind, cx);
    }

    /// Opens a reply to the open message with `text` already written in it —
    /// a quick reply, ready to change and send. Never sends it.
    pub(crate) fn reply_with_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let envelope = open.envelope.clone();
        let body = open.body.clone();
        let html = self.quotable_html(&body);
        self.reply_written(envelope, body, html, ReplyKind::Sender, Some(text), cx);
    }

    /// Opens a follow-up to a message you sent: a reply to everyone it went
    /// to, with `nudge` written in when there is one. Never sends it.
    pub(crate) fn follow_up_on(
        &mut self,
        envelope: spool_core::Envelope,
        body: spool_core::Body,
        nudge: String,
        cx: &mut Context<Self>,
    ) {
        let html = self.quotable_html(&body);
        self.reply_written(envelope, body, html, ReplyKind::All, Some(nudge), cx);
    }

    /// Opens a reply to a message that need not be the one selected — the
    /// client book's history answers from here.
    ///
    /// `html` is the original's HTML to quote under the reply, already
    /// sanitized, when it was read as HTML; `None` answers in plain text.
    pub(crate) fn reply_to_message(
        &mut self,
        envelope: spool_core::Envelope,
        body: spool_core::Body,
        html: Option<String>,
        kind: ReplyKind,
        cx: &mut Context<Self>,
    ) {
        self.reply_written(envelope, body, html, kind, None, cx);
    }

    /// As [`Self::reply_to_message`], with what to say already in place
    /// above the quote and the signature.
    fn reply_written(
        &mut self,
        envelope: spool_core::Envelope,
        body: spool_core::Body,
        html: Option<String>,
        kind: ReplyKind,
        written: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(from) = self.reply_identity(&envelope, &body) else {
            self.status = "No account configured to send from".into();
            cx.notify();
            return;
        };
        let date = format_date_long(envelope.date, crate::clock::local_offset(envelope.date));
        let quoted = draft::reply_body(&envelope, &date, &body_text(&body));

        let original = Original {
            envelope: &envelope,
            // Threading on the receiving end is done by these two, not by the
            // subject. Without them a reply looks right until somebody edits
            // the subject line, at which point it silently starts a second
            // conversation.
            message_id: Some(&body.message_id).filter(|id| !id.is_empty()).map(String::as_str),
            references: &body.references,
            to: &body.to,
            cc: &body.cc,
            // Where the sender asked for answers to go. This is what makes
            // replying to a mailing list reach the list.
            reply_to: &body.reply_to,
        };

        let mut draft = draft::reply(original, &from, kind, &quoted);
        draft.body = draft::sign(&draft.body, &self.signature_for(&draft.from));
        if let Some(written) = written.filter(|w| !w.trim().is_empty()) {
            draft.body = assist::place_written(&draft.body, written.trim()).0;
        }
        // A message that came as HTML goes back as HTML, with the original
        // under the reply as it was sent rather than flattened into text.
        if let Some(html) = html {
            draft.format = Format::Html;
            draft.quoted_html = Some(draft::reply_quote_html(&envelope, &date, &html));
        }
        self.open_composer(&draft, Focus::Body, None, cx);
    }

    /// Opens a blank message to somebody, from the account they belong to.
    ///
    /// `from` is that account's address when the client book knows it — a
    /// product's signup is written to from that product's address — and
    /// otherwise whichever account a new message would go from.
    pub(crate) fn compose_to(&mut self, to: Address, from: Option<&str>, cx: &mut Context<Self>) {
        let identity = from
            .and_then(|email| self.accounts.iter().find(|a| a.email.eq_ignore_ascii_case(email)))
            .map(|account| Address::new(account.name.clone(), account.email.clone()))
            .or_else(|| self.sending_identity());
        let Some(identity) = identity else {
            self.status = "No account configured to send from".into();
            cx.notify();
            return;
        };
        let mut draft = Draft::new(identity);
        draft.to = vec![to];
        draft.body = draft::sign(&draft.body, &self.signature_for(&draft.from));
        self.open_composer(&draft, Focus::Subject, None, cx);
    }

    /// Opens the message a `mailto:` link asked for — one clicked in a
    /// browser or another app, with Spool as the app that opens them.
    ///
    /// The caret goes to the first thing the link left blank, so a "contact
    /// us" link with only an address starts at the subject, and one that
    /// filled everything in starts at the writing.
    pub(crate) fn compose_mailto(&mut self, mail: spool_core::mailto::Mailto, cx: &mut Context<Self>) {
        let Some(from) = self.sending_identity() else {
            self.say("No account configured to send from", cx);
            return;
        };
        let mut draft = Draft::new(from);
        draft.to = mail.to;
        draft.cc = mail.cc;
        draft.bcc = mail.bcc;
        draft.subject = mail.subject;
        draft.body = draft::sign(&mail.body, &self.signature_for(&draft.from));

        let focus = match (draft.to.is_empty(), draft.subject.is_empty()) {
            (true, _) => Focus::Recipients,
            (false, true) => Focus::Subject,
            (false, false) => Focus::Body,
        };
        self.open_composer(&draft, focus, None, cx);
    }

    /// Opens a forward of the selected message.
    /// The open message's HTML, sanitized to go under a reply or forward —
    /// when it is read as HTML at all.
    ///
    /// Only when the reader shows it as HTML: a message whose HTML part is a
    /// wrapper round one line of text is read as text, and answered as text.
    /// What executes or loads a document of its own is taken out; remote
    /// images are kept, because what the recipient's client loads is theirs to
    /// decide and a logo missing from their own message looks like breakage.
    fn quotable_html(&self, body: &spool_core::Body) -> Option<String> {
        let open = self.open.as_ref()?;
        if open.source != spool_core::html::Source::Rich || body.html.trim().is_empty() {
            return None;
        }
        let inline = crate::message_view::inline_data_urls(body);
        Some(spool_core::sanitize::for_web_view(&body.html, spool_core::sanitize::Remote::Allowed, &inline))
    }

    pub fn open_forward(&mut self, cx: &mut Context<Self>) {
        let Some(envelope) = self.selected_envelope().cloned() else { return };
        let body = self.open.as_ref().map(|open| open.body.clone()).unwrap_or_default();

        // The same rule as a reply: a message forwarded on goes out from the
        // address it came to.
        let Some(from) = self.reply_identity(&envelope, &body) else {
            self.status = "No account configured to send from".into();
            cx.notify();
            return;
        };
        let date = format_date_long(envelope.date, crate::clock::local_offset(envelope.date));
        let quoted = draft::forward_body(&envelope, &date, &body.to, &body_text(&body));

        // `Original::new` and not the threading headers: `draft::forward`
        // deliberately starts a new conversation rather than continuing the
        // one being forwarded.
        let mut draft = draft::forward(Original::new(&envelope), &from, &quoted);
        draft.body = draft::sign(&draft.body, &self.signature_for(&draft.from));
        if let Some(html) = self.quotable_html(&body) {
            draft.format = Format::Html;
            draft.quoted_html = Some(draft::forward_quote_html(&envelope, &date, &body.to, &html));
        }

        // The files come too. Anything the sender attached is most of why a
        // message gets forwarded, and a forward that silently dropped them is
        // a bug the recipient discovers rather than the sender.
        //
        // Inline images are left out: they belong to the HTML body and are
        // already in the quote, so carrying them again would attach the
        // sender's signature logo to every forward.
        let files: Vec<(usize, String)> = body
            .files()
            .map(|(index, attachment)| (index, attachment.name.clone()))
            .collect();

        if files.is_empty() {
            self.open_composer(&draft, Focus::Recipients, None, cx);
            return;
        }

        // Gathered before the composer opens rather than filled in behind it.
        // Most of these are already on disk and arrive in milliseconds; the
        // ones past the sync cap are fetched from the server, and waiting is
        // the price of a forward that carries what the original had. A
        // composer that opened first would have a window where Send is live
        // and the message is not yet what it claims to be.
        let store = self.store.clone();
        let accounts = self.account_configs();
        let message = envelope.id;
        let count = files.len();

        self.set_sticky_status(match count {
            1 => "Fetching the attachment\u{2026}".into(),
            n => format!("Fetching {n} attachments\u{2026}"),
        });
        cx.notify();

        cx.spawn(async move |this, cx| {
            let meter = Arc::new(AtomicU64::new(0));
            let gathered = cx
                .background_executor()
                .spawn(async move {
                    let mut attached = Vec::new();
                    let mut missing = Vec::new();

                    for (index, name) in files {
                        match Self::attachment_bytes_anywhere(
                            store.clone(),
                            accounts.clone(),
                            message,
                            index,
                            meter.clone(),
                        )
                        .await
                        {
                            Ok(bytes) => attached.push(Attached::new(name, bytes)),
                            // One unreachable file does not cancel the
                            // forward: the note is still worth sending, and
                            // saying which one is missing beats refusing to
                            // open a composer at all.
                            Err(e) => missing.push(format!("{name}: {e:#}")),
                        }
                    }
                    (attached, missing)
                })
                .await;

            this.update(cx, |this, cx| {
                let (attached, missing) = gathered;
                draft.attachments = attached;

                this.set_sticky_status(match missing.is_empty() {
                    true => String::new(),
                    false => format!("Could not attach {}", missing.join(", ")),
                });
                this.open_composer(&draft, Focus::Recipients, None, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Opens a composer window on a draft.
    ///
    /// Answering the same message twice brings the window that is already open
    /// forward instead of opening a second one: two composers for one reply are
    /// two drafts that will disagree, and only one of them can be sent.
    fn open_composer(
        &mut self,
        draft: &Draft,
        focus: Focus,
        draft_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        self.open_composer_with(draft, focus, draft_id, None, cx);
    }

    /// As [`Self::open_composer`], for a message that may be to a group.
    fn open_composer_with(
        &mut self,
        draft: &Draft,
        focus: Focus,
        draft_id: Option<i64>,
        group: Option<Group>,
        cx: &mut Context<Self>,
    ) {
        // Windows that have closed leave their handle behind; this is the
        // only moment worth sweeping them.
        self.composers.retain(|handle| handle.read(cx).is_ok());

        if let Some(answering) = draft.replying_to
            && let Some(open) = self.composer_answering(answering, cx)
        {
            open.update(cx, |_, window, _| window.activate_window()).ok();
            return;
        }

        let workspace = cx.entity().downgrade();
        let store = self.store.clone();
        let draft = draft.clone();
        // In the sidebar's order, so the menu reads the way the accounts do.
        let identities: Vec<Address> = self
            .sorted_accounts()
            .into_iter()
            .map(|account| Address::new(account.name.clone(), account.email.clone()))
            .collect();
        // Read here, where the workspace is in hand, rather than by the
        // composer while it draws.
        let text_size = self.settings.compose_text();
        // Read here, not by the composer: it draws its first frame while the
        // mailbox is still in the middle of opening it, and cannot read it.
        let reminders = self.settings.follow_ups;

        // Cascaded rather than stacked: a second composer landing exactly on
        // the first is a window you cannot see is there.
        let step = self.composers.len().min(6) as f32 * 26.;
        // A group's needs room down the right for who it goes to.
        let (width, height) = if group.is_some() { (1060., 680.) } else { (720., 620.) };
        let mut bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        bounds.origin += point(px(step), px(step));

        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some("New Message".into()),
                // The mailbox draws its own chrome because its sidebar runs to
                // the top of the window. A composer is an ordinary window and
                // takes the system's, which is part of what makes it feel like
                // a document rather than a dialog.
                appears_transparent: false,
                traffic_light_position: None,
            }),
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(420.), px(320.))),
            ..Default::default()
        };

        let opened = cx.open_window(options, |window, cx| {
            cx.new(|cx| {
                Composer::new(
                    &draft, focus, draft_id, identities, store, workspace, group, text_size, reminders, window,
                    cx,
                )
            })
        });

        match opened {
            Ok(handle) => {
                handle
                    .update(cx, |composer, window, cx| {
                        composer.attach(handle);

                        // Saved on the way out however the window is closed:
                        // the red button does not go through this app's own
                        // close action. Skipped once the draft has been dealt
                        // with, because by then the window is going away and
                        // reaching into it again is an error in the log.
                        let settled = composer.settled();
                        window.on_window_should_close(cx, move |_, cx| {
                            if !settled.get() {
                                handle
                                    .update(cx, |composer, _, cx| composer.hand_back(cx))
                                    .ok();
                            }
                            true
                        });
                    })
                    .ok();
                self.composers.push(handle);
            }
            Err(e) => {
                self.status = format!("Could not open the composer: {e}");
                cx.notify();
            }
        }
    }

    /// The composer already answering this message, if one is open.
    ///
    /// Doubles as the sweep for windows that have since closed, which is the
    /// only signal a window handle gives that it is gone.
    fn composer_answering(
        &mut self,
        message: MessageId,
        cx: &mut Context<Self>,
    ) -> Option<WindowHandle<Composer>> {
        self.composers.retain(|handle| handle.read(cx).is_ok());
        self.composers
            .iter()
            .find(|handle| {
                handle.read(cx).map(|c| c.replying_to == Some(message)).unwrap_or(false)
            })
            .copied()
    }

    /// Writes out every open composer's draft.
    ///
    /// Called on quit: the autosave is debounced, so the last sentence typed
    /// before ⌘Q has not necessarily been written yet.
    pub fn save_open_drafts(&mut self, cx: &mut Context<Self>) {
        let composers = std::mem::take(&mut self.composers);
        for handle in &composers {
            // Windows that have already closed are not errors to report; they
            // saved on the way out.
            if handle.read(cx).is_err() {
                continue;
            }
            handle.update(cx, |composer, _, cx| composer.save_now(cx)).ok();
        }
        self.composers = composers;
        self.composers.retain(|handle| handle.read(cx).is_ok());
    }

    /// This account's signature, from `accounts.toml`.
    ///
    /// Read at the moment a composer opens rather than cached: the file is
    /// small, it is edited in Settings a few feet away, and a signature that
    /// needs a restart to take effect is one people assume is broken.
    fn signature_for(&self, from: &Address) -> String {
        signature_of(&from.addr)
    }

    /// The address to send as.
    ///
    /// The account owning the selected message where there is one, so replying
    /// from a unified view answers as the person who was written to; otherwise
    /// the first configured account.
    fn sending_identity(&self) -> Option<Address> {
        let account = self
            .selected_envelope()
            .and_then(|envelope| self.accounts.iter().find(|a| a.id == envelope.account_id))
            .or_else(|| self.accounts.first())?;

        Some(Address::new(account.name.clone(), account.email.clone()))
    }

    /// Who to answer a message as.
    ///
    /// Whichever account the message was actually addressed to, before the
    /// account that happens to own the folder it is filed in. The two are
    /// usually the same and differ exactly where it matters: mail that arrived
    /// for one address and was filed under another — a forwarding address, a
    /// second account subscribed to the same list — should be answered from
    /// the address the sender wrote to, or the reply arrives from a stranger.
    fn reply_identity(
        &self,
        envelope: &spool_core::Envelope,
        body: &spool_core::Body,
    ) -> Option<Address> {
        let addressed: Vec<&Address> =
            body.to.iter().chain(body.cc.iter()).chain(body.reply_to.iter()).collect();

        addressed_account(&self.accounts, &addressed)
            .map(|account| Address::new(account.name.clone(), account.email.clone()))
            .or_else(|| {
                self.accounts
                    .iter()
                    .find(|a| a.id == envelope.account_id)
                    .map(|account| Address::new(account.name.clone(), account.email.clone()))
            })
            .or_else(|| self.sending_identity())
    }
}

/// How an identity reads in the From row.
///
/// `Name <address>` when the account has a name of its own, and the bare
/// address when it does not — or when the "name" is the address again, which
/// is what an account gets called when nobody has given it a name.
fn identity_label(identity: &Address) -> String {
    let name = identity.name.trim();
    match name.is_empty() || name.eq_ignore_ascii_case(&identity.addr) {
        true => identity.addr.clone(),
        false => format!("{name} <{}>", identity.addr),
    }
}

/// The account among `accounts` that one of `addressed` is written to.
///
/// In the order the message names them rather than the order the accounts are
/// configured in: a message written to both of your addresses is answered from
/// whichever the sender put first, which is the one they meant.
fn addressed_account<'a>(
    accounts: &'a [spool_core::Account],
    addressed: &[&Address],
) -> Option<&'a spool_core::Account> {
    addressed.iter().find_map(|written_to| {
        accounts.iter().find(|account| account.email.eq_ignore_ascii_case(&written_to.addr))
    })
}

/// The signature configured for an address, or nothing.
///
/// Read from the config each time rather than cached: it is wanted when a
/// draft is made and when the identity changes, both of which are things a
/// person does, not things a frame does.
pub(crate) fn signature_of(addr: &str) -> String {
    spool_net::Config::load(spool_net::Config::default_path())
        .ok()
        .and_then(|config| {
            config
                .accounts
                .into_iter()
                .find(|a| a.email.eq_ignore_ascii_case(addr))
                .map(|a| a.signature)
        })
        .unwrap_or_default()
}

/// The text a reply quotes: the plain body, or the formatted one flattened.
/// A message's text, for quoting: its plain part, or its HTML read as text
/// when that is all it has — never the HTML's tags.
pub(crate) fn body_text(body: &spool_core::Body) -> String {
    match body.plain.trim().is_empty() {
        false => body.plain.clone(),
        true => spool_core::html::html_to_blocks(&body.html)
            .iter()
            .map(spool_core::html::Block::text)
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// Sends a draft: finds its account, renders it, and hands it to SMTP.
///
/// Free rather than a method because it runs on a background thread and must
/// not touch the composer.
/// The wait after `wait`, as the composer's reminder steps through them:
/// each of `follow_up::WAITS` in turn, then none, then round again.
fn next_wait(wait: Option<u32>) -> Option<u32> {
    use spool_core::follow_up::WAITS;
    match wait {
        None => WAITS.first().copied(),
        Some(days) => WAITS.iter().copied().find(|&w| w > days),
    }
}

/// Returns the sent message's Message-ID, which a reply to it will quote.
pub(crate) async fn deliver(store: Store, draft: Draft) -> anyhow::Result<String> {
    use anyhow::Context as _;

    let config = spool_net::Config::load(spool_net::Config::default_path())?;
    let account = config
        .accounts
        .iter()
        .find(|a| a.email.eq_ignore_ascii_case(&draft.from.addr))
        .or_else(|| config.accounts.first())
        .with_context(|| format!("no account configured for {}", draft.from.addr))?;

    let credentials = account.credentials().await?;

    // A random-enough left-hand side, and the sender's own domain on the
    // right, which is what receiving servers expect to see.
    let unique = format!(
        "{:x}.{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
        std::process::id(),
    );
    let message_id = spool_net::compose::message_id(&draft.from, &unique);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    use spool_net::compose::Copy;

    let message = spool_net::compose::render_with(&draft, &message_id, now, Copy::Sent);
    // Every field, which is what makes a blind copy arrive at all: the
    // envelope is what delivers, and Bcc appears nowhere else.
    let recipients: Vec<String> =
        draft.recipients().iter().map(|a| a.addr.trim().to_string()).collect();

    spool_net::smtp::send(account, &credentials, &draft.from.addr, &recipients, &message)
        .await?;

    // Past this point the message has been sent, and nothing below is allowed
    // to say otherwise. Filing the copy is bookkeeping: worth doing, worth
    // logging when it fails, and not worth reporting as a failed send — which
    // is the one error message that would make somebody send it twice.
    // The filed copy is rendered again rather than reusing what went out: it
    // is the one that names the blind copies, so that Sent can answer "who
    // did I send this to" completely.
    let filed = spool_net::compose::render_with(&draft, &message_id, now, Copy::Filed);
    if let Err(e) = file_sent_copy(&store, account, &credentials, &filed, now).await {
        log::warn!("the message was sent, but no copy could be filed: {e:#}");
    }

    Ok(message_id)
}

/// Puts a copy of what was just sent into the account's Sent folder.
async fn file_sent_copy(
    store: &Store,
    account: &spool_net::AccountConfig,
    credentials: &spool_net::Credentials,
    message: &str,
    now: i64,
) -> anyhow::Result<()> {
    file_sent_copies(store, account, credentials, &[(message, now)]).await
}

/// Puts copies of what was just sent into the account's Sent folder, each
/// with the time it went.
///
/// Locally first, so they are in the list before the network is involved at
/// all, and then on the server unless the submission server files its own —
/// on one connection, however many there are. The local rows have no UID; the
/// next sync of that folder hands them one, which is what stops the server's
/// copies arriving as second messages.
pub(crate) async fn file_sent_copies(
    store: &Store,
    account: &spool_net::AccountConfig,
    credentials: &spool_net::Credentials,
    copies: &[(&str, i64)],
) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let account_id = store
        .accounts()?
        .into_iter()
        .find(|a| a.email.eq_ignore_ascii_case(&account.email))
        .map(|a| a.id);
    // No account row means nothing has ever been synced for this address, so
    // there is no Sent folder to file into and no list to file it for.
    let Some(account_id) = account_id else { return Ok(()) };

    let sent = store
        .mailboxes()?
        .into_iter()
        .find(|m| m.account_id == account_id && m.role == spool_core::MailboxRole::Sent);
    let Some(sent) = sent else {
        anyhow::bail!("{} has no Sent folder yet; sync once and it will", account.email)
    };

    let parsed: Vec<_> = copies
        .iter()
        .filter_map(|&(message, sent_at)| {
            let mut parsed = spool_net::parse::parse_message(
                message.as_bytes(),
                account_id,
                sent.id,
                // No UID: this copy is local until the server's own is synced
                // and the two are reconciled by Message-ID.
                None,
                spool_core::Flags(spool_core::Flags::SEEN),
            )?;
            // The renderer stamps the message with the time it built it;
            // trust that over anything re-parsing might make of the Date
            // header.
            parsed.date = sent_at;
            Some(parsed)
        })
        .collect();
    store.ingest(&parsed)?;

    if account.files_sent_mail_itself() {
        return Ok(());
    }

    let messages: Vec<&str> = copies.iter().map(|&(message, _)| message).collect();
    spool_net::append_all(account, credentials, &sent.path, &messages)
        .await
        .context("filing the copy on the server")
}

#[cfg(test)]
mod tests {

    /// The composer's body is a white page in every theme, so the colours it
    /// draws on that page have to work against *it* rather than against the
    /// window. The dark theme's own caret is white and its own selection fill
    /// is dark, and both were invisible here: a white caret on white paper,
    /// and near-black ink on a near-black fill.
    #[test]
    fn the_reminder_steps_through_its_waits_then_off_then_round() {
        use super::next_wait;
        assert_eq!(next_wait(Some(3)), Some(7));
        assert_eq!(next_wait(Some(7)), None);
        assert_eq!(next_wait(None), Some(1));
        assert_eq!(next_wait(Some(1)), Some(3));
    }

    #[test]
    fn the_page_can_be_written_and_read_on_in_every_theme() {
        use super::{INK, PAGE};
        use crate::theme::{ThemeId, contrast};

        for id in ThemeId::ALL {
            let (caret, selection) = super::page_marks();

            // The caret has to be findable against the paper it sits on.
            let against_page = contrast(caret, PAGE);
            assert!(
                against_page >= 4.5,
                "{}: the caret is only {against_page:.1}:1 against the page",
                id.name(),
            );

            // And the ink has to survive being selected.
            let ink_on_selection = contrast(INK, selection);
            assert!(
                ink_on_selection >= 4.5,
                "{}: ink on the selection is only {ink_on_selection:.1}:1",
                id.name(),
            );

            // A fill you cannot tell from the paper is not a selection.
            let selection_on_page = contrast(selection, PAGE);
            assert!(
                selection_on_page >= 1.5,
                "{}: the selection is only {selection_on_page:.2}:1 against the page",
                id.name(),
            );
        }
    }
    use super::{addressed_account, identity_label, looks_complete};
    use spool_core::{Account, AccountId, Address, address, draft::Draft};

    fn account(id: i64, email: &str) -> Account {
        Account { id: AccountId(id), name: email.into(), email: email.into(), color: 0 }
    }

    #[test]
    fn an_identity_is_not_written_out_twice() {
        // What an account with no name of its own looks like in the store:
        // the address in both fields, which would otherwise be drawn as
        // `wes@x.com <wes@x.com>`.
        assert_eq!(identity_label(&Address::new("wes@x.com", "wes@x.com")), "wes@x.com");
        assert_eq!(identity_label(&Address::new("", "wes@x.com")), "wes@x.com");
        assert_eq!(identity_label(&Address::new("Wes", "wes@x.com")), "Wes <wes@x.com>");
    }

    #[test]
    fn a_reply_is_sent_from_the_address_it_was_written_to() {
        let accounts = [account(1, "work@example.com"), account(2, "home@example.com")];
        let addressed = [Address::new("", "home@example.com")];
        let addressed: Vec<&Address> = addressed.iter().collect();

        assert_eq!(addressed_account(&accounts, &addressed).unwrap().id, AccountId(2));
    }

    #[test]
    fn the_address_the_sender_named_first_is_the_one_to_answer_as() {
        // Written to both of your addresses: the reply comes from the one the
        // sender put first, rather than from whichever account was configured
        // first, which is an accident of a file.
        let accounts = [account(1, "work@example.com"), account(2, "home@example.com")];
        let addressed = [
            Address::new("", "home@example.com"),
            Address::new("", "work@example.com"),
        ];
        let addressed: Vec<&Address> = addressed.iter().collect();

        assert_eq!(addressed_account(&accounts, &addressed).unwrap().id, AccountId(2));
    }

    #[test]
    fn an_address_is_matched_however_it_is_capitalised() {
        let accounts = [account(1, "Work@Example.com")];
        let addressed = [Address::new("", "work@example.COM")];
        let addressed: Vec<&Address> = addressed.iter().collect();

        assert!(addressed_account(&accounts, &addressed).is_some());
    }

    #[test]
    fn mail_addressed_to_nobody_here_has_no_identity_of_its_own() {
        // A list posting, or mail that arrived by a forwarding rule: nothing
        // matches, and the caller falls back to the account it is filed under.
        let accounts = [account(1, "work@example.com")];
        let addressed = [Address::new("", "list@example.org")];
        let addressed: Vec<&Address> = addressed.iter().collect();

        assert!(addressed_account(&accounts, &addressed).is_none());
    }

    #[test]
    fn a_finished_address_is_not_a_question() {
        assert!(looks_complete("ana@example.com"));
        assert!(!looks_complete("ana@example"), "no dot yet, still being typed");
        assert!(!looks_complete("ana@"), "nor is a bare at sign");
        assert!(!looks_complete("@example.com"), "and neither is a domain alone");
        assert!(!looks_complete("ana"));
    }

    /// A composer is worth saving once it differs from what it opened with.
    ///
    /// Not "once it is non-empty": a reply opens with recipients, a quoted
    /// original and a signature already in it, and none of that is something
    /// the user wrote.
    #[test]
    fn a_draft_is_only_worth_saving_once_something_changes() {
        let mut opened = Draft::new(Address::new("Wes", "wes@example.com"));
        opened.to = address::decode_list("ana@example.com");
        opened.subject = "Re: the roof".into();
        opened.body = "\n\n-- \nWes\n\nOn Tuesday, Ana wrote:\n> hello".into();

        assert_eq!(opened.clone(), opened, "an untouched composer has nothing to save");

        let mut typed = opened.clone();
        typed.body.insert_str(0, "on my way");
        assert_ne!(typed, opened);

        let mut added_someone = opened.clone();
        added_someone.cc = address::decode_list("bo@example.com");
        assert_ne!(added_someone, opened);
    }

    /// The composer round-trips its recipient fields through the same encoding
    /// the store uses, so a name containing a comma has to survive.
    #[test]
    fn recipient_fields_round_trip_through_the_text_field() {
        let people = vec![
            Address::new("Ruiz, Ana", "ana@example.com"),
            Address::bare("bo@example.com"),
        ];

        let encoded = address::encode_list(&people);
        assert_eq!(address::decode_list(&encoded), people);
    }

    #[test]
    fn a_draft_with_no_recipients_reports_why_it_cannot_send() {
        let draft = Draft::new(Address::new("Wes", "wes@example.com"));
        assert_eq!(draft.problem(), Some("no recipients"));
    }

    #[test]
    fn a_typo_in_a_recipient_is_caught_before_the_connection_opens() {
        let mut draft = Draft::new(Address::new("Wes", "wes@example.com"));
        draft.to = address::decode_list("not-an-address");
        // `decode_list` drops anything without an `@`, so this is empty rather
        // than invalid — either way it is not sendable.
        assert!(draft.problem().is_some());

        draft.to = address::decode_list("bo@example");
        assert_eq!(draft.problem(), Some("a recipient address looks wrong"));
    }
}
