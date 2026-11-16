//! Where written-down secrets go.
//!
//! `accounts.toml` is a plaintext file people keep in dotfile repos, so
//! nothing the app writes ever puts a password in it. Secrets go to the system
//! keychain and the config keeps only a note that one is there.
//!
//! On macOS this shells out to `security`, the same tool the documented
//! `password_command` uses — one mechanism to understand rather than two.
//! Elsewhere there is no equivalent CLI, so it falls back to a file only the
//! owner can read, which is weaker and says so.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::encoding::{base64, base64_decode};

/// Marks a value this app wrote, and therefore base64-encoded.
///
/// `security -w` prints a secret verbatim only while it is printable ASCII;
/// give it anything else and it prints a hex dump instead, with nothing to
/// say that it did. A password with an accent in it would come back as
/// `68756e...` and fail to authenticate for no visible reason.
///
/// Encoding on the way in sidesteps that: what is stored is always printable
/// ASCII, so what comes back is always what went in. The prefix is what tells
/// a value we wrote from one a user added by hand with the documented
/// `security add-generic-password`, which is stored raw.
const ENCODED_PREFIX: &str = "spool-b64:";

/// What [`ENCODED_PREFIX`] was before the app was renamed to Spool.
///
/// Only [`decode`] looks at it. A secret stored under the old prefix is still
/// marked as encoded, and reading it without stripping the marker would hand
/// the literal string `spool-b64:...` to the server as a password — an
/// authentication failure with nothing on screen to explain it.
const LEGACY_ENCODED_PREFIX: &str = "mailbox-b64:";

/// Which secret. An account can have three at once, so they need separate
/// keys rather than one entry per address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// The IMAP/SMTP password.
    Password,
    /// The OAuth refresh token: long-lived, and the one most worth protecting.
    RefreshToken,
    /// The ChatGPT sign-in the writing tools use. Not a mail account's, so
    /// it is kept out of [`Slot::ALL`] and stored under a fixed name.
    ChatGpt,
    /// The token a client book source sends with each pull, stored under the
    /// source's own name rather than a mail account's.
    ClientSource,
}

impl Slot {
    /// The keychain service name. Everything the app writes goes here.
    ///
    /// Renaming one orphans every secret already stored under the old name,
    /// leaving accounts that silently stop authenticating. The rename to Spool
    /// is survivable only because [`Slot::legacy_service`] is still read.
    pub fn service(self) -> &'static str {
        match self {
            Slot::Password => "spool",
            Slot::RefreshToken => "spool-refresh-token",
            Slot::ChatGpt => "spool-chatgpt",
            Slot::ClientSource => "spool-client-source",
        }
    }

    /// What [`Slot::service`] returned while the app was called Mailbox.
    ///
    /// Read from, never written to, and only when the current name holds
    /// nothing: it exists so renaming the app does not sign out every account
    /// that was already set up.
    /// `None` for a slot that arrived after the rename.
    pub fn legacy_service(self) -> Option<&'static str> {
        match self {
            Slot::Password => Some("mailbox"),
            Slot::RefreshToken => Some("mailbox-refresh-token"),
            Slot::ChatGpt | Slot::ClientSource => None,
        }
    }

    /// The slots a mail account can have, which removing or renaming one
    /// walks through.
    pub const ALL: [Slot; 2] = [Slot::Password, Slot::RefreshToken];
}

/// True when secrets go to a real keychain rather than a file.
///
/// The UI says which, because "stored in your keychain" and "stored in a file
/// on this disk" deserve different amounts of trust.
pub const fn keychain_available() -> bool {
    cfg!(target_os = "macos")
}

/// The shell command that reads this secret back, for display and for the
/// `password_command` a hand-edited config would use.
pub fn read_command(slot: Slot, account: &str) -> String {
    if keychain_available() {
        format!(
            "security find-generic-password -s {} -a {} -w",
            slot.service(),
            account
        )
    } else {
        format!("(from {})", fallback_path().display())
    }
}

pub fn store(slot: Slot, account: &str, secret: &str) -> Result<()> {
    if keychain_available() {
        keychain_store(slot, account, secret)
    } else {
        file_store(slot, account, Some(secret))
    }
}

