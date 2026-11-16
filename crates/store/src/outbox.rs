//! The queue of changes owed to the server.
//!
//! Archiving a message is two writes, not one: the local row moves so the list
//! updates in the same frame, and a note goes in here so the same thing
//! eventually happens on IMAP. Without the second write the first is a lie —
//! the next sync pulls the message back into the inbox and the archive
//! silently undoes itself, which is the failure that costs people trust in a
//! mail client faster than a missing feature ever does.
//!
//! Two things make this a queue rather than a network call:
//!
//! * **Offline.** Mail gets read on a train. An action taken with no
//!   connection has to survive until there is one.
//! * **Ordering.** Flagging a message and then moving it are two operations on
//!   the same message in two different folders. Replayed out of order the
//!   second one addresses a UID that no longer exists there.
//!
//! Everything stored here is in *server* coordinates — a folder path and a
//! UID, captured at the moment the action was taken — not local row ids. The
//! local move destroys the UID (a message has a different one in every folder)
//! and the local delete destroys the row, so by the time this is drained there
//! is nothing left to look up.

use anyhow::Result;
use spool_core::{AccountId, Flags, MailboxId, MessageId};
use rusqlite::{OptionalExtension, Row, Transaction, params};

use crate::Store;

/// How many times an operation is tried before it stops being retried.
///
/// It is kept rather than dropped: a move that never reached the server is
/// something the person who made it should be able to find out about, and a
/// silently discarded one is indistinguishable from one that worked.
pub const MAX_ATTEMPTS: u32 = 5;

/// What is owed to the server for one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpKind {
    /// Replace the server's flags with these. Not a delta: the local flags are
    /// the whole truth about a message the user just changed, and `+FLAGS`
    /// cannot express turning one off.
    Flags(Flags),
    /// Move to another folder in the same account, by path.
    Move { to: String },
    /// Really delete: `\Deleted` and an expunge.
    Delete,
}

impl OpKind {
    /// The discriminant as it is stored, so a value never depends on the
    /// order of the enum.
    fn tag(&self) -> &'static str {
        match self {
            OpKind::Flags(_) => "flags",
            OpKind::Move { .. } => "move",
            OpKind::Delete => "delete",
        }
    }
}

/// One queued operation, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub id: i64,
    pub account_id: AccountId,
    /// The folder the UID lives in, as the server spells it.
    pub folder: String,
    pub uid: u32,
    pub kind: OpKind,
    /// How many times this has been handed out, including now.
    pub attempts: u32,
}

/// An operation that ran out of attempts, for the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedOp {
    pub account_id: AccountId,
    pub folder: String,
    pub kind: OpKind,
    pub last_error: String,
}

impl Store {
    /// Queues a flag change for a message, given its server coordinates.
    ///
    /// Called from inside the store's own writes rather than by the UI: the
    /// UID is read off the row being changed, and after `move_message` or
    /// `delete_message` there is no row left to read it from.
    pub(crate) fn queue_flags(tx: &Transaction<'_>, id: MessageId, flags: Flags) -> Result<()> {
        let Some((account_id, folder, uid)) = coordinates(tx, id)? else { return Ok(()) };
        queue(tx, account_id, &folder, uid, OpKind::Flags(flags))
    }

    pub(crate) fn queue_move(tx: &Transaction<'_>, id: MessageId, to: MailboxId) -> Result<()> {
        let Some((account_id, folder, uid)) = coordinates(tx, id)? else { return Ok(()) };
        let target: Option<String> = tx
            .query_row("SELECT path FROM mailboxes WHERE id = ?1", [to.0], |r| r.get(0))
            .optional()?;
        // A destination with no path is not a folder anything can be moved
        // into; the local move still stands, and the next sync will correct
        // it.
        let Some(target) = target else { return Ok(()) };
        if target == folder {
            return Ok(());
        }
        queue(tx, account_id, &folder, uid, OpKind::Move { to: target })
    }

    pub(crate) fn queue_delete(tx: &Transaction<'_>, id: MessageId) -> Result<()> {
        let Some((account_id, folder, uid)) = coordinates(tx, id)? else { return Ok(()) };
        queue(tx, account_id, &folder, uid, OpKind::Delete)
    }

