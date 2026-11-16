//! What this client says to an SMTP server when it has several messages to
//! send.
//!
//! The one thing worth proving over a real socket: a batch is one login, and a
//! message the server refuses leaves the session usable for the next one. A
//! refusal that is not reset leaves `MAIL FROM` half-open, and every message
//! after it is refused for reasons that have nothing to do with it.

use std::sync::{Arc, Mutex};

use futures::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use spool_net::config::Security;
use spool_net::smtp::{SendError, Session};

/// A server that accepts mail for anyone but `nobody@`, and writes down every
/// line it was sent.
struct FakeServer {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
    /// How many connections it was asked for.
    connections: Arc<Mutex<usize>>,
}

impl FakeServer {
    fn start() -> FakeServer {
        let listener =
            smol::block_on(smol::net::TcpListener::bind("127.0.0.1:0")).expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0));

        let (recorder, counter) = (seen.clone(), connections.clone());
        std::thread::spawn(move || {
            smol::block_on(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else { return };
                    *counter.lock().unwrap() += 1;
                    let mut writer = socket.clone();
                    let mut reader = BufReader::new(socket);
                    writer.write_all(b"220 scripted server ready\r\n").await.ok();

                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            break;
                        }
                        let line = line.trim_end().to_string();
                        recorder.lock().unwrap().push(line.clone());
                        let verb = line.split([' ', ':']).next().unwrap_or_default().to_ascii_uppercase();

                        let reply: &[u8] = match verb.as_str() {
                            "EHLO" => b"250-scripted\r\n250 AUTH PLAIN\r\n",
                            "AUTH" => b"235 welcome\r\n",
                            "RCPT" if line.contains("nobody@") => b"550 no such user\r\n",
                            "DATA" => {
                                writer.write_all(b"354 go ahead\r\n").await.ok();
                                // Read up to the lone dot, as a server does.
                                let mut body = Vec::new();
                                while !body.ends_with(b"\r\n.\r\n") {
                                    let mut byte = [0u8; 1];
                                    if reader.read(&mut byte).await.unwrap_or(0) == 0 {
                                        return;
                                    }
                                    body.push(byte[0]);
                                }
                                recorder.lock().unwrap().push("<message>".into());
                                b"250 queued\r\n"
                            }
                            "QUIT" => {
                                writer.write_all(b"221 bye\r\n").await.ok();
                                break;
                            }
                            _ => b"250 ok\r\n",
                        };
                        writer.write_all(reply).await.ok();
                    }
                }
            });
        });

        FakeServer { port, seen, connections }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn account_at(port: u16) -> spool_net::AccountConfig {
    spool_net::AccountConfig {
        name: "Test".into(),
        email: "me@example.com".into(),
        smtp_host: Some("127.0.0.1".into()),
        smtp_port: port,
        smtp_security: Security::Plaintext,
        // In the file rather than the keychain, so a test never touches the
        // machine's own credentials.
        password: Some("secret".into()),
        ..Default::default()
    }
}

fn message(to: &str) -> String {
    format!("From: me@example.com\r\nTo: {to}\r\nSubject: Hello\r\n\r\nHi there.\r\n")
}

#[test]
fn several_messages_go_on_one_login_and_a_refusal_does_not_stop_the_rest() {
    let server = FakeServer::start();
    let account = account_at(server.port);
    let credentials = spool_net::Credentials::Password("secret".into());

    let outcomes = smol::block_on(async {
        let mut session = Session::open(&account, &credentials).await.expect("logs in");
        let mut outcomes = Vec::new();
        for to in ["ana@example.com", "nobody@example.com", "bo@example.com"] {
            outcomes.push(session.send("me@example.com", &[to.to_string()], &message(to)).await);
        }
        session.quit().await;
        outcomes
    });

    assert!(outcomes[0].is_ok(), "{:?}", outcomes[0]);
    match &outcomes[1] {
        Err(e @ SendError::Refused { code: 550, .. }) => {
            assert!(!e.is_temporary(), "a 5xx is about this address, not about later");
            assert!(e.to_string().contains("nobody@example.com"), "names the address: {e}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(outcomes[2].is_ok(), "the one after a refusal still goes: {:?}", outcomes[2]);

    let seen = server.seen();
    assert_eq!(*server.connections.lock().unwrap(), 1, "one connection for the lot");
    assert_eq!(seen.iter().filter(|l| l.starts_with("AUTH")).count(), 1, "one login: {seen:?}");
    assert_eq!(seen.iter().filter(|l| *l == "<message>").count(), 2, "two delivered: {seen:?}");
    // The refused transaction was reset before the next began.
    let refused = seen.iter().position(|l| l.contains("nobody@")).unwrap();
    assert_eq!(seen[refused + 1], "RSET", "{seen:?}");
}

#[test]
fn sending_one_message_still_says_which_address_was_refused() {
    let server = FakeServer::start();
    let account = account_at(server.port);
    let credentials = spool_net::Credentials::Password("secret".into());

    let sent = smol::block_on(spool_net::smtp::send(
        &account,
        &credentials,
        "me@example.com",
        &["nobody@example.com".to_string()],
        &message("nobody@example.com"),
    ));

    let error = sent.expect_err("refused").to_string();
    assert!(error.contains("the server refused nobody@example.com: 550"), "{error}");
}
