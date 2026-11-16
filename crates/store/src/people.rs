//! Who you correspond with, and the mail you have had with each of them.
//!
//! The message table keeps addresses as encoded lists in text columns, which
//! is right for drawing a header and no use at all for the question a client
//! book asks — "everything with Ana in it". Answering that from `to_addrs`
//! means decoding every row in the store. So the answer is kept as an index of
//! its own: one row per person per message, written in the same transaction
//! as the message, and filled in once, in the background, for mail stored
//! before the index existed.
//!
//! All of it is derived. Nothing here is anything a person typed; emptying
//! both tables and running the backfill again gives back the same rows. That
//! is what lets it live in the mail store, which is a cache of the server,
//! while notes and the board live in a file of their own (see [`crate::book`]).
//!
//! Your own addresses are never indexed. Every message has you on it, so you
//! would be the largest entry in the book and the one nobody wants to see —
//! and "which way did this go" is a question about you, which is answered by
//! the direction on everyone else's row instead.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use spool_core::{AccountId, Address, Envelope, MailboxId, MailboxRole, address};

use crate::Store;

/// The key the backfill keeps its place under, in `store_meta`.
///
/// Every message with an id below this one still has to be indexed; mail at or
/// above it was indexed as it arrived. Zero means the backfill is finished.
const BACKFILL_BELOW: &str = "people.backfill_below";

/// Whether the index has been kept up as mail arrived, in `store_meta`.
///
/// [`RULES`] while Clients is on: every message stored since the last rebuild
/// was indexed as it landed, by the rules this build indexes with. Anything
/// else — Clients switched off, never on, or on under older rules — means the
/// rows are not what indexing the store now would give: mail stored without
/// being indexed sits above the backfill's watermark, where the backfill would
/// never find it, and mail indexed by older rules is missing whoever the new
/// ones would have found. So the next backfill starts again from the top.
const LIVE: &str = "people.live";

/// Which rules the index was built by. Raised whenever [`index`] would file
/// the same message under different people, so a store indexed before the
/// change is rebuilt rather than left half one way and half the other.
///
/// 2: a notice you sent yourself is from whoever it is about.
const RULES: i64 = 2;

/// Which way a message went, from where one person in it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// They sent it.
    Inbound,
    /// You sent it, and they were on it.
    Outbound,
    /// Somebody else sent it, and they were on it beside you.
    Copied,
}

impl Direction {
    fn stored(self) -> i64 {
        match self {
            Direction::Inbound => 0,
            Direction::Outbound => 1,
            Direction::Copied => 2,
        }
    }

    fn from_stored(value: i64) -> Self {
        match value {
            0 => Direction::Inbound,
            1 => Direction::Outbound,
            _ => Direction::Copied,
        }
    }
}

/// Everything the index knows about one address, counted up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correspondent {
    /// The row in `correspondents`. Local to this store, like every other id
    /// here: a rebuilt store numbers people differently, so nothing that
    /// outlives the store may keep one. The address is the stable name.
    pub id: i64,
    /// Normalized: trimmed and lowercased.
    pub addr: String,
    /// The most recent name they were seen under, or empty.
    pub name: String,
    /// Messages with them in, counted once however many folders hold a copy.
    pub messages: u32,
    /// Of those, how many they sent.
    pub received: u32,
    /// And how many you sent them.
    pub sent: u32,
    /// Unix seconds.
    pub first: i64,
    pub last: i64,
    /// The newest message from them, and the newest from you to them.
    pub last_inbound: Option<i64>,
    pub last_outbound: Option<i64>,
    /// The accounts this person has mail in, in no particular order.
    pub accounts: Vec<AccountId>,
}

impl Correspondent {
    /// Whether you have ever written to them.
    ///
    /// The filter a client book starts with, and it does nearly all the work
    /// on its own: newsletters, receipts and `noreply@` mail are mail nobody
    /// answers, so none of it is two-way.
    pub fn is_two_way(&self) -> bool {
        self.sent > 0
    }

    /// The part after the `@`.
    pub fn domain(&self) -> &str {
        self.addr.rsplit_once('@').map(|(_, domain)| domain).unwrap_or("")
    }
}

/// One message in somebody's history.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub envelope: Envelope,
    pub direction: Direction,
    /// Who it was addressed to, for "you → Ana, Ben".
    pub to: Vec<Address>,
    pub rfc_message_id: String,
    /// Every folder holding a copy. The first is the one `envelope` describes.
    pub mailboxes: Vec<MailboxId>,
}

/// How far the backfill has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backfill {
    /// Messages indexed by the call that returned this.
    pub indexed: usize,
    /// Messages still waiting. Zero means the index is complete.
    pub remaining: usize,
}

/// What counts as you, for deciding which way a message went.
pub(crate) struct Me {
    addresses: HashSet<String>,
    /// Folders that hold mail you sent.
    ///
    /// A message filed in Sent was written by you whatever its `From:` says —
    /// which is the case an alias produces, sending as an address no account
    /// here is configured with.
    sent: HashSet<i64>,
}

