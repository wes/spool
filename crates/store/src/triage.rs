//! Which mail needs you: the Priority list's verdicts, and the replies kept
//! ready for the mail that wants one.
//!
//! A verdict is kept per message row, and a row keeps its id when the message
//! is filed, so archiving something takes it out of Priority by taking it out
//! of the inbox — the list is "sorted as needing you, still in an inbox, not
//! yet answered", worked out by the query rather than kept up to date by
//! hand.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use spool_core::priority::{Kind, Learned, QuickReply};
use spool_core::{Flags, MessageId};

use crate::{Store, now_seconds};

/// Who decided where a message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortedBy {
    Model,
    /// Without asking a model: mail from a mailing list, or from yourself.
    Rule,
    /// By hand. Never overwritten by either of the others, and what the
    /// next sort is told about.
    Person,
}

impl SortedBy {
    fn code(self) -> i64 {
        match self {
            SortedBy::Model => 0,
            SortedBy::Rule => 1,
            SortedBy::Person => 2,
        }
    }

    fn from_code(code: i64) -> Self {
        match code {
            2 => SortedBy::Person,
            1 => SortedBy::Rule,
            _ => SortedBy::Model,
        }
    }
}

/// A message's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Triage {
    pub kind: Kind,
    /// What it needs from you, in a few words. Empty for [`Kind::Other`].
    pub reason: String,
    pub by: SortedBy,
}

/// The condition a message meets to be in the Priority list, for a query
/// over `messages m`.
///
/// Answered mail leaves it, because answering was what it was waiting for;
/// mail filed out of the inbox leaves it, because filing it is how people
/// say they are done with it.
pub(crate) fn in_priority_sql() -> String {
    format!(
        "m.id IN (SELECT message_id FROM triage WHERE kind > 0) \
         AND m.mailbox_id IN (SELECT id FROM mailboxes WHERE role = 'inbox') \
         AND m.flags & {} = 0",
        Flags::ANSWERED
    )
}

/// Separates one reply from the next, and a label from its text, where they
/// are stored. Control characters, so no text a model writes can contain
/// them once they are stripped out of it.
const BETWEEN_REPLIES: char = '\u{1e}';
const AFTER_LABEL: char = '\u{1f}';

impl Store {
    /// Inbox mail from `since` on that has not been sorted, newest first.
    ///
    /// Only inboxes: what is in a folder was put there, by someone or a
    /// rule, and has been dealt with. Drafts are your own.
    pub fn untriaged(&self, since: i64, limit: usize) -> Result<Vec<MessageId>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(&format!(
            "SELECT m.id FROM messages m
             WHERE m.mailbox_id IN (SELECT id FROM mailboxes WHERE role = 'inbox')
               AND m.date_ts >= ?1
               AND m.flags & {} = 0
               AND NOT EXISTS (SELECT 1 FROM triage t WHERE t.message_id = m.id)
             ORDER BY m.date_ts DESC
             LIMIT ?2",
            Flags::DRAFT
        ))?;
        let ids = stmt
            .query_map(params![since, limit as i64], |r| r.get::<_, i64>(0))?
            .map(|id| id.map(MessageId))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Records where a message goes. A decision made by hand stands: a
    /// model's or a rule's arriving after it is ignored.
    pub fn set_triage(&self, id: MessageId, triage: &Triage) -> Result<()> {
        let this = self.lock();
        this.conn.execute(
            "INSERT INTO triage (message_id, kind, reason, sorted_by, decided_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(message_id) DO UPDATE SET
                 kind = excluded.kind,
                 reason = excluded.reason,
                 sorted_by = excluded.sorted_by,
                 decided_at = excluded.decided_at
             WHERE triage.sorted_by != 2 OR excluded.sorted_by = 2",
            params![id.0, triage.kind.code(), triage.reason, triage.by.code(), now_seconds()],
        )?;
        Ok(())
    }

