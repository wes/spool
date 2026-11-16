//! Loads the spam scorer — a real WebAssembly component — and drives it
//! against a Jev that is not there.
//!
//! The fake answers `fetch` and records what was asked. That is the whole point
//! of the boundary being a trait: an extension whose entire job is a network
//! round trip can be tested end to end, with its real component, its real
//! manifest and its real capability checks, and no API key and no network.
//!
//! What these check is the part that would break silently. The questions Jev is
//! sent are not a detail — a `score` question with four levels instead of five
//! shifts every reading — and neither is which messages are sent at all, since
//! each one is somebody's money.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use spool_core::{
    Account, AccountId, Address, Body, Envelope, Flags, Mailbox, MailboxId, MailboxRole, MessageId,
    ThreadId,
};
use spool_ext::{
    api::{Effect, Emphasis},
    config::CONFIG_FILE,
    manifest::{MANIFEST_FILE, WASM_FILE},
    registry::Registry,
    runtime::{CommandContext, Engine, Instance},
    services::{HttpRequest, HttpResponse, MailQuery, Services},
};

const INBOX: i64 = 1;
const JUNK: i64 = 2;
const ARCHIVE: i64 = 3;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The built extension, building it first if it is not there.
fn built() -> Option<PathBuf> {
    let root = repo_root();
    let out = root.join("target/extensions/spam-scorer");

    if !out.join(WASM_FILE).exists() {
        let script = root.join("scripts/build-extension.sh");
        let status = Command::new("bash").arg(&script).arg("spam-scorer").status();

        if !matches!(status, Ok(status) if status.success()) {
            eprintln!(
                "SKIPPING: could not build the spam scorer. Install the target with \
                 `rustup target add wasm32-wasip2`."
            );
            return None;
        }
    }

    Some(out)
}

/// Installs the built component into a directory of this test's own, with
/// whatever `config.toml` the test wants — including none.
fn install(name: &str, config: Option<&str>) -> PathBuf {
    let built = built().expect("checked by the caller");

    let root = std::env::temp_dir()
        .join(format!("spool-scorer-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    let dir = root.join("spam-scorer");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(built.join(WASM_FILE), dir.join(WASM_FILE)).unwrap();
    std::fs::copy(built.join(MANIFEST_FILE), dir.join(MANIFEST_FILE)).unwrap();

    if let Some(config) = config {
        std::fs::write(dir.join(CONFIG_FILE), config).unwrap();
    }

    root
}

const CONFIGURED: &str = r#"
api-key = "sk-test"
model = "jev-latest"
threshold = 3
"#;

// --- the fake -----------------------------------------------------------

/// A mailbox, a key/value store and a Jev, all held in memory.
struct Fake {
    messages: Vec<Envelope>,
    store: Mutex<BTreeMap<String, String>>,
    /// Every request the extension made, in order.
    calls: Mutex<Vec<HttpRequest>>,
    /// What the next call gets. The last one is repeated once the script runs
    /// out, so a test that does not care how many calls there are does not
    /// have to count them.
    script: Mutex<Vec<(u16, String)>>,
}

impl Fake {
    fn new() -> Self {
        Self {
            messages: vec![
                inbox(10, "Lunch?"),
                inbox(11, "FINAL NOTICE: claim your refund"),
                junk(12, "Already filed"),
            ],
            store: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(vec![(200, answer(0.4, "Personal", 0.01))]),
        }
    }

    fn answering(self, replies: Vec<(u16, String)>) -> Self {
        *self.script.lock().unwrap() = replies;
        self
    }

    /// What this store holds, for the tests that care.
    ///
    /// `header` answers from it, so a test about two copies of one email has
    /// to put both copies here — exactly as a real store would have them.
    fn holding(mut self, messages: Vec<Envelope>) -> Self {
        self.messages = messages;
        self
    }

    fn calls(&self) -> Vec<HttpRequest> {
        self.calls.lock().unwrap().clone()
    }
}

/// One Jev response, in the shape the API documents.
fn answer(spam: f64, category: &str, impersonation: f64) -> String {
    format!(
        r#"{{
          "model": "jev-latest",
          "answers": {{
            "spam": {{
              "type": "score",
              "score": {spam},
              "legend": {{"0": "wanted", "4": "malicious"}},
              "probabilities": {{"0": 0.0, "4": 1.0}},
              "confidence": 0.93
            }},
            "category": {{
              "type": "choice",
              "choice": "{category}",
              "probabilities": {{"{category}": 0.88}},
              "confidence": 0.88
            }},
            "impersonation": {{"type": "noul", "noul": {impersonation}}}
          }},
          "usage": {{"input_tokens": 640, "output_tokens": 0}}
        }}"#
    )
}

