//! Mail you sent that is waiting for an answer.
//!
//! One row per sent message, kept by its Message-ID rather than by a message
//! row: the copy filed in Sent can fail to be written, or be replaced by the
//! server's own, and the ID is what a reply quotes either way.
//!
//! A row is deleted, not marked, once it is finished with — an answer came,
//! it was dismissed, or it turned out not to want one — so the table only ever
//! holds what is still waiting or due.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use spool_core::{AccountId, MessageId};

use crate::Store;

/// A sent message being watched for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reminder {
    /// The sent message's Message-ID, without angle brackets.
    pub message_id: String,
    pub account: AccountId,
    pub subject: String,
    /// Who it went to, as the list names them: `Ana Ruiz and 2 others`.
    pub sent_to: String,
    pub sent_at: i64,
    /// When it comes back if nothing has arrived.
    pub due_at: i64,
    /// The wait was chosen by hand, so it comes back without being asked
    /// whether it expected an answer.
    pub asked: bool,
    /// Past its wait, with no answer, and wanting one: shown.
    pub due: bool,
    /// A follow-up ready to send, when one was written.
    pub nudge: String,
}

const COLUMNS: &str = "message_id, account_id, subject, sent_to, sent_at, due_at, asked, state, nudge";

fn read_reminder(row: &rusqlite::Row<'_>) -> rusqlite::Result<Reminder> {
    Ok(Reminder {
        message_id: row.get(0)?,
        account: AccountId(row.get(1)?),
        subject: row.get(2)?,
        sent_to: row.get(3)?,
        sent_at: row.get(4)?,
        due_at: row.get(5)?,
        asked: row.get::<_, i64>(6)? != 0,
        due: row.get::<_, i64>(7)? != 0,
        nudge: row.get(8)?,
    })
}

/// A Message-ID as reminders keep it: what is between the angle brackets.
pub fn bare_message_id(id: &str) -> String {
    id.trim().trim_matches(['<', '>']).trim().to_string()
}

impl Store {
    /// Starts watching a sent message. Watching it again starts it over.
    pub fn remind(&self, reminder: &Reminder) -> Result<()> {
        let id = bare_message_id(&reminder.message_id);
        if id.is_empty() {
            return Ok(());
        }
        let this = self.lock();
        this.conn.execute(
            &format!("INSERT OR REPLACE INTO follow_ups ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)"),
            params![
                id,
                reminder.account.0,
                reminder.subject,
                reminder.sent_to,
                reminder.sent_at,
                reminder.due_at,
                reminder.asked as i64,
                reminder.due as i64,
                reminder.nudge,
            ],
        )?;
        Ok(())
    }

    /// Stops watching whatever has had an answer, and says how many that was.
    ///
    /// An answer is anything later in the same conversation: a message that
    /// quotes the sent one in its threading headers, or one filed in its
    /// thread. Your own later message counts too — following up, or carrying
    /// on in another reply, is the conversation moving, and the newer message
    /// is watched in its own right.
    pub fn settle_reminders(&self) -> Result<usize> {
        let this = self.lock();
        let settled = this.conn.execute(
            "DELETE FROM follow_ups WHERE EXISTS (
                 SELECT 1 FROM messages r
                 WHERE r.account_id = follow_ups.account_id
                   AND r.date_ts > follow_ups.sent_at
                   AND instr(r.rfc_message_id, follow_ups.message_id) = 0
                   AND (instr(r.in_reply_to, follow_ups.message_id) > 0
                        OR instr(r.refs, follow_ups.message_id) > 0
                        OR r.thread_id IN (
                            SELECT s.thread_id FROM messages s
                            WHERE s.rfc_message_id IN
                                  (follow_ups.message_id, '<' || follow_ups.message_id || '>')
                        ))
             )",
            [],
        )?;
        Ok(settled)
    }

    /// Reminders whose wait is over and that have not been looked at yet,
    /// longest overdue first.
    pub fn reminders_to_check(&self, now: i64, limit: usize) -> Result<Vec<Reminder>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM follow_ups WHERE state = 0 AND due_at <= ?1 ORDER BY due_at LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![now, limit as i64], read_reminder)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Brings a reminder back, with the follow-up written for it.
    pub fn reminder_due(&self, message_id: &str, nudge: &str) -> Result<()> {
        let this = self.lock();
        this.conn.execute(
            "UPDATE follow_ups SET state = 1, nudge = ?2 WHERE message_id = ?1",
            params![bare_message_id(message_id), nudge],
        )?;
        Ok(())
    }

    /// Stops watching a sent message: dismissed, or not wanting an answer.
    pub fn drop_reminder(&self, message_id: &str) -> Result<()> {
        let this = self.lock();
        this.conn.execute("DELETE FROM follow_ups WHERE message_id = ?1", params![bare_message_id(message_id)])?;
        Ok(())
    }

    /// What is due, for one account or every one, longest waiting first.
    pub fn due_reminders(&self, account: Option<AccountId>) -> Result<Vec<Reminder>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM follow_ups
             WHERE state = 1 AND (?1 IS NULL OR account_id = ?1)
             ORDER BY sent_at"
        ))?;
        let rows = stmt.query_map(params![account.map(|a| a.0)], read_reminder)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The copy of a sent message kept here, for writing its follow-up
    /// against: the one in Sent when there are several.
    pub fn sent_copy(&self, message_id: &str) -> Result<Option<MessageId>> {
        let id = bare_message_id(message_id);
        let this = self.lock();
        let found = this
            .conn
            .query_row(
                "SELECT m.id FROM messages m JOIN mailboxes b ON b.id = m.mailbox_id
                 WHERE m.rfc_message_id IN (?1, '<' || ?1 || '>')
                 ORDER BY b.role = 'sent' DESC, m.id
                 LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.map(MessageId))
    }
}

