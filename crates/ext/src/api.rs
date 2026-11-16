//! What an extension hands back, in the app's own vocabulary.
//!
//! Nothing in this module mentions WebAssembly. The app takes an [`Effect`] or
//! a [`Panel`] and never learns where it came from, which is what keeps the
//! window's code free of a second set of types that happen to be generated —
//! and what would let the runtime underneath be replaced without touching a
//! single call site.
//!
//! These types are also the trust boundary. Everything here arrives from a
//! component that may be buggy, abandoned or hostile, so the conversion into
//! them is where limits are enforced rather than hoped for: a panel tree is
//! checked for cycles before anything tries to draw it, and a string that would
//! be a megabyte of status bar is refused rather than truncated into something
//! that looks like it worked.

use std::fmt;

use spool_core::{Flags, MailboxId, MessageId};

/// The longest any single string from an extension may be.
///
/// Generous for a label and nowhere near enough to be a denial of service. A
/// banner is one line of a window; if an extension needs more room than this it
/// wants a panel, not a longer string.
pub const MAX_TEXT: usize = 4_096;

/// The most nodes one panel may contain.
///
/// A bound on what the host will draw, not on what an extension may compute.
/// Two thousand rows is far more than a pane can show and still small enough
/// that validating the tree is free.
pub const MAX_NODES: usize = 2_048;

/// How deeply a panel may nest.
pub const MAX_DEPTH: usize = 24;

/// The most effects one call may return.
pub const MAX_EFFECTS: usize = 512;

/// A change an extension asked the host to make.
///
/// Returned rather than performed. The host applies these itself, in order,
/// after the call has returned — so an extension that traps halfway through
/// leaves nothing half-done, and every change is a value that can be logged,
/// counted, put on the undo stack or refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Turn flags on and off. Needs `modify-mail`.
    SetFlags { message: MessageId, add: Flags, remove: Flags },
    /// File it somewhere. Needs `modify-mail`.
    MoveTo { message: MessageId, mailbox: MailboxId },
    /// File it in the account's archive. Needs `modify-mail`.
    Archive(MessageId),
    /// File it in the account's trash. Needs `modify-mail`.
    Trash(MessageId),
    /// A system notification. Needs `notify`.
    Notify { title: String, body: String, message: Option<MessageId> },
    /// A line in the status bar, which fades. Needs `ui`.
    Status(String),
    /// Open a URL in the browser. Needs `ui`.
    OpenUrl(String),
    /// Draw one of this extension's panels again. Needs `ui`.
    InvalidatePanel(String),
}

/// What an extension wants shown on one message.
///
/// A pure function of an envelope, computed off the UI thread and cached, so
/// that a row being drawn is a map lookup rather than a call into a sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decorations {
    pub banners: Vec<Banner>,
    pub badges: Vec<Badge>,
}

impl Decorations {
    pub fn is_empty(&self) -> bool {
        self.banners.is_empty() && self.badges.is_empty()
    }
}

/// A strip above a message in the reader.
///
/// One with no text is not a strip: its badges, and its button if it has one,
/// are drawn beside the message's subject instead. See [`Banner::is_tags`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    /// Marks at the start of the strip, drawn as a row's badges are.
    pub badges: Vec<Badge>,
    pub text: String,
    pub emphasis: Emphasis,
    pub action: Option<Button>,
}

impl Banner {
    /// Whether this is marks for beside the subject rather than a strip.
    pub fn is_tags(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// A small mark on a row in the message list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub label: String,
    pub emphasis: Emphasis,
}

/// How prominent something is, in terms the theme resolves.
///
/// An extension names a role and the theme picks the color. The alternative —
/// letting an extension name a color — produces something that looks right in
/// the theme it was written against and wrong in the other one, forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Emphasis {
    #[default]
    Normal,
    Muted,
    Strong,
    Heading,
    Accent,
    Danger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    pub label: String,
    /// The qualified id of a command in the same extension. Pressing this
    /// dispatches it exactly as the palette would — a button can therefore do
    /// nothing the keyboard cannot, which is deliberate.
    pub run: String,
    pub enabled: bool,
}

/// A panel, as a validated tree.
///
/// Held as an arena because that is how it crosses the boundary — WIT has no
/// recursive types — and kept that way because validating an arena is one pass
/// with no recursion, where rebuilding it into a nested structure would need
/// the very recursion the validation is there to bound.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Panel {
    nodes: Vec<Node>,
    root: Option<usize>,
}