fn envelope(id: i64, mailbox: i64, subject: &str) -> Envelope {
    Envelope {
        id: MessageId(id),
        account_id: AccountId(1),
        mailbox_id: MailboxId(mailbox),
        thread_id: ThreadId(id),
        from: Address::new("Ana Ruiz", "ana@example.com"),
        subject: subject.into(),
        date: 1_700_000_000 + id,
        snippet: "the first couple of hundred characters".into(),
        flags: Flags::default(),
        has_attachments: false,
        size: 4_000,
        group: None,
    }
}

fn inbox(id: i64, subject: &str) -> Envelope {
    envelope(id, INBOX, subject)
}

fn junk(id: i64, subject: &str) -> Envelope {
    envelope(id, JUNK, subject)
}

/// The same email as it also exists in "All Mail": a different row, a
/// different id, one `Message-ID`.
fn archived(id: i64, subject: &str) -> Envelope {
    envelope(id, ARCHIVE, subject)
}

fn mailbox(id: i64, name: &str, role: MailboxRole) -> Mailbox {
    Mailbox {
        id: MailboxId(id),
        account_id: AccountId(1),
        path: name.into(),
        name: name.into(),
        role,
        total: 0,
        unread: 0,
        server_total: 0,
    }
}

impl Services for Fake {
    fn now(&self) -> i64 {
        1_700_000_000
    }

    fn envelopes(&self, query: MailQuery) -> Vec<Envelope> {
        self.messages.iter().take(query.limit).cloned().collect()
    }

    fn body(&self, id: MessageId) -> Option<Body> {
        Some(Body {
            plain: format!("the body of message {}", id.0),
            to: vec![Address::new("Me", "me@example.com")],
            ..Default::default()
        })
    }

    /// A `Message-ID` derived from the subject, so two copies of one email in
    /// two folders share one — which is what happens in a real store and what
    /// the verdict cache is keyed on.
    fn header(&self, id: MessageId, name: &str) -> Option<String> {
        if !name.eq_ignore_ascii_case("message-id") {
            return None;
        }
        let subject = self.messages.iter().find(|m| m.id == id).map(|m| m.subject.clone())?;
        Some(format!("<{}@example.com>", subject.replace(' ', "-").to_lowercase()))
    }

    fn mailboxes(&self) -> Vec<Mailbox> {
        vec![
            mailbox(INBOX, "INBOX", MailboxRole::Inbox),
            mailbox(JUNK, "Junk", MailboxRole::Junk),
            mailbox(ARCHIVE, "All Mail", MailboxRole::Archive),
        ]
    }

    fn accounts(&self) -> Vec<Account> {
        vec![Account {
            id: AccountId(1),
            name: "Test".into(),
            email: "me@example.com".into(),
            color: 0,
        }]
    }

    fn load(&self, extension: &str, key: &str) -> Option<String> {
        self.store.lock().unwrap().get(&format!("{extension}\u{0}{key}")).cloned()
    }

    fn save(&self, extension: &str, key: &str, value: &str) {
        self.store
            .lock()
            .unwrap()
            .insert(format!("{extension}\u{0}{key}"), value.to_string());
    }

    fn forget(&self, extension: &str, key: &str) {
        self.store.lock().unwrap().remove(&format!("{extension}\u{0}{key}"));
    }

    fn fetch(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        self.calls.lock().unwrap().push(request);

        let mut script = self.script.lock().unwrap();
        let (status, body) = match script.len() {
            0 | 1 => script.first().cloned().unwrap_or((200, String::new())),
            _ => script.remove(0),
        };

        Ok(HttpResponse { status, headers: Vec::new(), body: body.into_bytes() })
    }
}

