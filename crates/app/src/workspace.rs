//! The root view: all application state, and the actions that change it.
//!
//! There is one entity for the whole window rather than an entity per pane.
//! Panes here are pure functions of this state (see the sibling modules), which
//! means there is exactly one place a message can be selected, one place a
//! query is issued, and no cross-pane synchronization to get wrong.
//!
//! The rule that keeps it fast: **`render` never touches the database.** Every
//! query runs on a background thread and writes its result back into this
//! struct, so painting only ever walks data that is already in memory.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Subscription, Task,
    UniformListScrollHandle, Window, WindowBounds, div, prelude::*, px,
};
use spool_core::{Account, Envelope, Flags, Mailbox, MailboxId, MailboxRole, MessageId, html};
use anyhow::Context as _;
use spool_net::{Config, Progress, SyncMode};
use spool_store::{Query, Scope, Store};

use crate::{
    activity::{Activities, ActivityKind},
    actions::*,
    folder_ui::{FolderAction, FolderPrompt},
    images::{Embedded, ImageStore, Remote},
    menus::CloseWindow,
    palette::{Destination, PaletteMode, PaletteState},
    perf::Perf,
    query_worker::QueryWorker,
    settings::{Settings, WindowState},
    settings_ui::SettingsState,
    text_input::{InputChanged, TextInput},
    theme::{ActiveTheme, Theme, ThemeAccess, ThemeId},
    update::{Stage, UpdateState},
};

/// How many envelopes are pulled into memory for the list.
///
/// The list is virtualized, so this bounds memory rather than frame time: at
/// roughly 200 bytes per envelope, 200k rows is about 40 MB, and scrolling
/// through all of them costs the same as scrolling through ten.
pub(crate) const LIST_LIMIT: usize = 200_000;

/// How often a copy left open asks whether there is a newer one.
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// What the search box says it will search.
const MAIL_SEARCH_PROMPT: &str = "Search all mail \u{2014} try from: or is:unread";

/// What the search box says when the window is too narrow for the sentence.
const SHORT_SEARCH_PROMPT: &str = "Search";

/// How long a message has to stay in the reader before it counts as read.
///
/// Long enough that cursoring through a list does not mark everything you
/// passed, short enough that actually reading something always beats it. Half
/// a second is about as fast as a held arrow key repeats, which is the case
/// this exists for: stopping on a message is deliberate, scrolling past one is
/// not.
const MARK_READ_AFTER: Duration = Duration::from_millis(500);

/// How many older messages one press of "load older" fetches.
///
/// A batch rather than the rest of the folder: an archive of forty thousand is
/// a download somebody should be able to stop after the first thousand, and
/// pressing again is a cheaper way to say "keep going" than a progress bar
/// with a cancel button.
const BACKFILL_BATCH: usize = 1_000;

/// How many filing operations can be taken back.
///
/// Deep enough that undo is never the thing that ran out, shallow enough that
/// the stack is not a second copy of the mailbox.
const UNDO_DEPTH: usize = 32;

/// What a drag out of the message list carries.
///
/// The ids are behind an `Rc` because the payload is built during render, once
/// per visible row: with the whole mailbox selected, cloning the list into
/// thirty rows a frame would be the one thing in the list that scales with the
/// mailbox rather than with the screen.
#[derive(Debug, Clone)]
pub struct DraggedMessages {
    pub ids: std::rc::Rc<Vec<MessageId>>,
    /// What the thing under the cursor says.
    pub label: SharedString,
}

/// An account being dragged into a new place in the sidebar.
///
/// Carries the address because that is what the order is written down as: an
/// account id is a local number that a rebuilt store hands out again in a
/// different order, and an arrangement that quietly permuted itself would be
/// worse than none.
#[derive(Debug, Clone)]
pub struct DraggedAccount {
    pub email: SharedString,
    pub label: SharedString,
}

/// One reversible filing operation: where each message was before it moved.
///
/// Only moves are recorded. Deleting out of the Trash removes the row and its
/// body from the store, and no stack can put those back \u{2014} an undo that
/// silently restores nothing would be worse than no undo at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undoable {
    /// What was moved, phrased for the status bar: a quoted subject, or
    /// `3 messages`.
    pub summary: String,
    /// Each message with the mailbox it came from.
    pub moves: Vec<(MessageId, MailboxId)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Unread,
    Flagged,
}

impl Filter {
    pub fn label(self) -> &'static str {
        match self {
            Filter::All => "All",
            Filter::Unread => "Unread",
            Filter::Flagged => "Flagged",
        }
    }

    fn next(self) -> Self {
        match self {
            Filter::All => Filter::Unread,
            Filter::Unread => Filter::Flagged,
            Filter::Flagged => Filter::All,
        }
    }
}

/// Which way the list is ordered by date.
///
/// Only by date: a mailbox is a timeline, and the columns that are not the
/// date — sender, subject — sort into orders nobody reads mail in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// Newest first. What a mail client means by "the top of the list".
    Newest,
    Oldest,
}

impl SortOrder {
    /// The header chip's label: the column, and the way it currently runs.
    pub fn label(self) -> &'static str {
        match self {
            SortOrder::Newest => "Date \u{2193}",
            SortOrder::Oldest => "Date \u{2191}",
        }
    }

    fn flipped(self) -> Self {
        match self {
            SortOrder::Newest => SortOrder::Oldest,
            SortOrder::Oldest => SortOrder::Newest,
        }
    }
}

/// Which divider is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divider {
    /// Between the sidebar and the message list.
    Sidebar,
    /// Between the message list and the reader.
    List,
}

/// A drag in progress.
#[derive(Debug, Clone, Copy)]
pub struct Drag {
    pub divider: Divider,
    /// Pointer x when the drag began.
    start_x: gpui::Pixels,
    /// The pane's width when the drag began, so the pane follows the pointer
    /// exactly rather than drifting by the offset within the divider.
    start_width: f32,
}

/// A message opened in the reader, with its body already parsed into blocks so
/// that scrolling it does no work beyond painting.
pub struct OpenMessage {
    pub envelope: Envelope,
    pub body: spool_core::Body,
    pub blocks: Vec<html::Block>,
    /// Which of the two bodies these blocks came from.
    pub source: html::Source,
    /// Images that arrived with the message, decoded once when it opened.
    pub embedded: Embedded,
    /// Indices of attachments whose bytes are in the store, so the reader can
    /// offer to open the ones it can and say why about the ones it cannot.
    pub stored_attachments: Vec<usize>,
}

/// One attachment being fetched.
pub(crate) struct AttachmentLoad {
    /// Which claim this is. A preview releases its chip when the download
    /// lands rather than when the panel closes, so the same chip can be
    /// claimed again while the first task is still running — and that task
    /// must not clear somebody else's claim on its way out.
    claim: u64,
    /// How much of the message has arrived, 0 to 1.
    pub fraction: f32,
}

/// What just happened to one attachment, for its own chip to say.
pub(crate) struct AttachmentNote {
    pub message: MessageId,
    pub index: usize,
    pub text: String,
    pub tone: NoteTone,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum NoteTone {
    /// Under way. Acknowledges the click while the bytes are still moving.
    Working,
    Done,
    Failed,
}

impl OpenMessage {
    /// Parses a body into what the reader draws.
    ///
    /// Deliberately free of anything to do with the window, so it can be done
    /// on a background thread: `body_to_blocks` on 60KB of marketing HTML is
    /// real work, and doing it on the main thread meant the keystroke that
    /// arrived during it — very often the one deleting the message — waited
    /// for a message the person had already decided they did not want.
    fn new(
        envelope: Envelope,
        body: spool_core::Body,
        requested: html::Source,
        stored_attachments: Vec<usize>,
    ) -> Self {
        // What was actually used, not what was asked for: the reader labels
        // this, and a message with only one body ignores the preference.
        let source = html::effective_source(&body.plain, &body.html, requested);
        let blocks = html::body_to_blocks(&body.plain, &body.html, source);
        let embedded = Embedded::build(&body, &blocks);
        Self { envelope, body, blocks, source, embedded, stored_attachments }
    }

    /// Whether offering the other body would show anything different.
    pub fn has_both_bodies(&self) -> bool {
        html::has_both(&self.body.plain, &self.body.html)
    }

    /// Remote images this message wants, which is what the load bar counts.
    pub fn remote_images(&self) -> Vec<String> {
        crate::images::remote_urls(&self.blocks)
    }
}

pub struct Workspace {
    pub store: Store,
    focus_handle: FocusHandle,
    pub list_focus: FocusHandle,
    /// The sidebar's, so it can take the keyboard; see `sidebar_nav`.
    pub(crate) sidebar_focus: FocusHandle,
    /// The sidebar row the keyboard last moved to or clicked. `None` follows
    /// whatever the list is showing.
    pub(crate) sidebar_cursor: Option<crate::sidebar_nav::RowKey>,

    pub accounts: Vec<Account>,
    pub mailboxes: Vec<Mailbox>,
    pub scope: Scope,
    /// The folders a sync is inside right now, as (account address, path).    /// The folders a sync is inside right now, as (account address, path).
    ///
    /// A set rather than one folder: several connections work at once, so
    /// "what is syncing" is a handful of rows rather than a position in a
    /// list. The sidebar shows a spinner where each one's count would be.
    pub(crate) syncing_folders: std::collections::HashSet<(String, String)>,
    /// The queue behind the folders being worked on, for the bar in the
    /// footer. `syncing_folders` is the middle of it; this is the two ends.
    pub(crate) sync_flow: crate::sync_flow::SyncFlow,
    /// When the cursor last moved to a different message.
    ///
    /// Only used to tell a deliberate opening from one the cursor is passing
    /// through on its way somewhere else.
    selection_moved_at: Instant,
    /// A transient message and when it was set, for the toast.
    ///
    /// `status` alone was not enough to tell anyone anything: the status bar
    /// is off by default and is the only place it is drawn, so every message
    /// the app produced about an action it had just taken went into a field
    /// nothing rendered.
    toast: Option<(SharedString, Instant)>,
    /// Takes the toast back down.
    toast_task: Option<Task<()>>,
    /// Attachments being fetched right now, and how far along.
    ///
    /// A chip in here is not clickable. A 16 MB fetch takes seconds, and a
    /// chip that still answers during it invites the second press that starts
    /// a second download of the same file.
    pub(crate) attachment_progress: HashMap<(MessageId, usize), AttachmentLoad>,
    /// An attachment chip the mouse went down on, and where: the start of
    /// what may become a drag out to Finder.
    pub(crate) attachment_press: Option<AttachmentPress>,
    /// Numbers the claims, so a finishing one cannot clear a newer one.
    next_attachment_claim: u64,
    /// What the last-touched attachment chip is saying, if anything.
    pub(crate) attachment_note: Option<AttachmentNote>,
    /// Clears that note when it has been up long enough.
    attachment_note_task: Option<Task<()>>,
    /// The unsubscribe being asked about or under way, if any.
    pub(crate) unsubscribe: Option<crate::unsubscribe::Unsubscribing>,
    /// Messages whose list headers have been asked of the server this session.
    pub(crate) list_header_lookups: HashSet<MessageId>,
    /// The recount of the folder list that is on its way, if any.
    folder_counts_task: Option<Task<()>>,
    /// Whether one is already coming, so a burst asks for one rather than one
    /// each. Counting every message in a quarter-million-message store is not
    /// something to do per keystroke.
    folder_counts_pending: bool,
    /// Drains the five-second window while a sync runs.
    ///
    /// Held rather than detached so that dropping it stops the ticking: the
    /// counts only decay while there is something to decay.
    sync_flow_tick: Option<Task<()>>,
    pub filter: Filter,
    /// One of an extension's filters, as `(extension, filter)`, narrowing the
    /// list to the messages it says are that kind. Only while the extension
    /// is switched on for the mailbox being looked at.
    pub(crate) label_filter: Option<(String, String)>,
    /// How many of the loaded messages each of the switched-on extension's
    /// filters holds, as `(filter, count)`, in the manifest's order.
    pub(crate) label_counts: Vec<(String, usize)>,
    /// A backlog being handed to an extension, by `(extension, mailbox)`.
    pub(crate) scans: HashMap<(String, MailboxId), Task<()>>,
    /// How far each of those has got: `(handed over, backlog)`.
    pub(crate) scan_progress: HashMap<(String, MailboxId), (usize, usize)>,
    pub sort: SortOrder,
    pub search: Entity<TextInput>,
    pub search_text: String,

    pub envelopes: Vec<Envelope>,
    /// The cursor: the row keyboard motion moves and the reader follows.
    pub selected: usize,
    /// The message the reader was pointed at, if one was chosen.
    ///
    /// `None` means the cursor is merely *at* the top rather than *on*
    /// anything: nobody has picked a message, so a reload leaves it on
    /// whatever is newest. With a choice recorded, a reload follows that
    /// message instead — mail arriving while you are reading something must
    /// not swap what you are reading, and mail arriving before you have
    /// chosen anything must not drag the cursor down the list away from it.
    pub(crate) chosen: Option<MessageId>,
    /// Rows explicitly marked beyond the cursor.
    ///
    /// Empty is the common case and means "just the cursor's message", so
    /// nothing has to be marked for the single-message actions to work.
    pub marked: std::collections::HashSet<MessageId>,
    /// Where a shift-range started.
    anchor: Option<usize>,
    pub list_scroll: UniformListScrollHandle,
    /// The reader's conversation view, laid out as it scrolls. See
    /// `render_reader`.
    pub(crate) reader_list: gpui::ListState,
    /// Which conversation `reader_list` was last reset for: the message
    /// opened, and how many there were.
    pub(crate) reader_list_key: std::cell::Cell<Option<(MessageId, usize)>>,

    pub open: Option<OpenMessage>,
    /// When set, the reader shows every message in the conversation.
    pub thread_view: bool,
    pub thread: Vec<OpenMessage>,

    /// Sticky across messages: someone who switches to plain text is usually
    /// telling you something about the mail they read, not about one message.
    pub body_source: html::Source,
    /// Decoded remote images and the consent to fetch them.
    pub images: ImageStore,

    /// The draft being written, kept across closes so escape hides rather
    /// than destroys it.
    /// Open composer windows.
    ///
    /// Held so a second reply to the same message can bring the first forward,
    /// and so every draft can be written out when the app quits.
    pub composers: Vec<gpui::WindowHandle<crate::compose::Composer>>,
    /// What was left unsent when the app last closed, waiting for the next
    /// composer to pick it up.
    ///
    /// Not opened at launch: a half-written message is something you left, and
    /// a window for it appearing over everything on start-up is not a greeting.
    pub restored_draft: Option<(i64, spool_core::draft::Draft)>,
    /// The divider being dragged, with where the drag started.
    ///
    /// Held on the workspace rather than the divider element because the
    /// pointer leaves the divider almost immediately — the move events that
    /// matter arrive on the root.
    pub dragging: Option<Drag>,
    /// How far below the top of the scrollbar thumb it was grabbed, while the
    /// message list's scrollbar is being dragged.
    ///
    /// On the workspace for the reason [`Workspace::dragging`] is: a
    /// seven-pixel thumb is not somewhere the pointer stays, so the move
    /// events that matter arrive on the root.
    pub scroll_drag: Option<f32>,
    pub palette: Option<PaletteState>,
    /// The folder being created, renamed or deleted, and the question on
    /// screen about it.
    pub folder_prompt: Option<FolderPrompt>,
    /// The context menu open on a sidebar row, if any.
    pub folder_menu: Option<crate::folder_menu::FolderMenu>,
    /// Held so the server round trip keeps running while the prompt waits.
    pub(crate) folder_task: Option<Task<()>>,
    pub settings: Settings,
    /// The extension host, or `None` where the wasm engine would not start.
    ///
    /// Everything that touches it checks first, so a build or a machine
    /// without extensions runs exactly the code it ran before they existed.
    pub extensions: Option<crate::extensions::Host>,
    /// Decorations the background has filled in since the last frame.
    ///
    /// A generation counter rather than a flag: a decoration pass that
    /// finishes after a newer one started has nothing new to say.
    pub(crate) extensions_task: Option<Task<()>>,
    /// Extensions switched on or off since launch, by id. Applied at the next
    /// launch, and shown in Settings until then.
    pub(crate) extension_switches: HashMap<String, bool>,
    pub settings_ui: Option<SettingsState>,
    /// The settings window, so a second ⌘, brings it forward rather than
    /// opening a rival copy of the same state.
    pub settings_window: Option<gpui::WindowHandle<crate::settings_window::SettingsWindow>>,
    /// The About window, so asking for it again brings it forward.
    pub about_window: Option<gpui::WindowHandle<crate::about_window::AboutWindow>>,
    pub sidebar_visible: bool,

    /// The web view HTML mail is shown in, on the platform that has one.
    ///
    /// Built once with the window, because a native subview belongs to a
    /// window rather than to a frame. `None` where the platform has no web
    /// view, and the block renderer carries the message instead.
    #[cfg(target_os = "macos")]
    pub web_view: Option<crate::webview::SharedWebView>,
    /// Where making the web view has got to. See `ensure_web_view`.
    #[cfg(target_os = "macos")]
    web_view_state: WebViewState,
    /// Whether a message list has landed since launch.
    list_landed: bool,

    pub status: String,
    /// What the window is busy with, shown at the foot of the sidebar.
    ///
    /// Separate from `status`, which says what just happened: this says what
    /// is still happening, and survives the next thing that wants the status
    /// line.
    pub activities: Activities,
    /// Where the search for a newer Spool has got to, once there is anything
    /// to say about it. `None` until a check finds something or fails.
    pub update: Option<UpdateState>,
    /// Who opens `mailto:` links — Spool, or another app it could offer to
    /// take the job from.
    pub(crate) default_mail: crate::default_mail::Handler,
    /// Waiting on macOS to answer a request to make Spool that app.
    pub(crate) claiming_default_mail: bool,
    /// Filing operations that can still be taken back, oldest first.
    pub undo_stack: Vec<Undoable>,
    /// A status line to show once the pending reload lands.
    ///
    /// [`Workspace::reload`] clears the status on success, so a message about
    /// what just happened has to survive the refresh that follows it.
    status_after_reload: Option<String>,
    pub perf: Perf,