impl Panel {
    /// Checks an arena and takes ownership of it.
    ///
    /// This is the only way to build one, so a `Panel` in hand is a tree that
    /// terminates: every index is in range, no node is its own ancestor, no
    /// node has two parents, and the whole thing is small enough and shallow
    /// enough to draw without thinking about it.
    ///
    /// The two-parent rule is not fussiness. A node reachable by two paths is
    /// drawn once per path, so a chain of twenty nodes that each appear twice
    /// is a million elements from an arena of forty — which is a frame budget
    /// gone, from a panel that looks small.
    pub fn validate(nodes: Vec<Node>, root: usize) -> Result<Self, TreeError> {
        if nodes.is_empty() {
            return Ok(Self::default());
        }
        if nodes.len() > MAX_NODES {
            return Err(TreeError::TooBig(nodes.len()));
        }
        if root >= nodes.len() {
            return Err(TreeError::OutOfRange(root));
        }

        for node in &nodes {
            if let Some(&child) = node.children.iter().find(|&&c| c as usize >= nodes.len()) {
                return Err(TreeError::OutOfRange(child as usize));
            }
        }

        // One walk from the root. `seen` catches both cycles and shared
        // subtrees, which is the same check because both mean arriving at a
        // node that has already been claimed by some other parent.
        let mut seen = vec![false; nodes.len()];
        let mut stack = vec![(root, 0usize)];
        seen[root] = true;

        while let Some((index, depth)) = stack.pop() {
            if depth >= MAX_DEPTH {
                return Err(TreeError::TooDeep);
            }
            for &child in &nodes[index].children {
                let child = child as usize;
                if std::mem::replace(&mut seen[child], true) {
                    return Err(TreeError::NotATree(child));
                }
                stack.push((child, depth + 1));
            }
        }

        Ok(Self { nodes, root: Some(root) })
    }

    /// The root, or `None` for a panel with nothing in it.
    pub fn root(&self) -> Option<&Node> {
        self.root.map(|i| &self.nodes[i])
    }

    /// One node's children, in order.
    pub fn children<'a>(&'a self, node: &'a Node) -> impl Iterator<Item = &'a Node> {
        node.children.iter().map(move |&i| &self.nodes[i as usize])
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    /// Indices into the panel's arena. Meaningless outside the [`Panel`] that
    /// owns them, which is why the field is only reachable through one.
    pub(crate) children: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Row,
    Column,
    Text { content: String, emphasis: Emphasis },
    Button(Button),
    Divider,
    Spacer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeError {
    TooBig(usize),
    TooDeep,
    OutOfRange(usize),
    /// A node reached twice: a cycle, or a subtree with two parents.
    NotATree(usize),
}

impl fmt::Display for TreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooBig(n) => write!(f, "a panel of {n} nodes is past the limit of {MAX_NODES}"),
            Self::TooDeep => write!(f, "a panel nested deeper than {MAX_DEPTH}"),
            Self::OutOfRange(i) => write!(f, "node {i} does not exist"),
            Self::NotATree(i) => write!(f, "node {i} is reachable twice — a cycle or a shared child"),
        }
    }
}

impl std::error::Error for TreeError {}

