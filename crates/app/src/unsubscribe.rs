//! The reader's unsubscribe button.
//!
//! A message from a list says how to leave it in `List-Unsubscribe`, and this
//! turns that into a button beside the sender. Pressing it does it, in the
//! background: a one-click POST or a short email, whichever the list offers,
//! with a bar under the header saying how it went. A list that offers only a
//! web page gets that page opened instead.
//!
//! Mail stored before these headers were kept has not got them. Opening such a
//! message asks the server for its header block, once per message, after the
//! cursor has settled on it — so travelling down the list with `j` asks for
//! nothing.

use std::time::Duration;

use gpui::{AnyElement, ClickEvent, Context, SharedString, Window, div, prelude::*};
use spool_core::{AccountId, Body, Envelope, MailboxRole, MessageId, unsubscribe::Method};

use crate::{actions::Unsubscribe, theme::Theme, workspace::Workspace};

/// How long a message has to stay open before its headers are fetched.
///
/// Each fetch is a connection to the server; this is what keeps skimming past
/// a hundred old messages from being a hundred of them.
const LOOKUP_DWELL: Duration = Duration::from_millis(700);

/// An unsubscribe the reader has started.
pub(crate) struct Unsubscribing {
    pub message: MessageId,
    pub method: Method,
    /// What the list is called in sentences: the sender's name.
    pub list: String,
    pub stage: Stage,
}

#[derive(Clone, PartialEq)]
pub(crate) enum Stage {
    Working,
    Done,
    /// The list's page is open in the browser; the rest is up to the reader.
    Opened,
    Failed(String),
}

impl Workspace {
    pub(crate) fn unsubscribe_action(&mut self, _: &Unsubscribe, _: &mut Window, cx: &mut Context<Self>) {
        match self.open.as_ref().map(|open| open.envelope.id) {
            Some(id) => self.unsubscribe_from(id, cx),
            None => self.say("Open a message first", cx),
        }
    }

    /// How this message could be unsubscribed from, if the button belongs on
    /// it at all.
    ///
    /// Never in Junk. Unsubscribing tells the sender that the address is read,
    /// and to a spammer that is the one thing worth knowing.
    pub(crate) fn unsubscribe_method(&self, envelope: &Envelope, body: &Body) -> Option<Method> {
        let junk = self
            .mailboxes
            .iter()
            .any(|m| m.id == envelope.mailbox_id && m.role == MailboxRole::Junk);
        if junk {
            return None;
        }
        body.list_unsubscribe.method()
    }

