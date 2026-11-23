//! Domain types shared by the store, the network layer and the UI.
//!
//! Everything here is deliberately plain data: cheap to clone, cheap to move
//! across threads, and free of any dependency on a database or a UI framework.
//! The message list holds hundreds of thousands of [`Envelope`]s in memory, so
//! their size is load-bearing — keep them lean.

pub mod address;
pub mod assist;
pub mod contacts;
pub mod draft;
pub mod follow_up;
pub mod html;
pub mod mailto;
pub mod markup;
pub mod paths;
pub mod priority;
pub mod sanitize;
pub mod thread;
pub mod unsubscribe;

pub use address::Address;

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize,
            Deserialize,
        )]
        pub struct $name(pub i64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(/// Identifies a configured account.
    AccountId);
id_type!(/// Identifies a mailbox (folder) within an account.
    MailboxId);
id_type!(/// Identifies a single message in the local store.
    MessageId);
id_type!(/// Identifies a conversation: a set of messages linked by reply headers.
    ThreadId);

/// IMAP message flags, packed into a single integer so an [`Envelope`] stays small.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Flags(pub u32);

impl Flags {
    pub const SEEN: u32 = 1 << 0;
    pub const FLAGGED: u32 = 1 << 1;
    pub const ANSWERED: u32 = 1 << 2;
    pub const DRAFT: u32 = 1 << 3;
    pub const DELETED: u32 = 1 << 4;
    /// Set by the agent layer to mark a message it has already looked at.
    pub const TRIAGED: u32 = 1 << 5;
    /// Set when this message has been forwarded on.
    ///
    /// IMAP's own flags have no word for this — `\Answered` covers a reply
    /// and nothing else — so the keyword `$Forwarded` is what the clients
    /// that track it agree on, and this is that keyword.
    pub const FORWARDED: u32 = 1 << 6;

    /// The bits no IMAP server knows about.
    ///
    /// A flag sync overwrites a message's flags with the server's view, which
    /// is right for the five that came from there and would silently erase
    /// these. They are kept across that, so a message you forwarded still
    /// says so tomorrow.
    pub const LOCAL_ONLY: u32 = Self::TRIAGED | Self::FORWARDED;

    pub fn is_seen(self) -> bool {
        self.has(Self::SEEN)
    }
    pub fn is_flagged(self) -> bool {
        self.has(Self::FLAGGED)
    }
    pub fn is_answered(self) -> bool {
        self.has(Self::ANSWERED)
    }
    pub fn is_draft(self) -> bool {
        self.has(Self::DRAFT)
    }
    pub fn is_forwarded(self) -> bool {
        self.has(Self::FORWARDED)
    }
    pub fn is_triaged(self) -> bool {
        self.has(Self::TRIAGED)
    }

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    pub fn with(self, bit: u32, on: bool) -> Self {
        Flags(if on { self.0 | bit } else { self.0 & !bit })
    }

    pub fn toggled(self, bit: u32) -> Self {
        Flags(self.0 ^ bit)
    }
}

/// The well-known role a mailbox plays, resolved from IMAP SPECIAL-USE
/// attributes or, failing that, from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MailboxRole {
    Inbox,
    Sent,
    Drafts,
    Archive,
    Trash,
    Junk,
    Other,
}

impl MailboxRole {
    /// Guesses a role from a folder path. IMAP servers disagree wildly about
    /// naming, so this is a best-effort fallback behind SPECIAL-USE.
    pub fn guess(path: &str) -> Self {
        // INBOX is the one name IMAP reserves, and it reserves it only at the
        // top of the hierarchy — RFC 3501 gives it to the primary mailbox and
        // to nothing else. Matched against the whole path rather than the
        // leaf, because a folder called `[AirMail]/INBOX` is an ordinary
        // folder that happens to share the name, and calling it the inbox
        // made it impossible to rename or delete.
        //
        // The other roles do read the leaf: Gmail's real Sent folder is
        // `[Gmail]/Sent Mail`, nested and special-use both, and the servers
        // that nest under `INBOX.` do the same with the rest.
        if path.eq_ignore_ascii_case("inbox") {
            return Self::Inbox;
        }

        let leaf = path.rsplit(['/', '.']).next().unwrap_or(path).to_lowercase();
        match leaf.as_str() {
            "sent" | "sent items" | "sent mail" | "sent messages" => Self::Sent,
            "drafts" | "draft" => Self::Drafts,
            "archive" | "all mail" | "archived" => Self::Archive,
            "trash" | "deleted" | "deleted items" | "bin" => Self::Trash,
            "junk" | "spam" | "bulk mail" => Self::Junk,
            _ => Self::Other,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Archive => "archive",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Other => "other",
        }
    }

