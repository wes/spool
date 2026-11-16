//! The settings pane.
//!
//! A modal with a section list on the left and the section's controls on the
//! right — the shape settings panes have everywhere, chosen so that adding the
//! second and third section is layout that already exists rather than a
//! redesign.
//!
//! The theme cards paint themselves in the palette they are offering, so the
//! choice is made by looking rather than by reading four names.

use std::collections::{BTreeMap, BTreeSet};

use gpui::{
    AnyElement, Context, Entity, FontWeight, SharedString, Window, div, prelude::*, px,
};

use crate::{
    accounts_ui::AccountsState,
    settings::{Column, Density, Preview},
    text_input::TextInput,
    theme::{Theme, ThemeId},
    workspace::Workspace,
};

/// Sections in the left rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    /// First and default: setting up an account is what a new install needs,
    /// and re-authorizing one is what an old install comes here for.
    #[default]
    Accounts,
    Appearance,
    /// The composer's writing tools, and the model behind them.
    Writing,
    Extensions,
}

impl Section {
    pub const ALL: [Section; 4] =
        [Section::Accounts, Section::Appearance, Section::Writing, Section::Extensions];

    pub fn name(self) -> &'static str {
        match self {
            Section::Accounts => "Accounts",
            Section::Appearance => "Appearance",
            Section::Writing => "Writing",
            Section::Extensions => "Extensions",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Section::Accounts => "\u{2709}",
            Section::Appearance => "\u{25D0}",
            Section::Writing => "\u{2726}",
            Section::Extensions => "\u{2699}",
        }
    }
}

#[derive(Default)]
pub struct SettingsState {
    pub section: Section,
    pub accounts: AccountsState,
    pub extensions: ExtensionSettings,
}

/// One text field per declared setting, per extension.
///
/// Built when the section is opened rather than when the window is, because
/// most people never open it — and a field is an entity with a focus handle
/// and an event subscription, not a string.
#[derive(Default)]
pub struct ExtensionSettings {
    /// `(extension id, setting key)` to the field holding it.
    pub fields: BTreeMap<(String, String), Entity<TextInput>>,
    /// `(extension id, setting key)` to the rows of a list setting, each a
    /// field per column. A switch column is a field too, holding `true` or
    /// `false`, and drawn as a switch rather than as text.
    pub lists: BTreeMap<(String, String), Vec<BTreeMap<String, Entity<TextInput>>>>,
    /// Extensions whose fields differ from what is on disk.
    pub edited: BTreeSet<String>,
}

impl ExtensionSettings {
    pub fn field(&self, extension: &str, key: &str) -> Option<&Entity<TextInput>> {
        self.fields.get(&(extension.to_string(), key.to_string()))
    }
}

impl Workspace {
    /// The settings pane: the section rail and whichever section is showing.
    ///
    /// Rendered into a window of its own rather than over the mailbox. It is
    /// the same argument as the composer: settings are a second thing you are
    /// doing, usually *because* of something in the first, and a pane that
    /// covers the mail is a pane you cannot check anything against.
    pub(crate) fn render_settings(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.settings_ui.as_ref()?;
        let section = state.section;

        let rail = div()
            .flex()
            .flex_col()
            .gap_px()
            .w(px(168.))
            .flex_shrink_0()
            .p_2()
            .border_r_1()
            .border_color(theme.border)
            .children(Section::ALL.map(|item| {
                let selected = item == section;
                div()
                    .id(("settings-section", item as usize))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |s| s.bg(theme.selected))
                    .when(!selected, |s| s.hover(|s| s.bg(theme.hover)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(state) = this.settings_ui.as_mut() {
                            state.section = item;
                        }
                        if item == Section::Accounts {
                            this.load_accounts(cx);
                        }
                        if item == Section::Extensions {
                            this.load_extension_settings(cx);
                        }
                        if item == Section::Writing {
                            crate::assist::load_models(false, cx);
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .w(px(14.))
                            .flex_shrink_0()
                            .text_color(if selected { theme.accent } else { theme.text_faint })
                            .child(item.glyph()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(if selected { theme.text } else { theme.text_muted })
                            .child(item.name()),
                    )
            }));

        let body = match section {
            Section::Accounts => self.render_accounts(theme, window, cx),
            Section::Appearance => self.render_appearance(theme, cx),
            Section::Writing => self.render_writing(theme, cx),
            Section::Extensions => self.render_extensions(theme, window, cx),
        };

        Some(
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .child(rail)
                .child(div().flex().flex_1().min_w_0().child(body))
                .into_any_element(),
        )
    }

    fn render_appearance(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let active = self.settings.theme;

        div()
            .id("settings-appearance")
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
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Theme"),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child("Applies immediately."),
                    ),
            )
            .child(
                div().flex().gap_1p5().children(ThemeId::ALL.map(|id| {
                    toggle(("theme", id as usize), theme, id == active)
                        .on_click(cx.listener(move |this, _, _, cx| this.choose_theme(id, cx)))
                        .child(id.name())
                })),
            )
            .child(div().h(px(1.)).my_2().bg(theme.border_subtle))
            .child(self.render_message_list_settings(theme, cx))
            .child(div().h(px(1.)).my_2().bg(theme.border_subtle))
            .child(self.render_sidebar_settings(theme, cx))
            .child(div().h(px(1.)).my_2().bg(theme.border_subtle))
            .child(self.render_reading_settings(theme, cx))
            .into_any_element()
    }

