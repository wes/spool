//! Loading components and calling into them.
//!
//! Three things bound what a loaded extension can do to the app, and all three
//! are set here rather than trusted to the extension:
//!
//! * **Time.** Every call runs under a deadline, enforced by wasmtime's epoch
//!   interruption — a counter a background thread advances, which the engine
//!   checks at loop backedges and function entries. An extension in an infinite
//!   loop is stopped, not waited for. The deadlines differ per call because the
//!   calls differ: a command a user just asked for may take seconds, and a
//!   decoration that a list row is waiting on may not.
//! * **Memory.** A store limiter caps the linear memory a component can grow
//!   to. Past it, the allocation fails inside the sandbox and the extension
//!   gets an allocation error, which is its problem rather than the machine's.
//! * **Reach.** The component's imports are compared against its manifest
//!   before instantiation. An extension importing something it never declared
//!   never starts.
//!
//! What is deliberately *not* bounded is how long a host function takes. An
//! extension blocked in `fetch` is blocked in the host's code, on the host's
//! own timeout, and the epoch deadline does not run there — which is correct,
//! because that time is not the extension's to be charged for.

use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use spool_core::Envelope;
use wasmtime::{
    StoreLimits, StoreLimitsBuilder,
    component::{Component, HasSelf, Linker, ResourceTable, types::ComponentItem},
};
use wasmtime_wasi::WasiCtxBuilder;

use crate::{
    api,
    bindings,
    capability::Grants,
    config,
    convert::{self, wit},
    host::{HostState, cost_of},
    manifest::Manifest,
    registry::Installed,
    services::Services,
};

/// How often the epoch advances.
///
/// The granularity of every deadline below, and the cost of enforcing them: one
/// thread waking a hundred times a second to increment a counter. Finer would
/// buy precision nothing here needs; coarser would make the shortest deadline
/// meaningless.
const TICK: Duration = Duration::from_millis(10);

/// How long each kind of call gets.
///
/// A struct rather than constants so that the one thing a test of the deadline
/// cannot do — wait thirty seconds to find out whether a deadline works — stops
/// being necessary. The app uses [`Budgets::default`]; nothing else should need
/// to name these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    /// Starting up. Generous, because a component's first call also pays for
    /// its own initialization.
    pub init: Duration,
    /// A batch of new mail. Long, because this is background work nobody is
    /// waiting on, and an extension may be asking the host questions about
    /// every message in the batch.
    pub messages: Duration,
    /// One message's decorations.
    ///
    /// Short on purpose. This is the only call on a path a drawn row depends
    /// on — through a cache, but a cache that has to fill. An extension that
    /// cannot decide in a twentieth of a second what to put on a message is one
    /// whose decorations are not worth the list being late for.
    pub decorate: Duration,
    /// A command the user just ran. The longest, because they asked for it and
    /// may be waiting on a network round trip inside it.
    pub command: Duration,
    /// Drawing a panel.
    pub panel: Duration,
}

impl Default for Budgets {
    fn default() -> Self {
        Self {
            init: Duration::from_secs(5),
            messages: Duration::from_secs(30),
            decorate: Duration::from_millis(50),
            command: Duration::from_secs(30),
            panel: Duration::from_millis(500),
        }
    }
}

/// The most linear memory one extension may hold.
///
/// Enough for a component doing real work over a few thousand envelopes, and
/// far short of what would make a mail client's footprint an extension's
/// decision.
const MAX_MEMORY: usize = 64 * 1024 * 1024;

/// The compiler and the clock, shared by every extension.
///
/// One engine rather than one per extension: compiled code is cached in it, the
/// epoch thread serves all of them, and instances are isolated by having their
/// own `Store` regardless.
pub struct Engine {
    engine: wasmtime::Engine,
    budgets: Budgets,
    ticker: Ticker,
}

impl Engine {
    pub fn new() -> Result<Self> {
        Self::with_budgets(Budgets::default())
    }

    /// An engine whose calls get something other than the usual deadlines.
    pub fn with_budgets(budgets: Budgets) -> Result<Self> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        // The whole basis of the time limit. Without it a `loop {}` in an
        // extension is a window that never paints again.
        config.epoch_interruption(true);
        config.cranelift_opt_level(wasmtime::OptLevel::Speed);

        let engine = wt(wasmtime::Engine::new(&config)).context("building the wasm engine")?;
        let ticker = Ticker::start(engine.clone())?;

