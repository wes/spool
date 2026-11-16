//! What this client actually says to an IMAP server.
//!
//! The unit tests elsewhere cover the decisions — what merges with what, how a
//! UID set is written, which flags get named. None of them prove a single byte
//! reaches a socket in the right order, and that is where this kind of code
//! goes wrong: a response stream left undrained does not fail, it quietly
//! makes the *next* command read somebody else's answer, and an `APPEND`
//! whose literal is miscounted hangs.
//!
//! So this drives the real code paths over a real TCP connection to a
//! scripted server, and asserts on what arrives.

use std::sync::{Arc, Mutex};

use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use spool_core::{Address, Flags, MailboxRole};
use spool_net::config::Security;
use spool_net::outbox::push_pending;
use spool_net::stream::Stream as Transport;
use spool_store::{IncomingMessage, Store};

/// A server that answers `OK` to everything and writes down what it was asked.
///
/// Enough IMAP to get a session to the point of issuing the commands under
/// test, and not one line more.
struct FakeServer {
    port: u16,
    /// Every line the server was sent, tags and all.
    seen: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    fn start(capabilities: &'static str) -> FakeServer {
        let listener =
            smol::block_on(smol::net::TcpListener::bind("127.0.0.1:0")).expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));

        let recorder = seen.clone();
        std::thread::spawn(move || {
            smol::block_on(async move {
                let Ok((socket, _)) = listener.accept().await else { return };
                let mut writer = socket.clone();
                let mut lines = BufReader::new(socket).lines();
                let mut idle_tag = String::new();

                writer.write_all(b"* OK scripted server ready\r\n").await.ok();

                use futures::StreamExt;
                while let Some(Ok(line)) = lines.next().await {
                    recorder.lock().unwrap().push(line.clone());

                    // Every command arrives as `<tag> <verb> <rest>`.
                    let mut parts = line.splitn(3, ' ');
                    let tag = parts.next().unwrap_or_default().to_string();
                    let verb = parts.next().unwrap_or_default().to_ascii_uppercase();
                    // `DONE` is sent untagged, so it arrives as the whole
                    // line and there is no verb after it.
                    let verb = match tag.eq_ignore_ascii_case("DONE") {
                        true => "DONE".to_string(),
                        false => verb,
                    };
                    // The tag `IDLE` was issued under, so `DONE` can close it.
                    if verb == "IDLE" {
                        idle_tag = tag.clone();
                    }

                    // `APPEND` is the one command that sends a body. The
                    // server has to invite it with a continuation and then
                    // read exactly the number of bytes the client promised —
                    // which is the part worth testing, because a client that
                    // miscounts hangs rather than failing.
                    if verb == "APPEND" {
                        let announced: usize = line
                            .rsplit_once('{')
                            .and_then(|(_, rest)| rest.trim_end_matches('}').parse().ok())
                            .unwrap_or(0);

                        writer.write_all(b"+ go ahead\r\n").await.ok();

                        let mut body = String::new();
                        while body.len() < announced {
                            let Some(Ok(more)) = lines.next().await else { return };
                            body.push_str(&more);
                            body.push_str("\r\n");
                        }
                        // The client writes a CRLF after the literal. When
                        // the content already ended in one, that terminator
                        // is still sitting in the stream — and leaving it
                        // there makes the *next* command read as an empty
                        // line, which is how a fake server invents a bug the
                        // client does not have.
                        if body.len() == announced {
                            let _ = lines.next().await;
                        }
                        recorder.lock().unwrap().push(format!("<literal {announced} bytes>"));

                        writer.write_all(format!("{tag} OK appended\r\n").as_bytes()).await.ok();
                        continue;
                    }

                    let reply = match verb.as_str() {
                        "CAPABILITY" => {
                            format!("* CAPABILITY IMAP4rev1{capabilities}\r\n{tag} OK done\r\n")
                        }
                        // A two-folder account, which is what makes "did it
                        // walk the whole account or just the inbox" a
                        // question the tests can ask.
                        "LIST" => format!(
                            "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                             * LIST (\\HasNoChildren) \"/\" \"Archive\"\r\n\
                             {tag} OK done\r\n"
                        ),
                        "SELECT" => format!(
                            "* 3 EXISTS\r\n\
                             * 0 RECENT\r\n\
                             * FLAGS (\\Seen \\Deleted)\r\n\
                             * OK [UIDVALIDITY 1] ok\r\n\
                             * OK [UIDNEXT 100] ok\r\n\
                             {tag} OK [READ-WRITE] selected\r\n"
                        ),
                        // A flags-only FETCH over the stored range: the
                        // server answers with one line per message, and here
                        // it disagrees with the client about both of them.
                        _ if line.to_ascii_uppercase().contains("UID FETCH")
                            && line.to_ascii_uppercase().contains("(UID FLAGS)") =>
                        {
                            format!(
                                "* 1 FETCH (UID 10 FLAGS (\\Seen))\r\n\
                                 * 2 FETCH (UID 11 FLAGS (\\Seen \\Flagged))\r\n\
                                 {tag} OK done\r\n"
                            )
                        }
                        // `UID SEARCH …` also arrives as verb `UID`, so it
                        // has to be picked out before the catch-all below.
                        _ if line.to_ascii_uppercase().contains("UID SEARCH") => {
                            format!("* SEARCH 1 2 3 4 5\r\n{tag} OK done\r\n")
                        }
                        // An expunge answers with the messages it removed
                        // before its tagged line; sending one makes the test
                        // cover the draining, not just the command.
                        "UID" | "EXPUNGE" => {
                            format!("* 1 EXPUNGE\r\n{tag} OK done\r\n")
                        }
                        // `IDLE` answers with a continuation and then says
                        // nothing until something happens. Here, something
                        // happens straight away: an unsolicited `EXISTS`,
                        // which is exactly what a server sends when mail
                        // arrives in the selected folder.
                        "IDLE" => "+ idling\r\n* 4 EXISTS\r\n".to_string(),
                        // `DONE` is untagged and ends the idle; the tagged OK
                        // belongs to the IDLE command that started it.
                        "DONE" => "IDLE-OK".to_string(),
                        "LOGOUT" => format!("* BYE bye\r\n{tag} OK done\r\n"),
                        _ => format!("{tag} OK done\r\n"),
                    };

                    // Filled in here because the tag belongs to the earlier
                    // `IDLE`, not to the `DONE` being answered.
                    let reply = match reply.as_str() {
                        "IDLE-OK" => format!("{idle_tag} OK idle done\r\n"),
                        _ => reply,
                    };

                    if writer.write_all(reply.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        });

        FakeServer { port, seen }
    }

    /// The commands the server was sent, with their tags stripped.
    fn commands(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|line| line.splitn(2, ' ').nth(1).unwrap_or_default().to_string())
            .collect()
    }
}

/// A store holding one account, an inbox and an archive, and two messages.
fn store() -> (Store, spool_core::AccountId, spool_core::MailboxId, Vec<spool_core::MessageId>)
{
    let store = Store::in_memory().unwrap();
    let account = store.add_account("Test", "me@example.com", 0).unwrap();
    let inbox = store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
    store.upsert_mailbox(account, "Archive", MailboxRole::Archive).unwrap();

    let incoming: Vec<IncomingMessage> = [10u32, 11]
        .into_iter()
        .map(|uid| IncomingMessage {
            account_id: account,
            mailbox_id: inbox,
            uid: Some(uid),
            rfc_message_id: format!("<{uid}@example.com>"),
            from: Address::new("Ana", "ana@example.com"),
            subject: format!("Message {uid}"),
            date: 1_700_000_000 + uid as i64,
            plain: "text".into(),
            ..Default::default()
        })
        .collect();
    store.ingest(&incoming).unwrap();

    let mut ids: Vec<_> = store
        .envelopes(spool_store::Scope::Mailbox(inbox), &Default::default(), 10)
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    ids.sort();
    (store, account, inbox, ids)
}

/// Connects to the scripted server and drains the queue over it.
fn drain(store: &Store, account: spool_core::AccountId, server: &FakeServer) -> usize {
    smol::block_on(async {
        let transport = Transport::connect("127.0.0.1", server.port, Security::Plaintext)
            .await
            .expect("connect");
        let mut client = async_imap::Client::new(transport);
        client.read_response().await.unwrap().unwrap();
        let mut session = client
            .login("me@example.com", "password")
            .await
            .map_err(|(e, _)| e)
            .expect("login");

        let report = push_pending(store, &mut session, account).await.expect("drain");
        session.logout().await.ok();
        assert_eq!(report.failed, 0, "nothing should fail against a server saying OK");
        report.done
    })
}

#[test]
fn a_flag_change_becomes_one_uid_store() {
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, account, _, ids) = store();

    store.set_flags(ids[0], Flags(Flags::SEEN | Flags::FLAGGED)).unwrap();
    assert_eq!(drain(&store, account, &server), 1);

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c == "SELECT \"INBOX\""),
        "the folder has to be selected first: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "UID STORE 10 +FLAGS.SILENT (\\Seen \\Flagged)"),
        "{commands:?}"
    );
    // A delta, not a replacement: the one flag that is off is turned off by
    // name, and nothing else the server holds is mentioned at all.
    assert!(
        commands.iter().any(|c| c == "UID STORE 10 -FLAGS.SILENT (\\Answered)"),
        "{commands:?}"
    );
    assert!(
        !commands.iter().any(|c| replaces_every_flag(c)),
        "a bare FLAGS would wipe every keyword another client set: {commands:?}"
    );
}