    pub fn triage(&self, id: MessageId) -> Result<Option<Triage>> {
        let this = self.lock();
        let found = this
            .conn
            .query_row(
                "SELECT kind, reason, sorted_by FROM triage WHERE message_id = ?1",
                [id.0],
                |r| {
                    Ok(Triage {
                        kind: Kind::from_code(r.get(0)?),
                        reason: r.get(1)?,
                        by: SortedBy::from_code(r.get(2)?),
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    /// Senders whose mail was moved in or out of Priority by hand, most
    /// recent first, one entry per sender: the latest decision about them.
    pub fn learned(&self, limit: usize) -> Result<Vec<Learned>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(
            "SELECT lower(m.from_addr), t.kind FROM triage t
             JOIN messages m ON m.id = t.message_id
             WHERE t.sorted_by = 2 AND m.from_addr != ''
             ORDER BY t.decided_at DESC, t.message_id DESC",
        )?;
        let mut learned: Vec<Learned> = Vec::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (sender, kind) = row?;
            if learned.iter().any(|l| l.sender == sender) {
                continue;
            }
            learned.push(Learned { sender, priority: Kind::from_code(kind).is_priority() });
            if learned.len() == limit {
                break;
            }
        }
        Ok(learned)
    }

    /// Priority mail that wants an answer and has no replies ready for it,
    /// from `since` on, newest first.
    pub fn wanting_quick_replies(&self, since: i64, limit: usize) -> Result<Vec<MessageId>> {
        let this = self.lock();
        let mut stmt = this.conn.prepare_cached(&format!(
            "SELECT m.id FROM messages m
             JOIN triage t ON t.message_id = m.id
             WHERE t.kind = {} AND {}
               AND m.date_ts >= ?1
               AND NOT EXISTS (SELECT 1 FROM quick_replies q WHERE q.message_id = m.id)
             ORDER BY m.date_ts DESC
             LIMIT ?2",
            Kind::Reply.code(),
            in_priority_sql()
        ))?;
        let ids = stmt
            .query_map(params![since, limit as i64], |r| r.get::<_, i64>(0))?
            .map(|id| id.map(MessageId))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    /// Keeps the replies offered for a message, in place of any it had. An
    /// empty list is kept too: it means "asked, and nothing came back", which
    /// is not worth asking again on every sync.
    pub fn set_quick_replies(&self, id: MessageId, replies: &[QuickReply]) -> Result<()> {
        let strip = |text: &str| text.replace([BETWEEN_REPLIES, AFTER_LABEL], " ");
        let joined = replies
            .iter()
            .map(|r| format!("{}{AFTER_LABEL}{}", strip(&r.label), strip(&r.text)))
            .collect::<Vec<_>>()
            .join(&BETWEEN_REPLIES.to_string());
        let this = self.lock();
        this.conn.execute(
            "INSERT INTO quick_replies (message_id, replies, made_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(message_id) DO UPDATE SET replies = excluded.replies, made_at = excluded.made_at",
            params![id.0, joined, now_seconds()],
        )?;
        Ok(())
    }

    /// The replies kept for a message. `None` when none were ever made.
    pub fn quick_replies(&self, id: MessageId) -> Result<Option<Vec<QuickReply>>> {
        let this = self.lock();
        let joined: Option<String> = this
            .conn
            .query_row("SELECT replies FROM quick_replies WHERE message_id = ?1", [id.0], |r| r.get(0))
            .optional()?;
        Ok(joined.map(|joined| {
            joined
                .split(BETWEEN_REPLIES)
                .filter_map(|reply| {
                    let (label, text) = reply.split_once(AFTER_LABEL)?;
                    Some(QuickReply { label: label.to_string(), text: text.to_string() })
                })
                .filter(|r| !r.text.trim().is_empty())
                .collect()
        }))
    }
}

#[cfg(test)]
mod tests {
    use spool_core::{Address, MailboxId, MailboxRole};

    use super::*;
    use crate::{IncomingMessage, Query, Scope};

    fn verdict(kind: Kind, by: SortedBy) -> Triage {
        let reason = if kind.is_priority() { "Needs you".to_string() } else { String::new() };
        Triage { kind, reason, by }
    }

    /// An inbox with three messages from three people, and an archive.
    fn store() -> (Store, Vec<MessageId>, MailboxId) {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        let archive = store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();
        let incoming: Vec<IncomingMessage> = ["ana", "bo", "cy"]
            .iter()
            .enumerate()
            .map(|(i, who)| IncomingMessage {
                account_id: account,
                mailbox_id: inbox,
                uid: Some(i as u32 + 1),
                rfc_message_id: format!("<{i}@example.com>"),
                from: Address::new(*who, format!("{who}@example.com")),
                subject: format!("From {who}"),
                date: 1_700_000_000 + i as i64,
                plain: "Hello".into(),
                ..Default::default()
            })
            .collect();
        let ids = store.ingest(&incoming).unwrap().into_iter().map(|i| i.id).collect();
        (store, ids, archive)
    }

    #[test]
    fn unsorted_mail_is_found_newest_first_until_it_is_sorted() {
        let (store, ids, _) = store();
        assert_eq!(store.untriaged(0, 10).unwrap(), vec![ids[2], ids[1], ids[0]]);
        store.set_triage(ids[2], &verdict(Kind::Other, SortedBy::Rule)).unwrap();
        assert_eq!(store.untriaged(0, 10).unwrap(), vec![ids[1], ids[0]]);
        assert_eq!(store.untriaged(1_700_000_001, 10).unwrap(), vec![ids[1]], "nothing before `since`");
    }

    #[test]
    fn the_priority_list_is_sorted_mail_still_waiting_in_an_inbox() {
        let (store, ids, archive) = store();
        store.set_triage(ids[0], &verdict(Kind::Reply, SortedBy::Model)).unwrap();
        store.set_triage(ids[1], &verdict(Kind::Act, SortedBy::Model)).unwrap();
        store.set_triage(ids[2], &verdict(Kind::Other, SortedBy::Model)).unwrap();

        let listed = |store: &Store| -> Vec<MessageId> {
            store.envelopes(Scope::Everything, &Query::Priority, 100).unwrap().iter().map(|e| e.id).collect()
        };
        assert_eq!(listed(&store), vec![ids[1], ids[0]]);

        // Answered: what it was waiting for has happened.
        store.set_flags(ids[0], Flags::default().with(Flags::ANSWERED, true)).unwrap();
        // Filed: done with.
        store.move_message(ids[1], archive).unwrap();
        assert!(listed(&store).is_empty());
    }

    #[test]
    fn the_list_shows_why_in_place_of_the_first_line() {
        let (store, ids, _) = store();
        store
            .set_triage(ids[0], &Triage { kind: Kind::Reply, reason: "Asks about Thursday".into(), by: SortedBy::Model })
            .unwrap();
        let rows = store.envelopes(Scope::Everything, &Query::Priority, 10).unwrap();
        assert_eq!(rows[0].snippet, "Asks about Thursday");
        let everything = store.envelopes(Scope::Everything, &Query::All, 10).unwrap();
        assert!(everything.iter().all(|e| e.snippet == "Hello"), "and only there");
    }

    #[test]
    fn a_decision_made_by_hand_outlasts_a_model() {
        let (store, ids, _) = store();
        store.set_triage(ids[0], &verdict(Kind::Other, SortedBy::Person)).unwrap();
        store.set_triage(ids[0], &verdict(Kind::Reply, SortedBy::Model)).unwrap();
        assert_eq!(store.triage(ids[0]).unwrap().unwrap().kind, Kind::Other);
        // And a person can change their mind.
        store.set_triage(ids[0], &verdict(Kind::Act, SortedBy::Person)).unwrap();
        assert_eq!(store.triage(ids[0]).unwrap().unwrap().kind, Kind::Act);
    }

    #[test]
    fn what_was_decided_by_hand_is_learned_once_per_sender() {
        let (store, ids, _) = store();
        store.set_triage(ids[0], &verdict(Kind::Reply, SortedBy::Person)).unwrap();
        store.set_triage(ids[1], &verdict(Kind::Other, SortedBy::Person)).unwrap();
        store.set_triage(ids[2], &verdict(Kind::Reply, SortedBy::Model)).unwrap();
        let mut learned = store.learned(10).unwrap();
        learned.sort_by(|a, b| a.sender.cmp(&b.sender));
        assert_eq!(
            learned,
            vec![
                Learned { sender: "ana@example.com".into(), priority: true },
                Learned { sender: "bo@example.com".into(), priority: false },
            ]
        );
    }

    #[test]
    fn replies_are_wanted_once_and_kept_as_written() {
        let (store, ids, _) = store();
        store.set_triage(ids[0], &verdict(Kind::Reply, SortedBy::Model)).unwrap();
        store.set_triage(ids[1], &verdict(Kind::Act, SortedBy::Model)).unwrap();
        assert_eq!(store.wanting_quick_replies(0, 10).unwrap(), vec![ids[0]], "only mail that wants an answer");

        let replies = vec![
            QuickReply { label: "Yes".into(), text: "Hi Ana,\n\nYes.\n\nWes".into() },
            QuickReply { label: "Not yet".into(), text: "Hi Ana,\n\nNot yet.\n\nWes".into() },
        ];
        store.set_quick_replies(ids[0], &replies).unwrap();
        assert_eq!(store.quick_replies(ids[0]).unwrap(), Some(replies));
        assert!(store.wanting_quick_replies(0, 10).unwrap().is_empty());

        // Nothing came back: remembered, so it is not asked for again.
        store.set_quick_replies(ids[2], &[]).unwrap();
        assert_eq!(store.quick_replies(ids[2]).unwrap(), Some(vec![]));
        assert_eq!(store.quick_replies(ids[1]).unwrap(), None);
    }
}
