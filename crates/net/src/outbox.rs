//! Replaying queued changes onto the IMAP server.
//!
//! The store records what is owed (see `spool_store::outbox`); this is the
//! half that pays it. It runs at the front of every sync, on the connection
//! the sync was going to open anyway, and before any fetching — so a message
//! archived a moment ago is already out of the inbox by the time the same
//! session asks the inbox what is in it.
//!
//! Two rules shape the code:
//!
//! * **Order is preserved.** Operations are replayed in the order they were
//!   taken. Batching only ever merges *adjacent* operations that agree on
//!   folder and effect, so a flag change and a move on the same message can
//!   never swap places.
//! * **A failure is local to its batch.** One folder the server will not
//!   select must not strand the work owed to every other folder, so each batch
//!   reports for itself and the drain carries on.

use anyhow::{Result, anyhow};
use futures::StreamExt;
use spool_core::{AccountId, Flags};
use spool_store::{
    Store,
    outbox::{OpKind, PendingOp},
};

/// How many operations one drain will take on.
///
/// A bound rather than "everything": a queue that grew during a fortnight
/// offline should make visible progress and then let the sync it is holding up
/// proceed, rather than spending ten minutes before a single message arrives.
const DRAIN_LIMIT: usize = 500;

/// What a drain managed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushReport {
    pub done: usize,
    pub failed: usize,
}

impl PushReport {
    pub fn is_empty(&self) -> bool {
        self.done == 0 && self.failed == 0
    }

    pub fn summary(&self) -> String {
        match (self.done, self.failed) {
            (0, 0) => String::new(),
            (n, 0) => format!("Sent {n} pending change{}", plural(n)),
            (0, f) => format!("{f} change{} could not be sent", plural(f)),
            (n, f) => format!("Sent {n} pending change{}, {f} failed", plural(n)),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// One command's worth of work: the same thing, to several messages, in one
/// folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub folder: String,
    pub kind: OpKind,
    pub uids: Vec<u32>,
    /// The queue rows this batch settles, so they can be retired together.
    pub ids: Vec<i64>,
}

/// Merges adjacent operations that would issue the same command.
///
/// Archiving forty messages is forty queue entries and should be one `UID
/// MOVE`. Only *adjacent* ones merge: reordering to find more merges would
/// break the guarantee the queue exists to provide, and the win is not worth
/// it — the runs that matter come from bulk actions, which are already
/// adjacent.
pub fn group(ops: &[PendingOp]) -> Vec<Batch> {
    let mut batches: Vec<Batch> = Vec::new();

    for op in ops {
        match batches.last_mut() {
            Some(batch) if batch.folder == op.folder && batch.kind == op.kind => {
                batch.uids.push(op.uid);
                batch.ids.push(op.id);
            }
            _ => batches.push(Batch {
                folder: op.folder.clone(),
                kind: op.kind.clone(),
                uids: vec![op.uid],
                ids: vec![op.id],
            }),
        }
    }

    batches
}

/// Formats UIDs as an IMAP sequence set, collapsing runs into ranges.
///
/// `1,2,3,7,9,10` becomes `1:3,7,9:10`. Worth doing: archiving a screenful of
/// mail is a run, and the alternative is a command line long enough that some
/// servers reject it.
pub fn sequence_set(uids: &[u32]) -> String {
    let mut sorted: Vec<u32> = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();

    let mut parts: Vec<String> = Vec::new();
    let mut run = match sorted.first() {
        Some(&first) => (first, first),
        None => return String::new(),
    };

    for &uid in &sorted[1..] {
        if uid == run.1 + 1 {
            run.1 = uid;
        } else {
            parts.push(format_run(run));
            run = (uid, uid);
        }
    }
    parts.push(format_run(run));
    parts.join(",")
}

fn format_run((start, end): (u32, u32)) -> String {
    match start == end {
        true => start.to_string(),
        // A two-long run is written out: `4:5` and `4,5` are the same length,
        // and the list form is the one a server log is readable in.
        false if end == start + 1 => format!("{start},{end}"),
        false => format!("{start}:{end}"),
    }
}

/// The three flags this client actually changes, and their IMAP names.
///
/// Not the whole set the store models. `\Deleted` has its own path — the
/// delete operation, which follows it with an expunge — and `\Draft` is not
/// something a reader toggles. Pushing either through a flag change would let
/// a stray toggle unmark a message another client had queued for deletion.
const PUSHED_FLAGS: [(u32, &str); 3] = [
    (Flags::SEEN, "\\Seen"),
    (Flags::ANSWERED, "\\Answered"),
    (Flags::FLAGGED, "\\Flagged"),
];

/// The flags to add and the flags to remove, as `STORE` wants them.
///
/// Two `+FLAGS`/`-FLAGS` commands rather than one `FLAGS`, and the extra round
/// trip buys something specific: `FLAGS` *replaces* everything the server
/// holds for that message, so sending only the flags this client knows about
/// would silently delete every keyword another client had set — a junk
/// classification, a mail rule's tag, anything. A delta touches exactly what
/// changed and leaves the rest of the message alone.
pub fn flag_delta(flags: Flags) -> (String, String) {
    let mut add = Vec::new();
    let mut remove = Vec::new();

    for (bit, name) in PUSHED_FLAGS {
        match flags.has(bit) {
            true => add.push(name),
            false => remove.push(name),
        }
    }

    (add.join(" "), remove.join(" "))
}

/// What the server can do, as far as this matters here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServerAbilities {
    /// RFC 6851: `UID MOVE` in one command.
    pub mv: bool,
    /// RFC 4315: `UID EXPUNGE`, which only removes the messages named.
    pub uidplus: bool,
}

/// Replays what is owed for one account over an already-authenticated session.
///
/// Takes the session rather than opening one: this runs at the front of a
/// sync, and a second connection to the same account is both slower and, on
/// servers that cap concurrent connections, a way to lock yourself out.
pub async fn push_pending(
    store: &Store,
    session: &mut super::imap::Session,
    account_id: AccountId,
) -> Result<PushReport> {
    let ops = store.claim_pending(account_id, DRAIN_LIMIT)?;
    if ops.is_empty() {
        return Ok(PushReport::default());
    }

    let abilities = abilities(session).await;
    let mut report = PushReport::default();
    // The folder the session has selected, so a run of batches in one folder
    // costs one SELECT.
    let mut selected: Option<String> = None;

    for batch in group(&ops) {
        match run_batch(session, &batch, abilities, &mut selected).await {
            Ok(()) => {
                for id in &batch.ids {
                    store.op_done(*id)?;
                }
                report.done += batch.ids.len();
            }
            Err(e) => {
                // A failed batch may have left the session mid-mailbox.
                selected = None;
                let message = format!("{e:#}");
                log::warn!("outbox: {} in {} failed: {message}", describe(&batch.kind), batch.folder);
                for id in &batch.ids {
                    store.op_failed(*id, &message)?;
                }
                report.failed += batch.ids.len();
            }
        }
    }

    Ok(report)
}

fn describe(kind: &OpKind) -> &'static str {
    match kind {
        OpKind::Flags(_) => "a flag change",
        OpKind::Move { .. } => "a move",
        OpKind::Delete => "a delete",
    }
}

