//! IMAP synchronization.
//!
//! The model is a local-first cache: the store is what the UI reads, and this
//! brings it up to date. Two shapes of sync fall out of that:
//!
//! * **First sync** — pull the newest `initial_limit` messages in each folder.
//!   Not the whole folder: a decade-old archive should not stand between you
//!   and reading today's mail.
//! * **Later syncs** — ask only for UIDs above the highest one already stored,
//!   which is one round trip and usually zero messages.
//!
//! Messages are fetched in batches and handed to the store in batches, so a
//! long sync makes visible progress instead of appearing hung and then
//! finishing all at once.
//!
//! Within an account the folders are walked over several connections at once
//! rather than one after another. A sync spends its time waiting for a server
//! to answer, and waiting on four answers at a time is four times less
//! waiting; the ceiling is the server's tolerance for connections, not this
//! machine's, which is why the number is per account and modest by default.
//! The connections pull from one queue rather than being dealt a share each,
//! because folders differ in size by orders of magnitude and whoever drew the
//! archive would otherwise still be going long after the rest had finished.

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use spool_core::{AccountId, Flags, MailboxId, MailboxRole, MessageId, unsubscribe::ListUnsubscribe};
use async_imap::types::{Fetch, Flag};
use futures::Stream;
use spool_store::{IncomingMessage, MessageOrigin, Store};

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::{AccountConfig, Credentials, MAX_SYNC_CONNECTIONS, Security};
use crate::stream::Stream as Transport;
use crate::{oauth, parse};

/// Messages requested per FETCH. Large enough to amortize round trips, small
/// enough that progress is visible and memory stays bounded.
const FETCH_BATCH: u32 = 200;

/// What a sync did, for the status bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub folders: usize,
    pub fetched: usize,
    /// Local changes replayed onto the server before anything was fetched.
    pub pushed: crate::outbox::PushReport,
    /// Mail that landed in an inbox during this sync, for whatever wants to
    /// say so out loud. Capped: a first sync brings two thousand messages and
    /// nobody needs two thousand banners.
    pub arrived: Vec<Arrival>,
    /// Every message this sync stored that was not here before, in the order
    /// the folders were worked through.
    ///
    /// Separate from `arrived`, which is five messages for a notification.
    /// This is the whole set, and it is what anything that has to *act* on new
    /// mail is given — a rule, or an extension's `on-messages` hook. Ids
    /// rather than messages: the caller that wants the mail can read it back
    /// out of the store it was just committed to, and a first sync that put
    /// fifty thousand messages away should not also carry fifty thousand
    /// copies of them up through the report.
    pub ingested: Vec<MessageId>,
    /// Messages already here whose flags the server disagreed with — read on
    /// another device, flagged in a web client. Counted so the first sync
    /// after this existed can say how much it put right.
    pub reconciled: usize,
    pub account: String,
    /// Local folders dropped because the server no longer lists them —
    /// renamed, moved or deleted from another client.
    pub removed: Vec<String>,
    /// Folders the server listed but would not hand over, with the reason.
    ///
    /// Carried in the report rather than only logged: a folder that silently
    /// fails to sync is indistinguishable from one that is empty, and the
    /// person looking at the sidebar is the one who can tell the difference.
    pub skipped: Vec<(String, String)>,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let mail = match self.fetched {
            0 => format!("{}: up to date", self.account),
            1 => format!("{}: 1 new message", self.account),
            n => format!("{}: {n} new messages", self.account),
        };

        // What was sent comes first when there is any: it is the answer to
        // "did my archive stick", which is a more pressing question than how
        // much new mail arrived.
        let mail = match self.pushed.is_empty() {
            true => mail,
            false => format!("{} \u{00b7} {mail}", self.pushed.summary()),
        };

        // Only when it found something. A sync that says "0 updated" every
        // time is a sync whose numbers stop being read.
        let mail = match self.reconciled {
            0 => mail,
            1 => format!("{mail} \u{00b7} 1 flag updated"),
            n => format!("{mail} \u{00b7} {n} flags updated"),
        };

        let mail = match self.removed.len() {
            0 => mail,
            1 => format!("{mail} \u{00b7} dropped {}, gone from the server", self.removed[0]),
            n => format!("{mail} \u{00b7} dropped {n} folders gone from the server"),
        };

        match self.skipped.len() {
            0 => mail,
            1 => format!("{mail} \u{00b7} skipped {}: {}", self.skipped[0].0, self.skipped[0].1),
            n => format!("{mail} \u{00b7} skipped {n} folders (see the log)"),
        }
    }
}

/// One message that arrived, as much of it as a notification needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    pub account: String,
    /// The sender's name, or their address when they gave no name.
    pub from: String,
    pub subject: String,
}

/// How many arrivals a sync will describe.
///
/// Past a handful the individual messages stop being the news and the number
/// is; a first sync of two thousand should not become two thousand banners.
const MAX_ARRIVALS: usize = 5;

/// How much new mail one sync will describe as having *just* arrived.
///
/// Generous next to [`MAX_ARRIVALS`], because this is not for telling anybody
/// anything — it is the set a rule runs over, and a sync that pulls a few
/// hundred messages is an ordinary morning. It is capped at all because the
/// first sync of an account is not an ordinary morning.
const MAX_INGESTED: usize = 5_000;

/// Progress callbacks, so the UI can say what is happening rather than
/// spinning silently.
#[derive(Debug, Clone)]
pub enum Progress {
    Connecting { host: String },
    ListingFolders,
    /// Replaying local changes the server has not been told about yet.
    Pushing,
    /// Every folder is now in the store, before any of them has messages.
    FoldersListed { count: usize },
    /// A folder is being fetched, with how many of the account's folders are
    /// already finished.
    ///
    /// `done` rather than "which one this is": folders are fetched over
    /// several connections at once, so there is no single position in the
    /// list, and two of them finishing together must not send the bar
    /// backwards.
    Folder { name: String, done: usize, total: usize },
    Fetched { folder: String, count: usize },
    /// A folder is finished with, whether it had anything new or not.
    ///
    /// The pair to `Folder`: with several connections working at once, "which
    /// folder is being synced" is a set rather than a position, and a caller
    /// showing that set needs to be told when one leaves it.
    FolderDone { name: String },
    Done(SyncReport),
}

