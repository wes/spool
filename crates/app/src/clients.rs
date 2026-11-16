//! The client book, as the app holds it: one copy, shared by every window.
//!
//! A gpui [`Global`] rather than a field on the workspace, for the reason the
//! writing tools are one (see `assist`): the Clients window, the mailbox and
//! the composers all read it while drawing, and a global can be read from any
//! window's render and observed from any entity. The workspace is already the
//! largest thing in the app, and nothing here needs it.
//!
//! What lives here is the part that must outlive a window. The backfill of the
//! people index runs whether or not anybody has opened Clients, so the first
//! time somebody does the book is already full; sources are pulled on their
//! schedule with the window closed, because new signups do not wait for it.
//!
//! The rest of the file is pure: turning the index and the book into the rows
//! the directory shows and the signals the board puts on its cards. That is
//! where the judgement is, so that is what the tests hold still.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use gpui::{App, BorrowAppContext, Global, SharedString, Task};
use spool_core::{AccountId, address};
use spool_store::{
    Store,
    book::{Book, Client, Kind, Snapshot, Source, Stage, StageKind},
    people::{Correspondent, PeopleReader},
};

use crate::clock::now_seconds;

/// How many messages one step of the backfill indexes.
///
/// Small enough that a sync waiting behind a step waits a few tens of
/// milliseconds, large enough that a quarter of a million messages is a
/// hundred-odd steps rather than thousands.
const BACKFILL_BATCH: usize = 2_000;

/// How long after launch the backfill starts.
///
/// Launch is when the list loads, the first sync dials out and the threader
/// seeds — all of it wanting the store. The index is wanted the first time
/// somebody opens Clients, which is not in the first few seconds.
const BACKFILL_DELAY: Duration = Duration::from_secs(4);

/// How often the scheduler looks for a source that is due.
const SOURCE_TICK: Duration = Duration::from_secs(60);

/// A message from them that has gone unanswered this long is no longer
/// "needs a reply" — it is a conversation that ended.
const REPLY_WINDOW: i64 = 30 * 86_400;

/// Nothing either way for this long, on a client still in an open stage, is a
/// sale going cold.
pub(crate) const QUIET_AFTER: i64 = 14 * 86_400;

/// Addresses at these domains are people, never a company: everyone at
/// gmail.com is not one client.
const FREEMAIL: &[&str] = &[
    "gmail.com", "googlemail.com", "yahoo.com", "ymail.com", "outlook.com", "hotmail.com", "live.com",
    "msn.com", "icloud.com", "me.com", "mac.com", "aol.com", "proton.me", "protonmail.com", "pm.me",
    "gmx.com", "gmx.de", "gmx.net", "yandex.ru", "yandex.com", "mail.com", "comcast.net", "fastmail.com",
    "hey.com", "zoho.com", "att.net", "verizon.net", "sbcglobal.net", "qq.com", "163.com", "web.de",
];

pub(crate) fn is_freemail(domain: &str) -> bool {
    FREEMAIL.contains(&domain)
}

/// Whether an address is a machine.
///
/// Mail from one is never a person to sell to, and it is a large share of
/// everybody who has ever written to you. Two-way correspondence already
/// filters most of it; this catches the rest in the "everyone" view.
pub(crate) fn is_automated(addr: &str) -> bool {
    let local = addr.split('@').next().unwrap_or(addr);
    let key: String = local.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    const MACHINES: &[&str] = &[
        "noreply", "donotreply", "notifications", "notification", "mailerdaemon", "postmaster", "bounce",
        "bounces", "nobody", "automated", "alerts", "newsletter", "news", "digest", "updates", "notify",
    ];
    MACHINES.contains(&key.as_str()) || key.starts_with("noreply") || key.starts_with("bounce")
}

/// Who a directory row is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Client(i64),
    /// Somebody in your mail who is not a client, by normalized address.
    Person(SharedString),
}

/// What the index says about somebody, summed over their addresses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub messages: u32,
    pub sent: u32,
    pub received: u32,
    pub first: Option<i64>,
    pub last: Option<i64>,
    pub last_in: Option<i64>,
    pub last_out: Option<i64>,
}

impl Stats {
    fn absorb(&mut self, c: &Correspondent) {
        self.messages += c.messages;
        self.sent += c.sent;
        self.received += c.received;
        self.first = Some(self.first.map_or(c.first, |f| f.min(c.first)));
        self.last = Some(self.last.map_or(c.last, |l| l.max(c.last)));
        self.last_in = self.last_in.max(c.last_inbound);
        self.last_out = self.last_out.max(c.last_outbound);
    }

    pub fn is_two_way(&self) -> bool {
        self.sent > 0
    }
}

/// What a card or a row should say about where things stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// Their message came last, and recently. Since when.
    NeedsReply(i64),
    /// An open sale with nothing either way for a fortnight. Since when.
    GoneQuiet(i64),
    Nothing,
}

/// The signal for somebody, from their mail and where they are on the board.
///
/// `open_stage` is whether they are on the board in a stage that is still
/// going: going quiet is only news about a sale in progress. `stage_since` is
/// when they got there, which stands in for the last contact when there has
/// been none at all — a lead imported a month ago and never written to is a
/// lead going cold.
pub fn signal(stats: &Stats, open_stage: bool, stage_since: Option<i64>, curated: bool, now: i64) -> Signal {
    if let Some(last_in) = stats.last_in
        && stats.last_out.is_none_or(|out| last_in > out)
        && now - last_in <= REPLY_WINDOW
        && (curated || stats.is_two_way())
    {
        return Signal::NeedsReply(last_in);
    }
    if open_stage {
        let last = stats.last.max(stage_since);
        if let Some(last) = last
            && now - last >= QUIET_AFTER
        {
            return Signal::GoneQuiet(last);
        }
    }
    Signal::Nothing
}

