//! `spool.toml` — what an extension says about itself before any of it runs.
//!
//! The manifest exists so the host can answer three questions without
//! instantiating anything: what is this, what does it want, and what does it
//! contribute. That matters more than it sounds. It means the command palette
//! can list an extension's commands before the extension has ever been loaded,
//! it means the permission sheet can be shown before a single instruction
//! executes, and it means a broken extension is one bad file rather than a
//! crash inside somebody else's WebAssembly.
//!
//! Everything here is validated on the way in. A manifest that declares a panel
//! without asking for `ui` is rejected at load with that sentence, rather than
//! loading fine and drawing nothing for reasons the author has to guess at.

use std::{collections::BTreeSet, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

use crate::capability::{Capability, Grants};

/// The API version this host implements.
///
/// Matches the WIT package version — `spool:extension@0.2.0` — and is the
/// number a manifest's `api` field is checked against.
pub const HOST_API: ApiVersion = ApiVersion { major: 0, minor: 2 };

/// The file every extension is loaded from.
///
/// Fixed rather than named in the manifest, and that is a security decision
/// rather than a simplification: a manifest that could name its own entry point
/// is a manifest that can name `../../../something`, and the whole directory
/// would then have to be treated as a path to be sanitized instead of a name to
/// be joined.
pub const WASM_FILE: &str = "extension.wasm";

/// The file this module parses.
pub const MANIFEST_FILE: &str = "spool.toml";

/// A validated manifest.
///
/// Only constructed through [`Manifest::parse`], so holding one is proof that
/// the ids are well-formed, the api version is one this host speaks, and the
/// contributions line up with the capabilities that were asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: Id,
    pub name: String,
    pub version: String,
    pub api: ApiVersion,
    pub description: String,
    pub authors: Vec<String>,
    pub grants: Grants,
    hooks: BTreeSet<Hook>,
    pub commands: Vec<CommandDecl>,
    pub panels: Vec<PanelDecl>,
    pub config: Vec<ConfigDecl>,
    pub filters: Vec<FilterDecl>,
    /// See `Meta::badge_filters`.
    pub badge_filters: bool,
    pub scan: Option<ScanDecl>,
}

impl Manifest {
    /// Reads a manifest, or says why it will not.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest = toml::from_str(text).map_err(|e| ManifestError::Syntax(e.to_string()))?;
        let meta = raw.extension;

        if !HOST_API.supports(meta.api) {
            return Err(ManifestError::Api { wanted: meta.api, host: HOST_API });
        }

        let grants = Grants::new(meta.capabilities);

        // Contributions are checked against the grants here rather than when
        // they are first drawn, because an extension whose panel silently never
        // appears is one the author debugs by bisecting their own code.
        if !raw.panels.is_empty() && !grants.draws() {
            return Err(ManifestError::Ungranted {
                what: "a panel",
                capability: Capability::Ui,
            });
        }

        let hooks: BTreeSet<Hook> = meta.hooks.into_iter().collect();
        if hooks.contains(&Hook::Decorate) && !grants.draws() {
            return Err(ManifestError::Ungranted {
                what: "the `decorate` hook",
                capability: Capability::Ui,
            });
        }

        unique(raw.commands.iter().map(|c| &c.id), "command")?;
        unique(raw.panels.iter().map(|p| &p.id), "panel")?;
        unique(raw.config.iter().map(|c| &c.key), "setting")?;
        unique(raw.filters.iter().map(|f| &f.id), "filter")?;

        // A filter sorts the list by the badges this extension puts on rows,
        // so an extension that never decorates would contribute a filter that
        // matches nothing, ever.
        if (!raw.filters.is_empty() || meta.badge_filters) && !hooks.contains(&Hook::Decorate) {
            return Err(ManifestError::Unhooked { what: "a filter", hook: Hook::Decorate });
        }

        // The switch works by handing mail to `on-messages` — new mail as it
        // lands, and the backlog when it is turned on.
        if let Some(scan) = &raw.scan {
            if !hooks.contains(&Hook::Messages) {
                return Err(ManifestError::Unhooked { what: "a mailbox switch", hook: Hook::Messages });
            }
            if let Some(key) = &scan.since
                && !raw.config.iter().any(|c| c.key == *key)
            {
                return Err(ManifestError::Undeclared { what: "`scan.since`", key: key.to_string() });
            }
        }