/// Fetches one attachment straight from the server.
///
/// For the ones sync did not keep. Anything past `MAX_STORED_ATTACHMENT` lands
/// with its name, type and size and no bytes — the cap is what stops a mailbox
/// of large attachments becoming a database nobody wants — and until now that
/// left the reader able to say a 15 MB PDF had arrived and unable to do
/// anything else about it.
///
/// The whole message comes back rather than the one part. IMAP can fetch a
/// single body section, but only by a part number that has to be derived from
/// a `BODYSTRUCTURE` whose numbering has to agree exactly with the parser's
/// own walk of the message. Refetching the message and re-parsing it reuses
/// the walk that produced the attachment list in the first place, so the index
/// cannot drift; the cost is the rest of the message, which next to a 15 MB
/// attachment is nothing.
///
/// Nothing is written to the store. The cap is a decision about disk, and
/// fetching on demand is what it leaves room for.
pub async fn fetch_attachment(
    config: &AccountConfig,
    origin: &MessageOrigin,
    index: usize,
    meter: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<Vec<u8>> {
    let credentials = config.credentials().await?;
    let mut session = connect_metered(config, &credentials, meter).await?;

    // PEEK, so asking to look at an attachment does not mark the message read.
    session
        .select(&origin.mailbox)
        .await
        .with_context(|| format!("selecting {}", origin.mailbox))?;
    let mut messages = session
        .uid_fetch(origin.uid.to_string(), "(BODY.PEEK[])")
        .await
        .with_context(|| format!("fetching message {} of {}", origin.uid, origin.mailbox))?;

    let mut raw = None;
    while let Some(message) = messages.next().await {
        let message = message.context("reading the fetch response")?;
        if let Some(body) = message.body() {
            raw = Some(body.to_vec());
            break;
        }
    }
    drop(messages);
    session.logout().await.ok();

    let raw = raw.with_context(|| {
        format!("{} is no longer on the server", origin.mailbox)
    })?;

    let parsed = mail_parser::MessageParser::default()
        .parse(&raw[..])
        .context("the server sent something this could not parse")?;
    let part = parsed
        .attachments()
        .nth(index)
        .context("the message on the server no longer has that attachment")?;

    Ok(part.contents().to_vec())
}

/// Reads a stored message's `List-Unsubscribe` headers off the server.
///
/// For mail synced before those headers were kept. Only the header block is
/// fetched — a few kilobytes, whatever the size of the message — and with
/// `PEEK`, so looking does not mark anything read.
pub async fn fetch_list_unsubscribe(
    config: &AccountConfig,
    origin: &MessageOrigin,
) -> Result<ListUnsubscribe> {
    let credentials = config.credentials().await?;
    let mut session = connect(config, &credentials).await?;

    session
        .examine(&origin.mailbox)
        .await
        .with_context(|| format!("opening {}", origin.mailbox))?;
    let mut messages = session
        .uid_fetch(origin.uid.to_string(), "(BODY.PEEK[HEADER])")
        .await
        .with_context(|| format!("fetching the headers of {} in {}", origin.uid, origin.mailbox))?;

    let mut header = None;
    while let Some(message) = messages.next().await {
        let message = message.context("reading the fetch response")?;
        if let Some(bytes) = message.header() {
            header = Some(bytes.to_vec());
            break;
        }
    }
    drop(messages);
    session.logout().await.ok();

    let header = header.with_context(|| format!("{} is no longer on the server", origin.mailbox))?;
    // A header block with nothing after it still parses as a message.
    Ok(mail_parser::MessageParser::default()
        .parse_headers(&header[..])
        .map(|parsed| ListUnsubscribe::from_message(&parsed))
        .unwrap_or(ListUnsubscribe::Absent))
}

/// Syncs one account. Runs on a background thread; never touches the UI.
pub async fn sync_account(
    store: Store,
    config: AccountConfig,
    mode: SyncMode,
    mut progress: impl FnMut(Progress),
) -> Result<SyncReport> {
    progress(Progress::Connecting { host: config.imap_host.clone() });

    let credentials = config.credentials().await?;
    let account_id = store
        .add_account(&config.name, &config.email, config.color)
        .context("registering the account locally")?;

    let mut session = connect(&config, &credentials).await?;

    // What this run is going to walk. Checking for new mail asks the store
    // which folders are inboxes and goes straight there — the `LIST` and the
    // dozen `SELECT`s behind it are what make a full sync take as long as it
    // does, and neither of them can tell you anything about mail that has just
    // arrived in the inbox.
    let known_inboxes = match mode {
        SyncMode::Everything => Vec::new(),
        SyncMode::Inboxes => inboxes_of(&store, account_id),
    };

    let listed = known_inboxes.is_empty();
    let folders = match listed {
        // Nothing known yet — a first sync, or an account whose folders have
        // never been listed. Ask, then keep the inboxes if that is all this
        // run wanted.
        true => {
            progress(Progress::ListingFolders);
            let mut folders = list_folders(&mut session, &config).await?;
            if folders.is_empty() {
                bail!("server listed no folders for {}", config.email);
            }
            if mode == SyncMode::Inboxes {
                let inboxes: Vec<Folder> = folders
                    .iter()
                    .filter(|folder| folder.role == MailboxRole::Inbox)
                    .cloned()
                    .collect();
                if !inboxes.is_empty() {
                    // Registered first, so the account has its folders in the
                    // sidebar even though this run only fetches the inbox.
                    register(&store, account_id, &folders);
                    folders = inboxes;
                }
            }
            folders
        }
        false => known_inboxes,
    };

    // Register every folder before fetching any of them, so the sidebar shows
    // the whole account as soon as it is known and then fills in its counts.
    // Registering inside the fetch loop instead makes an account appear to
    // grow a folder at a time, which reads as "my folders are missing" for as
    // long as the first sync takes.
    register(&store, account_id, &folders);

    // And drop what the server no longer lists. A folder renamed or moved in
    // another client comes back under a new path, and the row for the old one
    // would otherwise sit in the sidebar forever, holding the mail that was
    // downloaded into it — two folders with the same name, one of them a
    // ghost. Only safe when this listing is the whole account: with `folders`
    // configured the ones left out were never asked for, and an inbox check
    // never listed anything to reconcile against.
    let removed = match config.folders.is_empty() && mode == SyncMode::Everything {
        false => Vec::new(),
        true => {
            let keep: Vec<String> = folders.iter().map(|f| f.path.clone()).collect();
            store.retain_mailboxes(account_id, &keep).unwrap_or_else(|e| {
                log::warn!("could not reconcile the folder list: {e:#}");
                Vec::new()
            })
        }
    };
    for path in &removed {
        log::info!("dropped {path}: the server no longer lists it");
    }

    // Only when there was a listing to report. An inbox check that said
    // "Found 1 folders" would be describing its own shortcut as the account.
    if listed {
        progress(Progress::FoldersListed { count: folders.len() });
    }

    let mut report = SyncReport {
        folders: folders.len(),
        fetched: 0,
        pushed: Default::default(),
        arrived: Vec::new(),
        ingested: Vec::new(),
        reconciled: 0,
        account: config.name.clone(),
        removed,
        skipped: Vec::new(),
    };

    // Before any fetching, and on the connection that was going to be opened
    // anyway. Order matters both ways round: a message archived a moment ago
    // has to leave the inbox before this session asks the inbox what is in it,
    // or the fetch pulls it straight back.
    if store.pending_count(account_id)? > 0 {
        progress(Progress::Pushing);
        match crate::outbox::push_pending(&store, &mut session, account_id).await {
            Ok(pushed) => report.pushed = pushed,
            // The queue keeps its entries and its attempt counts, so this is
            // not the last chance. Fetching is still worth doing.
            Err(e) => log::warn!("could not send pending changes: {e:#}"),
        }
    }

    // From here the connection belongs to whichever worker takes it, so it
    // becomes something that can be handed over.
    let mut session = Some(session);

    // The folders, handed out to whichever connection is free rather than
    // dealt evenly: folders differ in size by orders of magnitude, and a
    // worker that drew the archive would otherwise still be going long after
    // the others had finished their share of empty ones.
    let (queue, work) = smol::channel::unbounded::<Folder>();
    for folder in &folders {
        let _ = queue.try_send(folder.clone());
    }
    // Closed now rather than later: a worker's `recv` ends when the queue is
    // empty *and* closed, and nothing more is ever added.
    queue.close();

    // Progress arrives from several workers at once, so it goes through a
    // channel to one caller-facing callback rather than being handed round.
    let (reports, updates) = smol::channel::unbounded::<Progress>();
    let finished = AtomicUsize::new(0);
    let total = folders.len();
    let workers = worker_count(config.connections(), total);

    let mut running = Vec::with_capacity(workers);
    for index in 0..workers {
        running.push(sync_folders(
            &store,
            account_id,
            &config,
            &credentials,
            // The first worker inherits the connection everything so far was
            // done on. The rest open their own.
            (index == 0).then(|| session.take()).flatten(),
            work.clone(),
            reports.clone(),
            &finished,
            total,
        ));
    }
    // The callback's copy of the channel has to go, or draining it would wait
    // for a sender that is this very function.
    drop(reports);

    // One future forwards progress while the others do the work. Both halves
    // run on this task: the sync is waiting on the network, not on a CPU, and
    // a connection each is what makes it faster.
    let forward = async {
        let mut progress = progress;
        while let Ok(update) = updates.recv().await {
            progress(update);
        }
        progress
    };
    let (mut progress, partials) =
        futures::future::join(forward, futures::future::join_all(running)).await;

    for partial in partials {
        report.fetched += partial.fetched;
        report.reconciled += partial.reconciled;
        report.arrived.extend(partial.arrived);
        report.ingested.extend(partial.ingested);
        report.skipped.extend(partial.skipped);
    }
    // What nobody got to: every connection died before the queue was empty.
    // Reported rather than quietly left out, because a folder that did not
    // sync is indistinguishable in the sidebar from one that is empty.
    while let Ok(folder) = work.try_recv() {
        report.skipped.push((folder.path, "no working connection was left".into()));
    }

    // Each worker capped its own arrivals, so several of them together can
    // still exceed what one notification run should describe.
    report.arrived.truncate(MAX_ARRIVALS);
    // A first sync stores everything there has ever been, and handing all of
    // it to a rule as though it just landed is how somebody's inbox gets
    // classified twice. Past this it is a backfill, and a backfill is
    // something you ask for.
    report.ingested.truncate(MAX_INGESTED);
    // Whatever order the workers finished in, the report reads the same way
    // twice.
    report.skipped.sort_by(|a, b| a.0.cmp(&b.0));

    // Only if it was never handed to a worker — a worker logs its own out.
    if let Some(mut session) = session {
        session.logout().await.ok();
    }
    progress(Progress::Done(report.clone()));
    Ok(report)
}

/// Makes sure a worker still has a connection, replacing it if it does not.
///
/// `NOOP` is the cheapest question a session can be asked, and the only
/// reliable way to tell "that folder cannot be selected" from "this
/// connection is gone" — both arrive as an error on the folder.
async fn revive(
    session: &mut Session,
    config: &AccountConfig,
    credentials: &Credentials,
) -> bool {
    if session.noop().await.is_ok() {
        return true;
    }

    match connect(config, credentials).await {
        Ok(fresh) => {
            log::info!("{}: reopened a sync connection the server closed", config.email);
            *session = fresh;
            true
        }
        Err(e) => {
            log::warn!("{}: could not reopen a closed sync connection: {e:#}", config.email);
            false
        }
    }
}

/// How many connections to actually open.
///
/// Never more than there are folders to walk: opening four connections to
/// fetch one inbox is three logins nobody needed, and on a check for new mail
/// that is the common case.
fn worker_count(configured: usize, folders: usize) -> usize {
    configured.clamp(1, MAX_SYNC_CONNECTIONS).min(folders.max(1))
}

/// What one connection's share of the folders came to.
///
/// Merged into the account's [`SyncReport`] once every worker has finished;
/// kept separate until then so nothing is shared between them but the queue.
#[derive(Debug, Default)]
struct Partial {
    fetched: usize,
    reconciled: usize,
    arrived: Vec<Arrival>,
    ingested: Vec<MessageId>,
    skipped: Vec<(String, String)>,
}

/// One connection, working through the queue until it is empty.
///
/// A worker that cannot connect is one worker fewer, not a failed sync: the
/// others go on draining the queue, and the account still ends up in the
/// store. That is also what happens when a server refuses the fifth
/// connection, which is the failure this is most likely to meet.
#[allow(clippy::too_many_arguments)]
async fn sync_folders(
    store: &Store,
    account_id: AccountId,
    config: &AccountConfig,
    credentials: &Credentials,
    session: Option<Session>,
    work: smol::channel::Receiver<Folder>,
    reports: smol::channel::Sender<Progress>,
    finished: &AtomicUsize,
    total: usize,
) -> Partial {
    let mut partial = Partial::default();

    let mut session = match session {
        Some(session) => session,
        None => match connect(config, credentials).await {
            Ok(session) => session,
            Err(e) => {
                log::warn!("{}: a sync connection could not be opened: {e:#}", config.email);
                return partial;
            }
        },
    };

    let mut failed = false;

    while let Ok(folder) = work.recv().await {
        // `try_send` on an unbounded channel only fails once the receiver is
        // gone, which here means the sync is being torn down.
        let _ = reports.try_send(Progress::Folder {
            name: folder.path.clone(),
            done: finished.load(Ordering::Relaxed),
            total,
        });

        let mut send = |update: Progress| {
            let _ = reports.try_send(update);
        };

        let result = sync_folder(
            store,
            &mut session,
            account_id,
            &folder,
            config,
            &mut partial.arrived,
            &mut partial.ingested,
            &mut partial.reconciled,
            &mut send,
        )
        .await;

        match result {
            Ok(count) => {
                partial.fetched += count;
                if count > 0 {
                    let _ = reports
                        .try_send(Progress::Fetched { folder: folder.path.clone(), count });
                }
            }
            // One unreadable folder should not abandon the rest of the
            // account — servers routinely list folders they will not select.
            Err(e) => {
                log::warn!("skipping folder {}: {e:#}", folder.path);
                partial.skipped.push((folder.path.clone(), format!("{e:#}")));
                failed = true;
            }
        }

        let _ = reports.try_send(Progress::FolderDone { name: folder.path.clone() });
        finished.fetch_add(1, Ordering::Relaxed);

        // A connection the server has hung up on fails every folder it is
        // handed next, instantly. Sequentially that cost the rest of one
        // account; with the folders coming from a shared queue it is worse —
        // the dead worker races through the whole queue marking everything
        // skipped while the healthy workers sit idle. So a worker that has
        // just failed proves its connection before taking another, and gets
        // out of the way when it cannot.
        if failed && !revive(&mut session, config, credentials).await {
            log::warn!(
                "{}: a sync connection died and could not be replaced; \
                 its remaining folders go back to the other connections",
                config.email,
            );
            return partial;
        }
        failed = false;
    }

    session.logout().await.ok();
    partial
}

/// How much of an account a sync walks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncMode {
    /// Every folder the server lists, and the folder list itself.
    #[default]
    Everything,
    /// Only where new mail lands. No `LIST`, and one `SELECT` per inbox
    /// rather than one per folder — which on a Gmail account is the
    /// difference between a dozen round trips and one.
    Inboxes,
}