/// One row in the directory: a client, or somebody in your mail.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: Key,
    pub client: Option<Client>,
    pub name: SharedString,
    /// The address, or the domain for a company.
    pub detail: SharedString,
    /// Where they work, as the book says or as their domain does.
    pub company: SharedString,
    pub stats: Stats,
    /// What to ask the index for, for their history.
    pub addrs: Vec<String>,
    pub domains: Vec<String>,
    /// The accounts they have mail in.
    pub accounts: Vec<AccountId>,
    pub hidden: bool,
    pub automated: bool,
    /// Lowercased name, addresses, company and tags, for the search box.
    haystack: String,
}

impl Entry {
    pub fn is_client(&self) -> bool {
        self.client.is_some()
    }

    pub fn stage(&self) -> Option<i64> {
        self.client.as_ref().and_then(|c| c.stage)
    }
}

/// Everybody, built once per change to the index or the book.
///
/// Filtering is cheap and happens on every keystroke; this is the part that
/// is not, so it is built off the UI thread and kept.
pub fn everyone(people: &[Correspondent], snapshot: &Snapshot, me: &HashSet<String>, tag_names: &HashMap<i64, String>) -> Vec<Entry> {
    let by_addr: HashMap<&str, &Correspondent> = people.iter().map(|c| (c.addr.as_str(), c)).collect();
    let mut claimed: HashSet<&str> = HashSet::new();
    let mut entries: Vec<Entry> = Vec::with_capacity(snapshot.clients.len() + people.len() / 4);

    // People at a company's domain, for its sums. Built only when there is a
    // company to build it for, since it is a pass over everybody.
    let companies: Vec<&Client> = snapshot.clients.iter().filter(|c| !c.domains.is_empty()).collect();
    let mut at_domain: HashMap<&str, Vec<&Correspondent>> = HashMap::new();
    if !companies.is_empty() {
        let wanted: HashSet<&str> = companies.iter().flat_map(|c| c.domains.iter().map(String::as_str)).collect();
        for person in people {
            let domain = person.domain();
            if wanted.contains(domain) {
                at_domain.entry(domain).or_default().push(person);
            }
        }
    }

    for client in &snapshot.clients {
        let mut stats = Stats::default();
        let mut accounts: Vec<AccountId> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut name_from_mail: Option<&str> = None;

        let members = client
            .addrs
            .iter()
            .filter_map(|addr| by_addr.get(addr.as_str()).copied())
            .chain(client.domains.iter().flat_map(|d| at_domain.get(d.as_str()).into_iter().flatten().copied()));
        for person in members {
            if !seen.insert(person.addr.as_str()) {
                continue;
            }
            stats.absorb(person);
            for account in &person.accounts {
                if !accounts.contains(account) {
                    accounts.push(*account);
                }
            }
            if name_from_mail.is_none() && !person.name.is_empty() {
                name_from_mail = Some(&person.name);
            }
        }
        claimed.extend(client.addrs.iter().map(String::as_str));

        let primary = client.primary_addr().map(str::to_string);
        let domain = client.domains.first().cloned();
        let detail: SharedString = match (client.kind, &primary, &domain) {
            (Kind::Company, _, Some(domain)) => format!("@{domain}").into(),
            (_, Some(addr), _) => addr.clone().into(),
            (_, None, Some(domain)) => format!("@{domain}").into(),
            _ => SharedString::default(),
        };
        let name: SharedString = match client.name.trim() {
            "" => name_from_mail
                .map(str::to_string)
                .or_else(|| domain.clone())
                .or_else(|| primary.clone())
                .unwrap_or_else(|| "Unnamed".into())
                .into(),
            name => name.to_string().into(),
        };
        let company: SharedString = match client.company.trim() {
            "" if client.kind == Kind::Person => {
                primary.as_deref().and_then(company_from_addr).unwrap_or_default().into()
            }
            company => company.to_string().into(),
        };
        let haystack = haystack(&name, client.addrs.iter().chain(&client.domains), &company, &client.tags, tag_names);

        entries.push(Entry {
            key: Key::Client(client.id),
            name,
            detail,
            company,
            stats,
            addrs: client.addrs.clone(),
            domains: client.domains.clone(),
            accounts,
            hidden: client.hidden,
            automated: false,
            haystack,
            client: Some(client.clone()),
        });
    }

    let hidden: HashSet<&str> = snapshot.hidden.iter().map(String::as_str).collect();
    for person in people {
        if claimed.contains(person.addr.as_str()) || me.contains(&person.addr) {
            continue;
        }
        let mut stats = Stats::default();
        stats.absorb(person);
        let name: SharedString = match person.name.trim() {
            "" => person.addr.split('@').next().unwrap_or(&person.addr).to_string().into(),
            name => name.to_string().into(),
        };
        let company: SharedString = company_from_addr(&person.addr).unwrap_or_default().into();
        let haystack = haystack(&name, std::iter::once(&person.addr), &company, &[], tag_names);
        entries.push(Entry {
            key: Key::Person(person.addr.clone().into()),
            client: None,
            name,
            detail: person.addr.clone().into(),
            company,
            stats,
            addrs: vec![person.addr.clone()],
            domains: Vec::new(),
            accounts: person.accounts.clone(),
            hidden: hidden.contains(person.addr.as_str()),
            automated: is_automated(&person.addr),
            haystack,
        });
    }

    entries
}

