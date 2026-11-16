//! Clients, in a window of its own.
//!
//! The argument the settings window makes, made again: what you know about a
//! client is something you consult *because of* a message, and a pane that
//! covered the mailbox would be a pane you could not check the message
//! against. So it is a second window, with the mailbox exactly as it was
//! beside it — and, unlike settings, a window that owns its own state, because
//! none of this belongs to the mailbox and the workspace is big enough.
//!
//! It shares two things. The book and the people index come from the
//! [`ClientBook`] global; replying, writing and showing a message in the
//! mailbox go through the workspace, which is where composers come from.
//!
//! The rule that keeps the mailbox fast holds here too: `render` never touches
//! a database. Everything it draws was loaded on a background thread into
//! `entries` and `profile`, and a keystroke in the search box filters what is
//! already in memory.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gpui::{
    AnyElement, App, BorrowAppContext, Bounds, Context, Div, Entity, FocusHandle, Focusable, FontWeight, Hsla,
    MouseButton, Pixels, Point, SharedString, Stateful, Subscription, Task, TitlebarOptions,
    UniformListScrollHandle, WeakEntity, Window, WindowBounds, WindowOptions, anchored, div, prelude::*, px,
    size, uniform_list,
};
use spool_core::{Account, AccountId, Address, Envelope, address};
use spool_store::{
    book::{Book, Detail, Kind, Note, Placement, Snapshot, StageKind},
    people::{Correspondent, Exchange},
};

use crate::{
    actions::{
        Cancel, ExtendNext, ExtendPrev, FocusSearch, ImportContacts, ManageSources, NewClient, SaveNote, SelectNext, SelectPrev,
        ShowClientBoard, ShowClientDirectory, StageLeft, StageRight, Undo as UndoAction,
    },
    chrome::Tip,
    clients::{self, BookFilter, ClientBook, Counts, Entry, Key, Section, Signal, Sort},
    clock::now_seconds,
    menus::CloseWindow,
    text_area::TextArea,
    text_input::{InputChanged, TextInput},
    theme::{Theme, ThemeAccess},
    workspace::Workspace,
};

/// The directory's rows are a fixed height, which is what lets the list be
/// virtualized: twenty thousand people cost what twenty do.
pub(crate) const ROW_HEIGHT: f32 = 50.;

/// The column at the right of a directory row: when, or how much mail when
/// that is the order, and the tags.
const ROW_AFTER: f32 = 80.;

/// The sidebar's width. Fixed: it holds short labels and counts.
pub(crate) const SIDEBAR_WIDTH: f32 = 196.;

/// The directory list's width beside a profile.
pub(crate) const LIST_WIDTH: f32 = 372.;

/// How much history a profile loads. A client of fifteen years is thousands
/// of messages, and the newest thousand is what anybody scrolls.
pub(crate) const HISTORY_LIMIT: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum View {
    Directory,
    Board,
}

/// Something a press of ⌘Z puts back.
pub(crate) enum Undo {
    Stage { client: i64, before: Placement },
    /// Several moved at once, from the directory.
    Stages(Vec<(i64, Placement)>),
    Note(Note),
    /// A book put away, by its address.
    BookHidden(String),
}

/// Which tab of a profile is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Tab {
    #[default]
    History,
    Notes,
    Details,
}

/// Everything about the selected client that is not in their [`Entry`].
pub(crate) struct Profile {
    pub key: Key,
    pub history: Vec<Exchange>,
    pub detail: Detail,
    /// Mail and events, newest first, as one timeline.
    pub timeline: Vec<Item>,
}

/// One line of a profile's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Item {
    /// An index into `history`.
    Mail(usize),
    /// An index into `detail.events`.
    Event(usize),
}

/// A message opened from the history.
pub(crate) struct Peek {
    pub exchange: Exchange,
    /// The message as the reader draws it, once it has been read and parsed.
    pub read: Option<Arc<Read>>,
    /// Whether its remote images may load: what the reader would do, decided
    /// when it was opened.
    pub images: bool,
}

/// A message's body, parsed the way the reader parses one.
pub(crate) struct Read {
    pub body: spool_core::Body,
    pub blocks: Vec<spool_core::html::Block>,
    pub source: spool_core::html::Source,
}

/// The fields of a client being edited.
pub(crate) struct EditForm {
    pub name: Entity<TextInput>,
    pub company: Entity<TextInput>,
    pub value: Entity<TextInput>,
    pub kind: Kind,
}

/// A sheet over the window: one thing being done that needs more room than
/// a row of buttons.
pub(crate) enum Sheet {
    NewClient(crate::clients_import::NewClientForm),
    Import(crate::clients_import::ImportSheet),
    Sources(crate::clients_import::SourcesSheet),
    /// One board's stages, being changed.
    Stages(crate::clients_board::StagesSheet),
    /// A message to a group being sent, or the report on it; the state is
    /// the book's.
    Mailing,
}

pub struct ClientsWindow {
    pub(crate) workspace: WeakEntity<Workspace>,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) list_focus: FocusHandle,
    pub(crate) board_focus: FocusHandle,

    /// What the index says, as last read.
    people: Option<Arc<Vec<Correspondent>>>,
    pub(crate) snapshot: Arc<Snapshot>,
    pub(crate) accounts: Vec<Account>,
    /// Your own addresses, never listed.
    me: Arc<HashSet<String>>,
    pub(crate) entries: Arc<Vec<Entry>>,
    index_of: Arc<HashMap<Key, usize>>,
    /// Indices into `entries`, in the order shown.
    pub(crate) shown: Vec<usize>,
    pub(crate) counts: Counts,
    loaded_mail: Option<u64>,
    loaded_book: Option<u64>,
    pub(crate) loaded_once: bool,
    generation: u64,
    load_task: Option<Task<()>>,

    pub(crate) view: View,
    pub(crate) section: Section,
    pub(crate) book_filter: Option<BookFilter>,
    pub(crate) sort: Sort,
    /// The sort menu, open where it was asked for.
    sort_menu: Option<Point<Pixels>>,
    pub(crate) two_way: bool,
    pub(crate) search: Entity<TextInput>,
    pub(crate) search_text: String,
    pub(crate) selected: Option<Key>,
    /// Rows picked with ⌘ or ⇧, to change together. Empty, or more than one:
    /// a pick of one is just the selection.
    pub(crate) picked: Vec<Key>,
    /// The row a ⇧-click's range runs from.
    pick_anchor: Option<usize>,
    /// The selection was asked for from outside — a message, the reader's
    /// mark — and its row is still to be brought into view, which waits for
    /// the list to have loaded.
    reveal: bool,
    pub(crate) cursor: usize,
    pub(crate) list_scroll: UniformListScrollHandle,

    pub(crate) profile: Option<Profile>,
    profile_task: Option<Task<()>>,
    pub(crate) tab: Tab,
    pub(crate) timeline_scroll: UniformListScrollHandle,
    pub(crate) peek: Option<Peek>,
    /// The reader's web view, for an HTML message opened from a history:
    /// made the first time one is, and kept for the next.
    #[cfg(target_os = "macos")]
    pub(crate) web_view: Option<crate::webview::SharedWebView>,
    /// Whether this frame drew it. A native view is not part of the scene, so
    /// a frame that does not draw it has to take it off screen.
    #[cfg(target_os = "macos")]
    pub(crate) web_view_drawn: bool,
    peek_task: Option<Task<()>>,
    pub(crate) note_input: Entity<TextArea>,
    pub(crate) editing_note: Option<i64>,
    pub(crate) tag_input: Entity<TextInput>,
    pub(crate) identity_input: Entity<TextInput>,
    pub(crate) field_key: Entity<TextInput>,
    pub(crate) field_value: Entity<TextInput>,
    pub(crate) edit: Option<EditForm>,
    /// A client waiting to have another folded into it: the next client
    /// chosen in the list is the one that goes.
    pub(crate) merge_into: Option<i64>,
    /// The delete button has been pressed once.
    pub(crate) confirm_delete: bool,

    /// The closed columns, folded to their counts until opened.
    pub(crate) show_closed: bool,
    /// A card on the board was pressed: the press the board hears after it is
    /// not one outside the card, and does not put the card away.
    pub(crate) card_pressed: bool,

    pub(crate) sheet: Option<Sheet>,
    pub(crate) undo: Vec<Undo>,
    pub(crate) status: SharedString,
    _subscriptions: Vec<Subscription>,
}