    /// Hands out the work owed for one account, oldest first, and counts each
    /// row as attempted.
    ///
    /// Claiming on read rather than on success is what keeps a crash mid-drain
    /// from replaying the same operation forever, and it is why coalescing
    /// below only touches rows nobody has taken yet.
    pub fn claim_pending(&self, account_id: AccountId, limit: usize) -> Result<Vec<PendingOp>> {
        let mut this = self.lock();
        let tx = this.conn.transaction()?;

        let ops = {
            let mut stmt = tx.prepare(
                "SELECT id, account_id, folder, uid, kind, flags, target, attempts
                 FROM outbox
                 WHERE account_id = ?1 AND attempts < ?2
                 ORDER BY id
                 LIMIT ?3",
            )?;
            let rows = stmt.query_map(params![account_id.0, MAX_ATTEMPTS, limit as i64], read_op)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        for op in &ops {
            tx.execute("UPDATE outbox SET attempts = attempts + 1 WHERE id = ?1", [op.id])?;
        }
        tx.commit()?;

        // The value handed back is what the row now says, not what it said
        // when it was read.
        Ok(ops.into_iter().map(|mut op| {
            op.attempts += 1;
            op
        }).collect())
    }

    /// Whether anything at all is waiting, without claiming it.
    ///
    /// Cheap enough to ask before opening a connection, which is the point:
    /// most syncs owe the server nothing.
    pub fn pending_count(&self, account_id: AccountId) -> Result<usize> {
        let this = self.lock();
        let count: i64 = this.conn.query_row(
            "SELECT COUNT(*) FROM outbox WHERE account_id = ?1 AND attempts < ?2",
            params![account_id.0, MAX_ATTEMPTS],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// The operation reached the server. Nothing more is owed.
    pub fn op_done(&self, id: i64) -> Result<()> {
        let this = self.lock();
        this.conn.execute("DELETE FROM outbox WHERE id = ?1", [id])?;
        Ok(())
    }

    /// The operation did not reach the server, with the reason.
    ///
    /// The attempt was already counted when the row was claimed, so this only
    /// records why — a failure that never returns here (a crash, a dropped
    /// connection) still counts, which is what stops a poisoned operation
    /// from retrying forever.
    pub fn op_failed(&self, id: i64, error: &str) -> Result<()> {
        let this = self.lock();
        this.conn.execute(
            "UPDATE outbox SET last_error = ?2 WHERE id = ?1",
            params![id, truncate(error)],
        )?;
        Ok(())
    }

    /// Operations that have run out of attempts, so the UI can say so.
    pub fn failed_ops(&self) -> Result<Vec<FailedOp>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare(
            "SELECT id, account_id, folder, uid, kind, flags, target, attempts, last_error
             FROM outbox
             WHERE attempts >= ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map([MAX_ATTEMPTS], |row| {
            let op = read_op(row)?;
            Ok(FailedOp {
                account_id: op.account_id,
                folder: op.folder,
                kind: op.kind,
                last_error: row.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Forgets everything queued for an account.
    ///
    /// For the case where the queue itself is the problem — a folder that no
    /// longer exists, a server that rejects an operation it will never
    /// accept — so the person can stop being told about it.
    pub fn clear_outbox(&self, account_id: AccountId) -> Result<usize> {
        let this = self.lock();
        Ok(this.conn.execute("DELETE FROM outbox WHERE account_id = ?1", [account_id.0])?)
    }
}

/// Where a message lives on the server, or `None` if it does not live there.
///
/// A row with no UID is one that has never been to the server or has already
/// been moved locally and not yet re-synced. There is nothing to address it
/// with, so nothing is queued — the local change stands, and the next sync is
/// what makes it agree.
fn coordinates(tx: &Transaction<'_>, id: MessageId) -> Result<Option<(AccountId, String, u32)>> {
    let row: Option<(i64, String, Option<i64>)> = tx
        .query_row(
            "SELECT m.account_id, b.path, m.uid
             FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
             WHERE m.id = ?1",
            [id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    Ok(match row {
        Some((account_id, folder, Some(uid))) if uid > 0 => {
            Some((AccountId(account_id), folder, uid as u32))
        }
        _ => None,
    })
}

/// Appends an operation, or folds it into the one already at the end.
///
/// Coalescing is only ever right for flags, and only when the newest thing
/// queued for that message is also a flag change: pressing `u` five times owes
/// the server one `STORE`, but a flag change queued *after* a move belongs in
/// the destination folder and must not be folded back into the source.
fn queue(
    tx: &Transaction<'_>,
    account_id: AccountId,
    folder: &str,
    uid: u32,
    kind: OpKind,
) -> Result<()> {
    if let OpKind::Flags(flags) = &kind {
        let folded = tx.execute(
            "UPDATE outbox SET flags = ?3, queued_at = ?4
             WHERE id = (
                 SELECT id FROM outbox
                 WHERE folder = ?1 AND uid = ?2 AND attempts = 0
                 ORDER BY id DESC LIMIT 1
             )
             AND kind = 'flags'",
            params![folder, uid, flags.0, now()],
        )?;
        if folded > 0 {
            return Ok(());
        }
    }

    let (flags, target) = match &kind {
        OpKind::Flags(flags) => (flags.0 as i64, String::new()),
        OpKind::Move { to } => (0, to.clone()),
        OpKind::Delete => (0, String::new()),
    };

    tx.execute(
        "INSERT INTO outbox (account_id, folder, uid, kind, flags, target, queued_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![account_id.0, folder, uid, kind.tag(), flags, target, now()],
    )?;
    Ok(())
}

fn read_op(row: &Row<'_>) -> rusqlite::Result<PendingOp> {
    let tag: String = row.get(4)?;
    let kind = match tag.as_str() {
        "move" => OpKind::Move { to: row.get(6)? },
        "delete" => OpKind::Delete,
        // Anything unrecognised reads as a flag change of exactly the flags
        // stored, which is the harmless interpretation: a `STORE` that sets a
        // message to what it already is.
        _ => OpKind::Flags(Flags(row.get::<_, i64>(5)? as u32)),
    };

    Ok(PendingOp {
        id: row.get(0)?,
        account_id: AccountId(row.get(1)?),
        folder: row.get(2)?,
        uid: row.get::<_, i64>(3)? as u32,
        kind,
        attempts: row.get::<_, i64>(7)? as u32,
    })
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Server errors can be long; the column is for a person reading a status
/// line, not a log.
fn truncate(error: &str) -> String {
    const LIMIT: usize = 300;
    match error.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}\u{2026}", &error[..cut]),
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IncomingMessage;
    use spool_core::{Address, MailboxRole};

    /// An account with an inbox and an archive, and one message in the inbox.
    fn store() -> (Store, AccountId, MailboxId, MailboxId, MessageId) {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let archive = store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();

        store
            .ingest(&[IncomingMessage {
                account_id: account,
                mailbox_id: inbox,
                uid: Some(42),
                rfc_message_id: "<one@example.com>".into(),
                from: Address::new("Ana", "ana@example.com"),
                subject: "Hello".into(),
                date: 1_700_000_000,
                plain: "text".into(),
                ..Default::default()
            }])
            .unwrap();

        let id = store.envelopes(crate::Scope::Mailbox(inbox), &Default::default(), 10).unwrap()[0].id;
        (store, account, inbox, archive, id)
    }

    fn claim(store: &Store, account: AccountId) -> Vec<PendingOp> {
        store.claim_pending(account, 100).unwrap()
    }

    #[test]
    fn changing_flags_owes_the_server_a_store() {
        let (store, account, _, _, id) = store();
        store.set_flags(id, Flags(Flags::SEEN)).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].folder, "INBOX");
        assert_eq!(ops[0].uid, 42);
        assert_eq!(ops[0].kind, OpKind::Flags(Flags(Flags::SEEN)));
    }

    #[test]
    fn toggling_a_flag_repeatedly_owes_the_server_one_store() {
        // Pressing `u` five times is one change of mind, not five round
        // trips, and the queue should say what the message ended up as.
        let (store, account, _, _, id) = store();
        for _ in 0..3 {
            store.set_flags(id, Flags(Flags::SEEN)).unwrap();
            store.set_flags(id, Flags::default()).unwrap();
        }
        store.set_flags(id, Flags(Flags::FLAGGED)).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 1, "one message, one pending flag change");
        assert_eq!(ops[0].kind, OpKind::Flags(Flags(Flags::FLAGGED)));
    }

    #[test]
    fn a_flag_change_after_a_move_is_not_folded_back_into_the_old_folder() {
        // The move is the point of no return: anything queued after it is
        // about a message that no longer exists at that UID. Folding a later
        // flag change into the earlier entry would send it to the folder the
        // message just left.
        let (store, account, _, archive, id) = store();
        store.set_flags(id, Flags(Flags::SEEN)).unwrap();
        store.move_message(id, archive).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].kind, OpKind::Flags(Flags(Flags::SEEN)));
        assert_eq!(ops[1].kind, OpKind::Move { to: "Archive".into() });
        assert_eq!(ops[1].folder, "INBOX", "moves are addressed from the source");
    }

    #[test]
    fn a_move_captures_the_uid_the_local_row_is_about_to_lose() {
        // The reason the queue is written inside the store's own transaction
        // rather than by the caller afterwards: one statement later there is
        // no UID left to record.
        let (store, account, _, archive, id) = store();
        store.move_message(id, archive).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 1);
        assert_eq!((ops[0].folder.as_str(), ops[0].uid), ("INBOX", 42));
        assert_eq!(ops[0].kind, OpKind::Move { to: "Archive".into() });
    }

    #[test]
    fn deleting_a_message_leaves_the_instruction_behind() {
        // The row is gone and the outbox row must not go with it — it is now
        // the only record that the deletion was ever asked for.
        let (store, account, _, _, id) = store();
        store.delete_message(id).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].kind, OpKind::Delete);
        assert_eq!(ops[0].uid, 42);
    }

    #[test]
    fn a_message_the_server_has_never_seen_owes_it_nothing() {
        let (store, account, inbox, _, _) = store();
        store
            .ingest(&[IncomingMessage {
                account_id: account,
                mailbox_id: inbox,
                uid: None,
                rfc_message_id: "<local@example.com>".into(),
                subject: "Local only".into(),
                date: 1,
                ..Default::default()
            }])
            .unwrap();

        let local = store
            .envelopes(crate::Scope::Mailbox(inbox), &Default::default(), 10)
            .unwrap()
            .into_iter()
            .find(|e| e.subject == "Local only")
            .unwrap();

        store.set_flags(local.id, Flags(Flags::SEEN)).unwrap();
        assert!(claim(&store, account).is_empty());
    }

    #[test]
    fn claiming_counts_the_attempt_so_a_crash_cannot_replay_forever() {
        let (store, account, _, archive, id) = store();
        store.move_message(id, archive).unwrap();

        for expected in 1..=MAX_ATTEMPTS {
            let ops = claim(&store, account);
            assert_eq!(ops.len(), 1, "attempt {expected}");
            assert_eq!(ops[0].attempts, expected);
            store.op_failed(ops[0].id, "server said no").unwrap();
        }

        assert!(claim(&store, account).is_empty(), "gives up after {MAX_ATTEMPTS}");
        assert_eq!(store.pending_count(account).unwrap(), 0);

        // Kept, not dropped: an action that never reached the server is
        // something its owner is entitled to find out about.
        let failed = store.failed_ops().unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].kind, OpKind::Move { to: "Archive".into() });
        assert_eq!(failed[0].last_error, "server said no");
    }

