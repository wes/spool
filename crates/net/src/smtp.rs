//! SMTP: proving an account can send, and sending.
//!
//! [`verify`] does everything a send does except deliver anything, so a broken
//! host, a failed TLS negotiation or a rejected credential is found at setup
//! time rather than the first time someone tries to reply. [`send`] adds the
//! envelope and the `DATA` payload on top of the same session.
//!
//! The protocol surface is small on purpose — greeting, `EHLO`, optionally
//! `STARTTLS`, `AUTH`, `MAIL FROM`, `RCPT TO`, `DATA`, `QUIT`.

use anyhow::{Context, Result, bail};
use futures::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{AccountConfig, Credentials, Security};
use crate::encoding::base64;
use crate::oauth;
use crate::stream::Stream;

/// One reply: its code, and the text with any continuation lines joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub text: String,
}

impl Reply {
    fn is_positive(&self) -> bool {
        (200..400).contains(&self.code)
    }
}

/// Connects and authenticates, then hangs up without sending anything.
///
/// The whole point is the side effect it does *not* have: no message is
/// queued, nothing is delivered, and a server that would reject the login
/// rejects it here, where there is a form open to fix it in.
pub async fn verify(account: &AccountConfig, credentials: &Credentials) -> Result<()> {
    let host = account
        .smtp_host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .context("no SMTP server configured")?;

    let mut session = open(account, credentials, host).await?;
    session.command("QUIT").await.ok();
    Ok(())
}

/// Sends a rendered message.
///
/// `envelope_from` and `recipients` are the SMTP envelope, which is what
/// actually decides delivery — the headers inside `message` are just text. Bcc
/// works precisely because a recipient can appear here and nowhere else.
pub async fn send(
    account: &AccountConfig,
    credentials: &Credentials,
    envelope_from: &str,
    recipients: &[String],
    message: &str,
) -> Result<()> {
    if recipients.is_empty() {
        bail!("a message needs at least one recipient");
    }

    let mut session = Session::open(account, credentials).await?;
    let sent = session.send(envelope_from, recipients, message).await;
    session.quit().await;
    sent.map_err(|e| anyhow::anyhow!(e))
}

/// A logged-in session that sends one message after another.
///
/// For the same note going to forty clients: forty messages on one connection
/// and one login, rather than forty of each. A shared host counts logins, and
/// forty in a minute from one address is what its firewall is there to stop.
pub struct Session {
    inner: SmtpSession,
}

/// Why one message did not go.
#[derive(Debug)]
pub enum SendError {
    /// The server answered, and the answer was no. The code says which kind:
    /// 4xx is "not now" — most often a sending limit — and 5xx is "not this".
    Refused { code: u16, message: String },
    /// The connection failed, and nothing more can be sent on this session.
    Broken(anyhow::Error),
}