    /// Bumped on every reload so a slow query that finishes after a newer one
    /// discards its own result instead of overwriting fresher data.
    generation: u64,
    /// Where list queries run: one at a time, and only the newest of those
    /// waiting. See [`QueryWorker`].
    list_queries: QueryWorker,
    /// The list's own connection to the store, when it has one. See
    /// [`spool_store::ListReader`].
    list_reader: Option<Arc<spool_store::ListReader>>,
    /// What the list on screen was asked for: scope, query, grouping. A reload
    /// asking for something else is a new list, which `reload` shows a first
    /// page of before the rest.
    landed_query: Option<(Scope, Query, bool)>,
    /// The newest reload's generation, where the query thread can see it. An
    /// interrupt stops the statement a superseded reload is running, not the
    /// reload: without this, one stopped during its first page went straight
    /// on to read the whole list for a search nobody was making any more.
    newest_reload: Arc<AtomicU64>,
    /// When the list was last re-queried because mail arrived.
    ///
    /// A full sync reports every few hundred messages, and re-querying on
    /// each would put a scan of the whole mailbox between every batch. The
    /// list grows in steps instead.
    last_arrival_reload: Instant,
    reload_task: Option<Task<()>>,
    body_task: Option<Task<()>>,
    /// What `body_task` is loading: the message, and whether as a thread.
    ///
    /// A click selects and then opens, and both ask for the same message.
    /// Without this the second asked a moment after the first, looked like a
    /// cursor travelling, and threw the first load away to start again behind
    /// the travel debounce — 90ms on every click, for nothing.
    loading: Option<(MessageId, bool)>,
    /// Held so a sync keeps running; dropping the task would cancel it.
    sync_task: Option<Task<()>>,
    /// The backfill in flight, and the flag that stops a second one starting.
    backfill_task: Option<Task<()>>,
    pub backfilling: bool,
    /// One watcher per account, holding an idle IMAP connection. Dropping
    /// these is what stops them.
    watch_tasks: Vec<Task<()>>,
    /// The debounced sync a watcher asked for.
    watch_sync_task: Option<Task<()>>,
    /// Accounts a watcher has said changed, by address, waiting for a sync.
    ///
    /// A set, so a server that pings five times is synced once. Kept across a
    /// sync that is already running rather than dropped: mail that lands in
    /// the middle of one is fetched the moment it ends, where it used to wait
    /// for the next time anything at all happened.
    pending_syncs: std::collections::BTreeSet<String>,
    /// What is wrong with an account, by address, for the sidebar to say.
    ///
    /// Without this a failing account was silent: a sync's failures went to a
    /// status line the next update overwrote, and a watcher's to the log. An
    /// account could stop receiving mail for days with nothing on screen
    /// different from one that simply had none.
    pub(crate) account_problems: HashMap<String, AccountProblem>,
    /// The pending "this has been read". Replaced whenever the reader changes
    /// message, which is what cancels it.
    mark_read_task: Option<Task<()>>,
    /// Whether the sync now running was asked for by a person.
    ///
    /// The difference decides whether mail arriving is news: a sync you
    /// pressed and watched finish does not need a banner telling you what you
    /// were looking at, and one that ran because the server spoke does.
    pub(crate) asked_for_sync: bool,
    /// The debounced send of queued changes. Replacing it cancels the one it
    /// replaces, which is the whole debounce: archiving forty messages one
    /// keystroke at a time is one connection, not forty.
    push_task: Option<Task<()>>,
    pub syncing: bool,
    /// Whether this store is made up — the welcome screen's demo, or one
    /// generated by `spool --demo`.
    ///
    /// Nothing in it is anyone's, so nothing about it goes to a server: no
    /// sync, no idle watch, no queued changes pushed, and no send. That last
    /// one matters most. A draft whose sender has no account falls back to
    /// the first account configured, and a demo opened on a Mac with real
    /// accounts would otherwise send from one of them.
    pub demo: bool,
    /// Whether `accounts.toml` lists any account, as of launch or the last
    /// change made in Settings. With no mail stored either, it is what makes
    /// this a first run.
    pub has_accounts: bool,
    /// Going into the demo or coming out of it, while that is under way.
    pub(crate) switching: Option<crate::demo::Switch>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        store: Store,
        settings: Settings,
        extensions_root: std::path::PathBuf,
        demo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            let mut input = TextInput::new(MAIL_SEARCH_PROMPT, cx);
            input.set_short_placeholder(SHORT_SEARCH_PROMPT);
            input
        });

        // Re-query as the user types. Search is fast enough that debouncing it
        // would only add latency.
        let subscription = cx.subscribe(&search, |this: &mut Self, _, event: &InputChanged, cx| {
            this.search_text = event.0.to_string();
            this.selected = 0;
            this.chosen = None;
            this.reload(cx);
        });

        let list_focus = cx.focus_handle();
        window.focus(&list_focus);

        // A handle, so the clone costs nothing and the host holds one for the
        // background threads it answers extensions on.
        let store_for_extensions = store.clone();
        let list_reader = match store.list_reader() {
            Ok(reader) => reader.map(Arc::new),
            Err(e) => {
                log::warn!("no separate reader for the list ({e:#}); sharing the store's");
                None
            }
        };

        let mut this = Self {
            store,
            focus_handle: cx.focus_handle(),
            list_focus,
            sidebar_focus: cx.focus_handle(),
            sidebar_cursor: None,
            accounts: Vec::new(),
            mailboxes: Vec::new(),
            scope: Scope::Everything,
            syncing_folders: std::collections::HashSet::new(),
            sync_flow: crate::sync_flow::SyncFlow::default(),
            selection_moved_at: Instant::now(),
            attachment_progress: HashMap::new(),
            attachment_press: None,
            next_attachment_claim: 0,
            toast: None,
            toast_task: None,
            attachment_note: None,
            attachment_note_task: None,
            unsubscribe: None,
            list_header_lookups: HashSet::new(),
            sync_flow_tick: None,
            folder_counts_task: None,
            folder_counts_pending: false,
            filter: Filter::All,
            label_filter: None,
            label_counts: Vec::new(),
            scans: HashMap::new(),
            scan_progress: HashMap::new(),
            sort: SortOrder::Newest,
            search,
            search_text: String::new(),
            envelopes: Vec::new(),
            selected: 0,
            chosen: None,
            marked: std::collections::HashSet::new(),
            anchor: None,
            list_scroll: UniformListScrollHandle::new(),
            reader_list: gpui::ListState::new(0, gpui::ListAlignment::Top, px(600.)),
            reader_list_key: std::cell::Cell::new(None),
            open: None,
            thread_view: false,
            thread: Vec::new(),
            body_source: html::Source::default(),
            // Seeded from the preference, so a reader who has said yes once
            // is not asked again on every launch.
            images: {
                let mut store = ImageStore::default();
                store.set_always(settings.load_remote_images);
                store.set_domains(settings.image_domains.clone());
                store
            },
            composers: Vec::new(),
            restored_draft: None,
            dragging: None,
            scroll_drag: None,
            palette: None,
            folder_prompt: None,
            folder_menu: None,
            folder_task: None,
            settings,
            settings_ui: None,
            settings_window: None,
            about_window: None,
            sidebar_visible: true,
            // Made once the first list is on screen: see `ensure_web_view`.
            #[cfg(target_os = "macos")]
            web_view: None,
            #[cfg(target_os = "macos")]
            web_view_state: WebViewState::NotYet,
            list_landed: false,
            status: String::new(),
            activities: Activities::default(),
            update: None,
            default_mail: crate::default_mail::Handler::Unknown,
            claiming_default_mail: false,
            undo_stack: Vec::new(),
            status_after_reload: None,
            perf: Perf::default(),
            generation: 0,
            list_queries: QueryWorker::new(),
            list_reader,
            landed_query: None,
            newest_reload: Arc::new(AtomicU64::new(0)),
            last_arrival_reload: Instant::now(),
            reload_task: None,
            body_task: None,
            loading: None,
            sync_task: None,
            backfill_task: None,
            backfilling: false,
            watch_tasks: Vec::new(),
            watch_sync_task: None,
            pending_syncs: std::collections::BTreeSet::new(),
            account_problems: HashMap::new(),
            mark_read_task: None,
            asked_for_sync: false,
            push_task: None,
            syncing: false,
            demo,
            has_accounts: false,
            switching: None,
            extensions: crate::extensions::Host::open(extensions_root, store_for_extensions),
            extensions_task: None,
            extension_switches: HashMap::new(),
            _subscriptions: vec![subscription],
        };

        // The one store read still in front of the first frame. It has to be:
        // the sidebar cannot be drawn without knowing what the folders are.
        let folders = Instant::now();
        this.refresh_folders(cx);
        log::debug!("folder list read in {:?}", folders.elapsed());

        // Whatever was half-written when the app last closed.
        this.restore_draft(cx);
        // Open on the first account's inbox, the way a mail client should.
        if let Some(inbox) = this.mailboxes.iter().find(|m| m.role == MailboxRole::Inbox) {
            this.scope = Scope::Mailbox(inbox.id);
        }
        // Reading the folders above usually lands on the inbox already, and
        // that started the query for it. A second would replace it with the
        // same query, and the work the first had done would be thrown away.
        if this.reload_task.is_none() {
            this.reload(cx);
        }

        // After the reload is asked for rather than before it: compiling a
        // component is a compiler run, and nothing about it belongs in front
        // of the first list of messages.
        this.start_extensions(cx);

        // A mail client should be current by the time you have finished
        // looking at the window, so start a sync if an account is configured
        // — unless the mail on screen is made up, and there is nothing to
        // sync it with.
        this.has_accounts = Config::load(Config::default_path())
            .is_ok_and(|config| !config.accounts.is_empty());
        if this.has_accounts && !this.demo {
            // Everything on the way in: a launch is where the folder list
            // gets built and reconciled, and it is the one sync nobody is
            // waiting on a keystroke for.
            this.start_sync(SyncMode::Everything, cx);
            // And from then on the server says when, rather than being asked.
            this.start_watching(cx);
        }

        // Quietly, and only for a copy that could act on the answer: a build
        // running out of `target/` has nothing to replace, and telling its
        // developer that their own working tree is out of date every time
        // they press run would be a notification with no action behind it.
        if crate::update::can_install() {
            this.check_for_updates(false, cx);
            // And again every few hours while it stays open: a Mac that is
            // never quit is a copy that would otherwise never hear of a
            // release. Quiet, like the one on launch.
            cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(UPDATE_CHECK_EVERY).await;
                    if this.update(cx, |this, cx| this.check_for_updates(false, cx)).is_err() {
                        break;
                    }
                }
            })
            .detach();
        }
        this
    }

    /// The query implied by the current search box and filter chip.
    fn query(&self) -> Query {
        if !self.search_text.trim().is_empty() {
            Query::Text(self.search_text.clone())
        } else {
            match self.filter {
                Filter::All => Query::All,
                Filter::Unread => Query::Unread,
                Filter::Flagged => Query::Flagged,
            }
        }
    }

    /// Reloads the folder list and its counts. Cheap enough to run
    /// synchronously — it is one grouped query over an indexed column.
    pub fn refresh_folders(&mut self, cx: &mut Context<Self>) {
        match (self.store.accounts(), self.store.mailboxes()) {
            (Ok(accounts), Ok(mailboxes)) => {
                self.accounts = accounts;
                self.mailboxes = mailboxes;
                self.leave_hidden_scope(cx);
                // Here rather than in render: the count changes when the
                // folders do, and putting it in the draw path would set it
                // sixty times a second to the same value.
                crate::badge::set(&self.mailboxes);
            }
            (Err(e), _) | (_, Err(e)) => self.status = format!("Could not read folders: {e}"),
        }
    }

    /// Keeps the footer's five-second window draining while a sync runs.
    ///
    /// The counts in the bar are worked out against *now*, and most of what
    /// changes them is an event: a folder opening, a folder finishing. What is
    /// not an event is a completion ageing out, so without a clock the bar
    /// would freeze mid-burst every time the network went quiet — which is the
    /// exact moment it is supposed to show going quiet.
    ///
    /// Slower than a frame on purpose. This is a five-second window; a tick
    /// eight times a second moves each bar by well under a pixel and there is
    /// nothing to be gained by asking for more.
    const SYNC_FLOW_TICK: Duration = Duration::from_millis(125);

    fn start_sync_flow_tick(&mut self, cx: &mut Context<Self>) {
        self.sync_flow_tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Self::SYNC_FLOW_TICK).await;

                // Ends itself when the sync does, so the last tick after a run
                // is one timer rather than a loop nobody turned off.
                let running = this.update(cx, |this, cx| {
                    this.sync_flow.forget_old(Instant::now());
                    cx.notify();
                    this.syncing
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Reloads the folder list off the UI thread.
    ///
    /// The synchronous [`Workspace::refresh_folders`] is fine in response to a
    /// click, but during a sync the store is busy writing and the counts query
    /// walks every message — blocking a frame on that is exactly the thing
    /// this app is supposed not to do.
    pub fn refresh_folders_async(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();

        // One recount at a time, and never more than one waiting. Deleting a
        // stack of mail one key at a time asked for a count of every message
        // per keystroke, and those queries queued behind each other on the one
        // store lock — which is most of what made an instant action take
        // seconds.
        //
        // A throttle rather than a debounce, deliberately. A sync asks for
        // this on every progress event, and a debounce that restarts its timer
        // each time would never fire at all while one was running: the folder
        // list would sit frozen for the whole sync, which is the thing that
        // asking on every event was for.
        if self.folder_counts_pending {
            return;
        }
        self.folder_counts_pending = true;

        self.folder_counts_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COUNTS_SETTLE).await;
            let loaded = cx
                .background_executor()
                .spawn(async move { (store.accounts(), store.mailboxes()) })
                .await;

            this.update(cx, |this, cx| {
                this.folder_counts_pending = false;
                if let (Ok(accounts), Ok(mailboxes)) = loaded {
                    this.accounts = accounts;
                    this.mailboxes = mailboxes;
                    crate::badge::set(&this.mailboxes);
                    this.leave_hidden_scope(cx);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Re-runs the message query on a background thread.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;

        let store = self.store.clone();
        let scope = self.scope;
        let query = self.query();
        let grouped = self.settings.messages.conversations;
        // What to put the cursor back on: the chosen message, or the top.
        let selected_id = self.chosen;
        // An extension switched on for this mailbox sorts the list into its
        // filters, which means asking it about every loaded message rather
        // than only the rows on screen. On a launch that is the component
        // compiling and then a call per message in the mailbox: seconds.
        //
        // So only a chosen filter is waited for, because only then are the
        // rows different. With none chosen — always the case at launch — the
        // list goes up first and the counts above it follow.
        let labels = self.active_scanner().and_then(|extension| {
            let host = self.extensions.clone()?;
            Some((host, extension, self.label_filter.clone()))
        });
        let (sort_now, count_after) = match labels {
            Some((host, extension, Some(filter))) if filter.0 == extension => {
                (Some((host, extension, Some(filter))), None)
            }
            Some((host, extension, _)) => (None, Some((host, extension))),
            None => (None, None),
        };
        let counting = count_after.is_some();

        // A new list — launch, another folder, a different search — goes up a
        // page at a time: the top `FIRST_PAGE` rows are a few milliseconds to
        // read, all 150,000 of a large inbox are a few hundred. The rest
        // follows in the same job. A reload of the list already on screen
        // (mail arriving, a flag changing) does not do this, because there the
        // list would shrink to a page under someone scrolled halfway down it.
        // Nor does anything that reorders or regroups what the query returns.
        //
        // Not for a search, either. Its answer is already small — the index
        // has done the narrowing — and two passes over it cost more than one:
        // measured at 250,000 messages, a search went from 131ms to 153ms.
        let asked = (scope, query.clone(), grouped);
        let page_first = (self.envelopes.is_empty() || self.landed_query.as_ref() != Some(&asked))
            && !matches!(&query, Query::Text(text) if !text.trim().is_empty())
            && self.sort != SortOrder::Oldest
            && sort_now.is_none()
            && !counting;
        let (page_tx, page_rx) = smol::channel::bounded::<Vec<Envelope>>(1);

        // Queued here, not from inside the task below: that task first runs
        // when the main thread is next free, and at launch that is 50ms of
        // window set-up later — 50ms the query could have spent running.
        let started = Instant::now();
        // Whatever list query is running now is for a list nobody wants any
        // more — the search box has moved on, or the folder has — so it stops
        // where it is rather than finishing first.
        let reader = self.list_reader.clone();
        self.newest_reload.store(generation, Ordering::Relaxed);
        let newest = self.newest_reload.clone();
        if let Some(reader) = &reader {
            reader.interrupt();
        }
        let query = self.list_queries.run(move || {
            if page_first {
                let page = match (&reader, grouped) {
                    (Some(reader), true) => reader.conversations_page(scope, &query, FIRST_PAGE),
                    (Some(reader), false) => reader.envelopes(scope, &query, FIRST_PAGE),
                    (None, true) => store.conversations_page(scope, &query, FIRST_PAGE),
                    (None, false) => store.envelopes(scope, &query, FIRST_PAGE),
                };
                if let Ok(page) = page {
                    let _ = page_tx.try_send(page);
                }
                if newest.load(Ordering::Relaxed) != generation {
                    anyhow::bail!("superseded");
                }
            }
            let mut envelopes = match (&reader, grouped) {
                (Some(reader), true) => reader.conversations(scope, &query, LIST_LIMIT),
                (Some(reader), false) => reader.envelopes(scope, &query, LIST_LIMIT),
                (None, true) => store.conversations(scope, &query, LIST_LIMIT),
                (None, false) => store.envelopes(scope, &query, LIST_LIMIT),
            }?;
            let counts = match sort_now {
                Some((host, extension, filter)) => {
                    crate::extension_flow::sort_into_filters(&host, &extension, filter, &mut envelopes)
                }
                None => Vec::new(),
            };
            // Copied here rather than on the UI thread, where a whole
            // mailbox of envelopes is a frame's worth of cloning.
            let to_count = counting.then(|| envelopes.clone());
            anyhow::Ok((envelopes, counts, to_count))
        });

        self.reload_task = Some(cx.spawn(async move |this, cx| {
            // The first page, when there is one. Nothing arriving means the job
            // was superseded or the page failed; either way the full answer
            // below says which.
            if page_first && let Ok(page) = page_rx.recv().await {
                let landed = this.update(cx, |this, cx| {
                    if this.generation != generation {
                        return false;
                    }
                    this.list_landed = true;
                    this.envelopes = page;
                    this.selected = selected_id
                        .and_then(|id| row_for(&this.envelopes, id))
                        .unwrap_or(0);
                    this.sync_open_message(Showing::Landed, cx);
                    cx.notify();
                    true
                });
                if !matches!(landed, Ok(true)) {
                    return;
                }
            }

            // Superseded by a newer reload before it ran: that one will land.
            let Some(result) = query.await else { return };
            let elapsed = started.elapsed();
            let mut to_count = None;

            this.update(cx, |this, cx| {
                // A newer reload has already landed; this result is stale.
                if this.generation != generation {
                    return;
                }
                this.list_landed = true;

                match result {
                    Ok((envelopes, counts, later)) => {
                        this.perf.record_query(elapsed, envelopes.len());
                        this.envelopes = envelopes;
                        this.landed_query = Some(asked);
                        // Counts still to come keep the last ones up until
                        // they land, so a reload after new mail does not
                        // empty the chips for a moment.
                        match later {
                            Some(later) => to_count = Some(later),
                            None => this.label_counts = counts,
                        }
                        this.apply_sort();

                        // Keep the cursor on the chosen message where the new
                        // list still has it, so marking something read does
                        // not jump the selection somewhere else. Where nothing
                        // was chosen — at launch, after a filter change — the
                        // top is where it belongs.
                        this.selected = selected_id
                            .and_then(|id| row_for(&this.envelopes, id))
                            .unwrap_or(0);
                        this.status = this.status_after_reload.take().unwrap_or_default();
                    }
                    Err(e) => {
                        this.envelopes.clear();
                        this.status = format!("Query failed: {e}");
                    }
                }
                // `Landed`, and this is the line the whole distinction
                // exists for. A reload is the list changing underneath the
                // cursor — a background sync fetching mail, most often — and
                // the message that ends up under it was chosen by nobody.
                this.sync_open_message(Showing::Landed, cx);
                // What the rows should carry, worked out in the background
                // against a cache. Costs nothing at all when nothing is
                // installed, and nothing after the first pass over a page.
                this.decorate_messages(cx);
                cx.notify();
            })
            .ok();

            let (Some((host, extension)), Some(envelopes)) = (count_after, to_count) else { return };
            let counts = cx
                .background_executor()
                .spawn(async move { crate::extension_flow::count_filters(&host, &extension, &envelopes) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.label_counts = counts;
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn selected_envelope(&self) -> Option<&Envelope> {
        self.envelopes.get(self.selected)
    }

    /// The messages an action applies to.
    ///
    /// With nothing marked this is just the cursor's message, which is why
    /// every single-message action works unchanged.
    pub fn selection(&self) -> Vec<MessageId> {
        if self.marked.is_empty() {
            return self.selected_envelope().map(|e| e.id).into_iter().collect();
        }
        // List order rather than set order, so a bulk move files things in the
        // order they appear rather than in whatever order the hash produced.
        self.envelopes
            .iter()
            .filter(|e| self.marked.contains(&e.id))
            .map(|e| e.id)
            .collect()
    }

    /// Whether more than one row is marked.
    pub fn is_multi_select(&self) -> bool {
        self.marked.len() > 1
    }

    /// Whether a row should draw as selected.
    pub fn is_selected(&self, index: usize) -> bool {
        if self.marked.is_empty() {
            return index == self.selected;
        }
        self.envelopes.get(index).is_some_and(|e| self.marked.contains(&e.id))
    }

    /// Adds or removes one row from the marks — ⌘-click.
    pub fn toggle_mark(&mut self, index: usize, cx: &mut Context<Self>) {
        // The first ⌘-click has to capture the cursor's row too, or it would
        // silently drop the message that was already selected.
        if self.marked.is_empty()
            && let Some(current) = self.selected_envelope().map(|e| e.id)
        {
            self.marked.insert(current);
        }

        if let Some(envelope) = self.envelopes.get(index) {
            let id = envelope.id;
            if !self.marked.remove(&id) {
                self.marked.insert(id);
            }
            self.selected = index;
            self.chosen = Some(id);
            self.anchor = Some(index);
        }
        self.sync_open_message(Showing::Chosen, cx);
        cx.notify();
    }

    /// Marks everything between the anchor and `index` — shift-click and
    /// shift-arrow.
    pub fn extend_to(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.envelopes.is_empty() {
            return;
        }
        let index = index.min(self.envelopes.len() - 1);
        let anchor = self.anchor.unwrap_or(self.selected);

        let (low, high) = if anchor <= index { (anchor, index) } else { (index, anchor) };
        self.marked = self.envelopes[low..=high].iter().map(|e| e.id).collect();
        self.anchor = Some(anchor);
        self.selected = index;
        self.chosen = self.envelopes.get(index).map(|e| e.id);
        self.list_scroll.scroll_to_item(index, gpui::ScrollStrategy::Top);

        self.sync_open_message(Showing::Chosen, cx);
        cx.notify();
    }

    pub fn select_all_messages(&mut self, cx: &mut Context<Self>) {
        self.marked = self.envelopes.iter().map(|e| e.id).collect();
        self.anchor = Some(0);
        cx.notify();
    }

    /// Drops the marks, leaving the cursor where it is.
    pub fn clear_marks(&mut self) {
        self.marked.clear();
        self.anchor = None;
    }

    /// Makes the web view, once, on the frame after the first message list.
    ///
    /// The first `WKWebView` in a process brings WebKit's helper services up
    /// with it: about 45ms on an M1, all of it on the main thread. Made with
    /// the window, that was 45ms in front of the window appearing; made on the
    /// frame after it, it still held up the list, whose query cannot start
    /// until the main thread is free to hand it off. After the list, nobody is
    /// waiting on it — and a body takes a trip to the store before it needs
    /// one. Until then an HTML message waits on a blank reader rather than
    /// drawing as blocks and jumping a frame later.
    #[cfg(target_os = "macos")]
    fn ensure_web_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.web_view_state != WebViewState::NotYet || !self.list_landed {
            return;
        }
        self.web_view_state = WebViewState::Coming;
        cx.on_next_frame(window, |this, window, cx| {
            let started = Instant::now();
            this.web_view = crate::webview::WebView::new(window);
            // Made or failed, either way final: a failure falls back to the
            // block renderer, which is what a machine without one gets.
            this.web_view_state = WebViewState::Made;
            log::debug!("web view ready in {:?}", started.elapsed());
            cx.notify();
        });
    }

    /// Whether an HTML message is waiting for the web view to exist.
    #[cfg(target_os = "macos")]
    pub(crate) fn web_view_pending(&self) -> bool {
        self.web_view_state != WebViewState::Made
    }

    /// Loads the body for whatever is selected, unless it is already open.
    /// Whether the reader should show the whole conversation.
    ///
    /// Either because `t` asked for it, or because the row that was opened
    /// *is* a conversation: clicking a row labelled "3" and being shown one
    /// message is the row lying about what it stands for.
    pub fn showing_thread(&self) -> bool {
        self.thread_view || self.selected_envelope().is_some_and(|e| e.message_count() > 1)
    }

    fn sync_open_message(&mut self, why: Showing, cx: &mut Context<Self>) {
        let Some(envelope) = self.selected_envelope().cloned() else {
            self.open = None;
            self.thread.clear();
            self.mark_read_task = None;
            return;
        };

        let showing_thread = self.showing_thread();
        let already_open = self.open.as_ref().is_some_and(|open| open.envelope.id == envelope.id);

        // A question about unsubscribing belongs to the message it was asked
        // on. One already under way carries on and reports in the status line.
        if self.unsubscribe.as_ref().is_some_and(|u| u.message != envelope.id) {
            self.unsubscribe = None;
        }

        if starts_reading(why, already_open) {
            self.schedule_mark_read(cx);
        }

        if already_open && !showing_thread {
            return;
        }
        if already_open && showing_thread && !self.thread.is_empty() {
            return;
        }
        if self.loading == Some((envelope.id, showing_thread)) {
            return;
        }

        let store = self.store.clone();
        let thread_view = showing_thread;
        let thread_id = envelope.thread_id;
        let source = self.body_source;
        let message = envelope.id;

        // Whether the cursor is travelling. Replacing `body_task` cancels the
        // one it replaces, so a load that has not started yet never starts —
        // which is the point: holding backspace down the inbox should not read
        // and parse every message it passes over on the way.
        let moving = self.selection_moved_at.elapsed() < TRAVELLING;
        self.selection_moved_at = Instant::now();
        self.loading = Some((message, showing_thread));

        self.body_task = Some(cx.spawn(async move |this, cx| {
            if moving {
                cx.background_executor().timer(SETTLE).await;
            }
            let started = Instant::now();
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    // In conversation mode the reader shows every message in
                    // the thread, so fetch them in one hop rather than one
                    // round trip per message.
                    let envelopes = if thread_view {
                        store.thread(thread_id).unwrap_or_default()
                    } else {
                        Vec::new()
                    };

                    let body = store.body(envelope.id).unwrap_or_default().unwrap_or_default();
                    let files = store.stored_attachments(envelope.id).unwrap_or_default();
                    // Parsed here rather than on the other side of the await:
                    // this is the expensive half, and the main thread has a
                    // window to keep answering.
                    let primary = OpenMessage::new(envelope, body, source, files);

                    let others = envelopes
                        .into_iter()
                        .map(|e| {
                            let body = store.body(e.id).unwrap_or_default().unwrap_or_default();
                            let files = store.stored_attachments(e.id).unwrap_or_default();
                            OpenMessage::new(e, body, source, files)
                        })
                        .collect::<Vec<_>>();

                    (primary, others)
                })
                .await;
            let elapsed = started.elapsed();

            this.update(cx, |this, cx| {
                this.loading = None;
                // The selection can move while a body is loading, and an
                // early return above can leave an older load in flight: select
                // A, then B, then A again, and B's load would land on top of a
                // selection that has gone back to A. Only the message that is
                // selected right now may be shown.
                if this.selected_envelope().map(|e| e.id) != Some(message) {
                    return;
                }

                let (primary, others) = loaded;
                this.open = Some(primary);
                this.thread = others;

                // The preference can be toggled while a body is in flight, and
                // this one was parsed against the older answer.
                if this.body_source != source {
                    this.reparse_open_messages(cx);
                }

                this.perf.record_open(elapsed);
                this.look_up_list_unsubscribe(cx);

                // Permission is not the same as loading. The web view fetches
                // its own images once the sanitizer stops stripping them, but
                // the block renderer draws from what this has already
                // fetched — so with the preference on, opening a message has
                // to go and get them.
                let trusted = this
                    .open
                    .as_ref()
                    .and_then(|open| open.envelope.from.domain())
                    .is_some_and(|domain| this.images.trusts(Some(&domain)));
                if this.images.always() || trusted {
                    this.fetch_remote_images(false, cx);
                }

                cx.notify();
            })
            .ok();
        }));
    }

    pub fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.envelopes.is_empty() {
            return;
        }
        self.selected = index.min(self.envelopes.len() - 1);
        self.chosen = self.selected_envelope().map(|e| e.id);
        // Moving without a modifier collapses the selection: the alternative
        // is a set that silently outlives the intent that built it.
        let was_multi_select = self.is_multi_select();
        self.clear_marks();
        self.list_scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        self.sync_open_message(Showing::Chosen, cx);
        // Collapsing onto the message that was already open: it was never
        // put on the fuse while other rows were marked, so start it now.
        if was_multi_select && self.mark_read_task.is_none() {
            self.schedule_mark_read(cx);
        }
        cx.notify();
    }

    pub fn select_mailbox(&mut self, id: MailboxId, cx: &mut Context<Self>) {
        // Reached from the palette or ctrl-n as well as from the row, and
        // those two can land on a folder that is folded out of sight. The
        // sidebar has to show you where you now are.
        self.reveal_mailbox(id, cx);
        self.scope = Scope::Mailbox(id);
        self.label_filter = None;
        self.label_counts.clear();
        self.selected = 0;
        self.chosen = None;
        self.clear_marks();
        self.open = None;
        self.list_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.reload(cx);
    }

    /// Puts a dragged account where another one is.
    ///
    /// The whole arrangement is written down, not just the account that
    /// moved: the order has to survive accounts being added and removed, and
    /// a note saying "this one goes third" would be undefined the moment
    /// another arrived.
    pub fn reorder_account(&mut self, dragged: &DraggedAccount, onto: &str, cx: &mut Context<Self>) {
        if dragged.email.as_ref() == onto {
            return;
        }

        let current: Vec<String> =
            self.sorted_accounts().into_iter().map(|a| a.email.clone()).collect();
        let arranged = crate::sidebar::reorder(&current, &dragged.email, onto);
        if arranged == current {
            return;
        }

        self.settings.sidebar.set_account_order(arranged);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Whether a sync is inside this folder right now.
    ///
    /// Matched on the address and the server path because that is all the
    /// progress callback knows: it runs on a background thread with no row
    /// ids to hand, and the sidebar is the only place that has them.
    pub(crate) fn syncing_folder(&self, email: &str, path: &str) -> bool {
        folder_is_syncing(&self.syncing_folders, email, Some(path))
    }

    /// Whether a sync is inside any of an account's folders.
    pub(crate) fn syncing_account(&self, email: &str) -> bool {
        folder_is_syncing(&self.syncing_folders, email, None)
    }

    /// The accounts in the order the sidebar draws them.
    pub fn sorted_accounts(&self) -> Vec<&Account> {
        arrange_accounts(&self.accounts, self.settings.sidebar.account_order())
    }

    /// Switches the mail to a different scope: one folder, one account, or
    /// everything.
    pub fn select_scope(&mut self, scope: Scope, cx: &mut Context<Self>) {
        self.scope = scope;
        self.label_filter = None;
        self.label_counts.clear();
        self.selected = 0;
        self.chosen = None;
        self.clear_marks();
        self.open = None;
        self.list_scroll.scroll_to_item(0, gpui::ScrollStrategy::Top);
        self.reload(cx);
    }

    /// Applies a flag change locally and writes it through to the store.
    ///
    /// The in-memory envelope is updated first so the row repaints on this
    /// frame; the write is the slow part and nobody should wait for it.
    /// Sets the flags on a row and on every message it stands for.
    ///
    /// `id` is the row; `ids` is what the row means. They are the same thing
    /// for a single message and differ for a conversation, and keeping them
    /// as two arguments is what stops the list update and the store write
    /// from disagreeing about which.
    pub(crate) fn update_flags(
        &mut self,
        id: MessageId,
        ids: &[MessageId],
        flags: Flags,
        cx: &mut Context<Self>,
    ) {
        if let Some(envelope) = self.envelopes.iter_mut().find(|e| e.id == id) {
            envelope.flags = flags;
            // The row's own summary of the conversation has to move with it,
            // or a thread just marked read still draws as unread.
            if let Some(group) = envelope.group.as_mut() {
                group.unread = match flags.is_seen() {
                    true => 0,
                    false => group.ids.len() as u32,
                };
            }
        }
        if let Some(open) = self.open.as_mut().filter(|o| o.envelope.id == id) {
            open.envelope.flags = flags;
        }

        let ids = ids.to_vec();
        let store = self.store.clone();
        // After the write, so the unread count in the sidebar agrees with the
        // row that just changed rather than lagging one action behind.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    for id in ids {
                        if let Err(e) = store.set_flags(id, flags) {
                            log::error!("failed to persist flags for {id}: {e}");
                        }
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                this.refresh_folders_async(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();

        self.schedule_push(cx);
        cx.notify();
    }

    /// Removes rows from the list without a full reload, keeping the cursor
    /// near where it was.
    ///
    /// The cursor is placed by *position* rather than kept on its message:
    /// after archiving what you were looking at, the next message is where
    /// attention already is.
    fn remove_messages(&mut self, ids: &[MessageId], land: Landing, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let first_removed = self
            .envelopes
            .iter()
            .position(|e| ids.contains(&e.id))
            .unwrap_or(self.selected);

        self.envelopes.retain(|e| !ids.contains(&e.id));
        for id in ids {
            self.marked.remove(id);
        }
        self.anchor = None;

        self.selected = cursor_after_removal(first_removed, self.envelopes.len(), land);
        self.chosen = self.selected_envelope().map(|e| e.id);
        // The cursor moved, so the list follows it — the same rule as an
        // arrow key, and for the same reason. Deleting a run of messages
        // walks the cursor backwards a row at a time, and without this it
        // walks straight off the top of the viewport and keeps going: you are
        // then deleting mail you cannot see, from a list that looks still.
        self.list_scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Top);
        self.open = None;
        // `Landed`. Archiving a message moves the cursor onto the next one,
        // which is a message the filing chose rather than the reader did.
        self.sync_open_message(Showing::Landed, cx);
        cx.notify();
    }

    // ---- action handlers ----

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        // While the palette is open the same keys walk its list instead, so
        // arrow keys never move two cursors at once.
        if self.move_palette_selection(1, cx) {
            return;
        }
        self.select(self.selected.saturating_add(1), cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        if self.move_palette_selection(-1, cx) {
            return;
        }
        self.select(self.selected.saturating_sub(1), cx);
    }

    /// Moves the palette cursor. Returns whether the palette consumed the key.
    fn move_palette_selection(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let count = self.palette_row_count();
        let Some(palette) = self.palette.as_mut() else { return false };

        if count > 0 {
            let next = (palette.selected as isize + delta).rem_euclid(count as isize);
            palette.selected = next as usize;
        }
        cx.notify();
        true
    }

    /// How many rows the palette is showing, whichever list it is showing.
    fn palette_row_count(&self) -> usize {
        let Some(palette) = self.palette.as_ref() else { return 0 };
        let all = match palette.mode {
            PaletteMode::Commands => {
                crate::palette::matching_in(self.palette_commands(), &palette.filter).len()
            }
            PaletteMode::Folders => {
                crate::palette::matching_folders(&self.move_destinations(), &palette.filter).len()
            }
        };
        all.min(crate::palette::VISIBLE)
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.folder_prompt.is_some() {
            self.confirm_folder_prompt(cx);
            return;
        }

        let Some(palette) = self.palette.as_ref() else {
            self.open_selected(cx);
            return;
        };

        let chosen = match palette.mode {
            PaletteMode::Commands => {
                crate::palette::matching_in(self.palette_commands(), &palette.filter)
                    .into_iter()
                    .nth(palette.selected)
                    .map(|(command, _)| Chosen::Command(command.action))
            }
            PaletteMode::Folders => {
                crate::palette::matching_folders(&self.move_destinations(), &palette.filter)
                    .into_iter()
                    .nth(palette.selected)
                    .map(|(folder, _)| Chosen::Folder(folder.id))
            }
        };

        self.palette = None;
        window.focus(&self.list_focus);

        match chosen {
            // Dispatching rather than calling keeps one path from intent to
            // behaviour, whether it was typed, clicked or picked here.
            Some(Chosen::Command(action)) => window.dispatch_action(action, cx),
            Some(Chosen::Folder(target)) => self.move_selection_to(target, cx),
            None => {}
        }
        cx.notify();
    }

    pub fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |input, cx| input.clear(cx));
        window.focus(&self.list_focus);
        cx.notify();
    }

    /// Reports how much disk the local store occupies. Wired to the status
    /// bar, because "where did my mail go" deserves an answer.
    pub fn report_store_size(&mut self, cx: &mut Context<Self>) {
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let measured = cx
                .background_executor()
                .spawn(async move { (store.size_on_disk(), store.message_count()) })
                .await;

            this.update(cx, |this, cx| {
                if let (Ok(bytes), Ok(count)) = measured {
                    this.status = format!(
                        "{} messages in {}",
                        crate::perf::format_count(count as usize),
                        spool_core::format_size(bytes.min(u32::MAX as u64) as u32),
                    );
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn select_first(&mut self, _: &SelectFirst, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0, cx);
    }

    fn select_last(&mut self, _: &SelectLast, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.envelopes.len().saturating_sub(1), cx);
    }

    fn page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected + self.perf.visible_rows.max(1), cx);
    }

    fn page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.select(self.selected.saturating_sub(self.perf.visible_rows.max(1)), cx);
    }

    fn open_message(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.open_selected(cx);
    }

    /// Opens whatever is selected and marks it read. Reached from `enter`, a
    /// click on a row, and the command palette.
    pub fn open_selected(&mut self, cx: &mut Context<Self>) {
        // Reading is what `sync_open_message` starts the clock on. Nothing is
        // marked here: pressing enter and archiving a second later should
        // leave the message unread, the same as cursoring past it.
        self.sync_open_message(Showing::Chosen, cx);
    }

    /// Starts the clock on the open message being read.
    ///
    /// Not marked on arrival, which is what it used to do: holding `j` down a
    /// list marks twenty messages read on the way past, and an unread count
    /// that empties because you scrolled is a count that has stopped meaning
    /// anything. [`MARK_READ_AFTER`] is long enough to be a glance rather than
    /// a scroll.
    ///
    /// Only ever reached for a message somebody moved to — see [`Showing`].
    /// Mail that arrives under a stationary cursor is shown and left unread,
    /// however long the app sits there with it on screen.
    ///
    /// The task is replaced whenever the reader changes message, and dropping
    /// a task cancels it — so moving on before the deadline is the whole of
    /// "changed my mind".
    fn schedule_mark_read(&mut self, cx: &mut Context<Self>) {
        let Some(envelope) = self.selected_envelope() else {
            self.mark_read_task = None;
            return;
        };
        // Already read, by hand or by having been read before — or one of
        // several rows marked, which is a selection being gathered for a
        // bulk action, not a message being read. Nothing to wait for.
        if !envelope.any_unread() || self.is_multi_select() {
            self.mark_read_task = None;
            return;
        }

        let id = envelope.id;
        self.mark_read_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(MARK_READ_AFTER).await;

            this.update(cx, |this, cx| {
                // Half a second is a long time in a mailbox. The selection
                // may have moved, the message may have been marked read by
                // hand, or the list may have reloaded under it — and in each
                // case this has nothing left to say.
                let Some(current) = this.selected_envelope().cloned() else { return };
                if current.id != id || !current.any_unread() || this.is_multi_select() {
                    return;
                }

                // A conversation is read as a whole: the reader shows every
                // message in the thread, so leaving the older ones unread
                // would be a count for mail that is on screen.
                let ids = current.message_ids();
                this.update_flags(
                    current.id,
                    &ids,
                    current.flags.with(Flags::SEEN, true),
                    cx,
                );
            })
            .ok();
        }));
    }

    fn toggle_read(&mut self, _: &ToggleRead, _: &mut Window, cx: &mut Context<Self>) {
        // Inverted from what the row *shows*, not from the flags on the
        // message the row happens to be headed by. A conversation draws as
        // unread when any message in it is, so a thread whose newest reply
        // has been read but whose older ones have not would otherwise read
        // "unread" on screen and be marked unread again by this — the one
        // press that visibly does nothing.
        let Some(current) = self.selected_envelope() else { return };
        self.set_read_on_selection(current.any_unread(), cx);
    }

    fn mark_read(&mut self, _: &MarkRead, _: &mut Window, cx: &mut Context<Self>) {
        self.set_read_on_selection(true, cx);
    }

    fn mark_unread(&mut self, _: &MarkUnread, _: &mut Window, cx: &mut Context<Self>) {
        self.set_read_on_selection(false, cx);
    }

    /// Sets read or unread across the selection, rather than flipping each.
    ///
    /// Set rather than toggle, because a toggle over a mixed selection has no
    /// good answer: whatever it does, half the rows go the way you did not
    /// mean. "Mark these read" is unambiguous however they started.
    fn set_read_on_selection(&mut self, read: bool, cx: &mut Context<Self>) {
        // The message being looked at is on a two-second fuse to be marked
        // read. Saying so by hand — in either direction — is the answer, so
        // the fuse goes out. Without this, marking something unread and
        // watching it turn itself back read a second later is the whole
        // feature undoing itself in front of you.
        self.mark_read_task = None;

        let count = self.selection().len();
        for id in self.selection() {
            let Some(envelope) = self.envelopes.iter().find(|e| e.id == id) else { continue };
            let flags = envelope.flags.with(Flags::SEEN, read);
            // Every message the row stands for: marking a conversation read
            // has to mean the conversation, or the row comes back unread the
            // moment the list reloads.
            let ids = envelope.message_ids();
            self.update_flags(id, &ids, flags, cx);
        }

        self.status = match (read, count) {
            (_, 0) => return,
            (true, 1) => "Marked read".into(),
            (false, 1) => "Marked unread".into(),
            (true, n) => format!("Marked {n} read"),
            (false, n) => format!("Marked {n} unread"),
        };
        cx.notify();
    }

    /// Flips one flag over everything selected.
    ///
    /// The cursor's message decides the direction, so a mixed selection all
    /// ends up the same rather than each message inverting independently —
    /// pressing `u` on ten messages should mean one thing, not ten.
    fn toggle_flag_across_selection(&mut self, bit: u32, cx: &mut Context<Self>) {
        let Some(current) = self.selected_envelope() else { return };
        let target = !current.flags.has(bit);

        for id in self.selection() {
            let Some(envelope) = self.envelopes.iter().find(|e| e.id == id) else { continue };
            let flags = envelope.flags.with(bit, target);
            // Every message the row stands for. Marking a conversation read
            // has to mean the conversation, or the row comes back unread the
            // moment the list reloads.
            let ids = envelope.message_ids();
            self.update_flags(id, &ids, flags, cx);
        }
    }

    fn toggle_flag(&mut self, _: &ToggleFlag, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_flag_on_selected(cx);
    }

    pub fn toggle_flag_on_selected(&mut self, cx: &mut Context<Self>) {
        self.toggle_flag_across_selection(Flags::FLAGGED, cx);
    }

    fn archive(&mut self, _: &Archive, _: &mut Window, cx: &mut Context<Self>) {
        self.file_selection(MailboxRole::Archive, "Archived", Landing::Replacement, cx);
    }

    fn mark_as_spam(&mut self, _: &MarkAsSpam, _: &mut Window, cx: &mut Context<Self>) {
        self.file_selection(MailboxRole::Junk, "Marked as spam \u{2014}", Landing::Replacement, cx);
    }

    /// The inverse, which is the one that has to exist: a client that can only
    /// mark spam is a client whose mistakes are permanent.
    fn not_spam(&mut self, _: &NotSpam, _: &mut Window, cx: &mut Context<Self>) {
        self.file_selection(MailboxRole::Inbox, "Moved to Inbox \u{2014}", Landing::Replacement, cx);
    }

    /// Opens the palette as a folder picker for the selection.
    fn move_to_folder(&mut self, _: &MoveToFolder, window: &mut Window, cx: &mut Context<Self>) {
        if self.selection().is_empty() {
            self.status = "Select a message first".into();
            cx.notify();
            return;
        }
        if self.move_destinations().is_empty() {
            // Said here rather than as an empty picker: the reason is almost
            // always a selection spanning two accounts, and mail cannot cross
            // accounts.
            self.status = "Nowhere to move these \u{2014} a selection cannot span accounts".into();
            cx.notify();
            return;
        }

        self.palette = Some(crate::palette::PaletteState::folders(window, cx));
        cx.notify();
    }

    /// The folders the current selection could be filed into.
    pub(crate) fn move_destinations(&self) -> Vec<Destination> {
        move_destinations(&self.mailboxes, &self.envelopes, &self.selection(), &self.accounts)
    }

    /// Files the selection into one specific folder, from the picker.
    pub(crate) fn move_selection_to(&mut self, target: MailboxId, cx: &mut Context<Self>) {
        let Some(destination) = self.mailboxes.iter().find(|m| m.id == target).cloned() else {
            return;
        };

        let moves: Vec<(MessageId, MailboxId)> = self
            .selection()
            .into_iter()
            .filter(|id| {
                self.envelopes
                    .iter()
                    .any(|e| e.id == *id && e.account_id == destination.account_id)
            })
            .map(|id| (id, target))
            .collect();

        self.apply_moves(
            moves,
            &format!("Moved to {} \u{2014}", destination.name),
            Landing::Replacement,
            cx,
        );
    }

    /// Moves everything selected into a role's mailbox.
    ///
    /// The destination is resolved per message, within that message's own
    /// account, so a bulk archive from the unified view does not file work
    /// mail into a personal folder.
    fn file_selection(
        &mut self,
        role: MailboxRole,
        verb: &str,
        land: Landing,
        cx: &mut Context<Self>,
    ) {
        let selection = self.selection();
        if selection.is_empty() {
            return;
        }

        let mut moves: Vec<(MessageId, MailboxId)> = Vec::with_capacity(selection.len());
        let mut missing = 0usize;

        for id in &selection {
            let Some(envelope) = self.envelopes.iter().find(|e| e.id == *id) else { continue };
            let target = self
                .mailboxes
                .iter()
                .find(|m| m.account_id == envelope.account_id && m.role == role)
                .map(|m| m.id);

            match target {
                Some(target) => moves.push((*id, target)),
                None => missing += 1,
            }
        }

        if moves.is_empty() && missing > 0 {
            self.status = format!("No {} folder for that account", role.as_str());
            cx.notify();
            return;
        }

        self.apply_moves(moves, verb, land, cx);
    }

    /// Applies resolved moves: updates the list, records the undo, writes to
    /// the store, and says what happened.
    ///
    /// Messages already in the destination are dropped rather than moved — a
    /// no-op that still costs the row its place in the list.
    pub(crate) fn apply_moves(
        &mut self,
        moves: Vec<(MessageId, MailboxId)>,
        verb: &str,
        land: Landing,
        cx: &mut Context<Self>,
    ) {
        let mut pending: Vec<(MessageId, MailboxId)> = Vec::with_capacity(moves.len());
        // Where each message came from, so the move can be taken back.
        let mut origins: Vec<(MessageId, MailboxId)> = Vec::with_capacity(moves.len());
        // Rows this view no longer holds.
        let mut leaving: Vec<MessageId> = Vec::new();
        let mut staying: Vec<(usize, MailboxId)> = Vec::new();
        let mut last_subject = String::new();

        for (id, target) in moves {
            let Some(index) = self.envelopes.iter().position(|e| e.id == id) else { continue };
            let envelope = &self.envelopes[index];
            if envelope.mailbox_id == target {
                continue;
            }

            origins.push((id, envelope.mailbox_id));
            last_subject = envelope.subject.clone();
            pending.push((id, target));

            if scope_holds(self.scope, &self.mailboxes, target) {
                staying.push((index, target));
            } else {
                leaving.push(id);
            }
        }

        if pending.is_empty() {
            self.status.clear();
            cx.notify();
            return;
        }

        let moved = pending.len();

        // What the store is told, as opposed to what the list is told. A row
        // standing for a conversation moves every message in it; the list
        // still treats the row as one thing, which is what keeps the counting
        // and the undo honest.
        let writes: Vec<(MessageId, MailboxId)> = pending
            .iter()
            .flat_map(|(id, target)| {
                let ids = self
                    .envelopes
                    .iter()
                    .find(|e| e.id == *id)
                    .map(Envelope::message_ids)
                    .unwrap_or_else(|| vec![*id]);
                ids.into_iter().map(move |member| (member, *target))
            })
            .collect();

        // Filing inside the unified view moves a message between folders
        // without taking it out of "everything", so the row stays and only its
        // folder changes. Dropping it would be a lie the next reload corrects.
        for (index, target) in staying {
            self.envelopes[index].mailbox_id = target;
        }
        self.remove_messages(&leaving, land, cx);

        let store = self.store.clone();
        // The counts are refreshed *after* the writes, not alongside them.
        // Reading them first is reading the store as it was a moment ago: the
        // row leaves the list and the sidebar goes on claiming the folder
        // still holds it, until something unrelated happens to reload.
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    for (id, target) in writes {
                        if let Err(e) = store.move_message(id, target) {
                            log::error!("failed to move {id}: {e}");
                        }
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                this.refresh_folders_async(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();

        self.schedule_push(cx);
        let summary = summarize(&last_subject, moved);
        self.status = format!("{verb} {summary}");
        self.push_undo(Undoable { summary, moves: origins });
        cx.notify();
    }

    /// The drop half of drag and drop: messages onto a folder in the sidebar.
    ///
    /// Mail cannot cross accounts. IMAP has no move between servers, and
    /// pretending otherwise would mean uploading a copy under a different
    /// identity, which is not what dragging a row looks like it does.
    pub fn drop_on_mailbox(
        &mut self,
        dragged: &DraggedMessages,
        target: MailboxId,
        cx: &mut Context<Self>,
    ) {
        let Some(destination) = self.mailboxes.iter().find(|m| m.id == target) else { return };
        let account = destination.account_id;
        let name = destination.name.clone();

        let mut moves = Vec::with_capacity(dragged.ids.len());
        let mut foreign = 0usize;
        for id in dragged.ids.iter() {
            let Some(envelope) = self.envelopes.iter().find(|e| e.id == *id) else { continue };
            if envelope.account_id == account {
                moves.push((*id, target));
            } else {
                foreign += 1;
            }
        }

        if moves.is_empty() {
            self.status = if foreign > 0 {
                "Mail cannot move between accounts".into()
            } else {
                String::new()
            };
            cx.notify();
            return;
        }

        self.apply_moves(moves, &format!("Moved to {name} \u{2014}"), Landing::Replacement, cx);
        if foreign > 0 {
            self.status.push_str(&format!(" ({foreign} from another account stayed put)"));
        }
    }

    /// What dragging a given row carries.
    ///
    /// Dragging a row that is part of the selection takes the whole selection;
    /// dragging one outside it takes just that row, which is what the pointer
    /// is pointing at.
    pub fn drag_payload(
        &self,
        index: usize,
        selected: &std::rc::Rc<Vec<MessageId>>,
    ) -> Option<DraggedMessages> {
        let envelope = self.envelopes.get(index)?;
        if selected.len() > 1 && selected.contains(&envelope.id) {
            return Some(DraggedMessages {
                ids: selected.clone(),
                label: format!("{} messages", selected.len()).into(),
            });
        }
        Some(DraggedMessages {
            ids: std::rc::Rc::new(vec![envelope.id]),
            label: truncate(&envelope.subject, 32).into(),
        })
    }

    /// Sets a status line that survives the reload that follows it.
    ///
    /// [`Workspace::reload`] clears the status when it lands, which would
    /// otherwise swallow anything said just before a re-query.
    pub(crate) fn set_sticky_status(&mut self, text: String) {
        self.status = text.clone();
        self.status_after_reload = Some(text);
    }

    /// Records a move that can be taken back, dropping the oldest once the
    /// stack is full.
    fn push_undo(&mut self, entry: Undoable) {
        self.undo_stack.push(entry);
        if self.undo_stack.len() > UNDO_DEPTH {
            self.undo_stack.remove(0);
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        self.undo_last_move(cx);
    }

    /// Puts the most recent archive, trash or move back where it came from.
    pub fn undo_last_move(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.undo_stack.pop() else {
            self.status = "Nothing to undo".into();
            cx.notify();
            return;
        };

        self.set_sticky_status(format!("Put {} back", entry.summary));
        cx.notify();

        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            // The moves have to finish before the re-query runs, or the list
            // gets rebuilt from rows that have not moved yet.
            cx.background_executor()
                .spawn(async move {
                    for (id, home) in entry.moves {
                        if let Err(e) = store.move_message(id, home) {
                            log::error!("failed to undo the move of {id}: {e}");
                        }
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                // Where the messages belong now depends on which folder you
                // are standing in, so re-query rather than guess.
                this.reload(cx);
                this.refresh_folders_async(cx);
            })
            .ok();
        })
        .detach();
    }

    fn delete_message(&mut self, _: &DeleteMessage, _: &mut Window, cx: &mut Context<Self>) {
        let selection = self.selection();
        if selection.is_empty() {
            return;
        }

        // Deleting from the trash is the only real delete; everywhere else it
        // means "move to trash".
        let in_trash = self
            .mailboxes
            .iter()
            .find(|m| Scope::Mailbox(m.id) == self.scope)
            .is_some_and(|m| m.role == MailboxRole::Trash);

        if !in_trash {
            // The everyday delete: from anywhere but the Trash it *is* a
            // move, so the cursor rule has to travel with it or pressing
            // backspace in the inbox would behave differently from pressing
            // it in the Trash.
            self.file_selection(MailboxRole::Trash, "Moved to Trash \u{2014}", Landing::Previous, cx);
            return;
        }

        let count = selection.len();
        let last_subject = self
            .envelopes
            .iter()
            .find(|e| selection.last() == Some(&e.id))
            .map(|e| e.subject.clone())
            .unwrap_or_default();

        // Every message the selected rows stand for, before the rows go.
        let doomed: Vec<MessageId> = selection
            .iter()
            .flat_map(|id| {
                self.envelopes
                    .iter()
                    .find(|e| e.id == *id)
                    .map(Envelope::message_ids)
                    .unwrap_or_else(|| vec![*id])
            })
            .collect();

        self.remove_messages(&selection, Landing::Previous, cx);
        // Nothing brings these back, so any undo entry that mentions them is
        // now a promise the store cannot keep.
        prune_undo(&mut self.undo_stack, &doomed);

        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    for id in doomed {
                        if let Err(e) = store.delete_message(id) {
                            log::error!("failed to delete {id}: {e}");
                        }
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                this.refresh_folders_async(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();

        self.schedule_push(cx);
        self.status = format!("Deleted {}", summarize(&last_subject, count));
    }

    fn mark_all_read(&mut self, _: &MarkAllRead, _: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let scope = self.scope;

        for envelope in &mut self.envelopes {
            envelope.flags = envelope.flags.with(Flags::SEEN, true);
        }

        cx.spawn(async move |this, cx| {
            let changed = cx
                .background_executor()
                .spawn(async move { store.mark_all_read(scope) })
                .await;

            this.update(cx, |this, cx| {
                this.status = match changed {
                    Ok(n) => format!("Marked {n} messages read"),
                    Err(e) => format!("Could not mark read: {e}"),
                };
                this.refresh_folders_async(cx);
                this.schedule_push(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Flips the list between newest-first and oldest-first.
    ///
    /// Reordering in place rather than re-querying: the store hands the list
    /// back newest-first, and which end of the same rows you start from is not
    /// a question worth a round trip to SQLite.
    pub fn toggle_sort(&mut self, cx: &mut Context<Self>) {
        self.sort = self.sort.flipped();
        self.envelopes.reverse();
        // The cursor stays on the message it was on, which has just moved.
        self.selected = self
            .chosen
            .and_then(|id| self.envelopes.iter().position(|e| e.id == id))
            .unwrap_or(0);
        cx.notify();
    }

    /// Puts a freshly loaded list in the order the header chip claims.
    ///
    /// The store always answers newest-first, so this is a reverse or nothing.
    fn apply_sort(&mut self) {
        if self.sort == SortOrder::Oldest {
            self.envelopes.reverse();
        }
    }

    fn cycle_filter(&mut self, _: &CycleFilter, _: &mut Window, cx: &mut Context<Self>) {
        self.cycle_filter_now(cx);
    }

    /// Drops back to the unfiltered list.
    pub fn clear_filter(&mut self, cx: &mut Context<Self>) {
        if self.filter == Filter::All {
            return;
        }
        self.filter = Filter::All;
        self.selected = 0;
        self.chosen = None;
        self.reload(cx);
    }

    /// Narrows the list to flagged mail, or widens it back — the flag button
    /// above the list.
    pub fn toggle_flagged_filter(&mut self, cx: &mut Context<Self>) {
        self.filter = match self.filter {
            Filter::Flagged => Filter::All,
            _ => Filter::Flagged,
        };
        self.selected = 0;
        self.chosen = None;
        self.reload(cx);
    }

    pub fn cycle_filter_now(&mut self, cx: &mut Context<Self>) {
        self.filter = self.filter.next();
        self.selected = 0;
        // A different list is a different question; the answer starts at the
        // top rather than hunting for where the last message went.
        self.chosen = None;
        self.reload(cx);
    }

    fn toggle_body_source(
        &mut self,
        _: &ToggleBodySource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dispatch_toggle_body_source(window, cx);
    }

    /// The body of the action, so a click can reach it too.
    pub fn dispatch_toggle_body_source(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.body_source = self.body_source.other();
        self.status = match self.body_source {
            html::Source::Rich => "Showing the formatted message".into(),
            html::Source::Plain => "Showing the plain-text version".into(),
        };
        self.reparse_open_messages(cx);
    }

    /// Re-lays-out whatever is open, after the chosen body changed.
    ///
    /// Re-parses from the bodies already in memory rather than going back to
    /// the store — the text is already here, and a toggle should be instant.
    fn reparse_open_messages(&mut self, cx: &mut Context<Self>) {
        let source = self.body_source;
        let reparse = |message: &mut OpenMessage| {
            *message = OpenMessage::new(
                message.envelope.clone(),
                std::mem::take(&mut message.body),
                source,
                std::mem::take(&mut message.stored_attachments),
            );
        };

        if let Some(open) = self.open.as_mut() {
            reparse(open);
        }
        for message in &mut self.thread {
            reparse(message);
        }
        cx.notify();
    }

    /// Fetches the open message's remote images, because the reader asked for
    /// this one.
    pub fn load_remote_images(&mut self, cx: &mut Context<Self>) {
        self.fetch_remote_images(true, cx);
    }

    /// Says yes to this sender for good, and loads what is on screen now.
    ///
    /// Narrower than the preference by design: it answers for one sender
    /// rather than for everyone who ever writes, which is the difference
    /// between "I read this newsletter for the pictures" and "track me".
    pub fn always_load_images_from_sender(&mut self, cx: &mut Context<Self>) {
        let Some(domain) = self.open_sender_domain() else {
            // Nothing to key a rule on. Loading this once is still what the
            // press meant, so it is not silently dropped.
            self.load_remote_images(cx);
            return;
        };

        self.images.trust_domain(&domain);
        self.settings.image_domains = self.images.domains();
        self.save_settings(cx);

        self.fetch_remote_images(false, cx);
        self.status = format!("Images will load for {domain}");
        cx.notify();
    }

    /// Takes that back.
    ///
    /// The message on screen goes back to blocked as well as the sender's
    /// future mail: leaving it loaded would make the click look like it did
    /// nothing, and the images are already in the cache should the reader
    /// change their mind.
    pub fn stop_loading_images_from_sender(&mut self, cx: &mut Context<Self>) {
        let Some(domain) = self.open_sender_domain() else { return };

        self.images.forget_domain(&domain);
        self.settings.image_domains = self.images.domains();
        self.save_settings(cx);

        // Every message on screen from that sender, not just the one whose
        // line was clicked: in a conversation they are usually all from the
        // same newsletter.
        let open = self.open.iter().chain(self.thread.iter());
        let theirs: Vec<MessageId> = open
            .filter(|message| message.envelope.from.domain().as_deref() == Some(&domain))
            .map(|message| message.envelope.id)
            .collect();
        for message in theirs {
            self.images.disallow(message);
        }

        self.status = format!("Images will not load for {domain}");
        cx.notify();
    }

    /// Drops a sender from the settings pane's list.
    pub fn forget_image_domain(&mut self, domain: String, cx: &mut Context<Self>) {
        self.images.forget_domain(&domain);
        self.settings.image_domains = self.images.domains();
        self.save_settings(cx);
        self.status = format!("Images will not load for {domain}");
        cx.notify();
    }

    /// The domain of whoever sent the open message.
    fn open_sender_domain(&self) -> Option<String> {
        self.open.as_ref()?.envelope.from.domain()
    }

    /// Fetches the open message's remote images.
    ///
    /// Never called from render. A request to a URL in a message tells the
    /// sender the moment their mail was opened, so this happens on exactly two
    /// triggers: a press of "Load images", or a message opening while the
    /// standing preference says yes. Both are the reader having decided;
    /// neither is the app deciding for them.
    ///
    /// `announce` is what separates them. A press deserves an answer in the
    /// status bar, and a preference that fires on every message opened does
    /// not — it would say "Images loaded" over whatever the status bar was
    /// actually for, once per message, for ever.
    fn fetch_remote_images(&mut self, announce: bool, cx: &mut Context<Self>) {
        let Some(open) = self.open.as_ref() else { return };
        let message = open.envelope.id;
        let urls = open.remote_images();

        self.images.allow(message);
        let wanted: Vec<String> =
            urls.into_iter().filter(|url| self.images.begin(url)).collect();

        if announce {
            self.status = match wanted.len() {
                0 => "Images loaded".into(),
                1 => "Loading 1 image\u{2026}".into(),
                n => format!("Loading {n} images\u{2026}"),
            };
        }
        cx.notify();

        for url in wanted {
            cx.spawn(async move |this, cx| {
                let fetched = cx
                    .background_executor()
                    .spawn({
                        let url = url.clone();
                        async move {
                            let bytes = spool_net::http::get(
                                &url,
                                crate::images::MAX_REMOTE_BYTES,
                            )
                            .await?;
                            // The declared type is not available here without
                            // keeping the headers, and senders mislabel images
                            // anyway; the bytes decide.
                            crate::images::decode("", bytes)
                                .ok_or_else(|| anyhow::anyhow!("not an image format we can draw"))
                        }
                    })
                    .await;

                this.update(cx, |this, cx| {
                    let outcome = match fetched {
                        Ok(image) => Remote::Ready(image),
                        Err(e) => Remote::Failed(format!("{e:#}")),
                    };
                    this.images.finish(url, outcome);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }

    fn toggle_thread_view(&mut self, _: &ToggleThreadView, _: &mut Window, cx: &mut Context<Self>) {
        self.thread_view = !self.thread_view;
        self.thread.clear();
        self.sync_open_message(Showing::Chosen, cx);
        cx.notify();
    }

    fn next_mailbox(&mut self, _: &NextMailbox, _: &mut Window, cx: &mut Context<Self>) {
        self.step_mailbox(1, cx);
    }

    fn prev_mailbox(&mut self, _: &PrevMailbox, _: &mut Window, cx: &mut Context<Self>) {
        self.step_mailbox(-1, cx);
    }

    fn step_mailbox(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.mailboxes.is_empty() {
            return;
        }
        let current = match self.scope {
            Scope::Mailbox(id) => self.mailboxes.iter().position(|m| m.id == id).unwrap_or(0),
            _ => 0,
        };
        let count = self.mailboxes.len() as isize;
        let next = (current as isize + delta).rem_euclid(count) as usize;
        self.select_mailbox(self.mailboxes[next].id, cx);
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search.read(cx).focus_handle.clone();
        window.focus(&handle);
        self.search.update(cx, |input, cx| input.select_all_text(cx));
        cx.notify();
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // One key, unwinding one layer at a time: the folder prompt, the
        // context menu, the account editor, then settings, the palette,
        // search, and finally back to the list.
        if self.folder_prompt.is_some() {
            self.close_folder_prompt(window, cx);
            return;
        }
        if self.folder_menu.take().is_some() {
            cx.notify();
            return;
        }
        if self.palette.take().is_some() {
            window.focus(&self.list_focus);
        } else if !self.search_text.is_empty() {
            self.search.update(cx, |input, cx| input.clear(cx));
            window.focus(&self.list_focus);
        } else {
            window.focus(&self.list_focus);
        }
        cx.notify();
    }

    /// Commits a theme choice and writes it to disk.
    pub fn choose_theme(&mut self, id: ThemeId, cx: &mut Context<Self>) {
        self.settings.theme = id;
        self.apply_theme(id, cx);

        // Saving touches the filesystem, so it goes to a background thread —
        // the UI has already switched and does not depend on the outcome.
        let settings = self.settings.clone();
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move { settings.save() })
                .await;

            if let Err(e) = saved {
                this.update(cx, |this, cx| {
                    this.status = format!("Could not save settings: {e}");
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub fn choose_density(&mut self, density: crate::settings::Density, cx: &mut Context<Self>) {
        if self.settings.messages.density == density {
            return;
        }
        self.settings.messages.density = density;
        self.save_settings(cx);
    }

    pub fn choose_preview(&mut self, preview: crate::settings::Preview, cx: &mut Context<Self>) {
        if self.settings.messages.preview == preview {
            return;
        }
        self.settings.messages.preview = preview;
        self.save_settings(cx);
    }

    pub fn toggle_column(&mut self, column: crate::settings::Column, cx: &mut Context<Self>) {
        self.settings.messages.columns.toggle(column);
        self.save_settings(cx);
    }

    /// Switches between the two list shapes, for the keymap and the palette.
    fn toggle_density(&mut self, _: &ToggleDensity, _: &mut Window, cx: &mut Context<Self>) {
        use crate::settings::Density;
        let next = match self.settings.messages.density {
            Density::Slim => Density::Detail,
            Density::Detail => Density::Slim,
        };
        self.status = format!("{} message list", next.label());
        self.choose_density(next, cx);
    }

    fn toggle_conversations(
        &mut self,
        _: &ToggleConversations,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let on = !self.settings.messages.conversations;
        self.settings.messages.conversations = on;
        self.status = match on {
            true => "Grouping into conversations".into(),
            false => "Showing every message".into(),
        };

        // The cursor was on a row that is about to mean something else — a
        // conversation becoming its newest message, or the reverse — so it
        // goes back to the top rather than to whatever now sits at that index.
        self.chosen = None;
        self.selected = 0;
        self.marked.clear();

        self.save_settings(cx);
        self.reload(cx);
        cx.notify();
    }

    /// Persists the current settings, off the UI thread.
    ///
    /// The row height changed the instant the setting did, so the scroll
    /// position now refers to a different offset; re-anchoring on the selected
    /// row is what stops a density switch from teleporting you somewhere else
    /// in the mailbox.
    pub fn save_settings(&mut self, cx: &mut Context<Self>) {
        // A width or a density change moves every row, so the selection is
        // brought back into view; that is not wanted for a preference that
        // changes nothing about the list.
        self.list_scroll.scroll_to_item(self.selected, gpui::ScrollStrategy::Center);
        cx.notify();
        self.persist_settings(cx);
    }

    /// Writes the settings out without touching anything on screen.
    pub(crate) fn persist_settings(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        cx.spawn(async move |this, cx| {
            let saved = cx.background_executor().spawn(async move { settings.save() }).await;
            if let Err(e) = saved {
                this.update(cx, |this, cx| {
                    this.status = format!("Could not save settings: {e}");
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Everything that has to happen while the mailbox window still exists.
    ///
    /// Both ways out lead here — the red button through
    /// `on_window_should_close`, ⌘W through [`Self::close_action`] — because
    /// the two are the same event as far as the drafts and the geometry are
    /// concerned, and only one of them is a gpui action.
    pub fn window_closing(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.save_open_drafts(cx);
        self.remember_window(window);
    }

    /// Writes down where the window is, now, before it goes.
    ///
    /// Saved synchronously: closing the last window quits, and a write handed
    /// to the background executor at that point is a write that may not
    /// happen.
    fn remember_window(&mut self, window: &Window) {
        let (bounds, maximized) = match window.window_bounds() {
            WindowBounds::Windowed(bounds) => (bounds, false),
            // Both remember the rectangle to un-maximize back to; only the
            // maximized case reopens filling the screen.
            WindowBounds::Maximized(bounds) => (bounds, true),
            WindowBounds::Fullscreen(bounds) => (bounds, false),
        };

        let state = WindowState {
            x: f32::from(bounds.origin.x),
            y: f32::from(bounds.origin.y),
            width: f32::from(bounds.size.width),
            height: f32::from(bounds.size.height),
            maximized,
        };

        if self.settings.window == Some(state) {
            return;
        }
        self.settings.window = Some(state);

        // Nothing to tell the user about: the window they would be told in
        // is the one on its way out.
        if let Err(e) = self.settings.save() {
            log::warn!("could not remember the window position: {e:#}");
        }
    }

    /// ⌘W on the mailbox.
    ///
    /// Handled here rather than by the app-wide fallback in `menus` so the
    /// window is measured before it is taken away; the fallback removes the
    /// window and nothing else, which is right for a window with nothing to
    /// remember and wrong for this one.
    fn close_action(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.window_closing(window, cx);
        window.remove_window();
    }

    /// Installs a palette as the global and repaints every window.
    pub(crate) fn apply_theme(&mut self, id: ThemeId, cx: &mut Context<Self>) {
        // Swapping the global is skipped when it already holds this palette,
        // but the notify is not: clicking a card you were already previewing
        // changes which one is checked even though the colors do not move.
        if cx.theme().id != id {
            cx.set_global(ActiveTheme(Theme::new(id)));
            cx.refresh_windows();
        }
        cx.notify();
    }

    fn resume_draft_action(&mut self, _: &ResumeDraft, _: &mut Window, cx: &mut Context<Self>) {
        self.resume_draft(cx);
    }

    fn compose_action(&mut self, _: &Compose, _: &mut Window, cx: &mut Context<Self>) {
        self.open_compose(cx);
    }

    fn reply_action(&mut self, _: &Reply, _: &mut Window, cx: &mut Context<Self>) {
        self.open_reply(spool_core::draft::ReplyKind::Sender, cx);
    }

    fn reply_all_action(&mut self, _: &ReplyAll, _: &mut Window, cx: &mut Context<Self>) {
        self.open_reply(spool_core::draft::ReplyKind::All, cx);
    }

    fn forward_action(&mut self, _: &Forward, _: &mut Window, cx: &mut Context<Self>) {
        self.open_forward(cx);
    }

    /// Picks up whatever was left unsent when the app last closed.
    ///
    /// The draft is not opened — it is held until the next composer, and the
    /// status line says it is there. A half-written message is something you
    /// left, not something to be handed back the instant the window opens, and
    /// now that a composer is a window of its own that would mean one
    /// appearing over everything at start-up.
    fn restore_draft(&mut self, cx: &mut Context<Self>) {
        let Ok(mut drafts) = self.store.drafts() else { return };
        if drafts.is_empty() {
            return;
        }

        // The composer holds one draft, so the newest is the one to bring
        // back. Any others stay in the table rather than being cleaned up:
        // deleting something the user wrote, to tidy up, is not a trade this
        // app gets to make.
        let (id, draft) = drafts.remove(0);

        // Sticky, because the first reload of the message list lands moments
        // after this and would otherwise clear it before it was read.
        self.set_sticky_status(match draft.subject.trim() {
            "" => "Unsent draft waiting \u{2014} cmd-shift-d to keep writing".into(),
            subject => format!(
                "Unsent draft waiting: \u{201c}{}\u{201d} \u{2014} cmd-shift-d to keep writing",
                truncate(subject, 40)
            ),
        });
        self.restored_draft = Some((id, draft));
        cx.notify();
    }

    /// Marks a message answered, which is the flag every client shows as a
    /// little arrow.
    ///
    /// Called by a composer window once its send has gone through: the
    /// composer knows the send succeeded and the mailbox owns the row.
    /// Takes a draft off a composer's hands and delivers it.
    ///
    /// The mailbox window owns the send rather than the composer, and that is
    /// the whole point of it: the composer flies off the screen the moment the
    /// button is pressed, and a task belonging to a window that has closed is
    /// a task that gets dropped mid-flight — the message would go nowhere,
    /// silently. This outlives the window it came from.
    ///
    /// The draft row is deliberately left in the store until the server has
    /// taken the message. A send that fails must leave something to send
    /// again, and the draft is that something.
    pub fn send_draft(
        &mut self,
        draft: spool_core::draft::Draft,
        draft_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        if self.demo {
            self.say(
                "The demo mailbox does not send mail. The draft is kept: cmd-shift-d to reopen it.",
                cx,
            );
            if let Some(id) = draft_id {
                self.restored_draft = Some((id, draft));
            }
            return;
        }

        let store = self.store.clone();
        let answered = draft.replying_to;
        let forwarded = draft.forwarding;
        let subject = draft.subject.trim().to_string();

        self.activities.begin(ActivityKind::Send, "Sending\u{2026}");
        cx.notify();

        // Detached, not held: this must run to the end whatever happens to the
        // windows around it, including the one that started it.
        cx.spawn(async move |this, cx| {
            let sending = crate::compose::deliver(store.clone(), draft.clone());
            let sent = cx.background_executor().spawn(sending).await;

            this.update(cx, |this, cx| {
                this.activities.end(ActivityKind::Send);

                match sent {
                    Ok(()) => {
                        // Only now: the message is somewhere other than here.
                        if let Some(id) = draft_id
                            && let Err(e) = store.delete_draft(id)
                        {
                            log::error!("sent, but the draft would not delete: {e}");
                        }
                        this.status = match subject.as_str() {
                            "" => "Message sent".into(),
                            subject => format!("Sent \u{201c}{subject}\u{201d}"),
                        };
                        // The copy filed in Sent is on screen only once the
                        // folder counts and the list have caught up with it.
                        this.refresh_folders_async(cx);
                        this.reload(cx);
                        if let Some(id) = answered {
                            this.mark_answered(id, cx);
                        }
                        if let Some(id) = forwarded {
                            this.mark_forwarded(id, cx);
                        }
                    }
                    Err(e) => {
                        // Said in full, and the draft named as still being
                        // here: the window that would have shown this error
                        // has already gone, so this line is the only thing
                        // standing between the sender and believing it went.
                        this.status = format!(
                            "Not sent \u{2014} {:#}. The draft is kept: cmd-shift-d to try again.",
                            e
                        );
                        if let Some(id) = draft_id {
                            this.restored_draft = Some((id, draft));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn mark_answered(&mut self, id: MessageId, cx: &mut Context<Self>) {
        self.set_flag(id, Flags::ANSWERED, cx);
    }

    /// Marks a message forwarded, the companion to [`Workspace::mark_answered`].
    ///
    /// Not pushed to the server, and that is why it does not call
    /// `schedule_push`: `$Forwarded` is a keyword rather than an IMAP flag,
    /// and a server that does not take keywords rejects the whole `STORE` —
    /// which would cost every `\Seen` in the same batch. The bit is kept
    /// locally instead, carried across flag syncs by `Flags::LOCAL_ONLY`, and
    /// read back from the server when another client has set it.
    pub fn mark_forwarded(&mut self, id: MessageId, cx: &mut Context<Self>) {
        self.set_flag(id, Flags::FORWARDED, cx);
    }

    /// Turns one flag bit on, in the row on screen and in the store.
    fn set_flag(&mut self, id: MessageId, bit: u32, cx: &mut Context<Self>) {
        let Some(envelope) = self.envelopes.iter_mut().find(|e| e.id == id) else { return };
        let flags = envelope.flags.with(bit, true);
        envelope.flags = flags;

        // A local-only bit is written without queueing anything to push: see
        // `Store::set_local_flags` for why an entry nobody drains is worse
        // than no entry at all.
        let local = bit & Flags::LOCAL_ONLY != 0;
        let store = self.store.clone();
        cx.background_executor()
            .spawn(async move {
                match local {
                    true => store.set_local_flags(id, flags).ok(),
                    false => store.set_flags(id, flags).ok(),
                };
            })
            .detach();
        if !local {
            self.schedule_push(cx);
        }
        cx.notify();
    }

    // ---- updates ----

    /// Looks for a newer Spool.
    ///
    /// `asked_for` separates the two callers. A check someone chose from the
    /// menu owes them an answer either way, including "you are up to date".
    /// The one on launch owes them nothing unless it finds something: an app
    /// that reports its own health every time you open it is an app that has
    /// made its maintenance your problem.
    pub fn check_for_updates(&mut self, asked_for: bool, cx: &mut Context<Self>) {
        // A check already running is the answer to a second press.
        if matches!(
            self.update.as_ref().map(|u| &u.stage),
            Some(Stage::Checking | Stage::Downloading(_) | Stage::Installing(_))
        ) {
            return;
        }
        // A background check has nothing to add once an update is known about
        // — and replacing a downloaded, verified one with "found" again would
        // throw the download away.
        if !asked_for && matches!(self.update.as_ref().map(|u| &u.stage), Some(Stage::Found(_) | Stage::Ready(_))) {
            return;
        }

        if asked_for {
            self.update = Some(UpdateState::new(Stage::Checking));
            cx.notify();
        }

        cx.spawn(async move |this, cx| {
            let found = crate::update::check().await;

            this.update(cx, |this, cx| {
                this.update = match found {
                    Ok(Some(release)) => Some(UpdateState::new(Stage::Found(release))),
                    Ok(None) if asked_for => Some(UpdateState::new(Stage::UpToDate)),
                    Ok(None) => None,
                    // A background check that could not reach the network is
                    // not worth a word: the machine was asleep, or the wifi
                    // is a captive portal, and either way it will run again.
                    Err(_) if !asked_for => None,
                    Err(e) => Some(UpdateState::new(Stage::Failed(format!(
                        "Could not check for updates: {e:#}"
                    )))),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pressing the row: downloads and verifies, or installs what is ready.
    pub fn advance_update(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.update.as_ref() else { return };

        let installable = state.installable;

        match state.stage.clone() {
            Stage::Found(release) if !installable => {
                crate::accounts_ui::open_in_browser(&release.page_url);
            }
            Stage::Found(release) => self.download_update(release, true, cx),
            Stage::Ready(release) => self.install_update(release, cx),
            Stage::Failed(_) => self.check_for_updates(true, cx),
            _ => {}
        }
    }

    /// Fetches the disk image and proves it is ours before saying it is ready.
    ///
    /// Downloading and verifying are one step from the outside because they
    /// are one decision: an image that does not verify is not something to
    /// offer, it is a download that failed.
    ///
    /// With `then_install`, a verified download is installed straight away —
    /// what one press of Update Now means. Nothing is installed that did not
    /// verify.
    fn download_update(&mut self, release: crate::update::Release, then_install: bool, cx: &mut Context<Self>) {
        self.update = Some(UpdateState::new(Stage::Downloading(release.clone())));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let dmg = crate::update::download(&release).await;

            // Mounting and the three checks are blocking work -- four
            // subprocesses -- and belong off the frame thread.
            let verified = match dmg {
                Ok(path) => {
                    cx.background_executor()
                        .spawn(async move {
                            let mounted = crate::update::verify(&path);
                            // The image is on disk either way and has served
                            // its purpose; what is mounted is what gets
                            // installed.
                            std::fs::remove_file(&path).ok();
                            mounted
                        })
                        .await
                }
                Err(e) => Err(e),
            };

            this.update(cx, |this, cx| {
                let ready = verified.is_ok();
                this.update = Some(match verified {
                    Ok(mounted) => UpdateState {
                        mounted: Some(mounted),
                        ..UpdateState::new(Stage::Ready(release.clone()))
                    },
                    Err(e) => UpdateState::new(Stage::Failed(format!("{e:#}"))),
                });
                if ready && then_install {
                    this.install_update(release, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Puts the new version in place and restarts into it.
    fn install_update(&mut self, release: crate::update::Release, cx: &mut Context<Self>) {
        let Some(mounted) = self.update.as_mut().and_then(|u| u.mounted.take()) else {
            return;
        };

        // Written out first. The quit below is not a request -- the process
        // ends -- and an update is the worst possible moment to lose a draft.
        self.save_open_drafts(cx);
        self.update = Some(UpdateState::new(Stage::Installing(release)));
        cx.notify();

        cx.spawn(async move |this, cx| {
            // Off the frame thread: this copies thirty megabytes with `ditto`
            // and waits on three more subprocesses. Run here, the frame that
            // says "Installing" would never be painted, because the thread
            // that would paint it is the one doing the copying.
            let installed = cx
                .background_executor()
                .spawn(async move { crate::update::install(&mounted) })
                .await;

            this.update(cx, |this, cx| match installed {
                // The new copy is already starting. This one is now the old
                // version running out of a bundle that has been replaced, and
                // the sooner it is gone the better.
                Ok(()) => cx.quit(),
                Err(e) => {
                    this.update = Some(UpdateState::new(Stage::Failed(format!("{e:#}"))));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn check_for_updates_action(
        &mut self,
        _: &CheckForUpdates,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.check_for_updates(true, cx);
    }

    /// Current width of the sidebar.
    pub fn sidebar_width(&self) -> f32 {
        self.settings.panes.sidebar
    }

    /// Current width of the message list, which depends on the density.
    pub fn list_width(&self) -> f32 {
        self.settings.panes.list(self.settings.messages.density)
    }

    pub fn begin_drag(&mut self, divider: Divider, at: gpui::Pixels, cx: &mut Context<Self>) {
        let start_width = match divider {
            Divider::Sidebar => self.sidebar_width(),
            Divider::List => self.list_width(),
        };
        self.dragging = Some(Drag { divider, start_x: at, start_width });
        cx.notify();
    }

    /// Applies a drag. Called from the root, which is where the pointer is
    /// once it has left the four-pixel divider it grabbed.
    pub fn drag_to(&mut self, at: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(drag) = self.dragging else { return };
        let delta = f32::from(at - drag.start_x);
        let width = drag.start_width + delta;

        let density = self.settings.messages.density;
        match drag.divider {
            Divider::Sidebar => self.settings.panes.set_sidebar(width),
            Divider::List => self.settings.panes.set_list(density, width),
        }
        cx.notify();
    }

    /// Ends a drag and writes the width to disk.
    pub fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging.take().is_none() {
            return;
        }
        self.save_settings(cx);
        cx.notify();
    }

    /// Restores a divider to its default. Bound to a double-click, which is
    /// the gesture every resizable pane has taught people to try.
    pub fn reset_divider(&mut self, divider: Divider, cx: &mut Context<Self>) {
        let defaults = crate::settings::PaneWidths::default();
        let density = self.settings.messages.density;

        match divider {
            Divider::Sidebar => self.settings.panes.sidebar = defaults.sidebar,
            Divider::List => self.settings.panes.set_list(density, defaults.list(density)),
        }
        self.save_settings(cx);
        cx.notify();
    }

    /// A draggable divider between two panes.
    ///
    /// Four pixels wide but with a wider invisible grab area, because a
    /// hairline is the correct thing to draw and the wrong thing to aim at.
    pub(crate) fn render_divider(
        &self,
        divider: Divider,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.dragging.map(|d| d.divider) == Some(divider);
        let id = match divider {
            Divider::Sidebar => "divider-sidebar",
            Divider::List => "divider-list",
        };

        div()
            .id(id)
            .w(px(5.))
            .flex_shrink_0()
            .h_full()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .when(active, |s| s.bg(theme.divider_active))
            .when(!active, |s| s.hover(|s| s.bg(theme.divider_hover)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        this.reset_divider(divider, cx);
                    } else {
                        this.begin_drag(divider, event.position.x, cx);
                    }
                }),
            )
    }

    /// Whether this attachment is already being fetched.
    pub(crate) fn attachment_busy(&self, message: MessageId, index: usize) -> bool {
        self.attachment_progress.contains_key(&(message, index))
    }

    /// Runs `work` with the chip showing how far along it is.
    ///
    /// Claims the chip first and releases it when `work` finishes, so the
    /// disabled state cannot outlive the download that justified it. Work that
    /// carries on afterwards — a preview panel someone leaves open — releases
    /// the chip itself, at the point the download is actually done. Returns
    /// without doing anything if the chip is already claimed, which is the
    /// whole of "only once": enforced where the state lives rather than by
    /// hoping the button is drawn inert.
    fn with_attachment_progress<F>(
        &mut self,
        message: MessageId,
        index: usize,
        cx: &mut Context<Self>,
        work: F,
    ) where
        F: FnOnce(&mut Context<Self>, Arc<AtomicU64>, u32, u64) -> Task<()>,
    {
        if self.attachment_busy(message, index) {
            log::debug!("attachment {index} of message {} is already loading", message.0);
            return;
        }

        let expected = self
            .store
            .message_origin(message)
            .ok()
            .flatten()
            .map(|origin| origin.size)
            .unwrap_or(0);

        self.next_attachment_claim += 1;
        let claim = self.next_attachment_claim;
        self.attachment_progress.insert((message, index), AttachmentLoad { claim, fraction: 0. });
        cx.notify();

        let meter = Arc::new(AtomicU64::new(0));
        let task = work(cx, meter.clone(), expected, claim);

        // Polled rather than pushed: the meter is written by whichever thread
        // is reading the socket, and a channel per read would cost more than
        // the number it carries is worth.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PROGRESS_POLL).await;
                let running = this.update(cx, |this, cx| {
                    let Some(load) = this.attachment_progress.get_mut(&(message, index)) else {
                        return false;
                    };
                    // Somebody else's download now. Stop rather than drive
                    // their bar from this one's meter.
                    if load.claim != claim {
                        return false;
                    }
                    if expected > 0 {
                        let read = meter.load(Ordering::Relaxed) as f32;
                        // Never quite 1: the message is still being parsed
                        // after the last byte lands, and a bar that sits full
                        // while something is still happening is a lie.
                        load.fraction = (read / expected as f32).clamp(0., 0.99);
                    }
                    cx.notify();
                    true
                });
                if !matches!(running, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            task.await;
            this.update(cx, |this, cx| this.release_attachment(message, index, claim, cx)).ok();
        })
        .detach();
    }

    /// Gives a chip back, if this is still the claim holding it.
    fn release_attachment(
        &mut self,
        message: MessageId,
        index: usize,
        claim: u64,
        cx: &mut Context<Self>,
    ) {
        let key = (message, index);
        if self.attachment_progress.get(&key).is_some_and(|load| load.claim == claim) {
            self.attachment_progress.remove(&key);
            cx.notify();
        }
    }

    /// An attachment's bytes, from the store if sync kept them and from the
    /// server if it did not.
    ///
    /// Anything past the size cap arrives with its name and no contents, which
    /// is most of a mail client's large attachments — a 15 MB PDF is exactly
    /// the kind nobody wants to lose and exactly the kind the cap excludes.
    /// Rather than refuse, go and get it.
    ///
    /// Not written back to the store afterwards: the cap is a decision about
    /// disk, and this is what it leaves room for.
    pub(crate) async fn attachment_bytes_anywhere(
        store: Store,
        accounts: Vec<spool_net::AccountConfig>,
        message: MessageId,
        index: usize,
        meter: Arc<AtomicU64>,
    ) -> anyhow::Result<Vec<u8>> {
        if let Some(bytes) = store.attachment_bytes(message, index)? {
            return Ok(bytes);
        }

        let origin = store
            .message_origin(message)?
            .context("this message did not come from a server, so its attachment is gone")?;
        let config = accounts
            .into_iter()
            .find(|account| account.email.eq_ignore_ascii_case(&origin.email))
            .with_context(|| format!("{} is no longer configured", origin.email))?;

        log::debug!(
            "fetching attachment {index} of message {} from {} uid {}",
            message.0,
            origin.mailbox,
            origin.uid,
        );
        spool_net::fetch_attachment(&config, &origin, index, meter)
            .await
            .context("fetching it from the server")
    }

    /// The accounts as configured, for a fetch that has to go to the server.
    pub(crate) fn account_configs(&self) -> Vec<spool_net::AccountConfig> {
        spool_net::Config::load(spool_net::Config::default_path())
            .map(|config| config.accounts)
            .unwrap_or_default()
    }

    /// Says something to whoever is looking, whether or not the status bar is.
    ///
    /// Writes the status line as before — it is still the right place for this
    /// when the bar is on — and additionally raises a toast when it is not. An
    /// action that reports only into hidden chrome is an action that, from the
    /// outside, did nothing.
    pub(crate) fn say(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        let text = text.into();
        self.status = text.clone();
        self.toast = Some((SharedString::from(text), Instant::now()));
        cx.notify();

        self.toast_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TOAST_LINGER).await;
            this.update(cx, |this, cx| {
                if this.toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST_LINGER) {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The toast, when there is one and nothing else is showing it.
    fn render_toast(&self, theme: &Theme) -> Option<gpui::AnyElement> {
        // With the bar on, this would be the same sentence twice, a few
        // pixels apart.
        if self.settings.status_bar {
            return None;
        }
        let (text, _) = self.toast.as_ref()?;
        Some(
            // No id and no handlers, so it has no hitbox and cannot swallow a
            // click meant for the message underneath it.
            div()
                .absolute()
                .bottom(px(18.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(460.))
                        .truncate()
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(theme.border)
                        .text_size(theme.small_text_size)
                        .text_color(theme.text)
                        .child(text.clone()),
                )
                .into_any_element(),
        )
    }

    /// The open message's attachments that actually have bytes.
    ///
    /// Embedded images are left out for the same reason the chips leave them
    /// out — they are already in the body — and so is anything past the size
    /// cap, which kept its name and nothing else.
    fn open_attachments(&self) -> Option<(MessageId, Vec<(usize, String)>)> {
        let open = self.open.as_ref()?;
        let files = open
            .body
            // Not filtered by what is stored any more: an attachment past
            // the size cap can be fetched from the server, and leaving it out
            // here is what made "too large to store" mean "gone".
            .files()
            .map(|(index, attachment)| (index, attachment.name.clone()))
            .collect();
        Some((open.envelope.id, files))
    }

    /// Writes every attachment on the open message to Downloads, and shows
    /// them in Finder.
    ///
    /// Finder coming forward with the files selected is the whole point of the
    /// reveal: it is confirmation that cannot be missed, on a machine where
    /// the status line this used to write to is hidden by default.
    fn save_attachments_action(
        &mut self,
        _: &SaveAttachments,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((message, files)) = self.open_attachments() else {
            self.say("Open a message first", cx);
            return;
        };
        if files.is_empty() {
            self.say("Nothing to save: this message has no stored attachments", cx);
            return;
        }

        log::debug!("saving {} attachment(s) of message {}", files.len(), message.0);
        let store = self.store.clone();
        let accounts = self.account_configs();
        let count = files.len();

        // Every chip this touches goes inert for the duration, which is also
        // what stops a second ⌘⇧S starting the same downloads again. No
        // percentage: one meter across several files would be measuring a
        // total nothing on screen represents.
        if files.iter().any(|(index, _)| self.attachment_busy(message, *index)) {
            self.say("Already saving", cx);
            return;
        }
        self.next_attachment_claim += 1;
        let claim = self.next_attachment_claim;
        for (index, _) in &files {
            self.attachment_progress
                .insert((message, *index), AttachmentLoad { claim, fraction: 0. });
        }
        let claimed: Vec<usize> = files.iter().map(|(index, _)| *index).collect();
        let meter = Arc::new(AtomicU64::new(0));
        if let Some((index, _)) = files.first() {
            self.note_attachment(message, *index, "Saving\u{2026}", NoteTone::Working, cx);
        }

        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let mut paths = Vec::new();
                    let mut failures = Vec::new();
                    for (index, name) in files {
                        let fetched = Self::attachment_bytes_anywhere(
                            store.clone(),
                            accounts.clone(),
                            message,
                            index,
                            meter.clone(),
                        )
                        .await
                        .and_then(|bytes| write_to_downloads(&name, &bytes));
                        match fetched {
                            Ok(path) => paths.push(path),
                            Err(e) => failures.push(format!("{name}: {e:#}")),
                        }
                    }
                    (paths, failures)
                })
                .await;

            let (paths, failures) = saved;
            this.update(cx, |this, cx| {
                for index in claimed {
                    this.release_attachment(message, index, claim, cx);
                }
                for failure in &failures {
                    log::warn!("could not save attachment — {failure}");
                }
                if paths.is_empty() {
                    this.say(format!("Could not save: {}", failures.join("; ")), cx);
                    return;
                }

                let said = match failures.is_empty() {
                    true => format!("Saved {count} to Downloads"),
                    false => format!("Saved {} of {count} to Downloads", paths.len()),
                };
                this.say(said, cx);
                // One reveal for all of them, so Finder opens once with the
                // whole set selected rather than once per file.
                std::process::Command::new("/usr/bin/open")
                    .arg("-R")
                    .args(&paths)
                    .spawn()
                    .ok();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Quick Looks the open message's first attachment.
    fn preview_attachment_action(
        &mut self,
        _: &PreviewAttachment,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((message, files)) = self.open_attachments() else {
            self.say("Open a message first", cx);
            return;
        };
        let Some((index, name)) = files.into_iter().next() else {
            self.say("Nothing to preview: this message has no stored attachments", cx);
            return;
        };
        self.preview_attachment(message, index, name, cx);
    }

    /// Says on the chip itself what just happened to an attachment.
    ///
    /// `status` is not enough. It is the only channel these two had, and the
    /// status bar it writes to is off by default — so an attachment that could
    /// not be opened failed in complete silence, which is indistinguishable
    /// from a click that never landed. The note goes where the pointer already
    /// is.
    ///
    /// It also acknowledges the click before the work is done: reading a large
    /// attachment out of the store and writing it to disk takes long enough to
    /// look like nothing happening.
    pub(crate) fn note_attachment(
        &mut self,
        message: MessageId,
        index: usize,
        text: impl Into<String>,
        tone: NoteTone,
        cx: &mut Context<Self>,
    ) {
        self.attachment_note = Some(AttachmentNote {
            message,
            index,
            text: text.into(),
            tone,
        });
        cx.notify();

        // A note that stays forever becomes part of the chip. Failures linger
        // longest, because they are the ones worth reading twice.
        let linger = match tone {
            NoteTone::Working => return,
            NoteTone::Done => Duration::from_secs(4),
            NoteTone::Failed => Duration::from_secs(12),
        };
        self.attachment_note_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(linger).await;
            this.update(cx, |this, cx| {
                if this.attachment_note.as_ref().is_some_and(|n| {
                    n.message == message && n.index == index
                }) {
                    this.attachment_note = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Clears a note, if it is still the one this attachment put there.
    fn clear_attachment_note(&mut self, message: MessageId, index: usize, cx: &mut Context<Self>) {
        if self
            .attachment_note
            .as_ref()
            .is_some_and(|n| n.message == message && n.index == index)
        {
            self.attachment_note = None;
            cx.notify();
        }
    }

    /// Shows an attachment in Quick Look.
    ///
    /// The other half of what an attachment is for. Saving answers "I want to
    /// keep this"; most of the time the question is "what is it", and the
    /// answer should not require a trip through the Downloads folder and back.
    ///
    /// Quick Look rather than `open`, which is the distinction the whole of
    /// [`Workspace::save_attachment`] rests on: `qlmanage` renders the file in
    /// the system's own previewer instead of handing it to whichever
    /// application claims the extension. A crafted attachment does not get to
    /// choose what processes it, which is the property worth keeping. It is
    /// not nothing — Quick Look has had its own holes — but it is a far
    /// smaller surface than every installed app that ever registered a type.
    ///
    /// The bytes have to reach the filesystem for this, since Quick Look takes
    /// a path. They go to a private directory this process owns, readable by
    /// nobody else, and are deleted as soon as the preview closes.
    /// Turns a press on an attachment chip into a drag out of the window,
    /// once the pointer has moved far enough to mean it.
    pub(crate) fn drag_attachment_out(
        &mut self,
        event: &gpui::MouseMoveEvent,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(gpui::MouseButton::Left) {
            self.attachment_press = None;
            return;
        }
        let Some(press) = &self.attachment_press else { return };
        let moved = event.position - press.at;
        if f32::from(moved.x).abs() + f32::from(moved.y).abs() < ATTACHMENT_DRAG_THRESHOLD {
            return;
        }
        // Taken, so the rest of this drag cannot start a second one. AppKit
        // runs the drag from here and keeps the mouse-up, so the chip's click
        // — its preview — does not fire as well.
        let Some(press) = self.attachment_press.take() else { return };

        #[cfg(target_os = "macos")]
        crate::attachment_drag::begin(
            window,
            crate::attachment_drag::DragFile {
                store: self.store.clone(),
                accounts: self.account_configs(),
                message: press.message,
                index: press.index,
                name: press.name,
            },
        );
        #[cfg(not(target_os = "macos"))]
        let _ = (window, press);
    }

    pub fn preview_attachment(
        &mut self,
        message: MessageId,
        index: usize,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let accounts = self.account_configs();
        log::debug!("preview attachment {index} of message {}", message.0);
        self.note_attachment(message, index, "Opening\u{2026}", NoteTone::Working, cx);

        self.with_attachment_progress(message, index, cx, move |cx, meter, _, claim| {
        cx.spawn(async move |this, cx| {
            let prepared = cx
                .background_executor()
                .spawn(async move {
                    let bytes =
                        Self::attachment_bytes_anywhere(store, accounts, message, index, meter)
                            .await?;
                    write_to_preview_dir(&name, &bytes)
                })
                .await;

            let path = match prepared {
                Ok(path) => path,
                Err(e) => return Self::report_attachment_failure(&this, cx, message, index, e),
            };

            let started = std::process::Command::new(QUICK_LOOK)
                .arg("-p")
                .arg(&path)
                // qlmanage narrates to both streams whatever happens.
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();

            let mut child = match started {
                Ok(child) => child,
                Err(e) => {
                    clear_preview_dir(&path);
                    return Self::report_attachment_failure(
                        &this,
                        cx,
                        message,
                        index,
                        anyhow::Error::new(e).context("could not start Quick Look"),
                    );
                }
            };

            // The download is over and the panel is up, so the chip is done
            // being busy. Released here rather than when this task ends,
            // because this task goes on to outlive the preview window — a bar
            // still sweeping behind a document you are already reading is the
            // chip describing work that finished.
            this.update(cx, |this, cx| {
                this.release_attachment(message, index, claim, cx);
                this.clear_attachment_note(message, index, cx);
            })
            .ok();

            // Polled rather than waited on. Waiting holds a thread for as long
            // as somebody leaves the preview open, which for a long read is
            // minutes — and the executor's threads are how the rest of the app
            // fetches mail.
            loop {
                cx.background_executor().timer(PREVIEW_POLL).await;
                match child.try_wait() {
                    Ok(None) => continue,
                    // Gone, or unwaitable. Either way there is nothing left to
                    // wait for and the file should not outlive it.
                    Ok(Some(_)) | Err(_) => break,
                }
            }
            clear_preview_dir(&path);
        })
        });
    }

    /// Says why an attachment did not open, on the chip and in the log.
    ///
    /// Both, because the chip has room for a few words and the reason is
    /// sometimes a chain of them.
    fn report_attachment_failure(
        this: &gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
        message: MessageId,
        index: usize,
        error: anyhow::Error,
    ) {
        log::warn!("attachment {index} of message {}: {error:#}", message.0);
        this.update(cx, |this, cx| {
            this.say(format!("Could not open: {error:#}"), cx);
            this.note_attachment(message, index, "Could not open", NoteTone::Failed, cx);
        })
        .ok();
    }

    /// Writes an attachment to the Downloads folder and reveals it.
    ///
    /// Saving rather than opening: handing bytes that arrived in an email
    /// straight to whatever application claims the extension is how a mail
    /// client becomes an attack surface. Revealing in Finder puts the decision
    /// to open it back where it belongs.
    pub fn save_attachment(
        &mut self,
        message: MessageId,
        index: usize,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let store = self.store.clone();
        let accounts = self.account_configs();
        log::debug!("save attachment {index} of message {}", message.0);
        self.note_attachment(message, index, "Saving\u{2026}", NoteTone::Working, cx);

        // Saving ends when the file is written, so the chip is released by the
        // task finishing and this needs no claim of its own.
        self.with_attachment_progress(message, index, cx, move |cx, meter, _, _claim| {
        cx.spawn(async move |this, cx| {
            let saved = cx
                .background_executor()
                .spawn(async move {
                    let bytes =
                        Self::attachment_bytes_anywhere(store, accounts, message, index, meter)
                            .await?;
                    write_to_downloads(&name, &bytes)
                })
                .await;

            match saved {
                Ok(path) => {
                    this.update(cx, |this, cx| {
                        this.say(format!("Saved to {}", path.display()), cx);
                        this.note_attachment(
                            message,
                            index,
                            "Saved to Downloads",
                            NoteTone::Done,
                            cx,
                        );
                        // Reveal rather than open, for the reason above.
                        std::process::Command::new("/usr/bin/open")
                            .arg("-R")
                            .arg(&path)
                            .spawn()
                            .ok();
                    })
                    .ok();
                }
                Err(e) => Self::report_attachment_failure(&this, cx, message, index, e),
            }
        })
        });
    }

    fn select_all_messages_action(
        &mut self,
        _: &SelectAllMessages,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_all_messages(cx);
    }

    fn extend_next(&mut self, _: &ExtendNext, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(self.selected.saturating_add(1), cx);
    }

    fn extend_prev(&mut self, _: &ExtendPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.extend_to(self.selected.saturating_sub(1), cx);
    }

    fn next_field(&mut self, _: &NextField, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next();
        cx.notify();
    }

    fn prev_field(&mut self, _: &PrevField, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev();
        cx.notify();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_visible = !self.sidebar_visible;
        cx.notify();
    }

    fn toggle_status_bar(&mut self, _: &ToggleStatusBar, _: &mut Window, cx: &mut Context<Self>) {
        self.settings.status_bar = !self.settings.status_bar;
        // Remembered, because a readout you turned on to watch something is
        // one you want to still be there next time you look.
        self.persist_settings(cx);
        cx.notify();
    }

    fn toggle_unified_inbox(
        &mut self,
        _: &ToggleUnifiedInbox,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_unified_inbox_now(cx);
    }


    /// Shows or hides the sidebar's "All Mail" row.
    /// Flips the standing answer on remote images.
    ///
    /// Turning it on loads what is on screen straight away rather than at the
    /// next message: the answer to "show me the images" is these images.
    pub fn toggle_remote_images_now(&mut self, cx: &mut Context<Self>) {
        let on = !self.settings.load_remote_images;
        self.settings.load_remote_images = on;
        self.images.set_always(on);

        self.status = match on {
            true => "Images in messages will load".into(),
            false => "Images will be blocked until asked for".into(),
        };

        if on {
            self.load_remote_images(cx);
        }
        self.persist_settings(cx);
        cx.notify();
    }

    pub fn toggle_unified_inbox_now(&mut self, cx: &mut Context<Self>) {
        self.settings.unified_inbox = !self.settings.unified_inbox;
        self.status = match self.settings.unified_inbox {
            true => "All Mail shown".into(),
            false => "All Mail hidden".into(),
        };
        self.leave_hidden_scope(cx);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Moves off the unified view when the sidebar no longer offers it.
    ///
    /// Hiding the row you are standing on would leave the list showing every
    /// account with nothing selected and no way back to it — a state the
    /// sidebar cannot explain and the user cannot leave except by picking
    /// another folder, which is what this does for them.
    fn leave_hidden_scope(&mut self, cx: &mut Context<Self>) {
        if self.settings.unified_inbox || self.scope != Scope::Everything {
            return;
        }
        let Some(inbox) = self.mailboxes.iter().find(|m| m.role == MailboxRole::Inbox) else {
            // No inbox yet — a first run, mid-sync. The folders arriving is
            // what calls this again.
            return;
        };
        self.select_scope(Scope::Mailbox(inbox.id), cx);
    }

    /// Opens whatever was folded over a folder, so the sidebar can show it.
    ///
    /// Its ancestors are its own path with the last segment removed, one
    /// segment at a time — the account's other folders are not consulted,
    /// because a container that only exists as a prefix has the same identity
    /// either way.
    fn reveal_mailbox(&mut self, id: MailboxId, cx: &mut Context<Self>) {
        let Some(mailbox) = self.mailboxes.iter().find(|m| m.id == id) else { return };
        let Some(account) = self.accounts.iter().find(|a| a.id == mailbox.account_id) else {
            return;
        };
        let (email, path) = (account.email.clone(), mailbox.path.clone());

        let mut changed = self.settings.sidebar.account_collapsed(&email);
        if changed {
            self.settings.sidebar.toggle_account(&email);
        }

        // Every prefix of the path, however the server spells its delimiter:
        // one of the two characters is the separator and the other cannot
        // appear where this would matter.
        let mut ancestor = path.as_str();
        while let Some((parent, _)) = ancestor.rsplit_once(['/', '.']) {
            if self.settings.sidebar.folder_collapsed(&email, parent) {
                self.settings.sidebar.toggle_folder(&email, parent);
                changed = true;
            }
            ancestor = parent;
        }

        if changed {
            self.persist_settings(cx);
        }
    }

    /// Folds an account's folder list away, or brings it back.
    pub fn toggle_account_collapsed(&mut self, email: &str, cx: &mut Context<Self>) {
        self.settings.sidebar.toggle_account(email);
        self.persist_settings(cx);
        cx.notify();
    }

    /// Folds one folder's children away, or brings them back.
    pub fn toggle_folder_collapsed(&mut self, email: &str, path: &str, cx: &mut Context<Self>) {
        self.settings.sidebar.toggle_folder(email, path);
        self.persist_settings(cx);
        cx.notify();
    }

    fn copy_sender(&mut self, _: &CopySender, _: &mut Window, cx: &mut Context<Self>) {
        let Some(envelope) = self.selected_envelope() else { return };
        let addr = envelope.from.addr.clone();
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(addr.clone()));
        self.status = format!("Copied {addr}");
        cx.notify();
    }

    /// Sends what the local store owes the server, a moment from now.
    ///
    /// Called by every action that changes something a server also knows
    /// about. The delay is a debounce, not politeness: mail actions come in
    /// bursts — archive, archive, archive down a list — and each one replacing
    /// the last pending send is what turns the burst into a single connection
    /// carrying a single command.
    ///
    /// Nothing here reports success. A change that reached the server is the
    /// expected outcome and does not deserve to overwrite the status bar; only
    /// a failure interrupts.
    pub fn schedule_push(&mut self, cx: &mut Context<Self>) {
        const DEBOUNCE: Duration = Duration::from_secs(3);

        // The demo's changes have no server to go to. They stay queued in its
        // store, which is thrown away with it.
        if self.demo {
            return;
        }

        self.push_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;

            let store = match this.update(cx, |this, _| this.store.clone()) {
                Ok(store) => store,
                Err(_) => return,
            };

            // Said out loud only once there is something to say. Most
            // presses of `e` owe the server one command and are gone before
            // the eye finds the sidebar; an archive of forty messages over a
            // slow connection is the case this exists for.
            let owed = {
                let store = match this.update(cx, |this, _| this.store.clone()) {
                    Ok(store) => store,
                    Err(_) => return,
                };
                cx.background_executor()
                    .spawn(async move { pending_everywhere(&store) })
                    .await
            };

            if owed > 0 {
                this.update(cx, |this, cx| {
                    this.activities.begin(
                        ActivityKind::Push,
                        match owed {
                            1 => "Sending 1 change\u{2026}".to_string(),
                            n => format!("Sending {n} changes\u{2026}"),
                        },
                    );
                    cx.notify();
                })
                .ok();
            }

            let failures = cx
                .background_executor()
                .spawn(async move {
                    // Read here rather than held on the workspace: the file is
                    // the truth about which accounts exist, and it can have
                    // changed since launch.
                    let Ok(config) = Config::load(Config::default_path()) else {
                        return Vec::new();
                    };

                    let mut failures = Vec::new();
                    for account in &config.accounts {
                        match spool_net::push_account(store.clone(), account).await {
                            Ok(Some(report)) if report.failed > 0 => {
                                failures.push(format!("{}: {}", account.name, report.summary()));
                            }
                            Ok(_) => {}
                            Err(e) => {
                                failures.push(format!("{}: {e:#}", account.name));
                            }
                        }
                    }
                    failures
                })
                .await;

            this.update(cx, |this, cx| {
                this.activities.end(ActivityKind::Push);
                // Not while a sync is talking: it is drawing the same queue
                // down and its own report is the more complete one.
                if !failures.is_empty() && !this.syncing {
                    this.status = failures.join(" \u{00b7} ");
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Everything: the folder list, and every folder in it.
    fn sync(&mut self, _: &Sync, _: &mut Window, cx: &mut Context<Self>) {
        // Pressed, so the result is being watched and needs no banner.
        self.asked_for_sync = true;
        self.start_sync(SyncMode::Everything, cx);
    }

    /// Just the inboxes, which is what "is there anything new" means.
    fn get_new_mail(&mut self, _: &GetNewMail, _: &mut Window, cx: &mut Context<Self>) {
        self.asked_for_sync = true;
        self.start_sync(SyncMode::Inboxes, cx);
    }

    /// Opens an idle connection per account, so mail arrives without being
    /// asked for.
    ///
    /// Started once, after the configuration is known. Each watcher runs until
    /// its task is dropped, reconnecting and backing off on its own — the
    /// workspace's only job is to sync when one says something changed.
    pub fn start_watching(&mut self, cx: &mut Context<Self>) {
        if self.demo {
            return;
        }
        let Ok(config) = Config::load(Config::default_path()) else { return };

        // Rebuilt when the number of accounts changes rather than started
        // once: an account added in Settings has to be watched too, and one
        // removed has to stop being.
        if self.watch_tasks.len() == config.accounts.len() {
            return;
        }
        self.watch_tasks.clear();
        self.watch_sync_task = None;

        for account in config.accounts {
            // By address, which is what a sync is asked for by: an account's
            // name is a label and two accounts may share one.
            let name = account_key(&account.email);
            let (tx, rx) = smol::channel::unbounded::<spool_net::WatchEvent>();

            self.watch_tasks.push(cx.spawn(async move |this, cx| {
                let executor = cx.background_executor().clone();
                let watcher = cx.background_executor().spawn(async move {
                    spool_net::watch(
                        account,
                        move |event| {
                            // Unbounded, so this only fails once the receiver
                            // is gone — which is the app shutting down.
                            let _ = tx.try_send(event);
                        },
                        move |after| {
                            let executor = executor.clone();
                            Box::pin(async move { executor.timer(after).await })
                        },
                    )
                    .await;
                });

                while let Ok(event) = rx.recv().await {
                    let handled = this.update(cx, |this, cx| this.watch_event(&name, event, cx));
                    if handled.is_err() {
                        break;
                    }
                }

                // Holds the watcher for as long as this task lives; dropping
                // the task drops both.
                drop(watcher);
            }));
        }
    }

    /// What to do about something a watcher said.
    fn watch_event(&mut self, account: &str, event: spool_net::WatchEvent, cx: &mut Context<Self>) {
        use spool_net::WatchEvent;

        match event {
            // Connected and logged in, which is the answer to whatever the
            // sidebar was saying was wrong.
            WatchEvent::Watching => {
                if self.account_problems.remove(account).is_some() {
                    cx.notify();
                }
            }
            WatchEvent::Polling => {}

            WatchEvent::Changed => self.sync_soon(account.to_string(), cx),

            // Logged, and shown only once it has gone on a while. A watcher
            // retrying is normal on any machine that sleeps, and a sidebar
            // reporting every one would be reporting the weather — but by the
            // time the backoff has grown past a minute it has failed four or
            // five times running, and that is an account not getting mail.
            WatchEvent::Retrying { after, reason } => {
                log::info!("{account}: retrying in {}s ({reason})", after.as_secs());
                if after >= Duration::from_secs(60) {
                    self.account_problems.insert(account.to_string(), AccountProblem::unreachable(reason));
                    cx.notify();
                }
            }

            // Shown at once: nothing will change until somebody fixes the
            // password, and the watcher has stopped trying so as not to get
            // the address banned.
            WatchEvent::Refused { after, reason } => {
                log::warn!("{account}: login refused, next try in {}m ({reason})", after.as_secs() / 60);
                self.account_problems.insert(account.to_string(), AccountProblem::refused(reason));
                cx.notify();
            }
        }
    }

    /// Syncs an account's inbox shortly, once, however many times this is
    /// asked.
    ///
    /// A server that has just taken delivery of five messages says so five
    /// times. The delay is what turns that into one sync — of that account.
    /// It used to sync every account's inbox, which with ten accounts meant
    /// ten logins every time any one of them so much as had a flag change,
    /// including the ones this client made itself. A shared mail host counts
    /// those, and bans an address that makes too many.
    fn sync_soon(&mut self, account: String, cx: &mut Context<Self>) {
        const SETTLE: Duration = Duration::from_secs(2);

        self.pending_syncs.insert(account);
        self.watch_sync_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            this.update(cx, |this, cx| this.run_pending_syncs(cx)).ok();
        }));
    }

    /// Syncs the inboxes watchers have asked about, unless a sync is running
    /// — in which case its end comes back here.
    fn run_pending_syncs(&mut self, cx: &mut Context<Self>) {
        if self.syncing || self.pending_syncs.is_empty() {
            return;
        }
        let accounts = std::mem::take(&mut self.pending_syncs);
        // The server said mail arrived. Mail arrives in an inbox, so that is
        // all this looks at.
        self.start_sync_of(SyncMode::Inboxes, Some(accounts), cx);
    }

    /// The folder being looked at, when the view is one folder.
    ///
    /// `None` in the unified view and in an account view: "older" only means
    /// something against a single folder's UIDs, and a button that quietly
    /// backfilled whichever folder it guessed would be worse than no button.
    pub fn open_mailbox(&self) -> Option<&Mailbox> {
        match self.scope {
            Scope::Mailbox(id) => self.mailboxes.iter().find(|m| m.id == id),
            _ => None,
        }
    }

    fn load_older_action(&mut self, _: &LoadOlder, _: &mut Window, cx: &mut Context<Self>) {
        self.load_older(cx);
    }

    /// Fetches the next batch of mail older than anything stored here.
    pub fn load_older(&mut self, cx: &mut Context<Self>) {
        if self.backfilling {
            return;
        }
        let Some(mailbox) = self.open_mailbox().cloned() else {
            self.status = "Open a folder first \u{2014} \"older\" is per folder".into();
            cx.notify();
            return;
        };

        let config = match Config::load(Config::default_path()) {
            Ok(config) => config,
            Err(e) => {
                self.status = format!("Could not read accounts.toml: {e}");
                cx.notify();
                return;
            }
        };

        let Some(account) = self.accounts.iter().find(|a| a.id == mailbox.account_id).cloned()
        else {
            return;
        };
        let Some(config) = config
            .accounts
            .into_iter()
            .find(|a| a.email.eq_ignore_ascii_case(&account.email))
        else {
            self.status = format!("{} is not in accounts.toml any more", account.email);
            cx.notify();
            return;
        };

        self.backfilling = true;
        self.activities
            .begin(ActivityKind::Backfill, format!("Fetching older {}\u{2026}", mailbox.name));
        self.status = format!("Fetching older {}\u{2026}", mailbox.name);
        cx.notify();

        let store = self.store.clone();
        let path = mailbox.path.clone();
        let (mailbox_id, account_id) = (mailbox.id, mailbox.account_id);

        let label = format!("Fetching older {}\u{2026}", mailbox.name);
        let (tx, rx) = smol::channel::unbounded::<f32>();

        self.backfill_task = Some(cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move {
                spool_net::backfill(
                    store,
                    &config,
                    &path,
                    mailbox_id,
                    account_id,
                    BACKFILL_BATCH,
                    move |fraction| {
                        let _ = tx.try_send(fraction);
                    },
                )
                .await
            });

            // The search says exactly how many messages are coming, so this
            // bar fills rather than sweeping. Drained until the sender is
            // dropped, which is when the work has finished.
            while let Ok(fraction) = rx.recv().await {
                this.update(cx, |this, cx| {
                    this.activities.update(
                        ActivityKind::Backfill,
                        label.clone(),
                        Some(fraction),
                    );
                    cx.notify();
                })
                .ok();
            }

            let result = work.await;

            this.update(cx, |this, cx| {
                this.backfilling = false;
                this.activities.end(ActivityKind::Backfill);
                this.status = match &result {
                    Ok(report) => report.summary(),
                    Err(e) => format!("Could not fetch older mail: {e:#}"),
                };

                this.refresh_folders_async(cx);
                // The cursor stays where it is: the new mail lands below
                // everything on screen, and jumping to it would move someone
                // away from what they were reading to show them a decade-old
                // message.
                this.reload(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Runs an IMAP sync for every configured account.
    ///
    /// The network work happens on a background thread and reports progress
    /// through a channel; the foreground task does nothing but copy those
    /// messages into the status bar. Nothing here can block a frame.
    pub fn start_sync(&mut self, mode: SyncMode, cx: &mut Context<Self>) {
        self.start_sync_of(mode, None, cx);
    }

    /// Syncs some accounts, by address, or every account with `None`.
    pub(crate) fn start_sync_of(
        &mut self,
        mode: SyncMode,
        only: Option<std::collections::BTreeSet<String>>,
        cx: &mut Context<Self>,
    ) {
        if self.demo {
            // Only ever asked for by a person here — launch and the watchers
            // do not start one in the demo — so it gets an answer they see.
            if only.is_none() {
                self.say("The demo mailbox has no server to check", cx);
            }
            return;
        }
        if self.syncing {
            self.status = "Already syncing".into();
            cx.notify();
            return;
        }

        let mut config = match Config::load(Config::default_path()) {
            Ok(config) => config,
            Err(e) => {
                self.status = format!("Could not read accounts.toml: {e}");
                cx.notify();
                return;
            }
        };

        if config.accounts.is_empty() {
            self.status =
                "No accounts configured \u{2014} press cmd-, to add one".into();
            cx.notify();
            return;
        }

        config.accounts = accounts_to_sync(config.accounts, only.as_ref(), &self.account_problems, Instant::now());
        match only {
            Some(_) if config.accounts.is_empty() => return,
            Some(_) => {}
            // Every account is about to be looked at, which covers whatever
            // the watchers were waiting on.
            None => self.pending_syncs.clear(),
        }

        // Named for what it is doing, because the two take very different
        // amounts of time and the strip at the foot of the sidebar is the
        // only thing that says which one is running.
        let label = match mode {
            SyncMode::Everything => "Syncing\u{2026}",
            SyncMode::Inboxes => "Checking for new mail\u{2026}",
        };
        self.syncing = true;
        self.status = label.into();
        self.activities.begin(ActivityKind::Sync, label);
        self.sync_flow.begin();
        self.start_sync_flow_tick(cx);
        cx.notify();

        let store = self.store.clone();
        let (tx, rx) = smol::channel::unbounded::<SyncUpdate>();

        self.sync_task = Some(cx.spawn(async move |this, cx| {
            let accounts = config.accounts.len();
            // Named in the status line only when there is more than one, and
            // now that there is: the accounts run at the same time, so
            // "Syncing INBOX" on its own no longer says whose inbox.
            let name_them = accounts > 1;

            // One task per account rather than a loop over them. They are
            // different servers with no bearing on each other, and running
            // them one after another means every account waits for the
            // slowest one ahead of it.
            let workers: Vec<_> = config
                .accounts
                .into_iter()
                .enumerate()
                .map(|(index, account)| {
                    let store = store.clone();
                    let tx = tx.clone();

                    cx.background_executor().spawn(async move {
                        let name = account.name.clone();
                        let key = account_key(&account.email);
                        let label = name_them.then(|| name.clone());
                        // Held across the callbacks because a step that cannot
                        // say where it is should leave the bar where the last
                        // one put it, not send it back to sweeping.
                        let mut fraction = None;

                        let email = account.email.clone();

                        let result = spool_net::sync_account(store, account, mode, move |progress| {
                            if let Some(within) = fraction_within_account(&progress) {
                                fraction = Some(within);
                            }

                            // `try_send` on an unbounded channel only fails
                            // once the receiver is gone, in which case the UI
                            // has moved on and the update is not wanted.
                            let _ = tx.try_send(SyncUpdate {
                                account: index,
                                label: describe_progress(&progress, label.as_deref()),
                                fraction,
                                arrived: matches!(progress, Progress::Fetched { .. }),
                                folder: folder_step(&progress, &email),
                                folders: folder_total(&progress),
                            });
                        })
                        .await;

                        // Whether the server refused the login is decided
                        // here, while the error still has its type; past this
                        // it is a sentence.
                        let result = result.map_err(|e| SyncFailure {
                            refused: spool_net::is_login_refused(&e),
                            message: format!("{name}: {e:#}"),
                        });
                        (key, result)
                    })
                })
                .collect();

            // The senders left are the workers' own, so the drain below ends
            // when the last of them finishes. Without this it would wait on a
            // sender held by this very task.
            drop(tx);

            // Every account fills its own share of the bar. Held per account
            // rather than summed as it arrives, because updates from the
            // accounts interleave and adding them up would count one account's
            // progress several times.
            let mut fractions: Vec<Option<f32>> = vec![None; accounts];

            while let Ok(update) = rx.recv().await {
                fractions[update.account] = update.fraction;
                let fraction = Some(
                    fractions.iter().flatten().sum::<f32>() / accounts.max(1) as f32,
                );

                this.update(cx, |this, cx| {
                    // The rows that are being worked on, before anything else:
                    // a folder finishing has no label of its own, and this is
                    // the whole of what it is for.
                    if let Some(step) = update.folder {
                        let key = (step.email, step.path);
                        match step.started {
                            true => this.syncing_folders.insert(key),
                            false => this.syncing_folders.remove(&key),
                        };
                        // Counted from the event rather than from the `done`
                        // that rides along with the next folder to open: at
                        // the end of a run the last folders finish and nothing
                        // opens after them, so that number never arrives.
                        if !step.started {
                            this.sync_flow.note_finished(Instant::now());
                        }
                    }
                    if let Some(folders) = update.folders {
                        this.sync_flow.note_total(update.account, folders);
                    }

                    if !update.label.is_empty() {
                        this.status = update.label.clone();
                        this.activities.update(
                            ActivityKind::Sync,
                            update.label,
                            fraction,
                        );
                    }
                    // The sync registers folders as it discovers them, so the
                    // sidebar has to be re-read to show them. Without this the
                    // folder list stays frozen at whatever it was on launch
                    // until the whole account has finished, which on a first
                    // sync looks like folders are missing.
                    this.refresh_folders_async(cx);

                    // The list grows underneath whoever is reading it. A
                    // download of a whole mailbox takes a long time, and a
                    // list that stays at two thousand rows until the end of
                    // it is a list that looks stuck.
                    const AT_MOST_EVERY: Duration = Duration::from_secs(2);
                    if update.arrived && this.last_arrival_reload.elapsed() >= AT_MOST_EVERY {
                        this.last_arrival_reload = Instant::now();
                        this.reload(cx);
                    }
                    cx.notify();
                })
                .ok();
            }

            let mut fetched = 0usize;
            let mut pushed = spool_net::PushReport::default();
            let mut arrived: Vec<spool_net::Arrival> = Vec::new();
            let mut ingested: Vec<MessageId> = Vec::new();
            let mut reconciled = 0usize;
            let mut failures = Vec::new();

            // The accounts have been running all along; this only collects
            // what they came back with, in the order they were configured so
            // that the summary reads the same way twice.
            let mut outcomes: Vec<(String, Option<SyncFailure>)> = Vec::new();
            for worker in workers {
                let (key, result) = worker.await;
                outcomes.push((key, result.as_ref().err().cloned()));
                match result {
                    Ok(report) => {
                        fetched += report.fetched;
                        pushed.done += report.pushed.done;
                        pushed.failed += report.pushed.failed;
                        arrived.extend(report.arrived);
                        ingested.extend(report.ingested);
                        reconciled += report.reconciled;
                    }
                    Err(failure) => failures.push(failure.message),
                }
            }

            this.update(cx, |this, cx| {
                this.syncing = false;
                // Whatever the workers were in the middle of, they are not any
                // more. Cleared here rather than trusted to arrive as events:
                // a connection that dies mid-folder sends no ending, and a row
                // left spinning for the rest of the session would be a lie the
                // next sync does not correct.
                this.syncing_folders.clear();
                // Dropping the ticker stops it. The counts go with it: a bar
                // left showing the last five seconds of a sync that is over
                // is describing a window that will never drain.
                this.sync_flow_tick = None;
                this.sync_flow.begin();
                this.activities.end(ActivityKind::Sync);
                // What each account came back with is what the sidebar says
                // about it: a sync that worked clears whatever was wrong, and
                // one that failed says why until one works.
                for (key, failure) in outcomes {
                    match failure {
                        None => {
                            this.account_problems.remove(&key);
                        }
                        Some(failure) => {
                            let problem = match failure.refused {
                                true => AccountProblem::refused(failure.message),
                                false => AccountProblem::unreachable(failure.message),
                            };
                            this.account_problems.insert(key, problem);
                        }
                    }
                }
                // What was sent leads when there is any of it: after
                // archiving a stack of mail, "did that stick" is the question
                // the status bar is being read for.
                let mail_line = match (fetched, reconciled) {
                    (0, 0) => "Up to date".to_string(),
                    (0, n) => format!("{} flags updated", crate::perf::format_count(n)),
                    (n, 0) => format!("Synced {} new messages", crate::perf::format_count(n)),
                    (n, r) => format!(
                        "Synced {} new messages \u{00b7} {} flags updated",
                        crate::perf::format_count(n),
                        crate::perf::format_count(r),
                    ),
                };
                this.status = if !failures.is_empty() {
                    failures.join(" \u{00b7} ")
                } else if pushed.is_empty() {
                    mail_line
                } else {
                    format!("{} \u{00b7} {mail_line}", pushed.summary())
                };

                this.refresh_folders_async(cx);
                this.reload(cx);
                // The people index grew in the same transactions as the mail.
                crate::clients::mail_changed(cx);
                // Accounts can be added and removed while the app is running,
                // and the watchers have to follow.
                this.start_watching(cx);

                // Rules run whether or not the sync was asked for: an
                // extension that classifies mail should classify the mail that
                // arrived while you were watching it arrive.
                this.extensions_on_messages(ingested, cx);

                // Only for mail that turned up on its own. A sync somebody
                // asked for and watched finish does not need a banner telling
                // them what they were looking at.
                if !this.asked_for_sync {
                    crate::notify::post(&crate::notify::banners(&arrived, fetched));
                }
                this.asked_for_sync = false;

                // Whatever the watchers asked for while this ran.
                this.run_pending_syncs(cx);

                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.palette.take() {
            Some(_) => window.focus(&self.list_focus),
            None => self.palette = Some(PaletteState::new(window, cx)),
        }
        cx.notify();
    }

    fn new_mailbox(&mut self, _: &NewMailbox, window: &mut Window, cx: &mut Context<Self>) {
        let Some(account) = self.current_account() else {
            self.status = "Pick an account in the sidebar first".into();
            cx.notify();
            return;
        };
        self.open_folder_prompt(FolderAction::Create { account }, window, cx);
    }

    fn rename_mailbox(&mut self, _: &RenameMailbox, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mailbox) = self.selected_mailbox() else {
            self.status = "Pick the folder to rename in the sidebar".into();
            cx.notify();
            return;
        };
        self.open_folder_prompt(FolderAction::Rename { mailbox }, window, cx);
    }

    fn delete_mailbox(&mut self, _: &DeleteMailbox, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mailbox) = self.selected_mailbox() else {
            self.status = "Pick the folder to delete in the sidebar".into();
            cx.notify();
            return;
        };
        self.open_folder_prompt(FolderAction::Delete { mailbox }, window, cx);
    }

}

/// Writes bytes into the Downloads folder without overwriting anything.
///
/// A second copy of `report.pdf` becomes `report (2).pdf` rather than
/// replacing the first: mail routinely carries files with identical names, and
/// silently overwriting one is not a recoverable mistake.
fn write_to_downloads(name: &str, bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;

    let downloads = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join("Downloads"))
        .context("no home directory to save into")?;
    std::fs::create_dir_all(&downloads)
        .with_context(|| format!("creating {}", downloads.display()))?;

    let safe = sanitize_filename(name);
    let (stem, extension) = match safe.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (safe.clone(), String::new()),
    };

    let mut path = downloads.join(&safe);
    for attempt in 2..1_000 {
        if !path.exists() {
            break;
        }
        path = downloads.join(format!("{stem} ({attempt}){extension}"));
    }

    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The web view is made after launch rather than with the window.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum WebViewState {
    /// Waiting for the first message list to be on screen.
    NotYet,
    /// Asked for on the next frame.
    Coming,
    /// Made, or failed to be; `web_view` says which.
    Made,
}

/// How many rows a new list shows before the rest of it has been read. More
/// than any window is tall, few enough to read in a couple of milliseconds.
const FIRST_PAGE: usize = 300;

/// How close together two selections have to be to count as travelling.
///
/// Slower than key repeat, so holding an arrow key or backspace reads as one
/// movement rather than a series of deliberate openings.
const TRAVELLING: Duration = Duration::from_millis(250);

/// How long the cursor has to stop before the message under it is read.
///
/// Only ever waited when the cursor is already travelling, so a click or a
/// single keypress still opens with nothing added to it. Short enough that
/// arriving somewhere and looking at it does not feel delayed.
const SETTLE: Duration = Duration::from_millis(90);

/// How long a recount of the folder list waits before it runs.
///
/// Long enough that a run of deletes is one count rather than one per message,
/// short enough that the sidebar is not visibly stale after a single one.
const COUNTS_SETTLE: Duration = Duration::from_millis(200);

/// How often a running download's byte count is read.
///
/// Fast enough that the bar moves rather than steps, slow enough that it is
/// not a frame's work per frame.
const PROGRESS_POLL: Duration = Duration::from_millis(120);

/// How long a toast stays up.
///
/// Long enough to read a sentence, short enough that it is gone before it is
/// in the way.
const TOAST_LINGER: Duration = Duration::from_secs(5);

/// The system previewer. An absolute path, not a name looked up on `PATH`:
/// what this runs on a file that arrived in an email should not be decidable
/// by the environment the app happened to launch with.
const QUICK_LOOK: &str = "/usr/bin/qlmanage";

/// How often a running preview is checked for having been closed.
///
/// Nobody is waiting on this — it only decides how long a temporary file
/// outlives the window it was for.
const PREVIEW_POLL: Duration = Duration::from_millis(400);

/// Removes a previewed file and the directory made for it.
fn clear_preview_dir(path: &std::path::Path) {
    // The directory as well as the file: one is made per preview, and they
    // would otherwise accumulate for the life of the process.
    match path.parent() {
        Some(dir) => drop(std::fs::remove_dir_all(dir)),
        None => drop(std::fs::remove_file(path)),
    }
}

/// Writes bytes somewhere Quick Look can reach them and nobody else can.
///
/// Not the Downloads folder: previewing is a glance, and a glance should not
/// leave a file behind for the user to tidy up. This is a directory belonging
/// to this process, mode 0700, with the file inside it mode 0600 — an
/// attachment can be a bank statement, and `/tmp` is world-readable.
///
/// The sanitized filename is kept rather than replaced with something unique,
/// because Quick Look chooses its previewer from the extension and names the
/// window after the file. Uniqueness comes from a directory per preview, so
/// two attachments called `scan.pdf` cannot collide.
fn write_to_preview_dir(name: &str, bytes: &[u8]) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    let dir = std::env::temp_dir()
        .join(format!("spool-preview.{}", std::process::id()))
        .join(NEXT.fetch_add(1, Ordering::Relaxed).to_string());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", dir.display()))?;

    let path = dir.join(sanitize_filename(name));
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing {}", path.display()))?;
    Ok(path)
}

/// Reduces a sender-supplied filename to something safe to join onto a path.
///
/// The name came from a stranger. Without this, an attachment called
/// `../../.ssh/authorized_keys` would be written exactly there.
/// See [`Workspace::attachment_press`].
pub(crate) struct AttachmentPress {
    pub message: MessageId,
    pub index: usize,
    pub name: String,
    pub at: gpui::Point<gpui::Pixels>,
}

/// How far, in pixels, a press on an attachment has to travel to be a drag
/// rather than a click that wobbled.
const ATTACHMENT_DRAG_THRESHOLD: f32 = 4.;

pub(crate) fn sanitize_filename(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .trim_start_matches('.');

    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect();

    if cleaned.is_empty() { "attachment".into() } else { cleaned }
}

/// One step of a sync, on its way from the worker to the UI.
/// An account that failed to sync, as the worker that ran it saw it.
#[derive(Debug, Clone)]
struct SyncFailure {
    message: String,
    /// The server refused the login, as opposed to not being reachable.
    refused: bool,
}

/// What the sidebar says is wrong with an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccountProblem {
    pub reason: String,
    /// When the server refused the login, if that is the problem.
    pub refused_at: Option<Instant>,
}

impl AccountProblem {
    /// How long a refused login keeps an account out of automatic syncs.
    /// The watcher waits the same, for the same reason; see
    /// `spool_net::idle::REFUSED_PAUSE`.
    const REFUSED_FOR: Duration = spool_net::idle::REFUSED_PAUSE;

    fn refused(reason: String) -> Self {
        Self { reason, refused_at: Some(Instant::now()) }
    }

    fn unreachable(reason: String) -> Self {
        Self { reason, refused_at: None }
    }

    fn refused_recently(&self, now: Instant) -> bool {
        self.refused_at.is_some_and(|at| now.duration_since(at) < Self::REFUSED_FOR)
    }

    /// The sidebar's sentence: what is wrong, then what to do.
    pub fn summary(&self) -> String {
        match self.refused_at {
            Some(_) => format!(
                "The server refused the login. Check the password in Settings \u{2014} Spool will not retry \
                 on its own for an hour, so a wrong one cannot get this address banned.\n\n{}",
                self.reason
            ),
            None => format!("Not syncing: {}", self.reason),
        }
    }
}

/// Which accounts a sync visits.
///
/// All of them when somebody asked (`only` is `None`). When a watcher asked,
/// just the ones it named — less any whose login was refused within the hour:
/// that login would be refused again, and every refusal counts towards a ban.
/// A sync somebody asks for still tries; they may just have fixed the password.
fn accounts_to_sync(
    accounts: Vec<spool_net::AccountConfig>,
    only: Option<&std::collections::BTreeSet<String>>,
    problems: &HashMap<String, AccountProblem>,
    now: Instant,
) -> Vec<spool_net::AccountConfig> {
    let Some(only) = only else { return accounts };
    accounts
        .into_iter()
        .filter(|account| {
            let key = account_key(&account.email);
            only.contains(&key) && !problems.get(&key).is_some_and(|p| p.refused_recently(now))
        })
        .collect()
}

/// How an account is named in the sync bookkeeping: its address, compared as
/// addresses are.
pub(crate) fn account_key(email: &str) -> String {
    email.trim().to_lowercase()
}

struct SyncUpdate {
    /// Which account it came from, as an index into the configured list.
    ///
    /// Carried because the accounts sync at the same time: the bar has to
    /// keep each one's progress apart to add them up.
    account: usize,
    label: String,
    /// How far through its own account this step is, on a scale of 0 to 1.
    fraction: Option<f32>,
    /// How many folders this account has to walk, when the step says.
    ///
    /// Per account rather than summed, for the same reason `fraction` is: the
    /// accounts interleave, and adding these up as they arrive would count one
    /// account's folders once per step it sends.
    folders: Option<usize>,
    /// Whether mail actually landed, as opposed to a step being announced.
    ///
    /// Only these reload the list. A sync that downloads a hundred thousand
    /// messages sends a great many updates, and re-querying on the ones that
    /// merely say "connecting" would be a query per round trip.
    arrived: bool,
    /// A folder starting or finishing, so the sidebar can mark the rows that
    /// are being worked on. `None` for every step that is not about one.
    folder: Option<FolderStep>,
}

/// One folder entering or leaving the set being synced.
#[derive(Debug, Clone)]
struct FolderStep {
    /// The account's address, which is what the sidebar's rows can be matched
    /// against — the progress callback has no row ids to give.
    email: String,
    path: String,
    started: bool,
}

/// Whether the set names this folder, or any folder of this account.
///
/// The address is compared without regard to case, because it comes from a
/// config file somebody typed; the path is compared exactly, because it is the
/// server's own name for the folder and `INBOX` and `Inbox` can be two
/// different folders on the same server.
fn folder_is_syncing(
    syncing: &std::collections::HashSet<(String, String)>,
    email: &str,
    path: Option<&str>,
) -> bool {
    syncing.iter().any(|(account, folder)| {
        account.eq_ignore_ascii_case(email) && path.is_none_or(|path| folder == path)
    })
}

/// The folder a step is about, if it is about one.
///
/// Only the two that bracket the work: a folder being opened and a folder
/// being finished with. What happens in between says how much arrived, not
/// whether anything is still going on.
fn folder_step(progress: &Progress, email: &str) -> Option<FolderStep> {
    match progress {
        Progress::Folder { name, .. } => Some(FolderStep {
            email: email.to_string(),
            path: name.clone(),
            started: true,
        }),
        Progress::FolderDone { name } => Some(FolderStep {
            email: email.to_string(),
            path: name.clone(),
            started: false,
        }),
        _ => None,
    }
}

/// How many folders the account has, when the step happens to say.
///
/// Two steps do. The listing announces the count up front, which is the number
/// the queue is drawn against for most of a first sync; every folder opening
/// repeats it, which is the only source on a run that skipped the listing
/// because it already knew the account's inboxes.
fn folder_total(progress: &Progress) -> Option<usize> {
    match progress {
        Progress::FoldersListed { count } => Some(*count),
        Progress::Folder { total, .. } => Some(*total),
        _ => None,
    }
}

/// How far through one account a step is, on a scale of 0 to 1.
///
/// `None` where the honest answer is unknown, which is most of the early
/// steps: waiting on a connection and waiting on a folder listing take as long
/// as the server takes. A bar that guessed would stall at the same wrong
/// number every sync, and a bar people learn to distrust is worse than none.
fn fraction_within_account(progress: &Progress) -> Option<f32> {
    match progress {
        Progress::Connecting { .. } | Progress::ListingFolders | Progress::Pushing => None,
        Progress::FoldersListed { .. } => Some(0.),
        Progress::Folder { done, total, .. } => {
            Some(*done as f32 / (*total).max(1) as f32)
        }
        // A folder's count arrives without saying which folder number it was,
        // so it moves the label and leaves the bar alone. The same for a
        // folder finishing: the count of finished folders comes with the next
        // one to start.
        Progress::Fetched { .. } | Progress::FolderDone { .. } => None,
        Progress::Done(_) => Some(1.),
    }
}

/// Renders a sync step as a status line.
///
/// `account` names the account when there is more than one syncing, since
/// with all of them running at once "Syncing INBOX" does not say whose. The
/// closing report is left alone: it already names its account.
fn describe_progress(progress: &Progress, account: Option<&str>) -> String {
    let line = match progress {
        Progress::Connecting { host } => format!("Connecting to {host}\u{2026}"),
        Progress::ListingFolders => "Listing folders\u{2026}".into(),
        Progress::Pushing => "Sending your changes\u{2026}".into(),
        Progress::FoldersListed { count } => format!("Found {count} folders"),
        Progress::Folder { name, done, total } => {
            format!("Syncing {name} ({}/{total})", (done + 1).min(*total))
        }
        Progress::Fetched { folder, count } => format!("{folder}: {count} new"),
        // Nothing to say: a folder finishing is only interesting to the
        // sidebar, which is watching the set rather than reading the line.
        Progress::FolderDone { .. } => return String::new(),
        Progress::Done(report) => report.summary(),
    };

    match account {
        Some(name) if !matches!(progress, Progress::Done(_)) => format!("{name}: {line}"),
        _ => line,
    }
}

/// Shortens a string for a status message, on a character boundary.
/// Whether a message living in `mailbox` belongs in the view `scope` is
/// showing.
///
/// The unified and per-account views hold every folder, so filing a message
/// from one of them moves it between folders without taking it out of the
/// list. Dropping the row anyway would be a lie the next reload corrects.
/// Where a set of messages could legally be filed.
///
/// Two rules, both of them about not offering something that cannot happen:
///
/// * **Mail cannot cross accounts.** IMAP has no move between servers, and
///   pretending otherwise would mean uploading a copy under a different
///   identity. A selection spanning two accounts therefore has no destinations
///   at all, rather than a list that would fail on use.
/// * **A folder the messages are already in is not a destination.** Offering
///   it costs the row its place in the list for a move that does nothing.
fn move_destinations(
    mailboxes: &[Mailbox],
    envelopes: &[Envelope],
    selection: &[MessageId],
    accounts: &[Account],
) -> Vec<Destination> {
    let mut account = None;
    let mut origins: Vec<MailboxId> = Vec::new();

    for id in selection {
        let Some(envelope) = envelopes.iter().find(|e| e.id == *id) else { continue };
        match account {
            None => account = Some(envelope.account_id),
            Some(existing) if existing != envelope.account_id => return Vec::new(),
            Some(_) => {}
        }
        if !origins.contains(&envelope.mailbox_id) {
            origins.push(envelope.mailbox_id);
        }
    }

    let Some(account) = account else { return Vec::new() };
    // Only worth naming when there is more than one, which is exactly the
    // unified view where a folder called "Archive" is ambiguous.
    let show_account = accounts.len() > 1;
    let account_name = accounts
        .iter()
        .find(|a| a.id == account)
        .map(|a| a.name.clone())
        .unwrap_or_default();

    mailboxes
        .iter()
        .filter(|m| m.account_id == account)
        // A single origin is the folder being moved out of. With several,
        // every one of them is still a destination for the messages that came
        // from the others, so nothing is excluded.
        .filter(|m| origins.len() > 1 || origins.first() != Some(&m.id))
        .map(|m| Destination {
            id: m.id,
            path: m.path.clone().into(),
            account: match show_account {
                true => account_name.clone().into(),
                false => SharedString::default(),
            },
        })
        .collect()
}

/// How many changes every account together owes the server.
///
/// Asked before a push rather than after, because the answer is what the
/// readout says and the push is about to make it zero.
fn pending_everywhere(store: &Store) -> usize {
    store
        .accounts()
        .map(|accounts| {
            accounts
                .iter()
                .filter_map(|account| store.pending_count(account.id).ok())
                .sum()
        })
        .unwrap_or(0)
}

/// Where the cursor goes when rows leave the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Landing {
    /// Onto whatever slid up into the gap — the next message down.
    ///
    /// Right for filing: archiving what you were reading moves you on through
    /// the list, which is the direction you were already going.
    Replacement,
    /// Onto the row above the one that went.
    ///
    /// Right for deleting. The replacement row is mail you have not looked at
    /// yet, and landing on it means the message immediately after the one you
    /// threw away is opened — and marked read — by the act of throwing the
    /// first one away.
    Previous,
}

/// Where the cursor lands after rows are taken out of the list.
///
/// `first_removed` is the index the earliest removed row had; `remaining` is
/// how many rows are left. Pure arithmetic, kept out of the method because it
/// is the whole of the behaviour and none of the bookkeeping.
fn cursor_after_removal(first_removed: usize, remaining: usize, land: Landing) -> usize {
    if remaining == 0 {
        return 0;
    }

    let index = match land {
        Landing::Replacement => first_removed,
        // Off the top there is no previous row, so the replacement is the only
        // answer — moving "up" from the first message is not a thing the list
        // can do.
        Landing::Previous => first_removed.saturating_sub(1),
    };

    index.min(remaining - 1)
}

/// What pressing enter in the palette settled on.
enum Chosen {
    Command(Box<dyn gpui::Action>),
    Folder(MailboxId),
}

/// Why the reader is pointed at the message it is pointed at.
///
/// The reader follows the cursor, and the cursor moves for two quite different
/// reasons: because somebody moved it, or because the list moved underneath it.
/// Telling them apart is what stops mail marking itself read while nobody is
/// looking — a sync that lands new mail reloads the list, and the newest
/// message arrives under a stationary cursor having never been seen by anyone.
///
/// A required argument rather than a flag with a default, so that adding a new
/// way to point the reader at something forces the question to be answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Showing {
    /// The user moved here: an arrow key, a click, enter, a filter. The clock
    /// on reading it starts.
    Chosen,
    /// The message under the cursor changed without anybody choosing it — new
    /// mail landed and the list reloaded, or filing one moved the cursor on.
    /// Show it, load it, and leave it unread.
    Landed,
}

/// Whether pointing the reader at a message should start the clock on reading
/// it.
///
/// Two conditions, and the first is the whole point: only a message somebody
/// moved to is one they are reading. The second stops the redundant calls
/// `sync_open_message` gets — a reload, a switch into thread view — from
/// pushing the deadline back on a message that has been on screen the whole
/// time.
fn starts_reading(why: Showing, already_open: bool) -> bool {
    matches!(why, Showing::Chosen) && !already_open
}

/// The row a message is on, which is not always a row of its own.
///
/// With conversations on, a row stands for a thread and carries the id of its
/// newest message; a message reached from somewhere else — a search result,
/// a jump from elsewhere — is very often one of the older ones. The
/// exact row first, then the row that contains it, so the cursor lands on the
/// conversation rather than falling back to the top of the list.
fn row_for(envelopes: &[Envelope], message: MessageId) -> Option<usize> {
    envelopes
        .iter()
        .position(|envelope| envelope.id == message)
        .or_else(|| envelopes.iter().position(|e| e.message_ids().contains(&message)))
}

/// The arrangement first, then any account it does not name, in the order the
/// store gave them.
///
/// An account added after the last drag therefore appears at the bottom,
/// which is where a new thing can be seen — rather than in the middle of an
/// order somebody set.
fn arrange_accounts<'a>(accounts: &'a [Account], order: &[String]) -> Vec<&'a Account> {
    let mut arranged: Vec<&Account> = accounts.iter().collect();
    // Stable, so accounts nobody has dragged keep the store's order among
    // themselves rather than being shuffled by an unstable tie-break.
    arranged.sort_by_key(|account| {
        order
            .iter()
            .position(|email| email.eq_ignore_ascii_case(&account.email))
            .unwrap_or(usize::MAX)
    });
    arranged
}

fn scope_holds(scope: Scope, mailboxes: &[Mailbox], mailbox: MailboxId) -> bool {
    match scope {
        Scope::Everything => true,
        Scope::Mailbox(id) => id == mailbox,
        // The account row lists that account's inbox, so filing a message
        // anywhere else in the same account takes it out of the list — the
        // same way filing out of a folder view does.
        Scope::Account(account) => mailboxes.iter().any(|m| {
            m.id == mailbox && m.account_id == account && m.role == MailboxRole::Inbox
        }),
    }
}

/// How an operation names what it acted on, for the status bar.
///
/// One message is worth naming; a dozen are worth counting.
fn summarize(subject: &str, count: usize) -> String {
    match count {
        1 => format!("\u{201c}{}\u{201d}", truncate(subject, 40)),
        n => format!("{n} messages"),
    }
}

/// Drops undo entries that mention a permanently deleted message.
///
/// The whole entry goes, not just the message: a partial undo would put some
/// of a batch back and claim to have restored all of it.
fn prune_undo(stack: &mut Vec<Undoable>, gone: &[MessageId]) {
    stack.retain(|entry| !entry.moves.iter().any(|(id, _)| gone.contains(id)));
}

pub fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((ix, _)) => format!("{}\u{2026}", &text[..ix]),
        None => text.to_string(),
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for Workspace {}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(target_os = "macos")]
        self.ensure_web_view(window, cx);
        // The timing readout is a development tool; a release build does not
        // time its own frames.
        let frame_started = cfg!(debug_assertions).then(Instant::now);

        // The global is the only source of truth for the palette. Cloned once
        // per frame and passed down, so no render helper has to reach back
        // into `cx` while it is already borrowed for event listeners.
        let theme = cx.theme().clone();

        // How many rows fit, so PageDown moves by a screen rather than a guess.
        self.perf.visible_rows =
            (window.viewport_size().height / self.settings.messages.row_height())
                .floor()
                .max(1.) as usize;

        // The sidebar runs the full height of the window and the header bar
        // sits only over the content beside it, the way a native app is built:
        // one horizontal rule across the whole window cuts the sidebar in two
        // and is the thing that reads as "not a Mac app".
        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(self.render_title_bar(&theme, window, cx))
            .child(
                // The content is a card lifted off the chrome: the header bar
                // above it and the sidebar beside it are the same panel, and
                // the corner where they meet is where the message surface
                // begins. One rounded corner does most of the work of saying
                // which of the two you are looking at.
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .rounded_tl(px(10.))
                    .border_t_1()
                    .border_l_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(self.render_message_list(&theme, cx))
                    .child(self.render_divider(Divider::List, &theme, cx))
                    .child(self.render_reader(&theme, window, cx)),
            );

        let body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .when(self.sidebar_visible, |this| {
                this.child(self.render_sidebar(&theme, window, cx))
                    .child(self.render_divider(Divider::Sidebar, &theme, cx))
            })
            .child(content);

        let root = div()
            .key_context("Spool")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::close_action))
            .on_action(cx.listener(Self::check_for_updates_action))
            .on_action(cx.listener(Self::save_attachments_action))
            .on_action(cx.listener(Self::preview_attachment_action))
            .on_action(cx.listener(Self::toggle_palette))
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::unsubscribe_action))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::sync))
            .on_action(cx.listener(Self::get_new_mail))
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::select_first))
            .on_action(cx.listener(Self::select_last))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::toggle_density))
            .on_action(cx.listener(Self::toggle_conversations))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_status_bar))
            .on_action(cx.listener(Self::toggle_unified_inbox))
            .on_action(cx.listener(Self::compose_action))
            .on_action(cx.listener(Self::resume_draft_action))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::open_message))
            .on_action(cx.listener(Self::toggle_read))
            .on_action(cx.listener(Self::mark_read))
            .on_action(cx.listener(Self::mark_unread))
            .on_action(cx.listener(Self::toggle_flag))
            .on_action(cx.listener(Self::archive))
            .on_action(cx.listener(Self::move_to_folder))
            .on_action(cx.listener(Self::mark_as_spam))
            .on_action(cx.listener(Self::not_spam))
            .on_action(cx.listener(Self::delete_message))
            .on_action(cx.listener(Self::mark_all_read))
            .on_action(cx.listener(Self::cycle_filter))
            .on_action(cx.listener(Self::toggle_thread_view))
            .on_action(cx.listener(Self::toggle_body_source))
            .on_action(cx.listener(Self::load_older_action))
            .on_action(cx.listener(Self::next_mailbox))
            .on_action(cx.listener(Self::focus_sidebar))
            .on_action(cx.listener(Self::focus_message_list))
            .on_action(cx.listener(Self::sidebar_next))
            .on_action(cx.listener(Self::sidebar_prev))
            .on_action(cx.listener(Self::sidebar_expand))
            .on_action(cx.listener(Self::sidebar_collapse))
            .on_action(cx.listener(Self::prev_mailbox))
            .on_action(cx.listener(Self::copy_sender))
            .on_action(cx.listener(Self::reply_action))
            .on_action(cx.listener(Self::reply_all_action))
            .on_action(cx.listener(Self::forward_action))
            .on_action(cx.listener(Self::select_all_messages_action))
            .on_action(cx.listener(Self::extend_next))
            .on_action(cx.listener(Self::extend_prev))
            .on_action(cx.listener(Self::new_mailbox))
            .on_action(cx.listener(Self::rename_mailbox))
            .on_action(cx.listener(Self::delete_mailbox))
            .on_action(cx.listener(Self::run_extension_command_action))
            .on_action(cx.listener(Self::next_field))
            .on_action(cx.listener(Self::prev_field))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .when(self.dragging.is_some(), |this| {
                this.on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                    this.drag_to(event.position.x, cx);
                }))
            })
            .when(self.scroll_drag.is_some(), |this| {
                this.on_mouse_move(cx.listener(|this, event: &gpui::MouseMoveEvent, _, cx| {
                    this.drag_scrollbar_to(event.position.y, cx);
                }))
            })
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    this.end_drag(cx);
                    this.end_scrollbar_drag(cx);
                }),
            )
            .on_mouse_up_out(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _, cx| {
                    this.end_drag(cx);
                    this.end_scrollbar_drag(cx);
                }),
            )
            // The window's own surface is the chrome; the content sits on top
            // of it as a card, so what shows through at the edges is panel.
            .bg(theme.panel)
            .font_family(theme.font)
            .text_size(theme.text_size)
            .text_color(theme.text)
            .line_height(px(18.))
            .relative()
            .child(body)
            .children(self.render_toast(&theme))
            .children(
                self.settings.status_bar.then(|| self.render_status_bar(&theme, cx)),
            )
            .children(self.render_palette(&theme, cx))
            .children(self.render_folder_menu(&theme, cx))
            .children(self.render_folder_prompt(&theme, cx))
;

        if let Some(started) = frame_started {
            self.perf.record_frame(started.elapsed());
        }
        root
    }
}

/// The height of a frame budget at 120 Hz, used to color the perf readout.
pub const FRAME_BUDGET: Duration = Duration::from_micros(8_333);

#[cfg(test)]
mod tests {
    use super::{Showing, starts_reading};

    #[test]
    fn mail_that_arrives_under_the_cursor_is_not_read() {
        // The bug this exists for: Spool sits in the background, a sync
        // fetches mail, the list reloads, and the newest message lands under a
        // stationary cursor. Nobody has seen it. Nothing may mark it read.
        assert!(!starts_reading(Showing::Landed, false));
        assert!(!starts_reading(Showing::Landed, true));
    }

    #[test]
    fn a_message_somebody_moved_to_starts_the_clock() {
        assert!(starts_reading(Showing::Chosen, false));
    }

    #[test]
    fn a_message_already_on_screen_does_not_have_its_deadline_pushed_back() {
        // `sync_open_message` is called again for things that are not a change
        // of message — a reload, a switch into thread view. Restarting the
        // clock each time would mean a message being looked at the whole time
        // is never quite finished being read.
        assert!(!starts_reading(Showing::Chosen, true));
    }


    #[test]
    fn a_travelling_cursor_settles_before_it_would_move_again() {
        // The settle has to be shorter than the gap that counts as
        // travelling. Longer, and a cursor moving at exactly the threshold
        // would start a load, be judged still travelling, and wait again —
        // never reading anything until the movement stopped completely.
        assert!(SETTLE < TRAVELLING, "{SETTLE:?} >= {TRAVELLING:?}");
        // And short enough that arriving somewhere deliberately and looking
        // at it does not feel like waiting.
        assert!(SETTLE <= std::time::Duration::from_millis(150), "{SETTLE:?}");
    }

    #[test]
    fn a_previewed_file_is_readable_by_nobody_else() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = write_to_preview_dir("statement.pdf", b"%PDF-1.4").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.4");

        // An attachment can be a bank statement and the temp directory is
        // shared with every other account on the machine.
        let file = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file, 0o600, "file mode {file:o}");
        let dir = std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir, 0o700, "directory mode {dir:o}");

        clear_preview_dir(&path);
        assert!(!path.exists(), "and it does not outlive the preview");
    }

    #[test]
    fn a_previewed_file_keeps_its_extension_but_not_its_path() {
        // Quick Look picks its previewer from the extension, so the name has
        // to survive. What must not survive is the rest of it: this name is a
        // stranger's, and joining it whole would write outside the directory.
        let path = write_to_preview_dir("../../../../etc/passwd", b"x").unwrap();
        assert_eq!(path.file_name().unwrap(), "passwd");
        assert_eq!(path.parent(), path.parent());
        assert!(
            path.starts_with(std::env::temp_dir()),
            "escaped to {}",
            path.display(),
        );
        clear_preview_dir(&path);
    }

    #[test]
    fn two_previews_of_the_same_name_do_not_collide() {
        // Two messages both carrying `scan.pdf` is ordinary, and the second
        // must not overwrite the first while it is still on screen.
        let one = write_to_preview_dir("scan.pdf", b"first").unwrap();
        let two = write_to_preview_dir("scan.pdf", b"second").unwrap();

        assert_ne!(one, two);
        assert_eq!(std::fs::read(&one).unwrap(), b"first");
        assert_eq!(std::fs::read(&two).unwrap(), b"second");

        clear_preview_dir(&one);
        clear_preview_dir(&two);
    }
    use spool_core::AccountId;

    use super::{
        Account, Envelope, Landing, Mailbox, MailboxId, MailboxRole, MessageId, Progress, Scope,
        Undoable, arrange_accounts, clear_preview_dir, cursor_after_removal, describe_progress,
        folder_is_syncing, fraction_within_account, move_destinations, prune_undo, row_for,
        SETTLE, TRAVELLING, sanitize_filename, scope_holds, summarize, write_to_preview_dir,
    };

    fn mailbox(id: i64, account: i64) -> Mailbox {
        Mailbox {
            id: MailboxId(id),
            account_id: AccountId(account),
            path: "INBOX".into(),
            name: "Inbox".into(),
            role: MailboxRole::Inbox,
            total: 0,
            unread: 0,
            server_total: 0,
        }
    }

    fn folder(id: i64, account: i64, path: &str) -> Mailbox {
        // The role comes from the path, the way the store's does: a helper
        // that made everything an inbox would let a test about special
        // folders pass without one.
        Mailbox {
            path: path.into(),
            name: path.into(),
            role: MailboxRole::guess(path),
            ..mailbox(id, account)
        }
    }

    fn account(id: i64, name: &str) -> Account {
        Account {
            id: AccountId(id),
            name: name.into(),
            email: format!("{name}@example.com"),
            color: 0,
        }
    }

    fn envelope(id: i64, account: i64, mailbox: i64) -> Envelope {
        Envelope {
            id: MessageId(id),
            account_id: AccountId(account),
            mailbox_id: MailboxId(mailbox),
            thread_id: spool_core::ThreadId(id),
            from: spool_core::Address::new("Ana", "ana@example.com"),
            subject: "Subject".into(),
            date: 0,
            snippet: String::new(),
            flags: spool_core::Flags::default(),
            has_attachments: false,
            size: 0,
            group: None,
        }
    }

    #[test]
    fn deleting_moves_the_cursor_up_rather_than_onto_the_next_message() {
        // The replacement row is mail that has not been looked at yet.
        // Landing on it means the message after the one you threw away is
        // opened — and marked read — by the act of throwing the first away.
        assert_eq!(cursor_after_removal(4, 9, Landing::Previous), 3);
    }

    #[test]
    fn filing_moves_the_cursor_on_through_the_list() {
        // Archiving what you were reading carries you forward, which is the
        // direction you were already going.
        assert_eq!(cursor_after_removal(4, 9, Landing::Replacement), 4);
    }

    #[test]
    fn deleting_the_top_message_stays_at_the_top() {
        // There is no row above the first one, and moving "up" off the top is
        // not something the list can do.
        assert_eq!(cursor_after_removal(0, 5, Landing::Previous), 0);
    }

    #[test]
    fn deleting_the_last_message_lands_on_what_is_left() {
        // Removing the final row leaves the cursor past the end unless it is
        // pulled back, which is a selection nothing can draw.
        assert_eq!(cursor_after_removal(5, 5, Landing::Replacement), 4);
        assert_eq!(cursor_after_removal(5, 5, Landing::Previous), 4);
    }

    #[test]
    fn emptying_the_list_leaves_the_cursor_somewhere_representable() {
        assert_eq!(cursor_after_removal(3, 0, Landing::Previous), 0);
        assert_eq!(cursor_after_removal(3, 0, Landing::Replacement), 0);
    }

    #[test]
    fn deleting_a_run_goes_above_the_whole_run() {
        // A multi-selection removes several rows; "up one" means up from the
        // earliest of them, not up from wherever the cursor happened to sit.
        assert_eq!(cursor_after_removal(6, 20, Landing::Previous), 5);
    }

    #[test]
    fn a_move_is_offered_every_folder_but_the_one_it_is_in() {
        let boxes = [folder(1, 1, "INBOX"), folder(2, 1, "Archive"), folder(3, 1, "Spam")];
        let mail = [envelope(10, 1, 1)];
        let accounts = [account(1, "Work")];

        let destinations = move_destinations(&boxes, &mail, &[MessageId(10)], &accounts);
        let paths: Vec<_> = destinations.iter().map(|d| d.path.to_string()).collect();

        assert_eq!(paths, ["Archive", "Spam"], "moving into the current folder does nothing");
    }

    #[test]
    fn a_selection_spanning_two_accounts_has_nowhere_to_go() {
        // IMAP has no move between servers, and pretending otherwise would
        // mean uploading a copy under a different identity. An empty list is
        // the honest answer; a list that fails on use is not.
        let boxes = [folder(1, 1, "INBOX"), folder(2, 2, "INBOX")];
        let mail = [envelope(10, 1, 1), envelope(11, 2, 2)];
        let accounts = [account(1, "Work"), account(2, "Home")];

        assert!(
            move_destinations(&boxes, &mail, &[MessageId(10), MessageId(11)], &accounts).is_empty()
        );
    }

    #[test]
    fn only_the_selections_own_account_is_offered() {
        let boxes = [folder(1, 1, "INBOX"), folder(2, 1, "Archive"), folder(3, 2, "Archive")];
        let mail = [envelope(10, 1, 1)];
        let accounts = [account(1, "Work"), account(2, "Home")];

        let destinations = move_destinations(&boxes, &mail, &[MessageId(10)], &accounts);
        assert_eq!(destinations.len(), 1);
        assert_eq!(destinations[0].id, MailboxId(2));
        assert_eq!(
            destinations[0].account.to_string(),
            "Work",
            "with two accounts configured, a folder called Archive needs saying whose"
        );
    }

    #[test]
    fn one_account_needs_no_account_label() {
        let boxes = [folder(1, 1, "INBOX"), folder(2, 1, "Archive")];
        let mail = [envelope(10, 1, 1)];
        let accounts = [account(1, "Work")];

        let destinations = move_destinations(&boxes, &mail, &[MessageId(10)], &accounts);
        assert_eq!(destinations[0].account.to_string(), "", "there is nothing to disambiguate");
    }

    #[test]
    fn a_selection_from_several_folders_keeps_all_of_them_as_destinations() {
        // Two messages from two folders: each is still somewhere the other one
        // could go, so excluding either would be wrong.
        let boxes = [folder(1, 1, "INBOX"), folder(2, 1, "Archive"), folder(3, 1, "Spam")];
        let mail = [envelope(10, 1, 1), envelope(11, 1, 2)];
        let accounts = [account(1, "Work")];

        let destinations = move_destinations(&boxes, &mail, &[MessageId(10), MessageId(11)], &accounts);
        assert_eq!(destinations.len(), 3);
    }

    #[test]
    fn nothing_selected_offers_nothing() {
        let boxes = [folder(1, 1, "INBOX"), folder(2, 1, "Archive")];
        assert!(move_destinations(&boxes, &[], &[], &[account(1, "Work")]).is_empty());
    }

    #[test]
    fn filing_inside_a_folder_view_takes_the_message_out_of_it() {
        let boxes = [mailbox(1, 1), mailbox(2, 1)];
        assert!(!scope_holds(Scope::Mailbox(MailboxId(1)), &boxes, MailboxId(2)));
        assert!(scope_holds(Scope::Mailbox(MailboxId(1)), &boxes, MailboxId(1)));
    }

    #[test]
    fn filing_inside_the_unified_view_keeps_the_row() {
        // Archiving from All Mail moves a message between folders; it is still
        // one of "everything", so the row stays and only its folder changes.
        let boxes = [mailbox(1, 1)];
        assert!(scope_holds(Scope::Everything, &boxes, MailboxId(9)));
    }

    fn syncing(pairs: &[(&str, &str)]) -> std::collections::HashSet<(String, String)> {
        pairs.iter().map(|(a, f)| ((*a).to_string(), (*f).to_string())).collect()
    }

    #[test]
    fn only_the_folder_being_synced_wears_the_spinner() {
        let set = syncing(&[("me@example.com", "INBOX")]);

        assert!(folder_is_syncing(&set, "me@example.com", Some("INBOX")));
        assert!(!folder_is_syncing(&set, "me@example.com", Some("Archive")));
        // The same folder name in another account is another folder.
        assert!(!folder_is_syncing(&set, "other@example.com", Some("INBOX")));
    }

    #[test]
    fn an_account_is_syncing_while_any_of_its_folders_is() {
        let set = syncing(&[("me@example.com", "[Gmail]/All Mail")]);

        assert!(folder_is_syncing(&set, "me@example.com", None));
        assert!(!folder_is_syncing(&set, "other@example.com", None));
        assert!(!folder_is_syncing(&syncing(&[]), "me@example.com", None));
    }

    #[test]
    fn an_address_is_matched_however_the_config_capitalised_it() {
        let set = syncing(&[("Me@Example.com", "INBOX")]);
        assert!(folder_is_syncing(&set, "me@example.com", Some("INBOX")));

        // The path is not: a server is free to have both.
        let set = syncing(&[("me@example.com", "inbox")]);
        assert!(!folder_is_syncing(&set, "me@example.com", Some("INBOX")));
    }

    #[test]
    fn a_message_reached_from_elsewhere_is_found_on_its_own_row() {
        let rows = [envelope(1, 1, 1), envelope(2, 1, 1)];
        assert_eq!(row_for(&rows, MessageId(2)), Some(1));
        assert_eq!(row_for(&rows, MessageId(9)), None);
    }

    #[test]
    fn a_message_inside_a_conversation_is_found_on_the_conversations_row() {
        // What happens when the list is grouped: the row carries the newest
        // message's id, and the message being looked for is an older one in
        // the same thread. Without this the cursor falls back to the top of
        // the list, which is a different message entirely.
        let mut newest = envelope(10, 1, 1);
        newest.group = Some(spool_core::Group {
            ids: vec![MessageId(10), MessageId(4)],
            unread: 0,
            participants: Vec::new(),
        });
        let rows = [envelope(1, 1, 1), newest];

        assert_eq!(row_for(&rows, MessageId(4)), Some(1));
    }

    #[test]
    fn the_sidebar_draws_the_accounts_in_the_order_they_were_dragged_into() {
        let accounts = [account(1, "work"), account(2, "home"), account(3, "list")];
        let order = vec!["home@example.com".to_string(), "work@example.com".to_string()];

        let arranged = arrange_accounts(&accounts, &order);
        assert_eq!(
            arranged.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
            ["home", "work", "list"],
            "the two that were arranged, then the one that was not",
        );
    }

    #[test]
    fn an_arrangement_matches_an_address_however_it_is_capitalised() {
        let accounts = [account(1, "work"), account(2, "home")];
        let order = vec!["HOME@EXAMPLE.COM".to_string()];

        assert_eq!(arrange_accounts(&accounts, &order)[0].name, "home");
    }

    #[test]
    fn with_nothing_arranged_the_store_order_stands() {
        let accounts = [account(1, "work"), account(2, "home")];
        assert_eq!(
            arrange_accounts(&accounts, &[]).iter().map(|a| a.id).collect::<Vec<_>>(),
            [AccountId(1), AccountId(2)],
        );
    }

    #[test]
    fn an_account_view_holds_its_own_inbox_and_nothing_else() {
        let boxes = [mailbox(1, 1), mailbox(2, 2), folder(3, 1, "Archive")];
        assert!(scope_holds(Scope::Account(AccountId(1)), &boxes, MailboxId(1)));
        // Another account's inbox.
        assert!(!scope_holds(Scope::Account(AccountId(1)), &boxes, MailboxId(2)));
        // This account, but archiving from the account row takes the row out
        // of the list rather than leaving it there pointing at a folder the
        // list is not showing.
        assert!(!scope_holds(Scope::Account(AccountId(1)), &boxes, MailboxId(3)));
        // A folder the sidebar has not heard of is not in any account's view.
        assert!(!scope_holds(Scope::Account(AccountId(1)), &boxes, MailboxId(99)));
    }


    fn entry(summary: &str, ids: &[u64]) -> Undoable {
        Undoable {
            summary: summary.into(),
            moves: ids.iter().map(|id| (MessageId(*id as i64), MailboxId(1))).collect(),
        }
    }

    #[test]
    fn one_message_is_named_and_several_are_counted() {
        assert_eq!(summarize("Invoice", 1), "\u{201c}Invoice\u{201d}");
        assert_eq!(summarize("Invoice", 4), "4 messages");
        // A subject long enough to push the metrics off the status bar is cut.
        assert!(summarize(&"x".repeat(200), 1).ends_with("\u{2026}\u{201d}"));
    }

    #[test]
    fn a_permanent_delete_drops_the_undo_entries_that_mention_it() {
        let mut stack = vec![entry("a", &[1, 2]), entry("b", &[3]), entry("c", &[4, 5])];
        prune_undo(&mut stack, &[MessageId(3)]);
        assert_eq!(stack, vec![entry("a", &[1, 2]), entry("c", &[4, 5])]);
    }

    #[test]
    fn a_batch_with_one_deleted_message_is_dropped_whole() {
        // Restoring two of three and reporting "3 messages" would be a lie.
        let mut stack = vec![entry("3 messages", &[1, 2, 3])];
        prune_undo(&mut stack, &[MessageId(2)]);
        assert!(stack.is_empty());
    }

    #[test]
    fn deleting_something_never_moved_leaves_the_stack_alone() {
        let mut stack = vec![entry("a", &[1, 2])];
        prune_undo(&mut stack, &[MessageId(9)]);
        assert_eq!(stack.len(), 1);
    }

    /// Selection arithmetic, extracted so the range and toggle rules can be
    /// checked without a window.
    ///
    /// Mirrors `Workspace::extend_to` — a shift-range runs between the anchor
    /// and the cursor regardless of which came first.
    fn range(anchor: usize, index: usize, len: usize) -> Vec<usize> {
        if len == 0 {
            return Vec::new();
        }
        let index = index.min(len - 1);
        let (low, high) = if anchor <= index { (anchor, index) } else { (index, anchor) };
        (low..=high).collect()
    }

    #[test]
    fn a_shift_range_runs_both_ways_from_the_anchor() {
        assert_eq!(range(2, 5, 10), vec![2, 3, 4, 5]);
        assert_eq!(range(5, 2, 10), vec![2, 3, 4, 5], "dragging back up selects the same rows");
        assert_eq!(range(3, 3, 10), vec![3]);
    }

    #[test]
    fn a_range_past_the_end_is_clamped_rather_than_panicking() {
        assert_eq!(range(8, 99, 10), vec![8, 9]);
        assert!(range(0, 5, 0).is_empty());
    }

    #[test]
    fn a_traversal_in_a_filename_cannot_escape_the_folder() {
        // The name came from whoever sent the mail.
        assert_eq!(sanitize_filename("../../.ssh/authorized_keys"), "authorized_keys");
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"..\..\windows\system32\evil.dll"), "evil.dll");
    }

    #[test]
    fn a_leading_dot_does_not_produce_a_hidden_file() {
        assert_eq!(sanitize_filename(".bashrc"), "bashrc");
    }

    #[test]
    fn control_characters_and_colons_are_replaced() {
        assert_eq!(sanitize_filename("re\u{7}port:v2.pdf"), "re_port_v2.pdf");
    }

    #[test]
    fn an_unusable_name_still_produces_a_file() {
        assert_eq!(sanitize_filename(""), "attachment");
        assert_eq!(sanitize_filename("   "), "attachment");
        assert_eq!(sanitize_filename("/"), "attachment");
    }

    #[test]
    fn an_ordinary_name_is_left_alone() {
        assert_eq!(sanitize_filename("Q3 report (final).pdf"), "Q3 report (final).pdf");
    }

    #[test]
    fn a_step_says_which_account_it_belongs_to_when_several_are_running() {
        let step = Progress::Folder { name: "INBOX".into(), done: 2, total: 8 };

        assert_eq!(describe_progress(&step, Some("Work")), "Work: Syncing INBOX (3/8)");
        // One account: naming it every line would be noise.
        assert_eq!(describe_progress(&step, None), "Syncing INBOX (3/8)");
    }

    #[test]
    fn the_closing_report_is_not_given_a_second_account_name() {
        // `SyncReport::summary` already opens with the account.
        let report = spool_net::SyncReport { account: "Work".into(), ..Default::default() };
        let line = describe_progress(&Progress::Done(report), Some("Work"));
        assert_eq!(line, "Work: up to date");
    }

    #[test]
    fn a_folder_finishing_moves_the_bar_forward_and_nothing_else_does() {
        let listing = fraction_within_account(&Progress::ListingFolders);
        assert_eq!(listing, None, "waiting on a server is not progress that can be measured");

        let started = Progress::Folder { name: "INBOX".into(), done: 0, total: 4 };
        let half = Progress::Folder { name: "Sent".into(), done: 2, total: 4 };
        assert_eq!(fraction_within_account(&started), Some(0.));
        assert_eq!(fraction_within_account(&half), Some(0.5));
    }


    /// Which accounts a sync visits, and which it leaves alone.
    mod sync_selection {
        use std::{
            collections::{BTreeSet, HashMap},
            time::{Duration, Instant},
        };

        use super::super::{AccountProblem, account_key, accounts_to_sync};

        fn config(email: &str) -> spool_net::AccountConfig {
            spool_net::AccountConfig { name: email.into(), email: email.into(), ..Default::default() }
        }

        fn emails(accounts: &[spool_net::AccountConfig]) -> Vec<&str> {
            accounts.iter().map(|a| a.email.as_str()).collect()
        }

        #[test]
        fn a_watcher_syncs_only_the_account_it_watches() {
            // The bug: one Gmail inbox changing logged in to all ten accounts,
            // four of them on one shared host that counted the logins.
            let accounts = vec![config("wes@studio.example"), config("wes@side.example"), config("wes@other.example")];
            let only: BTreeSet<String> = ["wes@studio.example".to_string()].into_iter().collect();
            let chosen = accounts_to_sync(accounts.clone(), Some(&only), &HashMap::new(), Instant::now());
            assert_eq!(emails(&chosen), ["wes@studio.example"]);

            // Somebody pressing the key still gets everything.
            assert_eq!(accounts_to_sync(accounts, None, &HashMap::new(), Instant::now()).len(), 3);
        }

        #[test]
        fn a_refused_login_is_left_alone_by_watchers_for_an_hour_but_not_by_a_person() {
            let accounts = vec![config("Wes@Side.example")];
            let only: BTreeSet<String> = [account_key("wes@side.example")].into_iter().collect();
            let mut problems = HashMap::new();
            problems.insert(account_key("wes@side.example"), AccountProblem::refused("no".into()));

            let now = Instant::now();
            assert!(accounts_to_sync(accounts.clone(), Some(&only), &problems, now).is_empty());
            assert_eq!(accounts_to_sync(accounts.clone(), None, &problems, now).len(), 1, "pressed, so tried");

            let later = now + AccountProblem::REFUSED_FOR + Duration::from_secs(1);
            assert_eq!(accounts_to_sync(accounts.clone(), Some(&only), &problems, later).len(), 1, "an hour on, tried again");

            // A server that is merely unreachable is not a password problem, and
            // is retried as soon as anybody asks.
            problems.insert(account_key("wes@side.example"), AccountProblem::unreachable("timed out".into()));
            assert_eq!(accounts_to_sync(accounts, Some(&only), &problems, now).len(), 1);
        }

    }
}