    /// Inverse of [`MailboxRole::as_str`], for reading a role back out of the
    /// store. Deliberately not `FromStr`: an unrecognized role is `Other`,
    /// not an error.
    pub fn from_stored(s: &str) -> Self {
        match s {
            "inbox" => Self::Inbox,
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "archive" => Self::Archive,
            "trash" => Self::Trash,
            "junk" => Self::Junk,
            _ => Self::Other,
        }
    }

    /// A single glyph used in the sidebar, chosen to render in the UI font
    /// without pulling in an icon set.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Inbox => "\u{2709}",
            Self::Sent => "\u{2197}",
            Self::Drafts => "\u{270E}",
            Self::Archive => "\u{25A4}",
            Self::Trash => "\u{2298}",
            Self::Junk => "\u{2298}",
            Self::Other => "\u{25AB}",
        }
    }

    /// Ordering used to sort the sidebar so the important folders sit on top.
    pub fn sort_key(self) -> u8 {
        match self {
            Self::Inbox => 0,
            Self::Drafts => 1,
            Self::Sent => 2,
            Self::Archive => 3,
            Self::Junk => 4,
            Self::Trash => 5,
            Self::Other => 6,
        }
    }
}

/// A configured account. Credentials never live here — see `spool_net::Credentials`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub email: String,
    /// Index into the UI accent palette, so each account gets a stable color.
    pub color: u8,
}

/// A folder within an account, with its cached counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: MailboxId,
    pub account_id: AccountId,
    /// Server path, e.g. `INBOX.Lists.rust`.
    pub path: String,
    /// Display name, e.g. `rust`.
    pub name: String,
    pub role: MailboxRole,
    pub total: u32,
    pub unread: u32,
    /// How many messages the server said this folder holds, at the last sync.
    ///
    /// Kept beside the local count so the difference between them can be shown
    /// rather than guessed at. `0` means nobody has asked yet — a folder that
    /// has never synced, or one from a database written before this was
    /// recorded.
    pub server_total: u32,
}

impl Mailbox {
    /// How many messages are on the server and not here.
    ///
    /// `None` when there is no reason to think anything is missing, which
    /// includes never having asked. A local count *above* the server's is not
    /// a negative number of missing messages — it is a folder something was
    /// moved into since the last sync.
    pub fn not_downloaded(&self) -> Option<u32> {
        (self.server_total > self.total).then(|| self.server_total - self.total)
    }
}

#[cfg(test)]
mod mailbox_tests {
    use super::*;

    fn mailbox(total: u32, server_total: u32) -> Mailbox {
        Mailbox {
            id: MailboxId(1),
            account_id: AccountId(1),
            path: "INBOX".into(),
            name: "Inbox".into(),
            role: MailboxRole::Inbox,
            total,
            unread: 0,
            server_total,
        }
    }

    #[test]
    fn a_partly_downloaded_folder_says_how_much_is_missing() {
        assert_eq!(mailbox(2_000, 8_412).not_downloaded(), Some(6_412));
    }

    #[test]
    fn a_folder_that_is_all_here_says_nothing() {
        assert_eq!(mailbox(2_000, 2_000).not_downloaded(), None);
    }

    #[test]
    fn a_folder_nobody_has_asked_about_says_nothing() {
        // `server_total` is zero on a folder that has never synced and on one
        // from a database written before it was recorded. Neither is a reason
        // to claim mail is missing.
        assert_eq!(mailbox(2_000, 0).not_downloaded(), None);
    }

    #[test]
    fn holding_more_than_the_server_does_is_not_a_negative_shortfall() {
        // A folder something was moved into since the last sync.
        assert_eq!(mailbox(2_010, 2_000).not_downloaded(), None);
    }
}

