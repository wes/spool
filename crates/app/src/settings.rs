//! User preferences, stored as TOML next to the account config.
//!
//! Kept separate from `accounts.toml` on purpose: that file holds credentials
//! and is hand-edited, this one is written by the app every time you click
//! something. Mixing them would mean rewriting a file containing secrets in
//! response to a UI event.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::theme::ThemeId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub theme: ThemeId,
    /// Where the window was when you last quit. `None` until the first quit.
    pub window: Option<WindowState>,
    pub messages: MessageList,
    pub panes: PaneWidths,
    /// Whether the status bar is showing. Off by default: it is a readout, and
    /// a readout you did not ask for is a strip of numbers at the bottom of
    /// every window for the rest of time. View → Toggle Status Bar brings it
    /// back, and the choice is remembered.
    pub status_bar: bool,
    /// Whether the sidebar shows the "All Mail" row that selects every
    /// account at once.
    ///
    /// Off by default. It is a triage view, not a place to work: it stacks
    /// every account's mail into one list, and for someone with one account it
    /// is a second name for the same thing they already have. View → Show All
    /// Mail brings it back, and the choice is remembered.
    pub unified_inbox: bool,
    /// What the sidebar has been collapsed down to.
    pub sidebar: SidebarState,
    /// Whether an HTML message's remote images are fetched without being
    /// asked for.
    ///
    /// Off by default, and that is a privacy decision rather than a
    /// performance one: fetching an image from a sender's server tells them
    /// the moment their mail was opened, and from which address. A tracking
    /// pixel is a one-by-one transparent GIF that exists for nothing else.
    /// Turning this on says you would rather see mail as it was sent and
    /// accept that; leaving it off still lets any single message be loaded by
    /// hand.
    pub load_remote_images: bool,
    /// Senders whose images load without being asked for, as lowercase
    /// domains.
    ///
    /// The answer to the fact that "load images" is a question every
    /// newsletter asks every week. Saying yes to one sender for good is a far
    /// smaller decision than [`Settings::load_remote_images`], which says yes
    /// to everyone including the ones who have not written yet — and it is
    /// reversible from the message itself, which is where you find out you
    /// regret it.
    pub image_domains: Vec<String>,
    /// Point size of the text in a composer's body.
    ///
    /// Its own setting rather than the app's text size: writing is the one
    /// place where the size that suits you is not the size that suits a list
    /// of five hundred subjects. Adjusted with cmd-+ and cmd-- while a
    /// composer is in front, and remembered.
    pub compose_text_size: f32,
    /// The ChatGPT model the writing tools use, by its slug. `None` leaves it
    /// to the account's default, which is what changes when OpenAI retires a
    /// model; a picked one that is retired falls back to the same.
    pub writing_model: Option<String>,
    /// Which AI the AI features ask: the writing tools, the Priority sort and
    /// quick replies. ChatGPT unless changed, since that was the only one
    /// there was.
    pub ai_provider: spool_net::ai::Provider,
    /// The Claude model, by its id. `None` is `claude::DEFAULT_MODEL`.
    pub claude_model: Option<String>,
    /// Whether new inbox mail is sorted by whether it needs you, which sends
    /// it to the AI provider. Off until turned on in Settings → AI: it is the
    /// one AI feature that reads mail nobody asked it to.
    pub priority: bool,
    /// Whether mail sorted as wanting a reply gets replies written for it in
    /// the background, ready to send. Only while `priority` is on.
    pub quick_replies: bool,
    /// Whether mail you send is watched for an answer, and brought back with
    /// a follow-up ready when none comes. Off until turned on in Settings →
    /// AI, since deciding which sent mail expected an answer sends it to the
    /// AI provider.
    pub follow_ups: bool,
    /// The mailboxes each extension with a switch has been switched on for.
    pub scanning: Vec<Scanning>,
    /// Whether Clients — the client book and the board — has been added, in
    /// Settings → Extensions.
    ///
    /// Off by default, and off means absent rather than hidden: no menu item,
    /// no shortcut, no mark in the reader, and no people index kept as mail
    /// arrives. Somebody who never wanted a CRM has a mail client that is
    /// exactly what it was before there was one.
    pub clients: bool,
    /// Whether "No Thanks" was the answer to the sidebar's offer to make
    /// Spool the app that opens email links.
    ///
    /// Asked once. Somebody who keeps Mail for links has answered, and an
    /// offer that comes back every launch is a nag; Settings → Accounts keeps
    /// the button for a change of mind.
    pub default_mail_declined: bool,
    /// Whether the mailbox to open is the demo, chosen on the welcome screen.
    ///
    /// Remembered, so quitting does not quietly put somebody back in an empty
    /// window; the card at the foot of the sidebar is the way out. Written
    /// only while true, so a file from a copy that never tried the demo still
    /// loads in a version from before it existed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub demo: bool,
    /// A setting that no longer exists: the sidebar's sort order, replaced by
    /// arranging the top-level folders by hand.
    ///
    /// Kept as a field that is read and never written because
    /// `deny_unknown_fields` is what makes a mistyped key an error rather
    /// than a silent default — and the price of that is that a key we retire
    /// has to be retired here, or every file written by the version that had
    /// it stops loading.
    #[serde(default, skip_serializing, rename = "folder_sort")]
    retired_folder_sort: Option<String>,
}