/// A bare `FLAGS` command, as opposed to `+FLAGS` or `-FLAGS`. It replaces
/// everything the server holds for the message, keywords included.
fn replaces_every_flag(command: &str) -> bool {
    command.contains(" FLAGS.SILENT")
        && !command.contains("+FLAGS.SILENT")
        && !command.contains("-FLAGS.SILENT")
}

#[test]
fn archiving_two_messages_is_one_uid_move() {
    // The batching earning its keep, end to end: two queued operations, one
    // command on the wire, one SELECT.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, account, _, ids) = store();

    let archive = store
        .mailboxes()
        .unwrap()
        .into_iter()
        .find(|m| m.role == MailboxRole::Archive)
        .unwrap()
        .id;
    store.move_message(ids[0], archive).unwrap();
    store.move_message(ids[1], archive).unwrap();

    assert_eq!(drain(&store, account, &server), 2);

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c == "UID MOVE 10,11 \"Archive\""),
        "expected one move for both: {commands:?}"
    );
    assert_eq!(
        commands.iter().filter(|c| c.starts_with("SELECT")).count(),
        1,
        "one folder, one SELECT: {commands:?}"
    );
}

#[test]
fn a_server_without_move_gets_the_three_commands_it_replaced() {
    let server = FakeServer::start("");
    let (store, account, _, ids) = store();

    let archive = store
        .mailboxes()
        .unwrap()
        .into_iter()
        .find(|m| m.role == MailboxRole::Archive)
        .unwrap()
        .id;
    store.move_message(ids[0], archive).unwrap();

    assert_eq!(drain(&store, account, &server), 1);

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c == "UID COPY 10 \"Archive\""),
        "copy first, so a failure leaves the message where it was: {commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "UID STORE 10 +FLAGS.SILENT (\\Deleted)"),
        "{commands:?}"
    );
    // No UIDPLUS, so the blunt expunge is the only one available.
    assert!(commands.iter().any(|c| c == "EXPUNGE"), "{commands:?}");
    assert!(
        !commands.iter().any(|c| c.starts_with("UID EXPUNGE")),
        "UID EXPUNGE must not be used against a server that never offered it: {commands:?}"
    );

    // Ordering is the whole point of the fallback: copy, then mark, then
    // remove. Any other order loses mail.
    let position = |needle: &str| commands.iter().position(|c| c.starts_with(needle)).unwrap();
    assert!(position("UID COPY") < position("UID STORE"));
    assert!(position("UID STORE") < position("EXPUNGE"));
}

