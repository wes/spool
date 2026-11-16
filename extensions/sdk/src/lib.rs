//! Write a Spool extension in Rust.
//!
//! The component model requires a world's every export to be implemented, which
//! for `spool:extension` means five functions whether or not an extension has
//! anything to say in four of them. This crate's whole job is to make that
//! untrue in practice: implement [`Extension`], override what you care about,
//! and the rest are no-ops that cost nothing — the host skips calls your
//! manifest does not declare anyway.
//!
//! ```ignore
//! use spool_extension::{Extension, Effect, CommandContext, export_extension};
//!
//! struct Mine;
//!
//! impl Extension for Mine {
//!     fn run_command(id: String, _: CommandContext) -> Vec<Effect> {
//!         vec![Effect::Status(format!("ran {id}"))]
//!     }
//! }
//!
//! export_extension!(Mine);
//! ```
//!
//! The host is reachable through [`host`]: `host::query_envelopes`,
//! `host::read_body`, `host::log` and the rest. Each needs the matching
//! capability in `spool.toml`, and calling one you did not declare is a trap
//! rather than an empty answer — so the manifest and the code have to agree.

/// The generated bindings.
///
/// Public because the export macro has to name them from inside your crate.
/// Nothing here needs reading: the types are re-exported below under shorter
/// paths.
pub mod bindings {
    wit_bindgen::generate!({
        path: "../../crates/ext/wit",
        world: "extension",
        pub_export_macro: true,
    });
}

pub use bindings::spool::extension::effects::{Effect, FlagChange, MoveTo, Notification};
pub use bindings::spool::extension::types::{
    Account, Address, Attachment, Body, Envelope, Filter, Mailbox, MailboxRole, MessageFlags,
    Query, Scope,
};
pub use bindings::spool::extension::ui::{Badge, Banner, Button, Emphasis, Node, NodeKind, Text, Tree};
pub use bindings::{CommandContext, Decorations};

/// What the host can be asked for. Each function costs the capability named in
/// `wit/spool.wit`.
pub use bindings::spool::extension::host;

/// Everything an extension may implement.
///
/// Every method has a default, so an extension implements only the surfaces it
/// contributes. The defaults are the honest empty answer in each case: no
/// effects, no decorations, an empty panel.
pub trait Extension {
    /// Called once, before anything else.
    ///
    /// Returning an error switches the extension off for the session and puts
    /// the message in the log, so it is the place to fail loudly about
    /// configuration rather than half-working.
    fn init() -> Result<(), String> {
        Ok(())
    }

    /// New mail, in batches, after it has been committed to the store.
    ///
    /// Needs `hooks = ["messages"]` in the manifest; without it the host never
    /// calls this.
    fn on_messages(messages: Vec<Envelope>) -> Vec<Effect> {
        let _ = messages;
        Vec::new()
    }

    /// What to show on one message.
    ///
    /// Must be a pure function of its argument: the host caches what comes back
    /// and draws rows from the cache, so an answer that varies between calls
    /// shows up as flicker. Needs `hooks = ["decorate"]`.
    fn decorate(message: Envelope) -> Decorations {
        let _ = message;
        Decorations { banners: Vec::new(), badges: Vec::new() }
    }

    /// Runs one of the commands declared in the manifest.
    ///
    /// `id` is the bare id — `summarise`, not `inbox-stats.summarise`.
    fn run_command(id: String, context: CommandContext) -> Vec<Effect> {
        let _ = (id, context);
        Vec::new()
    }

    /// Draws one of the panels declared in the manifest.
    fn render_panel(id: String) -> Tree {
        let _ = id;
        ui::empty()
    }
}

/// Wires a type implementing [`Extension`] up as the component's exports.
///
/// Call this exactly once, at the top level of your crate.
#[macro_export]
macro_rules! export_extension {
    ($type:ty) => {
        const _: () = {
            struct Exports;

            impl $crate::bindings::Guest for Exports {
                fn init() -> Result<(), String> {
                    <$type as $crate::Extension>::init()
                }

                fn on_messages(
                    messages: ::std::vec::Vec<$crate::Envelope>,
                ) -> ::std::vec::Vec<$crate::Effect> {
                    <$type as $crate::Extension>::on_messages(messages)
                }

                fn decorate(message: $crate::Envelope) -> $crate::Decorations {
                    <$type as $crate::Extension>::decorate(message)
                }

                fn run_command(
                    id: ::std::string::String,
                    context: $crate::CommandContext,
                ) -> ::std::vec::Vec<$crate::Effect> {
                    <$type as $crate::Extension>::run_command(id, context)
                }

                fn render_panel(id: ::std::string::String) -> $crate::Tree {
                    <$type as $crate::Extension>::render_panel(id)
                }
            }

            $crate::bindings::export!(Exports with_types_in $crate::bindings);
        };
    };
}

/// Building panels without counting indices by hand.
///
/// A panel crosses the boundary as an arena — a flat list of nodes that name
/// their children by position — because WIT has no recursive types. Writing one
/// by hand means tracking indices, which is exactly the kind of bookkeeping that
/// produces a tree the host rejects. [`Builder`] does it instead.
pub mod ui {
    use super::{Badge, Banner, Button, Emphasis, Node, NodeKind, Text, Tree};