/// Writes a folder listing into the store.
fn register(store: &Store, account_id: AccountId, folders: &[Folder]) {
    for folder in folders {
        if let Err(e) = store.upsert_mailbox(account_id, &folder.path, folder.role) {
            log::warn!("could not register folder {}: {e:#}", folder.path);
        }
    }
}

/// The account's inboxes, as the store already knows them.
///
/// Empty when the account has never been synced, which is the caller's signal
/// that it has to ask the server after all.
fn inboxes_of(store: &Store, account_id: AccountId) -> Vec<Folder> {
    store
        .mailboxes()
        .unwrap_or_default()
        .into_iter()
        .filter(|mailbox| mailbox.account_id == account_id && mailbox.role == MailboxRole::Inbox)
        .map(|mailbox| Folder { path: mailbox.path, role: mailbox.role })
        .collect()
}

/// What a backfill managed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub fetched: usize,
    /// Whether there is still older mail on the server after this.
    pub more: bool,
}

impl BackfillReport {
    pub fn summary(&self) -> String {
        match (self.fetched, self.more) {
            (0, _) => "No older messages".into(),
            (1, true) => "1 older message \u{00b7} more to come".into(),
            (1, false) => "1 older message \u{00b7} that is all of it".into(),
            (n, true) => format!("{n} older messages \u{00b7} more to come"),
            (n, false) => format!("{n} older messages \u{00b7} that is all of it"),
        }
    }
}

