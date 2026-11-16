//! Bringing people in: by hand, from a file, and from a source on a schedule.
//!
//! An import is the one place in the book where a machine decides something
//! about what a person meant — which column is the address — so the sheet
//! shows its guess and lets it be changed before anything is written. Every
//! column says what it was taken to be and what is in it, and a click cycles
//! it through the alternatives. A wrong guess costs a click, not an undo of
//! two thousand rows.
//!
//! Sources are the same import, run again on a timer by `clients`. The sheet
//! here is where one is described and tried: "Test" pulls it once and says
//! what came back, without keeping any of it, so a URL or a token that is
//! wrong is found out before it is saved and starts failing every hour.

use std::path::PathBuf;

use gpui::{AnyElement, Context, Div, Entity, FontWeight, SharedString, Window, div, prelude::*, px};
use spool_core::contacts::{self, Column, Contact, Table};
use spool_store::book::{Kind, Source, SourceFormat};

use crate::{
    actions::Confirm,
    clients::{self, ClientBook},
    clients_profile::link,
    clients_window::{ClientsWindow, Sheet, chip},
    clock::now_seconds,
    text_input::TextInput,
    theme::Theme,
};

pub(crate) struct NewClientForm {
    pub kind: Kind,
    pub name: Entity<TextInput>,
    pub email: Entity<TextInput>,
    pub book: Option<String>,
}

pub(crate) struct ImportSheet {
    pub file: String,
    /// A CSV or JSON file, whose columns are mapped here.
    pub table: Option<Table>,
    /// A vCard file, which says what each field is itself.
    pub cards: Option<Vec<Contact>>,
    pub columns: Vec<Column>,
    pub book: Option<String>,
    pub tags: Entity<TextInput>,
    pub importing: bool,
}

impl ImportSheet {
    fn contacts(&self) -> Vec<Contact> {
        match (&self.cards, &self.table) {
            (Some(cards), _) => cards.clone(),
            (None, Some(table)) => contacts::contacts(table, &self.columns),
            (None, None) => Vec::new(),
        }
    }
}

pub(crate) struct SourcesSheet {
    pub sources: Vec<Source>,
    /// The source being added or changed.
    pub form: Option<SourceForm>,
}

pub(crate) struct SourceForm {
    pub id: i64,
    pub name: Entity<TextInput>,
    pub url: Entity<TextInput>,
    pub token: Entity<TextInput>,
    pub tags: Entity<TextInput>,
    pub format: SourceFormat,
    pub every: i64,
    pub book: Option<String>,
    pub enabled: bool,
    /// A token is already in the keychain, so an empty field keeps it.
    pub has_token: bool,
    /// What the last "Test" found.
    pub tested: Option<Result<String, String>>,
    pub testing: bool,
}

/// How often a source can be pulled, as the sheet offers it.
const SCHEDULES: [(i64, &str); 5] = [(900, "15 min"), (3_600, "Hourly"), (21_600, "6 hours"), (86_400, "Daily"), (0, "By hand")];

impl ClientsWindow {
    pub(crate) fn open_new_client(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = cx.new(|cx| TextInput::new("Ana Ruiz, or Acme Inc.", cx));
        let email = cx.new(|cx| TextInput::new("ana@acme.com", cx));
        let focus = name.read(cx).focus_handle.clone();
        window.focus(&focus);
        let book = self.book_filter.as_ref().map(|b| b.email.clone());
        self.sheet = Some(Sheet::NewClient(NewClientForm { kind: Kind::Person, name, email, book }));
        cx.notify();
    }

