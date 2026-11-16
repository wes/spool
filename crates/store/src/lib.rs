//! The local message store: a SQLite database that is the source of truth for
//! everything the UI shows.
//!
//! The design rule is that the UI thread never touches this. Every call here
//! runs on a background thread and hands back plain owned data, so a slow
//! query can stall a sync but can never drop a frame.
//!
//! Search is FTS5 over subject, sender and body. The index is contentless — it
//! stores postings, not a second copy of every message — which keeps a
//! six-figure mailbox's index small enough to stay in the page cache, and is
//! why queries come back in single-digit milliseconds.

pub mod book;
pub mod demo;
pub mod outbox;
pub mod people;
pub mod search;
pub mod suggest;

use search::Search;

use anyhow::{Context as _, Result};
use spool_core::{
    Account, AccountId, Address, Attachment, Body, Envelope, Flags, Group, InlineImage, Mailbox,
    MailboxId, MailboxRole, MessageId, ThreadId, address,
    draft::{Attached, Draft},
    html,
    thread::{ThreadKeys, Threader, normalize_subject},
    unsubscribe::ListUnsubscribe,
};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter, types::Value};
use std::{
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

/// Whether a draft save has to write the draft's files as well as its text.
///
/// A required argument rather than something inferred, because the cheap
/// answer and the correct answer differ by however large the attachments are,
/// and only the caller knows which applies. An autosave that fires while
/// somebody is typing has nothing to say about the files; attaching one has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftFiles {
    /// Write the row only. The stored files are already right.
    Unchanged,
    /// Write the row and replace the files.
    Replace,
}

/// A message on its way into the store, before threading has been resolved.
#[derive(Debug, Clone, Default)]
pub struct IncomingMessage {
    pub account_id: AccountId,
    pub mailbox_id: MailboxId,
    /// IMAP UID within the mailbox. `None` for locally created messages.
    pub uid: Option<u32>,
    pub rfc_message_id: String,
    pub in_reply_to: String,
    pub references: Vec<String>,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// `Reply-To`, when the sender asked for answers to go elsewhere.
    pub reply_to: Vec<Address>,
    pub subject: String,
    pub date: i64,
    pub plain: String,
    pub html: String,
    pub flags: Flags,
    pub attachments: Vec<Attachment>,
    pub inline_images: Vec<InlineImage>,
    /// Attachment payloads, positionally matching `attachments`.
    ///
    /// Empty where the part was too large to be worth keeping; the metadata
    /// still lands so the message can say what it has.
    pub attachment_bytes: Vec<Vec<u8>>,
    pub size: u32,
    /// Its `List-Unsubscribe` headers. Left [`ListUnsubscribe::Unknown`] by
    /// anything that did not read them, which stores "not looked" rather
    /// than a "none" nobody established.
    pub list_unsubscribe: ListUnsubscribe,
}

/// How the message list is filtered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Query {
    #[default]
    All,
    Unread,
    Flagged,
    /// Full-text search. Empty behaves as [`Query::All`].
    Text(String),
}

impl Query {
    pub fn is_search(&self) -> bool {
        matches!(self, Query::Text(t) if !t.trim().is_empty())
    }
}

/// Where a query applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Mailbox(MailboxId),
    Account(AccountId),
    /// Every message in every account — the unified view.
    Everything,
}

/// Where a message can be found again on its server.
///
/// Wanted by attachments too large to have been kept: the metadata says a
/// 15 MB PDF arrived, and this is how the bytes are found when somebody
/// finally asks for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageOrigin {
    pub email: String,
    pub mailbox: String,
    pub uid: u32,
    /// The whole message's size on the server, as it reported at sync.
    ///
    /// What a download of it is measured against. Zero when the server never
    /// said, in which case there is no denominator and no percentage.
    pub size: u32,
}

#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Inner>>,
    /// Where the database lives, so a [`ListReader`] can open its own
    /// connection to it. `None` for an in-memory store, which has no second
    /// connection to give.
    path: Option<Arc<Path>>,
}

/// How every connection reads: through a memory map, with a small cache.
///
/// The cache used to be 64MB per connection, and a launch fills both — the
/// list reads every row of the inbox, the sync every UID and flag — so a
/// quarter-million-message store held 130MB of SQLite's private copies of
/// pages the OS already had in its file cache. Mapped, a read is served from
/// that file cache directly: no copy, no `pread`, and nothing charged to the
/// app's footprint for pages that are only being read. The cache that is left
/// holds what the map does not cover — pages being written, and the WAL.
const READ_PRAGMAS: &str = "PRAGMA mmap_size = 1073741824; PRAGMA cache_size = -16384;";

/// A connection of its own, for the message list and nothing else.
///
/// The store's one connection is shared by everything: a sync writing a batch
/// of three hundred messages holds it, and a list query asked for in the
/// meantime waits for the batch. SQLite in WAL mode lets a reader run beside a
/// writer, so the list reads through this instead and never waits on a write.
///
/// And because nothing but the list ever runs on it, a list query can be
/// interrupted: when the search box has moved on, the query for what it said a
/// keystroke ago is stopped where it is rather than run to the end and thrown
/// away. Interrupting the shared connection would risk stopping a sync's write.
pub struct ListReader {
    conn: Mutex<Connection>,
    interrupt: rusqlite::InterruptHandle,
}

impl ListReader {
    /// As [`Store::envelopes`], on this connection.
    pub fn envelopes(&self, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        query_envelopes(&conn, scope, query, None, limit)
    }

    /// As [`Store::conversations`], on this connection.
    pub fn conversations(&self, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
        let depth = limit.saturating_mul(8).max(limit);
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        query_conversations(&conn, scope, query, None, depth, limit)
    }

    /// The first `limit` rows of [`ListReader::conversations`], exactly as the
    /// whole list would show them, read without reading the whole list.
    pub fn conversations_page(&self, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conversations_page(&conn, scope, query, limit)
    }

    /// Stops whatever list query is running, which then returns an error.
    /// Nothing running, nothing happens.
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }
}

struct Inner {
    conn: Connection,
    /// Built the first time a message has to be threaded, not at open.
    ///
    /// Seeding it reads every row in `messages` — a hundred-odd megabytes on a
    /// large mailbox, and on a cold page cache seconds of it. None of that is
    /// needed to draw a window: threading is what [`Store::ingest`] does with
    /// mail as it arrives, which happens on a background thread several
    /// network round-trips after launch. Doing it in `open` put the whole scan
    /// in front of the first frame, where it was the difference between a
    /// mail client that is up before the dock icon finishes bouncing and one
    /// that is still reading disk three bounces later.
    ///
    /// A launch that syncs nothing now never pays for it at all.
    threader: Option<Threader>,
    /// Whether mail is added to the people index as it arrives. Off until
    /// Clients is switched on; see [`Store::set_people_index`].
    people: bool,
}