/// Hand-written rather than derived, and the container's `#[serde(default)]`
/// is why it matters: a settings file written before a field existed takes
/// that field's value from here. A derived `Default` would give the composer
/// a text size of zero.
impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeId::default(),
            window: None,
            messages: MessageList::default(),
            panes: PaneWidths::default(),
            status_bar: false,
            unified_inbox: false,
            sidebar: SidebarState::default(),
            load_remote_images: false,
            image_domains: Vec::new(),
            compose_text_size: Self::COMPOSE_TEXT,
            writing_model: None,
            ai_provider: spool_net::ai::Provider::default(),
            claude_model: None,
            priority: false,
            quick_replies: true,
            follow_ups: false,
            scanning: Vec::new(),
            clients: false,
            default_mail_declined: false,
            demo: false,
            retired_folder_sort: None,
        }
    }
}

/// One extension switched on for one mailbox.
///
/// By address and server path rather than by the store's ids, like
/// [`CollapsedFolder`]: those are what survive the database being rebuilt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scanning {
    pub extension: String,
    pub account: String,
    pub path: String,
}

impl Settings {
    pub fn is_scanning(&self, extension: &str, email: &str, path: &str) -> bool {
        self.scanning
            .iter()
            .any(|s| s.extension == extension && s.account.eq_ignore_ascii_case(email) && s.path == path)
    }

    pub fn set_scanning(&mut self, extension: &str, email: &str, path: &str, on: bool) {
        self.scanning
            .retain(|s| !(s.extension == extension && s.account.eq_ignore_ascii_case(email) && s.path == path));
        if on {
            self.scanning.push(Scanning {
                extension: extension.to_string(),
                account: email.to_string(),
                path: path.to_string(),
            });
        }
    }

    /// What a composer's body starts at.
    pub const COMPOSE_TEXT: f32 = 13.;

    /// The range the body can be scaled to.
    ///
    /// The bottom is where a monospace face stops being legible and the top is
    /// where one line stops fitting in a window; outside either, the setting
    /// has stopped being an accessibility feature and become a way to break
    /// the composer.
    pub const COMPOSE_TEXT_RANGE: (f32, f32) = (9., 32.);

    /// Steps the composer's body size, clamped, and answers with the new one.
    ///
    /// One point per press. Two is faster to reach a size with and harder to
    /// land on the one you wanted.
    pub fn step_compose_text(&mut self, delta: f32) -> f32 {
        self.compose_text_size = clamp(self.compose_text_size + delta, Self::COMPOSE_TEXT_RANGE);
        self.compose_text_size
    }

    /// The size to draw at, whatever is stored.
    ///
    /// A hand-edited settings file can say anything, including `0` and `nan`,
    /// and neither should reach a text system.
    pub fn compose_text(&self) -> f32 {
        clamp(self.compose_text_size, Self::COMPOSE_TEXT_RANGE)
    }
}

/// Widths of the resizable panes, in logical points.
///
/// The list is stored per density because the two shapes genuinely want
/// different widths — detail gives each line the full width, slim has five
/// columns sharing one — and carrying a single number would make switching
/// density feel like the app forgot your last adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaneWidths {
    pub sidebar: f32,
    pub list_detail: f32,
    pub list_slim: f32,
}

impl Default for PaneWidths {
    fn default() -> Self {
        Self { sidebar: 214., list_detail: 380., list_slim: 560. }
    }
}

impl PaneWidths {
    pub const SIDEBAR_RANGE: (f32, f32) = (160., 440.);
    pub const LIST_RANGE: (f32, f32) = (280., 900.);

    /// The stored list width for a density.
    pub fn list(&self, density: Density) -> f32 {
        match density {
            Density::Slim => self.list_slim,
            Density::Detail => self.list_detail,
        }
    }

    pub fn set_list(&mut self, density: Density, width: f32) {
        let width = clamp(width, Self::LIST_RANGE);
        match density {
            Density::Slim => self.list_slim = width,
            Density::Detail => self.list_detail = width,
        }
    }

    pub fn set_sidebar(&mut self, width: f32) {
        self.sidebar = clamp(width, Self::SIDEBAR_RANGE);
    }

