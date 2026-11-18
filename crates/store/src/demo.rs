//! Generates a realistic mailbox with no server attached.
//!
//! This exists to make the performance claim falsifiable. "Fast" is easy to
//! assert against forty messages; the interesting question is what the list,
//! the search box and the scrollbar do at a hundred thousand. Running
//! `spool --demo 100000` builds exactly that, deterministically, so a
//! regression shows up as a number rather than as a feeling.

use crate::{IncomingMessage, Store};
use anyhow::Result;
use spool_core::{
    AccountId, Address, Attachment, Flags, InlineImage, MailboxId, MailboxRole,
    unsubscribe::ListUnsubscribe,
};

/// xorshift64*. Seeded explicitly so two runs produce byte-identical corpora
/// and timings are comparable across builds.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }

    /// True with probability `percent`.
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

const FIRST_NAMES: &[&str] = &[
    "Ana", "Bo", "Carmen", "Dev", "Elif", "Farouk", "Greta", "Hana", "Ivan", "Jo", "Kwame",
    "Lena", "Mira", "Nils", "Omar", "Priya", "Quinn", "Rosa", "Sami", "Tomas", "Uma", "Vera",
    "Wes", "Xiu", "Yara", "Zane",
];

const LAST_NAMES: &[&str] = &[
    "Alvarez", "Bergman", "Chen", "Dubois", "Egan", "Fischer", "Gupta", "Haddad", "Ivanova",
    "Jensen", "Kowalski", "Lindqvist", "Moreau", "Nakamura", "Okafor", "Petrov", "Quintero",
    "Rossi", "Silva", "Takahashi", "Ueda", "Vargas", "Weber", "Yilmaz", "Zhang",
];

const DOMAINS: &[&str] = &[
    "example.com", "corp.example", "mail.example", "lists.example.org", "notify.example.net",
    "team.example.io",
];

const SUBJECT_HEADS: &[&str] = &[
    "Q3 planning", "Design review", "Deploy window", "Invoice", "Standup notes", "Contract",
    "Roadmap", "Postmortem", "Hiring loop", "Budget", "Security audit", "Migration",
    "Customer escalation", "Board update", "Release", "Retro", "Onboarding", "Renewal",
    "Performance regression", "API deprecation", "Data request", "Offsite", "Pricing",
    "Support handoff", "Compliance",
];

const SUBJECT_TAILS: &[&str] = &[
    "for next week", "— needs your sign-off", "(draft)", "follow-up", "before Friday",
    "v2", "questions", "final", "revisited", "blocked", "thoughts?", "recap", "action items",
];

const SENTENCES: &[&str] = &[
    "Circling back on this before it slips another week.",
    "I pushed the branch and left two comments inline.",
    "The numbers moved about four percent after the change.",
    "Can you take a look when you get a moment?",
    "Attaching the latest version; the second half is new.",
    "We should decide by Thursday or it lands in the next cycle.",
    "Short version: it works, but the migration path is ugly.",
    "I disagree on the timeline, and here is why.",
    "Confirmed with the vendor this morning.",
    "No action needed, just keeping you in the loop.",
    "Let me know if you would rather discuss live.",
    "The staging environment is back up.",
    "This is the third time we have hit the same edge case.",
    "Ballpark is two weeks, assuming nothing else lands on us.",
    "Approved on my side.",
];

const CLOSERS: &[&str] = &["Thanks,", "Best,", "Cheers,", "Talk soon,", "\u{2014}"];

/// The seed every demo mailbox is built from, so the one a visitor tries is
/// the one the screenshots were taken of.
pub const SEED: u64 = 0x5EED;