        for decl in &raw.config {
            if decl.is_list() {
                unique(decl.fields.iter().map(|f| &f.key), "field")?;
                if decl.default.is_some() || !decl.choices.is_empty() || decl.secret {
                    return Err(ManifestError::List { key: decl.key.to_string() });
                }
                for row in &decl.rows {
                    if let Some(unknown) = row.keys().find(|k| !decl.fields.iter().any(|f| f.key == *k.as_str())) {
                        return Err(ManifestError::Undeclared {
                            what: "a default row",
                            key: unknown.clone(),
                        });
                    }
                }
            } else if !decl.rows.is_empty() {
                return Err(ManifestError::List { key: decl.key.to_string() });
            }
            if let Some(default) = &decl.default
                && !decl.allows(default)
            {
                return Err(ManifestError::Choice { key: decl.key.to_string(), value: default.clone() });
            }
        }

        // Config reaches an extension through its own key/value store, under
        // keys the host owns. An extension that declares settings and cannot
        // read them would be configured and never know it, so the capability
        // is required here rather than discovered at the first `store-get`.
        if !raw.config.is_empty() && !grants.has(&Capability::Storage) {
            return Err(ManifestError::Ungranted {
                what: "settings",
                capability: Capability::Storage,
            });
        }

        // A nameless extension is listed by its id rather than by a blank row,
        // which is at least something to click.
        let name = match meta.name.trim().is_empty() {
            true => meta.id.to_string(),
            false => meta.name,
        };

        Ok(Self {
            id: meta.id,
            name,
            version: meta.version,
            api: meta.api,
            description: meta.description,
            authors: meta.authors,
            grants,
            hooks,
            commands: raw.commands,
            panels: raw.panels,
            config: raw.config,
            filters: raw.filters,
            badge_filters: meta.badge_filters,
            scan: raw.scan,
        })
    }

    /// Whether the host should call this extension when mail lands.
    ///
    /// Declared rather than discovered, so an extension with no interest in new
    /// mail costs nothing during a sync — the host never instantiates it, let
    /// alone calls into it.
    pub fn wants_messages(&self) -> bool {
        self.hooks.contains(&Hook::Messages)
    }

    /// Whether the host should ask this extension to decorate messages.
    pub fn decorates(&self) -> bool {
        self.hooks.contains(&Hook::Decorate)
    }

    /// Whether anything about this extension has to be live before the user
    /// asks for it by name.
    ///
    /// An extension that only adds commands is loaded the first time one is
    /// run. One that watches mail or decorates it has to be running to do its
    /// job, so it starts with the app.
    pub fn starts_eagerly(&self) -> bool {
        self.wants_messages() || self.decorates()
    }

    /// The settings this extension declares, if any.
    pub fn settings(&self) -> &[ConfigDecl] {
        &self.config
    }

    /// One declared setting, by key.
    pub fn setting(&self, key: &str) -> Option<&ConfigDecl> {
        self.config.iter().find(|c| c.key == *key)
    }

    /// The command as the rest of the app names it: `unsubscribe.open`.
    ///
    /// Qualified, because two extensions will eventually both call something
    /// `open` and the palette has to be able to dispatch to the right one.
    pub fn qualified(&self, command: &str) -> String {
        format!("{}.{}", self.id, command)
    }
}

/// Splits `unsubscribe.open` back into the extension and its command.
///
/// The inverse of [`Manifest::qualified`], and the reason ids may not contain a
/// dot: the split has to be unambiguous from the left.
pub fn split_qualified(qualified: &str) -> Option<(&str, &str)> {
    qualified.split_once('.').filter(|(ext, cmd)| !ext.is_empty() && !cmd.is_empty())
}

fn unique<'a>(
    ids: impl Iterator<Item = &'a Id>,
    what: &'static str,
) -> Result<(), ManifestError> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(ManifestError::Duplicate { what, id: id.to_string() });
        }
    }
    Ok(())
}