    /// Brings anything hand-edited or corrupt back into range.
    ///
    /// A width of zero or NaN in the file would otherwise produce a pane that
    /// cannot be seen or grabbed, with no way back short of deleting the file.
    pub fn sanitized(self) -> Self {
        Self {
            sidebar: clamp(self.sidebar, Self::SIDEBAR_RANGE),
            list_detail: clamp(self.list_detail, Self::LIST_RANGE),
            list_slim: clamp(self.list_slim, Self::LIST_RANGE),
        }
    }
}

fn clamp(value: f32, (low, high): (f32, f32)) -> f32 {
    if value.is_finite() { value.clamp(low, high) } else { (low + high) / 2. }
}

/// Height of one line of row text. Matches the theme's line height; stated
/// here because the row height is arithmetic on it.
const LINE: f32 = 18.;

/// How the message list draws its rows.
///
/// Which parts of the sidebar are folded away.
///
/// Remembered between launches: an account with forty folders is collapsed
/// precisely so it stops taking up the sidebar, and re-collapsing it every
/// morning would make the control worse than useless.
///
/// Identified by account address and server path rather than by row id, which
/// is a local number that a rebuilt database hands out again in a different
/// order. The cost is that a renamed folder forgets it was collapsed, which is
/// the right way round: an entry that outlives what it referred to is a folder
/// that mysteriously will not open.
// No `Eq`: the retired field holds whatever the old key contained, and a
// TOML value is only ever `PartialEq` — floats are in it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarState {
    /// Accounts showing no folders at all, by email address.
    pub collapsed_accounts: Vec<String>,
    /// Folders showing none of their children.
    pub collapsed_folders: Vec<CollapsedFolder>,
    /// The order the accounts have been dragged into, by address.
    ///
    /// An account the list does not name — one added since — follows the ones
    /// it does, in the order the store returns them.
    pub account_order: Vec<String>,
    /// A list that no longer exists: an arrangement of each account's
    /// top-level folders, from a version where those could be dragged. Read
    /// and never written, so a settings file that has it still loads. See
    /// [`Settings::retired_folder_sort`].
    #[serde(default, skip_serializing, rename = "folder_order")]
    retired_folder_order: Option<toml::Value>,
}

/// One folded-up folder: which account, and which path within it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollapsedFolder {
    pub account: String,
    pub path: String,
}

impl SidebarState {
    /// Where each account sits, by address, or empty until one is dragged.
    pub fn account_order(&self) -> &[String] {
        &self.account_order
    }

    /// Records a new arrangement of the accounts.
    pub fn set_account_order(&mut self, emails: Vec<String>) {
        self.account_order = emails;
    }

    pub fn account_collapsed(&self, email: &str) -> bool {
        self.collapsed_accounts.iter().any(|a| a.eq_ignore_ascii_case(email))
    }

    /// Flips one account, and reports what it is now.
    pub fn toggle_account(&mut self, email: &str) -> bool {
        match self.account_collapsed(email) {
            true => {
                self.collapsed_accounts.retain(|a| !a.eq_ignore_ascii_case(email));
                false
            }
            false => {
                self.collapsed_accounts.push(email.to_string());
                true
            }
        }
    }

    pub fn folder_collapsed(&self, email: &str, path: &str) -> bool {
        self.collapsed_folders
            .iter()
            .any(|f| f.account.eq_ignore_ascii_case(email) && f.path == path)
    }

    /// Flips one folder, and reports what it is now.
    pub fn toggle_folder(&mut self, email: &str, path: &str) -> bool {
        match self.folder_collapsed(email, path) {
            true => {
                self.collapsed_folders
                    .retain(|f| !(f.account.eq_ignore_ascii_case(email) && f.path == path));
                false
            }
            false => {
                self.collapsed_folders
                    .push(CollapsedFolder { account: email.to_string(), path: path.to_string() });
                true
            }
        }
    }
}

/// `Default` is hand-written below: the container's serde default fills a
/// missing field from it, and conversations default to on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessageList {
    pub density: Density,
    /// Which columns a slim row shows. Ignored in detail.
    pub columns: Columns,
    /// How much of the body a detail row previews. Ignored in slim, which
    /// never previews.
    pub preview: Preview,
    /// One row per conversation rather than per message.
    ///
    /// On by default, because a thread of twelve replies as twelve rows is
    /// twelve times the list for one exchange. Off is a real preference and
    /// not a fallback, which is why it is a setting rather than a mode you
    /// have to keep re-entering.
    pub conversations: bool,
}

impl Default for MessageList {
    fn default() -> Self {
        Self {
            density: Density::default(),
            columns: Columns::default(),
            preview: Preview::default(),
            conversations: true,
        }
    }
}