    /// A panel with nothing in it.
    pub fn empty() -> Tree {
        Tree { nodes: Vec::new(), root: 0 }
    }

    /// Assembles a panel bottom-up: build the children, then the thing that
    /// holds them.
    ///
    /// ```ignore
    /// let mut ui = Builder::new();
    /// let title = ui.text("Unread", Emphasis::Heading);
    /// let count = ui.text("14", Emphasis::Normal);
    /// let root = ui.column([title, count]);
    /// ui.finish(root)
    /// ```
    #[derive(Default)]
    pub struct Builder {
        nodes: Vec<Node>,
    }

    /// A node's position in the arena being built.
    ///
    /// Returned rather than a bare `u32` so an index from one builder cannot be
    /// handed to another, where it would point at whatever happened to be
    /// there.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Id(u32);

    impl Builder {
        pub fn new() -> Self {
            Self::default()
        }

        fn push(&mut self, kind: NodeKind, children: Vec<u32>) -> Id {
            self.nodes.push(Node { kind, children });
            Id(self.nodes.len() as u32 - 1)
        }

        pub fn text(&mut self, content: impl Into<String>, emphasis: Emphasis) -> Id {
            self.push(NodeKind::Text(Text { content: content.into(), emphasis }), Vec::new())
        }

        /// A button that runs one of this extension's commands.
        pub fn button(&mut self, label: impl Into<String>, run: impl Into<String>) -> Id {
            self.push(
                NodeKind::Button(Button {
                    label: label.into(),
                    run: run.into(),
                    enabled: true,
                }),
                Vec::new(),
            )
        }

        pub fn row(&mut self, children: impl IntoIterator<Item = Id>) -> Id {
            let children = children.into_iter().map(|id| id.0).collect();
            self.push(NodeKind::Row, children)
        }

        pub fn column(&mut self, children: impl IntoIterator<Item = Id>) -> Id {
            let children = children.into_iter().map(|id| id.0).collect();
            self.push(NodeKind::Column, children)
        }

        pub fn divider(&mut self) -> Id {
            self.push(NodeKind::Divider, Vec::new())
        }

        pub fn spacer(&mut self) -> Id {
            self.push(NodeKind::Spacer, Vec::new())
        }

        /// Finishes the panel, with `root` as the node the host draws.
        ///
        /// A node may be used as a child exactly once. The host refuses a tree
        /// where one is reachable twice, because drawing it would multiply
        /// rather than share.
        pub fn finish(self, root: Id) -> Tree {
            Tree { nodes: self.nodes, root: root.0 }
        }
    }

    /// A strip above a message.
    pub fn banner(text: impl Into<String>, emphasis: Emphasis) -> Banner {
        Banner { badges: Vec::new(), text: text.into(), emphasis, action: None }
    }

    /// A strip above a message that starts with badges — what the message
    /// is, drawn as a row's badges are — and goes on to a line about it.
    pub fn banner_tagged(badges: Vec<Badge>, text: impl Into<String>, emphasis: Emphasis) -> Banner {
        Banner { badges, text: text.into(), emphasis, action: None }
    }

    /// Badges beside the subject of the open message, rather than a strip
    /// above it: a banner with no text.
    pub fn tags(badges: Vec<Badge>) -> Banner {
        Banner { badges, text: String::new(), emphasis: Emphasis::Muted, action: None }
    }

    /// Puts a button on the end of a banner, which runs one of your commands.
    pub fn with_action(banner: Banner, label: impl Into<String>, run: impl Into<String>) -> Banner {
        Banner {
            action: Some(Button { label: label.into(), run: run.into(), enabled: true }),
            ..banner
        }
    }

    /// A banner with a button on the end, which runs one of your commands.
    pub fn banner_with(
        text: impl Into<String>,
        emphasis: Emphasis,
        label: impl Into<String>,
        run: impl Into<String>,
    ) -> Banner {
        Banner {
            badges: Vec::new(),
            text: text.into(),
            emphasis,
            action: Some(Button { label: label.into(), run: run.into(), enabled: true }),
        }
    }

    /// A mark on a row in the message list.
    pub fn badge(label: impl Into<String>, emphasis: Emphasis) -> Badge {
        Badge { label: label.into(), emphasis }
    }
}

/// Shorthands for the flag effects, which are the fiddliest to write by hand.
pub mod flags {
    use super::{Effect, FlagChange, MessageFlags};

    /// Turns flags on, leaving the rest alone.
    pub fn add(message: u64, flags: MessageFlags) -> Effect {
        Effect::SetFlags(FlagChange { message, add: flags, remove: MessageFlags::empty() })
    }

    /// Turns flags off, leaving the rest alone.
    pub fn remove(message: u64, flags: MessageFlags) -> Effect {
        Effect::SetFlags(FlagChange { message, add: MessageFlags::empty(), remove: flags })
    }
}