async fn abilities(session: &mut super::imap::Session) -> ServerAbilities {
    match session.capabilities().await {
        Ok(caps) => ServerAbilities {
            mv: caps.has_str("MOVE"),
            uidplus: caps.has_str("UIDPLUS"),
        },
        // Assuming neither costs an extra round trip per move and expunges a
        // little more bluntly. Assuming either, wrongly, fails the operation.
        Err(e) => {
            log::warn!("could not read server capabilities ({e}); assuming the minimum");
            ServerAbilities::default()
        }
    }
}

async fn run_batch(
    session: &mut super::imap::Session,
    batch: &Batch,
    abilities: ServerAbilities,
    selected: &mut Option<String>,
) -> Result<()> {
    if selected.as_deref() != Some(batch.folder.as_str()) {
        session
            .select(&batch.folder)
            .await
            .map_err(|e| anyhow!("selecting {}: {e}", batch.folder))?;
        *selected = Some(batch.folder.clone());
    }

    let set = sequence_set(&batch.uids);

    match &batch.kind {
        OpKind::Flags(flags) => {
            // `.SILENT` on both: the reply would be a FETCH of what was just
            // sent, and there is nothing to learn from it.
            let (add, remove) = flag_delta(*flags);
            if !add.is_empty() {
                store_flags(session, &set, &format!("+FLAGS.SILENT ({add})")).await?;
            }
            if !remove.is_empty() {
                store_flags(session, &set, &format!("-FLAGS.SILENT ({remove})")).await?;
            }
            Ok(())
        }

        OpKind::Move { to } => {
            if abilities.mv {
                return session
                    .uid_mv(&set, to)
                    .await
                    .map_err(|e| anyhow!("moving to {to}: {e}"));
            }

            // The three steps MOVE was invented to replace. Copy first: if it
            // fails, nothing has been marked deleted and the message is still
            // where it was.
            session
                .uid_copy(&set, to)
                .await
                .map_err(|e| anyhow!("copying to {to}: {e}"))?;
            store_flags(session, &set, "+FLAGS.SILENT (\\Deleted)").await?;
            expunge(session, &set, abilities).await
        }

        OpKind::Delete => {
            store_flags(session, &set, "+FLAGS.SILENT (\\Deleted)").await?;
            expunge(session, &set, abilities).await
        }
    }
}