/// Fetches the next batch of mail *older* than anything stored for a folder.
///
/// The ordinary sync only ever looks above the highest UID it has, which is
/// what makes it one round trip and usually nothing. This is the other
/// direction, and it is asked for rather than automatic: pulling a decade of
/// archive is a decision, not a side effect of opening a folder.
///
/// A `UID SEARCH` first, and the extra round trip earns its keep. A UID range
/// says nothing about how many messages are in it — deletions leave gaps — so
/// fetching `1:<lowest>` blind could ask for one message or forty thousand.
/// Searching says exactly which UIDs exist, and the newest `batch` of those is
/// what gets fetched.
pub async fn backfill(
    store: Store,
    config: &AccountConfig,
    folder: &str,
    mailbox_id: MailboxId,
    account_id: AccountId,
    batch: usize,
    mut progress: impl FnMut(f32),
) -> Result<BackfillReport> {
    let lowest = store.min_uid(mailbox_id)?;
    if lowest <= 1 {
        // Either the folder is empty here, or UID 1 is already stored and
        // there is nothing below it to ask for.
        return Ok(BackfillReport::default());
    }

    let credentials = config.credentials().await?;
    let mut session = connect(config, &credentials).await?;
    let result = backfill_folder(
        &store,
        &mut session,
        folder,
        mailbox_id,
        account_id,
        lowest,
        batch,
        &mut progress,
    )
    .await;
    session.logout().await.ok();
    result
}

async fn backfill_folder(
    store: &Store,
    session: &mut Session,
    folder: &str,
    mailbox_id: MailboxId,
    account_id: AccountId,
    lowest: u32,
    batch: usize,
    progress: &mut impl FnMut(f32),
) -> Result<BackfillReport> {
    let status =
        session.select(folder).await.with_context(|| format!("selecting {folder}"))?;
    store.set_server_total(mailbox_id, status.exists).ok();

    let older = session
        .uid_search(format!("UID 1:{}", lowest - 1))
        .await
        .with_context(|| format!("looking for older mail in {folder}"))?;
    if older.is_empty() {
        return Ok(BackfillReport::default());
    }

    // Newest first, so a backfill walks backwards through the folder rather
    // than starting at mail from a decade ago.
    let mut uids: Vec<u32> = older.into_iter().collect();
    uids.sort_unstable_by(|a, b| b.cmp(a));

    let more = uids.len() > batch;
    uids.truncate(batch);

    const ITEMS: &str = "(UID FLAGS RFC822.SIZE BODY.PEEK[])";
    let mut fetched = 0usize;

    // The one thing here that *can* be measured: the search has already said
    // exactly how many messages are coming, so the bar fills honestly instead
    // of sweeping for the minute a thousand messages takes.
    let wanted = uids.len().max(1);
    let mut done = 0usize;

    for chunk in uids.chunks(FETCH_BATCH as usize) {
        let set = crate::outbox::sequence_set(chunk);
        let messages = session.uid_fetch(&set, ITEMS).await?;
        let parsed = collect_batch(messages, account_id, mailbox_id).await?;
        if !parsed.is_empty() {
            fetched += store.ingest(&parsed)?.len();
        }

        // Counted from what was asked for rather than from what was stored: a
        // message already here is one fewer to fetch, not a stall.
        done += chunk.len();
        progress(done as f32 / wanted as f32);
    }

    Ok(BackfillReport { fetched, more })
}

/// Files a copy of a sent message in a folder on the server.
///
/// A connection of its own, right after the send. It is deliberately not
/// wrapped into the send itself: a message the SMTP server has accepted has
/// been sent, and failing the send because the copy could not be filed would
/// report a worse outcome than the one that happened.
///
/// The `\Seen` flag is set, because you have read what you wrote, and the
/// internal date is left to the server — an `APPEND` without one is stamped
/// with now, which is when it was sent.
pub async fn append_to_folder(
    config: &AccountConfig,
    credentials: &Credentials,
    folder: &str,
    message: &str,
) -> Result<()> {
    append_all(config, credentials, folder, std::slice::from_ref(&message)).await
}

/// Files several messages in one folder, on one connection.
///
/// The copies of a message sent to forty people one at a time: forty appends
/// in one session, not forty logins. Stops at the first refusal, since the
/// next would be refused for the same reason.
pub async fn append_all(
    config: &AccountConfig,
    credentials: &Credentials,
    folder: &str,
    messages: &[&str],
) -> Result<()> {
    let mut session = connect(config, credentials).await?;
    let mut result = Ok(());
    for message in messages {
        result = session
            .append(folder, Some("(\\Seen)"), None, message.as_bytes())
            .await
            .with_context(|| format!("appending to {folder}"));
        if result.is_err() {
            break;
        }
    }
    session.logout().await.ok();
    result
}

/// Sends one account's queued changes and hangs up, without syncing.
///
/// The fast path for something that just happened: archiving a message should
/// reach the server in the next few seconds, not at the next full sync. A
/// connection of its own rather than the sync's, because the sync is not
/// running — and if it is, it drains the same queue and one of the two finds
/// it empty.
///
/// Returns `Ok(None)` when the account owes nothing, which is the common case
/// and costs no connection at all.
pub async fn push_account(
    store: Store,
    config: &AccountConfig,
) -> Result<Option<crate::outbox::PushReport>> {
    // Looked up rather than registered: this must not bring an account into
    // existence as a side effect of having nothing to say about it.
    let account_id = store
        .accounts()?
        .into_iter()
        .find(|a| a.email.eq_ignore_ascii_case(&config.email))
        .map(|a| a.id);
    let Some(account_id) = account_id else { return Ok(None) };

    if store.pending_count(account_id)? == 0 {
        return Ok(None);
    }

    let credentials = config.credentials().await?;
    let mut session = connect(config, &credentials).await?;
    let report = crate::outbox::push_pending(&store, &mut session, account_id).await;
    session.logout().await.ok();
    report.map(Some)
}

/// What to do to a folder on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderOp {
    /// Create `path`, and subscribe so other clients show it too.
    Create { path: String },
    /// Rename `from` to `to`. Servers move the children with it.
    Rename { from: String, to: String },
    /// Delete `path` and everything in it.
    Delete { path: String },
}

impl FolderOp {
    /// What the status bar says while this is in flight.
    pub fn describing(&self) -> String {
        match self {
            FolderOp::Create { path } => format!("Creating {path}\u{2026}"),
            FolderOp::Rename { from, to } => format!("Renaming {from} to {to}\u{2026}"),
            FolderOp::Delete { path } => format!("Deleting {path}\u{2026}"),
        }
    }
}

/// Runs one folder operation and hangs up.
///
/// A connection per operation rather than a pooled session: these happen at
/// human speed, minutes apart, and a session held open between them is a
/// session that has silently timed out by the time it is used.
pub async fn folder_op(
    config: &AccountConfig,
    credentials: &Credentials,
    op: &FolderOp,
) -> Result<()> {
    let mut session = connect(config, credentials).await?;
    let result = run_folder_op(&mut session, op).await;
    session.logout().await.ok();
    result
}