/// Builds a complete demo mailbox at `path`: the messages, the people index,
/// and a client book beside it, replacing whatever was there.
///
/// One function for both ways in — `spool --demo` and the welcome screen's
/// button — so the two cannot drift into different demos. `progress` hears
/// how many messages are written so far, a batch at a time.
///
/// Returns the store, already open, and how many clients the book holds.
pub fn create(
    path: &std::path::Path,
    count: usize,
    mut progress: impl FnMut(usize),
) -> Result<(Store, usize)> {
    let book_path = crate::book::Book::path_beside(path);
    for file in [path, book_path.as_path()] {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", file.display()));
        }
    }

    let store = Store::open(path)?;
    // Indexed as it is written, whether or not Clients is on, so the book has
    // people to be built from — and so a demo that adds Clients later finds
    // it full.
    store.set_people_index(true)?;
    populate_with_progress(&store, count, SEED, &mut progress)?;

    let book = crate::book::Book::open(&book_path)?;
    let clients = populate_book(&store, &book)?;
    Ok((store, clients))
}

/// Builds `count` messages spread across two accounts and their folders.
///
/// Returns the accounts created, so the caller can select one.
pub fn populate(store: &Store, count: usize, seed: u64) -> Result<Vec<AccountId>> {
    populate_with_progress(store, count, seed, &mut |_| {})
}