/// One row per message, or three lines per message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    /// A single line of columns. The default: it is what a mailbox of
    /// thousands wants, and it is what most mail clients open as.
    #[default]
    Slim,
    /// Sender, subject and a preview of the body.
    Detail,
}

impl MessageList {
    /// The fixed height of a row, which `uniform_list` needs before it can
    /// decide which rows are on screen.
    ///
    /// Derived rather than stored: it is a consequence of the density and the
    /// preview, and a stored copy would be one more thing to keep in step.
    pub fn row_height(&self) -> gpui::Pixels {
        match self.density {
            // One line, tight padding, and the separator.
            Density::Slim => gpui::px(LINE + 8. + 1.),
            // Sender, subject, then however many preview lines, plus 8px of
            // padding above and below and the separator.
            Density::Detail => {
                gpui::px((2. + self.preview.lines() as f32) * LINE + 16. + 1.)
            }
        }
    }
}

impl Density {
    pub const ALL: [Density; 2] = [Density::Slim, Density::Detail];

    pub fn label(self) -> &'static str {
        match self {
            Density::Slim => "Slim",
            Density::Detail => "Detail",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Density::Slim => "One line per message",
            Density::Detail => "Sender, subject and a preview",
        }
    }
}

/// How many lines of the body a detail row shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preview {
    /// No preview at all — sender and subject only.
    Hidden,
    #[default]
    OneLine,
    TwoLines,
}

impl Preview {
    pub const ALL: [Preview; 3] = [Preview::Hidden, Preview::OneLine, Preview::TwoLines];

    pub fn label(self) -> &'static str {
        match self {
            Preview::Hidden => "No preview",
            Preview::OneLine => "One line",
            Preview::TwoLines => "Two lines",
        }
    }

    /// Lines of body text to draw, which is also what the row height is
    /// measured from.
    pub fn lines(self) -> usize {
        match self {
            Preview::Hidden => 0,
            Preview::OneLine => 1,
            Preview::TwoLines => 2,
        }
    }
}

/// A column of the slim row.
///
/// The order here is the order on screen; the settings pane offers presence,
/// not arrangement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Unread,
    Flag,
    From,
    Subject,
    Date,
    Attachments,
}

impl Column {
    pub const ALL: [Column; 6] = [
        Column::Unread,
        Column::Flag,
        Column::From,
        Column::Subject,
        Column::Date,
        Column::Attachments,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Column::Unread => "Unread",
            Column::Flag => "Flag",
            Column::From => "From",
            Column::Subject => "Subject",
            Column::Date => "Date",
            Column::Attachments => "Attachments",
        }
    }
}

/// Which columns are shown.
///
/// A struct of flags rather than a list, so the file cannot describe a column
/// twice and the on-screen order is not something a config can get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Columns {
    pub unread: bool,
    pub flag: bool,
    pub from: bool,
    pub subject: bool,
    pub date: bool,
    pub attachments: bool,
}

impl Default for Columns {
    fn default() -> Self {
        Self { unread: true, flag: true, from: true, subject: true, date: true, attachments: true }
    }
}

impl Columns {
    pub fn shows(&self, column: Column) -> bool {
        match column {
            Column::Unread => self.unread,
            Column::Flag => self.flag,
            Column::From => self.from,
            Column::Subject => self.subject,
            Column::Date => self.date,
            Column::Attachments => self.attachments,
        }
    }

    fn set(&mut self, column: Column, on: bool) {
        let slot = match column {
            Column::Unread => &mut self.unread,
            Column::Flag => &mut self.flag,
            Column::From => &mut self.from,
            Column::Subject => &mut self.subject,
            Column::Date => &mut self.date,
            Column::Attachments => &mut self.attachments,
        };
        *slot = on;
    }

    pub fn count(&self) -> usize {
        Column::ALL.iter().filter(|c| self.shows(**c)).count()
    }

    /// Flips a column, refusing to turn off the last one.
    ///
    /// A row with no columns is a row with nothing in it, and a list of blank
    /// rows is not a state worth being able to reach by clicking.
    pub fn toggle(&mut self, column: Column) {
        if self.shows(column) && self.count() == 1 {
            return;
        }
        self.set(column, !self.shows(column));
    }

    /// Whether toggling this column would do anything.
    pub fn can_toggle(&self, column: Column) -> bool {
        !self.shows(column) || self.count() > 1
    }
}

/// Remembered window geometry.
///
/// Stored in logical points, which is what gpui works in, so moving the window
/// between displays of different scale factors restores the same apparent size
/// rather than the same pixel count.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Restored as maximized rather than at the stored rectangle, which is the
    /// size to come back to when you un-maximize.
    #[serde(default)]
    pub maximized: bool,
}

impl WindowState {
    /// Smallest window worth restoring. Anything under this is treated as
    /// junk — a minimized or mid-animation frame captured at quit — and the
    /// default placement is used instead.
    const MIN_SIDE: f32 = 400.;