async fn run_folder_op(session: &mut Session, op: &FolderOp) -> Result<()> {
    match op {
        FolderOp::Create { path } => {
            session.create(path).await.with_context(|| format!("creating {path}"))?;
            // Unsubscribed folders are invisible in clients that list only
            // subscriptions, which is most of them.
            if let Err(e) = session.subscribe(path).await {
                log::warn!("created {path} but could not subscribe to it: {e}");
            }
        }
        FolderOp::Rename { from, to } => {
            session.rename(from, to).await.with_context(|| format!("renaming {from}"))?;
            session.unsubscribe(from).await.ok();
            if let Err(e) = session.subscribe(to).await {
                log::warn!("renamed {from} but could not subscribe to {to}: {e}");
            }
        }
        FolderOp::Delete { path } => {
            // Unsubscribe first: a subscription to a folder that no longer
            // exists is a stale entry other clients will keep showing.
            session.unsubscribe(path).await.ok();
            session.delete(path).await.with_context(|| format!("deleting {path}"))?;
        }
    }
    Ok(())
}

/// The server's hierarchy delimiter, from `LIST "" ""` — the one query whose
/// whole purpose is to report it.
///
/// It varies: `/` on Gmail, `.` on Dovecot's default layout, `\` on some
/// Exchange front ends. Assuming one is how a client ends up creating a folder
/// literally named `Work/Invoices` next to the hierarchy it meant to build.
pub async fn hierarchy_delimiter(
    config: &AccountConfig,
    credentials: &Credentials,
) -> Result<Option<String>> {
    let mut session = connect(config, credentials).await?;
    let delimiter = root_delimiter(&mut session).await;
    session.logout().await.ok();
    delimiter
}

async fn root_delimiter(session: &mut Session) -> Result<Option<String>> {
    let mut names = session.list(Some(""), Some("")).await?;
    let mut delimiter = None;
    while let Some(name) = names.next().await {
        if let Some(found) = name?.delimiter() {
            delimiter = Some(found.to_string());
        }
    }
    Ok(delimiter)
}

/// A folder as the server described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub path: String,
    pub role: MailboxRole,
}

pub type Session = async_imap::Session<Transport>;
type Client = async_imap::Client<Transport>;

/// Connects and authenticates, then hangs up — the IMAP half of a settings
/// pane's "test connection".
///
/// Logging out rather than leaking the session matters: some servers cap
/// concurrent connections low enough that a few abandoned tests would lock the
/// account out of syncing.
pub async fn verify(config: &AccountConfig, credentials: &Credentials) -> Result<()> {
    let mut session = connect(config, credentials).await?;
    session.logout().await.ok();
    Ok(())
}

/// Connects and authenticates, handing back a session ready for commands.
///
/// Public so the watcher can open a connection of its own: a session sitting
/// in `IDLE` accepts nothing else, so it cannot be the sync's.
/// Opens an ordinary connection: one that is expected to be answering.
pub async fn connect_session(
    config: &AccountConfig,
    credentials: &Credentials,
) -> Result<Session> {
    connect_with(config, credentials, None).await
}

/// Opens a connection for a watcher, which will spend its life in `IDLE`.
///
/// A sync connection that says nothing for two minutes is dead. An idling one
/// saying nothing for twenty-five is doing exactly what it was told to, so it
/// gets a tolerance longer than the interval it waits for — see
/// [`crate::idle`], which owns that number.
pub async fn connect_idle_session(
    config: &AccountConfig,
    credentials: &Credentials,
    quiet_for: std::time::Duration,
) -> Result<Session> {
    connect_with(config, credentials, Some(quiet_for)).await
}

async fn connect(config: &AccountConfig, credentials: &Credentials) -> Result<Session> {
    connect_with(config, credentials, None).await
}

/// Opens a connection that counts what it reads, for a caller showing
/// progress over a fetch large enough to need one.
async fn connect_metered(
    config: &AccountConfig,
    credentials: &Credentials,
    meter: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<Session> {
    let host = config.imap_host.trim();
    if host.is_empty() {
        bail!("no IMAP server configured for {}", config.email);
    }
    let stream =
        Transport::connect_metered(host, config.imap_port, config.imap_security, meter).await?;
    finish_connecting(stream, config, credentials).await
}

async fn connect_with(
    config: &AccountConfig,
    credentials: &Credentials,
    quiet_for: Option<std::time::Duration>,
) -> Result<Session> {
    let host = config.imap_host.trim();
    if host.is_empty() {
        bail!("no IMAP server configured for {}", config.email);
    }

    let stream = match quiet_for {
        Some(idle) => {
            Transport::connect_quiet(host, config.imap_port, config.imap_security, idle).await?
        }
        None => Transport::connect(host, config.imap_port, config.imap_security).await?,
    };
    finish_connecting(stream, config, credentials).await
}

/// The greeting, the optional upgrade and the login — everything after the
/// socket exists, which is the same whichever way it was opened.
async fn finish_connecting(
    stream: Transport,
    config: &AccountConfig,
    credentials: &Credentials,
) -> Result<Session> {
    let host = config.imap_host.trim();
    let mut client = async_imap::Client::new(stream);
    client
        .read_response()
        .await
        .context("reading the server greeting")?
        .context("connection closed before the server said hello")?;

    if config.imap_security == Security::Starttls {
        client = upgrade(client, host).await?;
    }

    authenticate(client, config, credentials).await
}

/// Negotiates STARTTLS and hands back a client speaking over the encrypted
/// connection.
async fn upgrade(mut client: Client, host: &str) -> Result<Client> {
    client
        .run_command_and_check_ok("STARTTLS", None)
        .await
        .with_context(|| format!("{host} refused STARTTLS"))?;

    // `into_inner` drops the read buffer along with the client. That is what
    // is wanted: anything the server pipelined before the handshake arrived in
    // the clear, and carrying it into the encrypted session would let an
    // attacker inject responses.
    let secured = client.into_inner().upgrade(host).await?;

    let mut client = async_imap::Client::new(secured);
    // A server does not re-greet after STARTTLS, so unlike the initial
    // connect there is no response to read here.
    client
        .run_command_and_check_ok("NOOP", None)
        .await
        .with_context(|| format!("{host} did not answer after the TLS upgrade"))?;
    Ok(client)
}

async fn authenticate(
    client: Client,
    config: &AccountConfig,
    credentials: &Credentials,
) -> Result<Session> {
    let user = config.user();

    let outcome = match credentials {
        // The error carries the client back so it can be reused; we only want
        // the reason, and we do not want a password in the message.
        Credentials::Password(password) => {
            client.login(user, password).await.map_err(|(e, _)| e)
        }
        Credentials::Bearer(token) => client
            .authenticate("XOAUTH2", XOAuth2 { user: user.to_string(), token: token.clone() })
            .await
            .map_err(|(e, _)| e),
    };

    outcome.map_err(|e| {
        let reason = describe(&e);
        let message = match credentials {
            Credentials::Password(_) => format!("{user}: {reason}"),
            Credentials::Bearer(_) => format!(
                "{user}: {reason} — if this keeps failing, sign in with Google again to \
                 grant access"
            ),
        };
        // A `NO` or `BAD` is the server answering, and the answer is no. That
        // is a different thing from a connection that dropped mid-login, and
        // callers need to tell them apart: retrying a dropped connection is
        // how you get through a flaky network, while retrying a refused login
        // is how a mail host's brute-force protection learns your address.
        match e {
            async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_) => {
                anyhow::Error::new(LoginRefused { message })
            }
            _ => anyhow::anyhow!(message),
        }
    })
}

/// The server answered a login with a refusal.
///
/// Its own type so that anything holding the error can ask "was this the
/// password?" without matching on the server's wording, which varies by
/// server and by language. See [`is_login_refused`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRefused {
    pub message: String,
}