/// The extension, installed, loaded and started.
fn start(root: &Path, fake: Arc<Fake>) -> (Engine, Instance) {
    let registry = Registry::load(root);
    assert!(registry.problems().is_empty(), "{:?}", registry.problems());

    let installed = registry.get("spam-scorer").expect("it is installed");
    let engine = Engine::new().expect("the engine starts");
    let mut instance = engine.load(installed, fake).expect("the component loads");
    instance.init().expect("it starts");
    (engine, instance)
}

// --- the tests ----------------------------------------------------------

#[test]
fn what_goes_to_jev_is_three_typed_questions_about_one_message() {
    let Some(_) = built() else { return };
    let root = install("asks", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(3.4, "Promotion", 0.02))]));
    let (_engine, mut instance) = start(&root, fake.clone());

    let outcome = instance
        .on_messages(&[inbox(10, "Lunch?"), junk(12, "Already filed")])
        .expect("the rule runs");
    assert_eq!(outcome.refused, 0, "it asks for nothing it did not declare");

    // One message, one call: the junk one is not worth paying for.
    let calls = fake.calls();
    assert_eq!(calls.len(), 1, "mail already in junk is not sent anywhere");

    let request = &calls[0];
    assert_eq!(request.url, "https://api.typesafe.ai/v1/systemone");
    assert_eq!(request.method, "POST");
    assert!(
        request.headers.iter().any(|(k, v)| k == "authorization" && v == "Bearer sk-test"),
        "the key from config.toml is what authenticates: {:?}",
        request.headers,
    );

    let sent: serde_json::Value =
        serde_json::from_slice(request.body.as_ref().unwrap()).expect("it sends json");

    assert_eq!(sent["model"], "jev-latest");
    assert_eq!(sent["state"]["subject"], "Lunch?");
    assert_eq!(sent["state"]["from_address"], "ana@example.com");
    assert_eq!(
        sent["state"]["body"], "the body of message 10",
        "the excerpt goes by default, and the setting that withholds it is tested below",
    );

    // The three questions, each of the type that makes its answer usable.
    assert_eq!(sent["questions"]["spam"]["type"], "score");
    assert_eq!(
        sent["questions"]["spam"]["criteria"].as_array().unwrap().len(),
        5,
        "five levels, 0 to 4 — the scale every threshold in this extension is read against",
    );
    assert_eq!(sent["questions"]["category"]["type"], "choice");
    assert_eq!(sent["questions"]["impersonation"]["type"], "noul");

    // The default labels, by the names they are drawn with, and `Other` for
    // mail that is none of them.
    let categories = sent["questions"]["category"]["criteria"].as_object().unwrap();
    for expected in ["Personal", "Receipt", "Newsletter", "Promotion", "Cold outreach", "Fraud", "Other"] {
        assert!(categories.contains_key(expected), "no `{expected}` category");
    }
    assert!(categories["Promotion"].as_str().unwrap().contains("Marketing"), "definitions go too");
}

/// A message nobody has scored yet is blank, and that is the whole point of
/// the mark above.
///
/// These two tests are a pair. If either drifts, the list stops being able to
/// say the difference between "looked at, fine" and "not got to yet" — which
/// is the difference that makes a quiet classifier legible.
#[test]
fn a_message_that_was_never_scored_carries_nothing_at_all() {
    let Some(_) = built() else { return };
    let root = install("unscored", Some(CONFIGURED));
    let fake = Arc::new(Fake::new());
    let (_engine, mut instance) = start(&root, fake.clone());

    // Decorated without ever being scored: no request, no verdict, no mark.
    let decorated = instance.decorate(&inbox(99, "Never looked at")).expect("decorating works");

    assert!(decorated.is_empty(), "nothing has an opinion about this message yet");
    assert!(fake.calls().is_empty(), "and decorating must never go and get one");
}

