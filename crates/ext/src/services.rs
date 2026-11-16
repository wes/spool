//! What the host does on an extension's behalf.
//!
//! A trait rather than a direct call into [`spool_store::Store`], for two
//! reasons that turned out to be the same reason. The obvious one is testing:
//! the whole extension system can be exercised against a few messages held in a
//! `Vec`, with no database and no window. The less obvious one is that some of
//! these cannot be answered by the store at all — `fetch` needs the network
//! layer, and the key/value store is the app's file — so a trait is what lets
//! this crate stay above all of them instead of depending on all of them.
//!
//! Every method here is called from a background thread while a component is
//! blocked waiting on it, so an implementation that takes a second is a second
//! of an extension's deadline. None of them may touch the UI.

use spool_core::{Account, Body, Envelope, Mailbox, MessageId};
use spool_store::{Query, Scope};

/// A request for messages, in the store's own vocabulary.
#[derive(Debug, Clone)]
pub struct MailQuery {
    pub scope: Scope,
    pub filter: Query,
    pub limit: usize,
}

/// The most messages one query may return, however many were asked for.
///
/// An extension that asks for everything gets a page. The cap is here rather
/// than left to the implementation because it bounds the copy across the
/// boundary, which is the expensive part: a quarter of a million envelopes
/// serialized into a sandbox is not a query, it is an outage.
pub const MAX_QUERY: usize = 1_000;

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The host's side of the boundary.
///
/// `Send + Sync` because one of these is shared by every loaded extension, each
/// of which may be running on a different thread of the background executor.
pub trait Services: Send + Sync + 'static {
    /// Seconds since the unix epoch.
    fn now(&self) -> i64;

    /// Messages matching a query, newest first.
    ///
    /// The implementation is not required to honour a limit above
    /// [`MAX_QUERY`]; the host clamps before calling.
    fn envelopes(&self, query: MailQuery) -> Vec<Envelope>;

    fn body(&self, id: MessageId) -> Option<Body>;

    /// One of the headers the store keeps, by name, case-insensitively.
    ///
    /// See the WIT: this answers for the parsed headers that have a column and
    /// returns `None` for everything else, which an extension cannot tell apart
    /// from a header that was not there.
    fn header(&self, id: MessageId, name: &str) -> Option<String>;

    fn mailboxes(&self) -> Vec<Mailbox>;

    fn accounts(&self) -> Vec<Account>;

    /// Reads one of an extension's own stored values.
    ///
    /// `extension` is the id, supplied by the host rather than by the
    /// extension — which is what makes the namespace a fact instead of a
    /// convention one extension could decline to follow.
    fn load(&self, extension: &str, key: &str) -> Option<String>;

    fn save(&self, extension: &str, key: &str, value: &str);

    fn forget(&self, extension: &str, key: &str);

    /// Makes an HTTPS request.
    ///
    /// The host has already checked the URL against the extension's declared
    /// hosts, so an implementation may take the target as given. It still owns
    /// the timeout: this blocks a thread, and the extension's own deadline does
    /// not run while the host is the one waiting.
    ///
    /// Defaulted to a refusal so that an app which has not wired up networking
    /// yet is one where extensions cannot reach the network — the safe way
    /// round for something to be unimplemented.
    fn fetch(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let _ = request;
        Err("this build does not give extensions network access".into())
    }
}