/// The `[extension]` table.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    id: Id,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    api: ApiVersion,
    #[serde(default)]
    description: String,
    #[serde(default)]
    authors: Vec<String>,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default)]
    hooks: Vec<Hook>,
    /// Every badge that no `[[filters]]` entry claims is a filter too, named
    /// as the badge is — for an extension whose badges are not known until
    /// someone has configured them.
    #[serde(default, rename = "badge-filters")]
    badge_filters: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    extension: Meta,
    #[serde(default)]
    commands: Vec<CommandDecl>,
    #[serde(default)]
    panels: Vec<PanelDecl>,
    #[serde(default)]
    config: Vec<ConfigDecl>,
    #[serde(default)]
    filters: Vec<FilterDecl>,
    #[serde(default)]
    scan: Option<ScanDecl>,
}

/// A command the extension contributes to the palette and the keymap.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDecl {
    pub id: Id,
    /// What the palette shows. The extension's own name is shown beside it, so
    /// this does not need to repeat it.
    pub title: String,
    /// A default keystroke, in gpui's spelling: `cmd-shift-u`.
    ///
    /// Advisory. The host binds it only if nothing already has it, because an
    /// extension that could take `cmd-z` away from undo by being installed is
    /// an extension that breaks the app by existing.
    #[serde(default)]
    pub key: Option<String>,
}

/// A pane the extension contributes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelDecl {
    pub id: Id,
    pub title: String,
    /// A single character drawn in the sidebar beside the title.
    ///
    /// One glyph from the UI font rather than an image: the sidebar is chrome
    /// that is always on screen, and an icon set loaded per extension is a
    /// per-extension chance to make the whole window look wrong.
    #[serde(default)]
    pub glyph: Option<String>,
}

/// A setting the extension needs before it can do its job.
///
/// Declared rather than invented, because an extension has no way to ask for
/// one. WASI is linked empty — no environment, no filesystem — and the UI
/// vocabulary has no text field, so there is no surface inside the sandbox
/// through which an API key could ever arrive. The host reads `config.toml`
/// from the extension's own directory, checks it against this declaration, and
/// puts the values in the extension's key/value store before `init` runs.
///
/// Declaring a setting is also what makes it showable: the settings pane can
/// list what an extension wants, and mask what it says is secret, without
/// loading a single instruction of it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDecl {
    pub key: Id,
    /// What the settings pane labels the field.
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Whether the extension is useless without it.
    ///
    /// A missing required setting stops the extension loading, with a message
    /// naming the key and the file — rather than letting it start and fail on
    /// the first call in a way its author has to explain to every user.
    #[serde(default)]
    pub required: bool,
    /// Whether this is a credential.
    ///
    /// Masked in the settings pane and never written to the log. It does not
    /// make the value any safer inside the sandbox — an extension that can
    /// read its own key can send it anywhere its manifest lets it reach — and
    /// that is the honest extent of the promise.
    #[serde(default)]
    pub secret: bool,
    /// Used when `config.toml` does not name this key. A setting with one is
    /// never missing, whatever `required` says.
    #[serde(default)]
    pub default: Option<String>,
    /// The only values it may take, when there is a short list of them.
    ///
    /// Shown as a pick-one rather than a text field, and checked when
    /// `config.toml` is read: a value that is not one of them is refused with
    /// the list, rather than reaching an extension that cannot make sense of
    /// it.
    #[serde(default)]
    pub choices: Vec<String>,
    /// What each row holds, for a setting that is a list rather than a value
    /// — labels, rules, anything someone may want several of.
    ///
    /// A list reaches the extension as JSON: an array of objects, one per
    /// row, every field present and every value a string.
    #[serde(default)]
    pub fields: Vec<FieldDecl>,
    /// The rows a list starts with, until someone changes it.
    #[serde(default)]
    pub rows: Vec<std::collections::BTreeMap<String, String>>,
}

impl ConfigDecl {
    /// Whether `value` is one this setting may take.
    pub fn allows(&self, value: &str) -> bool {
        self.choices.is_empty() || self.choices.iter().any(|c| c == value)
    }

    /// Whether this setting is a list of rows.
    pub fn is_list(&self) -> bool {
        !self.fields.is_empty()
    }

    /// One row with every field at its default.
    pub fn blank_row(&self) -> std::collections::BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.key.to_string(), f.default.clone().unwrap_or_else(|| f.kind.empty().to_string())))
            .collect()
    }

    /// A row as given, filled out with defaults for the fields it leaves out.
    pub fn complete_row(
        &self,
        row: &std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, String> {
        let mut full = self.blank_row();
        for (key, value) in row {
            if full.contains_key(key) {
                full.insert(key.clone(), value.clone());
            }
        }
        full
    }

    /// The default rows, as the JSON an extension reads.
    pub fn default_rows(&self) -> String {
        let rows: Vec<_> = self.rows.iter().map(|row| self.complete_row(row)).collect();
        serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into())
    }
}