fn haystack<'a>(
    name: &str,
    addrs: impl Iterator<Item = &'a String>,
    company: &str,
    tags: &[i64],
    tag_names: &HashMap<i64, String>,
) -> String {
    let mut text = name.to_lowercase();
    for addr in addrs {
        text.push(' ');
        text.push_str(addr);
    }
    text.push(' ');
    text.push_str(&company.to_lowercase());
    for tag in tags {
        if let Some(name) = tag_names.get(tag) {
            text.push_str(" #");
            text.push_str(&name.to_lowercase());
        }
    }
    text
}

/// The company an address suggests: its domain without the TLD, capitalized,
/// unless it is a mailbox provider.
fn company_from_addr(addr: &str) -> Option<String> {
    let domain = addr.rsplit_once('@')?.1;
    if is_freemail(domain) {
        return None;
    }
    let parts: Vec<&str> = domain.split('.').collect();
    // `mail.acme.co.uk` is Acme: the label before the registrable suffix,
    // which for a two-letter country code under `co`/`com`/`org` is one
    // further in.
    let core = match parts.as_slice() {
        [.., name, second, tld] if tld.len() == 2 && matches!(*second, "co" | "com" | "org" | "net" | "ac") => name,
        [.., name, _] => name,
        [only] => only,
        [] => return None,
    };
    let mut chars = core.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

/// Which part of the directory is being looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Everybody worth seeing: clients, and people you write to.
    Everyone,
    Clients,
    NeedsReply,
    GoneQuiet,
    Tag(i64),
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sort {
    /// Most recent contact first.
    #[default]
    Recent,
    /// Most mail first.
    Volume,
    Name,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::Recent, Sort::Volume, Sort::Name];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Recent => "Recent",
            Sort::Volume => "Most mail",
            Sort::Name => "Name",
        }
    }
}

/// An account's book: the account's address, and its id for matching mail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFilter {
    pub email: String,
    pub account: Option<AccountId>,
}

impl BookFilter {
    pub(crate) fn admits(&self, entry: &Entry) -> bool {
        let listed = entry.client.as_ref().is_some_and(|c| c.books.contains(&self.email));
        listed || self.account.is_some_and(|a| entry.accounts.contains(&a))
    }
}

/// How many are in each section, for the sidebar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    pub everyone: usize,
    pub clients: usize,
    pub needs_reply: usize,
    pub gone_quiet: usize,
    pub hidden: usize,
    pub tags: HashMap<i64, usize>,
}

/// Whether an entry is in the "everyone" view at all.
///
/// Clients always are. Somebody who is merely in your mail is there when you
/// have written to them, or when the two-way filter is off and they are not
/// a machine.
/// Which stages a book's board uses: its own address, when it has stages of
/// its own, or `""` for the shared ones.
pub fn pipeline_key<'a>(snapshot: &Snapshot, book: &'a str) -> &'a str {
    match !book.is_empty() && snapshot.stages.iter().any(|s| s.book == book) {
        true => book,
        false => "",
    }
}

/// The stages under one key, in order. See [`pipeline_key`].
pub fn pipeline<'a>(snapshot: &'a Snapshot, key: &str) -> Vec<&'a Stage> {
    snapshot.stages.iter().filter(|s| s.book == key).collect()
}

/// The books with stages of their own, in the order the book keeps them.
pub fn own_pipelines(snapshot: &Snapshot) -> Vec<String> {
    let mut books: Vec<String> = Vec::new();
    for stage in snapshot.stages.iter().filter(|s| !s.book.is_empty()) {
        if books.last() != Some(&stage.book) {
            books.push(stage.book.clone());
        }
    }
    books
}

/// A stage, and where it sits among its own book's — which is what its
/// colour goes by, so a book's third column is the same colour as anyone
/// else's third.
pub fn stage_index(snapshot: &Snapshot, stage: i64) -> Option<(usize, &Stage)> {
    let stage = snapshot.stages.iter().find(|s| s.id == stage)?;
    let index = snapshot.stages.iter().filter(|s| s.book == stage.book).position(|s| s.id == stage.id)?;
    Some((index, stage))
}

fn worth_listing(entry: &Entry, two_way_only: bool) -> bool {
    match entry.client {
        Some(_) => true,
        None => !entry.automated && (!two_way_only || entry.stats.is_two_way()),
    }
}