    fn create_client(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::NewClient(form)) = self.sheet.as_ref() else { return };
        let kind = form.kind;
        let name = form.name.read(cx).text().trim().to_string();
        let address = form.email.read(cx).text().trim().trim_start_matches('@').to_string();
        let book = form.book.clone();
        if name.is_empty() && address.is_empty() {
            self.status = "A client needs a name or an address".into();
            cx.notify();
            return;
        }
        self.sheet = None;
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let books = global.book.clone();
        cx.spawn(async move |this, cx| {
            let made = cx
                .background_executor()
                .spawn(async move {
                    let now = now_seconds();
                    match (kind, address.contains('@')) {
                        (Kind::Company, false) if !address.is_empty() => {
                            let id = books.ensure_company(&address, &name, now)?;
                            if let Some(book) = &book {
                                books.add_to_book(id, book)?;
                            }
                            Ok(id)
                        }
                        _ => {
                            let addr = (!address.is_empty()).then_some(address.as_str());
                            books.create(kind, &name, addr, book.as_deref(), now)
                        }
                    }
                })
                .await;
            this.update(cx, |this, _| match made {
                Ok(id) => {
                    this.status = "Client added".into();
                    this.selected = Some(clients::Key::Client(id));
                    this.section = clients::Section::Everyone;
                }
                Err(e) => this.status = format!("{e:#}").into(),
            })
            .ok();
            cx.update(clients::reload).ok();
        })
        .detach();
    }

    /// Asks for a file, then opens the sheet on what is in it.
    pub(crate) fn open_import(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        let book = self.book_filter.as_ref().map(|b| b.email.clone()).or_else(|| self.accounts.first().map(|a| a.email.clone()));
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let read = cx.background_executor().spawn(async move { read_contacts_file(path) }).await;
            this.update(cx, |this, cx| {
                match read {
                    Ok((file, table, cards)) => {
                        let columns = table.as_ref().map(contacts::guess).unwrap_or_default();
                        let tags = cx.new(|cx| TextInput::new("Tags for everyone imported, e.g. signup, 2026", cx));
                        this.sheet = Some(Sheet::Import(ImportSheet { file, table, cards, columns, book, tags, importing: false }));
                    }
                    Err(e) => this.status = format!("{e:#}").into(),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn run_import(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::Import(sheet)) = self.sheet.as_mut() else { return };
        if sheet.importing {
            return;
        }
        let rows = sheet.contacts();
        let book = sheet.book.clone();
        let tags: Vec<String> = sheet
            .tags
            .read(cx)
            .text()
            .split(',')
            .map(|t| t.trim().trim_start_matches('#').to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let file = sheet.file.clone();
        sheet.importing = true;
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let books = global.book.clone();
        cx.spawn(async move |this, cx| {
            let report = cx
                .background_executor()
                .spawn(async move { books.import(&rows, book.as_deref(), None, &tags, now_seconds()) })
                .await;
            this.update(cx, |this, cx| {
                this.sheet = None;
                this.status = match report {
                    Ok(report) => format!("{file}: {}", report.summary()).into(),
                    Err(e) => format!("Import failed: {e:#}").into(),
                };
                this.section = clients::Section::Clients;
                cx.notify();
            })
            .ok();
            cx.update(clients::reload).ok();
        })
        .detach();
    }

    pub(crate) fn open_sources(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.sheet = Some(Sheet::Sources(SourcesSheet { sources: Vec::new(), form: None }));
        self.reload_sources(cx);
        cx.notify();
    }

    fn reload_sources(&mut self, cx: &mut Context<Self>) {
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let book = global.book.clone();
        cx.spawn(async move |this, cx| {
            let sources = cx.background_executor().spawn(async move { book.sources().unwrap_or_default() }).await;
            this.update(cx, |this, cx| {
                if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut() {
                    sheet.sources = sources;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn edit_source(&mut self, source: Option<Source>, window: &mut Window, cx: &mut Context<Self>) {
        let source = source.unwrap_or_else(|| Source {
            every: 3_600,
            enabled: true,
            book: self.book_filter.as_ref().map(|b| b.email.clone()).or_else(|| self.accounts.first().map(|a| a.email.clone())).unwrap_or_default(),
            ..Default::default()
        });
        let input = |cx: &mut Context<Self>, placeholder: &str, text: String| {
            let placeholder = placeholder.to_string();
            cx.new(move |cx| {
                let mut input = TextInput::new(placeholder, cx);
                input.set_content(text, cx);
                input
            })
        };
        let name = input(cx, "Website signups", source.name.clone());
        let url = input(cx, "https://example.com/admin/signups.json?since={since}", source.url.clone());
        let token = cx.new(|cx| TextInput::masked("Token sent as Authorization: Bearer \u{2026} (optional)", cx));
        let tags = input(cx, "signup", source.tags.join(", "));
        let focus = name.read(cx).focus_handle.clone();
        window.focus(&focus);
        let id = source.id;
        // Whether a token is already saved is the keychain's to say, which is
        // a process to run: asked in the background, and the hint follows.
        if id != 0 {
            cx.spawn(async move |this, cx| {
                let has = smol::unblock(move || crate::client_sync::has_token(id)).await;
                this.update(cx, |this, cx| {
                    if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                        && let Some(form) = sheet.form.as_mut().filter(|f| f.id == id)
                    {
                        form.has_token = has;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        let has_token = false;
        if let Some(Sheet::Sources(sheet)) = self.sheet.as_mut() {
            sheet.form = Some(SourceForm {
                id,
                name,
                url,
                token,
                tags,
                format: source.format,
                every: source.every,
                book: (!source.book.is_empty()).then_some(source.book),
                enabled: source.enabled,
                has_token,
                tested: None,
                testing: false,
            });
        }
        cx.notify();
    }

    /// The form, read into a source.
    fn source_from_form(form: &SourceForm, cx: &Context<Self>) -> Source {
        Source {
            id: form.id,
            name: form.name.read(cx).text().trim().to_string(),
            url: form.url.read(cx).text().trim().to_string(),
            book: form.book.clone().unwrap_or_default(),
            format: form.format,
            tags: form.tags.read(cx).text().split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
            every: form.every,
            enabled: form.enabled,
            ..Default::default()
        }
    }

    fn save_source(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::Sources(sheet)) = self.sheet.as_mut() else { return };
        let Some(form) = sheet.form.as_ref() else { return };
        let mut source = Self::source_from_form(form, cx);
        if source.name.is_empty() {
            source.name = source.url.clone();
        }
        let token = form.token.read(cx).text().to_string();
        let keep_token = token.trim().is_empty() && form.has_token;
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let book = global.book.clone();
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let id = book.save_source(&source)?;
                    if !keep_token {
                        crate::client_sync::store_token(id, &token)?;
                    }
                    anyhow::Ok(id)
                })
                .await;
            this.update(cx, |this, cx| {
                match saved {
                    Ok(_) => {
                        if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut() {
                            sheet.form = None;
                        }
                        this.status = "Source saved. It pulls on its schedule, or now with Pull.".into();
                        this.reload_sources(cx);
                    }
                    Err(e) => {
                        if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                            && let Some(form) = sheet.form.as_mut()
                        {
                            form.tested = Some(Err(format!("{e:#}")));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pulls the source in the form once and says what came back, keeping
    /// none of it.
    fn test_source(&mut self, cx: &mut Context<Self>) {
        let Some(Sheet::Sources(sheet)) = self.sheet.as_mut() else { return };
        let Some(form) = sheet.form.as_mut() else { return };
        let mut source = Self::source_from_form(form, cx);
        // A test asks for everything, so `{since}` cannot make an empty
        // answer look like a broken one.
        source.last_success = None;
        let token = form.token.read(cx).text().trim().to_string();
        form.testing = true;
        form.tested = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            // A token typed but not saved yet is tried under a scratch id.
            // Off the UI thread, since the keychain is a process to run.
            let scratch = (!token.is_empty()).then(|| -(now_seconds()));
            if let Some(scratch) = scratch {
                let token = token.clone();
                let _ = smol::unblock(move || crate::client_sync::store_token(scratch, &token)).await;
                source.id = scratch;
            }
            let result = crate::client_sync::pull(&source, now_seconds()).await;
            if let Some(scratch) = scratch {
                let _ = smol::unblock(move || crate::client_sync::store_token(scratch, "")).await;
            }
            let said = match result {
                Ok(found) => {
                    let with_address = found.iter().filter(|c| c.email.contains('@')).count();
                    let examples: Vec<String> = found.iter().filter(|c| c.email.contains('@')).take(3).map(|c| c.email.clone()).collect();
                    Ok(format!("Found {} contact{}, {with_address} with an address: {}", found.len(), if found.len() == 1 { "" } else { "s" }, examples.join(", ")))
                }
                Err(e) => Err(format!("{e:#}")),
            };
            this.update(cx, |this, cx| {
                if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                    && let Some(form) = sheet.form.as_mut()
                {
                    form.testing = false;
                    form.tested = Some(said);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn render_sheet(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let body = match self.sheet.as_ref()? {
            Sheet::NewClient(_) => self.render_new_client(theme, window, cx),
            Sheet::Import(_) => self.render_import(theme, cx),
            Sheet::Sources(_) => self.render_sources(theme, window, cx),
            Sheet::Stages(_) => self.render_stages(theme, window, cx),
            Sheet::Mailing => self.render_mailing(theme, cx),
        };
        Some(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.scrim)
                .occlude()
                .child(
                    div()
                        .id("sheet")
                        .w(px(620.))
                        .max_h(px(620.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_5()
                        .rounded_lg()
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(body),
                )
                .into_any_element(),
        )
    }

    fn render_new_client(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(Sheet::NewClient(form)) = self.sheet.as_ref() else { return div().into_any_element() };
        let kind = form.kind;
        let book = form.book.clone();
        let accounts = self.accounts.clone();

        div()
            .key_context("ClientField")
            .on_action(cx.listener(|this, _: &Confirm, _, cx| this.create_client(cx)))
            .flex()
            .flex_col()
            .gap_3()
            .child(title("New client", "Somebody to keep notes on, tag, or put on the board.", theme))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(chip("new-person", "Person", kind == Kind::Person, theme).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(Sheet::NewClient(form)) = this.sheet.as_mut() {
                            form.kind = Kind::Person;
                        }
                        cx.notify();
                    })))
                    .child(chip("new-company", "Company", kind == Kind::Company, theme).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(Sheet::NewClient(form)) = this.sheet.as_mut() {
                            form.kind = Kind::Company;
                        }
                        cx.notify();
                    }))),
            )
            .child(labelled("Name", &form.name, theme, window, cx))
            .child(labelled(
                match kind {
                    Kind::Person => "Email",
                    Kind::Company => "Domain \u{2014} everyone at it is this company",
                },
                &form.email,
                theme,
                window,
                cx,
            ))
            .children((!accounts.is_empty()).then(|| {
                let mut chips = div().flex().flex_wrap().gap_1();
                for account in accounts {
                    let on = book.as_deref() == Some(account.email.as_str());
                    let email = account.email.clone();
                    chips = chips.child(chip(("new-book", account.id.0 as usize), account.email.clone(), on, theme).on_click(
                        cx.listener(move |this, _, _, cx| {
                            if let Some(Sheet::NewClient(form)) = this.sheet.as_mut() {
                                form.book = match on {
                                    true => None,
                                    false => Some(email.clone()),
                                };
                            }
                            cx.notify();
                        }),
                    ));
                }
                div().flex().flex_col().gap_1().child(caption("Book", theme)).child(chips)
            }))
            .child(buttons(
                chip("new-cancel", "Cancel", false, theme).on_click(cx.listener(|this, _, _, cx| {
                    this.sheet = None;
                    cx.notify();
                })),
                chip("new-create", "Add client  \u{21a9}", true, theme).on_click(cx.listener(|this, _, _, cx| this.create_client(cx))),
            ))
            .into_any_element()
    }

    fn render_import(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(Sheet::Import(sheet)) = self.sheet.as_ref() else { return div().into_any_element() };
        let found = sheet.contacts();
        let with_address = found.iter().filter(|c| c.email.contains('@')).count();
        let importing = sheet.importing;
        let book = sheet.book.clone();
        let accounts = self.accounts.clone();

        let mut mapping = div().flex().flex_col().gap_1();
        match (&sheet.table, &sheet.cards) {
            (Some(table), None) => {
                mapping = mapping.child(caption("Columns \u{2014} click what a column was taken to be, to change it", theme));
                for (i, header) in table.headers.iter().enumerate() {
                    let column = sheet.columns.get(i).copied().unwrap_or(Column::Skip);
                    let samples = table.samples(i, 3).join(" \u{00b7} ");
                    mapping = mapping.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .py_0p5()
                            .child(
                                div()
                                    .w(px(150.))
                                    .flex_shrink_0()
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(if header.is_empty() { format!("Column {}", i + 1) } else { header.clone() }),
                            )
                            .child(
                                chip(("map", i), column.label(), column != Column::Skip && column != Column::Field, theme).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        if let Some(Sheet::Import(sheet)) = this.sheet.as_mut()
                                            && let Some(slot) = sheet.columns.get_mut(i)
                                        {
                                            let at = Column::ALL.iter().position(|c| c == slot).unwrap_or(0);
                                            *slot = Column::ALL[(at + 1) % Column::ALL.len()];
                                        }
                                        cx.notify();
                                    }),
                                ),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .child(samples),
                            ),
                    );
                }
            }
            _ => {
                let examples: Vec<String> = found.iter().take(3).map(|c| format!("{} <{}>", c.name, c.email)).collect();
                mapping = mapping
                    .child(caption("Cards", theme))
                    .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(examples.join(" \u{00b7} ")));
            }
        }

        let mut books = div().flex().flex_wrap().gap_1();
        for account in accounts {
            let on = book.as_deref() == Some(account.email.as_str());
            let email = account.email.clone();
            books = books.child(chip(("import-book", account.id.0 as usize), account.email.clone(), on, theme).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(Sheet::Import(sheet)) = this.sheet.as_mut() {
                        sheet.book = match on {
                            true => None,
                            false => Some(email.clone()),
                        };
                    }
                    cx.notify();
                },
            )));
        }

        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(title(
                "Import contacts",
                "People already in the book are matched by address and updated; a name you typed is never overwritten.",
                theme,
            ))
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .child(format!("{}: {} rows, {with_address} with an address", sheet.file, found.len())),
            )
            .child(mapping)
            .child(div().flex().flex_col().gap_1().child(caption("Into the book of", theme)).child(books))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(caption("Tags", theme))
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(sheet.tags.clone()),
                    ),
            )
            .child(buttons(
                chip("import-cancel", "Cancel", false, theme).on_click(cx.listener(|this, _, _, cx| {
                    this.sheet = None;
                    cx.notify();
                })),
                chip("import-go", if importing { "Importing\u{2026}".to_string() } else { format!("Import {with_address}") }, with_address > 0, theme)
                    .on_click(cx.listener(|this, _, _, cx| this.run_import(cx))),
            ))
            .into_any_element()
    }

    fn render_sources(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(Sheet::Sources(sheet)) = self.sheet.as_ref() else { return div().into_any_element() };
        let pulling = cx.try_global::<ClientBook>().map(|g| g.pulling.clone()).unwrap_or_default();
        let now = now_seconds();

        let mut column = div().flex().flex_col().gap_3().child(title(
            "Sources",
            "An address that lists your signups as JSON or CSV, pulled on a schedule. New people land in the book you choose, tagged.",
            theme,
        ));

        if sheet.form.is_none() {
            if sheet.sources.is_empty() {
                column = column.child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child(
                            "No sources yet. Any endpoint that answers with a list of people works \u{2014} an admin route that returns \
                             your users as JSON, or a sheet published as CSV. Put {since} in the address and only people new since the \
                             last pull are asked for.",
                        ),
                );
            }
            for source in sheet.sources.clone() {
                let id = source.id;
                let is_pulling = pulling.contains(&id);
                let when = source.last_run.map(|t| format!("{} ago", clients::ago(now - t))).unwrap_or_else(|| "never pulled".into());
                let schedule = SCHEDULES.iter().find(|(s, _)| *s == source.every).map(|(_, l)| *l).unwrap_or("Custom");
                let edit = source.clone();
                let pull = source.clone();
                column = column.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().font_weight(FontWeight::MEDIUM).child(source.name.clone()))
                                .when(!source.enabled, |s| s.child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child("Off")))
                                .child(
                                    chip(("pull", id as usize), if is_pulling { "Pulling\u{2026}" } else { "Pull now" }, false, theme)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            clients::pull(pull.clone(), cx);
                                            this.status = format!("Pulling {}\u{2026}", pull.name).into();
                                            cx.notify();
                                        })),
                                )
                                .child(chip(("edit-source", id as usize), "Edit", false, theme).on_click(cx.listener(
                                    move |this, _, window, cx| this.edit_source(Some(edit.clone()), window, cx),
                                )))
                                .child(link(("delete-source", id as usize), "Delete", theme).on_click(cx.listener(move |this, _, _, cx| {
                                    this.run("Source deleted. The people it brought in stay.", cx, move |book, _| {
                                        book.delete_source(id)?;
                                        let _ = crate::client_sync::store_token(id, "");
                                        Ok(None)
                                    });
                                    this.reload_sources(cx);
                                }))),
                        )
                        .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).overflow_hidden().text_ellipsis().line_clamp(1).child(source.url.clone()))
                        .child(
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(theme.text_muted)
                                .child(format!(
                                    "{schedule} \u{00b7} into {} \u{00b7} {when}{}",
                                    if source.book.is_empty() { "no book" } else { source.book.as_str() },
                                    if source.last_note.is_empty() { String::new() } else { format!(" \u{2014} {}", source.last_note) },
                                )),
                        ),
                );
            }
            column = column.child(buttons(
                chip("sources-close", "Done", false, theme).on_click(cx.listener(|this, _, _, cx| {
                    this.sheet = None;
                    cx.notify();
                })),
                chip("sources-add", "Add a source", true, theme).on_click(cx.listener(|this, _, window, cx| this.edit_source(None, window, cx))),
            ));
            return column.into_any_element();
        }

        let form = sheet.form.as_ref().expect("checked above");
        let format = form.format;
        let every = form.every;
        let enabled = form.enabled;
        let book = form.book.clone();
        let testing = form.testing;
        let tested = form.tested.clone();
        let accounts = self.accounts.clone();

        let mut formats = div().flex().gap_1();
        for (option, label) in [(SourceFormat::Json, "JSON"), (SourceFormat::Csv, "CSV")] {
            formats = formats.child(chip(("format", option as usize), label, format == option, theme).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                        && let Some(form) = sheet.form.as_mut()
                    {
                        form.format = option;
                    }
                    cx.notify();
                },
            )));
        }
        let mut schedules = div().flex().flex_wrap().gap_1();
        for (seconds, label) in SCHEDULES {
            schedules = schedules.child(chip(("every", seconds as usize), label, every == seconds, theme).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                        && let Some(form) = sheet.form.as_mut()
                    {
                        form.every = seconds;
                    }
                    cx.notify();
                },
            )));
        }
        let mut books = div().flex().flex_wrap().gap_1();
        for account in accounts {
            let on = book.as_deref() == Some(account.email.as_str());
            let email = account.email.clone();
            books = books.child(chip(("source-book", account.id.0 as usize), account.email.clone(), on, theme).on_click(cx.listener(
                move |this, _, _, cx| {
                    if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                        && let Some(form) = sheet.form.as_mut()
                    {
                        form.book = Some(email.clone());
                    }
                    cx.notify();
                },
            )));
        }
        let token_hint = match form.has_token {
            true => "A token is saved in your keychain. Leave this empty to keep it.",
            false => "Kept in your keychain, never in the book.",
        };

        column
            .child(
                div()
                    .key_context("ClientField")
                    .on_action(cx.listener(|this, _: &Confirm, _, cx| this.save_source(cx)))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(labelled("Name", &form.name, theme, window, cx))
                    .child(labelled("Address", &form.url, theme, window, cx))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(labelled("Token", &form.token, theme, window, cx))
                            .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(token_hint)),
                    )
                    .child(div().flex().gap_6().child(div().flex().flex_col().gap_1().child(caption("Answers in", theme)).child(formats)).child(
                        div().flex().flex_col().gap_1().child(caption("Pull", theme)).child(schedules),
                    ))
                    .child(div().flex().flex_col().gap_1().child(caption("Into the book of", theme)).child(books))
                    .child(labelled("Tags for everyone it brings in", &form.tags, theme, window, cx))
                    .child(
                        div().flex().items_center().gap_2().child(crate::settings_ui::switch("source-enabled", theme, enabled).on_click(cx.listener(
                            |this, _, _, cx| {
                                if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut()
                                    && let Some(form) = sheet.form.as_mut()
                                {
                                    form.enabled = !form.enabled;
                                }
                                cx.notify();
                            },
                        )))
                        .child(div().text_size(theme.small_text_size).text_color(theme.text_muted).child("Pull on the schedule")),
                    ),
            )
            .children(tested.map(|said| {
                let (color, text) = match said {
                    Ok(text) => (theme.green, text),
                    Err(text) => (theme.red, text),
                };
                div().px_2p5().py_1p5().rounded_md().bg(color.opacity(0.12)).text_color(color).text_size(theme.small_text_size).child(text)
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(chip("source-test", if testing { "Testing\u{2026}" } else { "Test" }, false, theme).on_click(cx.listener(
                        |this, _, _, cx| this.test_source(cx),
                    )))
                    .child(div().flex_1())
                    .child(chip("source-cancel", "Cancel", false, theme).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(Sheet::Sources(sheet)) = this.sheet.as_mut() {
                            sheet.form = None;
                        }
                        cx.notify();
                    })))
                    .child(chip("source-save", "Save  \u{21a9}", true, theme).on_click(cx.listener(|this, _, _, cx| this.save_source(cx)))),
            )
            .into_any_element()
    }
}