#[test]
fn a_delete_expunges_only_the_message_it_names() {
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, account, _, ids) = store();

    store.delete_message(ids[1]).unwrap();
    assert_eq!(drain(&store, account, &server), 1);

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c == "UID STORE 11 +FLAGS.SILENT (\\Deleted)"),
        "{commands:?}"
    );
    assert!(
        commands.iter().any(|c| c == "UID EXPUNGE 11"),
        "with UIDPLUS, nothing another client marked gets caught up in it: {commands:?}"
    );
}

#[test]
fn a_drained_queue_is_empty_and_a_second_drain_says_nothing() {
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, account, _, ids) = store();

    store.set_flags(ids[0], Flags(Flags::SEEN)).unwrap();
    assert_eq!(drain(&store, account, &server), 1);
    assert_eq!(store.pending_count(account).unwrap(), 0);

    // The second connection should cost nothing: no work, no commands beyond
    // the login it needed to get here.
    let quiet = FakeServer::start(" MOVE UIDPLUS");
    assert_eq!(drain(&store, account, &quiet), 0);
    assert!(
        !quiet.commands().iter().any(|c| c.starts_with("UID")),
        "{:?}",
        quiet.commands()
    );
}

#[test]
fn a_flag_change_and_a_move_arrive_in_the_order_they_were_made() {
    // The reason the queue is ordered and batching only merges neighbours: the
    // STORE has to reach the source folder while the message is still in it.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, account, _, ids) = store();

    let archive = store
        .mailboxes()
        .unwrap()
        .into_iter()
        .find(|m| m.role == MailboxRole::Archive)
        .unwrap()
        .id;
    store.set_flags(ids[0], Flags(Flags::SEEN)).unwrap();
    store.move_message(ids[0], archive).unwrap();

    assert_eq!(drain(&store, account, &server), 2);

    let commands = server.commands();
    let store_at = commands.iter().position(|c| c.starts_with("UID STORE")).unwrap();
    let move_at = commands.iter().position(|c| c.starts_with("UID MOVE")).unwrap();
    assert!(store_at < move_at, "{commands:?}");
}