impl std::fmt::Display for LoginRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LoginRefused {}

/// Whether an error, under whatever context was added to it on the way up,
/// is a server refusing a login.
pub fn is_login_refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<LoginRefused>())
}

/// Turns an IMAP error into something worth putting in front of a person.
///
/// A `NO` response is where the useful part is — `[AUTHENTICATIONFAILED]
/// Invalid credentials` is exactly what someone needs to see — but the library
/// wraps it in the debug formatting of its own response struct, so the
/// sentence arrives as `no response: code: None, info: Some("…")`. This digs
/// the server's text back out.
fn describe(error: &async_imap::error::Error) -> String {
    use async_imap::error::Error;

    match error {
        Error::No(detail) | Error::Bad(detail) => {
            unwrap_info(detail).unwrap_or_else(|| detail.clone())
        }
        Error::ConnectionLost => {
            "the server closed the connection during login".to_string()
        }
        other => other.to_string(),
    }
}

/// Pulls `X` out of `… info: Some("X") …`, undoing the debug escaping.
fn unwrap_info(detail: &str) -> Option<String> {
    let start = detail.find("info: Some(\"")? + "info: Some(\"".len();
    let rest = &detail[start..];
    // The payload ends at the closing quote, which cannot itself be escaped
    // in practice — IMAP response text is not quoted.
    let end = rest.rfind("\")")?;
    let text = rest[..end].replace("\\\"", "\"").replace("\\\\", "\\");

    let text = text.trim();
    // Servers append a parenthesized status that adds nothing: Gmail's
    // "(Failure)" after an already-explicit authentication failure.
    let text = text.strip_suffix("(Failure)").unwrap_or(text).trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The SASL exchange for `AUTHENTICATE XOAUTH2`.
///
/// A server that rejects the token replies with a challenge carrying a JSON
/// error rather than failing outright, and expects an empty line before it
/// will send the final `NO`. Answering the first challenge and then nothing
/// afterwards is what gets the real error out of it instead of a hang.
struct XOAuth2 {
    user: String,
    token: String,
}

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;

    fn process(&mut self, challenge: &[u8]) -> Self::Response {
        if challenge.is_empty() {
            return oauth::xoauth2(&self.user, &self.token);
        }
        log::debug!(
            "XOAUTH2 was refused for {}: {}",
            self.user,
            String::from_utf8_lossy(challenge)
        );
        String::new()
    }
}

async fn list_folders(session: &mut Session, config: &AccountConfig) -> Result<Vec<Folder>> {
    use async_imap::imap_proto::NameAttribute;

    let mut folders = Vec::new();
    {
        let mut names = session.list(Some(""), Some("*")).await?;
        while let Some(name) = names.next().await {
            let name = name?;

            // \Noselect marks a container that holds other folders but no
            // messages; selecting it is an error.
            if name.attributes().contains(&NameAttribute::NoSelect) {
                continue;
            }

            // SPECIAL-USE (RFC 6154) is authoritative when the server offers
            // it. Otherwise fall back to guessing from the name.
            let role = name
                .attributes()
                .iter()
                .find_map(|attr| match attr {
                    NameAttribute::Sent => Some(MailboxRole::Sent),
                    NameAttribute::Drafts => Some(MailboxRole::Drafts),
                    NameAttribute::Archive | NameAttribute::All => Some(MailboxRole::Archive),
                    NameAttribute::Trash => Some(MailboxRole::Trash),
                    NameAttribute::Junk => Some(MailboxRole::Junk),
                    _ => None,
                })
                .unwrap_or_else(|| MailboxRole::guess(name.name()));

            folders.push(Folder { path: name.name().to_string(), role });
        }
    }

    // An explicit folder list in the config wins.
    if !config.folders.is_empty() {
        folders.retain(|f| {
            config.folders.iter().any(|wanted| wanted.eq_ignore_ascii_case(&f.path))
        });
    }

    // INBOX first, so the folder people care about is populated first.
    folders.sort_by_key(|f| (f.role.sort_key(), f.path.clone()));
    Ok(folders)
}

/// Syncs one folder, returning how many messages were stored.
#[allow(clippy::too_many_arguments)]
async fn sync_folder(
    store: &Store,
    session: &mut Session,
    account_id: AccountId,
    folder: &Folder,
    config: &AccountConfig,
    arrived: &mut Vec<Arrival>,
    ingested: &mut Vec<MessageId>,
    reconciled: &mut usize,
    progress: &mut impl FnMut(Progress),
) -> Result<usize> {
    let mailbox_id = store.upsert_mailbox(account_id, &folder.path, folder.role)?;

    let status = session
        .select(&folder.path)
        .await
        .with_context(|| format!("selecting {}", folder.path))?;

    // Recorded here because the `SELECT` has just said it: the list can then
    // show how much of a folder is actually downloaded without a round trip
    // of its own.
    if let Err(e) = store.set_server_total(mailbox_id, status.exists) {
        log::warn!("could not record the size of {}: {e:#}", folder.path);
    }

    // Before anything is read or removed on the strength of a UID. If the
    // server has renumbered this folder, every UID here means something else
    // now, and the fetch below must start from an empty folder rather than
    // reconcile against rows that no longer refer to anything.
    if let Some(validity) = status.uid_validity {
        match store.set_uid_validity(mailbox_id, validity) {
            Ok(true) => log::info!(
                "{} was renumbered by the server; its local copy has been cleared",
                folder.path
            ),
            Ok(false) => {}
            Err(e) => log::warn!("could not check the UIDVALIDITY of {}: {e:#}", folder.path),
        }
    }

    if status.exists == 0 {
        // Emptied somewhere else. The server is unambiguous here in a way it
        // never is inside `refresh_flags` -- which is not even reached, since
        // there is nothing to ask about -- so this is where a folder that has
        // been cleared elsewhere gets cleared here.
        match store.clear_mailbox(mailbox_id, &folder.path) {
            Ok(0) => {}
            Ok(n) => log::info!("{} is empty on the server; dropped {n} here", folder.path),
            Err(e) => log::warn!("could not empty {}: {e:#}", folder.path),
        }
        return Ok(0);
    }

    let since = store.max_uid(mailbox_id)?;
    let ranges = fetch_ranges(since, status.exists, config.initial_limit);

    // BODY.PEEK[] rather than BODY[]: syncing must not mark mail read on the
    // server as a side effect of downloading it.
    const ITEMS: &str = "(UID FLAGS RFC822.SIZE BODY.PEEK[])";

    let mut stored = 0usize;
    for range in ranges {
        // `uid_fetch` and `fetch` return different stream types, so each arm
        // drains its own; `collect_batch` holds the logic they share.
        let batch = match &range {
            Range::Uid(spec) => {
                let messages = session.uid_fetch(spec, ITEMS).await?;
                collect_batch(messages, account_id, mailbox_id).await?
            }
            Range::Sequence(spec) => {
                let messages = session.fetch(spec, ITEMS).await?;
                collect_batch(messages, account_id, mailbox_id).await?
            }
        };

        if !batch.is_empty() {
            let new = store.ingest(&batch)?;
            stored += new.len();

            // Everything new, from every folder. What lands in a filed folder
            // is still mail that arrived, and a rule that only ever sees the
            // inbox cannot be written to watch anything else.
            if ingested.len() < MAX_INGESTED {
                ingested.extend(new.iter().map(|new| new.id));
            }

            // Only the inbox, and only up to a handful. The rest of the
            // folders are where mail is filed, not where it lands, and a
            // notification for every message a rule moved is a notification
            // for the filing rather than for the mail.
            if folder.role == MailboxRole::Inbox {
                for ingested in &new {
                    if arrived.len() >= MAX_ARRIVALS {
                        break;
                    }
                    let msg = &batch[ingested.position];
                    arrived.push(Arrival {
                        account: config.name.clone(),
                        from: match msg.from.name.trim().is_empty() {
                            true => msg.from.addr.clone(),
                            false => msg.from.name.clone(),
                        },
                        subject: msg.subject.clone(),
                    });
                }
            }
        }
    }

    // A folder that was first synced under a cap that has since been lifted
    // still has a hole below what it holds. Filled here, on the session
    // already open, and to completion — nothing waits for a button, and the
    // list grows underneath whoever is reading it.
    if config.initial_limit == 0 {
        stored += fill_backwards(store, session, account_id, mailbox_id, folder, progress).await?;
    }

    // Everything above was about mail that is new here. This is the other
    // direction: mail that is already here and has changed elsewhere.
    match refresh_flags(store, session, mailbox_id, &folder.path).await {
        Ok(n) => *reconciled += n,
        // Worth a line in the log and nothing more. The messages that were
        // fetched are stored either way, and the next sync tries again.
        Err(e) => log::warn!("could not refresh flags in {}: {e:#}", folder.path),
    }

    Ok(stored)
}

