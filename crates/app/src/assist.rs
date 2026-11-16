//! Whether the writing tools can run, shared by every window, and running
//! them.
//!
//! A global rather than a field on the workspace: composers are windows of
//! their own and draw while the workspace may be mid-update, and reading an
//! entity that is being updated is a panic. A global can be read from any
//! window's render, and observed, so an open composer shows the tools the
//! moment the person signs in from Settings.
//!
//! The tools run on the person's ChatGPT account, through the sign-in the
//! Codex CLI uses (see `spool_net::chatgpt` for what that means).

use anyhow::Result;
use gpui::{AnyElement, App, FontWeight, Global, SharedString, div, prelude::*, px};
use spool_core::assist::{
    Answering, Request, Write, clean, read_suggestions, read_written, restore_layout,
    suggestion_instructions, suggestion_prompt,
};
use spool_net::chatgpt::{self, Account, Offered};

use crate::{
    settings_ui::{caption, toggle},
    theme::Theme,
    workspace::Workspace,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Connection {
    /// The keychain has not been read yet, so nothing claims the tools are
    /// unavailable before anyone knows.
    Checking,
    SignedOut,
    /// Waiting on the browser.
    SigningIn,
    SignedIn(Account),
}

pub struct Assistant {
    pub connection: Connection,
    /// Why the last sign-in or sign-out did not work.
    pub error: Option<String>,
    /// What Settings offers to pick from.
    pub models: Models,
}

/// The account's models, as far as Settings knows.
///
/// Read when the Writing section is opened rather than at launch: reading
/// it renews the sign-in, and most launches never open Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Models {
    Unread,
    Reading,
    Read(Offered),
    Failed(String),
}

impl Assistant {
    pub fn is_ready(&self) -> bool {
        matches!(self.connection, Connection::SignedIn(_))
    }
}

impl Global for Assistant {}

/// `model` is the one picked in Settings, if any.
pub fn init(model: Option<String>, cx: &mut App) {
    chatgpt::set_model(model);
    cx.set_global(Assistant { connection: Connection::Checking, error: None, models: Models::Unread });

    cx.spawn(async move |cx| {
        let found = cx.background_executor().spawn(async { chatgpt::signed_in() }).await;
        cx.update(|cx| {
            let assistant = cx.global_mut::<Assistant>();
            match found {
                Ok(Some(account)) => assistant.connection = Connection::SignedIn(account),
                Ok(None) => assistant.connection = Connection::SignedOut,
                Err(e) => {
                    log::warn!("reading the ChatGPT sign-in: {e:#}");
                    assistant.connection = Connection::SignedOut;
                    assistant.error = Some(format!("{e:#}"));
                }
            }
            cx.refresh_windows();
        })
        .ok();
    })
    .detach();
}

fn sign_in(cx: &mut App) {
    let assistant = cx.global_mut::<Assistant>();
    if assistant.connection == Connection::SigningIn {
        return;
    }
    assistant.connection = Connection::SigningIn;
    assistant.error = None;
    cx.refresh_windows();

    cx.spawn(async move |cx| {
        let result = cx
            .background_executor()
            .spawn(async { chatgpt::sign_in(crate::accounts_ui::open_in_browser).await })
            .await;
        cx.update(|cx| {
            let assistant = cx.global_mut::<Assistant>();
            match result {
                Ok(account) => assistant.connection = Connection::SignedIn(account),
                Err(e) => {
                    log::warn!("ChatGPT sign-in failed: {e:#}");
                    assistant.connection = Connection::SignedOut;
                    assistant.error = Some(format!("{e:#}"));
                }
            }
            // Settings is where the sign-in started, so it is showing.
            load_models(true, cx);
            cx.refresh_windows();
        })
        .ok();
    })
    .detach();
}

fn sign_out(cx: &mut App) {
    let assistant = cx.global_mut::<Assistant>();
    match chatgpt::sign_out() {
        Ok(()) => {
            assistant.connection = Connection::SignedOut;
            assistant.error = None;
            assistant.models = Models::Unread;
        }
        Err(e) => assistant.error = Some(format!("{e:#}")),
    }
    cx.refresh_windows();
}