        Ok(Self { engine, budgets, ticker })
    }

    /// Compiles and instantiates one extension.
    ///
    /// Slow enough to belong on a background thread — compilation is a real
    /// compiler run — and fallible in ways that are all the extension's fault
    /// rather than the app's, so every error here names the extension.
    pub fn load(&self, installed: &Installed, services: Arc<dyn Services>) -> Result<Instance> {
        let manifest = installed.manifest.clone();
        let id = manifest.id.to_string();

        let bytes = std::fs::read(installed.wasm())
            .with_context(|| format!("reading {}", installed.wasm().display()))?;
        let component = wt(Component::new(&self.engine, &bytes))
            .with_context(|| format!("`{id}` is not a WebAssembly component"))?;

        self.check_imports(&component, &manifest)?;

        // Settings, before anything of the extension's own runs.
        //
        // An extension declaring a required setting it has not been given is
        // refused here rather than allowed to start and fail on its first
        // call. The difference matters: "needs `api-key` — put it in
        // config.toml" is a sentence somebody can act on, and "request failed"
        // three screens later is not.
        if !installed.config.complete() {
            bail!(
                "`{id}` needs {} set in {} — see `{}`",
                installed.config.missing().join(", "),
                installed.config_file().display(),
                crate::manifest::MANIFEST_FILE,
            );
        }
        for (key, value) in installed.config.entries() {
            services.save(&id, &config::storage_key(key), value);
        }

        let mut linker: Linker<HostState> = Linker::new(&self.engine);
        // Linked first and linked empty. See `HostState::wasi`.
        wt(wasmtime_wasi::p2::add_to_linker_sync(&mut linker)).context("linking wasi")?;
        wt(bindings::Extension::add_to_linker::<HostState, HasSelf<HostState>>(
            &mut linker,
            |state| state,
        ))
        .context("linking the spool host")?;

        let state = HostState {
            id: id.clone(),
            grants: manifest.grants.clone(),
            services,
            limits: limits(),
            // No preopens, no environment, no inherited stdio, no sockets.
            wasi: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        };

        let mut store = wasmtime::Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        let clock = self.ticker.shared.clone();
        let _busy = clock.busy();
        store.set_epoch_deadline(ticks(self.budgets.init));

        let bindings = wt(bindings::Extension::instantiate(&mut store, &component, &linker))
            .with_context(|| format!("starting `{id}`"))?;

        Ok(Instance { id, manifest, budgets: self.budgets, store, bindings, clock })
    }

    /// Refuses a component that imports more than its manifest declared.
    ///
    /// The real enforcement of the capability model, and it happens here —
    /// before instantiation, before `init`, before any of the extension's code
    /// has run at all. An extension that wants to read mail without saying so
    /// does not get a failing call; it does not get to start.
    fn check_imports(&self, component: &Component, manifest: &Manifest) -> Result<()> {
        let component_type = component.component_type();

        for (name, item) in component_type.imports(&self.engine) {
            // Only this host's own interface is charged for. WASI is linked
            // empty, and anything else is a function the linker does not
            // provide, so the component fails to instantiate on it by itself.
            if !name.starts_with("spool:extension/host") {
                continue;
            }
            let ComponentItem::ComponentInstance(instance) = item.ty else {
                continue;
            };

            for (function, _) in instance.exports(&self.engine) {
                let cost = cost_of(function);
                if cost.covered_by(&manifest.grants) {
                    continue;
                }
                let wanted = cost.wanted().unwrap_or_default();
                bail!(
                    "`{}` calls `{function}` but its manifest does not ask for `{wanted}` — \
                     add it to `capabilities` in {}",
                    manifest.id,
                    crate::manifest::MANIFEST_FILE,
                );
            }
        }

        Ok(())
    }
}

/// wasmtime carries an error type of its own; this is the one place it becomes
/// the one the rest of the workspace speaks.
fn wt<T>(result: wasmtime::Result<T>) -> Result<T> {
    result.map_err(anyhow::Error::from)
}

fn limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY)
        // Not one apiece. A component is several *core* instances once it has
        // been linked — the guest's own module, plus the adapters the
        // component model puts between it and each interface — so a limit of
        // one refuses to start anything at all. These are bounds against a
        // component that instantiates in a loop, not an accounting of what a
        // well-behaved one needs.
        .instances(64)
        .tables(64)
        .build()
}

