//! Account configuration.
//!
//! Credentials are the part of a mail client people are right to be nervous
//! about, so this file never holds one. It holds *how to get* one: a keychain
//! entry the app wrote, a shell command, or an environment variable. A literal
//! password is still accepted, because sometimes you are just testing, but it
//! is the last option rather than the first.
//!
//! Two ways in are supported. A **password** account is the standard
//! IMAP/SMTP arrangement, where the secret is a password or an app-specific
//! password. A **Google OAuth** account holds a refresh token instead, because
//! Gmail stopped accepting passwords for most accounts; the token is traded
//! for a short-lived bearer token at sync time.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::oauth::{self, ClientCredentials};
use crate::secrets::{self, Slot};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<AccountConfig>,
}

/// How a connection is protected.
///
/// Named after what the user picks in every other mail client rather than
/// after the mechanism, because "SSL/TLS on 993 or STARTTLS on 143" is the
/// question the settings of their old client already asked them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// TLS from the first byte: IMAP 993, SMTP 465.
    #[default]
    Tls,
    /// Plaintext, then upgraded in-band: IMAP 143, SMTP 587.
    #[serde(alias = "start-tls", alias = "startTls")]
    Starttls,
    /// No encryption at all. Only sane against a server on this machine.
    #[serde(alias = "none")]
    Plaintext,
}

impl Security {
    pub const ALL: [Security; 3] = [Security::Tls, Security::Starttls, Security::Plaintext];

    pub fn label(self) -> &'static str {
        match self {
            Security::Tls => "SSL/TLS",
            Security::Starttls => "STARTTLS",
            Security::Plaintext => "None",
        }
    }
}

/// How the connection proves who you are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// `LOGIN` / `AUTH PLAIN` with a password or an app-specific password.
    #[default]
    Password,
    /// `XOAUTH2` with a bearer token from Google.
    GoogleOauth,
}

impl Auth {
    pub const ALL: [Auth; 2] = [Auth::Password, Auth::GoogleOauth];

    pub fn label(self) -> &'static str {
        match self {
            Auth::Password => "Password",
            Auth::GoogleOauth => "Sign in with Google",
        }
    }
}

/// A resolved credential, ready to authenticate with.
///
/// Deliberately not `String`: the two are used with different commands, and a
/// bearer token sent as a password fails with an error that blames the
/// password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    Password(String),
    Bearer(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
    /// Display name in the sidebar, e.g. `Work`.
    pub name: String,
    pub email: String,

    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    /// Login name, when it differs from the address.
    #[serde(default)]
    pub imap_user: Option<String>,
    #[serde(default)]
    pub imap_security: Security,

    #[serde(default)]
    pub smtp_host: Option<String>,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_user: Option<String>,
    #[serde(default)]
    pub smtp_security: Security,

    #[serde(default)]
    pub auth: Auth,

    /// Shell command whose stdout is the password:
    /// `security find-generic-password -s spool -a me@example.com -w`
    #[serde(default)]
    pub password_command: Option<String>,
    /// Environment variable holding the password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// A literal password. Discouraged; it puts a secret in a plaintext file.
    #[serde(default)]
    pub password: Option<String>,

    /// Folders to sync. Empty means every folder the server lists.
    #[serde(default)]
    pub folders: Vec<String>,

    /// How many of the newest messages to keep per folder. `0` means all of
    /// them, and is the default.
    ///
    /// Sync works backwards from the newest until the folder is complete, so
    /// this is a ceiling on the local copy rather than a page size — nothing
    /// asks to see more, and there is no point at which the list stops and
    /// waits. A number here caps a folder you do not want the whole of; a
    /// hundred-thousand-message archive is the case it exists for.
    #[serde(default = "default_initial_limit")]
    pub initial_limit: usize,

    /// How many IMAP connections a sync may open to this server at once.
    ///
    /// Folders are fetched over these in parallel, which is most of what
    /// makes a first sync of a dozen-folder account bearable: the work is
    /// waiting on round trips, and waiting on four at once takes a quarter of
    /// the time. Modest by default and clamped either way, because the
    /// ceiling is the server's: Gmail allows fifteen simultaneous connections
    /// per account, a small Dovecot box rather fewer, and an account locked
    /// out for opening too many is a worse outcome than a slower sync.
    #[serde(default = "default_sync_connections")]
    pub sync_connections: usize,

    /// Index into the UI accent palette.
    #[serde(default)]
    pub color: u8,

    /// Signed off with, on every message sent from this account.
    ///
    /// Stored per account rather than globally because the whole point of
    /// having two accounts is that they are two different people.
    #[serde(default)]
    pub signature: String,
}

fn default_imap_port() -> u16 {
    993
}
fn default_smtp_port() -> u16 {
    465
}
fn default_initial_limit() -> usize {
    0
}
fn default_sync_connections() -> usize {
    4
}

/// The most connections a sync will open to one server, whatever the file
/// says.
///
/// Past a handful the round trips are no longer what the sync is waiting on,
/// and the risk stops being a slow sync and starts being the server refusing
/// the account.
pub const MAX_SYNC_CONNECTIONS: usize = 8;