/// As [`populate`], saying after each batch how many messages are written.
pub fn populate_with_progress(
    store: &Store,
    count: usize,
    seed: u64,
    progress: &mut dyn FnMut(usize),
) -> Result<Vec<AccountId>> {
    let mut rng = Rng(seed | 1);

    let work = store.add_account("Work", "wes@example.com", 0)?;
    let personal = store.add_account("Personal", "wes@personal.example", 5)?;

    let mut destinations = Vec::new();
    for (account, folders) in [
        (
            work,
            &[
                ("INBOX", MailboxRole::Inbox, 60u32),
                ("Sent", MailboxRole::Sent, 15),
                ("Archive", MailboxRole::Archive, 15),
                ("Lists/rust", MailboxRole::Other, 5),
                ("Junk", MailboxRole::Junk, 3),
                ("Trash", MailboxRole::Trash, 2),
            ][..],
        ),
        (
            personal,
            &[
                ("INBOX", MailboxRole::Inbox, 70),
                ("Sent", MailboxRole::Sent, 20),
                ("Archive", MailboxRole::Archive, 10),
            ][..],
        ),
    ] {
        for (path, role, weight) in folders {
            let id = store.upsert_mailbox(account, path, *role)?;
            destinations.push((account, id, *role, *weight));
        }
    }

    // Weighted pick, so the inbox holds most of the mail like a real account.
    let total_weight: u32 = destinations.iter().map(|(_, _, _, w)| w).sum();

    // Walk backwards from now so the newest message is "just arrived".
    let now = now_seconds();
    let mut timestamp = now;

    const BATCH: usize = 2_000;
    let mut batch: Vec<IncomingMessage> = Vec::with_capacity(BATCH);
    let mut threads: Vec<(String, String, MailboxId)> = Vec::new();

    for seq in 0..count {
        // Messages arrive in bursts during the day and go quiet at night;
        // an even spacing makes the date column look obviously synthetic.
        timestamp -= (rng.below(900) + 30) as i64;

        let (account, mailbox, role, _) = {
            let mut pick = rng.below(total_weight as usize) as u32;
            destinations
                .iter()
                .find(|(_, _, _, w)| {
                    if pick < *w {
                        true
                    } else {
                        pick -= w;
                        false
                    }
                })
                .copied()
                .unwrap_or(destinations[0])
        };

        let from = random_address(&mut rng);
        let me = Address::new(
            "Wes",
            if account == work { "wes@example.com" } else { "wes@personal.example" },
        );

        // Roughly a third of messages continue an existing conversation.
        let reply_to = (!threads.is_empty() && rng.chance(32))
            .then(|| threads[rng.below(threads.len())].clone());

        let (subject, in_reply_to, references) = match &reply_to {
            Some((parent_id, parent_subject, _)) => (
                format!("Re: {}", parent_subject.trim_start_matches("Re: ")),
                parent_id.clone(),
                vec![parent_id.clone()],
            ),
            None => (
                format!("{} {}", rng.pick(SUBJECT_HEADS), rng.pick(SUBJECT_TAILS)),
                String::new(),
                Vec::new(),
            ),
        };

        let rfc_message_id = format!("<{seq:08x}.{}@example.com>", rng.next() & 0xffff_ffff);
        let plain = body_text(&mut rng, &from, &subject);

        let mut flags = Flags::default();
        // Older mail has mostly been read; the newest is what is still
        // unread. Capped short of certainty so that even the archive holds a
        // few stragglers, the way a real account does.
        let age_days = (now - timestamp) / 86_400;
        let seen_chance = (58 + age_days.min(30)).min(88) as u64;
        if role == MailboxRole::Sent || rng.chance(seen_chance) {
            flags = flags.with(Flags::SEEN, true);
        }
        if rng.chance(6) {
            flags = flags.with(Flags::FLAGGED, true);
        }
        if reply_to.is_some() && rng.chance(40) {
            flags = flags.with(Flags::ANSWERED, true);
        }
        // Rarer than a reply, which is the true proportion and also what keeps
        // the second arrow worth noticing when it does turn up.
        if rng.chance(10) {
            flags = flags.with(Flags::FORWARDED, true);
        }

        let attachments = if rng.chance(12) {
            vec![Attachment {
                name: rng
                    .pick(&["report.pdf", "notes.md", "budget.xlsx", "mock.png", "patch.diff"])
                    .to_string(),
                mime: "application/octet-stream".into(),
                size: (rng.below(4_000_000) + 8_000) as u32,
                content_id: None,
                inline: false,
            }]
        } else {
            Vec::new()
        };

        // Most real mail is HTML, and the reader needs some to render. A
        // third of the corpus gets a body built from one of the shapes that
        // actually turn up: a receipt, a newsletter, a rich reply.
        let (html, inline_images) = match rng.below(3) {
            0 => rich_body(&mut rng, &subject),
            _ => (String::new(), Vec::new()),
        };

        let outgoing = role == MailboxRole::Sent;

        // Some of the new conversations arrive from a list, so the reader's
        // unsubscribe button has something to show up on. Chosen from the
        // sequence number rather than the generator: another draw would shift
        // every message after it, and the benchmarks compare corpora.
        let list_unsubscribe = match !outgoing && reply_to.is_none() && seq % 5 == 0 {
            true => ListUnsubscribe::Offered {
                header: format!(
                    "<mailto:leave-{seq}@lists.example.com>, <https://lists.example.com/u/{seq}>"
                ),
                one_click: seq % 2 == 0,
            },
            false => ListUnsubscribe::Absent,
        };

        let message = IncomingMessage {
            account_id: account,
            mailbox_id: mailbox,
            uid: Some(seq as u32 + 1),
            rfc_message_id: rfc_message_id.clone(),
            in_reply_to,
            references,
            from: if outgoing { me.clone() } else { from.clone() },
            to: vec![if outgoing { from } else { me }],
            cc: Vec::new(),
            reply_to: Vec::new(),
            subject: subject.clone(),
            date: timestamp,
            size: (plain.len() + rng.below(4_000)) as u32,
            plain,
            html,
            flags,
            // The demo corpus names attachments but carries no payloads: it
            // exists to measure the list and the search, not the disk.
            attachment_bytes: attachments.iter().map(|_| Vec::new()).collect(),
            attachments,
            inline_images,
            list_unsubscribe,
        };

        // Keep a bounded pool of recent conversations to reply into.
        if threads.len() < 4_000 {
            threads.push((rfc_message_id, subject, mailbox));
        } else {
            let slot = rng.below(threads.len());
            threads[slot] = (rfc_message_id, subject, mailbox);
        }

        batch.push(message);
        if batch.len() == BATCH {
            store.ingest(&batch)?;
            batch.clear();
            progress(seq + 1);
        }
    }

    if !batch.is_empty() {
        store.ingest(&batch)?;
    }
    progress(count);

    Ok(vec![work, personal])
}