/// A message that [`Store::ingest`] had not seen before.
///
/// Both halves earn their place. The position points back into the batch the
/// caller still holds, which is what a summary of what arrived is written
/// from. The id is what the message is called from here on — the thing a rule
/// that wants to flag it, an extension that wants to score it, or a
/// notification that wants to open it has to be given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ingested {
    /// Where it was in the slice handed to `ingest`.
    pub position: usize,
    /// The row it went into.
    pub id: MessageId,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening the mail store at {}", path.display()))?;
        let mut store = Self::from_connection(conn)?;
        store.path = Some(Arc::from(path));
        Ok(store)
    }

    /// A second, read-only connection for the message list. See [`ListReader`].
    ///
    /// `None` for an in-memory store: its database exists only inside the
    /// first connection, and the caller reads through the store instead.
    pub fn list_reader(&self) -> Result<Option<ListReader>> {
        let Some(conn) = self.read_connection()? else { return Ok(None) };
        let interrupt = conn.get_interrupt_handle();
        Ok(Some(ListReader { conn: Mutex::new(conn), interrupt }))
    }

    /// A read-only connection of its own to the same database.
    fn read_connection(&self) -> Result<Option<Connection>> {
        let Some(path) = self.path.as_ref() else { return Ok(None) };
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening a reader on {}", path.display()))?;
        conn.execute_batch(&format!("PRAGMA temp_store = MEMORY; {READ_PRAGMAS}"))?;
        Ok(Some(conn))
    }

    /// Where the database is, for a file that belongs beside it.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // WAL lets the sync engine write while the UI reads. `normal`
        // synchronous is the right trade for a cache of data that also lives
        // on a server: a crash can cost the last transaction, never the mail.
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA temp_store = MEMORY;
             {READ_PRAGMAS}",
        ))?;

        let mut store = Inner { conn, threader: None, people: false };
        store.migrate()?;

        Ok(Self { inner: Arc::new(Mutex::new(store)), path: None })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A poisoned lock means a previous caller panicked mid-query. The
        // database itself is still consistent (SQLite rolls back), so recover
        // rather than cascading the panic into the UI.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn add_account(&self, name: &str, email: &str, color: u8) -> Result<AccountId> {
        let this = self.lock();
        // The colour is updated as well as the name: the config file is where
        // both are decided, and a colour that only ever applied to accounts
        // that did not exist yet was a colour nobody could change.
        this.conn.execute(
            "INSERT INTO accounts (name, email, color) VALUES (?1, ?2, ?3)
             ON CONFLICT(email) DO UPDATE SET name = excluded.name, color = excluded.color",
            params![name, email, color],
        )?;
        let id = this
            .conn
            .query_row("SELECT id FROM accounts WHERE email = ?1", [email], |r| r.get(0))?;
        Ok(AccountId(id))
    }

    /// Recolours an account that is already here.
    ///
    /// Saving the settings should recolour the sidebar now, not at the next
    /// sync — which for an account that is merely configured and not yet
    /// reachable may be a long way off.
    pub fn set_account_color(&self, email: &str, color: u8) -> Result<()> {
        let this = self.lock();
        this.conn.execute(
            "UPDATE accounts SET color = ?2 WHERE email = ?1",
            params![email, color],
        )?;
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let this = self.lock();
        let mut stmt = this
            .conn
            .prepare("SELECT id, name, email, color FROM accounts ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok(Account {
                id: AccountId(r.get(0)?),
                name: r.get(1)?,
                email: r.get(2)?,
                color: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn upsert_mailbox(
        &self,
        account_id: AccountId,
        path: &str,
        role: MailboxRole,
    ) -> Result<MailboxId> {
        let name = path.rsplit(['/', '.']).next().unwrap_or(path);
        let this = self.lock();
        this.conn.execute(
            "INSERT INTO mailboxes (account_id, path, name, role) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, path) DO UPDATE SET role = excluded.role",
            params![account_id.0, path, name, role.as_str()],
        )?;
        let id = this.conn.query_row(
            "SELECT id FROM mailboxes WHERE account_id = ?1 AND path = ?2",
            params![account_id.0, path],
            |r| r.get(0),
        )?;
        Ok(MailboxId(id))
    }

    /// Deletes an account and everything downloaded for it.
    ///
    /// Mailboxes and messages cascade from the account row, but the full-text
    /// index is contentless and has no foreign keys, so its postings have to
    /// be removed by hand — otherwise a removed account's mail keeps turning
    /// up in search results with nothing behind it.
    pub fn delete_account(&self, id: AccountId) -> Result<usize> {
        let this = self.lock();
        let tx = this.conn.unchecked_transaction()?;

        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM messages WHERE account_id = ?1")?;
            stmt.query_map([id.0], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?
        };

        for message in &ids {
            tx.execute(
                "INSERT INTO messages_fts (messages_fts, rowid) VALUES ('delete', ?1)",
                [message],
            )
            .ok();
        }

        tx.execute("DELETE FROM messages WHERE account_id = ?1", [id.0])?;
        tx.execute("DELETE FROM mailboxes WHERE account_id = ?1", [id.0])?;
        tx.execute("DELETE FROM accounts WHERE id = ?1", [id.0])?;
        tx.commit()?;
        Ok(ids.len())
    }

    /// Renames a mailbox, and every mailbox nested inside it.
    ///
    /// Servers move the children with a `RENAME`, so the local copy has to as
    /// well — otherwise `Work/Invoices` is left pointing at a path the server
    /// no longer has, and the next sync creates a duplicate beside it.
    pub fn rename_mailbox(&self, id: MailboxId, path: &str, delimiter: &str) -> Result<usize> {
        let this = self.lock();
        let tx = this.conn.unchecked_transaction()?;

        let old: String =
            tx.query_row("SELECT path FROM mailboxes WHERE id = ?1", [id.0], |r| r.get(0))?;

        tx.execute(
            "UPDATE mailboxes SET path = ?2, name = ?3 WHERE id = ?1",
            params![id.0, path, leaf_name(path, delimiter)],
        )?;

        // Children carry the old path plus the delimiter as a prefix.
        let mut moved = 1;
        let prefix = format!("{old}{delimiter}");
        let mut stmt =
            tx.prepare("SELECT id, path FROM mailboxes WHERE path LIKE ?1 ESCAPE '\\'")?;
        let children: Vec<(i64, String)> = stmt
            .query_map([format!("{}%", escape_like(&prefix))], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        for (child, child_path) in children {
            let suffix = &child_path[prefix.len()..];
            let renamed = format!("{path}{delimiter}{suffix}");
            tx.execute(
                "UPDATE mailboxes SET path = ?2, name = ?3 WHERE id = ?1",
                params![child, renamed, leaf_name(&renamed, delimiter)],
            )?;
            moved += 1;
        }

        tx.commit()?;
        Ok(moved)
    }

    /// Deletes a mailbox and the messages it held.
    ///
    /// The rows would go anyway — `messages.mailbox_id` cascades — but the
    /// full-text index is contentless and has no foreign keys, so its postings
    /// have to be removed by hand or a deleted folder's mail keeps turning up
    /// in search results.
    pub fn delete_mailbox(&self, id: MailboxId) -> Result<usize> {
        let this = self.lock();
        let tx = this.conn.unchecked_transaction()?;

        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM messages WHERE mailbox_id = ?1")?;
            stmt.query_map([id.0], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?
        };

        for message in &ids {
            tx.execute(
                "INSERT INTO messages_fts (messages_fts, rowid) VALUES ('delete', ?1)",
                [message],
            )
            .ok();
        }

        tx.execute("DELETE FROM messages WHERE mailbox_id = ?1", [id.0])?;
        tx.execute("DELETE FROM mailboxes WHERE id = ?1", [id.0])?;
        tx.commit()?;
        Ok(ids.len())
    }

    /// Removes this account's local folders that the server no longer lists.
    ///
    /// Folders are keyed by path, and a rename or a move on the server is,
    /// from here, a new path: without this, the old row survives with the mail
    /// that was downloaded into it and the sidebar grows a second folder with
    /// the same leaf name. The server's list is the truth about what exists,
    /// so anything not in it goes, along with the mail it held — which is
    /// still on the server under the new path, and arrives again on the same
    /// sync that noticed.
    ///
    /// `keep` must be a *complete* listing for the account. A caller that
    /// filtered the server's folders has no business calling this: absence
    /// from a filtered list means "not asked for", not "not there".
    ///
    /// Returns the paths removed.
    pub fn retain_mailboxes(&self, account_id: AccountId, keep: &[String]) -> Result<Vec<String>> {
        let stale: Vec<(i64, String)> = {
            let this = self.lock();
            let mut stmt =
                this.conn.prepare("SELECT id, path FROM mailboxes WHERE account_id = ?1")?;
            let rows = stmt
                .query_map([account_id.0], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter().filter(|(_, path)| !keep.iter().any(|k| same_folder(k, path))).collect()
        };

        // The lock is released first: `delete_mailbox` takes it itself, and
        // each folder is its own transaction so one failure does not undo the
        // rest.
        for (id, _) in &stale {
            self.delete_mailbox(MailboxId(*id))?;
        }
        Ok(stale.into_iter().map(|(_, path)| path).collect())
    }

    /// All mailboxes with live counts, ordered for display.
    pub fn mailboxes(&self) -> Result<Vec<Mailbox>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare(
            "SELECT m.id, m.account_id, m.path, m.name, m.role,
                    COUNT(msg.id),
                    COALESCE(SUM(CASE WHEN msg.flags & ?1 = 0 THEN 1 ELSE 0 END), 0),
                    m.server_total
             FROM mailboxes m
             LEFT JOIN messages msg ON msg.mailbox_id = m.id
             GROUP BY m.id
             ORDER BY m.account_id, m.role, m.name",
        )?;
        let rows = stmt.query_map([Flags::SEEN], |r| {
            let role: String = r.get(4)?;
            Ok(Mailbox {
                id: MailboxId(r.get(0)?),
                account_id: AccountId(r.get(1)?),
                path: r.get(2)?,
                name: r.get(3)?,
                role: MailboxRole::from_stored(&role),
                total: r.get(5)?,
                unread: r.get(6)?,
                server_total: r.get(7)?,
            })
        })?;

        let mut mailboxes: Vec<Mailbox> = rows.collect::<Result<_, _>>()?;
        mailboxes.sort_by_key(|m| (m.account_id.0, m.role.sort_key(), m.name.clone()));
        Ok(mailboxes)
    }

    /// Ingests a batch of messages in one transaction.
    ///
    /// Batching matters: committing per message turns a 50,000-message initial
    /// sync into 50,000 fsyncs.
    /// Stores a batch, returning an [`Ingested`] for each message that was new.
    ///
    /// The position is there because a caller that wants to say "two messages
    /// arrived, one from Ana" needs the *messages*, and it already has them —
    /// this only has to say which. The id is there because anything that acts
    /// on what arrived needs to name it afterwards, and the row it went into
    /// is the only name the rest of the app knows it by. A count is `.len()`
    /// on the answer.
    pub fn ingest(&self, messages: &[IncomingMessage]) -> Result<Vec<Ingested>> {
        let mut this = self.lock();
        // Before the borrow is split, because seeding needs the connection
        // too. On the first batch of a run this is where the threading scan
        // happens; every batch after it finds the work already done.
        this.threader()?;
        let Inner { conn, threader, people } = &mut *this;
        let threader = threader.as_mut().expect("seeded just above");
        let tx = conn.unchecked_transaction()?;
        let mut inserted = Vec::new();
        // Who "you" are, for the people index: read once per batch rather than
        // once per message, and inside the transaction so it agrees with it.
        // Not at all while the index is off, which is what keeps a mailbox
        // that never opens Clients paying nothing for it.
        let me = match *people {
            true => Some(people::Me::read(&tx)?),
            false => None,
        };

        for (position, msg) in messages.iter().enumerate() {
            let refs: Vec<&str> = msg.references.iter().map(String::as_str).collect();
            let thread = threader.assign(ThreadKeys {
                message_id: Some(&msg.rfc_message_id),
                in_reply_to: (!msg.in_reply_to.is_empty()).then_some(&msg.in_reply_to),
                references: &refs,
                subject: &msg.subject,
            });

            let snippet = html::snippet(&msg.plain, &msg.html, 200);

            // A message this mailbox is holding but cannot name.
            //
            // That is what a local move leaves behind: the row is here with
            // the right contents, but its UID was cleared because the one it
            // had belonged to the folder it came from. The server has now
            // handed back the same message under its new UID, and inserting
            // it would put the message in the list twice — the same mail,
            // side by side, one of them unopenable.
            //
            // So the orphan adopts the UID instead. `LIMIT 1` because the
            // partial unique index on (mailbox_id, uid) means only one row can
            // take it, and a second matching orphan is a duplicate the next
            // sync gets to claim.
            //
            // Tried before the insert rather than on its conflict path: the
            // new UID does not collide with anything, so the insert would
            // succeed and the duplicate is exactly what succeeds.
            let adopted: Option<i64> = match (msg.uid, msg.rfc_message_id.is_empty()) {
                (Some(_), false) => tx
                    .query_row(
                        "UPDATE messages SET uid = ?2, flags = ?3
                         WHERE id = (
                             SELECT id FROM messages
                             WHERE mailbox_id = ?1
                               AND uid IS NULL
                               AND rfc_message_id = ?4
                             ORDER BY id LIMIT 1
                         )
                         RETURNING id",
                        params![
                            msg.mailbox_id.0,
                            msg.uid,
                            msg.flags.0,
                            msg.rfc_message_id
                        ],
                        |row| row.get(0),
                    )
                    .optional()?,
                _ => None,
            };

            // The same reunion, for a server that did not keep our
            // `Message-ID`.
            //
            // The case this exists for is a sent copy: the message was filed
            // locally under the id this client generated, and the server may
            // hand it back under one of its own. Matched instead on the three
            // things that cannot coincide by accident inside one folder —
            // the same second, the same subject, the same recipients — and
            // only against a row with no UID, which is only ever a copy this
            // client put there itself.
            let adopted = match (adopted, msg.uid) {
                (None, Some(_)) => tx
                    .query_row(
                        "UPDATE messages SET uid = ?2, flags = ?3
                         WHERE id = (
                             SELECT id FROM messages
                             WHERE mailbox_id = ?1
                               AND uid IS NULL
                               AND date_ts = ?4
                               AND norm_subject = ?5
                               AND to_addrs = ?6
                             ORDER BY id LIMIT 1
                         )
                         RETURNING id",
                        params![
                            msg.mailbox_id.0,
                            msg.uid,
                            msg.flags.0,
                            msg.date,
                            normalize_subject(&msg.subject),
                            address::encode_list(&msg.to),
                        ],
                        |row| row.get(0),
                    )
                    .optional()?,
                (existing, _) => existing,
            };

            // `DO NOTHING … RETURNING id`, and this is load-bearing.
            //
            // It used to be `DO UPDATE` followed by `last_insert_rowid()`. On
            // the update path nothing is inserted, so that function still
            // names whichever row was inserted *last* — a different message —
            // and the body, the attachments and the search entry below were
            // all written onto that one. That is how a message ends up
            // rendering somebody else's mail under its own subject.
            //
            // Answering "was this new" from `last_insert_rowid()` does not
            // work either: it is per connection, not per table, so a fresh
            // message can share its value with the last mailbox inserted. The
            // statement itself has to say, and this is it saying so: a row
            // back means inserted, nothing back means it was already here.
            let (list_unsub, list_unsub_post) = encode_list_unsubscribe(&msg.list_unsubscribe);

            let inserted_id: Option<i64> = match adopted {
                Some(_) => None,
                None => tx.query_row(
                "INSERT INTO messages (
                     account_id, mailbox_id, uid, rfc_message_id, thread_id, in_reply_to, refs,
                     from_name, from_addr, to_addrs, cc_addrs, reply_to, subject, norm_subject,
                     date_ts, snippet, flags, size, has_attachments, list_unsub, list_unsub_post
                 ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
                 -- The uniqueness of (mailbox_id, uid) is enforced by a partial
                 -- index, so the conflict target has to repeat its predicate.
                 ON CONFLICT(mailbox_id, uid) WHERE uid IS NOT NULL DO NOTHING
                 RETURNING id",
                params![
                    msg.account_id.0,
                    msg.mailbox_id.0,
                    msg.uid,
                    msg.rfc_message_id,
                    thread.0,
                    msg.in_reply_to,
                    msg.references.join(" "),
                    msg.from.name,
                    msg.from.addr,
                    address::encode_list(&msg.to),
                    address::encode_list(&msg.cc),
                    address::encode_list(&msg.reply_to),
                    msg.subject,
                    normalize_subject(&msg.subject),
                    msg.date,
                    snippet,
                    msg.flags.0,
                    msg.size,
                    !msg.attachments.is_empty(),
                    list_unsub,
                    list_unsub_post,
                ],
                    |row| row.get(0),
                )
                .optional()?,
            };


            let (id, is_new) = match (inserted_id, adopted) {
                (Some(id), _) => (id, true),
                (None, Some(id)) => (id, false),
                // Already here. A UID never changes what message it names, so
                // the only thing that can have moved is the flags — but the
                // body is rewritten below regardless, which is what repairs a
                // message the old bug wrote over.
                (None, None) => {
                    let existing: Option<i64> = tx
                        .query_row(
                            // The list headers are filled in only where they
                            // were never read: a message stored before they
                            // were kept picks them up on its next resync.
                            "UPDATE messages SET flags = ?3,
                                 list_unsub_post = CASE WHEN list_unsub IS NULL
                                     THEN ?5 ELSE list_unsub_post END,
                                 list_unsub = COALESCE(list_unsub, ?4)
                             WHERE mailbox_id = ?1 AND uid = ?2
                             RETURNING id",
                            params![
                                msg.mailbox_id.0,
                                msg.uid,
                                msg.flags.0,
                                list_unsub,
                                list_unsub_post,
                            ],
                            |row| row.get(0),
                        )
                        .optional()?;
                    match existing {
                        Some(id) => (id, false),
                        None => continue,
                    }
                }
            };

            tx.execute(
                "INSERT INTO bodies (message_id, plain, html, attachments)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(message_id) DO UPDATE SET plain = excluded.plain,
                                                       html = excluded.html",
                params![id, msg.plain, msg.html, encode_attachments(&msg.attachments)],
            )?;

            for (index, bytes) in msg.attachment_bytes.iter().enumerate() {
                // An attachment too large to keep is stored as nothing rather
                // than as a truncated file, and `Attachment::stored` reports
                // it so the UI can say why it cannot be opened.
                if bytes.is_empty() {
                    continue;
                }
                tx.execute(
                    "INSERT INTO attachment_blobs (message_id, idx, bytes)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(message_id, idx) DO UPDATE SET bytes = excluded.bytes",
                    params![id, index as i64, bytes],
                )?;
            }

            for image in &msg.inline_images {
                tx.execute(
                    "INSERT INTO inline_images (message_id, content_id, mime, bytes)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(message_id, content_id) DO UPDATE SET
                         mime = excluded.mime, bytes = excluded.bytes",
                    params![id, image.content_id, image.mime, image.bytes],
                )?;
            }

            // Only new messages are indexed. An update carries a flag change
            // and nothing the index holds, and inserting again would leave the
            // same message in a contentless index twice — every search
            // returning it as two hits.
            if is_new {
                tx.execute(
                    "INSERT INTO messages_fts (rowid, subject, sender, body)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        id,
                        msg.subject,
                        format!("{} {}", msg.from.name, msg.from.addr),
                        if msg.plain.is_empty() { &snippet } else { &msg.plain },
                    ],
                )?;
                // Who is on it, in the same transaction, so the client book
                // can never name a message the store does not have or miss one
                // it does.
                if let Some(me) = &me {
                    people::index(
                        &tx,
                        me,
                        people::Indexed {
                            id,
                            account: msg.account_id.0,
                            mailbox: msg.mailbox_id.0,
                            date: msg.date,
                            from: &msg.from,
                            to: &msg.to,
                            cc: &msg.cc,
                            reply_to: &msg.reply_to,
                            snippet: &snippet,
                        },
                        true,
                    )?;
                }
                inserted.push(Ingested { position, id: MessageId(id) });
            }
        }

        tx.commit()?;
        Ok(inserted)
    }

    /// The message list, newest first.
    pub fn envelopes(&self, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
        let this = self.lock();
        query_envelopes(&this.conn, scope, query, None, limit)
    }

    /// The message list as conversations: one row per thread.
    ///
    /// Built by asking for messages the ordinary way and folding them here,
    /// rather than by a `GROUP BY` in SQL. Two reasons, and both are about the
    /// answer being right rather than the query being pretty:
    ///
    /// * The row has to be a real message — its sender, its snippet, its
    ///   flags — and SQLite's bare-column-in-a-group behaviour is a promise
    ///   about *some* row, not the newest one.
    /// * Search and the unread filter apply to messages. A thread is in the
    ///   list when a message in it matches, and the row then stands for the
    ///   matching messages, not for the whole thread.
    ///
    /// `limit` counts conversations, so the underlying fetch reads deeper than
    /// that: a page of rows where each is a long thread would otherwise come
    /// back short. A scope where every message is its own thread costs one
    /// extra comparison per row and nothing else.
    pub fn conversations(&self, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
        // Never fewer messages than conversations asked for, and up to eight
        // times as many when that number is small enough for a page to run
        // short. `saturating_mul` because the caller's limit is a bound rather
        // than a promise, and the list's is large enough to overflow one.
        let depth = limit.saturating_mul(8).max(limit);
        let this = self.lock();
        query_conversations(&this.conn, scope, query, None, depth, limit)
    }

    /// The first `limit` rows of [`Store::conversations`], read without
    /// reading the rest. See `conversations_page`.
    pub fn conversations_page(&self, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
        let this = self.lock();
        conversations_page(&this.conn, scope, query, limit)
    }

    /// Every message in a conversation, oldest first, across all mailboxes.
    pub fn thread(&self, thread_id: ThreadId) -> Result<Vec<Envelope>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(
            "SELECT id, account_id, mailbox_id, thread_id, from_name, from_addr,
                    subject, date_ts, snippet, flags, has_attachments, size
             FROM messages WHERE thread_id = ?1 ORDER BY date_ts ASC",
        )?;
        let rows = stmt.query_map([thread_id.0], read_envelope)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn body(&self, id: MessageId) -> Result<Option<Body>> {
        let this = self.lock();
        let body = this
            .conn
            .query_row(
                // The reply headers ride along with the body deliberately.
                // They are wanted exactly when a message is open, which is
                // exactly when it might be replied to, so they cost nothing on
                // the list's hot path.
                "SELECT b.plain, b.html, b.attachments, m.to_addrs, m.cc_addrs,
                        m.rfc_message_id, m.refs, m.reply_to, m.list_unsub, m.list_unsub_post
                 FROM bodies b JOIN messages m ON m.id = b.message_id
                 WHERE b.message_id = ?1",
                [id.0],
                |r| {
                    let attachments: String = r.get(2)?;
                    let to: String = r.get(3)?;
                    let cc: String = r.get(4)?;
                    let refs: String = r.get(6)?;
                    let reply_to: String = r.get(7)?;
                    Ok(Body {
                        plain: r.get(0)?,
                        html: r.get(1)?,
                        attachments: decode_attachments(&attachments),
                        to: address::decode_list(&to),
                        cc: address::decode_list(&cc),
                        message_id: r.get(5)?,
                        // Stored space-joined, which is how `References`
                        // arrives on the wire.
                        references: refs
                            .split_whitespace()
                            .map(str::to_string)
                            .collect(),
                        reply_to: address::decode_list(&reply_to),
                        inline_images: Vec::new(),
                        list_unsubscribe: decode_list_unsubscribe(r.get(8)?, r.get(9)?),
                    })
                },
            )
            .optional()?;

        // Fetched separately, and only for a message that has any: the blobs
        // are the one place this schema stores anything large.
        let Some(mut body) = body else { return Ok(None) };
        if body.attachments.iter().any(Attachment::is_embedded_image) {
            let mut statement = this.conn.prepare(
                "SELECT content_id, mime, bytes FROM inline_images WHERE message_id = ?1",
            )?;
            let rows = statement.query_map([id.0], |r| {
                Ok(InlineImage { content_id: r.get(0)?, mime: r.get(1)?, bytes: r.get(2)? })
            })?;
            body.inline_images = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        }

        Ok(Some(body))
    }

    /// One attachment's bytes, by its position in the message's list.
    ///
    /// Deliberately not loaded with the body: showing that a message has a
    /// 4 MB PDF should not cost 4 MB of reads.
    /// One message's envelope, by id.
    ///
    /// For the callers that have an id and need the message: a rule being told
    /// what just arrived, an action working on a selection that has scrolled
    /// out of the loaded page. `None` when it has since been deleted, which a
    /// caller holding an id from a moment ago has to be able to find out.
    pub fn envelope(&self, id: MessageId) -> Result<Option<Envelope>> {
        Ok(self
            .lock()
            .conn
            .query_row(
                "SELECT id, account_id, mailbox_id, thread_id, from_name, from_addr,
                        subject, date_ts, snippet, flags, has_attachments, size
                 FROM messages WHERE id = ?1",
                params![id.0],
                read_envelope,
            )
            .optional()?)
    }

    /// One of the headers this store keeps a column for, by name.
    ///
    /// Answers for the parsed headers that have a column and for nothing else.
    /// A name it does not know is `None`, which means "never kept" and which a
    /// caller cannot tell from "was not there" — Spool stores parsed mail
    /// rather than raw source, and that is the honest extent of it.
    ///
    /// Separate from [`Store::body`] because the common question — is this a
    /// reply, and to what? — should not copy a megabyte of HTML to answer
    /// something about forty bytes.
    pub fn header(&self, id: MessageId, name: &str) -> Result<Option<String>> {
        // A fixed map rather than interpolating the name into the SQL, which
        // is the difference between a lookup and an injection.
        let column = match name.to_ascii_lowercase().as_str() {
            "message-id" => "rfc_message_id",
            "in-reply-to" => "in_reply_to",
            "references" => "refs",
            "from" => "from_addr",
            "to" => "to_addrs",
            "cc" => "cc_addrs",
            "reply-to" => "reply_to",
            "subject" => "subject",
            "date" => "date_ts",
            _ => return Ok(None),
        };

        let sql = format!("SELECT {column} FROM messages WHERE id = ?1");
        let value: Option<String> = self
            .lock()
            .conn
            .query_row(&sql, params![id.0], |row| row.get::<_, Option<String>>(0))
            .optional()?
            .flatten();

        // An empty column is a header that was not there, which reads better
        // as absent than as the empty string.
        Ok(value.filter(|v| !v.is_empty()))
    }

    /// Reads one of an extension's stored values.
    ///
    /// `extension` comes from the host rather than from the extension, so the
    /// namespace is not something an extension can step outside of.
    pub fn ext_get(&self, extension: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .conn
            .query_row(
                "SELECT value FROM extension_state WHERE extension = ?1 AND key = ?2",
                params![extension, key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn ext_set(&self, extension: &str, key: &str, value: &str) -> Result<()> {
        self.lock().conn.execute(
            "INSERT INTO extension_state (extension, key, value) VALUES (?1, ?2, ?3)
             ON CONFLICT(extension, key) DO UPDATE SET value = excluded.value",
            params![extension, key, value],
        )?;
        Ok(())
    }

    pub fn ext_delete(&self, extension: &str, key: &str) -> Result<()> {
        self.lock().conn.execute(
            "DELETE FROM extension_state WHERE extension = ?1 AND key = ?2",
            params![extension, key],
        )?;
        Ok(())
    }

    /// Forgets everything one extension knows.
    ///
    /// For uninstalling. Kept here rather than left to the caller because an
    /// extension's state outliving its removal is how a reinstall inherits
    /// decisions nobody remembers making.
    pub fn ext_clear(&self, extension: &str) -> Result<usize> {
        Ok(self
            .lock()
            .conn
            .execute("DELETE FROM extension_state WHERE extension = ?1", params![extension])?)
    }

    pub fn attachment_bytes(&self, id: MessageId, index: usize) -> Result<Option<Vec<u8>>> {
        let this = self.lock();
        let bytes = this
            .conn
            .query_row(
                "SELECT bytes FROM attachment_blobs WHERE message_id = ?1 AND idx = ?2",
                params![id.0, index as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(bytes)
    }

    /// Where a message lives on its server: account address, mailbox path, UID.
    ///
    /// Everything needed to go back and ask for it again. Wanted by
    /// attachments too large to have been kept — the metadata says a 15 MB PDF
    /// arrived, and this is how the bytes are found when somebody finally
    /// wants them.
    ///
    /// `None` when the message has no UID, which means it never came from a
    /// server and there is nowhere to ask.
    pub fn message_origin(&self, id: MessageId) -> Result<Option<MessageOrigin>> {
        let this = self.lock();
        let origin = this
            .conn
            .query_row(
                "SELECT a.email, b.path, m.uid, m.size
                 FROM messages m
                 JOIN mailboxes b ON b.id = m.mailbox_id
                 JOIN accounts  a ON a.id = m.account_id
                 WHERE m.id = ?1 AND m.uid IS NOT NULL",
                params![id.0],
                |r| {
                    Ok(MessageOrigin {
                        email: r.get(0)?,
                        mailbox: r.get(1)?,
                        uid: r.get(2)?,
                        size: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(origin)
    }

    /// Records a message's `List-Unsubscribe` headers, read after the fact.
    ///
    /// For mail stored before the headers were kept, once the server has been
    /// asked. An unknown answer is not written: it would say nothing new.
    pub fn set_list_unsubscribe(&self, id: MessageId, list: &ListUnsubscribe) -> Result<()> {
        if list.is_unknown() {
            return Ok(());
        }
        let (header, one_click) = encode_list_unsubscribe(list);
        self.lock().conn.execute(
            "UPDATE messages SET list_unsub = ?2, list_unsub_post = ?3 WHERE id = ?1",
            params![id.0, header, one_click],
        )?;
        Ok(())
    }

    /// Which of a message's attachments actually have bytes stored.
    ///
    /// Returned as a set of indices so the reader can offer to open the ones
    /// it can and say why about the ones it cannot.
    pub fn stored_attachments(&self, id: MessageId) -> Result<Vec<usize>> {
        let this = self.lock();
        let mut stmt = this
            .conn
            .prepare_cached("SELECT idx FROM attachment_blobs WHERE message_id = ?1")?;
        let rows = stmt.query_map([id.0], |r| r.get::<_, i64>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|i| i as usize).collect())
    }

    /// Sets a message's flags locally and queues the same change for IMAP.
    ///
    /// One call rather than two, and in one transaction, because the order
    /// cannot be got wrong that way: the queue records where the message is on
    /// the server, and `move_message` below is about to make that unknowable.
    pub fn set_flags(&self, id: MessageId, flags: Flags) -> Result<()> {
        let mut this = self.lock();
        let tx = this.conn.transaction()?;
        Store::queue_flags(&tx, id, flags)?;
        tx.execute("UPDATE messages SET flags = ?2 WHERE id = ?1", params![id.0, flags.0])?;
        tx.commit()?;
        Ok(())
    }

    /// Writes flags down without telling the server.
    ///
    /// For [`Flags::LOCAL_ONLY`] bits, which have nothing to push. Queueing
    /// them anyway would leave an outbox entry that never drains, and an
    /// undrained entry is how a message stops accepting flag updates from the
    /// server altogether — `apply_server_flags` skips any row with one
    /// waiting, on the grounds that the local change is the newer truth.
    pub fn set_local_flags(&self, id: MessageId, flags: Flags) -> Result<()> {
        let this = self.lock();
        this.conn
            .execute("UPDATE messages SET flags = ?2 WHERE id = ?1", params![id.0, flags.0])?;
        Ok(())
    }

    /// Marks every message in a mailbox as read. One statement, so it stays
    /// fast on a mailbox with tens of thousands of unread messages.
    pub fn mark_all_read(&self, scope: Scope) -> Result<usize> {
        let mut this = self.lock();
        // The same predicate twice: once against the joined query below,
        // where the columns need qualifying, and once against the bare
        // `UPDATE`, which SQLite will not let us alias.
        let (sql, joined, param): (&str, &str, i64) = match scope {
            Scope::Mailbox(id) => ("mailbox_id = ?2", "m.mailbox_id = ?2", id.0),
            Scope::Account(id) => (
                // The same rows the account row lists, so marking them read
                // cannot reach mail that was never on screen.
                "mailbox_id IN (SELECT id FROM mailboxes WHERE account_id = ?2 AND role = 'inbox')",
                "m.mailbox_id IN (SELECT id FROM mailboxes WHERE account_id = ?2 AND role = 'inbox')",
                id.0,
            ),
            Scope::Everything => ("?2 = ?2", "?2 = ?2", 0),
        };
        let tx = this.conn.transaction()?;

        // Queued in one statement rather than one call per message: this is
        // the action that can touch tens of thousands of rows, and the whole
        // reason it is a single `UPDATE` below is that it has to stay fast on
        // a mailbox that size.
        //
        // Only messages that are actually unread, and only ones the server
        // knows about. Marking an already-read message read owes IMAP nothing.
        tx.execute(
            &format!(
                "INSERT INTO outbox (account_id, folder, uid, kind, flags, target, queued_at)
                 SELECT m.account_id, b.path, m.uid, 'flags', m.flags | ?1, '', ?3
                 FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
                 WHERE m.uid IS NOT NULL AND m.flags & ?1 = 0 AND {joined}"
            ),
            params![Flags::SEEN, param, outbox::now()],
        )?;

        let changed = tx.execute(
            &format!("UPDATE messages SET flags = flags | ?1 WHERE {sql}"),
            params![Flags::SEEN, param],
        )?;
        tx.commit()?;
        Ok(changed)
    }

    /// Moves a message to another mailbox locally and queues the IMAP move.
    ///
    /// The UID is cleared because a message has a different one in every
    /// folder and this row no longer knows its own. That is also why the queue
    /// is written first, inside the same transaction: after this statement
    /// there is nothing left to tell the server which message was meant.
    ///
    /// A row left with no UID is picked back up by `ingest` when the
    /// destination folder is next synced, which is where it learns its new
    /// one.
    pub fn move_message(&self, id: MessageId, to: MailboxId) -> Result<()> {
        let mut this = self.lock();
        let tx = this.conn.transaction()?;
        Store::queue_move(&tx, id, to)?;
        tx.execute(
            "UPDATE messages SET mailbox_id = ?2, uid = NULL WHERE id = ?1",
            params![id.0, to.0],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_message(&self, id: MessageId) -> Result<()> {
        let this = self.lock();
        let tx = this.conn.unchecked_transaction()?;
        Store::queue_delete(&tx, id)?;
        tx.execute(
            "INSERT INTO messages_fts (messages_fts, rowid) VALUES ('delete', ?1)",
            [id.0],
        )
        .ok();
        tx.execute("DELETE FROM bodies WHERE message_id = ?1", [id.0])?;
        tx.execute("DELETE FROM messages WHERE id = ?1", [id.0])?;
        tx.commit()?;
        Ok(())
    }

    /// Saves the draft being written, replacing the previous copy.
    ///
    /// Pass the id of an earlier save to update it, or `None` to start a row.
    /// The composer holds one draft at a time, so this is an upsert against
    /// that one row rather than a growing pile of near-identical copies.
    pub fn save_draft(
        &self,
        id: Option<i64>,
        draft: &Draft,
        now: i64,
        files: DraftFiles,
    ) -> Result<i64> {
        let mut this = self.lock();
        let from = draft.from.encoded();
        let to = address::encode_list(&draft.to);
        let cc = address::encode_list(&draft.cc);
        let bcc = address::encode_list(&draft.bcc);
        let refs = draft.references.join(" ");
        let in_reply_to = draft.in_reply_to.clone().unwrap_or_default();
        let replying_to = draft.replying_to.map(|id| id.0);
        let forwarding = draft.forwarding.map(|id| id.0);

        // One transaction for the row and its files. An autosave that wrote
        // the body and then failed on the attachments would leave a draft
        // describing a message it no longer carries.
        let tx = this.conn.transaction()?;

        let id = match id {
            Some(id) => {
                tx.execute(
                    "UPDATE drafts SET from_addr = ?2, to_addrs = ?3, cc_addrs = ?4,
                            bcc_addrs = ?5, subject = ?6, body = ?7, in_reply_to = ?8,
                            refs = ?9, replying_to = ?10, updated_ts = ?11, forwarding = ?12,
                            format = ?13, quoted_html = ?14, html = ?15
                     WHERE id = ?1",
                    params![
                        id, from, to, cc, bcc, draft.subject, draft.body, in_reply_to, refs,
                        replying_to, now, forwarding, draft.format.as_str(), draft.quoted_html,
                        draft.html
                    ],
                )?;
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO drafts (from_addr, to_addrs, cc_addrs, bcc_addrs, subject,
                                         body, in_reply_to, refs, replying_to, updated_ts,
                                         forwarding, format, quoted_html, html)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        from, to, cc, bcc, draft.subject, draft.body, in_reply_to, refs,
                        replying_to, now, forwarding, draft.format.as_str(), draft.quoted_html,
                        draft.html
                    ],
                )?;
                tx.last_insert_rowid()
            }
        };

        // Only when they actually changed, and the caller is the one who
        // knows. An autosave fires every few seconds while somebody types, and
        // the attachments are by far the largest thing on a draft — rewriting
        // twenty-five megabytes because a word was added to the body is a
        // stall in the one place an app must never stall.
        //
        // Replaced wholesale when they do change, rather than diffed: that
        // happens once per attach, where the cost is a deliberate act and a
        // diff that got it wrong would send the wrong file.
        if files == DraftFiles::Replace {
            tx.execute("DELETE FROM draft_attachments WHERE draft_id = ?1", [id])?;
            for (index, attached) in draft.attachments.iter().enumerate() {
                tx.execute(
                    "INSERT INTO draft_attachments (draft_id, idx, name, mime, bytes)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, index as i64, attached.name, attached.mime, attached.bytes],
                )?;
            }
        }

        tx.commit()?;
        Ok(id)
    }

    /// Whether a save has to write the draft's files as well as its text.
    ///
    /// A required argument rather than something inferred, because the cheap
    /// answer and the correct answer differ by twenty-five megabytes and only
    /// the caller knows which applies.
    ///
    /// Every unsent draft, most recently touched first.
    pub fn drafts(&self) -> Result<Vec<(i64, Draft)>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(
            "SELECT id, from_addr, to_addrs, cc_addrs, bcc_addrs, subject, body,
                    in_reply_to, refs, replying_to, forwarding, format, quoted_html, html
             FROM drafts ORDER BY updated_ts DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                let in_reply_to: String = row.get(7)?;
                let refs: String = row.get(8)?;
                let replying_to: Option<i64> = row.get(9)?;
                let forwarding: Option<i64> = row.get(10)?;
                let format: String = row.get(11)?;
                let quoted_html: Option<String> = row.get(12)?;
                let html: Option<String> = row.get(13)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    Draft {
                        from: address::decode_list(&row.get::<_, String>(1)?)
                            .into_iter()
                            .next()
                            .unwrap_or_default(),
                        to: address::decode_list(&row.get::<_, String>(2)?),
                        cc: address::decode_list(&row.get::<_, String>(3)?),
                        bcc: address::decode_list(&row.get::<_, String>(4)?),
                        subject: row.get(5)?,
                        body: row.get(6)?,
                        in_reply_to: (!in_reply_to.is_empty()).then_some(in_reply_to),
                        references: refs
                            .split_whitespace()
                            .map(str::to_string)
                            .collect::<Vec<_>>(),
                        replying_to: replying_to.map(MessageId),
                        forwarding: forwarding.map(MessageId),
                        format: spool_core::draft::Format::parse(&format),
                        quoted_html,
                        html,
                        // Filled in below: one query for the lot rather than
                        // one per draft, since the common case is a handful of
                        // drafts and at most one of them has files.
                        attachments: Vec::new(),
                    },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut rows = rows;
        let mut files = this.conn.prepare_cached(
            "SELECT draft_id, name, mime, bytes FROM draft_attachments ORDER BY draft_id, idx",
        )?;
        let attached = files.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                Attached {
                    name: row.get(1)?,
                    mime: row.get(2)?,
                    bytes: row.get(3)?,
                },
            ))
        })?;

        for row in attached {
            let (draft_id, attached) = row?;
            if let Some((_, draft)) = rows.iter_mut().find(|(id, _)| *id == draft_id) {
                draft.attachments.push(attached);
            }
        }

        Ok(rows)
    }

    /// Throws a draft away: sent, or discarded.
    pub fn delete_draft(&self, id: i64) -> Result<()> {
        let this = self.lock();
        this.conn.execute("DELETE FROM drafts WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Whether the store holds no mail at all.
    ///
    /// Separate from [`Store::message_count`] because the two questions cost
    /// very different amounts and only one of them is asked during start-up.
    /// Counting has to visit every row of an index; this stops at the first.
    pub fn is_empty(&self) -> Result<bool> {
        let this = self.lock();
        let any: Option<i64> = this
            .conn
            .query_row("SELECT 1 FROM messages LIMIT 1", [], |r| r.get(0))
            .optional()?;
        Ok(any.is_none())
    }

    pub fn message_count(&self) -> Result<u64> {
        let this = self.lock();
        let count: i64 = this.conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
        Ok(count.max(0) as u64)
    }

    /// Folds the write-ahead log back into the database and truncates it.
    ///
    /// For the *next* launch rather than for this one. A WAL is only cheap to
    /// open while the `-shm` index beside it is valid; when it is not — the
    /// first run after a reboot, or after the app was killed — SQLite rebuilds
    /// that index by reading the entire log before it will answer a single
    /// query. A log left at a few hundred megabytes, which one large sync is
    /// enough to produce, is therefore a start-up that stalls for reasons that
    /// have nothing to do with the mailbox's size and everything to do with
    /// how the app happened to exit last time.
    ///
    /// Automatic checkpointing already folds pages back as it goes, so by the
    /// time this runs there is usually little left to copy — but it never
    /// shrinks the file, and a reader holding a snapshot at the wrong moment
    /// is all it takes for the log to keep the high-water mark of the busiest
    /// sync of the session. `TRUNCATE` is what gives the space back.
    /// Skipped rather than waited for when the store is busy. This runs while
    /// the app is quitting, and a sync still finishing a batch holds the lock
    /// for as long as that batch takes — which is a perfectly good reason to
    /// leave the log alone and a very poor reason to make somebody watch a
    /// window that will not go away.
    pub fn checkpoint(&self) -> Result<()> {
        let Ok(this) = self.inner.try_lock() else {
            log::debug!("the store was busy; leaving the write-ahead log as it is");
            return Ok(());
        };
        this.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Size of the database on disk, for the status bar.
    pub fn size_on_disk(&self) -> Result<u64> {
        let this = self.lock();
        let pages: i64 = this.conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
        let page_size: i64 = this.conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
        Ok((pages * page_size).max(0) as u64)
    }

    /// The highest UID seen in a mailbox, so an incremental sync knows where
    /// to resume.
    /// Records what the server says a folder holds.
    ///
    /// Written at the top of every folder sync, from the `SELECT` response,
    /// so it costs no round trip of its own.
    pub fn set_server_total(&self, mailbox: MailboxId, total: u32) -> Result<()> {
        let this = self.lock();
        this.conn.execute(
            "UPDATE mailboxes SET server_total = ?2 WHERE id = ?1",
            params![mailbox.0, total],
        )?;
        Ok(())
    }

    /// The lowest UID stored for a mailbox, or `0` when it holds nothing.
    ///
    /// Where a backfill starts looking: everything below this is mail the
    /// server has and this copy does not.
    pub fn min_uid(&self, mailbox: MailboxId) -> Result<u32> {
        let this = self.lock();
        let uid: Option<u32> = this.conn.query_row(
            "SELECT MIN(uid) FROM messages WHERE mailbox_id = ?1 AND uid IS NOT NULL",
            [mailbox.0],
            |r| r.get(0),
        )?;
        Ok(uid.unwrap_or(0))
    }

    /// The span of UIDs stored for a mailbox, or `None` when it holds none.
    ///
    /// What a flag refresh asks the server about: everything between the
    /// oldest and newest message here. Narrower than `1:*`, which on a folder
    /// of twenty thousand would fetch a reply about nineteen thousand
    /// messages this copy has never seen.
    pub fn uid_range(&self, mailbox: MailboxId) -> Result<Option<(u32, u32)>> {
        let this = self.lock();
        let range: (Option<u32>, Option<u32>) = this.conn.query_row(
            "SELECT MIN(uid), MAX(uid) FROM messages
             WHERE mailbox_id = ?1 AND uid IS NOT NULL",
            [mailbox.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(match range {
            (Some(low), Some(high)) => Some((low, high)),
            _ => None,
        })
    }

    /// Takes the server's word for the flags on messages already stored.
    ///
    /// The other half of sync: fetching only ever looks *above* the highest
    /// UID here, so a message downloaded once keeps whatever flags it had that
    /// day. Read something on your phone and this copy goes on calling it
    /// unread for ever.
    ///
    /// Local changes not yet pushed win. A row with an entry in the outbox is
    /// one this client has changed and the server has not been told about, so
    /// the server's answer is the older of the two — applying it would undo
    /// the change and then, on the next drain, apply it again.
    ///
    /// Returns how many rows actually changed, so a sync can say whether it
    /// found anything rather than reporting work it did not do.
    pub fn apply_server_flags(
        &self,
        mailbox: MailboxId,
        folder: &str,
        flags: &[(u32, Flags)],
    ) -> Result<usize> {
        if flags.is_empty() {
            return Ok(0);
        }

        let mut this = self.lock();
        let tx = this.conn.transaction()?;
        let mut changed = 0usize;

        {
            // The server's view wins for the flags that came from the server,
            // and the local-only bits are carried across it — see
            // `Flags::LOCAL_ONLY`. Written as one expression rather than a
            // read-modify-write so the row is never briefly wrong, and
            // compared against the same expression so an unchanged row still
            // counts as unchanged.
            let mut stmt = tx.prepare(
                "UPDATE messages SET flags = (flags & ?5) | ?3
                 WHERE mailbox_id = ?1 AND uid = ?2 AND flags <> ((flags & ?5) | ?3)
                   AND NOT EXISTS (
                       SELECT 1 FROM outbox
                       WHERE outbox.folder = ?4 AND outbox.uid = ?2
                   )",
            )?;
            for (uid, server) in flags {
                changed +=
                    stmt.execute(params![mailbox.0, uid, server.0, folder, Flags::LOCAL_ONLY])?;
            }
        }

        tx.commit()?;
        Ok(changed)
    }

    /// Records what the server says this folder's UIDs are numbered against,
    /// and says whether they still mean what they used to.
    ///
    /// `UIDVALIDITY` is IMAP's way of saying "forget everything you knew about
    /// this folder": the server promises a UID identifies the same message for
    /// as long as this number is unchanged, and promises nothing at all once
    /// it changes. It changes rarely — a mailbox deleted and recreated under
    /// the same name, a restore from backup — and when it does, every stored
    /// UID now points at a different message.
    ///
    /// Returns `true` when the folder was reset, which it does by emptying it:
    /// the mail comes back on the next fetch under its new numbering. Keeping
    /// it would be keeping rows whose UIDs are lies, and the first thing to
    /// act on such a row would be [`Store::remove_expunged`].
    pub fn set_uid_validity(&self, mailbox: MailboxId, validity: u32) -> Result<bool> {
        let this = self.lock();
        let stored: Option<u32> = this.conn.query_row(
            "SELECT uid_validity FROM mailboxes WHERE id = ?1",
            [mailbox.0],
            |r| r.get(0),
        )?;

        // First time through -- including every folder in a store written
        // before this column existed -- is not a change. There is nothing to
        // have invalidated.
        if stored == Some(validity) || stored.is_none() {
            this.conn.execute(
                "UPDATE mailboxes SET uid_validity = ?2 WHERE id = ?1",
                params![mailbox.0, validity],
            )?;
            return Ok(false);
        }

        let tx = this.conn.unchecked_transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare("SELECT id FROM messages WHERE mailbox_id = ?1")?;
            stmt.query_map([mailbox.0], |r| r.get(0))?.collect::<Result<Vec<i64>, _>>()?
        };
        for message in &ids {
            tx.execute(
                "INSERT INTO messages_fts (messages_fts, rowid) VALUES ('delete', ?1)",
                [message],
            )
            .ok();
        }
        tx.execute("DELETE FROM bodies WHERE message_id IN (SELECT id FROM messages WHERE mailbox_id = ?1)", [mailbox.0])?;
        tx.execute("DELETE FROM messages WHERE mailbox_id = ?1", [mailbox.0])?;
        tx.execute(
            "UPDATE mailboxes SET uid_validity = ?2 WHERE id = ?1",
            params![mailbox.0, validity],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Empties a folder the server says holds nothing.
    ///
    /// The unambiguous half of [`Store::remove_expunged`]: there is no range
    /// to ask about and no reply to read, only a `SELECT` reporting zero. Mail
    /// still waiting in the outbox stays, because the server has not been told
    /// about it yet and its absence there proves nothing.
    pub fn clear_mailbox(&self, mailbox: MailboxId, folder: &str) -> Result<usize> {
        self.remove_expunged(mailbox, folder, 0, u32::MAX, &[])
    }

    /// Removes mail that is no longer on the server.
    ///
    /// `present` is every UID the server returned for the range `low..=high`,
    /// which the flag refresh has already asked for and paid for. Anything
    /// stored in that range and missing from it has been expunged by another
    /// client, and a message deleted on your phone should not still be here.
    ///
    /// Bounded by the range that was actually asked about, because a UID above
    /// it is mail that arrived after the question was put and has not been
    /// ruled on. Rows with an entry in the outbox are left alone for the same
    /// reason [`Store::apply_server_flags`] leaves them: a local change not yet
    /// pushed is the newer truth, and the server has not been told yet.
    pub fn remove_expunged(
        &self,
        mailbox: MailboxId,
        folder: &str,
        low: u32,
        high: u32,
        present: &[u32],
    ) -> Result<usize> {
        let kept: std::collections::HashSet<u32> = present.iter().copied().collect();

        let this = self.lock();
        let tx = this.conn.unchecked_transaction()?;

        let gone: Vec<(i64, u32)> = {
            let mut stmt = tx.prepare(
                "SELECT m.id, m.uid FROM messages m
                 WHERE m.mailbox_id = ?1 AND m.uid IS NOT NULL
                   AND m.uid BETWEEN ?2 AND ?3
                   AND NOT EXISTS (
                       SELECT 1 FROM outbox
                       WHERE outbox.folder = ?4 AND outbox.uid = m.uid
                   )",
            )?;
            stmt.query_map(params![mailbox.0, low, high, folder], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|(_, uid)| !kept.contains(uid))
            .collect()
        };

        for (id, _) in &gone {
            tx.execute("INSERT INTO messages_fts (messages_fts, rowid) VALUES ('delete', ?1)", [id])
                .ok();
            tx.execute("DELETE FROM bodies WHERE message_id = ?1", [id])?;
            tx.execute("DELETE FROM messages WHERE id = ?1", [id])?;
        }

        tx.commit()?;
        Ok(gone.len())
    }

    pub fn max_uid(&self, mailbox: MailboxId) -> Result<u32> {
        let this = self.lock();
        let uid: Option<u32> = this.conn.query_row(
            "SELECT MAX(uid) FROM messages WHERE mailbox_id = ?1",
            [mailbox.0],
            |r| r.get(0),
        )?;
        Ok(uid.unwrap_or(0))
    }
}

/// The first `limit` conversations, as the whole list would have them.
///
/// Not `fold_conversations` over the newest few thousand messages, which finds
/// the right conversations but counts them wrong: a thread whose older
/// messages fall outside the window shows fewer messages and fewer people
/// than it has, and the row corrects itself a moment later when the whole
/// list lands — a visible flicker, and a moment of wrong numbers. So the
/// newest messages pick the conversations, and then every matching message
/// in just those conversations is read to count them.
fn conversations_page(conn: &Connection, scope: Scope, query: &Query, limit: usize) -> Result<Vec<Envelope>> {
    let depth = limit.saturating_mul(8).max(limit);
    let picked = query_conversations(conn, scope, query, None, depth, limit)?;
    if picked.is_empty() {
        return Ok(picked);
    }
    let threads: Vec<i64> = picked.iter().map(|row| row.thread_id.0).collect();
    query_conversations(conn, scope, query, Some(&threads), usize::MAX >> 1, limit)
}

/// Folds messages, newest first, into one row per thread. See
/// [`Store::conversations`].
fn fold_conversations(messages: impl IntoIterator<Item = Envelope>, limit: usize) -> Vec<Envelope> {
    let mut rows: Vec<Envelope> = Vec::new();
    // Where each thread's row lives in `rows`, so folding stays linear.
    let mut seen: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();

    for message in messages {
        match seen.get(&message.thread_id.0) {
            Some(&index) => {
                let row: &mut Envelope = &mut rows[index];
                let group = row.group.get_or_insert_with(|| Group {
                    ids: vec![row.id],
                    unread: u32::from(!row.flags.is_seen()),
                    participants: vec![display_name(&row.from)],
                });

                group.ids.push(message.id);
                group.unread += u32::from(!message.flags.is_seen());

                // Oldest first: the messages arrive newest first, so each
                // new participant goes on the front. Bounded because a
                // hundred-message thread has to fit on one line.
                let name = display_name(&message.from);
                if !group.participants.contains(&name)
                    && group.participants.len() < MAX_PARTICIPANTS
                {
                    group.participants.insert(0, name);
                }

                // The row keeps the newest message but takes on the
                // conversation's marks: a thread holding a flagged reply
                // is a flagged thread, and one with an attachment
                // anywhere has an attachment.
                row.flags = Flags(row.flags.0 | (message.flags.0 & Flags::FLAGGED));
                row.has_attachments |= message.has_attachments;
            }
            // A thread not on the page yet. Once the page is full, it
            // stays off it — but the loop carries on, because an older
            // message may still belong to a conversation that *is* on the
            // page, and stopping here would undercount it. The rows were
            // fetched in one query either way, so folding the rest of them
            // costs a comparison each.
            None if rows.len() < limit => {
                seen.insert(message.thread_id.0, rows.len());
                rows.push(message);
            }
            None => {}
        }
    }

    rows
}

/// The message list query, on whichever connection is asked.
fn query_envelopes(
    conn: &Connection,
    scope: Scope,
    query: &Query,
    threads: Option<&[i64]>,
    limit: usize,
) -> Result<Vec<Envelope>> {
    with_envelopes(conn, scope, query, threads, limit, |rows| rows.collect())
}

/// The message list as conversations, folded as the rows arrive.
///
/// Rather than `fold_conversations(query_envelopes(…))`, which holds every
/// message in the scope at once to keep one row per thread: 150,000 of them in
/// a large inbox, for 4,000 rows — a peak on every reload that the allocator
/// then keeps.
fn query_conversations(
    conn: &Connection,
    scope: Scope,
    query: &Query,
    threads: Option<&[i64]>,
    depth: usize,
    limit: usize,
) -> Result<Vec<Envelope>> {
    with_envelopes(conn, scope, query, threads, depth, |rows| fold_conversations(rows, limit))
}

/// Runs the list query and hands its rows, newest first, to `consume`.
fn with_envelopes<R>(
    conn: &Connection,
    scope: Scope,
    query: &Query,
    threads: Option<&[i64]>,
    limit: usize,
    consume: impl FnOnce(&mut dyn Iterator<Item = Envelope>) -> R,
) -> Result<R> {
    // Bound in the order the placeholders appear: scope, then whatever the
    // search box added, then the limit.
    let mut values: Vec<Value> = Vec::new();

    let scope_sql: String = match scope {
        Scope::Mailbox(id) => {
            values.push(id.0.into());
            "m.mailbox_id = ?".to_string()
        }
        // The account's inbox, not the account's every folder. A Gmail
        // account keeps a copy of each message in All Mail as well as in
        // the label it arrived under, so "everything in this account" is
        // a list where most rows appear twice — and one mixing sent mail,
        // spam and trash into the reading order besides. The account row
        // is the front door, and this is what is behind it.
        //
        // The folders are resolved here rather than left as an `IN
        // (SELECT …)`, which reads better and costs the index: SQLite
        // stops walking `(mailbox_id, date_ts)` in order and sorts the
        // whole result instead — 232ms against a 14,000-message inbox,
        // where naming the folder is 6ms.
        Scope::Account(id) => {
            let inboxes = inbox_ids(conn, id)?;
            match inboxes.len() {
                // An account still syncing has no inbox yet. Nothing is
                // the honest answer, and the folders arriving reloads it.
                0 => "1 = 0".to_string(),
                _ => {
                    let places = vec!["?"; inboxes.len()].join(", ");
                    values.extend(inboxes.into_iter().map(Value::from));
                    format!("m.mailbox_id IN ({places})")
                }
            }
        }
        Scope::Everything => "1 = 1".to_string(),
    };

    let mut filter_sql = String::new();
    match query {
        Query::All => {}
        Query::Unread => filter_sql = format!(" AND m.flags & {} = 0", Flags::SEEN),
        Query::Flagged => filter_sql = format!(" AND m.flags & {} != 0", Flags::FLAGGED),
        Query::Text(text) => {
            let search = Search::parse(text, now_seconds());
            filter_sql = search.clauses(&mut values);

            // The full-text index only sees what was left after the
            // operators were taken out, so `from:ana roof` is a sender
            // filter and a one-word search rather than a search for both.
            if let Some(fts) = search.fts() {
                filter_sql.push_str(
                    " AND m.id IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)",
                );
                values.push(fts.into());
            }
        }
    }

    // Narrowed to some conversations, for a first page that has to count each
    // of them in full. See `conversations_page`.
    if let Some(threads) = threads {
        filter_sql.push_str(&format!(" AND m.thread_id IN ({})", vec!["?"; threads.len()].join(", ")));
        values.extend(threads.iter().map(|&t| Value::from(t)));
    }

    values.push((limit as i64).into());

    let sql = format!(
        "SELECT m.id, m.account_id, m.mailbox_id, m.thread_id, m.from_name, m.from_addr,
                m.subject, m.date_ts, m.snippet, m.flags, m.has_attachments, m.size
         FROM messages m
         WHERE {scope_sql}{filter_sql}
         ORDER BY m.date_ts DESC
         LIMIT ?"
    );

    let mut stmt = conn.prepare_cached(&sql)?;
    let mut failed = None;
    let mut rows = stmt.query_map(params_from_iter(values), read_envelope)?.map_while(|row| {
        row.map_err(|e| failed = Some(e)).ok()
    });
    let consumed = consume(&mut rows);
    drop(rows);
    match failed {
        // An interrupted list query ends up here, and so does a bad row: the
        // caller gets the error rather than a list cut short without saying.
        Some(e) => Err(e.into()),
        None => Ok(consumed),
    }
}

/// The inbox folders one account has — usually exactly one.
///
/// A list rather than an `Option` because nothing in IMAP says an account has
/// only one: a server can mark two folders `\\Inbox`, and an account that has
/// just been added has none at all until its first `LIST` comes back.
fn inbox_ids(conn: &Connection, account: AccountId) -> Result<Vec<i64>> {
    let mut stmt = conn
        .prepare_cached("SELECT id FROM mailboxes WHERE account_id = ?1 AND role = 'inbox'")?;
    let ids = stmt
        .query_map(params![account.0], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn read_envelope(r: &Row<'_>) -> rusqlite::Result<Envelope> {
    Ok(Envelope {
        id: MessageId(r.get(0)?),
        account_id: AccountId(r.get(1)?),
        mailbox_id: MailboxId(r.get(2)?),
        thread_id: ThreadId(r.get(3)?),
        from: Address { name: r.get(4)?, addr: r.get(5)? },
        subject: r.get(6)?,
        date: r.get(7)?,
        snippet: r.get(8)?,
        flags: Flags(r.get(9)?),
        has_attachments: r.get(10)?,
        size: r.get(11)?,
        group: None,
    })
}

/// How many names a conversation row will show before it stops counting.
///
/// The row has one line. Past a handful, more names is less information: what
/// matters at that point is that it is a crowd, which the message count
/// already says.
const MAX_PARTICIPANTS: usize = 5;

/// The short name for a participant: what they call themselves, or their
/// address when they gave no name.
fn display_name(from: &Address) -> String {
    let name = from.name.trim();
    if !name.is_empty() {
        // Just the first word. A row of full names is a row that truncates
        // after two of them, and it is also what turns a mailing list's
        // `'Spotify' via wes@example.com` back into `Spotify`.
        let first = name.split_whitespace().next().unwrap_or(name);
        // Trimmed *after* the split, not before: the quotes a display name
        // needed sit around the name, not around the whole header, so
        // trimming the string's ends leaves the closing one attached. That is
        // how `Spotify', Spotify` — one sender, listed twice — got on screen.
        let cleaned = first.trim_matches(['"', '\'', ',']).trim();
        if !cleaned.is_empty() {
            return cleaned.to_string();
        }
    }
    from.addr.split('@').next().unwrap_or(&from.addr).to_string()
}

/// The last segment of a mailbox path — its display name.
fn leaf_name<'a>(path: &'a str, delimiter: &str) -> &'a str {
    match delimiter.is_empty() {
        true => path,
        false => path.rsplit(delimiter).next().unwrap_or(path),
    }
}

/// Escapes the wildcards in a value used as a `LIKE` prefix.
fn escape_like(value: &str) -> String {
    value.replace('\\', r"\\").replace('%', r"\%").replace('_', r"\_")
}

/// The wall clock, for the search box's relative dates.
fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

fn encode_attachments(attachments: &[Attachment]) -> String {
    // Fields are appended, never reordered: a row written by an older build
    // has three of them and decodes to the same attachment with the newer two
    // defaulted.
    attachments
        .iter()
        .map(|a| {
            format!(
                "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
                a.name,
                a.mime,
                a.size,
                a.content_id.as_deref().unwrap_or_default(),
                u8::from(a.inline),
            )
        })
        .collect::<Vec<_>>()
        .join("\u{2}")
}

/// `List-Unsubscribe` as its two columns.
///
/// `NULL` is "never read", an empty string is "read, and there is none" — the
/// difference between mail that predates the column and mail that is simply
/// not from a list.
fn encode_list_unsubscribe(list: &ListUnsubscribe) -> (Option<&str>, bool) {
    match list {
        ListUnsubscribe::Unknown => (None, false),
        ListUnsubscribe::Absent => (Some(""), false),
        ListUnsubscribe::Offered { header, one_click } => (Some(header), *one_click),
    }
}

fn decode_list_unsubscribe(header: Option<String>, one_click: bool) -> ListUnsubscribe {
    match header {
        None => ListUnsubscribe::Unknown,
        Some(header) if header.is_empty() => ListUnsubscribe::Absent,
        Some(header) => ListUnsubscribe::Offered { header, one_click },
    }
}

fn decode_attachments(encoded: &str) -> Vec<Attachment> {
    encoded
        .split('\u{2}')
        .filter(|s| !s.is_empty())
        .filter_map(|entry| {
            let mut parts = entry.split('\u{1}');
            Some(Attachment {
                name: parts.next()?.to_string(),
                mime: parts.next().unwrap_or("application/octet-stream").to_string(),
                size: parts.next().and_then(|s| s.parse().ok()).unwrap_or(0),
                // Absent in rows written before inline images existed.
                content_id: parts.next().filter(|s| !s.is_empty()).map(str::to_string),
                inline: parts.next().is_some_and(|s| s == "1"),
            })
        })
        .collect()
}

/// The schema version a migrated database ends on.
///
/// Every `if version < n` block below has to leave the database at this
/// number; they are applied in order for exactly that reason.
pub const SCHEMA_VERSION: u32 = 18;

impl Inner {
    fn migrate(&mut self) -> Result<()> {
        let version: u32 =
            self.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);

        if version < 1 {
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS accounts (
                     id      INTEGER PRIMARY KEY,
                     name    TEXT NOT NULL,
                     email   TEXT NOT NULL UNIQUE,
                     color   INTEGER NOT NULL DEFAULT 0
                 );

                 CREATE TABLE IF NOT EXISTS mailboxes (
                     id          INTEGER PRIMARY KEY,
                     account_id  INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                     path        TEXT NOT NULL,
                     name        TEXT NOT NULL,
                     role        TEXT NOT NULL,
                     UNIQUE(account_id, path)
                 );

                 CREATE TABLE IF NOT EXISTS messages (
                     id              INTEGER PRIMARY KEY,
                     account_id      INTEGER NOT NULL,
                     mailbox_id      INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
                     uid             INTEGER,
                     rfc_message_id  TEXT NOT NULL DEFAULT '',
                     thread_id       INTEGER NOT NULL,
                     in_reply_to     TEXT NOT NULL DEFAULT '',
                     refs            TEXT NOT NULL DEFAULT '',
                     from_name       TEXT NOT NULL DEFAULT '',
                     from_addr       TEXT NOT NULL DEFAULT '',
                     to_addrs        TEXT NOT NULL DEFAULT '',
                     cc_addrs        TEXT NOT NULL DEFAULT '',
                     subject         TEXT NOT NULL DEFAULT '',
                     norm_subject    TEXT NOT NULL DEFAULT '',
                     date_ts         INTEGER NOT NULL,
                     snippet         TEXT NOT NULL DEFAULT '',
                     flags           INTEGER NOT NULL DEFAULT 0,
                     size            INTEGER NOT NULL DEFAULT 0,
                     has_attachments INTEGER NOT NULL DEFAULT 0
                 );

                 -- The list is always ordered by date within a scope, so these
                 -- two cover every query the UI makes without a sort step.
                 CREATE INDEX IF NOT EXISTS idx_messages_mailbox_date
                     ON messages(mailbox_id, date_ts DESC);
                 CREATE INDEX IF NOT EXISTS idx_messages_account_date
                     ON messages(account_id, date_ts DESC);
                 CREATE INDEX IF NOT EXISTS idx_messages_thread
                     ON messages(thread_id, date_ts);
                 CREATE INDEX IF NOT EXISTS idx_messages_rfc_id
                     ON messages(rfc_message_id);
                 CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_mailbox_uid
                     ON messages(mailbox_id, uid) WHERE uid IS NOT NULL;

                 CREATE TABLE IF NOT EXISTS bodies (
                     message_id  INTEGER PRIMARY KEY
                                 REFERENCES messages(id) ON DELETE CASCADE,
                     plain       TEXT NOT NULL DEFAULT '',
                     html        TEXT NOT NULL DEFAULT '',
                     attachments TEXT NOT NULL DEFAULT ''
                 );

                 PRAGMA user_version = 1;",
            )?;
        }

        if version < 2 {
            // Inline image bytes live in their own table rather than in
            // `bodies`: they are the only large binary column in the schema,
            // and opening a message should not drag them off disk unless the
            // message actually has one.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS inline_images (
                     message_id INTEGER NOT NULL
                                REFERENCES messages(id) ON DELETE CASCADE,
                     content_id TEXT NOT NULL,
                     mime       TEXT NOT NULL,
                     bytes      BLOB NOT NULL,
                     PRIMARY KEY (message_id, content_id)
                 ) WITHOUT ROWID;

                 PRAGMA user_version = 2;",
            )?;
        }

        if version < 3 {
            // Attachment bytes, keyed by their position in the message's
            // attachment list. Their own table for the same reason inline
            // images have one: they are large and binary, and opening a
            // message should not drag a 4 MB PDF off disk to show its name.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS attachment_blobs (
                     message_id INTEGER NOT NULL
                                REFERENCES messages(id) ON DELETE CASCADE,
                     idx        INTEGER NOT NULL,
                     bytes      BLOB NOT NULL,
                     PRIMARY KEY (message_id, idx)
                 ) WITHOUT ROWID;

                 PRAGMA user_version = 3;",
            )?;
        }

        if version < 4 {
            // Unsent mail. Its own table rather than a row in `messages`: a
            // draft has no UID, no mailbox and no thread, and every query in
            // the app would have to learn to exclude it.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS drafts (
                     id           INTEGER PRIMARY KEY,
                     from_addr    TEXT NOT NULL DEFAULT '',
                     to_addrs     TEXT NOT NULL DEFAULT '',
                     cc_addrs     TEXT NOT NULL DEFAULT '',
                     bcc_addrs    TEXT NOT NULL DEFAULT '',
                     subject      TEXT NOT NULL DEFAULT '',
                     body         TEXT NOT NULL DEFAULT '',
                     in_reply_to  TEXT NOT NULL DEFAULT '',
                     refs         TEXT NOT NULL DEFAULT '',
                     replying_to  INTEGER,
                     updated_ts   INTEGER NOT NULL
                 );

                 PRAGMA user_version = 4;",
            )?;
        }

        // Version 5 was the address book. It is gone, and the number is not
        // reused: a database that already ran it keeps the unused tables, and
        // the next migration takes it the rest of the way.

        if version < 6 {
            // No foreign key to `messages`, and that is the whole point: a
            // queued delete outlives the row it is about. `ON DELETE CASCADE`
            // here would take the instruction away at exactly the moment it
            // became the only record that the deletion ever happened.
            //
            // The account reference does cascade: removing an account means
            // its mail is gone locally and its server is no longer ours to
            // write to.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS outbox (
                     id         INTEGER PRIMARY KEY,
                     account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                     folder     TEXT NOT NULL,
                     uid        INTEGER NOT NULL,
                     kind       TEXT NOT NULL,
                     flags      INTEGER NOT NULL DEFAULT 0,
                     target     TEXT NOT NULL DEFAULT '',
                     attempts   INTEGER NOT NULL DEFAULT 0,
                     last_error TEXT NOT NULL DEFAULT '',
                     queued_at  INTEGER NOT NULL
                 );

                 -- Drained per account, oldest first, so the id order the
                 -- queue depends on is the order this index hands back.
                 CREATE INDEX IF NOT EXISTS idx_outbox_account
                     ON outbox(account_id, id);
                 -- Coalescing looks up the newest entry for one message.
                 CREATE INDEX IF NOT EXISTS idx_outbox_message
                     ON outbox(folder, uid, id);

                 PRAGMA user_version = 6;",
            )?;
        }

        if version < 7 {
            // A reply's most important header, and the one that was being
            // thrown away: without it, answering a mailing list writes to
            // whoever happened to post rather than to the list.
            //
            // Added as a column on `messages` next to the other addresses
            // rather than on `bodies`, so it stays with the headers it belongs
            // to. It is read only when a message is opened.
            //
            // Guarded, because `ALTER TABLE ADD COLUMN` has no `IF NOT
            // EXISTS` and every other block here is safe to re-run. A database
            // whose `user_version` was wound back — a downgrade, a repair —
            // must migrate forward again rather than fail on the column it
            // already has.
            if !self.has_column("messages", "reply_to")? {
                self.conn
                    .execute("ALTER TABLE messages ADD COLUMN reply_to TEXT NOT NULL DEFAULT ''", [])?;
            }
            self.conn.execute_batch("PRAGMA user_version = 7;")?;
        }

        if version < 8 {
            // What the server said the folder holds, so the list can say how
            // much of it is here instead of leaving someone to wonder whether
            // 2,000 is all their mail.
            if !self.has_column("mailboxes", "server_total")? {
                self.conn.execute(
                    "ALTER TABLE mailboxes ADD COLUMN server_total INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            self.conn.execute_batch("PRAGMA user_version = 8;")?;
        }

        if version < 9 {
            // Which message a draft is passing on, so a forward saved today
            // and sent tomorrow still marks its original forwarded. Its own
            // column beside `replying_to` rather than a flag on that one: a
            // forward carries no threading headers, so there is nothing else
            // on the row that remembers where it came from.
            if !self.has_column("drafts", "forwarding")? {
                self.conn.execute("ALTER TABLE drafts ADD COLUMN forwarding INTEGER", [])?;
            }
            self.conn.execute_batch("PRAGMA user_version = 9;")?;
        }

        if version < 10 {
            // The number that says whether this folder's UIDs still mean what
            // they meant last time. Without it, a server that reissues them --
            // a restore from backup, a mailbox recreated under the same name
            // -- leaves every stored UID pointing at a different message, and
            // the expunge detection in `remove_expunged` would act on it.
            if !self.has_column("mailboxes", "uid_validity")? {
                self.conn.execute("ALTER TABLE mailboxes ADD COLUMN uid_validity INTEGER", [])?;
            }
            self.conn.execute_batch("PRAGMA user_version = 10;")?;
        }

        if version < 11 {
            // Files travelling with an unsent draft.
            //
            // The bytes live here rather than a path on disk being remembered,
            // because a draft outlives the state of the filesystem around it:
            // saved on Monday and sent on Friday, by which time the file may
            // have moved, been deleted, or been edited into something the
            // sender never meant to send. It is also the only representation
            // that suits both sources — a file picked from disk and a part
            // carried along from a forward arrive the same way.
            //
            // `idx` is the position in the message, so attachments go out in
            // the order they were added rather than in whatever order the rows
            // came back.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS draft_attachments (
                     draft_id INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
                     idx      INTEGER NOT NULL,
                     name     TEXT NOT NULL,
                     mime     TEXT NOT NULL,
                     bytes    BLOB NOT NULL,
                     PRIMARY KEY (draft_id, idx)
                 ) WITHOUT ROWID;

                 PRAGMA user_version = 11;",
            )?;
        }

        if version < 12 {
            // Two indexes, both of them about start-up rather than about any
            // query the reader types.
            //
            // `idx_messages_threading` replaces `idx_messages_rfc_id`. It
            // leads on the same column, so every lookup the narrow one served
            // it still serves, and it carries the other two columns seeding
            // the threader wants — which turns that seed from a walk of the
            // whole `messages` table into an index-only scan roughly a fifth
            // the size. On a 220,000-message store that is the difference
            // between two seconds of cold disk and a few hundred milliseconds.
            //
            // `idx_messages_mailbox_flags` is for the sidebar's counts. That
            // query groups by mailbox and sums a flag, and the list index next
            // to it holds `date_ts` rather than `flags` — so counting meant
            // fetching every one of those rows from the table, at random, to
            // read one integer out of it. With flags in the key it never
            // touches the table at all.
            //
            // Building both is a one-off cost on the first launch after this
            // change, and proportional to the mailbox: a large one will take a
            // few seconds once, and start quickly every time after.
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_threading
                     ON messages(rfc_message_id, norm_subject, thread_id);
                 DROP INDEX IF EXISTS idx_messages_rfc_id;

                 CREATE INDEX IF NOT EXISTS idx_messages_mailbox_flags
                     ON messages(mailbox_id, flags);

                 PRAGMA user_version = 12;",
            )?;
        }

        if version < 13 {
            // `List-Unsubscribe` and whether it offers one-click, for the
            // reader's unsubscribe button. On `messages` beside the other
            // headers, read only when a message is opened.
            //
            // `list_unsub` has no default on purpose: every row that exists
            // now reads back `NULL`, which is "never looked", and that is what
            // tells the reader to go and ask the server about old mail rather
            // than conclude it is not from a list. Adding a nullable column
            // with no default is a change to the schema only, so it costs
            // nothing on a large store.
            if !self.has_column("messages", "list_unsub")? {
                self.conn.execute("ALTER TABLE messages ADD COLUMN list_unsub TEXT", [])?;
            }
            if !self.has_column("messages", "list_unsub_post")? {
                self.conn.execute(
                    "ALTER TABLE messages ADD COLUMN list_unsub_post INTEGER NOT NULL DEFAULT 0",
                    [],
                )?;
            }
            self.conn.execute_batch("PRAGMA user_version = 13;")?;
        }

        if version < 14 {
            // Where extensions keep what they know.
            //
            // In the database rather than a file per extension because of what
            // goes in it: a classifier writes one row per message it has
            // looked at, and a quarter of a million of those is a table, not a
            // directory. The extension's id is a column rather than part of
            // the key so that uninstalling one is a `DELETE ... WHERE`.
            //
            // The host, not the extension, supplies the `extension` value on
            // every call — which is what makes the namespace a fact rather
            // than a convention an extension could decline to follow.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS extension_state (
                     extension TEXT NOT NULL,
                     key       TEXT NOT NULL,
                     value     TEXT NOT NULL,
                     PRIMARY KEY (extension, key)
                 ) WITHOUT ROWID;

                 PRAGMA user_version = 14;",
            )?;
        }

        if version < 15 {
            // How a draft is sent, and the HTML of what it answers. Both
            // default to what every draft before them was: plain text, with
            // nothing but the text quote.
            if !self.has_column("drafts", "format")? {
                self.conn.execute(
                    "ALTER TABLE drafts ADD COLUMN format TEXT NOT NULL DEFAULT 'plain'",
                    [],
                )?;
            }
            if !self.has_column("drafts", "quoted_html")? {
                self.conn.execute("ALTER TABLE drafts ADD COLUMN quoted_html TEXT", [])?;
            }
            self.conn.execute_batch("PRAGMA user_version = 15;")?;
        }

        if version < 16 {
            // The body as edited in the HTML editor, when it was.
            if !self.has_column("drafts", "html")? {
                self.conn.execute("ALTER TABLE drafts ADD COLUMN html TEXT", [])?;
            }
            self.conn.execute_batch("PRAGMA user_version = 16;")?;
        }

        if version < 17 {
            // The rows `ingest` looks for before every insert: a message this
            // client filed itself, whose UID is not known yet. There are a
            // handful at most, but with nothing indexing them the look-up read
            // the whole folder — once per incoming message, so a first sync
            // slowed as the folder filled. Measured on an M1 Ultra against a
            // 250,000-message account: ten minutes had synced 165,000 and the
            // rate was still falling; with the index the whole account took
            // 39 seconds.
            self.conn.execute_batch(
                "CREATE INDEX IF NOT EXISTS idx_messages_unfiled
                     ON messages(mailbox_id, rfc_message_id) WHERE uid IS NULL;
                 PRAGMA user_version = 17;",
            )?;
        }

        if version < 18 {
            // Who is on each message, for the client book. See `people`.
            //
            // One row per person per message, keyed so that a person's mail is
            // a single range in date order: opening someone's history is a seek
            // and a walk, with nothing decoded and nothing sorted. Without
            // rowids, because the key is the whole row and a rowid would be a
            // second copy of it.
            //
            // The second index is for the cascade. Deleting a message has to
            // find its rows here, and with nothing leading on `message_id`
            // every delete would scan the table.
            //
            // Existing mail is not indexed here. On a quarter of a million
            // messages that is a pass over every address column in the store,
            // and a migration runs before the first frame — so the tables are
            // created empty, the first message id *not* to backfill is noted,
            // and `Store::backfill_people` does the rest in the background.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS correspondents (
                     id   INTEGER PRIMARY KEY,
                     addr TEXT NOT NULL UNIQUE,
                     name TEXT NOT NULL DEFAULT ''
                 );

                 CREATE TABLE IF NOT EXISTS message_people (
                     person     INTEGER NOT NULL,
                     date_ts    INTEGER NOT NULL,
                     message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
                     account_id INTEGER NOT NULL,
                     direction  INTEGER NOT NULL,
                     PRIMARY KEY (person, date_ts, message_id)
                 ) WITHOUT ROWID;

                 CREATE INDEX IF NOT EXISTS idx_message_people_message
                     ON message_people(message_id);

                 CREATE TABLE IF NOT EXISTS store_meta (
                     key   TEXT PRIMARY KEY,
                     value INTEGER NOT NULL
                 ) WITHOUT ROWID;

                 INSERT OR IGNORE INTO store_meta (key, value)
                     VALUES ('people.backfill_below', (SELECT COALESCE(MAX(id), 0) + 1 FROM messages));

                 PRAGMA user_version = 18;",
            )?;
        }

        // The FTS index is created separately: contentless-delete needs a
        // recent SQLite, and an old one should cost search, not startup.
        let fts = self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                 subject, sender, body,
                 content='', contentless_delete=1,
                 tokenize='unicode61 remove_diacritics 2'
             );",
        );
        if let Err(e) = fts {
            log::warn!("contentless-delete FTS unavailable ({e}); falling back");
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
                     subject, sender, body, content=''
                 );",
            )?;
        }

        Ok(())
    }

    /// Whether a table already has a column, for the one kind of migration
    /// SQLite gives no `IF NOT EXISTS` for.
    fn has_column(&self, table: &str, column: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut names = stmt.query_map([], |row| row.get::<_, String>(1))?;
        Ok(names.any(|name| name.as_deref() == Ok(column)))
    }

    /// The threader, built from what is already stored the first time anything
    /// asks for it, so conversations survive a restart.
    ///
    /// The scan behind this is the most expensive thing the store does, and it
    /// is why it happens here rather than in `open` — see [`Inner::threader`].
    /// `idx_messages_threading` makes it index-only: the three columns wanted
    /// are all in the key, so it reads about a fifth of what walking the table
    /// would.
    fn threader(&mut self) -> Result<&mut Threader> {
        if self.threader.is_none() {
            let started = std::time::Instant::now();
            let mut threader = Threader::new();
            let mut stmt = self.conn.prepare(
                "SELECT rfc_message_id, norm_subject, thread_id FROM messages",
            )?;
            let mut rows = stmt.query([])?;

            let mut seeded = 0u64;
            while let Some(row) = rows.next()? {
                let message_id: String = row.get(0)?;
                let subject: String = row.get(1)?;
                let thread = ThreadId(row.get(2)?);
                threader.seed(&message_id, thread);
                threader.seed_subject(&subject, thread);
                seeded += 1;
            }
            self.threader = Some(threader);

            // The only expensive thing left in the store that a person can
            // wait on without a spinner to explain it. Logged so that "the
            // first sync of a session is slow" is a sentence with a number
            // attached rather than a hunch.
            log::debug!("threading seeded from {seeded} messages in {:?}", started.elapsed());
        }

        Ok(self.threader.as_mut().expect("seeded just above"))
    }
}