impl Default for AccountConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            email: String::new(),
            imap_host: String::new(),
            imap_port: default_imap_port(),
            imap_user: None,
            imap_security: Security::Tls,
            smtp_host: None,
            smtp_port: default_smtp_port(),
            smtp_user: None,
            smtp_security: Security::Tls,
            auth: Auth::Password,
            password_command: None,
            password_env: None,
            password: None,
            folders: Vec::new(),
            initial_limit: default_initial_limit(),
            sync_connections: default_sync_connections(),
            color: 0,
            signature: String::new(),
        }
    }
}

impl Config {
    /// `~/.config/spool/accounts.toml`, or the platform equivalent.
    pub fn default_path() -> PathBuf {
        spool_core::paths::config_dir().join("accounts.toml")
    }

    /// Loads the config, treating a missing file as "no accounts yet" rather
    /// than an error — a fresh install should open, not fail.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(Self::default_path())
    }

    /// Rewrites the file.
    ///
    /// This does not preserve comments, which is the price of the settings
    /// pane being able to edit accounts at all. The header says so, so nobody
    /// discovers it by losing something.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let text = format!(
            "# Spool accounts.\n\
             #\n\
             # Editable by hand, but the settings pane rewrites this file when\n\
             # you change an account, and rewriting drops comments. Passwords\n\
             # and OAuth tokens are not here; they are in the keychain.\n\n{}",
            toml::to_string_pretty(self).context("serializing accounts")?
        );

        // Write-then-rename, so an interrupted save leaves the previous
        // accounts intact rather than a half-written file.
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, text)
            .with_context(|| format!("writing {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }

    /// Replaces the account at `index`, or appends when it is `None`.
    ///
    /// One method for both so the settings pane does not need to know whether
    /// it is adding or editing anywhere except in its own title bar.
    pub fn upsert(&mut self, index: Option<usize>, account: AccountConfig) {
        match index.and_then(|i| self.accounts.get_mut(i)) {
            Some(slot) => *slot = account,
            None => self.accounts.push(account),
        }
    }

    /// Removes an account and forgets its secrets.
    ///
    /// Removing the entry without the secrets would leave a keychain full of
    /// passwords for accounts that no longer exist.
    pub fn remove(&mut self, index: usize) -> Option<AccountConfig> {
        if index >= self.accounts.len() {
            return None;
        }
        let account = self.accounts.remove(index);
        oauth::forget(&account.email);
        secrets::forget_all(&account.email);
        Some(account)
    }

    /// Written out when the user asks how to configure an account.
    pub fn example() -> &'static str {
        r#"# ~/.config/spool/accounts.toml
#
# One [[accounts]] block per mailbox. Most people should use the settings pane
# (cmd-,) instead of this file; it writes the same thing and puts the password
# in your keychain.
#
# Passwords are looked up, not stored. In order of preference:
#   1. nothing at all — the settings pane put it in the keychain
#   2. password_command, which can read any keychain or password manager
#   3. password_env
#   4. password, which puts a secret in a plaintext file
#
# On macOS, to fill the keychain by hand:
#   security add-generic-password -s spool -a me@example.com -w
#
# Most providers reject your normal login password over IMAP and want an
# app-specific password instead. Gmail is the exception worth knowing: set
# auth = "google-oauth" and sign in from the settings pane, and there is no
# password to manage at all.

[[accounts]]
name = "Work"
email = "me@example.com"
auth = "password"

imap_host = "imap.example.com"
imap_port = 993
imap_security = "tls"       # tls | starttls | plaintext

smtp_host = "smtp.example.com"
smtp_port = 465
smtp_security = "tls"

password_command = "security find-generic-password -s spool -a me@example.com -w"
# Or, for a quick test:
# password_env = "SPOOL_PASSWORD"

# Sync everything the server lists, or name folders explicitly:
# folders = ["INBOX", "Sent", "Archive"]

# 0 keeps everything the folder holds. Set a number to cap a folder you do
# not want all of.
initial_limit = 0

# How many connections a sync may open to this server at once. Folders are
# fetched over all of them, which is most of what makes a first sync bearable.
# Lower it if the server complains about too many connections; 1 syncs the
# folders one after another, as this used to.
sync_connections = 4

color = 0

# A Gmail account. Nothing to configure beyond the servers: open Settings >
# Accounts and press "Sign in with Google". The refresh token that comes back
# lives in the keychain, never here.
#
# [[accounts]]
# name = "Gmail"
# email = "me@gmail.com"
# auth = "google-oauth"
# imap_host = "imap.gmail.com"
# smtp_host = "smtp.gmail.com"
"#
    }
}

impl AccountConfig {
    /// The IMAP login name.
    pub fn user(&self) -> &str {
        self.imap_user.as_deref().filter(|u| !u.is_empty()).unwrap_or(&self.email)
    }

    /// How many connections a sync may open to this account, as a number that
    /// can actually be used.
    ///
    /// Clamped rather than validated: a `0` in the file means "the person
    /// wanted this off", which is one connection, and a `40` means they were
    /// optimistic about what the server will tolerate. Neither is worth
    /// refusing to sync over.
    pub fn connections(&self) -> usize {
        self.sync_connections.clamp(1, MAX_SYNC_CONNECTIONS)
    }