#[test]
fn checking_for_new_mail_asks_about_the_inbox_and_nothing_else() {
    // The whole point of the fast path: no `LIST`, and one `SELECT` for the
    // inbox rather than one per folder. The store here has an Archive too,
    // and a full sync would go into it.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, _, _, _) = store();

    let report = smol::block_on(spool_net::sync_account(
        store,
        account_at(server.port),
        spool_net::SyncMode::Inboxes,
        |_| {},
    ))
    .expect("the inbox check should finish");

    let commands = server.commands();
    assert!(
        !commands.iter().any(|c| c.to_ascii_uppercase().starts_with("LIST")),
        "the folder list is what it is skipping: {commands:?}",
    );

    let selects: Vec<&String> =
        commands.iter().filter(|c| c.to_ascii_uppercase().starts_with("SELECT")).collect();
    assert!(!selects.is_empty(), "it still has to open the inbox: {commands:?}");
    assert!(
        selects.iter().all(|c| c.contains("INBOX")),
        "nothing but the inbox is opened: {selects:?}",
    );
    assert_eq!(report.folders, 1);
}

#[test]
fn a_full_sync_still_lists_the_account_and_walks_it() {
    let server = FakeServer::start(" MOVE UIDPLUS");
    let (store, _, _, _) = store();

    smol::block_on(spool_net::sync_account(
        store,
        account_at(server.port),
        spool_net::SyncMode::Everything,
        |_| {},
    ))
    .expect("the full sync should finish");

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c.to_ascii_uppercase().starts_with("LIST")),
        "the folder list is where a full sync starts: {commands:?}",
    );
    assert!(
        commands.iter().any(|c| c.to_ascii_uppercase().starts_with("SELECT") && c.contains("Archive")),
        "and it goes into the folders it found: {commands:?}",
    );
}

/// What the tests use in place of the watcher's real waits.
const TICK: std::time::Duration = std::time::Duration::from_millis(5);

/// An account pointed at the scripted server in the clear.
fn account_at(port: u16) -> spool_net::AccountConfig {
    spool_net::AccountConfig {
        name: "Test".into(),
        email: "me@example.com".into(),
        imap_host: "127.0.0.1".into(),
        imap_port: port,
        imap_security: Security::Plaintext,
        // In the file rather than the keychain, so a test never touches the
        // machine's own credentials.
        password: Some("secret".into()),
        ..Default::default()
    }
}