/// Whether two IMAP folder paths name the same folder.
///
/// Byte equality, because paths are case-sensitive and a server hands the same
/// folder back spelled the same way every time — matching loosely would keep
/// exactly the stale row this is here to remove. `INBOX` is the one name the
/// standard declares case-insensitive, and servers do vary on it.
fn same_folder(a: &str, b: &str) -> bool {
    a == b || (a.eq_ignore_ascii_case("INBOX") && b.eq_ignore_ascii_case("INBOX"))
}

#[cfg(test)]
mod tests {
    use spool_core::unix_from_civil;

    use super::*;

    #[test]
    fn a_draft_survives_reopening_the_database() {
        let dir = std::env::temp_dir().join(format!("spool-draft-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("drafts.db");
        let _ = std::fs::remove_file(&path);

        let draft = Draft {
            from: Address::new("Me", "me@example.com"),
            // A name with a comma is the case that silently corrupted the
            // recipient list before addresses learned to quote themselves.
            to: vec![Address::new("Ruiz, Ana", "ana@example.com")],
            cc: vec![Address::new("", "cc@example.com")],
            bcc: vec![Address::new("", "bcc@example.com")],
            subject: "Half a thought".into(),
            body: "and the other half\nis still coming".into(),
            in_reply_to: Some("abc@example.com".into()),
            references: vec!["one@example.com".into(), "two@example.com".into()],
            replying_to: Some(MessageId(7)),
            forwarding: Some(MessageId(9)),
            // Sent as HTML, with the original's formatting to go under it —
            // both of which a reopened draft has to remember.
            format: spool_core::draft::Format::Html,
            quoted_html: Some("<div>On Monday, Ana wrote:</div><blockquote><p>Hi</p></blockquote>".into()),
            html: Some("<div id=\"spool-writing\"><p><b>Yes</b></p></div>".into()),
            attachments: vec![
                Attached::new("q3.pdf", vec![1, 2, 3, 4, 5]),
                // Bytes that are not text, because a BLOB column that
                // round-trips a string proves nothing about a PDF.
                Attached::new("shot.png", (0u8..=255).collect()),
            ],
        };

        let id = {
            let store = Store::open(&path).unwrap();
            store.save_draft(None, &draft, 1_700_000_000, DraftFiles::Replace).unwrap()
        };

        let store = Store::open(&path).unwrap();
        let restored = store.drafts().unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].0, id);
        assert_eq!(restored[0].1, draft);

        // Named explicitly as well, so a failure says which half broke.
        let files = &restored[0].1.attachments;
        assert_eq!(files.len(), 2, "both files came back, in order");
        assert_eq!(files[0].name, "q3.pdf");
        assert_eq!(files[0].mime, "application/pdf");
        assert_eq!(files[1].bytes, (0u8..=255).collect::<Vec<u8>>());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_autosave_leaves_the_files_alone() {
        // The reason `DraftFiles` exists. An autosave fires every few seconds
        // while somebody types; rewriting the attachments each time would mean
        // writing however many megabytes they weigh in response to a keystroke.
        let store = Store::in_memory().unwrap();
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        draft.attachments = vec![Attached::new("q3.pdf", vec![7; 64])];

        let id = store.save_draft(None, &draft, 1, DraftFiles::Replace).unwrap();

        // Typing changes the body and nothing else.
        draft.body = "another sentence".into();
        draft.attachments.clear();
        store.save_draft(Some(id), &draft, 2, DraftFiles::Unchanged).unwrap();

        let restored = &store.drafts().unwrap()[0].1;
        assert_eq!(restored.body, "another sentence", "the text was written");
        assert_eq!(restored.attachments.len(), 1, "the files were not touched");
        assert_eq!(restored.attachments[0].bytes, vec![7; 64]);
    }

    #[test]
    fn removing_a_file_is_written_when_the_caller_says_so() {
        let store = Store::in_memory().unwrap();
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        draft.attachments = vec![
            Attached::new("one.pdf", vec![1; 8]),
            Attached::new("two.pdf", vec![2; 8]),
        ];

        let id = store.save_draft(None, &draft, 1, DraftFiles::Replace).unwrap();

        draft.attachments.remove(0);
        store.save_draft(Some(id), &draft, 2, DraftFiles::Replace).unwrap();

        let restored = &store.drafts().unwrap()[0].1;
        assert_eq!(restored.attachments.len(), 1);
        assert_eq!(restored.attachments[0].name, "two.pdf", "and the right one survived");
    }

    #[test]
    fn a_draft_thrown_away_takes_its_files_with_it() {
        // `ON DELETE CASCADE`, which is the only thing stopping a discarded
        // draft's attachments sitting in the database forever.
        let store = Store::in_memory().unwrap();
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        draft.attachments = vec![Attached::new("big.bin", vec![0; 1024])];

        let id = store.save_draft(None, &draft, 1, DraftFiles::Replace).unwrap();
        store.delete_draft(id).unwrap();

        let orphans: i64 = {
            let this = store.lock();
            this.conn
                .query_row("SELECT COUNT(*) FROM draft_attachments", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(orphans, 0, "the files went with the draft");
    }

    #[test]
    fn saving_again_replaces_the_draft_rather_than_piling_up_copies() {
        let store = Store::in_memory().unwrap();
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        draft.subject = "First".into();

        let id = store.save_draft(None, &draft, 1, DraftFiles::Replace).unwrap();
        draft.subject = "Second".into();
        store.save_draft(Some(id), &draft, 2, DraftFiles::Replace).unwrap();

        let drafts = store.drafts().unwrap();
        assert_eq!(drafts.len(), 1, "an autosave every keystroke must not grow the table");
        assert_eq!(drafts[0].1.subject, "Second");

        store.delete_draft(id).unwrap();
        assert!(store.drafts().unwrap().is_empty());
    }

    #[test]
    fn drafts_come_back_most_recently_touched_first() {
        let store = Store::in_memory().unwrap();
        let mut draft = Draft::new(Address::new("Me", "me@example.com"));
        draft.subject = "Older".into();
        store.save_draft(None, &draft, 100, DraftFiles::Replace).unwrap();
        draft.subject = "Newer".into();
        store.save_draft(None, &draft, 200, DraftFiles::Replace).unwrap();

        let subjects: Vec<String> =
            store.drafts().unwrap().into_iter().map(|(_, d)| d.subject).collect();
        assert_eq!(subjects, ["Newer", "Older"]);
    }

    #[test]
    fn a_fresh_database_reports_the_current_schema_version() {
        // The version is read once and every block that applies sets it, so a
        // migration written out of order leaves a brand-new database claiming
        // an older schema than it has.
        let store = Store::in_memory().unwrap();
        let version: u32 = store
            .lock()
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn an_older_database_migrates_forward_without_losing_its_mail() {
        let dir = std::env::temp_dir().join(format!("spool-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);

        {
            let store = Store::open(&path).unwrap();
            let account = store.add_account("Test", "me@example.com", 0).unwrap();
            let mailbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
            store.ingest(&[message(mailbox, account, 1, "Still here")]).unwrap();

            // Wind it back to a database written before drafts existed.
            let this = store.lock();
            this.conn.execute_batch("DROP TABLE drafts; PRAGMA user_version = 2;").unwrap();
        }

        let store = Store::open(&path).unwrap();
        let version: u32 =
            store.lock().conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let mail = store.envelopes(Scope::Everything, &Query::All, 10).unwrap();
        assert_eq!(mail.len(), 1, "migrating must not disturb the mail already there");

        // A column added by a later migration reads as absent on mail that
        // predates it, rather than failing the query the reader makes.
        let body = store.body(mail[0].id).unwrap().expect("its body survived too");
        assert!(body.reply_to.is_empty());
        assert_eq!(body.list_unsubscribe, ListUnsubscribe::Unknown);

        let draft = Draft::new(Address::new("Me", "me@example.com"));
        store.save_draft(None, &draft, 1, DraftFiles::Replace).expect("the new table exists after the upgrade");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn list_unsubscribe_headers_come_back_with_the_body() {
        let (store, inbox, account) = store_with_mailbox();
        let offered = ListUnsubscribe::Offered {
            header: "<https://lists.example/u/1>".into(),
            one_click: true,
        };

        let mut from_a_list = message(inbox, account, 1, "Newsletter");
        from_a_list.list_unsubscribe = offered.clone();
        let mut not_from_one = message(inbox, account, 2, "Hello");
        not_from_one.list_unsubscribe = ListUnsubscribe::Absent;
        let never_read = message(inbox, account, 3, "Old");
        store.ingest(&[from_a_list, not_from_one, never_read]).unwrap();

        let body = |subject: &str| {
            let mail = store.envelopes(Scope::Everything, &Query::All, 10).unwrap();
            let id = mail.iter().find(|e| e.subject == subject).unwrap().id;
            store.body(id).unwrap().unwrap().list_unsubscribe
        };
        assert_eq!(body("Newsletter"), offered);
        assert_eq!(body("Hello"), ListUnsubscribe::Absent);
        assert_eq!(body("Old"), ListUnsubscribe::Unknown, "not read is not the same as none");
    }

    #[test]
    fn a_resync_fills_in_list_headers_but_never_overwrites_them() {
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 1, "Old")]).unwrap();
        let id = store.envelopes(Scope::Everything, &Query::All, 10).unwrap()[0].id;

        let offered = ListUnsubscribe::Offered { header: "<mailto:u@l.example>".into(), one_click: false };
        let mut again = message(inbox, account, 1, "Old");
        again.list_unsubscribe = offered.clone();
        store.ingest(&[again]).unwrap();
        assert_eq!(store.body(id).unwrap().unwrap().list_unsubscribe, offered);

        // A later sync that did not read them leaves what is known alone.
        store.ingest(&[message(inbox, account, 1, "Old")]).unwrap();
        assert_eq!(store.body(id).unwrap().unwrap().list_unsubscribe, offered);
    }

    #[test]
    fn list_headers_fetched_later_are_recorded() {
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 1, "Old")]).unwrap();
        let id = store.envelopes(Scope::Everything, &Query::All, 10).unwrap()[0].id;

        store.set_list_unsubscribe(id, &ListUnsubscribe::Unknown).unwrap();
        assert_eq!(store.body(id).unwrap().unwrap().list_unsubscribe, ListUnsubscribe::Unknown);

        store.set_list_unsubscribe(id, &ListUnsubscribe::Absent).unwrap();
        assert_eq!(store.body(id).unwrap().unwrap().list_unsubscribe, ListUnsubscribe::Absent);
    }

    fn store_with_mailbox() -> (Store, MailboxId, AccountId) {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        let mailbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        (store, mailbox, account)
    }

    fn message(mailbox: MailboxId, account: AccountId, uid: u32, subject: &str) -> IncomingMessage {
        IncomingMessage {
            account_id: account,
            mailbox_id: mailbox,
            uid: Some(uid),
            rfc_message_id: format!("<{uid}@example.com>"),
            from: Address::new("Ana Ruiz", "ana@example.com"),
            subject: subject.into(),
            date: 1_700_000_000 + uid as i64,
            plain: format!("Body of {subject} with a distinctive keyword: pomegranate"),
            size: 1024,
            ..Default::default()
        }
    }

    /// A mailbox with enough variety for the search operators to bite on.
    fn store_with_a_mixed_mailbox() -> (Store, MailboxId, MailboxId) {
        let (store, inbox, account) = store_with_mailbox();
        let archive = store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();

        let mut from_ana = message(inbox, account, 1, "Quarterly report");
        from_ana.to = vec![Address::new("Bo Chen", "bo@example.com")];
        from_ana.date = unix_from_civil(2026, 1, 15);
        from_ana.attachments = vec![Attachment {
            name: "report.pdf".into(),
            mime: "application/pdf".into(),
            size: 1024,
            content_id: None,
            inline: false,
        }];

        let mut from_bo = message(inbox, account, 2, "Roof leak");
        from_bo.from = Address::new("Bo Chen", "bo@example.com");
        from_bo.to = vec![Address::new("Ana Ruiz", "ana@example.com")];
        from_bo.date = unix_from_civil(2026, 3, 20);
        from_bo.flags = Flags::default().with(Flags::SEEN, true).with(Flags::FLAGGED, true);

        let mut filed = message(archive, account, 3, "Old news");
        filed.date = unix_from_civil(2025, 6, 1);

        store.ingest(&[from_ana, from_bo, filed]).unwrap();
        (store, inbox, archive)
    }

    #[test]
    fn search_operators_narrow_by_sender_recipient_and_subject() {
        let (store, ..) = store_with_a_mixed_mailbox();
        let subjects = |q: &str| {
            store
                .envelopes(Scope::Everything, &Query::Text(q.into()), 100)
                .unwrap()
                .into_iter()
                .map(|e| e.subject)
                .collect::<Vec<_>>()
        };

        assert_eq!(subjects("from:bo"), ["Roof leak"]);
        assert_eq!(subjects("from:\"Ana Ruiz\""), ["Quarterly report", "Old news"]);
        assert_eq!(subjects("to:bo@example.com"), ["Quarterly report"]);
        assert_eq!(subjects("subject:roof"), ["Roof leak"]);
        // An operator and free text together: the sender narrows, the word
        // searches what is left.
        assert_eq!(subjects("from:ana pomegranate"), ["Quarterly report", "Old news"]);
        assert_eq!(subjects("from:ana watermelon"), Vec::<String>::new());
    }

    #[test]
    fn search_operators_narrow_by_state_folder_and_attachment() {
        let (store, _inbox, _archive) = store_with_a_mixed_mailbox();
        let subjects = |q: &str| {
            store
                .envelopes(Scope::Everything, &Query::Text(q.into()), 100)
                .unwrap()
                .into_iter()
                .map(|e| e.subject)
                .collect::<Vec<_>>()
        };

        assert_eq!(subjects("is:unread"), ["Quarterly report", "Old news"]);
        assert_eq!(subjects("is:read"), ["Roof leak"]);
        assert_eq!(subjects("is:flagged"), ["Roof leak"]);
        assert_eq!(subjects("has:attachment"), ["Quarterly report"]);
        assert_eq!(subjects("in:Archive"), ["Old news"]);
        // Operators stack, and an impossible combination is empty rather than
        // an error.
        assert_eq!(subjects("is:read has:attachment"), Vec::<String>::new());
    }

    #[test]
    fn search_operators_narrow_by_date() {
        let (store, ..) = store_with_a_mixed_mailbox();
        let subjects = |q: &str| {
            store
                .envelopes(Scope::Everything, &Query::Text(q.into()), 100)
                .unwrap()
                .into_iter()
                .map(|e| e.subject)
                .collect::<Vec<_>>()
        };

        assert_eq!(subjects("after:2026-01-01"), ["Roof leak", "Quarterly report"]);
        assert_eq!(subjects("before:2026-01-01"), ["Old news"]);
        // `after:` includes the day named; `before:` stops at its midnight.
        assert_eq!(subjects("after:2026-01-15 before:2026-01-16"), ["Quarterly report"]);
    }

    #[test]
    fn renaming_a_mailbox_carries_its_children_with_it() {
        let (store, _inbox, account) = store_with_mailbox();
        let work = store.upsert_mailbox(account, "Work", MailboxRole::Other).unwrap();
        store.upsert_mailbox(account, "Work/Invoices", MailboxRole::Other).unwrap();
        store.upsert_mailbox(account, "Work/Invoices/2026", MailboxRole::Other).unwrap();
        // A sibling that merely starts with the same letters must not move.
        store.upsert_mailbox(account, "Workshop", MailboxRole::Other).unwrap();

        let moved = store.rename_mailbox(work, "Clients", "/").unwrap();
        assert_eq!(moved, 3);

        let paths: Vec<String> = store.mailboxes().unwrap().into_iter().map(|m| m.path).collect();
        assert!(paths.contains(&"Clients".to_string()));
        assert!(paths.contains(&"Clients/Invoices".to_string()));
        assert!(paths.contains(&"Clients/Invoices/2026".to_string()));
        assert!(paths.contains(&"Workshop".to_string()), "a sibling was dragged along");
    }

    #[test]
    fn a_renamed_mailbox_takes_its_display_name_from_the_delimiter() {
        let (store, _inbox, account) = store_with_mailbox();
        let folder = store.upsert_mailbox(account, "Work", MailboxRole::Other).unwrap();

        // Dovecot's default layout, where `.` separates rather than `/`.
        store.rename_mailbox(folder, "INBOX.Clients", ".").unwrap();
        let renamed =
            store.mailboxes().unwrap().into_iter().find(|m| m.id == folder).unwrap();
        assert_eq!(renamed.name, "Clients");
        assert_eq!(renamed.path, "INBOX.Clients");
    }

    #[test]
    fn deleting_a_mailbox_takes_its_mail_out_of_search_too() {
        let (store, inbox, account) = store_with_mailbox();
        let scratch = store.upsert_mailbox(account, "Scratch", MailboxRole::Other).unwrap();
        store.ingest(&[message(inbox, account, 1, "Kept")]).unwrap();
        store.ingest(&[message(scratch, account, 2, "Going")]).unwrap();

        let removed = store.delete_mailbox(scratch).unwrap();
        assert_eq!(removed, 1);

        assert!(store.mailboxes().unwrap().iter().all(|m| m.id != scratch));
        let all = store.envelopes(Scope::Everything, &Query::All, 100).unwrap();
        assert_eq!(all.len(), 1);
        // The FTS index is contentless and has no foreign keys, so this is the
        // half that a cascade cannot do.
        let hits = store
            .envelopes(Scope::Everything, &Query::Text("pomegranate".into()), 100)
            .unwrap();
        assert_eq!(hits.len(), 1, "the deleted folder's mail is still in the index");
    }

    #[test]
    fn a_folder_the_server_stopped_listing_is_dropped_with_its_mail() {
        let (store, inbox, account) = store_with_mailbox();
        let moved = store.upsert_mailbox(account, "ccc", MailboxRole::Other).unwrap();
        store.ingest(&[message(inbox, account, 1, "Kept")]).unwrap();
        store.ingest(&[message(moved, account, 2, "Moved")]).unwrap();

        // What the server says after the folder was moved under a parent in
        // another client: the old path is simply not there any more.
        let listed = vec!["INBOX".to_string(), "Lists/ccc".to_string()];
        store.upsert_mailbox(account, "Lists/ccc", MailboxRole::Other).unwrap();

        let removed = store.retain_mailboxes(account, &listed).unwrap();
        assert_eq!(removed, vec!["ccc".to_string()]);

        let paths: Vec<String> =
            store.mailboxes().unwrap().into_iter().map(|m| m.path).collect();
        assert_eq!(paths.iter().filter(|p| p.ends_with("ccc")).count(), 1, "{paths:?}");

        // The mail under the old path goes with it — it is still on the
        // server under the new one, and a second copy locally is the bug.
        let all = store.envelopes(Scope::Everything, &Query::All, 100).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].subject, "Kept");
    }

    #[test]
    fn reconciling_one_account_leaves_the_others_and_the_inbox_alone() {
        let (store, inbox, account) = store_with_mailbox();
        let other = store.add_account("Other", "other@example.com", 1).unwrap();
        let other_folder = store.upsert_mailbox(other, "Work", MailboxRole::Other).unwrap();
        store.ingest(&[message(inbox, account, 1, "Kept")]).unwrap();
        store.ingest(&[message(other_folder, other, 2, "Also kept")]).unwrap();

        // `INBOX` is the one folder name the standard makes case-insensitive,
        // and servers do vary on how they spell it back.
        let removed = store.retain_mailboxes(account, &["inbox".to_string()]).unwrap();
        assert!(removed.is_empty(), "the inbox was dropped over its casing: {removed:?}");

        // Another account's folders are not in this account's listing, and
        // must not be read as missing.
        assert_eq!(store.mailboxes().unwrap().len(), 2);
        assert_eq!(store.envelopes(Scope::Everything, &Query::All, 100).unwrap().len(), 2);
    }

    #[test]
    fn deleting_an_account_takes_its_folders_mail_and_search_entries() {
        let (store, inbox, account) = store_with_mailbox();
        let other = store.add_account("Other", "other@example.com", 1).unwrap();
        let other_inbox = store.upsert_mailbox(other, "INBOX", MailboxRole::Inbox).unwrap();

        store.ingest(&[message(inbox, account, 1, "Going")]).unwrap();
        store.ingest(&[message(other_inbox, other, 2, "Staying")]).unwrap();

        let removed = store.delete_account(account).unwrap();
        assert_eq!(removed, 1);

        // The account and its folders are gone from the sidebar's sources.
        assert!(store.accounts().unwrap().iter().all(|a| a.id != account));
        assert!(store.mailboxes().unwrap().iter().all(|m| m.account_id != account));

        // The other account is untouched.
        let left = store.envelopes(Scope::Everything, &Query::All, 100).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].subject, "Staying");

        // And the removed account's mail is out of the index, not just the
        // table — the half a cascade cannot do.
        let hits = store
            .envelopes(Scope::Everything, &Query::Text("pomegranate".into()), 100)
            .unwrap();
        assert_eq!(hits.len(), 1, "the removed account's mail is still searchable");
    }

    /// Three messages in one thread and one on its own, newest last.
    fn store_with_a_conversation() -> (Store, MailboxId, AccountId) {
        let (store, inbox, account) = store_with_mailbox();

        let reply = |uid: u32, from: &str, at: i64| {
            let mut msg = message(inbox, account, uid, "Re: The proposal");
            msg.from = Address::new(from, &format!("{}@example.com", from.to_lowercase()));
            msg.rfc_message_id = format!("<{uid}@example.com>");
            msg.in_reply_to = "1@example.com".into();
            msg.references = vec!["1@example.com".into()];
            msg.date = at;
            msg
        };

        let mut root = message(inbox, account, 1, "The proposal");
        root.from = Address::new("Ana Ruiz", "ana@example.com");
        root.rfc_message_id = "1@example.com".into();
        root.date = 100;
        root.flags = Flags(Flags::SEEN);

        let mut other = message(inbox, account, 9, "Unrelated");
        other.rfc_message_id = "<9@example.com>".into();
        other.date = 150;

        store
            .ingest(&[root, reply(2, "Bo", 200), reply(3, "Cass", 300), other])
            .unwrap();
        (store, inbox, account)
    }

    #[test]
    fn one_sender_is_one_participant_however_their_name_was_quoted() {
        // A quoted display name and a bare one are the same person, and
        // listing both produced rows reading `'Spotify', Spotify`.
        assert_eq!(display_name(&Address::new("'Spotify'", "no-reply@spotify.com")), "Spotify");
        // The real one: a Google Groups relay, where the closing quote is in
        // the middle of the header rather than at its end.
        assert_eq!(
            display_name(&Address::new("'Spotify' via wes@example.com", "wes@example.com")),
            "Spotify"
        );
        assert_eq!(display_name(&Address::new("Ruiz, Ana", "ana@example.com")), "Ruiz");
        assert_eq!(display_name(&Address::new("\"Ana Ruiz\"", "ana@example.com")), "Ana");
        assert_eq!(
            display_name(&Address::new("", "ops@example.com")),
            "ops",
            "no name at all falls back to the local part"
        );
    }

    #[test]
    fn the_server_can_correct_flags_on_mail_already_here() {
        // The bug this exists for: a message read on another device stays
        // unread here for ever, because sync only ever asks about UIDs above
        // the highest one it has.
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 7, "Read elsewhere")]).unwrap();

        let before = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap();
        assert!(!before[0].flags.is_seen());

        let changed = store
            .apply_server_flags(inbox, "INBOX", &[(7, Flags(Flags::SEEN))])
            .unwrap();
        assert_eq!(changed, 1);

        let after = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap();
        assert!(after[0].flags.is_seen());
    }

    #[test]
    fn flags_that_already_agree_are_not_counted_as_changes() {
        // A sync that reports "reconciled 2,000 messages" every time it runs
        // is a sync nobody will believe when it matters.
        let (store, inbox, account) = store_with_mailbox();
        let mut msg = message(inbox, account, 7, "Already read");
        msg.flags = Flags(Flags::SEEN);
        store.ingest(&[msg]).unwrap();

        assert_eq!(
            store.apply_server_flags(inbox, "INBOX", &[(7, Flags(Flags::SEEN))]).unwrap(),
            0
        );
    }

    #[test]
    fn a_local_change_still_waiting_to_be_sent_beats_the_server() {
        // The server's answer predates the change: applying it would undo
        // what was just done, and the next drain would then do it again.
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 7, "Just flagged")]).unwrap();

        let id = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap()[0].id;
        store.set_flags(id, Flags(Flags::FLAGGED)).unwrap();

        let changed = store
            .apply_server_flags(inbox, "INBOX", &[(7, Flags::default())])
            .unwrap();
        assert_eq!(changed, 0, "the queued change is the newer truth");

        let after = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap();
        assert!(after[0].flags.is_flagged());
    }

    #[test]
    fn an_accounts_colour_can_be_changed() {
        let store = Store::in_memory().unwrap();
        store.add_account("Me", "me@example.com", 2).unwrap();
        assert_eq!(store.accounts().unwrap()[0].color, 2);

        // The config file is the authority, and a sync carries it across.
        store.add_account("Me", "me@example.com", 5).unwrap();
        assert_eq!(store.accounts().unwrap()[0].color, 5, "an upsert recolours too");

        // And saving the settings does not wait for one.
        store.set_account_color("me@example.com", 7).unwrap();
        assert_eq!(store.accounts().unwrap()[0].color, 7);
    }

    #[test]
    fn mail_expunged_somewhere_else_stops_being_here() {
        let (store, inbox, account) = store_with_mailbox();
        store
            .ingest(&[
                message(inbox, account, 10, "Still there"),
                message(inbox, account, 11, "Deleted on the phone"),
                message(inbox, account, 12, "Also still there"),
            ])
            .unwrap();

        // The server answered the range 10:12 with two of the three.
        let removed = store.remove_expunged(inbox, "INBOX", 10, 12, &[10, 12]).unwrap();
        assert_eq!(removed, 1);

        let left: Vec<String> = store
            .envelopes(Scope::Mailbox(inbox), &Query::All, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.subject)
            .collect();
        assert_eq!(left.len(), 2);
        assert!(!left.iter().any(|s| s == "Deleted on the phone"));
    }

    #[test]
    fn mail_the_server_was_not_asked_about_is_left_alone() {
        let (store, inbox, account) = store_with_mailbox();
        store
            .ingest(&[
                message(inbox, account, 10, "Asked about"),
                message(inbox, account, 40, "Arrived after the question"),
            ])
            .unwrap();

        // The fetch covered 10:20 only. UID 40 was never ruled on, and
        // treating silence about it as a deletion would delete new mail.
        let removed = store.remove_expunged(inbox, "INBOX", 10, 20, &[10]).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(store.envelopes(Scope::Mailbox(inbox), &Query::All, 10).unwrap().len(), 2);
    }

    #[test]
    fn a_local_change_not_yet_pushed_is_not_an_expunge() {
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 10, "Flagged here a moment ago")]).unwrap();

        // `set_flags` queues the change, which is what marks this row as one
        // the server has not been told about.
        let id = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap()[0].id;
        store.set_flags(id, Flags(Flags::FLAGGED)).unwrap();

        let removed = store.remove_expunged(inbox, "INBOX", 10, 10, &[]).unwrap();
        assert_eq!(removed, 0, "the queued change is the newer truth");
    }

    #[test]
    fn a_folder_the_server_says_is_empty_is_emptied() {
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 10, "Gone"), message(inbox, account, 11, "Also")])
            .unwrap();

        assert_eq!(store.clear_mailbox(inbox, "INBOX").unwrap(), 2);
        assert!(store.envelopes(Scope::Mailbox(inbox), &Query::All, 10).unwrap().is_empty());
    }

    #[test]
    fn a_renumbered_folder_starts_again_rather_than_reconciling_against_lies() {
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 10, "Under the old numbering")]).unwrap();

        // First sight of the number is not a change: there is nothing it could
        // have invalidated, and every folder in a store written before the
        // column existed arrives here.
        assert!(!store.set_uid_validity(inbox, 1).unwrap());
        assert!(!store.set_uid_validity(inbox, 1).unwrap(), "and neither is seeing it again");
        assert_eq!(store.envelopes(Scope::Mailbox(inbox), &Query::All, 10).unwrap().len(), 1);

        // A different number means UID 10 is now a different message. Keeping
        // the row would hand `remove_expunged` a UID that refers to nothing.
        assert!(store.set_uid_validity(inbox, 2).unwrap());
        assert!(store.envelopes(Scope::Mailbox(inbox), &Query::All, 10).unwrap().is_empty());
    }

    #[test]
    fn a_sync_does_not_erase_the_flags_the_server_has_never_heard_of() {
        // `$Forwarded` is a keyword this client does not push, and TRIAGED is
        // not an IMAP flag at all. A sync overwrites a row's flags with the
        // server's view, so without `Flags::LOCAL_ONLY` these two would be
        // erased by the next refresh -- a message you forwarded this morning
        // would stop saying so this afternoon.
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 7, "Passed along")]).unwrap();

        let id = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap()[0].id;
        store.set_local_flags(id, Flags(Flags::FORWARDED | Flags::TRIAGED)).unwrap();

        // The server reports what it does know: this one has been read.
        store.apply_server_flags(inbox, "INBOX", &[(7, Flags(Flags::SEEN))]).unwrap();

        let after = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap();
        assert!(after[0].flags.is_seen(), "the server is the authority on \\Seen");
        assert!(after[0].flags.is_forwarded(), "and knows nothing about this one");
        assert!(after[0].flags.is_triaged());
    }

    #[test]
    fn the_server_still_wins_on_the_flags_it_owns() {
        let (store, inbox, account) = store_with_mailbox();
        store.ingest(&[message(inbox, account, 7, "Read elsewhere")]).unwrap();

        let id = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap()[0].id;
        store.set_local_flags(id, Flags(Flags::SEEN | Flags::FORWARDED)).unwrap();

        // Marked unread on another device, and nothing of ours is queued.
        let changed = store.apply_server_flags(inbox, "INBOX", &[(7, Flags::default())]).unwrap();
        assert_eq!(changed, 1);

        let after = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap();
        assert!(!after[0].flags.is_seen(), "unread there is unread here");
        assert!(after[0].flags.is_forwarded(), "which says nothing about the local bit");
    }

    #[test]
    fn a_flag_refresh_asks_only_about_the_range_it_holds() {
        let (store, inbox, account) = store_with_mailbox();
        assert_eq!(store.uid_range(inbox).unwrap(), None, "nothing here to ask about");

        store
            .ingest(&[
                message(inbox, account, 400, "Older"),
                message(inbox, account, 900, "Newer"),
            ])
            .unwrap();

        assert_eq!(store.uid_range(inbox).unwrap(), Some((400, 900)));
    }

    #[test]
    fn a_conversation_is_one_row_headed_by_its_newest_message() {
        let (store, inbox, _) = store_with_a_conversation();
        let rows = store.conversations(Scope::Mailbox(inbox), &Query::All, 20).unwrap();

        assert_eq!(rows.len(), 2, "one thread and one loner: {rows:#?}");

        let thread = &rows[0];
        assert_eq!(thread.message_count(), 3);
        assert_eq!(thread.from.name, "Cass", "the row is the newest message");
        assert_eq!(
            thread.group.as_ref().unwrap().participants,
            ["Ana", "Bo", "Cass"],
            "oldest first, first names only"
        );

        // The message on its own is a row with no group at all.
        assert_eq!(rows[1].subject, "Unrelated");
        assert!(rows[1].group.is_none());
        assert_eq!(rows[1].message_count(), 1);
    }

    #[test]
    fn a_row_acts_on_every_message_it_stands_for() {
        // What archiving a conversation depends on.
        let (store, inbox, _) = store_with_a_conversation();
        let rows = store.conversations(Scope::Mailbox(inbox), &Query::All, 20).unwrap();

        let ids = rows[0].message_ids();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&rows[0].id), "including the one the row shows");

        assert_eq!(rows[1].message_ids(), vec![rows[1].id], "and a loner acts on itself");
    }

    #[test]
    fn a_thread_is_unread_if_any_of_it_is() {
        // Not the same as the newest message being unread: a conversation
        // whose latest reply you have read can still hold one you have not.
        let (store, inbox, _) = store_with_a_conversation();
        let rows = store.conversations(Scope::Mailbox(inbox), &Query::All, 20).unwrap();

        assert_eq!(rows[0].group.as_ref().unwrap().unread, 2, "the root arrived read");
        assert!(rows[0].any_unread());
    }

    #[test]
    fn a_filtered_list_groups_only_what_matched() {
        // The row stands for the messages that matched, not for the whole
        // thread — otherwise a count next to a search result is a promise the
        // row cannot keep when you open it.
        let (store, inbox, _) = store_with_a_conversation();
        let rows = store.conversations(Scope::Mailbox(inbox), &Query::Unread, 20).unwrap();

        let thread = rows.iter().find(|r| r.subject.starts_with("Re:")).unwrap();
        assert_eq!(thread.message_count(), 2, "the read root is not in this list");
    }

    #[test]
    fn the_limit_counts_conversations_not_messages() {
        let (store, inbox, _) = store_with_a_conversation();
        let rows = store.conversations(Scope::Mailbox(inbox), &Query::All, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].message_count(), 3, "and the one it kept is still whole");
    }

    #[test]
    fn a_flagged_reply_flags_the_whole_row() {
        let (store, inbox, account) = store_with_mailbox();
        let mut root = message(inbox, account, 1, "The proposal");
        root.rfc_message_id = "1@example.com".into();
        root.date = 100;

        let mut reply = message(inbox, account, 2, "Re: The proposal");
        reply.rfc_message_id = "<2@example.com>".into();
        reply.in_reply_to = "1@example.com".into();
        reply.date = 50;
        reply.flags = Flags(Flags::FLAGGED);
        // Older than the root, so the row is the root and the flag has to be
        // carried up rather than read off the row's own message.
        store.ingest(&[root, reply]).unwrap();

        let rows = store.conversations(Scope::Mailbox(inbox), &Query::All, 20).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].flags.is_flagged(), "a conversation with a flagged reply is flagged");
    }

    #[test]
    fn a_limit_large_enough_to_overflow_its_own_arithmetic_is_still_a_limit() {
        // The list asks for 200,000 rows. Eight times that overflows nothing
        // on 64 bits, but the depth calculation must still come out as a
        // number `envelopes` will accept rather than panicking on the way.
        let (store, inbox, _) = store_with_a_conversation();
        let rows = store.conversations(Scope::Mailbox(inbox), &Query::All, usize::MAX).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn grouping_an_empty_mailbox_is_an_empty_list() {
        let (store, inbox, _) = store_with_mailbox();
        assert!(store.conversations(Scope::Mailbox(inbox), &Query::All, 20).unwrap().is_empty());
    }

    #[test]
    fn a_body_carries_what_a_reply_needs() {
        // These three ride with the body rather than the envelope: they are
        // wanted for exactly one message at a time — the open one — and the
        // list would pay for them on every row.
        let (store, inbox, account) = store_with_mailbox();

        let mut msg = message(inbox, account, 1, "A question");
        msg.rfc_message_id = "q@lists.example.org".into();
        msg.references = vec!["root@example.org".into(), "parent@example.org".into()];
        msg.reply_to = vec![Address::new("Rust Users", "users@lists.example.org")];
        store.ingest(&[msg]).unwrap();

        let id = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap()[0].id;
        let body = store.body(id).unwrap().expect("a body");

        assert_eq!(body.message_id, "q@lists.example.org");
        assert_eq!(body.references, ["root@example.org", "parent@example.org"]);
        assert_eq!(body.reply_to.len(), 1);
        assert_eq!(body.reply_to[0].addr, "users@lists.example.org");
        assert_eq!(body.reply_to[0].name, "Rust Users", "the display name survives too");
    }

    #[test]
    fn a_message_with_no_reply_headers_comes_back_empty_rather_than_wrong() {
        let (store, inbox, account) = store_with_mailbox();
        let mut msg = message(inbox, account, 1, "Plain");
        msg.rfc_message_id = String::new();
        store.ingest(&[msg]).unwrap();

        let id = store.envelopes(Scope::Mailbox(inbox), &Query::default(), 10).unwrap()[0].id;
        let body = store.body(id).unwrap().expect("a body");

        assert_eq!(body.message_id, "");
        assert!(body.references.is_empty());
        assert!(body.reply_to.is_empty());
    }

    #[test]
    fn a_moved_message_reunites_with_its_server_copy_instead_of_doubling() {
        // A local move clears the UID, because the one it had belonged to the
        // folder it came from. The server then hands the same message back
        // under its new UID in the destination. Inserting that would show the
        // message twice — the same mail side by side, one copy with no UID
        // and so no way to act on it.
        let (store, inbox, account) = store_with_mailbox();
        let archive = store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();

        store.ingest(&[message(inbox, account, 7, "Quarterly report")]).unwrap();
        let id = store
            .envelopes(Scope::Mailbox(inbox), &Query::default(), 10)
            .unwrap()[0]
            .id;

        store.move_message(id, archive).unwrap();

        // The same message, as the destination folder now reports it.
        let mut back = message(archive, account, 981, "Quarterly report");
        back.rfc_message_id = "<7@example.com>".into();
        back.flags = Flags(Flags::SEEN);
        store.ingest(&[back]).unwrap();

        let in_archive = store
            .envelopes(Scope::Mailbox(archive), &Query::default(), 10)
            .unwrap();
        assert_eq!(in_archive.len(), 1, "one message, not two: {in_archive:#?}");
        assert_eq!(in_archive[0].id, id, "the row that was moved, not a new one");
        assert!(in_archive[0].flags.has(Flags::SEEN), "and it took the server's flags");
    }

    #[test]
    fn a_sent_copy_is_reunited_even_if_the_server_renamed_it() {
        // The sent-mail case. The copy is filed locally under the id this
        // client generated; some submission servers hand it back under one of
        // their own, and matching on the id alone would show the message
        // twice in Sent.
        let (store, _inbox, account) = store_with_mailbox();
        let sent = store.upsert_mailbox(account, "Sent", MailboxRole::Sent).unwrap();

        let mut local = message(sent, account, 1, "Re: the proposal");
        local.uid = None;
        local.rfc_message_id = "<ours@spool.local>".into();
        local.to = vec![Address::new("Bo Chen", "bo@example.com")];
        local.date = 1_700_000_500;
        store.ingest(&[local.clone()]).unwrap();

        // The server's copy: same message, different name for it.
        let mut theirs = local.clone();
        theirs.uid = Some(4001);
        theirs.rfc_message_id = "<rewritten@mail.gmail.com>".into();
        theirs.flags = Flags(Flags::SEEN);
        store.ingest(&[theirs]).unwrap();

        let in_sent = store.envelopes(Scope::Mailbox(sent), &Query::default(), 10).unwrap();
        assert_eq!(in_sent.len(), 1, "one sent message, not two: {in_sent:#?}");
        assert!(in_sent[0].flags.has(Flags::SEEN));
    }

    #[test]
    fn a_different_message_in_the_same_folder_is_not_swallowed_by_the_fallback() {
        // The fallback matches on three things at once so that it cannot do
        // this. A different recipient, a different subject or a different
        // second is a different message.
        let (store, _inbox, account) = store_with_mailbox();
        let sent = store.upsert_mailbox(account, "Sent", MailboxRole::Sent).unwrap();

        let mut local = message(sent, account, 1, "Re: the proposal");
        local.uid = None;
        local.to = vec![Address::new("Bo Chen", "bo@example.com")];
        local.date = 1_700_000_500;
        store.ingest(&[local.clone()]).unwrap();

        let mut other = local.clone();
        other.uid = Some(4001);
        other.rfc_message_id = "<other@example.com>".into();
        other.to = vec![Address::new("Cass", "cass@example.com")];
        store.ingest(&[other]).unwrap();

        assert_eq!(
            store.envelopes(Scope::Mailbox(sent), &Query::default(), 10).unwrap().len(),
            2,
            "a different recipient is a different message"
        );
    }

    #[test]
    fn adoption_needs_something_to_match_on() {
        // No `Message-ID`, and a different timestamp, so neither the id match
        // nor the three-column fallback fires. Two rows is the right answer:
        // adopting on subject alone would merge unrelated mail.
        let (store, inbox, account) = store_with_mailbox();
        let archive = store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();

        let mut anonymous = message(inbox, account, 7, "No identity");
        anonymous.rfc_message_id = String::new();
        anonymous.date = 1_700_000_007;
        store.ingest(&[anonymous]).unwrap();
        let id = store
            .envelopes(Scope::Mailbox(inbox), &Query::default(), 10)
            .unwrap()[0]
            .id;
        store.move_message(id, archive).unwrap();

        let mut back = message(archive, account, 981, "No identity");
        back.rfc_message_id = String::new();
        back.date = 1_700_000_981;
        store.ingest(&[back]).unwrap();

        assert_eq!(
            store.envelopes(Scope::Mailbox(archive), &Query::default(), 10).unwrap().len(),
            2,
            "nothing to match on, so the safe answer is two rows rather than a wrong merge"
        );
    }

    #[test]
    fn re_ingesting_a_message_does_not_write_its_body_onto_another_one() {
        // The shape of a re-sync: a message already in the store comes back
        // with its flags changed, in the same batch as a new one. The update
        // has to know its own row id — reaching for the last *inserted* row
        // id instead put this message's body on the message before it, and
        // the reader then showed one message's subject over another's mail.
        let (store, mailbox, account) = store_with_mailbox();

        let mut old = message(mailbox, account, 1, "The old one");
        old.plain = "body of the old one".into();
        store.ingest(&[old.clone()]).unwrap();

        let mut fresh = message(mailbox, account, 2, "The new one");
        fresh.plain = "body of the new one".into();

        // The new one is inserted first, then the old one arrives as an
        // update: exactly the order that produced the damage.
        old.flags = Flags::default().with(Flags::SEEN, true);
        store.ingest(&[fresh, old]).unwrap();

        let bodies = |subject: &str| {
            let envelope = store
                .envelopes(Scope::Mailbox(mailbox), &Query::All, 100)
                .unwrap()
                .into_iter()
                .find(|e| e.subject == subject)
                .unwrap();
            store.body(envelope.id).unwrap().unwrap().plain
        };

        assert!(bodies("The old one").contains("body of the old one"));
        assert!(bodies("The new one").contains("body of the new one"));
    }

    #[test]
    fn a_message_that_comes_back_is_not_indexed_twice() {
        // A contentless index cannot tell it already has the row, so the same
        // message would be returned as two results for every search.
        let (store, mailbox, account) = store_with_mailbox();
        let mut msg = message(mailbox, account, 1, "Only once");

        store.ingest(&[msg.clone()]).unwrap();
        msg.flags = Flags::default().with(Flags::SEEN, true);
        let counted = store.ingest(&[msg]).unwrap();

        assert!(counted.is_empty(), "a message coming back is not a new message");
        let hits = store
            .envelopes(Scope::Mailbox(mailbox), &Query::Text("pomegranate".into()), 100)
            .unwrap();
        assert_eq!(hits.len(), 1, "one message, one result");
    }

    #[test]
    fn ingest_then_list() {
        let (store, mailbox, account) = store_with_mailbox();
        store
            .ingest(&[
                message(mailbox, account, 1, "First"),
                message(mailbox, account, 2, "Second"),
            ])
            .unwrap();

        let list = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 100).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].subject, "Second", "newest sorts first");
        assert_eq!(list[0].from.name, "Ana Ruiz");
        assert!(!list[0].snippet.is_empty());
    }

    #[test]
    fn re_ingesting_the_same_uid_updates_rather_than_duplicates() {
        let (store, mailbox, account) = store_with_mailbox();
        store.ingest(&[message(mailbox, account, 1, "First")]).unwrap();

        let mut again = message(mailbox, account, 1, "First");
        again.flags = Flags::default().with(Flags::SEEN, true);
        store.ingest(&[again]).unwrap();

        let list = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 100).unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].flags.is_seen(), "flags from the server win");
    }

    #[test]
    fn full_text_search_finds_body_subject_and_sender() {
        let (store, mailbox, account) = store_with_mailbox();
        store.ingest(&[message(mailbox, account, 1, "Quarterly report")]).unwrap();

        let hits = |q: &str| {
            store
                .envelopes(Scope::Mailbox(mailbox), &Query::Text(q.into()), 100)
                .unwrap()
                .len()
        };
        assert_eq!(hits("pomegranate"), 1, "body");
        assert_eq!(hits("quarterly"), 1, "subject");
        assert_eq!(hits("ana@example.com"), 1, "sender");
        assert_eq!(hits("quart"), 1, "prefix match, so results narrow as you type");
        assert_eq!(hits("watermelon"), 0);
    }

    #[test]
    fn search_survives_punctuation_that_would_break_fts_syntax() {
        let (store, mailbox, account) = store_with_mailbox();
        store.ingest(&[message(mailbox, account, 1, "Report")]).unwrap();

        // Each of these is a syntax error if passed to FTS5 verbatim.
        for query in ["\"unbalanced", "- ", "report OR", "a*(b"] {
            let result = store.envelopes(Scope::Mailbox(mailbox), &Query::Text(query.into()), 10);
            assert!(result.is_ok(), "query {query:?} should not error");
        }
    }

    #[test]
    fn unread_and_flagged_filters() {
        let (store, mailbox, account) = store_with_mailbox();
        let mut read = message(mailbox, account, 1, "Read");
        read.flags = Flags::default().with(Flags::SEEN, true);
        let mut flagged = message(mailbox, account, 2, "Flagged");
        flagged.flags = Flags::default().with(Flags::FLAGGED, true);
        store.ingest(&[read, flagged, message(mailbox, account, 3, "Plain")]).unwrap();

        let unread = store.envelopes(Scope::Mailbox(mailbox), &Query::Unread, 100).unwrap();
        assert_eq!(unread.len(), 2);
        let flagged = store.envelopes(Scope::Mailbox(mailbox), &Query::Flagged, 100).unwrap();
        assert_eq!(flagged.len(), 1);
    }

    #[test]
    fn replies_land_in_one_thread() {
        let (store, mailbox, account) = store_with_mailbox();
        let mut reply = message(mailbox, account, 2, "Re: First");
        reply.in_reply_to = "<1@example.com>".into();
        reply.references = vec!["<1@example.com>".into()];
        store.ingest(&[message(mailbox, account, 1, "First"), reply]).unwrap();

        let list = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 100).unwrap();
        assert_eq!(list[0].thread_id, list[1].thread_id);
        assert_eq!(store.thread(list[0].thread_id).unwrap().len(), 2);
    }

    #[test]
    fn mailbox_counts_track_unread() {
        let (store, mailbox, account) = store_with_mailbox();
        store
            .ingest(&[
                message(mailbox, account, 1, "One"),
                message(mailbox, account, 2, "Two"),
            ])
            .unwrap();

        let before = store.mailboxes().unwrap().remove(0);
        assert_eq!((before.total, before.unread), (2, 2));

        store.mark_all_read(Scope::Mailbox(mailbox)).unwrap();
        let after = store.mailboxes().unwrap().remove(0);
        assert_eq!((after.total, after.unread), (2, 0));
    }

    #[test]
    fn bodies_round_trip_with_attachments() {
        let (store, mailbox, account) = store_with_mailbox();
        let mut msg = message(mailbox, account, 1, "With file");
        msg.to = vec![Address::new("Bo", "bo@example.com")];
        msg.attachments =
            vec![Attachment {
                name: "q3.pdf".into(),
                mime: "application/pdf".into(),
                size: 9,
                content_id: None,
                inline: false,
            }];
        store.ingest(&[msg]).unwrap();

        let id = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 1).unwrap()[0].id;
        let body = store.body(id).unwrap().unwrap();
        assert!(body.plain.contains("pomegranate"));
        assert_eq!(body.to[0].addr, "bo@example.com");
        assert_eq!(body.attachments[0].name, "q3.pdf");
        assert_eq!(body.attachments[0].size, 9);
    }

    #[test]
    fn attachment_bytes_round_trip_and_stay_out_of_the_body() {
        let (store, mailbox, account) = store_with_mailbox();
        let mut msg = message(mailbox, account, 1, "With files");
        msg.attachments = vec![
            Attachment {
                name: "q3.pdf".into(),
                mime: "application/pdf".into(),
                size: 5,
                content_id: None,
                inline: false,
            },
            Attachment {
                name: "huge.zip".into(),
                mime: "application/zip".into(),
                size: 0,
                content_id: None,
                inline: false,
            },
        ];
        // The second was too large to keep, so it arrives with no payload.
        msg.attachment_bytes = vec![b"%PDF!".to_vec(), Vec::new()];
        store.ingest(&[msg]).unwrap();

        let id = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 1).unwrap()[0].id;

        assert_eq!(store.attachment_bytes(id, 0).unwrap().as_deref(), Some(&b"%PDF!"[..]));
        assert_eq!(
            store.attachment_bytes(id, 1).unwrap(),
            None,
            "an attachment too large to keep stores nothing rather than a truncated file",
        );
        assert_eq!(store.stored_attachments(id).unwrap(), vec![0]);

        // Both are still listed: the message can say what it has either way.
        let body = store.body(id).unwrap().unwrap();
        assert_eq!(body.attachments.len(), 2);
    }

    #[test]
    fn deleting_a_message_drops_its_attachment_bytes() {
        let (store, mailbox, account) = store_with_mailbox();
        let mut msg = message(mailbox, account, 1, "With a file");
        msg.attachments = vec![Attachment {
            name: "a.bin".into(),
            mime: "application/octet-stream".into(),
            size: 3,
            content_id: None,
            inline: false,
        }];
        msg.attachment_bytes = vec![b"abc".to_vec()];
        store.ingest(&[msg]).unwrap();

        let id = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 1).unwrap()[0].id;
        store.delete_message(id).unwrap();

        assert!(
            store.stored_attachments(id).unwrap().is_empty(),
            "the cascade should take the blobs with the message",
        );
    }

    #[test]
    fn deleting_a_message_removes_it_from_search() {
        let (store, mailbox, account) = store_with_mailbox();
        store.ingest(&[message(mailbox, account, 1, "Ephemeral")]).unwrap();
        let id = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 1).unwrap()[0].id;

        store.delete_message(id).unwrap();
        let hits = store
            .envelopes(Scope::Mailbox(mailbox), &Query::Text("pomegranate".into()), 10)
            .unwrap();
        assert!(hits.is_empty());
        assert_eq!(store.message_count().unwrap(), 0);
    }

    #[test]
    fn scopes_separate_accounts_and_unify_everything() {
        let store = Store::in_memory().unwrap();
        let a = store.add_account("A", "a@example.com", 0).unwrap();
        let b = store.add_account("B", "b@example.com", 1).unwrap();
        let ma = store.upsert_mailbox(a, "INBOX", MailboxRole::Inbox).unwrap();
        let mb = store.upsert_mailbox(b, "INBOX", MailboxRole::Inbox).unwrap();
        store.ingest(&[message(ma, a, 1, "A one"), message(mb, b, 1, "B one")]).unwrap();

        assert_eq!(store.envelopes(Scope::Account(a), &Query::All, 100).unwrap().len(), 1);
        assert_eq!(store.envelopes(Scope::Everything, &Query::All, 100).unwrap().len(), 2);
    }

    #[test]
    fn an_account_row_lists_its_inbox_rather_than_every_copy_of_every_message() {
        // A Gmail account, as it actually arrives: the message is in the
        // inbox, and All Mail holds the same message again under a different
        // uid, and Sent and Trash hold mail nobody wants in a reading list.
        let store = Store::in_memory().unwrap();
        let account = store.add_account("A", "a@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let all = store.upsert_mailbox(account, "[Gmail]/All Mail", MailboxRole::Archive).unwrap();
        let sent = store.upsert_mailbox(account, "[Gmail]/Sent", MailboxRole::Sent).unwrap();
        store
            .ingest(&[
                message(inbox, account, 1, "Roof quote"),
                message(all, account, 900, "Roof quote"),
                message(sent, account, 7, "Re: roof quote"),
            ])
            .unwrap();

        let rows = store.envelopes(Scope::Account(account), &Query::All, 100).unwrap();
        assert_eq!(rows.len(), 1, "one row per message in the inbox, and nothing else");
        assert_eq!(rows[0].mailbox_id, inbox);

        // The folders are still there to be opened on their own.
        assert_eq!(store.envelopes(Scope::Mailbox(all), &Query::All, 100).unwrap().len(), 1);
        assert_eq!(store.envelopes(Scope::Mailbox(sent), &Query::All, 100).unwrap().len(), 1);
    }

    #[test]
    fn marking_an_account_read_stops_at_the_rows_the_account_row_showed() {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("A", "a@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let trash = store.upsert_mailbox(account, "Trash", MailboxRole::Trash).unwrap();
        store
            .ingest(&[
                message(inbox, account, 1, "Unread here"),
                message(trash, account, 2, "Unread in the trash"),
            ])
            .unwrap();

        assert_eq!(store.mark_all_read(Scope::Account(account)).unwrap(), 1);

        let left = store.envelopes(Scope::Mailbox(trash), &Query::Unread, 10).unwrap();
        assert_eq!(left.len(), 1, "the trash was never on screen, so it was not touched");
    }

    #[test]
    fn an_account_with_no_inbox_yet_lists_nothing_rather_than_everything() {
        // The state a freshly added account is in for the first few seconds:
        // folders have not arrived, so nothing has a role. Showing the whole
        // account here would be showing exactly the mixed, doubled list the
        // inbox rule exists to avoid.
        let store = Store::in_memory().unwrap();
        let account = store.add_account("A", "a@example.com", 0).unwrap();
        let holding = store.upsert_mailbox(account, "Later", MailboxRole::Other).unwrap();
        store.ingest(&[message(holding, account, 1, "Waiting")]).unwrap();

        assert!(store.envelopes(Scope::Account(account), &Query::All, 100).unwrap().is_empty());
    }

    #[test]
    fn an_account_with_two_inboxes_lists_both_of_them() {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("A", "a@example.com", 0).unwrap();
        let one = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let two = store.upsert_mailbox(account, "INBOX/Priority", MailboxRole::Inbox).unwrap();
        store
            .ingest(&[message(one, account, 1, "First"), message(two, account, 1, "Second")])
            .unwrap();

        assert_eq!(store.envelopes(Scope::Account(account), &Query::All, 100).unwrap().len(), 2);
    }

    #[test]
    fn the_list_reader_sees_what_the_store_writes() {
        let dir = std::env::temp_dir().join(format!("spool-reader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reader.db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }

        let store = Store::open(&path).unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let reader = store.list_reader().unwrap().expect("a file-backed store has a reader");
        // Nothing running: an interrupt is a no-op, not a trap for the next query.
        reader.interrupt();

        store.ingest(&[message(inbox, account, 1, "First"), message(inbox, account, 2, "Second")]).unwrap();
        let scope = Scope::Mailbox(inbox);
        assert_eq!(
            reader.envelopes(scope, &Query::All, 10).unwrap().len(),
            store.envelopes(scope, &Query::All, 10).unwrap().len(),
            "a write committed on the store's connection is visible on the reader's",
        );
        assert_eq!(reader.conversations(scope, &Query::All, 10).unwrap().len(), 2);
        let found = reader.envelopes(scope, &Query::Text("pomegranate".into()), 10).unwrap();
        assert_eq!(found.len(), 2, "search goes through the reader too");
    }

    #[test]
    fn a_first_page_of_conversations_counts_each_one_in_full() {
        // Thread A is the newest conversation but most of it is old: with a
        // window of the newest messages alone it would count one message, not
        // three. The page has to say three, as the whole list does.
        let (store, inbox, account) = store_with_mailbox();
        let mut batch = Vec::new();
        for uid in 1..=40u32 {
            let mut m = message(inbox, account, uid, &format!("Filler {uid}"));
            m.date = 1_700_000_000 + uid as i64 * 100;
            batch.push(m);
        }
        let mut root = message(inbox, account, 100, "Old thread");
        root.date = 1_600_000_000;
        let mut middle = message(inbox, account, 101, "Re: Old thread");
        middle.in_reply_to = root.rfc_message_id.clone();
        middle.references = vec![root.rfc_message_id.clone()];
        middle.date = 1_600_000_100;
        let mut newest = message(inbox, account, 102, "Re: Old thread");
        newest.in_reply_to = middle.rfc_message_id.clone();
        newest.references = vec![root.rfc_message_id.clone(), middle.rfc_message_id.clone()];
        newest.date = 1_800_000_000;
        batch.extend([root, middle, newest]);
        store.ingest(&batch).unwrap();

        let scope = Scope::Mailbox(inbox);
        let full = store.conversations(scope, &Query::All, 1000).unwrap();
        let page = store.conversations_page(scope, &Query::All, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].message_count(), 3, "the whole thread, not the part inside the window");
        for (a, b) in page.iter().zip(&full) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.message_ids(), b.message_ids());
        }
    }

    #[test]
    fn an_in_memory_store_has_no_reader() {
        let (store, _, _) = store_with_mailbox();
        assert!(store.list_reader().unwrap().is_none());
    }

    #[test]
    fn threads_survive_reopening_the_database() {
        let dir = std::env::temp_dir().join(format!("spool-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("threads.db");
        let _ = std::fs::remove_file(&path);

        let thread_id = {
            let store = Store::open(&path).unwrap();
            let account = store.add_account("Test", "me@example.com", 0).unwrap();
            let mailbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
            store.ingest(&[message(mailbox, account, 1, "First")]).unwrap();
            store.envelopes(Scope::Mailbox(mailbox), &Query::All, 1).unwrap()[0].thread_id
        };

        // Reopen, then deliver a reply. It must join the existing conversation
        // rather than starting a new one with a recycled id.
        let store = Store::open(&path).unwrap();
        let mailbox = store.mailboxes().unwrap()[0].id;
        let account = store.accounts().unwrap()[0].id;
        let mut reply = message(mailbox, account, 2, "Re: First");
        reply.in_reply_to = "<1@example.com>".into();
        store.ingest(&[reply]).unwrap();

        let list = store.envelopes(Scope::Mailbox(mailbox), &Query::All, 10).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].thread_id, thread_id);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn headers_answer_for_the_columns_there_are_and_for_nothing_else() {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();

        let mut incoming = message(inbox, account, 1, "Re: lunch");
        incoming.in_reply_to = "<first@example.com>".into();
        incoming.rfc_message_id = "<second@example.com>".into();
        store.ingest(&[incoming]).unwrap();

        let id = store.envelopes(Scope::Everything, &Query::All, 10).unwrap()[0].id;

        assert_eq!(store.header(id, "in-reply-to").unwrap().as_deref(), Some("<first@example.com>"));
        assert_eq!(store.header(id, "Message-ID").unwrap().as_deref(), Some("<second@example.com>"));
        assert_eq!(store.header(id, "subject").unwrap().as_deref(), Some("Re: lunch"));

        // Not kept, and indistinguishable from not sent — which is the honest
        // answer, since the raw source was never stored.
        assert_eq!(store.header(id, "authentication-results").unwrap(), None);
        // And a name that is really an attempt at the query.
        assert_eq!(store.header(id, "subject FROM messages --").unwrap(), None);
    }

    #[test]
    fn an_extensions_state_is_its_own_and_survives_being_written_over() {
        let store = Store::in_memory().unwrap();

        assert_eq!(store.ext_get("scorer", "tally").unwrap(), None);

        store.ext_set("scorer", "tally", "{\"scored\":1}").unwrap();
        assert_eq!(store.ext_get("scorer", "tally").unwrap().as_deref(), Some("{\"scored\":1}"));

        store.ext_set("scorer", "tally", "{\"scored\":2}").unwrap();
        assert_eq!(store.ext_get("scorer", "tally").unwrap().as_deref(), Some("{\"scored\":2}"));

        // Another extension's key of the same name is another value.
        store.ext_set("stats", "tally", "elsewhere").unwrap();
        assert_eq!(store.ext_get("scorer", "tally").unwrap().as_deref(), Some("{\"scored\":2}"));

        store.ext_delete("scorer", "tally").unwrap();
        assert_eq!(store.ext_get("scorer", "tally").unwrap(), None);
        assert_eq!(store.ext_get("stats", "tally").unwrap().as_deref(), Some("elsewhere"));
    }

    #[test]
    fn uninstalling_an_extension_can_forget_everything_it_knew() {
        let store = Store::in_memory().unwrap();
        for n in 0..5 {
            store.ext_set("scorer", &format!("v1.{n}"), "{}").unwrap();
        }
        store.ext_set("stats", "kept", "yes").unwrap();

        assert_eq!(store.ext_clear("scorer").unwrap(), 5);
        assert_eq!(store.ext_get("scorer", "v1.0").unwrap(), None);
        assert_eq!(store.ext_get("stats", "kept").unwrap().as_deref(), Some("yes"));
    }

    #[test]
    fn max_uid_drives_incremental_sync() {
        let (store, mailbox, account) = store_with_mailbox();
        assert_eq!(store.max_uid(mailbox).unwrap(), 0);
        store
            .ingest(&[
                message(mailbox, account, 7, "Seven"),
                message(mailbox, account, 3, "Three"),
            ])
            .unwrap();
        assert_eq!(store.max_uid(mailbox).unwrap(), 7);
    }
}