    /// The SMTP login name, which falls back to the IMAP one — the same
    /// credential works for both on every provider worth naming.
    pub fn smtp_login(&self) -> &str {
        self.smtp_user.as_deref().filter(|u| !u.is_empty()).unwrap_or_else(|| self.user())
    }

    /// Problems that would make this account fail, in the order they appear in
    /// the form.
    ///
    /// Returned as a list rather than a `Result` so the pane can mark every
    /// bad field at once instead of revealing them one save at a time.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.name.trim().is_empty() {
            problems.push("Give the account a name.".into());
        }
        if self.email.trim().is_empty() {
            problems.push("Enter an email address.".into());
        } else if !is_plausible_address(&self.email) {
            problems.push(format!("{} does not look like an email address.", self.email));
        }
        if self.imap_host.trim().is_empty() {
            problems.push("Enter an IMAP server.".into());
        }
        if self.imap_port == 0 {
            problems.push("The IMAP port must be between 1 and 65535.".into());
        }
        if self.smtp_host.as_deref().is_some_and(|h| !h.trim().is_empty()) && self.smtp_port == 0 {
            problems.push("The SMTP port must be between 1 and 65535.".into());
        }
        problems
    }

    /// Whether this account's own submission server files sent mail for us.
    ///
    /// Gmail does, for anything submitted through `smtp.gmail.com`: the
    /// message lands in `[Gmail]/Sent Mail` without being asked, and appending
    /// a copy as well produces two. Keyed on the submission host rather than
    /// on the provider, because a Gmail address relaying through somebody
    /// else's SMTP gets no such favour and does need the copy.
    pub fn files_sent_mail_itself(&self) -> bool {
        let host = self.smtp_host.as_deref().unwrap_or_default().trim().to_ascii_lowercase();
        host == "smtp.gmail.com" || host == "smtp.googlemail.com"
    }

    /// The OAuth client Spool itself signs in with.
    ///
    /// A property of the build rather than of the account: every Google
    /// account here goes through the same registered client, which is what
    /// makes signing in one button instead of a trip to a cloud console.
    pub fn oauth_client(&self) -> Result<ClientCredentials> {
        ClientCredentials::bundled()
    }

    /// Whether a credential is available without going to the network.
    ///
    /// Used to mark an account as needing attention in the list, so a revoked
    /// sign-in is visible before the next sync fails.
    pub fn has_stored_credential(&self) -> bool {
        match self.auth {
            Auth::Password => {
                self.password_command.is_some()
                    || self.password_env.is_some()
                    || self.password.is_some()
                    || matches!(secrets::load(Slot::Password, &self.email), Ok(Some(_)))
            }
            Auth::GoogleOauth => {
                matches!(secrets::load(Slot::RefreshToken, &self.email), Ok(Some(_)))
            }
        }
    }

    /// Resolves whatever this account authenticates with.
    ///
    /// Async because the OAuth path may have to go to Google for a fresh
    /// bearer token. The password path shells out and blocks, so both must run
    /// off the UI thread regardless.
    pub async fn credentials(&self) -> Result<Credentials> {
        match self.auth {
            Auth::Password => Ok(Credentials::Password(self.resolve_password()?)),
            Auth::GoogleOauth => {
                let client = self.oauth_client()?;
                let refresh_token = secrets::load(Slot::RefreshToken, &self.email)?
                    .with_context(|| {
                        format!(
                            "{} is not signed in — open Settings \u{203a} Accounts and \
                             sign in with Google",
                            self.email
                        )
                    })?;

                let token = oauth::access_token(&self.email, &client, &refresh_token).await?;
                Ok(Credentials::Bearer(token))
            }
        }
    }

    /// Resolves the password, in order of preference.
    ///
    /// The keychain comes last, not because it is the worst but because it is
    /// the default: an explicit `password_command` in the file is someone
    /// saying where they want the password to come from, and that should win
    /// over whatever the app happened to store.
    pub fn resolve_password(&self) -> Result<String> {
        if let Some(command) = &self.password_command {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .with_context(|| format!("running password_command for {}", self.email))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "password_command for {} exited with {}: {}",
                    self.email,
                    output.status,
                    stderr.trim()
                );
            }
            let password = String::from_utf8(output.stdout)
                .context("password_command produced non-UTF-8 output")?;
            // Keychain tools terminate with a newline; sending it as part of
            // the password produces a baffling authentication failure.
            return Ok(password.trim_end_matches(['\n', '\r']).to_string());
        }

        if let Some(var) = &self.password_env {
            return std::env::var(var)
                .with_context(|| format!("environment variable {var} is not set"));
        }

        if let Some(password) = &self.password {
            return Ok(password.clone());
        }

        if let Some(password) = secrets::load(Slot::Password, &self.email)? {
            return Ok(password);
        }

        bail!(
            "no password stored for {} — add one in Settings \u{203a} Accounts, or set \
             password_command in accounts.toml",
            self.email
        )
    }
}