#[cfg(test)]
mod tests {
    use spool_core::{Address, MailboxRole};

    use super::*;
    use crate::IncomingMessage;

    const SENT_AT: i64 = 1_700_000_000;
    const DAY: i64 = 86_400;

    struct Fixture {
        store: Store,
        account: AccountId,
        inbox: spool_core::MailboxId,
    }

    /// A store holding one sent message, `<sent-1@example.com>`, watched.
    fn fixture() -> Fixture {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Me", "me@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let sent = store.upsert_mailbox(account, "Sent", MailboxRole::Sent).unwrap();
        store
            .ingest(&[IncomingMessage {
                account_id: account,
                mailbox_id: sent,
                uid: None,
                rfc_message_id: "<sent-1@example.com>".into(),
                from: Address::new("Me", "me@example.com"),
                subject: "Pricing".into(),
                date: SENT_AT,
                plain: "What would it cost?".into(),
                ..Default::default()
            }])
            .unwrap();
        store.remind(&reminder("sent-1@example.com", account, SENT_AT + 3 * DAY)).unwrap();
        Fixture { store, account, inbox }
    }

    fn reminder(id: &str, account: AccountId, due_at: i64) -> Reminder {
        Reminder {
            message_id: id.into(),
            account,
            subject: "Pricing".into(),
            sent_to: "Ana Ruiz".into(),
            sent_at: SENT_AT,
            due_at,
            asked: false,
            due: false,
            nudge: String::new(),
        }
    }

    fn arrives(f: &Fixture, id: &str, subject: &str, date: i64, in_reply_to: &str, references: Vec<String>) {
        f.store
            .ingest(&[IncomingMessage {
                account_id: f.account,
                mailbox_id: f.inbox,
                uid: Some(date as u32),
                rfc_message_id: id.into(),
                from: Address::new("Ana", "ana@example.com"),
                subject: subject.into(),
                date,
                in_reply_to: in_reply_to.into(),
                references,
                plain: "About that".into(),
                ..Default::default()
            }])
            .unwrap();
    }

    #[test]
    fn a_reminder_is_checked_only_once_its_wait_is_over() {
        let f = fixture();
        assert!(f.store.reminders_to_check(SENT_AT + DAY, 10).unwrap().is_empty());
        let ready = f.store.reminders_to_check(SENT_AT + 3 * DAY, 10).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].message_id, "sent-1@example.com");
    }

    #[test]
    fn a_due_reminder_is_shown_and_not_checked_again() {
        let f = fixture();
        f.store.reminder_due("<sent-1@example.com>", "Any thoughts?").unwrap();
        assert!(f.store.reminders_to_check(SENT_AT + 9 * DAY, 10).unwrap().is_empty());
        let due = f.store.due_reminders(Some(f.account)).unwrap();
        assert_eq!(due.len(), 1);
        assert!(due[0].due);
        assert_eq!(due[0].nudge, "Any thoughts?");
        assert_eq!(f.store.due_reminders(None).unwrap().len(), 1);
        assert!(f.store.due_reminders(Some(AccountId(f.account.0 + 1))).unwrap().is_empty());
    }

    #[test]
    fn a_reply_quoting_the_message_settles_it() {
        let f = fixture();
        arrives(&f, "<r1@example.com>", "Re: Pricing", SENT_AT + DAY, "<sent-1@example.com>", vec![]);
        assert_eq!(f.store.settle_reminders().unwrap(), 1);
        assert!(f.store.reminders_to_check(SENT_AT + 9 * DAY, 10).unwrap().is_empty());
    }

    #[test]
    fn a_reply_further_down_the_thread_settles_it_too() {
        let f = fixture();
        arrives(
            &f,
            "<r2@example.com>",
            "Re: Pricing",
            SENT_AT + DAY,
            "<someone-else@example.com>",
            vec!["<sent-1@example.com>".into(), "<someone-else@example.com>".into()],
        );
        assert_eq!(f.store.settle_reminders().unwrap(), 1);
    }

    #[test]
    fn mail_that_is_not_an_answer_leaves_it_waiting() {
        let f = fixture();
        // Unrelated, and later.
        arrives(&f, "<other@example.com>", "Lunch?", SENT_AT + DAY, "", vec![]);
        // Related, but from before it was sent: what it was answering.
        arrives(&f, "<before@example.com>", "Pricing", SENT_AT - DAY, "", vec!["<sent-1@example.com>".into()]);
        assert_eq!(f.store.settle_reminders().unwrap(), 0);
        assert_eq!(f.store.reminders_to_check(SENT_AT + 3 * DAY, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_reply_without_threading_headers_is_known_by_its_subject() {
        // Spool's threading files a headerless "Re: Pricing" with "Pricing",
        // and so does this.
        let f = fixture();
        arrives(&f, "<r3@example.com>", "Re: Pricing", SENT_AT + DAY, "", vec![]);
        assert_eq!(f.store.settle_reminders().unwrap(), 1);
    }

    #[test]
    fn the_sent_copy_is_found_by_either_spelling_of_its_id() {
        let f = fixture();
        let bare = f.store.sent_copy("sent-1@example.com").unwrap();
        assert!(bare.is_some());
        assert_eq!(f.store.sent_copy("<sent-1@example.com>").unwrap(), bare);
        assert_eq!(f.store.sent_copy("nope@example.com").unwrap(), None);
    }

    #[test]
    fn a_dropped_reminder_is_gone() {
        let f = fixture();
        f.store.drop_reminder("sent-1@example.com").unwrap();
        assert!(f.store.reminders_to_check(SENT_AT + 9 * DAY, 10).unwrap().is_empty());
    }
}