fn random_address(rng: &mut Rng) -> Address {
    let first = rng.pick(FIRST_NAMES);
    let last = rng.pick(LAST_NAMES);
    let domain = rng.pick(DOMAINS);
    Address::new(
        format!("{first} {last}"),
        format!("{}.{}@{domain}", first.to_lowercase(), last.to_lowercase()),
    )
}

fn body_text(rng: &mut Rng, from: &Address, subject: &str) -> String {
    let mut out = String::with_capacity(512);

    out.push_str("Hi Wes,\n\n");
    for _ in 0..(2 + rng.below(4)) {
        out.push_str(rng.pick(SENTENCES));
        out.push(' ');
    }
    out.push_str("\n\n");

    if rng.chance(30) {
        out.push_str("A few things:\n");
        for i in 0..(2 + rng.below(3)) {
            out.push_str(&format!("- {} ({})\n", rng.pick(SENTENCES), i + 1));
        }
        out.push('\n');
    }

    if rng.chance(25) {
        out.push_str("More detail: https://example.com/docs/");
        out.push_str(&subject.to_lowercase().replace(' ', "-"));
        out.push_str("\n\n");
    }

    out.push_str(rng.pick(CLOSERS));
    out.push('\n');
    out.push_str(from.short());

    // A quoted reply tail, so the snippet logic and the quote renderer both
    // get exercised by the demo corpus.
    if rng.chance(35) {
        out.push_str("\n\nOn an earlier day, someone wrote:\n");
        for _ in 0..(1 + rng.below(3)) {
            out.push_str("> ");
            out.push_str(rng.pick(SENTENCES));
            out.push('\n');
        }
    }

    out
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// A 16x16 PNG, used as the demo corpus's inline image.
///
/// Embedded rather than generated: writing a PNG encoder to make a blue square
/// for test data would be a strange place to spend a dependency.
const DEMO_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAFklEQVR42mPI\
                        W/GPJMQwqmFUw/DVAABCwhQfcoen6wAAAABJRU5ErkJggg==";

const DEMO_CID: &str = "demo-logo@spool.invalid";

/// Builds an HTML body in one of the shapes mail actually arrives in.
///
/// The point is coverage of the renderer: a data table, a layout table, an
/// attached image, a remote image and a tracking pixel each appear here,
/// because each is a different path through the parser.
fn rich_body(rng: &mut Rng, subject: &str) -> (String, Vec<InlineImage>) {
    let logo = InlineImage {
        content_id: DEMO_CID.to_string(),
        mime: "image/png".to_string(),
        bytes: decode_demo_png(),
    };

    match rng.below(3) {
        // A receipt: a real data table, which should render as a grid.
        0 => {
            let total = 20 + rng.below(400);
            let shipping = rng.below(12);
            (
                format!(
                    "<html><body>\
                     <table><tr><td>\
                       <img src=\"cid:{DEMO_CID}\" width=\"16\" height=\"16\" alt=\"Logo\">\
                       <h2>{subject}</h2>\
                       <p>Thanks for your order. Here is what you paid for.</p>\
                       <table>\
                         <tr><th>Item</th><th>Qty</th><th>Price</th></tr>\
                         <tr><td>Widget</td><td>2</td><td>${total}.00</td></tr>\
                         <tr><td>Shipping</td><td>&mdash;</td><td>${shipping}.00</td></tr>\
                         <tr><td><b>Total</b></td><td></td><td><b>${}.00</b></td></tr>\
                       </table>\
                       <p><a href=\"https://example.com/orders/{}\">View your order</a></p>\
                     </td></tr></table>\
                     <img src=\"https://tracker.example.com/open.gif\" width=\"1\" height=\"1\">\
                     </body></html>",
                    total + shipping,
                    1000 + rng.below(9000),
                ),
                vec![logo],
            )
        }

        // A newsletter: nested layout tables and a remote hero image, which
        // should unwrap to ordinary flow with one blocked image.
        1 => (
            format!(
                "<html><body>\
                 <table width=\"100%\"><tr><td>\
                   <table><tr><td>\
                     <img src=\"https://cdn.example.com/hero.png\" width=\"600\" height=\"200\" \
                          alt=\"This week at Example\">\
                     <h1>{subject}</h1>\
                     <p>The <b>short version</b>: three things shipped and one \
                        <i>almost</i> did.</p>\
                     <ul><li>Faster sync</li><li>Better search</li><li>Fewer crashes</li></ul>\
                     <p><a href=\"https://example.com/read\">Read the whole thing</a></p>\
                     <hr>\
                     <p><small>You are receiving this because you signed up.</small></p>\
                   </td></tr></table>\
                 </td></tr></table></body></html>"
            ),
            Vec::new(),
        ),

        // A plain rich reply, with a signature image.
        _ => (
            format!(
                "<html><body>\
                 <p>Hi Wes,</p>\
                 <p>Confirmed on my side &mdash; see <a href=\"https://example.com/doc\">the \
                    doc</a> for the details.</p>\
                 <blockquote><p>Are we still on for Thursday?</p></blockquote>\
                 <p>Cheers</p>\
                 <img src=\"cid:{DEMO_CID}\" width=\"16\" height=\"16\" alt=\"Signature\">\
                 </body></html>"
            ),
            vec![logo],
        ),
    }
}

/// Decodes [`DEMO_PNG`]. Standard base64, and only ever this one string.
fn decode_demo_png() -> Vec<u8> {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = Vec::new();
    let (mut bits, mut held) = (0u32, 0u32);
    for byte in DEMO_PNG.bytes().filter(|b| !b.is_ascii_whitespace() && *b != b'=') {
        let Some(value) = ALPHABET.iter().position(|a| *a == byte) else { continue };
        bits = (bits << 6) | value as u32;
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
        }
    }
    out
}