/// Reads a secret, distinguishing "not stored" from "could not look it up".
///
/// A missing entry is `Ok(None)`: the first time an account is set up there is
/// nothing there yet, and that is not a failure.
pub fn load(slot: Slot, account: &str) -> Result<Option<String>> {
    if keychain_available() {
        keychain_load(slot, account)
    } else {
        let all = file_load()?;
        all.get(slot.service())
            .and_then(|m| m.get(account))
            // Stored before the app was renamed to Spool.
            .or_else(|| slot.legacy_service().and_then(|l| all.get(l)).and_then(|m| m.get(account)))
            .map(|stored| decode(stored))
            .transpose()
    }
}

pub fn delete(slot: Slot, account: &str) -> Result<()> {
    if keychain_available() {
        keychain_delete(slot, account)
    } else {
        file_store(slot, account, None)
    }
}

/// Forgets every secret belonging to one address, for account removal.
///
/// Best effort by design: a keychain the user has locked should not stop them
/// from deleting the account, it should just leave an unused entry behind.
pub fn forget_all(account: &str) {
    for slot in Slot::ALL {
        if let Err(e) = delete(slot, account) {
            log::warn!("could not remove the {} entry for {account}: {e:#}", slot.service());
        }
    }
}

fn keychain_store(slot: Slot, account: &str, secret: &str) -> Result<()> {
    // The secret travels in the environment rather than in the argument list.
    // `ps` shows another user's arguments; it does not show their environment
    // without root. `-U` updates an existing entry instead of failing.
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(
            r#"security add-generic-password -U \
                 -s "$SPOOL_SERVICE" -a "$SPOOL_ACCOUNT" \
                 -w "$SPOOL_SECRET" 2>&1"#,
        )
        .env("SPOOL_SERVICE", slot.service())
        .env("SPOOL_ACCOUNT", account)
        .env("SPOOL_SECRET", encode(secret))
        .output()
        .context("running security(1)")?;

    if !output.status.success() {
        bail!(
            "could not save to the keychain: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    Ok(())
}

fn keychain_load(slot: Slot, account: &str) -> Result<Option<String>> {
    if let Some(secret) = keychain_find(slot.service(), account)? {
        return Ok(Some(secret));
    }
    // Stored before the app was renamed to Spool. Left where it is rather than
    // copied forward: a read is not the place to start writing to the
    // keychain, and re-saving the account moves it under the new name anyway.
    match slot.legacy_service() {
        Some(legacy) => keychain_find(legacy, account),
        None => Ok(None),
    }
}

fn keychain_find(service: &str, account: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .context("running security(1)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // The only failure that is not an error: nothing has been stored yet.
        if stderr.contains("could not be found") {
            return Ok(None);
        }
        bail!("could not read from the keychain: {}", stderr.trim());
    }

    let secret = String::from_utf8(output.stdout)
        .context("the keychain returned a non-UTF-8 secret")?;
    // `security` terminates its output with a newline; sending it as part of
    // a password produces a baffling authentication failure.
    decode(secret.trim_end_matches(['\n', '\r'])).map(Some)
}

fn keychain_delete(slot: Slot, account: &str) -> Result<()> {
    // Both names, or removing an account would leave the secret it was set up
    // with sitting in the keychain under the app's old name.
    keychain_forget(slot.service(), account)?;
    match slot.legacy_service() {
        Some(legacy) => keychain_forget(legacy, account),
        None => Ok(()),
    }
}

fn keychain_forget(service: &str, account: &str) -> Result<()> {
    let output = std::process::Command::new("security")
        .args(["delete-generic-password", "-s", service, "-a", account])
        .output()
        .context("running security(1)")?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() && !stderr.contains("could not be found") {
        bail!("could not remove from the keychain: {}", stderr.trim());
    }
    Ok(())
}

fn encode(secret: &str) -> String {
    format!("{ENCODED_PREFIX}{}", base64(secret.as_bytes()))
}

/// Undoes [`encode`], passing through anything it did not write.
fn decode(stored: &str) -> Result<String> {
    let Some(encoded) = stored
        .strip_prefix(ENCODED_PREFIX)
        .or_else(|| stored.strip_prefix(LEGACY_ENCODED_PREFIX))
    else {
        return Ok(stored.to_string());
    };

    let bytes = base64_decode(encoded)
        .context("the keychain entry is marked as encoded but is not valid base64")?;
    String::from_utf8(bytes).context("the keychain entry did not decode to text")
}

fn fallback_path() -> PathBuf {
    spool_core::paths::data_dir().join("secrets.toml")
}

type SecretFile = BTreeMap<String, BTreeMap<String, String>>;

fn file_load() -> Result<SecretFile> {
    let path = fallback_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SecretFile::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// `None` deletes. One function for both so the read-modify-write of the whole
/// file exists in one place rather than two.
fn file_store(slot: Slot, account: &str, secret: Option<&str>) -> Result<()> {
    let mut all = file_load()?;
    match secret {
        Some(secret) => {
            // Encoded here too, so both back ends store the same bytes and
            // `decode` is the only thing that has to know the difference.
            all.entry(slot.service().to_string())
                .or_default()
                .insert(account.to_string(), encode(secret));
        }
        None => {
            if let Some(entries) = all.get_mut(slot.service()) {
                entries.remove(account);
                if entries.is_empty() {
                    all.remove(slot.service());
                }
            }
        }
    }

    let path = fallback_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let text = format!(
        "# Secrets, because this platform has no keychain Spool knows how to\n\
         # use. Readable only by you. Do not commit it.\n{}",
        toml::to_string_pretty(&all).context("serializing secrets")?
    );

    // Created with the right mode from the start; writing then chmod-ing
    // leaves a window in which the file is world-readable.
    let temporary = path.with_extension("toml.tmp");
    write_private(&temporary, &text)?;
    std::fs::rename(&temporary, &path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_stored_under_the_old_name_still_decodes() {
        // What the app wrote when it was called Mailbox.
        let stored = format!("{LEGACY_ENCODED_PREFIX}{}", base64(b"hunter2"));
        assert_eq!(
            decode(&stored).unwrap(),
            "hunter2",
            "the rename must not turn a stored password into a literal marker"
        );
    }

    #[test]
    fn no_slot_reuses_another_slots_name_across_the_rename() {
        let mut names: Vec<_> = [Slot::Password, Slot::RefreshToken, Slot::ChatGpt]
            .iter()
            .flat_map(|s| std::iter::once(s.service()).chain(s.legacy_service()))
            .collect();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total, "a fallback would read another slot's secret");
    }

    #[test]
    fn the_slots_do_not_share_a_service_name() {
        let slots = [Slot::Password, Slot::RefreshToken, Slot::ChatGpt];
        let mut names: Vec<_> = slots.iter().map(|s| s.service()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), slots.len(), "one slot would overwrite another");
    }

    #[test]
    fn the_read_command_names_the_account() {
        let command = read_command(Slot::Password, "me@example.com");
        assert!(command.contains("me@example.com"), "{command}");
    }

    #[test]
    fn encoding_round_trips_and_leaves_hand_written_entries_alone() {
        for secret in ["hunter2", "", "with spaces", "e\u{301}\u{4e2d}\u{1f512}", "line\nbreak"] {
            assert_eq!(decode(&encode(secret)).unwrap(), secret, "{secret:?}");
        }

        // What the encoder produces is printable ASCII, which is the whole
        // reason it exists — `security -w` mangles anything else.
        assert!(encode("\u{4e2d}").is_ascii());

        // A password put there by hand, per the documented command, is stored
        // raw and must come back untouched.
        assert_eq!(decode("plain-password").unwrap(), "plain-password");
        // Including one that happens to look like base64.
        assert_eq!(decode("aGVsbG8=").unwrap(), "aGVsbG8=");
    }

    #[test]
    fn a_corrupt_encoded_entry_is_an_error_rather_than_garbage() {
        // Better to say the entry is unreadable than to hand a mangled
        // password to a server and report an authentication failure.
        assert!(decode(&format!("{ENCODED_PREFIX}not base64!")).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_secret_round_trips_through_the_keychain() {
        // A unique account name, so a failed run leaves nothing that a later
        // run or a real account could collide with.
        let account = format!("spool-test-{}@example.invalid", std::process::id());

        assert_eq!(load(Slot::Password, &account).unwrap(), None, "starts empty");

        store(Slot::Password, &account, "hunter2\u{a0}\u{4e2d}").unwrap();
        assert_eq!(
            load(Slot::Password, &account).unwrap().as_deref(),
            Some("hunter2\u{a0}\u{4e2d}"),
            "non-ASCII survives the round trip"
        );

        // Storing twice updates rather than failing on a duplicate.
        store(Slot::Password, &account, "second").unwrap();
        assert_eq!(load(Slot::Password, &account).unwrap().as_deref(), Some("second"));

        // The slots are independent.
        assert_eq!(load(Slot::RefreshToken, &account).unwrap(), None);

        delete(Slot::Password, &account).unwrap();
        assert_eq!(load(Slot::Password, &account).unwrap(), None);

        // Deleting what is not there is not an error; account removal runs it
        // over every slot without knowing which were ever used.
        delete(Slot::Password, &account).unwrap();
    }
}
