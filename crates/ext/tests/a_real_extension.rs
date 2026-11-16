//! Loads the example extension — a real WebAssembly component, built by
//! `scripts/build-extension.sh` — and drives every surface it has.
//!
//! This is the test that would notice the WIT and the host drifting apart.
//! Everything else in this crate checks a piece in isolation; this one checks
//! that a component compiled from `extensions/inbox-stats` against the
//! published contract still links, starts, and answers.
//!
//! It builds the component itself if it is missing, and skips loudly if the
//! machine has no toolchain that can — a silent skip on the one test that
//! exercises the whole boundary would be worse than no test at all.

use std::{path::PathBuf, process::Command, sync::Arc};

use spool_core::{Account, AccountId, Address, Body, Envelope, Flags, MailboxId, MessageId, ThreadId};
use spool_ext::{
    api::{Effect, Emphasis, NodeKind},
    manifest::{MANIFEST_FILE, WASM_FILE},
    registry::Registry,
    runtime::{Budgets, CommandContext, Engine, Instance},
    services::{MailQuery, Services},
};

const BIG: u32 = 8 * 1024 * 1024;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The built extensions directory, building it first if it is not there.
///
/// Returns `None` when this machine cannot build for wasm, which is the only
/// excuse for not running the rest.
fn built() -> Option<PathBuf> {
    let root = repo_root();
    let out = root.join("target/extensions/inbox-stats");

    if !out.join(WASM_FILE).exists() {
        let script = root.join("scripts/build-extension.sh");
        let status = Command::new("bash").arg(&script).arg("inbox-stats").status();

        match status {
            Ok(status) if status.success() => {}
            _ => {
                eprintln!(
                    "SKIPPING: could not build the example extension. \
                     Install the target with `rustup target add wasm32-wasip2` \
                     and run scripts/build-extension.sh."
                );
                return None;
            }
        }
    }

    Some(root.join("target/extensions"))
}

/// A mailbox held in a `Vec`, so the boundary can be tested without a database.
struct Fake {
    messages: Vec<Envelope>,
}

impl Fake {
    fn new() -> Self {
        Self { messages: vec![small(1, "Lunch?"), big(2, "Quarterly deck")] }
    }
}

fn envelope(id: i64, subject: &str, size: u32, seen: bool) -> Envelope {
    Envelope {
        id: MessageId(id),
        account_id: AccountId(1),
        mailbox_id: MailboxId(1),
        thread_id: ThreadId(id),
        from: Address::new("Ana Ruiz", "ana@example.com"),
        subject: subject.into(),
        date: 1_700_000_000 + id,
        snippet: "...".into(),
        flags: Flags::default().with(Flags::SEEN, seen),
        has_attachments: size > 1024,
        size,
        group: None,
    }
}

fn small(id: i64, subject: &str) -> Envelope {
    envelope(id, subject, 4_000, false)
}

fn big(id: i64, subject: &str) -> Envelope {
    envelope(id, subject, BIG, false)
}

impl Services for Fake {
    fn now(&self) -> i64 {
        1_700_000_000
    }

    fn envelopes(&self, query: MailQuery) -> Vec<Envelope> {
        self.messages
            .iter()
            .filter(|m| match &query.filter {
                spool_store::Query::Unread => !m.flags.is_seen(),
                spool_store::Query::Flagged => m.flags.is_flagged(),
                _ => true,
            })
            .take(query.limit)
            .cloned()
            .collect()
    }

    fn body(&self, _: MessageId) -> Option<Body> {
        None
    }

    fn header(&self, _: MessageId, _: &str) -> Option<String> {
        None
    }

    fn mailboxes(&self) -> Vec<spool_core::Mailbox> {
        Vec::new()
    }

    fn accounts(&self) -> Vec<Account> {
        vec![Account {
            id: AccountId(1),
            name: "Test".into(),
            email: "me@example.com".into(),
            color: 0,
        }]
    }