/// A deadline in epoch ticks, never zero.
///
/// A budget shorter than one tick would round to a deadline that has already
/// passed, which would fail every call rather than allowing a short one.
fn ticks(budget: Duration) -> u64 {
    (budget.as_millis() / TICK.as_millis()).max(1) as u64
}

/// The thread that makes deadlines mean anything.
///
/// It only runs while a call into an extension is in flight. A deadline is a
/// number of epochs from when it was set, so an epoch that stands still
/// between calls changes no deadline — and a thread waking a hundred times a
/// second for the life of the app, to count time nobody is measuring, was a
/// mail client sitting idle that never let the CPU sleep.
struct Ticker {
    shared: Arc<Clock>,
}

/// What the ticker thread and the calls it times share.
struct Clock {
    running: AtomicBool,
    /// Calls in flight. The thread ticks while this is above zero.
    calls: Mutex<usize>,
    wake: Condvar,
}

impl Clock {
    /// Keeps the epoch advancing until the guard is dropped.
    fn busy(self: &Arc<Self>) -> Busy {
        *self.calls.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.wake.notify_all();
        Busy(self.clone())
    }
}

/// A call in flight. See [`Clock::busy`].
struct Busy(Arc<Clock>);

impl Drop for Busy {
    fn drop(&mut self) {
        let mut calls = self.0.calls.lock().unwrap_or_else(|e| e.into_inner());
        *calls = calls.saturating_sub(1);
    }
}

impl Ticker {
    fn start(engine: wasmtime::Engine) -> Result<Self> {
        let shared = Arc::new(Clock {
            running: AtomicBool::new(true),
            calls: Mutex::new(0),
            wake: Condvar::new(),
        });
        let clock = shared.clone();

        thread::Builder::new()
            .name("spool-extension-epoch".into())
            .spawn(move || {
                loop {
                    {
                        let mut calls = clock.calls.lock().unwrap_or_else(|e| e.into_inner());
                        while *calls == 0 && clock.running.load(Ordering::Relaxed) {
                            calls = clock.wake.wait(calls).unwrap_or_else(|e| e.into_inner());
                        }
                    }
                    if !clock.running.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::sleep(TICK);
                    engine.increment_epoch();
                }
            })
            .context("starting the extension deadline thread")?;

        Ok(Self { shared })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        // The thread checks this once a tick, or as soon as it is woken, so it
        // is gone within `TICK` of the engine being dropped. It holds a clone
        // of the engine, which is what keeps incrementing an epoch nobody
        // reads from being a use after free rather than merely pointless.
        self.shared.running.store(false, Ordering::Relaxed);
        let _calls = self.shared.calls.lock().unwrap_or_else(|e| e.into_inner());
        self.shared.wake.notify_all();
    }
}

/// What a call into an extension produced: its effects, and what was refused.
///
/// Re-exported here rather than left inside a private module, because every
/// public method on [`Instance`] that does anything returns one.
pub use crate::convert::Outcome;

/// One running extension.
///
/// Not `Sync`, and deliberately so: a component instance is single-threaded,
/// and the app serializes calls behind a lock rather than this type pretending
/// otherwise. Calls take `&mut self` because every one of them is a mutation of
/// the instance's own memory.
pub struct Instance {
    id: String,
    manifest: Manifest,
    budgets: Budgets,
    store: wasmtime::Store<HostState>,
    bindings: bindings::Extension,
    /// Held so every call can keep the epoch moving while it runs.
    clock: Arc<Clock>,
}

impl Instance {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn grants(&self) -> Grants {
        self.manifest.grants.clone()
    }

    /// Starts the extension.
    ///
    /// Called once, by the host, before anything else. An extension that fails
    /// here is switched off for the session rather than retried on every call.
    pub fn init(&mut self) -> Result<()> {
        let _busy = self.clock.busy();
        self.store.set_epoch_deadline(ticks(self.budgets.init));
        self.bindings
            .call_init(&mut self.store)
            .map_err(|e| self.blame(e))?
            .map_err(|message| anyhow!("`{}` refused to start: {message}", self.id))
    }