/// One column of a list setting.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldDecl {
    pub key: Id,
    pub title: String,
    #[serde(default)]
    pub kind: FieldKind,
    /// What a new row starts with.
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FieldKind {
    /// A line of text.
    #[default]
    Text,
    /// A longer passage, given the width of the row.
    Passage,
    /// On or off, as `"true"` or `"false"`.
    Switch,
}

impl FieldKind {
    fn empty(self) -> &'static str {
        match self {
            FieldKind::Switch => "false",
            _ => "",
        }
    }
}

/// One kind of message an extension can tell apart, as a filter over the list.
///
/// Matched against the badges the extension itself puts on rows, so the
/// filter and the badge cannot disagree about what a message is: a row is in
/// the filter exactly when it wears the badge. `badge` is the label, or its
/// first word when the label goes on to say more — `Spam` matches `Spam 3.4`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilterDecl {
    pub id: Id,
    /// What the chip above the list says.
    pub title: String,
    pub badge: String,
}

impl FilterDecl {
    pub fn matches(&self, label: &str) -> bool {
        label == self.badge
            || label.strip_prefix(self.badge.as_str()).is_some_and(|rest| rest.starts_with(' '))
    }
}

/// A switch, per mailbox, that the extension only works on the mail in.
///
/// Off everywhere to start with. Switched on, the host hands the extension
/// that mailbox's mail through `on-messages` — the backlog straight away, as
/// far back as `since` says, then new mail as it lands — and mail anywhere
/// else is not handed to it at all. The switch lives in the host, not the
/// extension: it is a promise about what is sent into the sandbox, and only
/// the side doing the sending can keep it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScanDecl {
    /// What the switch is called, shown on hover.
    pub title: String,
    /// A single character for the switch's button, like a panel's.
    #[serde(default)]
    pub glyph: Option<String>,
    /// The setting that says how far back switching on reaches, as a span
    /// such as `24h` or `7d`. With none, only new mail is handed over.
    #[serde(default)]
    pub since: Option<Id>,
}

/// The filter a badge falls in when no `[[filters]]` entry claims it: its
/// label, less a trailing number — `Spam 3.4` and `Spam 2.9` are one filter.
pub fn badge_filter_id(label: &str) -> String {
    let label = label.trim();
    let name = match label.rsplit_once(' ') {
        Some((name, last)) if last.parse::<f64>().is_ok() => name.trim(),
        _ => label,
    };
    format!("badge:{name}")
}

/// Seconds in a span written `24h`, `7d` or `2w`.
pub fn span_seconds(text: &str) -> Option<i64> {
    let text = text.trim();
    let unit = text.chars().last()?;
    let count: i64 = text[..text.len() - unit.len_utf8()].trim().parse().ok()?;
    let seconds = match unit {
        'h' => 3_600,
        'd' => 86_400,
        'w' => 7 * 86_400,
        _ => return None,
    };
    (count > 0).then(|| count * seconds)
}

/// An id: an extension's, a command's or a panel's.
///
/// Lowercase ascii, digits and hyphens. Narrow on purpose — an extension id is
/// also a directory name, a storage namespace and half of a qualified command,
/// and each of those is a reason not to accept a dot, a slash or a space.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(String);

impl Id {
    pub const MAX: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Id {
    type Err = ManifestError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = text.is_empty()
            || text.len() > Self::MAX
            || text.starts_with('-')
            || text.ends_with('-')
            || !text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');

        match bad {
            true => Err(ManifestError::Id(text.to_string())),
            false => Ok(Self(text.to_string())),
        }
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl PartialEq<str> for Id {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// Which of the host's calls an extension has asked to receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Hook {
    /// New mail, in batches, after it has been committed.
    Messages,
    /// Banners and badges for a message.
    Decorate,
}

impl Hook {
    /// As a manifest spells it.
    pub fn name(self) -> &'static str {
        match self {
            Hook::Messages => "messages",
            Hook::Decorate => "decorate",
        }
    }
}

/// The `major.minor` an extension was built against.
///
/// Only two numbers: a patch release of the API is by definition one an
/// extension cannot observe, so asking an author to name one would be asking
/// them to pin against something that does not affect them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiVersion {
    pub major: u16,
    pub minor: u16,
}