/// One email in two folders is one reading, not two.
///
/// A message copied into an archive or an "All Mail" folder is a second row
/// with a second id and the same `Message-ID`. Keying the cache on the row
/// pays twice to learn the same thing — and then draws the two copies
/// differently when the second reading lands somewhere else on the scale.
#[test]
fn the_same_email_in_two_folders_is_scored_once() {
    let Some(_) = built() else { return };
    let root = install("duplicates", Some(CONFIGURED));
    let fake = Arc::new(
        Fake::new()
            .holding(vec![inbox(40, "Fall sale"), archived(41, "Fall sale")])
            .answering(vec![(200, answer(2.4, "Promotion", 0.02))]),
    );
    let (_engine, mut instance) = start(&root, fake.clone());

    // Both copies arrive in the same batch, which is the case a caller-side
    // filter cannot catch: neither is scored when the batch starts.
    instance
        .on_messages(&[inbox(40, "Fall sale"), archived(41, "Fall sale")])
        .expect("the rule runs");

    assert_eq!(fake.calls().len(), 1, "one email, one request");

    // And both rows draw the same, because they resolve to the same verdict.
    let from_inbox = instance.decorate(&inbox(40, "Fall sale")).unwrap();
    let from_archive = instance.decorate(&archived(41, "Fall sale")).unwrap();

    assert_eq!(from_inbox.badges[0].label, "Promotion");
    assert_eq!(
        from_inbox.badges[0].label, from_archive.badges[0].label,
        "the same email must not be Promotion in one folder and a bare checkmark in another",
    );
}

/// Scoring a selection that has already been scored costs nothing.
///
/// The everyday mistake: select a screenful, press the button, press it again
/// because nothing seemed to happen. The second press must not be a second
/// bill.
#[test]
fn scoring_something_twice_does_not_pay_twice() {
    let Some(_) = built() else { return };
    let root = install("rescore", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(2.4, "Promotion", 0.02))]));
    let (_engine, mut instance) = start(&root, fake.clone());

    let context = CommandContext {
        selection: vec![MessageId(10), MessageId(11)],
        mailbox: Some(MailboxId(INBOX)),
        account: Some(AccountId(1)),
        search: String::new(),
    };

    let first = instance.run_command("score", &context).expect("the command runs");
    assert_eq!(fake.calls().len(), 2, "two messages, two requests");
    match &first.effects[..] {
        [Effect::Status(line)] => assert!(line.contains("Scored 2"), "{line}"),
        other => panic!("expected one status line, got {other:?}"),
    }

    let again = instance.run_command("score", &context).expect("the command runs");
    assert_eq!(fake.calls().len(), 2, "and the second press is free");
    match &again.effects[..] {
        [Effect::Status(line)] => {
            assert!(line.contains("already scored"), "it says why nothing happened: {line}")
        }
        other => panic!("expected one status line, got {other:?}"),
    }
}

#[test]
fn a_verdict_is_cached_so_the_same_message_is_never_paid_for_twice() {
    let Some(_) = built() else { return };
    let root = install("cached", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(3.4, "Promotion", 0.02))]));
    let (_engine, mut instance) = start(&root, fake.clone());

    instance.on_messages(&[inbox(10, "Lunch?")]).unwrap();
    assert_eq!(fake.calls().len(), 1);

    // The same message again, through both surfaces that could ask.
    instance.on_messages(&[inbox(10, "Lunch?")]).unwrap();
    instance.decorate(&inbox(10, "Lunch?")).unwrap();
    assert_eq!(fake.calls().len(), 1, "a message is scored once and looked up after");
}

#[test]
fn what_jev_said_becomes_a_badge_and_a_banner_offering_the_one_command() {
    let Some(_) = built() else { return };
    let root = install("badges", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(3.4, "Promotion", 0.02))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(11, "FINAL NOTICE")]).unwrap();

    let decorated = instance.decorate(&inbox(11, "FINAL NOTICE")).expect("decorating works");
    assert_eq!(decorated.badges.len(), 1);
    assert_eq!(decorated.badges[0].label, "Spam 3.4", "the score is shown, not a verdict word");
    assert_eq!(decorated.badges[0].emphasis, Emphasis::Danger);

    // The spam banner, then the line saying what Jev read it as.
    assert_eq!(decorated.banners.len(), 2);
    assert!(decorated.banners[0].text.contains("3.4"), "{}", decorated.banners[0].text);
    assert_eq!(labels_of(&decorated.banners[1].badges), ["Promotion"], "what Jev read it as");
    assert!(decorated.banners[1].text.is_empty(), "drawn beside the subject, not as a strip");

    // The banner's button runs a command by name — the only thing a button can
    // do — and it is the command that files, which is the only thing that
    // moves mail.
    let action = decorated.banners[0].action.as_ref().expect("the banner offers a way out");
    assert_eq!(action.run, "file");
}

