//! Waiting for the server to say something, instead of asking it.
//!
//! Sync answers "what has changed since I last looked", and it only runs when
//! something asks it to. That makes new mail a thing you go and fetch, which
//! is not what a mail client is for. `IDLE` (RFC 2177) turns it round: the
//! connection stays open and the server speaks first.
//!
//! Three things make this more than a loop around one command:
//!
//! * **It is a second connection.** The sync session cannot be used — a
//!   session in `IDLE` accepts no other command until it is stopped — so each
//!   watched account holds one connection of its own.
//! * **It has to be re-issued.** Servers are entitled to log out a client that
//!   has idled too long, and RFC 2177 puts the safe interval at 29 minutes.
//! * **It will die.** Laptops sleep, networks change, servers restart. A
//!   watcher that gives up the first time the socket drops is a watcher that
//!   works until you close the lid.
//!
//! So this is a loop that reconnects, backs off when it cannot, and says what
//! it is doing rather than failing silently.

use std::time::Duration;

use anyhow::Result;

use crate::config::AccountConfig;
use crate::imap::connect_idle_session;

/// How long to wait before re-issuing `IDLE`.
///
/// Under the 29 minutes RFC 2177 gives as the limit, because the limit is
/// where servers *start* being entitled to hang up and there is nothing to be
/// gained from the last minute of it.
const IDLE_INTERVAL: Duration = Duration::from_secs(25 * 60);

/// How long a watcher's socket may hear nothing before it is called dead.
///
/// Longer than [`IDLE_INTERVAL`], and it has to be: `IDLE` produces no bytes
/// at all until either something arrives or the wait is up, so the transport's
/// ordinary two-minute patience would tear down every watcher on the machine
/// once a minute. The margin covers a server that is slow to answer the
/// `DONE`.
const IDLE_SILENCE: Duration = Duration::from_secs(30 * 60);

/// How often to look when the server has no `IDLE` to offer.
///
/// Frequent enough to be worth having, rare enough that it is not a sync every
/// time you glance at the window.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The first pause after a failure, and the longest one.
///
/// It starts short because the common failure is a laptop that has just woken
/// and has no network for another second. It stops growing at five minutes
/// because past that the watcher has stopped being a watcher.
const BACKOFF_FIRST: Duration = Duration::from_secs(5);
const BACKOFF_LIMIT: Duration = Duration::from_secs(5 * 60);

/// How long to leave an account alone after its server refused the login.
///
/// Not the backoff, and much longer than it. A dropped connection is retried
/// because the next attempt will probably work; a refused password will be
/// refused again, and every attempt is a failed login on the server's books.
/// Shared mail hosts ban an address after about ten of those in an hour —
/// every port, every account on the host — so a watcher retrying every five
/// minutes was a watcher that got the whole machine locked out. Once an hour
/// is below any threshold worth having, and still notices a password fixed on
/// the server's side.
pub const REFUSED_PAUSE: Duration = Duration::from_secs(60 * 60);

/// What a watcher has to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// Connected, and now waiting on the server.
    Watching,
    /// The server reported activity in the watched folder.
    ///
    /// Deliberately not "new mail": `IDLE` says something changed, which may
    /// be a message arriving, one being deleted from another client, or a flag
    /// moving. What to do about it is a sync, and the sync is what finds out
    /// which it was.
    Changed,
    /// This server offers no `IDLE`, so the watcher is on a timer instead.
    Polling,
    /// The connection failed and the watcher is waiting before trying again.
    Retrying { after: Duration, reason: String },
    /// The server refused the login. The watcher waits [`REFUSED_PAUSE`]
    /// before trying again rather than backing off, because trying again
    /// sooner only adds to the failures a server counts towards a ban.
    Refused { after: Duration, reason: String },
}

/// The next pause after a failure: double the last, to a limit.
pub fn backoff(previous: Option<Duration>) -> Duration {
    match previous {
        None => BACKOFF_FIRST,
        Some(last) => (last * 2).min(BACKOFF_LIMIT),
    }
}

/// Watches one account's inbox until the task is dropped.
///
/// The inbox only. `IDLE` watches the selected folder, so watching everything
/// would mean a connection per folder — and the folder whose mail is worth
/// interrupting someone for is the one they are waiting on.
///
/// Never returns. Every failure becomes a pause and another attempt, because
/// the alternative is a watcher that quietly stops watching.
pub async fn watch(
    config: AccountConfig,
    mut on_event: impl FnMut(WatchEvent),
    sleep: impl Fn(Duration) -> futures::future::BoxFuture<'static, ()>,
) {
    let mut waited: Option<Duration> = None;

    loop {
        match watch_once(&config, &mut on_event, &sleep).await {
            // A clean return means the loop inside stopped for a reason it
            // has already reported; go straight back to connecting.
            Ok(()) => waited = None,
            Err(e) => {
                let event = after_failure(&e, &mut waited);
                let after = match &event {
                    WatchEvent::Retrying { after, .. } | WatchEvent::Refused { after, .. } => *after,
                    _ => BACKOFF_LIMIT,
                };
                on_event(event);
                sleep(after).await;
            }
        }
    }
}