impl Me {
    pub(crate) fn read(conn: &Connection) -> Result<Self> {
        let mut addresses = HashSet::new();
        let mut stmt = conn.prepare_cached("SELECT email FROM accounts")?;
        for email in stmt.query_map([], |r| r.get::<_, String>(0))? {
            addresses.insert(address::normalize(&email?));
        }

        let mut sent = HashSet::new();
        let mut stmt = conn.prepare_cached("SELECT id FROM mailboxes WHERE role = ?1")?;
        for id in stmt.query_map([MailboxRole::Sent.as_str()], |r| r.get::<_, i64>(0))? {
            sent.insert(id?);
        }

        Ok(Self { addresses, sent })
    }

    fn is(&self, addr: &str) -> bool {
        self.addresses.contains(addr)
    }

    fn wrote(&self, mailbox: i64, from: &str) -> bool {
        self.sent.contains(&mailbox) || self.is(from)
    }
}

/// One message, as the index needs to see it.
pub(crate) struct Indexed<'a> {
    pub id: i64,
    pub account: i64,
    pub mailbox: i64,
    pub date: i64,
    pub from: &'a Address,
    pub to: &'a [Address],
    pub cc: &'a [Address],
    pub reply_to: &'a [Address],
    /// The start of its text, as the message list shows it.
    pub snippet: &'a str,
}

/// Writes the rows for one message.
///
/// `newest` says the message is the latest word on whoever is in it, so the
/// names it carries replace the ones on file. The backfill passes `false`: it
/// walks newest first, so the first name it finds for someone is already the
/// newest, and an older message must not overwrite it.
pub(crate) fn index(conn: &Connection, me: &Me, message: Indexed<'_>, newest: bool) -> Result<()> {
    let from = address::normalize(&message.from.addr);
    let outbound = me.wrote(message.mailbox, &from);

    // Strongest part first: the insert ignores a second row for the same
    // person on the same message, so an address in both `From` and `Cc`
    // keeps the part that says more.
    let mut people: Vec<(&Address, Direction)> = Vec::with_capacity(1 + message.to.len() + message.cc.len());
    if !outbound {
        people.push((message.from, Direction::Inbound));
    }

    // Mail you sent only to yourself is a notice — a sign-up, an order, a
    // contact form — that your own site or app sent on someone's behalf, and
    // it is from them. A form that knows how puts them in `Reply-To`, where
    // an answer goes; one that does not names them in its text. Neither
    // counts on mail from anyone else: there `Reply-To` is mostly a
    // newsletter's reply address, thousands of them, none of them a person.
    let named: Option<Address>;
    if outbound && message.to.iter().chain(message.cc).all(|a| me.is(&address::normalize(&a.addr))) {
        let reply_to = message.reply_to.iter().find(|a| {
            let addr = address::normalize(&a.addr);
            addr != from && !me.is(&addr)
        });
        named = match reply_to {
            Some(_) => None,
            None => addresses_in(message.snippet).into_iter().find(|addr| !me.is(addr)).map(Address::bare),
        };
        people.extend(reply_to.or(named.as_ref()).map(|a| (a, Direction::Inbound)));
    }
    let addressed = if outbound { Direction::Outbound } else { Direction::Copied };
    people.extend(message.to.iter().chain(message.cc).map(|a| (a, addressed)));

    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO message_people (person, date_ts, message_id, account_id, direction)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (person, direction) in people {
        let addr = address::normalize(&person.addr);
        if !addr.contains('@') || me.is(&addr) {
            continue;
        }
        let id = correspondent(conn, &addr, clean_name(&person.name), newest)?;
        insert.execute(params![id, message.date, message.id, message.account, direction.stored()])?;
    }
    Ok(())
}

/// The row for an address, made if it is new, with its name kept current.
fn correspondent(conn: &Connection, addr: &str, name: &str, newest: bool) -> Result<i64> {
    let existing: Option<(i64, String)> = conn
        .prepare_cached("SELECT id, name FROM correspondents WHERE addr = ?1")?
        .query_row([addr], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;

    match existing {
        Some((id, known)) => {
            if !name.is_empty() && known != name && (newest || known.is_empty()) {
                conn.prepare_cached("UPDATE correspondents SET name = ?2 WHERE id = ?1")?
                    .execute(params![id, name])?;
            }
            Ok(id)
        }
        None => Ok(conn
            .prepare_cached("INSERT INTO correspondents (addr, name) VALUES (?1, ?2) RETURNING id")?
            .query_row(params![addr, name], |r| r.get(0))?),
    }
}

/// The addresses written in some text, in order, normalized.
///
/// A snippet cut short ends in an ellipsis, and an address that runs into it
/// was cut too: `ana@acme.co…` is somebody else. The ellipsis stays on the
/// word, so the word is not an address and is passed over.
fn addresses_in(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || "<>()[]{},;:\"'".contains(c))
        // Sentence punctuation after an address is not part of it.
        .map(|word| word.trim_end_matches(['.', '!', '?']))
        .filter(|word| is_address(word))
        .map(address::normalize)
        .collect()
}