/// A client book for the demo mailbox: some of the people it writes to most,
/// in a sale, with notes and tags, and a handful of signups who have never
/// written at all.
///
/// Built from the people index rather than invented, so every client in it has
/// the mail the corpus says they do, and the board's signals — waiting on you,
/// gone quiet — come out of real history rather than being set by hand.
pub fn populate_book(store: &Store, book: &crate::book::Book) -> Result<usize> {
    let now = now_seconds();
    let day = 86_400;
    let stages = book.stages()?;
    let stage = |name: &str| stages.iter().find(|s| s.name == name).map(|s| s.id);

    let mut people = store.people()?;
    people.retain(|p| p.is_two_way() && !p.addr.contains("lists.") && !p.addr.contains("notify."));
    people.sort_by(|a, b| b.messages.cmp(&a.messages).then(a.addr.cmp(&b.addr)));

    let plan: &[Planned] = &[
        ("Lead", 2, None, &["web"], "Found us through the conference talk. Wants a quote for a marketing site."),
        ("Lead", 5, None, &["referral"], ""),
        ("Lead", 21, None, &["branding"], "Asked about a rebrand in spring. Follow up after their board meets."),
        ("Lead", 1, None, &[], ""),
        ("Qualified", 4, Some(12_000), &["web"], "Budget confirmed. Decision maker is their COO."),
        ("Qualified", 18, Some(8_500), &["branding", "referral"], ""),
        ("Qualified", 3, Some(22_000), &["web", "vip"], ""),
        ("Proposal", 6, Some(18_000), &["web", "retainer"], "Sent the proposal v2 with the phased timeline. They liked phase one."),
        ("Proposal", 25, Some(9_000), &["branding"], ""),
        ("Proposal", 2, Some(31_000), &["vip"], "Wants the launch before Q1. Price is not the issue; the timeline is."),
        ("Negotiation", 8, Some(26_500), &["retainer"], "Pushing back on the retainer hours. Offer 20h/month at the same rate."),
        ("Won", 40, Some(15_000), &["web"], ""),
        ("Won", 90, Some(42_000), &["retainer", "vip"], "Renewal due in March."),
        ("Won", 12, Some(7_500), &["branding"], ""),
        ("Lost", 30, Some(11_000), &["web"], "Went with a cheaper agency. Check back in six months."),
        (OFF_BOARD, 0, None, &["retainer"], "Monthly maintenance client. Invoice on the 1st."),
        (OFF_BOARD, 0, None, &["vip"], ""),
    ];

    let mut made = 0;
    for ((stage_name, days, value, tags, note), person) in plan.iter().zip(&people) {
        let client = book.ensure_client(&person.addr, &person.name, Some("wes@example.com"), now - 120 * day)?;
        if let Some(id) = stage(stage_name) {
            book.set_stage(client, Some(id), None, now - days * day)?;
        }
        if value.is_some() {
            book.edit(client, &crate::book::Edit { value: Some(*value), ..Default::default() }, now)?;
        }
        for tag in tags.iter() {
            book.tag(client, tag, now)?;
        }
        if !note.is_empty() {
            let id = book.add_note(client, note, now - days * day)?;
            if note.contains("before Q1") || note.contains("Renewal") {
                book.pin_note(id, true)?;
            }
        }
        made += 1;
    }

    // A company: everyone at one of the corpus's domains.
    let corp = book.ensure_company("corp.example", "Corp Example Inc.", now - 60 * day)?;
    book.set_stage(corp, stage("Negotiation"), None, now - 10 * day)?;
    book.edit(corp, &crate::book::Edit { value: Some(Some(64_000)), ..Default::default() }, now)?;
    book.add_note(corp, "Three teams want the design system. Procurement wants a single contract.", now - 9 * day)?;
    made += 1;

    // Signups to the second account's product, who have never written: the
    // case an import exists for.
    let signups: Vec<crate::book::Contact> = [
        ("Maya Lindqvist", "maya@studio-north.example", "pro"),
        ("Theo Okafor", "theo.okafor@gmail.com", "free"),
        ("Inés Vargas", "ines@galleria.example", "pro"),
        ("Ravi Nakamura", "ravi@printhaus.example", "team"),
        ("Lou Bergman", "lou.bergman@icloud.com", "free"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (name, email, plan))| crate::book::Contact {
        email: email.to_string(),
        name: name.to_string(),
        external_id: format!("u-{}", 1040 + i),
        fields: vec![("Plan".into(), plan.to_string()), ("Signed up".into(), format!("2026-10-{:02}", 10 + i * 3))],
        ..Default::default()
    })
    .collect();
    let report = book.import(&signups, Some("wes@personal.example"), None, &["signup".to_string()], now - 3 * day)?;
    made += report.added;
    if let Some(first) = book.client_for("ravi@printhaus.example")? {
        book.set_stage(first, stage("Lead"), None, now - 3 * day)?;
    }

    Ok(made)
}

