//! What an extension is allowed to reach.
//!
//! A capability is a string in a manifest and a decision at link time. The host
//! reads what an extension asked for, and then builds that extension's linker
//! out of exactly those imports — so an extension that did not ask to modify
//! mail is not an extension whose `set-flags` effect gets rejected, it is an
//! extension that was never handed a way to express one.
//!
//! That distinction is the whole point. A permission checked at the call site
//! is a permission somebody forgets to check; a permission enforced by absence
//! cannot be forgotten, and it shows up as a link error when the extension is
//! loaded rather than as a silent no-op six months later.
//!
//! The set is deliberately small. Every capability here is one a user could be
//! shown and would understand — "reads your mail", "files your mail", "talks to
//! api.github.com" — because a permission nobody can evaluate is a dialog
//! everybody clicks through.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// One permission, as written in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// Read envelopes, bodies, headers, folders and accounts.
    ReadMail,
    /// Change flags, move, archive and trash.
    ///
    /// Separate from [`Capability::ReadMail`] because the two are wildly
    /// different promises: one extension summarizes your inbox, another can
    /// empty it, and a manifest that could not tell them apart would make the
    /// distinction unaskable.
    ModifyMail,
    /// Draw banners, badges and panels, and put a line in the status bar.
    Ui,
    /// Post a system notification.
    ///
    /// Its own capability rather than part of `ui`, because a notification is
    /// the one piece of UI that reaches the user when the app is not even in
    /// front of them.
    Notify,
    /// Keep a private key/value store across launches.
    Storage,
    /// Make HTTPS requests to one host.
    Net(NetHost),
}

impl Capability {
    /// The capability a manifest has to declare for each host function.
    ///
    /// Kept beside the enum rather than in the linker so that the answer to
    /// "what does this extension need in order to do that" has one source.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ReadMail => "read-mail",
            Self::ModifyMail => "modify-mail",
            Self::Ui => "ui",
            Self::Notify => "notify",
            Self::Storage => "storage",
            Self::Net(_) => "net",
        }
    }

    /// A sentence a person could be shown before they agree to it.
    pub fn describe(&self) -> String {
        match self {
            Self::ReadMail => "Read your mail, folders and accounts".into(),
            Self::ModifyMail => "Flag, move, archive and delete your mail".into(),
            Self::Ui => "Add banners, badges and panels to the window".into(),
            Self::Notify => "Send you notifications".into(),
            Self::Storage => "Keep its own settings between launches".into(),
            Self::Net(host) => format!("Send and receive data from {host}"),
        }
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Net(host) => write!(f, "net:https:{host}"),
            other => f.write_str(other.name()),
        }
    }
}

impl FromStr for Capability {
    type Err = CapabilityError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        match text {
            "read-mail" => return Ok(Self::ReadMail),
            "modify-mail" => return Ok(Self::ModifyMail),
            "ui" => return Ok(Self::Ui),
            "notify" => return Ok(Self::Notify),
            "storage" => return Ok(Self::Storage),
            _ => {}
        }

        // The only compound form. Split into at most three so a host
        // containing a colon — which is malformed, but is what somebody will
        // write — is reported as a bad host rather than as a bad capability.
        let mut parts = text.splitn(3, ':');
        match (parts.next(), parts.next(), parts.next()) {
            (Some("net"), Some(scheme), Some(host)) => {
                // Plaintext HTTP is refused rather than merely discouraged.
                // An extension's traffic is the user's mail leaving the
                // machine, and there is no version of that worth sending in
                // the clear.
                if scheme != "https" {
                    return Err(CapabilityError::Scheme(scheme.to_string()));
                }
                Ok(Self::Net(host.parse()?))
            }
            (Some("net"), _, _) => Err(CapabilityError::NetForm(text.to_string())),
            _ => Err(CapabilityError::Unknown(text.to_string())),
        }
    }
}

