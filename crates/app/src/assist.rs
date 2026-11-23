//! Whether the AI features can run, shared by every window, and running
//! them.
//!
//! A global rather than a field on the workspace: composers are windows of
//! their own and draw while the workspace may be mid-update, and reading an
//! entity that is being updated is a panic. A global can be read from any
//! window's render, and observed, so an open composer shows the tools the
//! moment the person signs in from Settings.
//!
//! Two providers can be connected — ChatGPT, through the sign-in the Codex
//! CLI uses (see `spool_net::chatgpt`), and Claude, through an Anthropic API
//! key (see `spool_net::claude`) — and one of them is chosen. Everything that
//! asks a model goes through `spool_net::ai::respond`, which asks the chosen
//! one.

use anyhow::Result;
use gpui::{AnyElement, App, FontWeight, Global, SharedString, div, prelude::*, px};
use spool_core::assist::{
    Answering, Request, Write, clean, read_suggestions, read_written, restore_layout,
    suggestion_instructions, suggestion_prompt,
};
use spool_net::ai::{self, Offered, Provider};
use spool_net::{chatgpt, claude};

use crate::{
    settings::Settings,
    settings_ui::{caption, switch, toggle},
    text_input::TextInput,
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
    SignedIn(chatgpt::Account),
}

/// The Anthropic API key, as far as Settings knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Checking,
    Missing,
    /// Being checked with Anthropic.
    Connecting,
    Connected(claude::Key),
}

pub struct Assistant {
    /// Which of the two the features use.
    pub provider: Provider,
    /// The ChatGPT sign-in.
    pub connection: Connection,
    /// Why the last ChatGPT sign-in or sign-out did not work.
    pub error: Option<String>,
    /// What Settings offers to pick from, for ChatGPT.
    pub models: Models,
    pub key: KeyState,
    /// Why the last key was not accepted.
    pub key_error: Option<String>,
    pub claude_models: Models,
}

/// A provider's models, as far as Settings knows.
///
/// Read when the AI section is opened rather than at launch: for ChatGPT,
/// reading them renews the sign-in, and most launches never open Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Models {
    Unread,
    Reading,
    Read(Offered),
    Failed(String),
}

impl Assistant {
    /// Whether the chosen provider can be asked.
    pub fn is_ready(&self) -> bool {
        match self.provider {
            Provider::ChatGpt => matches!(self.connection, Connection::SignedIn(_)),
            Provider::Claude => matches!(self.key, KeyState::Connected(_)),
        }
    }

    /// Whether it is still too early to say: the keychain has not answered.
    pub fn is_checking(&self) -> bool {
        match self.provider {
            Provider::ChatGpt => self.connection == Connection::Checking,
            Provider::Claude => self.key == KeyState::Checking,
        }
    }

    /// What to do to make the features work, for a link where they are
    /// missing.
    pub fn what_to_connect(&self) -> &'static str {
        match self.provider {
            Provider::ChatGpt => "Sign in with ChatGPT",
            Provider::Claude => "Add a Claude API key",
        }
    }

    fn models_mut(&mut self, provider: Provider) -> &mut Models {
        match provider {
            Provider::ChatGpt => &mut self.models,
            Provider::Claude => &mut self.claude_models,
        }
    }

    fn connected(&self, provider: Provider) -> bool {
        match provider {
            Provider::ChatGpt => matches!(self.connection, Connection::SignedIn(_)),
            Provider::Claude => matches!(self.key, KeyState::Connected(_)),
        }
    }
}

impl Global for Assistant {}