/// A deliberately loose check.
///
/// Validating addresses properly is famously not worth it, and being strict
/// here would reject a working address. This catches the typo that matters —
/// no `@`, or nothing on one side of it.
fn is_plausible_address(email: &str) -> bool {
    let email = email.trim();
    match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !email.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// A known mail provider, so setting up the common cases is picking a name
/// rather than looking up four hostnames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provider {
    pub id: ProviderId,
    pub label: &'static str,
    /// Address domains that imply this provider.
    pub domains: &'static [&'static str],
    pub imap_host: &'static str,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: &'static str,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub auth: Auth,
    /// The one thing about this provider that trips people up when signing in
    /// the way it recommends.
    pub note: &'static str,
    /// The same, for a password account. Split because the advice genuinely
    /// differs: Gmail's OAuth note is actively misleading to someone typing an
    /// app password.
    pub password_note: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Google,
    /// Anything else: a standard IMAP/SMTP server, entered by hand.
    Other,
}

pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: ProviderId::Google,
        label: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        imap_host: "imap.gmail.com",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "smtp.gmail.com",
        smtp_port: 465,
        smtp_security: Security::Tls,
        auth: Auth::GoogleOauth,
        note: "Signing in opens your browser. Spool never sees your Google password, and \
               you can revoke its access at any time from your Google account.",
        password_note: "Gmail rejects your Google password. Turn on 2-Step Verification, then \
                        generate an app password at myaccount.google.com/apppasswords \u{2014} \
                        or sign in with Google instead, which is easier.",
    },
    Provider {
        id: ProviderId::Other,
        label: "Other",
        domains: &[],
        imap_host: "",
        imap_port: 993,
        imap_security: Security::Tls,
        smtp_host: "",
        smtp_port: 465,
        smtp_security: Security::Tls,
        auth: Auth::Password,
        note: "Enter the servers your provider documents.",
        password_note: "Enter the servers your provider documents. IMAP is usually 993 with \
                        SSL/TLS, or 143 with STARTTLS.",
    },
    // Other providers work today through "Other" — these presets only save
    // looking up four hostnames. They are kept, commented, so that turning one
    // back on is uncommenting rather than researching it again. Each also
    // needs its `ProviderId` variant restored above.
    //
    // Provider {
    //     id: ProviderId::ICloud,
    //     label: "iCloud",
    //     domains: &["icloud.com", "me.com", "mac.com"],
    //     imap_host: "imap.mail.me.com",
    //     imap_port: 993,
    //     imap_security: Security::Tls,
    //     smtp_host: "smtp.mail.me.com",
    //     smtp_port: 587,
    //     smtp_security: Security::Starttls,
    //     auth: Auth::Password,
    //     note / password_note: iCloud rejects your Apple ID password; generate
    //     an app-specific password at account.apple.com.
    // },
    // Provider {
    //     id: ProviderId::Outlook,
    //     label: "Outlook",
    //     domains: &["outlook.com", "hotmail.com", "live.com", "msn.com"],
    //     imap_host: "outlook.office365.com",  imap_port: 993, Tls,
    //     smtp_host: "smtp.office365.com",     smtp_port: 587, Starttls,
    //     Personal accounts need an app password, and many Microsoft 365
    //     tenants disable password IMAP entirely — those need OAuth, which
    //     Spool only implements for Google so far.
    // },
    // Provider {
    //     id: ProviderId::Fastmail,
    //     label: "Fastmail",
    //     domains: &["fastmail.com", "fastmail.fm"],
    //     imap_host: "imap.fastmail.com",  imap_port: 993, Tls,
    //     smtp_host: "smtp.fastmail.com",  smtp_port: 465, Tls,
    //     App password from Settings > Privacy & Security, with Mail access.
    // },
    // Provider {
    //     id: ProviderId::Yahoo,
    //     label: "Yahoo",
    //     domains: &["yahoo.com", "ymail.com"],
    //     imap_host: "imap.mail.yahoo.com",  imap_port: 993, Tls,
    //     smtp_host: "smtp.mail.yahoo.com",  smtp_port: 465, Tls,
    //     App password from the account security page.
    // },
];

