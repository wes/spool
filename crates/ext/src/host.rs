//! The host side of the boundary: what happens when an extension calls out.
//!
//! Every function here is a capability check followed by a delegation to
//! [`Services`]. The check is the interesting part, and it is deliberately
//! belt-and-braces:
//!
//! * At load, [`crate::runtime`] reads the component's imports and refuses to
//!   start anything that imports a function its manifest did not pay for. That
//!   is the real enforcement, and it happens before a single instruction runs.
//! * Here, the same question is asked again at the call. It should be
//!   unreachable, and it is a trap rather than an empty result, because an
//!   extension reaching a function it was refused is either a host bug or an
//!   attack — and neither should look to the extension like an empty inbox.
//!
//! The second check is what makes the first one safe to be wrong about.

use std::sync::Arc;

use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::{
    capability::{Capability, Grants},
    config,
    convert::{self, wit},
    services::Services,
};

/// Everything one loaded extension can reach.
pub(crate) struct HostState {
    /// The extension's id, used to namespace its storage and to prefix its log
    /// lines. Held by the host, never supplied by the extension.
    pub(crate) id: String,
    pub(crate) grants: Grants,
    pub(crate) services: Arc<dyn Services>,
    pub(crate) limits: wasmtime::StoreLimits,
    /// WASI, with nothing in it.
    ///
    /// A Rust component built for `wasm32-wasip2` imports pieces of WASI
    /// whether its author asked for them or not — the standard library wires
    /// up stdout and the panic handler through it. So WASI is linked, and it
    /// is linked empty: no preopened directories, no environment, no inherited
    /// stdio, no sockets. An extension that tries to read a file finds nothing
    /// to read from rather than finding the user's home directory.
    pub(crate) wasi: WasiCtx,
    pub(crate) table: ResourceTable,
}

impl HostState {
    /// Refuses a call the manifest never asked for.
    ///
    /// Returns an error, which becomes a trap: the extension's call fails, the
    /// host logs which capability was missing, and nothing partial is left
    /// behind.
    fn require(&self, capability: Capability) -> wasmtime::Result<()> {
        if self.grants.has(&capability) {
            return Ok(());
        }
        wasmtime::bail!(
            "extension `{}` reached a host function it has no `{capability}` capability for",
            self.id,
        )
    }

    fn require_mail(&self) -> wasmtime::Result<()> {
        self.require(Capability::ReadMail)
    }

    fn require_storage(&self) -> wasmtime::Result<()> {
        self.require(Capability::Storage)
    }

    /// Refuses a write to the part of the extension's own storage the host
    /// owns.
    ///
    /// Settings are seeded under `config.`, and they are readable — an
    /// extension has to be able to read its own API key. They are not
    /// writable, because a setting an extension can write is a setting the
    /// user did not give it, and the value of `config.toml` being the whole
    /// story is that it is the whole story.
    fn require_own(&self, key: &str) -> wasmtime::Result<()> {
        if !config::is_reserved(key) {
            return Ok(());
        }
        wasmtime::bail!(
            "extension `{}` tried to write `{key}`, which is a setting the host owns — \
             `{}` values come from {}",
            self.id,
            config::PREFIX,
            config::CONFIG_FILE,
        )
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView { ctx: &mut self.wasi, table: &mut self.table }
    }
}

// `types`, `ui` and `effects` carry no functions, so their generated traits are
// markers. Implementing them is what tells the linker this state can serve the
// whole world rather than part of it.
impl wit::types::Host for HostState {}
impl wit::ui::Host for HostState {}
impl wit::effects::Host for HostState {}

impl wit::host::Host for HostState {
    fn log(&mut self, level: wit::host::Level, message: String) -> wasmtime::Result<()> {
        // Needs no capability, and is clamped like everything else: an
        // extension logging a megabyte per message would fill a disk that is
        // not its own.
        let message = crate::api::clamp(message);
        let id = &self.id;
        match level {
            wit::host::Level::Debug => log::debug!("[{id}] {message}"),
            wit::host::Level::Info => log::info!("[{id}] {message}"),
            wit::host::Level::Warn => log::warn!("[{id}] {message}"),
            wit::host::Level::Error => log::error!("[{id}] {message}"),
        }
        Ok(())
    }

    fn now(&mut self) -> wasmtime::Result<i64> {
        Ok(self.services.now())
    }

    fn query_envelopes(
        &mut self,
        query: wit::types::Query,
    ) -> wasmtime::Result<Vec<wit::types::Envelope>> {
        self.require_mail()?;
        let found = self.services.envelopes(convert::query_from_wit(query));
        Ok(found.iter().map(convert::envelope_to_wit).collect())
    }

    fn read_body(&mut self, id: u64) -> wasmtime::Result<Option<wit::types::Body>> {
        self.require_mail()?;
        Ok(self
            .services
            .body(spool_core::MessageId(id as i64))
            .as_ref()
            .map(convert::body_to_wit))
    }

    fn header(&mut self, id: u64, name: String) -> wasmtime::Result<Option<String>> {
        self.require_mail()?;
        Ok(self.services.header(spool_core::MessageId(id as i64), &name))
    }

    fn mailboxes(&mut self) -> wasmtime::Result<Vec<wit::types::Mailbox>> {
        self.require_mail()?;
        Ok(self.services.mailboxes().iter().map(convert::mailbox_to_wit).collect())
    }