/// Whether a word is plainly an email address: something, an `@`, and a
/// domain with a dot in it and letters at the end.
fn is_address(word: &str) -> bool {
    let Some((local, domain)) = word.split_once('@') else { return false };
    let local_ok = !local.is_empty() && local.chars().all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c));
    let labels: Vec<&str> = domain.split('.').collect();
    let domain_ok = labels.len() >= 2
        && labels.iter().all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        && labels.last().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()));
    local_ok && domain_ok
}

/// A display name with the quoting it arrived in taken off.
///
/// `'Ana Ruiz'` and `"Ana Ruiz"` are what some clients put in a header, and
/// a book that shows the quotes looks like it parsed the header wrong. A name
/// that is just the address again says nothing the address does not.
fn clean_name(name: &str) -> &str {
    let name = name.trim().trim_matches(['"', '\'']).trim();
    if name.contains('@') { "" } else { name }
}

impl Store {
    /// Switches the people index on or off.
    ///
    /// Off, mail is stored exactly as it was before the index existed: nobody
    /// is indexed, nothing is backfilled, and a mailbox whose owner never
    /// opens Clients pays nothing for it. On, mail is indexed as it arrives,
    /// and the first backfill after a stretch off rebuilds what was missed.
    ///
    /// Set before the first sync, so nothing lands in between: what is
    /// stored while the switch is on is indexed, and what was stored while it
    /// was off is the backfill's.
    pub fn set_people_index(&self, on: bool) -> Result<()> {
        let mut this = self.lock();
        this.people = on;
        let live = is_live(&this.conn)?;
        match on {
            // An empty store has nothing to have missed, so it is already
            // whole — and a store being written for the first time is
            // indexed as it goes, with nothing for a backfill to do.
            true if !live => {
                let empty: bool = this.conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM messages)", [], |r| r.get(0))?;
                if empty {
                    let tx = this.conn.unchecked_transaction()?;
                    set_meta(&tx, BACKFILL_BELOW, 0)?;
                    set_meta(&tx, LIVE, RULES)?;
                    tx.commit()?;
                }
            }
            false if live => set_meta(&this.conn, LIVE, 0)?,
            _ => {}
        }
        Ok(())
    }

    /// Indexes the next `batch` messages that were stored before the index
    /// existed, newest first.
    ///
    /// Newest first because the people worth seeing are the recent ones: a
    /// client book opened halfway through the backfill should have this
    /// month's correspondence in it, not 2014's.
    ///
    /// One batch per call, each its own transaction, so a sync waiting on the
    /// store waits for one batch rather than for a quarter of a million
    /// messages. The caller loops until `remaining` is zero.
    pub fn backfill_people(&self, batch: usize) -> Result<Backfill> {
        let this = self.lock();
        if !this.people {
            return Ok(Backfill { indexed: 0, remaining: 0 });
        }
        // Mail was stored while the index was off: start over, and from here
        // on it is kept up.
        if !is_live(&this.conn)? {
            let tx = this.conn.unchecked_transaction()?;
            restart(&tx)?;
            set_meta(&tx, LIVE, RULES)?;
            tx.commit()?;
        }
        let below: i64 = this
            .conn
            .query_row("SELECT value FROM store_meta WHERE key = ?1", [BACKFILL_BELOW], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        if below <= 0 {
            return Ok(Backfill { indexed: 0, remaining: 0 });
        }

        let tx = this.conn.unchecked_transaction()?;
        let me = Me::read(&tx)?;

        struct Row {
            id: i64,
            account: i64,
            mailbox: i64,
            date: i64,
            from: Address,
            to: Vec<Address>,
            cc: Vec<Address>,
            reply_to: Vec<Address>,
            snippet: String,
        }

        let rows: Vec<Row> = {
            let mut stmt = tx.prepare(
                "SELECT id, account_id, mailbox_id, date_ts, from_name, from_addr, to_addrs, cc_addrs,
                        reply_to, snippet
                 FROM messages WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            stmt.query_map(params![below, batch as i64], |r| {
                Ok(Row {
                    id: r.get(0)?,
                    account: r.get(1)?,
                    mailbox: r.get(2)?,
                    date: r.get(3)?,
                    from: Address::new(r.get::<_, String>(4)?, r.get::<_, String>(5)?),
                    to: address::decode_list(&r.get::<_, String>(6)?),
                    cc: address::decode_list(&r.get::<_, String>(7)?),
                    reply_to: address::decode_list(&r.get::<_, String>(8)?),
                    snippet: r.get(9)?,
                })
            })?
            .collect::<Result<_, _>>()?
        };

        for row in &rows {
            let message = Indexed {
                id: row.id,
                account: row.account,
                mailbox: row.mailbox,
                date: row.date,
                from: &row.from,
                to: &row.to,
                cc: &row.cc,
                reply_to: &row.reply_to,
                snippet: &row.snippet,
            };
            index(&tx, &me, message, false)?;
        }

        // A short batch is the last one: there is nothing below it.
        let next = match rows.len() < batch {
            true => 0,
            false => rows.last().map(|row| row.id).unwrap_or(0),
        };
        tx.execute("UPDATE store_meta SET value = ?2 WHERE key = ?1", params![BACKFILL_BELOW, next])?;
        let remaining: i64 = match next {
            0 => 0,
            next => tx.query_row("SELECT COUNT(*) FROM messages WHERE id < ?1", [next], |r| r.get(0))?,
        };
        tx.commit()?;

        Ok(Backfill { indexed: rows.len(), remaining: remaining as usize })
    }

    /// Empties the index and starts the backfill again from the top.
    ///
    /// For when what counts as you has changed under mail already indexed —
    /// an account added that was, until now, somebody you wrote to.
    pub fn reindex_people(&self) -> Result<()> {
        let this = self.lock();
        let tx = this.conn.unchecked_transaction()?;
        restart(&tx)?;
        tx.commit()?;
        Ok(())
    }

    /// Everyone in the index, counted up. See [`correspondents`].
    pub fn people(&self) -> Result<Vec<Correspondent>> {
        correspondents(&self.lock().conn)
    }

    /// One person's history, or several people's merged. See [`history`].
    pub fn history(&self, addrs: &[String], domains: &[String], limit: usize) -> Result<Vec<Exchange>> {
        history(&self.lock().conn, addrs, domains, limit)
    }

    /// A read-only connection for the client book.
    ///
    /// Its own, for the reason the message list has one: a sync writing a
    /// batch holds the shared connection, and opening someone's history
    /// should not wait for three hundred messages to commit. `None` for an
    /// in-memory store, which has no second connection to give; read through
    /// the store instead.
    pub fn people_reader(&self) -> Result<Option<PeopleReader>> {
        Ok(self.read_connection()?.map(|conn| PeopleReader { conn: std::sync::Mutex::new(conn) }))
    }
}

/// Empties the index and points the backfill at the newest message.
fn restart(conn: &Connection) -> Result<()> {
    conn.execute_batch("DELETE FROM message_people; DELETE FROM correspondents;")?;
    conn.execute(
        "INSERT OR REPLACE INTO store_meta (key, value) VALUES (?1, (SELECT COALESCE(MAX(id), 0) + 1 FROM messages))",
        [BACKFILL_BELOW],
    )?;
    Ok(())
}

/// See [`LIVE`].
fn is_live(conn: &Connection) -> Result<bool> {
    let live: Option<i64> =
        conn.query_row("SELECT value FROM store_meta WHERE key = ?1", [LIVE], |r| r.get(0)).optional()?;
    Ok(live == Some(RULES))
}

fn set_meta(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute("INSERT OR REPLACE INTO store_meta (key, value) VALUES (?1, ?2)", params![key, value])?;
    Ok(())
}

/// See [`Store::people_reader`].
pub struct PeopleReader {
    conn: std::sync::Mutex<Connection>,
}

impl PeopleReader {
    pub fn people(&self) -> Result<Vec<Correspondent>> {
        correspondents(&self.conn.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn history(&self, addrs: &[String], domains: &[String], limit: usize) -> Result<Vec<Exchange>> {
        history(&self.conn.lock().unwrap_or_else(|e| e.into_inner()), addrs, domains, limit)
    }
}

/// Everyone in the index, with their mail counted up.
///
/// Counted here rather than by a `GROUP BY`, because of what has to be counted
/// once. The same message stored in two folders — Inbox and Gmail's All Mail,
/// a sent copy and its reply's quote of it — is two rows with the same date,
/// and the index's key puts a person's rows in date order, so the copies are
/// always next to each other. One pass over the key, comparing each row with
/// the one before it, counts messages rather than copies; SQL would need a
/// `COUNT(DISTINCT …)` per column, and a temporary index for each.
///
/// Two different messages to one person in the same second are counted as
/// one. That is a mail merge, and its count was never going to mean much.
fn correspondents(conn: &Connection) -> Result<Vec<Correspondent>> {
    let mut names: HashMap<i64, (String, String)> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT id, addr, name FROM correspondents")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?, r.get(2)?)))? {
            let (id, addr, name) = row?;
            names.insert(id, (addr, name));
        }
    }

    let mut out: Vec<Correspondent> = Vec::with_capacity(names.len());
    let mut stmt = conn.prepare(
        "SELECT person, date_ts, direction, account_id FROM message_people
         ORDER BY person, date_ts",
    )?;
    let mut rows = stmt.query([])?;

    // The date of the last row counted for the current person, per direction
    // and overall, so a copy is recognised as one.
    let mut last_seen: Option<(i64, i64)> = None;
    let mut last_by_direction: [Option<i64>; 3] = [None; 3];

    while let Some(row) = rows.next()? {
        let person: i64 = row.get(0)?;
        let date: i64 = row.get(1)?;
        let direction = Direction::from_stored(row.get(2)?);
        let account = AccountId(row.get(3)?);

        if out.last().is_none_or(|c| c.id != person) {
            let (addr, name) = names.remove(&person).unwrap_or_default();
            out.push(Correspondent {
                id: person,
                addr,
                name,
                messages: 0,
                received: 0,
                sent: 0,
                first: date,
                last: date,
                last_inbound: None,
                last_outbound: None,
                accounts: Vec::new(),
            });
            last_seen = None;
            last_by_direction = [None; 3];
        }
        let current = out.last_mut().expect("pushed above");

        if last_seen != Some((person, date)) {
            current.messages += 1;
            last_seen = Some((person, date));
        }
        let slot = direction.stored() as usize;
        if last_by_direction[slot] != Some(date) {
            match direction {
                Direction::Inbound => current.received += 1,
                Direction::Outbound => current.sent += 1,
                Direction::Copied => {}
            }
            last_by_direction[slot] = Some(date);
        }

        current.last = current.last.max(date);
        match direction {
            Direction::Inbound => current.last_inbound = Some(date),
            Direction::Outbound => current.last_outbound = Some(date),
            Direction::Copied => {}
        }
        if !current.accounts.contains(&account) {
            current.accounts.push(account);
        }
    }

    Ok(out)
}

/// The mail a set of addresses and domains has been on, newest first.
///
/// Several addresses because one client is often several: a work address and
/// a personal one, or everyone at a company, which is what a domain means.
///
/// Each message comes back once. A message to two of the client's addresses is
/// one message, and a message held in two folders is one message with two
/// copies — the copy described is the one in the folder that says most about
/// it, so a message that is both in Inbox and in Trash reads as in the Inbox.
fn history(conn: &Connection, addrs: &[String], domains: &[String], limit: usize) -> Result<Vec<Exchange>> {
    let mut people: Vec<i64> = Vec::new();
    {
        let mut by_addr = conn.prepare_cached("SELECT id FROM correspondents WHERE addr = ?1")?;
        for addr in addrs {
            if let Some(id) = by_addr.query_row([address::normalize(addr)], |r| r.get(0)).optional()? {
                people.push(id);
            }
        }
        let mut by_domain = conn.prepare_cached(
            "SELECT id FROM correspondents WHERE addr LIKE ?1 ESCAPE '\\'",
        )?;
        for domain in domains {
            let pattern = format!("%@{}", crate::escape_like(&domain.trim().to_lowercase()));
            for id in by_domain.query_map([pattern], |r| r.get::<_, i64>(0))? {
                people.push(id?);
            }
        }
    }
    people.sort_unstable();
    people.dedup();
    if people.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    // Bound parameters rather than a list spliced into the text, and in
    // chunks under SQLite's limit on them. A company of a thousand addresses
    // is a thousand placeholders, which is past it.
    let mut found: Vec<(Exchange, MailboxRole)> = Vec::new();
    for chunk in people.chunks(500) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT m.id, m.account_id, m.mailbox_id, m.thread_id, m.from_name, m.from_addr,
                    m.subject, m.date_ts, m.snippet, m.flags, m.has_attachments, m.size,
                    p.direction, m.to_addrs, m.rfc_message_id, b.role
             FROM message_people p
             JOIN messages m ON m.id = p.message_id
             JOIN mailboxes b ON b.id = m.mailbox_id
             WHERE p.person IN ({placeholders})
             ORDER BY p.date_ts DESC
             LIMIT {}",
            limit.saturating_mul(2),
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(chunk.iter()), |r| {
            let envelope = crate::read_envelope(r)?;
            let role: String = r.get(15)?;
            Ok((
                Exchange {
                    mailboxes: vec![envelope.mailbox_id],
                    envelope,
                    direction: Direction::from_stored(r.get(12)?),
                    to: address::decode_list(&r.get::<_, String>(13)?),
                    rfc_message_id: r.get(14)?,
                },
                MailboxRole::from_stored(&role),
            ))
        })?;
        for row in rows {
            found.push(row?);
        }
    }
    found.sort_by(|a, b| b.0.envelope.date.cmp(&a.0.envelope.date).then(a.0.envelope.id.cmp(&b.0.envelope.id)));

    Ok(fold_copies(found, limit))
}

