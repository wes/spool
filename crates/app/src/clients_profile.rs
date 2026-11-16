//! One client: who they are, where they stand, and everything you have had
//! with them.
//!
//! The header and the stage are always on screen, because they are what you
//! came to check or change. Below them, three tabs, rather than one long page:
//! the history is the thing most often read and it wants the height, and a
//! client with forty notes and twelve addresses would otherwise push it off
//! the bottom of the window.

use std::collections::HashMap;

use gpui::{AnyElement, Context, Div, Entity, FontWeight, SharedString, Window, div, prelude::*, px, uniform_list};
use spool_core::{ThreadId, format_date, format_date_long};
use spool_store::{
    book::{Edit, Kind},
    people::Direction,
};

use crate::{
    actions::Confirm,
    chrome::Tip,
    clients::{self, Entry, Signal},
    clients_window::{ClientsWindow, EditForm, Item, Peek, Tab, Undo, View, avatar, chip, pill, stage_color},
    clock::{local_offset, now_seconds},
    text_input::TextInput,
    theme::Theme,
};

/// Height of one line of history. Fixed, so the history is virtualized like
/// the list: a client of ten years is as quick to open as one of a week.
const TIMELINE_ROW: f32 = 56.;

/// The profile's width beside the board, which needs the rest.
const BOARD_PROFILE_WIDTH: f32 = 420.;

/// The date and account at the right of a line of history.
const DATE_COLUMN: f32 = 118.;

impl ClientsWindow {
    pub(crate) fn render_profile(&mut self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pane = div()
            .flex()
            .flex_col()
            .h_full()
            .min_w_0()
            .bg(theme.background)
            .when(self.view == View::Board, |s| {
                s.w(px(BOARD_PROFILE_WIDTH)).flex_shrink_0().border_l_1().border_color(theme.border)
            })
            .when(self.view == View::Directory, |s| s.flex_1());

        let Some(entry) = self.selected_entry().cloned() else {
            return pane
                .items_center()
                .justify_center()
                .gap_1()
                .child(div().text_color(theme.text_muted).font_weight(FontWeight::MEDIUM).child("Nobody chosen"))
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child("Choose somebody to see everything you have had with them."),
                )
                .into_any_element();
        };

        if self.peek.is_some() {
            return pane.child(self.render_peek(theme, window, cx)).into_any_element();
        }