/// The directory: which entries to show, in what order, and the counts.
#[allow(clippy::too_many_arguments)]
pub fn filter(
    entries: &[Entry],
    section: Section,
    book: Option<&BookFilter>,
    search: &str,
    two_way_only: bool,
    sort: Sort,
    open_stages: &HashSet<i64>,
    hidden_books: &HashSet<AccountId>,
    now: i64,
) -> (Vec<usize>, Counts) {
    let needle = search.trim().to_lowercase();
    let terms: Vec<&str> = needle.split_whitespace().collect();
    let mut counts = Counts::default();
    let mut shown: Vec<usize> = Vec::new();

    for (index, entry) in entries.iter().enumerate() {
        if book.is_some_and(|b| !b.admits(entry)) {
            continue;
        }
        // Somebody whose mail is all in books you do not keep clients in is
        // not anybody here. A client stays: that was a choice.
        if entry.client.is_none() && !entry.accounts.is_empty() && entry.accounts.iter().all(|a| hidden_books.contains(a)) {
            continue;
        }
        if entry.hidden {
            counts.hidden += 1;
            if section == Section::Hidden && matches(entry, &terms) {
                shown.push(index);
            }
            continue;
        }
        if !worth_listing(entry, two_way_only) {
            continue;
        }

        let open = entry.stage().is_some_and(|s| open_stages.contains(&s));
        let since = entry.client.as_ref().and_then(|c| c.stage_since);
        let signal = signal(&entry.stats, open, since, entry.is_client(), now);

        counts.everyone += 1;
        if entry.is_client() {
            counts.clients += 1;
        }
        if matches!(signal, Signal::NeedsReply(_)) {
            counts.needs_reply += 1;
        }
        if matches!(signal, Signal::GoneQuiet(_)) {
            counts.gone_quiet += 1;
        }
        if let Some(client) = &entry.client {
            for tag in &client.tags {
                *counts.tags.entry(*tag).or_default() += 1;
            }
        }

        let in_section = match section {
            Section::Everyone => true,
            Section::Clients => entry.is_client(),
            Section::NeedsReply => matches!(signal, Signal::NeedsReply(_)),
            Section::GoneQuiet => matches!(signal, Signal::GoneQuiet(_)),
            Section::Tag(tag) => entry.client.as_ref().is_some_and(|c| c.tags.contains(&tag)),
            Section::Hidden => false,
        };
        if in_section && matches(entry, &terms) {
            shown.push(index);
        }
    }

    // Most of a list is people written to once or twice, so "most mail" is
    // mostly ties. Left in the order the index happened to store them they
    // read as no order at all; the most recent of them first reads as one.
    let recent = |e: &Entry| e.stats.last.or(e.client.as_ref().map(|c| c.updated)).unwrap_or(0);
    match sort {
        Sort::Recent => shown.sort_by_key(|&i| std::cmp::Reverse(recent(&entries[i]))),
        Sort::Volume => shown.sort_by_key(|&i| std::cmp::Reverse((entries[i].stats.messages, recent(&entries[i])))),
        Sort::Name => shown.sort_by_cached_key(|&i| entries[i].name.to_lowercase()),
    }
    (shown, counts)
}

fn matches(entry: &Entry, terms: &[&str]) -> bool {
    terms.iter().all(|term| entry.haystack.contains(term))
}

/// What the mailbox needs to know about a sender, to draw its pill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub client: i64,
    pub stage: Option<SharedString>,
    pub closed: bool,
}

/// Who is a client, by address and by domain, for windows that only ever ask
/// about one sender at a time.
#[derive(Debug, Default)]
pub struct Lookup {
    by_addr: HashMap<String, Badge>,
    by_domain: HashMap<String, Badge>,
}

impl Lookup {
    pub fn build(snapshot: &Snapshot) -> Self {
        let stages: HashMap<i64, (&str, bool)> =
            snapshot.stages.iter().map(|s| (s.id, (s.name.as_str(), s.kind.is_closed()))).collect();
        let mut lookup = Self::default();
        for client in snapshot.clients.iter().filter(|c| !c.hidden) {
            let stage = client.stage.and_then(|s| stages.get(&s));
            let badge = Badge {
                client: client.id,
                stage: stage.map(|(name, _)| SharedString::from(name.to_string())),
                closed: stage.is_some_and(|(_, closed)| *closed),
            };
            for addr in &client.addrs {
                lookup.by_addr.insert(addr.clone(), badge.clone());
            }
            for domain in &client.domains {
                lookup.by_domain.insert(domain.clone(), badge.clone());
            }
        }
        lookup
    }

    /// The client a sender is, by their address first and their domain after.
    pub fn find(&self, addr: &str) -> Option<&Badge> {
        let addr = address::normalize(addr);
        self.by_addr.get(&addr).or_else(|| {
            let domain = addr.rsplit_once('@')?.1;
            self.by_domain.get(domain)
        })
    }
}

/// Where the backfill of the people index has got to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Backfilling {
    /// Messages still to index.
    pub remaining: usize,
    /// The most there were waiting, for a fraction.
    pub total: usize,
}

impl Backfilling {
    pub fn fraction(&self) -> f32 {
        match self.total {
            0 => 1.,
            total => 1. - self.remaining as f32 / total as f32,
        }
    }
}

/// The book and everything shared about it.
pub struct ClientBook {
    pub store: Store,
    pub book: Arc<Book>,
    /// The index's own connection; `None` for an in-memory store.
    pub reader: Option<Arc<PeopleReader>>,
    pub snapshot: Arc<Snapshot>,
    pub lookup: Arc<Lookup>,
    /// Bumped whenever the book changes, so a window can tell a change it has
    /// already loaded from one it has not.
    pub version: u64,
    /// Bumped whenever mail arrives or the index grows.
    pub mail_version: u64,
    /// `None` once the index is complete.
    pub backfill: Option<Backfilling>,
    /// The Clients window, so asking for it again brings it forward.
    pub window: Option<gpui::WindowHandle<crate::clients_window::ClientsWindow>>,
    /// Sources being pulled right now, by id.
    pub pulling: HashSet<i64>,
    /// A message to a group, being sent or just sent.
    pub mailing: Option<crate::clients_mail::Mailing>,
    _backfill_task: Option<Task<()>>,
    _sync_task: Option<Task<()>>,
}

impl Global for ClientBook {}

