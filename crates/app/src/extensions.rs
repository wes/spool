//! The extension host, as the window sees it.
//!
//! `spool-ext` knows how to load a component, call it under a deadline and hand
//! back plain data. It deliberately knows nothing about gpui, which is what
//! lets the whole extension system be tested without a window. This module is
//! the other half of that arrangement: it owns the engine, answers the calls
//! extensions make back into the app, and keeps the results somewhere a frame
//! can read them without waiting for anything.
//!
//! **Nothing here runs on the UI thread.** A frame is built in about 85µs and
//! holds that number by never letting arbitrary code run while one is being
//! built. Every call into a component happens on the background executor; what
//! comes back is plain data, and the window picks it up on its next frame. The
//! one thing the UI thread does touch is [`Host::decoration`], which is a map
//! lookup against a cache somebody else filled.
//!
//! The types cooperate with that. [`Instance`] is `Send` and deliberately not
//! `Sync` — a component instance is single-threaded — so each one lives behind
//! its own mutex and calls to one extension serialise while calls to different
//! extensions do not.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use anyhow::Result;
use spool_core::{Account, Body, Envelope, Mailbox, MailboxId, MessageId};
use spool_ext::{
    api,
    registry::{Installed, Registry},
    runtime::{CommandContext, Engine, Instance},
    services::{HttpRequest, HttpResponse, MailQuery, Services},
};
use spool_store::Store;

/// The most a response to an extension's `fetch` may be.
///
/// Generous for an API answer and far short of what would let a hostile server
/// decide this process's memory footprint. The component's own linear memory is
/// capped too, so a body that fits here can still fail to fit there — which is
/// the extension's problem, as it should be.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// Where extensions are installed.
pub fn directory(db_path: &Path) -> PathBuf {
    // Beside the mail store rather than in a directory of its own, so that the
    // answer to "where does Spool keep its things" stays one path.
    db_path
        .parent()
        .map(|dir| dir.join("extensions"))
        .unwrap_or_else(|| PathBuf::from("extensions"))
}

/// Everything the window needs to run extensions, all of it shareable.
///
/// Cloned into background tasks rather than borrowed, because the work happens
/// off the UI thread and the window cannot be held across an await.
#[derive(Clone)]
pub struct Host {
    /// What is installed, re-read whenever settings are written.
    ///
    /// Behind a lock because the settings pane changes it while background
    /// threads are reading it. Every method here takes a snapshot rather than
    /// holding the guard across a call into a component — a lock held while
    /// arbitrary code runs under a deadline is a lock that can be held for the
    /// length of that deadline.
    registry: Arc<RwLock<Registry>>,
    engine: Arc<Engine>,
    services: Arc<AppServices>,
    /// Components that have been compiled and started, by id.
    ///
    /// Loading one is a real compiler run, so it happens once and on demand:
    /// an extension of nothing but commands is not compiled until somebody
    /// runs one of them.
    live: Arc<Mutex<HashMap<String, Loaded>>>,
    /// What each message should be decorated with, filled in the background.
    ///
    /// An `RwLock` rather than a `Mutex` because the read side is a list being
    /// drawn and the write side is one background task: readers must never
    /// queue behind each other for this.
    decorations: Arc<RwLock<HashMap<MessageId, Marks>>>,
    /// The last tree each panel described, by qualified id.
    ///
    /// Panels are drawn every frame the sidebar is and asked for far less
    /// often — on start, after an extension does something, and when one says
    /// its panel is stale. The cache is what makes the difference between the
    /// two rates invisible.
    panels: Arc<RwLock<HashMap<String, api::Panel>>>,
}

/// What every extension wants shown on one message.
///
/// Not `api::Decorations`, because that is one extension's answer and this is
/// all of them merged. A banner carries the extension that raised it: its
/// button runs a command, commands are qualified by their extension, and a
/// merged list has no other way to say whose `file` it meant.
#[derive(Debug, Clone, Default)]
pub struct Marks {
    pub badges: Vec<api::Badge>,
    pub banners: Vec<OwnedBanner>,
    /// The filters this message is in, as `(extension, filter)`.
    ///
    /// Worked out when the badges arrive, from the extension's own badges
    /// against its own manifest, so one extension's filter can never catch a
    /// row because of another extension's badge that happens to share a word.
    pub filters: Vec<(String, String)>,
}

/// A banner and who raised it.
#[derive(Debug, Clone)]
pub struct OwnedBanner {
    pub extension: String,
    pub banner: api::Banner,
}

impl Marks {
    pub fn is_empty(&self) -> bool {
        self.badges.is_empty() && self.banners.is_empty()
    }
}

/// A component that was compiled, or the reason it was not.
///
/// A failure is remembered rather than retried. Compiling takes real time, and
/// an extension that failed to start will fail the same way every time a row is
/// drawn — the log line is worth having once, not once per frame.
#[derive(Clone)]
enum Loaded {
    Running(Arc<Mutex<Instance>>),
    Failed(String),
}