impl SendError {
    /// A refusal the server means as "later": a limit reached, a queue full.
    /// Sending the next message straight away would only be refused too.
    pub fn is_temporary(&self) -> bool {
        matches!(self, SendError::Refused { code, .. } if (400..500).contains(code))
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Refused { message, .. } => f.write_str(message),
            SendError::Broken(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for SendError {}

impl From<anyhow::Error> for SendError {
    fn from(e: anyhow::Error) -> Self {
        SendError::Broken(e)
    }
}

impl Session {
    /// Connects and logs in.
    pub async fn open(account: &AccountConfig, credentials: &Credentials) -> Result<Self> {
        let host = account
            .smtp_host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .context("no SMTP server configured")?;
        Ok(Self { inner: open(account, credentials, host).await? })
    }

    /// Sends one message.
    ///
    /// A refusal leaves the session ready for the next one: the half-made
    /// transaction is reset, so one address the server will not take does not
    /// cost every message after it.
    pub async fn send(&mut self, envelope_from: &str, recipients: &[String], message: &str) -> Result<(), SendError> {
        let sent = self.transaction(envelope_from, recipients, message).await;
        if let Err(SendError::Refused { .. }) = &sent {
            let reset = self.inner.command("RSET").await;
            if !reset.as_ref().is_ok_and(Reply::is_positive) {
                // The refusal is still the news; the reset failing only means
                // the next message will find out the session is gone.
                log::debug!("RSET after a refusal failed: {reset:?}");
            }
        }
        sent
    }

    async fn transaction(&mut self, envelope_from: &str, recipients: &[String], message: &str) -> Result<(), SendError> {
        let refused = |what: String, reply: Reply| SendError::Refused {
            code: reply.code,
            message: format!("the server refused {what}: {} {}", reply.code, reply.text.trim()),
        };

        let reply = self.inner.command(&format!("MAIL FROM:<{envelope_from}>")).await?;
        if !reply.is_positive() {
            return Err(refused("the sender".into(), reply));
        }

        for recipient in recipients {
            let reply = self.inner.command(&format!("RCPT TO:<{recipient}>")).await?;
            if !reply.is_positive() {
                // Naming the address matters: with several recipients, "550 no
                // such user" alone leaves you guessing which one.
                return Err(refused(recipient.clone(), reply));
            }
        }

        let reply = self.inner.command("DATA").await?;
        // 354 is the one place SMTP asks for more rather than reporting success.
        if reply.code != 354 {
            return Err(refused("DATA".into(), reply));
        }

        let reply = self.inner.send_data(message).await?;
        if !reply.is_positive() {
            return Err(SendError::Refused {
                code: reply.code,
                message: format!("the server rejected the message: {} {}", reply.code, reply.text.trim()),
            });
        }
        Ok(())
    }

    /// Says goodbye. Whatever the server makes of it, everything sent is sent.
    pub async fn quit(mut self) {
        self.inner.command("QUIT").await.ok();
    }
}

/// Connects, upgrades if asked, and authenticates.
///
/// Shared by [`verify`] and [`send`] so the two cannot drift on how a session
/// is established — a test that passes must mean a send will get that far.
async fn open(
    account: &AccountConfig,
    credentials: &Credentials,
    host: &str,
) -> Result<SmtpSession> {
    let stream = Stream::connect(host, account.smtp_port, account.smtp_security).await?;
    let mut session = SmtpSession::new(stream);

    let greeting = session.read_reply().await.context("reading the SMTP greeting")?;
    if !greeting.is_positive() {
        bail!("{host} refused the connection: {}", greeting.text);
    }

    // The EHLO name is what we claim to be. `localhost` is what a client
    // behind NAT can honestly say, and no public server checks it.
    let mut capabilities = session.ehlo().await?;

    if account.smtp_security == Security::Starttls {
        let reply = session.command("STARTTLS").await?;
        if !reply.is_positive() {
            bail!("{host} refused STARTTLS: {}", reply.text.trim());
        }
        session = SmtpSession::new(session.into_stream().upgrade(host).await?);

        // Capabilities from before the upgrade are not trustworthy, and
        // servers routinely advertise a different set afterward — notably
        // AUTH, which many refuse to offer over plaintext.
        capabilities = session.ehlo().await?;
    }

    session.authenticate(account, credentials, &capabilities).await?;
    Ok(session)
}

/// Prepares a message body for `DATA`.
///
/// A line consisting of a single `.` ends the command, so any line starting
/// with a dot gets another prepended; the receiver removes it. Without this a
/// message containing such a line is silently truncated — and a body of
/// quoted text is exactly where one shows up.
pub fn dot_stuff(message: &str) -> String {
    message
        .split("\r\n")
        .map(|line| if line.starts_with('.') { format!(".{line}") } else { line.to_string() })
        .collect::<Vec<_>>()
        .join("\r\n")
}

struct SmtpSession {
    stream: BufReader<Stream>,
}

impl SmtpSession {
    fn new(stream: Stream) -> Self {
        Self { stream: BufReader::new(stream) }
    }

    /// Unwraps for the STARTTLS upgrade, dropping the read buffer with it.
    ///
    /// Dropping it is the point rather than a side effect: anything a server
    /// pipelined before the handshake was sent in the clear, and carrying it
    /// into the encrypted session would let an attacker inject it.
    fn into_stream(self) -> Stream {
        self.stream.into_inner()
    }

    async fn ehlo(&mut self) -> Result<Vec<String>> {
        let reply = self.command("EHLO localhost").await?;
        if !reply.is_positive() {
            bail!("the server rejected EHLO: {}", reply.text.trim());
        }
        Ok(reply.text.lines().map(|line| line.trim().to_ascii_uppercase()).collect())
    }

    async fn authenticate(
        &mut self,
        account: &AccountConfig,
        credentials: &Credentials,
        capabilities: &[String],
    ) -> Result<()> {
        let mechanisms = capabilities
            .iter()
            .find(|line| line.starts_with("AUTH"))
            .cloned()
            .unwrap_or_default();

        let reply = match credentials {
            Credentials::Bearer(token) => {
                if !mechanisms.contains("XOAUTH2") {
                    bail!("the server does not offer XOAUTH2{}", describe_offered(&mechanisms));
                }
                let initial = base64(oauth::xoauth2(account.smtp_login(), token).as_bytes());
                self.command(&format!("AUTH XOAUTH2 {initial}")).await?
            }

            // An empty capability line means the server advertised no AUTH at
            // all. Trying PLAIN anyway is better than refusing: some servers
            // only list it after a successful STARTTLS they already did.
            Credentials::Password(password) if mechanisms.contains("PLAIN") || mechanisms.is_empty() => {
                // SASL PLAIN: an empty authorization identity, then the login
                // and the password, NUL-separated.
                let sasl = format!("\0{}\0{}", account.smtp_login(), password);
                self.command(&format!("AUTH PLAIN {}", base64(sasl.as_bytes()))).await?
            }
            Credentials::Password(password) if mechanisms.contains("LOGIN") => {
                self.auth_login(account.smtp_login(), password).await?
            }
            Credentials::Password(_) => bail!(
                "the server offers no password mechanism Spool can use{}",
                describe_offered(&mechanisms)
            ),
        };

        if !reply.is_positive() {
            // The server's own words: "535 5.7.8 Username and Password not
            // accepted" says more than anything this could add.
            bail!("{} {}", reply.code, reply.text.trim());
        }
        Ok(())
    }

    /// `AUTH LOGIN`: two challenges, each answered with one base64 field.
    ///
    /// Obsolete and never standardized, but still the only mechanism some
    /// corporate servers offer.
    async fn auth_login(&mut self, user: &str, password: &str) -> Result<Reply> {
        let reply = self.command("AUTH LOGIN").await?;
        if reply.code != 334 {
            return Ok(reply);
        }
        let reply = self.command(&base64(user.as_bytes())).await?;
        if reply.code != 334 {
            return Ok(reply);
        }
        self.command(&base64(password.as_bytes())).await
    }

    /// Writes the message and the terminating `.` line.
    async fn send_data(&mut self, message: &str) -> Result<Reply> {
        let mut payload = dot_stuff(message);
        if !payload.ends_with("\r\n") {
            payload.push_str("\r\n");
        }
        payload.push_str(".\r\n");

        let socket = self.stream.get_mut();
        socket.write_all(payload.as_bytes()).await.context("sending the message body")?;
        socket.flush().await.context("flushing the message body")?;
        self.read_reply().await
    }

    async fn command(&mut self, line: &str) -> Result<Reply> {
        let socket = self.stream.get_mut();
        socket
            .write_all(format!("{line}\r\n").as_bytes())
            .await
            .context("sending an SMTP command")?;
        socket.flush().await.context("flushing an SMTP command")?;
        self.read_reply().await
    }

    async fn read_reply(&mut self) -> Result<Reply> {
        read_reply(&mut self.stream).await
    }
}

/// Reads one reply, following `250-` continuations through to the final
/// `250 `.
///
/// Free and generic over the reader so the parser can be tested against canned
/// transcripts — which is the only way its awkward cases ever get exercised.
async fn read_reply<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Reply> {
    let mut text = String::new();

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.context("reading the SMTP reply")?;
        if read == 0 {
            bail!("the server closed the connection without replying");
        }

        let line = line.trim_end_matches(['\r', '\n']);
        // A reply is three digits followed by a space on the last line, or a
        // `-` when more is coming. Anything shorter is not SMTP.
        if line.len() < 3 {
            bail!("unreadable SMTP reply: {line:?}");
        }
        let code: u16 = line[..3]
            .parse()
            .with_context(|| format!("unreadable SMTP reply: {line:?}"))?;

        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(line[3..].trim_start_matches(['-', ' ']));

        if line.as_bytes().get(3) != Some(&b'-') {
            return Ok(Reply { code, text });
        }
    }
}

fn describe_offered(mechanisms: &str) -> String {
    match mechanisms.strip_prefix("AUTH").map(str::trim).filter(|m| !m.is_empty()) {
        Some(offered) => format!(" — it offers {offered}"),
        None => " — it advertised no AUTH mechanisms at all".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_stuffing_protects_a_line_that_would_end_the_message() {
        // The classic case: quoted text where a line happens to be a lone dot.
        let message = "Subject: x\r\n\r\nbefore\r\n.\r\nafter\r\n";
        let stuffed = dot_stuff(message);

        assert!(stuffed.contains("\r\n..\r\n"), "{stuffed:?}");
        assert!(
            !stuffed.split("\r\n").any(|line| line == "."),
            "no line may be a bare dot: {stuffed:?}",
        );
    }

    #[test]
    fn dot_stuffing_covers_any_line_starting_with_a_dot() {
        let stuffed = dot_stuff("ok\r\n.hidden\r\n..already\r\n");
        assert!(stuffed.contains("\r\n..hidden\r\n"), "{stuffed:?}");
        assert!(stuffed.contains("\r\n...already\r\n"), "{stuffed:?}");
    }

    #[test]
    fn dot_stuffing_leaves_ordinary_text_untouched() {
        let message = "Subject: x\r\n\r\nnothing to do here\r\n";
        assert_eq!(dot_stuff(message), message);
        // A dot mid-line is not a problem and must not be doubled.
        assert_eq!(dot_stuff("a.b\r\n"), "a.b\r\n");
    }
    use futures::io::Cursor;

    /// Runs the real parser over a canned server transcript.
    fn read_replies(transcript: &str, count: usize) -> Vec<Reply> {
        smol::block_on(async {
            let mut reader = BufReader::new(Cursor::new(transcript.as_bytes().to_vec()));
            let mut replies = Vec::new();
            for _ in 0..count {
                replies.push(read_reply(&mut reader).await.unwrap());
            }
            replies
        })
    }

    #[test]
    fn a_single_line_reply_parses() {
        let replies = read_replies("220 smtp.example.com ESMTP ready\r\n", 1);
        assert_eq!(replies[0].code, 220);
        assert_eq!(replies[0].text, "smtp.example.com ESMTP ready");
        assert!(replies[0].is_positive());
    }

    #[test]
    fn a_multi_line_ehlo_is_joined_into_one_reply() {
        let transcript = "250-smtp.example.com\r\n\
                          250-PIPELINING\r\n\
                          250-AUTH PLAIN LOGIN XOAUTH2\r\n\
                          250 STARTTLS\r\n";
        let replies = read_replies(transcript, 1);

        assert_eq!(replies[0].code, 250);
        assert_eq!(
            replies[0].text.lines().collect::<Vec<_>>(),
            ["smtp.example.com", "PIPELINING", "AUTH PLAIN LOGIN XOAUTH2", "STARTTLS"]
        );
    }

    #[test]
    fn a_rejection_keeps_the_servers_own_words() {
        // This exact string is what someone will paste into a search box, so
        // it has to survive intact.
        let replies = read_replies("535 5.7.8 Username and Password not accepted\r\n", 1);
        assert_eq!(replies[0].code, 535);
        assert!(!replies[0].is_positive());
        assert_eq!(replies[0].text, "5.7.8 Username and Password not accepted");
    }

    #[test]
    fn consecutive_replies_do_not_bleed_into_each_other() {
        let replies = read_replies("220 ready\r\n250 ok\r\n334 \r\n", 3);
        assert_eq!(replies[0].code, 220);
        assert_eq!(replies[1].code, 250);
        assert_eq!(replies[2].code, 334, "an empty challenge is still a reply");
        assert_eq!(replies[2].text, "");
    }

    #[test]
    fn a_truncated_connection_is_an_error_rather_than_an_empty_reply() {
        smol::block_on(async {
            let mut reader = BufReader::new(Cursor::new(Vec::new()));
            assert!(read_reply(&mut reader).await.is_err());

            let mut reader = BufReader::new(Cursor::new(b"nope\r\n".to_vec()));
            assert!(read_reply(&mut reader).await.is_err());
        });
    }

    #[test]
    fn the_sasl_plain_payload_has_the_shape_the_rfc_asks_for() {
        // Authorization identity empty, then authentication identity, then
        // password, NUL-separated — and base64 of the whole thing, not of the
        // pieces.
        let sasl = format!("\0{}\0{}", "me@example.com", "hunter2");
        assert_eq!(base64(sasl.as_bytes()), "AG1lQGV4YW1wbGUuY29tAGh1bnRlcjI=");
    }

    #[test]
    fn the_offered_mechanisms_are_named_when_none_fit() {
        assert_eq!(describe_offered("AUTH GSSAPI NTLM"), " — it offers GSSAPI NTLM");
        assert!(describe_offered("").contains("no AUTH mechanisms"));
    }

    #[test]
    fn an_account_with_no_smtp_server_says_so_rather_than_dialing_nowhere() {
        let account = AccountConfig { smtp_host: Some("  ".into()), ..Default::default() };
        let error = smol::block_on(verify(&account, &Credentials::Password("x".into())))
            .unwrap_err()
            .to_string();
        assert!(error.contains("no SMTP server"), "{error}");
    }
}