/// One entry per message, out of one row per copy and per address.
fn fold_copies(found: Vec<(Exchange, MailboxRole)>, limit: usize) -> Vec<Exchange> {
    let mut out: Vec<(Exchange, MailboxRole)> = Vec::new();
    let mut by_rfc: HashMap<String, usize> = HashMap::new();
    let mut seen_ids: HashSet<i64> = HashSet::new();

    for (exchange, role) in found {
        if !seen_ids.insert(exchange.envelope.id.0) {
            continue;
        }
        // A message with no `Message-ID` cannot be recognised as a copy of
        // anything, so it stands on its own.
        let key = (!exchange.rfc_message_id.is_empty()).then(|| exchange.rfc_message_id.clone());
        match key.as_ref().and_then(|key| by_rfc.get(key)).copied() {
            Some(index) => {
                let (kept, kept_role) = &mut out[index];
                let mailbox = exchange.envelope.mailbox_id;
                if worth(role) > worth(*kept_role) {
                    let mut mailboxes = std::mem::take(&mut kept.mailboxes);
                    mailboxes.insert(0, mailbox);
                    *kept = Exchange { mailboxes, ..exchange };
                    *kept_role = role;
                } else if !kept.mailboxes.contains(&mailbox) {
                    kept.mailboxes.push(mailbox);
                }
            }
            None => {
                if out.len() == limit {
                    continue;
                }
                if let Some(key) = key {
                    by_rfc.insert(key, out.len());
                }
                out.push((exchange, role));
            }
        }
    }

    out.into_iter().map(|(exchange, _)| exchange).collect()
}