impl Host {
    /// Reads the extensions directory and starts the engine.
    ///
    /// A machine with no extensions gets a working host with nothing in it,
    /// which costs one `read_dir` of a directory that is not there. The engine
    /// failing is the only thing that returns `None`, and it means wasmtime
    /// could not be configured at all.
    pub fn open(root: PathBuf, store: Store) -> Option<Self> {
        let registry = Registry::load(root);

        for problem in registry.problems() {
            log::warn!("extension in {}: {}", problem.dir.display(), problem.why);
        }

        let engine = match Engine::new() {
            Ok(engine) => Arc::new(engine),
            Err(e) => {
                log::error!("extensions are off: the wasm engine would not start: {e:#}");
                return None;
            }
        };

        Some(Self {
            registry: Arc::new(RwLock::new(registry)),
            engine,
            services: Arc::new(AppServices { store }),
            live: Arc::new(Mutex::new(HashMap::new())),
            decorations: Arc::new(RwLock::new(HashMap::new())),
            panels: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Where extensions are installed.
    pub fn root(&self) -> PathBuf {
        self.registry.read().map(|r| r.root().to_path_buf()).unwrap_or_default()
    }

    /// Everything installed, working or not switched on.
    pub fn installed(&self) -> Vec<Installed> {
        self.registry.read().map(|r| r.all().to_vec()).unwrap_or_default()
    }

    /// One of them, by id.
    pub fn get(&self, id: &str) -> Option<Installed> {
        self.registry.read().ok()?.get(id).cloned()
    }

    /// Every command every enabled extension contributes.
    pub fn commands(&self) -> Vec<spool_ext::registry::Contributed> {
        self.registry.read().map(|r| r.commands()).unwrap_or_default()
    }

    /// Directories that could not be loaded, and why.
    ///
    /// Shown in the settings pane rather than only logged: an extension that
    /// is installed and invisible is worse than one that is installed and
    /// visibly broken.
    pub fn problems(&self) -> Vec<spool_ext::registry::Problem> {
        self.registry.read().map(|r| r.problems().to_vec()).unwrap_or_default()
    }

    /// Everything installed and switched on, as a snapshot.
    ///
    /// A snapshot rather than a borrow, so nothing holds the registry lock
    /// while a component is running.
    fn enabled(&self) -> Vec<Installed> {
        self.registry
            .read()
            .map(|r| r.enabled().cloned().collect())
            .unwrap_or_default()
    }

    /// Re-reads the extensions directory, and forgets what one extension had
    /// already become.
    ///
    /// Called after settings are written. Dropping the compiled instance is
    /// what makes a new API key take effect without a restart: settings are
    /// seeded when an extension is loaded, so the next thing that needs it
    /// gets one built against the file as it is now. A background thread
    /// already inside the old instance holds its own handle and finishes
    /// undisturbed.
    pub fn reload(&self, id: &str) {
        if let Ok(mut registry) = self.registry.write() {
            *registry = Registry::load(registry.root().to_path_buf());
        }
        if let Ok(mut live) = self.live.lock() {
            live.remove(id);
        }
        if let Ok(mut panels) = self.panels.write() {
            panels.retain(|qualified, _| {
                spool_ext::manifest::split_qualified(qualified).map(|(e, _)| e) != Some(id)
            });
        }
    }

    /// Where one extension has got to, for the settings pane.
    ///
    /// Never compiles anything — this is read from what has already happened,
    /// so the pane can be drawn on the UI thread like any other.
    pub fn status(&self, installed: &Installed) -> Status {
        if !installed.enabled {
            return Status::Off;
        }
        if !installed.unconfigured().is_empty() {
            return Status::NeedsSettings(installed.unconfigured().to_vec());
        }
        match self.live.lock().ok().and_then(|live| live.get(installed.id()).cloned()) {
            Some(Loaded::Running(_)) => Status::Running,
            Some(Loaded::Failed(why)) => Status::Failed(why),
            // Everything an extension contributes is read from its manifest,
            // so one that only adds commands is listed, searchable and
            // runnable while never having been compiled. That is not a
            // problem to report; it is the design.
            None => Status::Waiting,
        }
    }

    /// Whether anything is installed and switched on.
    ///
    /// The window asks before doing any extension work at all, so a machine
    /// with none pays for none of this.
    pub fn any(&self) -> bool {
        !self.enabled().is_empty()
    }

    /// Whether anything wants to hear about new mail.
    pub fn any_rules(&self) -> bool {
        self.enabled().iter().any(|e| e.manifest.wants_messages())
    }

    /// Whether anything decorates messages.
    pub fn any_decorators(&self) -> bool {
        self.enabled().iter().any(|e| e.manifest.decorates())
    }

    /// What an extension wants shown on a message, if anything has said.
    ///
    /// The only method here a frame may call. A map lookup against a cache
    /// filled by [`Host::decorate`], and `None` until it has been.
    pub fn decoration(&self, message: MessageId) -> Option<Marks> {
        let known = self.decorations.read().ok()?;
        let marks = known.get(&message)?;
        // An entry exists for every message that has been asked about,
        // including the ones nothing had anything to say about — that empty
        // entry is what stops the row being asked again. The reader and the
        // list both want nothing back for it.
        match marks.is_empty() {
            true => None,
            false => Some(marks.clone()),
        }
    }

    /// Compiles and starts one extension, or says why it will not run.
    ///
    /// Slow the first time — compiling a component is a compiler run — and a
    /// map lookup afterwards. Never call this from the UI thread.
    fn instance(&self, installed: &Installed) -> Result<Arc<Mutex<Instance>>, String> {
        let id = installed.id().to_string();

        // An extension still waiting for a value is not attempted at all.
        //
        // The host would refuse it anyway, naming the key and the file — but
        // it would refuse it as an error, and a required setting nobody has
        // filled in yet is not one. It is the state every extension with a
        // setting is in the first time it is installed, and the settings pane
        // is where it is said.
        let missing = installed.unconfigured();
        if !missing.is_empty() {
            return Err(format!(
                "needs {} set in {}",
                missing.join(", "),
                installed.config_file().display(),
            ));
        }

        // Held across the load, not merely around the lookup.
        //
        // Three background passes reach for an extension at once when the
        // window opens — the eager start, the first decoration pass and the
        // first panel draw — and a lock held only over the map lets all three
        // miss, compile the same component, and log the same failure three
        // times. Compiling is a compiler run; doing it once is worth making
        // the other two wait for.
        let mut live = match self.live.lock() {
            Ok(live) => live,
            Err(_) => return Err("the extension host is wedged".into()),
        };

        if let Some(loaded) = live.get(&id) {
            return match loaded {
                Loaded::Running(instance) => Ok(instance.clone()),
                Loaded::Failed(why) => Err(why.clone()),
            };
        }

        let started = std::time::Instant::now();
        let result = self
            .engine
            .load(installed, self.services.clone())
            .and_then(|mut instance| {
                instance.init()?;
                Ok(instance)
            });

        let loaded = match result {
            Ok(instance) => {
                log::info!("extension `{id}` started in {:?}", started.elapsed());
                Loaded::Running(Arc::new(Mutex::new(instance)))
            }
            Err(e) => {
                let why = format!("{e:#}");
                log::error!("extension `{id}` will not run: {why}");
                Loaded::Failed(why)
            }
        };

        live.insert(id, loaded.clone());

        match loaded {
            Loaded::Running(instance) => Ok(instance),
            Loaded::Failed(why) => Err(why),
        }
    }

    /// Starts everything that has to be live before anybody asks for it.
    ///
    /// An extension with a hook has to be running to do its job; one with only
    /// commands waits to be run. Called once, on a background thread, after the
    /// window is up — compiling components in front of the first frame would be
    /// paying for extensions at launch whether or not any of them ever do
    /// anything.
    pub fn start_eager(&self) {
        let eager: Vec<Installed> = self
            .registry
            .read()
            .map(|r| r.eager().cloned().collect())
            .unwrap_or_default();

        for installed in &eager {
            let _ = self.instance(installed);
        }
    }

    /// Runs a command, named as the palette names it: `spam-scorer.file`.
    pub fn run_command(&self, qualified: &str, context: CommandContext) -> Outcome {
        let Some((id, command)) = spool_ext::manifest::split_qualified(qualified) else {
            return Outcome::failed(qualified, "that is not a command name".into());
        };
        let Some(installed) = self.get(id).filter(|e| e.enabled) else {
            return Outcome::failed(id, "no such extension is switched on".into());
        };

        let instance = match self.instance(&installed) {
            Ok(instance) => instance,
            Err(why) => return Outcome::failed(id, why),
        };

        let mut guard = match instance.lock() {
            Ok(guard) => guard,
            Err(_) => return Outcome::failed(id, "this extension is wedged".into()),
        };

        match guard.run_command(command, &context) {
            Ok(outcome) => Outcome::ran(id, outcome),
            Err(e) => Outcome::failed(id, format!("{e:#}")),
        }
    }

    /// Tells every rule about mail that just landed.
    ///
    /// Sequential across extensions on purpose: they return effects the host
    /// applies, and two extensions filing the same message in different places
    /// should do so in a defined order — manifest id order, which is also the
    /// order the settings pane lists them in.
    ///
    /// An extension with a mailbox switch is handed only the mail in the
    /// mailboxes `scanning` has it switched on for, and nothing at all when
    /// that is none of them.
    pub fn on_messages(
        &self,
        messages: &[Envelope],
        scanning: &HashMap<String, HashSet<MailboxId>>,
    ) -> Vec<Outcome> {
        if messages.is_empty() {
            return Vec::new();
        }

        let mut outcomes = Vec::new();

        for installed in self.enabled().iter().filter(|e| e.manifest.wants_messages()) {
            let id = installed.id().to_string();

            let switched: Vec<Envelope>;
            let messages = match installed.manifest.scan {
                None => messages,
                Some(_) => {
                    let on = scanning.get(&id);
                    switched = messages
                        .iter()
                        .filter(|m| on.is_some_and(|on| on.contains(&m.mailbox_id)))
                        .cloned()
                        .collect();
                    if switched.is_empty() {
                        continue;
                    }
                    &switched
                }
            };

            let instance = match self.instance(installed) {
                Ok(instance) => instance,
                Err(why) => {
                    outcomes.push(Outcome::failed(&id, why));
                    continue;
                }
            };

            let Ok(mut guard) = instance.lock() else { continue };
            match guard.on_messages(messages) {
                Ok(outcome) => outcomes.push(Outcome::ran(&id, outcome)),
                Err(e) => outcomes.push(Outcome::failed(&id, format!("{e:#}"))),
            }
        }

        outcomes
    }

    /// Hands one extension a batch of mail it has been switched on for, as
    /// though it had just landed. How a mailbox's backlog reaches it.
    pub fn scan(&self, id: &str, messages: &[Envelope]) -> Outcome {
        let Some(installed) = self.get(id).filter(|e| e.enabled && e.manifest.scan.is_some()) else {
            return Outcome::failed(id, "no such extension is switched on".into());
        };
        let instance = match self.instance(&installed) {
            Ok(instance) => instance,
            Err(why) => return Outcome::failed(id, why),
        };
        let Ok(mut guard) = instance.lock() else {
            return Outcome::failed(id, "this extension is wedged".into());
        };
        match guard.on_messages(messages) {
            Ok(outcome) => Outcome::ran(id, outcome),
            Err(e) => Outcome::failed(id, format!("{e:#}")),
        }
    }

    /// Every enabled extension with a mailbox switch.
    pub fn scanners(&self) -> Vec<Installed> {
        self.enabled().into_iter().filter(|e| e.manifest.scan.is_some()).collect()
    }

    /// Fills the decoration cache for messages that have no entry yet.
    ///
    /// Returns whether anything changed, so the window can skip a redraw when
    /// nothing did. Every message gets one entry even when no extension had
    /// anything to say about it — an empty answer that is cached is a row that
    /// is never asked about again, and the alternative is re-entering a sandbox
    /// once per extension per scroll.
    pub fn decorate(&self, messages: &[Envelope]) -> bool {
        let decorators: Vec<Installed> =
            self.enabled().into_iter().filter(|e| e.manifest.decorates()).collect();
        if decorators.is_empty() {
            return false;
        }

        let wanted: Vec<&Envelope> = {
            let Ok(known) = self.decorations.read() else { return false };
            messages.iter().filter(|m| !known.contains_key(&m.id)).collect()
        };
        if wanted.is_empty() {
            return false;
        }

        let mut fresh: Vec<(MessageId, &Installed, api::Decorations)> = Vec::new();

        for installed in &decorators {
            let Ok(instance) = self.instance(installed) else { continue };
            let Ok(mut guard) = instance.lock() else { continue };
            let id = installed.id().to_string();

            for message in &wanted {
                match guard.decorate(message) {
                    Ok(decorations) if decorations.is_empty() => {}
                    Ok(decorations) => fresh.push((message.id, installed, decorations)),
                    // One message failing is not a reason to stop decorating
                    // the rest: the usual cause is that extension taking too
                    // long on this one message, and the next is very often
                    // fine.
                    Err(e) => log::debug!("`{id}` could not decorate: {e:#}"),
                }
            }
        }

        // Each message's answer is put together whole — every extension's
        // badges merged, the second to answer never erasing the first —
        // before it goes anywhere near the cache.
        let mut answers: HashMap<MessageId, Marks> = HashMap::new();
        for (message, installed, decorations) in fresh {
            let extension = installed.id().to_string();
            let entry = answers.entry(message).or_default();
            let manifest = &installed.manifest;
            for badge in &decorations.badges {
                let filter = match manifest.filters.iter().find(|f| f.matches(&badge.label)) {
                    Some(declared) => declared.id.to_string(),
                    None if manifest.badge_filters => spool_ext::manifest::badge_filter_id(&badge.label),
                    None => continue,
                };
                let tagged = (extension.clone(), filter);
                if !entry.filters.contains(&tagged) {
                    entry.filters.push(tagged);
                }
            }
            entry.badges.extend(decorations.badges);
            entry.banners.extend(
                decorations
                    .banners
                    .into_iter()
                    .map(|banner| OwnedBanner { extension: extension.clone(), banner }),
            );
        }

        let Ok(mut known) = self.decorations.write() else { return false };

        // Every message asked about gets an entry, including an empty one.
        // That empty entry is the answer "nothing to say", and caching it is
        // what stops the row being asked again on every scroll.
        //
        // Inserted only where there is none. Two passes can be asking about
        // the same message at once — the rows on screen, and a reload sorting
        // the whole list into filters — and each finds it missing before
        // either has answered. Adding to an entry the other already wrote is
        // what drew every badge twice; the first answer stands instead.
        for message in &wanted {
            let answer = answers.remove(&message.id).unwrap_or_default();
            known.entry(message.id).or_insert(answer);
        }

        true
    }

    /// Forgets every decoration, so the next pass asks about all of it again.
    ///
    /// Blunt on purpose, and the bluntness is the contract's fault rather than
    /// this function's. `decorate` is cached per message and an extension has
    /// no way to say "I know something new about these" — there is no effect
    /// for it, and adding one to the WIT before 1.0 would retire every
    /// extension built against 0.1 in order to deliver a hint.
    ///
    /// So after an extension does anything at all, the window assumes it may
    /// now answer differently about anything. Dropping the lot and asking
    /// again is cheap — `decorate` on a scored message is a key/value lookup,
    /// and the pass is bounded by what is on screen rather than by the
    /// mailbox. Being selective would mean guessing which messages a command
    /// touched, and a sweep touches messages that were never selected.
    pub fn forget_decorations(&self) {
        let Ok(mut known) = self.decorations.write() else { return };
        known.clear();
    }

    /// Forgets the decorations of these messages only: the ones a scan just
    /// handed over, which are the only ones it can have changed.
    pub fn forget_decorations_of(&self, messages: &[MessageId]) {
        let Ok(mut known) = self.decorations.write() else { return };
        for message in messages {
            known.remove(message);
        }
    }

    /// Which of one extension's filters each message is in, under one read
    /// of the cache. Empty for a message that has not been decorated.
    pub fn filters_of(&self, extension: &str, messages: &[MessageId]) -> Vec<Vec<String>> {
        let Ok(known) = self.decorations.read() else { return vec![Vec::new(); messages.len()] };
        messages
            .iter()
            .map(|message| match known.get(message) {
                Some(marks) => {
                    marks.filters.iter().filter(|(e, _)| e == extension).map(|(_, f)| f.clone()).collect()
                }
                None => Vec::new(),
            })
            .collect()
    }

    /// Every panel any enabled extension contributes.
    ///
    /// Read from the manifests, so the sidebar knows what sections exist — and
    /// what to call them — before a single component has been compiled.
    pub fn panels(&self) -> Vec<spool_ext::registry::ContributedPanel> {
        self.registry.read().map(|r| r.panels()).unwrap_or_default()
    }

    /// The last tree a panel described, if it has been asked.
    ///
    /// A cache read, safe on the UI thread. `None` until [`Host::draw_panels`]
    /// has run, which is a frame or two after the window opens.
    pub fn panel(&self, qualified: &str) -> Option<api::Panel> {
        self.panels.read().ok()?.get(qualified).cloned()
    }

    /// Asks every contributed panel to describe itself again.
    ///
    /// Returns whether anything changed. Never call this from the UI thread:
    /// it enters every extension that has a panel.
    pub fn draw_panels(&self) -> bool {
        let mut drawn = Vec::new();

        for contributed in self.panels() {
            let Some((id, panel)) = spool_ext::manifest::split_qualified(&contributed.qualified)
            else {
                continue;
            };
            let Some(installed) = self.get(id).filter(|e| e.enabled) else { continue };
            let Ok(instance) = self.instance(&installed) else { continue };
            let Ok(mut guard) = instance.lock() else { continue };

            match guard.render_panel(panel) {
                Ok(tree) => drawn.push((contributed.qualified.clone(), tree)),
                Err(e) => log::warn!("`{id}` could not draw `{panel}`: {e:#}"),
            }
        }

        if drawn.is_empty() {
            return false;
        }

        let Ok(mut panels) = self.panels.write() else { return false };
        for (qualified, tree) in drawn {
            panels.insert(qualified, tree);
        }
        true
    }
}

/// Where an extension has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Compiled, started, and answering.
    Running,
    /// Installed and switched on, but nothing has needed it yet.
    Waiting,
    /// Switched off. It keeps its stored state and contributes nothing.
    Off,
    /// Required settings it has not been given.
    NeedsSettings(Vec<String>),
    /// It would not start, and this is why.
    Failed(String),
}

impl Status {
    pub fn summary(&self) -> String {
        match self {
            Status::Running => "Running".into(),
            Status::Waiting => "Ready".into(),
            Status::Off => "Off".into(),
            Status::NeedsSettings(keys) => format!("Needs {}", keys.join(", ")),
            Status::Failed(_) => "Will not start".into(),
        }
    }

    /// Whether this is something the person has to do something about.
    pub fn wants_attention(&self) -> bool {
        matches!(self, Status::NeedsSettings(_) | Status::Failed(_))
    }
}

/// What one call into an extension produced.
///
/// Carries the extension's id because the window has to be able to say which
/// one asked for something, and because a failure nobody can attribute is a
/// failure nobody can switch off.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub extension: String,
    pub effects: Vec<api::Effect>,
    pub failure: Option<String>,
}

impl Outcome {
    fn ran(extension: &str, outcome: spool_ext::runtime::Outcome) -> Self {
        if outcome.refused > 0 {
            log::warn!(
                "`{extension}` asked for {} thing(s) its manifest does not declare",
                outcome.refused,
            );
        }
        if outcome.dropped > 0 {
            log::warn!("`{extension}` returned more effects than one call may: {} dropped", outcome.dropped);
        }
        Self { extension: extension.to_string(), effects: outcome.effects, failure: None }
    }