impl ClientsWindow {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            let mut input = TextInput::new("Search people, companies, #tags", cx);
            input.set_short_placeholder("Search");
            input
        });
        let note_input = cx.new(|cx| TextArea::new("Write a note\u{2026}  \u{2318}\u{21a9} to save", cx));
        let tag_input = cx.new(|cx| TextInput::new("Add a tag", cx));
        let identity_input = cx.new(|cx| TextInput::new("Add an address, or @domain for a whole company", cx));
        let field_key = cx.new(|cx| TextInput::new("Field", cx));
        let field_value = cx.new(|cx| TextInput::new("Value", cx));

        let mut subscriptions = vec![
            cx.subscribe(&search, |this: &mut Self, _, event: &InputChanged, cx| {
                this.search_text = event.0.to_string();
                this.refilter(true, cx);
            }),
            // Enter in a one-line field is "add this": the field says what.
            cx.subscribe(&tag_input, |this: &mut Self, input, event: &InputChanged, cx| {
                if event.0.contains(',') {
                    let text = event.0.replace(',', "");
                    input.update(cx, |input, cx| input.clear(cx));
                    this.add_tag(text, cx);
                }
            }),
            cx.observe_global::<ClientBook>(|this: &mut Self, cx| this.load(cx)),
        ];
        // Mail arriving repaints the mailbox, and the mailbox is where the
        // accounts are kept; following it keeps the list of books in step.
        if let Some(workspace) = workspace.upgrade() {
            subscriptions.push(cx.observe(&workspace, |this: &mut Self, workspace, cx| {
                let accounts = workspace.read(cx).accounts.clone();
                if accounts.len() != this.accounts.len() {
                    this.accounts = accounts;
                    this.load(cx);
                }
            }));
        }

        let list_focus = cx.focus_handle();
        window.focus(&list_focus);
        let accounts = workspace.upgrade().map(|w| w.read(cx).accounts.clone()).unwrap_or_default();

        let mut this = Self {
            workspace,
            focus_handle: cx.focus_handle(),
            list_focus,
            board_focus: cx.focus_handle(),
            people: None,
            snapshot: Arc::new(Snapshot::default()),
            accounts,
            me: Arc::new(HashSet::new()),
            entries: Arc::new(Vec::new()),
            index_of: Arc::new(HashMap::new()),
            shown: Vec::new(),
            counts: Counts::default(),
            loaded_mail: None,
            loaded_book: None,
            loaded_once: false,
            generation: 0,
            load_task: None,
            view: View::Directory,
            section: Section::Everyone,
            book_filter: None,
            sort: Sort::Recent,
            sort_menu: None,
            two_way: true,
            search,
            search_text: String::new(),
            selected: None,
            picked: Vec::new(),
            pick_anchor: None,
            reveal: false,
            cursor: 0,
            list_scroll: UniformListScrollHandle::new(),
            profile: None,
            profile_task: None,
            tab: Tab::History,
            timeline_scroll: UniformListScrollHandle::new(),
            peek: None,
            #[cfg(target_os = "macos")]
            web_view: None,
            #[cfg(target_os = "macos")]
            web_view_drawn: false,
            peek_task: None,
            note_input,
            editing_note: None,
            tag_input,
            identity_input,
            field_key,
            field_value,
            edit: None,
            merge_into: None,
            confirm_delete: false,
            show_closed: false,
            card_pressed: false,
            sheet: None,
            undo: Vec::new(),
            status: SharedString::default(),
            _subscriptions: subscriptions,
        };
        this.load(cx);
        this
    }

    /// Reads whatever has changed since the last read, off the UI thread.
    ///
    /// The index is the expensive half — a pass over every row of it — so it
    /// is re-read only when mail has arrived or the backfill has moved on.
    /// The book is a few thousand rows and is taken from the global, which
    /// already holds the latest copy.
    pub(crate) fn load(&mut self, cx: &mut Context<Self>) {
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let mail_version = global.mail_version;
        let book_version = global.version;
        if self.loaded_mail == Some(mail_version) && self.loaded_book == Some(book_version) && self.loaded_once {
            // Something else changed — a pull started, the backfill ticked.
            cx.notify();
            return;
        }

        let reload_people = self.loaded_mail != Some(mail_version) || self.people.is_none();
        let reader = global.reader.clone();
        let store = global.store.clone();
        let snapshot = global.snapshot.clone();
        let people = self.people.clone();
        self.generation += 1;
        let generation = self.generation;

        let me: HashSet<String> = self.accounts.iter().map(|a| address::normalize(&a.email)).collect();
        let me = Arc::new(me);
        let me_for_build = me.clone();

        self.load_task = Some(cx.spawn(async move |this, cx| {
            let built = cx
                .background_executor()
                .spawn(async move {
                    let people = match (reload_people, people) {
                        (false, Some(people)) => people,
                        _ => Arc::new(match &reader {
                            Some(reader) => reader.people()?,
                            None => store.people()?,
                        }),
                    };
                    let tag_names: HashMap<i64, String> =
                        snapshot.tags.iter().map(|t| (t.id, t.name.clone())).collect();
                    let entries = clients::everyone(&people, &snapshot, &me_for_build, &tag_names);
                    let index_of: HashMap<Key, usize> =
                        entries.iter().enumerate().map(|(i, e)| (e.key.clone(), i)).collect();
                    anyhow::Ok((people, snapshot, entries, index_of))
                })
                .await;

            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match built {
                    Ok((people, snapshot, entries, index_of)) => {
                        let first = !this.loaded_once;
                        this.people = Some(people);
                        this.snapshot = snapshot;
                        this.entries = Arc::new(entries);
                        this.index_of = Arc::new(index_of);
                        this.me = me;
                        this.loaded_mail = Some(mail_version);
                        this.loaded_book = Some(book_version);
                        this.loaded_once = true;
                        this.follow_selection();
                        this.refilter(false, cx);
                        if this.reveal {
                            this.reveal_selected(cx);
                        }
                        // A window that opens on nobody is a window with half
                        // of itself blank: the first time, the top of the list.
                        match (first, this.selected.is_none(), this.view, this.shown.is_empty()) {
                            (true, true, View::Directory, false) => this.select_row(0, cx),
                            _ => this.load_profile(false, cx),
                        }
                    }
                    Err(e) => this.status = format!("Could not read the people index: {e:#}").into(),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// A person just made into a client is a different key: the selection
    /// follows them across.
    fn follow_selection(&mut self) {
        if let Some(key) = self.selected.as_ref().and_then(|key| self.followed(key)) {
            self.selected = Some(key);
        }
        // And the picks: moving ten people to Lead makes ten clients, and
        // they stay picked for whatever is done to them next.
        let picked = std::mem::take(&mut self.picked);
        self.picked = picked
            .into_iter()
            .map(|key| self.followed(&key).unwrap_or(key))
            .filter(|key| self.index_of.contains_key(key))
            .collect();
        if self.picked.len() < 2 {
            self.picked.clear();
        }
    }

    /// The key somebody has now, if it is not the one given.
    fn followed(&self, key: &Key) -> Option<Key> {
        let Key::Person(addr) = key else { return None };
        if self.index_of.contains_key(key) {
            return None;
        }
        let client = self.snapshot.clients.iter().find(|c| c.addrs.iter().any(|a| a == addr.as_ref()))?;
        Some(Key::Client(client.id))
    }

    /// Re-runs the filter over what is in memory.
    pub(crate) fn refilter(&mut self, to_top: bool, cx: &mut Context<Self>) {
        let open = clients::open_stages(&self.snapshot);
        let hidden_books = self.hidden_accounts();
        // A book put away is not one to be looking at.
        if let Some(filter) = &self.book_filter
            && filter.account.is_some_and(|a| hidden_books.contains(&a))
        {
            self.book_filter = None;
        }
        let (shown, counts) = clients::filter(
            &self.entries,
            self.section,
            self.book_filter.as_ref(),
            &self.search_text,
            self.two_way,
            self.sort,
            &open,
            &hidden_books,
            now_seconds(),
        );
        if to_top {
            self.pick_anchor = None;
        }
        self.shown = shown;
        self.counts = counts;
        let at = self.selected.as_ref().and_then(|key| {
            let index = *self.index_of.get(key)?;
            self.shown.iter().position(|&i| i == index)
        });
        self.cursor = match (at, to_top) {
            (Some(at), false) => at,
            _ => 0,
        };
        if to_top {
            self.list_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        }
        cx.notify();
    }

    /// Puts the card beside the board away.
    pub(crate) fn close_card(&mut self, cx: &mut Context<Self>) {
        if self.selected.take().is_some() {
            self.profile = None;
            self.peek = None;
            self.edit = None;
            cx.notify();
        }
    }

    /// Whether the sort menu is open, which covers part of the window.
    pub(crate) fn sort_menu_open(&self) -> bool {
        self.sort_menu.is_some()
    }

    /// The accounts whose books are put away.
    pub(crate) fn hidden_accounts(&self) -> HashSet<AccountId> {
        self.accounts
            .iter()
            .filter(|a| self.snapshot.hidden_books.iter().any(|b| *b == address::normalize(&a.email)))
            .map(|a| a.id)
            .collect()
    }

    /// The books somebody is in: those the book lists them in, and those
    /// their mail is in.
    fn books_of(&self, entry: &Entry) -> Vec<String> {
        let mut books: Vec<String> = entry.client.as_ref().map(|c| c.books.clone()).unwrap_or_default();
        for account in &entry.accounts {
            if let Some(email) = self.account_email(*account).map(address::normalize)
                && !books.contains(&email)
            {
                books.push(email);
            }
        }
        books
    }

    /// Whose stages somebody moves through, as a [`clients::pipeline_key`]:
    /// the ones they are in now; or the book being looked at; or the one book
    /// of theirs with stages of its own; or the shared ones.
    pub(crate) fn pipeline_for(&self, entry: Option<&Entry>) -> String {
        if let Some(stage) = entry.and_then(Entry::stage)
            && let Some(stage) = self.snapshot.stages.iter().find(|s| s.id == stage)
        {
            return stage.book.clone();
        }
        if let Some(filter) = &self.book_filter {
            return clients::pipeline_key(&self.snapshot, &filter.email).to_string();
        }
        let own: Vec<String> = entry
            .map(|e| self.books_of(e))
            .unwrap_or_default()
            .into_iter()
            .filter(|b| !clients::pipeline_key(&self.snapshot, b).is_empty())
            .collect();
        match own.as_slice() {
            [only] => only.clone(),
            _ => String::new(),
        }
    }

    /// The book the board is showing: the one chosen in the sidebar, or
    /// `""` for all of them.
    pub(crate) fn board_book(&self) -> String {
        self.book_filter.as_ref().map(|f| f.email.clone()).unwrap_or_default()
    }

    pub(crate) fn entry(&self, key: &Key) -> Option<&Entry> {
        self.index_of.get(key).and_then(|&i| self.entries.get(i))
    }

    pub(crate) fn selected_entry(&self) -> Option<&Entry> {
        self.selected.as_ref().and_then(|key| self.entry(key))
    }

    /// A click on a row: ⌘ adds it to the picks or takes it out, ⇧ picks
    /// the run from the last row clicked, and a plain click is one person.
    fn click_row(&mut self, row: usize, modifiers: gpui::Modifiers, cx: &mut Context<Self>) {
        let Some(key) = self.shown.get(row).map(|&i| self.entries[i].key.clone()) else { return };
        if modifiers.shift {
            self.pick_run(row, cx);
            return;
        } else if modifiers.secondary() {
            if self.picked.is_empty()
                && let Some(selected) = self.selected.clone()
                && selected != key
            {
                self.picked.push(selected);
            }
            match self.picked.iter().position(|k| *k == key) {
                Some(at) => {
                    self.picked.remove(at);
                }
                None => self.picked.push(key),
            }
            self.pick_anchor = Some(row);
        } else {
            self.picked.clear();
            self.pick_anchor = Some(row);
            self.select_row(row, cx);
            return;
        }
        self.cursor = row;
        // One left is one person, shown as one.
        if self.picked.len() == 1 {
            let only = self.picked.remove(0);
            self.select(only, cx);
        }
        cx.notify();
    }

    /// Picks every row from where the picking started — the last row clicked
    /// or chosen with the arrows — to `row`, which becomes the cursor. A run
    /// back down to one row is that row, chosen.
    fn pick_run(&mut self, row: usize, cx: &mut Context<Self>) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() - 1;
        let row = row.min(last);
        let anchor = self.pick_anchor.unwrap_or(self.cursor).min(last);
        let (from, to) = (anchor.min(row), anchor.max(row));
        self.picked = self.shown[from..=to].iter().map(|&i| self.entries[i].key.clone()).collect();
        self.pick_anchor = Some(anchor);
        self.cursor = row;
        if self.picked.len() == 1 {
            let only = self.picked.remove(0);
            self.select(only, cx);
        }
        cx.notify();
    }

    /// ⇧↓ and ⇧↑: the run grows or shrinks a row at a time, as it does in
    /// the mailbox. The list scrolls only as far as it must — from the side
    /// the cursor is heading for, so the row does not jump to the other edge.
    fn extend_next(&mut self, _: &ExtendNext, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(self.cursor.saturating_add(1), gpui::ScrollStrategy::Bottom, cx);
    }

    fn extend_prev(&mut self, _: &ExtendPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(self.cursor.saturating_sub(1), gpui::ScrollStrategy::Top, cx);
    }

    fn extend_to(&mut self, row: usize, scroll: gpui::ScrollStrategy, cx: &mut Context<Self>) {
        if self.view != View::Directory || self.shown.is_empty() {
            return;
        }
        // Nobody chosen yet: the first press chooses the top row, as a plain
        // arrow would.
        if self.selected.is_none() && self.picked.is_empty() {
            self.select_row(0, cx);
            return;
        }
        self.pick_run(row, cx);
        self.list_scroll.scroll_to_item(self.cursor, scroll);
    }

    /// Moves everyone picked to a stage, or takes them off the board.
    ///
    /// Somebody only in your mail becomes a client to be put on the board —
    /// and is left alone by "Off board", which they already are.
    pub(crate) fn set_picked_stage(&mut self, stage: Option<i64>, cx: &mut Context<Self>) {
        enum Who {
            Client(i64),
            Person(usize),
        }
        let mut who: Vec<Who> = Vec::new();
        let mut people: Vec<(String, String, Option<String>)> = Vec::new();
        for key in &self.picked {
            match key {
                Key::Client(id) => who.push(Who::Client(*id)),
                Key::Person(_) if stage.is_none() => {}
                Key::Person(addr) => {
                    let entry = self.entry(key);
                    who.push(Who::Person(people.len()));
                    people.push((addr.to_string(), entry.map(|e| e.name.to_string()).unwrap_or_default(), self.book_for(entry)));
                }
            }
        }
        let count = who.len();
        let label = match stage.and_then(|s| self.snapshot.stages.iter().find(|st| st.id == s)) {
            Some(stage) => format!("Moved {count} to {}", stage.name),
            None => format!("Took {count} off the board"),
        };
        self.run(label, cx, move |book, now| {
            let made = book.ensure_clients(&people, now)?;
            let ids: Vec<i64> = who
                .iter()
                .map(|w| match w {
                    Who::Client(id) => *id,
                    Who::Person(at) => made[*at],
                })
                .collect();
            let before = book.set_stages(&ids, stage, now)?;
            Ok(Some(Undo::Stages(before)))
        });
    }

    /// Puts a book away: out of the sidebar, and its people out of the list.
    fn hide_book(&mut self, email: String, cx: &mut Context<Self>) {
        let label = format!("Put {email} away \u{2014} Settings \u{2192} Extensions brings it back");
        self.run(label, cx, move |book, _| {
            book.set_book_hidden(&email, true)?;
            Ok(Some(Undo::BookHidden(email)))
        });
    }

    pub(crate) fn select_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(&index) = self.shown.get(row) else { return };
        let key = self.entries[index].key.clone();
        self.cursor = row;
        self.list_scroll.scroll_to_item(row, gpui::ScrollStrategy::Center);
        self.select(key, cx);
    }

    /// Points the profile at somebody.
    pub(crate) fn select(&mut self, key: Key, cx: &mut Context<Self>) {
        // A merge waiting for its other half takes this choice as it — when
        // it is another client. Somebody only in your mail is just looked at,
        // and the merge goes on waiting.
        if let Some(into) = self.merge_into
            && let Key::Client(from) = key
            && from != into
        {
            self.merge_into = None;
            let name = self.entry(&key).map(|e| e.name.clone()).unwrap_or_default();
            self.run(format!("Merged {name} in"), cx, move |book, now| {
                book.merge(into, from, now)?;
                Ok(None)
            });
            self.selected = Some(Key::Client(into));
            self.load_profile(true, cx);
            return;
        }

        if self.selected.as_ref() != Some(&key) {
            self.peek = None;
            self.edit = None;
            self.editing_note = None;
            self.confirm_delete = false;
            self.note_input.update(cx, |input, cx| input.set_text("", cx));
        }
        self.selected = Some(key);
        self.load_profile(true, cx);
        cx.notify();
    }

    /// Loads the selected client's history and notes.
    ///
    /// `fresh` is a new selection, whose timeline starts at the top; a reload
    /// of the same one keeps its place.
    pub(crate) fn load_profile(&mut self, fresh: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry().cloned() else {
            self.profile = None;
            return;
        };
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let reader = global.reader.clone();
        let store = global.store.clone();
        let book = global.book.clone();
        let client = entry.client.as_ref().map(|c| c.id);
        let key = entry.key.clone();
        if fresh {
            self.timeline_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        }

        self.profile_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let history = match &reader {
                        Some(reader) => reader.history(&entry.addrs, &entry.domains, HISTORY_LIMIT)?,
                        None => store.history(&entry.addrs, &entry.domains, HISTORY_LIMIT)?,
                    };
                    let detail = match client {
                        Some(client) => book.detail(client)?,
                        None => Detail::default(),
                    };
                    anyhow::Ok((history, detail))
                })
                .await;
            this.update(cx, |this, cx| {
                if this.selected.as_ref() != Some(&key) {
                    return;
                }
                match loaded {
                    Ok((history, detail)) => {
                        let timeline = timeline(&history, &detail);
                        this.profile = Some(Profile { key, history, detail, timeline });
                    }
                    Err(e) => this.status = format!("Could not load the history: {e:#}").into(),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Runs a change to the book off the UI thread, then re-reads it.
    ///
    /// The status line says what happened, or what went wrong — a change that
    /// failed silently would be a client who is not where you put them.
    pub(crate) fn run(
        &mut self,
        done: impl Into<SharedString>,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Book, i64) -> anyhow::Result<Option<Undo>> + Send + 'static,
    ) {
        let Some(book) = cx.try_global::<ClientBook>().map(|g| g.book.clone()) else { return };
        let done = done.into();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { change(&book, now_seconds()) }).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(undo) => {
                        if let Some(undo) = undo {
                            this.undo.push(undo);
                            if this.undo.len() > 32 {
                                this.undo.remove(0);
                            }
                        }
                        this.status = done;
                    }
                    Err(e) => this.status = format!("{e:#}").into(),
                }
                cx.notify();
            })
            .ok();
            cx.update(clients::reload).ok();
        })
        .detach();
    }

    /// As [`Self::run`], for a change that needs a client to make it to:
    /// somebody who is only in your mail becomes one first.
    pub(crate) fn run_on_client(
        &mut self,
        key: Key,
        done: impl Into<SharedString>,
        cx: &mut Context<Self>,
        change: impl FnOnce(&Book, i64, i64) -> anyhow::Result<Option<Undo>> + Send + 'static,
    ) {
        let target = match &key {
            Key::Client(id) => Target::Client(*id),
            Key::Person(addr) => {
                let entry = self.entry(&key);
                let name = entry.map(|e| e.name.to_string()).unwrap_or_default();
                let book = self.book_for(entry);
                Target::Person { addr: addr.to_string(), name, book }
            }
        };
        self.run(done, cx, move |book, now| {
            let client = match target {
                Target::Client(id) => id,
                Target::Person { addr, name, book: home } => book.ensure_client(&addr, &name, home.as_deref(), now)?,
            };
            change(book, client, now)
        });
    }

    /// Which account's book somebody new to the book belongs in: the one
    /// being looked at, or the one their mail is in.
    pub(crate) fn book_for(&self, entry: Option<&Entry>) -> Option<String> {
        if let Some(filter) = &self.book_filter {
            return Some(filter.email.clone());
        }
        let account = entry.and_then(|e| e.accounts.first())?;
        self.accounts.iter().find(|a| a.id == *account).map(|a| a.email.clone())
    }

    pub(crate) fn account_email(&self, id: AccountId) -> Option<&str> {
        self.accounts.iter().find(|a| a.id == id).map(|a| a.email.as_str())
    }

    // Actions.

    fn close(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        forget_window(cx);
        window.remove_window();
    }

    /// Escape unwinds one layer at a time.
    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.sort_menu.is_some() {
            self.sort_menu = None;
        } else if self.sheet.is_some() {
            self.sheet = None;
        } else if !self.picked.is_empty() {
            self.picked.clear();
        } else if self.merge_into.is_some() {
            self.merge_into = None;
            self.status = "Merge cancelled".into();
        } else if self.peek.is_some() {
            self.peek = None;
        } else if self.edit.is_some() {
            self.edit = None;
        } else if !self.search_text.is_empty() {
            self.search.update(cx, |input, cx| input.clear(cx));
        } else if self.view == View::Board && self.selected.is_some() {
            self.selected = None;
            self.profile = None;
        }
        window.focus(match self.view {
            View::Directory => &self.list_focus,
            View::Board => &self.board_focus,
        });
        cx.notify();
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search.read(cx).focus_handle.clone();
        window.focus(&handle);
        self.search.update(cx, |input, cx| input.select_all_text(cx));
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.picked.clear();
        self.pick_anchor = None;
        if self.view == View::Directory && !self.shown.is_empty() {
            let next = match self.selected {
                Some(_) => (self.cursor + 1).min(self.shown.len() - 1),
                None => 0,
            };
            self.select_row(next, cx);
        }
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.picked.clear();
        self.pick_anchor = None;
        if self.view == View::Directory && !self.shown.is_empty() {
            self.select_row(self.cursor.saturating_sub(1), cx);
        }
    }

    fn show_directory(&mut self, _: &ShowClientDirectory, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Directory, window, cx);
    }

    fn show_board(&mut self, _: &ShowClientBoard, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Board, window, cx);
    }

    pub(crate) fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        self.view = view;
        window.focus(match view {
            View::Directory => &self.list_focus,
            View::Board => &self.board_focus,
        });
        cx.notify();
    }

    fn new_client(&mut self, _: &NewClient, window: &mut Window, cx: &mut Context<Self>) {
        self.open_new_client(window, cx);
    }

    fn import_contacts(&mut self, _: &ImportContacts, window: &mut Window, cx: &mut Context<Self>) {
        self.open_import(window, cx);
    }

    fn manage_sources(&mut self, _: &ManageSources, window: &mut Window, cx: &mut Context<Self>) {
        self.open_sources(window, cx);
    }

    fn stage_left(&mut self, _: &StageLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.step_stage(false, cx);
    }

    fn stage_right(&mut self, _: &StageRight, _: &mut Window, cx: &mut Context<Self>) {
        self.step_stage(true, cx);
    }

    /// Moves the selected client one column along the board. From off the
    /// board, right puts them in the first stage.
    fn step_stage(&mut self, right: bool, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else { return };
        let key = self.pipeline_for(Some(entry));
        let stages = clients::pipeline(&self.snapshot, &key);
        let at = entry.stage().and_then(|s| stages.iter().position(|st| st.id == s));
        let next = match (at, right) {
            (None, true) => stages.first().map(|s| Some(s.id)),
            (None, false) => None,
            (Some(0), false) => Some(None),
            (Some(at), false) => Some(Some(stages[at - 1].id)),
            (Some(at), true) => stages.get(at + 1).map(|s| Some(s.id)),
        };
        if let Some(stage) = next {
            self.set_stage(entry.key.clone(), stage, None, cx);
        }
    }

    pub(crate) fn set_stage(&mut self, key: Key, stage: Option<i64>, pos: Option<f64>, cx: &mut Context<Self>) {
        let label = match stage.and_then(|s| self.snapshot.stages.iter().find(|st| st.id == s)) {
            Some(stage) => format!("Moved to {}", stage.name),
            None => "Taken off the board".to_string(),
        };
        self.run_on_client(key, label, cx, move |book, client, now| {
            let before = book.set_stage(client, stage, pos, now)?;
            Ok(Some(Undo::Stage { client, before }))
        });
    }

    fn undo(&mut self, _: &UndoAction, _: &mut Window, cx: &mut Context<Self>) {
        let Some(undo) = self.undo.pop() else {
            self.status = "Nothing to undo".into();
            cx.notify();
            return;
        };
        let (label, change): (&str, Restore) = match undo {
            Undo::Stage { client, before } => (
                "Put back where it was",
                Box::new(move |book: &Book, now| book.restore_placement(client, before, now)),
            ),
            Undo::Stages(before) => (
                "Put back where they were",
                Box::new(move |book: &Book, now| book.restore_placements(&before, now)),
            ),
            Undo::Note(note) => ("Note restored", Box::new(move |book: &Book, _| book.restore_note(&note))),
            Undo::BookHidden(email) => {
                ("Book brought back", Box::new(move |book: &Book, _| book.set_book_hidden(&email, false)))
            }
        };
        self.run(label, cx, move |book, now| {
            change(book, now)?;
            Ok(None)
        });
    }

    fn save_note_action(&mut self, _: &SaveNote, _: &mut Window, cx: &mut Context<Self>) {
        self.save_note(cx);
    }

    pub(crate) fn save_note(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else { return };
        let body = self.note_input.read(cx).text().trim().to_string();
        if body.is_empty() {
            return;
        }
        self.note_input.update(cx, |input, cx| input.set_text("", cx));
        match self.editing_note.take() {
            Some(note) => self.run("Note saved", cx, move |book, now| {
                book.update_note(note, &body, now)?;
                Ok(None)
            }),
            None => self.run_on_client(key, "Note added", cx, move |book, client, now| {
                book.add_note(client, &body, now)?;
                Ok(None)
            }),
        }
        self.tab = Tab::Notes;
    }

    pub(crate) fn add_tag(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(key) = self.selected.clone() else { return };
        let names: Vec<String> =
            text.split(',').map(|t| t.trim().trim_start_matches('#').to_string()).filter(|t| !t.is_empty()).collect();
        if names.is_empty() {
            return;
        }
        let label = format!("Tagged #{}", names.join(" #"));
        self.run_on_client(key, label, cx, move |book, client, now| {
            for name in &names {
                book.tag(client, name, now)?;
            }
            Ok(None)
        });
    }

    // Through the workspace.

    /// Opens a message in the mailbox and brings the mailbox forward.
    pub(crate) fn show_in_mail(&mut self, envelope: Envelope, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        workspace.update(cx, |workspace, cx| workspace.reveal_message(&envelope, cx));
        activate_mailbox(cx);
    }

    pub(crate) fn reply(&mut self, exchange: Exchange, all: bool, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else { return };
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let store = global.store.clone();
        let envelope = exchange.envelope.clone();
        let known = self
            .peek
            .as_ref()
            .filter(|p| p.exchange.envelope.id == envelope.id)
            .and_then(|p| p.read.as_ref().map(|r| r.body.clone()));
        cx.spawn(async move |_, cx| {
            let body = match known {
                Some(body) => Some(body),
                None => {
                    let id = envelope.id;
                    cx.background_executor().spawn(async move { store.body(id).ok().flatten() }).await
                }
            };
            let body = body.unwrap_or_default();
            let html = (!body.html.trim().is_empty()).then(|| {
                let inline = crate::message_view::inline_data_urls(&body);
                spool_core::sanitize::for_web_view(&body.html, spool_core::sanitize::Remote::Allowed, &inline)
            });
            let kind = match all {
                true => spool_core::draft::ReplyKind::All,
                false => spool_core::draft::ReplyKind::Sender,
            };
            workspace
                .update(cx, |workspace, cx| workspace.reply_to_message(envelope, body, html, kind, cx))
                .ok();
        })
        .detach();
    }

    pub(crate) fn write_to(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else { return };
        let Some(addr) = entry.addrs.first().cloned() else {
            self.status = "No address to write to".into();
            cx.notify();
            return;
        };
        let to = Address::new(entry.name.to_string(), addr);
        let from = self.book_for(Some(entry));
        let Some(workspace) = self.workspace.upgrade() else { return };
        workspace.update(cx, |workspace, cx| workspace.compose_to(to, from.as_deref(), cx));
    }

    /// Opens a message from the history beside it.
    pub(crate) fn open_peek(&mut self, exchange: Exchange, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(global) = cx.try_global::<ClientBook>() else { return };
        let store = global.store.clone();
        let id = exchange.envelope.id;
        // Remote images as the reader has them for this message or sender:
        // shown here only where they would be shown there.
        let images = self.workspace.upgrade().is_some_and(|workspace| {
            workspace.read(cx).images.is_allowed(id, exchange.envelope.from.domain().as_deref())
        });
        self.peek = Some(Peek { exchange, read: None, images });
        self.peek_task = Some(cx.spawn(async move |this, cx| {
            // Parsed off the main thread, as the reader parses: turning a
            // newsletter's HTML into blocks is real work.
            let read = cx
                .background_executor()
                .spawn(async move {
                    let body = store.body(id).ok().flatten().unwrap_or_default();
                    let source = spool_core::html::effective_source(&body.plain, &body.html, spool_core::html::Source::Rich);
                    let blocks = spool_core::html::body_to_blocks(&body.plain, &body.html, source);
                    Read { body, blocks, source }
                })
                .await;
            this.update(cx, |this, cx| {
                let Some(peek) = this.peek.as_mut().filter(|p| p.exchange.envelope.id == id) else { return };
                peek.read = Some(Arc::new(read));
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// What an undo does to the book.
type Restore = Box<dyn FnOnce(&Book, i64) -> anyhow::Result<()> + Send>;

/// Whose a change is made to, decided before it leaves the UI thread.
enum Target {
    Client(i64),
    Person { addr: String, name: String, book: Option<String> },
}

/// One timeline out of the mail and the events, newest first.
pub(crate) fn timeline(history: &[Exchange], detail: &Detail) -> Vec<Item> {
    let mut items: Vec<(i64, Item)> = history
        .iter()
        .enumerate()
        .map(|(i, e)| (e.envelope.date, Item::Mail(i)))
        .chain(detail.events.iter().enumerate().map(|(i, e)| (e.ts, Item::Event(i))))
        .collect();
    items.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    items.into_iter().map(|(_, item)| item).collect()
}

impl Focusable for ClientsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ClientsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let main = match self.view {
            View::Directory => self.render_directory(&theme, window, cx),
            View::Board => self.render_board(&theme, window, cx),
        };
        #[cfg(target_os = "macos")]
        {
            self.web_view_drawn = false;
        }
        let profile = match (self.view, &self.selected) {
            (View::Directory, _) if self.picked.len() > 1 => Some(self.render_picked(&theme, window, cx)),
            (View::Directory, _) | (View::Board, Some(_)) => Some(self.render_profile(&theme, window, cx)),
            _ => None,
        };
        // Off screen whenever this frame has no message in it to show.
        #[cfg(target_os = "macos")]
        if !self.web_view_drawn
            && let Some(web_view) = &self.web_view
        {
            web_view.borrow_mut().hide();
        }

        div()
            .key_context("Clients")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::extend_next))
            .on_action(cx.listener(Self::extend_prev))
            .on_action(cx.listener(Self::show_directory))
            .on_action(cx.listener(Self::show_board))
            .on_action(cx.listener(Self::new_client))
            .on_action(cx.listener(Self::import_contacts))
            .on_action(cx.listener(Self::manage_sources))
            .on_action(cx.listener(Self::stage_left))
            .on_action(cx.listener(Self::stage_right))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::save_note_action))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .font_family(theme.font)
            .text_size(theme.text_size)
            .text_color(theme.text)
            .line_height(px(18.))
            .child(self.render_toolbar(&theme, window, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(&theme, cx))
                    .child(main)
                    .children(profile),
            )
            .child(self.render_status(&theme, cx))
            .children(self.render_sheet(&theme, window, cx))
            .children(self.render_sort_menu(&theme, cx))
    }
}

