//! The sale, as columns.
//!
//! A client is on the board when they have a stage and not otherwise — most
//! people you correspond with are not in a sale, and a board that showed them
//! all would be a second directory. Dragging a card is the gesture people
//! arrive with; `[` and `]` move the selected card a column without it, so the
//! board is as usable from the keyboard as the mailbox is.
//!
//! The closed columns — won, lost — fold to a narrow strip with a count. They
//! are where cards go to be finished with, and at full width they are most of
//! the board.
//!
//! A book can have stages of its own. Chosen in the sidebar, the board is that
//! book's; on all books, each set of stages is a board of its own, one under
//! the other, so nobody on any of them is out of sight.

use std::collections::HashMap;

use gpui::{AnyElement, Context, Div, Entity, FontWeight, SharedString, Stateful, Window, div, prelude::*, px};
use spool_store::book::{Client, Stage, StageKind};

use crate::{
    actions::Confirm,
    chrome::Tip,
    clients::{self, Entry, Key, Signal},
    clients_window::{ClientsWindow, Sheet, avatar, chip, stage_color},
    text_input::TextInput,
    clock::now_seconds,
    message_list::DragGhost,
    theme::Theme,
};

/// What a card carries while it is being dragged.
#[derive(Debug, Clone)]
pub struct DraggedClient {
    pub client: i64,
    pub label: SharedString,
}

/// A stage's column: where it sits among its book's, and its cards.
type Column<'a> = (usize, Stage, Vec<&'a Entry>);

const COLUMN_WIDTH: f32 = 248.;
const FOLDED_WIDTH: f32 = 44.;