    fn failed(extension: &str, why: String) -> Self {
        Self { extension: extension.to_string(), effects: Vec::new(), failure: Some(why) }
    }
}

/// The host's side of the boundary, answered from the store and the network.
///
/// Every method here is called from a background thread with a component
/// blocked waiting on it, so none of them may touch the UI and none of them
/// should be slow for reasons of their own.
struct AppServices {
    store: Store,
}

impl Services for AppServices {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or_default()
    }

    fn envelopes(&self, query: MailQuery) -> Vec<Envelope> {
        self.store
            .envelopes(query.scope, &query.filter, query.limit)
            .unwrap_or_else(|e| {
                log::warn!("an extension's query failed: {e:#}");
                Vec::new()
            })
    }

    fn body(&self, id: MessageId) -> Option<Body> {
        self.store.body(id).ok().flatten()
    }

    fn header(&self, id: MessageId, name: &str) -> Option<String> {
        self.store.header(id, name).ok().flatten()
    }

    fn mailboxes(&self) -> Vec<Mailbox> {
        self.store.mailboxes().unwrap_or_default()
    }

    fn accounts(&self) -> Vec<Account> {
        self.store.accounts().unwrap_or_default()
    }

    fn load(&self, extension: &str, key: &str) -> Option<String> {
        self.store.ext_get(extension, key).ok().flatten()
    }