/// The band a real inbox actually lives in.
///
/// Written after the first run against one. Most unwanted mail does not come
/// back as a confident 3.5 — it comes back as "2.35, and the model is split
/// between levels 2 and 3", which on an ordered scale is a precise answer
/// rather than a hedge. A confidence floor set by analogy with a yes/no
/// question threw all of it away and marked two messages in twenty-eight,
/// which reads from the outside as an extension that does not work.
#[test]
fn a_reading_between_two_levels_is_an_answer_and_gets_a_mark() {
    let Some(_) = built() else { return };
    let root = install("midband", Some(CONFIGURED));

    // Exactly the shape a real promotional message came back as.
    let fake = Arc::new(Fake::new().answering(vec![(200, split(2.35, 0.45, "Promotion", 1.0))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(10, "50% off, today only")]).unwrap();
    let decorated = instance.decorate(&inbox(10, "50% off, today only")).unwrap();

    assert_eq!(decorated.badges.len(), 1, "it knows what this is");
    assert_eq!(decorated.badges[0].label, "Promotion");
    assert_eq!(decorated.badges[0].emphasis, Emphasis::Muted, "quietly, though");

    // What it is, beside the subject, with the way to file it.
    assert_eq!(decorated.banners.len(), 1);
    assert!(decorated.banners[0].text.is_empty(), "{}", decorated.banners[0].text);
    assert_eq!(labels_of(&decorated.banners[0].badges), ["Promotion"]);
    assert_eq!(decorated.banners[0].action.as_ref().map(|a| a.run.as_str()), Some("file"));
}

/// A score the model is split on is still marked, when it knows what the
/// message is.
///
/// The two questions have separate confidences and on real mail they diverge
/// hard: `category promotional, confidence 0.99` alongside `score 2.06,
/// confidence 0.21`. Gating the label on the score's confidence hid exactly
/// the mail the band exists for.
#[test]
fn a_confident_classification_is_marked_however_split_the_score_is() {
    let Some(_) = built() else { return };
    let root = install("split", Some(CONFIGURED));

    // The shape DoorDash and HotelTonight actually came back as.
    let fake = Arc::new(Fake::new().answering(vec![(200, split(2.06, 0.21, "Promotion", 0.99))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(10, "Your usual can wait")]).unwrap();
    let decorated = instance.decorate(&inbox(10, "Your usual can wait")).unwrap();

    assert_eq!(decorated.badges.len(), 1, "it knows exactly what this is");
    assert_eq!(decorated.badges[0].label, "Promotion");
}

/// The only real shrug: it cannot place the message *or* classify it.
#[test]
fn a_message_the_model_cannot_place_or_classify_is_not_drawn() {
    let Some(_) = built() else { return };
    let root = install("scattered", Some(CONFIGURED));

    // Mid-scale and no idea what it is — both questions came back spread.
    let fake = Arc::new(Fake::new().answering(vec![(200, split(2.35, 0.15, "Promotion", 0.28))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(10, "Could be anything")]).unwrap();
    let decorated = instance.decorate(&inbox(10, "Could be anything")).unwrap();

    // Not nothing at all: it was looked at, and saying so is the difference
    // between a shrug and a message nobody has got to yet.
    assert_eq!(decorated.badges.len(), 1);
    assert_eq!(decorated.badges[0].label, SCANNED, "a shrug is a mark, not a label");
    // What Jev said is still shown, with nothing offered to do about it.
    assert_eq!(decorated.banners.len(), 1);
    assert!(decorated.banners[0].action.is_none());
}

/// A label that is switched off is marked as passed, not badged. Receipts
/// start switched off: they are most of an inbox, and all of them wanted.
#[test]
fn a_label_that_does_not_show_is_marked_as_passed() {
    let Some(_) = built() else { return };
    let root = install("receipt", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, split(0.3, 0.8, "Receipt", 0.95))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(10, "Your order has shipped")]).unwrap();
    let decorated = instance.decorate(&inbox(10, "Your order has shipped")).unwrap();

    assert_eq!(decorated.badges.len(), 1);
    assert_eq!(decorated.badges[0].label, SCANNED, "looked at, and left alone");
    assert_eq!(labels_of(&decorated.banners[0].badges), ["Receipt"], "named, though not badged on the row");
}

/// Labels someone wrote are what Jev is asked, and what the row is badged.
#[test]
fn labels_from_the_settings_are_asked_and_drawn() {
    let Some(_) = built() else { return };
    let config = format!(
        "{CONFIGURED}\n[[labels]]\nname = \"Invoice\"\nshow = \"true\"\n\
         definition = \"A bill asking me to pay.\"\n\n[[labels]]\nname = \"Chatter\"\nshow = \"false\"\n"
    );
    let root = install("custom", Some(&config));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(1.1, "Invoice", 0.01))]));
    let (_engine, mut instance) = start(&root, fake.clone());

    instance.on_messages(&[inbox(10, "Invoice #42")]).unwrap();

    let sent: serde_json::Value = serde_json::from_slice(fake.calls()[0].body.as_ref().unwrap()).unwrap();
    let categories = sent["questions"]["category"]["criteria"].as_object().unwrap();
    assert_eq!(categories.keys().collect::<Vec<_>>(), ["Chatter", "Invoice", "Other"]);
    assert_eq!(categories["Invoice"], "A bill asking me to pay.");
    assert!(categories["Chatter"].is_null(), "a label with no definition is its own name");

    let decorated = instance.decorate(&inbox(10, "Invoice #42")).unwrap();
    assert_eq!(decorated.badges[0].label, "Invoice");
}

