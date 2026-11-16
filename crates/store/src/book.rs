//! The client book: what you know about the people you sell to.
//!
//! A database of its own, beside the mail store rather than inside it, and the
//! reason is what each one is. The mail store is a cache of your servers — it
//! can be deleted and synced back, and the code says so in several places. This
//! is the opposite: every row in it is something a person typed, imported or
//! decided, and none of it exists anywhere else. Keeping the two apart is what
//! means rebuilding the mail can never take a note with it, and that backing
//! this up is copying one file.
//!
//! It follows from that that nothing here may hold one of the mail store's
//! row ids, which a rebuilt store hands out again in a different order. People
//! are named by their addresses and companies by their domains; the people
//! index ([`crate::people`]) turns those back into mail.
//!
//! A *correspondent* is anybody the index has seen; a *client* is somebody you
//! have done something about — noted, tagged, staged, imported. Most people in
//! your mail are never clients, and the book holds only the ones who are.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use spool_core::address;

/// What a client is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Person,
    /// A company: usually everyone at a domain.
    Company,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Person => "person",
            Kind::Company => "company",
        }
    }

    fn from_stored(value: &str) -> Self {
        match value {
            "company" => Kind::Company,
            _ => Kind::Person,
        }
    }
}

/// Where a stage sits in the sale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageKind {
    #[default]
    Open,
    Won,
    Lost,
}

impl StageKind {
    fn as_str(self) -> &'static str {
        match self {
            StageKind::Open => "open",
            StageKind::Won => "won",
            StageKind::Lost => "lost",
        }
    }

    fn from_stored(value: &str) -> Self {
        match value {
            "won" => StageKind::Won,
            "lost" => StageKind::Lost,
            _ => StageKind::Open,
        }
    }

    /// Whether a client in this stage is finished with.
    pub fn is_closed(self) -> bool {
        self != StageKind::Open
    }
}

/// A column on the board.
#[derive(Debug, Clone, PartialEq)]
pub struct Stage {
    pub id: i64,
    pub name: String,
    /// Where it sits among its book's stages.
    pub position: f64,
    pub kind: StageKind,
    /// The book whose stages these are, by its account's address, or empty
    /// for the stages every book shares until it is given its own.
    pub book: String,
}

/// A tag, as the book has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    /// Index into the theme's avatar palette.
    pub color: u8,
}

/// A client, as the directory and the board need one.
#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    pub id: i64,
    pub kind: Kind,
    pub name: String,
    /// For a person: where they work, as written. Not a link — a company
    /// client is found by domain, and this is only what to show.
    pub company: String,
    /// Normalized addresses that are this client.
    pub addrs: Vec<String>,
    /// Domains that are this client: everyone at them.
    pub domains: Vec<String>,
    /// Which account's books it is in, by that account's address. Empty is
    /// in none in particular — every book shows it.
    pub books: Vec<String>,
    /// Tag ids.
    pub tags: Vec<i64>,
    pub stage: Option<i64>,
    /// Where it sits in its stage's column. Fractional, so a card dropped
    /// between two others is one write.
    pub stage_pos: f64,
    pub stage_since: Option<i64>,
    /// The deal, in whole units of whatever currency it is in.
    pub value: Option<i64>,
    pub hidden: bool,
    pub created: i64,
    pub updated: i64,
}

impl Client {
    /// The address to write to, when there is one.
    pub fn primary_addr(&self) -> Option<&str> {
        self.addrs.first().map(String::as_str)
    }
}

/// A note on a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub id: i64,
    pub client: i64,
    pub body: String,
    pub pinned: bool,
    pub created: i64,
    pub updated: i64,
}

/// Something that happened to a client, for its history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: i64,
    pub client: i64,
    pub ts: i64,
    pub kind: String,
    pub detail: String,
}

/// Everything about one client the profile shows beyond the [`Client`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Detail {
    pub notes: Vec<Note>,
    pub events: Vec<Event>,
    /// Anything an import brought that has no column of its own: a phone
    /// number, a plan, the date they signed up. In key order.
    pub fields: Vec<(String, String)>,
}

/// The whole book in one read, for the window to hold.
///
/// One read rather than a query per row, because the directory needs to know
/// about every client to say which correspondents are one — and a few thousand
/// clients is a few hundred kilobytes, which is a small price for `render`
/// never having to ask.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub clients: Vec<Client>,
    pub stages: Vec<Stage>,
    pub tags: Vec<Tag>,
    /// Addresses you have asked not to see, that are not clients.
    pub hidden: Vec<String>,
    /// Books you do not keep clients in, by their account's address.
    pub hidden_books: Vec<String>,
}

/// One contact arriving from outside: a file, a feed.
pub use spool_core::contacts::Contact;

/// What an import did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Imported {
    pub added: usize,
    pub updated: usize,
    /// Rows with no usable address.
    pub skipped: usize,
}

impl Imported {
    /// The sentence for the status line.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.added > 0 {
            parts.push(format!("{} added", self.added));
        }
        if self.updated > 0 {
            parts.push(format!("{} updated", self.updated));
        }
        if self.skipped > 0 {
            parts.push(format!("{} skipped without an address", self.skipped));
        }
        match parts.is_empty() {
            true => "Nothing new".into(),
            false => parts.join(", "),
        }
    }
}

/// Where contacts are pulled from on a schedule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    pub id: i64,
    pub name: String,
    /// The account whose book they go into.
    pub book: String,
    /// `https://…`. `{since}` in it is replaced with the time of the last
    /// successful pull, in unix seconds, and `{since_iso}` with the same as
    /// an RFC 3339 date — so an endpoint that can answer "who signed up after
    /// this" is asked only for that.
    pub url: String,
    pub format: SourceFormat,
    /// Tags put on everyone the source brings in.
    pub tags: Vec<String>,
    /// Seconds between pulls. Zero is by hand only.
    pub every: i64,
    pub enabled: bool,
    pub last_run: Option<i64>,
    /// When the last pull that worked started. What `{since}` is.
    pub last_success: Option<i64>,
    /// What the last pull said, worked or not.
    pub last_note: String,
}

impl Source {
    /// Whether a pull is due at `now`.
    pub fn due(&self, now: i64) -> bool {
        self.enabled && self.every > 0 && self.last_run.is_none_or(|last| now - last >= self.every)
    }
}

/// What a source answers in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SourceFormat {
    /// An array of objects, or an object holding one.
    #[default]
    Json,
    Csv,
}

impl SourceFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceFormat::Json => "json",
            SourceFormat::Csv => "csv",
        }
    }

    pub fn from_stored(value: &str) -> Self {
        match value {
            "csv" => SourceFormat::Csv,
            _ => SourceFormat::Json,
        }
    }
}

/// Changes to a client's own fields. `None` leaves a field as it is.
#[derive(Debug, Clone, Default)]
pub struct Edit {
    pub name: Option<String>,
    pub company: Option<String>,
    pub kind: Option<Kind>,
    /// `Some(None)` clears it.
    pub value: Option<Option<i64>>,
}

/// What a stage change replaced, so it can be put back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub stage: Option<i64>,
    pub pos: f64,
    pub since: Option<i64>,
}

/// The stages a new book starts with: the usual sale, in order.
const DEFAULT_STAGES: [(&str, StageKind); 6] = [
    ("Lead", StageKind::Open),
    ("Qualified", StageKind::Open),
    ("Proposal", StageKind::Open),
    ("Negotiation", StageKind::Open),
    ("Won", StageKind::Won),
    ("Lost", StageKind::Lost),
];

pub struct Book {
    conn: Mutex<Connection>,
}

impl Book {
    /// Where the book lives for a given mail store: beside it, so `--db
    /// elsewhere.db` gets a book of its own rather than writing into the real
    /// one.
    pub fn path_beside(store: &Path) -> PathBuf {
        let stem = store.file_stem().and_then(|s| s.to_str()).unwrap_or("mail");
        let name = match stem {
            "mail" => "clients.db".to_string(),
            other => format!("{other}.clients.db"),
        };
        store.with_file_name(name)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening the client book at {}", path.display()))?;
        Self::from_connection(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // `FULL` rather than the mail store's `NORMAL`. The mail store can
        // lose its last transaction to a crash and get it back from the
        // server; a note typed into this one has nowhere to come back from.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;",
        )?;
        let book = Self { conn: Mutex::new(conn) };
        book.migrate()?;
        Ok(book)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.lock();
        let version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);

        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS stages (
                     id       INTEGER PRIMARY KEY,
                     name     TEXT NOT NULL,
                     position REAL NOT NULL,
                     kind     TEXT NOT NULL DEFAULT 'open'
                 );