/// Issues a `UID STORE` and drains its response.
///
/// Draining is not optional. The reply is a stream borrowed from the
/// connection, and dropping it early leaves unread bytes in front of the next
/// command's response — the session does not fail, it starts answering the
/// wrong questions.
async fn store_flags(
    session: &mut super::imap::Session,
    set: &str,
    query: &str,
) -> Result<()> {
    let updates = session.uid_store(set, query).await.map_err(|e| anyhow!("{e}"))?;
    futures::pin_mut!(updates);
    while let Some(update) = updates.next().await {
        update.map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

async fn expunge(
    session: &mut super::imap::Session,
    set: &str,
    abilities: ServerAbilities,
) -> Result<()> {
    // `UID EXPUNGE` removes only what was named. Plain `EXPUNGE` removes
    // everything in the folder carrying `\Deleted`, including messages another
    // client marked and has not got round to expunging — so it is the fallback
    // and not the default.
    if abilities.uidplus {
        let gone = session.uid_expunge(set).await.map_err(|e| anyhow!("{e}"))?;
        futures::pin_mut!(gone);
        while let Some(uid) = gone.next().await {
            uid.map_err(|e| anyhow!("{e}"))?;
        }
        return Ok(());
    }

    let gone = session.expunge().await.map_err(|e| anyhow!("{e}"))?;
    futures::pin_mut!(gone);
    while let Some(seq) = gone.next().await {
        seq.map_err(|e| anyhow!("{e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(id: i64, folder: &str, uid: u32, kind: OpKind) -> PendingOp {
        PendingOp {
            id,
            account_id: AccountId(1),
            folder: folder.into(),
            uid,
            kind,
            attempts: 1,
        }
    }

    fn archive() -> OpKind {
        OpKind::Move { to: "Archive".into() }
    }

    #[test]
    fn archiving_a_screenful_is_one_command() {
        let ops: Vec<PendingOp> =
            (0..40).map(|i| op(i as i64, "INBOX", 100 + i, archive())).collect();

        let batches = group(&ops);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].uids.len(), 40);
        assert_eq!(sequence_set(&batches[0].uids), "100:139");
    }

    #[test]
    fn operations_that_differ_are_not_merged() {
        let ops = [
            op(1, "INBOX", 1, archive()),
            op(2, "INBOX", 2, OpKind::Move { to: "Trash".into() }),
            op(3, "Sent", 3, archive()),
            op(4, "INBOX", 4, OpKind::Delete),
        ];

        let batches = group(&ops);
        assert_eq!(batches.len(), 4, "different destination, folder and kind each split");
    }

    #[test]
    fn a_move_between_flag_changes_keeps_them_apart() {
        // The guarantee the queue exists for. Merging the two flag changes
        // across the move would replay them in the wrong order, against a UID
        // the source folder no longer has.
        let seen = OpKind::Flags(Flags(Flags::SEEN));
        let ops = [
            op(1, "INBOX", 1, seen.clone()),
            op(2, "INBOX", 2, archive()),
            op(3, "INBOX", 3, seen.clone()),
        ];

        let batches = group(&ops);
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[0].kind, seen);
        assert_eq!(batches[1].kind, archive());
        assert_eq!(batches[2].kind, seen);
    }

    #[test]
    fn a_sequence_set_collapses_runs() {
        assert_eq!(sequence_set(&[1, 2, 3, 7, 9, 10, 11]), "1:3,7,9:11");
        assert_eq!(sequence_set(&[5]), "5");
        assert_eq!(sequence_set(&[]), "");
        // Two in a row stay a list: same length, easier to read in a log.
        assert_eq!(sequence_set(&[4, 5]), "4,5");
    }

    #[test]
    fn a_sequence_set_is_sorted_and_deduplicated() {
        // The server is entitled to reject a set it cannot parse, and a
        // repeated UID inside one is exactly the kind of thing that gets a
        // whole batch refused.
        assert_eq!(sequence_set(&[9, 2, 1, 9, 3]), "1:3,9");
    }

    #[test]
    fn a_flag_change_is_sent_as_what_changed() {
        let (add, remove) = flag_delta(Flags(Flags::SEEN | Flags::FLAGGED));
        assert_eq!(add, "\\Seen \\Flagged");
        assert_eq!(remove, "\\Answered");

        let (add, remove) = flag_delta(Flags::default());
        assert_eq!(add, "");
        assert_eq!(remove, "\\Seen \\Answered \\Flagged");
    }

    #[test]
    fn a_flag_change_never_mentions_deleted_or_draft() {
        // Both are somebody else's business: `\Deleted` belongs to the delete
        // operation and its expunge, and marking a message unread must never
        // be a way to un-delete one.
        let everything = Flags(Flags::SEEN | Flags::FLAGGED | Flags::ANSWERED | Flags::DELETED);
        let (add, remove) = flag_delta(everything);
        for command in [add, remove] {
            assert!(!command.contains("Deleted"), "{command}");
            assert!(!command.contains("Draft"), "{command}");
        }
    }

    #[test]
    fn a_report_says_what_happened() {
        assert!(PushReport::default().is_empty());
        assert_eq!(PushReport { done: 1, failed: 0 }.summary(), "Sent 1 pending change");
        assert_eq!(PushReport { done: 4, failed: 0 }.summary(), "Sent 4 pending changes");
        assert_eq!(
            PushReport { done: 0, failed: 2 }.summary(),
            "2 changes could not be sent"
        );
        assert_eq!(
            PushReport { done: 3, failed: 1 }.summary(),
            "Sent 3 pending changes, 1 failed"
        );
    }
}