/// One demo client: the stage, how many days in it, the deal, tags and a note.
type Planned = (&'static str, i64, Option<i64>, &'static [&'static str], &'static str);

/// Not a stage: the client stays off the board.
const OFF_BOARD: &str = "";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_png_decodes_to_a_real_png() {
        // If this string is ever mangled, the inline images in the demo
        // corpus silently become empty boxes.
        let bytes = decode_demo_png();
        assert_eq!(&bytes[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert_eq!(bytes.len(), 79);
    }

    #[test]
    fn the_rich_bodies_cover_the_shapes_the_renderer_has_to_handle() {
        let mut rng = Rng(7);
        let mut seen = (false, false, false, false);

        for _ in 0..60 {
            let (html, images) = rich_body(&mut rng, "Subject");
            if html.is_empty() {
                continue;
            }
            seen.0 |= html.contains("<th>");
            seen.1 |= html.contains("cid:") && !images.is_empty();
            seen.2 |= html.contains("https://cdn.example.com/hero.png");
            seen.3 |= html.contains(r#"width="1" height="1""#);
        }

        assert!(seen.0, "no data table");
        assert!(seen.1, "no attached image");
        assert!(seen.2, "no remote image");
        assert!(seen.3, "no tracking pixel");
    }

    #[test]
    fn a_demo_receipt_renders_as_a_grid_with_its_images_resolved() {
        // End to end through the real parser: the shape the demo generates
        // has to survive it, or the corpus is not exercising anything.
        let mut rng = Rng(0);
        let (html, images) = loop {
            let candidate = rich_body(&mut rng, "Your receipt");
            if candidate.0.contains("<th>") {
                break candidate;
            }
        };

        let blocks = spool_core::html::html_to_blocks(&html);
        let tables = blocks
            .iter()
            .filter(|b| matches!(b, spool_core::html::Block::Table(_)))
            .count();
        assert_eq!(tables, 1, "the wrapper unwraps, the receipt does not");

        // The tracking pixel is dropped and the logo is not.
        let inline: Vec<_> = blocks
            .iter()
            .filter_map(|b| match b {
                spool_core::html::Block::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        assert_eq!(inline.len(), 1, "the 1x1 pixel is not an image");
        assert_eq!(
            inline[0].source,
            spool_core::html::ImageSource::Cid(DEMO_CID.into())
        );
        assert_eq!(images[0].content_id, DEMO_CID);
    }
    use crate::{Query, Scope};

    #[test]
    fn populates_multiple_accounts_and_folders() {
        let store = Store::in_memory().unwrap();
        let accounts = populate(&store, 500, 42).unwrap();

        assert_eq!(accounts.len(), 2);
        assert_eq!(store.message_count().unwrap(), 500);

        let mailboxes = store.mailboxes().unwrap();
        assert!(mailboxes.len() >= 8);
        assert!(mailboxes.iter().any(|m| m.role == MailboxRole::Inbox && m.total > 0));
    }

    #[test]
    fn generation_is_deterministic() {
        let subjects = |seed| {
            let store = Store::in_memory().unwrap();
            populate(&store, 100, seed).unwrap();
            store
                .envelopes(Scope::Everything, &Query::All, 100)
                .unwrap()
                .into_iter()
                .map(|e| e.subject)
                .collect::<Vec<_>>()
        };
        assert_eq!(subjects(7), subjects(7));
        assert_ne!(subjects(7), subjects(8));
    }

    #[test]
    fn produces_conversations_not_just_singletons() {
        let store = Store::in_memory().unwrap();
        populate(&store, 1_000, 3).unwrap();

        let envelopes = store.envelopes(Scope::Everything, &Query::All, 1_000).unwrap();
        let distinct: std::collections::HashSet<_> =
            envelopes.iter().map(|e| e.thread_id).collect();
        assert!(
            distinct.len() < envelopes.len(),
            "some messages should share a thread: {} threads for {} messages",
            distinct.len(),
            envelopes.len()
        );
    }

    #[test]
    fn progress_counts_up_to_the_total() {
        let store = Store::in_memory().unwrap();
        let mut heard = Vec::new();
        populate_with_progress(&store, 4_500, 5, &mut |done| heard.push(done)).unwrap();
        assert_eq!(heard, vec![2_000, 4_000, 4_500]);
    }

    #[test]
    fn create_replaces_what_was_there_and_writes_a_book_beside_it() {
        let dir = std::env::temp_dir().join(format!("spool-demo-create-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.db");

        let (store, _) = create(&path, 300, |_| {}).unwrap();
        drop(store);
        let (store, clients) = create(&path, 200, |_| {}).unwrap();

        let all = store.envelopes(Scope::Everything, &Query::All, 1_000).unwrap().len();
        assert_eq!(all, 200, "a second demo starts from empty, not on top of the first");
        assert!(clients > 0, "the book has clients in it");
        assert!(dir.join("clients.db").exists(), "the book sits beside the store");

        drop(store);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn leaves_a_believable_mix_of_read_and_unread() {
        let store = Store::in_memory().unwrap();
        populate(&store, 1_000, 11).unwrap();

        let all = store.envelopes(Scope::Everything, &Query::All, 1_000).unwrap().len();
        let unread = store.envelopes(Scope::Everything, &Query::Unread, 1_000).unwrap().len();
        assert!(unread > 0 && unread < all, "{unread} unread of {all}");
    }
}