/// One host an extension may talk to.
///
/// Either an exact name or a single leading `*.` wildcard. A bare `*` is
/// refused: an extension that wants to reach anywhere is an extension whose
/// manifest tells the user nothing, and the whole value of declaring a host is
/// that the declaration is worth reading.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NetHost {
    /// Lowercased, and without the `*.` when this is a wildcard.
    suffix: String,
    wildcard: bool,
}

impl NetHost {
    /// Whether this permits a request to `host`.
    ///
    /// `*.example.com` covers `api.example.com` and `a.b.example.com` but not
    /// `example.com` itself, which is the reading that makes a wildcard a
    /// statement about subdomains rather than a slightly wider exact match.
    /// Comparison is case-insensitive, because DNS is.
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if !self.wildcard {
            return host == self.suffix;
        }
        // `notexample.com` must not match `*.example.com`, so the dot is part
        // of what has to be there rather than something stripped first.
        host.len() > self.suffix.len() + 1
            && host.ends_with(&self.suffix)
            && host.as_bytes()[host.len() - self.suffix.len() - 1] == b'.'
    }
}

impl fmt::Display for NetHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.wildcard {
            true => write!(f, "*.{}", self.suffix),
            false => f.write_str(&self.suffix),
        }
    }
}

impl FromStr for NetHost {
    type Err = CapabilityError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim().to_ascii_lowercase();
        let (suffix, wildcard) = match text.strip_prefix("*.") {
            Some(rest) => (rest.to_string(), true),
            None => (text.clone(), false),
        };

        let plausible = !suffix.is_empty()
            && suffix.contains('.')
            && !suffix.starts_with('.')
            && !suffix.ends_with('.')
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');

        match plausible {
            true => Ok(Self { suffix, wildcard }),
            false => Err(CapabilityError::Host(text)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityError {
    Unknown(String),
    Scheme(String),
    NetForm(String),
    Host(String),
}

impl fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(text) => write!(f, "`{text}` is not a capability Spool knows"),
            Self::Scheme(scheme) => {
                write!(
                    f,
                    "`{scheme}` is not allowed — network access is https only"
                )
            }
            Self::NetForm(text) => {
                write!(
                    f,
                    "`{text}` is missing a host — write it as `net:https:example.com`"
                )
            }
            Self::Host(host) => write!(f, "`{host}` is not a host name"),
        }
    }
}

impl std::error::Error for CapabilityError {}

impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

impl Serialize for Capability {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Everything one extension was granted, ready to be asked questions.
///
/// Built once when the extension is loaded and then consulted while its linker
/// is assembled, so the cost of a lookup never lands on a call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Grants {
    granted: Vec<Capability>,
}