/// A verdict reached under other labels answered a different question, so
/// the message is scored again rather than drawn by an old answer.
#[test]
fn changing_a_label_scores_the_mail_again() {
    let Some(_) = built() else { return };
    let root = install("relabelled", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(2.0, "Promotion", 0.01))]));
    let (engine, mut instance) = start(&root, fake.clone());
    instance.on_messages(&[inbox(10, "Sale")]).unwrap();
    instance.on_messages(&[inbox(10, "Sale")]).unwrap();
    assert_eq!(fake.calls().len(), 1, "the same labels: scored once");
    drop((engine, instance));

    // The same store, and different labels.
    let edited = format!("{CONFIGURED}\n[[labels]]\nname = \"Deals\"\n");
    std::fs::write(root.join("spam-scorer").join("config.toml"), edited).unwrap();
    let (_engine, mut instance) = start(&root, fake.clone());
    instance.on_messages(&[inbox(10, "Sale")]).unwrap();
    assert_eq!(fake.calls().len(), 2, "new labels: scored again");
}

/// A Jev answer with both confidences set explicitly.
fn split(spam: f64, score_confidence: f64, category: &str, category_confidence: f64) -> String {
    answer(spam, category, 0.02)
        .replace(r#""confidence": 0.93"#, &format!(r#""confidence": {score_confidence}"#))
        .replace(r#""confidence": 0.88"#, &format!(r#""confidence": {category_confidence}"#))
}

#[test]
fn an_unremarkable_message_is_left_alone() {
    let Some(_) = built() else { return };
    let root = install("quiet", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(0.4, "Personal", 0.01))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(10, "Lunch?")]).unwrap();

    let decorated = instance.decorate(&inbox(10, "Lunch?")).unwrap();

    assert_eq!(decorated.badges.len(), 1, "a classifier that marks everything has said nothing");
    assert_eq!(decorated.badges[0].label, SCANNED);
    assert_eq!(decorated.badges[0].emphasis, Emphasis::Muted, "texture, not information");
}

/// The mark the scorer puts on a message it looked at and had no opinion on.
///
/// One character, so the host draws it bare. Named here so the tests and the
/// extension cannot drift on what it is.
const SCANNED: &str = "\u{2713}";

#[test]
fn a_forgery_is_named_as_one_rather_than_scored_as_bulk() {
    let Some(_) = built() else { return };
    let root = install("forged", Some(CONFIGURED));
    // Low on the bulk scale, high on impersonation — which is exactly the mail
    // a single number would miss: a targeted forgery is not bulk.
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(1.2, "Fraud", 0.95))]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(11, "Your account is on hold")]).unwrap();

    let decorated = instance.decorate(&inbox(11, "Your account is on hold")).unwrap();
    assert_eq!(decorated.badges[0].label, "Forged");
    assert_eq!(decorated.badges[0].emphasis, Emphasis::Danger);
    assert!(
        decorated.banners[0].text.contains("95%"),
        "it says how sure it is: {}",
        decorated.banners[0].text,
    );
}