    /// What is installed, what it is allowed to do, and what it still needs.
    ///
    /// Everything here is read from manifests and from the registry, so the
    /// list is complete whether or not a single component has been compiled —
    /// which is the whole point of an extension declaring itself in a file
    /// rather than at runtime. Nothing on this screen enters a sandbox.
    fn render_extensions(&self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let mut pane = div()
            .id("settings-extensions")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .gap_3()
            .p_4()
            .overflow_y_scroll();

        // Built in, so it is offered whether or not this build can run
        // anybody else's.
        pane = pane.child(self.render_clients_extension(theme, cx));

        let Some(host) = &self.extensions else {
            return pane
                .child(
                    div()
                        .text_color(theme.text_muted)
                        .child("Other extensions are not available in this build."),
                )
                .into_any_element();
        };

        let installed = host.installed();

        pane = pane.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text).child(
                    match installed.len() {
                        0 => "No extensions installed".to_string(),
                        1 => "1 extension".to_string(),
                        n => format!("{n} extensions"),
                    },
                ))
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Installed in {}. Switching one off takes effect at the next launch.",
                            host.root().display()
                        ))),
                ),
        );

        for extension in installed {
            pane = pane.child(self.render_one_extension(&extension, theme, window, cx));
        }

        // Directories that would not load. Shown rather than only logged: an
        // extension that is installed and invisible is worse than one that is
        // installed and visibly broken.
        for problem in host.problems() {
            pane = pane.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.red.opacity(0.4))
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(
                                problem
                                    .dir
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| problem.dir.display().to_string()),
                            )),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.red)
                            .child(SharedString::from(problem.why.clone())),
                    ),
            );
        }

        pane.into_any_element()
    }

    /// Clients, which comes with the app: added and taken away here, with
    /// nothing to install, and at once rather than at the next launch.
    fn render_clients_extension(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let on = self.settings.clients;

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Clients"),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(if on { "Added" } else { "Built in" }),
                    )
                    .when(on, |row| {
                        row.child(
                            toggle("clients-open", theme, false)
                                .on_click(cx.listener(|_, _, window, cx| {
                                    window.dispatch_action(Box::new(crate::actions::OpenClients), cx)
                                }))
                                .child("Open"),
                        )
                    })
                    .child(
                        toggle("clients-add", theme, !on)
                            .on_click(cx.listener(move |this, _, _, cx| this.set_clients_enabled(!on, cx)))
                            .child(if on { "Remove" } else { "Add" }),
                    ),
            )
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .child(
                        "A client book and a sales board, built from your mail: everyone you write to and \
                         everything you have said to each other, notes and tags, a board of where each sale \
                         stands, and a way to write to a group of them at once. Opens from the Window menu, \
                         \u{2318}\u{21e7}C, or the mark beside a sender.",
                    ),
            )
            .children(on.then(|| self.render_client_books(theme, cx)))
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child(match on {
                        false => "Adding it indexes who is on each message, a few seconds' work in the background. \
                                  Nothing leaves this Mac.",
                        true => "Removing it stops the index and puts Clients away. The book \u{2014} notes, tags, \
                                 the board \u{2014} is kept for if you add it back.",
                    }),
            )
            .into_any_element()
    }

    /// Which accounts' books Clients uses. A book switched off leaves the
    /// Clients sidebar, and so do the people whose mail is only in it.
    fn render_client_books(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let hidden: Vec<String> = cx
            .try_global::<crate::clients::ClientBook>()
            .map(|c| c.snapshot.hidden_books.clone())
            .unwrap_or_default();
        let mut list = div().flex().flex_col().gap_1().child(
            div()
                .text_size(theme.small_text_size)
                .text_color(theme.text_muted)
                .child("Books to keep clients in"),
        );
        for account in &self.accounts {
            let email = spool_core::address::normalize(&account.email);
            let shown = !hidden.contains(&email);
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        switch(("client-book", account.id.0 as usize), theme, shown).on_click(cx.listener(
                            move |_, _, _, cx| {
                                let Some(book) = cx.try_global::<crate::clients::ClientBook>().map(|c| c.book.clone())
                                else {
                                    return;
                                };
                                if let Err(e) = book.set_book_hidden(&email, shown) {
                                    log::warn!("putting a book away: {e:#}");
                                }
                                crate::clients::reload(cx);
                            },
                        )),
                    )
                    .child(div().size(px(7.)).rounded_full().bg(theme.account_color(account.color)))
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(if shown { theme.text } else { theme.text_faint })
                            .child(account.email.clone()),
                    ),
            );
        }
        list.into_any_element()
    }

    /// One extension: what it is, what it may do, and what it wants.
    fn render_one_extension(
        &self,
        extension: &spool_ext::registry::Installed,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = self.extensions.as_ref().expect("checked by the caller");
        let status = host.status(extension);
        let id = extension.id().to_string();
        // What it will be at the next launch, where that has been changed
        // since this one: switching takes effect then, and the switch should
        // show what was chosen rather than spring back.
        let pending = self.extension_switches.get(&id).copied();
        let enabled = pending.unwrap_or(extension.enabled);
        let summary = match pending.filter(|on| *on != extension.enabled) {
            Some(true) => "On at next launch".to_string(),
            Some(false) => "Off at next launch".to_string(),
            None => status.summary(),
        };

        let manifest = &extension.manifest;

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            // Name, status and the switch.
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(manifest.name.clone())),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(theme.small_text_size)
                            .text_color(match status.wants_attention() && pending.is_none() {
                                true => theme.red,
                                false => theme.text_faint,
                            })
                            .child(SharedString::from(summary)),
                    )
                    .child({
                        let for_toggle = id.clone();
                        switch(SharedString::from(format!("ext-{id}")), theme, enabled).on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.set_extension_enabled(&for_toggle, !enabled, cx);
                            }),
                        )
                    }),
            )
            .children((!manifest.description.trim().is_empty()).then(|| {
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .child(SharedString::from(manifest.description.clone()))
            }))
            // What it was told, and what it is still waiting for.
            .children((!manifest.settings().is_empty()).then(|| {
                div().flex().flex_col().gap_2().children(
                    manifest
                        .settings()
                        .iter()
                        .map(|decl| self.render_extension_setting(&id, decl, theme, window, cx)),
                )
            }))
            // Saving is explicit. A secret written to disk on every keystroke
            // is a secret written to disk forty times, and the half-typed ones
            // are as real as the last.
            .children((!manifest.settings().is_empty()).then(|| {
                let for_save = id.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(format!(
                                "Saved to {}",
                                extension.config_file().display()
                            ))),
                    )
                    .child(
                        toggle(SharedString::from(format!("save-{id}")), theme, false)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.save_extension_settings(&for_save, cx);
                            }))
                            .child("Save"),
                    )
            }))
            // Why it will not start, where that is the answer. The one thing
            // somebody in this pane needs and cannot get from anywhere else.
            .children(match &status {
                crate::extensions::Status::Failed(why) => Some(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.red)
                        .child(SharedString::from(why.clone())),
                ),
                _ => None,
            })
            .into_any_element()
    }

    /// One declared setting, as a labelled field.
    ///
    /// The label and the description come from the manifest, so this screen
    /// describes a setting it has never seen before — which is the whole point
    /// of an extension declaring what it wants rather than documenting it.
    fn render_extension_setting(
        &self,
        extension: &str,
        decl: &spool_ext::manifest::ConfigDecl,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(state) = self.settings_ui.as_ref() else { return div().into_any_element() };
        if decl.is_list() {
            return self.render_extension_list(extension, decl, theme, window, cx);
        }
        let Some(input) = state.extensions.field(extension, decl.key.as_str()) else {
            return div().into_any_element();
        };

        let input = input.clone();
        let focused = input.read(cx).focus_handle.is_focused(window);
        let empty = input.read(cx).text().trim().is_empty();
        // A required setting with nothing in it is the one thing on this
        // screen somebody has to act on, so it is the one thing coloured.
        let wanting = empty && decl.required && decl.default.is_none();

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(match wanting {
                                true => theme.red,
                                false => theme.text_faint,
                            })
                            .child(SharedString::from(decl.title.clone())),
                    )
                    .children((!decl.description.trim().is_empty()).then(|| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(decl.description.clone()))
                    })),
            )
            // A short list of values is picked from rather than typed, and
            // applied as soon as it is picked: there is nothing half-done
            // about choosing one of four.
            .when(!decl.choices.is_empty(), |this| {
                let current = match input.read(cx).text().trim() {
                    "" => decl.default.clone().unwrap_or_default(),
                    text => text.to_string(),
                };
                this.child(div().flex().flex_wrap().gap_1().children(decl.choices.iter().map(|choice| {
                    let (input, choice, extension) = (input.clone(), choice.clone(), extension.to_string());
                    toggle(
                        SharedString::from(format!("choice-{extension}-{}-{choice}", decl.key)),
                        theme,
                        current == choice,
                    )
                    .child(SharedString::from(choice.clone()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        input.update(cx, |input, cx| input.set_content(choice.clone(), cx));
                        this.save_extension_settings(&extension, cx);
                    }))
                })))
            })
            .when(decl.choices.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .bg(theme.background)
                        .border_1()
                        .border_color(match (focused, wanting) {
                            (true, _) => theme.focus_ring,
                            (false, true) => theme.red.opacity(0.5),
                            (false, false) => theme.border,
                        })
                        .child(input),
                )
            })
            .into_any_element()
    }

    /// A list setting: a card per row, a field per column, and a way to add
    /// and remove rows. Saved with the rest of the extension's settings.
    fn render_extension_list(
        &self,
        extension: &str,
        decl: &spool_ext::manifest::ConfigDecl,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use spool_ext::manifest::FieldKind;

        let key = decl.key.to_string();
        let rows = self
            .settings_ui
            .as_ref()
            .and_then(|s| s.extensions.lists.get(&(extension.to_string(), key.clone())))
            .cloned()
            .unwrap_or_default();

        let boxed = |input: Entity<TextInput>, window: &Window, cx: &gpui::App| {
            let focused = input.read(cx).focus_handle.is_focused(window);
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.background)
                .border_1()
                .border_color(if focused { theme.focus_ring } else { theme.border })
                .child(input)
        };

        let cards = rows.iter().enumerate().map(|(index, row)| {
            let mut line = div().flex().items_center().gap_2();
            let mut below = Vec::new();
            let mut first_text = true;

            for field in &decl.fields {
                let Some(input) = row.get(field.key.as_str()).cloned() else { continue };
                match field.kind {
                    FieldKind::Switch => {
                        let on = input.read(cx).text().trim() == "true";
                        let track = SharedString::from(format!("list-{extension}-{key}-{index}-{}-track", field.key));
                        let extension = extension.to_string();
                        line = line.child(
                            div()
                                .id(SharedString::from(format!("list-{extension}-{key}-{index}-{}", field.key)))
                                .flex()
                                .flex_shrink_0()
                                .items_center()
                                .gap_1p5()
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    input.update(cx, |input, cx| {
                                        input.set_content(if on { "false" } else { "true" }, cx)
                                    });
                                    if let Some(state) = this.settings_ui.as_mut() {
                                        state.extensions.edited.insert(extension.clone());
                                    }
                                    cx.notify();
                                }))
                                .child(switch(track, theme, on))
                                .child(
                                    div()
                                        .text_size(theme.small_text_size)
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(field.title.clone())),
                                ),
                        );
                    }
                    FieldKind::Text if first_text => {
                        first_text = false;
                        line = line.child(boxed(input, window, cx));
                    }
                    _ => below.push(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(theme.text_faint)
                                    .child(SharedString::from(field.title.clone())),
                            )
                            .child(boxed(input, window, cx)),
                    ),
                }
            }

            let (for_remove, key_remove) = (extension.to_string(), key.clone());
            line = line.child(
                div()
                    .id(SharedString::from(format!("list-{extension}-{key}-{index}-remove")))
                    .flex_shrink_0()
                    .cursor_pointer()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .hover(|s| s.text_color(theme.red))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.remove_list_row(&for_remove, &key_remove, index, cx)
                    }))
                    .child("Remove"),
            );

            // Each row its own raised card, so where one label ends and the
            // next begins is plain at a glance rather than read off a rule.
            div()
                .flex()
                .flex_col()
                .gap_2()
                .p_3()
                .rounded_lg()
                .bg(theme.elevated)
                .border_1()
                .border_color(theme.border)
                .child(line)
                .children(below)
        });

        let (for_add, key_add) = (extension.to_string(), key.clone());
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(SharedString::from(decl.title.clone())),
                    )
                    .children((!decl.description.trim().is_empty()).then(|| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(10.))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(decl.description.clone()))
                    })),
            )
            .children(cards)
            .child(
                div().flex().child(
                    toggle(SharedString::from(format!("list-{extension}-{key}-add")), theme, false)
                        .on_click(cx.listener(move |this, _, _, cx| this.add_list_row(&for_add, &key_add, cx)))
                        .child("Add"),
                ),
            )
            .into_any_element()
    }

    /// What the reader is allowed to fetch on your behalf.
    fn render_reading_settings(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let on = self.settings.load_remote_images;

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Images"),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(
                                "Fetching an image from a sender's server tells them when \
                                 their mail was opened, and from where. Off, every message \
                                 still offers to load its own.",
                            ),
                    ),
            )
            .child(
                toggle("remote-images", theme, on)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_remote_images_now(cx)))
                    .child(format!(
                        "{} Load images in messages",
                        if on { "\u{2713}" } else { "\u{2003}" }
                    )),
            )
            .children(self.render_trusted_senders(theme, cx))
            .into_any_element()
    }

    /// The senders whose images load without being asked for.
    ///
    /// Only when there are any: an empty list would be a heading explaining a
    /// feature nobody has used yet, and the place this is actually set up is
    /// the message itself. What this pane is for is the other half — seeing
    /// the whole list at once, and undoing one without waiting for that
    /// sender to write again.
    fn render_trusted_senders(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let domains = self.images.domains();
        if domains.is_empty() {
            return None;
        }

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(match domains.len() {
                            1 => "Images always load from this sender:".to_string(),
                            n => format!("Images always load from these {n} senders:"),
                        }),
                )
                .children(domains.into_iter().map(|domain| {
                    let id = SharedString::from(domain.clone());
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_3()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme.panel)
                        .text_size(theme.small_text_size)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .line_clamp(1)
                                .text_color(theme.text)
                                .child(id.clone()),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("forget-sender-{id}")))
                                .flex_shrink_0()
                                .px_2()
                                .py_0p5()
                                .rounded_md()
                                .cursor_pointer()
                                .text_color(theme.text_muted)
                                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.forget_image_domain(domain.clone(), cx)
                                }))
                                .child("Stop"),
                        )
                }))
                .into_any_element(),
        )
    }

    /// What the sidebar shows above the accounts.
    fn render_sidebar_settings(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let on = self.settings.unified_inbox;

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Sidebar"),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(
                                "\u{201c}All Mail\u{201d} stacks every account into one \
                                 list. Useful for triage, and off unless you want it.",
                            ),
                    ),
            )
            .child(
                toggle("unified-inbox", theme, on)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_unified_inbox_now(cx)))
                    .child(format!(
                        "{} Show All Mail",
                        if on { "\u{2713}" } else { "\u{2003}" }
                    )),
            )
            .into_any_element()
    }

    /// How the message list draws itself: the row shape, and what goes in it.
    fn render_message_list_settings(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let settings = &self.settings.messages;

        let body = match settings.density {
            // Slim rows are columns, so the choice is which columns.
            Density::Slim => div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(caption(theme, "Columns"))
                .child(
                    div().flex().flex_wrap().gap_1p5().children(Column::ALL.map(|column| {
                        let on = settings.columns.shows(column);
                        // The last remaining column cannot be turned off: a
                        // row with nothing in it is not a state worth being
                        // able to click your way into.
                        let locked = !settings.columns.can_toggle(column);
                        toggle(("column", column as usize), theme, on)
                            .when(locked, |s| s.opacity(0.5))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_column(column, cx)
                            }))
                            .child(format!(
                                "{} {}",
                                if on { "\u{2713}" } else { "\u{2003}" },
                                column.label()
                            ))
                    })),
                ),

            // Detail rows have a preview, and the question is how much of it.
            Density::Detail => div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(caption(theme, "Message preview"))
                .child(
                    div().flex().gap_1p5().children(Preview::ALL.map(|option| {
                        toggle(("preview", option as usize), theme, option == settings.preview)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.choose_preview(option, cx)
                            }))
                            .child(option.label())
                    })),
                ),
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child("Message list"),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child("Applies immediately."),
                    ),
            )
            .child(
                div().flex().gap_2().children(Density::ALL.map(|option| {
                    let selected = option == self.settings.messages.density;
                    div()
                        .id(("density", option as usize))
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .w(px(220.))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .cursor_pointer()
                        .border_1()
                        .border_color(if selected { theme.text } else { theme.border })
                        // Inverted, like a chosen toggle.
                        .when(selected, |s| s.bg(theme.text))
                        .when(!selected, |s| s.hover(|s| s.bg(theme.hover)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.choose_density(option, cx)
                        }))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(if selected { theme.background } else { theme.text })
                                        .child(option.label()),
                                )
                                .child(
                                    div()
                                        .text_color(theme.background)
                                        .child(if selected { "\u{2713}" } else { "" }),
                                ),
                        )
                        .child(
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(match selected {
                                    true => theme.background.opacity(0.65),
                                    false => theme.text_faint,
                                })
                                .child(option.description()),
                        )
                })),
            )
            .child(body)
            .into_any_element()
    }

}