/// Reads the account's models for Settings, if signed in and not already
/// read; `again` reads them even if they were.
pub fn load_models(again: bool, cx: &mut App) {
    let assistant = cx.global_mut::<Assistant>();
    let wanted = match assistant.models {
        Models::Reading => false,
        Models::Unread | Models::Failed(_) => true,
        Models::Read(_) => again,
    };
    if !assistant.is_ready() || !wanted {
        return;
    }
    assistant.models = Models::Reading;
    cx.refresh_windows();

    cx.spawn(async move |cx| {
        let read = cx.background_executor().spawn(async { chatgpt::models().await }).await;
        cx.update(|cx| {
            let assistant = cx.global_mut::<Assistant>();
            // Signed out while it was on the wire.
            if !assistant.is_ready() {
                return;
            }
            assistant.models = match read {
                Ok(offered) => Models::Read(offered),
                Err(e) => {
                    log::warn!("reading the ChatGPT models: {e:#}");
                    Models::Failed(format!("{e:#}"))
                }
            };
            cx.refresh_windows();
        })
        .ok();
    })
    .detach();
}

/// What a tool produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Changed(String),
    /// The model had nothing to change.
    Unchanged,
}

/// Runs a writing tool, keeping the passage's layout.
///
/// The reply is put back into the original's line structure when it has as
/// many lines (see `assist::restore_layout`), which undoes a stray blank line
/// or a dropped list marker. When it has more or fewer, the model merged or
/// split lines on purpose — Shorten does that — and its version is used.
pub async fn run(request: &Request) -> Result<Outcome> {
    let reply = chatgpt::respond(&request.instructions(), request.prompt()).await?;
    let text = clean(&reply);
    let original = request.passage.trim();

    let changed = restore_layout(&request.passage, &text).unwrap_or(text);
    match changed.trim() == original {
        true => Ok(Outcome::Unchanged),
        false => Ok(Outcome::Changed(changed)),
    }
}

/// Writes an email from a short description: the subject, when one was
/// asked for and given, and the body.
pub async fn write(request: &Write) -> Result<(Option<String>, String)> {
    let reply = chatgpt::respond(&request.instructions(), &request.prompt()).await?;
    let (subject, body) = read_written(&reply, request.wants_subject());
    if body.trim().is_empty() {
        anyhow::bail!("ChatGPT sent back an empty email. Try again.");
    }
    Ok((subject, body))
}

/// Directions a reply to this message could take, specific to it.
pub async fn suggest(answering: &Answering) -> Result<Vec<String>> {
    let reply = chatgpt::respond(&suggestion_instructions(), &suggestion_prompt(answering)).await?;
    Ok(read_suggestions(&reply))
}

impl Workspace {
    /// The Writing section of Settings: whether the tools can run here and,
    /// if not, the one thing to do about it.
    pub(crate) fn render_writing(&self, theme: &Theme, cx: &mut gpui::Context<Self>) -> AnyElement {
        let assistant = cx.global::<Assistant>();
        let connection = assistant.connection.clone();
        let error = assistant.error.clone();

        let status: SharedString = match &connection {
            Connection::Checking => "Checking\u{2026}".into(),
            Connection::SignedOut => "Not signed in.".into(),
            Connection::SigningIn => "Finish signing in in your browser\u{2026}".into(),
            Connection::SignedIn(account) => {
                let who = account.email.as_deref().unwrap_or("your ChatGPT account");
                match account.plan.as_deref() {
                    Some(plan) => format!("Signed in as {who} ({}).", capitalized(plan)).into(),
                    None => format!("Signed in as {who}.").into(),
                }
            }
        };
        let ready = matches!(connection, Connection::SignedIn(_));

        let button = match &connection {
            Connection::Checking => None,
            Connection::SignedOut => Some(
                toggle("writing-sign-in", theme, false)
                    .on_click(|_, _, cx| sign_in(cx))
                    .child("Sign in with ChatGPT\u{2026}"),
            ),
            // Again, for a browser tab that was closed: the listener is still
            // waiting, and a second press is ignored until it gives up.
            Connection::SigningIn => None,
            Connection::SignedIn(_) => Some(
                toggle("writing-sign-out", theme, false)
                    .on_click(|_, _, cx| sign_out(cx))
                    .child("Sign out"),
            ),
        };

        div()
            .id("settings-writing")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_3()
            .p_4()
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text).child("Writing tools"))
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(
                                "Rewrite, proofread, shorten and change the tone of what you are \
                                 writing, from the composer. They use your ChatGPT account and \
                                 count toward its plan\u{2019}s usage. The text you run a tool on is \
                                 sent to OpenAI; nothing else is.",
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(8.)).rounded_full().flex_shrink_0().bg(if ready {
                        theme.green
                    } else {
                        theme.text_faint
                    }))
                    .child(div().text_color(theme.text).child(status)),
            )
            .children(button.map(|button| div().flex().child(button)))
            .children(error.map(|error| {
                div().text_size(theme.small_text_size).text_color(theme.red).child(SharedString::from(error))
            }))
            .children(ready.then(|| self.render_model_picker(theme, cx)))
            .child(
                div().text_size(theme.small_text_size).text_color(theme.text_faint).child(
                    "Signing in uses the same sign-in as OpenAI\u{2019}s Codex app. OpenAI doesn\u{2019}t \
                     offer one for other apps, so this is unofficial and may stop working.",
                ),
            )
            .into_any_element()
    }
}