#[test]
fn nothing_moves_mail_except_the_command_named_for_it() {
    let Some(_) = built() else { return };
    let root = install("filing", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(3.9, "Fraud", 0.99))]));
    let (_engine, mut instance) = start(&root, fake);

    // The worst reading there is, arriving on its own.
    let outcome = instance.on_messages(&[inbox(11, "FINAL NOTICE")]).unwrap();
    assert!(
        !outcome.effects.iter().any(|e| matches!(e, Effect::MoveTo { .. } | Effect::Trash(_))),
        "scoring never files: {:?}",
        outcome.effects,
    );

    // Asked by name, it files — into the junk mailbox of the account in front
    // of the user.
    let context = CommandContext {
        selection: vec![MessageId(11)],
        mailbox: Some(MailboxId(INBOX)),
        account: Some(AccountId(1)),
        search: String::new(),
    };
    let outcome = instance.run_command("file", &context).expect("the command runs");
    assert_eq!(outcome.refused, 0);

    match &outcome.effects[0] {
        Effect::MoveTo { message, mailbox } => {
            assert_eq!(*message, MessageId(11));
            assert_eq!(*mailbox, MailboxId(JUNK));
        }
        other => panic!("expected a move, got {other:?}"),
    }
}

#[test]
fn a_refused_key_is_reported_rather_than_retried() {
    let Some(_) = built() else { return };
    let root = install("refused", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(401, r#"{"error":"bad key"}"#.into())]));
    let (_engine, mut instance) = start(&root, fake.clone());

    let outcome = instance.on_messages(&[inbox(10, "Lunch?"), inbox(11, "FINAL NOTICE")]).unwrap();

    // One call per message and not one more: there is no way to sleep inside
    // the sandbox, so a retry loop would be a spin against the deadline.
    assert_eq!(fake.calls().len(), 2);

    match &outcome.effects[..] {
        [Effect::Status(line)] => assert!(line.contains("failed"), "{line}"),
        other => panic!("expected one status line, got {other:?}"),
    }

    // And nothing was cached, so a sweep after the key is fixed will try again.
    let decorated = instance.decorate(&inbox(11, "FINAL NOTICE")).unwrap();
    assert!(decorated.is_empty());
}

#[test]
fn the_body_stays_here_when_config_says_it_should() {
    let Some(_) = built() else { return };
    let root = install("headers-only", Some("api-key = \"sk-test\"\nbody = \"none\""));
    let fake = Arc::new(Fake::new());
    let (_engine, mut instance) = start(&root, fake.clone());

    instance.on_messages(&[inbox(10, "Lunch?")]).unwrap();

    let calls = fake.calls();
    let sent: serde_json::Value =
        serde_json::from_slice(calls[0].body.as_ref().unwrap()).unwrap();

    assert_eq!(sent["state"]["body"], "", "the body does not leave the machine");
    assert_eq!(sent["state"]["subject"], "Lunch?", "the headers still do, or it classifies nothing");
}

#[test]
fn an_extension_that_was_never_given_a_key_does_not_start() {
    // The whole reason settings are declared: an extension with no way to ask
    // for an API key should fail with a sentence naming the key and the file,
    // before any of its code runs — not on its first request, three screens
    // later, as "unauthorized".
    let Some(_) = built() else { return };
    let root = install("keyless", None);

    let registry = Registry::load(&root);
    assert!(registry.problems().is_empty(), "a missing value is not a broken extension");

    let installed = registry.get("spam-scorer").expect("it is installed");
    assert_eq!(installed.unconfigured(), ["api-key"]);

    let engine = Engine::new().unwrap();
    let why = match engine.load(installed, Arc::new(Fake::new())) {
        Ok(_) => panic!("it must not start without the key it declared as required"),
        Err(refused) => format!("{refused:#}"),
    };

    assert!(why.contains("api-key"), "the error names the setting: {why}");
    assert!(why.contains(CONFIG_FILE), "and the file to put it in: {why}");
}

#[test]
fn settings_are_seeded_where_the_extension_can_read_them_and_not_write_them() {
    let Some(_) = built() else { return };
    let root = install("seeded", Some(CONFIGURED));
    let fake = Arc::new(Fake::new());
    let (_engine, _instance) = start(&root, fake.clone());

    // Namespaced by the extension's id, under keys the host owns.
    assert_eq!(fake.load("spam-scorer", "config.api-key").as_deref(), Some("sk-test"));
    // Defaults from the manifest fill in beside the values from the file.
    assert_eq!(fake.load("spam-scorer", "config.model").as_deref(), Some("jev-latest"));
    assert_eq!(fake.load("spam-scorer", "config.body").as_deref(), Some("excerpt"));
    // And no other extension's namespace was touched.
    assert_eq!(fake.load("inbox-stats", "config.api-key"), None);
}

#[test]
fn a_sweep_scores_what_the_user_is_looking_at_and_says_what_is_left() {
    let Some(_) = built() else { return };
    let root = install("sweep", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(200, answer(3.6, "Promotion", 0.02))]));
    let (_engine, mut instance) = start(&root, fake.clone());

    // The fake mailbox holds three: two in the inbox and one already in junk.
    let context = CommandContext {
        mailbox: Some(MailboxId(INBOX)),
        account: Some(AccountId(1)),
        ..Default::default()
    };
    let outcome = instance.run_command("sweep", &context).expect("the command runs");

    assert_eq!(fake.calls().len(), 2, "the one in junk is not swept");
    match &outcome.effects[..] {
        [Effect::Status(line)] => assert!(line.contains("2"), "{line}"),
        other => panic!("expected one status line, got {other:?}"),
    }

    // Run again and there is nothing left to do — and nothing left to pay for.
    let outcome = instance.run_command("sweep", &context).unwrap();
    assert_eq!(fake.calls().len(), 2);
    match &outcome.effects[..] {
        [Effect::Status(line)] => assert!(line.contains("scored"), "{line}"),
        other => panic!("expected one status line, got {other:?}"),
    }
}