impl ApiVersion {
    /// Whether this host can run something built against `wanted`.
    ///
    /// Before 1.0 the minor is the breaking number — the same rule cargo
    /// applies to a `0.x` dependency — so 0.1 and 0.2 are different APIs rather
    /// than one that grew. After 1.0 a newer host runs anything built against
    /// an older minor of the same major, which is the promise that makes
    /// publishing an extension worth doing.
    pub fn supports(self, wanted: Self) -> bool {
        if self.major != wanted.major {
            return false;
        }
        match self.major {
            0 => self.minor == wanted.minor,
            _ => wanted.minor <= self.minor,
        }
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl FromStr for ApiVersion {
    type Err = ManifestError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let malformed = || ManifestError::ApiForm(text.to_string());
        let (major, minor) = text.trim().split_once('.').ok_or_else(malformed)?;
        Ok(Self {
            major: major.parse().map_err(|_| malformed())?,
            minor: minor.parse().map_err(|_| malformed())?,
        })
    }
}

impl<'de> Deserialize<'de> for ApiVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Syntax(String),
    Id(String),
    ApiForm(String),
    Api { wanted: ApiVersion, host: ApiVersion },
    Duplicate { what: &'static str, id: String },
    Ungranted { what: &'static str, capability: Capability },
    Unhooked { what: &'static str, hook: Hook },
    Undeclared { what: &'static str, key: String },
    Choice { key: String, value: String },
    List { key: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax(why) => write!(f, "{why}"),
            Self::Id(id) => write!(
                f,
                "`{id}` is not a usable id — lowercase letters, digits and hyphens, \
                 up to {} characters",
                Id::MAX
            ),
            Self::ApiForm(text) => {
                write!(f, "`{text}` is not an api version — write it as `\"{HOST_API}\"`")
            }
            Self::Api { wanted, host } => write!(
                f,
                "built against api {wanted}, and this copy of Spool implements {host}"
            ),
            Self::Duplicate { what, id } => write!(f, "two {what}s are both called `{id}`"),
            Self::Ungranted { what, capability } => write!(
                f,
                "declares {what} without asking for the `{capability}` capability"
            ),
            Self::Unhooked { what, hook } => {
                write!(f, "declares {what} without the `{}` hook", hook.name())
            }
            Self::Undeclared { what, key } => {
                write!(f, "{what} names `{key}`, which is not a declared setting")
            }
            Self::List { key } => write!(
                f,
                "`{key}` mixes a list's `fields` and `rows` with a single value's \
                 `default`, `choices` or `secret`"
            ),
            Self::Choice { key, value } => {
                write!(f, "the default for `{key}` is `{value}`, which is not one of its choices")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
        [extension]
        id = "unsubscribe"
        name = "Unsubscribe"
        version = "1.2.0"
        api = "0.2"
        description = "Offers a way out of a mailing list"
        authors = ["Someone"]
        capabilities = ["read-mail", "ui", "net:https:*.example.com"]
        hooks = ["decorate"]

        [[commands]]
        id = "open"
        title = "Unsubscribe from this sender"
        key = "cmd-shift-u"

        [[commands]]
        id = "list-all"
        title = "List every subscription"

        [[panels]]
        id = "subscriptions"
        title = "Subscriptions"
        glyph = "✉"
    "#;

    fn parse(text: &str) -> Manifest {
        Manifest::parse(text).expect("manifest should parse")
    }

    #[test]
    fn a_whole_manifest_reads_back_as_it_was_written() {
        let m = parse(GOOD);

        assert_eq!(m.id.as_str(), "unsubscribe");
        assert_eq!(m.name, "Unsubscribe");
        assert_eq!(m.api, ApiVersion { major: 0, minor: 2 });
        assert_eq!(m.commands.len(), 2);
        assert_eq!(m.commands[0].key.as_deref(), Some("cmd-shift-u"));
        assert_eq!(m.commands[1].key, None, "a command need not want a key");
        assert_eq!(m.panels[0].title, "Subscriptions");

        assert!(m.grants.reads_mail());
        assert!(m.grants.draws());
        assert!(m.grants.may_reach("lists.example.com"));
        assert!(!m.grants.modifies_mail(), "it never asked to file anything");
    }

    #[test]
    fn hooks_decide_whether_the_extension_starts_with_the_app() {
        let m = parse(GOOD);
        assert!(m.decorates());
        assert!(!m.wants_messages(), "it did not ask for new mail");
        assert!(m.starts_eagerly(), "something it declared has to be live");

        let quiet = parse(
            r#"
            [extension]
            id = "quiet"
            api = "0.2"

            [[commands]]
            id = "go"
            title = "Go"
        "#,
        );
        assert!(!quiet.starts_eagerly(), "commands are reason enough to load lazily");
    }

    #[test]
    fn a_command_is_named_by_its_extension_as_well_as_itself() {
        let m = parse(GOOD);
        assert_eq!(m.qualified("open"), "unsubscribe.open");
        assert_eq!(split_qualified("unsubscribe.open"), Some(("unsubscribe", "open")));
        assert_eq!(split_qualified("nodot"), None);
        assert_eq!(split_qualified(".open"), None);
        assert_eq!(split_qualified("unsubscribe."), None);
    }

    #[test]
    fn an_api_from_another_major_is_refused_rather_than_attempted() {
        let err = Manifest::parse(
            r#"
            [extension]
            id = "future"
            api = "9.0"
        "#,
        )
        .unwrap_err();

        assert!(matches!(err, ManifestError::Api { .. }), "{err}");
        assert!(err.to_string().contains("9.0"), "the error names the version it wanted");
    }

    #[test]
    fn before_one_point_zero_a_minor_bump_is_a_different_api() {
        // The rule cargo applies to a `0.x` dependency, applied here for the
        // same reason: while the API is still moving, a minor is where a
        // breaking change goes.
        let zero_one = ApiVersion { major: 0, minor: 1 };
        assert!(zero_one.supports(zero_one));
        assert!(!zero_one.supports(ApiVersion { major: 0, minor: 2 }));
        assert!(!ApiVersion { major: 0, minor: 2 }.supports(zero_one));

        // After 1.0 a host runs anything built against an older minor.
        let one_four = ApiVersion { major: 1, minor: 4 };
        assert!(one_four.supports(ApiVersion { major: 1, minor: 0 }));
        assert!(one_four.supports(ApiVersion { major: 1, minor: 4 }));
        assert!(!one_four.supports(ApiVersion { major: 1, minor: 5 }), "built against a newer host");
        assert!(!one_four.supports(ApiVersion { major: 2, minor: 0 }));
    }

    #[test]
    fn a_panel_without_the_ui_capability_is_refused_at_load() {
        let err = Manifest::parse(
            r#"
            [extension]
            id = "sneaky"
            api = "0.2"

            [[panels]]
            id = "p"
            title = "P"
        "#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            ManifestError::Ungranted { what: "a panel", capability: Capability::Ui },
        );
    }

    #[test]
    fn decorating_without_the_ui_capability_is_refused_too() {
        let err = Manifest::parse(
            r#"
            [extension]
            id = "sneaky"
            api = "0.2"
            hooks = ["decorate"]
        "#,
        )
        .unwrap_err();

        assert!(matches!(err, ManifestError::Ungranted { .. }), "{err}");
    }

    #[test]
    fn two_commands_with_one_id_are_a_manifest_error() {
        let err = Manifest::parse(
            r#"
            [extension]
            id = "clash"
            api = "0.2"

            [[commands]]
            id = "go"
            title = "One"

            [[commands]]
            id = "go"
            title = "Two"
        "#,
        )
        .unwrap_err();

        assert_eq!(err, ManifestError::Duplicate { what: "command", id: "go".into() });
    }

    #[test]
    fn ids_are_narrow_because_they_are_also_paths_and_namespaces() {
        for bad in [
            "",
            "Unsubscribe",
            "un subscribe",
            "un.subscribe",
            "../escape",
            "-leading",
            "trailing-",
            "un_subscribe",
        ] {
            assert!(bad.parse::<Id>().is_err(), "`{bad}` should not be a usable id");
        }
        for good in ["u", "unsubscribe", "un-subscribe", "spf9"] {
            assert!(good.parse::<Id>().is_ok(), "`{good}` should be a usable id");
        }
        assert!("x".repeat(Id::MAX).parse::<Id>().is_ok());
        assert!("x".repeat(Id::MAX + 1).parse::<Id>().is_err());
    }

    #[test]
    fn an_unknown_key_in_the_manifest_is_an_error() {
        // `deny_unknown_fields`, so a typo in a field name is caught at install
        // rather than becoming a setting that silently does nothing.
        let err = Manifest::parse(
            r#"
            [extension]
            id = "typo"
            api = "0.2"
            capabilties = ["ui"]
        "#,
        )
        .unwrap_err();

        assert!(matches!(err, ManifestError::Syntax(_)), "{err}");
    }

    #[test]
    fn an_unknown_capability_fails_the_whole_manifest() {
        let err = Manifest::parse(
            r#"
            [extension]
            id = "greedy"
            api = "0.2"
            capabilities = ["read-everything"]
        "#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("read-everything"), "{err}");
    }

    #[test]
    fn a_nameless_extension_is_listed_by_its_id() {
        let m = parse(
            r#"
            [extension]
            id = "nameless"
            api = "0.2"
        "#,
        );
        assert_eq!(m.name, "nameless");
    }

    const SCORER: &str = r#"
        [extension]
        id = "scorer"
        api = "0.2"
        capabilities = ["read-mail", "ui", "storage"]
        hooks = ["messages", "decorate"]

        [[config]]
        key = "lookback"
        title = "How far back"
        default = "7d"
        choices = ["24h", "7d", "30d", "365d"]

        [[filters]]
        id = "spam"
        title = "Spam"
        badge = "Spam"

        [[filters]]
        id = "clean"
        title = "Clean"
        badge = "\u2713"

        [scan]
        title = "Spam scoring"
        since = "lookback"
    "#;

    #[test]
    fn filters_and_a_switch_are_read() {
        let m = parse(SCORER);
        assert_eq!(m.filters.len(), 2);
        assert_eq!(m.scan.as_ref().unwrap().since.as_ref().unwrap().as_str(), "lookback");
        assert_eq!(m.setting("lookback").unwrap().choices.len(), 4);
    }

    #[test]
    fn a_filter_matches_its_badge_and_the_badge_with_more_to_say() {
        let m = parse(SCORER);
        let spam = &m.filters[0];
        assert!(spam.matches("Spam"));
        assert!(spam.matches("Spam 3.4"));
        assert!(!spam.matches("Spammy"), "a longer word is a different badge");
        assert!(!spam.matches("Bulk"));
        assert!(m.filters[1].matches("\u{2713}"));
    }

    #[test]
    fn a_filter_needs_the_decorate_hook_and_a_switch_needs_messages() {
        let err = Manifest::parse(&SCORER.replace(r#"hooks = ["messages", "decorate"]"#, r#"hooks = ["messages"]"#))
            .unwrap_err();
        assert!(err.to_string().contains("`decorate` hook"), "{err}");

        let err = Manifest::parse(&SCORER.replace(r#"hooks = ["messages", "decorate"]"#, r#"hooks = ["decorate"]"#))
            .unwrap_err();
        assert!(err.to_string().contains("`messages` hook"), "{err}");
    }

    #[test]
    fn a_switch_must_name_a_declared_setting() {
        let err = Manifest::parse(&SCORER.replace(r#"since = "lookback""#, r#"since = "horizon""#)).unwrap_err();
        assert!(err.to_string().contains("horizon"), "{err}");
    }

    #[test]
    fn a_default_must_be_one_of_the_choices() {
        let err = Manifest::parse(&SCORER.replace(r#"default = "7d""#, r#"default = "9d""#)).unwrap_err();
        assert!(err.to_string().contains("9d"), "{err}");
    }

    #[test]
    fn spans_are_hours_days_and_weeks() {
        assert_eq!(span_seconds("24h"), Some(86_400));
        assert_eq!(span_seconds("7d"), Some(7 * 86_400));
        assert_eq!(span_seconds(" 2w "), Some(14 * 86_400));
        assert_eq!(span_seconds("365d"), Some(365 * 86_400));
        assert_eq!(span_seconds("0d"), None);
        assert_eq!(span_seconds("7"), None);
        assert_eq!(span_seconds("d"), None);
        assert_eq!(span_seconds(""), None);
    }
}