    fn accounts(&mut self) -> wasmtime::Result<Vec<wit::types::Account>> {
        self.require_mail()?;
        Ok(self.services.accounts().iter().map(convert::account_to_wit).collect())
    }

    fn store_get(&mut self, key: String) -> wasmtime::Result<Option<String>> {
        self.require_storage()?;
        Ok(self.services.load(&self.id, &key))
    }

    fn store_set(&mut self, key: String, value: String) -> wasmtime::Result<()> {
        self.require_storage()?;
        self.require_own(&key)?;
        // Clamped on both halves. This is the one host function an extension
        // can use to consume disk without limit, and the limit is cheap.
        self.services.save(&self.id, &crate::api::clamp(key), &crate::api::clamp(value));
        Ok(())
    }

    fn store_delete(&mut self, key: String) -> wasmtime::Result<()> {
        self.require_storage()?;
        self.require_own(&key)?;
        self.services.forget(&self.id, &key);
        Ok(())
    }

    fn fetch(
        &mut self,
        request: wit::host::Request,
    ) -> wasmtime::Result<Result<wit::host::Response, String>> {
        // The host is checked against the declared list, not the URL's text.
        // `url_host` resolves what the request will actually reach, which is
        // the only thing worth comparing.
        let Some(host) = convert::url_host(&request.url) else {
            return Ok(Err("only https urls can be fetched".into()));
        };
        if !self.grants.may_reach(host) {
            log::warn!(
                "[{}] tried to reach {host}, which its manifest does not declare",
                self.id
            );
            return Ok(Err(format!("`{host}` is not a host this extension declared")));
        }

        Ok(self
            .services
            .fetch(convert::request_from_wit(request))
            .map(convert::response_to_wit))
    }
}

/// What each host function costs to import.
///
/// Used at load to compare what a component imports against what its manifest
/// declared. The names are the WIT ones, in kebab case, because that is the
/// spelling a component's import list uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cost {
    /// No capability. An extension that cannot log is one nobody can debug,
    /// and one that cannot read a clock would only be given a worse way to.
    Free,
    Needs(Capability),
    /// Any declared host will do. Which host is checked per call, against the
    /// URL, because that is the only point at which the answer is knowable.
    AnyHost,
    /// Not a function this host provides. A component importing one fails to
    /// instantiate on it, so there is nothing to charge for.
    Unknown,
}

pub(crate) fn cost_of(function: &str) -> Cost {
    match function {
        "log" | "now" => Cost::Free,
        "query-envelopes" | "read-body" | "header" | "mailboxes" | "accounts" => {
            Cost::Needs(Capability::ReadMail)
        }
        "store-get" | "store-set" | "store-delete" => Cost::Needs(Capability::Storage),
        "fetch" => Cost::AnyHost,
        _ => Cost::Unknown,
    }
}

impl Cost {
    /// Whether a set of grants covers this.
    pub(crate) fn covered_by(&self, grants: &Grants) -> bool {
        match self {
            Self::Free | Self::Unknown => true,
            Self::Needs(capability) => grants.has(capability),
            Self::AnyHost => grants.networks(),
        }
    }

    /// What to tell the author they are missing.
    pub(crate) fn wanted(&self) -> Option<String> {
        match self {
            Self::Free | Self::Unknown => None,
            Self::Needs(capability) => Some(capability.to_string()),
            Self::AnyHost => Some("net:https:<host>".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(caps: &[&str]) -> Grants {
        Grants::new(caps.iter().map(|c| c.parse::<Capability>().unwrap()))
    }

    #[test]
    fn every_mail_reading_function_costs_the_mail_capability() {
        for name in ["query-envelopes", "read-body", "header", "mailboxes", "accounts"] {
            assert_eq!(cost_of(name), Cost::Needs(Capability::ReadMail), "{name}");
            assert!(!cost_of(name).covered_by(&grants(&["ui"])), "{name}");
            assert!(cost_of(name).covered_by(&grants(&["read-mail"])), "{name}");
        }
    }

    #[test]
    fn logging_and_the_clock_cost_nothing() {
        assert!(cost_of("log").covered_by(&Grants::default()));
        assert!(cost_of("now").covered_by(&Grants::default()));
        assert_eq!(cost_of("log").wanted(), None);
    }

    #[test]
    fn storage_is_its_own_capability() {
        for name in ["store-get", "store-set", "store-delete"] {
            assert_eq!(cost_of(name), Cost::Needs(Capability::Storage), "{name}");
        }
    }

    #[test]
    fn fetching_costs_having_declared_some_host_but_not_a_particular_one() {
        assert_eq!(cost_of("fetch"), Cost::AnyHost);
        assert!(!cost_of("fetch").covered_by(&grants(&["read-mail"])));
        assert!(cost_of("fetch").covered_by(&grants(&["net:https:api.example.com"])));
        // Which host is a question for the call, not for the import.
        assert!(cost_of("fetch").covered_by(&grants(&["net:https:*.other.com"])));
    }

    #[test]
    fn a_function_nobody_knows_is_not_charged_for() {
        // An unknown name is one the host never provides, so the component
        // fails to instantiate on it rather than being refused here.
        assert_eq!(cost_of("drop-the-database"), Cost::Unknown);
        assert!(cost_of("drop-the-database").covered_by(&Grants::default()));
    }
}