/// What a failed connection means for when to try again.
///
/// A refused login waits [`REFUSED_PAUSE`] and resets the backoff, so the
/// attempt after a fixed password starts from short pauses again. Anything
/// else backs off as usual.
fn after_failure(error: &anyhow::Error, waited: &mut Option<Duration>) -> WatchEvent {
    let reason = format!("{error:#}");
    if crate::imap::is_login_refused(error) {
        *waited = None;
        return WatchEvent::Refused { after: REFUSED_PAUSE, reason };
    }
    let after = backoff(*waited);
    *waited = Some(after);
    WatchEvent::Retrying { after, reason }
}

/// One connection's worth of watching: connect, select, and idle until
/// something goes wrong.
async fn watch_once(
    config: &AccountConfig,
    on_event: &mut impl FnMut(WatchEvent),
    sleep: &impl Fn(Duration) -> futures::future::BoxFuture<'static, ()>,
) -> Result<()> {
    let credentials = config.credentials().await?;
    let mut session = connect_idle_session(config, &credentials, IDLE_SILENCE).await?;
    session.select("INBOX").await?;

    let idles = session
        .capabilities()
        .await
        .map(|caps| caps.has_str("IDLE"))
        // A server that will not say is a server to treat as the lesser of the
        // two: polling works everywhere, and `IDLE` against a server that does
        // not have it fails every 25 minutes for ever.
        .unwrap_or(false);

    if !idles {
        on_event(WatchEvent::Polling);
        loop {
            sleep(POLL_INTERVAL).await;
            // A poll cannot tell whether anything changed, so it says so
            // unconditionally and lets the sync find out.
            on_event(WatchEvent::Changed);
        }
    }

    loop {
        let mut handle = session.idle();
        handle.init().await?;
        on_event(WatchEvent::Watching);

        let outcome = {
            let (wait, _interrupt) = handle.wait_with_timeout(IDLE_INTERVAL);
            wait.await
        };

        // The session comes back whatever happened, and it has to: the next
        // `IDLE` goes out on it. `done()` consumes the handle either way, so
        // the error is read first.
        let outcome = outcome?;
        session = handle.done().await?;

        match outcome {
            async_imap::extensions::idle::IdleResponse::NewData(_) => {
                on_event(WatchEvent::Changed)
            }
            // The re-issue RFC 2177 asks for. Nothing has happened; go round.
            async_imap::extensions::idle::IdleResponse::Timeout => {}
            // Nothing interrupts this handle — the stop source is dropped
            // immediately above — so this is a connection that has gone away.
            async_imap::extensions::idle::IdleResponse::ManualInterrupt => {
                anyhow::bail!("the idle connection closed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_pause_is_short_because_the_usual_failure_is_brief() {
        // A laptop that has just woken has no network for a second, not for
        // five minutes.
        assert_eq!(backoff(None), BACKOFF_FIRST);
    }

    #[test]
    fn each_failure_waits_twice_as_long_up_to_a_limit() {
        let mut waited = backoff(None);
        for _ in 0..20 {
            let next = backoff(Some(waited));
            assert!(next >= waited);
            waited = next;
        }
        assert_eq!(waited, BACKOFF_LIMIT, "and stops there rather than growing for ever");
    }

    #[test]
    fn a_refused_login_waits_an_hour_instead_of_backing_off() {
        // The failure that gets a whole machine banned from a shared host:
        // a password the server will keep refusing, retried every five
        // minutes for ever.
        let refused = anyhow::Error::new(crate::imap::LoginRefused {
            message: "wes@example.com: [AUTHENTICATIONFAILED] Authentication failed.".into(),
        })
        .context("connecting the watcher");
        let mut waited = Some(Duration::from_secs(40));
        match after_failure(&refused, &mut waited) {
            WatchEvent::Refused { after, reason } => {
                assert_eq!(after, REFUSED_PAUSE);
                assert!(reason.contains("AUTHENTICATIONFAILED"), "the server's own words survive: {reason}");
            }
            other => panic!("a refused login is not a network blip: {other:?}"),
        }
        assert_eq!(waited, None, "the backoff starts over once the password is fixed");
        assert!(REFUSED_PAUSE >= Duration::from_secs(30 * 60), "rare enough to stay under a ban threshold");
    }

    #[test]
    fn a_dropped_connection_still_backs_off_as_before() {
        let dropped = anyhow::anyhow!("connection timed out");
        let mut waited = None;
        assert_eq!(
            after_failure(&dropped, &mut waited),
            WatchEvent::Retrying { after: BACKOFF_FIRST, reason: "connection timed out".into() }
        );
        assert!(matches!(after_failure(&dropped, &mut waited), WatchEvent::Retrying { after, .. } if after == BACKOFF_FIRST * 2));
    }

    #[test]
    fn the_idle_interval_stays_under_what_the_rfc_allows() {
        // Past 29 minutes a server is entitled to hang up, and there is
        // nothing to be gained from the last minute of it.
        assert!(IDLE_INTERVAL < Duration::from_secs(29 * 60));
        // The transport gives up on a socket that has gone quiet, and IDLE is
        // a command that is quiet on purpose. If this ever inverts, every
        // watcher is torn down and rebuilt on a timer instead of idling.
        assert!(
            IDLE_SILENCE > IDLE_INTERVAL,
            "a watcher would time out mid-IDLE: {IDLE_SILENCE:?} <= {IDLE_INTERVAL:?}",
        );
    }
}