    fn load(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    fn save(&self, _: &str, _: &str, _: &str) {}

    fn forget(&self, _: &str, _: &str) {}
}

/// The example extension, loaded and started.
fn start(root: &PathBuf) -> (Engine, Instance) {
    let registry = Registry::load(root);
    assert!(
        registry.problems().is_empty(),
        "the built extension should load cleanly: {:?}",
        registry.problems()
    );

    let installed = registry.get("inbox-stats").expect("inbox-stats is installed");
    let engine = Engine::new().expect("the engine starts");
    let mut instance = engine
        .load(installed, Arc::new(Fake::new()))
        .expect("the component loads");

    instance.init().expect("the extension starts");
    (engine, instance)
}

#[test]
fn a_real_component_loads_and_answers_on_every_surface() {
    let Some(root) = built() else { return };
    let (_engine, mut instance) = start(&root);

    // --- new mail -------------------------------------------------------
    // One notification for the batch, about the large message and not the
    // small one.
    let outcome = instance
        .on_messages(&[small(10, "Lunch?"), big(11, "Quarterly deck")])
        .expect("the rule runs");

    assert_eq!(outcome.refused, 0, "it asks for nothing it did not declare");
    assert_eq!(outcome.effects.len(), 1, "one notification for the batch, not one per message");

    match &outcome.effects[0] {
        Effect::Notify { title, body, message } => {
            assert!(title.contains("8.0 MB"), "{title}");
            assert!(title.contains("Ana Ruiz"), "the sender is named: {title}");
            assert_eq!(body, "Quarterly deck");
            assert_eq!(*message, Some(MessageId(11)), "clicking it opens the message");
        }
        other => panic!("expected a notification, got {other:?}"),
    }

    // Nothing at all when nothing is worth mentioning.
    let quiet = instance.on_messages(&[small(12, "Lunch?")]).expect("the rule runs");
    assert!(quiet.effects.is_empty());

    // --- decoration -----------------------------------------------------
    let decorated = instance.decorate(&big(11, "Quarterly deck")).expect("decorating works");
    assert_eq!(decorated.badges.len(), 1);
    assert_eq!(decorated.badges[0].label, "8.0 MB");
    assert_eq!(decorated.badges[0].emphasis, Emphasis::Muted);

    let plain = instance.decorate(&small(10, "Lunch?")).expect("decorating works");
    assert!(plain.is_empty(), "an ordinary message gets nothing");

    // --- a command ------------------------------------------------------
    // Both fake messages are unread, and the extension reaches back through
    // the host to count them — so this also proves the import direction works.
    let outcome = instance
        .run_command("summarise", &CommandContext::default())
        .expect("the command runs");

    match &outcome.effects[..] {
        [Effect::Status(line)] => assert_eq!(line, "2 unread, 0 flagged"),
        other => panic!("expected one status line, got {other:?}"),
    }

    // --- a panel --------------------------------------------------------
    let panel = instance.render_panel("stats").expect("the panel draws");
    let root_node = panel.root().expect("the panel has a root");
    assert_eq!(root_node.kind, NodeKind::Column);

    let text: Vec<String> = collect_text(&panel);
    assert!(text.contains(&"Unread".to_string()), "{text:?}");
    assert!(text.contains(&"me@example.com".to_string()), "the account is listed: {text:?}");
    assert!(text.contains(&"2".to_string()), "with its unread count: {text:?}");

    // The button the panel offers runs a command by name, which is the only
    // thing a button is allowed to do.
    assert!(
        has_button(&panel, "Summarise", "summarise"),
        "the panel offers its command as a button",
    );
}

#[test]
fn an_extension_is_refused_the_moment_its_manifest_understates_it() {
    // The same component, with a manifest that does not ask to read mail. It
    // imports `query-envelopes`, so it must not start — and must not start
    // *before* running, rather than failing on the call.
    let Some(root) = built() else { return };

    let dir = std::env::temp_dir().join(format!("spool-ext-understated-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ext = dir.join("inbox-stats");
    std::fs::create_dir_all(&ext).unwrap();

    std::fs::copy(root.join("inbox-stats").join(WASM_FILE), ext.join(WASM_FILE)).unwrap();
    std::fs::write(
        ext.join(MANIFEST_FILE),
        r#"
        [extension]
        id = "inbox-stats"
        api = "0.2"
        capabilities = ["ui", "notify"]
        hooks = ["decorate"]
        "#,
    )
    .unwrap();

    let registry = Registry::load(&dir);
    let installed = registry.get("inbox-stats").expect("it is installed");
    let engine = Engine::new().unwrap();

    let why = match engine.load(installed, Arc::new(Fake::new())) {
        Ok(_) => panic!("an understated manifest must not start"),
        Err(refused) => format!("{refused:#}"),
    };

    assert!(why.contains("read-mail"), "the error names the missing capability: {why}");
    assert!(why.contains("query-envelopes"), "and the call that needs it: {why}");

    let _ = std::fs::remove_dir_all(&dir);
}

fn collect_text(panel: &spool_ext::api::Panel) -> Vec<String> {
    let mut found = Vec::new();
    let Some(root) = panel.root() else { return found };
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        if let NodeKind::Text { content, .. } = &node.kind {
            found.push(content.clone());
        }
        stack.extend(panel.children(node));
    }
    found
}

fn has_button(panel: &spool_ext::api::Panel, label: &str, run: &str) -> bool {
    let Some(root) = panel.root() else { return false };
    let mut stack = vec![root];

    while let Some(node) = stack.pop() {
        if let NodeKind::Button(button) = &node.kind
            && button.label == label
            && button.run == run
        {
            return true;
        }
        stack.extend(panel.children(node));
    }
    false
}

/// Builds the fixture that will not stop, and returns where it landed.
fn runaway() -> Option<PathBuf> {
    let root = repo_root();
    let out = root.join("target/extensions/runaway");

    if !out.join(WASM_FILE).exists() {
        let script = root.join("scripts/build-extension.sh");
        let fixture = root.join("crates/ext/tests/fixtures/runaway");
        let status = Command::new("bash").arg(&script).arg(&fixture).status();

        if !matches!(status, Ok(status) if status.success()) {
            eprintln!("SKIPPING: could not build the runaway fixture.");
            return None;
        }
    }

    Some(root.join("target/extensions"))
}

#[test]
fn an_extension_that_will_not_stop_is_stopped() {
    // The claim this backs is the one the whole sandbox rests on: a call runs
    // under a deadline, and an extension in an infinite loop is interrupted
    // rather than waited for. Without it, one bad extension is a window that
    // never paints again.
    let Some(root) = runaway() else { return };

    let registry = Registry::load(&root);
    let installed = registry.get("runaway").expect("the fixture is installed");

    // A second is as good a proof as thirty, and thirty seconds in the suite
    // is thirty seconds nobody spends running it.
    let engine = Engine::with_budgets(Budgets {
        command: std::time::Duration::from_secs(1),
        ..Budgets::default()
    })
    .unwrap();
    let mut instance = engine.load(installed, Arc::new(Fake::new())).expect("it loads");
    instance.init().expect("it starts");

    let started = std::time::Instant::now();
    let result = instance.run_command("hang", &CommandContext::default());
    let waited = started.elapsed();

    let why = match result {
        Ok(outcome) => panic!("the call returned {outcome:?} instead of being stopped"),
        Err(error) => format!("{error:#}"),
    };

    assert!(
        why.contains("epoch") || why.to_lowercase().contains("interrupt"),
        "it was stopped by the deadline rather than by something else: {why}",
    );
    assert!(why.contains("runaway"), "the error names the extension to switch off: {why}");

    // Bounded by the budget it was given, with enough slack that a loaded
    // machine does not fail the suite over scheduling.
    assert!(waited < std::time::Duration::from_secs(20), "it took {waited:?}");
}