/// The summary of a message: everything the list needs, and nothing more.
///
/// This is the hot type. One is held per message in the open mailbox, so it
/// stays free of large owned buffers — the body lives in the store until asked
/// for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: MessageId,
    pub account_id: AccountId,
    pub mailbox_id: MailboxId,
    pub thread_id: ThreadId,
    pub from: Address,
    pub subject: String,
    /// Unix seconds. Sorting key for the list.
    pub date: i64,
    /// First ~200 chars of the plain-text body, precomputed at ingest.
    pub snippet: String,
    pub flags: Flags,
    pub has_attachments: bool,
    pub size: u32,
    /// The rest of the conversation, when the list is grouping.
    ///
    /// `None` means this row is one message and nothing more — which is what
    /// every row is with grouping off, and what a one-message thread is even
    /// with it on. A row that stands for several messages carries them here
    /// rather than in a parallel list, so an action on the row cannot act on a
    /// different set than the one it was drawn from.
    pub group: Option<Group>,
}

/// A conversation, as one row of the list.
///
/// The counts are of the messages **in the current scope**, not of the whole
/// thread: a conversation with nine replies filed away and one in the inbox is
/// one message in the inbox, and saying "10" next to it would be a promise the
/// row cannot keep when you open it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// Every message the row stands for, newest first. Includes the one the
    /// envelope itself describes.
    pub ids: Vec<MessageId>,
    pub unread: u32,
    /// Who wrote, oldest first, deduplicated — the short form for the row.
    pub participants: Vec<String>,
}

impl Group {
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

impl Envelope {
    /// Every message this row acts on: the conversation, or just itself.
    pub fn message_ids(&self) -> Vec<MessageId> {
        match &self.group {
            Some(group) if !group.ids.is_empty() => group.ids.clone(),
            _ => vec![self.id],
        }
    }

    /// How many messages the row stands for.
    pub fn message_count(&self) -> usize {
        self.group.as_ref().map_or(1, Group::len).max(1)
    }

    /// Whether anything the row stands for is unread.
    ///
    /// Not the same as the newest message being unread: a thread whose latest
    /// reply you have read can still hold one you have not.
    pub fn any_unread(&self) -> bool {
        match &self.group {
            Some(group) => group.unread > 0,
            None => !self.flags.is_seen(),
        }
    }
}

/// The full content of one message, loaded on demand when it is opened.
#[derive(Debug, Clone, Default)]
pub struct Body {
    pub plain: String,
    pub html: String,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    /// The original's own `Message-ID`, as it arrived.
    ///
    /// Carried on the body rather than the envelope because the envelope is
    /// loaded for every row in the list and this is wanted for exactly one
    /// message at a time — the open one, which is the only one anybody
    /// replies to.
    pub message_id: String,
    /// Its `References` chain, oldest first.
    pub references: Vec<String>,
    /// Its `Reply-To`, which overrides the sender when a reply is composed.
    ///
    /// This is what makes replying to a mailing list address the list rather
    /// than whoever happened to post.
    pub reply_to: Vec<Address>,
    pub attachments: Vec<Attachment>,
    /// Images the message carries, keyed by the `cid:` the HTML refers to.
    ///
    /// Held as bytes because that is the whole point: an image that arrived
    /// with the message can be shown without asking anyone for anything.
    pub inline_images: Vec<InlineImage>,
    /// How to leave the mailing list this came from, if it is from one.
    pub list_unsubscribe: unsubscribe::ListUnsubscribe,
}

impl Body {
    /// Finds the bytes for a `cid:` reference.
    ///
    /// Content-IDs are compared case-insensitively and without brackets,
    /// because senders are inconsistent about both and a near-miss shows the
    /// reader a broken image for no reason.
    pub fn inline_image(&self, content_id: &str) -> Option<&InlineImage> {
        let wanted = normalize_content_id(content_id);
        self.inline_images.iter().find(|i| normalize_content_id(&i.content_id) == wanted)
    }

    /// Whether `attachment` is drawn in the body: an embedded image whose
    /// bytes the body holds, which means the HTML points at it.
    ///
    /// Not [`Attachment::is_embedded_image`] alone. That says what the part
    /// looks like, and plenty of parts look embedded that nothing draws —
    /// Gmail gives every attachment a Content-ID, so a photo sent from it is
    /// an inline image by every header it has, and the HTML never mentions
    /// it. Nor is an image past the size the store keeps drawn, however the
    /// HTML refers to it.
    pub fn draws(&self, attachment: &Attachment) -> bool {
        attachment.is_embedded_image()
            && attachment.content_id.as_deref().is_some_and(|id| self.inline_image(id).is_some())
    }

