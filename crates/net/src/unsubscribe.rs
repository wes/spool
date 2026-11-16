//! Carrying out an unsubscribe that needs no browser.
//!
//! [`Method`] decides how; this does it. A one-click request is a POST, and a
//! `mailto:` is a short message sent from the account the list wrote to. A
//! plain link is not handled here — it needs a person on the other end of a
//! web page, and the app opens it for them.

use std::time::Duration;

use anyhow::{Result, bail};
use spool_core::{
    Address,
    draft::Draft,
    unsubscribe::{Method, ONE_CLICK_BODY},
};

use crate::{compose, config::AccountConfig, http, smtp};

/// How long a one-click request may take before it is reported as failed.
///
/// Shorter than a socket's own idle limit, which is sized for IMAP: somebody
/// pressed a button and is watching for the answer.
const ONE_CLICK_TIMEOUT: Duration = Duration::from_secs(20);

/// Unsubscribes, using `account` for anything that has to be sent.
pub async fn unsubscribe(account: &AccountConfig, method: &Method) -> Result<()> {
    match method {
        Method::OneClick(url) => one_click(url).await,
        Method::Mail { to, subject, body } => mail(account, to, subject, body).await,
        Method::Browser(_) => bail!("this list can only be left from its web page"),
    }
}

async fn one_click(url: &str) -> Result<()> {
    let request = http::post_form_to(url, &[ONE_CLICK_BODY]);
    let timeout = async {
        smol::Timer::after(ONE_CLICK_TIMEOUT).await;
        bail!("the list's server did not answer")
    };
    let status = smol::future::or(request, timeout).await?;

    // A redirect after a POST is how plenty of senders say "done, here is a
    // page about it". Only a refusal is a failure.
    match status {
        200..=399 => Ok(()),
        status => bail!("the list's server refused the request (HTTP {status})"),
    }
}

async fn mail(account: &AccountConfig, to: &str, subject: &str, body: &str) -> Result<()> {
    let credentials = account.credentials().await?;

    let mut draft = Draft::new(Address::bare(account.email.clone()));
    draft.to = vec![Address::bare(to)];
    draft.subject = subject.to_string();
    draft.body = body.to_string();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let unique = format!("{:x}.{:x}.unsubscribe", now.as_nanos(), std::process::id());
    let message_id = compose::message_id(&draft.from, &unique);
    let message = compose::render(&draft, &message_id, now.as_secs() as i64);

    // Not filed in Sent: nobody wrote this, and a row in Sent for every list
    // left is clutter rather than a record anyone wants.
    smtp::send(account, &credentials, &account.email, &[to.to_string()], &message).await
}
