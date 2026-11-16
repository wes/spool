//! The extension host.
//!
//! Extensions are WebAssembly components. That choice is what makes the rest of
//! this crate's shape inevitable: a component cannot be handed a pointer into
//! the app's memory, cannot call a function it was not linked against, and
//! cannot outrun a deadline the host sets — so capabilities are enforceable,
//! a buggy extension cannot corrupt the store, and a runaway one is stopped
//! rather than hung on.
//!
//! The crate knows nothing about gpui. It reads manifests, loads components,
//! calls them off the UI thread and hands back plain data. Everything about how
//! that data is drawn lives in the app, which is what lets the whole extension
//! system be tested without a window.

pub mod api;
pub mod capability;
pub mod config;
mod convert;
mod host;
pub mod manifest;
pub mod registry;
pub mod runtime;
pub mod services;

/// The generated bindings for `wit/spool.wit`.
///
/// Private: the rest of the app should never see a generated type. The host
/// translates at the boundary, in `convert`, so a change to the WIT is a change
/// to one module rather than to every call site in the window.
mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "extension",
        // Host functions return `Result`, so a capability an extension was
        // never granted becomes a trap it cannot mistake for an empty answer.
        imports: { default: trappable },
    });
}