impl ClientsWindow {
    fn render_toolbar(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = !self.search_text.is_empty();
        let focused = self.search.read(cx).focus_handle.is_focused(window);
        let backfill = cx.try_global::<ClientBook>().and_then(|g| g.backfill);

        div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .bg(theme.panel)
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .p(px(2.))
                    .gap(px(2.))
                    .rounded_md()
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .child(segment("view-directory", "Directory", "\u{2318}1", self.view == View::Directory, theme).on_click(
                        cx.listener(|this, _, window, cx| this.set_view(View::Directory, window, cx)),
                    ))
                    .child(segment("view-board", "Board", "\u{2318}2", self.view == View::Board, theme).on_click(
                        cx.listener(|this, _, window, cx| this.set_view(View::Board, window, cx)),
                    )),
            )
            .child(
                div()
                    .id("clients-search")
                    .flex()
                    .items_center()
                    .gap_2()
                    .w(px(300.))
                    .flex_shrink()
                    .min_w(px(120.))
                    .overflow_hidden()
                    .px_2p5()
                    .py_1()
                    .rounded_md()
                    .bg(theme.background)
                    .border_1()
                    .border_color(if focused || searching { theme.focus_ring } else { theme.border })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(17.))
                            .line_height(px(16.))
                            .text_color(theme.text_faint)
                            .child("\u{2315}"),
                    )
                    .child(self.search.clone())
                    .children(searching.then(|| {
                        div()
                            .id("clients-clear-search")
                            .flex_shrink_0()
                            .cursor_pointer()
                            .text_color(theme.text_faint)
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search.update(cx, |input, cx| input.clear(cx));
                            }))
                            .child("\u{2715}")
                    })),
            )
            .child(div().flex_1())
            .children(backfill.map(|b| {
                div()
                    .id("backfill")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .tooltip(|_, cx| {
                        cx.new(|_| Tip {
                            text: "Reading who is on your mail, newest first. Older correspondence fills in as it goes.".into(),
                        })
                        .into()
                    })
                    .child(
                        div().w(px(60.)).h(px(4.)).rounded_full().bg(theme.border).child(
                            div().h_full().rounded_full().bg(theme.accent).w(px(60. * b.fraction())),
                        ),
                    )
                    .child(format!("Indexing mail {:.0}%", b.fraction() * 100.))
            }))
            .child(
                tool("clients-new", "+ New", "New client  \u{2318}N", theme)
                    .on_click(cx.listener(|this, _, window, cx| this.open_new_client(window, cx))),
            )
            .child(
                tool("clients-import", "Import\u{2026}", "Import contacts from a CSV, vCard or JSON file  \u{2318}\u{21e7}I", theme)
                    .on_click(cx.listener(|this, _, window, cx| this.open_import(window, cx))),
            )
            .child(
                tool("clients-sources", "Sources\u{2026}", "Pull new signups from your apps on a schedule", theme)
                    .on_click(cx.listener(|this, _, window, cx| this.open_sources(window, cx))),
            )
    }

    fn render_sidebar(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let counts = self.counts.clone();
        let on_board = self.snapshot.clients.iter().filter(|c| c.stage.is_some() && !c.hidden).count();

        let mut column = div()
            .id("clients-sidebar")
            .flex()
            .flex_col()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .h_full()
            .overflow_y_scroll()
            .px_2()
            .py_2()
            .gap(px(1.))
            .bg(theme.panel)
            .border_r_1()
            .border_color(theme.border);

        let section_row = |this: &mut Self, id: &'static str, glyph: &'static str, label: SharedString, count: usize, section: Section, cx: &mut Context<Self>| {
            let selected = this.view == View::Directory && this.section == section;
            nav_row(id, glyph, label, Some(count), selected, theme).on_click(cx.listener(move |this, _, window, cx| {
                this.section = section;
                this.set_view(View::Directory, window, cx);
                this.refilter(true, cx);
            }))
        };

        column = column
            .child(section_row(self, "nav-everyone", "\u{25CE}", "Everyone".into(), counts.everyone, Section::Everyone, cx))
            .child(section_row(self, "nav-clients", "\u{2605}", "Clients".into(), counts.clients, Section::Clients, cx))
            .child(section_row(self, "nav-reply", "\u{21A9}", "Needs reply".into(), counts.needs_reply, Section::NeedsReply, cx))
            .child(section_row(self, "nav-quiet", "\u{25CC}", "Gone quiet".into(), counts.gone_quiet, Section::GoneQuiet, cx))
            .child(
                nav_row("nav-board", "\u{25A6}", "Board".into(), Some(on_board), self.view == View::Board, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.set_view(View::Board, window, cx))),
            );

        if !self.snapshot.tags.is_empty() {
            column = column.child(nav_heading("Tags", theme));
            for tag in self.snapshot.tags.clone() {
                let count = counts.tags.get(&tag.id).copied().unwrap_or(0);
                let selected = self.view == View::Directory && self.section == Section::Tag(tag.id);
                let color = theme.avatar(tag.color as u32);
                column = column.child(
                    nav_row(("nav-tag", tag.id as usize), "", format!("#{}", tag.name).into(), Some(count), selected, theme)
                        .child(div().absolute().left(px(10.)).top(px(9.)).size(px(7.)).rounded_full().bg(color))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.section = Section::Tag(tag.id);
                            this.set_view(View::Directory, window, cx);
                            this.refilter(true, cx);
                        })),
                );
            }
        }

        if !self.accounts.is_empty() {
            column = column.child(nav_heading("Books", theme));
            column = column.child(
                nav_row("nav-book-all", "", "All books".into(), None, self.book_filter.is_none(), theme).on_click(
                    cx.listener(|this, _, _, cx| {
                        this.book_filter = None;
                        this.refilter(true, cx);
                    }),
                ),
            );
            let hidden = self.hidden_accounts();
            for account in self.accounts.clone().into_iter().filter(|a| !hidden.contains(&a.id)) {
                let email = address::normalize(&account.email);
                let selected = self.book_filter.as_ref().is_some_and(|b| b.email == email);
                let color = theme.account_color(account.color);
                let label: SharedString = account.email.clone().into();
                let group: SharedString = format!("book-{}", account.id.0).into();
                let hide = email.clone();
                column = column.child(
                    nav_row(("nav-book", account.id.0 as usize), "", label, None, selected, theme)
                        .group(group.clone())
                        .child(div().absolute().left(px(10.)).top(px(9.)).size(px(7.)).rounded_full().bg(color))
                        // Put away from where it is seen: shown only under
                        // the pointer, so the list is not a column of buttons.
                        .child(
                            div()
                                .id(("hide-book", account.id.0 as usize))
                                .absolute()
                                .right(px(6.))
                                .top(px(4.))
                                .px_1()
                                .rounded_sm()
                                .text_size(theme.small_text_size)
                                .text_color(gpui::transparent_black())
                                .group_hover(group, |s| s.text_color(theme.text_faint))
                                .hover(|s| s.bg(theme.hover).text_color(theme.text))
                                .tooltip(|_, cx| {
                                    cx.new(|_| Tip { text: "Don't use this book for clients. Its people leave the list; clients stay.".into() })
                                        .into()
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.hide_book(hide.clone(), cx);
                                }))
                                .child("Hide"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.book_filter = Some(BookFilter { email: email.clone(), account: Some(account.id) });
                            this.refilter(true, cx);
                        })),
                );
            }
            if !hidden.is_empty() {
                let label: SharedString = match hidden.len() {
                    1 => "1 book put away".into(),
                    n => format!("{n} books put away").into(),
                };
                column = column.child(
                    nav_row("nav-books-hidden", "", label, None, false, theme)
                        .text_color(theme.text_faint)
                        .tooltip(|_, cx| cx.new(|_| Tip { text: "Bring them back in Settings \u{2192} Extensions".into() }).into())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(workspace) = this.workspace.upgrade() {
                                crate::settings_window::open_at(workspace, crate::settings_ui::Section::Extensions, cx);
                            }
                        })),
                );
            }
        }

        column = column.child(div().flex_1().min_h(px(12.)));
        if counts.hidden > 0 || self.section == Section::Hidden {
            column = column.child(section_row(self, "nav-hidden", "\u{2205}", "Hidden".into(), counts.hidden, Section::Hidden, cx));
        }
        column
    }

    fn render_directory(&mut self, theme: &Theme, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let count = self.shown.len();
        let row_theme = theme.clone();
        let list_focused = self.list_focus.is_focused(window);
        let entries = self.entries.clone();

        let list = uniform_list(
            "clients",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                let now = now_seconds();
                range
                    .map(|row| match this.shown.get(row).and_then(|&i| entries.get(i)) {
                        Some(entry) => this.render_row(row, entry, list_focused, now, &row_theme, cx).into_any_element(),
                        None => div().h(px(ROW_HEIGHT)).into_any_element(),
                    })
                    .collect()
            }),
        )
        .track_scroll(self.list_scroll.clone())
        .flex_grow();

        let heading = self.list_heading();

        div()
            .flex()
            .flex_col()
            .w(px(LIST_WIDTH))
            .flex_shrink_0()
            .h_full()
            .border_r_1()
            .border_color(theme.border)
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
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(heading))
                    .child(
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(crate::perf::format_count(count)),
                    )
                    .child(div().flex_1())
                    // A filter, so it keeps its name and lights up while it is
                    // on. Saying "Everyone" when off read as the opposite of
                    // what it was about to do.
                    .when(self.section == Section::Everyone, |this| {
                        let two_way = self.two_way;
                        this.child(
                            chip("two-way", "Written to", two_way, theme)
                                .tooltip(move |_, cx| {
                                    cx.new(|_| Tip {
                                        text: if two_way {
                                            "Showing only people you have sent mail to. Click to include everyone who has written to you"
                                        } else {
                                            "Showing everyone who has written to you. Click for only people you have sent mail to"
                                        }
                                        .into(),
                                    })
                                    .into()
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.two_way = !this.two_way;
                                    this.refilter(true, cx);
                                })),
                        )
                    })
                    // The order, with a menu of the others: cycling through
                    // three on each click made it look like one more filter.
                    .child(
                        chip("sort", format!("{} \u{25be}", self.sort.label()), false, theme)
                            .tooltip(|_, cx| cx.new(|_| Tip { text: "Sort the list".into() }).into())
                            .on_click(cx.listener(|this, event: &gpui::ClickEvent, _, cx| {
                                this.sort_menu = Some(event.position());
                                cx.notify();
                            })),
                    )
                    // Whoever the list is showing — a tag, a book, a search —
                    // is the group: narrowing the list is how one is chosen.
                    .when(count > 0, |this| {
                        this.child(
                            chip("email-shown", "Email\u{2026}", false, theme)
                                .tooltip(|_, cx| {
                                    cx.new(|_| Tip { text: "Write to everyone in this list, each on their own".into() }).into()
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.email_shown(cx))),
                        )
                    }),
            )
            .children(self.merge_into.map(|_| {
                div()
                    .px_3()
                    .py_1p5()
                    .bg(theme.selected)
                    .text_size(theme.small_text_size)
                    .text_color(theme.text)
                    .child("Choose the client to fold into this one \u{2014} escape to cancel")
            }))
            .child(
                div()
                    .key_context("ClientList")
                    .track_focus(&self.list_focus)
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, _, window, _| window.focus(&this.list_focus)),
                    )
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .when(count == 0, |this| this.child(self.render_empty_directory(theme)))
                    .when(count > 0, |this| this.child(list)),
            )
            .into_any_element()
    }

    /// What the list is showing, as its header names it.
    fn list_heading(&self) -> SharedString {
        match self.section {
            Section::Everyone => "Everyone".into(),
            Section::Clients => "Clients".into(),
            Section::NeedsReply => "Needs reply".into(),
            Section::GoneQuiet => "Gone quiet".into(),
            Section::Hidden => "Hidden".into(),
            Section::Tag(tag) => self
                .snapshot
                .tags
                .iter()
                .find(|t| t.id == tag)
                .map(|t| format!("#{}", t.name))
                .unwrap_or_default()
                .into(),
        }
    }

    /// Writes to everyone the list is showing.
    fn email_shown(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<Entry> = self.shown.iter().filter_map(|&i| self.entries.get(i).cloned()).collect();
        let mut group = self.list_heading().to_string();
        if let Some(book) = &self.book_filter {
            group.push_str(&format!(" in {}", book.email));
        }
        if !self.search_text.trim().is_empty() {
            group.push_str(&format!(" matching \u{201c}{}\u{201d}", self.search_text.trim()));
        }
        self.open_mail(group, entries, cx);
    }

    fn render_empty_directory(&self, theme: &Theme) -> impl IntoElement {
        let backfilling = !self.loaded_once;
        let (title, body): (&str, &str) = match (backfilling, self.section, self.search_text.is_empty()) {
            (true, _, _) => ("Reading your mail\u{2026}", "Everyone you correspond with will be here in a moment."),
            (_, _, false) => ("Nobody matches", "Try fewer words, or a #tag."),
            (_, Section::Clients, _) => (
                "No clients yet",
                "Anybody becomes a client the moment you give them a stage, a tag or a note \u{2014} or import a list.",
            ),
            (_, Section::NeedsReply, _) => ("Nobody is waiting on you", "Mail from people you write to lands here until you answer."),
            (_, Section::GoneQuiet, _) => ("Nothing has gone quiet", "Clients on the board with no mail either way for two weeks land here."),
            (_, Section::Hidden, _) => ("Nobody is hidden", ""),
            _ => ("Nobody here yet", "People appear as mail is read."),
        };
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .flex_1()
            .gap_1()
            .px_6()
            .text_center()
            .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text_muted).child(title))
            .child(div().text_size(theme.small_text_size).text_color(theme.text_faint).child(body))
    }

    fn render_row(
        &mut self,
        row: usize,
        entry: &Entry,
        list_focused: bool,
        now: i64,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let selected = match self.picked.is_empty() {
            true => self.selected.as_ref() == Some(&entry.key),
            false => self.picked.contains(&entry.key),
        };
        let open = entry.stage().is_some_and(|s| self.snapshot.stages.iter().any(|st| st.id == s && st.kind == StageKind::Open));
        let since = entry.client.as_ref().and_then(|c| c.stage_since);
        let signal = clients::signal(&entry.stats, open, since, entry.is_client(), now);
        let stage = entry.stage().and_then(|s| self.stage_badge(s, theme));

        let second: SharedString = match (entry.company.is_empty(), entry.detail.is_empty()) {
            (false, false) if entry.detail.starts_with('@') => entry.detail.clone(),
            (false, false) => format!("{} \u{00b7} {}", entry.company, entry.detail).into(),
            (true, _) => entry.detail.clone(),
            (false, true) => entry.company.clone(),
        };

        let when = entry.stats.last.map(|last| {
            let arrow = match (entry.stats.last_in, entry.stats.last_out) {
                (Some(i), Some(o)) if o >= i => "\u{2192}",
                (Some(_), _) => "\u{2190}",
                (None, Some(_)) => "\u{2192}",
                _ => "",
            };
            format!("{} {arrow}", clients::ago(now - last))
        });
        let tags: Vec<Hsla> = entry
            .client
            .as_ref()
            .map(|c| {
                c.tags
                    .iter()
                    .filter_map(|t| self.snapshot.tags.iter().find(|tag| tag.id == *t))
                    .map(|t| theme.avatar(t.color as u32))
                    .take(4)
                    .collect()
            })
            .unwrap_or_default();
        let when = match signal {
            Signal::NeedsReply(_) => div().text_color(theme.yellow).child(when.unwrap_or_default()),
            Signal::GoneQuiet(_) => div().text_color(theme.red).child(when.unwrap_or_else(|| "quiet".into())),
            Signal::Nothing => div().text_color(theme.text_faint).child(when.unwrap_or_default()),
        };
        let dots = div().flex().gap_0p5().children(tags.into_iter().map(|c| div().size(px(6.)).rounded_full().bg(c)));
        // Sorted by mail, the count is what the order is by, so it is what the
        // row says first, and when moves down beside the tags. Otherwise the
        // time is both the order and the first thing read.
        let after = if self.sort == Sort::Volume {
            let count = match entry.stats.messages {
                1 => "1 message".to_string(),
                n => format!("{} messages", crate::perf::format_count(n as usize)),
            };
            [
                div().whitespace_nowrap().text_color(theme.text_muted).child(count),
                div().flex().items_center().gap_1().child(dots).child(when),
            ]
        } else {
            [when, dots]
        };

        div()
            .id(("client-row", row))
            .flex()
            .items_center()
            .gap_2p5()
            // Definite, not flexed: gpui ellipsizes only text whose width is
            // known when it is measured, and a row in a uniform list is as
            // wide as its contents unless told otherwise.
            .w(px(LIST_WIDTH - 1.))
            .h(px(ROW_HEIGHT))
            .px_3()
            .cursor_pointer()
            .border_b_1()
            .border_color(theme.border_subtle)
            .when(selected && list_focused, |s| s.bg(theme.selected))
            .when(selected && !list_focused, |s| s.bg(theme.selected_inactive))
            .when(!selected, |s| s.hover(|s| s.bg(theme.hover)))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                window.focus(&this.list_focus);
                this.click_row(row, event.modifiers(), cx);
            }))
            .child(avatar(&entry.name, &entry.detail, 30., theme))
            .child(
                div()
                    .flex()
                    .flex_col()
                    // The row less its padding, the avatar, the gaps and the
                    // column on the right — definite, like the row, so a long
                    // name ends in an ellipsis instead of measuring as nothing.
                    .w(px(LIST_WIDTH - 1. - 24. - 30. - 20. - ROW_AFTER))
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .min_w_0()
                            .child(
                                // Unwrapped, not clamped: an item that shrinks
                                // in a row measures as nothing while it may wrap.
                                div()
                                    .flex_shrink()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .when(entry.is_client(), |s| s.font_weight(FontWeight::SEMIBOLD))
                                    .child(entry.name.clone()),
                            )
                            .children(stage),
                    )
                    .child(
                        div()
                            .overflow_hidden()
                            .line_clamp(1)
                            .text_ellipsis()
                            .text_size(theme.small_text_size)
                            .text_color(theme.text_faint)
                            .child(second),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_0p5()
                    .w(px(ROW_AFTER))
                    .flex_shrink_0()
                    .text_size(theme.small_text_size)
                    .children(after),
            )
    }

    /// The sort menu: the three orders, the one in use ticked.
    ///
    /// Drawn like the folder menu — a scrim over the window that the next
    /// click dismisses, and the menu anchored where the chip was clicked.
    fn render_sort_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let at = self.sort_menu?;
        let items = Sort::ALL.into_iter().map(|sort| {
            let on = sort == self.sort;
            div()
                .id(sort.label())
                .flex()
                .items_center()
                .gap_2()
                .pl_2()
                .pr_4()
                .py_1()
                .cursor_pointer()
                .text_color(theme.text)
                .hover(|s| s.bg(theme.hover))
                .child(div().w(px(12.)).text_color(theme.accent).child(if on { "\u{2713}" } else { "" }))
                .child(sort.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.sort_menu = None;
                    if this.sort != sort {
                        this.sort = sort;
                        this.refilter(true, cx);
                    }
                    cx.notify();
                }))
        });

        Some(
            div()
                .id("sort-menu-scrim")
                .absolute()
                .inset_0()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        this.sort_menu = None;
                        cx.notify();
                        cx.stop_propagation();
                    }),
                )
                .child(
                    anchored().position(at).child(
                        div()
                            .flex()
                            .flex_col()
                            .py_1()
                            .min_w(px(140.))
                            .rounded_md()
                            .bg(theme.elevated)
                            .border_1()
                            .border_color(theme.border)
                            .shadow_lg()
                            .text_size(theme.text_size)
                            // Or the scrim takes the press and the item never
                            // sees its click.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .children(items),
                    ),
                )
                .into_any_element(),
        )
    }

    /// A stage's name as a small pill in its column's colour.
    pub(crate) fn stage_badge(&self, stage: i64, theme: &Theme) -> Option<Div> {
        let (index, stage) = clients::stage_index(&self.snapshot, stage)?;
        let color = stage_color(stage.kind, index, theme);
        Some(pill(stage.name.clone(), color, theme))
    }

    fn render_status(&mut self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let pulling = cx.try_global::<ClientBook>().map(|g| g.pulling.len()).unwrap_or(0);
        // A sending carries on with its sheet put away; this is where it
        // still says how far it has got, and a click brings the sheet back.
        let mailing = cx.try_global::<ClientBook>().and_then(|g| g.mailing.as_ref()).map(|m| m.summary());
        div()
            .flex()
            .items_center()
            .gap_3()
            .h(px(24.))
            .flex_shrink_0()
            .px_3()
            .bg(theme.panel)
            .border_t_1()
            .border_color(theme.border)
            .text_size(theme.small_text_size)
            .text_color(theme.text_faint)
            .child(div().flex_1().overflow_hidden().line_clamp(1).text_ellipsis().child(self.status.clone()))
            .when(pulling > 0, |s| s.child(format!("Pulling {pulling} source{}\u{2026}", if pulling == 1 { "" } else { "s" })))
            .children(mailing.filter(|_| self.sheet.is_none()).map(|summary| {
                div()
                    .id("clients-mailing")
                    .cursor_pointer()
                    .text_color(theme.text_muted)
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sheet = Some(Sheet::Mailing);
                        cx.notify();
                    }))
                    .child(summary)
            }))
            .when(!self.undo.is_empty(), |s| {
                s.child(
                    div()
                        .id("clients-undo")
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.text))
                        .on_click(cx.listener(|this, _, window, cx| this.undo(&UndoAction, window, cx)))
                        .child("Undo \u{2318}Z"),
                )
            })
    }
}