impl ClientsWindow {
    /// The cards on one set of stages, per stage, in their order. Clients
    /// come from the entries rather than the book alone, because a card shows
    /// the mail too.
    fn columns<'a>(&self, entries: &'a [Entry], key: &str) -> Vec<Column<'a>> {
        let mut columns: Vec<Column<'a>> =
            clients::pipeline(&self.snapshot, key).into_iter().cloned().enumerate().map(|(i, s)| (i, s, Vec::new())).collect();
        let needle = self.search_text.trim().to_lowercase();
        for entry in entries {
            let Some(client) = entry.client.as_ref().filter(|c| !c.hidden) else { continue };
            if self.book_filter.as_ref().is_some_and(|b| !b.admits(entry)) {
                continue;
            }
            if !needle.is_empty()
                && !entry.name.to_lowercase().contains(&needle)
                && !entry.detail.to_lowercase().contains(&needle)
            {
                continue;
            }
            if let Some(stage) = client.stage
                && let Some(column) = columns.iter_mut().find(|(_, s, _)| s.id == stage)
            {
                column.2.push(entry);
            }
        }
        for (_, _, cards) in &mut columns {
            cards.sort_by(|a, b| {
                let (a, b) = (a.client.as_ref().expect("filtered"), b.client.as_ref().expect("filtered"));
                a.stage_pos.partial_cmp(&b.stage_pos).unwrap_or(std::cmp::Ordering::Equal).then(a.id.cmp(&b.id))
            });
        }
        columns
    }

    pub(crate) fn render_board(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.board_focus.is_focused(window);
        let entries = self.entries.clone();

        // One board for a book, or for all books while they share their
        // stages; otherwise the shared board and then each book's own. A
        // book put away keeps its board only while somebody is on it.
        let hidden: Vec<String> = self.snapshot.hidden_books.clone();
        let sections: Vec<(Option<String>, Vec<Column>)> = match &self.book_filter {
            Some(filter) => vec![(None, self.columns(&entries, clients::pipeline_key(&self.snapshot, &filter.email)))],
            None => {
                let own = clients::own_pipelines(&self.snapshot);
                let mut sections = vec![(Some(String::new()), self.columns(&entries, ""))];
                for book in own {
                    let columns = self.columns(&entries, &book);
                    if hidden.contains(&book) && columns.iter().all(|(_, _, cards)| cards.is_empty()) {
                        continue;
                    }
                    sections.push((Some(book), columns));
                }
                match sections.len() {
                    1 => vec![(None, sections.pop().expect("one").1)],
                    _ => sections,
                }
            }
        };

        let empty = sections.iter().all(|(_, columns)| columns.iter().all(|(_, _, cards)| cards.is_empty()));
        let now = now_seconds();
        let stacked = sections.len() > 1;
        let body: AnyElement = match stacked {
            false => {
                let columns = &sections[0].1;
                let rendered: Vec<AnyElement> = columns
                    .iter()
                    .map(|(index, stage, cards)| self.render_column(*index, stage, cards, false, now, theme, cx))
                    .collect();
                div()
                    .id("board-columns")
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .p_2()
                    .overflow_x_scroll()
                    .children(rendered)
                    .into_any_element()
            }
            true => {
                let mut rendered: Vec<AnyElement> = Vec::new();
                for (book, columns) in &sections {
                    let book = book.clone().unwrap_or_default();
                    let cards: usize = columns.iter().map(|(_, _, cards)| cards.len()).sum();
                    rendered.push(self.render_section_heading(&book, cards, theme, cx).into_any_element());
                    let row: Vec<AnyElement> = columns
                        .iter()
                        .map(|(index, stage, cards)| self.render_column(*index, stage, cards, true, now, theme, cx))
                        .collect();
                    rendered.push(
                        div()
                            .id(SharedString::from(format!("board-row-{book}")))
                            .flex()
                            .flex_shrink_0()
                            .gap_2()
                            .px_2()
                            .pb_3()
                            .overflow_x_scroll()
                            .children(row)
                            .into_any_element(),
                    );
                }
                div()
                    .id("board-sections")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .pt_1()
                    .overflow_y_scroll()
                    .children(rendered)
                    .into_any_element()
            }
        };

        div()
            .key_context("ClientBoard")
            .track_focus(&self.board_focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    window.focus(&this.board_focus);
                    // Anywhere on the board but a card is outside the card
                    // beside it, and puts it away. A card heard the press
                    // first, and said so.
                    if !std::mem::take(&mut this.card_pressed) {
                        this.close_card(cx);
                    }
                }),
            )
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(34.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme.border_subtle)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Board"))
                    .children(self.book_filter.as_ref().map(|filter| {
                        let own = !clients::pipeline_key(&self.snapshot, &filter.email).is_empty();
                        div()
                            .flex_shrink_0()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_muted)
                            .child(if own { "its own stages" } else { "the shared stages" })
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(match focused {
                                true => "Drag a card to move it, or [ and ] to move the chosen one",
                                false => "Drag a card to move it between stages",
                            }),
                    )
                    .child(div().flex_1())
                    .child(
                        crate::clients_window::chip("show-closed", if self.show_closed { "Fold closed" } else { "Show closed" }, self.show_closed, theme)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_closed = !this.show_closed;
                                cx.notify();
                            })),
                    )
                    .child(
                        chip("edit-stages", "Edit stages\u{2026}", false, theme)
                            .tooltip(|_, cx| cx.new(|_| Tip { text: "Add, rename, reorder or delete this board's columns".into() }).into())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let book = this.board_book();
                                this.open_stages(book, window, cx);
                            })),
                    ),
            )
            .child(body)
            .when(empty, |s| {
                s.child(
                    div()
                        .px_4()
                        .pb_3()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .child("Nobody is on the board yet. Give anyone a stage from their profile \u{2014} or select them in the directory and press ] \u{2014} and they land here."),
                )
            })
            .into_any_element()
    }

    /// The name over one of the stacked boards, and the way into editing it.
    fn render_section_heading(&mut self, book: &str, cards: usize, theme: &Theme, cx: &mut Context<Self>) -> Div {
        let account = self.accounts.iter().find(|a| spool_core::address::normalize(&a.email) == book).cloned();
        let label: SharedString = match book {
            "" => "Shared stages".into(),
            book => book.to_string().into(),
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .pt_2()
            .pb_1p5()
            .children(account.as_ref().map(|a| div().size(px(7.)).rounded_full().bg(theme.account_color(a.color))))
            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child(label))
            .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(cards.to_string()))
            .child({
                let book = book.to_string();
                chip(SharedString::from(format!("edit-stages-{book}")), "Edit stages", false, theme)
                    .on_click(cx.listener(move |this, _, window, cx| this.open_stages(book.clone(), window, cx)))
            })
    }

    /// One stage's column. `stacked` is one of several boards on a page that
    /// scrolls: as tall as its cards, rather than the window's height with
    /// its own scrolling.
    #[allow(clippy::too_many_arguments)]
    fn render_column(
        &mut self,
        index: usize,
        stage: &Stage,
        cards: &[&Entry],
        stacked: bool,
        now: i64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = stage_color(stage.kind, index, theme);
        let total: i64 = cards.iter().filter_map(|e| e.client.as_ref().and_then(|c| c.value)).sum();
        let stage_id = stage.id;
        let folded = stage.kind.is_closed() && !self.show_closed;
        let last_pos = cards.last().and_then(|e| e.client.as_ref()).map(|c| c.stage_pos);

        let column = div()
            .id(("column", stage.id as usize))
            .flex()
            .flex_col()
            .flex_shrink_0()
            .when(!stacked, |s| s.h_full())
            .when(stacked, |s| s.min_h(px(96.)))
            .rounded_lg()
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border_subtle)
            .drag_over::<DraggedClient>(move |style, _, _, _| style.border_color(color).bg(color.opacity(0.06)))
            .on_drop(cx.listener(move |this, dragged: &DraggedClient, _, cx| {
                // Dropped on the column rather than on a card: the bottom of it.
                let pos = last_pos.map(|p| p + 1.).unwrap_or(0.);
                this.set_stage(Key::Client(dragged.client), Some(stage_id), Some(pos), cx);
            }));

        if folded {
            return column
                .w(px(FOLDED_WIDTH))
                .items_center()
                .pt_2()
                .gap_2()
                .cursor_pointer()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.show_closed = true;
                    cx.notify();
                }))
                .child(div().size(px(8.)).rounded_full().bg(color))
                .child(div().text_size(theme.small_text_size).text_color(theme.text_muted).child(cards.len().to_string()))
                .child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        // Letters stacked, since gpui does not rotate text.
                        .flex()
                        .flex_col()
                        .items_center()
                        .children(stage.name.chars().take(10).map(|c| div().line_height(px(13.)).child(c.to_string()))),
                )
                .into_any_element();
        }

        let positions: Vec<f64> = cards.iter().filter_map(|e| e.client.as_ref().map(|c| c.stage_pos)).collect();
        let rendered: Vec<AnyElement> = cards
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                // Dropping on a card puts the dragged one above it: halfway
                // between it and the card before.
                let above = match i {
                    0 => positions[0] - 1.,
                    i => (positions[i - 1] + positions[i]) / 2.,
                };
                self.render_card(entry, stage, above, now, theme, cx).into_any_element()
            })
            .collect();

        column
            .w(px(COLUMN_WIDTH))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .pt_2p5()
                    .pb_2()
                    .flex_shrink_0()
                    .child(div().size(px(8.)).rounded_full().bg(color))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(stage.name.clone()))
                    .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(cards.len().to_string()))
                    .child(div().flex_1())
                    .when(total > 0, |s| {
                        s.child(div().text_size(theme.small_text_size).text_color(theme.text_muted).child(clients::money(total)))
                    })
                    // A stage is a group with a reason to hear from you: the
                    // proposals out, the deals in negotiation.
                    .when(!cards.is_empty(), |s| {
                        let group = stage.name.clone();
                        let entries: Vec<Entry> = cards.iter().map(|e| (*e).clone()).collect();
                        s.child(
                            div()
                                .id(("column-email", stage.id as usize))
                                .flex_shrink_0()
                                .px_1()
                                .rounded_sm()
                                .cursor_pointer()
                                .text_color(theme.text_faint)
                                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                                .tooltip(|_, cx| cx.new(|_| crate::chrome::Tip { text: "Write to everyone in this stage".into() }).into())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_mail(group.clone(), entries.clone(), cx)
                                }))
                                .child("\u{2709}"),
                        )
                    }),
            )
            .child(
                div()
                    .id(("column-cards", stage.id as usize))
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .px_2()
                    .pb_2()
                    .when(!stacked, |s| s.flex_1().min_h_0().overflow_y_scroll())
                    .children(rendered),
            )
            .into_any_element()
    }

    fn render_card(&mut self, entry: &Entry, stage: &Stage, above: f64, now: i64, theme: &Theme, cx: &mut Context<Self>) -> Stateful<Div> {
        let client: &Client = entry.client.as_ref().expect("a card is a client");
        let id = client.id;
        let selected = self.selected.as_ref() == Some(&entry.key);
        let open = stage.kind == StageKind::Open;
        let signal = clients::signal(&entry.stats, open, client.stage_since, true, now);
        let stage_id = stage.id;
        let dragged = DraggedClient { client: id, label: entry.name.clone() };
        let key = entry.key.clone();

        let line: Option<(gpui::Hsla, String)> = match signal {
            Signal::NeedsReply(at) => Some((theme.yellow, format!("\u{2190} needs reply \u{00b7} {}", clients::ago(now - at)))),
            Signal::GoneQuiet(at) => Some((theme.red, format!("quiet {}", clients::ago(now - at)))),
            Signal::Nothing => entry.stats.last.map(|last| {
                let arrow = match (entry.stats.last_in, entry.stats.last_out) {
                    (Some(i), Some(o)) if o >= i => "\u{2192}",
                    (Some(_), _) => "\u{2190}",
                    _ => "\u{2192}",
                };
                (theme.text_faint, format!("{arrow} {}", clients::ago(now - last)))
            }),
        };
        let in_stage = client.stage_since.filter(|_| open).map(|since| clients::ago(now - since));
        let tags: Vec<gpui::Hsla> = client
            .tags
            .iter()
            .filter_map(|t| self.snapshot.tags.iter().find(|tag| tag.id == *t))
            .map(|t| theme.avatar(t.color as u32))
            .take(5)
            .collect();

        div()
            .id(("card", id as usize))
            .flex()
            .flex_col()
            .gap_1()
            .p_2p5()
            .flex_shrink_0()
            .rounded_md()
            .bg(theme.background)
            .border_1()
            .cursor_pointer()
            .when(selected, |s| s.border_color(theme.focus_ring).bg(theme.selected_inactive))
            .when(!selected, |s| s.border_color(theme.border).hover(|s| s.border_color(theme.text_faint)))
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, _, _| this.card_pressed = true))
            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| DragGhost { label: dragged.label.clone() }))
            .drag_over::<DraggedClient>({
                let line = theme.accent;
                move |style, _, _, _| style.border_t_2().border_color(line)
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedClient, _, cx| {
                if dragged.client != id {
                    this.set_stage(Key::Client(dragged.client), Some(stage_id), Some(above), cx);
                }
                cx.stop_propagation();
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                window.focus(&this.board_focus);
                this.select(key.clone(), cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(avatar(&entry.name, &entry.detail, 22., theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_weight(FontWeight::MEDIUM)
                            .child(entry.name.clone()),
                    )
                    .children(client.value.map(|v| {
                        div().flex_shrink_0().text_size(theme.small_text_size).text_color(theme.text_muted).child(clients::money(v))
                    })),
            )
            .when(!entry.company.is_empty(), |s| {
                s.child(
                    div()
                        .text_size(theme.small_text_size)
                        .text_color(theme.text_faint)
                        .overflow_hidden()
                        .line_clamp(1)
                        .text_ellipsis()
                        .child(entry.company.clone()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(theme.small_text_size)
                    .children(line.map(|(color, text)| div().text_color(color).child(text)))
                    .child(div().flex_1())
                    .child(div().flex().gap_0p5().children(tags.into_iter().map(|c| div().size(px(6.)).rounded_full().bg(c))))
                    .children(in_stage.map(|t| div().text_color(theme.text_faint).child(t))),
            )
    }

}

/// One board's stages, being changed: a book's own, or the shared ones.
pub(crate) struct StagesSheet {
    /// Whose: a book's address, or empty for the shared ones. A book still
    /// on the shared ones is changing those.
    pub book: String,
    /// The name of a stage to add.
    pub new_name: Entity<TextInput>,
    /// A stage being renamed, and its field.
    pub renaming: Option<(i64, Entity<TextInput>)>,
    /// A stage whose Delete was pressed once.
    pub deleting: Option<i64>,
    /// "Back to the shared stages" was pressed once.
    pub sharing: bool,
}

impl ClientsWindow {
    /// Opens a board's stages to change: `book`'s, or the shared ones for
    /// `""`.
    pub(crate) fn open_stages(&mut self, book: String, window: &mut Window, cx: &mut Context<Self>) {
        let new_name = cx.new(|cx| TextInput::new("A new stage", cx));
        let focus = new_name.read(cx).focus_handle.clone();
        window.focus(&focus);
        self.sheet = Some(Sheet::Stages(StagesSheet { book, new_name, renaming: None, deleting: None, sharing: false }));
        cx.notify();
    }

    /// Return in the sheet: saves the name being changed, or adds the new one.
    fn stages_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Sheet::Stages(sheet)) = self.sheet.as_mut() else { return };
        if let Some((id, input)) = sheet.renaming.clone()
            && input.read(cx).focus_handle.is_focused(window)
        {
            let name = input.read(cx).text().trim().to_string();
            sheet.renaming = None;
            let focus = sheet.new_name.read(cx).focus_handle.clone();
            window.focus(&focus);
            if !name.is_empty() {
                self.run(format!("Renamed to {name}"), cx, move |book, _| {
                    book.rename_stage(id, &name)?;
                    Ok(None)
                });
            }
            cx.notify();
            return;
        }
        let name = sheet.new_name.read(cx).text().trim().to_string();
        if name.is_empty() {
            return;
        }
        sheet.new_name.update(cx, |input, cx| input.clear(cx));
        let into = sheet.book.clone();
        self.run(format!("Added {name}"), cx, move |book, _| {
            book.add_stage(&into, &name)?;
            Ok(None)
        });
    }

    fn stages_sheet(&mut self) -> Option<&mut StagesSheet> {
        match self.sheet.as_mut() {
            Some(Sheet::Stages(sheet)) => Some(sheet),
            _ => None,
        }
    }

    pub(crate) fn render_stages(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(Sheet::Stages(sheet)) = self.sheet.as_ref() else { return div().into_any_element() };
        let book = sheet.book.clone();
        let key = clients::pipeline_key(&self.snapshot, &book).to_string();
        let own = !key.is_empty();
        let stages: Vec<Stage> = clients::pipeline(&self.snapshot, &key).into_iter().cloned().collect();
        let renaming = sheet.renaming.clone();
        let deleting = sheet.deleting;
        let sharing = sheet.sharing;
        let new_name = sheet.new_name.clone();
        let new_focused = new_name.read(cx).focus_handle.is_focused(window);

        let mut on_stage: HashMap<i64, usize> = HashMap::new();
        for client in self.snapshot.clients.iter().filter(|c| !c.hidden) {
            if let Some(stage) = client.stage {
                *on_stage.entry(stage).or_default() += 1;
            }
        }
        // The books a change to the shared stages reaches.
        let hidden = self.hidden_accounts();
        let sharing_books: Vec<String> = self
            .accounts
            .iter()
            .filter(|a| !hidden.contains(&a.id))
            .map(|a| spool_core::address::normalize(&a.email))
            .filter(|email| clients::pipeline_key(&self.snapshot, email).is_empty())
            .collect();

        let (heading, about): (String, String) = match (book.is_empty(), own) {
            (true, _) => (
                "Shared stages".into(),
                format!(
                    "The board for every book without stages of its own \u{2014} {} of them.",
                    sharing_books.len()
                ),
            ),
            (false, true) => (format!("Stages for {book}"), "Its own: changing these changes no other book.".into()),
            (false, false) => (
                format!("Stages for {book}"),
                format!(
                    "It uses the shared stages, so a change here is a change for all {} books that do.",
                    sharing_books.len()
                ),
            ),
        };

        let field = |input: &Entity<TextInput>, focused: bool| {
            div()
                .flex_1()
                .min_w_0()
                .px_2()
                .py_1()
                .rounded_md()
                .bg(theme.background)
                .border_1()
                .border_color(if focused { theme.focus_ring } else { theme.border })
                .child(input.clone())
        };

        let last = stages.len().saturating_sub(1);
        let rows = stages.iter().enumerate().map(|(index, stage)| {
            let id = stage.id;
            let name = stage.name.clone();
            let count = on_stage.get(&id).copied().unwrap_or(0);
            let color = stage_color(stage.kind, index, theme);
            let being_renamed = renaming.as_ref().filter(|(r, _)| *r == id).map(|(_, input)| input.clone());
            let confirm_delete = deleting == Some(id);
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .hover(|s| s.bg(theme.hover))
                .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(color))
                .child(match being_renamed {
                    Some(input) => {
                        let focused = input.read(cx).focus_handle.is_focused(window);
                        field(&input, focused).into_any_element()
                    }
                    None => div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(SharedString::from(name.clone())))
                        .children(match stage.kind {
                            StageKind::Won => Some("won"),
                            StageKind::Lost => Some("lost"),
                            StageKind::Open => None,
                        }
                        .map(|kind| div().text_size(theme.small_text_size).text_color(theme.text_faint).child(kind)))
                        .child(
                            div()
                                .text_size(theme.small_text_size)
                                .text_color(theme.text_faint)
                                .child(SharedString::from(match count {
                                    0 => String::new(),
                                    1 => "1 on it".to_string(),
                                    n => format!("{n} on it"),
                                })),
                        )
                        .into_any_element(),
                })
                .child(
                    chip(("stage-up", id as usize), "\u{2191}", false, theme)
                        .when(index == 0, |s| s.opacity(0.35))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if index > 0 {
                                this.run("Moved", cx, move |book, _| {
                                    book.move_stage(id, false)?;
                                    Ok(None)
                                });
                            }
                        })),
                )
                .child(
                    chip(("stage-down", id as usize), "\u{2193}", false, theme)
                        .when(index == last, |s| s.opacity(0.35))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if index < last {
                                this.run("Moved", cx, move |book, _| {
                                    book.move_stage(id, true)?;
                                    Ok(None)
                                });
                            }
                        })),
                )
                .child(chip(("stage-rename", id as usize), "Rename", false, theme).on_click(cx.listener(
                    move |this, _, window, cx| {
                        let input = cx.new(|cx| {
                            let mut input = TextInput::new("Stage name", cx);
                            input.set_content(name.clone(), cx);
                            input
                        });
                        let focus = input.read(cx).focus_handle.clone();
                        if let Some(sheet) = this.stages_sheet() {
                            sheet.renaming = Some((id, input));
                            sheet.deleting = None;
                        }
                        window.focus(&focus);
                        cx.notify();
                    },
                )))
                .child(
                    chip(
                        ("stage-delete", id as usize),
                        match (confirm_delete, count) {
                            (false, _) => "Delete".to_string(),
                            (true, 0) => "Delete it?".to_string(),
                            (true, n) => format!("Delete? {n} come off the board"),
                        },
                        confirm_delete,
                        theme,
                    )
                    .when(confirm_delete, |s| s.text_color(theme.red).border_color(theme.red))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let Some(sheet) = this.stages_sheet() else { return };
                        if sheet.deleting != Some(id) {
                            sheet.deleting = Some(id);
                            cx.notify();
                            return;
                        }
                        sheet.deleting = None;
                        this.run("Stage deleted", cx, move |book, _| {
                            book.delete_stage(id)?;
                            Ok(None)
                        });
                    })),
                )
        });

        // A book on the shared stages can be given its own; one with its own
        // can go back.
        let switch = (!book.is_empty()).then(|| {
            let book = book.clone();
            match own {
                false => div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_3()
                    .rounded_md()
                    .bg(theme.selected)
                    .child(
                        div()
                            .flex_1()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_muted)
                            .child("Give this book stages of its own to change them without changing anyone else's. They start as a copy of the shared ones, and its clients keep their places."),
                    )
                    .child(chip("own-stages", "Give it its own", true, theme).on_click(cx.listener(move |this, _, _, cx| {
                        let filter = this
                            .accounts
                            .iter()
                            .find(|a| spool_core::address::normalize(&a.email) == book)
                            .map(|a| clients::BookFilter { email: book.clone(), account: Some(a.id) });
                        let on_board: Vec<i64> = this
                            .entries
                            .iter()
                            .filter(|e| filter.as_ref().is_some_and(|f| f.admits(e)))
                            .filter_map(|e| e.client.as_ref().map(|c| c.id))
                            .collect();
                        let book = book.clone();
                        this.run(format!("{book} has stages of its own"), cx, move |b, _| {
                            b.own_stages(&book, &on_board)?;
                            Ok(None)
                        });
                    })))
                    .into_any_element(),
                true => div()
                    .flex()
                    .justify_end()
                    .child(
                        chip(
                            "share-stages",
                            if sharing { "Delete these and use the shared ones?" } else { "Back to the shared stages" },
                            sharing,
                            theme,
                        )
                        .tooltip(|_, cx| {
                            cx.new(|_| Tip {
                                text: "Its clients go to the shared stage of the same name; anyone in a stage the shared ones have no name for comes off the board.".into(),
                            })
                            .into()
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let Some(sheet) = this.stages_sheet() else { return };
                            if !sheet.sharing {
                                sheet.sharing = true;
                                cx.notify();
                                return;
                            }
                            sheet.sharing = false;
                            let book = book.clone();
                            this.run(format!("{book} is on the shared stages"), cx, move |b, now| {
                                b.share_stages(&book, now)?;
                                Ok(None)
                            });
                        })),
                    )
                    .into_any_element(),
            }
        });

        div()
            .key_context("ClientField")
            .on_action(cx.listener(|this, _: &Confirm, window, cx| this.stages_confirm(window, cx)))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_size(px(16.)).font_weight(FontWeight::SEMIBOLD).child(SharedString::from(heading)))
                    .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(SharedString::from(about))),
            )
            .children(switch)
            .child(div().flex().flex_col().gap_0p5().children(rows))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(field(&new_name, new_focused))
                    .child(chip("stage-add", "Add", false, theme).on_click(cx.listener(|this, _, window, cx| {
                        if let Some(sheet) = this.stages_sheet() {
                            sheet.renaming = None;
                            let focus = sheet.new_name.read(cx).focus_handle.clone();
                            window.focus(&focus);
                        }
                        this.stages_confirm(window, cx);
                    }))),
            )
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child("A new stage goes before Won and Lost. Return adds it, or saves a name being changed."),
            )
            .child(div().flex().justify_end().child(chip("stages-done", "Done", true, theme).on_click(cx.listener(
                |this, _, _, cx| {
                    this.sheet = None;
                    cx.notify();
                },
            ))))
            .into_any_element()
    }
}