    fn save(&self, extension: &str, key: &str, value: &str) {
        if let Err(e) = self.store.ext_set(extension, key, value) {
            log::warn!("`{extension}` could not store `{key}`: {e:#}");
        }
    }

    fn forget(&self, extension: &str, key: &str) {
        if let Err(e) = self.store.ext_delete(extension, key) {
            log::warn!("`{extension}` could not forget `{key}`: {e:#}");
        }
    }

    /// Makes the request the host has already decided is allowed.
    ///
    /// The capability check happened before this was called, against the
    /// hostname the URL resolves to. What is left here is the timeout, which
    /// belongs to the host: an extension blocked in `fetch` is blocked in our
    /// code, and its own deadline does not run while we are the ones waiting.
    fn fetch(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let timeout = std::time::Duration::from_secs(30);

        let work = spool_net::http::request(
            &request.method,
            &request.url,
            &request.headers,
            request.body.as_deref(),
            MAX_RESPONSE,
        );

        // Blocking is correct here: this is a background thread, the component
        // that called us is waiting on it, and `Services::fetch` is documented
        // as blocking. The timeout is what stops a server that accepts a
        // connection and then says nothing from holding the thread forever.
        let answered = smol::block_on(async {
            smol::future::or(
                async { Some(work.await) },
                async {
                    smol::Timer::after(timeout).await;
                    None
                },
            )
            .await
        });

        match answered {
            Some(Ok(reply)) => Ok(HttpResponse {
                status: reply.status,
                headers: reply.headers,
                body: reply.body,
            }),
            Some(Err(e)) => Err(format!("{e:#}")),
            None => Err(format!("no answer within {}s", timeout.as_secs())),
        }
    }
}