    /// The attachments to offer as files — every one the body does not draw —
    /// each with its position in the full list, which is how its bytes are
    /// found.
    pub fn files(&self) -> impl Iterator<Item = (usize, &Attachment)> {
        self.attachments.iter().enumerate().filter(|(_, attachment)| !self.draws(attachment))
    }
}

/// An image part carried by the message itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    /// The `Content-ID`, stored without angle brackets.
    pub content_id: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Strips the brackets and case that senders vary between the header form
/// (`<id@host>`) and the URL form (`cid:id@host`).
pub fn normalize_content_id(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_ascii_lowercase()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub size: u32,
    /// `Content-ID`, when the part has one. Its presence is what lets the HTML
    /// body point at this part with a `cid:` URL.
    #[serde(default)]
    pub content_id: Option<String>,
    /// `Content-Disposition: inline` — the sender meant it to appear in the
    /// message rather than hang off the bottom of it.
    #[serde(default)]
    pub inline: bool,
}

impl Attachment {
    /// Whether this part is made to be shown in the body: an inline image the
    /// HTML could point at. Whether it actually is shown is
    /// [`Body::draws`], which is what decides if it is listed as a file.
    pub fn is_embedded_image(&self) -> bool {
        self.inline && self.content_id.is_some() && self.mime.starts_with("image/")
    }
}

/// Human-readable byte size, e.g. `1.4 MB`.
pub fn format_size(bytes: u32) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / (KB * KB))
    }
}

/// Formats a timestamp for the message list: always a time, prefixed by the
/// day it belongs to.
///
/// The time is the part you scan for — "did this land before or after the
/// standup?" — so it is never dropped, and the day is named relatively while
/// that still means something: `Today 10:30am`, then `Sun 10:30am` back
/// through the past week, then a date.
///
/// `now` and `offset` are passed in so this stays pure. `offset` is seconds
/// east of UTC — the viewer's zone at `ts`, since a message from last summer
/// arrived on summer's clock. It shifts `now` too, which is wrong only for
/// the minutes either side of midnight on a changeover day, and only in which
/// of two adjacent days the row is said to belong to.
pub fn format_date(ts: i64, now: i64, offset: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let (ts, now) = (ts + offset, now + offset);
    let (y, mo, d, h, mi) = civil_from_unix(ts);
    let (ny, _, _, _, _) = civil_from_unix(now);
    let (hour12, ampm) = to_12h(h);
    let time = format!("{hour12}:{mi:02}{}", ampm.to_ascii_lowercase());

    // Whole days apart, not seconds apart: 11pm yesterday is yesterday at
    // 1am today, however few hours ago it was.
    let days_ago = now.div_euclid(86_400) - ts.div_euclid(86_400);

    let day = if days_ago == 0 {
        "Today".to_string()
    } else if (1..7).contains(&days_ago) {
        // 1970-01-01 was a Thursday, hence the table's rotation.
        DAYS[(ts.div_euclid(86_400)).rem_euclid(7) as usize].to_string()
    } else if y == ny {
        format!("{} {d}", MONTHS[(mo - 1) as usize])
    } else {
        format!("{mo}/{d}/{:02}", y.rem_euclid(100))
    };

    format!("{day} {time}")
}

/// The same thing, in its two halves.
///
/// The list wants them apart so the day can sit against the left of its column
/// and the time against the right — a ragged single string makes the times of
/// a hundred rows impossible to compare down the page. Built from the same
/// function so the two forms cannot drift.
pub fn format_date_parts(ts: i64, now: i64, offset: i64) -> (String, String) {
    let both = format_date(ts, now, offset);
    match both.rsplit_once(' ') {
        Some((day, time)) => (day.to_string(), time.to_string()),
        // Unreachable while the format above has a space in it, and a whole
        // string in the day half is the harmless way to be wrong.
        None => (both, String::new()),
    }
}

/// The long form shown in an open message's header, and in the attribution
/// line of a reply.
///
/// `offset` is seconds east of UTC, as in [`format_date`].
pub fn format_date_long(ts: i64, offset: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September",
        "October", "November", "December",
    ];
    let (y, mo, d, h, mi) = civil_from_unix(ts + offset);
    let (hour12, ampm) = to_12h(h);
    format!("{} {d}, {y} at {hour12}:{mi:02} {ampm}", MONTHS[(mo - 1) as usize])
}