/// Which copy of a message to describe, when there are several.
///
/// The folders where a message is being kept beat the ones where it is on its
/// way out: the copy in Trash is the least interesting thing about a message
/// that is also still in the Inbox.
fn worth(role: MailboxRole) -> u8 {
    match role {
        MailboxRole::Inbox | MailboxRole::Sent => 4,
        MailboxRole::Archive => 3,
        MailboxRole::Other => 2,
        MailboxRole::Drafts => 1,
        MailboxRole::Junk | MailboxRole::Trash => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IncomingMessage;
    use spool_core::{AccountId, MailboxId};

    fn mail(account: AccountId, mailbox: MailboxId, uid: u32, from: Address, to: &[Address]) -> IncomingMessage {
        IncomingMessage {
            account_id: account,
            mailbox_id: mailbox,
            uid: Some(uid),
            rfc_message_id: format!("<{uid}@example.com>"),
            from,
            to: to.to_vec(),
            subject: format!("Message {uid}"),
            date: 1_700_000_000 + uid as i64 * 60,
            plain: "hello".into(),
            ..Default::default()
        }
    }

    fn me() -> Address {
        Address::new("Wes", "wes@example.com")
    }

    fn ana() -> Address {
        Address::new("Ana Ruiz", "ana@acme.com")
    }

    fn setup() -> (Store, AccountId, MailboxId, MailboxId) {
        let store = Store::in_memory().unwrap();
        store.set_people_index(true).unwrap();
        let account = store.add_account("Wes", "wes@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let sent = store.upsert_mailbox(account, "Sent", MailboxRole::Sent).unwrap();
        (store, account, inbox, sent)
    }

    fn find<'a>(people: &'a [Correspondent], addr: &str) -> &'a Correspondent {
        people.iter().find(|c| c.addr == addr).unwrap_or_else(|| panic!("{addr} is indexed"))
    }

    #[test]
    fn mail_is_indexed_by_who_is_on_it_and_which_way_it_went() {
        let (store, account, inbox, sent) = setup();
        let bo = Address::new("Bo Chen", "Bo@Acme.com");
        let mut from_ana = mail(account, inbox, 1, ana(), &[me()]);
        from_ana.cc = vec![bo.clone()];
        store.ingest(&[from_ana, mail(account, sent, 2, me(), &[ana()])]).unwrap();

        let people = store.people().unwrap();
        assert!(people.iter().all(|c| c.addr != "wes@example.com"), "you are not in your own book");

        let ana = find(&people, "ana@acme.com");
        assert_eq!((ana.messages, ana.received, ana.sent), (2, 1, 1));
        assert_eq!(ana.name, "Ana Ruiz");
        assert!(ana.is_two_way());
        assert!(ana.last_outbound > ana.last_inbound, "you wrote last");

        // Copied on Ana's message, never written to: there, but not two-way.
        let bo = find(&people, "bo@acme.com");
        assert_eq!((bo.messages, bo.received, bo.sent), (1, 0, 0));
        assert!(!bo.is_two_way());
        assert_eq!(bo.domain(), "acme.com");
    }

    #[test]
    fn mail_filed_in_sent_is_yours_whatever_address_it_came_from() {
        // An alias: sent as an address no account is configured with.
        let (store, account, _, sent) = setup();
        let alias = Address::new("Wes", "hello@example.com");
        store.ingest(&[mail(account, sent, 1, alias, &[ana()])]).unwrap();

        let people = store.people().unwrap();
        assert_eq!(find(&people, "ana@acme.com").sent, 1);
        assert!(people.iter().all(|c| c.addr != "hello@example.com"), "the alias is you too");
    }

    #[test]
    fn a_message_in_two_folders_is_counted_once_and_shown_once() {
        let (store, account, inbox, _) = setup();
        let archive = store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();
        let trash = store.upsert_mailbox(account, "Trash", MailboxRole::Trash).unwrap();
        let first = mail(account, trash, 1, ana(), &[me()]);
        let mut copy = mail(account, inbox, 2, ana(), &[me()]);
        copy.rfc_message_id = first.rfc_message_id.clone();
        copy.date = first.date;
        let mut third = mail(account, archive, 3, ana(), &[me()]);
        third.rfc_message_id = first.rfc_message_id.clone();
        third.date = first.date;
        store.ingest(&[first, copy, third]).unwrap();

        let people = store.people().unwrap();
        assert_eq!(find(&people, "ana@acme.com").messages, 1);

        let history = store.history(&["ana@acme.com".into()], &[], 50).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].envelope.mailbox_id, inbox, "the Inbox copy says more than Trash");
        assert_eq!(history[0].mailboxes.len(), 3, "and the other copies are still accounted for");
    }

    #[test]
    fn a_company_is_everyone_at_its_domain() {
        let (store, account, inbox, sent) = setup();
        let bo = Address::new("Bo", "bo@acme.com");
        let other = Address::new("Cy", "cy@elsewhere.com");
        store
            .ingest(&[
                mail(account, inbox, 1, ana(), &[me()]),
                mail(account, inbox, 2, bo, &[me()]),
                mail(account, sent, 3, me(), &[other]),
            ])
            .unwrap();

        let history = store.history(&[], &["acme.com".into()], 50).unwrap();
        assert_eq!(history.len(), 2);
        assert!(history[0].envelope.date > history[1].envelope.date, "newest first");
        assert!(history.iter().all(|e| e.direction == Direction::Inbound));
    }

    #[test]
    fn deleting_a_message_takes_it_out_of_the_index() {
        let (store, account, inbox, _) = setup();
        let ids = store.ingest(&[mail(account, inbox, 1, ana(), &[me()])]).unwrap();
        store.delete_message(ids[0].id).unwrap();

        assert!(store.history(&["ana@acme.com".into()], &[], 50).unwrap().is_empty());
        assert!(store.people().unwrap().is_empty());
    }

    #[test]
    fn the_backfill_indexes_old_mail_in_batches_and_then_stops() {
        let (store, account, inbox, sent) = setup();
        let mut batch = Vec::new();
        for uid in 1..=25 {
            batch.push(match uid % 2 {
                0 => mail(account, sent, uid, me(), &[ana()]),
                _ => mail(account, inbox, uid, ana(), &[me()]),
            });
        }
        store.ingest(&batch).unwrap();
        let indexed = store.people().unwrap();

        // Wind it back to a store written before the index existed.
        store.reindex_people().unwrap();
        assert!(store.people().unwrap().is_empty());

        let mut calls = 0;
        loop {
            let step = store.backfill_people(10).unwrap();
            calls += 1;
            if step.remaining == 0 {
                break;
            }
            assert!(calls < 10, "the backfill must finish");
        }
        assert_eq!(calls, 3, "25 messages in batches of ten");
        assert_eq!(store.backfill_people(10).unwrap(), Backfill { indexed: 0, remaining: 0 });

        let rebuilt = store.people().unwrap();
        assert_eq!(rebuilt.len(), indexed.len());
        let (before, after) = (find(&indexed, "ana@acme.com"), find(&rebuilt, "ana@acme.com"));
        assert_eq!((before.messages, before.sent, before.received), (after.messages, after.sent, after.received));
    }

    #[test]
    fn mail_arriving_mid_backfill_is_not_indexed_twice() {
        let (store, account, inbox, _) = setup();
        store.ingest(&[mail(account, inbox, 1, ana(), &[me()])]).unwrap();
        store.reindex_people().unwrap();

        // Arrives after the index was emptied: indexed as it lands, and above
        // where the backfill starts.
        store.ingest(&[mail(account, inbox, 2, ana(), &[me()])]).unwrap();
        while store.backfill_people(10).unwrap().remaining > 0 {}

        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").messages, 2);
    }

    #[test]
    fn with_the_index_off_mail_is_stored_and_nobody_is_indexed() {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Wes", "wes@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        store.ingest(&[mail(account, inbox, 1, ana(), &[me()])]).unwrap();

        assert_eq!(store.message_count().unwrap(), 1);
        assert!(store.people().unwrap().is_empty());
        assert_eq!(store.backfill_people(10).unwrap(), Backfill { indexed: 0, remaining: 0 }, "off is off");
    }

    #[test]
    fn switching_the_index_back_on_catches_up_on_what_arrived_while_it_was_off() {
        let (store, account, inbox, _) = setup();
        store.ingest(&[mail(account, inbox, 1, ana(), &[me()])]).unwrap();

        // Off for a while, and mail keeps arriving — above where any backfill
        // would start, so only a rebuild can find it.
        store.set_people_index(false).unwrap();
        store.ingest(&[mail(account, inbox, 2, ana(), &[me()]), mail(account, inbox, 3, ana(), &[me()])]).unwrap();
        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").messages, 1);

        store.set_people_index(true).unwrap();
        // And some more as it comes back, before the backfill has run.
        store.ingest(&[mail(account, inbox, 4, ana(), &[me()])]).unwrap();
        while store.backfill_people(2).unwrap().remaining > 0 {}

        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").messages, 4, "each counted once");
    }

    #[test]
    fn an_index_kept_up_is_not_rebuilt_when_the_app_starts_again() {
        let (store, account, inbox, _) = setup();
        store.ingest(&[mail(account, inbox, 1, ana(), &[me()])]).unwrap();
        while store.backfill_people(10).unwrap().remaining > 0 {}

        // What a relaunch with Clients still on does.
        store.set_people_index(true).unwrap();
        assert_eq!(store.backfill_people(10).unwrap(), Backfill { indexed: 0, remaining: 0 }, "nothing to redo");
        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").messages, 1);
    }

    #[test]
    fn a_notice_you_sent_yourself_is_from_whoever_its_reply_to_names() {
        // A sign-up form on your own site, sending from your address to your
        // address, with the person who signed up where an answer would go.
        let (store, account, _, sent) = setup();
        let mut notice = mail(account, sent, 1, me(), &[me()]);
        notice.reply_to = vec![Address::new("Dee Park", "Dee@Example.org")];
        store.ingest(&[notice]).unwrap();

        let people = store.people().unwrap();
        let dee = find(&people, "dee@example.org");
        assert_eq!((dee.messages, dee.received, dee.sent), (1, 1, 0), "they wrote, by way of the form");
        assert_eq!(dee.name, "Dee Park");
        assert_eq!(people.len(), 1, "and nobody else is on it");
    }

    #[test]
    fn a_notice_without_reply_to_is_from_the_address_in_its_text() {
        let (store, account, _, sent) = setup();
        let mut notice = mail(account, sent, 1, me(), &[me()]);
        notice.plain = "A new user signed up!!! Email: kim@example.org".into();
        // Your own address in the text is still you.
        let mut yours = mail(account, sent, 2, me(), &[me()]);
        yours.plain = "Invitation from wes@example.com to view your screen".into();
        store.ingest(&[notice, yours]).unwrap();

        let people = store.people().unwrap();
        assert_eq!(find(&people, "kim@example.org").received, 1);
        assert_eq!(people.len(), 1, "{people:?}");
    }

    #[test]
    fn reply_to_on_mail_from_anyone_else_adds_nobody() {
        // A newsletter's reply address is not a person who wrote to you.
        let (store, account, inbox, sent) = setup();
        let shop = Address::new("Shop", "news@shop.example");
        let mut letter = mail(account, inbox, 1, shop, &[me()]);
        letter.reply_to = vec![Address::bare("replies@shop.example")];
        letter.plain = "Write to help@shop.example with questions".into();
        // Nor is one on mail you sent to somebody.
        let mut to_ana = mail(account, sent, 2, me(), &[ana()]);
        to_ana.reply_to = vec![Address::bare("desk@example.org")];
        store.ingest(&[letter, to_ana]).unwrap();

        let mut addrs: Vec<String> = store.people().unwrap().into_iter().map(|c| c.addr).collect();
        addrs.sort();
        assert_eq!(addrs, ["ana@acme.com", "news@shop.example"]);
    }

    #[test]
    fn an_address_is_found_in_text_and_one_cut_short_is_not() {
        assert_eq!(addresses_in("Email: Kim@Example.org. Thanks!"), ["kim@example.org"]);
        assert_eq!(addresses_in("<div>mailto:ana@acme.co.uk</div>"), ["ana@acme.co.uk"]);
        assert_eq!(addresses_in("Contact (bo@acme.com), or cy@acme.com!"), ["bo@acme.com", "cy@acme.com"]);
        // Where the snippet ended.
        assert!(addresses_in("invited to the desktop of wes@studio\u{2026}").is_empty());
        assert!(addresses_in("invited to the desktop of wes@studio.co\u{2026}").is_empty());
        // Things with an @ in them that are not addresses.
        assert!(addresses_in("@everyone, meet at 5 @ the office; name@localhost").is_empty());
    }

    #[test]
    fn an_index_built_by_older_rules_is_built_again() {
        let (store, account, _, sent) = setup();
        let mut notice = mail(account, sent, 1, me(), &[me()]);
        notice.plain = "A new user signed up!!! Email: kim@example.org".into();
        store.ingest(&[notice]).unwrap();
        // What an index kept up by the build before this one looks like: the
        // notice was indexed, under rules that found nobody on it.
        {
            let this = store.lock();
            this.conn.execute_batch("DELETE FROM message_people; DELETE FROM correspondents;").unwrap();
            set_meta(&this.conn, LIVE, 1).unwrap();
        }

        store.set_people_index(true).unwrap();
        while store.backfill_people(10).unwrap().remaining > 0 {}
        assert_eq!(find(&store.people().unwrap(), "kim@example.org").received, 1);
    }

    #[test]
    fn the_newest_name_wins_and_a_bare_address_never_erases_one() {
        let (store, account, inbox, _) = setup();
        store.ingest(&[mail(account, inbox, 1, Address::new("'Ana'", "ana@acme.com"), &[me()])]).unwrap();
        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").name, "Ana");

        store.ingest(&[mail(account, inbox, 2, Address::bare("ana@acme.com"), &[me()])]).unwrap();
        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").name, "Ana");

        store.ingest(&[mail(account, inbox, 3, ana(), &[me()])]).unwrap();
        assert_eq!(find(&store.people().unwrap(), "ana@acme.com").name, "Ana Ruiz");
    }
}