    /// Offers a batch of new mail.
    pub fn on_messages(&mut self, messages: &[Envelope]) -> Result<Outcome> {
        if !self.manifest.wants_messages() {
            return Ok(Outcome::default());
        }
        let converted: Vec<_> = messages.iter().map(convert::envelope_to_wit).collect();

        let _busy = self.clock.busy();
        self.store.set_epoch_deadline(ticks(self.budgets.messages));
        let raw = self
            .bindings
            .call_on_messages(&mut self.store, &converted)
            .map_err(|e| self.blame(e))?;

        Ok(convert::effects_from_wit(&self.grants(), raw))
    }

    /// Asks what to show on one message.
    pub fn decorate(&mut self, message: &Envelope) -> Result<api::Decorations> {
        if !self.manifest.decorates() {
            return Ok(api::Decorations::default());
        }
        let converted = convert::envelope_to_wit(message);

        let _busy = self.clock.busy();
        self.store.set_epoch_deadline(ticks(self.budgets.decorate));
        let raw = self
            .bindings
            .call_decorate(&mut self.store, &converted)
            .map_err(|e| self.blame(e))?;

        Ok(convert::decorations_from_wit(raw))
    }

    /// Runs one of the extension's commands.
    ///
    /// `command` is the bare id — `open`, not `unsubscribe.open` — because the
    /// extension never needed the qualification it did not choose.
    pub fn run_command(&mut self, command: &str, context: &CommandContext) -> Result<Outcome> {
        let ctx = wit::CommandContext {
            selection: context.selection.iter().map(|id| id.0 as u64).collect(),
            mailbox: context.mailbox.map(|id| id.0 as u64),
            account: context.account.map(|id| id.0 as u64),
            search: context.search.clone(),
        };

        let _busy = self.clock.busy();
        self.store.set_epoch_deadline(ticks(self.budgets.command));
        let raw = self
            .bindings
            .call_run_command(&mut self.store, command, &ctx)
            .map_err(|e| self.blame(e))?;

        Ok(convert::effects_from_wit(&self.grants(), raw))
    }

    /// Draws one of the extension's panels.
    pub fn render_panel(&mut self, panel: &str) -> Result<api::Panel> {
        let _busy = self.clock.busy();
        self.store.set_epoch_deadline(ticks(self.budgets.panel));
        let raw = self
            .bindings
            .call_render_panel(&mut self.store, panel)
            .map_err(|e| self.blame(e))?;

        convert::panel_from_wit(raw)
            .map_err(|e| anyhow!("`{}` returned a panel that cannot be drawn: {e}", self.id))
    }

    /// Names the extension in an error that came out of it.
    ///
    /// Every failure in this module is somebody's extension misbehaving, and an
    /// error that does not say whose is one the user cannot act on — the only
    /// useful response to a broken extension is knowing which one to switch
    /// off.
    fn blame(&self, error: wasmtime::Error) -> anyhow::Error {
        anyhow::Error::from(error).context(format!("in extension `{}`", self.id))
    }
}

/// What the user had in front of them when a command was run.
#[derive(Debug, Clone, Default)]
pub struct CommandContext {
    pub selection: Vec<spool_core::MessageId>,
    pub mailbox: Option<spool_core::MailboxId>,
    pub account: Option<spool_core::AccountId>,
    pub search: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_budget_is_at_least_one_tick() {
        // Rounding to zero would set a deadline in the past, which fails the
        // call it was meant to allow.
        assert_eq!(ticks(Duration::from_millis(0)), 1);
        assert_eq!(ticks(Duration::from_millis(1)), 1);
        assert_eq!(ticks(TICK), 1);
    }

    #[test]
    fn budgets_convert_to_the_number_of_ticks_they_are_worth() {
        assert_eq!(ticks(Duration::from_millis(50)), 5);
        assert_eq!(ticks(Duration::from_secs(1)), 100);
        assert_eq!(ticks(Budgets::default().decorate), 5);
        assert_eq!(ticks(Budgets::default().command), 3_000);
    }

    #[test]
    fn the_decoration_budget_is_far_shorter_than_the_others() {
        // The one call a drawn row waits on. If this ever creeps up to seconds
        // the list stops being the thing this app is about.
        let budgets = Budgets::default();
        assert!(budgets.decorate < Duration::from_millis(100));
        assert!(budgets.decorate < budgets.command);
        assert!(budgets.decorate < budgets.messages);
    }

    #[test]
    fn an_engine_starts_and_stops_its_ticker() {
        // Dropping the engine has to stop the thread; otherwise every opened
        // window leaks one.
        let engine = Engine::new().expect("the engine builds");
        drop(engine);
    }
}