        pane.child(self.render_header(&entry, theme, window, cx))
            .child(self.render_stage_row(&entry, theme, cx))
            .child(self.render_tag_row(&entry, theme, window, cx))
            .child(self.render_tabs(&entry, theme, cx))
            .child(match self.tab {
                Tab::History => self.render_history(&entry, theme, window, cx),
                Tab::Notes => self.render_notes(&entry, theme, window, cx),
                Tab::Details => self.render_details(&entry, theme, window, cx),
            })
            .into_any_element()
    }

    fn render_header(&mut self, entry: &Entry, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = now_seconds();
        let stats = entry.stats;
        let open = entry
            .stage()
            .is_some_and(|s| clients::open_stages(&self.snapshot).contains(&s));
        let since = entry.client.as_ref().and_then(|c| c.stage_since);
        let signal = clients::signal(&stats, open, since, entry.is_client(), now);

        let mut facts: Vec<String> = Vec::new();
        if stats.messages > 0 {
            facts.push(format!(
                "{} message{} \u{00b7} {} from them \u{00b7} {} from you",
                stats.messages,
                if stats.messages == 1 { "" } else { "s" },
                stats.received,
                stats.sent
            ));
        }
        if let Some(first) = stats.first {
            facts.push(format!("since {}", month_year(first)));
        }
        if let Some(value) = entry.client.as_ref().and_then(|c| c.value) {
            facts.push(clients::money(value));
        }

        let subtitle: SharedString = match (entry.company.is_empty(), entry.detail.starts_with('@')) {
            (false, false) => format!("{} \u{00b7} {}", entry.detail, entry.company).into(),
            _ => entry.detail.clone(),
        };

        let banner = match signal {
            Signal::NeedsReply(at) => Some((theme.yellow, format!("Waiting on you for {} \u{2014} their message came last", clients::ago(now - at)))),
            Signal::GoneQuiet(at) => Some((theme.red, format!("Gone quiet: nothing either way for {}", clients::ago(now - at)))),
            Signal::Nothing => None,
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_4()
            .pt_4()
            .pb_3()
            .flex_shrink_0()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(avatar(&entry.name, &entry.detail, 44., theme))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(18.))
                                            .line_height(px(24.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .overflow_hidden()
                                            .line_clamp(1)
                                            .text_ellipsis()
                                            .child(entry.name.clone()),
                                    )
                                    .when(entry.client.as_ref().is_some_and(|c| c.kind == Kind::Company), |s| {
                                        s.child(pill("Company", theme.text_muted, theme))
                                    })
                                    .when(!entry.is_client(), |s| s.child(pill("In your mail", theme.text_faint, theme))),
                            )
                            .child(
                                div()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_muted)
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .child(subtitle),
                            )
                            .when(!facts.is_empty(), |s| {
                                s.child(
                                    div()
                                        .text_size(theme.small_text_size)
                                        .text_color(theme.text_faint)
                                        .child(facts.join(" \u{00b7} ")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .flex_shrink_0()
                            .child(
                                crate::clients_window::tool("profile-write", "Write", "A new message to them", theme)
                                    .on_click(cx.listener(|this, _, _, cx| this.write_to(cx))),
                            )
                            .child(
                                crate::clients_window::tool("profile-edit", "Edit", "Name, company and the deal", theme)
                                    .on_click(cx.listener(|this, _, window, cx| this.start_edit(window, cx))),
                            )
                            // Beside the board it is a card over the columns,
                            // and closes; a click on the board does too.
                            .when(self.view == View::Board, |s| {
                                s.child(
                                    crate::clients_window::tool("profile-close", "\u{00d7}", "Close \u{2014} or click the board", theme)
                                        .on_click(cx.listener(|this, _, _, cx| this.close_card(cx))),
                                )
                            }),
                    ),
            )
            .children(self.edit.as_ref().map(|form| self.render_edit(form, theme, window, cx)))
            .children(banner.map(|(color, text)| {
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2p5()
                    .py_1p5()
                    .rounded_md()
                    .bg(color.opacity(0.12))
                    .text_size(theme.small_text_size)
                    .text_color(color)
                    .child(text)
            }))
    }

    fn render_edit(&self, form: &EditForm, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = form.kind;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(theme.elevated)
            .border_1()
            .border_color(theme.border)
            .key_context("ClientField")
            .on_action(cx.listener(|this, _: &Confirm, _, cx| this.save_edit(cx)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(field("Name", &form.name, theme, window, cx))
                    .child(field("Company", &form.company, theme, window, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap_2()
                    .child(field("Deal value", &form.value, theme, window, cx))
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .pb_1()
                            .child(chip("kind-person", "Person", kind == Kind::Person, theme).on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = this.edit.as_mut() {
                                    form.kind = Kind::Person;
                                }
                                cx.notify();
                            })))
                            .child(chip("kind-company", "Company", kind == Kind::Company, theme).on_click(cx.listener(|this, _, _, cx| {
                                if let Some(form) = this.edit.as_mut() {
                                    form.kind = Kind::Company;
                                }
                                cx.notify();
                            }))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap_2()
                    .child(chip("edit-cancel", "Cancel", false, theme).on_click(cx.listener(|this, _, _, cx| {
                        this.edit = None;
                        cx.notify();
                    })))
                    .child(chip("edit-save", "Save  \u{21a9}", true, theme).on_click(cx.listener(|this, _, _, cx| this.save_edit(cx)))),
            )
    }

    fn start_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else { return };
        let client = entry.client.as_ref();
        let name_text = client.map(|c| c.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| entry.name.to_string());
        let company_text = client.map(|c| c.company.clone()).unwrap_or_default();
        let value_text = client.and_then(|c| c.value).map(clients::money).unwrap_or_default();
        let name = cx.new(|cx| {
            let mut input = TextInput::new("Name", cx);
            input.set_content(name_text, cx);
            input
        });
        let company = cx.new(|cx| {
            let mut input = TextInput::new("Where they work", cx);
            input.set_content(company_text, cx);
            input
        });
        let value = cx.new(|cx| {
            let mut input = TextInput::new("$18,000 or 18k", cx);
            input.set_content(value_text, cx);
            input
        });
        let focus = name.read(cx).focus_handle.clone();
        window.focus(&focus);
        self.edit = Some(EditForm { name, company, value, kind: client.map(|c| c.kind).unwrap_or_default() });
        cx.notify();
    }

    fn save_edit(&mut self, cx: &mut Context<Self>) {
        let (Some(form), Some(key)) = (self.edit.take(), self.selected.clone()) else { return };
        let value_text = form.value.read(cx).text().trim().to_string();
        let value = match value_text.is_empty() {
            true => Some(None),
            false => match clients::parse_money(&value_text) {
                Some(value) => Some(Some(value)),
                None => {
                    self.status = format!("\u{201c}{value_text}\u{201d} is not an amount").into();
                    self.edit = Some(form);
                    cx.notify();
                    return;
                }
            },
        };
        let edit = Edit {
            name: Some(form.name.read(cx).text().to_string()),
            company: Some(form.company.read(cx).text().to_string()),
            kind: Some(form.kind),
            value,
        };
        self.run_on_client(key, "Saved", cx, move |book, client, now| {
            book.edit(client, &edit, now)?;
            Ok(None)
        });
        cx.notify();
    }

    /// What takes the profile's place while several people are picked:
    /// who, and what can be done to all of them at once.
    pub(crate) fn render_picked(&mut self, theme: &Theme, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let picked: Vec<Entry> = self.picked.iter().filter_map(|key| self.entry(key).cloned()).collect();
        let count = picked.len();

        // One set of stages to offer: the book being looked at, or the one
        // they all share. People from books with different stages get the
        // shared ones, and are told so.
        let books: Vec<String> = picked.iter().map(|e| self.pipeline_for(Some(e))).collect();
        let mixed = books.windows(2).any(|w| w[0] != w[1]);
        let book = match (&self.book_filter, mixed) {
            (Some(filter), _) => clients::pipeline_key(&self.snapshot, &filter.email).to_string(),
            (None, false) => books.first().cloned().unwrap_or_default(),
            (None, true) => String::new(),
        };
        let stages: Vec<_> = clients::pipeline(&self.snapshot, &book).into_iter().cloned().collect();
        // A stage they are all in shows as chosen; any other shows how many
        // of them are in it already.
        let in_stage = |id: Option<i64>| picked.iter().filter(|e| e.stage() == id).count();

        let names: String = {
            let first: Vec<&str> = picked.iter().take(4).map(|e| e.name.as_ref()).collect();
            match count.saturating_sub(first.len()) {
                0 => first.join(", "),
                more => format!("{} and {more} more", first.join(", ")),
            }
        };

        let mut row = div().flex().flex_wrap().items_center().gap_1();
        let off = in_stage(None);
        row = row.child(
            stage_chip("picked-off", count_label("Off board", off, count), off == count, theme.text_faint, theme)
                .on_click(cx.listener(move |this, _, _, cx| this.set_picked_stage(None, cx))),
        );
        for (index, stage) in stages.into_iter().enumerate() {
            let here = in_stage(Some(stage.id));
            let color = stage_color(stage.kind, index, theme);
            let id = stage.id;
            row = row.child(
                stage_chip(("picked-stage", id as usize), count_label(&stage.name, here, count), here == count, color, theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.set_picked_stage(Some(id), cx))),
            );
        }

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .min_w_0()
            .gap_4()
            .p_5()
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().text_size(px(18.)).font_weight(FontWeight::SEMIBOLD).child(format!("{count} picked")))
                    .child(div().flex_1())
                    .child(chip("picked-email", "Email\u{2026}", false, theme).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let picked: Vec<Entry> = this.picked.iter().filter_map(|key| this.entry(key).cloned()).collect();
                            this.open_mail(format!("{} people", picked.len()), picked, cx);
                        },
                    )))
                    .child(chip("picked-clear", "Clear", false, theme).on_click(cx.listener(|this, _, _, cx| {
                        this.picked.clear();
                        cx.notify();
                    }))),
            )
            .child(div().text_color(theme.text_muted).line_clamp(2).child(names))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(match (mixed && self.book_filter.is_none(), book.is_empty()) {
                                (true, _) => "Move them all to \u{2014} they are in books with different stages, so these are the shared ones".to_string(),
                                (false, true) => "Move them all to".to_string(),
                                (false, false) => format!("Move them all to \u{2014} {book}'s stages"),
                            }),
                    )
                    .child(row),
            )
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child("\u{2318}-click adds or takes out one; \u{21e7}-click or \u{21e7}\u{2191}\u{2193} picks a run of them. Escape lets go. Undo puts them all back."),
            )
            .into_any_element()
    }

    fn render_stage_row(&mut self, entry: &Entry, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let current = entry.stage();
        let key = entry.key.clone();
        let since = entry.client.as_ref().and_then(|c| c.stage_since);
        let now = now_seconds();

        let mut row = div()
            .id("stage-row")
            .flex()
            .items_center()
            .gap_1()
            .px_4()
            .pb_2()
            .flex_shrink_0()
            .overflow_x_scroll()
            .child(
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child("Stage"),
            );

        let off = current.is_none();
        let key_off = key.clone();
        row = row.child(
            stage_chip("stage-off", "Off board".into(), off, theme.text_faint, theme)
                .tooltip(|_, cx| cx.new(|_| Tip { text: "Not in the sale. Choosing a stage puts them on the board.".into() }).into())
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !off {
                        this.set_stage(key_off.clone(), None, None, cx);
                    }
                })),
        );
        // Their book's stages: a sign-up on a board of its own moves through
        // that board's columns, not the shared ones.
        let book = self.pipeline_for(Some(entry));
        let stages: Vec<_> = clients::pipeline(&self.snapshot, &book).into_iter().cloned().collect();
        for (index, stage) in stages.into_iter().enumerate() {
            let on = current == Some(stage.id);
            let color = stage_color(stage.kind, index, theme);
            let key = key.clone();
            let label: SharedString = match (on, since) {
                (true, Some(since)) if now - since >= 86_400 => format!("{} \u{00b7} {}", stage.name, clients::ago(now - since)).into(),
                _ => stage.name.clone().into(),
            };
            row = row.child(stage_chip(("stage", stage.id as usize), label, on, color, theme).on_click(cx.listener(
                move |this, _, _, cx| {
                    if !on {
                        this.set_stage(key.clone(), Some(stage.id), None, cx);
                    }
                },
            )));
        }
        row
    }

    fn render_tag_row(&mut self, entry: &Entry, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.tag_input.read(cx).focus_handle.is_focused(window);
        let tags: Vec<(i64, String, u8)> = entry
            .client
            .as_ref()
            .map(|c| {
                c.tags
                    .iter()
                    .filter_map(|t| self.snapshot.tags.iter().find(|tag| tag.id == *t))
                    .map(|t| (t.id, t.name.clone(), t.color))
                    .collect()
            })
            .unwrap_or_default();
        let client = entry.client.as_ref().map(|c| c.id);
        let typed = self.tag_input.read(cx).text().trim().to_lowercase();
        // Tags already in the book that start with what is typed, so the
        // same tag is not made twice under two spellings.
        let suggestions: Vec<String> = match typed.is_empty() {
            true => Vec::new(),
            false => self
                .snapshot
                .tags
                .iter()
                .filter(|t| t.name.to_lowercase().starts_with(typed.trim_start_matches('#')) && !tags.iter().any(|(id, _, _)| *id == t.id))
                .take(4)
                .map(|t| t.name.clone())
                .collect(),
        };

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_4()
            .pb_3()
            .flex_shrink_0()
            .child(
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child("Tags"),
            )
            .children(tags.into_iter().map(|(id, name, color)| {
                let color = theme.avatar(color as u32);
                div()
                    .id(("tag", id as usize))
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl_1p5()
                    .pr_1()
                    .rounded(px(4.))
                    .bg(color.opacity(0.16))
                    .text_color(color)
                    .text_size(theme.small_text_size)
                    .child(format!("#{name}"))
                    .child(
                        div()
                            .id(("untag", id as usize))
                            .cursor_pointer()
                            .opacity(0.6)
                            .hover(|s| s.opacity(1.))
                            .child("\u{2715}")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(client) = client {
                                    this.run("Tag removed", cx, move |book, now| {
                                        book.untag(client, id, now)?;
                                        Ok(None)
                                    });
                                }
                            })),
                    )
            }))
            .child(
                div()
                    .key_context("ClientField")
                    .on_action(cx.listener(|this, _: &Confirm, _, cx| {
                        let text = this.tag_input.read(cx).text().to_string();
                        this.tag_input.update(cx, |input, cx| input.clear(cx));
                        this.add_tag(text, cx);
                    }))
                    .w(px(120.))
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(if focused { theme.focus_ring } else { theme.border_subtle })
                    .text_size(theme.small_text_size)
                    .child(self.tag_input.clone()),
            )
            .children(suggestions.into_iter().enumerate().map(|(i, name)| {
                chip(("tag-suggest", i), format!("#{name}"), false, theme).on_click(cx.listener(move |this, _, _, cx| {
                    this.tag_input.update(cx, |input, cx| input.clear(cx));
                    this.add_tag(name.clone(), cx);
                }))
            }))
    }

    fn render_tabs(&mut self, entry: &Entry, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let profile = self.profile.as_ref().filter(|p| p.key == entry.key);
        let history = profile.map(|p| p.history.len());
        let notes = profile.map(|p| p.detail.notes.len()).unwrap_or(0);
        let tab = |this: &mut Self, id: &'static str, label: String, which: Tab, cx: &mut Context<Self>| {
            let on = this.tab == which;
            div()
                .id(id)
                .px_1()
                .pb_1p5()
                .cursor_pointer()
                .border_b_2()
                .when(on, |s| s.border_color(theme.accent).text_color(theme.text))
                .when(!on, |s| s.border_color(gpui::transparent_black()).text_color(theme.text_muted).hover(|s| s.text_color(theme.text)))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = which;
                    cx.notify();
                }))
        };
        let history_label = match history {
            Some(n) if n >= crate::clients_window::HISTORY_LIMIT => format!("History {n}+"),
            Some(n) => format!("History {n}"),
            None => "History".into(),
        };
        div()
            .flex()
            .gap_4()
            .px_4()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border)
            .text_size(theme.small_text_size)
            .child(tab(self, "tab-history", history_label, Tab::History, cx))
            .child(tab(self, "tab-notes", if notes > 0 { format!("Notes {notes}") } else { "Notes".into() }, Tab::Notes, cx))
            .child(tab(self, "tab-details", "Details".into(), Tab::Details, cx))
    }

    fn render_history(&mut self, entry: &Entry, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(profile) = self.profile.as_ref().filter(|p| p.key == entry.key) else {
            return placeholder("Loading the history\u{2026}", theme).into_any_element();
        };
        if profile.timeline.is_empty() {
            let text = match entry.addrs.is_empty() && entry.domains.is_empty() {
                true => "No address yet, so no mail. Add one under Details.",
                false => "No mail with them yet.",
            };
            return placeholder(text, theme).into_any_element();
        }

        let pinned: Vec<String> = profile.detail.notes.iter().filter(|n| n.pinned).map(|n| n.body.clone()).collect();
        let mut threads: HashMap<ThreadId, usize> = HashMap::new();
        for exchange in &profile.history {
            *threads.entry(exchange.envelope.thread_id).or_default() += 1;
        }
        let threads = std::sync::Arc::new(threads);
        let count = profile.timeline.len();
        let row_theme = theme.clone();
        // The rows' width, worked out rather than flexed, for the reason the
        // directory's is: text is cut with an ellipsis only inside a width
        // that is known before it is measured.
        let width = match self.view {
            View::Directory => {
                (window.viewport_size().width / px(1.)) - crate::clients_window::SIDEBAR_WIDTH - crate::clients_window::LIST_WIDTH - 1.
            }
            View::Board => BOARD_PROFILE_WIDTH - 1.,
        }
        .max(240.);

        let list = uniform_list(
            "client-history",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let now = now_seconds();
                let offset = local_offset(now);
                range
                    .map(|row| this.render_timeline_row(row, width, &threads, now, offset, &row_theme, cx))
                    .collect()
            }),
        )
        .track_scroll(self.timeline_scroll.clone())
        .flex_grow();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .children(pinned.into_iter().take(3).map(|body| {
                div()
                    .mx_4()
                    .mt_2()
                    .px_2p5()
                    .py_1p5()
                    .rounded_md()
                    .bg(theme.yellow.opacity(0.10))
                    .border_l_2()
                    .border_color(theme.yellow)
                    .text_size(theme.small_text_size)
                    .text_color(theme.text)
                    .child(first_lines(&body, 3))
            }))
            .child(list)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_timeline_row(
        &mut self,
        row: usize,
        width: f32,
        threads: &HashMap<ThreadId, usize>,
        now: i64,
        offset: i64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(profile) = self.profile.as_ref() else { return div().h(px(TIMELINE_ROW)).into_any_element() };
        let Some(item) = profile.timeline.get(row).copied() else {
            return div().h(px(TIMELINE_ROW)).into_any_element();
        };

        match item {
            Item::Event(index) => {
                let Some(event) = profile.detail.events.get(index) else { return div().into_any_element() };
                let text: SharedString = match event.kind.as_str() {
                    "created" => "Became a client".into(),
                    "imported" => format!("Imported from {}", event.detail).into(),
                    _ => event.detail.clone().into(),
                };
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .w(px(width))
                    .h(px(TIMELINE_ROW))
                    .px_4()
                    .border_b_1()
                    .border_color(theme.border_subtle)
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .child(div().w(px(18.)).text_color(theme.accent).child("\u{25C6}"))
                    .child(div().flex_1().child(text))
                    .child(div().text_color(theme.text_faint).child(format_date(event.ts, now, offset)))
                    .into_any_element()
            }
            Item::Mail(index) => {
                let Some(exchange) = profile.history.get(index).cloned() else { return div().into_any_element() };
                let to_open = exchange.clone();
                let envelope = &exchange.envelope;
                let (glyph, color, tip) = match exchange.direction {
                    Direction::Inbound => ("\u{2190}", theme.accent, "From them"),
                    Direction::Outbound => ("\u{2192}", theme.green, "From you"),
                    Direction::Copied => ("\u{21C4}", theme.text_faint, "They were copied"),
                };
                let in_thread = threads.get(&envelope.thread_id).copied().unwrap_or(1);
                let subject: SharedString = match envelope.subject.trim() {
                    "" => "(no subject)".into(),
                    subject => subject.to_string().into(),
                };
                let unread = !envelope.flags.is_seen();
                let who: SharedString = match exchange.direction {
                    Direction::Outbound => match exchange.to.first() {
                        Some(to) => format!("You \u{2192} {}", to.short()).into(),
                        None => "You".into(),
                    },
                    _ => envelope.from.short().to_string().into(),
                };
                let account = self.account_email(envelope.account_id).map(str::to_string);

                div()
                    .id(("history", row))
                    .flex()
                    .items_start()
                    .gap_2()
                    .w(px(width))
                    .h(px(TIMELINE_ROW))
                    .px_4()
                    .pt_2()
                    .cursor_pointer()
                    .border_b_1()
                    .border_color(theme.border_subtle)
                    .hover(|s| s.bg(theme.hover))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_peek(to_open.clone(), window, cx)))
                    .child(
                        div()
                            .id(("direction", row))
                            .w(px(18.))
                            .flex_shrink_0()
                            .text_color(color)
                            .tooltip(move |_, cx| cx.new(|_| Tip { text: tip.into() }).into())
                            .child(glyph),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            // The row less its padding, the arrow, the gaps
                            // and the date column: definite, so the snippet
                            // ends in an ellipsis rather than under the date.
                            .w(px((width - 32. - 18. - 16. - DATE_COLUMN).max(80.)))
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        div()
                                            .flex_shrink()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .line_clamp(1)
                                            .text_ellipsis()
                                            .when(unread, |s| s.font_weight(FontWeight::SEMIBOLD))
                                            .child(subject),
                                    )
                                    .when(in_thread > 1, |s| {
                                        s.child(
                                            div()
                                                .flex_shrink_0()
                                                .text_size(px(10.5))
                                                .text_color(theme.text_faint)
                                                .child(format!("{in_thread}")),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .overflow_hidden()
                                    .line_clamp(1)
                                    .text_ellipsis()
                                    .text_size(theme.small_text_size)
                                    .text_color(theme.text_faint)
                                    .child(format!("{who} \u{2014} {}", envelope.snippet.trim())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_end()
                            .w(px(DATE_COLUMN))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(format_date(envelope.date, now, offset))
                            .children(account.filter(|_| self.accounts.len() > 1).map(|a| {
                                div().text_size(px(10.)).child(a)
                            })),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_peek(&mut self, theme: &Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(peek) = self.peek.as_ref() else { return div().into_any_element() };
        let Peek { exchange, read, images } = peek;
        let (exchange, read, images) = (exchange.clone(), read.clone(), *images);
        let envelope = exchange.envelope.clone();
        let offset = local_offset(envelope.date);
        let to: Vec<String> = read
            .as_ref()
            .map(|r| r.body.to.iter().chain(&r.body.cc).map(|a| a.short().to_string()).collect())
            .unwrap_or_else(|| exchange.to.iter().map(|a| a.short().to_string()).collect());
        let copies = exchange.mailboxes.len();
        let reply_exchange = exchange.clone();
        let reply_all_exchange = exchange.clone();
        let reveal = envelope.clone();

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .h(px(40.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(chip("peek-back", "\u{2190} History", false, theme).on_click(cx.listener(|this, _, _, cx| {
                        this.peek = None;
                        cx.notify();
                    })))
                    .child(div().flex_1())
                    .child(chip("peek-reply", "Reply", false, theme).on_click(cx.listener(move |this, _, _, cx| {
                        this.reply(reply_exchange.clone(), false, cx)
                    })))
                    .child(chip("peek-reply-all", "Reply all", false, theme).on_click(cx.listener(move |this, _, _, cx| {
                        this.reply(reply_all_exchange.clone(), true, cx)
                    })))
                    .child(chip("peek-open", "Open in Mail", true, theme).on_click(cx.listener(move |this, _, _, cx| {
                        this.show_in_mail(reveal.clone(), cx)
                    }))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_4()
                    .py_3()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_size(px(16.))
                            .line_height(px(22.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(match envelope.subject.trim() {
                                "" => "(no subject)".to_string(),
                                subject => subject.to_string(),
                            }),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_muted)
                            .child(format!("{} \u{2192} {}", envelope.from.short(), to.join(", "))),
                    )
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(match copies {
                                1 => format_date_long(envelope.date, offset),
                                n => format!("{} \u{00b7} in {n} folders", format_date_long(envelope.date, offset)),
                            }),
                    ),
            )
            .child(self.render_peek_body(&envelope, read, images, theme, window, cx))
            .into_any_element()
    }

    /// The message itself, drawn by the reader's own means: HTML in its web
    /// view, on the white page it was written for; text as the reader sets
    /// it, in the theme's colours.
    fn render_peek_body(
        &mut self,
        envelope: &spool_core::Envelope,
        read: Option<std::sync::Arc<crate::clients_window::Read>>,
        images: bool,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let page = || {
            div()
                .id("peek-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px_4()
                .pt_3()
                .pb_6()
                .border_t_1()
                .border_color(theme.border_subtle)
        };
        let Some(read) = read else {
            return page().child(div().text_color(theme.text_faint).child("Loading\u{2026}")).into_any_element();
        };

        #[cfg(target_os = "macos")]
        if read.source == spool_core::html::Source::Rich {
            // Something drawn over the window — a sheet, a menu — would open
            // under a native view, so while there is one the page steps aside.
            if self.sheet.is_some() || self.sort_menu_open() {
                return div().flex_1().into_any_element();
            }
            if self.web_view.is_none() {
                self.web_view = crate::webview::WebView::new(window);
            }
            if let Some(web_view) = self.web_view.clone() {
                let remote = match images {
                    true => spool_core::sanitize::Remote::Allowed,
                    false => spool_core::sanitize::Remote::Blocked,
                };
                let key = (envelope.id.0 as u64) << 1 | images as u64;
                let loading = web_view.borrow_mut().show(key, || crate::message_view::web_document(&read.body, remote));
                // gpui draws on events, and a page finishing loading is not
                // one it hears about: ask for frames until it has.
                if loading {
                    let polled = web_view.clone();
                    cx.spawn(async move |this, cx| {
                        for _ in 0..128 {
                            cx.background_executor().timer(std::time::Duration::from_millis(16)).await;
                            let settled = polled.borrow_mut().settled();
                            if this.update(cx, |_, cx| cx.notify()).is_err() || settled {
                                break;
                            }
                        }
                    })
                    .detach();
                }
                self.web_view_drawn = true;
                return div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(theme.border_subtle)
                    .child(crate::webview::WebViewElement::new(web_view))
                    .into_any_element();
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = images;

        let key = ("peek-plain", envelope.id.0 as usize);
        page()
            .child(crate::message_view::plain_body_area(key, &read.blocks, theme, self.list_focus.clone(), window, cx))
            .into_any_element()
    }

    fn render_notes(&mut self, entry: &Entry, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let notes = self
            .profile
            .as_ref()
            .filter(|p| p.key == entry.key)
            .map(|p| p.detail.notes.clone())
            .unwrap_or_default();
        let focused = self.note_input.read(cx).focus_handle.is_focused(window);
        // In the theme's ink, as the group message is: see `area_style`.
        self.note_input.update(cx, |input, cx| input.set_style(crate::clients_mail::area_style(theme), cx));
        let editing = self.editing_note.is_some();
        let now = now_seconds();
        let offset = local_offset(now);

        div()
            .id("notes")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .gap_3()
            .child(
                div()
                    .key_context("ClientNote")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .min_h(px(64.))
                            .px_2p5()
                            .py_2()
                            .rounded_md()
                            .bg(theme.elevated)
                            .border_1()
                            .border_color(if focused { theme.focus_ring } else { theme.border })
                            .cursor_text()
                            .child(self.note_input.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .when(editing, |s| {
                                s.child(chip("note-cancel", "Cancel", false, theme).on_click(cx.listener(|this, _, _, cx| {
                                    this.editing_note = None;
                                    this.note_input.update(cx, |input, cx| input.set_text("", cx));
                                    cx.notify();
                                })))
                            })
                            .child(
                                chip("note-save", if editing { "Save note  \u{2318}\u{21a9}" } else { "Add note  \u{2318}\u{21a9}" }, true, theme)
                                    .on_click(cx.listener(|this, _, _, cx| this.save_note(cx))),
                            ),
                    ),
            )
            .when(notes.is_empty(), |s| {
                s.child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child("Nothing written down yet. What they want, who decides, when to follow up \u{2014} it lives here, not in your head."),
                )
            })
            .children(notes.into_iter().map(|note| {
                let id = note.id;
                let body = note.body.clone();
                let pinned = note.pinned;
                let deleted = note.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if pinned { theme.yellow.opacity(0.6) } else { theme.border })
                    .when(pinned, |s| s.bg(theme.yellow.opacity(0.06)))
                    .child(div().whitespace_normal().child(note.body.clone()))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(div().flex_1().child(format_date(note.created, now, offset)))
                            .child(link(("note-pin", id as usize), if pinned { "Unpin" } else { "Pin" }, theme).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.run(if pinned { "Unpinned" } else { "Pinned to the top of the history" }, cx, move |book, _| {
                                        book.pin_note(id, !pinned)?;
                                        Ok(None)
                                    })
                                },
                            )))
                            .child(link(("note-edit", id as usize), "Edit", theme).on_click(cx.listener(move |this, _, window, cx| {
                                this.editing_note = Some(id);
                                this.note_input.update(cx, |input, cx| input.set_text(body.clone(), cx));
                                let focus = this.note_input.read(cx).focus_handle.clone();
                                window.focus(&focus);
                                cx.notify();
                            })))
                            .child(link(("note-delete", id as usize), "Delete", theme).on_click(cx.listener(move |this, _, _, cx| {
                                let deleted = deleted.clone();
                                this.run("Note deleted \u{2014} \u{2318}Z to bring it back", cx, move |book, _| {
                                    book.delete_note(deleted.id)?;
                                    Ok(Some(Undo::Note(deleted)))
                                })
                            }))),
                    )
            }))
            .into_any_element()
    }

    fn render_details(&mut self, entry: &Entry, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let client = entry.client.clone();
        let fields = self
            .profile
            .as_ref()
            .filter(|p| p.key == entry.key)
            .map(|p| p.detail.fields.clone())
            .unwrap_or_default();
        let identity_focused = self.identity_input.read(cx).focus_handle.is_focused(window);
        let key_focused = self.field_key.read(cx).focus_handle.is_focused(window);
        let value_focused = self.field_value.read(cx).focus_handle.is_focused(window);

        let mut column = div()
            .id("details")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_4()
            .py_3()
            .gap_4();

        // Addresses and domains.
        let mut addresses = section("Addresses", theme);
        let identities: Vec<String> = match &client {
            Some(client) => client.addrs.iter().cloned().chain(client.domains.iter().map(|d| format!("@{d}"))).collect(),
            None => entry.addrs.clone(),
        };
        for (i, value) in identities.into_iter().enumerate() {
            let primary = i == 0;
            let removable = client.as_ref().map(|c| c.id).filter(|_| !primary || entry.addrs.len() + entry.domains.len() > 1);
            addresses = addresses.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().child(value.clone()))
                    .when(primary, |s| s.child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child("writes to")))
                    .children(removable.map(|client| {
                        let value = value.clone();
                        link(("remove-identity", i), "Remove", theme).on_click(cx.listener(move |this, _, _, cx| {
                            let value = value.clone();
                            this.run("Removed", cx, move |book, now| {
                                book.remove_identity(client, &value, now)?;
                                Ok(None)
                            })
                        }))
                    })),
            );
        }
        let key = entry.key.clone();
        addresses = addresses.child(
            div()
                .key_context("ClientField")
                .on_action(cx.listener(move |this, _: &Confirm, _, cx| {
                    let text = this.identity_input.read(cx).text().trim().to_string();
                    if text.is_empty() {
                        return;
                    }
                    this.identity_input.update(cx, |input, cx| input.clear(cx));
                    this.run_on_client(key.clone(), format!("Added {text}"), cx, move |book, client, now| {
                        book.add_identity(client, &text, now)?;
                        Ok(None)
                    });
                }))
                .px_2()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(if identity_focused { theme.focus_ring } else { theme.border })
                .child(self.identity_input.clone()),
        );
        column = column.child(addresses);

        // Books.
        if !self.accounts.is_empty() {
            let mut books = section("Books", theme).child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child("Which accounts they belong to. Mail with them in an account counts on its own."),
            );
            let mut chips = div().flex().flex_wrap().gap_1();
            for account in self.accounts.clone() {
                let email = spool_core::address::normalize(&account.email);
                let listed = client.as_ref().is_some_and(|c| c.books.contains(&email));
                let has_mail = entry.accounts.contains(&account.id);
                let key = entry.key.clone();
                let label = match (listed, has_mail) {
                    (_, true) if !listed => format!("{} \u{00b7} mail", account.email),
                    _ => account.email.clone(),
                };
                chips = chips.child(chip(("book", account.id.0 as usize), label, listed, theme).on_click(cx.listener(
                    move |this, _, _, cx| {
                        let email = email.clone();
                        this.run_on_client(key.clone(), if listed { "Removed from the book" } else { "Added to the book" }, cx, move |book, client, _| {
                            match listed {
                                true => book.remove_from_book(client, &email)?,
                                false => book.add_to_book(client, &email)?,
                            }
                            Ok(None)
                        })
                    },
                )));
            }
            books = books.child(chips);
            column = column.child(books);
        }

        // Fields.
        let mut fields_section = section("Fields", theme);
        for (i, (name, value)) in fields.into_iter().enumerate() {
            let client_id = client.as_ref().map(|c| c.id);
            let name_for_remove = name.clone();
            fields_section = fields_section.child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(div().w(px(110.)).flex_shrink_0().text_color(theme.text_faint).child(name))
                    .child(div().flex_1().min_w_0().child(value))
                    .children(client_id.map(|client| {
                        let name = name_for_remove.clone();
                        link(("remove-field", i), "Remove", theme).on_click(cx.listener(move |this, _, _, cx| {
                            let name = name.clone();
                            this.run("Removed", cx, move |book, now| {
                                book.set_field(client, &name, "", now)?;
                                Ok(None)
                            })
                        }))
                    })),
            );
        }
        let key = entry.key.clone();
        fields_section = fields_section.child(
            div()
                .key_context("ClientField")
                .on_action(cx.listener(move |this, _: &Confirm, window, cx| {
                    let name = this.field_key.read(cx).text().trim().to_string();
                    let value = this.field_value.read(cx).text().trim().to_string();
                    if name.is_empty() {
                        let focus = this.field_key.read(cx).focus_handle.clone();
                        window.focus(&focus);
                        return;
                    }
                    if value.is_empty() {
                        let focus = this.field_value.read(cx).focus_handle.clone();
                        window.focus(&focus);
                        return;
                    }
                    this.field_key.update(cx, |input, cx| input.clear(cx));
                    this.field_value.update(cx, |input, cx| input.clear(cx));
                    this.run_on_client(key.clone(), format!("Saved {name}"), cx, move |book, client, now| {
                        book.set_field(client, &name, &value, now)?;
                        Ok(None)
                    });
                }))
                .flex()
                .gap_2()
                .child(
                    div()
                        .w(px(110.))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(if key_focused { theme.focus_ring } else { theme.border })
                        .child(self.field_key.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .border_1()
                        .border_color(if value_focused { theme.focus_ring } else { theme.border })
                        .child(self.field_value.clone()),
                ),
        );
        column = column.child(fields_section);

        // What can be done to them as a whole.
        let mut actions = section("Manage", theme);
        let mut row = div().flex().flex_wrap().gap_2();
        match &client {
            Some(client) => {
                let id = client.id;
                let hidden = client.hidden;
                row = row
                    .child(chip("merge", "Merge another client in\u{2026}", false, theme).on_click(cx.listener(move |this, _, window, cx| {
                        this.merge_into = Some(id);
                        this.status = "Choose the client to fold into this one".into();
                        this.set_view(View::Directory, window, cx);
                    })))
                    .child(chip("hide", if hidden { "Show again" } else { "Hide" }, false, theme).on_click(cx.listener(move |this, _, _, cx| {
                        this.run(if hidden { "Shown again" } else { "Hidden" }, cx, move |book, now| {
                            book.set_hidden(id, !hidden, now)?;
                            Ok(None)
                        })
                    })))
                    .child(
                        chip("delete", if self.confirm_delete { "Press again to delete" } else { "Delete client" }, self.confirm_delete, theme)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.confirm_delete {
                                    this.confirm_delete = true;
                                    cx.notify();
                                    return;
                                }
                                this.confirm_delete = false;
                                this.selected = None;
                                this.profile = None;
                                this.run("Client deleted. Their mail is untouched.", cx, move |book, _| {
                                    book.delete(id)?;
                                    Ok(None)
                                })
                            })),
                    );
            }
            None => {
                let key = entry.key.clone();
                let addr = entry.addrs.first().cloned().unwrap_or_default();
                let hidden = entry.hidden;
                row = row
                    .child(chip("make-client", "Make a client", true, theme).on_click(cx.listener(move |this, _, _, cx| {
                        this.run_on_client(key.clone(), "Now a client", cx, |_, _, _| Ok(None))
                    })))
                    .child(chip("hide-person", if hidden { "Show again" } else { "Hide" }, false, theme).on_click(cx.listener(move |this, _, _, cx| {
                        let addr = addr.clone();
                        this.run(if hidden { "Shown again" } else { "Hidden \u{2014} find them under Hidden" }, cx, move |book, _| {
                            match hidden {
                                true => book.unhide_addr(&addr)?,
                                false => book.hide_addr(&addr)?,
                            }
                            Ok(None)
                        })
                    })));
                if let Some(domain) = entry.addrs.first().and_then(|a| a.rsplit_once('@')).map(|(_, d)| d.to_string())
                    && !clients::is_freemail(&domain)
                {
                    let name = entry.company.to_string();
                    row = row.child(chip("make-company", format!("Make @{domain} a company"), false, theme).on_click(cx.listener(
                        move |this, _, _, cx| {
                            let domain = domain.clone();
                            let name = name.clone();
                            this.run(format!("@{domain} is now a company"), cx, move |book, now| {
                                book.ensure_company(&domain, &name, now)?;
                                Ok(None)
                            })
                        },
                    )));
                }
            }
        }
        actions = actions.child(row);
        column.child(actions).into_any_element()
    }
}