fn to_12h(h: i64) -> (i64, &'static str) {
    let ampm = if h < 12 { "AM" } else { "PM" };
    let hour12 = match h % 12 {
        0 => 12,
        n => n,
    };
    (hour12, ampm)
}

/// The date format an email header wants: `Sat, 05 Sep 2026 12:34:56 +0000`.
///
/// Always UTC. A client that reports its local zone leaks roughly where the
/// sender is, and `+0000` is both truthful and universally accepted.
pub fn format_rfc5322_date(ts: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let (year, month, day, hour, minute) = civil_from_unix(ts);
    let second = ts.rem_euclid(60);
    // 1970-01-01 was a Thursday, hence the table's rotation.
    let weekday = DAYS[(ts.div_euclid(86_400)).rem_euclid(7) as usize];

    format!(
        "{weekday}, {day:02} {} {year} {hour:02}:{minute:02}:{second:02} +0000",
        MONTHS[(month - 1) as usize],
    )
}

/// Midnight UTC on a civil date, as unix seconds.
///
/// Howard Hinnant's days-from-civil, the forward direction of
/// [`civil_from_unix`]. Used by search, where `after:2026-01-01` has to become
/// a number the store can compare against.
pub fn unix_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;

    (era * 146_097 + doe - 719_468) * 86_400
}