#[test]
fn a_sent_copy_is_appended_with_its_literal_counted_correctly() {
    // The failure this catches is a hang, not an error: announce the wrong
    // length and the server waits for bytes that never come while the client
    // waits for a reply that never comes.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let account = account_at(server.port);
    let credentials = spool_net::Credentials::Password("secret".into());

    // A body with a non-ASCII character, so a length counted in characters
    // rather than bytes would be wrong by exactly one.
    let message = "From: me@example.com\r\n\
                  To: bo@example.com\r\n\
                  Subject: Caf\u{e9}\r\n\
                  \r\n\
                  Sent from a test.\r\n";

    smol::block_on(async {
        spool_net::append_to_folder(&account, &credentials, "Sent", message)
            .await
            .expect("append")
    });

    let commands = server.commands();
    let append = commands
        .iter()
        .find(|c| c.starts_with("APPEND"))
        .unwrap_or_else(|| panic!("{commands:?}"));

    assert!(append.contains("\"Sent\""), "{append}");
    assert!(
        append.contains("(\\Seen)"),
        "you have read what you wrote, so the copy is not unread: {append}"
    );
    assert!(
        append.ends_with(&format!("{{{}}}", message.len())),
        "the literal is counted in bytes: {append}"
    );
}

#[test]
fn appending_hangs_up_afterwards() {
    // Some servers cap concurrent connections low enough that a leaked one
    // per sent message locks the account out of syncing.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let account = account_at(server.port);
    let credentials = spool_net::Credentials::Password("secret".into());

    smol::block_on(async {
        spool_net::append_to_folder(&account, &credentials, "Sent", "From: a\r\n\r\nb\r\n")
            .await
            .expect("append")
    });

    assert!(
        server.commands().iter().any(|c| c == "LOGOUT"),
        "{:?}",
        server.commands()
    );
}

#[test]
fn a_backfill_asks_which_older_uids_exist_before_fetching_any() {
    // The round trip that earns its keep. A UID *range* says nothing about how
    // many messages are in it — deletions leave gaps — so fetching `1:<lowest>`
    // blind could ask for one message or forty thousand. The search says
    // exactly which exist, and only the newest batch of those is fetched.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let account = account_at(server.port);
    let (store, account_id, inbox, _) = store();

    let progress = Arc::new(Mutex::new(Vec::<f32>::new()));
    let seen_progress = progress.clone();

    smol::block_on(async {
        spool_net::backfill(store, &account, "INBOX", inbox, account_id, 3, move |fraction| {
            seen_progress.lock().unwrap().push(fraction)
        })
        .await
        .expect("backfill")
    });

    // The search already said how many are coming, so the bar fills rather
    // than sweeping through the minute a thousand messages takes.
    let reported = progress.lock().unwrap().clone();
    assert_eq!(
        reported.last().copied(),
        Some(1.0),
        "it has to reach the end, or the bar stops short of full: {reported:?}"
    );

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c == "SELECT \"INBOX\""),
        "{commands:?}"
    );
    // The store holds UIDs 10 and 11, so everything below 10 is what is asked
    // for — not `1:11`, which would re-fetch what is already here.
    assert!(
        commands.iter().any(|c| c == "UID SEARCH UID 1:9"),
        "{commands:?}"
    );

    let fetch = commands
        .iter()
        .find(|c| c.starts_with("UID FETCH"))
        .unwrap_or_else(|| panic!("{commands:?}"));
    assert!(
        fetch.starts_with("UID FETCH 3:5 "),
        "the newest of what the search found, bounded to the batch: {fetch}"
    );
    assert!(
        fetch.contains("BODY.PEEK[]"),
        "peeked, so backfilling does not mark a decade of mail read: {fetch}"
    );

    let search_at = commands.iter().position(|c| c.starts_with("UID SEARCH")).unwrap();
    let fetch_at = commands.iter().position(|c| c.starts_with("UID FETCH")).unwrap();
    assert!(search_at < fetch_at, "{commands:?}");
}

#[test]
fn a_folder_whose_oldest_message_is_uid_one_needs_no_connection() {
    // Nothing below UID 1, so there is nothing to ask and no reason to open a
    // socket to ask it.
    let server = FakeServer::start(" MOVE UIDPLUS");
    let account = account_at(server.port);
    let store = Store::in_memory().unwrap();
    let account_id = store.add_account("Test", "me@example.com", 0).unwrap();
    let inbox = store.upsert_mailbox(account_id, "INBOX", MailboxRole::Inbox).unwrap();
    store
        .ingest(&[IncomingMessage {
            account_id,
            mailbox_id: inbox,
            uid: Some(1),
            rfc_message_id: "<1@example.com>".into(),
            subject: "The first one".into(),
            date: 1,
            ..Default::default()
        }])
        .unwrap();

    let report = smol::block_on(async {
        spool_net::backfill(store, &account, "INBOX", inbox, account_id, 100, |_| {})
            .await
            .expect("backfill")
    });

    assert_eq!(report.fetched, 0);
    assert!(!report.more);
    assert!(server.commands().is_empty(), "{:?}", server.commands());
}