/// A stage's name, with how many of those picked are in it when it is some
/// of them but not all.
fn count_label(name: &str, here: usize, of: usize) -> SharedString {
    match here {
        0 => name.to_string().into(),
        here if here == of => name.to_string().into(),
        here => format!("{name} \u{00b7} {here}").into(),
    }
}

fn stage_chip(id: impl Into<gpui::ElementId>, label: SharedString, on: bool, color: gpui::Hsla, theme: &Theme) -> gpui::Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .text_size(theme.small_text_size)
        .when(on, |s| s.border_color(color).bg(color.opacity(0.18)).text_color(theme.text))
        .when(!on, |s| s.border_color(theme.border).text_color(theme.text_muted).hover(|s| s.border_color(color).text_color(theme.text)))
        .child(label)
}

fn section(title: &'static str, theme: &Theme) -> Div {
    div().flex().flex_col().gap_1p5().child(
        div()
            .text_size(px(11.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_faint)
            .child(title.to_uppercase()),
    )
}

pub(crate) fn link(id: impl Into<gpui::ElementId>, label: &'static str, theme: &Theme) -> gpui::Stateful<Div> {
    div().id(id).cursor_pointer().text_color(theme.text_faint).hover(|s| s.text_color(theme.text)).child(label)
}

fn placeholder(text: &'static str, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_1()
        .items_center()
        .justify_center()
        .px_6()
        .text_center()
        .text_size(theme.small_text_size)
        .text_color(theme.text_faint)
        .child(text)
}

fn field(label: &'static str, input: &Entity<TextInput>, theme: &Theme, window: &Window, cx: &Context<ClientsWindow>) -> Div {
    let focused = input.read(cx).focus_handle.is_focused(window);
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(label))
        .child(
            div()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.background)
                .border_1()
                .border_color(if focused { theme.focus_ring } else { theme.border })
                .child(input.clone()),
        )
}

/// `Mar 2024`.
fn month_year(ts: i64) -> String {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (y, mo, _, _, _) = spool_core::civil_from_unix(ts + local_offset(ts));
    format!("{} {y}", MONTHS[(mo - 1).clamp(0, 11) as usize])
}

fn first_lines(text: &str, lines: usize) -> String {
    let mut out: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).take(lines + 1).collect();
    let more = out.len() > lines;
    out.truncate(lines);
    let mut joined = out.join("\n");
    if more {
        joined.push('\u{2026}');
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_note_shows_its_first_lines() {
        assert_eq!(first_lines("one\n\ntwo\nthree\nfour", 3), "one\ntwo\nthree\u{2026}");
        assert_eq!(first_lines("one", 3), "one");
    }
}
