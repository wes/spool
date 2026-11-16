//! The composer, writing to a group of clients.
//!
//! The same window as any other message — the page, the From menu, text or
//! HTML, the writing tools, the signature, attachments — with the To line
//! given over to a group. Everyone in it gets a copy of their own, filled in
//! for them (see `clients_mail`). Who they are runs down the right, each with
//! a box to leave them out, and choosing one shows what they will get.
//!
//! Nothing here is a draft. A message full of `{first_name}` is not one to
//! anybody, and the group is not something the store can keep; closing one
//! that has been written asks first instead.

use gpui::{AnyElement, App, Context, FontWeight, SharedString, Window, div, prelude::*, px};
use spool_core::{
    draft::{Draft, Format},
    markup,
};

use super::{Composer, Focus, INK, PAGE};
use crate::{
    clients_mail::{self, FIELDS, MOST, Recipient, fill, fill_html, leaves_a_gap},
    clients_window::chip,
    theme::Theme,
    workspace::Workspace,
};

/// Who a message to a group goes to.
pub(crate) struct Group {
    /// What the group is, for the title: "Proposal", "#vip", "3 people".
    pub name: String,
    pub recipients: Vec<Recipient>,
    /// Whose copy the preview shows, by place in `recipients`.
    pub preview: usize,
    /// Send has been pressed once, and asks to be pressed again.
    pub confirm: bool,
    /// Escape has been pressed once on a message that closing would lose.
    pub closing: bool,
}

impl Group {
    pub(crate) fn new(name: String, recipients: Vec<Recipient>) -> Self {
        let preview = recipients.iter().position(|r| r.included).unwrap_or(0);
        Group { name, recipients, preview, confirm: false, closing: false }
    }

    fn included(&self) -> Vec<Recipient> {
        self.recipients.iter().filter(|r| r.included).cloned().collect()
    }
}

impl Workspace {
    /// Opens a composer on a message to a group: `draft` is what it starts
    /// with — a greeting and the signature, or a message being sent again.
    pub(crate) fn open_group_composer(&mut self, group: Group, draft: Draft, cx: &mut Context<Self>) {
        self.open_composer_with(&draft, Focus::Subject, None, Some(group), cx);
    }
}

impl Composer {
    /// Why it cannot go yet, if it cannot.
    pub(super) fn group_problem(&self, cx: &App) -> Option<String> {
        let group = self.group.as_ref()?;
        let count = group.recipients.iter().filter(|r| r.included).count();
        let body = self.body.read(cx).text();
        let written = &body[spool_core::assist::written_part(body)];
        match () {
            _ if count == 0 => Some("nobody is ticked".into()),
            _ if count > MOST => Some(format!(
                "that is {count} people, and one sending takes {MOST} at most \u{2014} about what a mail account may send in a day"
            )),
            // Asked now rather than once it has begun, when the only thing
            // left to say is that none of it went.
            _ if !clients_mail::can_send_as(&self.from.addr) => {
                Some(format!("{} cannot send from this copy of Spool", self.from.addr))
            }
            _ if self.subject.read(cx).text().trim().is_empty() => Some("it needs a subject".into()),
            _ if written.trim().is_empty() => Some("it needs something to say".into()),
            _ => None,
        }
    }

    /// Sends to the group: on the second press, because the first only says
    /// how many it is about to go to.
    pub(super) fn send_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(problem) = self.group_problem(cx) {
            self.error = Some(format!("Cannot send: {problem}"));
            if let Some(group) = self.group.as_mut() {
                group.confirm = false;
            }
            cx.notify();
            return;
        }
        let draft = self.draft(cx);
        let Some(group) = self.group.as_mut() else { return };
        if !group.confirm {
            group.confirm = true;
            self.error = None;
            cx.notify();
            return;
        }