/// A label above a group of controls.
pub(crate) fn caption(theme: &Theme, text: &'static str) -> gpui::Div {
    div()
        .text_size(theme.small_text_size)
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text_muted)
        .child(text)
}

/// A small on/off or one-of-many control.
/// An on/off switch: a track with a knob that sits right when on.
///
/// The one drawing of it, for everything that is a switch rather than a
/// choice — an extension, and an extension's work in a mailbox.
pub(crate) fn switch(id: impl Into<gpui::ElementId>, theme: &Theme, on: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .w(px(24.))
        .h(px(14.))
        .px(px(2.))
        .rounded_full()
        .cursor_pointer()
        .bg(if on { theme.accent } else { theme.border })
        .when(on, |s| s.justify_end())
        .child(div().size(px(10.)).rounded_full().bg(theme.background))
}

pub(crate) fn toggle(
    id: impl Into<gpui::ElementId>,
    theme: &Theme,
    on: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(theme.small_text_size)
        .border_1()
        // Chosen is inverted — filled with the text colour, lettered in the
        // background's — so it reads at a glance in either theme rather than
        // being told apart from its neighbours by a slightly lighter grey.
        .when(on, |s| {
            s.border_color(theme.text).bg(theme.text).text_color(theme.background)
        })
        .when(!on, |s| {
            s.border_color(theme.border)
                .text_color(theme.text_muted)
                .hover(|s| s.bg(theme.hover))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_section_has_a_label() {
        for section in Section::ALL {
            assert!(!section.name().is_empty());
            assert!(!section.glyph().is_empty());
        }
    }

    #[test]
    fn the_pane_opens_on_accounts() {
        assert_eq!(SettingsState::default().section, Section::Accounts);
    }
}