// Shared drawing.

/// The colour a stage is drawn in: won is green, lost is grey, and the open
/// ones walk the avatar palette so neighbouring columns differ.
pub(crate) fn stage_color(kind: StageKind, index: usize, theme: &Theme) -> Hsla {
    match kind {
        StageKind::Won => theme.green,
        StageKind::Lost => theme.text_faint,
        StageKind::Open => theme.avatar(index as u32 + 2),
    }
}

pub(crate) fn pill(label: impl Into<SharedString>, color: Hsla, _theme: &Theme) -> Div {
    div()
        .flex_shrink_0()
        .px_1p5()
        .rounded(px(4.))
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(px(10.5))
        .line_height(px(16.))
        .font_weight(FontWeight::MEDIUM)
        .child(label.into())
}

/// A circle with somebody's initials, coloured by who they are.
pub(crate) fn avatar(name: &str, seed: &str, size: f32, theme: &Theme) -> Div {
    let mut hash: u32 = 2_166_136_261;
    for byte in seed.bytes() {
        hash ^= byte.to_ascii_lowercase() as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .size(px(size))
        .rounded_full()
        .bg(theme.avatar(hash))
        .text_color(theme.text_on_accent)
        .text_size(px(size * 0.38))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initials(name))
}

/// Two letters for somebody: their first and last names', or the first two of
/// a single word.
pub(crate) fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| w.chars().next().is_some_and(char::is_alphanumeric)).collect();
    let letter = |w: &str| w.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
    match words.as_slice() {
        [] => "?".into(),
        [only] => only.chars().take(2).collect::<String>().to_uppercase(),
        [first, .., last] => format!("{}{}", letter(first), letter(last)),
    }
}