/// Opens the book beside the store and starts its background work.
///
/// A book that will not open is logged and left out: the mail client is still
/// a mail client, and every caller checks for the global before using it.
pub fn init(store: Store, cx: &mut App) {
    let path = store
        .path()
        .map(Book::path_beside)
        .unwrap_or_else(|| std::env::temp_dir().join("spool-memory.clients.db"));
    let book = match Book::open(&path) {
        Ok(book) => Arc::new(book),
        Err(e) => {
            log::warn!("client book unavailable: {e:#}");
            return;
        }
    };
    let snapshot = book.snapshot().unwrap_or_else(|e| {
        log::warn!("reading the client book: {e:#}");
        Snapshot::default()
    });
    let reader = store.people_reader().ok().flatten().map(Arc::new);
    let lookup = Arc::new(Lookup::build(&snapshot));

    cx.set_global(ClientBook {
        store,
        book,
        reader,
        snapshot: Arc::new(snapshot),
        lookup,
        version: 0,
        mail_version: 0,
        backfill: None,
        window: None,
        pulling: HashSet::new(),
        mailing: None,
        _backfill_task: None,
        _sync_task: None,
    });

    let backfill = cx.spawn(async move |cx| run_backfill(cx).await);
    let sync = cx.spawn(async move |cx| run_sources(cx).await);
    cx.update_global::<ClientBook, _>(|clients, _| {
        clients._backfill_task = Some(backfill);
        clients._sync_task = Some(sync);
    });
}

/// Puts Clients away again: its window closed, its background work stopped,
/// and the book let go of.
///
/// The index stops being kept by the store's own switch, which the caller
/// throws; nothing here writes to the book, which keeps everything in it for
/// whenever Clients is added back.
pub fn shut_down(cx: &mut App) {
    let Some(clients) = cx.try_global::<ClientBook>() else { return };
    if let Some(mailing) = &clients.mailing {
        mailing.stop();
    }
    if let Some(window) = clients.window {
        window.update(cx, |_, window, _| window.remove_window()).ok();
    }
    // Dropping it drops the backfill and the source schedule with it.
    cx.remove_global::<ClientBook>();
}

/// Indexes the mail stored before the index existed, a batch at a time.
async fn run_backfill(cx: &mut gpui::AsyncApp) {
    cx.background_executor().timer(BACKFILL_DELAY).await;
    let Ok(store) = cx.read_global::<ClientBook, _>(|c, _| c.store.clone()) else { return };

    let mut steps = 0usize;
    loop {
        let step = {
            let store = store.clone();
            cx.background_executor().spawn(async move { store.backfill_people(BACKFILL_BATCH) }).await
        };
        let step = match step {
            Ok(step) => step,
            Err(e) => {
                log::warn!("indexing people: {e:#}");
                return;
            }
        };
        steps += 1;
        let finished = step.remaining == 0;
        let _ = cx.update_global::<ClientBook, _>(|clients, _| {
            clients.backfill = match finished {
                true => None,
                false => {
                    let total = clients.backfill.map_or(0, |b| b.total).max(step.remaining + step.indexed);
                    Some(Backfilling { remaining: step.remaining, total })
                }
            };
            // The window reloads on this, so not on every step: a few
            // hundred milliseconds of re-reading the index every forty.
            if finished || steps.is_multiple_of(20) {
                clients.mail_version += 1;
            }
        });
        if finished {
            if steps > 1 {
                log::info!("people index complete after {steps} batches");
            }
            return;
        }
        // Room for anything else that wants the store.
        cx.background_executor().timer(Duration::from_millis(15)).await;
    }
}

/// Pulls each source when it is due, for as long as the app runs.
async fn run_sources(cx: &mut gpui::AsyncApp) {
    loop {
        cx.background_executor().timer(Duration::from_secs(10)).await;
        let due: Vec<Source> = match cx.read_global::<ClientBook, _>(|c, _| c.book.clone()) {
            Ok(book) => {
                let now = now_seconds();
                cx.background_executor()
                    .spawn(async move { book.sources().unwrap_or_default() })
                    .await
                    .into_iter()
                    .filter(|s| s.due(now))
                    .collect()
            }
            Err(_) => return,
        };
        for source in due {
            let _ = cx.update(|cx| pull(source, cx));
        }
        cx.background_executor().timer(SOURCE_TICK - Duration::from_secs(10)).await;
    }
}

/// Pulls one source now, whatever its schedule says.
pub fn pull(source: Source, cx: &mut App) {
    let Some(clients) = cx.try_global::<ClientBook>() else { return };
    if clients.pulling.contains(&source.id) {
        return;
    }
    let book = clients.book.clone();
    cx.update_global::<ClientBook, _>(|clients, _| {
        clients.pulling.insert(source.id);
    });

    cx.spawn(async move |cx| {
        let id = source.id;
        let started = now_seconds();
        let result = crate::client_sync::pull(&source, started).await;
        let book_for_import = book.clone();
        let note = cx
            .background_executor()
            .spawn(async move {
                let (worked, note) = match result {
                    Ok(contacts) => {
                        match book_for_import.import(&contacts, Some(&source.book), Some(source.id), &source.tags, started) {
                            Ok(report) => (true, report.summary()),
                            Err(e) => (false, format!("Could not save: {e:#}")),
                        }
                    }
                    Err(e) => (false, format!("{e:#}")),
                };
                if let Err(e) = book_for_import.record_pull(source.id, started, worked, &note) {
                    log::warn!("recording a pull: {e:#}");
                }
                note
            })
            .await;
        log::info!("source {id}: {note}");
        let _ = cx.update(|cx| {
            if cx.has_global::<ClientBook>() {
                cx.update_global::<ClientBook, _>(|clients, _| {
                    clients.pulling.remove(&id);
                });
            }
            reload(cx);
        });
    })
    .detach();
}

/// Re-reads the book after a change, and tells everyone who is watching.
pub fn reload(cx: &mut App) {
    let Some(clients) = cx.try_global::<ClientBook>() else { return };
    let book = clients.book.clone();
    cx.spawn(async move |cx| {
        let read = cx.background_executor().spawn(async move { book.snapshot() }).await;
        match read {
            Ok(snapshot) => {
                let lookup = Arc::new(Lookup::build(&snapshot));
                let _ = cx.update_global::<ClientBook, _>(|clients, _| {
                    clients.snapshot = Arc::new(snapshot);
                    clients.lookup = lookup;
                    clients.version += 1;
                });
            }
            Err(e) => log::warn!("reading the client book: {e:#}"),
        }
    })
    .detach();
}

