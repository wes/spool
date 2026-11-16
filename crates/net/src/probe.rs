//! "Test connection", as one call.
//!
//! Account setup fails in three distinct places — resolving the credential,
//! reaching IMAP, reaching SMTP — and which one failed is the whole diagnosis.
//! So this runs all three and reports each separately, rather than returning
//! the first error and leaving the user to guess whether the rest would have
//! worked.

use crate::config::{AccountConfig, Credentials};
use crate::{imap, smtp};

/// One thing that was checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What was tried, e.g. `IMAP  imap.gmail.com:993`.
    pub label: String,
    pub outcome: Outcome,
    /// On success, what worked; on failure, the server's own words.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed,
    /// Not checked, because nothing was configured to check.
    Skipped,
}

impl Step {
    pub fn passed(&self) -> bool {
        self.outcome == Outcome::Passed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub steps: Vec<Step>,
}

impl Probe {
    /// True when nothing failed. A skipped step is not a failure — an account
    /// with no SMTP server is a decision, not a mistake.
    pub fn passed(&self) -> bool {
        self.steps.iter().all(|step| step.outcome != Outcome::Failed)
    }

    /// A one-line summary for the status bar.
    pub fn summary(&self) -> String {
        match self.steps.iter().find(|step| step.outcome == Outcome::Failed) {
            Some(failed) => format!("{}: {}", failed.label, first_line(&failed.detail)),
            None => "Connection test passed".to_string(),
        }
    }
}

/// Runs the whole check. Never returns an error: every failure is a result to
/// display, not an exception to propagate.
pub async fn test_account(config: &AccountConfig) -> Probe {
    let credentials = match config.credentials().await {
        Ok(credentials) => credentials,
        Err(e) => {
            // Without a credential neither server can be tested, and saying
            // "IMAP failed" here would point at the wrong thing entirely.
            return Probe {
                steps: vec![Step {
                    label: credential_label(config),
                    outcome: Outcome::Failed,
                    detail: format!("{e:#}"),
                }],
            };
        }
    };

    let mut steps = vec![Step {
        label: credential_label(config),
        outcome: Outcome::Passed,
        detail: describe_credential(&credentials),
    }];

    let imap_label = format!("IMAP  {}:{}", config.imap_host, config.imap_port);
    steps.push(match imap::verify(config, &credentials).await {
        Ok(()) => Step {
            label: imap_label,
            outcome: Outcome::Passed,
            detail: format!("authenticated over {}", config.imap_security.label()),
        },
        Err(e) => Step {
            label: imap_label,
            outcome: Outcome::Failed,
            detail: format!("{e:#}"),
        },
    });

    let smtp_host = config.smtp_host.as_deref().map(str::trim).unwrap_or_default();
    steps.push(if smtp_host.is_empty() {
        Step {
            label: "SMTP".into(),
            outcome: Outcome::Skipped,
            detail: "no server configured — this account cannot send".into(),
        }
    } else {
        let label = format!("SMTP  {smtp_host}:{}", config.smtp_port);
        match smtp::verify(config, &credentials).await {
            Ok(()) => Step {
                label,
                outcome: Outcome::Passed,
                detail: format!("authenticated over {}", config.smtp_security.label()),
            },
            Err(e) => Step { label, outcome: Outcome::Failed, detail: format!("{e:#}") },
        }
    });

    Probe { steps }
}

fn credential_label(config: &AccountConfig) -> String {
    match config.auth {
        crate::config::Auth::Password => "Password".to_string(),
        crate::config::Auth::GoogleOauth => "Google".to_string(),
    }
}

fn describe_credential(credentials: &Credentials) -> String {
    match credentials {
        Credentials::Password(_) => "found".to_string(),
        Credentials::Bearer(_) => "token refreshed".to_string(),
    }
}

/// Errors arrive as a chain joined by `: `, and the status bar has room for
/// about one clause of it.
fn first_line(detail: &str) -> &str {
    detail.lines().next().unwrap_or(detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(label: &str, outcome: Outcome) -> Step {
        Step { label: label.into(), outcome, detail: "detail".into() }
    }

    #[test]
    fn a_probe_with_no_failures_passes() {
        let probe = Probe {
            steps: vec![step("Password", Outcome::Passed), step("IMAP", Outcome::Passed)],
        };
        assert!(probe.passed());
        assert_eq!(probe.summary(), "Connection test passed");
    }

    #[test]
    fn a_skipped_step_is_not_a_failure() {
        // An account with no SMTP server still has a working inbox, and
        // marking the whole test failed would be wrong.
        let probe = Probe {
            steps: vec![step("IMAP", Outcome::Passed), step("SMTP", Outcome::Skipped)],
        };
        assert!(probe.passed());
    }

    #[test]
    fn the_summary_names_the_first_thing_that_failed() {
        let probe = Probe {
            steps: vec![
                step("Password", Outcome::Passed),
                Step {
                    label: "IMAP  imap.example.com:993".into(),
                    outcome: Outcome::Failed,
                    detail: "AUTHENTICATIONFAILED\nsecond line".into(),
                },
                step("SMTP", Outcome::Failed),
            ],
        };

        assert!(!probe.passed());
        let summary = probe.summary();
        assert!(summary.contains("imap.example.com"), "{summary}");
        assert!(summary.contains("AUTHENTICATIONFAILED"), "{summary}");
        assert!(!summary.contains("second line"), "the status bar gets one line: {summary}");
    }

    #[test]
    fn a_missing_credential_is_reported_before_any_server_is_dialled() {
        // A unique address, so the keychain fallback cannot find anything.
        let config = AccountConfig {
            name: "A".into(),
            email: format!("nobody-{}@example.invalid", std::process::id()),
            // Deliberately unroutable: reaching it would mean the credential
            // check did not short-circuit.
            imap_host: "imap.invalid".into(),
            ..Default::default()
        };

        let probe = smol::block_on(test_account(&config));
        assert_eq!(probe.steps.len(), 1, "nothing else runs without a credential");
        assert_eq!(probe.steps[0].outcome, Outcome::Failed);
        assert!(!probe.passed());
    }
}