fn segment(id: &'static str, label: &'static str, keys: &'static str, on: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .px_2p5()
        .py_0p5()
        .rounded(px(4.))
        .cursor_pointer()
        .text_size(theme.small_text_size)
        .when(on, |s| s.bg(theme.selected).text_color(theme.text))
        .when(!on, |s| s.text_color(theme.text_muted).hover(|s| s.text_color(theme.text)))
        .tooltip(move |_, cx| cx.new(|_| Tip { text: keys.into() }).into())
        .child(label)
}

pub(crate) fn tool(id: &'static str, label: &'static str, tip: &'static str, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_2p5()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .border_color(theme.border)
        .text_size(theme.small_text_size)
        .text_color(theme.text_muted)
        .hover(|s| s.bg(theme.hover).text_color(theme.text))
        .tooltip(move |_, cx| cx.new(|_| Tip { text: tip.into() }).into())
        .child(label)
}

pub(crate) fn chip(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, on: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_2()
        .py_0p5()
        .rounded_md()
        .cursor_pointer()
        .border_1()
        .text_size(theme.small_text_size)
        .when(on, |s| s.border_color(theme.accent).bg(theme.selected).text_color(theme.text))
        .when(!on, |s| s.border_color(theme.border).text_color(theme.text_muted).hover(|s| s.bg(theme.hover).text_color(theme.text)))
        .child(label.into())
}