#[test]
fn a_watcher_idles_and_reports_what_the_server_pushes() {
    // The loop that makes new mail arrive without being asked for. The
    // scripted server answers IDLE with a continuation and then pushes an
    // unsolicited EXISTS — which is what a real server sends the moment mail
    // lands in the selected folder.
    let server = FakeServer::start(" IDLE");
    let account = account_at(server.port);

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = seen.clone();

    // The watcher never returns, so it is raced against the first two events
    // rather than awaited.
    smol::block_on(async move {
        let watching = spool_net::watch(
            account,
            move |event| recorder.lock().unwrap().push(format!("{event:?}")),
            |_| Box::pin(async { smol::Timer::after(TICK).await; }),
        );

        // Long enough for a connection over loopback, short enough that a
        // hang fails the test rather than the suite.
        let deadline = smol::Timer::after(std::time::Duration::from_secs(5));
        futures::future::select(Box::pin(watching), deadline).await;
    });

    let events = seen.lock().unwrap().clone();
    assert!(
        events.iter().any(|e| e == "Watching"),
        "it should say when it is connected and waiting: {events:?}"
    );
    assert!(
        events.iter().any(|e| e == "Changed"),
        "and report what the server pushed: {events:?}"
    );

    let commands = server.commands();
    assert!(commands.iter().any(|c| c == "SELECT \"INBOX\""), "{commands:?}");
    assert!(commands.iter().any(|c| c == "IDLE"), "{commands:?}");
    // The idle has to be stopped before the session can be used again, or the
    // next command is sent into a connection that is not listening for one.
    assert!(
        server.seen.lock().unwrap().iter().any(|l| l.eq_ignore_ascii_case("DONE")),
        "{commands:?}"
    );
}

#[test]
fn a_server_without_idle_is_polled_instead_of_being_asked_to_idle() {
    // Issuing IDLE at a server that never offered it fails every twenty-five
    // minutes for ever, which is worse than a timer.
    let server = FakeServer::start("");
    let account = account_at(server.port);

    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorder = seen.clone();

    smol::block_on(async move {
        let watching = spool_net::watch(
            account,
            move |event| recorder.lock().unwrap().push(format!("{event:?}")),
            // A tick instead of the real five minutes, so the poll loop comes
            // round while the test is still running. It has to actually
            // yield: a sleep that is ready immediately never gives the
            // executor a chance to fire the deadline below, and the test
            // spins for ever instead of failing.
            |_| Box::pin(async { smol::Timer::after(TICK).await; }),
        );
        let deadline = smol::Timer::after(std::time::Duration::from_secs(5));
        futures::future::select(Box::pin(watching), deadline).await;
    });

    let events = seen.lock().unwrap().clone();
    assert!(events.iter().any(|e| e == "Polling"), "{events:?}");
    assert!(
        !server.commands().iter().any(|c| c == "IDLE"),
        "{:?}",
        server.commands()
    );
}