        // The body already carries the signature, as any message in the
        // composer does, so it goes as written.
        let job = clients_mail::Job {
            from: draft.from.clone(),
            subject: draft.subject.trim().to_string(),
            body: draft.body.clone(),
            html: draft.html.clone(),
            attachments: draft.attachments.clone(),
            recipients: group.included(),
        };
        match clients_mail::start(job, group.name.clone(), self.store.clone(), cx) {
            Ok(()) => {
                self.settled.set(true);
                self.sending = true;
                clients_mail::show_progress(cx);
                self.take_off(window, cx);
            }
            Err(problem) => {
                self.error = Some(problem);
                group.confirm = false;
            }
        }
        cx.notify();
    }

    /// Whether closing should wait. A group message is kept nowhere, so the
    /// first Escape or ⌘W on one that has been written says so, and the
    /// second closes.
    pub(super) fn keep_group_open(&mut self, cx: &mut Context<Self>) -> bool {
        let untouched = self.untouched(cx);
        let Some(group) = self.group.as_mut() else { return false };
        if untouched || group.closing {
            return false;
        }
        group.closing = true;
        self.error = Some("This message is not saved anywhere \u{2014} press Escape again to throw it away".into());
        cx.notify();
        true
    }

    /// Puts a field into the message where the caret is.
    fn insert_field(&mut self, field: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(target_os = "macos")]
        if let Some(editor) = self.active_editor() {
            editor.borrow().insert_text(field);
            return;
        }
        let body = self.body.clone();
        body.update(cx, |body, cx| {
            let text = body.text().to_string();
            let at = body.selection();
            let (start, end) = (at.start.min(text.len()), at.end.min(text.len()));
            body.set_text(format!("{}{field}{}", &text[..start], &text[end..]), cx);
            let caret = start + field.len();
            body.select(caret..caret, cx);
        });
        let focus = body.read(cx).focus_handle.clone();
        window.focus(&focus);
        cx.notify();
    }

    /// What stands on the To line: the group, and how many it is.
    pub(super) fn render_group_to(&self, theme: &Theme) -> Option<AnyElement> {
        let group = self.group.as_ref()?;
        let count = group.recipients.iter().filter(|r| r.included).count();
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(SharedString::from(group.name.clone())))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "{count} {}, each sent a copy of their own",
                            if count == 1 { "person" } else { "people" }
                        ))),
                )
                .into_any_element(),
        )
    }

    /// The fields a message to a group can be filled in with.
    pub(super) fn render_fill_row(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.group.as_ref()?;
        let mut row = div().flex().flex_wrap().items_center().gap_1();
        for field in FIELDS {
            row = row.child(
                chip(SharedString::from(format!("compose-field-{field}")), field, false, theme)
                    .on_click(cx.listener(move |this, _, window, cx| this.insert_field(field, window, cx))),
            );
        }
        Some(
            row.child(
                div()
                    .pl_1()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child("blank when somebody has none \u{2014} {first_name|there} says what instead"),
            )
            .into_any_element(),
        )
    }

    /// Down the right: everyone in the group, and what the chosen one gets.
    pub(super) fn render_group_panel(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let group = self.group.as_ref()?;
        let subject = self.subject.read(cx).text().to_string();
        // What is filled in: the HTML while it is HTML.
        let template = match (self.format, &self.html) {
            (Format::Html, Some(html)) => html.clone(),
            _ => self.body.read(cx).text().to_string(),
        };
        let html = self.format == Format::Html && self.html.is_some();
        let count = group.recipients.iter().filter(|r| r.included).count();
        let gaps = group
            .recipients
            .iter()
            .filter(|r| r.included && (leaves_a_gap(&subject, r) || leaves_a_gap(&template, r)))
            .count();
        let all_on = group.recipients.iter().filter(|r| r.why_not != Some("No address to write to")).all(|r| r.included);
        let preview_at = group.preview;

        let rows = group.recipients.iter().enumerate().map(|(i, recipient)| {
            let ticked = recipient.included;
            let can = recipient.why_not != Some("No address to write to");
            let chosen = i == preview_at;
            div()
                .id(("group-to", i))
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_sm()
                .cursor_pointer()
                .when(chosen, |row| row.bg(theme.selected))
                .when(!chosen, |row| row.hover(|s| s.bg(theme.hover)))
                // The row shows their copy; the box leaves them out.
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(group) = this.group.as_mut() {
                        group.preview = i;
                    }
                    cx.notify();
                }))
                .child(
                    div()
                        .id(("group-tick", i))
                        .flex_shrink_0()
                        .size(px(13.))
                        .rounded(px(3.))
                        .border_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(10.))
                        .when(ticked, |b| b.bg(theme.accent).border_color(theme.accent).text_color(theme.text_on_accent).child("\u{2713}"))
                        .when(!ticked, |b| b.border_color(theme.border))
                        .when(can, |b| {
                            b.on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                if let Some(group) = this.group.as_mut()
                                    && let Some(recipient) = group.recipients.get_mut(i)
                                {
                                    recipient.included = !recipient.included;
                                    group.confirm = false;
                                }
                                cx.notify();
                            }))
                        }),
                )
                .child({
                    // Their name, and their address under it; or, for somebody
                    // the mail never named, the address alone — a dash where a
                    // name would be says nothing the address does not.
                    let (first, second) = match &recipient.name {
                        Some(name) => (name.clone(), Some(recipient.addr.clone())),
                        None => (recipient.addr.clone(), None),
                    };
                    let second = match (second, recipient.why_not.filter(|_| !ticked)) {
                        (Some(addr), Some(why)) => Some(format!("{addr} \u{00b7} {why}")),
                        (None, Some(why)) => Some(why.to_string()),
                        (second, None) => second,
                    };
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .text_color(if ticked { theme.text } else { theme.text_faint })
                                .child(SharedString::from(first)),
                        )
                        .children(second.map(|line| {
                            div()
                                .truncate()
                                .text_size(theme.small_text_size)
                                .text_color(theme.text_faint)
                                .child(SharedString::from(line))
                        }))
                })
                .into_any_element()
        });

        let preview = group.recipients.get(preview_at).map(|recipient| {
            let name = recipient.name.clone().unwrap_or_else(|| recipient.addr.clone());
            let filled_subject = fill(&subject, recipient);
            let filled_body = match html {
                true => markup::to_text(&fill_html(&template, recipient)),
                false => fill(&template, recipient),
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_muted)
                        .child(div().flex_1().min_w_0().truncate().child(SharedString::from(match recipient.included {
                            true => format!("What {name} gets"),
                            false => format!("What {name} would get"),
                        }))),
                )
                .child(
                    div()
                        .id("group-preview")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .p_3()
                        .rounded_md()
                        .bg(PAGE)
                        .text_color(INK)
                        .text_size(theme.small_text_size)
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(match filled_subject.trim() {
                            "" => "(no subject)".to_string(),
                            s => s.to_string(),
                        })))
                        .children(filled_body.lines().map(|line| div().min_h(px(14.)).child(SharedString::from(line.to_string())))),
                )
        });

        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .w(px(340.))
                .flex_shrink_0()
                .h_full()
                .p_3()
                .bg(theme.panel)
                .border_l_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(format!(
                            "{count} of {} get it",
                            group.recipients.len()
                        ))))
                        .child(chip("group-all", if all_on { "Untick all" } else { "Tick all" }, false, theme).on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Some(group) = this.group.as_mut() {
                                    for recipient in group.recipients.iter_mut().filter(|r| r.why_not != Some("No address to write to")) {
                                        recipient.included = !all_on;
                                    }
                                    group.confirm = false;
                                }
                                cx.notify();
                            }),
                        )),
                )
                .child(
                    div()
                        .id("group-recipients")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h(px(120.))
                        .overflow_y_scroll()
                        .children(rows),
                )
                .children((gaps > 0).then(|| {
                    div().text_size(theme.small_text_size).text_color(theme.yellow).child(SharedString::from(format!(
                        "{gaps} of them {} a blank where a field has nothing to fill it",
                        if gaps == 1 { "gets" } else { "get" }
                    )))
                }))
                .children(preview)
                .into_any_element(),
        )
    }

    /// The Send button's words, and the line beside it.
    pub(super) fn group_send_labels(&self) -> Option<(String, String)> {
        let group = self.group.as_ref()?;
        let count = group.recipients.iter().filter(|r| r.included).count();
        let emails = format!("{count} email{}, one each", if count == 1 { "" } else { "s" });
        let button = match group.confirm {
            true => format!("Send all {count} now"),
            false => format!("Send to {count}"),
        };
        Some((emails, button))
    }
}