/// Reads a file the person chose into a table or a list of cards.
fn read_contacts_file(path: PathBuf) -> anyhow::Result<(String, Option<Table>, Option<Vec<Contact>>)> {
    let bytes = std::fs::read(&path).map_err(|e| anyhow::anyhow!("Could not read {}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    match contacts::sniff(&file, &text) {
        contacts::Format::VCard => {
            let cards = contacts::parse_vcard(&text);
            if cards.is_empty() {
                anyhow::bail!("{file} has no contact cards in it");
            }
            Ok((file, None, Some(cards)))
        }
        contacts::Format::Json => {
            let table = contacts::parse_json(&text).map_err(|e| anyhow::anyhow!("{file}: {e}"))?;
            Ok((file, Some(table), None))
        }
        contacts::Format::Csv => {
            let table = contacts::parse_csv(&text);
            if table.headers.is_empty() {
                anyhow::bail!("{file} is empty");
            }
            Ok((file, Some(table), None))
        }
    }
}

fn title(heading: &'static str, description: &'static str, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(heading))
        .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(description))
}

fn caption(text: impl Into<SharedString>, theme: &Theme) -> Div {
    div().text_size(theme.small_text_size).font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(text.into())
}

fn labelled(label: &'static str, input: &Entity<TextInput>, theme: &Theme, window: &Window, cx: &Context<ClientsWindow>) -> Div {
    let focused = input.read(cx).focus_handle.is_focused(window);
    div().flex().flex_col().gap_1().child(caption(label, theme)).child(
        div()
            .px_2()
            .py_1p5()
            .rounded_md()
            .bg(theme.background)
            .border_1()
            .border_color(if focused { theme.focus_ring } else { theme.border })
            .child(input.clone()),
    )
}

fn buttons(cancel: impl IntoElement, confirm: impl IntoElement) -> Div {
    div().flex().justify_end().gap_2().pt_1().child(cancel).child(confirm)
}