impl Grants {
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        let mut granted: Vec<Capability> = capabilities.into_iter().collect();
        granted.sort();
        granted.dedup();
        Self { granted }
    }

    pub fn has(&self, capability: &Capability) -> bool {
        self.granted.contains(capability)
    }

    pub fn reads_mail(&self) -> bool {
        self.has(&Capability::ReadMail)
    }

    pub fn modifies_mail(&self) -> bool {
        self.has(&Capability::ModifyMail)
    }

    pub fn draws(&self) -> bool {
        self.has(&Capability::Ui)
    }

    pub fn notifies(&self) -> bool {
        self.has(&Capability::Notify)
    }

    pub fn stores(&self) -> bool {
        self.has(&Capability::Storage)
    }

    /// Whether any declared host covers this one.
    pub fn may_reach(&self, host: &str) -> bool {
        self.granted
            .iter()
            .any(|c| matches!(c, Capability::Net(net) if net.matches(host)))
    }

    /// Whether the extension asked for the network at all, which is what
    /// decides whether `fetch` is linked in the first place.
    pub fn networks(&self) -> bool {
        self.granted.iter().any(|c| matches!(c, Capability::Net(_)))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.granted.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.granted.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(text: &str) -> Capability {
        text.parse().expect(text)
    }

    #[test]
    fn the_plain_capabilities_round_trip_through_their_names() {
        for text in ["read-mail", "modify-mail", "ui", "notify", "storage"] {
            assert_eq!(cap(text).to_string(), text);
        }
    }

    #[test]
    fn a_network_capability_names_one_host() {
        let Capability::Net(host) = cap("net:https:api.github.com") else {
            panic!("expected a net capability");
        };
        assert!(host.matches("api.github.com"));
        assert!(
            !host.matches("github.com"),
            "an exact host is exactly that host"
        );
        assert!(!host.matches("evil.com"));
        assert_eq!(
            cap("net:https:api.github.com").to_string(),
            "net:https:api.github.com"
        );
    }

    #[test]
    fn a_wildcard_covers_subdomains_and_not_the_domain_itself() {
        let Capability::Net(host) = cap("net:https:*.example.com") else {
            panic!()
        };
        assert!(host.matches("api.example.com"));
        assert!(host.matches("a.b.example.com"), "any depth of subdomain");
        assert!(
            !host.matches("example.com"),
            "the bare domain was not asked for"
        );
        // The dot has to be there. Without this check a capability for
        // `*.example.com` would quietly cover a host somebody registered
        // specifically to collect it.
        assert!(!host.matches("notexample.com"));
        assert!(!host.matches("example.com.evil.com"));
    }

    #[test]
    fn hosts_are_matched_without_regard_to_case_or_a_trailing_dot() {
        let Capability::Net(host) = cap("net:https:API.Example.COM") else {
            panic!()
        };
        assert!(host.matches("api.example.com"));
        assert!(host.matches("API.EXAMPLE.COM"));
        // The fully-qualified form, which is what some resolvers hand back.
        assert!(host.matches("api.example.com."));
    }

    #[test]
    fn plaintext_http_is_refused_rather_than_quietly_upgraded() {
        assert_eq!(
            "net:http:example.com".parse::<Capability>(),
            Err(CapabilityError::Scheme("http".into())),
        );
    }

    #[test]
    fn a_blank_cheque_is_not_a_capability() {
        // `*` alone has no dot in it, so it fails the host test and never
        // becomes a wildcard that matches everything.
        assert!("net:https:*".parse::<Capability>().is_err());
        assert!("net:https:*.".parse::<Capability>().is_err());
        assert!("net:https:".parse::<Capability>().is_err());
        assert!("net:https".parse::<Capability>().is_err());
    }

    #[test]
    fn an_unknown_capability_is_an_error_rather_than_a_shrug() {
        // Silently dropping one would load an extension with less than it
        // asked for, which fails later and somewhere else.
        assert_eq!(
            "read-everything".parse::<Capability>(),
            Err(CapabilityError::Unknown("read-everything".into())),
        );
    }

    #[test]
    fn grants_answer_for_every_host_they_were_given() {
        let grants = Grants::new([
            cap("read-mail"),
            cap("net:https:api.github.com"),
            cap("net:https:*.example.com"),
        ]);

        assert!(grants.reads_mail());
        assert!(!grants.modifies_mail(), "not asked for, not granted");
        assert!(grants.networks());

        assert!(grants.may_reach("api.github.com"));
        assert!(grants.may_reach("anything.example.com"));
        assert!(!grants.may_reach("example.com"));
        assert!(!grants.may_reach("api.github.com.evil.net"));
    }

    #[test]
    fn an_extension_that_asked_for_nothing_reaches_nothing() {
        let grants = Grants::default();
        assert!(!grants.reads_mail());
        assert!(!grants.modifies_mail());
        assert!(!grants.draws());
        assert!(!grants.notifies());
        assert!(!grants.stores());
        assert!(!grants.networks());
        assert!(!grants.may_reach("example.com"));
    }

    #[test]
    fn the_same_capability_twice_is_still_one_grant() {
        let grants = Grants::new([cap("ui"), cap("ui"), cap("read-mail")]);
        assert_eq!(grants.iter().count(), 2);
    }
}