#[test]
fn a_run_of_failures_stops_asking() {
    let Some(_) = built() else { return };
    let root = install("failing", Some(CONFIGURED));
    let fake = Arc::new(Fake::new().answering(vec![(401, r#"{"error":"bad key"}"#.into())]));
    let (_engine, mut instance) = start(&root, fake.clone());

    let batch: Vec<Envelope> = (100..110).map(|id| inbox(id, "Hello")).collect();
    let outcome = instance.on_messages(&batch).unwrap();

    // A bad key fails every message the same way; three is enough to say so.
    assert_eq!(fake.calls().len(), 3, "it gave up after three in a row");
    match &outcome.effects[..] {
        [Effect::Status(line)] => assert!(line.contains("failed on 3"), "{line}"),
        other => panic!("expected one status line, got {other:?}"),
    }
}

fn labels_of(badges: &[spool_ext::api::Badge]) -> Vec<&str> {
    badges.iter().map(|b| b.label.as_str()).collect()
}

/// Only what Jev thinks is likely is named on the open message — both, when
/// it is split between two — and never `Other`.
#[test]
fn the_open_message_names_the_likely_kinds_only() {
    let Some(_) = built() else { return };
    let root = install("kinds", Some(CONFIGURED));
    let split_two = answer(1.5, "Promotion", 0.01).replace(
        r#""probabilities": {"Promotion": 0.88}"#,
        r#""probabilities": {"Promotion": 0.52, "Newsletter": 0.43, "Other": 0.05}"#,
    );
    let other = answer(0.5, "Other", 0.01);
    let fake = Arc::new(Fake::new().answering(vec![(200, split_two), (200, other)]));
    let (_engine, mut instance) = start(&root, fake);

    instance.on_messages(&[inbox(10, "Spring collection"), inbox(11, "Hello")]).unwrap();

    let two = instance.decorate(&inbox(10, "Spring collection")).unwrap();
    let details = two.banners.last().unwrap();
    assert_eq!(labels_of(&details.badges), ["Promotion", "Newsletter"]);
    assert!(details.text.is_empty(), "drawn beside the subject: {}", details.text);

    let none = instance.decorate(&inbox(11, "Hello")).unwrap();
    assert!(none.banners.is_empty(), "`Other` is not a kind of message");
}