impl Workspace {
    /// The model the tools use: the account's default, or one picked from
    /// the list it offers, read live so a new model shows up here without
    /// an update to Spool.
    fn render_model_picker(&self, theme: &Theme, cx: &mut gpui::Context<Self>) -> AnyElement {
        let models = cx.global::<Assistant>().models.clone();
        let picked = self.settings.writing_model.clone();
        let note = |text: String| div().text_size(theme.small_text_size).text_color(theme.text_faint).child(text);

        let body = match models {
            // Opened while the keychain was still being read, before there
            // was a sign-in to read them with.
            Models::Unread => note("Refresh to list the models your account offers.".into()),
            Models::Reading => note("Reading the models your account offers\u{2026}".into()),
            Models::Failed(error) => div()
                .text_size(theme.small_text_size)
                .text_color(theme.red)
                .child(format!("Couldn\u{2019}t read the models: {error}")),
            Models::Read(offered) => {
                let name_of = |slug: &str| offered.models.iter().find(|m| m.slug == slug).map(|m| m.name.clone());
                // A pick the account no longer offers is not what is used, so
                // it is not what is shown as chosen.
                let current = picked.clone().filter(|slug| name_of(slug).is_some());
                let automatic = match offered.default.as_deref().and_then(name_of) {
                    Some(name) => format!("Automatic ({name})"),
                    None => "Automatic".to_string(),
                };

                let choices = std::iter::once((None, automatic))
                    .chain(offered.models.iter().map(|m| (Some(m.slug.clone()), m.name.clone())))
                    .enumerate()
                    .map(|(i, (slug, name))| {
                        toggle(("writing-model", i), theme, slug == current)
                            .on_click(cx.listener(move |this, _, _, cx| this.choose_writing_model(slug.clone(), cx)))
                            .child(name)
                    });

                let explanation = match (&picked, &current) {
                    (Some(slug), None) => format!(
                        "{slug} is no longer offered, so the writing tools use the automatic choice."
                    ),
                    (_, Some(slug)) => offered
                        .models
                        .iter()
                        .find(|m| &m.slug == slug)
                        .and_then(|m| m.description.clone())
                        .unwrap_or_default(),
                    (None, None) => "Follows the model ChatGPT recommends, including when that changes.".into(),
                };

                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(div().flex().flex_wrap().gap_1().children(choices))
                    .when(!explanation.is_empty(), |this| this.child(note(explanation)))
            }
        };

        div()
            .flex()
            .flex_col()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(caption(theme, "Model"))
                    .child(
                        div()
                            .id("writing-models-refresh")
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .cursor_pointer()
                            .hover(|s| s.text_color(theme.text))
                            .on_click(|_, _, cx| load_models(true, cx))
                            .child("Refresh"),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    fn choose_writing_model(&mut self, model: Option<String>, cx: &mut gpui::Context<Self>) {
        if self.settings.writing_model == model {
            return;
        }
        chatgpt::set_model(model.clone());
        self.settings.writing_model = model;
        self.persist_settings(cx);
        cx.notify();
    }
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Opens Settings on the Writing section, from a composer that wants to say
/// why its tools are missing.
pub fn open_settings(workspace: &gpui::WeakEntity<Workspace>, cx: &mut App) {
    if let Some(workspace) = workspace.upgrade() {
        crate::settings_window::open_at(workspace, crate::settings_ui::Section::Writing, cx);
    }
}