fn nav_heading(label: &'static str, theme: &Theme) -> Div {
    div()
        .px_2()
        .pt_4()
        .pb_1()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_faint)
        .child(label.to_uppercase())
}

fn nav_row(
    id: impl Into<gpui::ElementId>,
    glyph: &'static str,
    label: SharedString,
    count: Option<usize>,
    selected: bool,
    theme: &Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .gap_2()
        .h(px(26.))
        .pl(px(26.))
        .pr_2()
        .rounded_md()
        .cursor_pointer()
        .when(selected, |s| s.bg(theme.selected).text_color(theme.text))
        .when(!selected, |s| s.text_color(theme.text_muted).hover(|s| s.bg(theme.hover).text_color(theme.text)))
        .child(div().absolute().left(px(8.)).w(px(12.)).text_color(theme.text_faint).child(glyph))
        .child(div().flex_1().min_w_0().overflow_hidden().line_clamp(1).text_ellipsis().child(label))
        .children(count.filter(|c| *c > 0).map(|c| {
            div().text_size(theme.small_text_size).text_color(theme.text_faint).child(crate::perf::format_count(c))
        }))
}

// The window.

/// Opens Clients, or brings it forward — on somebody, when there is one.
///
/// A free function taking the workspace, like `settings_window::open`: opening
/// a window draws it once, and drawing this one reads the workspace.
pub fn open(workspace: Entity<Workspace>, focus: Option<Address>, board: bool, cx: &mut App) {
    if !cx.has_global::<ClientBook>() {
        workspace.update(cx, |workspace, cx| {
            workspace.status = "The client book could not be opened; see the log".into();
            cx.notify();
        });
        return;
    }

    let existing = cx.global::<ClientBook>().window;
    if let Some(handle) = existing
        && handle
            .update(cx, |this, window, cx| {
                window.activate_window();
                this.arrive(focus.clone(), board, window, cx);
            })
            .is_ok()
    {
        return;
    }

    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Clients".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1240.), px(780.)), cx))),
        window_min_size: Some(size(px(900.), px(520.))),
        ..Default::default()
    };

    let weak = workspace.downgrade();
    let opened = cx.open_window(options, move |window, cx| {
        cx.new(|cx| {
            window.set_rem_size(px(16.));
            ClientsWindow::new(weak, window, cx)
        })
    });

    match opened {
        Ok(handle) => {
            handle
                .update(cx, |this, window, cx| {
                    window.on_window_should_close(cx, |_, cx| {
                        forget_window(cx);
                        true
                    });
                    this.arrive(focus, board, window, cx);
                })
                .ok();
            cx.update_global::<ClientBook, _>(|clients, _| clients.window = Some(handle));
        }
        Err(e) => workspace.update(cx, |workspace, cx| {
            workspace.status = format!("Could not open Clients: {e}");
            cx.notify();
        }),
    }
}