#[test]
fn a_sync_takes_the_servers_word_on_flags_for_mail_already_here() {
    // The reported bug: a message read on another device stayed unread here
    // for ever, because fetching only ever looks *above* the highest UID
    // stored. This asks about the range already held, flags only.
    let server = FakeServer::start(" IDLE");
    let account = account_at(server.port);
    let (store, account_id, inbox, ids) = store();

    // Both messages arrived unread and are still unread here.
    for id in &ids {
        let envelope = store
            .envelopes(spool_store::Scope::Mailbox(inbox), &Default::default(), 10)
            .unwrap()
            .into_iter()
            .find(|e| e.id == *id)
            .unwrap();
        assert!(!envelope.flags.is_seen());
    }

    smol::block_on(async {
        let transport = Transport::connect("127.0.0.1", server.port, Security::Plaintext)
            .await
            .expect("connect");
        let mut client = async_imap::Client::new(transport);
        client.read_response().await.unwrap().unwrap();
        let mut session = client
            .login("me@example.com", "password")
            .await
            .map_err(|(e, _)| e)
            .expect("login");

        session.select("INBOX").await.expect("select");
        let range = store.uid_range(inbox).unwrap().expect("two messages are stored");
        assert_eq!(range, (10, 11));

        // What `sync_folder` does, spelled out here because that function
        // needs a whole account behind it.
        let mut seen = Vec::new();
        {
            use futures::StreamExt;
            let responses = session
                .uid_fetch(format!("{}:{}", range.0, range.1), "(UID FLAGS)")
                .await
                .expect("fetch");
            futures::pin_mut!(responses);
            while let Some(response) = responses.next().await {
                let response = response.expect("response");
                let uid = response.uid.expect("the server answered with UIDs");
                let mut flags = spool_core::Flags::default();
                for flag in response.flags() {
                    match flag {
                        async_imap::types::Flag::Seen => {
                            flags = flags.with(spool_core::Flags::SEEN, true)
                        }
                        async_imap::types::Flag::Flagged => {
                            flags = flags.with(spool_core::Flags::FLAGGED, true)
                        }
                        _ => {}
                    }
                }
                seen.push((uid, flags));
            }
        }

        let changed = store.apply_server_flags(inbox, "INBOX", &seen).unwrap();
        assert_eq!(changed, 2, "both were corrected");
        session.logout().await.ok();
    });

    let after = store
        .envelopes(spool_store::Scope::Mailbox(inbox), &Default::default(), 10)
        .unwrap();
    assert!(after.iter().all(|e| e.flags.is_seen()), "read elsewhere, and read here now");
    assert!(
        after.iter().any(|e| e.flags.is_flagged()),
        "and a flag set elsewhere arrives too"
    );

    let commands = server.commands();
    assert!(
        commands.iter().any(|c| c == "UID FETCH 10:11 (UID FLAGS)"),
        "the stored range, flags only — not a re-download: {commands:?}"
    );
    let _ = account_id;
}

/// A server that greets, and then answers a login with `reply` — or, with
/// `None`, hangs up instead of answering at all.
fn login_answered_with(reply: Option<&'static str>) -> u16 {
    let listener =
        smol::block_on(smol::net::TcpListener::bind("127.0.0.1:0")).expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        smol::block_on(async move {
            let Ok((socket, _)) = listener.accept().await else { return };
            let mut writer = socket.clone();
            let mut lines = BufReader::new(socket).lines();
            writer.write_all(b"* OK scripted server ready\r\n").await.ok();

            use futures::StreamExt;
            while let Some(Ok(line)) = lines.next().await {
                let (tag, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                let verb = rest.split(' ').next().unwrap_or_default().to_ascii_uppercase();
                let answer = match (verb.as_str(), reply) {
                    ("LOGIN", Some(reply)) => format!("{tag} {reply}\r\n"),
                    ("LOGIN", None) => return,
                    ("CAPABILITY", _) => format!("* CAPABILITY IMAP4rev1\r\n{tag} OK done\r\n"),
                    _ => format!("{tag} OK done\r\n"),
                };
                if writer.write_all(answer.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    });
    port
}

#[test]
fn a_refused_login_is_reported_as_a_refusal() {
    // What decides whether Spool tries again in five minutes or in an hour —
    // and a shared host bans an address for the five-minute version.
    let port = login_answered_with(Some("NO [AUTHENTICATIONFAILED] Authentication failed."));
    let error = smol::block_on(spool_net::sync_account(
        Store::in_memory().unwrap(),
        account_at(port),
        spool_net::SyncMode::Inboxes,
        |_| {},
    ))
    .expect_err("the server said no");

    assert!(spool_net::is_login_refused(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains("Authentication failed"),
        "the server's own sentence is what the sidebar shows: {error:#}"
    );
}

#[test]
fn a_server_hanging_up_during_login_is_not_a_refusal() {
    // A dropped connection says nothing about the password, and is worth
    // retrying soon — it is how a flaky network gets through.
    let port = login_answered_with(None);
    let error = smol::block_on(spool_net::sync_account(
        Store::in_memory().unwrap(),
        account_at(port),
        spool_net::SyncMode::Inboxes,
        |_| {},
    ))
    .expect_err("the server went away");

    assert!(!spool_net::is_login_refused(&error), "{error:#}");
}