/// Writes one extension's `config.toml` from the values it was given.
///
/// The whole file each time, generated from what the manifest declares, in the
/// order it declares them. A key with no value is left out rather than written
/// empty: "not set" and "set to nothing" are different states, and only one of
/// them is what somebody means when they clear a field.
///
/// Values are written through `toml::Value::String`, so a key containing a
/// quote or a backslash comes back out as what went in rather than as a file
/// that will not parse.
pub fn write_settings(
    installed: &Installed,
    values: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let mut doc = String::from(
        "# Written by Spool. Every key here is declared in spool.toml; one\n\
         # that is not is refused by name when the extension loads.\n",
    );

    // Values first and lists after: a `[[list]]` header opens a table, and
    // every key written below it would belong to that table.
    let (lists, plain): (Vec<_>, Vec<_>) = installed.manifest.settings().iter().partition(|d| d.is_list());
    for decl in plain {
        let Some(value) = values.get(decl.key.as_str()) else { continue };
        doc.push('\n');
        if !decl.description.trim().is_empty() {
            doc.push_str(&format!("# {}\n", decl.description.trim()));
        }
        doc.push_str(&format!("{} = {}\n", decl.key, toml::Value::String(value.clone())));
    }
    for decl in lists {
        let Some(value) = values.get(decl.key.as_str()) else { continue };
        let rows: Vec<std::collections::BTreeMap<String, String>> = serde_json::from_str(value)?;
        if !decl.description.trim().is_empty() {
            doc.push_str(&format!("\n# {}\n", decl.description.trim()));
        }
        // An emptied list is written as one, rather than left out and read
        // back as the defaults the person just removed.
        if rows.is_empty() {
            doc.push_str(&format!("{} = []\n", decl.key));
        }
        for row in rows {
            doc.push_str(&format!("\n[[{}]]\n", decl.key));
            for field in &decl.fields {
                let value = row.get(field.key.as_str()).cloned().unwrap_or_default();
                doc.push_str(&format!("{} = {}\n", field.key, toml::Value::String(value)));
            }
        }
    }

    std::fs::write(installed.config_file(), doc)?;
    Ok(())
}