                 CREATE TABLE IF NOT EXISTS clients (
                     id          INTEGER PRIMARY KEY,
                     kind        TEXT NOT NULL DEFAULT 'person',
                     name        TEXT NOT NULL DEFAULT '',
                     company     TEXT NOT NULL DEFAULT '',
                     stage       INTEGER REFERENCES stages(id) ON DELETE SET NULL,
                     stage_pos   REAL NOT NULL DEFAULT 0,
                     stage_since INTEGER,
                     value       INTEGER,
                     hidden      INTEGER NOT NULL DEFAULT 0,
                     created     INTEGER NOT NULL,
                     updated     INTEGER NOT NULL
                 );

                 -- An address or a domain belongs to one client at most: the
                 -- constraint says so, and it is what makes merging two
                 -- clients a move of rows rather than a judgement about
                 -- duplicates. With a rowid, unlike the tables around it,
                 -- because the order addresses were added in is the order to
                 -- show them in — the first is the one to write to.
                 CREATE TABLE IF NOT EXISTS identities (
                     client INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     kind   TEXT NOT NULL,
                     value  TEXT NOT NULL,
                     UNIQUE (kind, value)
                 );
                 CREATE INDEX IF NOT EXISTS idx_identities_client ON identities(client);

                 CREATE TABLE IF NOT EXISTS client_books (
                     client INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     book   TEXT NOT NULL,
                     PRIMARY KEY (client, book)
                 ) WITHOUT ROWID;

                 CREATE TABLE IF NOT EXISTS fields (
                     client INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     key    TEXT NOT NULL,
                     value  TEXT NOT NULL,
                     PRIMARY KEY (client, key)
                 ) WITHOUT ROWID;

                 CREATE TABLE IF NOT EXISTS notes (
                     id      INTEGER PRIMARY KEY,
                     client  INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     body    TEXT NOT NULL,
                     pinned  INTEGER NOT NULL DEFAULT 0,
                     created INTEGER NOT NULL,
                     updated INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_notes_client ON notes(client, created);

                 CREATE TABLE IF NOT EXISTS tags (
                     id    INTEGER PRIMARY KEY,
                     name  TEXT NOT NULL UNIQUE COLLATE NOCASE,
                     color INTEGER NOT NULL DEFAULT 0
                 );

                 CREATE TABLE IF NOT EXISTS client_tags (
                     client INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     tag    INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                     PRIMARY KEY (client, tag)
                 ) WITHOUT ROWID;

                 CREATE TABLE IF NOT EXISTS events (
                     id     INTEGER PRIMARY KEY,
                     client INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     ts     INTEGER NOT NULL,
                     kind   TEXT NOT NULL,
                     detail TEXT NOT NULL DEFAULT ''
                 );
                 CREATE INDEX IF NOT EXISTS idx_events_client ON events(client, ts);

                 -- Correspondents you asked not to see. Not clients: hiding a
                 -- newsletter should not make it one.
                 CREATE TABLE IF NOT EXISTS hidden (
                     addr TEXT PRIMARY KEY
                 ) WITHOUT ROWID;

                 CREATE TABLE IF NOT EXISTS sources (
                     id           INTEGER PRIMARY KEY,
                     name         TEXT NOT NULL,
                     book         TEXT NOT NULL DEFAULT '',
                     url          TEXT NOT NULL,
                     format       TEXT NOT NULL DEFAULT 'json',
                     tags         TEXT NOT NULL DEFAULT '',
                     every        INTEGER NOT NULL DEFAULT 3600,
                     enabled      INTEGER NOT NULL DEFAULT 1,
                     last_run     INTEGER,
                     last_success INTEGER,
                     last_note    TEXT NOT NULL DEFAULT ''
                 );

                 -- Which client each of a source's contacts became, by the
                 -- source's own name for them.
                 CREATE TABLE IF NOT EXISTS source_items (
                     source   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
                     external TEXT NOT NULL,
                     client   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
                     PRIMARY KEY (source, external)
                 ) WITHOUT ROWID;",
            )?;

