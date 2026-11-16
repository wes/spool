//! Talking to mail servers.
//!
//! Kept apart from the store and the UI so that the parts of a mail client
//! that can hang — DNS, TLS, a server that stops responding mid-FETCH — are
//! confined to one crate with one entry point.

pub mod chatgpt;
pub mod compose;
pub mod config;
pub mod encoding;
pub mod http;
pub mod idle;
pub mod imap;
pub mod oauth;
pub mod outbox;
pub mod parse;
pub mod probe;
pub mod secrets;
pub mod smtp;
pub mod stream;
pub mod unsubscribe;

pub use config::{AccountConfig, Auth, Config, Credentials, Provider, ProviderId, Security};
pub use imap::{
    Arrival, BackfillReport, FolderOp, LoginRefused, Progress, SyncMode, SyncReport, append_all,
    append_to_folder, backfill, fetch_attachment, fetch_list_unsubscribe, folder_op,
    hierarchy_delimiter, is_login_refused, push_account, sync_account,
};
pub use idle::{WatchEvent, watch};
pub use oauth::ClientCredentials;
pub use outbox::PushReport;
pub use probe::{Outcome, Probe, Step, test_account};
pub use secrets::Slot;