/// Switches an extension on or off, and writes the decision to disk.
///
/// Reloads the registry to do it rather than mutating the one the host holds.
/// The host's copy is shared with every background thread that might be inside
/// a component right now, and tearing an instance down underneath one of those
/// is not a thing to do for a checkbox — so the change is written, and it takes
/// effect at the next launch.
pub fn set_enabled(root: &Path, id: &str, enabled: bool) -> Result<()> {
    let mut registry = Registry::load(root.to_path_buf());
    registry.set_enabled(id, enabled)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use spool_core::MailboxRole;

    /// Installs the real spam scorer, built by the same script a person would
    /// run, into a directory of this test's own.
    ///
    /// `None` when this machine cannot build for wasm, which is the only
    /// excuse for skipping the tests below.
    fn install(name: &str, config: Option<&str>) -> Option<PathBuf> {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().ok()?;
        let built = repo.join("target/extensions/spam-scorer");

        if !built.join("extension.wasm").exists() {
            let status = std::process::Command::new("bash")
                .arg(repo.join("scripts/build-extension.sh"))
                .arg("spam-scorer")
                .status();
            if !matches!(status, Ok(status) if status.success()) {
                eprintln!("SKIPPING: could not build the spam scorer for this test");
                return None;
            }
        }

        let root = std::env::temp_dir().join(format!("spool-host-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("spam-scorer");
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::copy(built.join("extension.wasm"), dir.join("extension.wasm")).ok()?;
        std::fs::copy(built.join("spool.toml"), dir.join("spool.toml")).ok()?;

        if let Some(config) = config {
            std::fs::write(dir.join("config.toml"), config).ok()?;
        }

        Some(root)
    }

    /// A store with an account, an inbox and a message in it.
    fn store_with_mail() -> Store {
        let store = Store::in_memory().unwrap();
        let account = store.add_account("Test", "me@example.com", 0).unwrap();
        store.upsert_mailbox(account, "INBOX", MailboxRole::Inbox).unwrap();
        store
    }

    #[test]
    fn extensions_live_beside_the_mail_store() {
        assert_eq!(
            directory(Path::new("/home/someone/.local/share/spool/mail.db")),
            PathBuf::from("/home/someone/.local/share/spool/extensions"),
        );
    }

    #[test]
    fn a_host_with_nothing_installed_is_a_host_that_does_nothing() {
        let store = Store::in_memory().unwrap();
        let host = Host::open(PathBuf::from("/nowhere/that/exists"), store).unwrap();

        assert!(!host.any(), "nothing is installed");
        assert!(!host.any_rules());
        assert!(!host.any_decorators());
        assert!(host.decoration(MessageId(1)).is_none());
        // And asking it to do things is not an error, it is nothing happening.
        assert!(host.on_messages(&[], &HashMap::new()).is_empty());
        assert!(host.panels().is_empty());
        assert!(!host.draw_panels(), "nothing to draw is not a change");
    }

    #[test]
    fn a_command_nobody_contributed_fails_by_name() {
        let store = Store::in_memory().unwrap();
        let host = Host::open(PathBuf::from("/nowhere/that/exists"), store).unwrap();

        let outcome = host.run_command("ghost.run", CommandContext::default());
        assert_eq!(outcome.extension, "ghost");
        assert!(outcome.failure.is_some());
        assert!(outcome.effects.is_empty());

        let nonsense = host.run_command("notqualified", CommandContext::default());
        assert!(nonsense.failure.is_some());
    }

    /// The whole chain, with nothing faked but the mailbox being small.
    ///
    /// A real component, compiled by the real build script, loaded by the real
    /// engine, answered by the real `Services` over a real SQLite store. What
    /// this proves that `crates/ext`'s own tests cannot is the half that lives
    /// here: that settings declared in a manifest reach the inside of a
    /// sandbox through the database, and that what comes back out draws.
    #[test]
    fn settings_reach_the_sandbox_through_the_store() {
        let Some(root) = install("panel", Some("api-key = \"sk-test\"\nthreshold = 2\n")) else {
            return;
        };
        let store = store_with_mail();
        let host = Host::open(root, store.clone()).expect("the engine starts");

        assert!(host.any());
        assert!(host.any_rules(), "it declares the messages hook");
        assert!(host.any_decorators(), "and the decorate hook");

        let installed = host.get("spam-scorer").expect("it is installed");
        assert!(installed.unconfigured().is_empty(), "the key is set");

        // Compiling and starting it is what seeds the settings, so nothing is
        // in the store until something needs the extension.
        host.start_eager();
        assert_eq!(host.status(&installed), Status::Running, "it started");

        // Through the real store, under the namespace the host owns.
        assert_eq!(
            store.ext_get("spam-scorer", "config.api-key").unwrap().as_deref(),
            Some("sk-test"),
        );
        // A default from the manifest, filled in beside the file's values.
        assert_eq!(
            store.ext_get("spam-scorer", "config.model").unwrap().as_deref(),
            Some("jev-latest"),
        );

        // Including a choice nobody has made yet.
        assert_eq!(
            store.ext_get("spam-scorer", "config.lookback").unwrap().as_deref(),
            Some("7d"),
        );
    }

    fn envelope_in(id: i64, mailbox: i64) -> spool_core::Envelope {
        spool_core::Envelope {
            id: spool_core::MessageId(id),
            account_id: spool_core::AccountId(1),
            mailbox_id: spool_core::MailboxId(mailbox),
            thread_id: spool_core::ThreadId(id),
            from: spool_core::Address::new("Ana", "ana@example.com"),
            subject: "Lunch?".into(),
            date: 1_700_000_000,
            snippet: "...".into(),
            flags: spool_core::Flags::default(),
            has_attachments: false,
            size: 4_000,
            group: None,
        }
    }

    /// The promise the switch makes: mail in a mailbox it is not on for is
    /// never handed to the extension, so it never leaves the machine.
    #[test]
    fn new_mail_reaches_a_switched_extension_only_from_its_mailboxes() {
        let Some(root) = install("switched", Some("api-key = \"sk-test\"\n")) else { return };
        let host = Host::open(root, store_with_mail()).expect("the engine starts");
        let mail = [envelope_in(1, 1)];

        assert!(host.on_messages(&mail, &HashMap::new()).is_empty(), "on nowhere");

        let elsewhere = HashMap::from([("spam-scorer".to_string(), HashSet::from([MailboxId(2)]))]);
        assert!(host.on_messages(&mail, &elsewhere).is_empty(), "on for another mailbox");
    }

    /// Two passes asking about the same messages at once — the rows on
    /// screen and a reload sorting the list into filters — must not both add
    /// their answer. That was every badge drawn twice.
    #[test]
    fn passes_racing_over_the_same_messages_leave_one_badge_each() {
        let Some(root) = install("racing", Some("api-key = \"sk-test\"\n")) else { return };
        let store = store_with_mail();
        for id in 1..=40 {
            let verdict = r#"{"s":0.4,"sc":0.9,"c":"personal","cc":0.9,"i":0.05,"at":0}"#;
            store.ext_set("spam-scorer", &format!("v2.local.{id}"), verdict).unwrap();
        }
        let host = Host::open(root, store).expect("the engine starts");
        let list: Vec<_> = (1..=40).map(|id| envelope_in(id, 1)).collect();

        for _ in 0..10 {
            host.forget_decorations();
            std::thread::scope(|scope| {
                for _ in 0..4 {
                    scope.spawn(|| host.decorate(&list));
                }
            });
            for message in &list {
                let marks = host.decoration(message.id).expect("scored, so marked");
                assert_eq!(marks.badges.len(), 1, "{:?}", marks.badges);
                assert_eq!(marks.filters.len(), 1, "{:?}", marks.filters);
            }
        }
    }

    /// Verdicts already stored sort the list into the extension's filters —
    /// counted, and narrowed to the one chosen.
    #[test]
    fn a_decorated_list_sorts_into_the_filters() {
        let Some(root) = install("filters", Some("api-key = \"sk-test\"\n")) else { return };
        let store = store_with_mail();
        let verdict = |score: f64, category: &str| {
            format!(r#"{{"s":{score},"sc":0.9,"c":"{category}","cc":0.9,"i":0.05,"at":0}}"#)
        };
        // Where the extension keeps a verdict for a message with no
        // `Message-ID`, which is every message in a store this empty.
        store.ext_set("spam-scorer", "v2.local.1", &verdict(3.6, "Promotion")).unwrap();
        store.ext_set("spam-scorer", "v2.local.2", &verdict(0.4, "Receipt")).unwrap();
        store.ext_set("spam-scorer", "v2.local.4", &verdict(1.9, "Promotion")).unwrap();
        let host = Host::open(root, store).expect("the engine starts");

        let mut list = vec![envelope_in(1, 1), envelope_in(2, 1), envelope_in(3, 1), envelope_in(4, 1)];
        let counts = crate::extension_flow::sort_into_filters(&host, "spam-scorer", None, &mut list);
        let count = |id: &str| counts.iter().find(|(f, _)| f == id).map(|(_, n)| *n);
        assert_eq!(count("spam"), Some(1), "over the threshold");
        assert_eq!(count("clean"), Some(1), "a receipt, which does not show");
        assert_eq!(count("forged"), Some(0));
        // A label is a filter of its own, named as it is.
        assert_eq!(count("badge:Promotion"), Some(1));
        assert_eq!(list.len(), 4, "nothing chosen, nothing hidden");

        let chosen = Some(("spam-scorer".to_string(), "spam".to_string()));
        crate::extension_flow::sort_into_filters(&host, "spam-scorer", chosen, &mut list);
        assert_eq!(list.iter().map(|e| e.id.0).collect::<Vec<_>>(), [1]);
    }

    /// An unscored message is a row with nothing on it.
    ///
    /// The other half of the decoration contract: `decorate` is a lookup, so a
    /// message nothing has decided about yet draws as it always did — and the
    /// empty answer is cached, so it is asked about once rather than per
    /// scroll.
    #[test]
    fn a_message_nothing_has_decided_about_draws_as_it_always_did() {
        let Some(root) = install("decorate", Some("api-key = \"sk-test\"\n")) else { return };
        let store = store_with_mail();
        let host = Host::open(root, store).expect("the engine starts");

        let message = spool_core::Envelope {
            id: spool_core::MessageId(1),
            account_id: spool_core::AccountId(1),
            mailbox_id: spool_core::MailboxId(1),
            thread_id: spool_core::ThreadId(1),
            from: spool_core::Address::new("Ana", "ana@example.com"),
            subject: "Lunch?".into(),
            date: 1_700_000_000,
            snippet: "...".into(),
            flags: spool_core::Flags::default(),
            has_attachments: false,
            size: 4_000,
            group: None,
        };

        assert!(host.decorate(&[message.clone()]), "it was asked");
        assert!(host.decoration(message.id).is_none(), "and had nothing to say");
        assert!(!host.decorate(&[message]), "and is not asked again");
    }

    /// The regression that made the whole thing look broken.
    ///
    /// A message is decorated once and the answer cached — including the empty
    /// answer, which is what stops every scroll re-entering a sandbox. Then a
    /// command scores it. The extension now has plenty to say about that
    /// message and no way at all to say so: `run-command` returns effects, and
    /// there is no effect meaning "ask me about this again".
    ///
    /// So the window drops the cache after anything an extension does. Without
    /// that, scoring the selection updates the counters, writes the verdicts,
    /// and leaves every row showing the answer it was given before it was
    /// scored — which reads as an extension that does not work.
    #[test]
    fn a_decorated_message_is_asked_about_again_once_something_has_run() {
        let Some(root) = install("invalidated", Some("api-key = \"sk-test\"\n")) else { return };
        let host = Host::open(root, store_with_mail()).expect("the engine starts");

        let message = spool_core::Envelope {
            id: spool_core::MessageId(1),
            account_id: spool_core::AccountId(1),
            mailbox_id: spool_core::MailboxId(1),
            thread_id: spool_core::ThreadId(1),
            from: spool_core::Address::new("Ana", "ana@example.com"),
            subject: "Lunch?".into(),
            date: 1_700_000_000,
            snippet: "...".into(),
            flags: spool_core::Flags::default(),
            has_attachments: false,
            size: 4_000,
            group: None,
        };

        assert!(host.decorate(&[message.clone()]), "asked the first time");
        assert!(!host.decorate(&[message.clone()]), "and cached, including the empty answer");

        host.forget_decorations();

        assert!(
            host.decorate(&[message]),
            "after an extension runs, the row has to be asked again",
        );
    }

    /// Without the key it declared as required, it does not start — and the
    /// pane says which key and which file rather than the extension failing
    /// on its first request.
    #[test]
    fn an_extension_that_was_never_configured_says_what_it_needs() {
        let Some(root) = install("unconfigured", None) else { return };
        let host = Host::open(root, store_with_mail()).expect("the engine starts");

        let installed = host.get("spam-scorer").expect("it is installed");
        assert_eq!(
            host.status(&installed),
            Status::NeedsSettings(vec!["api-key".to_string()]),
        );
        assert!(host.status(&installed).wants_attention());
        assert_eq!(host.status(&installed).summary(), "Needs api-key");

        // And it is still listed, still switched on, still contributing its
        // commands to the palette — waiting for a value, not broken.
        assert!(host.problems().is_empty());
        assert_eq!(host.commands().len(), 3);
        assert!(host.panels().is_empty(), "it has no sidebar panel");
    }

    /// Typing a key into the pane and pressing Save is the whole of this test.
    ///
    /// It is the one that proves the settings screen does something: an
    /// extension that would not start because a required value was missing
    /// starts, without the app being restarted, because the file was written
    /// and the compiled instance dropped.
    #[test]
    fn writing_a_setting_is_what_lets_an_extension_start() {
        let Some(root) = install("written", None) else { return };
        let host = Host::open(root, store_with_mail()).expect("the engine starts");

        let installed = host.get("spam-scorer").expect("it is installed");
        assert_eq!(host.status(&installed), Status::NeedsSettings(vec!["api-key".into()]));

        // What the pane does when Save is pressed.
        let mut values = std::collections::BTreeMap::new();
        values.insert("api-key".to_string(), "sk-typed-in".to_string());
        values.insert("threshold".to_string(), "2".to_string());
        write_settings(&installed, &values).unwrap();
        host.reload("spam-scorer");

        let reloaded = host.get("spam-scorer").expect("still installed");
        assert!(reloaded.unconfigured().is_empty(), "it has what it asked for");
        assert_eq!(reloaded.config.get("api-key"), Some("sk-typed-in"));
        // A setting nobody typed into still gets its manifest default.
        assert_eq!(reloaded.config.get("model"), Some("jev-latest"));

        // And it now starts, in the same session.
        host.start_eager();
        assert_eq!(host.status(&reloaded), Status::Running);
    }

    #[test]
    fn clearing_a_field_removes_the_key_rather_than_setting_it_to_nothing() {
        let Some(root) = install("cleared", Some("api-key = \"sk-old\"\nmodel = \"jev-1\"\n"))
        else {
            return;
        };
        let host = Host::open(root, store_with_mail()).expect("the engine starts");
        let installed = host.get("spam-scorer").unwrap();
        assert_eq!(installed.config.get("model"), Some("jev-1"));

        // The pane hands over only the fields that have something in them, so
        // a cleared `model` simply is not here.
        let mut values = std::collections::BTreeMap::new();
        values.insert("api-key".to_string(), "sk-old".to_string());
        write_settings(&installed, &values).unwrap();
        host.reload("spam-scorer");

        let reloaded = host.get("spam-scorer").unwrap();
        // Back to the manifest default, which is what "not set" means — and
        // not the empty string, which would be a model called "".
        assert_eq!(reloaded.config.get("model"), Some("jev-latest"));
    }

    #[test]
    fn a_value_with_quotes_in_it_comes_back_as_what_went_in() {
        let Some(root) = install("quoted", None) else { return };
        let host = Host::open(root, store_with_mail()).expect("the engine starts");
        let installed = host.get("spam-scorer").unwrap();

        let awkward = r#"sk-"with" \quotes\ and a # hash"#;
        let mut values = std::collections::BTreeMap::new();
        values.insert("api-key".to_string(), awkward.to_string());
        write_settings(&installed, &values).unwrap();
        host.reload("spam-scorer");

        // Round-tripped rather than producing a file that will not parse,
        // which is what hand-rolled quoting gets you.
        assert!(host.problems().is_empty(), "{:?}", host.problems());
        assert_eq!(host.get("spam-scorer").unwrap().config.get("api-key"), Some(awkward));
    }
}