/// Days-from-civil, inverted. Howard Hinnant's algorithm — avoids pulling in a
/// date library for the handful of conversions the UI needs.
pub fn civil_from_unix(ts: i64) -> (i64, i64, i64, i64, i64) {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, secs / 3600, (secs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(name: &str, content_id: &str) -> Attachment {
        Attachment {
            name: name.into(),
            mime: "image/png".into(),
            size: 1_500_000,
            content_id: Some(content_id.into()),
            inline: true,
        }
    }

    #[test]
    fn only_an_image_the_body_draws_is_left_out_of_the_files() {
        // Five photos from Gmail: a Content-ID on each, and an HTML body that
        // names none of them. They look embedded, and nothing draws them.
        let mut body = Body {
            attachments: (1..=5).map(|n| image(&format!("IMG_{n}.png"), &format!("id{n}"))).collect(),
            ..Body::default()
        };
        assert!(body.attachments.iter().all(Attachment::is_embedded_image));
        assert_eq!(body.files().count(), 5, "every one of them is offered as a file");

        // One the HTML does point at, whose bytes the body therefore holds,
        // is drawn — and so is the one left out. Ids compare as `cid:` URLs do.
        body.inline_images.push(InlineImage {
            content_id: "ID3".into(),
            mime: "image/png".into(),
            bytes: vec![0],
        });
        assert!(body.draws(&body.attachments[2]));
        let files: Vec<usize> = body.files().map(|(index, _)| index).collect();
        assert_eq!(files, [0, 1, 3, 4], "listed by position in the full list");
    }

    #[test]
    fn flags_round_trip() {
        let f = Flags::default().with(Flags::SEEN, true).with(Flags::FLAGGED, true);
        assert!(f.is_seen() && f.is_flagged());
        assert!(!f.toggled(Flags::SEEN).is_seen());
    }

    #[test]
    fn civil_conversion_matches_known_dates() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0));
        // 2024-02-29T12:34:00Z — a leap day, to exercise the era math.
        assert_eq!(civil_from_unix(1_709_210_040), (2024, 2, 29, 12, 34));
    }

    #[test]
    fn roles_are_guessed_from_leaf_names() {
        assert_eq!(MailboxRole::guess("INBOX"), MailboxRole::Inbox);
        assert_eq!(MailboxRole::guess("inbox"), MailboxRole::Inbox, "the name is case-insensitive");
        assert_eq!(MailboxRole::guess("INBOX.Sent Items"), MailboxRole::Sent);
        assert_eq!(MailboxRole::guess("[Gmail]/All Mail"), MailboxRole::Archive);
        assert_eq!(MailboxRole::guess("Lists/rust"), MailboxRole::Other);

        // Only the top-level INBOX is the inbox. A folder nested under
        // anything else is an ordinary folder, whatever it is called, and has
        // to stay renameable and deletable.
        assert_eq!(MailboxRole::guess("[AirMail]/INBOX"), MailboxRole::Other);
        assert_eq!(MailboxRole::guess("INBOX.Inbox"), MailboxRole::Other);
        assert_eq!(MailboxRole::guess("Archive/inbox"), MailboxRole::Other);
    }

    #[test]
    fn rfc5322_dates_match_the_header_format() {
        // 2024-02-29T12:34:00Z, a Thursday.
        assert_eq!(format_rfc5322_date(1_709_210_040), "Thu, 29 Feb 2024 12:34:00 +0000");
        assert_eq!(format_rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn civil_dates_convert_back_to_the_timestamps_they_came_from() {
        for ts in [0_i64, 1_709_210_040, 1_700_000_000, -86_400, 4_102_444_800] {
            let (y, m, d, _, _) = civil_from_unix(ts);
            assert_eq!(
                unix_from_civil(y, m, d),
                ts.div_euclid(86_400) * 86_400,
                "{ts} did not survive the round trip",
            );
        }
        // The leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(unix_from_civil(2024, 2, 29), 1_709_164_800);
        assert_eq!(unix_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn list_dates_always_carry_a_time() {
        // 2026-09-07 (a Monday) at 10:30 UTC, read four hours later.
        let today = unix_from_civil(2026, 9, 7) + 10 * 3600 + 30 * 60;
        let now = today + 4 * 3600;

        assert_eq!(format_date(today, now, 0), "Today 10:30am");
        // Yesterday, a Sunday — named, not "1 day ago".
        assert_eq!(format_date(today - 86_400, now, 0), "Sun 10:30am");
        // Late last night is still yesterday, though it is hours ago.
        assert_eq!(
            format_date(unix_from_civil(2026, 9, 6) + 23 * 3600, now, 0),
            "Sun 11:00pm",
        );
        // Past the week, the weekday stops locating anything.
        assert_eq!(format_date(unix_from_civil(2026, 8, 30) + 12 * 3600, now, 0), "Aug 30 12:00pm");
        assert_eq!(format_date(unix_from_civil(2025, 3, 4) + 60, now, 0), "3/4/25 12:01am");
    }

    #[test]
    fn the_two_halves_split_on_the_last_space() {
        // "Aug 30 12:00pm" has two spaces in it, and the day is the part
        // before the *last* one. Splitting on the first would give a column
        // reading "Aug" beside a time of "30 12:00pm".
        let now = unix_from_civil(2026, 9, 7) + 14 * 3600;

        assert_eq!(
            format_date_parts(unix_from_civil(2026, 8, 30) + 12 * 3600, now, 0),
            ("Aug 30".to_string(), "12:00pm".to_string())
        );
        assert_eq!(
            format_date_parts(unix_from_civil(2026, 9, 7) + 10 * 3600 + 30 * 60, now, 0),
            ("Today".to_string(), "10:30am".to_string())
        );
        assert_eq!(
            format_date_parts(unix_from_civil(2025, 3, 4) + 60, now, 0),
            ("3/4/25".to_string(), "12:01am".to_string())
        );
    }

    #[test]
    fn the_halves_rejoin_into_what_the_whole_says() {
        // They are two views of one value, and a test that says so is what
        // stops them drifting the next time either is edited.
        let now = unix_from_civil(2026, 9, 7) + 14 * 3600;
        for ts in [now - 60, now - 86_400, now - 40 * 86_400, now - 500 * 86_400] {
            let (day, time) = format_date_parts(ts, now, 0);
            assert_eq!(format!("{day} {time}"), format_date(ts, now, 0));
        }
    }

    #[test]
    fn dates_are_shown_on_the_viewers_clock() {
        // 2026-09-07T02:30Z. In New York that is still the 6th, at 10:30pm,
        // which is a different time *and* a different day — the whole reason
        // the offset cannot be left out.
        let ts = unix_from_civil(2026, 9, 7) + 2 * 3600 + 30 * 60;
        let now = ts + 8 * 3600;

        assert_eq!(format_date(ts, now, 0), "Today 2:30am");
        assert_eq!(format_date(ts, now, -4 * 3600), "Sun 10:30pm");
        assert_eq!(format_date(ts, now, 2 * 3600), "Today 4:30am");

        assert_eq!(format_date_long(ts, 0), "September 7, 2026 at 2:30 AM");
        assert_eq!(format_date_long(ts, -4 * 3600), "September 6, 2026 at 10:30 PM");
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(3_145_728), "3.0 MB");
    }
}