/// Mail arrived: the index has grown, and whatever shows it should look again.
pub fn mail_changed(cx: &mut App) {
    if cx.has_global::<ClientBook>() {
        cx.update_global::<ClientBook, _>(|clients, _| clients.mail_version += 1);
    }
}

/// The client a sender is, if they are one, for the mailbox's pill.
pub fn badge_for(addr: &str, cx: &App) -> Option<Badge> {
    cx.try_global::<ClientBook>()?.lookup.find(addr).cloned()
}

/// Which stages are still going.
pub fn open_stages(snapshot: &Snapshot) -> HashSet<i64> {
    snapshot.stages.iter().filter(|s| s.kind == StageKind::Open).map(|s| s.id).collect()
}

/// A span of time, as a card says it: `3d`, `5w`, `2mo`.
pub fn ago(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        s if s < 3_600 => format!("{}m", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 14 * 86_400 => format!("{}d", s / 86_400),
        s if s < 60 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s if s < 365 * 86_400 => format!("{}mo", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

/// A deal's value, as a column total and a card say it.
pub fn money(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    match value < 0 {
        true => format!("-${grouped}"),
        false => format!("${grouped}"),
    }
}

/// Reads a deal value as somebody types it: `18k`, `$18,000`, `2.5m`.
pub fn parse_money(text: &str) -> Option<i64> {
    let cleaned: String = text.trim().chars().filter(|c| !matches!(c, '$' | ',' | ' ' | '€' | '£')).collect();
    if cleaned.is_empty() {
        return None;
    }
    let (number, scale) = match cleaned.to_lowercase().chars().last()? {
        'k' => (&cleaned[..cleaned.len() - 1], 1_000.),
        'm' => (&cleaned[..cleaned.len() - 1], 1_000_000.),
        _ => (cleaned.as_str(), 1.),
    };
    let value: f64 = number.parse().ok()?;
    Some((value * scale).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_store::book::{Stage, Tag};

    const NOW: i64 = 1_760_000_000;
    const DAY: i64 = 86_400;

    fn person(id: i64, addr: &str, name: &str, sent: u32, received: u32, last_in: Option<i64>, last_out: Option<i64>) -> Correspondent {
        let last = last_in.max(last_out).unwrap_or(NOW - 100 * DAY);
        Correspondent {
            id,
            addr: addr.into(),
            name: name.into(),
            messages: sent + received,
            received,
            sent,
            first: NOW - 400 * DAY,
            last,
            last_inbound: last_in,
            last_outbound: last_out,
            accounts: vec![AccountId(1)],
        }
    }

    fn client(id: i64, name: &str, addrs: &[&str]) -> Client {
        Client {
            id,
            kind: Kind::Person,
            name: name.into(),
            company: String::new(),
            addrs: addrs.iter().map(|a| a.to_string()).collect(),
            domains: Vec::new(),
            books: Vec::new(),
            tags: Vec::new(),
            stage: None,
            stage_pos: 0.,
            stage_since: None,
            value: None,
            hidden: false,
            created: NOW - 10 * DAY,
            updated: NOW - 10 * DAY,
        }
    }

    fn snapshot(clients: Vec<Client>) -> Snapshot {
        Snapshot {
            clients,
            stages: vec![
                Stage { id: 1, name: "Lead".into(), position: 0., kind: StageKind::Open, book: String::new() },
                Stage { id: 2, name: "Won".into(), position: 1., kind: StageKind::Won, book: String::new() },
            ],
            tags: vec![Tag { id: 9, name: "vip".into(), color: 0 }],
            hidden: Vec::new(),
            hidden_books: Vec::new(),
        }
    }

    fn me() -> HashSet<String> {
        ["wes@example.com".to_string()].into_iter().collect()
    }

    fn names(entries: &[Entry], shown: &[usize]) -> Vec<String> {
        shown.iter().map(|&i| entries[i].name.to_string()).collect()
    }

    #[test]
    fn people_you_write_to_are_listed_and_newsletters_are_not() {
        let people = vec![
            person(1, "ana@acme.com", "Ana Ruiz", 3, 4, Some(NOW - DAY), Some(NOW - 2 * DAY)),
            person(2, "hello@shop.example", "Shop", 0, 40, Some(NOW - DAY), None),
            person(3, "noreply@service.example", "", 1, 9, Some(NOW), Some(NOW - 9 * DAY)),
        ];
        let entries = everyone(&people, &snapshot(vec![]), &me(), &HashMap::new());
        let (shown, counts) = filter(&entries, Section::Everyone, None, "", true, Sort::Recent, &HashSet::new(), &HashSet::new(), NOW);
        assert_eq!(names(&entries, &shown), ["Ana Ruiz"]);
        assert_eq!(counts.everyone, 1);

        // With the two-way filter off the shop appears, and the machine still
        // does not.
        let (shown, _) = filter(&entries, Section::Everyone, None, "", false, Sort::Recent, &HashSet::new(), &HashSet::new(), NOW);
        assert_eq!(names(&entries, &shown), ["Ana Ruiz", "Shop"]);
    }

    #[test]
    fn most_mail_puts_the_busiest_first_and_the_most_recent_of_a_tie() {
        let people = vec![
            person(1, "ada@example.com", "Ada", 1, 0, None, Some(NOW - 30 * DAY)),
            person(2, "zed@example.com", "Zed", 5, 9, Some(NOW - 60 * DAY), Some(NOW - 61 * DAY)),
            person(3, "mia@example.com", "Mia", 1, 0, None, Some(NOW - DAY)),
        ];
        let entries = everyone(&people, &snapshot(vec![]), &me(), &HashMap::new());
        let order = |sort| names(&entries, &filter(&entries, Section::Everyone, None, "", true, sort, &HashSet::new(), &HashSet::new(), NOW).0);

        // Ada came first into the index, so without the tie broken she would
        // lead Mia under "most mail" for no reason anyone could see.
        assert_eq!(order(Sort::Volume), ["Zed", "Mia", "Ada"]);
        assert_eq!(order(Sort::Recent), ["Mia", "Ada", "Zed"]);
        assert_eq!(order(Sort::Name), ["Ada", "Mia", "Zed"]);
    }

    #[test]
    fn a_book_put_away_takes_its_people_and_leaves_its_clients() {
        let mut personal = person(1, "mom@example.com", "Mom", 3, 4, Some(NOW - DAY), Some(NOW - DAY));
        personal.accounts = vec![AccountId(8)];
        let mut both = person(2, "bo@acme.com", "Bo", 1, 1, Some(NOW - DAY), Some(NOW - DAY));
        both.accounts = vec![AccountId(8), AccountId(1)];
        let mut client_there = person(3, "ana@acme.com", "Ana", 1, 1, Some(NOW - DAY), Some(NOW - DAY));
        client_there.accounts = vec![AccountId(8)];
        let ana = client(7, "Ana", &["ana@acme.com"]);
        let entries = everyone(&[personal, both, client_there], &snapshot(vec![ana]), &me(), &HashMap::new());

        let away: HashSet<AccountId> = [AccountId(8)].into_iter().collect();
        let (shown, counts) =
            filter(&entries, Section::Everyone, None, "", true, Sort::Name, &HashSet::new(), &away, NOW);
        assert_eq!(names(&entries, &shown), ["Ana", "Bo"], "Bo has mail in a book still in use");
        assert_eq!(counts.everyone, 2);
    }

    #[test]
    fn a_book_uses_its_own_stages_once_it_has_some_and_colours_go_by_place_in_them() {
        let mut snapshot = snapshot(vec![]);
        assert_eq!(pipeline_key(&snapshot, "hello@launch.example"), "");
        snapshot.stages.push(Stage {
            id: 5,
            name: "Signed up".into(),
            position: 0.,
            kind: StageKind::Open,
            book: "hello@launch.example".into(),
        });
        snapshot.stages.push(Stage {
            id: 6,
            name: "Paying".into(),
            position: 1.,
            kind: StageKind::Won,
            book: "hello@launch.example".into(),
        });

        assert_eq!(pipeline_key(&snapshot, "hello@launch.example"), "hello@launch.example");
        assert_eq!(pipeline_key(&snapshot, "wes@studio.example"), "");
        let names: Vec<&str> = pipeline(&snapshot, "hello@launch.example").iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Signed up", "Paying"]);
        assert_eq!(own_pipelines(&snapshot), ["hello@launch.example"]);
        assert_eq!(stage_index(&snapshot, 6).map(|(i, _)| i), Some(1), "second of its own, not sixth overall");
    }

    #[test]
    fn a_client_absorbs_its_addresses_and_is_listed_once() {
        let people = vec![
            person(1, "ana@acme.com", "Ana Ruiz", 2, 2, Some(NOW - 5 * DAY), Some(NOW - 6 * DAY)),
            person(2, "ana@home.example", "Ana", 1, 1, Some(NOW - DAY), Some(NOW - 3 * DAY)),
        ];
        let ana = client(7, "", &["ana@acme.com", "ana@home.example"]);
        let entries = everyone(&people, &snapshot(vec![ana]), &me(), &HashMap::new());
        assert_eq!(entries.len(), 1, "both addresses are the one client");
        let entry = &entries[0];
        assert_eq!(entry.name, "Ana Ruiz", "named from the mail when the book has no name");
        assert_eq!(entry.stats.messages, 6);
        assert_eq!(entry.stats.last_in, Some(NOW - DAY));
        assert_eq!(entry.company, "Acme");
    }

    #[test]
    fn a_company_sums_everyone_at_its_domain_without_hiding_them() {
        let people = vec![
            person(1, "ana@acme.com", "Ana", 2, 2, Some(NOW - DAY), Some(NOW - 2 * DAY)),
            person(2, "bo@acme.com", "Bo", 1, 3, Some(NOW - 3 * DAY), Some(NOW - 4 * DAY)),
        ];
        let mut acme = client(5, "Acme", &[]);
        acme.kind = Kind::Company;
        acme.domains = vec!["acme.com".into()];
        let entries = everyone(&people, &snapshot(vec![acme]), &me(), &HashMap::new());
        assert_eq!(entries.len(), 3);
        let company = entries.iter().find(|e| e.key == Key::Client(5)).unwrap();
        assert_eq!(company.stats.messages, 8);
        assert_eq!(company.detail, "@acme.com");
    }

    #[test]
    fn needs_reply_is_their_message_last_and_recent() {
        let mut stats = Stats { sent: 2, last_in: Some(NOW - DAY), last_out: Some(NOW - 2 * DAY), ..Default::default() };
        stats.last = stats.last_in;
        assert_eq!(signal(&stats, false, None, false, NOW), Signal::NeedsReply(NOW - DAY));

        stats.last_out = Some(NOW);
        assert_eq!(signal(&stats, false, None, false, NOW), Signal::Nothing, "you answered");

        let old = Stats { sent: 2, last_in: Some(NOW - 60 * DAY), last: Some(NOW - 60 * DAY), ..Default::default() };
        assert_eq!(signal(&old, false, None, false, NOW), Signal::Nothing, "a conversation that ended");

        let stranger = Stats { sent: 0, last_in: Some(NOW - DAY), last: Some(NOW - DAY), ..Default::default() };
        assert_eq!(signal(&stranger, false, None, false, NOW), Signal::Nothing, "a stranger is not owed a reply");
        assert!(matches!(signal(&stranger, false, None, true, NOW), Signal::NeedsReply(_)), "a client is");
    }

    #[test]
    fn an_open_sale_with_nothing_for_a_fortnight_is_going_quiet() {
        let stats = Stats { sent: 1, last_out: Some(NOW - 20 * DAY), last: Some(NOW - 20 * DAY), ..Default::default() };
        assert_eq!(signal(&stats, true, Some(NOW - 30 * DAY), true, NOW), Signal::GoneQuiet(NOW - 20 * DAY));
        assert_eq!(signal(&stats, false, None, true, NOW), Signal::Nothing, "only a sale in progress goes quiet");

        // Imported a month ago and never written to.
        let silent = Stats::default();
        assert_eq!(signal(&silent, true, Some(NOW - 30 * DAY), true, NOW), Signal::GoneQuiet(NOW - 30 * DAY));
        assert_eq!(signal(&silent, true, Some(NOW - DAY), true, NOW), Signal::Nothing, "just put on the board");
    }

    #[test]
    fn a_book_shows_its_own_clients_and_everyone_with_mail_in_its_account() {
        let people = vec![
            person(1, "ana@acme.com", "Ana", 1, 1, Some(NOW - DAY), Some(NOW - 2 * DAY)),
            Correspondent { accounts: vec![AccountId(2)], ..person(2, "bo@other.example", "Bo", 1, 1, Some(NOW), Some(NOW)) },
        ];
        let mut signup = client(3, "Signup", &["new@gallery.example"]);
        signup.books = vec!["wes@camp.example".into()];
        let entries = everyone(&people, &snapshot(vec![signup]), &me(), &HashMap::new());

        let gallery = BookFilter { email: "wes@camp.example".into(), account: Some(AccountId(2)) };
        let (shown, _) = filter(&entries, Section::Everyone, Some(&gallery), "", true, Sort::Name, &HashSet::new(), &HashSet::new(), NOW);
        assert_eq!(names(&entries, &shown), ["Bo", "Signup"]);
    }

    #[test]
    fn search_matches_every_word_across_name_address_company_and_tags() {
        let people = vec![person(1, "ana@acme.com", "Ana Ruiz", 1, 1, Some(NOW), Some(NOW))];
        let mut ana = client(1, "Ana Ruiz", &["ana@acme.com"]);
        ana.tags = vec![9];
        let tags: HashMap<i64, String> = [(9, "VIP".to_string())].into_iter().collect();
        let entries = everyone(&people, &snapshot(vec![ana]), &me(), &tags);
        let find = |q: &str| filter(&entries, Section::Everyone, None, q, true, Sort::Recent, &HashSet::new(), &HashSet::new(), NOW).0.len();
        assert_eq!(find("ruiz acme"), 1);
        assert_eq!(find("#vip"), 1);
        assert_eq!(find("ruiz globex"), 0);
    }

    #[test]
    fn you_are_never_in_your_own_book() {
        let people = vec![person(1, "wes@example.com", "Wes", 5, 5, Some(NOW), Some(NOW))];
        assert!(everyone(&people, &snapshot(vec![]), &me(), &HashMap::new()).is_empty());
    }

    #[test]
    fn the_pill_finds_a_client_by_address_then_by_domain() {
        let mut ana = client(1, "Ana", &["ana@acme.com"]);
        ana.stage = Some(1);
        let mut globex = client(2, "Globex", &[]);
        globex.domains = vec!["globex.com".into()];
        let lookup = Lookup::build(&snapshot(vec![ana, globex]));
        assert_eq!(lookup.find("Ana@Acme.com").unwrap().stage.as_ref().map(|s| s.as_ref()), Some("Lead"));
        assert_eq!(lookup.find("hank@globex.com").unwrap().client, 2);
        assert!(lookup.find("someone@else.com").is_none());
    }

    #[test]
    fn companies_are_named_from_their_domain_and_freemail_is_nobody() {
        assert_eq!(company_from_addr("ana@acme.com").as_deref(), Some("Acme"));
        assert_eq!(company_from_addr("ana@mail.acme.co.uk").as_deref(), Some("Acme"));
        assert_eq!(company_from_addr("ana@gmail.com"), None);
    }

    #[test]
    fn money_reads_the_way_people_type_it_and_prints_the_way_they_read_it() {
        assert_eq!(parse_money("18k"), Some(18_000));
        assert_eq!(parse_money("$18,500"), Some(18_500));
        assert_eq!(parse_money("2.5m"), Some(2_500_000));
        assert_eq!(parse_money("lots"), None);
        assert_eq!(money(18_000), "$18,000");
        assert_eq!(money(950), "$950");
        assert_eq!(money(1_234_567), "$1,234,567");
    }

    #[test]
    fn spans_of_time_read_at_a_glance() {
        assert_eq!(ago(30), "1m");
        assert_eq!(ago(3 * 3_600), "3h");
        assert_eq!(ago(3 * DAY), "3d");
        assert_eq!(ago(21 * DAY), "3w");
        assert_eq!(ago(90 * DAY), "3mo");
    }
}