fn forget_window(cx: &mut App) {
    if cx.has_global::<ClientBook>() {
        cx.update_global::<ClientBook, _>(|clients, _| clients.window = None);
    }
}

/// Brings the mailbox forward.
fn activate_mailbox(cx: &mut App) {
    for window in cx.windows() {
        if let Some(mailbox) = window.downcast::<Workspace>() {
            mailbox.update(cx, |_, window, _| window.activate_window()).ok();
            return;
        }
    }
}

impl ClientsWindow {
    /// What arriving at the window means: on the sender of the message that
    /// was open, or on the board.
    fn arrive(&mut self, focus: Option<Address>, board: bool, window: &mut Window, cx: &mut Context<Self>) {
        if board {
            self.set_view(View::Board, window, cx);
        }
        let Some(focus) = focus else { return };
        let addr = address::normalize(&focus.addr);
        let client = self.snapshot.clients.iter().find(|c| c.addrs.contains(&addr)).map(|c| c.id).or_else(|| {
            let domain = addr.rsplit_once('@')?.1;
            self.snapshot.clients.iter().find(|c| c.domains.iter().any(|d| d == domain)).map(|c| c.id)
        });
        let key = match client {
            Some(id) => Key::Client(id),
            None => Key::Person(addr.clone().into()),
        };
        if !board {
            self.set_view(View::Directory, window, cx);
        }
        // Somebody not in the list as it is filtered is shown anyway: arriving
        // on a person is a request to see that person.
        self.section = Section::Everyone;
        self.book_filter = None;
        self.search.update(cx, |input, cx| input.clear(cx));
        self.search_text.clear();
        if self.selected.as_ref() != Some(&key) {
            self.peek = None;
            self.edit = None;
        }
        self.selected = Some(key);
        self.reveal = true;
        self.refilter(false, cx);
        self.reveal_selected(cx);
        self.load_profile(true, cx);
    }