pub fn init(settings: &Settings, cx: &mut App) {
    ai::set_provider(settings.ai_provider);
    chatgpt::set_model(settings.writing_model.clone());
    claude::set_model(settings.claude_model.clone());
    cx.set_global(Assistant {
        provider: settings.ai_provider,
        connection: Connection::Checking,
        error: None,
        models: Models::Unread,
        key: KeyState::Checking,
        key_error: None,
        claude_models: Models::Unread,
    });

    cx.spawn(async move |cx| {
        let found = cx
            .background_executor()
            .spawn(async { (chatgpt::signed_in(), claude::connected()) })
            .await;
        cx.update(|cx| {
            let assistant = cx.global_mut::<Assistant>();
            let (chatgpt, claude) = found;
            match chatgpt {
                Ok(Some(account)) => assistant.connection = Connection::SignedIn(account),
                Ok(None) => assistant.connection = Connection::SignedOut,
                Err(e) => {
                    log::warn!("reading the ChatGPT sign-in: {e:#}");
                    assistant.connection = Connection::SignedOut;
                    assistant.error = Some(format!("{e:#}"));
                }
            }
            match claude {
                Ok(Some(key)) => assistant.key = KeyState::Connected(key),
                Ok(None) => assistant.key = KeyState::Missing,
                Err(e) => {
                    log::warn!("reading the Claude API key: {e:#}");
                    assistant.key = KeyState::Missing;
                    assistant.key_error = Some(format!("{e:#}"));
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

/// Checks a pasted key with Anthropic, and keeps it if it works.
fn connect_claude(key: String, cx: &mut App) {
    let assistant = cx.global_mut::<Assistant>();
    if assistant.key == KeyState::Connecting {
        return;
    }
    let before = std::mem::replace(&mut assistant.key, KeyState::Connecting);
    assistant.key_error = None;
    cx.refresh_windows();

    cx.spawn(async move |cx| {
        let result = cx.background_executor().spawn(async move { claude::connect(&key).await }).await;
        cx.update(|cx| {
            let assistant = cx.global_mut::<Assistant>();
            match result {
                Ok(key) => {
                    assistant.key = KeyState::Connected(key);
                    assistant.claude_models = Models::Unread;
                }
                Err(e) => {
                    log::warn!("the Claude API key was not accepted: {e:#}");
                    // A key that was already working still is: a typo in a
                    // replacement does not disconnect it.
                    assistant.key = match before {
                        KeyState::Connected(key) => KeyState::Connected(key),
                        _ => KeyState::Missing,
                    };
                    assistant.key_error = Some(format!("{e:#}"));
                }
            }
            load_models(false, cx);
            cx.refresh_windows();
        })
        .ok();
    })
    .detach();
}

fn disconnect_claude(cx: &mut App) {
    let assistant = cx.global_mut::<Assistant>();
    match claude::disconnect() {
        Ok(()) => {
            assistant.key = KeyState::Missing;
            assistant.key_error = None;
            assistant.claude_models = Models::Unread;
        }
        Err(e) => assistant.key_error = Some(format!("{e:#}")),
    }
    cx.refresh_windows();
}

/// Reads the chosen provider's models for Settings, if it is connected and
/// they have not been read; `again` reads them even if they have.
pub fn load_models(again: bool, cx: &mut App) {
    let assistant = cx.global_mut::<Assistant>();
    let provider = assistant.provider;
    let connected = assistant.connected(provider);
    let models = assistant.models_mut(provider);
    let wanted = match models {
        Models::Reading => false,
        Models::Unread | Models::Failed(_) => true,
        Models::Read(_) => again,
    };
    if !connected || !wanted {
        return;
    }
    *models = Models::Reading;
    cx.refresh_windows();

    cx.spawn(async move |cx| {
        let read = cx
            .background_executor()
            .spawn(async move {
                match provider {
                    Provider::ChatGpt => chatgpt::models().await,
                    Provider::Claude => claude::models().await,
                }
            })
            .await;
        cx.update(|cx| {
            let assistant = cx.global_mut::<Assistant>();
            // Signed out while it was on the wire.
            if !assistant.connected(provider) {
                return;
            }
            *assistant.models_mut(provider) = match read {
                Ok(offered) => Models::Read(offered),
                Err(e) => {
                    log::warn!("reading the {} models: {e:#}", provider.name());
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
    let reply = ai::respond(&request.instructions(), request.prompt()).await?;
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
    let reply = ai::respond(&request.instructions(), &request.prompt()).await?;
    let (subject, body) = read_written(&reply, request.wants_subject());
    if body.trim().is_empty() {
        anyhow::bail!("{} sent back an empty email. Try again.", ai::provider().name());
    }
    Ok((subject, body))
}

/// Directions a reply to this message could take, specific to it.
pub async fn suggest(answering: &Answering) -> Result<Vec<String>> {
    let reply = ai::respond(&suggestion_instructions(), &suggestion_prompt(answering)).await?;
    Ok(read_suggestions(&reply))
}

impl Workspace {
    /// Builds what the AI section needs that has to be made rather than
    /// drawn: the field a Claude key is pasted into.
    pub(crate) fn prepare_ai_settings(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(state) = self.settings_ui.as_mut() else { return };
        if state.claude_key.is_none() {
            state.claude_key = Some(cx.new(|cx| TextInput::masked("sk-ant-\u{2026}", cx)));
        }
        load_models(false, cx);
    }

    /// The AI section of Settings: which provider, whether it can run here
    /// and, if not, the one thing to do about it; then what uses it.
    pub(crate) fn render_ai(&self, theme: &Theme, window: &gpui::Window, cx: &mut gpui::Context<Self>) -> AnyElement {
        let provider = cx.global::<Assistant>().provider;
        let note = |text: SharedString| div().text_size(theme.small_text_size).text_color(theme.text_faint).child(text);

        // Made now rather than as the row is drawn: each holds a listener,
        // and `cx` is wanted again below.
        let choices: Vec<_> = Provider::ALL
            .into_iter()
            .enumerate()
            .map(|(i, choice)| {
                toggle(("ai-provider", i), theme, choice == provider)
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_provider(choice, cx)))
                    .child(choice.name())
            })
            .collect();

        let connection = match provider {
            Provider::ChatGpt => self.render_chatgpt(theme, cx),
            Provider::Claude => self.render_claude(theme, window, cx),
        };

        div()
            .id("settings-ai")
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
                    .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text).child("AI"))
                    .child(note(
                        "The writing tools in the composer, and sorting your mail by what needs you, \
                         use the AI you connect here."
                            .into(),
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(caption(theme, "Provider"))
                    .child(div().flex().gap_1().children(choices)),
            )
            .child(connection)
            .child(div().h_px().bg(theme.border_subtle))
            .child(self.render_priority_settings(theme, cx))
            .into_any_element()
    }

    fn render_chatgpt(&self, theme: &Theme, cx: &mut gpui::Context<Self>) -> AnyElement {
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
            .flex()
            .flex_col()
            .gap_3()
            .child(status_line(theme, ready, status))
            .children(button.map(|button| div().flex().child(button)))
            .children(error.map(|error| error_line(theme, error)))
            .children(ready.then(|| self.render_model_picker(Provider::ChatGpt, theme, cx)))
            .child(
                div().text_size(theme.small_text_size).text_color(theme.text_faint).child(
                    "Uses your ChatGPT plan, and counts toward its usage. Signing in uses the same \
                     sign-in as OpenAI\u{2019}s Codex app. OpenAI doesn\u{2019}t offer one for other \
                     apps, so this is unofficial and may stop working.",
                ),
            )
            .into_any_element()
    }

    fn render_claude(&self, theme: &Theme, window: &gpui::Window, cx: &mut gpui::Context<Self>) -> AnyElement {
        let assistant = cx.global::<Assistant>();
        let key = assistant.key.clone();
        let error = assistant.key_error.clone();
        let field = self.settings_ui.as_ref().and_then(|s| s.claude_key.clone());

        let status: SharedString = match &key {
            KeyState::Checking => "Checking\u{2026}".into(),
            KeyState::Missing => "No API key yet.".into(),
            KeyState::Connecting => "Checking the key with Anthropic\u{2026}".into(),
            KeyState::Connected(key) => format!("Connected with the key ending {}.", key.hint).into(),
        };
        let ready = matches!(key, KeyState::Connected(_));

        let entry = (!ready && key != KeyState::Checking).then(|| {
            let focused = field.as_ref().is_some_and(|f| f.read(cx).focus_handle.is_focused(window));
            div()
                .flex()
                .items_center()
                .gap_2()
                .children(field.clone().map(|field| {
                    div()
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .max_w(px(360.))
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(theme.background)
                        .border_1()
                        .border_color(if focused { theme.focus_ring } else { theme.border })
                        .text_color(theme.text)
                        .child(field)
                }))
                .child(
                    toggle("claude-connect", theme, false)
                        .on_click(cx.listener(|this, _, _, cx| {
                            let Some(field) = this.settings_ui.as_ref().and_then(|s| s.claude_key.clone()) else {
                                return;
                            };
                            let key = field.read(cx).text().to_string();
                            field.update(cx, |field, cx| field.clear(cx));
                            connect_claude(key, cx);
                        }))
                        .child("Connect"),
                )
        });

        let disconnect = ready.then(|| {
            div().flex().child(
                toggle("claude-disconnect", theme, false)
                    .on_click(|_, _, cx| disconnect_claude(cx))
                    .child("Remove key"),
            )
        });

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(status_line(theme, ready, status))
            .children(entry)
            .children(disconnect)
            .children(error.map(|error| error_line(theme, error)))
            .children(ready.then(|| self.render_model_picker(Provider::Claude, theme, cx)))
            .child(
                div().text_size(theme.small_text_size).text_color(theme.text_faint).child(
                    "Make a key in the Anthropic Console, at console.anthropic.com. Requests are \
                     billed to that account. Anthropic doesn\u{2019}t let other apps use a Claude.ai \
                     plan, so a key is the way in. It is kept in your keychain.",
                ),
            )
            .into_any_element()
    }

    /// The model a provider is asked with: its default, or one picked from
    /// the list it offers, read live so a new model shows up here without an
    /// update to Spool.
    fn render_model_picker(&self, provider: Provider, theme: &Theme, cx: &mut gpui::Context<Self>) -> AnyElement {
        let assistant = cx.global::<Assistant>();
        let models = match provider {
            Provider::ChatGpt => assistant.models.clone(),
            Provider::Claude => assistant.claude_models.clone(),
        };
        let picked = match provider {
            Provider::ChatGpt => self.settings.writing_model.clone(),
            Provider::Claude => self.settings.claude_model.clone(),
        };
        let note = |text: String| div().text_size(theme.small_text_size).text_color(theme.text_faint).child(text);

        let body = match models {
            // Opened while the keychain was still being read, before there
            // was a sign-in to read them with.
            Models::Unread => note("Refresh to list the models you can use.".into()),
            Models::Reading => note("Reading the models you can use\u{2026}".into()),
            Models::Failed(error) => div()
                .text_size(theme.small_text_size)
                .text_color(theme.red)
                .child(format!("Couldn\u{2019}t read the models: {error}")),
            Models::Read(offered) => {
                let name_of = |slug: &str| offered.models.iter().find(|m| m.slug == slug).map(|m| m.name.clone());
                // A pick no longer offered is not what is used, so it is not
                // what is shown as chosen.
                let current = picked.clone().filter(|slug| name_of(slug).is_some());
                let automatic = match offered.default.as_deref().and_then(name_of) {
                    Some(name) => format!("Automatic ({name})"),
                    None => "Automatic".to_string(),
                };

                let choices = std::iter::once((None, automatic))
                    .chain(offered.models.iter().map(|m| (Some(m.slug.clone()), m.name.clone())))
                    .enumerate()
                    .map(|(i, (slug, name))| {
                        toggle(("ai-model", i), theme, slug == current)
                            .on_click(cx.listener(move |this, _, _, cx| this.choose_model(provider, slug.clone(), cx)))
                            .child(name)
                    });

                let explanation = match (&picked, &current) {
                    (Some(slug), None) => format!("{slug} is no longer offered, so the automatic choice is used."),
                    (_, Some(slug)) => offered
                        .models
                        .iter()
                        .find(|m| &m.slug == slug)
                        .and_then(|m| m.description.clone())
                        .unwrap_or_default(),
                    (None, None) => match provider {
                        Provider::ChatGpt => "Follows the model ChatGPT recommends, including when that changes.".into(),
                        Provider::Claude => {
                            "Claude Opus 5.5. A smaller model, such as Haiku, costs less to sort a busy inbox with."
                                .into()
                        }
                    },
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
                            .id("ai-models-refresh")
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

    /// The switches for what reads mail on its own: the Priority sort, and
    /// the quick replies it makes possible.
    fn render_priority_settings(&self, theme: &Theme, cx: &mut gpui::Context<Self>) -> AnyElement {
        let provider = cx.global::<Assistant>().provider;
        let on = self.settings.priority;
        let replies = self.settings.quick_replies;
        let problem = self.priority.problem.clone().filter(|_| on);
        let follow_ups = self.settings.follow_ups;
        let follow_up_problem = self.follow_ups.problem.clone().filter(|_| follow_ups);
        let row = |id: &'static str, on: bool, title: &'static str, detail: String| {
            div()
                .id(id)
                .flex()
                .items_start()
                .gap_3()
                .cursor_pointer()
                .child(div().pt_0p5().child(switch((id, 0usize), theme, on)))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w_0()
                        .child(div().text_color(theme.text).child(title))
                        .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(detail)),
                )
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                row(
                    "priority-switch",
                    on,
                    "Sort new mail by priority",
                    format!(
                        "New mail in your inboxes is sent to {} to decide whether it needs you, and \
                         what for. The \u{2605} button above the list, or P, then narrows any \
                         mailbox to that mail; in All Mail, every inbox's. Mail from mailing lists \
                         is sorted without it.",
                        provider.company()
                    ),
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_priority(!this.settings.priority, cx))),
            )
            .child(
                row(
                    "quick-replies-switch",
                    on && replies,
                    "Write quick replies",
                    "For priority mail that wants an answer, three short replies are written ahead \
                     of time. Pick one to open it as a reply, ready to change and send. Nothing is \
                     ever sent without you."
                        .into(),
                )
                .when(!on, |row| row.opacity(0.5))
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.settings.priority {
                        this.settings.quick_replies = !this.settings.quick_replies;
                        this.persist_settings(cx);
                        this.sort_new_mail(cx);
                        cx.notify();
                    }
                })),
            )
            // Sorting happens out of sight, so this is the one place a
            // failure can be seen. Cleared by the next run that works.
            .children(problem.map(|problem| error_line(theme, format!("The last sort stopped: {problem}"))))
            .child(
                row(
                    "follow-ups-switch",
                    follow_ups,
                    "Remind me when nobody replies",
                    format!(
                        "Mail you send is watched for an answer. If none comes in 3 days, or the \
                         wait you pick by Send, {} checks whether it expected one, and if so it \
                         shows above the list with a follow-up ready. Nothing is ever sent \
                         without you.",
                        provider.company()
                    ),
                )
                .on_click(cx.listener(|this, _, _, cx| this.set_follow_ups(!this.settings.follow_ups, cx))),
            )
            .children(
                follow_up_problem
                    .map(|problem| error_line(theme, format!("The last follow-up check stopped: {problem}"))),
            )
            .into_any_element()
    }

    fn choose_provider(&mut self, provider: Provider, cx: &mut gpui::Context<Self>) {
        if self.settings.ai_provider == provider {
            return;
        }
        ai::set_provider(provider);
        cx.global_mut::<Assistant>().provider = provider;
        self.settings.ai_provider = provider;
        self.persist_settings(cx);
        self.prepare_ai_settings(cx);
        self.sort_new_mail(cx);
        cx.refresh_windows();
        cx.notify();
    }

    fn choose_model(&mut self, provider: Provider, model: Option<String>, cx: &mut gpui::Context<Self>) {
        let slot = match provider {
            Provider::ChatGpt => &mut self.settings.writing_model,
            Provider::Claude => &mut self.settings.claude_model,
        };
        if *slot == model {
            return;
        }
        *slot = model.clone();
        match provider {
            Provider::ChatGpt => chatgpt::set_model(model),
            Provider::Claude => claude::set_model(model),
        }
        self.persist_settings(cx);
        cx.notify();
    }
}

fn status_line(theme: &Theme, ready: bool, status: SharedString) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(div().size(px(8.)).rounded_full().flex_shrink_0().bg(if ready { theme.green } else { theme.text_faint }))
        .child(div().text_color(theme.text).child(status))
}

fn error_line(theme: &Theme, error: String) -> gpui::Div {
    div().text_size(theme.small_text_size).text_color(theme.red).child(SharedString::from(error))
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Opens Settings on the AI section, from a composer or a message that wants
/// to say why its tools are missing.
pub fn open_settings(workspace: &gpui::WeakEntity<Workspace>, cx: &mut App) {
    if let Some(workspace) = workspace.upgrade() {
        crate::settings_window::open_at(workspace, crate::settings_ui::Section::Ai, cx);
    }
}