/// Downloads everything older than what the folder already holds.
///
/// A batch at a time rather than one enormous `FETCH`, so the list fills in
/// visibly and a folder of a hundred thousand does not have to complete before
/// anything appears. Each batch is reported, which is what moves the progress
/// bar and reloads the list.
///
/// Stops when the server says there is nothing older, and gives up quietly on
/// an error: the mail already fetched is stored, and the next sync carries on
/// from where this stopped.
async fn fill_backwards(
    store: &Store,
    session: &mut Session,
    account_id: AccountId,
    mailbox_id: MailboxId,
    folder: &Folder,
    progress: &mut impl FnMut(Progress),
) -> Result<usize> {
    let mut total = 0usize;

    loop {
        let Some(lowest) = store.min_uid(mailbox_id)?.checked_sub(1).filter(|l| *l > 0) else {
            // Nothing below UID 1, so the folder is complete here.
            return Ok(total);
        };

        let report = backfill_folder(
            store,
            session,
            &folder.path,
            mailbox_id,
            account_id,
            lowest + 1,
            BACKFILL_BATCH,
            &mut |_| {},
        )
        .await;

        let report = match report {
            Ok(report) => report,
            Err(e) => {
                log::warn!("stopped filling {}: {e:#}", folder.path);
                return Ok(total);
            }
        };

        total += report.fetched;
        if report.fetched > 0 {
            progress(Progress::Fetched { folder: folder.path.clone(), count: report.fetched });
        }

        if !report.more {
            return Ok(total);
        }
    }
}

/// How many older messages one pass fetches before reporting and going round.
///
/// Small enough that the list visibly fills rather than jumping, large enough
/// that a hundred thousand messages is not a hundred thousand round trips.
const BACKFILL_BATCH: usize = 500;

/// Takes the server's word for the flags on messages already stored.
///
/// One `FETCH` per folder, flags only — no bodies, no headers — over the span
/// of UIDs this copy actually holds. On a folder of twenty thousand where two
/// thousand are here, that is a reply about two thousand tiny records rather
/// than a re-download.
///
/// This is what makes reading something on your phone show up here. Without
/// it, sync only ever looks above the highest UID it has, so a message keeps
/// whatever flags it had on the day it arrived — for ever.
async fn refresh_flags(
    store: &Store,
    session: &mut Session,
    mailbox_id: MailboxId,
    folder: &str,
) -> Result<usize> {
    let Some((low, high)) = store.uid_range(mailbox_id)? else { return Ok(0) };

    let mut seen: Vec<(u32, Flags)> = Vec::new();
    {
        let responses = session.uid_fetch(format!("{low}:{high}"), "(UID FLAGS)").await?;
        futures::pin_mut!(responses);

        while let Some(response) = responses.next().await {
            let response = response?;
            // A FETCH with no UID cannot be matched to a row. Servers do send
            // these — an untagged update about a message by sequence number.
            let Some(uid) = response.uid else { continue };
            seen.push((uid, flags_of(&response)));
        }
    }

    let mut changed = store.apply_server_flags(mailbox_id, folder, &seen)?;

    // The same answer read the other way round. A `UID FETCH` over a range
    // returns every message that exists in it, so a UID this copy holds and
    // the server did not mention has been expunged somewhere else — and the
    // question has already been asked and paid for above.
    //
    // Guarded on the reply not being empty. An empty one is indistinguishable
    // from a folder somebody emptied, and this is not where that is decided:
    // `sync_folder` knows what the server said the folder holds, and handles
    // it there where the count is beside the claim.
    if !seen.is_empty() {
        let present: Vec<u32> = seen.iter().map(|(uid, _)| *uid).collect();
        changed += store.remove_expunged(mailbox_id, folder, low, high, &present)?;
    }

    Ok(changed)
}

/// The flags on one FETCH response, in our packed form.
fn flags_of(message: &Fetch) -> Flags {
    message.flags().fold(Flags::default(), |flags, flag| {
        let bit = match flag {
            Flag::Seen => Flags::SEEN,
            Flag::Flagged => Flags::FLAGGED,
            Flag::Answered => Flags::ANSWERED,
            Flag::Draft => Flags::DRAFT,
            Flag::Deleted => Flags::DELETED,
            // The one keyword worth reading: no IMAP flag means "forwarded",
            // so the clients that track it agree on this name instead.
            Flag::Custom(name) if name.eq_ignore_ascii_case("$forwarded") => Flags::FORWARDED,
            // \Recent is per-session, and the rest of the keywords are not
            // ours — a mail rule's tag, another client's classification.
            _ => return flags,
        };
        flags.with(bit, true)
    })
}

/// Drains a FETCH response into parsed messages.
async fn collect_batch(
    messages: impl Stream<Item = async_imap::error::Result<Fetch>>,
    account_id: AccountId,
    mailbox_id: MailboxId,
) -> Result<Vec<IncomingMessage>> {
    futures::pin_mut!(messages);

    let mut batch = Vec::with_capacity(FETCH_BATCH as usize);
    while let Some(message) = messages.next().await {
        let message = message?;

        // A FETCH response can carry flag or size updates with no body at
        // all; there is nothing to store for those.
        let Some(raw) = message.body() else { continue };

        let flags = flags_of(&message);

        if let Some(parsed) =
            parse::parse_message(raw, account_id, mailbox_id, message.uid, flags)
        {
            batch.push(parsed);
        }
    }
    Ok(batch)
}

/// A FETCH range, either by UID or by sequence number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Range {
    Uid(String),
    Sequence(String),
}

/// Works out what to ask for.
///
/// On a first sync (`since == 0`) there are no UIDs to anchor to, so the
/// newest `limit` messages are addressed by sequence number. Afterwards
/// everything is UID-based, which is stable across sessions in a way sequence
/// numbers are not.
pub fn fetch_ranges(since: u32, exists: u32, limit: usize) -> Vec<Range> {
    if exists == 0 {
        return Vec::new();
    }

    if since == 0 {
        // `0` is not "fetch nothing", it is "fetch the folder". A cap is a
        // ceiling on the local copy; the absence of one means start at the
        // first message.
        let first = match limit {
            0 => 1,
            limit => exists.saturating_sub(limit as u32).saturating_add(1).max(1),
        };

        return (first..=exists)
            .step_by(FETCH_BATCH as usize)
            .map(|start| {
                let end = (start + FETCH_BATCH - 1).min(exists);
                Range::Sequence(format!("{start}:{end}"))
            })
            .collect();
    }

    // `*` is the highest UID in the folder, so this asks for everything that
    // has arrived since the last sync in one request.
    vec![Range::Uid(format!("{}:*", since + 1))]
}