/// Shortens a string from an extension to something a window can hold.
///
/// Truncated rather than refused, because the string is very often a subject
/// line or a sender's idea of a display name — data the extension was handed
/// rather than a value it chose — and refusing the whole call over it would
/// make an extension fail on exactly the mail it was written to deal with.
/// Effects and commands are held to the limit; this is for text.
pub fn clamp(text: String) -> String {
    match text.char_indices().nth(MAX_TEXT) {
        None => text,
        Some((cut, _)) => {
            let mut text = text;
            text.truncate(cut);
            text.push('\u{2026}');
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: NodeKind, children: &[u32]) -> Node {
        Node { kind, children: children.to_vec() }
    }

    fn text(content: &str) -> Node {
        node(NodeKind::Text { content: content.into(), emphasis: Emphasis::Normal }, &[])
    }

    #[test]
    fn an_ordinary_tree_validates_and_walks_in_order() {
        let panel = Panel::validate(
            vec![node(NodeKind::Column, &[1, 2]), text("first"), text("second")],
            0,
        )
        .unwrap();

        let root = panel.root().unwrap();
        assert_eq!(root.kind, NodeKind::Column);

        let labels: Vec<String> = panel
            .children(root)
            .map(|child| match &child.kind {
                NodeKind::Text { content, .. } => content.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(labels, ["first", "second"], "children keep the order they were given");
    }

    #[test]
    fn an_empty_arena_is_a_panel_that_draws_nothing() {
        let panel = Panel::validate(Vec::new(), 0).unwrap();
        assert!(panel.is_empty());
        assert!(panel.root().is_none());
    }

    #[test]
    fn a_cycle_is_refused_rather_than_drawn_forever() {
        // The node points at itself. Without the check this is an infinite
        // element tree and a window that never paints again.
        let err = Panel::validate(vec![node(NodeKind::Column, &[0])], 0).unwrap_err();
        assert_eq!(err, TreeError::NotATree(0));

        // And the two-node version of the same thing.
        let err = Panel::validate(
            vec![node(NodeKind::Column, &[1]), node(NodeKind::Column, &[0])],
            0,
        )
        .unwrap_err();
        assert_eq!(err, TreeError::NotATree(0));
    }

    #[test]
    fn a_shared_child_is_refused_because_sharing_multiplies() {
        // Both columns name node 2. Drawn naively that is one node appearing
        // twice; twenty layers of it is a million elements from a tiny arena.
        let err = Panel::validate(
            vec![
                node(NodeKind::Column, &[1, 3]),
                node(NodeKind::Row, &[2]),
                text("shared"),
                node(NodeKind::Row, &[2]),
            ],
            0,
        )
        .unwrap_err();
        assert_eq!(err, TreeError::NotATree(2));
    }

    #[test]
    fn an_index_past_the_end_is_refused() {
        let err = Panel::validate(vec![node(NodeKind::Column, &[7])], 0).unwrap_err();
        assert_eq!(err, TreeError::OutOfRange(7));

        let err = Panel::validate(vec![text("only")], 5).unwrap_err();
        assert_eq!(err, TreeError::OutOfRange(5));
    }

    #[test]
    fn a_tree_past_the_limits_is_refused() {
        let too_many: Vec<Node> = (0..MAX_NODES + 1).map(|_| text("x")).collect();
        assert_eq!(
            Panel::validate(too_many, 0).unwrap_err(),
            TreeError::TooBig(MAX_NODES + 1),
        );

        // A chain one deeper than allowed: every node holds the next.
        let deep: Vec<Node> = (0..=MAX_DEPTH)
            .map(|i| match i < MAX_DEPTH {
                true => node(NodeKind::Column, &[i as u32 + 1]),
                false => text("bottom"),
            })
            .collect();
        assert_eq!(Panel::validate(deep, 0).unwrap_err(), TreeError::TooDeep);
    }

    #[test]
    fn a_chain_at_exactly_the_limit_is_allowed() {
        // The boundary in the other direction, so the limit is a limit rather
        // than an off-by-one nobody notices until a panel stops rendering.
        let deep: Vec<Node> = (0..MAX_DEPTH)
            .map(|i| match i < MAX_DEPTH - 1 {
                true => node(NodeKind::Column, &[i as u32 + 1]),
                false => text("bottom"),
            })
            .collect();
        assert!(Panel::validate(deep, 0).is_ok());
    }

    #[test]
    fn a_node_nobody_points_at_is_harmless() {
        // Unreachable nodes are not an error: an extension building an arena
        // may well leave one behind, and it costs nothing to carry.
        let panel = Panel::validate(vec![text("drawn"), text("orphan")], 0).unwrap();
        assert_eq!(panel.len(), 2);
        assert!(panel.children(panel.root().unwrap()).next().is_none());
    }

    #[test]
    fn long_text_is_shortened_rather_than_refused() {
        let long = "a".repeat(MAX_TEXT * 2);
        let clamped = clamp(long);
        assert_eq!(clamped.chars().count(), MAX_TEXT + 1, "the limit, plus the ellipsis");
        assert!(clamped.ends_with('\u{2026}'));

        let short = "already fine".to_string();
        assert_eq!(clamp(short.clone()), short, "nothing is touched that fits");
    }

    #[test]
    fn clamping_cuts_on_a_character_and_not_a_byte() {
        // Truncating a multi-byte character in half panics `String::truncate`,
        // so the cut is found by character index rather than by byte count.
        let emoji = "\u{1F4E7}".repeat(MAX_TEXT + 10);
        let clamped = clamp(emoji);
        assert_eq!(clamped.chars().count(), MAX_TEXT + 1);
    }
}