    /// Whether the remembered rectangle still makes sense on this display.
    ///
    /// Monitors get unplugged and resolutions change. Restoring a window onto
    /// coordinates that no longer exist leaves someone with an app they can
    /// hear but cannot see, so the geometry has to earn its restoration: it
    /// must be a sane size and still substantially on screen.
    pub fn is_usable_on(&self, display: Bounds) -> bool {
        if !(self.width.is_finite() && self.height.is_finite())
            || !(self.x.is_finite() && self.y.is_finite())
        {
            return false;
        }
        if self.width < Self::MIN_SIDE || self.height < Self::MIN_SIDE {
            return false;
        }

        // Require a decent overlap with the display rather than mere
        // intersection, so a window one pixel onto the screen is not "usable".
        let overlap_w = (self.x + self.width).min(display.x + display.width) - self.x.max(display.x);
        let overlap_h =
            (self.y + self.height).min(display.y + display.height) - self.y.max(display.y);

        overlap_w > Self::MIN_SIDE / 2. && overlap_h > Self::MIN_SIDE / 2.
    }
}

/// A rectangle in logical points, free of any gpui types so that
/// [`WindowState`] stays testable without an app running.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Settings {
    pub fn path() -> PathBuf {
        spool_core::paths::config_dir().join("settings.toml")
    }

    /// Loads preferences, falling back to defaults for anything unreadable.
    ///
    /// A settings file is never worth failing to start over: a missing one is
    /// a first run, and a corrupt one is a reason to log and carry on, not to
    /// leave someone staring at a terminal instead of their mail.
    pub fn load() -> Self {
        let mut settings = Self::load_from(&Self::path()).unwrap_or_else(|e| {
            log::warn!("using default settings: {e:#}");
            Self::default()
        });
        settings.panes = settings.panes.sanitized();
        settings
    }

    fn load_from(path: &PathBuf) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &PathBuf) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let text = format!(
            "# Written by Spool. Edit it or delete it; both are fine.\n{}",
            toml::to_string_pretty(self).context("serializing settings")?
        );

        // Write-then-rename, so an interrupted save leaves the previous
        // settings intact rather than a half-written file.
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, text)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replacing {}", path.display()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("spool-settings-{}-{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_status_bar_starts_hidden_and_is_remembered() {
        assert!(!Settings::default().status_bar, "a readout nobody asked for");

        let dir = temp_dir("status-bar");
        let path = dir.join("settings.toml");
        let shown = Settings { status_bar: true, ..Default::default() };
        shown.save_to(&path).unwrap();
        assert!(Settings::load_from(&path).unwrap().status_bar);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn collapsing_is_per_account_and_survives_a_restart() {
        let mut sidebar = SidebarState::default();
        assert!(!sidebar.account_collapsed("me@example.com"), "everything starts open");

        assert!(sidebar.toggle_account("me@example.com"));
        assert!(sidebar.account_collapsed("ME@EXAMPLE.COM"), "an address is not case-sensitive");
        assert!(!sidebar.toggle_account("me@example.com"));
        assert!(sidebar.collapsed_accounts.is_empty(), "reopening leaves nothing behind");

        // Two accounts can hold the same folder path without sharing a state.
        sidebar.toggle_folder("me@example.com", "Lists");
        assert!(sidebar.folder_collapsed("me@example.com", "Lists"));
        assert!(!sidebar.folder_collapsed("other@example.com", "Lists"));
        assert!(!sidebar.folder_collapsed("me@example.com", "Lists/rust"), "not its children");

        let dir = temp_dir("sidebar-collapsed");
        let path = dir.join("settings.toml");
        Settings { sidebar: sidebar.clone(), ..Default::default() }.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap().sidebar, sidebar);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn all_mail_starts_hidden_and_is_remembered() {
        assert!(!Settings::default().unified_inbox, "a triage view nobody asked for");

        let dir = temp_dir("unified-inbox");
        let path = dir.join("settings.toml");
        let shown = Settings { unified_inbox: true, ..Default::default() };
        shown.save_to(&path).unwrap();
        assert!(Settings::load_from(&path).unwrap().unified_inbox);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_settings_file_written_before_the_status_bar_existed_still_loads() {
        // Every field added here has to be optional on the way in, or an
        // existing settings.toml stops the app on upgrade.
        let settings: Settings = toml::from_str("theme = \"dark\"").unwrap();
        assert!(!settings.status_bar);
        assert!(!settings.unified_inbox);
    }

    #[test]
    fn the_list_opens_slim_with_every_column() {
        let messages = MessageList::default();
        assert_eq!(messages.density, Density::Slim, "slim is the default view");
        assert_eq!(messages.columns.count(), Column::ALL.len());
        assert_eq!(messages.preview, Preview::OneLine);
    }

    #[test]
    fn a_slim_row_is_one_line_and_a_detail_row_grows_with_its_preview() {
        let slim = MessageList { density: Density::Slim, ..Default::default() };
        let detail = |preview| MessageList {
            density: Density::Detail,
            preview,
            ..Default::default()
        };

        assert!(slim.row_height() < detail(Preview::Hidden).row_height());
        assert!(
            detail(Preview::Hidden).row_height() < detail(Preview::OneLine).row_height(),
        );
        assert!(
            detail(Preview::OneLine).row_height() < detail(Preview::TwoLines).row_height(),
        );

        // The preview setting has no bearing on a row that has no preview.
        assert_eq!(
            MessageList { density: Density::Slim, preview: Preview::TwoLines, ..Default::default() }
                .row_height(),
            slim.row_height(),
        );
    }

    #[test]
    fn columns_toggle_but_the_last_one_cannot_be_turned_off() {
        let mut columns = Columns::default();

        columns.toggle(Column::From);
        assert!(!columns.shows(Column::From));
        columns.toggle(Column::From);
        assert!(columns.shows(Column::From));

        // Turn everything off but the subject.
        for column in [Column::Unread, Column::Flag, Column::From, Column::Date, Column::Attachments] {
            columns.toggle(column);
        }
        assert_eq!(columns.count(), 1);
        assert!(!columns.can_toggle(Column::Subject));

        // A row with no columns is a row with nothing in it.
        columns.toggle(Column::Subject);
        assert!(columns.shows(Column::Subject), "the last column stays");
        assert_eq!(columns.count(), 1);

        // Turning another back on releases the lock.
        columns.toggle(Column::Date);
        assert!(columns.can_toggle(Column::Subject));
    }

    #[test]
    fn images_are_blocked_until_asked_for() {
        // The default that matters: a fresh install must not tell every
        // sender the moment their mail was opened.
        assert!(!Settings::default().load_remote_images);
    }

    #[test]
    fn no_sender_is_trusted_with_images_until_one_is_named() {
        assert!(Settings::default().image_domains.is_empty());
    }

    #[test]
    fn the_demo_is_written_only_while_it_is_on() {
        let dir = temp_dir("demo");
        let path = dir.join("settings.toml");

        let mut settings = Settings::default();
        settings.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("demo"),
            "a copy that never tried the demo writes nothing a version before it would reject"
        );

        settings.demo = true;
        settings.save_to(&path).unwrap();
        assert!(Settings::load_from(&path).unwrap().demo);
    }

    #[test]
    fn trusted_senders_survive_a_round_trip_through_the_file() {
        let dir = temp_dir("image-domains");
        let path = dir.join("settings.toml");

        let mut settings = Settings::default();
        settings.image_domains = vec!["news.temu.com".into(), "mail.notion.so".into()];
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.image_domains, settings.image_domains);
    }

    #[test]
    fn a_settings_file_written_before_trusted_senders_existed_still_loads() {
        let dir = temp_dir("image-domains-missing");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "theme = \"dark\"\nload_remote_images = true\n").unwrap();

        let loaded = Settings::load_from(&path).expect("an older file still parses");
        assert!(loaded.image_domains.is_empty());
        assert!(loaded.load_remote_images);
    }

    #[test]
    fn a_settings_file_written_before_the_image_preference_blocks_images() {
        // The serde default fills the missing field from `Settings::default`,
        // and getting this one wrong would silently opt an existing reader
        // into being tracked.
        let dir = temp_dir("images-pref");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "theme = \"dark\"\nstatus_bar = true\n").unwrap();

        let loaded = Settings::load_from(&path).expect("an older file still parses");
        assert!(!loaded.load_remote_images);
        assert!(loaded.status_bar, "and the rest of the file still read");
    }

    #[test]
    fn the_composer_starts_at_thirteen_and_steps_one_point_at_a_time() {
        let mut settings = Settings::default();
        assert_eq!(settings.compose_text(), 13.);

        assert_eq!(settings.step_compose_text(1.), 14.);
        assert_eq!(settings.step_compose_text(-1.), 13.);
    }

    #[test]
    fn the_composer_size_stops_where_it_stops_being_useful() {
        let mut settings = Settings::default();
        for _ in 0..100 {
            settings.step_compose_text(1.);
        }
        assert_eq!(settings.compose_text(), Settings::COMPOSE_TEXT_RANGE.1);

        for _ in 0..200 {
            settings.step_compose_text(-1.);
        }
        assert_eq!(settings.compose_text(), Settings::COMPOSE_TEXT_RANGE.0);
    }

    #[test]
    fn a_settings_file_written_before_the_composer_size_existed_reads_as_thirteen() {
        // The reason `Default` is hand-written: the container's serde default
        // fills a missing field from it, and a derived one would hand the
        // text system a size of zero.
        let dir = temp_dir("compose-size");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "theme = \"light\"\nstatus_bar = true\n").unwrap();

        let loaded = Settings::load_from(&path).expect("an older file still parses");
        assert_eq!(loaded.compose_text(), Settings::COMPOSE_TEXT);
        assert_eq!(loaded.theme, ThemeId::Light, "and the rest of the file still read");
    }

    #[test]
    fn a_nonsense_size_in_the_file_does_not_reach_the_text_system() {
        let broken = Settings { compose_text_size: 0., ..Default::default() };
        assert!(broken.compose_text() >= Settings::COMPOSE_TEXT_RANGE.0);

        let worse = Settings { compose_text_size: f32::NAN, ..Default::default() };
        assert!(worse.compose_text().is_finite());
    }

    #[test]
    fn list_settings_round_trip_through_disk() {
        let dir = temp_dir("messages");
        let path = dir.join("settings.toml");

        let settings = Settings {
            panes: PaneWidths::default(),
            theme: ThemeId::Light,
            status_bar: false,
            window: None,
            messages: MessageList {
                density: Density::Detail,
                preview: Preview::TwoLines,
                columns: Columns { attachments: false, ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("detail"), "{text}");
        assert!(text.contains("two-lines"), "{text}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settings_file_written_before_this_existed_still_loads() {
        // Upgrading must not turn someone's settings into a parse error, and
        // must land them on the new default rather than on nothing. The theme
        // named here is a retired one, which is the sharpest version of the
        // problem: the rest of the file has to survive it.
        let dir = temp_dir("older");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "theme = \"dracula\"\n[window]\nx = 10.0\ny = 20.0\n\
             width = 900.0\nheight = 600.0\nmaximized = false\n")
            .unwrap();

        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.theme, ThemeId::Dark, "a retired theme reads as Dark");
        assert_eq!(settings.window.unwrap().width, 900., "the rest of the file survives");
        assert_eq!(settings.messages, MessageList::default());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_partial_messages_table_keeps_the_defaults_for_the_rest() {
        let dir = temp_dir("partial");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "[messages]\ndensity = \"detail\"\n").unwrap();

        let settings = Settings::load_from(&path).unwrap();
        assert_eq!(settings.messages.density, Density::Detail);
        assert_eq!(settings.messages.columns, Columns::default());
        assert_eq!(settings.messages.preview, Preview::OneLine);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pane_widths_clamp_to_something_grabbable() {
        let absurd = PaneWidths { sidebar: 0., list_detail: 5_000., list_slim: f32::NAN };
        let fixed = absurd.sanitized();

        assert_eq!(fixed.sidebar, PaneWidths::SIDEBAR_RANGE.0);
        assert_eq!(fixed.list_detail, PaneWidths::LIST_RANGE.1);
        assert!(fixed.list_slim.is_finite(), "NaN must not survive into a layout");
    }

    #[test]
    fn each_density_keeps_its_own_list_width() {
        let mut panes = PaneWidths::default();
        panes.set_list(Density::Detail, 420.);

        assert_eq!(panes.list(Density::Detail), 420.);
        assert_eq!(
            panes.list(Density::Slim),
            PaneWidths::default().list_slim,
            "resizing one density must not disturb the other",
        );
    }

    #[test]
    fn setting_a_width_clamps_rather_than_accepting_it() {
        let mut panes = PaneWidths::default();
        panes.set_sidebar(10_000.);
        assert_eq!(panes.sidebar, PaneWidths::SIDEBAR_RANGE.1);

        panes.set_list(Density::Slim, -50.);
        assert_eq!(panes.list_slim, PaneWidths::LIST_RANGE.0);
    }

    #[test]
    fn defaults_to_the_dark_theme_and_no_remembered_window() {
        assert_eq!(Settings::default().theme, ThemeId::Dark);
        assert_eq!(Settings::default().window, None);
    }

    fn display() -> Bounds {
        Bounds { x: 0., y: 0., width: 2056., height: 1329. }
    }

    fn window(x: f32, y: f32, width: f32, height: f32) -> WindowState {
        WindowState { x, y, width, height, maximized: false }
    }

    #[test]
    fn a_window_within_the_display_is_restored() {
        assert!(window(100., 80., 1400., 900.).is_usable_on(display()));
    }

    #[test]
    fn a_window_hanging_off_the_edge_is_still_restored_if_mostly_visible() {
        // Dragged partly off the right: plenty of it is still reachable.
        assert!(window(1800., 80., 1400., 900.).is_usable_on(display()));
    }

    #[test]
    fn a_window_on_a_display_that_is_gone_is_rejected() {
        // Saved on a second monitor to the right that is no longer attached.
        assert!(!window(3000., 200., 1400., 900.).is_usable_on(display()));
        // Or above the display, as happens when a monitor arrangement changes.
        assert!(!window(200., -1200., 1400., 900.).is_usable_on(display()));
    }

    #[test]
    fn an_absurdly_small_window_is_rejected() {
        assert!(!window(100., 100., 120., 90.).is_usable_on(display()));
        assert!(!window(100., 100., 1400., 10.).is_usable_on(display()));
    }

    #[test]
    fn non_finite_geometry_is_rejected() {
        assert!(!window(f32::NAN, 0., 1400., 900.).is_usable_on(display()));
        assert!(!window(0., 0., f32::INFINITY, 900.).is_usable_on(display()));
    }

    #[test]
    fn window_geometry_round_trips_through_disk() {
        let dir = temp_dir("window");
        let path = dir.join("settings.toml");

        let settings = Settings {
            panes: PaneWidths::default(),
            theme: ThemeId::Light,
            status_bar: false,
            messages: MessageList::default(),
            window: Some(WindowState {
                x: 120.5,
                y: 64.,
                width: 1400.,
                height: 900.,
                maximized: true,
            }),
            ..Default::default()
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settings_file_without_a_window_table_still_loads() {
        let dir = temp_dir("no-window");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "theme = \"light\"\n").unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.theme, ThemeId::Light);
        assert_eq!(loaded.window, None, "an older file is not an error");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_yields_defaults() {
        let path = temp_dir("missing").join("nope.toml");
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let dir = temp_dir("round-trip");
        let path = dir.join("settings.toml");

        for id in ThemeId::ALL {
            let settings = Settings { theme: id, ..Default::default() };
            settings.save_to(&path).unwrap();
            assert_eq!(Settings::load_from(&path).unwrap(), settings);
        }

        // A plain value declared after tables, which the writer must reorder.
        let settings = Settings { writing_model: Some("gpt-9".into()), ..Default::default() };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);

        // The AI choices, which a file from before they existed has none of.
        let settings = Settings {
            ai_provider: spool_net::ai::Provider::Claude,
            claude_model: Some("claude-haiku-5-5".into()),
            priority: true,
            quick_replies: false,
            follow_ups: true,
            ..Default::default()
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dragged_account_arrangement_survives_a_restart() {
        let dir = temp_dir("account-order");
        let path = dir.join("settings.toml");

        let mut sidebar = SidebarState::default();
        sidebar.set_account_order(vec!["b@example.com".into(), "a@example.com".into()]);
        let settings = Settings { sidebar, ..Default::default() };
        settings.save_to(&path).unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert_eq!(loaded.sidebar.account_order(), ["b@example.com", "a@example.com"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settings_file_written_before_arrangements_existed_still_loads() {
        let dir = temp_dir("account-order-missing");
        let path = dir.join("settings.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "theme = \"dark\"\n").unwrap();

        assert!(Settings::load_from(&path).unwrap().sidebar.account_order().is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_settings_file_naming_a_retired_folder_arrangement_still_loads() {
        // Written by the version where folders themselves could be dragged.
        let dir = temp_dir("retired-folder-order");
        let path = dir.join("settings.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "theme = \"dark\"\n\n[[sidebar.folder_order]]\naccount = \"a@example.com\"\n\
             paths = [\"INBOX\"]\n",
        )
        .unwrap();

        let loaded = Settings::load_from(&path).unwrap();
        assert!(loaded.sidebar.account_order().is_empty());

        loaded.save_to(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("folder_order"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_creates_missing_directories() {
        let dir = temp_dir("nested");
        let path = dir.join("a/b/settings.toml");

        Settings { theme: ThemeId::Light, ..Default::default() }.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap().theme, ThemeId::Light);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn saving_leaves_no_temporary_file_behind() {
        let dir = temp_dir("atomic");
        let path = dir.join("settings.toml");

        Settings { theme: ThemeId::Light, ..Default::default() }.save_to(&path).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_file_is_all_defaults() {
        let dir = temp_dir("empty");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "").unwrap();

        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_corrupt_file_is_an_error_rather_than_a_silent_default() {
        let dir = temp_dir("corrupt");
        let path = dir.join("settings.toml");
        std::fs::write(&path, "theme = \"not-a-theme\"").unwrap();

        // `load_from` reports it; `load` is the one that decides to carry on,
        // so a typo in the file is visible in the log rather than silently
        // reverting someone's choice with no explanation.
        assert!(Settings::load_from(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_written_file_is_commented_and_readable() {
        let dir = temp_dir("comment");
        let path = dir.join("settings.toml");
        Settings { theme: ThemeId::Light, ..Default::default() }.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with('#'), "{text}");
        assert!(text.contains("light"), "{text}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