impl Provider {
    pub fn get(id: ProviderId) -> &'static Provider {
        PROVIDERS
            .iter()
            .find(|p| p.id == id)
            // `Other` is always present, so this cannot fire for a value that
            // came from `ProviderId`.
            .unwrap_or(&PROVIDERS[PROVIDERS.len() - 1])
    }

    /// Guesses the provider from an address domain, defaulting to `Other`.
    pub fn for_email(email: &str) -> &'static Provider {
        let domain = email.rsplit('@').next().unwrap_or_default().trim().to_ascii_lowercase();
        PROVIDERS
            .iter()
            .find(|p| p.domains.contains(&domain.as_str()))
            .unwrap_or_else(|| Provider::get(ProviderId::Other))
    }

    /// Whether this provider's servers and sign-in are settled facts.
    ///
    /// True for every real preset and false for `Other`, which exists
    /// precisely because nothing about it is known. The form asks about
    /// hosts, ports, security and authentication only when the answer is not
    /// already written down here: showing Gmail a choice between OAuth and a
    /// password, next to four fields that must say `imap.gmail.com` and
    /// nothing else, is asking a question that has one right answer.
    pub fn is_preset(&self) -> bool {
        self.id != ProviderId::Other
    }

    /// Whether an account is set up exactly the way this preset sets accounts
    /// up.
    ///
    /// The domain alone is not enough to claim an account *is* the preset. A
    /// Gmail address signing in with an app password, or pointed at a proxy,
    /// is a hand-configured account that happens to end in `gmail.com` — and
    /// hiding its servers behind a preset it does not match would show it
    /// settings it is not using.
    pub fn matches(&self, account: &AccountConfig) -> bool {
        account.imap_host == self.imap_host
            && account.imap_port == self.imap_port
            && account.imap_security == self.imap_security
            && account.smtp_host.as_deref().unwrap_or_default() == self.smtp_host
            && account.smtp_port == self.smtp_port
            && account.smtp_security == self.smtp_security
            && account.auth == self.auth
            // A login name that is not the address is a hand-configuration
            // too, and one the preset has nothing to say about.
            && account.imap_user.as_deref().unwrap_or_default().is_empty()
            && account.smtp_user.as_deref().unwrap_or_default().is_empty()
    }

    /// The preset an existing account is set up as, or `Other` when it
    /// deviates from every one of them.
    pub fn for_account(account: &AccountConfig) -> &'static Provider {
        let guess = Provider::for_email(&account.email);
        match guess.is_preset() && guess.matches(account) {
            true => guess,
            false => Provider::get(ProviderId::Other),
        }
    }

    /// The servers this preset uses, in one line, for a form that no longer
    /// shows them as fields.
    pub fn servers(&self) -> String {
        format!(
            "{} port {} ({}), {} port {} ({})",
            self.imap_host,
            self.imap_port,
            self.imap_security.label(),
            self.smtp_host,
            self.smtp_port,
            self.smtp_security.label(),
        )
    }

    /// The advice that applies to the way this account is signing in.
    pub fn note_for(&self, auth: Auth) -> &'static str {
        match auth {
            Auth::Password => self.password_note,
            Auth::GoogleOauth => self.note,
        }
    }

    /// Fills in this provider's servers, leaving everything the user typed —
    /// name, address, credentials — alone.
    pub fn apply_to(&self, account: &mut AccountConfig) {
        account.imap_host = self.imap_host.to_string();
        account.imap_port = self.imap_port;
        account.imap_security = self.imap_security;
        account.smtp_host = Some(self.smtp_host.to_string()).filter(|h| !h.is_empty());
        account.smtp_port = self.smtp_port;
        account.smtp_security = self.smtp_security;
        account.auth = self.auth;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> AccountConfig {
        AccountConfig {
            name: "A".into(),
            email: "a@example.com".into(),
            imap_host: "imap.example.com".into(),
            initial_limit: 100,
            ..Default::default()
        }
    }

    #[test]
    fn a_multi_line_signature_survives_a_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("spool-signature-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");

        let mut account = base();
        account.signature = "Wes Edling\nstudio.example\n+1 555 0100".into();
        let config = Config { accounts: vec![account.clone()] };
        config.save_to(&path).unwrap();

        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.accounts[0].signature, account.signature);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_config_written_before_parallel_syncing_existed_still_loads() {
        // The upgrade case: no `sync_connections` in the file, and the
        // account syncs over the default handful rather than over none.
        let toml = r#"
            [[accounts]]
            name = "A"
            email = "a@example.com"
            imap_host = "imap.example.com"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.accounts[0].connections(), default_sync_connections());
    }

    #[test]
    fn an_unreasonable_connection_count_is_brought_back_to_something_usable() {
        let mut account = base();

        account.sync_connections = 0;
        assert_eq!(account.connections(), 1, "off means one connection, not none");

        account.sync_connections = 500;
        assert_eq!(account.connections(), MAX_SYNC_CONNECTIONS, "the server's limit is the real one");

        account.sync_connections = 2;
        assert_eq!(account.connections(), 2);
    }

    #[test]
    fn a_config_written_before_signatures_existed_still_loads() {
        // Every field added to this struct has to be optional on the way in,
        // or an existing accounts.toml stops working on upgrade.
        let toml = r#"
            [[accounts]]
            name = "A"
            email = "a@example.com"
            imap_host = "imap.example.com"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.accounts[0].signature, "");
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let config = Config::load("/nonexistent/spool/accounts.toml").unwrap();
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn the_example_config_parses() {
        let config: Config = toml::from_str(Config::example()).unwrap();
        assert_eq!(config.accounts.len(), 1);

        let account = &config.accounts[0];
        assert_eq!(account.email, "me@example.com");
        assert_eq!(account.imap_port, 993);
        assert_eq!(account.initial_limit, 0, "the example keeps the whole folder");
        assert_eq!(account.user(), "me@example.com", "defaults to the address");
        assert_eq!(account.auth, Auth::Password);
    }

    #[test]
    fn ports_and_limits_have_defaults() {
        let config: Config = toml::from_str(
            r#"
            [[accounts]]
            name = "A"
            email = "a@example.com"
            imap_host = "imap.example.com"
            password = "hunter2"
            "#,
        )
        .unwrap();

        let account = &config.accounts[0];
        assert_eq!(account.imap_port, 993);
        assert_eq!(account.smtp_port, 465);
        assert_eq!(account.initial_limit, 0, "no cap unless one is asked for");
        assert_eq!(account.imap_security, Security::Tls);
        assert_eq!(account.auth, Auth::Password);
        assert!(account.folders.is_empty());
    }

    #[test]
    fn a_config_written_before_oauth_existed_still_loads() {
        // The fields added for OAuth and STARTTLS are all defaulted, so an
        // upgrade must not turn someone's working config into a parse error.
        let config: Config = toml::from_str(
            r#"
            [[accounts]]
            name = "Work"
            email = "me@example.com"
            imap_host = "imap.example.com"
            imap_port = 993
            smtp_host = "smtp.example.com"
            smtp_port = 465
            password_command = "true"
            folders = ["INBOX"]
            initial_limit = 500
            color = 2
            "#,
        )
        .unwrap();

        let account = &config.accounts[0];
        assert_eq!(account.auth, Auth::Password);
        assert_eq!(account.imap_security, Security::Tls);
        assert_eq!(account.folders, ["INBOX"]);
    }

    #[test]
    fn security_accepts_the_spellings_people_write() {
        #[derive(Deserialize)]
        struct Holder {
            value: Security,
        }
        let parse = |text: &str| toml::from_str::<Holder>(text).unwrap().value;

        assert_eq!(parse(r#"value = "tls""#), Security::Tls);
        assert_eq!(parse(r#"value = "starttls""#), Security::Starttls);
        assert_eq!(parse(r#"value = "start-tls""#), Security::Starttls);
        assert_eq!(parse(r#"value = "none""#), Security::Plaintext);
    }

    #[test]
    fn auth_round_trips_through_toml() {
        for auth in Auth::ALL {
            let account = AccountConfig { auth, ..base() };
            let config = Config { accounts: vec![account] };
            let text = toml::to_string(&config).unwrap();
            assert_eq!(toml::from_str::<Config>(&text).unwrap().accounts[0].auth, auth);
        }
        // The wire spelling is the one documented in the example.
        assert!(
            toml::to_string(&Config {
                accounts: vec![AccountConfig { auth: Auth::GoogleOauth, ..base() }]
            })
            .unwrap()
            .contains("google-oauth")
        );
    }

    #[test]
    fn imap_user_overrides_the_address_and_smtp_follows_it() {
        let account = AccountConfig { imap_user: Some("login-name".into()), ..base() };
        assert_eq!(account.user(), "login-name");
        assert_eq!(account.smtp_login(), "login-name", "SMTP follows IMAP by default");

        let split = AccountConfig { smtp_user: Some("smtp-name".into()), ..account };
        assert_eq!(split.smtp_login(), "smtp-name");
    }

    #[test]
    fn an_empty_login_falls_back_rather_than_authenticating_as_nobody() {
        // The settings pane sends empty strings for untouched optional
        // fields, and `Some("")` as a username is an authentication failure
        // with a confusing message.
        let account = AccountConfig {
            imap_user: Some(String::new()),
            smtp_user: Some(String::new()),
            ..base()
        };
        assert_eq!(account.user(), "a@example.com");
        assert_eq!(account.smtp_login(), "a@example.com");
    }

    #[test]
    fn password_resolution_prefers_command_then_env_then_literal() {
        let account = base();

        let literal = AccountConfig { password: Some("from-literal".into()), ..account.clone() };
        assert_eq!(literal.resolve_password().unwrap(), "from-literal");

        // SAFETY: the variable name is unique to this test, so no other test
        // in the binary observes or races on it.
        const VAR: &str = "SPOOL_TEST_PASSWORD_PRECEDENCE";
        unsafe { std::env::set_var(VAR, "from-env") };
        let from_env = AccountConfig { password_env: Some(VAR.into()), ..literal.clone() };
        assert_eq!(from_env.resolve_password().unwrap(), "from-env", "env beats literal");

        let command = AccountConfig {
            password_command: Some("printf 'from-command\\n'".into()),
            ..from_env.clone()
        };
        assert_eq!(
            command.resolve_password().unwrap(),
            "from-command",
            "command beats env, and its trailing newline is stripped"
        );
    }

    #[test]
    fn a_failing_password_command_reports_why() {
        let account = AccountConfig {
            password_command: Some("echo 'no such item' >&2; exit 44".into()),
            ..base()
        };

        let error = account.resolve_password().unwrap_err().to_string();
        assert!(error.contains("no such item"), "got: {error}");
    }

    #[test]
    fn an_account_with_no_password_anywhere_says_where_to_put_one() {
        // The address is unique, so the keychain lookup this falls through to
        // cannot find a real entry.
        let account = AccountConfig {
            email: format!("nobody-{}@example.invalid", std::process::id()),
            ..base()
        };
        let error = account.resolve_password().unwrap_err().to_string();
        assert!(error.contains("Settings"), "got: {error}");
    }

    #[test]
    fn validation_names_every_problem_at_once() {
        let problems = AccountConfig::default().problems();
        assert!(problems.iter().any(|p| p.contains("name")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("email")), "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("IMAP server")), "{problems:?}");

        assert!(base().problems().is_empty(), "a filled-in account is valid");
    }

    #[test]
    fn a_google_account_needs_nothing_from_the_user_but_an_address() {
        // The OAuth client belongs to the build, so a Google account is
        // valid as soon as it is named — there is nothing to paste in.
        let account = AccountConfig { auth: Auth::GoogleOauth, ..base() };
        assert!(account.problems().is_empty(), "{:?}", account.problems());
    }

    #[test]
    fn addresses_are_checked_loosely_enough_to_be_useful() {
        assert!(is_plausible_address("me@example.com"));
        assert!(is_plausible_address("first.last+tag@sub.example.co.uk"));
        // Unicode local parts are real and must not be rejected.
        assert!(is_plausible_address("我@example.com"));

        assert!(!is_plausible_address("example.com"));
        assert!(!is_plausible_address("@example.com"));
        assert!(!is_plausible_address("me@localhost"));
        assert!(!is_plausible_address("me@example."));
        assert!(!is_plausible_address("me you@example.com"));
    }

    #[test]
    fn a_domain_picks_its_provider() {
        assert_eq!(Provider::for_email("me@gmail.com").id, ProviderId::Google);
        assert_eq!(Provider::for_email("me@GoogleMail.COM").id, ProviderId::Google);
        assert_eq!(Provider::for_email("me@me.com").id, ProviderId::Other);
        assert_eq!(Provider::for_email("me@example.com").id, ProviderId::Other);
        assert_eq!(Provider::for_email("").id, ProviderId::Other);
    }

    #[test]
    fn applying_a_provider_keeps_what_the_user_typed() {
        let mut account = AccountConfig {
            name: "Personal".into(),
            email: "me@gmail.com".into(),
            password: Some("secret".into()),
            folders: vec!["INBOX".into()],
            color: 3,
            ..Default::default()
        };
        Provider::get(ProviderId::Google).apply_to(&mut account);

        assert_eq!(account.imap_host, "imap.gmail.com");
        assert_eq!(account.smtp_host.as_deref(), Some("smtp.gmail.com"));
        assert_eq!(account.auth, Auth::GoogleOauth);

        assert_eq!(account.name, "Personal", "the servers change, nothing else does");
        assert_eq!(account.password.as_deref(), Some("secret"));
        assert_eq!(account.folders, ["INBOX"]);
        assert_eq!(account.color, 3);
    }

    #[test]
    fn other_leaves_the_servers_blank_to_be_filled_in() {
        let mut account = AccountConfig { imap_host: "old.example.com".into(), ..base() };
        Provider::get(ProviderId::Other).apply_to(&mut account);
        assert!(account.imap_host.is_empty());
        assert_eq!(account.smtp_host, None, "an empty host is None, not Some(\"\")");
    }

    #[test]
    fn every_provider_is_reachable_and_described() {
        for provider in PROVIDERS {
            assert_eq!(Provider::get(provider.id).id, provider.id);
            assert!(!provider.label.is_empty());
            assert!(!provider.note.is_empty(), "{} has no note", provider.label);
            assert!(
                !provider.password_note.is_empty(),
                "{} has no password note",
                provider.label
            );
            for auth in Auth::ALL {
                assert!(!provider.note_for(auth).is_empty());
            }
            if provider.id != ProviderId::Other {
                assert!(!provider.imap_host.is_empty(), "{} has no IMAP host", provider.label);
                assert!(!provider.smtp_host.is_empty(), "{} has no SMTP host", provider.label);
            }
        }
    }

    #[test]
    fn gmail_files_its_own_sent_mail_but_only_through_its_own_server() {
        let mut account = AccountConfig { email: "me@gmail.com".into(), ..base() };
        Provider::get(ProviderId::Google).apply_to(&mut account);
        assert!(account.files_sent_mail_itself(), "appending as well would duplicate it");

        // The same address relaying through somebody else gets no copy filed
        // for it, and does need one appended.
        let relayed = AccountConfig { smtp_host: Some("smtp.example.com".into()), ..account };
        assert!(!relayed.files_sent_mail_itself());

        let none = AccountConfig { smtp_host: None, ..base() };
        assert!(!none.files_sent_mail_itself());
    }

    #[test]
    fn an_account_the_preset_set_up_is_recognised_as_that_preset() {
        // What the account form leans on to decide it has nothing left to
        // ask: an account the preset produced needs no servers and no
        // sign-in question, because both are already settled.
        let mut account = AccountConfig { email: "me@gmail.com".into(), ..base() };
        Provider::get(ProviderId::Google).apply_to(&mut account);

        assert_eq!(Provider::for_account(&account).id, ProviderId::Google);
        assert!(Provider::get(ProviderId::Google).is_preset());
    }

    #[test]
    fn a_gmail_address_configured_by_hand_is_not_the_preset() {
        // A Gmail address is not the same claim as "set up the Gmail way".
        // Anything hand-configured has to keep showing its own settings, or
        // the form hides values it is not actually using.
        let preset = Provider::get(ProviderId::Google);
        let mut base_gmail = AccountConfig { email: "me@gmail.com".into(), ..base() };
        preset.apply_to(&mut base_gmail);

        let app_password = AccountConfig { auth: Auth::Password, ..base_gmail.clone() };
        assert_eq!(Provider::for_account(&app_password).id, ProviderId::Other);

        let proxied = AccountConfig { imap_host: "imap.example.com".into(), ..base_gmail.clone() };
        assert_eq!(Provider::for_account(&proxied).id, ProviderId::Other);

        let starttls =
            AccountConfig { imap_security: Security::Starttls, ..base_gmail.clone() };
        assert_eq!(Provider::for_account(&starttls).id, ProviderId::Other);

        let other_login =
            AccountConfig { imap_user: Some("someone-else".into()), ..base_gmail.clone() };
        assert_eq!(Provider::for_account(&other_login).id, ProviderId::Other);

        // An empty override is not an override.
        let blank = AccountConfig { imap_user: Some(String::new()), ..base_gmail };
        assert_eq!(Provider::for_account(&blank).id, ProviderId::Google);
    }

    #[test]
    fn other_is_never_a_preset_and_states_no_servers() {
        let other = Provider::get(ProviderId::Other);
        assert!(!other.is_preset());
        // Nothing should ever ask `Other` what its servers are, but if it
        // did, the answer must not read like a working hostname.
        assert!(!other.servers().contains("gmail"));
    }

    #[test]
    fn a_preset_can_state_its_servers_in_one_line() {
        let servers = Provider::get(ProviderId::Google).servers();
        assert!(servers.contains("imap.gmail.com"), "{servers}");
        assert!(servers.contains("smtp.gmail.com"), "{servers}");
        assert!(servers.contains("993") && servers.contains("465"), "{servers}");
        assert!(servers.contains("SSL/TLS"), "{servers}");
    }

    #[test]
    fn gmails_advice_depends_on_how_you_are_signing_in() {
        // Telling someone about to press a browser button that they need an
        // app password sends them to the wrong page entirely, and vice versa.
        let gmail = Provider::get(ProviderId::Google);
        assert!(gmail.note_for(Auth::GoogleOauth).contains("browser"));
        assert!(!gmail.note_for(Auth::GoogleOauth).contains("app password"));

        assert!(gmail.note_for(Auth::Password).contains("app password"));
        assert!(!gmail.note_for(Auth::Password).contains("browser"));
    }

    #[test]
    fn upsert_adds_then_replaces() {
        let mut config = Config::default();

        config.upsert(None, base());
        assert_eq!(config.accounts.len(), 1);

        config.upsert(Some(0), AccountConfig { name: "Renamed".into(), ..base() });
        assert_eq!(config.accounts.len(), 1, "editing does not append");
        assert_eq!(config.accounts[0].name, "Renamed");

        // An index past the end appends rather than panicking, so a stale
        // index from a list that changed underneath cannot crash the pane.
        config.upsert(Some(9), base());
        assert_eq!(config.accounts.len(), 2);
    }

    #[test]
    fn removing_out_of_range_is_none_rather_than_a_panic() {
        let mut config = Config { accounts: vec![base()] };
        assert!(config.remove(4).is_none());
        assert_eq!(config.accounts.len(), 1);
    }

    #[test]
    fn accounts_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("spool-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");

        let config = Config {
            accounts: vec![
                AccountConfig {
                    name: "Work".into(),
                    imap_security: Security::Starttls,
                    imap_port: 143,
                    smtp_host: Some("smtp.example.com".into()),
                    smtp_port: 587,
                    smtp_security: Security::Starttls,
                    folders: vec!["INBOX".into(), "Sent".into()],
                    ..base()
                },
                AccountConfig {
                    name: "Gmail".into(),
                    email: "me@gmail.com".into(),
                    auth: Auth::GoogleOauth,
                    imap_host: "imap.gmail.com".into(),
                    ..Default::default()
                },
            ],
        };

        config.save_to(&path).unwrap();
        let loaded = Config::load(&path).unwrap();

        assert_eq!(loaded.accounts.len(), 2);
        assert_eq!(loaded.accounts[0].imap_security, Security::Starttls);
        assert_eq!(loaded.accounts[0].smtp_port, 587);
        assert_eq!(loaded.accounts[0].folders, ["INBOX", "Sent"]);
        assert_eq!(loaded.accounts[1].auth, Auth::GoogleOauth);
        assert_eq!(loaded.accounts[1].imap_host, "imap.gmail.com");

        // No secret reaches the file, whatever else does.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with('#'), "{text}");
        assert!(!text.contains("password ="), "{text}");

        assert!(!path.with_extension("toml.tmp").exists(), "no temporary left behind");
        std::fs::remove_dir_all(&dir).ok();
    }
}