            let mut insert = conn.prepare("INSERT INTO stages (name, position, kind) VALUES (?1, ?2, ?3)")?;
            for (index, (name, kind)) in DEFAULT_STAGES.iter().enumerate() {
                insert.execute(params![name, index as f64, kind.as_str()])?;
            }
            drop(insert);
            conn.execute_batch("PRAGMA user_version = 1;")?;
        }

        if version < 2 {
            // Stages per book. Every stage there is so far is the shared set,
            // which is what the empty book means, so the default says so.
            let has_book: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info('stages') WHERE name = 'book')",
                [],
                |r| r.get(0),
            )?;
            if !has_book {
                conn.execute_batch("ALTER TABLE stages ADD COLUMN book TEXT NOT NULL DEFAULT '';")?;
            }
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS hidden_books (
                     book TEXT PRIMARY KEY
                 ) WITHOUT ROWID;
                 PRAGMA user_version = 2;",
            )?;
        }
        Ok(())
    }

    /// Everything, for the window. See [`Snapshot`].
    pub fn snapshot(&self) -> Result<Snapshot> {
        let conn = self.lock();

        let stages = read_stages(&conn)?;

        let tags = {
            let mut stmt = conn.prepare("SELECT id, name, color FROM tags ORDER BY name COLLATE NOCASE")?;
            stmt.query_map([], |r| Ok(Tag { id: r.get(0)?, name: r.get(1)?, color: r.get(2)? }))?
                .collect::<Result<Vec<_>, _>>()?
        };

        let mut clients: Vec<Client> = {
            let mut stmt = conn.prepare(
                "SELECT id, kind, name, company, stage, stage_pos, stage_since, value, hidden, created, updated
                 FROM clients ORDER BY id",
            )?;
            stmt.query_map([], |r| {
                Ok(Client {
                    id: r.get(0)?,
                    kind: Kind::from_stored(&r.get::<_, String>(1)?),
                    name: r.get(2)?,
                    company: r.get(3)?,
                    addrs: Vec::new(),
                    domains: Vec::new(),
                    books: Vec::new(),
                    tags: Vec::new(),
                    stage: r.get(4)?,
                    stage_pos: r.get(5)?,
                    stage_since: r.get(6)?,
                    value: r.get(7)?,
                    hidden: r.get(8)?,
                    created: r.get(9)?,
                    updated: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let index: HashMap<i64, usize> = clients.iter().enumerate().map(|(i, c)| (c.id, i)).collect();

        {
            let mut stmt = conn.prepare("SELECT client, kind, value FROM identities ORDER BY rowid")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
            for row in rows {
                let (client, kind, value) = row?;
                let Some(&i) = index.get(&client) else { continue };
                match kind.as_str() {
                    "domain" => clients[i].domains.push(value),
                    _ => clients[i].addrs.push(value),
                }
            }
        }

        {
            let mut stmt = conn.prepare("SELECT client, book FROM client_books")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
                let (client, book) = row?;
                if let Some(&i) = index.get(&client) {
                    clients[i].books.push(book);
                }
            }
        }
        {
            let mut stmt = conn.prepare("SELECT client, tag FROM client_tags")?;
            for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                let (client, tag) = row?;
                if let Some(&i) = index.get(&client) {
                    clients[i].tags.push(tag);
                }
            }
        }

        let hidden = {
            let mut stmt = conn.prepare("SELECT addr FROM hidden")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?
        };
        let hidden_books = {
            let mut stmt = conn.prepare("SELECT book FROM hidden_books ORDER BY book")?;
            stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?
        };

        Ok(Snapshot { clients, stages, tags, hidden, hidden_books })
    }

    /// Notes, history and imported fields for one client.
    pub fn detail(&self, client: i64) -> Result<Detail> {
        let conn = self.lock();
        let notes = {
            let mut stmt = conn.prepare(
                "SELECT id, client, body, pinned, created, updated FROM notes
                 WHERE client = ?1 ORDER BY pinned DESC, created DESC",
            )?;
            stmt.query_map([client], read_note)?.collect::<Result<Vec<_>, _>>()?
        };
        let events = {
            let mut stmt = conn.prepare(
                "SELECT id, client, ts, kind, detail FROM events WHERE client = ?1 ORDER BY ts DESC, id DESC",
            )?;
            stmt.query_map([client], |r| {
                Ok(Event { id: r.get(0)?, client: r.get(1)?, ts: r.get(2)?, kind: r.get(3)?, detail: r.get(4)? })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        let fields = {
            let mut stmt = conn.prepare("SELECT key, value FROM fields WHERE client = ?1 ORDER BY key")?;
            stmt.query_map([client], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        Ok(Detail { notes, events, fields })
    }

    /// The client an address belongs to, directly or by its domain.
    pub fn client_for(&self, addr: &str) -> Result<Option<i64>> {
        let conn = self.lock();
        client_for(&conn, &address::normalize(addr))
    }

    /// Makes a client of somebody in your mail, or finds the one they are.
    ///
    /// What touching a correspondent does — a note, a tag, a stage. They
    /// become a client then and not before.
    pub fn ensure_client(&self, addr: &str, name: &str, book: Option<&str>, now: i64) -> Result<i64> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let id = ensure_person(&tx, addr, name, book, now)?;
        tx.commit()?;
        Ok(id)
    }

    /// As [`Self::ensure_client`], for several people in one transaction:
    /// `(address, name, book)` each, and their ids back in the same order.
    pub fn ensure_clients(&self, people: &[(String, String, Option<String>)], now: i64) -> Result<Vec<i64>> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let ids = people
            .iter()
            .map(|(addr, name, book)| ensure_person(&tx, addr, name, book.as_deref(), now))
            .collect::<Result<Vec<_>>>()?;
        tx.commit()?;
        Ok(ids)
    }

    /// Makes a company: everyone at a domain.
    pub fn ensure_company(&self, domain: &str, name: &str, now: i64) -> Result<i64> {
        let domain = domain.trim().trim_start_matches('@').to_lowercase();
        if domain.is_empty() || !domain.contains('.') {
            bail!("`{domain}` is not a domain");
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let id = match tx
            .query_row("SELECT client FROM identities WHERE kind = 'domain' AND value = ?1", [&domain], |r| r.get(0))
            .optional()?
        {
            Some(id) => id,
            None => {
                let id = insert_client(&tx, Kind::Company, name, "", now)?;
                add_identity(&tx, id, "domain", &domain)?;
                log_event(&tx, id, now, "created", "")?;
                id
            }
        };
        tx.commit()?;
        Ok(id)
    }

    /// A client typed in by hand.
    pub fn create(&self, kind: Kind, name: &str, addr: Option<&str>, book: Option<&str>, now: i64) -> Result<i64> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let id = insert_client(&tx, kind, name, "", now)?;
        if let Some(addr) = addr.map(address::normalize).filter(|a| a.contains('@')) {
            if tx
                .query_row("SELECT 1 FROM identities WHERE kind = 'addr' AND value = ?1", [&addr], |_| Ok(()))
                .optional()?
                .is_some()
            {
                bail!("{addr} is already a client");
            }
            add_identity(&tx, id, "addr", &addr)?;
        }
        if let Some(book) = book.filter(|b| !b.is_empty()) {
            tx.execute("INSERT OR IGNORE INTO client_books (client, book) VALUES (?1, ?2)", params![id, normalize_book(book)])?;
        }
        log_event(&tx, id, now, "created", "")?;
        tx.commit()?;
        Ok(id)
    }

    pub fn edit(&self, client: i64, edit: &Edit, now: i64) -> Result<()> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        if let Some(name) = &edit.name {
            tx.execute("UPDATE clients SET name = ?2 WHERE id = ?1", params![client, name.trim()])?;
        }
        if let Some(company) = &edit.company {
            tx.execute("UPDATE clients SET company = ?2 WHERE id = ?1", params![client, company.trim()])?;
        }
        if let Some(kind) = edit.kind {
            tx.execute("UPDATE clients SET kind = ?2 WHERE id = ?1", params![client, kind.as_str()])?;
        }
        if let Some(value) = edit.value {
            tx.execute("UPDATE clients SET value = ?2 WHERE id = ?1", params![client, value])?;
        }
        touch(&tx, client, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Puts a client in a stage, or takes it off the board with `None`.
    ///
    /// Returns where it was, which is what undo puts back. The history gets a
    /// line only when the stage itself changed: shuffling a card up its own
    /// column is not news.
    pub fn set_stage(&self, client: i64, stage: Option<i64>, pos: Option<f64>, now: i64) -> Result<Placement> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let before = place(&tx, client, stage, pos, now)?;
        tx.commit()?;
        Ok(before)
    }

    /// Puts several clients in one stage, or takes them all off the board,
    /// in one transaction. They land at the top of the column in the order
    /// given. Returns where each was, for undo.
    pub fn set_stages(&self, clients: &[i64], stage: Option<i64>, now: i64) -> Result<Vec<(i64, Placement)>> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        // Each lands above the last, so the last goes first.
        let mut before = clients
            .iter()
            .rev()
            .map(|&client| Ok((client, place(&tx, client, stage, None, now)?)))
            .collect::<Result<Vec<_>>>()?;
        before.reverse();
        tx.commit()?;
        Ok(before)
    }

    /// Puts a stage change back exactly as it was, history line and all.
    pub fn restore_placement(&self, client: i64, placement: Placement, now: i64) -> Result<()> {
        self.restore_placements(&[(client, placement)], now)
    }

    /// As [`Self::restore_placement`], for what [`Self::set_stages`] did.
    pub fn restore_placements(&self, placements: &[(i64, Placement)], now: i64) -> Result<()> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        for &(client, placement) in placements {
            put_back(&tx, client, placement, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_note(&self, client: i64, body: &str, now: i64) -> Result<i64> {
        let body = body.trim();
        if body.is_empty() {
            bail!("an empty note");
        }
        let conn = self.lock();
        let id = conn.query_row(
            "INSERT INTO notes (client, body, created, updated) VALUES (?1, ?2, ?3, ?3) RETURNING id",
            params![client, body, now],
            |r| r.get(0),
        )?;
        touch(&conn, client, now)?;
        Ok(id)
    }

    pub fn update_note(&self, note: i64, body: &str, now: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("UPDATE notes SET body = ?2, updated = ?3 WHERE id = ?1", params![note, body.trim(), now])?;
        Ok(())
    }

    pub fn pin_note(&self, note: i64, pinned: bool) -> Result<()> {
        let conn = self.lock();
        conn.execute("UPDATE notes SET pinned = ?2 WHERE id = ?1", params![note, pinned])?;
        Ok(())
    }

    /// Deletes a note, handing it back so undo can restore it.
    pub fn delete_note(&self, note: i64) -> Result<Option<Note>> {
        let conn = self.lock();
        let found = conn
            .query_row(
                "SELECT id, client, body, pinned, created, updated FROM notes WHERE id = ?1",
                [note],
                read_note,
            )
            .optional()?;
        conn.execute("DELETE FROM notes WHERE id = ?1", [note])?;
        Ok(found)
    }

    /// Puts a deleted note back under its own id, so it lands where it was.
    pub fn restore_note(&self, note: &Note) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT OR REPLACE INTO notes (id, client, body, pinned, created, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![note.id, note.client, note.body, note.pinned, note.created, note.updated],
        )?;
        Ok(())
    }

    /// Tags a client, making the tag if nobody has used it yet.
    pub fn tag(&self, client: i64, name: &str, now: i64) -> Result<i64> {
        let name = name.trim().trim_start_matches('#').trim();
        if name.is_empty() {
            bail!("an empty tag");
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let tag = ensure_tag(&tx, name)?;
        tx.execute("INSERT OR IGNORE INTO client_tags (client, tag) VALUES (?1, ?2)", params![client, tag])?;
        touch(&tx, client, now)?;
        tx.commit()?;
        Ok(tag)
    }

    pub fn untag(&self, client: i64, tag: i64, now: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM client_tags WHERE client = ?1 AND tag = ?2", params![client, tag])?;
        // A tag nobody wears any more is a tag nobody chose to keep.
        conn.execute("DELETE FROM tags WHERE id = ?1 AND NOT EXISTS (SELECT 1 FROM client_tags WHERE tag = ?1)", [tag])?;
        touch(&conn, client, now)?;
        Ok(())
    }

    /// Adds an address or a domain to a client.
    ///
    /// An address that is already somebody else's moves: saying "this is
    /// also Ana" is a decision about the address, and the client it used to
    /// belong to is left with whatever else it has.
    pub fn add_identity(&self, client: i64, value: &str, now: i64) -> Result<()> {
        let value = value.trim().to_lowercase();
        let (kind, value) = match value.strip_prefix('@') {
            Some(domain) => ("domain", domain.to_string()),
            None if value.contains('@') => ("addr", address::normalize(&value)),
            None if value.contains('.') => ("domain", value),
            None => bail!("`{value}` is neither an address nor a domain"),
        };
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM identities WHERE kind = ?1 AND value = ?2", params![kind, value])?;
        add_identity(&tx, client, kind, &value)?;
        if kind == "addr" {
            tx.execute("DELETE FROM hidden WHERE addr = ?1", [&value])?;
        }
        touch(&tx, client, now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove_identity(&self, client: i64, value: &str, now: i64) -> Result<()> {
        let conn = self.lock();
        let value = value.trim().trim_start_matches('@').to_lowercase();
        conn.execute("DELETE FROM identities WHERE client = ?1 AND value = ?2", params![client, value])?;
        touch(&conn, client, now)?;
        Ok(())
    }

    /// Folds one client into another: addresses, notes, tags, books, fields
    /// and history all move, and `from` is gone.
    pub fn merge(&self, into: i64, from: i64, now: i64) -> Result<()> {
        if into == from {
            return Ok(());
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let from_name: String = tx.query_row("SELECT name FROM clients WHERE id = ?1", [from], |r| r.get(0))?;
        for table in ["identities", "notes", "events"] {
            tx.execute(&format!("UPDATE {table} SET client = ?1 WHERE client = ?2"), params![into, from])?;
        }
        for table in ["client_tags", "client_books", "fields", "source_items"] {
            tx.execute(&format!("UPDATE OR IGNORE {table} SET client = ?1 WHERE client = ?2"), params![into, from])?;
        }
        // A stage on the one being folded in survives only where the one it
        // goes into had none: the client you kept is the one you meant.
        tx.execute(
            "UPDATE clients SET
                 stage = COALESCE(stage, (SELECT stage FROM clients WHERE id = ?2)),
                 stage_since = CASE WHEN stage IS NULL THEN (SELECT stage_since FROM clients WHERE id = ?2) ELSE stage_since END,
                 value = COALESCE(value, (SELECT value FROM clients WHERE id = ?2))
             WHERE id = ?1",
            params![into, from],
        )?;
        tx.execute("DELETE FROM clients WHERE id = ?1", [from])?;
        let detail = match from_name.trim().is_empty() {
            true => "Merged in another client".to_string(),
            false => format!("Merged in {from_name}"),
        };
        log_event(&tx, into, now, "merged", &detail)?;
        touch(&tx, into, now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete(&self, client: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM clients WHERE id = ?1", [client])?;
        Ok(())
    }

    pub fn set_hidden(&self, client: i64, hidden: bool, now: i64) -> Result<()> {
        let conn = self.lock();
        conn.execute("UPDATE clients SET hidden = ?2 WHERE id = ?1", params![client, hidden])?;
        touch(&conn, client, now)?;
        Ok(())
    }

    /// Hides somebody in your mail who is not a client.
    pub fn hide_addr(&self, addr: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("INSERT OR IGNORE INTO hidden (addr) VALUES (?1)", [address::normalize(addr)])?;
        Ok(())
    }

    pub fn unhide_addr(&self, addr: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM hidden WHERE addr = ?1", [address::normalize(addr)])?;
        Ok(())
    }

    pub fn add_to_book(&self, client: i64, book: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("INSERT OR IGNORE INTO client_books (client, book) VALUES (?1, ?2)", params![client, normalize_book(book)])?;
        Ok(())
    }

    pub fn remove_from_book(&self, client: i64, book: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM client_books WHERE client = ?1 AND book = ?2", params![client, normalize_book(book)])?;
        Ok(())
    }

    pub fn set_field(&self, client: i64, key: &str, value: &str, now: i64) -> Result<()> {
        let key = key.trim();
        if key.is_empty() {
            bail!("a field with no name");
        }
        let conn = self.lock();
        match value.trim().is_empty() {
            true => conn.execute("DELETE FROM fields WHERE client = ?1 AND key = ?2", params![client, key])?,
            false => conn.execute(
                "INSERT INTO fields (client, key, value) VALUES (?1, ?2, ?3)
                 ON CONFLICT(client, key) DO UPDATE SET value = excluded.value",
                params![client, key, value.trim()],
            )?,
        };
        touch(&conn, client, now)?;
        Ok(())
    }

    // Stages.

    /// Every stage of every book. See [`Stage::book`].
    pub fn stages(&self) -> Result<Vec<Stage>> {
        read_stages(&self.lock())
    }

    /// Adds a column to a book's stages — the shared ones for `""`, or for a
    /// book still using them — before the closed ones: a new step in the
    /// sale goes where the sale is still going.
    pub fn add_stage(&self, book: &str, name: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a stage needs a name");
        }
        let conn = self.lock();
        let stages = stages_of(&conn, &normalize_book(book))?;
        let book = stages.first().map(|s| s.book.clone()).unwrap_or_else(|| normalize_book(book));
        let last_open = stages.iter().filter(|s| !s.kind.is_closed()).map(|s| s.position).fold(f64::NAN, f64::max);
        let first_closed = stages.iter().filter(|s| s.kind.is_closed()).map(|s| s.position).fold(f64::NAN, f64::min);
        let position = match (last_open.is_nan(), first_closed.is_nan()) {
            (false, false) => (last_open + first_closed) / 2.,
            (false, true) => last_open + 1.,
            (true, false) => first_closed - 1.,
            (true, true) => 0.,
        };
        Ok(conn.query_row(
            "INSERT INTO stages (name, position, kind, book) VALUES (?1, ?2, 'open', ?3) RETURNING id",
            params![name, position, book],
            |r| r.get(0),
        )?)
    }

    pub fn rename_stage(&self, stage: i64, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a stage needs a name");
        }
        self.lock().execute("UPDATE stages SET name = ?2 WHERE id = ?1", params![stage, name])?;
        Ok(())
    }

    /// Moves a column one place left or right among its book's.
    pub fn move_stage(&self, stage: i64, right: bool) -> Result<()> {
        let conn = self.lock();
        let book: Option<String> =
            conn.query_row("SELECT book FROM stages WHERE id = ?1", [stage], |r| r.get(0)).optional()?;
        let Some(book) = book else { return Ok(()) };
        let stages: Vec<Stage> = read_stages(&conn)?.into_iter().filter(|s| s.book == book).collect();
        let Some(at) = stages.iter().position(|s| s.id == stage) else { return Ok(()) };
        let other = match right {
            true => at + 1,
            false => match at.checked_sub(1) {
                Some(other) => other,
                None => return Ok(()),
            },
        };
        let Some(neighbour) = stages.get(other) else { return Ok(()) };
        let tx = conn.unchecked_transaction()?;
        tx.execute("UPDATE stages SET position = ?2 WHERE id = ?1", params![stage, neighbour.position])?;
        tx.execute("UPDATE stages SET position = ?2 WHERE id = ?1", params![neighbour.id, stages[at].position])?;
        tx.commit()?;
        Ok(())
    }

    /// Deletes a column. Its clients come off the board rather than going
    /// with it: they are still clients.
    pub fn delete_stage(&self, stage: i64) -> Result<()> {
        self.lock().execute("DELETE FROM stages WHERE id = ?1", [stage])?;
        Ok(())
    }

    /// Gives a book stages of its own: a copy of the shared ones, to change
    /// without changing anyone else's.
    ///
    /// `clients` are the ones on that book's board, and those on a shared
    /// stage move to its copy — same name, same place in the column, same
    /// time in the stage — so the board looks the same the moment after.
    /// Which clients those are is the window's to say: a book's board shows
    /// whoever has mail in its account, and the mail is not in this file.
    pub fn own_stages(&self, book: &str, clients: &[i64]) -> Result<()> {
        let book = normalize_book(book);
        if book.is_empty() {
            bail!("the shared stages are nobody's own");
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let all = read_stages(&tx)?;
        if all.iter().any(|s| s.book == book) {
            return Ok(());
        }
        let mut copies: HashMap<i64, i64> = HashMap::new();
        for stage in all.iter().filter(|s| s.book.is_empty()) {
            let id: i64 = tx.query_row(
                "INSERT INTO stages (name, position, kind, book) VALUES (?1, ?2, ?3, ?4) RETURNING id",
                params![stage.name, stage.position, stage.kind.as_str(), book],
                |r| r.get(0),
            )?;
            copies.insert(stage.id, id);
        }
        let mut moved = tx.prepare("UPDATE clients SET stage = ?2 WHERE id = ?1 AND stage = ?3")?;
        for &client in clients {
            for (shared, copy) in &copies {
                moved.execute(params![client, copy, shared])?;
            }
        }
        drop(moved);
        tx.commit()?;
        Ok(())
    }

    /// Puts a book back on the shared stages and deletes its own.
    ///
    /// Its clients go to the shared stage of the same name; one in a stage
    /// the shared set has no name for comes off the board, and its history
    /// says so.
    pub fn share_stages(&self, book: &str, now: i64) -> Result<()> {
        let book = normalize_book(book);
        if book.is_empty() {
            return Ok(());
        }
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let all = read_stages(&tx)?;
        let shared: HashMap<String, i64> =
            all.iter().filter(|s| s.book.is_empty()).map(|s| (s.name.to_lowercase(), s.id)).collect();
        for own in all.iter().filter(|s| s.book == book) {
            let clients: Vec<i64> = {
                let mut stmt = tx.prepare("SELECT id FROM clients WHERE stage = ?1")?;
                stmt.query_map([own.id], |r| r.get(0))?.collect::<Result<_, _>>()?
            };
            match shared.get(&own.name.to_lowercase()) {
                Some(&to) => {
                    tx.execute("UPDATE clients SET stage = ?2 WHERE stage = ?1", params![own.id, to])?;
                }
                None => {
                    for client in clients {
                        place(&tx, client, None, None, now)?;
                    }
                }
            }
            tx.execute("DELETE FROM stages WHERE id = ?1", [own.id])?;
        }
        tx.commit()?;
        Ok(())
    }

    // Books.

    /// Keeps a book out of Clients, or brings it back. See
    /// [`Snapshot::hidden_books`].
    pub fn set_book_hidden(&self, book: &str, hidden: bool) -> Result<()> {
        let book = normalize_book(book);
        let conn = self.lock();
        match hidden {
            true => conn.execute("INSERT OR IGNORE INTO hidden_books (book) VALUES (?1)", [&book])?,
            false => conn.execute("DELETE FROM hidden_books WHERE book = ?1", [&book])?,
        };
        Ok(())
    }

    // Importing.

    /// Brings contacts in, matching each to the client it already is.
    ///
    /// Matched on the source's own id for the contact first, then on the
    /// address: a signup who changed their email is still the signup. A
    /// match updates the name and company only where the book has none —
    /// what you typed beats what a file says — and every field the row
    /// carries.
    pub fn import(
        &self,
        rows: &[Contact],
        book: Option<&str>,
        source: Option<i64>,
        tags: &[String],
        now: i64,
    ) -> Result<Imported> {
        let conn = self.lock();
        let tx = conn.unchecked_transaction()?;
        let mut report = Imported::default();
        let book = book.filter(|b| !b.is_empty()).map(normalize_book);
        let source_name: Option<String> = match source {
            Some(id) => tx.query_row("SELECT name FROM sources WHERE id = ?1", [id], |r| r.get(0)).optional()?,
            None => None,
        };

        for row in rows {
            let addr = address::normalize(&row.email);
            if !addr.contains('@') {
                report.skipped += 1;
                continue;
            }

            let by_source: Option<i64> = match (source, row.external_id.trim()) {
                (Some(source), external) if !external.is_empty() => tx
                    .query_row(
                        "SELECT client FROM source_items WHERE source = ?1 AND external = ?2",
                        params![source, external],
                        |r| r.get(0),
                    )
                    .optional()?,
                _ => None,
            };
            let by_addr: Option<i64> = tx
                .query_row("SELECT client FROM identities WHERE kind = 'addr' AND value = ?1", [&addr], |r| r.get(0))
                .optional()?;

            let client = match by_source.or(by_addr) {
                Some(client) => {
                    tx.execute(
                        "UPDATE clients SET
                             name = CASE WHEN name = '' THEN ?2 ELSE name END,
                             company = CASE WHEN company = '' THEN ?3 ELSE company END,
                             updated = ?4
                         WHERE id = ?1",
                        params![client, row.name.trim(), row.company.trim(), now],
                    )?;
                    if by_addr.is_none() {
                        // Known to the source under a new address: the new one
                        // is theirs too.
                        add_identity(&tx, client, "addr", &addr)?;
                    }
                    report.updated += 1;
                    client
                }
                None => {
                    let client = insert_client(&tx, Kind::Person, row.name.trim(), row.company.trim(), now)?;
                    add_identity(&tx, client, "addr", &addr)?;
                    let detail = source_name.clone().unwrap_or_else(|| "Imported".into());
                    log_event(&tx, client, now, "imported", &detail)?;
                    report.added += 1;
                    client
                }
            };

            // The other addresses the row says are the same person, unless
            // they are already somebody else's: an import does not get to
            // take an address away from a client you made.
            for other in &row.other_emails {
                let other = address::normalize(other);
                if other.contains('@') {
                    add_identity(&tx, client, "addr", &other)?;
                }
            }

            if let (Some(source), external) = (source, row.external_id.trim())
                && !external.is_empty()
            {
                tx.execute(
                    "INSERT OR REPLACE INTO source_items (source, external, client) VALUES (?1, ?2, ?3)",
                    params![source, external, client],
                )?;
            }
            if let Some(book) = &book {
                tx.execute("INSERT OR IGNORE INTO client_books (client, book) VALUES (?1, ?2)", params![client, book])?;
            }
            for tag in tags.iter().chain(&row.tags) {
                let tag = tag.trim().trim_start_matches('#').trim();
                if tag.is_empty() {
                    continue;
                }
                let tag = ensure_tag(&tx, tag)?;
                tx.execute("INSERT OR IGNORE INTO client_tags (client, tag) VALUES (?1, ?2)", params![client, tag])?;
            }
            for (key, value) in &row.fields {
                if key.trim().is_empty() || value.trim().is_empty() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO fields (client, key, value) VALUES (?1, ?2, ?3)
                     ON CONFLICT(client, key) DO UPDATE SET value = excluded.value",
                    params![client, key.trim(), value.trim()],
                )?;
            }
        }
        tx.commit()?;
        Ok(report)
    }

    // Sources.

    pub fn sources(&self) -> Result<Vec<Source>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, book, url, format, tags, every, enabled, last_run, last_success, last_note
             FROM sources ORDER BY name COLLATE NOCASE",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Source {
                id: r.get(0)?,
                name: r.get(1)?,
                book: r.get(2)?,
                url: r.get(3)?,
                format: SourceFormat::from_stored(&r.get::<_, String>(4)?),
                tags: split_tags(&r.get::<_, String>(5)?),
                every: r.get(6)?,
                enabled: r.get(7)?,
                last_run: r.get(8)?,
                last_success: r.get(9)?,
                last_note: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Saves a source; an id of zero makes a new one. Returns its id.
    pub fn save_source(&self, source: &Source) -> Result<i64> {
        if !source.url.trim().starts_with("https://") {
            bail!("a source has to be an https:// address");
        }
        let conn = self.lock();
        let tags = source.tags.join(", ");
        let book = normalize_book(&source.book);
        match source.id {
            0 => Ok(conn.query_row(
                "INSERT INTO sources (name, book, url, format, tags, every, enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
                params![source.name.trim(), book, source.url.trim(), source.format.as_str(), tags, source.every, source.enabled],
                |r| r.get(0),
            )?),
            id => {
                conn.execute(
                    "UPDATE sources SET name = ?2, book = ?3, url = ?4, format = ?5, tags = ?6, every = ?7, enabled = ?8
                     WHERE id = ?1",
                    params![id, source.name.trim(), book, source.url.trim(), source.format.as_str(), tags, source.every, source.enabled],
                )?;
                Ok(id)
            }
        }
    }

    pub fn delete_source(&self, source: i64) -> Result<()> {
        self.lock().execute("DELETE FROM sources WHERE id = ?1", [source])?;
        Ok(())
    }

    /// Writes down how a pull went. `started` is when it began, which is
    /// what the next pull's `{since}` will be when this one worked.
    pub fn record_pull(&self, source: i64, started: i64, worked: bool, note: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE sources SET last_run = ?2, last_note = ?3,
                 last_success = CASE WHEN ?4 THEN ?2 ELSE last_success END
             WHERE id = ?1",
            params![source, started, note, worked],
        )?;
        Ok(())
    }
}

/// Every stage, each book's in order, the shared ones first.
fn read_stages(conn: &Connection) -> Result<Vec<Stage>> {
    let mut stmt = conn.prepare("SELECT id, name, position, kind, book FROM stages ORDER BY book, position, id")?;
    let rows = stmt.query_map([], |r| {
        Ok(Stage {
            id: r.get(0)?,
            name: r.get(1)?,
            position: r.get(2)?,
            kind: StageKind::from_stored(&r.get::<_, String>(3)?),
            book: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// One book's stages, in order: its own, or the shared ones while it has none.
fn stages_of(conn: &Connection, book: &str) -> Result<Vec<Stage>> {
    let all = read_stages(conn)?;
    let own = !book.is_empty() && all.iter().any(|s| s.book == book);
    let key = if own { book } else { "" };
    Ok(all.into_iter().filter(|s| s.book == key).collect())
}

fn read_note(r: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: r.get(0)?,
        client: r.get(1)?,
        body: r.get(2)?,
        pinned: r.get(3)?,
        created: r.get(4)?,
        updated: r.get(5)?,
    })
}

/// The client for an address, made if there is none. See
/// [`Book::ensure_client`].
fn ensure_person(tx: &Connection, addr: &str, name: &str, book: Option<&str>, now: i64) -> Result<i64> {
    let addr = address::normalize(addr);
    if !addr.contains('@') {
        bail!("`{addr}` is not an address");
    }
    let id = match tx
        .query_row("SELECT client FROM identities WHERE kind = 'addr' AND value = ?1", [&addr], |r| r.get(0))
        .optional()?
    {
        Some(id) => id,
        None => {
            let id = insert_client(tx, Kind::Person, name, "", now)?;
            add_identity(tx, id, "addr", &addr)?;
            log_event(tx, id, now, "created", "")?;
            id
        }
    };
    if let Some(book) = book.filter(|b| !b.is_empty()) {
        tx.execute("INSERT OR IGNORE INTO client_books (client, book) VALUES (?1, ?2)", params![id, normalize_book(book)])?;
    }
    tx.execute("DELETE FROM hidden WHERE addr = ?1", [&addr])?;
    Ok(id)
}

/// Moves one client, inside a caller's transaction. See [`Book::set_stage`].
fn place(tx: &Connection, client: i64, stage: Option<i64>, pos: Option<f64>, now: i64) -> Result<Placement> {
    let before: Placement = tx.query_row(
        "SELECT stage, stage_pos, stage_since FROM clients WHERE id = ?1",
        [client],
        |r| Ok(Placement { stage: r.get(0)?, pos: r.get(1)?, since: r.get(2)? }),
    )?;

    let pos = match (pos, stage) {
        (Some(pos), _) => pos,
        // At the top of the column it lands in, which is where the eye
        // goes to see that it arrived.
        (None, Some(stage)) => {
            let top: Option<f64> = tx.query_row(
                "SELECT MIN(stage_pos) FROM clients WHERE stage = ?1 AND id != ?2",
                params![stage, client],
                |r| r.get(0),
            )?;
            top.map(|top| top - 1.).unwrap_or(0.)
        }
        (None, None) => 0.,
    };
    let since = match before.stage == stage {
        true => before.since,
        false => stage.map(|_| now),
    };
    tx.execute(
        "UPDATE clients SET stage = ?2, stage_pos = ?3, stage_since = ?4 WHERE id = ?1",
        params![client, stage, pos, since],
    )?;

    if before.stage != stage {
        let detail = match (stage_name(tx, before.stage)?, stage_name(tx, stage)?) {
            (from, to) if from.is_empty() => format!("Put on the board in {to}"),
            (from, to) if to.is_empty() => format!("Taken off the board from {from}"),
            (from, to) => format!("{from} \u{2192} {to}"),
        };
        log_event(tx, client, now, "stage", &detail)?;
    }
    touch(tx, client, now)?;
    Ok(before)
}

fn stage_name(conn: &Connection, stage: Option<i64>) -> Result<String> {
    Ok(match stage {
        Some(id) => conn.query_row("SELECT name FROM stages WHERE id = ?1", [id], |r| r.get(0)).optional()?.unwrap_or_default(),
        None => String::new(),
    })
}

/// Undoes one [`place`], inside a caller's transaction.
fn put_back(tx: &Connection, client: i64, placement: Placement, now: i64) -> Result<()> {
    let current: Option<i64> = tx.query_row("SELECT stage FROM clients WHERE id = ?1", [client], |r| r.get(0))?;
    tx.execute(
        "UPDATE clients SET stage = ?2, stage_pos = ?3, stage_since = ?4 WHERE id = ?1",
        params![client, placement.stage, placement.pos, placement.since],
    )?;
    // The line the change wrote: undoing a move that then leaves "Lead →
    // Proposal" in the history is a history of something that did not
    // happen. Only when the stage itself is changing back — a card
    // shuffled up its own column wrote no line, and the newest one then
    // belongs to a move that is not being undone.
    if current != placement.stage {
        tx.execute(
            "DELETE FROM events WHERE id = (
                 SELECT id FROM events WHERE client = ?1 AND kind = 'stage'
                 ORDER BY id DESC LIMIT 1)",
            [client],
        )?;
    }
    touch(tx, client, now)?;
    Ok(())
}

fn insert_client(conn: &Connection, kind: Kind, name: &str, company: &str, now: i64) -> Result<i64> {
    Ok(conn.query_row(
        "INSERT INTO clients (kind, name, company, created, updated) VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
        params![kind.as_str(), name.trim(), company.trim(), now],
        |r| r.get(0),
    )?)
}

fn add_identity(conn: &Connection, client: i64, kind: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO identities (client, kind, value) VALUES (?1, ?2, ?3)",
        params![client, kind, value],
    )?;
    Ok(())
}

fn ensure_tag(conn: &Connection, name: &str) -> Result<i64> {
    if let Some(id) = conn
        .query_row("SELECT id FROM tags WHERE name = ?1", [name], |r| r.get(0))
        .optional()?
    {
        return Ok(id);
    }
    // A colour per tag, in the order they were made, so the first eight
    // tags anybody makes are eight different colours.
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM tags", [], |r| r.get(0))?;
    Ok(conn.query_row(
        "INSERT INTO tags (name, color) VALUES (?1, ?2) RETURNING id",
        params![name, count % 8],
        |r| r.get(0),
    )?)
}

fn log_event(conn: &Connection, client: i64, ts: i64, kind: &str, detail: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO events (client, ts, kind, detail) VALUES (?1, ?2, ?3, ?4)",
        params![client, ts, kind, detail],
    )?;
    Ok(())
}

fn touch(conn: &Connection, client: i64, now: i64) -> Result<()> {
    conn.execute("UPDATE clients SET updated = ?2 WHERE id = ?1", params![client, now])?;
    Ok(())
}

fn client_for(conn: &Connection, addr: &str) -> Result<Option<i64>> {
    if let Some(id) = conn
        .prepare_cached("SELECT client FROM identities WHERE kind = 'addr' AND value = ?1")?
        .query_row([addr], |r| r.get(0))
        .optional()?
    {
        return Ok(Some(id));
    }
    let Some((_, domain)) = addr.rsplit_once('@') else { return Ok(None) };
    Ok(conn
        .prepare_cached("SELECT client FROM identities WHERE kind = 'domain' AND value = ?1")?
        .query_row([domain], |r| r.get(0))
        .optional()?)
}

/// A book is named by its account's address, compared as addresses are.
fn normalize_book(book: &str) -> String {
    address::normalize(book)
}

fn split_tags(tags: &str) -> Vec<String> {
    tags.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_760_000_000;

    fn client(snapshot: &Snapshot, id: i64) -> &Client {
        snapshot.clients.iter().find(|c| c.id == id).expect("the client exists")
    }

    #[test]
    fn a_new_book_starts_with_the_usual_sale() {
        let book = Book::in_memory().unwrap();
        let stages = book.stages().unwrap();
        let names: Vec<&str> = stages.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Lead", "Qualified", "Proposal", "Negotiation", "Won", "Lost"]);
        assert_eq!(stages[4].kind, StageKind::Won);
        assert!(stages[5].kind.is_closed());
    }

    #[test]
    fn touching_a_correspondent_makes_one_client_however_often() {
        let book = Book::in_memory().unwrap();
        let first = book.ensure_client("Ana@Acme.com", "Ana Ruiz", Some("wes@example.com"), NOW).unwrap();
        let again = book.ensure_client("ana@acme.com", "", None, NOW).unwrap();
        assert_eq!(first, again);

        let snapshot = book.snapshot().unwrap();
        assert_eq!(snapshot.clients.len(), 1);
        let ana = client(&snapshot, first);
        assert_eq!(ana.addrs, ["ana@acme.com"]);
        assert_eq!(ana.books, ["wes@example.com"]);
        assert_eq!(ana.stage, None, "a client starts off the board");
    }

    #[test]
    fn a_stage_puts_a_client_on_the_board_and_says_so_in_its_history() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let stages = book.stages().unwrap();

        let before = book.set_stage(ana, Some(stages[0].id), None, NOW).unwrap();
        assert_eq!(before.stage, None);
        book.set_stage(ana, Some(stages[2].id), None, NOW + 60).unwrap();

        let snapshot = book.snapshot().unwrap();
        assert_eq!(client(&snapshot, ana).stage, Some(stages[2].id));
        assert_eq!(client(&snapshot, ana).stage_since, Some(NOW + 60), "the clock restarts on a new stage");

        let history: Vec<String> = book.detail(ana).unwrap().events.iter().map(|e| e.detail.clone()).collect();
        assert_eq!(history[0], "Lead \u{2192} Proposal");
        assert_eq!(history[1], "Put on the board in Lead");
    }

    #[test]
    fn undoing_a_move_leaves_no_trace_of_it() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let lead = book.stages().unwrap()[0].id;
        book.set_stage(ana, Some(lead), None, NOW).unwrap();
        let events = book.detail(ana).unwrap().events.len();

        let before = book.set_stage(ana, None, None, NOW + 5).unwrap();
        book.restore_placement(ana, before, NOW + 6).unwrap();

        let snapshot = book.snapshot().unwrap();
        assert_eq!(client(&snapshot, ana).stage, Some(lead));
        assert_eq!(client(&snapshot, ana).stage_since, Some(NOW));
        assert_eq!(book.detail(ana).unwrap().events.len(), events);
    }

    #[test]
    fn undoing_a_shuffle_within_a_column_leaves_the_history_alone() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let lead = book.stages().unwrap()[0].id;
        book.set_stage(ana, Some(lead), None, NOW).unwrap();
        let events = book.detail(ana).unwrap().events.len();

        // Dragged further down the same column: no line written, so undoing
        // it must not take away the line the real move wrote.
        let before = book.set_stage(ana, Some(lead), Some(5.), NOW + 5).unwrap();
        book.restore_placement(ana, before, NOW + 6).unwrap();
        assert_eq!(book.detail(ana).unwrap().events.len(), events);
    }

    #[test]
    fn deleting_a_stage_takes_its_clients_off_the_board_and_keeps_them() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let lead = book.stages().unwrap()[0].id;
        book.set_stage(ana, Some(lead), None, NOW).unwrap();
        book.delete_stage(lead).unwrap();

        let snapshot = book.snapshot().unwrap();
        assert_eq!(client(&snapshot, ana).stage, None);
    }

    #[test]
    fn a_new_stage_goes_where_the_sale_is_still_going() {
        let book = Book::in_memory().unwrap();
        book.add_stage("", "Demo booked").unwrap();
        let names: Vec<String> = book.stages().unwrap().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["Lead", "Qualified", "Proposal", "Negotiation", "Demo booked", "Won", "Lost"]);
    }

    #[test]
    fn notes_and_tags_round_trip_and_an_unused_tag_goes() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let note = book.add_note(ana, "  Wants the launch before Q1  ", NOW).unwrap();
        book.pin_note(note, true).unwrap();
        let tag = book.tag(ana, "#retainer", NOW).unwrap();
        assert_eq!(book.tag(ana, "Retainer", NOW).unwrap(), tag, "tags are matched without case");

        let detail = book.detail(ana).unwrap();
        assert_eq!(detail.notes[0].body, "Wants the launch before Q1");
        assert!(detail.notes[0].pinned);
        assert_eq!(book.snapshot().unwrap().tags[0].name, "retainer");

        let deleted = book.delete_note(note).unwrap().unwrap();
        assert!(book.detail(ana).unwrap().notes.is_empty());
        book.restore_note(&deleted).unwrap();
        assert_eq!(book.detail(ana).unwrap().notes, vec![deleted]);

        book.untag(ana, tag, NOW).unwrap();
        assert!(book.snapshot().unwrap().tags.is_empty());
    }

    #[test]
    fn an_import_matches_people_already_in_the_book_and_keeps_what_you_typed() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana (typed)", None, NOW).unwrap();

        let rows = vec![
            Contact {
                email: "ANA@acme.com".into(),
                name: "Ana Ruiz".into(),
                fields: vec![("plan".into(), "pro".into())],
                ..Default::default()
            },
            Contact { email: "new@gallery.example".into(), name: "New Signup".into(), ..Default::default() },
            Contact { email: "not an address".into(), ..Default::default() },
        ];
        let report = book
            .import(&rows, Some("wes@camp.example"), None, &["signup".into()], NOW)
            .unwrap();
        assert_eq!(report, Imported { added: 1, updated: 1, skipped: 1 });

        let snapshot = book.snapshot().unwrap();
        assert_eq!(snapshot.clients.len(), 2);
        let kept = client(&snapshot, ana);
        assert_eq!(kept.name, "Ana (typed)", "an import does not overwrite a name you gave");
        assert_eq!(kept.books, ["wes@camp.example"]);
        assert_eq!(book.detail(ana).unwrap().fields, [("plan".to_string(), "pro".to_string())]);
        assert!(snapshot.clients.iter().all(|c| c.tags.len() == 1), "the import's tag is on everyone it brought");
    }

    #[test]
    fn a_source_recognises_its_own_contact_under_a_new_address() {
        let book = Book::in_memory().unwrap();
        let source = book
            .save_source(&Source {
                name: "Website signups".into(),
                url: "https://camp.example/api/signups?since={since}".into(),
                every: 3600,
                enabled: true,
                ..Default::default()
            })
            .unwrap();

        let first = Contact { email: "old@x.example".into(), external_id: "u-7".into(), ..Default::default() };
        book.import(&[first], None, Some(source), &[], NOW).unwrap();
        let moved = Contact { email: "new@x.example".into(), external_id: "u-7".into(), ..Default::default() };
        let report = book.import(&[moved], None, Some(source), &[], NOW + 1).unwrap();
        assert_eq!(report.updated, 1);

        let snapshot = book.snapshot().unwrap();
        assert_eq!(snapshot.clients.len(), 1);
        assert!(snapshot.clients[0].addrs.contains(&"new@x.example".to_string()));
        assert_eq!(book.detail(snapshot.clients[0].id).unwrap().events[0].detail, "Website signups");
    }

    #[test]
    fn a_source_is_due_on_its_schedule_and_never_when_off() {
        let mut source = Source { every: 3600, enabled: true, ..Default::default() };
        assert!(source.due(NOW), "never pulled is due");
        source.last_run = Some(NOW - 60);
        assert!(!source.due(NOW));
        source.last_run = Some(NOW - 3600);
        assert!(source.due(NOW));
        source.enabled = false;
        assert!(!source.due(NOW));
        source = Source { every: 0, enabled: true, ..Default::default() };
        assert!(!source.due(NOW), "by hand only");
    }

    #[test]
    fn a_source_must_be_https() {
        let book = Book::in_memory().unwrap();
        let plain = Source { url: "http://example.com/users".into(), ..Default::default() };
        assert!(book.save_source(&plain).is_err());
    }

    #[test]
    fn merging_moves_everything_and_keeps_the_stage_you_kept() {
        let book = Book::in_memory().unwrap();
        let stages = book.stages().unwrap();
        let work = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let home = book.ensure_client("ana@home.example", "Ana R", None, NOW).unwrap();
        book.add_note(home, "met at the show", NOW).unwrap();
        book.tag(home, "vip", NOW).unwrap();
        book.set_stage(work, Some(stages[1].id), None, NOW).unwrap();
        book.set_stage(home, Some(stages[3].id), None, NOW).unwrap();

        book.merge(work, home, NOW).unwrap();

        let snapshot = book.snapshot().unwrap();
        assert_eq!(snapshot.clients.len(), 1);
        let ana = client(&snapshot, work);
        assert_eq!(ana.primary_addr(), Some("ana@acme.com"));
        assert_eq!(ana.addrs.len(), 2);
        assert_eq!(ana.tags.len(), 1);
        assert_eq!(ana.stage, Some(stages[1].id));
        assert_eq!(book.detail(work).unwrap().notes.len(), 1);
    }

    #[test]
    fn an_address_belongs_to_one_client_and_a_domain_finds_its_company() {
        let book = Book::in_memory().unwrap();
        let acme = book.ensure_company("acme.com", "Acme", NOW).unwrap();
        assert_eq!(book.client_for("bo@acme.com").unwrap(), Some(acme), "anyone at the domain");

        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        assert_eq!(book.client_for("ana@acme.com").unwrap(), Some(ana), "an address beats its domain");

        let other = book.create(Kind::Person, "Someone", None, None, NOW).unwrap();
        book.add_identity(other, "ana@acme.com", NOW).unwrap();
        assert_eq!(book.client_for("ana@acme.com").unwrap(), Some(other), "the address moved");
    }

    #[test]
    fn the_book_lives_beside_the_store_it_belongs_to() {
        assert_eq!(Book::path_beside(Path::new("/x/spool/mail.db")), Path::new("/x/spool/clients.db"));
        assert_eq!(Book::path_beside(Path::new("/tmp/spool-demo.db")), Path::new("/tmp/spool-demo.clients.db"));
    }

    fn names_of(book: &Book, of: &str) -> Vec<String> {
        book.stages().unwrap().into_iter().filter(|s| s.book == of).map(|s| s.name).collect()
    }

    #[test]
    fn several_clients_move_at_once_and_come_back_at_once() {
        let book = Book::in_memory().unwrap();
        let people: Vec<(String, String, Option<String>)> = ["ana@acme.com", "bo@acme.com", "cy@acme.com"]
            .iter()
            .map(|a| (a.to_string(), String::new(), Some("wes@example.com".to_string())))
            .collect();
        let ids = book.ensure_clients(&people, NOW).unwrap();
        assert_eq!(book.ensure_clients(&people[..1], NOW).unwrap(), ids[..1], "made once");
        let [lead, qualified] = [0, 1].map(|i| book.stages().unwrap()[i].id);
        book.set_stage(ids[0], Some(lead), None, NOW).unwrap();

        let before = book.set_stages(&ids, Some(qualified), NOW + 10).unwrap();
        let snapshot = book.snapshot().unwrap();
        assert!(ids.iter().all(|&id| client(&snapshot, id).stage == Some(qualified)));
        // In the order chosen, at the top of the column.
        let pos: Vec<f64> = ids.iter().map(|&id| client(&snapshot, id).stage_pos).collect();
        assert!(pos[0] < pos[1] && pos[1] < pos[2], "{pos:?}");
        assert_eq!(book.detail(ids[0]).unwrap().events[0].detail, "Lead \u{2192} Qualified");

        book.restore_placements(&before, NOW + 20).unwrap();
        let snapshot = book.snapshot().unwrap();
        assert_eq!(client(&snapshot, ids[0]).stage, Some(lead));
        assert_eq!(client(&snapshot, ids[1]).stage, None);
        assert_eq!(book.detail(ids[1]).unwrap().events.len(), 1, "only that it was made");
    }

    #[test]
    fn a_book_given_its_own_stages_keeps_its_clients_where_they_were() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let bo = book.ensure_client("bo@other.com", "Bo", None, NOW).unwrap();
        let proposal = book.stages().unwrap()[2].id;
        book.set_stage(ana, Some(proposal), None, NOW).unwrap();
        book.set_stage(bo, Some(proposal), None, NOW).unwrap();

        // Ana is on that book's board; Bo is not.
        book.own_stages("Hello@Launch.example", &[ana]).unwrap();
        let own = book.stages().unwrap().into_iter().filter(|s| s.book == "hello@launch.example").collect::<Vec<_>>();
        assert_eq!(own.len(), 6);
        let snapshot = book.snapshot().unwrap();
        assert_eq!(client(&snapshot, ana).stage, Some(own[2].id), "to the copy of Proposal");
        assert_eq!(client(&snapshot, ana).stage_since, Some(NOW), "and still there since then");
        assert_eq!(client(&snapshot, bo).stage, Some(proposal));
        assert_eq!(book.detail(ana).unwrap().events.len(), 2, "nothing happened to Ana");

        // Changing it changes nobody else's.
        book.add_stage("hello@launch.example", "Trial").unwrap();
        book.move_stage(own[0].id, true).unwrap();
        book.rename_stage(own[1].id, "Activated").unwrap();
        assert_eq!(names_of(&book, "hello@launch.example"), ["Activated", "Lead", "Proposal", "Negotiation", "Trial", "Won", "Lost"]);
        assert_eq!(names_of(&book, ""), ["Lead", "Qualified", "Proposal", "Negotiation", "Won", "Lost"]);
        // A book still on the shared stages adds to those.
        book.add_stage("wes@studio.example", "Demo").unwrap();
        assert!(names_of(&book, "").contains(&"Demo".to_string()));
        assert!(names_of(&book, "wes@studio.example").is_empty());
    }

    #[test]
    fn going_back_to_the_shared_stages_keeps_whoever_has_a_stage_of_that_name() {
        let book = Book::in_memory().unwrap();
        let ana = book.ensure_client("ana@acme.com", "Ana", None, NOW).unwrap();
        let bo = book.ensure_client("bo@acme.com", "Bo", None, NOW).unwrap();
        book.own_stages("hello@launch.example", &[]).unwrap();
        let trial = book.add_stage("hello@launch.example", "Trial").unwrap();
        let own_lead = book.stages().unwrap().into_iter().find(|s| s.book == "hello@launch.example" && s.name == "Lead").unwrap().id;
        book.set_stage(ana, Some(own_lead), None, NOW).unwrap();
        book.set_stage(bo, Some(trial), None, NOW).unwrap();

        book.share_stages("hello@launch.example", NOW + 5).unwrap();
        let snapshot = book.snapshot().unwrap();
        let shared_lead = snapshot.stages.iter().find(|s| s.book.is_empty() && s.name == "Lead").unwrap().id;
        assert_eq!(client(&snapshot, ana).stage, Some(shared_lead));
        assert_eq!(client(&snapshot, bo).stage, None, "no shared Trial");
        assert_eq!(book.detail(bo).unwrap().events[0].detail, "Taken off the board from Trial");
        assert!(snapshot.stages.iter().all(|s| s.book.is_empty()));
    }

    #[test]
    fn a_hidden_book_stays_hidden_until_shown() {
        let book = Book::in_memory().unwrap();
        book.set_book_hidden("Hobby@Example.com", true).unwrap();
        book.set_book_hidden("hobby@example.com", true).unwrap();
        assert_eq!(book.snapshot().unwrap().hidden_books, ["hobby@example.com"]);
        book.set_book_hidden("hobby@example.com", false).unwrap();
        assert!(book.snapshot().unwrap().hidden_books.is_empty());
    }

    #[test]
    fn a_book_from_before_stages_per_book_opens_with_its_stages_shared() {
        let dir = std::env::temp_dir().join(format!("spool-book-v1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clients.db");
        let _ = std::fs::remove_file(&path);
        {
            // What the first version of the book wrote.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE stages (id INTEGER PRIMARY KEY, name TEXT NOT NULL, position REAL NOT NULL, kind TEXT NOT NULL DEFAULT 'open');
                 INSERT INTO stages (name, position) VALUES ('Lead', 0);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let book = Book::open(&path).unwrap();
        let stages = book.stages().unwrap();
        assert_eq!(stages.len(), 1);
        assert_eq!(stages[0].book, "");
        // And the table for hidden books is there to write to.
        book.set_book_hidden("a@b.com", true).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