#[cfg(test)]
mod tests {
    use super::{describe, unwrap_info};

    #[test]
    fn a_no_response_is_reduced_to_what_the_server_said() {
        // Exactly the string async-imap produces for a Gmail refusal.
        let raw = r#"code: None, info: Some("[AUTHENTICATIONFAILED] Invalid credentials (Failure)")"#;
        assert_eq!(
            describe(&async_imap::error::Error::No(raw.into())),
            "[AUTHENTICATIONFAILED] Invalid credentials"
        );
    }

    #[test]
    fn a_response_with_no_info_is_passed_through_rather_than_dropped() {
        // Better to show the library's own wording than to show nothing.
        let raw = "code: None, info: None";
        assert_eq!(describe(&async_imap::error::Error::No(raw.into())), raw);
        assert_eq!(unwrap_info(raw), None);
        assert_eq!(unwrap_info(""), None);
    }

    #[test]
    fn escaped_quotes_in_the_server_text_survive() {
        let raw = r#"info: Some("mailbox \"Sent\" is not selectable")"#;
        assert_eq!(
            unwrap_info(raw).as_deref(),
            Some(r#"mailbox "Sent" is not selectable"#)
        );
    }

    #[test]
    fn a_lost_connection_says_so_in_words() {
        let message = describe(&async_imap::error::Error::ConnectionLost);
        assert!(message.contains("closed the connection"), "{message}");
    }


    use super::*;

    #[test]
    fn a_check_for_new_mail_opens_one_connection_not_four() {
        // One inbox to walk: three more logins would buy nothing and cost the
        // server three sessions.
        assert_eq!(worker_count(4, 1), 1);
        assert_eq!(worker_count(4, 2), 2);
    }

    #[test]
    fn a_full_account_uses_what_it_was_given() {
        assert_eq!(worker_count(4, 12), 4);
        assert_eq!(worker_count(1, 12), 1);
    }

    #[test]
    fn the_connection_count_is_clamped_at_both_ends() {
        // `0` in the file means "off", which is a sync over one connection —
        // not a sync with no connections at all.
        assert_eq!(worker_count(0, 12), 1);
        assert_eq!(worker_count(1_000, 12), MAX_SYNC_CONNECTIONS);
    }

    #[test]
    fn an_account_with_no_folders_still_asks_for_one_worker() {
        // Nothing to do, but a zero here would mean nobody to close the queue.
        assert_eq!(worker_count(4, 0), 1);
    }

    #[test]
    fn an_empty_folder_asks_for_nothing() {
        assert!(fetch_ranges(0, 0, 100).is_empty());
        assert!(fetch_ranges(50, 0, 100).is_empty());
    }

    #[test]
    fn a_first_sync_takes_the_newest_messages_by_sequence() {
        // 1000 messages, want the newest 500: sequence 501..=1000.
        let ranges = fetch_ranges(0, 1_000, 500);
        assert_eq!(ranges.first(), Some(&Range::Sequence("501:700".into())));
        assert_eq!(ranges.last(), Some(&Range::Sequence("901:1000".into())));
        assert_eq!(ranges.len(), 3, "500 messages in batches of {FETCH_BATCH}");
    }

    #[test]
    fn a_first_sync_of_a_small_folder_takes_all_of_it() {
        let ranges = fetch_ranges(0, 30, 500);
        assert_eq!(ranges, vec![Range::Sequence("1:30".into())]);
    }

    #[test]
    fn batches_do_not_run_past_the_end_of_the_folder() {
        let ranges = fetch_ranges(0, 250, 250);
        assert_eq!(
            ranges,
            vec![Range::Sequence("1:200".into()), Range::Sequence("201:250".into())]
        );
    }

    #[test]
    fn later_syncs_ask_only_for_what_is_new() {
        assert_eq!(fetch_ranges(1_234, 5_000, 500), vec![Range::Uid("1235:*".into())]);
    }

    #[test]
    fn no_cap_means_the_whole_folder_from_its_first_message() {
        use crate::imap::{Range, fetch_ranges};

        // `0` is not "fetch nothing". A first sync of a 450-message folder
        // with no cap starts at message 1, not at 451.
        let ranges = fetch_ranges(0, 450, 0);
        assert_eq!(ranges.first(), Some(&Range::Sequence("1:200".into())));
        assert_eq!(ranges.last(), Some(&Range::Sequence("401:450".into())));

        // And a cap still caps: the newest 100 of the same folder.
        let capped = fetch_ranges(0, 450, 100);
        assert_eq!(capped.first(), Some(&Range::Sequence("351:450".into())));
    }

    #[test]
    fn a_backfill_report_says_whether_that_was_all_of_it() {
        use crate::imap::BackfillReport;

        assert_eq!(BackfillReport::default().summary(), "No older messages");
        assert!(BackfillReport { fetched: 1_000, more: true }.summary().contains("more to come"));
        assert!(
            BackfillReport { fetched: 12, more: false }.summary().contains("that is all of it"),
            "the end of a folder is worth saying: it is the answer to \"is that everything\""
        );
    }

    #[test]
    fn reports_read_sensibly() {
        let report = |fetched| SyncReport {
            folders: 3,
            fetched,
            pushed: Default::default(),
            arrived: Vec::new(),
            ingested: Vec::new(),
            reconciled: 0,
            account: "Work".into(),
            removed: Vec::new(),
            skipped: Vec::new(),
        };
        assert_eq!(report(0).summary(), "Work: up to date");
        assert_eq!(report(1).summary(), "Work: 1 new message");
        assert_eq!(report(9).summary(), "Work: 9 new messages");

        // A folder the server refused is named, so it is not mistaken for an
        // empty one.
        let one_skipped = SyncReport {
            skipped: vec![("[Gmail]/All Mail".into(), "select failed".into())],
            ..report(4)
        };
        assert_eq!(
            one_skipped.summary(),
            "Work: 4 new messages \u{00b7} skipped [Gmail]/All Mail: select failed",
        );

        let many_skipped = SyncReport {
            skipped: vec![("a".into(), "x".into()), ("b".into(), "y".into())],
            ..report(0)
        };
        assert!(many_skipped.summary().contains("skipped 2 folders"));

        // A folder that has gone from the server is named too: it disappears
        // from the sidebar along with its mail, and that should not happen
        // silently.
        let dropped = SyncReport { removed: vec!["Lists/rust".into()], ..report(0) };
        assert_eq!(
            dropped.summary(),
            "Work: up to date \u{00b7} dropped Lists/rust, gone from the server",
        );

        // What was owed to the server leads: after archiving a stack of mail,
        // whether that stuck is a more pressing question than how much new
        // mail turned up.
        let with_push = SyncReport {
            pushed: crate::outbox::PushReport { done: 3, failed: 0 },
            ..report(2)
        };
        assert_eq!(
            with_push.summary(),
            "Sent 3 pending changes \u{00b7} Work: 2 new messages",
        );

        // What the server corrected while we were looking, said only when
        // there was something to correct.
        let reconciled = SyncReport { reconciled: 41, ..report(0) };
        assert_eq!(reconciled.summary(), "Work: up to date \u{00b7} 41 flags updated");
        assert_eq!(report(0).summary(), "Work: up to date", "and nothing when there was not");
    }
}