    /// Puts the selected row in the middle of the list, chosen.
    ///
    /// Somebody you have never written to is not in the list "Written to"
    /// shows, so arriving on them widens it to everyone rather than landing
    /// on a list without them in it. Before the list has loaded there is no
    /// row to scroll to; the load calls this again when there is.
    fn reveal_selected(&mut self, cx: &mut Context<Self>) {
        if !self.loaded_once {
            return;
        }
        self.reveal = false;
        let Some(key) = self.selected.clone() else { return };
        let Some(&index) = self.index_of.get(&key) else {
            if let Key::Person(addr) = &key {
                self.status = format!("{addr} is not in the people index yet").into();
            }
            return;
        };
        if self.two_way && !self.shown.contains(&index) {
            self.two_way = false;
            self.refilter(false, cx);
        }
        if let Some(row) = self.shown.iter().position(|&i| i == index) {
            self.cursor = row;
            self.list_scroll.scroll_to_item(row, gpui::ScrollStrategy::Center);
        }
        cx.notify();
    }
}

impl Workspace {
    /// Shows a message in the mailbox: its folder, with it chosen.
    ///
    /// The search and the filter are cleared first, because either could be
    /// what hides it, and a jump that lands on a list without the message in
    /// it is a jump that did nothing.
    pub(crate) fn reveal_message(&mut self, envelope: &Envelope, cx: &mut Context<Self>) {
        if !self.search_text.is_empty() {
            self.search.update(cx, |input, cx| input.clear(cx));
        }
        self.filter = crate::workspace::Filter::All;
        self.select_mailbox(envelope.mailbox_id, cx);
        self.chosen = Some(envelope.id);
        self.reload(cx);
    }

    /// Who the message being read is about, for arriving at Clients on them:
    /// the sender, or for mail you sent, whoever it went to.
    pub(crate) fn selected_person(&self) -> Option<Address> {
        let envelope = self.selected_envelope()?;
        let to = self
            .open
            .as_ref()
            .filter(|open| open.envelope.id == envelope.id)
            .map(|open| open.body.to.as_slice())
            .unwrap_or_default();
        person_on(&envelope.from, to, self.is_mine(envelope))
    }

    /// Whether you sent a message: from one of your addresses, or filed where
    /// your sent mail goes — which is how an alias is caught.
    fn is_mine(&self, envelope: &Envelope) -> bool {
        self.accounts.iter().any(|a| a.email.eq_ignore_ascii_case(&envelope.from.addr))
            || self
                .mailboxes
                .iter()
                .any(|m| m.id == envelope.mailbox_id && m.role == spool_core::MailboxRole::Sent)
    }

    /// Clients as it was left, from the toolbar's button or the sidebar's
    /// row. Neither is about a message, so it arrives on nobody: ⌘⇧C is the
    /// way in on the sender.
    pub(crate) fn open_clients(&mut self, cx: &mut Context<Self>) {
        let workspace = cx.entity();
        // After this update: opening Clients reads the workspace.
        cx.defer(move |cx| open(workspace, None, false, cx));
    }

    /// Adds Clients, or takes it away again, from Settings → Extensions.
    ///
    /// At once rather than at the next launch, unlike an extension's switch:
    /// it is built in, so there is nothing to compile, and somebody who has
    /// just added it wants to look at it.
    pub(crate) fn set_clients_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.clients == on {
            return;
        }
        self.settings.clients = on;
        self.persist_settings(cx);
        // Before anything else, so no message lands between the switch and
        // the backfill that starts from it.
        if let Err(e) = self.store.set_people_index(on) {
            log::warn!("switching the people index: {e:#}");
        }
        match on {
            true => clients::init(self.store.clone(), cx),
            false => clients::shut_down(cx),
        }
        crate::menus::refresh(on, cx);
        self.status = match on {
            true => "Clients added \u{2014} Window \u{2192} Clients, or \u{2318}\u{21e7}C on a message".into(),
            false => "Clients removed; the book is kept for if you add it back".into(),
        };
        cx.notify();
    }

    /// The way into Clients from a message: whoever it is about, with their
    /// place in the sale beside the mark when they have one.
    ///
    /// On every message once Clients is added, client or not — "who is this,
    /// and what have we said" is a question about anybody — but quiet unless
    /// they are a client, so it is not one more thing shouting in the header.
    pub(crate) fn render_client_chip(
        &self,
        envelope: &Envelope,
        body: &spool_core::Body,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.settings.clients {
            return None;
        }
        let person = person_on(&envelope.from, &body.to, self.is_mine(envelope))?;
        let badge = clients::badge_for(&person.addr, cx);
        let (label, color): (Option<SharedString>, Hsla) = match badge.as_ref().map(|b| (&b.stage, b.closed)) {
            Some((Some(stage), false)) => (Some(stage.clone()), theme.accent),
            Some((Some(stage), true)) => (Some(stage.clone()), theme.text_faint),
            Some((None, _)) => (Some("Client".into()), theme.text_faint),
            None => (None, theme.text_faint),
        };
        let who = match person.name.trim() {
            "" => person.addr.clone(),
            name => name.to_string(),
        };
        let tip: SharedString = format!("Open {who} in Clients").into();
        let is_client = label.is_some();

        Some(
            div()
                .id(("client-chip", envelope.id.0 as usize))
                .flex()
                .items_center()
                .gap_1()
                .px_1()
                .h(px(20.))
                .rounded(px(4.))
                .cursor_pointer()
                .text_color(color)
                .text_size(theme.small_text_size)
                .when(is_client, |s| s.px_1p5().bg(color.opacity(0.14)).hover(|s| s.bg(color.opacity(0.24))))
                .when(!is_client, |s| s.hover(|s| s.bg(theme.hover).text_color(theme.text)))
                .tooltip(move |_, cx| cx.new(|_| Tip { text: tip.clone() }).into())
                .on_click(cx.listener(move |_, _, _, cx| {
                    let workspace = cx.entity();
                    let person = person.clone();
                    // After this update: opening Clients reads the workspace.
                    cx.defer(move |cx| open(workspace, Some(person), false, cx));
                }))
                .child(gpui::svg().path(crate::icons::PERSON).size(px(13.)).flex_shrink_0().text_color(color))
                .children(label)
                .into_any_element(),
        )
    }
}

/// Who a message is about: whoever sent it — or, for one you sent, the first
/// person it went to.
pub(crate) fn person_on(from: &Address, to: &[Address], mine: bool) -> Option<Address> {
    match mine {
        false => Some(from.clone()).filter(|a| !a.addr.trim().is_empty()),
        true => to.first().cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_about_its_sender_or_for_your_own_whoever_it_went_to() {
        let ana = Address::new("Ana Ruiz", "ana@acme.com");
        let me = Address::new("Wes", "wes@example.com");
        let bo = Address::new("Bo", "bo@acme.com");

        assert_eq!(person_on(&ana, std::slice::from_ref(&me), false), Some(ana.clone()));
        assert_eq!(person_on(&me, &[bo.clone(), ana.clone()], true), Some(bo), "the first it went to");
        assert_eq!(person_on(&me, &[], true), None, "a draft to nobody is about nobody");
        assert_eq!(person_on(&Address::bare(""), &[], false), None);
    }

    #[test]
    fn initials_are_first_and_last() {
        assert_eq!(initials("Ana Ruiz"), "AR");
        assert_eq!(initials("Ana María Ruiz"), "AR");
        assert_eq!(initials("acme"), "AC");
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("\u{2018}Spotify\u{2019} team"), "TE", "a word opening on punctuation is not a name");
    }
}