    #[test]
    fn a_claimed_operation_is_not_altered_underneath_the_drain() {
        // Coalescing touches only what nobody has taken. Folding a new value
        // into an entry that is already on the wire would lose it the moment
        // that entry completes and is deleted.
        let (store, account, _, _, id) = store();
        store.set_flags(id, Flags(Flags::SEEN)).unwrap();

        let claimed = claim(&store, account);
        assert_eq!(claimed.len(), 1);

        store.set_flags(id, Flags(Flags::FLAGGED)).unwrap();
        store.op_done(claimed[0].id).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 1, "the second change survived the first completing");
        assert_eq!(ops[0].kind, OpKind::Flags(Flags(Flags::FLAGGED)));
    }

    #[test]
    fn marking_everything_read_queues_only_what_is_actually_unread() {
        let (store, account, inbox, _, id) = store();
        store
            .ingest(&[IncomingMessage {
                account_id: account,
                mailbox_id: inbox,
                uid: Some(43),
                rfc_message_id: "<two@example.com>".into(),
                subject: "Second".into(),
                date: 2,
                flags: Flags(Flags::SEEN),
                ..Default::default()
            }])
            .unwrap();

        // The first message is unread; the second arrived read.
        let _ = id;
        store.mark_all_read(crate::Scope::Mailbox(inbox)).unwrap();

        let ops = claim(&store, account);
        assert_eq!(ops.len(), 1, "only the unread one owes the server anything");
        assert_eq!(ops[0].uid, 42);
        assert_eq!(ops[0].kind, OpKind::Flags(Flags(Flags::SEEN)));
    }

    #[test]
    fn removing_an_account_takes_its_queue_with_it() {
        let (store, account, _, archive, id) = store();
        store.move_message(id, archive).unwrap();
        store.delete_account(account).unwrap();
        assert!(claim(&store, account).is_empty());
    }
}