    /// Unsubscribes from the list one message came from.
    pub(crate) fn unsubscribe_from(&mut self, id: MessageId, cx: &mut Context<Self>) {
        if self.demo {
            self.say("The demo's mailing lists are made up, so there is nothing to leave", cx);
            return;
        }
        // A second press while the first is still going is the same request.
        if self.unsubscribe.as_ref().is_some_and(|u| u.message == id && u.stage == Stage::Working) {
            return;
        }

        let message = self.open.iter().chain(self.thread.iter()).find(|m| m.envelope.id == id);
        let Some(message) = message else { return };

        let Some(method) = self.unsubscribe_method(&message.envelope, &message.body) else {
            let text = match message.body.list_unsubscribe.is_unknown() {
                true => "Still checking whether this message is from a list",
                false => "This message does not say how to unsubscribe",
            };
            self.say(text, cx);
            return;
        };

        let from = &message.envelope.from;
        let list = match from.name.trim() {
            "" => from.addr.clone(),
            name => name.to_string(),
        };
        let account = message.envelope.account_id;

        if let Method::Browser(url) = &method {
            cx.open_url(url);
            self.unsubscribe = Some(Unsubscribing { message: id, method, list, stage: Stage::Opened });
            cx.notify();
            return;
        }

        // The account the list wrote to is the one a `mailto:` is sent from:
        // that is the address the list knows.
        let Some(config) = self.account_config(account) else {
            let reason = "this message's account is not set up in Spool".to_string();
            self.unsubscribe =
                Some(Unsubscribing { message: id, method, list, stage: Stage::Failed(reason) });
            cx.notify();
            return;
        };

        self.unsubscribe = Some(Unsubscribing {
            message: id,
            method: method.clone(),
            list: list.clone(),
            stage: Stage::Working,
        });
        cx.notify();

        // Detached: once started, it finishes whether or not the reader
        // stays on the message.
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { spool_net::unsubscribe::unsubscribe(&config, &method).await })
                .await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        log::info!("unsubscribed from {list} (message {id})");
                        this.set_unsubscribe_stage(id, Stage::Done);
                        this.say(format!("Unsubscribed from {list}"), cx);
                    }
                    Err(e) => {
                        log::warn!("unsubscribing from {list} failed: {e:#}");
                        this.set_unsubscribe_stage(id, Stage::Failed(format!("{e:#}")));
                        this.say(format!("Could not unsubscribe from {list}"), cx);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn dismiss_unsubscribe(&mut self, cx: &mut Context<Self>) {
        self.unsubscribe = None;
        cx.notify();
    }

    /// Moves the bar on, if it is still about this message.
    ///
    /// The reader may have moved to another message since, in which case the
    /// bar is gone and the status line is where the outcome gets said.
    fn set_unsubscribe_stage(&mut self, id: MessageId, stage: Stage) {
        if let Some(state) = self.unsubscribe.as_mut().filter(|u| u.message == id) {
            state.stage = stage;
        }
    }

    /// The configuration for one of the store's accounts.
    fn account_config(&self, account: AccountId) -> Option<spool_net::AccountConfig> {
        let email = &self.accounts.iter().find(|a| a.id == account)?.email;
        self.account_configs().into_iter().find(|c| c.email.eq_ignore_ascii_case(email))
    }

    /// Asks the server for the open message's list headers, when nobody has.
    ///
    /// Called whenever a message finishes opening. Does nothing for a message
    /// whose headers are already known, one already asked about this session
    /// — successfully or not — or one without a place on a server.
    pub(crate) fn look_up_list_unsubscribe(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let id = open.envelope.id;
        let account_id = open.envelope.account_id;
        if !open.body.list_unsubscribe.is_unknown() || !self.list_header_lookups.insert(id) {
            return;
        }

        let store = self.store.clone();
        let Some(email) = self.accounts.iter().find(|a| a.id == account_id).map(|a| a.email.clone())
        else {
            return;
        };

        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOOKUP_DWELL).await;

            // Still the open message? A lookup for one already left behind is
            // forgotten, so it can be asked again when it is next opened.
            let wanted = this
                .update(cx, |this, _| {
                    let open = this.open.as_ref().is_some_and(|o| o.envelope.id == id);
                    if !open {
                        this.list_header_lookups.remove(&id);
                    }
                    open
                })
                .unwrap_or(false);
            if !wanted {
                return;
            }

            let found = cx
                .background_executor()
                .spawn(async move {
                    // Read here rather than up front: it is a file, and this
                    // is the side of the await that may touch the disk.
                    let config = spool_net::Config::load(spool_net::Config::default_path())?
                        .accounts
                        .into_iter()
                        .find(|c| c.email.eq_ignore_ascii_case(&email))
                        .ok_or_else(|| anyhow::anyhow!("{email} is not configured"))?;
                    let origin = store.message_origin(id)?.ok_or_else(|| {
                        anyhow::anyhow!("the message has no copy on the server")
                    })?;
                    let list = spool_net::fetch_list_unsubscribe(&config, &origin).await?;
                    store.set_list_unsubscribe(id, &list)?;
                    anyhow::Ok(list)
                })
                .await;

            let list = match found {
                Ok(list) => list,
                Err(e) => {
                    log::debug!("no list headers for message {id}: {e:#}");
                    return;
                }
            };

            this.update(cx, |this, cx| {
                for message in this.open.iter_mut().chain(this.thread.iter_mut()) {
                    if message.envelope.id == id {
                        message.body.list_unsubscribe = list.clone();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The control beside the sender: a button, or what became of pressing it.
    pub(crate) fn render_unsubscribe_button(
        &self,
        envelope: &Envelope,
        body: &Body,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.unsubscribe_method(envelope, body)?;
        let id = envelope.id;

        let stage = self.unsubscribe.as_ref().filter(|u| u.message == id).map(|u| &u.stage);
        let settled = match stage {
            Some(Stage::Working) => Some("Unsubscribing\u{2026}"),
            Some(Stage::Done) => Some("Unsubscribed"),
            _ => None,
        };
        if let Some(label) = settled {
            return Some(
                div()
                    .px_2()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(label)
                    .into_any_element(),
            );
        }

        Some(
            div()
                .id(("unsubscribe", id.0 as u64))
                .px_2()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .text_size(theme.small_text_size)
                .text_color(theme.text_muted)
                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.unsubscribe_from(id, cx)
                }))
                .child("Unsubscribe")
                .into_any_element(),
        )
    }

    /// The bar under the header, while an unsubscribe for this message is
    /// under way and once it has an answer.
    pub(crate) fn render_unsubscribe_bar(
        &self,
        id: MessageId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.unsubscribe.as_ref().filter(|u| u.message == id)?;
        let list = &state.list;
        let target = state.method.target();

        let text = match &state.stage {
            Stage::Working => match state.method {
                Method::Mail { .. } => {
                    format!("Unsubscribing from {list} \u{2014} emailing {target}\u{2026}")
                }
                _ => format!("Unsubscribing from {list} \u{2014} asking {target}\u{2026}"),
            },
            // Said plainly, because it is what makes a message from them
            // tomorrow not look like the button failed.
            Stage::Done => format!(
                "Unsubscribed from {list}. Mail already on its way may still arrive for a day \
                 or two."
            ),
            Stage::Opened => {
                format!("{list} can only be left from its own page \u{2014} opened {target} in your browser.")
            }
            Stage::Failed(reason) => format!("Could not unsubscribe from {list}: {reason}"),
        };

        let failed = matches!(state.stage, Stage::Failed(_));
        let working = state.stage == Stage::Working;

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
                .border_color(if failed { theme.red } else { theme.border })
                .text_size(theme.small_text_size)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(if failed { theme.red } else { theme.text_muted })
                        .child(SharedString::from(text)),
                )
                .when(!working, |bar| {
                    bar.child(
                        div()
                            .id("unsubscribe-dismiss")
                            .flex_shrink_0()
                            .px_2p5()
                            .py_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(theme.text_muted)
                            .hover(|s| s.bg(theme.hover).text_color(theme.text))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.dismiss_unsubscribe(cx)
                            }))
                            .child("Dismiss"),
                    )
                })
                .into_any_element(),
        )
    }
}
